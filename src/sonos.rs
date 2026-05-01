use anyhow::Result;
use std::net::TcpStream;
use std::time::Duration;

#[derive(Clone, serde::Serialize)]
pub struct Speaker {
    pub ip: String,
    pub name: String,
    pub model: String,
}

pub async fn discover(subnets: &[String]) -> Vec<Speaker> {
    let mut ips = Vec::new();
    for subnet in subnets {
        let prefix = subnet.rsplit_once('.').map(|(p, _)| p).unwrap_or(subnet);
        for i in 1..255 {
            ips.push(format!("{prefix}.{i}"));
        }
    }

    let handles: Vec<_> = ips
        .into_iter()
        .map(|ip| {
            tokio::task::spawn_blocking(move || {
                if TcpStream::connect_timeout(
                    &format!("{ip}:1400").parse().ok()?,
                    Duration::from_millis(400),
                )
                .is_ok()
                {
                    get_speaker_info(&ip).ok()
                } else {
                    None
                }
            })
        })
        .collect();

    let mut speakers = Vec::new();
    for handle in handles {
        if let Ok(Some(speaker)) = handle.await {
            speakers.push(speaker);
        }
    }
    speakers
}

fn get_speaker_info(ip: &str) -> Result<Speaker> {
    let url = format!("http://{ip}:1400/xml/device_description.xml");
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder().timeout_global(Some(Duration::from_secs(2))).build()
    );
    let body = agent.get(&url).call()?.body_mut().read_to_string()?;

    let room = extract_xml_value(&body, "roomName").unwrap_or_default();
    let model = extract_xml_value(&body, "modelName").unwrap_or_default();

    if room.is_empty() {
        anyhow::bail!("No room name");
    }

    Ok(Speaker {
        ip: ip.to_string(),
        name: room,
        model,
    })
}

pub async fn play(speaker_ip: &str, stream_url: &str, title: &str, art_url: Option<&str>) -> Result<()> {
    let radio_url = stream_url.replace("http://", "x-rincon-mp3radio://")
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
            ureq::config::Config::builder().timeout_global(Some(Duration::from_secs(10))).build()
        );
        agent.post(&url)
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
