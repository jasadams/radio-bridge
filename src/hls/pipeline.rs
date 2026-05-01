use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Instant;

use super::id3_inject::{self, TrackMetadata};
use super::muxer::TsMuxer;
use super::segment_store::SegmentStore;

pub struct HlsPipeline {
    pub channel: String,
    pub segment_store: Arc<RwLock<SegmentStore>>,
    pub last_access: Arc<RwLock<Instant>>,
    pub artwork_url: Arc<RwLock<Option<String>>>,
    pub track_info: Arc<RwLock<(String, String)>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    mux_task: Option<tokio::task::JoinHandle<()>>,
}

impl HlsPipeline {
    pub async fn start(
        channel: &str,
        hdhr_host: &str,
        hdhr_port: &str,
        bitrate: &str,
        fallback_art_url: Option<String>,
    ) -> Result<Self> {
        let input_url = format!("http://{hdhr_host}:{hdhr_port}/auto/v{channel}");
        let bitrate_num: u32 = bitrate
            .trim_end_matches('k')
            .parse::<u32>()
            .unwrap_or(256)
            * 1000;

        tracing::info!(channel, %input_url, "Starting pipeline");

        let config = transcoder::TranscoderConfig {
            input_url,
            bitrate: bitrate_num,
            sample_rate: 44100,
            channels: 2,
        };

        let mut handle = transcoder::native::start(config).await?;

        let segment_store = Arc::new(RwLock::new(SegmentStore::new(30)));
        let last_access = Arc::new(RwLock::new(Instant::now()));
        let artwork_url: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(fallback_art_url));
        let track_info: Arc<RwLock<(String, String)>> =
            Arc::new(RwLock::new((String::new(), String::new())));

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let store = segment_store.clone();
        let art = artwork_url.clone();
        let track = track_info.clone();
        let ch = channel.to_string();

        let mux_task = tokio::spawn(async move {
            let mut muxer = TsMuxer::new(4.0);

            loop {
                tokio::select! {
                    frame = handle.frames.recv() => {
                        let Some(frame) = frame else {
                            tracing::info!(channel = %ch, "Transcoder stream ended");
                            break;
                        };

                        // Update ID3 metadata from current track info
                        let (title, artist) = track.read().await.clone();
                        let art_guard = art.read().await;
                        let art_url = art_guard.as_deref().unwrap_or("");
                        let meta = TrackMetadata {
                            title: &title,
                            artist: &artist,
                            artwork_url: art_url,
                        };
                        muxer.set_id3(id3_inject::build_id3v2(&meta));
                        drop(art_guard);

                        if let Some(segment) = muxer.push_frame(frame) {
                            let mut s = store.write().await;
                            let seq = s.add(segment.data, segment.duration);
                            tracing::debug!(channel = %ch, seq, duration = segment.duration, "Segment ready");
                        }
                    }
                    _ = &mut shutdown_rx => {
                        tracing::info!(channel = %ch, "Pipeline shutdown");
                        break;
                    }
                }
            }

            if let Some(segment) = muxer.flush() {
                let mut s = store.write().await;
                s.add(segment.data, segment.duration);
            }

            handle.stop().await;
        });

        Ok(Self {
            channel: channel.to_string(),
            segment_store,
            last_access,
            artwork_url,
            track_info,
            shutdown: Some(shutdown_tx),
            mux_task: Some(mux_task),
        })
    }

    pub fn touch_access(&self) -> tokio::task::JoinHandle<()> {
        let last_access = self.last_access.clone();
        tokio::spawn(async move {
            *last_access.write().await = Instant::now();
        })
    }

    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.mux_task.take() {
            let _ = task.await;
        }
        tracing::info!(channel = %self.channel, "Pipeline stopped, tuner released");
    }
}

impl Drop for HlsPipeline {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
