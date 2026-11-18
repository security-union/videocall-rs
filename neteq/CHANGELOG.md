# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.1](https://github.com/security-union/videocall-rs/compare/neteq-v1.0.0...neteq-v1.0.1) - 2026-11-18

### Other

- update Cargo.toml dependencies

## [1.0.0]

A rewrite. The crate is now a line-by-line port of WebRTC NetEq (rev
`6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd`), checked against the C++ NetEq by
the harness in `parity/`. The 0.x float pipeline is gone.

### Added

- `api::NetEq` / `NetEqBuilder` with the WebRTC method set: `insert_packet`,
  `get_audio`, `set_codecs`, `register_payload_type`, delay knobs,
  `network_statistics`, `current_network_statistics`, `lifetime_statistics`,
  `operations_and_state`, `flush_buffers`, and `last_operation`.
- Fixed-point DSP ported from WebRTC: SPL kernels, expand, merge, accelerate,
  preemptive expand, background noise, comfort noise, DTMF, RED splitting.
- `codecs`: PCM16B, Opus on ropus with FEC and WebRTC's packet parsing, RFC
  3389 comfort noise, behind `BuiltinAudioDecoderFactory`.
- `telemetry`: `NetEqStats` (dashboard JSON, keeps the 0.x top-level and
  `network.*` field names, adds `lifetime` and `operations`),
  `TelemetryCollector` for per-second operation and reorder counters, and
  `q14` helpers.
- `tools::NetEqTest` (feature `tools`) and `parity::Observer` (feature
  `parity`).
- `LICENSE-WEBRTC`; the crate license is now `(MIT OR Apache-2.0) AND BSD-3-Clause`.

### Changed

- `WebNetEq::new(sample_rate, channels)` builds synchronously; the async
  `init()` and the `additional_delay_ms` argument are gone. 48 kHz only.
- The `web` feature decodes Opus with ropus inside the worker on every
  browser. WebCodecs and the embedded `opus-decoder.min.js` are removed:
  NetEq decodes synchronously inside `GetAudio`, which an async decoder
  cannot serve.
- The worker no longer produces audio on a 5 ms timer. An AudioWorklet
  attaches over a `MessageChannel` (`attachWorklet`) and pulls 10 ms frames
  as the device consumes them. The worker posts `audioLevel` (50 ms peak RMS)
  to the main thread instead of every PCM frame.
- Worker `flush` now really flushes NetEq; `clear` rebuilds it (it used to
  leave the worker without a NetEq).
- Defaults follow Chrome's receiver: no 80 ms additive delay, no 20 ms base
  minimum delay, smart flushing off (a full 200-packet buffer flushes
  completely), muted state on.
- RTP timestamps must be in codec-clock samples (48 kHz for Opus).

### Removed

- The 0.x `NetEq`, `NetEqConfig`, `AudioPacket`, `codec::*` decoders,
  `bypass_mode`, `additional_delay_ms`, and the unused `NetEqError` variants
  (`BufferFull`, `BufferEmpty`, `InvalidPacket`, `InvalidTimestamp`,
  `DecoderError`, `TimeStretchError`, `AudioFormatMismatch`).
- `neteq_player --no-neteq`.

## [0.9.1](https://github.com/security-union/videocall-rs/compare/neteq-v0.9.0...neteq-v0.9.1) - 2026-08-13

### Added

- pure-Rust VP9 codec (encoder + decoder) — remove C libvpx from videocall-cli & bot ([#884](https://github.com/security-union/videocall-rs/pull/884))

## [0.9.0](https://github.com/security-union/videocall-rs/compare/neteq-v0.8.3...neteq-v0.9.0) - 2026-06-30

### Added

- remove C libopus (audiopus-sys) across the board — pure-Rust ropus codec ([#872](https://github.com/security-union/videocall-rs/pull/872))

## [0.8.3](https://github.com/security-union/videocall-rs/compare/neteq-v0.8.2...neteq-v0.8.3) - 2026-02-19

### Other

- update Cargo.toml dependencies

## [0.8.2](https://github.com/security-union/videocall-rs/compare/neteq-v0.8.1...neteq-v0.8.2) - 2026-02-16

### Other

- Neteq expand improvements ([#614](https://github.com/security-union/videocall-rs/pull/614))

## [0.8.1](https://github.com/security-union/videocall-rs/compare/neteq-v0.8.0...neteq-v0.8.1) - 2026-01-27

### Other

- update Cargo.lock dependencies

## [0.8.0](https://github.com/security-union/videocall-rs/compare/neteq-v0.7.1...neteq-v0.8.0) - 2025-11-30

### Other

- Delete commented code and address clippy warnings ([#489](https://github.com/security-union/videocall-rs/pull/489))
- neteq preemptive expand improvements ([#476](https://github.com/security-union/videocall-rs/pull/476))
- improved accelerate ([#475](https://github.com/security-union/videocall-rs/pull/475))

## [0.7.1](https://github.com/security-union/videocall-rs/compare/neteq-v0.7.0...neteq-v0.7.1) - 2025-11-02

### Other

- allow using neteq wasm without the worker ([#477](https://github.com/security-union/videocall-rs/pull/477))

## [0.7.0](https://github.com/security-union/videocall-rs/compare/neteq-v0.6.2...neteq-v0.7.0) - 2025-10-30

### Other

- NetEQ Overhaul: WebCodecs Support, Critical Bug Fixes, and CI Improvements ([#466](https://github.com/security-union/videocall-rs/pull/466))

## [0.6.2](https://github.com/security-union/videocall-rs/compare/neteq-v0.6.1...neteq-v0.6.2) - 2025-10-13

### Other

- update Cargo.lock dependencies

## [0.6.1](https://github.com/security-union/videocall-rs/compare/neteq-v0.6.0...neteq-v0.6.1) - 2025-09-24

### Fixed

- neteq pcm worker  ([#429](https://github.com/security-union/videocall-rs/pull/429))

## [0.6.0](https://github.com/security-union/videocall-rs/compare/neteq-v0.5.1...neteq-v0.6.0) - 2025-09-24

### Other

- refactor to use a single context ([#428](https://github.com/security-union/videocall-rs/pull/428))
- Fix #415: Failed to enqueue PCM Data ([#417](https://github.com/security-union/videocall-rs/pull/417))

## [0.5.1](https://github.com/security-union/videocall-rs/compare/neteq-v0.5.0...neteq-v0.5.1) - 2025-08-20

### Fixed

- *(codecs)* route worker diag messages to health bus; refs #397 ([#400](https://github.com/security-union/videocall-rs/pull/400))

## [0.5.0](https://github.com/security-union/videocall-rs/compare/neteq-v0.4.1...neteq-v0.5.0) - 2025-08-15

### Other

- Add packets per sec, and matomo logs to debug system, handle mic errors more gracefully ([#385](https://github.com/security-union/videocall-rs/pull/385))

## [0.4.1](https://github.com/security-union/videocall-rs/compare/neteq-v0.4.0...neteq-v0.4.1) - 2025-08-08

### Other

- (feature) Add diagnostics with Prometheus and Grafana ([#365](https://github.com/security-union/videocall-rs/pull/365))

## [0.4.0](https://github.com/security-union/videocall-rs/compare/neteq-v0.3.1...neteq-v0.4.0) - 2025-08-02

### Other

- rewrite filter buffer, add a ton of tests  ([#356](https://github.com/security-union/videocall-rs/pull/356))

## [0.3.1](https://github.com/security-union/videocall-rs/compare/neteq-v0.3.0...neteq-v0.3.1) - 2025-08-02

### Other

- enable acceleration ([#354](https://github.com/security-union/videocall-rs/pull/354))

## [0.3.0](https://github.com/security-union/videocall-rs/compare/neteq-v0.2.4...neteq-v0.3.0) - 2025-08-02

### Other

- Fix neteq buffering and show app version ([#352](https://github.com/security-union/videocall-rs/pull/352))

## [0.2.4](https://github.com/security-union/videocall-rs/compare/neteq-v0.2.3...neteq-v0.2.4) - 2025-07-31

### Other

- stats fixed ([#347](https://github.com/security-union/videocall-rs/pull/347))

## [0.2.3](https://github.com/security-union/videocall-rs/compare/neteq-v0.2.2...neteq-v0.2.3) - 2025-07-31

### Other

- speaker selection, neteq worker audio reproduction ([#345](https://github.com/security-union/videocall-rs/pull/345))

## [0.2.1](https://github.com/security-union/videocall-rs/compare/neteq-v0.2.0...neteq-v0.2.1) - 2025-07-20

### Other

- Add High availability ([#325](https://github.com/security-union/videocall-rs/pull/325))

## [0.2.0](https://github.com/security-union/videocall-rs/compare/neteq-v0.1.2...neteq-v0.2.0) - 2025-07-10

### Other

- Add neteq to safari and use worklet for audio decoding across the board ([#315](https://github.com/security-union/videocall-rs/pull/315))

## [0.1.2](https://github.com/security-union/videocall-rs/compare/neteq-v0.1.1...neteq-v0.1.2) - 2025-07-07

### Other

- add neteq behind a feature flag ([#310](https://github.com/security-union/videocall-rs/pull/310))
