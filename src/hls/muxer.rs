use bytes::Bytes;
use transcoder::AacFrame;

const TS_PACKET_SIZE: usize = 188;
const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const AUDIO_PID: u16 = 0x0100;
const META_PID: u16 = 0x0150;

pub struct TsMuxer {
    segment_duration_pts: u64,
    current_segment: Vec<u8>,
    segment_start_pts: u64,
    segment_frame_count: u32,
    audio_cc: u8,
    meta_cc: u8,
    pat_cc: u8,
    pmt_cc: u8,
    id3_bytes: Vec<u8>,
    started: bool,
    sample_rate: u32,
}

pub struct CompletedSegment {
    pub data: Bytes,
    pub duration: f64,
}

impl TsMuxer {
    pub fn new(segment_duration_secs: f64) -> Self {
        Self {
            segment_duration_pts: (segment_duration_secs * 90_000.0) as u64,
            current_segment: Vec::with_capacity(512 * 1024),
            segment_start_pts: 0,
            segment_frame_count: 0,
            audio_cc: 0,
            meta_cc: 0,
            pat_cc: 0,
            pmt_cc: 0,
            id3_bytes: Vec::new(),
            started: false,
            sample_rate: 0,
        }
    }

    pub fn set_id3(&mut self, id3: Vec<u8>) {
        self.id3_bytes = id3;
    }

    pub fn push_frame(&mut self, frame: AacFrame) -> Option<CompletedSegment> {
        self.sample_rate = frame.sample_rate;
        if !self.started {
            self.begin_segment(frame.pts);
            self.started = true;
        }

        let elapsed = frame.pts.saturating_sub(self.segment_start_pts);
        if elapsed >= self.segment_duration_pts && self.segment_frame_count > 0 {
            let completed = self.finish_segment();
            self.begin_segment(frame.pts);
            self.write_audio_frame(&frame);
            return Some(completed);
        }

        self.write_audio_frame(&frame);
        None
    }

    pub fn flush(&mut self) -> Option<CompletedSegment> {
        if self.segment_frame_count > 0 {
            Some(self.finish_segment())
        } else {
            None
        }
    }

    fn begin_segment(&mut self, pts: u64) {
        self.current_segment.clear();
        self.segment_start_pts = pts;
        self.segment_frame_count = 0;

        self.write_pat();
        self.write_pmt();

        if !self.id3_bytes.is_empty() {
            self.write_metadata_pes(pts);
        }
    }

    fn finish_segment(&mut self) -> CompletedSegment {
        let duration = (self.segment_frame_count as f64 * 1024.0) / self.sample_rate as f64;
        CompletedSegment {
            data: Bytes::from(std::mem::take(&mut self.current_segment)),
            duration,
        }
    }

    fn write_audio_frame(&mut self, frame: &AacFrame) {
        let is_first = self.segment_frame_count == 0;
        let pes = build_audio_pes(&frame.data, frame.pts, is_first);
        self.write_pes_to_ts(&pes, AUDIO_PID, &mut self.audio_cc.clone(), is_first, frame.pts);
        self.segment_frame_count += 1;
    }

    fn write_pes_to_ts(
        &mut self,
        pes: &[u8],
        pid: u16,
        _cc_placeholder: &mut u8,
        include_pcr: bool,
        pcr: u64,
    ) {
        let cc = match pid {
            AUDIO_PID => &mut self.audio_cc,
            META_PID => &mut self.meta_cc,
            _ => return,
        };

        let mut offset = 0;
        let mut first = true;

        while offset < pes.len() {
            let mut packet = [0xFF_u8; TS_PACKET_SIZE];
            packet[0] = 0x47;

            let pusi = if first { 0x40 } else { 0x00 };
            packet[1] = pusi | ((pid >> 8) as u8 & 0x1F);
            packet[2] = pid as u8;

            let mut header_size = 4;
            let remaining = pes.len() - offset;

            if first && include_pcr && pid == AUDIO_PID {
                // Adaptation field with PCR
                packet[3] = 0x30 | (*cc & 0x0F); // adaptation + payload
                packet[4] = 7; // adaptation field length
                packet[5] = 0x10; // PCR flag
                write_pcr(&mut packet[6..12], pcr);
                header_size = 12;
            } else {
                packet[3] = 0x10 | (*cc & 0x0F); // payload only
            }

            let payload_space = TS_PACKET_SIZE - header_size;
            let payload_len = remaining.min(payload_space);

            if payload_len < payload_space {
                // Need stuffing via adaptation field
                let stuffing = payload_space - payload_len;
                if header_size == 4 {
                    // No adaptation field yet, add one
                    packet[3] = 0x30 | (*cc & 0x0F);
                    packet[4] = (stuffing - 1) as u8;
                    if stuffing > 1 {
                        packet[5] = 0x00;
                        for b in &mut packet[6..4 + stuffing] {
                            *b = 0xFF;
                        }
                    }
                    packet[4 + stuffing..4 + stuffing + payload_len]
                        .copy_from_slice(&pes[offset..offset + payload_len]);
                } else {
                    // Already have adaptation field, extend it
                    let current_adapt_len = packet[4] as usize;
                    packet[4] = (current_adapt_len + stuffing) as u8;
                    let adapt_end = 5 + current_adapt_len;
                    for b in &mut packet[adapt_end..adapt_end + stuffing] {
                        *b = 0xFF;
                    }
                    let new_payload_start = adapt_end + stuffing;
                    packet[new_payload_start..new_payload_start + payload_len]
                        .copy_from_slice(&pes[offset..offset + payload_len]);
                }
            } else {
                packet[header_size..header_size + payload_len]
                    .copy_from_slice(&pes[offset..offset + payload_len]);
            }

            self.current_segment.extend_from_slice(&packet);
            offset += payload_len;
            *cc = (*cc + 1) & 0x0F;
            first = false;
        }
    }

    fn write_pat(&mut self) {
        let mut section = Vec::with_capacity(20);
        section.push(0x00); // table_id
        section.extend_from_slice(&[0xB0, 0x0D]); // section_syntax + length = 13
        section.extend_from_slice(&[0x00, 0x01]); // transport_stream_id
        section.push(0xC1); // version 0, current
        section.push(0x00); // section_number
        section.push(0x00); // last_section_number
        section.extend_from_slice(&[0x00, 0x01]); // program_number = 1
        let pmt_hi = 0xE0 | ((PMT_PID >> 8) as u8 & 0x1F);
        section.push(pmt_hi);
        section.push(PMT_PID as u8);
        let crc = crc32_mpeg2(&section);
        section.extend_from_slice(&crc.to_be_bytes());

        self.write_section_packet(PAT_PID, &section, &mut self.pat_cc.clone());
    }

    fn write_pmt(&mut self) {
        let registration_desc = [0x05, 0x04, b'I', b'D', b'3', b' '];

        let mut section = Vec::with_capacity(40);
        section.push(0x02); // table_id
        section.extend_from_slice(&[0x00, 0x00]); // placeholder for section_length
        section.extend_from_slice(&[0x00, 0x01]); // program_number
        section.push(0xC1); // version 0, current
        section.push(0x00); // section_number
        section.push(0x00); // last_section_number
        // PCR PID
        section.push(0xE0 | ((AUDIO_PID >> 8) as u8 & 0x1F));
        section.push(AUDIO_PID as u8);
        section.extend_from_slice(&[0xF0, 0x00]); // program_info_length = 0

        // Audio ES: stream_type 0x0F (AAC), PID 0x0100
        section.push(0x0F);
        section.push(0xE0 | ((AUDIO_PID >> 8) as u8 & 0x1F));
        section.push(AUDIO_PID as u8);
        section.extend_from_slice(&[0xF0, 0x00]); // ES_info_length = 0

        // Metadata ES: stream_type 0x15, PID 0x0150
        if !self.id3_bytes.is_empty() {
            section.push(0x15);
            section.push(0xE0 | ((META_PID >> 8) as u8 & 0x1F));
            section.push(META_PID as u8);
            let es_info_len = registration_desc.len() as u16;
            section.push(0xF0 | ((es_info_len >> 8) as u8 & 0x0F));
            section.push(es_info_len as u8);
            section.extend_from_slice(&registration_desc);
        }

        // Fix section_length (includes everything after length field + 4 byte CRC)
        let section_length = section.len() - 3 + 4;
        section[1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        section[2] = section_length as u8;

        let crc = crc32_mpeg2(&section);
        section.extend_from_slice(&crc.to_be_bytes());

        self.write_section_packet(PMT_PID, &section, &mut self.pmt_cc.clone());
    }

    fn write_section_packet(&mut self, pid: u16, section: &[u8], _cc_placeholder: &mut u8) {
        let cc = match pid {
            PAT_PID => &mut self.pat_cc,
            PMT_PID => &mut self.pmt_cc,
            _ => return,
        };

        let mut packet = [0xFF_u8; TS_PACKET_SIZE];
        packet[0] = 0x47;
        packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
        packet[2] = pid as u8;

        let payload_needed = 1 + section.len(); // pointer byte + section
        let stuffing = TS_PACKET_SIZE - 4 - payload_needed;

        if stuffing > 0 {
            packet[3] = 0x30 | (*cc & 0x0F);
            packet[4] = (stuffing - 1) as u8;
            if stuffing > 1 {
                packet[5] = 0x00;
                for b in &mut packet[6..4 + stuffing] {
                    *b = 0xFF;
                }
            }
        } else {
            packet[3] = 0x10 | (*cc & 0x0F);
        }

        let payload_start = 4 + stuffing;
        packet[payload_start] = 0x00; // pointer field
        packet[payload_start + 1..payload_start + 1 + section.len()].copy_from_slice(section);

        self.current_segment.extend_from_slice(&packet);
        *cc = (*cc + 1) & 0x0F;
    }

    fn write_metadata_pes(&mut self, pts: u64) {
        let pes = build_metadata_pes(&self.id3_bytes, pts);
        let mut cc = self.meta_cc;
        self.write_pes_to_ts(&pes, META_PID, &mut cc, false, 0);
        self.meta_cc = cc;
    }
}

fn build_audio_pes(aac_data: &[u8], pts: u64, _is_first: bool) -> Vec<u8> {
    let pts_bytes = encode_pts(pts);
    let pes_header_data_len: u8 = 5;
    let pes_packet_len = 3 + pes_header_data_len as usize + aac_data.len();

    let mut pes = Vec::with_capacity(6 + pes_packet_len);
    pes.extend_from_slice(&[0x00, 0x00, 0x01]); // start code
    pes.push(0xC0); // audio stream 0
    if pes_packet_len <= 0xFFFF {
        pes.extend_from_slice(&(pes_packet_len as u16).to_be_bytes());
    } else {
        pes.extend_from_slice(&[0x00, 0x00]); // unbounded
    }
    pes.push(0x80); // marker bits
    pes.push(0x80); // PTS only
    pes.push(pes_header_data_len);
    pes.extend_from_slice(&pts_bytes);
    pes.extend_from_slice(aac_data);
    pes
}

fn build_metadata_pes(id3_bytes: &[u8], pts: u64) -> Vec<u8> {
    let pts_bytes = encode_pts(pts);
    let pes_header_data_len: u8 = 5;
    let pes_packet_len = 3 + pes_header_data_len as usize + id3_bytes.len();

    let mut pes = Vec::with_capacity(6 + pes_packet_len);
    pes.extend_from_slice(&[0x00, 0x00, 0x01]);
    pes.push(0xBD); // private_stream_1
    pes.extend_from_slice(&(pes_packet_len as u16).to_be_bytes());
    pes.push(0x84); // data_alignment_indicator
    pes.push(0x80); // PTS only
    pes.push(pes_header_data_len);
    pes.extend_from_slice(&pts_bytes);
    pes.extend_from_slice(id3_bytes);
    pes
}

fn encode_pts(pts: u64) -> [u8; 5] {
    [
        0x21 | (((pts >> 29) & 0x0E) as u8),
        ((pts >> 22) & 0xFF) as u8,
        0x01 | (((pts >> 14) & 0xFE) as u8),
        ((pts >> 7) & 0xFF) as u8,
        0x01 | (((pts << 1) & 0xFE) as u8),
    ]
}

fn write_pcr(buf: &mut [u8], pcr: u64) {
    let base = pcr;
    let ext: u16 = 0;
    buf[0] = (base >> 25) as u8;
    buf[1] = (base >> 17) as u8;
    buf[2] = (base >> 9) as u8;
    buf[3] = (base >> 1) as u8;
    buf[4] = ((base & 1) << 7) as u8 | 0x7E | ((ext >> 8) as u8 & 0x01);
    buf[5] = ext as u8;
}

fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            if crc & 0x80000000 != 0 {
                crc = (crc << 1) ^ 0x04C11DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}
