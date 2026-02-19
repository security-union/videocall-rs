# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [4.0.1](https://github.com/security-union/videocall-rs/compare/videocall-types-v4.0.0...videocall-types-v4.0.1) - 2026-02-19

### Other

- Decouple yew from videocall client ([#633](https://github.com/security-union/videocall-rs/pull/633))

## [4.0.0](https://github.com/security-union/videocall-rs/compare/videocall-types-v3.0.1...videocall-types-v4.0.0) - 2026-01-27

### Other

- breaking change: Protobuf enums should always have an _UNSPECIFIED = 0 variant ([#537](https://github.com/security-union/videocall-rs/pull/537))
- Fix firefox support by sending vp8 instead of vp9 ([#535](https://github.com/security-union/videocall-rs/pull/535))
- Fix GLIBC compatibility and make DATABASE_ENABLED optional ([#519](https://github.com/security-union/videocall-rs/pull/519))
- Meeting Ownership Project (behind feature flag) ([#503](https://github.com/security-union/videocall-rs/pull/503))

## [3.0.1](https://github.com/security-union/videocall-rs/compare/videocall-types-v3.0.0...videocall-types-v3.0.1) - 2025-08-18

### Other

- Add server stats ([#399](https://github.com/security-union/videocall-rs/pull/399))

## [3.0.0](https://github.com/security-union/videocall-rs/compare/videocall-types-v2.0.2...videocall-types-v3.0.0) - 2025-08-15

### Other

- Add packets per sec, and matomo logs to debug system, handle mic errors more gracefully ([#385](https://github.com/security-union/videocall-rs/pull/385))

## [2.0.2](https://github.com/security-union/videocall-rs/compare/videocall-types-v2.0.1...videocall-types-v2.0.2) - 2025-08-10

### Other

- Reduce RTT freq, add RTT to stats,  ([#376](https://github.com/security-union/videocall-rs/pull/376))

## [2.0.1](https://github.com/security-union/videocall-rs/compare/videocall-types-v2.0.0...videocall-types-v2.0.1) - 2025-08-08

### Other

- (feature) Add diagnostics with Prometheus and Grafana ([#365](https://github.com/security-union/videocall-rs/pull/365))

## [2.0.0](https://github.com/security-union/videocall-rs/compare/videocall-types-v1.0.3...videocall-types-v2.0.0) - 2025-07-20

### Other

- Add High availability ([#325](https://github.com/security-union/videocall-rs/pull/325))

## [1.0.3](https://github.com/security-union/videocall-rs/compare/videocall-types-v1.0.2...videocall-types-v1.0.3) - 2025-06-29

### Other

- Fix meeting creation ([#293](https://github.com/security-union/videocall-rs/pull/293))

## [1.0.2](https://github.com/security-union/videocall-rs/compare/videocall-types-v1.0.1...videocall-types-v1.0.2) - 2025-06-23

### Other

- Add new decoder and Add MIT - Apache 2 license to all files ([#285](https://github.com/security-union/videocall-rs/pull/285))

## [1.0.1](https://github.com/security-union/videocall-rs/compare/videocall-types-v1.0.0...videocall-types-v1.0.1) - 2025-03-28

### Added

- Add video, screen and mic state to heartbeat and to peer state ([#234](https://github.com/security-union/videocall-rs/pull/234))

## [0.2.1](https://github.com/security-union/videocall-rs/compare/videocall-types-v0.2.0...videocall-types-v0.2.1) - 2025-03-25

### Other

- Try to get release plz to work ([#216](https://github.com/security-union/videocall-rs/pull/216))

## [0.2.0](https://github.com/security-union/videocall-rs/compare/videocall-types-v0.1.0...videocall-types-v0.2.0) - 2025-03-24

### Other

- Diagnostics P2 ([#208](https://github.com/security-union/videocall-rs/pull/208))
- Diagnostics Part 1 ([#206](https://github.com/security-union/videocall-rs/pull/206))
