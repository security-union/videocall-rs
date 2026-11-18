# Rust NetEQ — Adaptive Jitter Buffer for Real-Time Audio

Part of the **[videocall.rs](https://github.com/security-union/videocall-rs)** real-time communication stack.

> **Project status:** early preview – API is settling, performance tuning in progress.

Rust NetEQ is a pure-Rust, libWebRTC-inspired jitter buffer designed for professional, low-latency audio applications. It smooths out bursty networks, conceals packet loss, and keeps playback on time so that downstream DSP or audio renderers can focus on sound, not sockets.

## A line-by-line port of WebRTC NetEq

Since 1.0 this crate is a line-by-line Rust port of Google WebRTC's NetEq: the jitter buffer, the decision logic, and the fixed-point packet loss concealment and time stretching that Chrome uses to play remote audio.

The port tracks WebRTC at rev `6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd`. The harness in [`parity/`](parity) builds the real C++ NetEq and diffs it against this crate tick by tick: every output sample, every operation, every statistic. All 100 scenario × codec runs (PCM16B at 8–48 kHz and Opus, mono and stereo) are identical.

## Keeping in sync with WebRTC

The WebRTC source is a Nix input, so everyone builds the same baseline:

* **The pin** is a hashed `fetchgit` of that rev in `nix/shells.nix` (`shells.neteq-parity`). The shell exports it as `WEBRTC_SRC`.
* **The gate** runs in CI on every change under `neteq/**` (`.github/workflows/neteq-parity.yaml`):
  ```bash
  nix-shell neteq/parity/shell.nix --run 'cd neteq/parity && cargo test --features rust-neteq'
  ```
* **The goldens** are per-component hashes from the real C++, committed in the Rust tests, so `cargo test -p neteq` needs no C++. Regenerate them with `neteq/src/spl/golden/build.sh` in the same shell.

To move to a newer WebRTC rev: bump the rev and hash in `nix/shells.nix` and `PINNED_REV` in `parity/build.rs`, run the gate, port whatever diverges, and regenerate the goldens.

## Why Rust NetEQ?

* **Latency you control** – adaptive target delay with hard minimum/maximum bounds.
* **Rock-solid ordering** – duplicate detection, re-sequencing, and smart flushing protect against wild packet arrival patterns.
* **Seamless playout** – accelerate, pre-emptive expand, and classical expand algorithms reduce audible artifacts without introducing heavy CPU cost.
* **Actionable metrics** – detailed runtime statistics make debugging network conditions trivial.
* **Zero unsafe ✨** – implemented in 100 % safe Rust; integrates into `no_std` builds that provide an allocator.

## At a Glance

### Native Rust Applications

```toml
[dependencies]
neteq = "1.0"
```

Insert RTP payloads as they arrive, call `get_audio()` every 10 ms, and let the buffer handle the rest.  Full runnable snippets live in the [examples](examples) directory — open `basic_usage.rs` first.

### Web Applications (WASM + WebWorker)

```toml
[dependencies]
neteq = { version = "1.0", features = ["web"], default-features = false }
```

For browser environments, NetEq ships with:
- **🕸️ WebWorker binary** (`neteq_worker`) for off-main-thread audio processing
- **🎵 Built-in Opus decoder** - pure-Rust ropus compiled into the worker, nothing external to serve
- **🌐 Cross-browser support** - unified decoder works on Chrome, Firefox, Safari
- **⚡ WASM integration** - `WebNetEq` wrapper with JavaScript bindings

```javascript
// Main thread: spawn NetEq worker
const worker = new Worker('/neteq_worker.js');

// Send Opus packets to worker
worker.postMessage({
  cmd: 'insert',
  seq: sequenceNumber,
  timestamp: rtpTimestamp,
  payload: opusPacketBytes
});

// Connect the worker straight to your AudioWorklet. The worklet asks for
// 10 ms frames as the sound card consumes them, so PCM never touches the
// main thread.
const channel = new MessageChannel();
worker.postMessage({ cmd: 'attachWorklet' }, [channel.port1]);
audioWorklet.port.postMessage({ command: 'attach' }, [channel.port2]);
```

`rtpTimestamp` counts 48 kHz samples, not milliseconds. The worklet side of the protocol is documented at the top of [`src/bin/neteq_worker.rs`](src/bin/neteq_worker.rs).

## Feature Highlights

| Category             | What you get                                                    |
|----------------------|-----------------------------------------------------------------|
| Delay management     | Quantile-based estimator, exponential smoothing                 |
| Time-stretching      | Accelerate • Pre-emptive Expand • Classical Expand              |
| Buffer housekeeping  | Age trimming • Overflow flush • Duplicate suppression           |
| Statistics           | Per-frame network & lifetime counters for observability         |
| Configuration        | Sample-rate aware, channel agnostic, compile-time feature flags |
| **Web deployment**   | **WebWorker binary • Embedded Opus decoder • Zero dependencies** |


## Architecture Overview

```
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│   AudioPacket   │    │  PacketBuffer   │    │  DelayManager   │
│                 │───▶│                 │───▶│                 │
│ • RTP Header    │    │ • Ordered Queue │    │ • Adaptive      │
│ • Audio Data    │    │ • Duplicate Det │    │ • Statistics    │
│ • Timestamps    │    │ • Smart Flush   │    │ • Target Delay  │
└─────────────────┘    └─────────────────┘    └─────────────────┘
                                   │
                                   ▼
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│ TimeStretching  │    │     NetEQ       │    │   Statistics    │
│                 │◀───│                 │───▶│                 │
│ • Accelerate    │    │ • Decision      │    │ • Network Stats │
│ • Preemptive    │    │ • Decode        │    │ • Lifetime      │
│ • Expand        │    │ • Control       │    │ • Operations    │
└─────────────────┘    └─────────────────┘    └─────────────────┘
```

Each 10 ms tick NetEQ decides between **Normal**, **Accelerate**, **Pre-emptive Expand**, or **Expand** based on buffer fullness and arrival trends.  Decisions and their rationale are exposed through the statistics API for post-analysis.

## 🌐 Web Deployment & Embedded Opus Decoder

NetEq includes a complete web deployment solution with **zero external dependencies**:

### WebWorker Architecture
- **`neteq_worker`** binary runs NetEq off the main thread
- Receives Opus packets via `postMessage()`
- Hands decoded 10 ms frames to the AudioWorklet over a `MessagePort`, when the worklet asks for them
- Built-in mute/unmute and diagnostics controls

### Self-Contained Opus Decoding
- **ropus** - a pure-Rust Opus decoder, compiled into the worker, no CDN required
- **Cross-browser compatibility** - works on Chrome, Firefox, Safari
- **Synchronous** - NetEq decodes inside `GetAudio`, which the async WebCodecs `AudioDecoder` can't do
- **Zero configuration** - nothing to load at runtime

### Build Integration
When you build with `--features web`, the Opus decoder is compiled straight into the WASM binary. This means:

- ✅ No external files to serve alongside your application
- ✅ No version mismatches between NetEq and Opus decoder
- ✅ Works in any deployment environment (CDN, static hosting, etc.)
- ✅ Offline-capable applications

```bash
# Build NetEq worker for web deployment
cargo build --target wasm32-unknown-unknown --bin neteq_worker --features web --no-default-features
```

## Examples

### Native Examples

| Example              | Highlights                                              |
|----------------------|---------------------------------------------------------|
| `basic_usage.rs`     | Minimal integration loop                                |
| `neteq_player.rs`    | WAV replay with user-defined jitter & re-ordering knobs |

```bash
cargo run --release --example basic_usage
```

### Web Integration

The **[videocall.rs](https://github.com/security-union/videocall-rs)** project demonstrates production NetEq WebWorker usage:
- Real-time Opus packet processing from WebRTC
- AudioWorklet integration for glitch-free playback  
- Mute/unmute synchronization across worker threads
- Live performance monitoring with metrics dashboard

## Performance Dashboard

Monitor NetEq performance in real-time with our interactive web dashboard. Track buffer health, network adaptation rates, and audio quality metrics.

📊 **[Complete Dashboard Setup Guide →](DASHBOARD_README.md)**

![NetEq Performance Dashboard](stats_dashboard_sample.png)

Key features:
- Real-time buffer size vs target monitoring
- Network adaptation rate visualization  
- Audio quality metrics and underrun detection
- Configurable volume control for silent testing
- Native Rust axum web server

## Roadmap

1. Advanced concealment (pitch-based PLC)
2. Voice Activity Detection integration
3. Multi-codec framing helpers (Opus, LC3 …)
4. SIMD accelerations for time-stretch routines

Contributions are welcome — please skim the [CONTRIBUTING.md](CONTRIBUTING.md) first.

## License

(MIT OR Apache-2.0) AND BSD-3-Clause

## License and Attribution

Our own code is licensed under MIT OR Apache-2.0.

### WebRTC Attribution

Since 1.0 most of this crate is a translation of the WebRTC NetEq source, so those parts stay under WebRTC's license. The original WebRTC project is:
- Copyright (c) 2011, The WebRTC project authors
- Licensed under the 3-clause BSD license  
- Available at: https://webrtc.googlesource.com/src/

The full notice and the `PATENTS` grant are in [`LICENSE-WEBRTC`](LICENSE-WEBRTC).

