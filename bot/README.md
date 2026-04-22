# Videocall Synthetic Client Bot

Synthetic bot that streams real VP9 video and Opus audio to videocall-rs meetings over WebSocket or WebTransport. Simulates up to 50 participants with costume video (recorded Google Meet costume filter clips) or EKG waveforms. Supports broadcaster/observer split for webinar-style load testing.

## Features

- **Costume video**: pre-recorded video clips (idle + talking) driven by audio RMS, VP9 at 30fps/1000kbps — realistic webcam-like compression load
- **EKG fallback**: animated waveform video for participants without costumes, 15fps/500kbps
- **DTX silence suppression**: silent audio packets are skipped, matching real client behavior
- **VAD heartbeat**: `is_speaking` flag updated from audio energy, reflected in heartbeats
- **Rich health packets**: quality scores, concealment stats, decoder metrics — visible in Prometheus/Grafana
- **Dual transport**: `ws_url` + `wt_url` with configurable `wt_ratio` split (0.0–1.0)
- **Broadcaster/observer mode**: first N participants send A/V, rest are receive-only observers
- **Warmup period**: configurable silence before conversation starts (one-time, no gap on loop)
- **50-participant manifest**: 20 named characters + 30 observer slots
- **JWT authentication**: mints per-client JWTs when `jwt_secret` is configured

## Prerequisites

### System packages (Ubuntu/Debian)

```bash
sudo apt-get install -y libopus-dev libvpx-dev nasm pkg-config build-essential
```

- `libopus-dev` — Opus audio encoding
- `libvpx-dev` + `nasm` — VP9 video encoding (used by `env-libvpx-sys`)
- `pkg-config`, `build-essential` — standard Rust build tooling

### Python (for conversation generation)

```bash
pip install edge-tts numpy scipy pyyaml
sudo apt install ffmpeg
```

## Quick Start

### 1. Generate conversation assets

Uses Microsoft Edge TTS neural voices to create per-line WAV clips and a manifest:

```bash
python3 generate-conversation-edge.py
```

Produces:
- `conversation/manifest.yaml` — participant roster + line metadata
- `conversation/lines/line_NNN.wav` — individual speech clips (48kHz mono)

The conversation text and participant list are in `generate-conversation-edge.py` — edit to customize.

### 2. Create costume video clips (optional)

For realistic video load, record costume clips using Google Meet's "Background and effects" costume filters:

1. Join a Google Meet call alone, apply a costume filter (pirate, cat, robot, etc.)
2. Record two clips per character using OBS or screen capture (crop to just the video tile):
   - `<name>-silent.mp4` — 8-10 seconds, no talking, natural idle movement
   - `<name>-talking.mp4` — 8-10 seconds, counting "one, two, three..." with mouth movement
3. Normalize to I420 frames:
   ```bash
   mkdir -p assets/costumes/pirate
   ffmpeg -y -i pirate-silent.mp4 -vf "scale=1280:720,fps=30" -pix_fmt yuv420p -f rawvideo assets/costumes/pirate/idle.i420
   ffmpeg -y -i pirate-talking.mp4 -vf "scale=1280:720,fps=30" -pix_fmt yuv420p -f rawvideo assets/costumes/pirate/talking.i420
   ```
4. Assign costumes to participants in `conversation/manifest.yaml`:
   ```yaml
   - name: alice
     voice: en-US-AvaNeural
     ekg_color: [0, 200, 220]
     costume_dir: assets/costumes/pirate
   ```

Each costume pair is ~800 MiB of I420 frames (gitignored). Save the raw MP4s (~2 MB each) for regeneration. The I420 files can always be recreated from the MP4s with the ffmpeg command above.

### 3. Configure

```yaml
# Transport — set one or both URLs
ws_url: "wss://websocket.example.com"
wt_url: "https://webtransport.example.com:443"
wt_ratio: 0.0                     # fraction on WebTransport (0.0–1.0, see note below)

# Or legacy single-transport
# transport: "websocket"
# server_url: "wss://websocket.example.com"

meeting_id: "1"
conversation_dir: "conversation"
video_mode: costume               # "costume" or "ekg"
broadcasters: 5                   # first 5 send A/V, rest observe (0 = all broadcast)
warmup_secs: 15                   # silence before conversation starts
ramp_up_delay_ms: 500
jwt_secret: "your-base64-secret"
token_ttl_secs: 86400
```

> **WebTransport limitation:** Bot WT clients currently send data but **do not receive
> inbound streams** due to a lost-waker bug in `web-transport-quinn` v0.8.1's
> `accept_uni` implementation (`FuturesUnordered` + single waker under concurrent
> callers). This was fixed upstream in v0.11.8 but the project hasn't upgraded yet.
> **Use `wt_ratio: 0.0` (all WebSocket) for load testing** until `web-transport-quinn`
> is upgraded. Set `wt_ratio` > 0 only for testing WT connectivity and outbound encoding.

### 4. Build & Run

```bash
cargo build --release -p bot
```

```bash
# 20 participants, all broadcasting with costumes
RUST_LOG=info ./target/release/bot --config config.yaml --users 20

# 50-person webinar: 5 broadcasters + 45 observers
RUST_LOG=info ./target/release/bot --config config.yaml --users 50
# (requires 50 participants in manifest — 20 named + 30 observer-NN entries)
```

### Static-linked build for remote deployment

By default, `cargo build` dynamically links libvpx. If you copy the binary to a
remote machine that doesn't have libvpx installed, you'll get a "shared library
not found" error. To statically link libvpx into the binary:

```bash
VPX_LIB_DIR=/usr/lib/x86_64-linux-gnu \
VPX_INCLUDE_DIR=/usr/include \
VPX_VERSION=1.11.0 \
VPX_STATIC=1 \
cargo build --release -p bot
```

The resulting binary embeds libvpx and can be copied to any Linux x86_64 machine
without installing libvpx-dev on the target. Other dependencies (libc, libopus,
libssl) are still dynamically linked — install `libopus0` and `libssl3` on the
target if needed.

## RX Quality Diagnostics

Every 10 seconds each bot logs a stats line:

```
[alice] RX STATS (10s): audio=500 pkts (40 KB, jitter=3.8ms, gaps=0), video=151 pkts (1 key, 162 KB, jitter=5.2ms, gaps=0), heartbeat=30, A/V sync=34ms, errors=0
```

| Metric | Excellent | Acceptable | Poor |
|--------|-----------|------------|------|
| Audio jitter | <10ms | 10-30ms | >50ms |
| Video jitter | <20ms | 20-50ms | >80ms |
| Audio gaps/10s | 0 | <10 | >50 |
| Video gaps/10s | 0 | <5 | >20 |
| A/V sync | <30ms | 30-80ms | >150ms |

## Media Protocol

- **Audio**: 48kHz Opus mono, 20ms packets (50fps), DTX silence suppression (RMS < 0.005)
- **Video (costume)**: VP9 Profile 0, 1280x720 @ 30fps, 1000kbps target, pre-recorded I420 frames
- **Video (EKG)**: VP9 Profile 0, 1280x720 @ 15fps, 500kbps target, rendered on-the-fly
- **Health**: HealthPacket every 1s with per-peer quality scores, FPS, bitrate, concealment
- **Heartbeat**: every 5s with VAD is_speaking flag
- **Wire format**: Protobuf `PacketWrapper` → `MediaPacket` (same as browser client)

## Remote Deployment

The bot binary dynamically links `libvpx.so.7`. Bundle it for machines without libvpx:

```bash
mkdir bot-deploy
cp target/release/bot bot-deploy/
strip bot-deploy/bot
cp /lib/x86_64-linux-gnu/libvpx.so.7 bot-deploy/
cp -r conversation bot-deploy/
cp -r assets bot-deploy/          # ~15 GB costume frames
cp config.yaml bot-deploy/

# Launcher script (sets LD_LIBRARY_PATH and RUST_LOG)
cat > bot-deploy/run.sh << 'EOF'
#!/bin/bash
DIR="$(cd "$(dirname "$0")" && pwd)"
export LD_LIBRARY_PATH="$DIR:$LD_LIBRARY_PATH"
export RUST_LOG="${RUST_LOG:-info}"
CONFIG="${1:-config.yaml}"
shift 2>/dev/null || true
exec "$DIR/bot" --config "$DIR/$CONFIG" "$@"
EOF
chmod +x bot-deploy/run.sh

rsync -avz --progress bot-deploy/ user@remote:bot/
```

On the remote machine:
```bash
./run.sh config.yaml --users 20 2>&1 | tee bot.log
```

## Configuration Reference

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `ws_url` | * | — | WebSocket relay URL (`wss://...`) |
| `wt_url` | * | — | WebTransport relay URL (`https://...:443`) |
| `wt_ratio` | no | `0.66` | Fraction of bots on WebTransport (0.0–1.0) |
| `transport` | legacy | `"webtransport"` | Legacy: `"websocket"` or `"webtransport"` |
| `server_url` | legacy | — | Legacy: single server URL |
| `meeting_id` | yes | — | Room to join |
| `conversation_dir` | no | `"conversation"` | Path to manifest + line WAVs |
| `video_mode` | no | `"ekg"` | `"costume"` (recorded clips) or `"ekg"` (waveform) |
| `broadcasters` | no | `0` | First N send A/V, rest observe (0 = all broadcast) |
| `warmup_secs` | no | `15` | Seconds of silence before conversation starts |
| `jwt_secret` | no | — | HMAC secret for JWT auth |
| `token_ttl_secs` | no | `86400` | JWT token lifetime in seconds |
| `ramp_up_delay_ms` | no | `1000` | Delay between starting each client |
| `insecure` | no | `false` | Skip TLS cert verification (WT only) |

\* At least one of `ws_url` / `wt_url` required, or use legacy `transport` + `server_url`.

CLI arguments:

| Flag | Description |
|------|-------------|
| `--config <path>` | Path to config YAML (or `BOT_CONFIG_PATH` env var) |
| `--users <N>` / `-n <N>` | Number of participants (default: all in manifest, max 50) |

## Architecture

```
main.rs
  ├── Reads manifest, determines broadcaster/observer split
  ├── Filters lines to broadcaster speakers, stitches audio
  ├── Spawns all clients (connect + heartbeat + health immediately)
  ├── Warmup sleep, then sets shared media_start via OnceCell
  └── Per participant:
        ├── transport.rs → websocket_client.rs / webtransport_client.rs
        ├── health_reporter.rs  (tokio task, 1Hz HealthPackets)
        ├── heartbeat producer  (5s keepalive, VAD is_speaking)
        ├── inbound_stats.rs    (per-sender RX quality diagnostics)
        └── [broadcasters only]:
              ├── audio_producer.rs     (OS thread, Opus + DTX, 50fps)
              ├── video_producer.rs     (OS thread, VP9 encoding)
              └── costume_renderer.rs   (I420 frame selection by RMS)
```

Audio and video producers run on OS threads (not tokio tasks) to avoid scheduler starvation under CPU-bound VP9/Opus encoding. They derive their position from a shared `Instant` epoch (set after warmup) and wrap at `loop_duration`, preventing drift.

## What This Bot Measures (and What It Doesn't)

The bot is a **relay and transport diagnostic tool**, not an end-to-end quality benchmark.

### What it measures

- **Relay forwarding performance**: how quickly the server fans packets between participants
- **Transport protocol differences**: TCP (WebSocket) vs QUIC/UDP (WebTransport) at the wire level
- **Network path characteristics**: jitter, reordering, and loss on the bot → relay → bot path
- **Server-side bugs**: e.g., incorrect packet routing, missing fields, relay regressions

### What it does NOT measure

The bot is a **native Rust binary** — it bypasses the entire browser stack that real users experience:

| Layer | Bot | Browser client |
|-------|-----|----------------|
| WebTransport API | Native `quinn` (QUIC library) | Browser WebTransport API |
| WebSocket API | `tokio-tungstenite` | Browser WebSocket API |
| Execution | Native x86_64, tokio async runtime | WASM in browser sandbox |
| Encode | On-the-fly EKG → VP9/Opus | `getUserMedia` → WebCodecs/libvpx |
| Decode | **None** — only tracks arrival timestamps | VP9/Opus decode → render pipeline |
| Jitter buffer | **None** — raw packet arrival analysis | Client-side reorder + playout buffer |
| Rendering | **None** | Canvas/WebGL + audio playout |
| GC / event loop | None (Rust, no GC) | Browser GC pauses, event loop contention |

Because the bot skips decode, jitter buffering, and rendering, its jitter and gap numbers reflect **transport-level behavior only**. In the browser:

- Audio "gaps" from UDP reordering would be absorbed by the jitter buffer and never heard
- Jitter numbers would be higher due to WASM overhead, GC pauses, and decode time
- A/V sync would include decode + render latency, not just packet arrival delta
- The relative WT-vs-WS difference might be dwarfed by client-side overhead

### When to use this bot

- Validating relay correctness after server changes
- Comparing transport protocol behavior in isolation
- Load testing (multiple bots to stress-test the relay)
- Smoke-testing a deployment (do packets flow at all?)

### When you need browser-level testing instead

- Measuring real user-perceived quality (MOS, end-to-end latency)
- Testing codec performance under browser constraints
- Evaluating jitter buffer effectiveness
- Benchmarking WASM client decode/render pipeline

## Development

```bash
cargo check -p bot
cargo clippy -p bot
RUST_LOG=debug ./target/release/bot --config config-myenv.yaml --users 2
```
