# `videocall-codecs`: Jitter Buffer & Decoder

<a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT" height="28"></a>
<a href="https://discord.gg/JP38NRe4CJ"><img src="https://img.shields.io/badge/Discord-Join%20Chat-7289DA?logo=discord&logoColor=white" alt="Discord" height="28"></a> 
<a href="https://www.digitalocean.com/?refcode=6de4e19c5193&utm_campaign=Referral_Invite&utm_medium=Referral_Program&utm_source=badge"><img src="https://web-platforms.sfo2.cdn.digitaloceanspaces.com/WWW/Badge%201.svg" alt="DigitalOcean Referral Badge" height="28"></a>

This crate is a core component of the **[videocall.rs](https://github.com/security-union/videocall-rs)** project. It provides a high-fidelity, cross-platform video decoder and jitter buffer, implemented in pure Rust.

## Vision

Currently focused on high-quality video decoding and jitter buffering, `videocall-codecs` is expanding to become a comprehensive multimedia codec solution. **Audio support is coming soon** - we're actively working on integrating audio codecs (Opus, AAC) alongside our existing video capabilities to provide a unified, cross-platform audio/video processing pipeline.

Our roadmap includes:
- **Audio Codec Integration**: Opus and AAC support with the same cross-platform design
- **Unified Jitter Buffer**: Combined audio/video synchronization
- **Enhanced Web Support**: Audio processing in Web Workers alongside video
- **Real-time Audio Processing**: Low-latency audio decoding optimized for live streaming

## Features

- **Cross-platform**: Works on native (libvpx) and WASM (WebCodecs) targets
- **Built-in Jitter Buffer**: Automatic frame reordering, gap detection, and adaptive playout delay
- **Ergonomic API**: Simple `push_frame()` interface hides complexity
- **VP9 Codec Support**: High-quality video compression
- **Real-time Optimized**: Designed for low-latency video streaming
- **Web-ready**: Easy WASM integration with automatic Web Worker setup

## Quick Start

### Native Usage

```rust
use videocall_codecs::{
    decoder::{Decoder, VideoCodec},
    frame::{FrameBuffer, VideoFrame, FrameType}
};

let decoder = Decoder::new(
    VideoCodec::VP9,
    Box::new(|decoded_frame| {
        println!("Decoded frame: {}x{}", decoded_frame.width, decoded_frame.height);
    })
);

let video_frame = VideoFrame {
    sequence_number: 1,
    frame_type: FrameType::KeyFrame,
    data: encoded_data,
    timestamp: 0.0,
};

decoder.decode(FrameBuffer::new(video_frame, current_time_ms));
```

### Web/WASM Usage

```bash
# 1. Build for web  
wasm-pack build --target web --features wasm

# 2. Install in your project
npm install ./pkg
```

```javascript
// 3. Use in JavaScript
import init, { WasmDecoder, VideoCodec } from './pkg/videocall_codecs.js';

await init();

const canvas = document.getElementById('video-canvas');
const ctx = canvas.getContext('2d');

const decoder = new WasmDecoder(
    VideoCodec.VP9,
    (videoFrame) => {
        ctx.drawImage(videoFrame, 0, 0, canvas.width, canvas.height);
        videoFrame.close(); // Important: release memory
    }
);

// Push frames from your video stream
decoder.push_frame({
    sequence_number: 1,
    frame_type: 'KeyFrame', // or 'DeltaFrame'
    data: frameData, // Uint8Array of VP9 data
    timestamp: Date.now()
});
```

## Web Worker Setup (WASM)

For WASM builds, the decoder runs in a Web Worker for better performance. Add this to your `index.html`:

```html
<!-- Compile the worker -->
<link
    data-trunk
    rel="rust"
    href="../videocall-codecs/Cargo.toml"
    data-bin="worker_decoder"
    data-type="worker"
    data-cargo-features="wasm"
    data-cargo-no-default-features
    data-loader-shim
/> 

<!-- Runtime link for decoder -->
<link id="codecs-worker" href="/worker_decoder_loader.js" />
```

## Architecture

### Jitter Buffer

The built-in jitter buffer automatically handles:

- **Frame Reordering**: Out-of-order frames are buffered and played in sequence
- **Gap Recovery**: Jumps to keyframes when frames are lost
- **Adaptive Delay**: Adjusts playout delay based on network jitter
- **Buffer Management**: Prevents buffer overflow with configurable limits

### Cross-Platform Design

```
┌─────────────────┐    ┌───────────────────┐    ┌─────────────────┐
│   Your App      │    │  videocall-codecs │    │   Platform      │
│                 │    │                   │    │                 │
│ decoder.decode()│───▶│  JitterBuffer     │───▶│ Native: libvpx  │
│                 │    │  FrameReordering  │    │ WASM: WebCodecs │
│                 │    │  AdaptiveDelay    │    │                 │
└─────────────────┘    └───────────────────┘    └─────────────────┘
```

The crate implements a trait-based abstraction that allows the same jitter buffer logic to work across both native and WASM targets, with platform-specific decoder implementations handling the actual video decoding.

## API Reference

### Core Types

```rust
pub struct VideoFrame {
    pub sequence_number: u64,
    pub frame_type: FrameType, // KeyFrame or DeltaFrame
    pub data: Vec<u8>,
    pub timestamp: f64,
}

pub struct FrameBuffer {
    pub frame: VideoFrame,
    pub arrival_time_ms: u128,
}
```

### Platform APIs

**Native:**
```rust
let decoder = Decoder::new(VideoCodec::VP9, Box::new(|frame| {
    // Handle decoded frame
}));
```

**WASM:**
```javascript
const decoder = new WasmDecoder(VideoCodec.VP9, (videoFrame) => {
    ctx.drawImage(videoFrame, 0, 0);
    videoFrame.close();
});
```

## Framework Integration

### React Example

```typescript
import { useEffect, useRef } from 'react';
import init, { WasmDecoder, VideoCodec } from 'videocall-codecs';

export const VideoPlayer = ({ websocketUrl }: { websocketUrl: string }) => {
    const canvasRef = useRef<HTMLCanvasElement>(null);

    useEffect(() => {
        const initDecoder = async () => {
            await init();
            
            const canvas = canvasRef.current!;
            const ctx = canvas.getContext('2d')!;
            
            const decoder = new WasmDecoder(VideoCodec.VP9, (videoFrame) => {
                ctx.drawImage(videoFrame, 0, 0, canvas.width, canvas.height);
                videoFrame.close();
            });
            
            // Connect WebSocket and handle frames...
        };
        initDecoder();
    }, [websocketUrl]);

    return <canvas ref={canvasRef} width={640} height={480} />;
};
```

## Configuration

### Jitter Buffer Settings

```rust
const MIN_PLAYOUT_DELAY_MS: f64 = 10.0;   
const MAX_PLAYOUT_DELAY_MS: f64 = 500.0;  
const JITTER_MULTIPLIER: f64 = 3.0;       
const MAX_BUFFER_SIZE: usize = 200;       
```

### Cargo Features

```toml
[features]
default = ["native"]
native = ["libvpx-sys"]
wasm = ["wasm-bindgen", "web-sys", "js-sys", "wasm-bindgen-futures"]  
```

## Troubleshooting

**WASM import issues:**
```javascript
// Always initialize first
import init, { WasmDecoder } from './pkg/videocall_codecs.js';
await init();
```

**Memory leaks:**
```javascript
// Always close VideoFrame objects
decoder = new WasmDecoder(VideoCodec.VP9, (videoFrame) => {
    ctx.drawImage(videoFrame, 0, 0);
    videoFrame.close(); // Required!
});
```

**Frame types:**
```javascript
// Use exact strings
decoder.push_frame({
    frame_type: 'KeyFrame', // Must be 'KeyFrame' or 'DeltaFrame'
    // ...
});
```

## Performance Tips

1. **Always call `videoFrame.close()`** in WASM to prevent memory leaks
2. **Use Web Workers**: Automatically enabled when available
3. **Send keyframes regularly** for gap recovery
4. **Use monotonic sequence numbers** for proper ordering

## Testing

```bash
cargo test --all-features
wasm-pack test --headless --chrome --features wasm
```

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
