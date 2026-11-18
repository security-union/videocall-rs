# neteq

A line-by-line Rust port of Google WebRTC's NetEq: the jitter buffer, the decision logic, and the fixed-point packet loss concealment and time stretching that Chrome uses to play remote audio. It's part of [videocall.rs](https://github.com/security-union/videocall-rs).

The port tracks WebRTC at rev `6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd`. The harness in [`parity/`](parity) builds the real C++ NetEq and diffs it against this crate tick by tick: every output sample, every operation, every statistic. PCM16B is the gate, and it matches on all 80 scenarios.

## What NetEq does

The example I give people is a water tower. Packets arrive in bursts, like a pump that runs when it feels like it. The speaker drinks at a steady 10 ms at a time, like the taps in town. The tower in between keeps enough water that the taps never notice the pump.

NetEq picks the water level for you. It watches how late packets arrive and sets a target delay that covers about 95% of them. When the tank is too full it plays audio a little faster (accelerate). When it's running low it stretches what it has (preemptive expand). When it runs dry it makes up audio that sounds like what came before (expand), which is where the water tower analogy runs out.[^1]

## Using it

Insert packets as they arrive, call `get_audio` every 10 ms, read stats when you want them. This is the core of [`examples/basic_usage.rs`](examples/basic_usage.rs):

```rust
let mut neteq = NetEq::builder(Config::default())
    .clock(Arc::new(clock.clone()))
    .build();
neteq.set_codecs(&codecs);

let info = RtpPacketInfo::new(&header, Some(clock.current_time_us()));
neteq.insert_packet(&header, &tone_packet(n * SAMPLES_PER_PACKET), info)?;

neteq.get_audio(&mut frame, None)?;
let operation = neteq.last_operation();
```

The rest of that file builds a jittery stream of tone packets on a simulated clock and prints what NetEq did with them. Run it with `cargo run -p neteq --example basic_usage`.

One thing trips people up: `RtpHeader::timestamp` counts samples of the codec clock (48 kHz for Opus), not milliseconds. NetEq matches packets to the playout point by timestamp, so wall-clock milliseconds make it expand on nearly every packet.

Codecs come from `codecs::BuiltinAudioDecoderFactory`: PCM16B (`L16`), Opus on [ropus](https://crates.io/crates/ropus) (pure Rust), and RFC 3389 comfort noise. RED and telephone-event are handled by NetEq itself.

## Statistics

`network_statistics()`, `current_network_statistics()`, `lifetime_statistics()` and `operations_and_state()` are the WebRTC structs with the WebRTC field meanings. Rates are Q14, like in WebRTC.

For dashboards, `telemetry::NetEqStats::from_neteq` packs those into one JSON object, adds per-second operation counts and reorder counters from `telemetry::TelemetryCollector`, and never resets NetEq's counters. That's the schema the browser worker posts and the [dashboard](DASHBOARD_README.md) reads.

## In the browser

Build with `--no-default-features --features web` for `wasm32-unknown-unknown`. You get `web::WebNetEq` and the `neteq_worker` binary, a Dedicated Worker that runs one NetEq per remote peer:

```bash
cargo build --target wasm32-unknown-unknown -p neteq --bin neteq_worker \
  --no-default-features --features web
```

Opus decodes with ropus inside the worker on every browser. NetEq decodes synchronously inside `GetAudio`, and the WebCodecs `AudioDecoder` is asynchronous, so it can't sit there.

Playback is pulled, not pushed. The page connects the worker to an AudioWorklet with a `MessageChannel`. The worklet keeps at least 30 ms queued and asks the worker for more 10 ms frames, two or three at a time, as the sound card consumes them, so `GetAudio` runs on the audio clock. Reply buffers travel back with the next request, so steady playback allocates no sample memory. NetEq measures arrival delay in `GetAudio` ticks, which is why a free-running timer isn't good enough. The message protocol is documented at the top of [`src/bin/neteq_worker.rs`](src/bin/neteq_worker.rs), and [`examples/worker-example`](examples/worker-example) is a standalone page that uses it.

Configuration matches Chrome's audio receiver: WebRTC defaults, no minimum delay floor, smart flushing off, and muted state on, so a silent peer costs almost nothing per tick.

## Examples

| Example | What it shows |
|---|---|
| `basic_usage.rs` | Insert, pull, read stats, on a simulated clock |
| `neteq_player.rs` | A WAV through Opus, simulated jitter and reordering, played from the sound card callback |
| `worker-example/` | The browser worker and worklet outside videocall.rs |

`cargo run -p neteq --example neteq_player --release -- song.wav --max-jitter-ms 80 --json-stats` also writes `neteq_stats.jsonl` for the dashboard.

## Features

| Feature | Default | What it adds |
|---|---|---|
| `native` | yes | ropus Opus decoder, and the deps for the player and dashboard |
| `audio_files` | yes | WAV reading for `neteq_player` |
| `web` | no | `WebNetEq`, the worker binary, ropus |
| `tools` | no | `tools::NetEqTest`, the simulation driver the tests and harness share |
| `parity` | no | Per-tick observer hooks for the harness, compiled out otherwise |

## License

MIT OR Apache-2.0 for our code. Most of this crate is a translation of WebRTC source, so those parts stay under WebRTC's BSD-3-Clause license; the notice and the `PATENTS` grant are in [`LICENSE-WEBRTC`](LICENSE-WEBRTC). Hence the crate's `(MIT OR Apache-2.0) AND BSD-3-Clause`.

[^1]: For the nerds: expand isn't interpolation. It runs LPC analysis on the last pitch period, then synthesizes from a mix of that periodic excitation and filtered noise, fading toward background noise the longer the gap lasts. That's `neteq/expand.rs`, ported from `modules/audio_coding/neteq/expand.cc`.
