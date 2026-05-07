mod discovery;
mod hls;
mod providers;
mod web;

use clap::Parser;
use std::collections::HashMap;

use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Duration;

#[derive(Parser)]
#[command(name = "radio-bridge", about = "HDHomeRun to HLS radio bridge")]
struct Args {
    #[arg(long, env = "HDHR_HOST")]
    hdhr_host: Option<String>,

    #[arg(long, default_value = "5004", env = "HDHR_PORT")]
    hdhr_port: String,

    #[arg(long, default_value = "256k", env = "BITRATE")]
    bitrate: String,

    #[arg(long, default_value = "8000", env = "PORT")]
    port: u16,

    #[arg(long, default_value = "30", env = "GRACE_PERIOD")]
    grace_period: u64,

    #[arg(long, env = "EXTERNAL_HOST")]
    external_host: Option<String>,

    #[arg(long, default_value = "2.0", env = "SEGMENT_DURATION")]
    segment_duration: f64,

    #[arg(long, default_value = "3", env = "MIN_SEGMENTS")]
    min_segments: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "radio_bridge=info".parse().expect("valid filter")),
        )
        .init();

    let args = Args::parse();

    let hdhr_host = match args.hdhr_host {
        Some(ref h) => h.clone(),
        None => {
            tracing::info!("Auto-discovering HDHomeRun...");
            let subnets = discovery::detect_subnets();
            tracing::info!("Scanning subnets: {}", subnets.join(", "));
            match discovery::find_hdhomerun(&subnets) {
                Some(ip) => {
                    tracing::info!("Found HDHomeRun at {ip}");
                    ip
                }
                None => {
                    tracing::error!("No HDHomeRun found. Specify --hdhr-host or set HDHR_HOST");
                    std::process::exit(1);
                }
            }
        }
    };

    let external_host = args
        .external_host
        .unwrap_or_else(|| format!("localhost:{}", args.port));

    let provider: Arc<dyn providers::MetadataProvider> = Arc::new(providers::abc::AbcProvider);

    let state = Arc::new(web::AppState {
        pipelines: RwLock::new(HashMap::new()),
        hdhr_host: hdhr_host.clone(),
        hdhr_port: args.hdhr_port,
        bitrate: args.bitrate,
        grace_period: args.grace_period,
        external_host: external_host.clone(),
        lineup_cache: RwLock::new(None),
        provider,
        segment_duration: args.segment_duration,
        min_segments: args.min_segments,
    });

    let monitor_state = state.clone();
    tokio::spawn(async move {
        grace_period_monitor(monitor_state).await;
    });

    let app = web::router(state);
    let bind = format!("0.0.0.0:{}", args.port);

    tracing::info!("Radio Bridge listening on {bind}");
    tracing::info!("HDHomeRun: {hdhr_host}");
    tracing::info!("External host: {external_host}");

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn grace_period_monitor(state: Arc<web::AppState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;

        let mut to_remove = Vec::new();
        {
            let pipelines = state.pipelines.read().await;
            for (channel, session) in pipelines.iter() {
                let sess = session.read().await;
                let last = *sess.pipeline.last_access.read().await;
                let idle = last.elapsed().as_secs();
                tracing::info!(channel, idle, grace = state.grace_period, "Checking pipeline");
                if idle > state.grace_period {
                    to_remove.push(channel.clone());
                }
            }
        }

        for channel in to_remove {
            let mut pipelines = state.pipelines.write().await;
            if let Some(session) = pipelines.remove(&channel) {
                let mut sess = session.write().await;
                if let Some(handle) = sess.poller_handle.take() {
                    handle.abort();
                }
                sess.pipeline.stop().await;
                tracing::info!(channel, "Pipeline removed (grace period expired)");
            }
        }
    }
}
