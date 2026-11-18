# neteq-parity

This crate runs Google's C++ NetEq and our Rust `neteq` side by side on the same packets and tells you exactly where they disagree. The C++ one is the reference. If the port is a carbon copy, every 10 ms frame comes out bit-identical, with the same decision (normal, expand, accelerate...) on every frame.

Think of it as calibrating a bathroom scale. You don't argue about whether it "feels right", you put a known mass on it next to a certified scale and read both dials. Here the certified scale is WebRTC at a pinned revision, and the known masses are seeded packet traces.

It lives in its own Cargo workspace, so the root workspace and the wasm builds never see it.

## Setup

```sh
nix-shell neteq/parity/shell.nix                 # C++20 compiler, abseil, libopus, WebRTC source, pinned via nixtamal
cd neteq/parity
cargo test                                       # C++-only smoke tests, always on
```

The nix shell fetches WebRTC itself: a sparse `fetchgit` of `6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd` (about 17 MB) lands in the store, and the shell exports `WEBRTC_SRC` at it and `WEBRTC_REV` with the rev. CI runs the same shell in `.github/workflows/neteq-parity.yaml` whenever something under `neteq/` changes.

Without Nix, run `neteq/parity/scripts/fetch_webrtc.sh`. It makes a sparse git clone in `neteq/parity/third_party/webrtc` (gitignored), which `build.rs` uses when `WEBRTC_SRC` is unset. You bring your own C++20 compiler, abseil and fixed-point libopus. `build.rs` warns if the source isn't at the pinned rev, reading `WEBRTC_REV` first and `git rev-parse HEAD` otherwise.

## Running it

```sh
cargo run -- list                                                # scenarios and codecs
cargo run -- run --scenario all --codec all --candidate none     # reference only
cargo run -- run --scenario burst_loss --codec opus-mono --candidate cpp   # self-check, must be IDENTICAL
cargo run --features rust-neteq -- run --scenario all --codec all --json report.json
cargo run --features rust-neteq -- run --scenario all --codec opus-mono --opus-decoder ropus   # non-gating drift report
cargo run -- gen --scenario long_mixed --codec l16-48k-stereo --out t.json --no-payloads
cargo run -- run --trace t.json --candidate none
```

`run` prints a summary per trace and exits 1 if anything differs. `--json` writes the full report: bit-exact sample ratio, first divergent frame, max abs diff, SNR, operation agreement plus a confusion list, target and filtered delay diffs, and per-field deltas for network, lifetime and operations-and-state stats. Add `--frames` to include every frame record.

`cargo test --features rust-neteq` adds `tests/parity.rs`, one test per scenario over every codec, asserting the Rust NetEq is identical. Opus runs on C libopus on both sides there. `ropus_drift_report` prints the `ropus` diff and never fails. Narrow it with `NETEQ_PARITY_CODEC=opus-mono`.

## Scenarios

All seeded and deterministic. Payloads come from a built-in speech synth (voiced segments with gliding pitch and formants, fricative bursts, pauses), packed as L16 or encoded with libopus. 20 ms packets.

`clean`, `uniform_jitter`, `gaussian_spikes` (jitter plus network stalls released in a burst), `burst_loss` (Gilbert-Elliott), `reordering`, `clock_drift_fast` / `clock_drift_slow` (sender at ±2000 ppm), `dtx` (RFC 3389 SID from WebRTC's own CNG encoder for mono L16, Opus DTX for Opus), `duplicates`, and `long_mixed` (60 s cycling through all of it).

Codecs: L16 at 8/16/32/48 kHz mono and stereo, Opus mono and stereo. Stereo L16 DTX pauses without CN because NetEq's comfort noise is mono-only.

## How the C++ side is wired

`cpp/neteq_shim.{h,cc}` is a plain C ABI over `webrtc::NetEqImpl`, built the same way `DefaultNetEqFactory` builds it, with a `SimulatedClock`. The per-frame decision comes from `NetEqImpl::last_operation_for_test()`, which is public upstream, so nothing in WebRTC is patched. The decoder factory only knows L16 (`AudioDecoderPcm16B`) and Opus (`AudioDecoderOpusImpl` over the system libopus).

`build.rs` compiles the `neteq` target's sources from `modules/audio_coding/BUILD.gn` plus the minimum of `api/`, `rtc_base/`, `common_audio/` and `system_wrappers/` the linker asks for. A few honest workarounds:

- Two forwarding headers in `OUT_DIR`: `third_party/opus/src/include/*.h` points at the system libopus, and an empty `experiments/registered_field_trials.h` stands in for a GN-generated file.
- `cpp/link_stubs.cc` defines a handful of symbols that are referenced but unreachable (the default task queue factory, RTP packet parsing). Each one aborts if ever called.
- Built with `NDEBUG`, `RTC_DISABLE_LOGGING`, `RTC_DISABLE_TRACE_EVENTS`, and `-ffp-contract=off` so the compiler can't fuse multiply-adds that Rust won't.
- The generic C signal-processing kernels, never the NEON ones.

## The Rust side

Opus decodes through `src/libopus_decoder.rs`, an `AudioDecoder` that calls the same C libopus the C++ side links (FFI, no libopus dependency in the `neteq` crate). It mirrors the ParsePayload, FEC, DTX and PLC behaviour of `neteq/src/codecs/opus.rs`, so both sides run identical codec arithmetic and the gate tests NetEq, not the codec. `--opus-decoder libopus` (default) is gated. `--opus-decoder ropus` swaps in the `ropus` decoder the crate ships and is a report only: its exit code ignores Opus diffs.

The known `ropus` divergence: `ropus` 0.12.18 drifts from C libopus in codec-internal CNG and in hybrid-fullband PLC after roughly the fourth concealed frame. That makes `dtx` and `long_mixed` differ on Opus, mono and stereo, with `ropus` only. The drift does not stay small. Measured maximum sample difference: `dtx` 48 (mono) and 66 (stereo), `long_mixed` 40 (mono) and 5500 (stereo). Stereo `long_mixed` reaches an SNR of 2.1 dB with 51% of samples bit-exact. Everything else is bit-exact with either decoder.

`NetEqUnderTest` (in `src/under_test.rs`) is the surface both implementations go through. `CNetEq` implements it today. `src/rust_adapter.rs` drives our Rust NetEq behind the `rust-neteq` feature.
