# videocall-probe: Single-Participant Stream Recorder

## Purpose

Diagnostic tool that connects to a videocall meeting as an observer, captures a
single participant's audio and video, and writes it to a playable file (WebM or
MKV). Useful for:

- Isolating whether quality issues (staccato audio, video artifacts) originate
  in the transport/encoding layer or the browser decode pipeline
- Capturing raw server-side output for offline analysis
- Validating bot A/V output without a browser

## Architecture

```
videocall-probe --meeting 1 --user alice \
                --server wss://websocket.example.com \
                --jwt-secret <secret> \
                --output alice-recording.webm

    Observer connection (heartbeat only, no media TX)
         |
         |--- receives all packets, filters for --user
         |
         +-- VP9 frames --> IVF pipe (pipe:0) --\
         |                                       +--> ffmpeg -c copy --> output.webm
         +-- Opus frames -> raw Opus pipe (pipe:1) --/
         |
         +-- logs RX stats (jitter, gaps, sequence, keyframes) to stderr
```

## Existing Code to Reuse

| Component | Source | Notes |
|-----------|--------|-------|
| WebSocket client | `bot/src/websocket_client.rs` | As-is |
| WebTransport client | `bot/src/webtransport_client.rs` | As-is |
| JWT auth | `bot/src/token.rs` | As-is |
| Config + CLI | `bot/src/config.rs` | Extend with clap for probe-specific args |
| Packet parsing | `videocall-types` protos | Already used by bot |
| RX stats | `bot/src/inbound_stats.rs` | Reuse for diagnostics output |
| Transport abstraction | `bot/src/transport.rs` | As-is |

## New Code Required

### 1. Observer-only connection mode (~1 hour)

The bot already supports `enable_audio: false, enable_video: false`. The probe
just needs to:

- Connect and send heartbeats (keep-alive)
- Receive all inbound packets
- Never send media packets

Minimal change to existing bot connection code.

### 2. User filtering + CLI (~1 hour)

```
videocall-probe [OPTIONS]

Required:
  --server <URL>          WebSocket (wss://) or WebTransport (https://) URL
  --meeting <ID>          Meeting/room ID to join
  --user <USER_ID>        Participant to record (or "list" to show active users)

Optional:
  --output <FILE>         Output file (default: recording.webm)
  --jwt-secret <SECRET>   JWT signing secret (or JWT_SECRET env var)
  --transport <ws|wt>     Transport type (default: websocket)
  --duration <SECONDS>    Auto-stop after N seconds
  --video-only            Skip audio, write IVF file directly (no ffmpeg needed)
  --stats-interval <SEC>  RX stats logging interval (default: 10)
```

### 3. IVF writer for VP9 (~30 minutes)

IVF is the trivial raw VP9 container. 32-byte global header + 12-byte per-frame
header. VLC, mpv, and ffmpeg all read it natively.

```rust
struct IvfWriter {
    width: u16,
    height: u16,
    timebase_num: u32,
    timebase_den: u32,
    frame_count: u32,
}

impl IvfWriter {
    fn write_header(&mut self, w: &mut impl Write) -> io::Result<()>;
    fn write_frame(&mut self, w: &mut impl Write, pts: u64, data: &[u8]) -> io::Result<()>;
}
```

This alone enables `--video-only` mode with zero external dependencies.

### 4. ffmpeg pipe integration for A/V (~1-2 hours)

For combined audio+video output:

```rust
// Spawn ffmpeg with two pipe inputs
let mut ffmpeg = Command::new("ffmpeg")
    .args(&[
        "-f", "ivf", "-i", "pipe:3",        // VP9 video
        "-f", "opus", "-i", "pipe:4",        // Opus audio
        "-c:v", "copy",                       // passthrough (no re-encode)
        "-c:a", "copy",                       // passthrough (no re-encode)
        "-f", "webm", &output_path,
    ])
    .stdin(Stdio::null())
    .spawn()?;
```

Since both VP9 and Opus are WebM-native codecs, ffmpeg can mux them with
`-c copy` (no transcoding) — very fast and lossless.

### 5. Graceful shutdown (~30 minutes)

On Ctrl+C:
- Stop receiving packets
- Flush any buffered frames to ffmpeg
- Close ffmpeg stdin pipes (triggers ffmpeg to finalize the container)
- Print summary stats (total frames, duration, average jitter, gaps)

## Estimated Effort

| Task | Hours |
|------|-------|
| Observer connection mode | 1 |
| CLI + user filtering | 1 |
| IVF writer | 0.5 |
| ffmpeg pipe integration | 1.5 |
| Testing + polish | 1.5 |
| **Total** | **~6 hours** |

## Output Formats

| Mode | Container | Codec | Requires ffmpeg |
|------|-----------|-------|-----------------|
| `--video-only` | IVF | VP9 | No |
| Default | WebM | VP9 + Opus | Yes |
| `--output *.mkv` | Matroska | VP9 + Opus | Yes |

## Example Usage

```bash
# Record alice's stream from meeting 1
videocall-probe --server wss://websocket.conceptcar7.com \
                --meeting 1 --user alice \
                --jwt-secret "$JWT_SECRET" \
                --output alice.webm

# Video-only (no ffmpeg needed) for quick diagnostics
videocall-probe --server wss://websocket.conceptcar7.com \
                --meeting 1 --user alice --video-only \
                --output alice.ivf

# List active participants
videocall-probe --server wss://websocket.conceptcar7.com \
                --meeting 1 --user list \
                --jwt-secret "$JWT_SECRET"

# Record for 60 seconds then stop
videocall-probe --server wss://websocket.conceptcar7.com \
                --meeting 1 --user bob \
                --jwt-secret "$JWT_SECRET" \
                --duration 60
```

## Dependencies

- Rust crates: reuses existing bot dependencies (no new crates)
- Optional: `ffmpeg` on PATH (only for combined A/V output)

## Future Enhancements

- Record multiple users to separate files simultaneously
- Add timestamp overlay for A/V sync verification
- Stream to stdout for piping to other tools (`| vlc -`)
- Feed into the meeting recorder (see MEETING_RECORDER.md)
