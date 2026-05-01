use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, serde::Serialize)]
pub struct Speaker {
    pub ip: String,
    pub name: String,
    pub model: String,
}

pub async fn discover() -> Vec<Speaker> {
    use futures_util::StreamExt;

    let discovery = match mdns::discover::all("_sonos._tcp.local", Duration::from_secs(2)) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("mDNS discovery failed: {e}");
            return Vec::new();
        }
    };

    let mut stream = std::pin::pin!(discovery.listen());
    let mut ips: HashMap<String, ()> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(response))) => {
                for record in response.records() {
                    if let mdns::RecordKind::A(addr) = &record.kind {
                        ips.insert(addr.to_string(), ());
                    }
                }
            }
            _ => break,
        }
    }

    let mut speakers = Vec::new();
    for ip in ips.keys() {
        let speaker = tokio::task::spawn_blocking({
            let ip = ip.clone();
            move || get_speaker_info(&ip)
        })
        .await
        .ok()
        .flatten();

        if let Some(s) = speaker {
            tracing::info!(ip = %s.ip, name = %s.name, model = %s.model, "Found Sonos speaker");
            speakers.push(s);
        }
    }

    if speakers.is_empty() {
        tracing::warn!("No Sonos speakers found via mDNS");
    }

    speakers
}

fn get_speaker_info(ip: &str) -> Option<Speaker> {
    let url = format!("http://{ip}:1400/xml/device_description.xml");
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .build(),
    );
    let body = agent.get(&url).call().ok()?.body_mut().read_to_string().ok()?;
    let name = extract_xml_value(&body, "roomName")?;
    let model = extract_xml_value(&body, "modelName").unwrap_or_default();
    Some(Speaker { ip: ip.to_string(), name, model })
}

pub async fn play(speaker_ip: &str, stream_url: &str, title: &str, art_url: Option<&str>) -> Result<()> {
    let radio_url = stream_url
        .replace("http://", "x-rincon-mp3radio://")
        .replace("https://", "x-rincon-mp3radio://");

    let art_xml = art_url
        .map(|u| format!("<upnp:albumArtURI>{}</upnp:albumArtURI>", xml_escape(u)))
        .unwrap_or_default();

    let metadata = format!(
        r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"><item id="R:0/0/0" parentID="R:0/0" restricted="true"><dc:title>{}</dc:title><upnp:class>object.item.audioItem.audioBroadcast</upnp:class>{}</item></DIDL-Lite>"#,
        xml_escape(title),
        art_xml
    );

    soap_action(
        speaker_ip,
        "SetAVTransportURI",
        &format!(
            "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
            xml_escape(&radio_url),
            xml_escape(&metadata)
        ),
    )
    .await?;

    soap_action(
        speaker_ip,
        "Play",
        "<InstanceID>0</InstanceID><Speed>1</Speed>",
    )
    .await?;

    tracing::info!(speaker = speaker_ip, title, "Playing on Sonos");
    Ok(())
}

async fn soap_action(speaker_ip: &str, action: &str, body: &str) -> Result<()> {
    let url = format!("http://{speaker_ip}:1400/MediaRenderer/AVTransport/Control");
    let soap_action_header = format!("\"urn:schemas-upnp-org:service:AVTransport:1#{action}\"");
    let soap = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">{body}</u:{action}></s:Body></s:Envelope>"#
    );

    tokio::task::spawn_blocking(move || {
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(Duration::from_secs(10)))
                .build(),
        );
        agent
            .post(&url)
            .header("Content-Type", r#"text/xml; charset="utf-8""#)
            .header("SOAPAction", &soap_action_header)
            .send(soap.as_bytes())?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;

    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn extract_xml_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}
