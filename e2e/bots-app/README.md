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

The bots-app stack is engineered to keep its footprint in the shipping Dioxus UI to an absolute minimum. The **only** prod-UI changes required for the bots to work are seven `data-testid` markers, in two categories.

Meeting-state detectors — the bot's Playwright driver reads these to tell waiting-room / rejection / error apart:

| File                                       | `data-testid` value        |
| ------------------------------------------ | -------------------------- |
| `dioxus-ui/src/pages/meeting.rs`           | `meeting-waiting-for-host` |
| `dioxus-ui/src/pages/meeting.rs`           | `meeting-rejected`         |
| `dioxus-ui/src/pages/meeting.rs`           | `meeting-error`            |
| `dioxus-ui/src/components/waiting_room.rs` | `meeting-waiting-room`     |

Action-bar controls the bot clicks (#2441). Targeting these by tooltip text was structurally fragile: the text is a localizable label, and action-bar customize mode renders a second no-op clone of each slot carrying the same tooltip, so a tooltip match could resolve to two elements:

| File                                                | `data-testid` value   |
| --------------------------------------------------- | --------------------- |
| `dioxus-ui/src/components/video_control_buttons.rs` | `screen-share-button` |
| `dioxus-ui/src/components/video_control_buttons.rs` | `peer-list-button`    |
| `dioxus-ui/src/components/video_control_buttons.rs` | `hang-up-button`      |

The bot also drives `mic-toggle-button` and `camera-toggle-button`, which are **not** bots-app additions — they predate it and were added for the Playwright specs in `e2e/tests/`. So nine prod-UI markers are load-bearing for a bot run; seven of them exist because of bots-app.

(Each bots-app-added marker is preceded by a `// data-testid added for the bots-app …` comment so the intent is visible in the source.)

Because the bots-app and the Dioxus UI deploy independently, a bot can run against a target that predates a marker. The three action-bar selectors therefore keep a scoped tooltip-text fallback, and a run that uses one logs a `PRE_MARKER_UI` line — treat that run's control coverage as degraded. The fallbacks are removable once every target carries the markers.

**This is the documented invariant.** Anything beyond the markers listed above — runtime hooks, conditional rendering, exported globals, debug surfaces — does **not** belong in `dioxus-ui` for bots-app's sake. The bot exercises the same WASM / WebCodecs / WebTransport code path a human peer would; if it needs more visibility into a state, the right move is usually to add a stable `data-testid` on UI that already exists, not to add new prod code paths. Reviewers and future contributors: please preserve this scope. If a change to `bots-app` looks like it requires more than a new `data-testid`, flag it for design discussion before landing.

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

Pass `--video-mode clock` to publish a live wall clock as the bot's camera with
a silent audio track. Capture geometry follows the bot's index — 640x480 unless
`--bot-index` selects an HD position (see _Capture geometry mix_ below). The
default, `--video-mode costume`, and the accepted `file` alias preserve the
existing manifest/override-backed fake-device behavior.

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
- **`BOT_CTL_PROXY_IDLE_TIMEOUT_MS` bounds a proxied `/api/*` upstream request** (issue #2120). **Inactivity**, not an absolute deadline: the timer resets on every byte, so a live SSE stream (`/api/assets/prep/:id/stream`) is never severed while it keeps emitting, but a ctl handler that accepts and then goes silent no longer hangs the tab. Default `600000`; `0` disables it. Unparseable, negative, and `-0` values keep the bound armed (a typo must not reinstate the hang) — the CLI warns when it ignores one — and anything above `2147483647` is clamped, because Node clamps a larger socket-timeout delay to that and warns. Two caveats: the prep-stream handler has **no keepalive tick**, so a conversion that idles past the bound _is_ cut; and the bound only applies to an out-of-process ctl (**attach mode**) — in self-hosted mode ctl shares the dashboard's event loop, so the blocking `spawnSync("ffmpeg")` in asset prep freezes this timer too ([#2210](https://github01.hclpnp.com/labs-projects/videocall/issues/2210)).

Implementation lives under `e2e/bots-app/dashboard/` with its own `package.json`, build, and test surface — no dependencies leak into the parent `e2e/` workspace. See [`dashboard/README.md`](dashboard/README.md) for the security model and dev workflow.

## Network simulation (`--network <profile>`)

Both `bots-app run` and `bots-app gen` accept `--network <profile>`, and meeting-config YAML files accept `network:` at both the meeting level and per-bot. When set, the bot's meeting URL is rewritten to include `?netsim=<profile>` before navigation — the in-tab `videocall-client` (built with `--features netsim`) installs the matching shim on its WT + WS send paths to mimic a degraded peer. Without that build flag the URL param is parsed by the browser but silently ignored.

Valid profiles: `none`, `good_wifi`, `good_4g`, `congested_wifi`, `lossy_mobile`, `satellite`, `dialup`.

> **⚠ Egress only — the download path is unaffected.** `?netsim=` installs the shim for `Direction::Up` only (`videocall-client`'s `netsim_url::try_install_from_url`), so it degrades what the bot **publishes**; the relay → bot direction runs at full datacenter speed. The client also reads `?netsim_down=` for the receive direction, but bots-app never sets it (`bot.ts` sets only `netsim`), and pod-level `tc`/netem via `ctl netem` shapes egress too. Reproducing the freeze a real user sees on a lossy download link needs the pod-level IFB ingress mirror (#2353), which only `BOT_NETEM_PROFILE` installs at startup — `?netsim=` and `ctl netem` do not. Without it this measures how a degraded publisher looks to receivers, not how a receiver on a bad link behaves.

## Receiver low-power caps (`--max-received-layer` / `--skip-canvas-paint`, #2068/#2069)

Two per-bot knobs cut a bot's **receive-side** cost so a room holds more bots before the box saturates — room capacity is TOTAL decode load, not bot count:

- `--max-received-layer <N>` (env `BOT_MAX_RECEIVED_LAYER`): cap the received simulcast layer. `0` = base rung only (lowest decode CPU); unset = no cap. Cuts **decode** CPU (#2068).
- `--skip-canvas-paint <bool>` (env `BOT_SKIP_CANVAS_PAINT`): decode-and-drop — skip the per-tile `drawImage`. Saves **paint/GPU** only; decode still runs (#2069). Unset = inherit the deployment.

They are independent and composable. The CLI flag wins over the env var. Both are **launch-time only**: they are injected as `window.__APP_CONFIG` overrides before the first navigation, via an accessor whose setter merges them over whatever `config.js` assigns (production `config.js` does `window.__APP_CONFIG = Object.freeze({...})`; the setter intercepts that assignment). The served client parses `__APP_CONFIG` exactly once and freezes it, so these **cannot** be toggled at runtime through the control API — there is deliberately no `/control` endpoint for them.

Effect requires the DEPLOYED client to carry the knobs (videocall-client PR #2078). Against an older deployment the fields are present in `__APP_CONFIG` but ignored (harmless). Each bot logs a post-join assertion (`receiver caps verified in __APP_CONFIG` or a `WARNING`) so a bot that is NOT actually capped is visible per run rather than silently skewing the load test.

The StatefulSet fleet (`k8s/statefulset.yaml`) leaves BOTH **unset by default** — bots are realistic full-decode clients, so a run reproduces the same total-room decode load (and any decode-saturation freeze) a real participant would. Enable the caps only to trade realism for scale: cap per-bot decode/paint so the box holds more bots while you load the _server_ with many participants (`BOT_MAX_RECEIVED_LAYER=0`, and optionally `BOT_SKIP_CANVAS_PAINT=true`).

## Camera duty cycle (`BOT_CAMERA_{ON,OFF}_SECS_{MIN,MAX}`, #2362)

Real participants turn cameras on and off. Opt in and each bot independently cycles its own camera: a random on-period drawn from `[BOT_CAMERA_ON_SECS_MIN, BOT_CAMERA_ON_SECS_MAX]`, then a random off-period from `[BOT_CAMERA_OFF_SECS_MIN, BOT_CAMERA_OFF_SECS_MAX]`, repeating. Each pod draws from its own process RNG, so a fleet de-synchronizes without any coordination — the same independence property `BOT_MAX_JOIN_STAGGER_SECS` relies on. Target duty cycle is the mean on-period over the mean full cycle, so `on=[20-90]s off=[60-240]s` targets ~26%: in a 20-bot fleet, ~5 cameras publishing at any moment.

Env-only (no CLI flag); read by `src/camera-cycle.ts` from the inherited environment. **All four are required together** — a partial set fails the pod at startup rather than silently running camera-always-on. Non-numeric, `0`, above 86400 seconds, or `MIN > MAX` are rejected the same way, before `tc` touches the interface. All four unset ⇒ **camera on for the whole run, byte-for-byte the previous behaviour**.

> **⚠ Cycling LOWERS the load a run represents.** Cameras-off time is publish load, and fan-out load on every other peer, that the run did not exercise — so a cycling run's pod count and its CPU/fps figures are **not comparable to an always-on run's**, and a capacity number from one must not be quoted as a fleet capacity. `k8s/statefulset.yaml` therefore leaves it unset (locked by `src/camera-cycle.drift.test.ts`); it is an opt-in realism-for-load trade, like `BOT_MAX_RECEIVED_LAYER`.

Each toggle asserts its post-condition — that the camera button's tooltip actually flipped — because the action bar auto-hides and a fire-and-forget click would leave the run reporting a duty cycle it never applied. The receipt is in two places:

- **The launch line**: `camera_cycle=[configured on=[20-90]s off=[60-240]s target_duty=26%]`, or `camera_cycle=[off]`. It says `configured`, never `applied` — the entrypoint cannot observe a toggle.
- **The bot's own line at shutdown**, one of `CAMERA_CYCLE_APPLIED` / `CAMERA_CYCLE_DEGRADED` / `CAMERA_CYCLE_NEVER_FIRED`, carrying `toggles=ok:N/failed:M` and the observed on-fraction. `DEGRADED` (any failed toggle) goes to stderr. A bot whose toggles all failed reads as `DEGRADED … observed_on=100%`, never as an always-on bot.

Known gaps: bots always join camera-**on** and cycle from there (joining camera-off is not modelled); `/launch`-created bots do not cycle (the spec carries no cycle, same as `BOT_HW_CONCURRENCY`); and only the pod log carries the receipt — the `BOT_RUN_DIR` artifacts do not (#2358). A SIGKILLed pod loses the shutdown receipt, but each toggle is logged as it happens.

## Capture geometry mix (`--bot-index`, #2236)

Clock-mode capture geometry is a pure function of the bot's index (`--bot-index <N>`, env `BOT_INDEX`, flag wins): every 6th index — 1, 7, 13, 19, … — captures **1280x720**, every other index **640x480**.

At 25 pods that is 4 of 25 = 16% at 1280x720, approaching 1/6 ≈ 16.7% as the fleet grows. It seeds an observed population of 4 of 25 human publishers at 1280x720 (#2171), which carries roughly ±7 points of sampling error at 1σ — do not read the mix as tighter than its own input.

Bots created through the control API / dashboard `/launch` carry no index and capture **640x480**, so a mix cannot be built that way.

**No single pod can report the fleet's mix**: each process knows only its own index and logs only its own geometry. The token is emitted only after a positive `window.__videocall_encoder_fps` reading — so a pod that joined with its camera never enabled is not counted (the enable is best-effort: `clickWhenVisible` warns and returns when no control matches its tooltip text). It is emitted once per join, so a pod that rejoins — a `tune-network` triggers one — emits again: the count is emissions, not pods. A label selector makes `kubectl logs` default to the last 10 lines per pod, so `--tail=-1` is required:

```bash
kubectl -n bot-load logs -l app.kubernetes.io/instance=videocall-bots --tail=-1 \
  | grep -c "captures 1280x720"
```

The count is **bots with a running camera encoder whose capture is 1280x720**. It does not say what they PUBLISH: the client fits the source into the top rung's bounding box (`180p/360p/720p`, `videocall-aq/src/constants.rs`) and never upscales, so a sub-rung capture publishes at its own size. A count below the fleet's HD-index count is a missing-encoder signal, not a wrong mix — read the per-bot encoder-fps line in each pod's run report before concluding anything about the posture.

### Measured per-pod cost at 8 replicas (2026-08-22, #2313)

Unshaped (`BOT_NETEM_PROFILE=""`), 20 samples at 30s over 10 minutes, image `0.1.0-20260822-3a95d0f@sha256:26de7a79…`, meeting `bottest`. Default scheduler scoring spread the 8 pods over seven 32-core workers, leaving 26–31 idle cores beside each one. CFS throttling stayed at 0.00–0.02%, so `limits.cpu: "8"` did not bind and these readings are the workload, not a capped one.

| pod        | source       | `BOT_HW_CONCURRENCY` | mean cores | max      |
| ---------- | ------------ | -------------------- | ---------- | -------- |
| bots-0     | 640x480      | 4                    | 3.34       | 3.48     |
| bots-1     | 1280x720     | 4                    | 3.49       | 3.60     |
| bots-2     | 640x480      | 6                    | 3.40       | 3.51     |
| bots-3     | 640x480      | 6                    | 3.29       | 3.42     |
| bots-4     | 640x480      | 10                   | 3.51       | 3.65     |
| bots-5     | 640x480      | 10                   | 3.51       | 3.58     |
| bots-6     | 640x480      | 10                   | 3.55       | 3.70     |
| **bots-7** | **1280x720** | **10**               | **4.03**   | **4.19** |

Memory working set was **1493–1736 MiB**, which is why `requests.memory` is `2Gi`. At the previous `1.5Gi` most pods sat above their own request — the pod stays Burstable either way, but usage above the request is what the kubelet ranks Burstable pods on when it picks one to evict under node memory pressure. `src/k8s-resources.drift.test.ts` pins the request against that measured peak.

**Stratify by ladder depth or the HD premium reads wrong.** Grouping HD against SD naively gives 3.76 vs 3.44 (+9%), which mixes in the per-ordinal `BOT_HW_CONCURRENCY_<N>` overrides that run used (0:4, 1:4, 2:6, 3:6, rest 10) — bots-1 is HD _and_ capped at 4, so it lands below three SD pods. Like-for-like it is **+4.3%** at `hw=4` (3.49 vs 3.34) and **+14.5%** at `hw=10` (4.03 vs 3.52). An HD source only costs more when the encoder actually builds the top rung, and `hw=4` caps the ladder before it can, so the worst case is HD source **and** `hw=10` — under those overrides, ordinal 7 alone.

**This is demand, not a reservation requirement, and it is not a capacity number.** Nothing competed for the cores, so `requests.cpu: 1500m` was never enforced as a floor and what a bot needs under contention is unmeasured. The request is deliberately left at `1500m`; raising it to cover 4 cores would cut per-node density from ~21 bots to 8 and force the quota's `requests.cpu` from 48 to ~96 against a contention scenario that has not been observed. The spread that made the run clean is the scheduler's default least-allocated scoring on an idle cluster — there is no `affinity` or `topologySpreadConstraints` in the manifest, so it is not guaranteed.

**Do not take capacity numbers from a small fleet either.** Below ~8 pods (>= 7 decoded peers) the HD premium is inflated in the other direction: the #1256 tile lid only binds from ~7 decoded peers (see _Per-bot capability cap_ below), so under that every receiver decodes an HD publisher's top rung. The committed `replicas: 3` is a smoke size, not a measurement size. The model this run replaced predicted SD 1.8–2.4 cores and a +33% premium — it under-predicted the SD floor by 43–90% and over-predicted the premium ~2x.

## Resource capture + `RESOURCE_STARVED` verdict

Every `bots-app run` (and the self-hosted `bots-app dashboard`) forks a background sampler for the run's duration that measures the box the bots actually run on — so a self-starved run (the host saturating its own CPU) is flagged instead of being mistaken for a product regression. There is nothing to enable; it is on by default and degrades to a no-op on a box without `/proc` (e.g. a macOS laptop).

Every ~5s the sampler (`scripts/resource-sampler.sh`, POSIX bash + `/proc`, no dependencies) appends a block of **raw** `/proc` counters — CPU jiffies (overall, per-core, steal), load average, memory (used/available/swap), NIC rx/tx bytes, and per-Chrome/orchestrator process cpu-jiffies + RSS — to `<run-dir>/resource/<label>-raw.csv`. At run end the raw counters are diffed in TypeScript into a **derived**, Prometheus-overlay CSV (`<label>-derived.csv`, epoch-seconds first) and a summary (`<label>-summary.txt`). All the delta math, aggregation, and the verdict live in `src/resource/` and are unit-tested there.

The run prints a prominent, greppable verdict banner:

- `RESOURCE_STARVED` when **either** rule fires: peak overall CPU stayed above 85% for 3+ consecutive samples (~15s sustained), **or** any bot's reported encoder FPS dipped below the base rung (default 5 fps). A starved run's client-signal regressions (encoder fps, RTT, sheds) should be treated as confounded by box saturation, not a product change.
- `RESOURCE_NO_EVIDENCE` when neither rule fired but the run produced nothing to judge — no derived samples at all (including an unsupported box), or join tracking saw zero bots reach the meeting. A shaped or slow pod that misses `FORM_LOGIN_TIMEOUT_MS` lands here; its CPU figures describe an idle box, so no number from that run is representative (#2358). The remote-host and self-hosted-`dashboard` receipts do not track joins, so they cannot reach this verdict on the join rule.
- `RESOURCE_OK` otherwise — a run that was sampled, had a bot in the meeting, and had headroom.

It also prints an **arrival spread** line — first→last join over the bots this process launched
locally — noting that the capture starts before the first launch, so the CPU, RAM, NIC, process and
verdict lines below it cover that ramp — and that it does not state how long the room held every bot.
Quote a smeared run's aggregates against it. One bot per pod reads `n/a`, and the note there scopes
the same list to the pre-join stretch instead. Per-bot encoder fps is in neither scope: a bot's
readings begin when its own encoder starts reporting one. Two things bound what the line can see:
SSH-launched bots join in their own process (and so are absent from `joinedAt` on `GET /bots` too),
and each bot's FIRST join is the one recorded, so a
`ctl` netsim retune does not read as a late arrival while a `ctl duplicate` — a different bot —
does. Reports with no tracked spread say `not tracked`: the self-hosted `dashboard` daemon, each
remote host's own block, and any run where no bot reached the meeting.

The room-held-every-bot window closes at the first departure, and no per-bot `[join, leave]`
interval reaches the receipt today — nothing under `src/resource/` consumes a departure at all.

A multi-launch paces spawns by `spawnDelaySeconds` — the dashboard's form defaults it to **2**
(`POST /launch/multi` on its own defaults to 0) — so a form-launched fleet of N bots carries an
`(N-1)×2s` ramp; set it to 0 when the ramp is what you are measuring. With one bot per pod every
pod's spread reads `n/a` — for the fleet-wide ramp, read join order off the pods' own logs, or
aggregate `GET /bots`'s `joinedAt` across pods (#2337):

```bash
kubectl -n bot-load logs -l app.kubernetes.io/instance=videocall-bots --tail=-1 --timestamps \
  | grep "] joined; ttl="
```

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
#    PUSH_LATEST defaults to 1 and moves :latest from whatever branch you are
#    on; pass 0 unless you mean to republish the alias the fleet's default pull
#    resolves. Only 0 and 1 are accepted.
REGISTRY_USER="$HARBOR_USERNAME" REGISTRY_PASS="$HARBOR_PASSWORD" \
  PODMAN_REMOTE=1 PUSH=1 PUSH_LATEST=0 ./build.sh
# A push prints the tag@sha256:<digest> to pin. The tag alone is not enough: it
# is reused by a later build at the same HEAD on the same day. Move all of
# k8s/{statefulset,bot-pod,conductor-job}.yaml onto it in one step, then re-run
# prepull-image.sh — it refuses to warm if the three disagree:
#   ./k8s/repin.sh <registry/repo:tag@sha256:digest>
#
# BUMPING THE PIN ON A LIVE FLEET: `kubectl -n bot-load set image
# sts/videocall-bots bot=<ref>` — not `apply -f`, which re-applies the whole
# committed spec, replicas: 3 included, under podManagementPolicy: Parallel.
# Prepull first, so the roll is not pulling under the measurement.
# k8s/bot-pod.yaml is a bare Pod under no controller: it keeps publishing the
# old image into the same room unless applied too.
#
# Step 1 is usually unnecessary: "Build Bots Fleet Image (HCL)" builds and pushes
# on merges to hcl-main or PR-staging that touch e2e/ source shipped in the image,
# and prints the repin.sh command to pin it in its run summary. Build by hand for
# an unmerged branch or any other branch — commit first, or the tag names a commit
# the image does not contain.
#
# On PR-staging that run also opens the re-pin PR (#2400): its `repin` job commits
# the pin on `bots-image-pin/PR-staging` and opens — or refreshes — one PR against
# PR-staging. The rollup carries that pin on to hcl-main; nothing re-pins hcl-main
# directly, because pinning both branches conflicts on the same image: lines every
# cycle. Without a `BOTS_REPIN_TOKEN` secret the job still commits and pushes the
# branch and prints a one-click compare link, leaving only the "open PR" click —
# which is also what makes the PR run CI, since a GITHUB_TOKEN-authored PR does not.
#
# prepull-image.sh refuses to warm a pin whose commit differs from your tree in
# any of that source, naming the files — including a tree BEHIND the pin, and an
# uncommitted local edit, so read the list before concluding the pin is stale.
# --verify-coverage warns instead: it reports on the cluster, not the tree.
# To read the pin's provenance, or to warm one deliberately mid-A/B:
#   ./k8s/prepull-image.sh --print-pinned-commit
#   ./k8s/prepull-image.sh --check-source-drift
#   ALLOW_SOURCE_DRIFT=1 ./k8s/prepull-image.sh

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

A carriage return or newline in any env value the entrypoint puts on the `bots-app run` line — `MEETING_URL`, `BOT_PARTICIPANT`, `TTL`, `BOT_AUTH`, `BOT_RUN_DIR`, `BOT_HW_CONCURRENCY`, `BOT_INDEX`, `BOT_CTL_PORT`, `BOT_CTL_BIND`, `BOT_CTL_STATE_DIR`, `BOT_EMAIL`, `BOT_EXTRA_ARGS` — exits 1 naming the variable, before the bot process starts. Downstream log lines carry these verbatim, so a rejected value is one that could otherwise emit a second, correctly-prefixed line into the pod log. `--participant` is the in-meeting handle, so the value is refused rather than rewritten.

## Running a fleet (StatefulSet, N bots — #2035)

For more than one bot, use `k8s/statefulset.yaml`. Each pod (`videocall-bots-<N>`) derives its identity from its ordinal: the entrypoint reads `${HOSTNAME##*-}` and selects `BOT_EMAIL_<N>` / `BOT_PASSWORD_<N>` from a fleet-wide `bot-accounts` Secret, and sets the display name to `bot-<N>`. So N bots join as N **distinct** accounts with no rename/host-race collisions.

```bash
cd e2e/bots-app

# Fleet accounts: one BOT_EMAIL_<n>/BOT_PASSWORD_<n> pair per ordinal 0..N-1.
# Build a local env-file (NEVER commit it), create the Secret, then shred it.
kubectl -n bot-load create secret generic bot-accounts --from-env-file=./bot-accounts.env

# Warm the image onto every schedulable worker FIRST (#2294). Without this the
# fleet is image-pull-bound and arrival is smeared over minutes, which makes the
# ramp unmeasurable. Reads the tag from statefulset.yaml; no arguments.
./k8s/prepull-image.sh          # blocks until every eligible node is warm
./k8s/prepull-image.sh --delete # once the run is under way

# Deploy + scale. TTL defaults to infinite.
kubectl apply -f k8s/service.yaml -f k8s/statefulset.yaml
kubectl -n bot-load scale statefulset/videocall-bots --replicas=10
kubectl -n bot-load get pods -w

# Before trusting the numbers (#2248):
#   * SCALE FIRST. The BOT_HW_CONCURRENCY=10 third rung only pays for itself at
#     >= 7 decoded peers (>= 8 replicas). The manifest ships replicas: 3, where
#     it is pure encode cost.
#   * The image is pinned by digest, so an env-only rollout cannot swap the bot
#     image underneath a 2-rung vs 3-rung comparison. Bumping the pin
#     mid-comparison still can — finish the A/B on one digest.
#   * IGNORE the "CPU Throttle Indicator" panel specifically (not the adjacent
#     "Tab Visible / Throttled" one) in the meeting-investigation
#     Grafana dashboard for these bots. It reads videocall_client_cpu_throttled,
#     which is capability_score/cores < 150 over the SPOOFED core count, so every
#     bot paints as throttled with no real CPU change. Ground truth is the pod's
#     CFS throttle fraction, which this tool does not sample:
#       rate(container_cpu_cfs_throttled_periods_total[5m])
#         / rate(container_cpu_cfs_periods_total[5m])
#     in kube-prometheus-stack-prometheus (the *-qs-prometheus services carry no
#     cAdvisor series at all). Do NOT use container_cpu_cfs_throttled_seconds_total:
#     the kubelet exposes it but that Prometheus does not keep it, so the query
#     returns EMPTY, which reads as "no throttling".
#   * videocall_encoder_active_layers{media_kind="camera"} is the AQ operating
#     point; the ceiling BOT_HW_CONCURRENCY=10 buys is a separate gauge,
#     videocall_encoder_effective_layers. The camera ladder seeds at 1 and each
#     rung must be EARNED by LAYER_PROBE_CLEAR_WINDOW_MS (6s) of uninterrupted
#     clear queue (probe_add_allowed), so a reading of 1-2 is a ramp that never
#     earned the rung — a different defect from a shed down from 3, and not the
#     relay's LAYER_HINT union, which suppresses only when EVERY receiver asked
#     for less and stays there past the hint's downgrade window (one non-lidding
#     watcher masks it).

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

**Per-bot capability cap.** A container reports the _node's_ core count to the browser, so an un-capped bot over-commits simulcast layers. The StatefulSet sets `BOT_HW_CONCURRENCY=10` (→ 3 layers), which the bot spoofs into `navigator.hardwareConcurrency` before the client's capability check runs (`<6`→1 layer, `6..9`→2, `>=10`→3). Set it to model a real client's ladder depth, not the pod's CPU budget. The cap governs BOTH directions, and for a bot that receives, MORE rungs is CHEAPER: a 2-rung `[low, hd]` ladder gives the #1256 tile lid no middle rung, so each receiver decodes `hd` per peer. **Scoped to >= 7 decoded remote peers** (>= 8 replicas) at the fleet's 1920x1080 / dpr 1 posture — below that both ladders pick the top rung and the third rung is pure encode cost; at dpr 2 under `Auto` the saving never materialises at all — CSS tile height saturates at 200px, so device height bottoms out at 400px, above the 396px boundary, and the top rung is picked at every peer count. Three rungs costs one extra encode rung and saves more on decode — figures in #2248, computed on the bot's SOURCE-BOUNDED ladder (a 640x480 source fits to 240x180 / 480x360 / 640x480; rung dims are a bounding box and are never upscaled), not the nominal 320x180 / 640x360 / 1280x720. It governs CAMERA depth only: the screen path publishes exactly one rung (`SCREEN_SIMULCAST_MAX_LAYERS`, #2343), so raising the cap cannot deepen a share. Audio is outside it too — `host.rs` pins the mic encoder to ONE layer via `audio_published_layer_count()`, never the capability check this var spoofs, so the cap cannot shorten audio.

## K8s fleet control via `bot-ctl` (#2072)

`k8s/bot-ctl` is a wrapper script that handles token fetching and port-forwarding automatically. It is the recommended way to drive a K8s-deployed fleet from your laptop — no raw kubectl or token management needed.

```bash
# One-time: put it on PATH (the symlink is resolved back to this repo's e2e/)
ln -sf "$PWD/e2e/bots-app/k8s/bot-ctl" ~/.local/bin/bot-ctl

# List all running bots across all pods
bot-ctl list

# Apply netem to every pod
bot-ctl netem congested_wifi

# Apply to one pod, by StatefulSet ordinal
bot-ctl netem satellite 2

# Remove netem shaping from all pods
bot-ctl clear

# Toggle camera off on all pods
bot-ctl video off

# Toggle camera on, one pod only
bot-ctl video on 4

# Mic
bot-ctl mute
bot-ctl unmute 4

# Set every bot's remaining TTL to 10 minutes (whole seconds; sent as --set 600s)
bot-ctl ttl 600

# One bot's full detail as JSON (pod 0 by default)
bot-ctl status 2

# Graceful leave
bot-ctl leave

# Env overrides (defaults shown; `bot-ctl help` lists them all):
BOT_CTL_KUBECONFIG=~/qsk8s-config BOT_NAMESPACE=bot-load BOT_STS=videocall-bots bot-ctl list
```

Requires `kubectl` on PATH and a node that can run `npm run bot` from `e2e/`. The script auto-reads `bot-ctl-token` from the `bot-load` namespace and port-forwards each pod's `:8080` to a local port. It calls `npm run bot -- ctl` internally against the `e2e/` directory (auto-detected from the script location).

`BOT_CTL_KUBECONFIG` wins over `KUBECONFIG`, so the fleet's kubeconfig can be pinned without disturbing an inherited one. `KUBECONFIG` is the fallback, and `~/qsk8s-config` the default.

Mutating commands print one `✓` or `✗` line per pod (`list` and `status` print their payload instead), and the script exits non-zero if any pod failed. `ctl`'s stderr is not discarded, so a failing pod reports the underlying error rather than looking like a success.

> **Why per-pod, not fleet-wide?** Each pod runs an independent control server that only knows about the single bot inside it, so the wrapper loops over pods and port-forwards to each in turn. `bot-ctl list` prints one table per pod under a `── <pod> ──` header — it does not aggregate. Every command defaults to all running pods and takes an ordinal to narrow to one; `status` is the exception, taking a single ordinal (default `0`).

## Remote per-bot control + network impairment (#2072)

Each pod runs a **token-authenticated control server** on `0.0.0.0:8080`, reachable in-cluster at `videocall-bots-<N>.videocall-bots.bot-load.svc:8080`. It drives that pod's bot: mute, camera, screen-share, leave, and per-pod network shaping via `tc`/netem.

Prereqs (once per fleet):

```bash
# Shared control-API token (the conductor holds the same one).
kubectl -n bot-load create secret generic bot-ctl-token --from-literal=token="$(openssl rand -hex 32)"

# netem needs kernel modules on each node: sch_netem, plus ifb / sch_ingress /
# cls_u32 / act_mirred for the ingress mirror. This DaemonSet is the ONLY
# privileged component (the bot pods stay non-root); apply it BEFORE any netem
# action — including a fleet with BOT_NETEM_PROFILE set — and wait for Ready
# (readiness == every module present on that node).
kubectl apply -f k8s/netem-preload-daemonset.yaml
kubectl -n bot-load rollout status ds/netem-preload
```

(The image also `setcap cap_net_admin+eip`'s `tc` and `ip` so the non-root bot can shape its own `eth0` and create its own `ifb0`: the pod grants `NET_ADMIN`, but a non-root process only receives it on those binaries via the file capability — K8s sets no ambient caps. For `ip` that file capability is not sufficient on its own — iproute2 clears its own capabilities unless `CAP_NET_ADMIN` is also in the process _inheritable_ set — so the entrypoint execs `ip` through a `setcap`'d copy of `setpriv` at `/usr/local/bin/netem-setpriv` that raises it first; see #2428, which is **not yet confirmed on the fleet**.)

### Impairment from the moment a bot joins, in both directions (#2354, #2353)

`POST /netem` can only shape a bot that already joined on a clean link, so initial connection, transport election and the first keyframe are never impaired. These env vars fix that. All are opt-in, all default to off, and none is present in `k8s/statefulset.yaml` — add them to its `env:` block to use them. `BOT_NETEM_IFACE` is grammar-checked on every run, profile or not, so a malformed device name fails the pod before any `tc` runs. Only `BOT_EMAIL`, `BOT_PASSWORD`, `BOT_NETEM_PROFILE` and `BOT_HW_CONCURRENCY` take a `_<N>` suffix; a suffix on any other `BOT_` var is read by nothing and the entrypoint warns about it. In ordinal mode it also warns when the suffix is not a plain integer (`_01` is not what `${ORDINAL}` produces for pod 1) or names an ordinal with no account in `bot-accounts` — both are read by no pod, so a run would report `netem=[no-netem]` for a pod the operator believes is shaped.

> ⚠️ **Read by the image's baked-in `docker-entrypoint.sh`, so they need the REBUILT IMAGE, not just `kubectl apply`.** On a digest predating #2354 the entrypoint ignores `BOT_NETEM_PROFILE` and the bots join **unshaped** while the run is described as impaired; on one predating #2353 it shapes EGRESS ONLY, so a receive-side conclusion from that run is unsupported. Bump the digest pin (step 0) first and confirm the launch line reports `netem=[shape … direction=both …]`.

| Var                                  | Effect                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `BOT_NETEM_PROFILE`                  | Applies a named profile from `src/control/netem.ts` before the bot process starts, so the join itself is impaired, in **both** directions: root netem on `BOT_NETEM_IFACE` (uplink, the profile's `rateKbit`) plus the same delay/jitter/loss on an `ifb0` mirror fed by an ingress redirect (downlink, its `downlinkRateKbit`). `clean`/`none` clear instead, removing the hook, the mirror's qdisc and the `ifb0` device. An unknown name, any failing `tc`/`ip` other than clearing an already-clean object, or a post-read that does not show the state just applied, exits 1 rather than joining half-shaped. With no profile set the entrypoint only _reads_, and reports what it read (`inherited` / `no-netem` / `unread`) plus an `ingress=` term, because "no netem" is not "no shaping" and a mirror does not appear in the root qdisc listing at all. |
| `BOT_NETEM_PROFILE_<N>`              | Per-ordinal profile (ordinal mode only), so one StatefulSet carries a mix of links. Unset ⇒ the fleet-wide profile; explicitly empty ⇒ that pod runs no `tc` _mutation_ (it still probes), so whatever is already on its interface stays. To put one pod on a known-clean link inside a shaped fleet set `clean`, which actively clears; empty does not.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `BOT_MAX_JOIN_STAGGER_SECS`          | Sleeps a random 0..N seconds (inclusive; an N above 32767 is rejected at startup, not clamped) before launching, so the fleet spreads its joins. Each pod draws independently — a distribution, not a schedule, so two pods can still collide. The control API is not bound during the sleep, so a conductor must wait for the API rather than for pod-Running, and must not schedule an action inside the first N seconds (#2356).                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `BOT_CAMERA_{ON,OFF}_SECS_{MIN,MAX}` | Per-bot random camera on/off cycling (#2362) — all four required together, all four unset ⇒ camera always on. Validated with the stagger, before any `tc` mutation. See "Camera duty cycle" above; a cycling run represents LESS load than an always-on one.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `BOT_HW_CONCURRENCY_<N>`             | Per-ordinal override of the fleet-wide cap (ordinal mode only), so one StatefulSet can mix ladder depths. Unset ⇒ the fleet value; explicitly empty ⇒ omit the flag for that pod. A **mixed** fleet makes `videocall_client_cpu_throttled` incomparable between pods — it divides by this spoofed count. Use `container_cpu_cfs_throttled_periods_total` and the fps `RESOURCE_STARVED` verdict.                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |

What a shaped run does **not** cover — do not quote a number from one as if it did:

- **A `rate` under the publisher's offer takes load OFF the room.** The root qdisc shapes the whole interface. A bot's camera rung targets sum to ~1970 kbit (`videocall-aq`'s `SIMULCAST_VIDEO_LAYERS`: ~120 + ~350 + ~1500), plus up to ~48 kbit of audio (`videocall-aq`'s `AUDIO_QUALITY_TIERS[0]`; the ONE published layer inits there and the AQ walk only lowers it). A share adds ONE more rung (`SCREEN_SIMULCAST_MAX_LAYERS`, #2343) sized from the share's own geometry: ~4423 kbit for a capture fitted to the tier-0 encode box (`screen_bitrate_kbps_for` over `SCREEN_MAX_ENCODE_WIDTH`/`_HEIGHT`/`SCREEN_TARGET_FPS`). A smaller share costs less, and the encoder's FIRST `configure()` sizes its budget from the raw capture dims (`resolve_capture_dimensions`) before that box is applied, so a sharing bot's ~6441 kbit is a REFERENCE TOTAL, not a hard ceiling; the shipped `k8s/scenario.example.yaml` does share. **Every figure here is a sum of configured targets, and the real offer is unmeasured in BOTH directions** (the framing the qdisc also counts is not in them; a VBR encoder on a near-static clock face spends under target; and a clock-mode bot's audio term is nominal because `src/clock-source.js` routes its oscillator through a gain of 0), so only a wide gap is reportable and only one way. **Cannot carry the default camera ladder:** `satellite` (1500 kbit), `lossy_mobile` (800 kbit), `dialup` (56 kbit). **Cannot carry a SHARING bot's total:** `congested_wifi` (2000 kbit), `satellite`, `lossy_mobile`, `dialup`. **On a sharing bot** `congested_wifi` carries under a third of the offer, though it clears the camera ladder on its own — by less than the audio layer alone. **Every profile NOT on those lists is unclassified, NOT faithful.** `good_4g` (10000 kbit) and `good_wifi` (20000 kbit) have nominal room for all three, which is not evidence that a bot on either published what a real client would. A shaped run's **pod count is not comparable** to an unshaped run's: what the link cannot carry never reaches the relay to be fanned out, so every _other_ bot decodes less too. `src/control/netem-ladder-drift.test.ts` derives every figure and all three lists from the real ladders and `NETEM_PROFILES`.
- **`POST /netem` is egress only; startup shaping is not.** A runtime action cannot install a mirror, so it REMOVES any mirror startup installed and reports `mirrorRemoved` — after one, the pod's downlink is at line rate under whatever label the action carried. Shaping ingress at runtime is not implemented. The ingress queue depth is the SAME `limitPkts` as egress, and the `ifb0` device queue is raised from its default 32 to 1000 so the netem limit is what binds; no run has measured any of these (#2355). At a 2-6.7x faster downlink the same packet depth is a proportionally shorter queue in TIME.
- **Nothing in the ingress path has run on a cluster.** Whether the pod's file capability on `ip` lets a non-root bot create an `ifb` device, and whether the four new modules load on a given node, are what `ds/netem-preload`'s readiness answers per node — not something a green suite establishes.
- **The #2363 baseline is an EGRESS-ONLY baseline and is not a control for a shaped run.** All three of its figures move from this change alone: the 677 rate-limited PLIs (shaped ingress makes the relay's `congested` path reachable on WS, changing `keyframe_per_pair_budget` from 1/s to 4/s, so the denominator differs), the 2,333 WS mailbox drops (that counter IS the outbound-channel-full event ingress shaping creates), and the 50,535ms peak staleness (RTT doubles, and relay shedding now removes frames the bot previously received). A run compared against those numbers measures the shaping change AND the fix together and credits both to the fix; a fresh bidirectional baseline has to come first.
- **`per_pair_budget=4` in a shaped run is the new correct value, not a regression.** This also falsifies a claim in `actix-api`'s `constants.rs` / `packet_handler.rs` that the `congested` relaxation can never fire on a lossless WS/TCP path: `CongestionTracker`'s input is `on_outbound_drop`, not inbound loss. Not fixed here — the relay is out of this tool's scope.
- **Nothing pins the transport, and #2363 is WS-specific.** A shaped run can land on WebTransport, where the loss sources and the `congested` path differ, and no run artifact records the shaping (#2358) or the transport — so a finished run cannot be checked after the fact.
- **`?netsim=` and `BOT_NETEM_PROFILE` are not interchangeable despite sharing profile names.** The shim's inbound path maps `Admission::Delay` to Pass, so inbound rate, delay and jitter have NO effect there; netem applies all three.
- **A fleet-wide `BOT_NETEM_PROFILE` gives every pod an identical link**, which no real room has. Use `BOT_NETEM_PROFILE_<N>` to spread the mix; the per-pod launch line reports the profile that pod actually applied.
- **The queue depth is a chosen budget, not a measured one.** Each shaping profile carries a `limitPkts` that `buildNetemShapeArgs` emits as netem's `limit`; the queue model is still a plain FIFO, and no run has measured the standing queue or drop rate any profile produces. `docker-entrypoint.sh` keeps its own copy, locked to `NETEM_PROFILES` by `src/docker-entrypoint.test.ts`. #2355.
- **Nothing but the pod log records the shaping.** The launch line reports what this entrypoint applied; the run artifacts under `BOT_RUN_DIR` do not carry it, so a CSV or a dashboard row from a shaped run is indistinguishable from an unshaped one. Correlate by pod log until #2358 lands. The root qdisc also shapes that pod's own control-API responses, so on a low-`rate` profile a `failed` conduct action has two possible causes and the run output does not separate them.
- **The impaired window now includes login.** The app-bundle fetch and the `form-login` flow run over the shaped link on a budget that does not scale with the profile (`FORM_LOGIN_TIMEOUT_MS`/`FORM_LOGIN_ACTION_TIMEOUT_MS`, 20s/10s, no override), so a low-`rate` pod gets the same 20s as an unshaped one. Transport election happens in that window too, and no run artifact records which transport a bot ended on.
- **A startup-shaped pod answers its control API over the shaped link.** `DELETE /netem`'s own response traverses the egress it is clearing, so at a low `rate` prefer out of band: `kubectl exec … -- tc qdisc del dev eth0 root`.
- **A camera duty cycle removes load in both directions.** Every camera-off second is a publish this run did not make and a decode every other peer did not do, so a cycling run's pod count is not comparable to an always-on run's and its CPU/fps figures are not a fleet capacity. The per-pod `CAMERA_CYCLE_*` receipt states the observed on-fraction, and `CAMERA_CYCLE_DEGRADED` means even that is not the configured cycle. Nothing but the pod log carries it (#2358). No toggle has been driven against a real meeting page from this branch — the toggle step and its post-condition are covered only by unit tests against a Playwright-shaped fake.
- **A join stagger removes a load characteristic.** N pods joining at once is itself load, and a nonzero `BOT_MAX_JOIN_STAGGER_SECS` spreads it, so a staggered run under-represents join-time cost and its pod count is not comparable to an unstaggered run's. Per-bot `ttl` runs from each bot's own join, so the window in which all N are present also shrinks by the spread — latent at the shipped `TTL: "infinite"`. Only the pod log carries the drawn delay, and nothing records when a pod actually joined (#2358).

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

Actions: `mute`/`unmute`, `camera-on`/`camera-off`, `screenshare-on`/`screenshare-off`, `netem` (`profile:` or raw `delayMs`/`jitterMs`/`lossPct`/`rateKbit`/`limitPkts`), `netem-clear`, `talk` (`durationMs` — unmute then re-mute), `leave`. Run it as an in-cluster Job:

```bash
kubectl -n bot-load create configmap bot-scenario --from-file=scenario.yaml=./my-scenario.yaml
kubectl -n bot-load apply -f k8s/conductor-job.yaml
kubectl -n bot-load logs -f job/bot-conductor      # prints each action as it fires
```

### Full fleet deploy order

0. **Build + push the image from this commit** (`PODMAN_REMOTE=1 PUSH=1 ./build.sh`) and use the tag it prints in step 5. `.github/workflows/build-bots-image-hcl.yaml` rebuilds on a push to `hcl-main`/`PR-staging` under `e2e/**` — but its `paths:` filter excludes `bots-app/k8s/**`, `dashboard/**`, `e2e/tests/**`, `**/*.test.ts` and `**/*.md`, and it moves `:latest` only on `hcl-main` (or a manual dispatch with `push_latest`). Even then `statefulset.yaml` pins the image **by digest** with `imagePullPolicy: IfNotPresent` — so a rebuilt `:latest` reaches no pod until that pin is bumped, and a `kubectl apply` alone leaves the fleet on the pinned digest. This step is not optional for #2157: the stale-token sweep lives in `docker-entrypoint.sh`, which is baked into the image, so a manifest-only deploy silently omits it (see [Rotating `bot-ctl-token`](#full-fleet-deploy-order) below).
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

> ⚠️ **Both #2157 behaviours need the REBUILT IMAGE, not just `kubectl apply`.** The token relocation is driven by `BOT_CTL_STATE_DIR` (a manifest change) **and** by `docker-entrypoint.sh`, which is baked into the image — and the manifest pins that image **by digest** (`imagePullPolicy: IfNotPresent`), so even after `build-bots-image-hcl.yaml` moves `:latest` the fleet stays on the pinned digest until it is bumped. On a fleet still running an older pinned digest, `rollout restart` re-reads the Secret but runs the OLD entrypoint, which has no sweep — so every `run-artifacts` PVC keeps its pre-#2157 cleartext token and the rotation does **not** retire it, exactly the condition #2157 exists to end.
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
2. Converts their costume's `talking.mp4` into `e2e/bots-app/run/costumes/<name>.y4m` (ffmpeg, 640×480 @ 30fps, yuv420p).

Both steps are idempotent — re-runs spawn ffmpeg only when the source file is newer than the cached output, or when the cached y4m was built at a different geometry (issue #2171). Output sizes: ~1.5 MB per audio WAV, ~123-130 MB per y4m (raw uncompressed). `e2e/bots-app/run/` is gitignored.

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
