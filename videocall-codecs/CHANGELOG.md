# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.6](https://github.com/security-union/videocall-rs/compare/videocall-codecs-v0.1.5...videocall-codecs-v0.1.6) - 2025-08-20

### Fixed

- *(codecs)* route worker diag messages to health bus; refs #397 ([#400](https://github.com/security-union/videocall-rs/pull/400))

## [0.1.5](https://github.com/security-union/videocall-rs/compare/videocall-codecs-v0.1.4...videocall-codecs-v0.1.5) - 2025-08-08

### Other

- (feature) Add diagnostics with Prometheus and Grafana ([#365](https://github.com/security-union/videocall-rs/pull/365))

## [0.1.4](https://github.com/security-union/videocall-rs/compare/videocall-codecs-v0.1.3...videocall-codecs-v0.1.4) - 2025-07-25

### Other

- Fix pin icon positioning and visibility on iOS and desktop ([#324](https://github.com/security-union/videocall-rs/pull/324)) ([#338](https://github.com/security-union/videocall-rs/pull/338))

## [0.1.3](https://github.com/security-union/videocall-rs/compare/videocall-codecs-v0.1.2...videocall-codecs-v0.1.3) - 2025-07-20

### Other

- Add High availability ([#325](https://github.com/security-union/videocall-rs/pull/325))

## [0.1.2](https://github.com/security-union/videocall-rs/compare/videocall-codecs-v0.1.1...videocall-codecs-v0.1.2) - 2025-07-03

### Other

- Reset decoder and jitter buffer when there's a decoder error ([#298](https://github.com/security-union/videocall-rs/pull/298))

## [0.1.1](https://github.com/security-union/videocall-rs/compare/videocall-codecs-v0.1.0...videocall-codecs-v0.1.1) - 2025-06-25

### Other

- use jitter buffer in wasm and improve diagrams ([#288](https://github.com/security-union/videocall-rs/pull/288))

## [0.1.0](https://github.com/security-union/videocall-rs/releases/tag/videocall-codecs-v0.1.0) - 2025-01-01

### Added

- Initial release of videocall-codecs crate
- VP8/VP9 video codec support with native and WASM implementations
- Worker-based video decoding for web environments
- Cross-platform codec abstraction layer
- Proof of concept decoder implementation
- Support for both native (libvpx) and WebCodecs API backends
