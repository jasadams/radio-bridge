use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::adts::AdtsStreamParser;
use crate::{TranscoderConfig, TranscoderHandle};

pub async fn start(config: TranscoderConfig) -> Result<TranscoderHandle> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-i",
            &config.input_url,
            "-c:a",
            "aac",
            "-b:a",
            &format!("{}k", config.bitrate / 1000),
            "-ac",
            &config.channels.to_string(),
            "-ar",
            &config.sample_rate.to_string(),
            "-f",
            "adts",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn ffmpeg")?;

    let stdout = child.stdout.take().context("No stdout from ffmpeg")?;
    let (tx, rx) = mpsc::channel(64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut parser = AdtsStreamParser::new();
        let mut buf = [0u8; 4096];

        loop {
            tokio::select! {
                result = reader.read(&mut buf) => {
                    match result {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let frames = parser.push(&buf[..n]);
                            for frame in frames {
                                if tx.send(frame).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                _ = &mut shutdown_rx => {
                    tracing::info!("Transcoder shutdown signal received");
                    return;
                }
            }
        }
        tracing::info!("Transcoder stream ended");
    });

    Ok(TranscoderHandle::new(rx, shutdown_tx).with_child(child))
}
