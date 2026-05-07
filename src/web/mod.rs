use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Instant;

use crate::hls::pipeline::HlsPipeline;

pub struct AppState {
    pub pipelines: RwLock<HashMap<String, Arc<RwLock<PipelineSession>>>>,
    pub hdhr_host: String,
    pub hdhr_port: String,
    pub bitrate: String,
    pub grace_period: u64,
    pub external_host: String,
    pub lineup_cache: RwLock<Option<(serde_json::Value, Instant)>>,
    pub provider: Arc<dyn crate::providers::MetadataProvider>,
    pub segment_duration: f64,
    pub min_segments: usize,
}

pub struct PipelineSession {
    pub pipeline: HlsPipeline,
    pub poller_handle: Option<tokio::task::JoinHandle<()>>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/hls/{channel}/live.m3u8", get(serve_playlist))
        .route("/hls/{channel}/seg/{*rest}", get(serve_segment))
        .route("/art/{channel}", get(serve_art))
        .route("/logo/{channel}", get(serve_logo))
        .route("/api/stations", get(api_stations))
        .route("/status.json", get(api_status))
        .route("/test/{channel}", get(test_player))
        .with_state(state)
}

async fn test_player(
    State(_state): State<Arc<AppState>>,
    Path(channel): Path<String>,
) -> Html<String> {
    let stream_url = format!("/hls/{channel}/live.m3u8");
    Html(format!(r#"<!DOCTYPE html>
<html><head><script src="https://cdn.jsdelivr.net/npm/hls.js@latest"></script></head>
<body style="background:#1a1a2e;color:white;font-family:sans-serif;text-align:center;padding:40px">
<h2>Stream Test — Channel {channel}</h2>
<audio id="a" controls autoplay style="width:80%"></audio>
<p id="s">Loading...</p>
<script>
var a=document.getElementById('a'),s=document.getElementById('s');
if(Hls.isSupported()){{var h=new Hls();h.loadSource('{stream_url}');h.attachMedia(a);
h.on(Hls.Events.MANIFEST_PARSED,function(){{s.textContent='Playing';a.play()}});
h.on(Hls.Events.ERROR,function(e,d){{s.textContent='Error: '+d.type+' '+d.details}})}}
else if(a.canPlayType('application/vnd.apple.mpegurl')){{a.src='{stream_url}';s.textContent='Native HLS'}}
else{{s.textContent='HLS not supported'}}
</script></body></html>"#))
}

async fn serve_playlist(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<String>,
) -> Response {
    let session = get_or_create_pipeline(&state, &channel).await;
    let session_guard = match session {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(channel, "Failed to create pipeline: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // Wait for enough segments to buffer before serving
    for i in 0..20 {
        {
            let sess = session_guard.read().await;
            let store = sess.pipeline.segment_store.read().await;
            let count = store.count();
            if count >= state.min_segments {
                let first = store.first_seq();
                let last = store.last_seq();
                let scheme = if state.external_host.contains(':') { "http" } else { "https" };
                let base_url = format!("{scheme}://{}", state.external_host);
                let playlist = store.generate_playlist(&base_url, &channel, state.min_segments);
                tracing::debug!(channel, count, first, last, "Serving playlist");
                let mut headers = HeaderMap::new();
                headers.insert(header::CONTENT_TYPE, "application/vnd.apple.mpegurl".parse().expect("valid header"));
                headers.insert(header::CACHE_CONTROL, "no-cache, no-store".parse().expect("valid header"));
                return (headers, playlist).into_response();
            }
            if i == 0 {
                tracing::info!(channel, count, "Waiting for segments");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    tracing::warn!(channel, "Timed out waiting for segments");
    (StatusCode::SERVICE_UNAVAILABLE, "Stream starting...").into_response()
}

async fn serve_segment(
    State(state): State<Arc<AppState>>,
    Path((channel, rest)): Path<(String, String)>,
) -> Response {
    let seq_str = rest.trim_end_matches(".ts");
    let seq: u64 = match seq_str.parse() {
        Ok(s) => s,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let pipelines = state.pipelines.read().await;
    let Some(session) = pipelines.get(&channel) else {
        tracing::warn!(channel, seq, "Segment request but no pipeline");
        return StatusCode::NOT_FOUND.into_response();
    };

    let sess = session.read().await;
    sess.pipeline.touch_access();

    let store = sess.pipeline.segment_store.read().await;
    let first_seq = store.first_seq();
    let last_seq = store.last_seq();
    let count = store.count();
    match store.get(seq) {
        Some(data) => {
            tracing::debug!(channel, seq, first_seq, last_seq, count, bytes = data.len(), "Serving segment");
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, "video/mp2t".parse().expect("valid header"));
            headers.insert(header::CACHE_CONTROL, "public, max-age=3600".parse().expect("valid header"));
            (headers, data.clone()).into_response()
        }
        None => {
            tracing::warn!(channel, seq, first_seq, last_seq, count, "Segment not found - requested seq outside available range");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

async fn serve_art(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<String>,
) -> Response {
    let pipelines = state.pipelines.read().await;
    if let Some(session) = pipelines.get(&channel) {
        let sess = session.read().await;
        let art = sess.pipeline.artwork_url.read().await;
        if let Some(url) = art.as_ref() {
            return (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, url.as_str())],
            )
                .into_response();
        }
    }

    if let Some(data) = resolve_logo(&state, &channel).await {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "image/png".parse().expect("valid header"));
        return (headers, data).into_response();
    }

    StatusCode::NOT_FOUND.into_response()
}

async fn serve_logo(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<String>,
) -> Response {
    if let Some(data) = resolve_logo(&state, &channel).await {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "image/png".parse().expect("valid header"));
        headers.insert(header::CACHE_CONTROL, "public, max-age=86400".parse().expect("valid header"));
        return (headers, data).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn resolve_logo(state: &AppState, channel: &str) -> Option<Vec<u8>> {
    let lineup = fetch_lineup(state).await;
    let guide_name = lineup
        .as_array()?
        .iter()
        .find(|c| c.get("GuideNumber").and_then(|v| v.as_str()) == Some(channel))?
        .get("GuideName")
        .and_then(|v| v.as_str())?;
    state.provider.logo_for(guide_name).map(|b| b.to_vec())
}


async fn api_stations(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(fetch_lineup(&state).await)
}


async fn api_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let pipelines = state.pipelines.read().await;
    let mut status = serde_json::Map::new();
    for (ch, session) in pipelines.iter() {
        let sess = session.read().await;
        let store = sess.pipeline.segment_store.read().await;
        status.insert(
            ch.clone(),
            serde_json::json!({ "segments": !store.is_empty() }),
        );
    }
    Json(serde_json::Value::Object(status))
}

async fn get_or_create_pipeline(
    state: &AppState,
    channel: &str,
) -> anyhow::Result<Arc<RwLock<PipelineSession>>> {
    {
        let pipelines = state.pipelines.read().await;
        if let Some(session) = pipelines.get(channel) {
            return Ok(session.clone());
        }
    }

    let mut pipelines = state.pipelines.write().await;
    if let Some(session) = pipelines.get(channel) {
        return Ok(session.clone());
    }

    tracing::info!(channel, "Creating new HLS pipeline");

    let lineup = fetch_lineup(state).await;
    let guide_name = lineup
        .as_array()
        .and_then(|chs| {
            chs.iter().find(|c| {
                c.get("GuideNumber").and_then(|v| v.as_str()) == Some(channel)
            })
        })
        .and_then(|c| c.get("GuideName").and_then(|v| v.as_str()))
        .unwrap_or("");

    tracing::info!(channel, guide_name, "Resolved channel info");
    let has_logo = state.provider.logo_for(guide_name).is_some();
    let scheme = if state.external_host.contains(':') { "http" } else { "https" };
    let fallback_art = if has_logo {
        Some(format!("{scheme}://{}/logo/{channel}", state.external_host))
    } else {
        None
    };

    let pipeline = HlsPipeline::start(
        channel,
        &state.hdhr_host,
        &state.hdhr_port,
        &state.bitrate,
        fallback_art.clone(),
        state.segment_duration,
    )
    .await?;

    let poller_handle = state.provider.station_id_for(guide_name).map(|station_id| {
        tracing::info!(channel, station_id = %station_id, "Starting metadata poller");
        state.provider.start_poller(
            &station_id,
            pipeline.artwork_url.clone(),
            pipeline.track_info.clone(),
            fallback_art,
        )
    });

    let session = Arc::new(RwLock::new(PipelineSession {
        pipeline,
        poller_handle,
    }));

    pipelines.insert(channel.to_string(), session.clone());

    Ok(session)
}

pub async fn fetch_lineup(state: &AppState) -> serde_json::Value {
    {
        let cache = state.lineup_cache.read().await;
        if let Some((data, fetched_at)) = cache.as_ref()
            && fetched_at.elapsed() < std::time::Duration::from_secs(300)
        {
            return data.clone();
        }
    }

    let url = format!("http://{}/lineup.json", state.hdhr_host);
    match reqwest::get(&url).await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(data) => {
                let mut cache = state.lineup_cache.write().await;
                *cache = Some((data.clone(), Instant::now()));
                data
            }
            Err(_) => serde_json::Value::Array(vec![]),
        },
        Err(_) => serde_json::Value::Array(vec![]),
    }
}

