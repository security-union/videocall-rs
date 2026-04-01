# Meeting Recorder: Server-Side Composite Recording

## Purpose

A standalone Rust binary that joins a videocall meeting as an invisible observer,
decodes all participants' audio and video, composites them into a single grid
layout (like a screen recording of the meeting), and writes a playable MP4/WebM
file. No browser required.

## Features

- **Grid layout** — equal-size tiles for all participants, reflows dynamically
  when someone joins or leaves
- **Screen share fullscreen** — when a participant shares their screen, the
  recording switches to show the shared screen full-frame with a small strip
  of participant thumbnails
- **Mixed audio** — all participants' audio decoded and mixed into a single
  stereo track
- **Name labels** — participant names burned into each tile
- **No transcoding on input** — VP9 and Opus decoded directly, only the final
  composited output is encoded (H.264 + AAC for MP4 compatibility)

## Architecture

```
videocall-recorder --meeting 1 --server wss://... --output meeting.mp4

    Observer connection (heartbeat only)
         |
         v
    Packet router (by user_id + media_type)
         |
         +-----------------------------+
         |                             |
    Per-participant:              Per-participant:
    VP9 decoder (libvpx)         Opus decoder
    raw YUV 4:2:0 frames         raw PCM f32 @ 48kHz
         |                             |
         v                             v
    +------------------+        +-------------+
    | Grid Compositor  |        | Audio Mixer |
    |                  |        |             |
    | - compute_grid() |        | - sum all   |
    | - scale + blit   |        |   PCM       |
    | - name labels    |        | - clamp to  |
    | - screen share   |        |   [-1, 1]   |
    |   detection      |        |             |
    +--------+---------+        +------+------+
             |                         |
             v                         v
        Composited YUV            Mixed PCM
        (e.g. 1920x1080           (48kHz stereo)
         @ 30fps)
             |                         |
             +------------+------------+
                          |
                          v
                    ffmpeg (encode)
                    H.264 + AAC → MP4
                    or VP9 + Opus → WebM
```

## Existing Code to Reuse

| Component | Source | Reuse |
|-----------|--------|-------|
| WebSocket/WebTransport clients | `bot/src/websocket_client.rs`, `webtransport_client.rs` | Direct |
| JWT auth | `bot/src/token.rs` | Direct |
| Packet parsing | `videocall-types` protos | Direct |
| RX stats | `bot/src/inbound_stats.rs` | Direct |
| VP9 **encoder** (libvpx) | `bot/src/video_encoder.rs` | Reference for decoder |
| Grid layout math | `dioxus-ui/src/components/attendants.rs` `compute_grid()` | Port to non-WASM |
| Image processing | `image` crate (already a dependency) | Direct |

## New Code Required

### Phase 1: Single-Participant Recording (~1 day)

Build the `videocall-probe` first (see `VIDEOCALL_PROBE.md`). This establishes
the observer connection, packet filtering, and file output pipeline. Phase 2
builds on top of it.

### Phase 2: Multi-Participant Compositor (~2 days)

#### 2a. VP9 Decoder per Participant (~3 hours)

The bot already links `env-libvpx-sys` for encoding. The same library provides
`vpx_codec_dec_init`, `vpx_codec_decode`, `vpx_codec_get_frame` for decoding.

```rust
struct Vp9Decoder {
    codec: vpx_codec_ctx_t,
    width: u32,
    height: u32,
}

impl Vp9Decoder {
    fn new() -> Result<Self>;
    fn decode(&mut self, data: &[u8]) -> Result<Option<YuvFrame>>;
}

struct YuvFrame {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    width: u32,
    height: u32,
}
```

Key considerations:
- Each participant needs their own decoder instance (maintains reference frames)
- Must handle keyframe requests (first frame after join must be a keyframe)
- Packet loss: decoder can skip frames, compositor shows last good frame

#### 2b. Opus Decoder per Participant (~2 hours)

The `opus` crate already supports decoding:

```rust
let decoder = opus::Decoder::new(48000, opus::Channels::Mono)?;
let mut pcm = vec![0f32; 960]; // 20ms at 48kHz
let samples = decoder.decode_float(opus_packet, &mut pcm, false)?;
```

Each participant gets a decoder instance. On packet loss, call
`decoder.decode_float(&[], ...)` for packet loss concealment (PLC).

#### 2c. Grid Compositor (~4 hours)

Operates at a fixed output framerate (e.g. 30fps). Each tick:

1. For each active participant, grab their most recent decoded frame
2. Compute grid layout: `compute_grid(n_participants, output_w, output_h)`
3. Scale each participant's frame to tile size (bilinear or nearest-neighbor)
4. Blit into the output canvas at the correct grid position
5. Optionally burn name label text into each tile
6. Output the composited frame

```rust
struct GridCompositor {
    output_width: u32,
    output_height: u32,
    fps: u32,
    participants: HashMap<String, ParticipantState>,
}

struct ParticipantState {
    display_name: String,
    last_frame: Option<YuvFrame>,
    video_decoder: Vp9Decoder,
    audio_decoder: opus::Decoder,
    audio_buffer: VecDeque<f32>,
}

impl GridCompositor {
    fn add_participant(&mut self, user_id: &str, display_name: &str);
    fn remove_participant(&mut self, user_id: &str);
    fn feed_video(&mut self, user_id: &str, vp9_data: &[u8]);
    fn feed_audio(&mut self, user_id: &str, opus_data: &[u8]);
    fn render_frame(&mut self) -> CompositeFrame;
    fn mix_audio(&mut self, samples: usize) -> Vec<f32>;
}
```

Grid layout algorithm (ported from `compute_grid` in attendants.rs):

```
n=1: 1x1          n=2: 2x1          n=3: 2x2 (one empty)
+----------+      +-----+-----+     +-----+-----+
|          |      |     |     |     |     |     |
|  Alice   |      |Alice| Bob |     |Alice| Bob |
|          |      |     |     |     +-----+-----+
+----------+      +-----+-----+     |Carol|     |
                                    +-----+-----+

n=4: 2x2          n=5-6: 3x2        n=7-9: 3x3
+-----+-----+     +---+---+---+     +---+---+---+
|Alice| Bob |     | A | B | C |     | A | B | C |
+-----+-----+     +---+---+---+     +---+---+---+
|Carol|Dave |     | D | E |   |     | D | E | F |
+-----+-----+     +---+---+---+     +---+---+---+
                                    | G | H |   |
                                    +---+---+---+
```

#### 2d. Screen Share Layout (~2 hours)

When a `MediaType::SCREEN` packet arrives:

```
+----------------------------------+
|                                  |
|        Screen Share              |
|        (full width)              |
|                                  |
+------+------+------+------+-----+
| Alice| Bob  |Carol | Dave | ... |
+------+------+------+------+-----+
        thumbnail strip (~20% height)
```

Detection: `MediaPacket.media_type == MediaType::SCREEN`
Switch back to grid when screen share packets stop (5s timeout).

#### 2e. Audio Mixer (~2 hours)

```rust
fn mix_audio(participants: &mut HashMap<String, ParticipantState>,
             samples_needed: usize) -> Vec<f32> {
    let mut mixed = vec![0.0f32; samples_needed];
    let active_count = participants.values()
        .filter(|p| p.audio_buffer.len() >= samples_needed)
        .count();

    for participant in participants.values_mut() {
        for i in 0..samples_needed {
            if let Some(sample) = participant.audio_buffer.pop_front() {
                mixed[i] += sample;
            }
        }
    }

    // Soft-clamp to prevent distortion with many participants
    let scale = if active_count > 1 { 1.0 / (active_count as f32).sqrt() } else { 1.0 };
    for sample in &mut mixed {
        *sample = (*sample * scale).clamp(-1.0, 1.0);
    }
    mixed
}
```

#### 2f. ffmpeg Output Encoding (~2 hours)

Pipe raw video frames + PCM audio to ffmpeg for final encoding:

```bash
ffmpeg \
  -f rawvideo -pix_fmt yuv420p -s 1920x1080 -r 30 -i pipe:0 \
  -f f32le -ar 48000 -ac 1 -i pipe:1 \
  -c:v libx264 -preset fast -crf 23 \
  -c:a aac -b:a 128k \
  -movflags +faststart \
  output.mp4
```

Or for WebM (no transcoding of audio, VP9 re-encode only for composite):

```bash
ffmpeg \
  -f rawvideo -pix_fmt yuv420p -s 1920x1080 -r 30 -i pipe:0 \
  -f f32le -ar 48000 -ac 1 -i pipe:1 \
  -c:v libvpx-vp9 -crf 30 -b:v 0 \
  -c:a libopus -b:a 128k \
  output.webm
```

#### 2g. Dynamic Join/Leave (~2 hours)

- Track active participants via heartbeat/presence packets
- On join: create new `ParticipantState` with fresh decoders, reflow grid
- On leave: remove participant, reflow grid
- Show last frame for 2s after leave (fade to black optional)
- Grid reflow triggers a brief transition (instant cut is fine for v1)

## CLI Interface

```
videocall-recorder [OPTIONS]

Required:
  --server <URL>          WebSocket or WebTransport server URL
  --meeting <ID>          Meeting/room to record

Optional:
  --output <FILE>         Output file (default: meeting-<ID>-<timestamp>.mp4)
  --resolution <WxH>      Output resolution (default: 1920x1080)
  --fps <N>               Output framerate (default: 30)
  --format <mp4|webm>     Output format (default: mp4)
  --jwt-secret <SECRET>   JWT signing secret (or JWT_SECRET env var)
  --transport <ws|wt>     Transport type (default: websocket)
  --duration <SECONDS>    Auto-stop after N seconds
  --no-labels             Disable participant name labels
  --no-audio              Record video only
```

## Effort Summary

| Phase | Task | Hours |
|-------|------|-------|
| 1 | Single-participant probe (see VIDEOCALL_PROBE.md) | 6 |
| 2a | VP9 decoder per participant | 3 |
| 2b | Opus decoder per participant | 2 |
| 2c | Grid compositor | 4 |
| 2d | Screen share layout | 2 |
| 2e | Audio mixer | 2 |
| 2f | ffmpeg output pipe | 2 |
| 2g | Dynamic join/leave | 2 |
| 2h | Testing + polish | 3 |
| | **Total** | **~26 hours (~3 days)** |

Phase 1 is a prerequisite for Phase 2 and independently useful as a diagnostic
tool.

## System Requirements

- **Rust stable** (same toolchain as the bot)
- **libvpx-dev** (VP9 decode + encode)
- **libopus-dev** (Opus decode)
- **ffmpeg** on PATH (for final encoding)
- **RAM**: ~50MB base + ~5MB per participant (decoded frame buffers)
- **CPU**: VP9 decoding is the bottleneck; 10 participants @ 15fps ≈ 150
  decode ops/sec — manageable on a modern machine

## Future Enhancements

- **Live preview**: pipe composited frames to a local window (SDL2 or similar)
- **Cloud recording**: run as a sidecar in Kubernetes, triggered via API
- **Timestamps + watermarks**: burn meeting time into the recording
- **Speaker highlight**: thicker border on the active speaker's tile
- **Chat overlay**: render chat messages as subtitles
- **Thumbnail generation**: extract keyframes for meeting summaries
- **S3 upload**: auto-upload completed recordings
