# Videocall Synthetic Client Bot

Enhanced bot that streams synthetic audio and video to videocall-rs for load testing and scale validation.

## Features

- **WebTransport Client**: Connects using WebTransport instead of WebSocket
- **Audio Streaming**: Loops WAV files and encodes to Opus (50fps, 20ms packets)
- **Video Streaming**: Cycles through JPEG images and sends mock video packets (30fps)
- **Per-Client Configuration**: Individual audio/video settings per client
- **Linear Ramp-up**: Configurable delay between client starts
- **Multi-Client Support**: Run multiple synthetic clients per container

## Configuration

### Option 1: YAML Configuration File (Recommended)

Create a `config.yaml` file:

```yaml
ramp_up_delay_ms: 1000
server_url: "https://webtransport-us-east.webtransport.video"
clients:
  - user_id: "bot001"
    meeting_id: "test-room"
    enable_audio: true
    enable_video: false
  - user_id: "bot002"
    meeting_id: "test-room"
    enable_audio: false
    enable_video: true
```

Then run:
```bash
BOT_CONFIG_PATH=config.yaml cargo run
```

### Option 2: Environment Variables (Backwards Compatible)

```bash
N_CLIENTS=3
SERVER_URL="https://webtransport-us-east.webtransport.video"
ROOM="test-room"
CLIENT_0_ENABLE_AUDIO=true
CLIENT_0_ENABLE_VIDEO=false
CLIENT_1_ENABLE_AUDIO=false  
CLIENT_1_ENABLE_VIDEO=true
CLIENT_2_ENABLE_AUDIO=true
CLIENT_2_ENABLE_VIDEO=true
cargo run
```

## Assets

The bot includes pre-loaded media assets:

- **Audio**: `BundyBests2.wav` - Looped and encoded to Opus
- **Video**: `output_120.jpg` to `output_124.jpg` - Cycled as mock video frames

## Usage Examples

### Test Audio-Only Clients
```bash
BOT_CONFIG_PATH=config.yaml RUST_LOG=info cargo run
```

### Quick 10-Client Test
```bash
N_CLIENTS=10 ROOM="load-test" cargo run
```

### Docker Build
```bash
docker build -t videocall-synthetic-client .
```

## Media Protocol

- **Audio**: 50fps (20ms Opus packets) following neteq_player.rs pattern
- **Video**: 30fps (~33ms packets) with mock VP9 data
- **Transport**: WebTransport unidirectional streams
- **Format**: videocall-types protobuf MediaPacket

## TODO

- [ ] Add proper VP9 encoding (currently sends mock data)
- [ ] Add Helm chart for Kubernetes deployment  
- [ ] Add metrics collection and Prometheus export
- [ ] Add graceful shutdown handling
- [ ] Add configurable test duration

## Development

Compile and check:
```bash
cargo check
cargo clippy
cargo test
```

Run with debug logging:
```bash
RUST_LOG=debug cargo run
```