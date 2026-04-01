# Videocall Synthetic Client Bot

Synthetic bot that streams real VP9 video and Opus audio to videocall-rs meetings over WebSocket or WebTransport. Useful for load testing, scale validation, and call quality measurement.

## Features

- **Dual Transport**: WebSocket (`wss://`) and WebTransport (`https://`) with per-config selection
- **Real VP9 Video**: Encodes JPEG image sequences to VP9 at 15fps (1280x720)
- **Opus Audio**: Encodes WAV files to Opus at 48kHz (50 packets/sec, 20ms frames)
- **Conversation Mode**: Two bots with interleaved TTS dialogue and synchronized EKG video
- **A/V Sync**: Shared media clock ensures audio and video stay aligned across loop boundaries
- **RX Quality Diagnostics**: Per-bot jitter, sequence gaps, A/V sync delta, and keyframe stats every 10s
- **JWT Authentication**: Mints per-client JWTs when `jwt_secret` is configured
- **Multi-Client Support**: Run multiple bots per process with configurable ramp-up delay
- **Loop Boundary Recovery**: Forces VP9 keyframes at loop restart so video recovers instantly

## Prerequisites

### System packages (Ubuntu/Debian)

```bash
sudo apt-get install -y libopus-dev libvpx-dev nasm pkg-config build-essential
```

- `libopus-dev` — Opus audio encoding
- `libvpx-dev` + `nasm` — VP9 video encoding (used by `env-libvpx-sys`)
- `pkg-config`, `build-essential` — standard Rust build tooling

### Rust

```bash
rustup update stable
```

The bot builds with `cargo build --release -p bot`.

## Quick Start

### 1. Generate conversation assets (optional)

This creates interleaved WAV files and EKG video frames for two bots using Piper TTS.

**Install Python dependencies:**

```bash
pip install piper-tts scipy pillow numpy
```

The `piper-tts` package pulls in `onnxruntime` automatically. On some systems you may also need:

```bash
sudo apt-get install -y libonnxruntime-dev
```

**Download Piper voice models** from Hugging Face (one-time, ~120 MB total):

```bash
mkdir -p voices
HF_BASE="https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/en/en_US"
# Amy (female) - alice's voice
curl -L -o voices/amy-medium.onnx      "$HF_BASE/amy/medium/en_US-amy-medium.onnx"
curl -L -o voices/amy-medium.onnx.json "$HF_BASE/amy/medium/en_US-amy-medium.onnx.json"
# Joe (male) - bob's voice
curl -L -o voices/joe-medium.onnx      "$HF_BASE/joe/medium/en_US-joe-medium.onnx"
curl -L -o voices/joe-medium.onnx.json "$HF_BASE/joe/medium/en_US-joe-medium.onnx.json"
```

**Generate the conversation:**

```bash
python3 generate-conversation.py
```

Produces:
- `conversation/conversation-alice.wav` / `conversation-bob.wav` (48kHz mono)
- `conversation/frames-alice/frame_NNNNN.jpg` / `frames-bob/` (1280x720 @ 15fps)

The conversation text is hardcoded in `generate-conversation.py` — edit the `CONVERSATION` list to customize.

### 2. Configure

Create a `config.yaml`:

```yaml
transport: "websocket"   # or "webtransport"
server_url: "wss://websocket.example.com"   # wss:// for websocket, https:// for webtransport
jwt_secret: "your-base64-secret"
ramp_up_delay_ms: 0

clients:
  - user_id: "alice"
    meeting_id: "1"
    enable_audio: true
    enable_video: true
    audio_file: "conversation/conversation-alice.wav"
    image_dir: "conversation/frames-alice"

  - user_id: "bob"
    meeting_id: "1"
    enable_audio: true
    enable_video: true
    audio_file: "conversation/conversation-bob.wav"
    image_dir: "conversation/frames-bob"
```

If `audio_file` is omitted, defaults to `BundyBests2.wav`. If `image_dir` is omitted, defaults to the current directory (legacy `output_120..124.jpg` pattern).

### 3. Build & Run

```bash
cargo build --release -p bot
```

Run with a config file (recommended):

```bash
RUST_LOG=info ./target/release/bot --config config.yaml
```

The config path can also be set via env var:

```bash
RUST_LOG=info BOT_CONFIG_PATH=config.yaml ./target/release/bot
```

Without a config file, the bot falls back to environment variables (see Configuration Reference).

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
- **Video**: VP9 Profile 0, 1280x720 @ 15fps, keyframes forced at loop boundaries
- **Wire format**: Protobuf `PacketWrapper` → `MediaPacket` (same as browser client)
- **Heartbeat**: 1Hz protobuf heartbeat via the same packet channel

## Configuration Reference

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `transport` | no | `"webtransport"` | `"websocket"` or `"webtransport"` |
| `server_url` | yes | — | Server URL (`wss://` for websocket, `https://` for webtransport) |
| `jwt_secret` | no | — | Base64-encoded HMAC secret for JWT auth |
| `ramp_up_delay_ms` | no | `1000` | Delay between starting each client |
| `insecure` | no | `false` | Skip TLS certificate verification |
| `clients[].user_id` | yes | — | Bot display name and identity |
| `clients[].meeting_id` | yes | — | Room to join |
| `clients[].enable_audio` | no | `false` | Stream audio |
| `clients[].enable_video` | no | `false` | Stream video |
| `clients[].audio_file` | no | `"BundyBests2.wav"` | Path to WAV file (48kHz recommended) |
| `clients[].image_dir` | no | `"."` | Directory containing `frame_NNNNN.jpg` files |

## Architecture

```
main.rs
  ├── Shared media clock (Instant) + loop duration (from WAV length)
  ├── Per-client:
  │     ├── transport.rs → websocket_client.rs / webtransport_client.rs
  │     ├── audio_producer.rs  (tokio task, Opus encoding)
  │     ├── video_producer.rs  (OS thread, JPEG decode → VP9 encoding)
  │     ├── heartbeat producer (1Hz, via shared mpsc channel)
  │     └── inbound_stats.rs   (RX quality diagnostics)
  └── mpsc channel (500 slots) connects producers → transport sender
```

Both audio and video producers derive their position from the same `Instant` epoch and wrap at the same `loop_duration`, preventing drift. Video uses a global monotonic PTS (never wraps) for VP9 encoding while selecting source frames by loop-relative position.

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
| Encode | Pre-encoded VP9/Opus, minimal CPU | `getUserMedia` → WebCodecs/libvpx |
| Decode | **None** — only timestamps arrivals | VP9/Opus decode → render pipeline |
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

## Future: Headless Chrome Browser-Level Testing

The bot measures transport and relay behavior in isolation. For true end-to-end quality metrics through the full browser stack (WASM decode, jitter buffer, canvas rendering), a Playwright + headless Chrome approach is feasible.

### How it would work

1. **Playwright launches headless Chrome instances** with fake media devices:
   ```
   --use-fake-device-for-media-stream
   --use-file-for-fake-video-capture=test-pattern.y4m
   --use-file-for-fake-audio-capture=test-tone.wav
   ```
2. Each instance navigates to the Dioxus UI and joins a meeting (JWT injection already works in the e2e harness)
3. The WASM client exposes internal metrics to JavaScript via `wasm-bindgen`
4. Playwright collects metrics periodically via `page.evaluate(() => window.getVideoCallStats())`

### Instrumentation points in videocall-client

The WASM client already tracks most of the data internally — it just needs to be surfaced to JS:

| Component | File | What to expose |
|-----------|------|----------------|
| **Connection** | `connection/connection_manager.rs` | `packets_received`, `packets_sent`, RTT measurements |
| **Video decode** | `decode/peer_decoder.rs` | Decode latency (WebCodecs `VideoDecoder` → canvas render), keyframe wait time, decode error count |
| **Audio decode** | `decode/neteq_audio_decoder.rs` | Jitter buffer depth, playout delay, concealment events (silence insertion), buffer underruns |
| **Bitrate controller** | `diagnostics/encoder_bitrate_controller.rs` | Encode FPS jitter (already calculated), target vs actual bitrate, PID adjustments |
| **Per-peer stats** | `decode/peer_decode_manager.rs` | Per-sender packet counts, last-received timestamps, A/V sync delta |

### Implementation sketch

```rust
// In videocall-client, add a stats export:
#[wasm_bindgen]
pub fn get_videocall_stats() -> JsValue {
    // Collect from ConnectionManager, PeerDecodeManager, NetEqAudioPeerDecoder
    // Return as JSON-serializable JsValue
}
```

```typescript
// In Playwright test:
const stats = await page.evaluate(() => (window as any).getVideoCallStats());
// stats.audio_jitter_buffer_ms, stats.video_decode_latency_ms, etc.
```

### What this would add over the native bot

| Metric | Native bot | Headless Chrome |
|--------|-----------|-----------------|
| Transport jitter | Yes | Yes |
| Decode latency | No | Yes |
| Jitter buffer behavior | No | Yes (NetEQ concealment, buffer depth) |
| Render-to-display time | No | Yes (canvas draw timing) |
| WASM overhead | No | Yes (real wasm32 execution) |
| Encode pipeline | No | Yes (getUserMedia → WebCodecs → send) |
| GC/event loop stalls | No | Yes (real browser runtime) |

### Prerequisites

- Existing e2e infrastructure (`docker/docker-compose.e2e.yaml`, Playwright config, JWT auth bypass)
- Chrome flag support in Playwright for fake media devices
- A `wasm-bindgen`-exported stats function in `videocall-client`

The existing Playwright e2e harness handles browser launch, JWT injection, and meeting join. The main new work is the `wasm-bindgen` stats export and a Playwright test that collects and reports the metrics over a timed run.

## Development

```bash
cargo check -p bot
cargo clippy -p bot
RUST_LOG=debug ./target/release/bot --config config.yaml
```
