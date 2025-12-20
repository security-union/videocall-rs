# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
