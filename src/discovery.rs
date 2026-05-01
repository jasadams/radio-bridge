use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

pub fn detect_subnets() -> Vec<String> {
    let mut subnets = Vec::new();

    // Bind a UDP socket to detect local interfaces
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        // Try connecting to various common gateways to discover interfaces
        for target in ["192.168.1.1:80", "192.168.0.1:80", "10.0.0.1:80", "172.16.0.1:80"] {
            if socket.connect(target).is_ok()
                && let Ok(addr) = socket.local_addr() {
                    let ip = addr.ip().to_string();
                    if let Some(prefix) = ip.rsplit_once('.') {
                        let subnet = format!("{}.0", prefix.0);
                        if !subnets.contains(&subnet) {
                            subnets.push(subnet);
                        }
                    }
                }
        }
    }

    // Also try getting all interfaces via /proc on Linux
    if let Ok(content) = std::fs::read_to_string("/proc/net/fib_trie") {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("|-- ") || trimmed.starts_with("+-- ") {
                let ip = trimmed.trim_start_matches("|-- ").trim_start_matches("+-- ");
                let ip = ip.split('/').next().unwrap_or("");
                if (ip.starts_with("192.168.") || ip.starts_with("10.") || ip.starts_with("172."))
                    && let Some(prefix) = ip.rsplit_once('.') {
                        let subnet = format!("{}.0", prefix.0);
                        if !subnets.contains(&subnet) {
                            subnets.push(subnet);
                        }
                    }
            }
        }
    }

    if subnets.is_empty() {
        subnets.push("192.168.1.0".to_string());
    }

    subnets
}

pub fn find_hdhomerun(subnets: &[String]) -> Option<String> {
    // Try mDNS first
    if let Some(ip) = resolve_mdns("hdhomerun.local")
        && verify_hdhomerun(&ip) {
            return Some(ip);
        }

    // Fall back to scanning subnets for HDHomeRun devices
    let mut handles = Vec::new();
    for subnet in subnets {
        let prefix = subnet.rsplit_once('.').map(|(p, _)| p).unwrap_or(subnet);
        for i in 1..255u8 {
            let ip = format!("{prefix}.{i}");
            handles.push(std::thread::spawn(move || {
                if TcpStream::connect_timeout(
                    &format!("{ip}:80").parse().ok()?,
                    Duration::from_millis(300),
                ).is_ok() && verify_hdhomerun(&ip) {
                    Some(ip)
                } else {
                    None
                }
            }));
        }
    }

    for handle in handles {
        if let Ok(Some(ip)) = handle.join() {
            return Some(ip);
        }
    }

    None
}

fn resolve_mdns(hostname: &str) -> Option<String> {
    use std::process::Command;
    let output = Command::new("ping")
        .args(["-c", "1", "-W", "1", hostname])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse "PING hdhomerun.local (192.168.4.204)"
    let start = stdout.find('(')? + 1;
    let end = stdout[start..].find(')')? + start;
    Some(stdout[start..end].to_string())
}

fn verify_hdhomerun(ip: &str) -> bool {
    let url = format!("http://{ip}/discover.json");
    ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .build(),
    )
    .get(&url)
    .call()
    .ok()
    .and_then(|mut resp| {
        let body = resp.body_mut().read_to_string().ok()?;
        Some(body.contains("DeviceID"))
    })
    .unwrap_or(false)
}
