# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **BREAKING**: Added `_UNKNOWN = 0` variants to all protobuf enums (`PacketType`, `MediaType`, `MeetingEventType`). This shifts all existing enum values by 1. Clients and servers must be updated together.
- CI: Updated workflows to trigger rebuilds when `videocall-types/` or `protobuf/` change. This ensures API, UI, and CLI are rebuilt when protobuf definitions change.

## [0.1.0](https://github.com/security-union/videocall-rs/releases/tag/sec-api-v0.1.0) - 2025-03-24

### Other

- Diagnostics P2 ([#208](https://github.com/security-union/videocall-rs/pull/208))
- Fix ci ([#191](https://github.com/security-union/videocall-rs/pull/191))
- Upload images with the latest TAG ([#190](https://github.com/security-union/videocall-rs/pull/190))
- Release video call daemon ([#176](https://github.com/security-union/videocall-rs/pull/176))
- Fix webtransport ([#165](https://github.com/security-union/videocall-rs/pull/165))
- make db optional and update instructions to digital ocean ([#163](https://github.com/security-union/videocall-rs/pull/163))
- Video daemon ([#144](https://github.com/security-union/videocall-rs/pull/144))
- Form validation ([#141](https://github.com/security-union/videocall-rs/pull/141))
- Async nats ([#135](https://github.com/security-union/videocall-rs/pull/135))
- Fix fmt & clippy on ci ([#124](https://github.com/security-union/videocall-rs/pull/124))
- Use cargo workspace ([#113](https://github.com/security-union/videocall-rs/pull/113))
- Remove spots that websocket API looks at packets to prep for e2ee ([#109](https://github.com/security-union/videocall-rs/pull/109))
- Moving to digital ocean ([#97](https://github.com/security-union/videocall-rs/pull/97))
- Fix UI stuttering by playing key frames asap ([#91](https://github.com/security-union/videocall-rs/pull/91))
- Deploy webtransport to k8s ([#88](https://github.com/security-union/videocall-rs/pull/88))
- Add Webtransport part 2 ([#86](https://github.com/security-union/videocall-rs/pull/86))
- Basic Webtransport ([#79](https://github.com/security-union/videocall-rs/pull/79))
- Bump openssl from 0.10.41 to 0.10.55 in /actix-api ([#78](https://github.com/security-union/videocall-rs/pull/78))
- refactor chat server join room method ([#77](https://github.com/security-union/videocall-rs/pull/77))
- Helm chart ([#71](https://github.com/security-union/videocall-rs/pull/71))
- Horizontal scaling with NATS ([#62](https://github.com/security-union/videocall-rs/pull/62))
- Fix audio && screenshare ([#65](https://github.com/security-union/videocall-rs/pull/65))
- cargo clippy ([#45](https://github.com/security-union/videocall-rs/pull/45))
- Add videoframe reordering on the UI ([#44](https://github.com/security-union/videocall-rs/pull/44))
- Video 2 version ([#42](https://github.com/security-union/videocall-rs/pull/42))
- Add bots ([#21](https://github.com/security-union/videocall-rs/pull/21))
- Refactor for video shooting ([#11](https://github.com/security-union/videocall-rs/pull/11))
- removes command module ([#8](https://github.com/security-union/videocall-rs/pull/8))
- Add audio ([#2](https://github.com/security-union/videocall-rs/pull/2))
- Initial MVP ([#1](https://github.com/security-union/videocall-rs/pull/1))
- Initial commit
