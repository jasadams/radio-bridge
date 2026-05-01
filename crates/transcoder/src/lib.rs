pub mod adts;
#[cfg(feature = "ffmpeg")]
pub mod ffmpeg;
#[cfg(feature = "native")]
pub mod native;

use bytes::Bytes;
use tokio::sync::mpsc;

pub struct AacFrame {
    pub data: Bytes,
    pub pts: u64,
    pub sample_rate: u32,
    pub samples: u32,
}

pub struct TranscoderConfig {
    pub input_url: String,
    pub bitrate: u32,
    pub sample_rate: u32,
    pub channels: u16,
}

pub struct TranscoderHandle {
    pub frames: mpsc::Receiver<AacFrame>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    child: Option<tokio::process::Child>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl TranscoderHandle {
    pub fn new(
        frames: mpsc::Receiver<AacFrame>,
        shutdown: tokio::sync::oneshot::Sender<()>,
    ) -> Self {
        Self {
            frames,
            shutdown: Some(shutdown),
            child: None,
            task: None,
        }
    }

    pub fn with_child(mut self, child: tokio::process::Child) -> Self {
        self.child = Some(child);
        self
    }

    pub fn with_task(mut self, task: tokio::task::JoinHandle<()>) -> Self {
        self.task = Some(task);
        self
    }

    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for TranscoderHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
