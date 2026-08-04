# bots-app

Browser-driven bot CLI for videocall meetings. Runs a real Chrome instance via Playwright so the bot exercises the same WASM / WebCodecs / WebTransport code path a human peer would — used to recreate real-life issues against the deployed meeting stack while a human peer evaluates the result.

See discussion [#793](https://github01.hclpnp.com/labs-projects/videocall/discussions/793) for the design discussion.

## Overview

`bots-app` is a CLI with several subcommands:

- `run` launches one or more bots into a meeting. With `--ctl-port` it also exposes a local HTTP control API so a long-lived fleet can be introspected and mutated without restarting.
- `ctl <subcommand>` talks to a running orchestrator over `127.0.0.1` to list bots, change TTL, swap netsim profiles, mute/unmute, toggle camera, duplicate, and leave/kill.
- `dashboard` opens a browser-based UI on top of the same control API. See [`dashboard/README.md`](dashboard/README.md).
- `gen` emits a meeting-config YAML with N randomly-shuffled participants (deterministic given `--seed`).
- `prep-assets` builds per-participant audio + video files for Chrome's fake-device flags.
- `login` and `sso-login` capture Playwright storage state for OAuth- and SSO-gated targets.

The bot:

- launches headed Chrome with a configurable `--ttl` lifetime and a clean leave-meeting on TTL expiry or SIGINT/SIGTERM,
- auto-fills the homepage display-name form, clicks "Join Meeting" when shown, then clicks "Start camera" and "Unmute Microphone" so media actually flows — no human-in-the-loop required after launch,
- prepares per-participant fake audio (stitched WAV from `bot/conversation/lines/*.wav`) and fake video (y4m from `bot/assets/costumes/<name>/talking.mp4`) on demand,
- wires those files into Chrome via `--use-file-for-fake-{audio,video}-capture`,
- authenticates via:
  - **JWT cookie injection** for local / HCL daily / preview targets,
  - **Captured Playwright storage state** (`bots-app login`) for `app.videocall.rs` and any other real-OAuth-protected target,
  - **HCL SSO state** (`bots-app sso-login`) loaded _in addition to_ the JWT cookie for HCL-gated targets that sit behind the corporate SSO portal,
  - **Form login** (`--auth form-login`) — drives the identity provider's own username/password form using the `BOT_EMAIL` / `BOT_PASSWORD` env vars, for a self-hosted identity-service target (the labsworkspace videocall deployment). No pre-captured state needed, but the target must run the identity app (NOT Google OAuth) and the login accounts must already exist.

Backend is auto-picked by hostname (JWT for local/HCL/preview, otherwise storage-state) unless `--auth` is set. **`form-login` is never auto-picked** — it must be requested explicitly (`--auth form-login`, or `auth: form-login` in a `--config` file) so ambient `BOT_EMAIL` / `BOT_PASSWORD` can't be typed into a third-party login form. As defense-in-depth it also refuses to submit credentials to a known public IdP (Google, Microsoft, …).

## Production-UI touchpoints (invariant)

The bots-app stack is engineered to keep its footprint in the shipping Dioxus UI to an absolute minimum. The **only** prod-UI changes required for the bots to work are four `data-testid` markers the bot's Playwright driver uses to detect waiting-room / rejection / error states:

| File                                       | Line | `data-testid` value        |
| ------------------------------------------ | ---- | -------------------------- |
| `dioxus-ui/src/pages/meeting.rs`           | 574  | `meeting-waiting-for-host` |
| `dioxus-ui/src/pages/meeting.rs`           | 602  | `meeting-rejected`         |
| `dioxus-ui/src/pages/meeting.rs`           | 630  | `meeting-error`            |
| `dioxus-ui/src/components/waiting_room.rs` | 420  | `meeting-waiting-room`     |

(Each marker is preceded by a `// data-testid added for the bots-app …` comment so the intent is visible in the source.)

**This is the documented invariant.** Anything beyond these four attributes — runtime hooks, conditional rendering, exported globals, debug surfaces — does **not** belong in `dioxus-ui` for bots-app's sake. The bot exercises the same WASM / WebCodecs / WebTransport code path a human peer would; if it needs more visibility into a state, the right move is usually to add a stable `data-testid` on UI that already exists, not to add new prod code paths. Reviewers and future contributors: please preserve this scope. If a change to `bots-app` looks like it requires more than a new `data-testid`, flag it for design discussion before landing.

## Usage

```bash
# from the repo root
cd e2e
npm install               # one-time; pulls tsx + commander + vitest
npm run bot -- run \
  --meeting-url https://app.videocall.fnxlabs.com/meeting/TonyBots \
  --participant alice \
  --ttl 5m
```

The bot opens a headed Chrome window, joins the meeting as `alice`, and holds the session for the configured TTL. On TTL expiry (or `Ctrl+C` / SIGTERM) the bot clicks the meeting's "Hang Up" button, waits briefly for the leave-meeting API call to settle, then exits.

Set `--ttl infinite` for a session that only ends on signal.

### Clock video mode

Pass `--video-mode clock` to publish a live 1280x720 wall clock as the
bot's camera with a silent audio track. The default, `--video-mode costume`,
and the accepted `file` alias preserve the existing manifest/override-backed
fake-device behavior.

```bash
npm run bot -- run \
  --meeting-url https://app.videocall.fnxlabs.com/meeting/TonyBots \
  --users 6 \
  --video-mode clock \
  --headless
```

Clock mode also applies to SSH-launched bots when `videoMode: "clock"` is
included in the control-API launch request (`POST /launch` / `/launch/multi`),
either from the CLI, the dashboard launch forms, or a direct API call. The
dashboard supports selecting clock mode for single- and multi-bot launches.
Freeze/sync checks
should use a tolerance rather than exact equality because capture, encode,
transport, and decode add real jitter near second and color boundaries. For SSH
launches, keep the local and remote hosts NTP-synced so clock skew is not
reported as false lag.

## Multi-bot mode (`--users N`)

To fill a meeting around a human peer, pass `--users N` instead of `--participant <name>`. The bot picks the first N named participants from the manifest in order (alice, bob, carol, dave, eve, ...) and launches them concurrently in one Node process. All bots share the same `--ttl`, `--meeting-url`, and auth backend.

```bash
cd e2e
# Prereq: prep audio + costumes for at least N participants
npm run bot -- prep-assets --participants alice,bob,carol --costume-source /tmp/costume-videos

npm run bot -- run \
  --meeting-url https://app.videocall.fnxlabs.com/meeting/TonyBots \
  --users 3 \
  --ttl 5m
```

Each bot opens its own headed Chrome window. SIGINT (Ctrl+C) signals all of them to leave cleanly before the parent exits. Default cap is 10 bots per invocation; raise with `--max-users <N>` if you need more (and your laptop can handle it — each bot is ~0.5-1 GB RAM).

An error in one bot's launch is logged and doesn't take the others down.

## Seeded random-N matrix (`gen` + `run --config`)

`bots-app gen` emits a meeting-config YAML with `--count` randomly-shuffled participants. Same `--seed` always produces the same picks, so any bug surfaced by a random run is reproducible by re-running with the same seed.

```bash
cd e2e
# Emit a 5-bot config to stdout (or --out path)
npm run bot -- gen \
  --count 5 \
  --seed 42 \
  --meeting-url https://app.videocall.fnxlabs.com/meeting/TonyBots \
  --ttl 5m \
  --out /tmp/meeting-42.yaml

# Replay it
npm run bot -- run --config /tmp/meeting-42.yaml
```

The generated file looks like:

```yaml
meeting_url: https://app.videocall.fnxlabs.com/meeting/TonyBots
ttl: 5m
bots:
  - participant: pete
  - participant: grace
  - participant: mona
meta:
  seed: 42
  generated_at: 2026-05-13T23:05:42.506Z
```

By default `gen` only picks from **costumed participants** in the manifest (the 19 named characters with `costume_dir`). Pass `--include-observers` to also pick from observer-NN seats — useful when you specifically want a meeting filled mostly with receive-only bots. Note that observer bots show up as Chrome's default fake pattern with no audio, since `prep-assets` doesn't produce any artifacts for them.

Meeting-config YAML files accept per-bot TTL overrides and a per-bot or meeting-level `network:` field.

## Control API (`--ctl-port` + `bots-app ctl`)

When `bots-app run` is invoked with `--ctl-port <port|auto>`, the orchestrator becomes long-lived and exposes an HTTP control surface so the running fleet can be introspected and mutated without restarting the process. Without `--ctl-port` the orchestrator behaves identically but stays headless of any control surface — the API is strictly opt-in.

```bash
cd e2e
# Start the orchestrator with the control surface enabled. `auto`
# lets the kernel pick a free ephemeral port — recommended.
npm run bot -- run \
  --meeting-url https://app.videocall.fnxlabs.com/meeting/TonyBots \
  --users 3 \
  --ttl 30m \
  --ctl-port auto

# In another shell:
npm run bot -- ctl list
# BOT_ID                                PARTICIPANT  STATUS      TTL_REMAINING  NETWORK  MEETING_URL
# 7f3b2d1e-1234-...                     alice        in-meeting  1799s          -        https://...
# c0ffee23-aaaa-...                     bob          in-meeting  1798s          -        https://...

# Add a fourth bot mid-flight by duplicating an existing one:
npm run bot -- ctl duplicate 7f3b2d1e-1234-... --participant frank --ttl 5m

# Extend a bot's TTL without restarting it:
npm run bot -- ctl ttl 7f3b2d1e-1234-... --extend 10m

# Swap a bot's netsim profile (forces a reconnect — see caveat below):
npm run bot -- ctl tune c0ffee23-aaaa-... --network lossy_mobile

# Mute / unmute / camera off / camera on:
npm run bot -- ctl mute 7f3b2d1e-1234-...        # mutes
npm run bot -- ctl mute 7f3b2d1e-1234-... --off  # unmutes
npm run bot -- ctl video 7f3b2d1e-1234-...       # camera off
npm run bot -- ctl video 7f3b2d1e-1234-... --on  # camera on

# Graceful leave (clicks HangUp in-browser) vs force-kill:
npm run bot -- ctl leave 7f3b2d1e-1234-...
npm run bot -- ctl kill c0ffee23-aaaa-...
```

### Subcommands

| Subcommand                                                                  | Endpoint                   | Notes                                                                             |
| --------------------------------------------------------------------------- | -------------------------- | --------------------------------------------------------------------------------- |
| `ctl list`                                                                  | `GET /bots`                | Table of every live + recently-finished bot.                                      |
| `ctl status <id>`                                                           | `GET /bots/:id`            | One bot's full detail as JSON (machine-parseable).                                |
| `ctl leave <id>`                                                            | `POST /bots/:id/leave`     | Clicks HangUp + tears the browser down cleanly.                                   |
| `ctl kill <id>`                                                             | `DELETE /bots/:id`         | Skips graceful leave; for tests + emergencies.                                    |
| `ctl ttl <id> --set <dur>` / `--extend <dur>`                               | `POST /bots/:id/ttl`       | Absolute set or additive extend (e.g. `--set 10m`, `--extend 5m`).                |
| `ctl tune <id> --network <profile>`                                         | `POST /bots/:id/network`   | Validates against `NETSIM_PRESETS` on both sides. Reconnects (see caveat).        |
| `ctl mute <id> [--off]`                                                     | `POST /bots/:id/mute`      | `mute` mutes; `mute --off` unmutes.                                               |
| `ctl video <id> [--on]`                                                     | `POST /bots/:id/video`     | `video` turns camera off; `video --on` turns it back on.                          |
| `ctl duplicate <id> [--participant <name>] [--ttl <dur>] [--network <pro>]` | `POST /bots/:id/duplicate` | Clones the source bot's config, applies overrides, launches the duplicate.        |
| `ctl <any>` — `--state-file <path>` / `--port <port> --token <tok>`         | (any of the above)         | Override token-file auto-discovery (e.g. for tests against an explicit instance). |

There's also an unauthenticated `GET /healthz` for readiness probes — returns `{ ok: true, bots: <count> }`.

### Security model

- At startup the orchestrator generates a 32-byte CSPRNG bearer token (64 hex chars) and writes `e2e/bots-app/run/ctl-<pid>.token` with mode `0600` (owner read/write only).
- **Token location is overridable via `BOT_CTL_STATE_DIR`** (issue #2157). The token file defaults to `--assets-dir` / `--run-dir`; setting this env var to a non-empty path writes it there instead, leaving run artifacts on `--assets-dir`. The K8s fleet uses this to keep the cleartext token off the retained artifacts PVC — see [Run artifacts persist deliberately](#run-artifacts-persist-deliberately). Unset (a plain `docker run`, or local dev) keeps the pre-#2157 behavior. Note that `ctl`'s auto-discovery is _not_ env-overridden — it scans exactly `--run-dir`, so when the override is in use point `ctl` at it with `--run-dir` / `--state-file`, or use `--port` + `--token`.
- Every endpoint except `/healthz` requires `Authorization: Bearer <token>`.
- The control server binds to `127.0.0.1` only — no network exposure.
- `e2e/bots-app/run/` is already `.gitignore`d, so token files never get committed. The token never leaves disk (it isn't logged or echoed to stdout); it's only written to the file mode-0600 token file.
- `ctl` auto-discovers the most-recently-started orchestrator's token file under `--run-dir` (default `e2e/bots-app/run`). Override with `--state-file <path>` or `--port <port> --token <token>`.

### Operational caveats

- **Network swap forces a reconnect.** `POST /bots/:id/network` (and `ctl tune`) rewrites the bot's URL with the new `?netsim=<profile>` param and re-navigates. The bot drops the meeting, re-runs `joinMeetingAndEnableMedia`, and rejoins the grid. This is intentional — the netsim shim is installed at client startup, so there's no way to swap profiles in place without a fresh page load. If you need to compare the same participant on two different profiles concurrently, use `ctl duplicate <id> --network <new_profile>` and leave the original running.
- **Done entries linger for ~60s.** A bot that completes its TTL or is leaved via `ctl leave` stays in `ctl list` (with `status=done` and a `finishReason`) for ~60 seconds before being swept. This lets a follow-up `ctl list` see the recent finish.
- **Dynamic add only via `ctl duplicate`.** There's no `ctl spawn <from-scratch>` today; new bots have to be cloned from an existing in-flight bot. That covers the canonical "fill a meeting around a human peer, then add one more" case; an arbitrary-participant spawn endpoint can be layered in later without a schema change.

## Browser dashboard (`bots-app dashboard`)

`bots-app dashboard` opens a browser-based UI for launching and managing bots. It is **self-contained**: it spawns the orchestrator + ctl server in the same Node process and serves the React UI on port 5174 by default. No separate `bots-app run --ctl-port auto` terminal is needed.

```bash
cd e2e
npm run bot -- dashboard
```

Highlights:

- Launch form covers all the `run` options (meeting URL, participant, TTL with suggestion chips, network preset, headless, auth backend, costume / audio).
- Per-bot row controls: extend / set TTL, leave, force-kill, mute, toggle camera, share screen, duplicate.
- Auth backend radio offers three options: **JWT (cookie injection)**, **Storage State (replay OAuth)**, and **Guest (no auth)** — the last one is used for meetings that allow guest join.
- Launch form is grouped into Meeting / Identity / Behavior / Assets / Runtime sections with per-field help popovers (hover, click, or focus to open).
- **Run Profiles** save the current set of bot configurations under a name and re-launch the whole group with one click. Profiles persist to `<runDir>/profiles/<name>.json` and survive restarts.
- In-app **Help** page documents auth backends, network profiles, run profiles, troubleshooting, and the dashboard architecture.
- Attach-mode is supported for headless / scripted setups: pass `--ctl-port` + `--ctl-token` (or `--ctl-token-file`) to point the dashboard at an externally-managed daemon. In attach mode the dashboard auto-discovers the token file (`run/ctl-*.token`) and injects the bearer token server-side — the browser never sees it.
- Run-location pick list exposes "Local machine" today; "Cloud VM", "SSH-able host", and "Docker container" are placeholders for future work.

Implementation lives under `e2e/bots-app/dashboard/` with its own `package.json`, build, and test surface — no dependencies leak into the parent `e2e/` workspace. See [`dashboard/README.md`](dashboard/README.md) for the security model and dev workflow.

## Network simulation (`--network <profile>`)

Both `bots-app run` and `bots-app gen` accept `--network <profile>`, and meeting-config YAML files accept `network:` at both the meeting level and per-bot. When set, the bot's meeting URL is rewritten to include `?netsim=<profile>` before navigation — the in-tab `videocall-client` (built with `--features netsim`) installs the matching shim on its WT + WS send paths to mimic a degraded peer. Without that build flag the URL param is parsed by the browser but silently ignored.

Valid profiles: `none`, `good_wifi`, `good_4g`, `congested_wifi`, `lossy_mobile`, `satellite`, `dialup`.

## Receiver low-power caps (`--max-received-layer` / `--skip-canvas-paint`, #2068/#2069)

Two per-bot knobs cut a bot's **receive-side** cost so a room holds more bots before the box saturates — room capacity is TOTAL decode load, not bot count:

- `--max-received-layer <N>` (env `BOT_MAX_RECEIVED_LAYER`): cap the received simulcast layer. `0` = base rung only (lowest decode CPU); unset = no cap. Cuts **decode** CPU (#2068).
- `--skip-canvas-paint <bool>` (env `BOT_SKIP_CANVAS_PAINT`): decode-and-drop — skip the per-tile `drawImage`. Saves **paint/GPU** only; decode still runs (#2069). Unset = inherit the deployment.

They are independent and composable. The CLI flag wins over the env var. Both are **launch-time only**: they are injected as `window.__APP_CONFIG` overrides before the first navigation, via an accessor whose setter merges them over whatever `config.js` assigns (production `config.js` does `window.__APP_CONFIG = Object.freeze({...})`; the setter intercepts that assignment). The served client parses `__APP_CONFIG` exactly once and freezes it, so these **cannot** be toggled at runtime through the control API — there is deliberately no `/control` endpoint for them.

Effect requires the DEPLOYED client to carry the knobs (videocall-client PR #2078). Against an older deployment the fields are present in `__APP_CONFIG` but ignored (harmless). Each bot logs a post-join assertion (`receiver caps verified in __APP_CONFIG` or a `WARNING`) so a bot that is NOT actually capped is visible per run rather than silently skewing the load test.

The StatefulSet fleet (`k8s/statefulset.yaml`) leaves BOTH **unset by default** — bots are realistic full-decode clients, so a run reproduces the same total-room decode load (and any decode-saturation freeze) a real participant would. Enable the caps only to trade realism for scale: cap per-bot decode/paint so the box holds more bots while you load the _server_ with many participants (`BOT_MAX_RECEIVED_LAYER=0`, and optionally `BOT_SKIP_CANVAS_PAINT=true`).

## Resource capture + `RESOURCE_STARVED` verdict

Every `bots-app run` (and the self-hosted `bots-app dashboard`) forks a background sampler for the run's duration that measures the box the bots actually run on — so a self-starved run (the host saturating its own CPU) is flagged instead of being mistaken for a product regression. There is nothing to enable; it is on by default and degrades to a no-op on a box without `/proc` (e.g. a macOS laptop).

Every ~5s the sampler (`scripts/resource-sampler.sh`, POSIX bash + `/proc`, no dependencies) appends a block of **raw** `/proc` counters — CPU jiffies (overall, per-core, steal), load average, memory (used/available/swap), NIC rx/tx bytes, and per-Chrome/orchestrator process cpu-jiffies + RSS — to `<run-dir>/resource/<label>-raw.csv`. At run end the raw counters are diffed in TypeScript into a **derived**, Prometheus-overlay CSV (`<label>-derived.csv`, epoch-seconds first) and a summary (`<label>-summary.txt`). All the delta math, aggregation, and the verdict live in `src/resource/` and are unit-tested there.

The run prints a prominent, greppable verdict banner:

- `RESOURCE_STARVED` when **either** rule fires: peak overall CPU stayed above 85% for 3+ consecutive samples (~15s sustained), **or** any bot's reported encoder FPS dipped below the base rung (default 5 fps). A starved run's client-signal regressions (encoder fps, RTT, sheds) should be treated as confounded by box saturation, not a product change.
- `RESOURCE_OK` otherwise.

For SSH-launched bots the **same** shell sampler is piped over `ssh … bash -s` to each remote box (the box whose CPU matters), and its CSV is copied back to `<run-dir>/resource/<label>-<host>-raw.csv` at run end — mirroring how remote bot commands are shipped.

**Per-bot encoder FPS is captured by polling the `window.__videocall_encoder_fps` global the client publishes (#2057).** Only positive readings are recorded; absent/`undefined` values, and a `0`, are skipped as no data, so a cold-start/idle bot is not mis-flagged as starved.

> **Known gap (#2079, open): a total encoder stall is currently NOT flagged by the FPS rule.** The client does not publish `0` only when it lacks data — since #2060 it decays `current_fps` to `0` and publishes that on a genuine stall, so a `0` reaching the bot is real information the sampler discards. Only the independent CPU rule backstops a total stall today. This is deliberate rather than an oversight: `0` cannot distinguish a starved _box_ (what the verdict is for) from a _wedged encoder_ — a product bug that must not be reported as a confounded run — and four consumer-side heuristics were each defeated by a reachable case (see #2079). The fix belongs at the source, as a stall signal distinct from no-data.
>
> **Practical consequence when reading a report — the symptom is misleading, not silent.** `RESOURCE_OK` does not rule out a frozen encoder. Worse: during a total stall every reading is `0`, all of them are discarded, so no bot ever lands in `fpsByBot` and the per-bot line reads
>
> `[resource] bot encoder fps: not reported (no window.__videocall_encoder_fps readings — client build without #2057, or camera off / encoder never warmed the whole run)`
>
> That is wrong in the stall case: the publisher **is** running and **is** publishing `0`s — they are just dropped by `coerceEncoderFps`. The run report cannot currently distinguish a stalled encoder from a client build lacking the publisher or a bot that never started its camera.
>
> **No currently-available surface resolves the ambiguity** — that is why #2079 is open rather than worked around here. Every candidate fails, and the two that look most promising fail in the most misleading way:
>
> - **The run directory** carries no per-bot encoder-fps series at all.
> - **`encoder_output_fps` (protobuf)** is `> 0`-gated (`health_reporter.rs`), so a stall makes the field ABSENT rather than `0` — the same conflation one layer up. Note it is literally the same value: the health loop reads the `output_fps` atom ONCE and feeds it both to this field and to the `window.__videocall_encoder_fps` global.
> - **`videocall_encoder_output_fps` (Prometheus)** is fed only from that field, so the server's `if let Some(fps) = …` never fires during a stall and the gauge is never updated. It is a `GaugeVec` swept only on session disconnect, so for a still-connected wedged bot it **retains its last healthy value** — actively asserting a working encoder rather than merely being absent.
> - **`content_staleness_ms`** is a RECEIVE-side gauge for a peer's painted video, gated on `fps_received > 0`, and holds its `0.0` default ("at live") when fps is 0. It cannot observe a local frozen encoder.
> - **The `.log.gz` console upload** cannot carry it either. The only console line reporting encoder output fps is a `log::trace!` inside the layer-0 chunk-output callback (`camera_encoder.rs`), so it structurally cannot fire when no chunks are being produced — and it was deliberately demoted `debug!`→`trace!` specifically to stay off when console-log collection bumps to Debug. The bot also forwards only `error`-level page console messages to stdout.
>
> Diagnosing a suspected stall today therefore means inferring it from adjacent signals rather than reading it directly.
>
> 📌 **The SCREEN encoder is the exception (#2147).** `videocall_screen_encoder_output_fps` /
> `screen_encoder_output_fps` (proto field 109) is deliberately **not** `> 0`-gated at either
> the source or the server, so a screen encoder that is bound but producing nothing reports an
> honest `0` instead of vanishing. It is the shape #2079 asks for, applied to the one encoder
> that previously had no publisher-side fps signal at all. It does **not** fix the camera
> ambiguity above (a separate metric, a separate encoder).
>
> ⚠️ **A `0` here does not mean "not sharing."** The client binds its screen encoder eagerly at
> Host mount, so it reports `0` while merely idle — the field is omitted only by a client that
> binds no screen encoder at all. Read it against `videocall_screen_sharing_active`: `0`+inactive
> is idle; `0`+active is sharing and is either static-and-fine (a static share goes quiet by
> design once its keyframe floor budget drains) or stalled. Receiver-side
> `content_staleness_ms` cannot separate those states once fps is 0 because the client publishes
> its `0.0` default then. Use `videocall_screen_encoder_stall_episodes_total` to identify a
> publisher tick-starvation freeze; fps alone remains ambiguous.
>
> ⚠️ **When looking for such a signal, check the guard, not just the metric name.** `videocall_video_fps` **used to be** gated the same way `encoder_output_fps` is — `metrics_server.rs` wrapped the `.set()` in `if video_stats.fps_received != 0.0`, so a genuine `0` was never written and the series held its **last healthy value** for a still-connected peer. That guard was **removed in issue 2145** (along with the identical one on `videocall_video_bitrate_kbps`): both now set unconditionally, matching `SCREEN_VIDEO_FPS` / `SCREEN_VIDEO_BITRATE_KBPS`, so a receiver-observed `0` is honest on camera and screen alike. The lesson still stands for any other metric: a gauge that lies is worse than one that is absent, and the guard is never on the line that names the metric — and it is not always on the same side of the wire. `encoder_output_fps` is a _separate_ metric whose `> 0` gate is **CLIENT**-side (`health_reporter.rs`, `if encoder_output_fps > 0`, so the field is omitted rather than zeroed); that gate is still live and is tracked by #2079, and 2145 did not touch it. `videocall_client_packets_sent_per_sec` also survives a zero — `pb.packets_sent_per_sec` is assigned unconditionally in `health_reporter.rs` and the server sets it from the `Option` — though audio traffic on the same connection will usually keep it nonzero, so it is corroborative rather than conclusive.

The per-bot client console-log upload (`.log.gz`, gated by the `consoleLogUploadEnabled` runtime flag and served by `meeting-api`) is a separate, browser-side mechanism this feature does not touch.

Not in scope (issue follow-ups): a `node_exporter` on the box for live Prometheus scraping, and any Prometheus/Grafana wiring.

## Authenticating against `app.videocall.rs`

For local / HCL daily / preview targets the bot mints a JWT cookie automatically. For `app.videocall.rs` (or any host that uses real Google OAuth), you first capture a Playwright storage state via:

```bash
cd e2e
npm run bot -- login videocall-bot-alice
# A headed Chrome opens. Log in normally with the Google account that
# should join meetings as "alice", then press Enter in the terminal.
# Captured session is saved to e2e/bots-app/run/auth/videocall-bot-alice.json.
```

Then run the bot with the same handle as `--participant`:

```bash
npm run bot -- run \
  --meeting-url https://app.videocall.rs/meeting/SomeRoom \
  --participant videocall-bot-alice \
  --ttl 5m
```

The bot auto-selects the storage-state backend because the hostname doesn't match a known JWT host. Pass `--auth jwt` or `--auth storage-state` to force a choice.

**Security:** the captured `auth/<account>.json` files contain real Google session tokens. `e2e/bots-app/run/` is gitignored — don't move these files out of it, don't share them, and rotate by re-running `bots-app login` whenever the Google session expires (typically every few weeks).

## Authenticating against HCL daily (`*.videocall.fnxlabs.com`)

HCL daily sits behind the corporate SSO portal AND the videocall app itself uses session-cookie auth. The bot needs **two** auth layers:

1. **HCL SSO state** — captured once via `bots-app sso-login`, lives in `e2e/bots-app/run/auth/hcl-sso.json`, lets the bot through the SSO challenge without an interactive auth step on every run.
2. **JWT cookie** — minted at launch time from the cluster's `JWT_SECRET`, authenticates the bot to the videocall app.

One-time setup per SSO session (typically hours to days, depending on HCL's policy):

```bash
cd e2e
npm run bot -- sso-login     # opens headed Chrome → complete SSO challenge → press Enter
# Captured cookies saved to e2e/bots-app/run/auth/hcl-sso.json (gitignored).
```

Then each bot run picks up both layers automatically:

```bash
export JWT_SECRET=$(kubectl --kubeconfig=$HCL_KUBECONFIG -n videocall get secret jwt-secret -o jsonpath='{.data.secret}' | base64 -d)
npm run bot -- run \
  --meeting-url https://app.videocall.fnxlabs.com/meeting/TonyBots \
  --participant alice \
  --ttl 5m
```

The terminal will log `auth: jwt + SSO state from .../hcl-sso.json (...)` confirming both layers are active. When the SSO session expires (you'll see the bot's page redirect to the SSO portal on next launch), re-run `sso-login` and you're back.

## Authenticating against labsworkspace (`--auth form-login`)

The labsworkspace videocall deployment (`*.videocall.labsworkspace.fnxlabs.com`) runs its own **identity-service** login form (a standard PKCE auth-code flow), not Google OAuth. For that target the bot can log in programmatically with a pre-created test account — no captured storage state needed:

```bash
cd e2e
export BOT_EMAIL='bot1@example.test'      # a pre-created labsworkspace account
export BOT_PASSWORD='…'
npm run bot -- run \
  --meeting-url https://app.videocall.labsworkspace.fnxlabs.com/meeting/bottest \
  --participant k8s-bot-1 \
  --auth form-login \
  --ttl 5m
```

Preconditions and guardrails:

- **`--auth form-login` is required** (or `auth: form-login` in a `--config` file). It is **never** auto-selected from the mere presence of `BOT_EMAIL` / `BOT_PASSWORD`, so exporting those creds and pointing at a Google-OAuth host (e.g. `app.videocall.rs`) will **not** type them into Google — that host stays on `storage-state`. As defense-in-depth the flow also refuses to submit to a known public IdP host (Google, Microsoft, …).
- The target must be running the **identity-service** app, and the **login accounts must already exist** (form-login does not register users).
- Credentials come from the environment only; a form-login bot is intentionally **not persistable** to a dashboard run profile.

## Running in Kubernetes (containerize + deploy, #2035)

`bots-app` ships a headless clock-mode container image and a single-pod manifest for running bots on `qsk8s` (the fleet lands in the dedicated `bot-load` namespace).

```bash
cd e2e/bots-app

# 1. Build (context is the repo root; image is pinned to linux/amd64).
#    PODMAN_REMOTE=1 uses the shared qsk8s podman builder; otherwise a local
#    docker/podman build runs. PUSH=1 publishes to Harbor (hclcr.io/hcllabs).
REGISTRY_USER="$HARBOR_USERNAME" REGISTRY_PASS="$HARBOR_PASSWORD" \
  PODMAN_REMOTE=1 PUSH=1 ./build.sh
# build.sh prints the immutable dated tag (0.1.0-YYYYMMDD-<sha7>) it also pushed;
# set k8s/bot-pod.yaml `image:` to that tag for a reproducible run.

# 2. One-time namespace + quota (ResourceQuota sized for ~20 bots).
kubectl apply -f k8s/namespace.yaml

# 3. Create the login creds Secret (see k8s/bot-creds.example.yaml for the
#    literal form). These are the form-login BOT_EMAIL/BOT_PASSWORD.
kubectl -n bot-load create secret generic bot-creds \
  --from-literal=BOT_EMAIL='bot1@example.test' \
  --from-literal=BOT_PASSWORD='…'

# 4. Copy the Harbor pull secret into the namespace (see the header comment in
#    k8s/bot-pod.yaml for the jq one-liner), then launch the pod.
kubectl apply -f k8s/bot-pod.yaml
kubectl -n bot-load logs -f videocall-bot-bottest

# Graceful leave: `kubectl -n bot-load delete pod videocall-bot-bottest` sends
# SIGTERM, which the orchestrator turns into a clean meeting-leave before exit.
```

The pod sets `BOT_AUTH=form-login` explicitly, so it does not rely on host auto-selection at all. See `Dockerfile`, `docker-entrypoint.sh`, and `k8s/` for the details.

## Running a fleet (StatefulSet, N bots — #2035)

For more than one bot, use `k8s/statefulset.yaml`. Each pod (`videocall-bots-<N>`) derives its identity from its ordinal: the entrypoint reads `${HOSTNAME##*-}` and selects `BOT_EMAIL_<N>` / `BOT_PASSWORD_<N>` from a fleet-wide `bot-accounts` Secret, and sets the display name to `bot-<N>`. So N bots join as N **distinct** accounts with no rename/host-race collisions.

```bash
cd e2e/bots-app

# Fleet accounts: one BOT_EMAIL_<n>/BOT_PASSWORD_<n> pair per ordinal 0..N-1.
# Build a local env-file (NEVER commit it), create the Secret, then shred it.
kubectl -n bot-load create secret generic bot-accounts --from-env-file=./bot-accounts.env

# Deploy + scale. Set the image tag from build.sh; TTL defaults to infinite.
kubectl apply -f k8s/service.yaml -f k8s/statefulset.yaml
kubectl -n bot-load scale statefulset/videocall-bots --replicas=10
kubectl -n bot-load get pods -w

# Collect the per-bot resource artifacts (#2032) — do this BEFORE scaling down.
# `kubectl cp` execs into a RUNNING pod, so once the replicas are gone there is no
# `videocall-bots-$i` to copy from. The DATA itself survives on a per-ordinal PVC
# (mounted at /var/lib/bots-run via BOT_RUN_DIR) and is not lost — but retrieving
# it after a scale-to-0 requires scaling back up, or mounting the claims in a
# separate collector pod. Collecting first is simply less work.
for i in $(seq 0 9); do
  kubectl -n bot-load cp "videocall-bots-$i:/var/lib/bots-run" "./run-artifacts/bot-$i"
done

# Stop: scale to 0 (pods leave the meeting gracefully on SIGTERM).
kubectl -n bot-load scale statefulset/videocall-bots --replicas=0
```

### Run artifacts persist deliberately

The resource sampler's `*-raw.csv` /
`*-derived.csv` / `*-summary.txt` are written to a `volumeClaimTemplates` PVC per
ordinal, because `BOT_RUN_DIR`'s default (`/tmp/bots-run`) is the container
filesystem — on the 2026-07-31 #2143 run every bot's CSV was destroyed by
`scale --replicas=0`, which made a pod-vs-Prometheus cross-check of the measured
per-bot CPU impossible after the fact. A StatefulSet keeps these claims across
restarts _and_ across scale-to-0, so **they are never reclaimed automatically** —
delete them deliberately when a run's data is no longer needed:

```bash
kubectl -n bot-load delete pvc -l app.kubernetes.io/name=videocall-bot
```

**…but the ctl token does NOT (issue #2157).** That same persistence is wrong for
a credential: the token file is the _cleartext fleet-wide_ control-API bearer
token, and a copy left on a claim nothing reclaims means **rotating the
`bot-ctl-token` Secret no longer invalidates it** (compounded by the nfs-subdir
provisioner creating each subdir `0777`). So the run dir holds artifacts only:

| Artifact                                        | Env var             | Path in-pod         | Volume                 | Survives `scale --replicas=0`? |
| ----------------------------------------------- | ------------------- | ------------------- | ---------------------- | ------------------------------ |
| `*-raw.csv` / `*-derived.csv` / `*-summary.txt` | `BOT_RUN_DIR`       | `/var/lib/bots-run` | `run-artifacts` PVC    | **Yes — deliberately** (#2032) |
| `ctl-<pid>.token` (cleartext bearer token)      | `BOT_CTL_STATE_DIR` | `/var/lib/bots-ctl` | `ctl-state` `emptyDir` | **No — deliberately** (#2157)  |

An `emptyDir` shares the pod's lifetime, so the token dies with the pod. The
entrypoint additionally `rm -f`s any `ctl-*.token` out of `BOT_RUN_DIR` at
startup — that is what retires the stale copies already sitting on PVCs
provisioned by the earlier deploy, which no sweep would otherwise ever touch
(only `ctl-*.token` is matched; the CSVs above are untouched, and the glob is
deliberately non-recursive so `<runDir>/resource/` is never entered). It sweeps
`BOT_CTL_STATE_DIR` too, which matters more than "ephemeral" suggests: an
`emptyDir`'s lifetime is the **pod**, not the container, so it survives the
container RESTARTS that this StatefulSet treats as routine (TTL reached, or a
crash — see the `k8s/statefulset.yaml` header). After a restart that dir is
therefore _not_ born empty, and the sweep is what clears the previous
container's token.

There is deliberately **no SIGTERM-time removal**: the entrypoint `exec`s the
Node process, so a shell `trap` could never fire. Note the scope of what the
`emptyDir` alone buys you — the token is destroyed when the **pod** goes away
(delete, eviction, `scale --replicas=0`), but a mere container restart leaves the
old file in place until the next startup sweep removes it.

⚠️ **Upgrading a fleet created before this claim existed requires delete+recreate**
— `volumeClaimTemplates` is immutable on a live StatefulSet, so `kubectl apply`
fails with _"updates to statefulset spec for fields other than … are forbidden"_.
Use `kubectl -n bot-load delete sts videocall-bots --cascade=orphan` (keeps the
pods running) then re-`apply`; a fresh namespace needs no special handling.

**Per-bot capability cap.** A container reports the _node's_ core count to the browser, so an un-capped bot over-commits simulcast layers. The StatefulSet sets `BOT_HW_CONCURRENCY=6` (→ 2 layers), which the bot spoofs into `navigator.hardwareConcurrency` before the client's capability check runs (`<6`→1 layer, `6..9`→2, `>=10`→3). Adjust per fleet to match the pod's real CPU budget.

## Remote per-bot control + network impairment (#2072)

Each pod runs a **token-authenticated control server** on `0.0.0.0:8080`, reachable in-cluster at `videocall-bots-<N>.videocall-bots.bot-load.svc:8080`. It drives that pod's bot: mute, camera, screen-share, leave, and per-pod network shaping via `tc`/netem.

Prereqs (once per fleet):

```bash
# Shared control-API token (the conductor holds the same one).
kubectl -n bot-load create secret generic bot-ctl-token --from-literal=token="$(openssl rand -hex 32)"

# netem needs the sch_netem kernel module on each node. This DaemonSet is the
# ONLY privileged component (the bot pods stay non-root); apply it BEFORE any
# netem action and wait for Ready (readiness == module loaded on that node).
kubectl apply -f k8s/netem-preload-daemonset.yaml
kubectl -n bot-load rollout status ds/netem-preload
```

(The image also `setcap cap_net_admin+eip`'s `tc` so the non-root bot can shape its own `eth0`: the pod grants `NET_ADMIN`, but a non-root process only receives it on `tc` via the file capability — K8s sets no ambient caps.)

## Scripted scenarios (`conduct` — #2072)

`bots-app conduct` runs a timeline against the fleet — it resolves each `bot: <N>` to its Service DNS name and fires actions on schedule.

```yaml
# my-scenario.yaml  (k8s/scenario.example.yaml is a ready-to-apply ConfigMap)
room: bottest
timeline:
  - { at: 0s, bot: 0, action: unmute }
  - { at: 5s, bot: 1, action: screenshare-on }
  - { at: 10s, bot: 2, action: netem, profile: lossy_mobile }
  - { at: 20s, bot: 1, action: screenshare-off }
  - { at: 40s, bot: 0, action: talk, durationMs: 15000 }
  - { at: 60s, bot: 2, action: netem-clear }
```

Actions: `mute`/`unmute`, `camera-on`/`camera-off`, `screenshare-on`/`screenshare-off`, `netem` (`profile:` or raw `delayMs`/`jitterMs`/`lossPct`/`rateKbit`), `netem-clear`, `talk` (`durationMs` — unmute then re-mute), `leave`. Run it as an in-cluster Job:

```bash
kubectl -n bot-load create configmap bot-scenario --from-file=scenario.yaml=./my-scenario.yaml
kubectl -n bot-load apply -f k8s/conductor-job.yaml
kubectl -n bot-load logs -f job/bot-conductor      # prints each action as it fires
```

### Full fleet deploy order

0. **Build + push the image from this commit** (`PODMAN_REMOTE=1 PUSH=1 ./build.sh`) and use the tag it prints in step 5. **No CI workflow builds this image** — publishing is manual, so a `kubectl apply` alone leaves the fleet on whatever tag was last pushed. This step is not optional for #2157: the stale-token sweep lives in `docker-entrypoint.sh`, which is baked into the image, so a manifest-only deploy silently omits it (see [Rotating `bot-ctl-token`](#full-fleet-deploy-order) below).
1. `k8s/namespace.yaml` (namespace + ResourceQuota)
2. Copy the `hclcr-io` pull secret into `bot-load` (jq one-liner in the `k8s/bot-pod.yaml` header)
3. `k8s/netem-preload-daemonset.yaml` + wait Ready — **only if using netem**
4. Create the `bot-accounts` and `bot-ctl-token` Secrets
5. `k8s/service.yaml` + `k8s/networkpolicy.yaml` + `k8s/statefulset.yaml` (with the tag from step 0); scale to N
6. `bot-scenario` ConfigMap + `k8s/conductor-job.yaml`

**Teardown:** scale the StatefulSet to 0; delete the conductor Job, the `bot-scenario` ConfigMap, the `NetworkPolicy`, and (if applied) the `netem-preload` DaemonSet.

**Security notes:** the control API is cluster-only (headless Service, no ingress) and bearer-token gated, AND `k8s/networkpolicy.yaml` (applied in step 5) restricts `:8080` ingress to the `bot-load` namespace (Calico-enforced deny-by-default) for defense-in-depth. netem shapes the pod's `eth0` egress, which also carries the control API's own responses — the validators cap impairment below total (lossPct ≤ 95, rateKbit ≥ 8) so a profile can't strand the pod before you can clear it.

**Rotating `bot-ctl-token`.** Recreate the Secret, then restart the fleet so every pod picks up the new value (`BOT_CTL_TOKEN` is injected at pod start, so a Secret edit alone does not reach a running pod):

```bash
kubectl -n bot-load create secret generic bot-ctl-token \
  --from-literal=token="$(openssl rand -hex 32)" --dry-run=client -o yaml | kubectl apply -f -
kubectl -n bot-load rollout restart statefulset/videocall-bots
```

Since #2157 that is sufficient — the old token existed only in the Secret and on each pod's `ctl-state` `emptyDir`, both gone with the pod. Before #2157 a copy also sat on each retained `run-artifacts` PVC and survived rotation.

> ⚠️ **Both #2157 behaviours need the REBUILT IMAGE, not just `kubectl apply`.** The token relocation is driven by `BOT_CTL_STATE_DIR` (a manifest change) **and** by `docker-entrypoint.sh`, which is baked into the image — and **no CI workflow builds this image**; publishing is a manual `PUSH=1 ./build.sh` (verified: nothing under `.github/workflows/` references `bots-app/Dockerfile` or `build.sh`). On a fleet still running a previously-pushed tag, `rollout restart` re-reads the Secret but runs the OLD entrypoint, which has no sweep — so every `run-artifacts` PVC keeps its pre-#2157 cleartext token and the rotation does **not** retire it, exactly the condition #2157 exists to end.
>
> So: build and push from this commit, set that tag on the StatefulSet, and only then treat the rotation as complete. To verify, exec into a pod and confirm `ls /var/lib/bots-run/ctl-*.token` is empty and the token is at `/var/lib/bots-ctl/`. If you cannot rebuild yet, delete the claims outright per [Run artifacts persist deliberately](#run-artifacts-persist-deliberately) — that retires the stale copies with no image dependency.

## Preparing assets (`prep-assets`)

`prep-assets` builds the per-participant audio + video files Chrome's `--use-file-for-fake-{audio,video}-capture` flags need. Run it once before launching bots that should send realistic media:

```bash
cd e2e
# Prereq: bot/conversation/manifest.yaml exists (run python3 bot/generate-conversation-edge.py)
# Prereq: costume MP4s unzipped under <costume-source>
npm run bot -- prep-assets \
  --costume-source /tmp/costume-videos      # or bot/assets/costumes/<name>/*.mp4 if you've kept them there
```

For each participant in the manifest, this:

1. Stitches their lines from `bot/conversation/lines/*.wav` into `e2e/bots-app/run/audio/<name>.wav` (ffmpeg concat with optional silence padding per the manifest's `pause_ms`).
2. Converts their costume's `talking.mp4` into `e2e/bots-app/run/costumes/<name>.y4m` (ffmpeg, 1280×720 @ 30fps, yuv420p).

Both steps are idempotent — re-runs only spawn ffmpeg when the source file is newer than the cached output. Output sizes: ~1.5 MB per audio WAV, ~370-390 MB per y4m (raw uncompressed). `e2e/bots-app/run/` is gitignored.

Flags:

```
bots-app prep-assets
  --manifest <path>         Path to bot/conversation/manifest.yaml (default: repo bot/conversation/manifest.yaml)
  --costume-source <dir>    Directory of <name>/talking.mp4 (default: repo bot/assets/costumes)
  --output-dir <dir>        Where to write run/audio + run/costumes (default: e2e/bots-app/run)
  --participants <list>     Comma-separated; defaults to every named participant in the manifest
```

Environment variables:

| Var           | Purpose                                                             | Default                    |
| ------------- | ------------------------------------------------------------------- | -------------------------- |
| `JWT_SECRET`  | HMAC secret for the session cookie. Must match the server's secret. | `dev-jwt-secret-change-me` |
| `COOKIE_NAME` | Session cookie name on the server.                                  | `session`                  |

## Flags

```
bots-app run
  --meeting-url <url>          Full meeting URL (required)
  --participant <name>         Single-bot: handle (alice/bob/...) or full email. Mutually exclusive with --users.
  --users <N>                  Multi-bot: launch N bots picking the first N manifest participants. Mutually exclusive with --participant / --config.
  --max-users <N>              Cap for --users (default 10)
  --config <path>              Multi-bot: load meeting-config YAML (from `gen` or hand-rolled). Mutually exclusive with --participant / --users.
  --display-name <name>        Display name (single-bot only; ignored in --users / --config modes)
  --headless                   Run Chrome headless (default: headed)
  --ttl <duration>             Bot lifespan — "<int>s|m|h" or "infinite" (default: 5m)
  --manifest <path>            Path to bot/conversation/manifest.yaml; pass "" to skip fake-device wiring
  --assets-dir <dir>           Directory of audio/<name>.wav + costumes/<name>.y4m (default: e2e/bots-app/run)
  --auth <backend>             Override auth backend: "jwt", "storage-state", "none" (guest join), or "form-login" (drive the identity-service login form via BOT_EMAIL/BOT_PASSWORD; explicit opt-in only, never auto-picked). Default: auto by hostname (jwt / storage-state).
  --storage-state-file <path>  Explicit storage-state JSON path (default: <assets-dir>/auth/<participant>.json)
  --sso-state-file <path>      HCL SSO state path (default: <assets-dir>/auth/hcl-sso.json; loaded only if present)
  --ctl-port <port|auto>       Bind a local HTTP control API. "auto" lets the kernel pick a free port. Token file written to run/ctl-<pid>.token (mode 0600), or to $BOT_CTL_STATE_DIR when that is set (#2157).

bots-app ctl <subcommand>      Control client; auto-discovers the most recent run/ctl-*.token (override with --state-file / --port + --token).
  list                         Tabular list of every bot in the registry
  status <id>                  Full bot detail as JSON
  leave <id>                   Graceful leave (HangUp + shutdown)
  kill <id>                    Force-kill (no graceful leave) — for tests
  ttl <id> --set <dur> | --extend <dur>   Set / extend a bot's TTL
  tune <id> --network <profile>           Swap netsim profile (forces a reconnect)
  mute <id> [--off]            Mute (default) or unmute (--off) the bot
  video <id> [--on]            Camera off (default) or on (--on)
  duplicate <id> [--participant <name>] [--ttl <dur>] [--network <profile>]
                               Clone this bot's config and launch the clone with optional overrides

bots-app login <account>
  --start-url <url>            Where to navigate headed Chrome (default: https://app.videocall.rs/)
  --assets-dir <dir>           Where to write auth/<account>.json (default: e2e/bots-app/run)

bots-app sso-login
  --start-url <url>            Where to navigate headed Chrome to trigger SSO (default: https://app.videocall.fnxlabs.com/)
  --assets-dir <dir>           Where to write auth/hcl-sso.json (default: e2e/bots-app/run)
  --out-file <path>            Override the output file location

bots-app gen
  --count <N>                  Number of bots in the generated config (required)
  --meeting-url <url>          Meeting URL baked into the generated config (required)
  --seed <S>                   RNG seed (integer; default: random per run)
  --ttl <duration>             Shared TTL baked into the generated config
  --manifest <path>            Manifest path (default: bot/conversation/manifest.yaml)
  --out <path>                 Write YAML to this file (default: stdout)
  --include-observers          Also pick from observer-NN seats (default: costumed participants only)
```

## Development

```bash
cd e2e
npm run ci:lint               # eslint + prettier + tsc
npm run test:unit             # vitest unit tests for bots-app/
npx playwright test --config bots-app/src/playwright.clock.config.ts # real-Chromium clock advancement test
```

## Remote hosts (SSH) — v1

The dashboard can launch bots on a remote machine using the operator's local `ssh` binary. Hosts are registered via the **Tools → Remote Hosts** card; once at least one host is registered, the launch form's **SSH-able host** radio activates.

How it works:

- The dashboard's Node sidecar stores host metadata at `e2e/bots-app/run/hosts.json` (mode `0o600`). No private keys live in this file — credentials are sourced from the operator's `ssh-agent` and `~/.ssh/config`.
- When SSH is selected, the orchestrator spawns the local `ssh` binary directly (`child_process.spawn("ssh", [...])`, no shell) and runs a single-line bash command on the remote host:
  ```
  ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new [-i <key>] user@host[:port] \
    "<shell> -lc '[ -f <profileFile> ] && . <profileFile>; <preCommand>; cd '\''<reposPath>'\''/e2e && npm run bot -- run --headless --ttl '\''<ttl>'\'' --meeting-url '\''<url>'\'' --participant '\''<p>'\'' [--video-mode '\''<mode>'\''] [--network '\''<net>'\''] [--auth '\''<auth>'\''] [--display-name '\''<name>'\'']'"
  ```
  Every dynamic substring is shell-escaped via the `shellEscape` helper (POSIX single-quote wrap + `'\''` for embedded quotes).
- The inner `cd … && npm run …` is wrapped in `<shell> -lc` so the remote shell runs as a **login shell** and sources the operator's profile. `<shell>` is the host's `shell` field (default `bash`); `bash -l` has a POSIX-defined login-shell init chain that always reads `~/.bash_profile`, so it's a safer default than `${SHELL:-/bin/bash}` (which expanded to `/bin/zsh` on zsh-default macOS hosts and missed operators whose nvm setup lived in `~/.bash_profile`).
- Each host carries three structured fields that shape the wrapper payload:
  - **`shell`** — bare name (`bash`, `zsh`, `sh`) or absolute path (`/opt/homebrew/bin/zsh`). Defaults to `bash` when unset. Becomes the `<shell>` token in `<shell> -lc …`.
  - **`profileFile`** — profile sourced via `[ -f <profileFile> ] && . <profileFile>;` before the bot command runs. The `[ -f … ] &&` guard keeps the prefix safe on hosts that lack the file, and the trailing `;` (not `&&`) keeps the rest of the chain running even when the source command returns non-zero. Defaults inferred client-side from the shell: `bash` → `~/.bash_profile`, `zsh` → `~/.zshrc`, `sh` → no source line.
  - **`preCommand`** — free-form bash run AFTER sourcing the profile, BEFORE the `cd && npm run …` chain. Use this for `nvm` version pinning (`. ~/.nvm/nvm.sh && nvm use 22`), `PATH` exports, etc. Max 512 chars, no embedded newlines or NUL bytes. Terminated with `;` in the emitted prefix so a non-zero exit doesn't abort the bot launch.
- The Add / Edit Host dialog includes a live **Sample command** preview backed by `POST /hosts/preview` (200ms debounce). The preview shows the exact `ssh` invocation that will run for the unsaved host config — operators see how their structured-field choices shape the wrapper payload before saving.
- Clock video mode over SSH is only meaningful when the local and remote hosts are NTP-synced. The clock frames use a shared system clock, so host clock skew is measured as false lag when the hosts are not NTP-tight.
- Stdout/stderr from the remote bot are tee'd into the registry entry's rolling log buffer (capped at 200 lines). The dashboard's per-bot "View logs" dialog polls `GET /api/bots/:id/log?since=<n>` every 2.5s.
- **Leave** sends `SIGTERM` to the local `ssh` ChildProcess (which propagates to the remote bot via the SSH connection). **Force-kill** sends `SIGKILL`.

v1 limitations (deliberately deferred):

- **Asset sync is out of scope.** Remote bots fall back to Chrome's default fake patterns unless an operator has manually prep'd `costumes/*.y4m` and `audio/*.wav` on the remote host's `<reposPath>/e2e/bots-app/run/` directory.
- **Remote ctl-API proxy is out of scope.** Mute / Camera / Share / Tune-network / Duplicate / Extend-TTL are not proxied for SSH-hosted bots. The dashboard greys them out with a tooltip ("Not available for remote bots (v1)") and the server returns `501` defense-in-depth.
- **Multi-launch fans out to one host only.** All N bots in a multi-launch land on the same chosen host in v1.

Security model: the dashboard process spawns `ssh` as the operator's local user; we do not elevate. The `127.0.0.1`-only bind + bearer token applies to the host registry endpoints just like the rest of the control API. The local `ssh-agent` + `~/.ssh/config` remain the source of truth for credentials.

## Architecture

```
e2e/
  helpers/auth.ts           ← existing; mints JWT session tokens
  bots-app/
    src/
      cli.ts                ← commander-based CLI entry point
      bot.ts                ← Playwright launch + cookie inject + navigate + leaveMeeting helper
      ttl.ts                ← parse "<int>s|m|h" / "infinite"; setTimeout-based scheduler
      manifest.ts           ← typed loader for bot/conversation/manifest.yaml
      stitcher.ts           ← ffmpeg-driven per-participant WAV stitcher (idempotent)
      costumes.ts           ← ffmpeg-driven MP4 → y4m converter (idempotent)
      assets.ts             ← resolves participant → {audioPath?, videoPath?} from run-dir
      meeting-join.ts       ← post-goto: fills display-name form, clicks Join Meeting, enables mic + camera
      orchestrator.ts       ← runBotsToCompletion — Map<botId, Promise> wait loop + registry + control server wiring
      meeting-config.ts     ← parse / emit meeting-config YAML + seeded random-N generator
      auth/
        jwt-cookie.ts       ← thin wrapper over helpers/auth.ts injectSessionCookie
        storage-state.ts    ← backend picker + captured-session path resolver (incl. HCL SSO state)
      control/              ← HTTP control surface + ctl client
        registry.ts         ← BotRegistryEntry + snapshot + retention sweeper (incl. SSH-host tag)
        auth.ts             ← token generation + token-file IO + bearer header parsing
        server.ts           ← Node http.createServer routes (`/healthz`, `/bots`, `/hosts`, `/bots/:id/*`)
        client.ts           ← thin node:http JSON client used by ctl subcommands
        ctl.ts              ← registerCtlCommands(program, runDir) — wires `bots-app ctl <subcmd>` family
        ssh-hosts.ts        ← `<runDir>/hosts.json` registry + validation + `shellEscape` + remote-cmd builder
        ssh-launcher.ts     ← `spawnRemoteBot` — wraps the `ssh` ChildProcess with a rolling log buffer
    scripts/
      setup-assets.sh       ← thin wrapper over `npm run bot -- prep-assets`
    run/                    ← gitignored; per-participant stitched WAVs + costume y4m caches + resource CSVs
                            ←   + ctl-<pid>.token, UNLESS BOT_CTL_STATE_DIR relocates it (#2157; the K8s fleet does)
    README.md               ← this file
    dashboard/              ← browser-based UX dashboard (see dashboard/README.md)
```
