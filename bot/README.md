# Videocall Synthetic Client Bot

Synthetic bot that streams real VP9 video and Opus audio to videocall-rs meetings over WebSocket or WebTransport. Simulates up to 20 participants in a scripted conversation with synchronized EKG waveform video. Useful for load testing, scale validation, and call quality measurement.

## Features

- **Dynamic N-participant mode**: generate once for 20 people, `--users N` at runtime
- **Dual transport**: WebSocket (`wss://`) and WebTransport (`https://`)
- **On-the-fly EKG video**: 1280x720 waveform rendered per-frame in Rust (< 1ms), no pre-generation
- **Real VP9 encoding**: libvpx at 15fps with forced keyframes at loop restart and every 5s
- **Opus audio**: 48kHz mono, 20ms packets (50fps), monotonic sequence numbers
- **Microsecond A/V sync**: shared `Instant` clock with µs precision prevents progressive drift
- **RX quality diagnostics**: per-sender jitter, sequence gaps, A/V sync delta every 10s
- **JWT authentication**: mints per-client JWTs when `jwt_secret` is configured
- **Loop boundary recovery**: forces VP9 keyframes at loop restart so video recovers instantly

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

### 2. Configure

Copy the template and fill in your server details:

```bash
cp config.yaml.template config-myenv.yaml
```

```yaml
transport: "websocket"                          # or "webtransport"
server_url: "wss://websocket.example.com"       # wss:// for WS, https:// for WT
meeting_id: "1"
conversation_dir: "conversation"
ramp_up_delay_ms: 500
# jwt_secret: "your-base64-secret-here"         # or set JWT_SECRET env var
# insecure: true                                # skip TLS verify (WT only)
```

### 3. Build & Run

```bash
cargo build --release -p bot
```

Run with any number of participants (1–20):

```bash
RUST_LOG=info ./target/release/bot --config config-myenv.yaml --users 5
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

- **Audio**: 48kHz Opus, 20ms packets (50fps), monotonic sequence numbers
- **Video**: VP9 Profile 0, 1280x720 @ 15fps, keyframes forced at loop boundaries and every 5s
- **Wire format**: Protobuf `PacketWrapper` → `MediaPacket` (same as browser client)
- **Heartbeat**: 1Hz protobuf heartbeat via the same packet channel

## Configuration Reference

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `transport` | no | `"webtransport"` | `"websocket"` or `"webtransport"` |
| `server_url` | yes | — | Server URL (`wss://` for WS, `https://` for WT) |
| `meeting_id` | yes | — | Room to join |
| `conversation_dir` | no | `"conversation"` | Path to manifest + line WAVs |
| `jwt_secret` | no | — | HMAC secret for JWT auth (or `JWT_SECRET` env var) |
| `token_ttl_secs` | no | `86400` | JWT token lifetime in seconds |
| `ramp_up_delay_ms` | no | `1000` | Delay between starting each client |
| `insecure` | no | `false` | Skip TLS certificate verification (WT only) |

CLI arguments:

| Flag | Description |
|------|-------------|
| `--config <path>` | Path to config YAML (or `BOT_CONFIG_PATH` env var) |
| `--users <N>` / `-n <N>` | Number of participants to simulate (default: all in manifest) |

## Architecture

```
main.rs
  ├── Reads manifest, takes first N participants
  ├── Filters lines to active speakers, stitches per-participant audio
  ├── Shared media clock (Instant) + loop_duration (from stitched timeline)
  └── Per participant:
        ├── transport.rs → websocket_client.rs / webtransport_client.rs
        ├── audio_producer.rs  (OS thread, Opus encoding, 50fps)
        ├── video_producer.rs  (OS thread, EKG render → VP9 encoding, 15fps)
        ├── heartbeat producer (1Hz, via shared mpsc channel)
        └── inbound_stats.rs   (per-sender RX quality diagnostics)
```

Both audio and video producers run on OS threads (not tokio tasks) to avoid scheduler starvation under CPU-bound VP9/Opus encoding. They derive their position from the same `Instant` epoch and wrap at the same `loop_duration`, preventing drift. Frame buffers (ImageBuffer + I420) are pre-allocated and reused to minimize heap churn at scale.

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
