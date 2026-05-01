use anyhow::Result;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::MetadataProvider;

const STATION_MAP: &[(&str, &str)] = &[
    ("triple j Uneart", "triplej"),
    ("triple j", "triplej"),
    ("Double J", "doublej"),
    ("ABC Jazz", "jazz"),
    ("ABC Classic", "classic"),
    ("ABC Country", "country"),
];

const LOGO_MAP: &[(&str, &[u8])] = &[
    ("triple j Uneart", include_bytes!("../../providers/abc/logos/triplej_unearthed.png")),
    ("triple j", include_bytes!("../../providers/abc/logos/triplej.png")),
    ("Double J", include_bytes!("../../providers/abc/logos/doublej.png")),
    ("ABC Jazz", include_bytes!("../../providers/abc/logos/abc_jazz.png")),
    ("ABC KIDS Listen", include_bytes!("../../providers/abc/logos/abc_kids.png")),
    ("ABC Country", include_bytes!("../../providers/abc/logos/abc_country.png")),
    ("ABC NewsRadio", include_bytes!("../../providers/abc/logos/abc_newsradio.png")),
    ("ABC SYDNEY", include_bytes!("../../providers/abc/logos/abc_sydney.png")),
    ("ABC RN", include_bytes!("../../providers/abc/logos/abc_rn.png")),
    ("ABC Classic", include_bytes!("../../providers/abc/logos/abc_classic.png")),
    ("SBS Radio 1", include_bytes!("../../providers/abc/logos/sbs_radio1.png")),
    ("SBS Radio 2", include_bytes!("../../providers/abc/logos/sbs_radio2.png")),
    ("SBS Radio 3", include_bytes!("../../providers/abc/logos/sbs_radio3.png")),
    ("SBS Arabic", include_bytes!("../../providers/abc/logos/sbs_arabic.png")),
    ("SBS South Asian", include_bytes!("../../providers/abc/logos/sbs_southasian.png")),
    ("SBS Chill", include_bytes!("../../providers/abc/logos/sbs_chill.png")),
    ("SBS PopAsia", include_bytes!("../../providers/abc/logos/sbs_popasia.png")),
];

pub struct AbcProvider;

impl MetadataProvider for AbcProvider {
    fn station_id_for(&self, guide_name: &str) -> Option<String> {
        STATION_MAP
            .iter()
            .find(|(prefix, _)| guide_name.starts_with(prefix))
            .map(|(_, id)| id.to_string())
    }

    fn logo_for(&self, guide_name: &str) -> Option<&'static [u8]> {
        LOGO_MAP
            .iter()
            .find(|(prefix, _)| guide_name.starts_with(prefix))
            .map(|(_, data)| *data)
    }


    fn start_poller(
        &self,
        station_id: &str,
        artwork_target: Arc<RwLock<Option<String>>>,
        track_target: Arc<RwLock<(String, String)>>,
    ) -> tokio::task::JoinHandle<()> {
        let station_id = station_id.to_string();
        tokio::spawn(async move {
            run_poller(station_id, artwork_target, track_target).await;
        })
    }
}

async fn run_poller(
    station_id: String,
    artwork_target: Arc<RwLock<Option<String>>>,
    track_target: Arc<RwLock<(String, String)>>,
) {
    tracing::info!(station = %station_id, "ABC now-playing poller running");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("valid client");
    let mut last_title = String::new();

    loop {
        match fetch_now_playing(&client, &station_id).await {
            Ok(Some(np)) => {
                let current_title = format!("{} - {}", np.artist, np.title);
                if current_title != last_title {
                    tracing::info!(
                        station = %station_id,
                        artist = %np.artist,
                        title = %np.title,
                        art = np.art_url.is_some(),
                        "Now playing"
                    );
                    last_title = current_title;
                    *artwork_target.write().await = np.art_url;
                    *track_target.write().await = (np.title, np.artist);
                }
            }
            Ok(None) => match fetch_program(&client, &station_id).await {
                Ok(Some(prog)) => {
                    if last_title != prog {
                        tracing::info!(station = %station_id, program = %prog, "Showing program");
                        last_title = prog.clone();
                        *artwork_target.write().await = None;
                        *track_target.write().await = (prog, String::new());
                    }
                }
                _ => {
                    if !last_title.is_empty() {
                        last_title.clear();
                        *artwork_target.write().await = None;
                        *track_target.write().await = (String::new(), String::new());
                    }
                }
            },
            Err(e) => {
                tracing::warn!(station = %station_id, "Now playing fetch failed: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    }
}

struct NowPlaying {
    artist: String,
    title: String,
    art_url: Option<String>,
}

async fn fetch_now_playing(client: &reqwest::Client, station_id: &str) -> Result<Option<NowPlaying>> {
    let url = format!("https://music.abcradio.net.au/api/v1/plays/{station_id}/now.json");
    let resp: ApiResponse = client.get(&url).timeout(std::time::Duration::from_secs(5)).send().await?.json().await?;

    if resp.now.is_null() || !resp.now.is_object() || resp.now.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(None);
    }
    let now: ApiNow = serde_json::from_value(resp.now)?;

    let artist = now.summary.artist.unwrap_or_default();
    let title = now.summary.title.unwrap_or_default();
    if artist.is_empty() && title.is_empty() {
        return Ok(None);
    }

    let art_url = now
        .recording
        .and_then(|r| r.releases.into_iter().next())
        .and_then(|rel| rel.artwork.into_iter().next())
        .and_then(|art| art.sizes.into_iter().find(|s| s.width >= 300).map(|s| s.url));

    Ok(Some(NowPlaying { artist, title, art_url }))
}

async fn fetch_program(client: &reqwest::Client, station_id: &str) -> Result<Option<String>> {
    let url = format!("https://program.abcradio.net.au/api/v1/programitems/{station_id}/live.json");
    let resp: ProgramResponse = client.get(&url).send().await?.json().await?;
    Ok(resp.now.map(|n| n.title))
}

#[derive(Deserialize)]
struct ApiResponse {
    #[serde(default)]
    now: serde_json::Value,
}

#[derive(Deserialize)]
struct ApiNow {
    summary: ApiSummary,
    recording: Option<ApiRecording>,
}

#[derive(Deserialize)]
struct ApiSummary {
    artist: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
struct ApiRecording {
    #[serde(default)]
    releases: Vec<ApiRelease>,
}

#[derive(Deserialize)]
struct ApiRelease {
    #[serde(default)]
    artwork: Vec<ApiArtwork>,
}

#[derive(Deserialize)]
struct ApiArtwork {
    #[serde(default)]
    sizes: Vec<ApiArtSize>,
}

#[derive(Deserialize)]
struct ApiArtSize {
    url: String,
    #[serde(default)]
    width: u32,
}

#[derive(Deserialize)]
struct ProgramResponse {
    now: Option<ProgramNow>,
}

#[derive(Deserialize)]
struct ProgramNow {
    title: String,
}
