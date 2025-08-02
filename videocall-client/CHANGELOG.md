# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.17](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.16...videocall-client-v1.1.17) - 2025-08-02

### Other

- updated the following local packages: neteq

## [1.1.16](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.15...videocall-client-v1.1.16) - 2025-08-02

### Other

- updated the following local packages: neteq

## [1.1.15](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.14...videocall-client-v1.1.15) - 2025-08-02

### Other

- updated the following local packages: neteq

## [1.1.14](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.13...videocall-client-v1.1.14) - 2025-07-31

### Other

- updated the following local packages: neteq

## [1.1.13](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.12...videocall-client-v1.1.13) - 2025-07-31

### Other

- speaker selection, neteq worker audio reproduction ([#345](https://github.com/security-union/videocall-rs/pull/345))

## [1.1.12](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.11...videocall-client-v1.1.12) - 2025-07-25

### Other

- Fix net eq 2 ([#340](https://github.com/security-union/videocall-rs/pull/340))
- Fix pin icon positioning and visibility on iOS and desktop ([#324](https://github.com/security-union/videocall-rs/pull/324)) ([#338](https://github.com/security-union/videocall-rs/pull/338))

## [1.1.11](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.10...videocall-client-v1.1.11) - 2025-07-20

### Other

- Add High availability ([#325](https://github.com/security-union/videocall-rs/pull/325))

## [1.1.10](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.9...videocall-client-v1.1.10) - 2025-07-10

### Other

- release ([#316](https://github.com/security-union/videocall-rs/pull/316))

## [1.1.9](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.8...videocall-client-v1.1.9) - 2025-07-07

### Other

- add neteq behind a feature flag ([#310](https://github.com/security-union/videocall-rs/pull/310))

## [1.1.8](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.7...videocall-client-v1.1.8) - 2025-07-03

### Other

- Reset decoder and jitter buffer when there's a decoder error ([#298](https://github.com/security-union/videocall-rs/pull/298))

## [1.1.7](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.6...videocall-client-v1.1.7) - 2025-06-23

### Other

- Add new decoder and Add MIT - Apache 2 license to all files ([#285](https://github.com/security-union/videocall-rs/pull/285))
- Fix rotation ios image aspect ratio ([#282](https://github.com/security-union/videocall-rs/pull/282))
- Hide screen share safari and fix selector ([#281](https://github.com/security-union/videocall-rs/pull/281))
- Test media track stream processor add wasm opus encoder to support Safari for realz ([#266](https://github.com/security-union/videocall-rs/pull/266))

## [1.1.6](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.5...videocall-client-v1.1.6) - 2025-03-31

### Added

- Subscribe to device changes and enable clippy in videocall-client ([#245](https://github.com/security-union/videocall-rs/pull/245))

## [1.1.5](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.4...videocall-client-v1.1.5) - 2025-03-30

### Added

- Added multipeer bitrate control ([#242](https://github.com/security-union/videocall-rs/pull/242))

## [1.1.4](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.3...videocall-client-v1.1.4) - 2025-03-29

### Fixed

- UI Prevent constant updates due to minor bitrate changes. ([#240](https://github.com/security-union/videocall-rs/pull/240))

## [1.1.3](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.2...videocall-client-v1.1.3) - 2025-03-29

### Other

- Show correct aspect ratio, disable screen while not streaming ([#238](https://github.com/security-union/videocall-rs/pull/238))

## [1.1.2](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.1...videocall-client-v1.1.2) - 2025-03-28

### Added

- Add video, screen and mic state to heartbeat and to peer state ([#234](https://github.com/security-union/videocall-rs/pull/234))

## [1.1.1](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.1.0...videocall-client-v1.1.1) - 2025-03-27

### Fixed

- screenshare and reduce gap of initial video frame ([#231](https://github.com/security-union/videocall-rs/pull/231))

## [1.1.0](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.0.1...videocall-client-v1.1.0) - 2025-03-26

### Added

- Publish UI as a standalone crate and incorporate into single videocall workspace ([#228](https://github.com/security-union/videocall-rs/pull/228))

## [1.0.1](https://github.com/security-union/videocall-rs/compare/videocall-client-v1.0.0...videocall-client-v1.0.1) - 2025-03-26

### Fixed

- bring back audio playback ([#226](https://github.com/security-union/videocall-rs/pull/226))

## [0.1.1](https://github.com/security-union/videocall-rs/compare/videocall-client-v0.1.0...videocall-client-v0.1.1) - 2025-03-25

### Other

- Try to get release plz to work ([#216](https://github.com/security-union/videocall-rs/pull/216))
- add release-plz.toml ([#212](https://github.com/security-union/videocall-rs/pull/212))

## [0.1.0](https://github.com/security-union/videocall-rs/releases/tag/videocall-client-v0.1.0) - 2025-03-24

### Fixed

- fix instructions ([#161](https://github.com/security-union/videocall-rs/pull/161))

### Other

- Diagnostics P2 ([#208](https://github.com/security-union/videocall-rs/pull/208))
- Diagnostics P1 (UI tweaks) ([#207](https://github.com/security-union/videocall-rs/pull/207))
- Diagnostics Part 1 ([#206](https://github.com/security-union/videocall-rs/pull/206))
- yew-ui enhance error handling for unable to access camera ([#200](https://github.com/security-union/videocall-rs/pull/200))
- Fix ci ([#191](https://github.com/security-union/videocall-rs/pull/191))
- Release video call daemon ([#176](https://github.com/security-union/videocall-rs/pull/176))
- Fix webtransport ([#165](https://github.com/security-union/videocall-rs/pull/165))
- updating base images ([#160](https://github.com/security-union/videocall-rs/pull/160))
- Add peer heartbeat monitor ([#157](https://github.com/security-union/videocall-rs/pull/157))
- Video daemon ([#144](https://github.com/security-union/videocall-rs/pull/144))
- Revert "Revert "Move `VideoCallClient` into its own crate ([#142](https://github.com/security-union/videocall-rs/pull/142))" ([#147](https://github.com/security-union/videocall-rs/pull/147))" ([#150](https://github.com/security-union/videocall-rs/pull/150))
- Revert "Move `VideoCallClient` into its own crate ([#142](https://github.com/security-union/videocall-rs/pull/142))" ([#147](https://github.com/security-union/videocall-rs/pull/147))
- Move `VideoCallClient` into its own crate ([#142](https://github.com/security-union/videocall-rs/pull/142))
