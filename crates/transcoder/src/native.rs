use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use fdk_aac::enc::{Encoder, EncoderParams, Transport};
use symphonia_bundle_mp3::MpaDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use tokio::sync::{mpsc, oneshot};

use crate::{AacFrame, TranscoderConfig, TranscoderHandle};

const TS_PACKET_SIZE: usize = 188;

pub async fn start(config: TranscoderConfig) -> Result<TranscoderHandle> {
    let (tx, rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let task = tokio::spawn(async move {
        if let Err(e) = run(config, tx, shutdown_rx).await {
            tracing::error!("Native transcoder error: {e}");
        }
    });

    Ok(TranscoderHandle::new(rx, shutdown_tx).with_task(task))
}

async fn run(
    config: TranscoderConfig,
    tx: mpsc::Sender<AacFrame>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    tracing::info!(url = %config.input_url, "Native transcoder starting");

    let client = reqwest::Client::new();
    let response = client
        .get(&config.input_url)
        .send()
        .await
        .context("Failed to connect to HDHomeRun")?;

    let mut stream = response.bytes_stream();

    let mut encoder: Option<Encoder> = None;
    let mut actual_sample_rate = config.sample_rate;

    let mut ts_buf = Vec::with_capacity(TS_PACKET_SIZE * 8);
    let mut pes_assembler = PesAssembler::new();
    let mut decoder: Option<MpaDecoder> = None;
    let mut pts_counter: u64 = 0;
    let mut pcm_buf: Vec<i16> = Vec::with_capacity(8192);
    let mut pmt_pid: Option<u16> = None;
    let mut audio_pid: Option<u16> = None;
    let mut encode_buf = vec![0u8; 16384];

    loop {
        tokio::select! {
            chunk = next_chunk(&mut stream) => {
                let Some(chunk) = chunk else { break; };
                ts_buf.extend_from_slice(&chunk);

                while ts_buf.len() >= TS_PACKET_SIZE {
                    if ts_buf[0] != 0x47 {
                        if let Some(pos) = ts_buf.iter().position(|&b| b == 0x47) {
                            ts_buf.drain(..pos);
                        } else {
                            ts_buf.clear();
                            break;
                        }
                        continue;
                    }

                    let packet: Vec<u8> = ts_buf.drain(..TS_PACKET_SIZE).collect();
                    let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;

                    if pmt_pid.is_none() && pid == 0x0000 {
                        pmt_pid = parse_pmt_pid_from_pat(&packet);
                        if let Some(ppid) = pmt_pid {
                            tracing::info!(pmt_pid = ppid, "Found PMT PID in PAT");
                        }
                    }

                    if audio_pid.is_none() && Some(pid) == pmt_pid {
                        audio_pid = parse_audio_pid_from_pmt(&packet);
                        if let Some(apid) = audio_pid {
                            tracing::info!(audio_pid = apid, "Found audio PID in PMT");
                        }
                    }

                    if Some(pid) != audio_pid {
                        continue;
                    }

                    if let Some(pes_data) = pes_assembler.push(&packet) {
                        tracing::debug!(len = pes_data.len(), "PES frame assembled");

                        if decoder.is_none() {
                            let mut params = AudioCodecParameters::new();
                            params.for_codec(symphonia_core::codecs::audio::well_known::CODEC_ID_MP2);
                            match MpaDecoder::try_new(&params, &AudioDecoderOptions::default()) {
                                Ok(d) => {
                                    tracing::info!("MP2 decoder initialized");
                                    decoder = Some(d);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to create MP2 decoder: {e}");
                                    continue;
                                }
                            }
                        }

                        let Some(dec) = decoder.as_mut() else { continue; };

                        let pkt = Packet::new(
                            0,
                            symphonia_core::units::Timestamp::ZERO,
                            symphonia_core::units::Duration::ZERO,
                            pes_data,
                        );
                        let decoded = match dec.decode(&pkt) {
                            Ok(d) => d,
                            Err(e) => {
                                tracing::debug!("MP2 decode error: {e}");
                                continue;
                            }
                        };

                        if encoder.is_none() {
                            let sr = decoded.spec().rate();
                            actual_sample_rate = sr;
                            tracing::info!(sample_rate = sr, "Detected source sample rate, creating AAC encoder");
                            encoder = Some(Encoder::new(EncoderParams {
                                bit_rate: fdk_aac::enc::BitRate::Cbr(256000),
                                sample_rate: sr,
                                transport: Transport::Adts,
                                channels: fdk_aac::enc::ChannelMode::Stereo,
                                audio_object_type: fdk_aac::enc::AudioObjectType::Mpeg4LowComplexity,
                            }).map_err(|e| anyhow!("Failed to create AAC encoder: {e:?}"))?);
                        }

                        let pcm = extract_interleaved_i16(&decoded);
                        if pcm_buf.len() + pcm.len() > MAX_PCM_BUFFER {
                            tracing::warn!("PCM buffer overflow, dropping");
                            pcm_buf.clear();
                        }
                        pcm_buf.extend_from_slice(&pcm);

                        let aac_frame_samples = 1024 * 2; // 1024 per channel, stereo interleaved
                        let Some(enc) = encoder.as_ref() else { continue; };

                        while pcm_buf.len() >= aac_frame_samples {
                            let chunk: Vec<i16> = pcm_buf.drain(..aac_frame_samples).collect();
                            match enc.encode(&chunk, &mut encode_buf) {
                                Ok(info) => {
                                    if info.output_size > 0 {
                                        let aac_data = Bytes::copy_from_slice(&encode_buf[..info.output_size]);
                                        let frame = AacFrame {
                                            data: aac_data,
                                            pts: pts_counter,
                                            sample_rate: actual_sample_rate,
                                            samples: 1024,
                                        };
                                        pts_counter += (1024 * 90_000) / actual_sample_rate as u64;

                                        if tx.send(frame).await.is_err() {
                                            return Ok(());
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("AAC encode error: {e:?}");
                                }
                            }
                        }
                    }
                }
            }
            _ = &mut shutdown_rx => {
                tracing::info!("Native transcoder shutdown");
                return Ok(());
            }
        }
    }

    Ok(())
}

async fn next_chunk(
    stream: &mut (impl tokio_stream::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin),
) -> Option<Bytes> {
    use tokio_stream::StreamExt;
    stream.next().await?.ok()
}

fn extract_interleaved_i16(buf: &GenericAudioBufferRef) -> Vec<i16> {
    match buf {
        GenericAudioBufferRef::F32(b) => {
            let channels = b.spec().channels().count();
            let frames = b.frames();
            let mut out = Vec::with_capacity(frames * channels);
            for frame in 0..frames {
                for ch in 0..channels {
                    let sample = b[ch][frame];
                    out.push((sample.clamp(-1.0, 1.0) * 32767.0) as i16);
                }
            }
            out
        }
        GenericAudioBufferRef::S16(b) => {
            let channels = b.spec().channels().count();
            let frames = b.frames();
            let mut out = Vec::with_capacity(frames * channels);
            for frame in 0..frames {
                for ch in 0..channels {
                    out.push(b[ch][frame]);
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

const MAX_PES_SIZE: usize = 256 * 1024;
const MAX_PCM_BUFFER: usize = 96000;

struct PesAssembler {
    buf: Vec<u8>,
    collecting: bool,
    expected_len: usize,
}

impl PesAssembler {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(8192),
            collecting: false,
            expected_len: 0,
        }
    }

    fn push(&mut self, ts_packet: &[u8]) -> Option<Vec<u8>> {
        let pusi = (ts_packet[1] >> 6) & 1;
        let has_adaptation = (ts_packet[3] >> 5) & 1;
        let has_payload = (ts_packet[3] >> 4) & 1;

        if has_payload == 0 {
            return None;
        }

        let mut offset = 4;
        if has_adaptation == 1 {
            let adapt_len = ts_packet[4] as usize;
            offset = 5 + adapt_len;
        }
        if offset >= TS_PACKET_SIZE {
            return None;
        }

        let payload = &ts_packet[offset..];

        if pusi == 1 {
            let result = if self.collecting && !self.buf.is_empty() {
                Some(self.extract_audio_data())
            } else {
                None
            };

            self.buf.clear();
            self.collecting = false;

            if payload.len() >= 6 && payload[0] == 0x00 && payload[1] == 0x00 && payload[2] == 0x01 {
                let pes_length = ((payload[4] as usize) << 8) | payload[5] as usize;
                self.expected_len = if pes_length > 0 { pes_length + 6 } else { 0 };
                self.buf.extend_from_slice(payload);
                self.collecting = true;
            }

            return result;
        }

        if self.collecting {
            self.buf.extend_from_slice(payload);

            if self.buf.len() > MAX_PES_SIZE {
                tracing::warn!("PES buffer exceeded max size, dropping");
                self.buf.clear();
                self.collecting = false;
                return None;
            }

            if self.expected_len > 0 && self.buf.len() >= self.expected_len {
                self.collecting = false;
                return Some(self.extract_audio_data());
            }
        }

        None
    }

    fn extract_audio_data(&self) -> Vec<u8> {
        if self.buf.len() < 9 {
            return Vec::new();
        }
        let header_data_len = self.buf[8] as usize;
        let data_start = 9 + header_data_len;
        if data_start >= self.buf.len() {
            return Vec::new();
        }
        self.buf[data_start..].to_vec()
    }
}

fn parse_pmt_pid_from_pat(packet: &[u8]) -> Option<u16> {
    if packet.len() < TS_PACKET_SIZE || packet[0] != 0x47 {
        return None;
    }
    let pusi = (packet[1] >> 6) & 1;
    if pusi == 0 {
        return None;
    }
    let has_adaptation = (packet[3] >> 5) & 1;
    let mut offset = 4;
    if has_adaptation == 1 {
        offset = 5 + packet[4] as usize;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = packet[offset] as usize;
    offset += 1 + pointer;
    if offset + 8 >= TS_PACKET_SIZE {
        return None;
    }
    if packet[offset] != 0x00 {
        return None;
    }
    offset += 8;
    if offset + 4 > TS_PACKET_SIZE {
        return None;
    }
    let _program = ((packet[offset] as u16) << 8) | packet[offset + 1] as u16;
    let pmt_pid = ((packet[offset + 2] as u16 & 0x1F) << 8) | packet[offset + 3] as u16;
    Some(pmt_pid)
}

fn parse_audio_pid_from_pmt(packet: &[u8]) -> Option<u16> {
    if packet.len() < TS_PACKET_SIZE || packet[0] != 0x47 {
        return None;
    }
    let pusi = (packet[1] >> 6) & 1;
    if pusi == 0 {
        return None;
    }
    let has_adaptation = (packet[3] >> 5) & 1;
    let mut offset = 4;
    if has_adaptation == 1 {
        offset = 5 + packet[4] as usize;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }

    let pointer = packet[offset] as usize;
    let section_start = offset + 1 + pointer;
    if section_start + 12 >= TS_PACKET_SIZE {
        return None;
    }

    if packet[section_start] != 0x02 {
        return None;
    }

    let section_length = ((packet[section_start + 1] as usize & 0x0F) << 8)
        | packet[section_start + 2] as usize;
    let section_end = (section_start + 3 + section_length).min(TS_PACKET_SIZE);

    let program_info_length = ((packet[section_start + 10] as usize & 0x0F) << 8)
        | packet[section_start + 11] as usize;

    let mut pos = section_start + 12 + program_info_length;
    while pos + 5 <= section_end - 4 {
        let stream_type = packet[pos];
        let es_pid = ((packet[pos + 1] as u16 & 0x1F) << 8) | packet[pos + 2] as u16;
        let es_info_len = ((packet[pos + 3] as usize & 0x0F) << 8) | packet[pos + 4] as usize;

        if stream_type == 0x03 || stream_type == 0x04 {
            return Some(es_pid);
        }

        pos += 5 + es_info_len;
    }

    None
}
