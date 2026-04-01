# Bot A/V Sync Diagnostics — Code Review

Review of the `bot-av-sync-diagnostics` branch (3 commits on top of main).

## Value Assessment

**Worth contributing.** The branch transforms the bot from a toy prototype into a
practical test/diagnostic tool. Five real problems solved:

1. **A/V sync** via shared media clock — previously audio and video drifted independently
2. **Real VP9 encoding** — the old bot sent mock video data that couldn't test actual decode paths
3. **WebSocket transport** — most deployments use WS, not WT; the bot previously only supported WT
4. **RX quality diagnostics** — turns the bot from write-only load generator into a diagnostic probe (jitter, sequence gaps, A/V sync delta)
5. **PCM buffer gradual speedup** — fixes audible glitches in the browser client's jitter buffer

The conversation mode and EKG waveform are nice bonuses for demos and visual sync verification.

## Critical — Fix Before Merge

- **JWT token logged at INFO level** (`main.rs` lobby URL) — the URL contains the full JWT
  as a query parameter. Redact or strip query params from the log.
- **Contradictory docs about `jwt_secret` encoding** — README says "Base64-encoded" but
  `token.rs` explicitly uses raw UTF-8 bytes (`jwt_secret.as_bytes()`). Fix the docs to
  match the code.

## Should Fix

- **`bot/.gitignore:3`** — `config-cc7.yaml` is environment-specific. Use `config-*.yaml`
  or remove it.
- **`inbound_stats.rs:56`** — `_my_user_id` parameter is accepted but unused. Remove it or
  use it to filter the bot's own packets.
- **`audio_producer.rs:44-46`** — `wav_duration` doesn't account for multi-channel WAVs.
  `hound::WavReader::len()` returns samples, not frames. Works today because conversation
  generator is mono, but will silently break A/V sync with stereo files.
- **`audio_producer.rs:150`** — Hardcoded 48kHz regardless of WAV file sample rate. A
  44.1kHz WAV would play at the wrong speed.
- **`pcmPlayerWorker.js:180`** — `targetLevel` property defined but never referenced
  anywhere.
- **`websocket_client.rs:106-108`** — `let _ = data;` in ping handler is dead code
  (tokio-tungstenite auto-pongs).

## Minor / Suggestions

- Per-frame JPEG decode+resize is CPU-expensive; at minimum cache the last decoded frame
  for consecutive identical indices
- URL scheme conversion via `replacen` is fragile; consider validating scheme against
  transport type
- Manual CLI arg parsing won't handle `--config=path` syntax; fine for now but consider
  `clap` if more flags come
- Stereo PCM streams still use burst-drop (speedup only implemented for mono) — known
  limitation, just track it
- Frame count vs loop duration mismatch silently clamps to last frame; worth a WARN log on
  startup

## What Looks Good

- Shared media clock design is clean and correct
- `build_heartbeat_packet()` properly shared between WS and WT clients
- `InboundStats` module well-structured with correct jitter calculation
- VP9 keyframe forcing at loop boundaries shows real codec understanding
- PCM gradual-speedup with linear interpolation is the right approach (8% max is
  perceptually inaudible)
- `TransportClient` enum cleanly abstracts WS vs WT without unnecessary trait objects
- Error handling is consistently non-panicking in media paths
- README documentation is thorough and above average
