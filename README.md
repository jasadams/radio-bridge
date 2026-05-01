# Radio Bridge

A self-contained Rust application that bridges HDHomeRun DVB-T radio channels to Sonos speakers via HLS streaming, with live track metadata and album artwork.

## What It Does

Radio Bridge takes over-the-air radio from an HDHomeRun network TV tuner, transcodes it to AAC, and serves it as HLS streams that Sonos speakers can play. It provides per-song artwork and track information for supported stations (currently Australian ABC/SBS networks).

```
Antenna → HDHomeRun → [Radio Bridge] → HLS → Sonos
                          │
                          ├── MP2 decode (symphonia, pure Rust)
                          ├── AAC-LC encode (fdk-aac, compiled in)
                          ├── MPEG-TS mux (custom, in-memory)
                          ├── ID3v2.4 metadata (track info + artwork)
                          └── Sonos UPnP control
```

**No ffmpeg. No disk I/O. Single static binary. 9.5MB.**

## Why

TuneIn (the default radio service on Sonos) is unreliable for some stations — dropouts, buffering, outages. An HDHomeRun pulling radio directly from a DVB-T antenna is rock solid. This project bridges the two.

## Features

- **On-demand**: Pipelines start when you hit play, stop 30 seconds after the last listener
- **Per-song metadata**: Track title, artist, and album artwork via ID3v2.4 WXXX/TIT2/TPE1 tags in each HLS segment
- **Program fallback**: Shows the current program name (e.g. "House Party") when no track is playing
- **PWA**: Installable web app for your phone — pick a speaker, tap a station
- **Multi-stream**: Each channel runs independently, limited only by HDHomeRun tuner count (typically 4)
- **Extensible**: `MetadataProvider` trait lets you add support for radio networks in other countries
- **Zero disk I/O**: Entire pipeline runs in memory — no wear on SSDs, no temp files
- **Scratch container**: ~9MB Docker image with nothing but the binary and station logos

## Requirements

- **HDHomeRun** network TV tuner (any model that receives radio — FLEX, QUATRO, etc.)
- **Sonos speakers** on the same network
- Linux host (bare metal, VM, or container)

## Quick Start

### From Source

```bash
git clone https://github.com/jasadams/radio-bridge.git
cd radio-bridge
cargo build --release

./target/release/radio-bridge \
  --hdhr-host 192.168.1.100 \
```

### Docker

```bash
docker build -t radio-bridge .
docker run -d \
  -e HDHR_HOST=192.168.1.100 \
  -p 8000:8000 \
  radio-bridge
```

### Kubernetes

See `k8s-example/` for deployment, service, and ingress manifests. Copy to `k8s/` and edit with your values:

```bash
cp -r k8s-example k8s
# Edit k8s/deployment.yaml with your HDHomeRun IP, subnets, etc.
kubectl apply -f k8s/
```

## Configuration

All settings are available as CLI flags or environment variables:

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--hdhr-host` | `HDHR_HOST` | *(required)* | HDHomeRun IP address |
| `--hdhr-port` | `HDHR_PORT` | `5004` | HDHomeRun streaming port |
| `--bitrate` | `BITRATE` | `256k` | AAC encoding bitrate |
| `--port` | `PORT` | `8000` | HTTP server port |
| `--grace-period` | `GRACE_PERIOD` | `30` | Seconds to keep pipeline alive after last listener |
| `--external-host` | `EXTERNAL_HOST` | `localhost:PORT` | Hostname for HLS segment URLs |

## Finding Your HDHomeRun

```bash
# mDNS
ping hdhomerun.local

# Or check your router's device list for "HDHR-XXXXXXXX"

# Verify it's working
curl http://<hdhr-ip>/lineup.json | python3 -m json.tool
```

Radio channels appear with no `VideoCodec` field in the lineup. The channel number (e.g. `28`) is what you use in the HLS URL.

## Network Setup

### Sonos Speaker Discovery

Radio Bridge discovers Sonos speakers via mDNS (multicast DNS), the same protocol Sonos uses natively. If your speakers are on a different VLAN, you need to:

1. Enable mDNS reflection on your router (UniFi: Settings > Services > MDNS)

### EXTERNAL_HOST

The `EXTERNAL_HOST` setting controls the URLs in the HLS playlist. Sonos speakers fetch segments from these URLs, so they must be reachable from the speaker.

- **Direct access** (simplest): Set to `<server-ip>:8000` — Sonos connects directly
- **Behind a reverse proxy**: Set to the proxy hostname (e.g. `radio.example.com`)

If the host contains a port number, URLs use `http://`. Otherwise they use `https://`.

### Kubernetes with LoadBalancer

If running on k8s with a LoadBalancer service, set `externalTrafficPolicy: Local` on the Service to preserve client IPs. This lets the server identify Sonos speakers by their real IP addresses.

## Using the PWA

Open `http://<server>:8000` in a mobile browser. You'll see:

1. **Speaker dropdown** — all discovered Sonos speakers
2. **Station list** — all radio channels from your HDHomeRun (with logos for known stations)

Tap a station to play it on the selected speaker. The app remembers your last speaker choice.

**Install as PWA**: Safari → Share → Add to Home Screen. Chrome → Menu → Install.

## How HLS + Metadata Works

Each HLS segment is a self-contained MPEG-TS file generated entirely in memory:

1. **PAT** — Program Association Table (points to PMT)
2. **PMT** — Program Map Table (declares audio stream + metadata stream with ID3 registration descriptor)
3. **ID3v2.4 metadata PES** — Contains TIT2 (title), TPE1 (artist), WXXX (artwork URL) frames
4. **Audio PES packets** — AAC-LC in ADTS format, with PCR on the first packet

The metadata stream is declared as `stream_type 0x15` (timed metadata) with a registration descriptor containing `"ID3 "`. This is the Apple HLS standard for timed metadata and is supported by Sonos.

Segments are 4 seconds long. The server waits for 3 segments before serving the playlist, giving Sonos a 12-second buffer to start with. 30 segments (60 seconds) are kept in a ring buffer.

## Audio Pipeline

```
HDHomeRun DVB-T stream
  Format: MPEG-TS, MP2 audio, 48000 Hz, stereo, 256 kbps
      │
      ▼
  MPEG-TS demux (PAT → PMT → audio PID discovery)
  PES reassembly → raw MP2 frames
      │
      ▼
  MP2 decode (symphonia-bundle-mp3, pure Rust)
  1152 samples/channel per frame → PCM i16 interleaved
      │
      ▼
  PCM buffer (bridges MP2 1152-sample frames to AAC 1024-sample frames)
      │
      ▼
  AAC-LC encode (fdk-aac, 256k CBR, 48000 Hz, stereo)
  1024 samples/channel → ADTS frame
      │
      ▼
  In-memory MPEG-TS mux + ID3v2.4 injection
  4-second HLS segments → ring buffer → HTTP
```

The sample rate is auto-detected from the first decoded frame, not hardcoded. The PCM buffer is necessary because MP2 and AAC have different frame sizes — without it, 128 samples per channel are lost every frame, causing audible artifacts.

## Adding Support for Other Radio Networks

The `MetadataProvider` trait separates station-specific metadata from the core pipeline:

```rust
pub trait MetadataProvider: Send + Sync + 'static {
    /// Maps a station name (from HDHomeRun lineup) to a metadata API ID
    fn station_id_for(&self, guide_name: &str) -> Option<String>;

    /// Maps a station name to a logo filename (without .png)
    fn logo_key_for(&self, guide_name: &str) -> Option<&'static str>;

    /// Starts a background task that polls for now-playing info
    /// and updates the shared artwork_target and track_target
    fn start_poller(
        &self,
        station_id: &str,
        artwork_target: Arc<RwLock<Option<String>>>,
        track_target: Arc<RwLock<(String, String)>>,
    ) -> tokio::task::JoinHandle<()>;
}
```

The included `AbcProvider` (in `src/providers/abc.rs`) implements this for Australian ABC and SBS stations. To add support for another network:

1. Create `src/providers/your_network.rs`
2. Implement `MetadataProvider`
3. Add logo PNGs to `logos/`
4. Update `src/main.rs` to use your provider (or chain multiple providers)

Without a provider, stations still work — they just won't have track metadata or artwork.

## Architecture

```
radio-bridge/
├── Cargo.toml              # Workspace root
├── Dockerfile              # Scratch container (musl static build)
├── logos/                   # Station logo PNGs (300x300)
├── k8s-example/            # Example Kubernetes manifests
├── crates/
│   └── transcoder/         # Audio transcoding crate
│       ├── src/lib.rs       # TranscoderHandle, AacFrame types
│       ├── src/adts.rs      # ADTS header parser
│       ├── src/ffmpeg.rs    # ffmpeg subprocess backend (alternative)
│       └── src/native.rs   # Native backend (symphonia + fdk-aac)
└── src/
    ├── main.rs              # CLI, config, grace period monitor
    ├── sonos.rs             # Sonos discovery + UPnP play control
    ├── providers/
    │   ├── mod.rs           # MetadataProvider trait
    │   └── abc.rs           # ABC/SBS Australia implementation
    ├── hls/
    │   ├── pipeline.rs      # Wires transcoder → muxer → store
    │   ├── muxer.rs         # In-memory MPEG-TS muxer (PAT/PMT/PES/ID3)
    │   ├── segment_store.rs # Ring buffer + playlist generation
    │   └── id3_inject.rs    # ID3v2.4 tag builder (TIT2/TPE1/WXXX)
    └── web/
        ├── mod.rs           # Axum routes, API endpoints
        └── pwa.html         # PWA frontend
```

The `transcoder` crate has a clean trait interface with two backends:
- **`native`** (default): symphonia MP2 decode + fdk-aac AAC encode — no external dependencies
- **`ffmpeg`**: spawns ffmpeg subprocess, pipes ADTS from stdout — useful as fallback

## Gotchas

### Sonos New App (2024+)

The new Sonos app removed support for custom radio stations, custom service registration (`/customsd`), and doesn't surface UPnP-created favorites. This is why the PWA exists — it's the only way to launch custom streams without a third-party controller app like noson.

### HDHomeRun Radio Channels

Not all HDHomeRun models receive radio. DVB-T models (common in Australia, Europe) typically do. ATSC models (US) may receive FM radio but with different channel numbering. Radio channels in the lineup have no `VideoCodec` field.

### Sample Rate

DVB-T radio is typically 48000 Hz. The transcoder auto-detects this from the first decoded frame. Don't hardcode 44100 Hz — it will cause pitch/speed distortion.

### MP2 vs AAC Frame Sizes

MP2 decodes 1152 samples per channel. AAC encodes 1024 samples per channel. The PCM buffer in the native transcoder bridges this mismatch. Without it, you get robotic/static audio from the 128 dropped samples per frame.

### Sonos and HLS

Sonos supports HLS with AAC-LC in MPEG-TS containers. It does NOT support MP3 in HLS. The timed metadata stream (ID3v2.4) must be declared in the PMT with `stream_type 0x15` and a registration descriptor — without this, Sonos treats the metadata packets as a corrupt audio stream and may refuse to play.

### Multiple Subnets


## License

MIT
