# bots-app

Browser-driven bot CLI for videocall meetings. Runs a real Chrome instance via Playwright so the bot exercises the same WASM / WebCodecs / WebTransport code path a human peer would — used to recreate real-life issues against the deployed meeting stack while a human peer evaluates the result.

See discussion [#793](https://github01.hclpnp.com/labs-projects/videocall/discussions/793) for the design and implementation plan.

## Status — phase 4 (stateful orchestrator + ctl control API)

Phase 4 turns `bots-app run` into a long-running orchestrator with an introspectable / mutable control surface. When started with `--ctl-port <port>` (or `--ctl-port auto`), the run process binds a local HTTP API and writes a token file under `run/ctl-<pid>.token` (mode `0600`); the new `bots-app ctl <subcommand>` family auto-discovers that file and talks to the orchestrator over `127.0.0.1`. See the "Phase 4: control API" section below for endpoint + subcommand details.

Without `--ctl-port` the orchestrator behaves byte-for-byte the same as in phase 3 — the control surface is strictly opt-in.

## Status — phase 2b (multi-bot + seeded random-N matrix)

Phase 2 adds multi-bot orchestration + a seeded random-N generator on top of the phase-1 single-bot foundation:

- `bots-app run --users N` launches N bots concurrently in one Node process (in-process Playwright `Browser` instances — cheaper than N subprocesses; resource cap configurable via `--max-users`, default 10).
- `bots-app gen --count N --seed S --meeting-url <url>` emits a meeting-config YAML with N randomly-shuffled participants (deterministic given `--seed`). By default the shuffle pool is **only costumed participants**; pass `--include-observers` to also pick observer-NN seats.
- `bots-app run --config <path>` consumes a meeting-config YAML — produced by `gen` or hand-rolled — and routes through the same orchestrator. Per-bot TTL overrides supported.
- Per-bot TTL timer; SIGINT/SIGTERM cleanly signals every bot to leave.
- An error in one bot's launch is logged and doesn't take the others down.

Phase 1 features carry through unchanged (`--ttl`, `--manifest`, `--auth`, `--sso-state-file`, etc.).

### Phase 1 still applies — the bot:

- launches headed Chrome with a configurable `--ttl` lifetime and a clean leave-meeting on TTL expiry or SIGINT/SIGTERM,
- auto-fills the homepage display-name form, clicks "Join Meeting" when shown, then clicks "Start camera" and "Unmute Microphone" so media actually flows — no human-in-the-loop required after launch,
- prepares per-participant fake audio (stitched WAV from `bot/conversation/lines/*.wav`) and fake video (y4m from `bot/assets/costumes/<name>/talking.mp4`) on demand,
- wires those files into Chrome via `--use-file-for-fake-{audio,video}-capture`,
- authenticates via:
  - **JWT cookie injection** for local / HCL daily / preview targets,
  - **Captured Playwright storage state** (`bots-app login`) for `app.videocall.rs` and any other real-OAuth-protected target,
  - **HCL SSO state** (`bots-app sso-login`) loaded _in addition to_ the JWT cookie for HCL-gated targets that sit behind the corporate SSO portal.

Backend is auto-picked by hostname unless `--auth` is set.

**Phase 2b** (random-N matrix testing via `bots-app gen`) is up next.

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

## Phase 4: control API (`--ctl-port` + `bots-app ctl`)

When `bots-app run` is invoked with `--ctl-port <port|auto>`, the orchestrator becomes long-lived and exposes an HTTP control surface so the running fleet can be introspected and mutated without restarting the process.

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
- Every endpoint except `/healthz` requires `Authorization: Bearer <token>`.
- The control server binds to `127.0.0.1` only — no network exposure.
- `e2e/bots-app/run/` is already `.gitignore`d, so token files never get committed. The token never leaves disk (it isn't logged or echoed to stdout); it's only written to the file mode-0600 token file.
- `ctl` auto-discovers the most-recently-started orchestrator's token file under `--run-dir` (default `e2e/bots-app/run`). Override with `--state-file <path>` or `--port <port> --token <token>`.

### Operational caveats

- **Network swap forces a reconnect.** `POST /bots/:id/network` (and `ctl tune`) rewrites the bot's URL with the new `?netsim=<profile>` param and re-navigates. The bot drops the meeting, re-runs `joinMeetingAndEnableMedia`, and rejoins the grid. This is intentional — the netsim shim is installed at client startup, so there's no way to swap profiles in place without a fresh page load. If you need to compare the same participant on two different profiles concurrently, use `ctl duplicate <id> --network <new_profile>` and leave the original running.
- **Done entries linger for ~60s.** A bot that completes its TTL or is leaved via `ctl leave` stays in `ctl list` (with `status=done` and a `finishReason`) for ~60 seconds before being swept. This lets a follow-up `ctl list` see the recent finish.
- **Dynamic add only via `ctl duplicate`.** There's no `ctl spawn <from-scratch>` today; new bots have to be cloned from an existing in-flight bot. That covers the canonical "fill a meeting around a human peer, then add one more" case; an arbitrary-participant spawn endpoint can be layered in later without a schema change.

## Network simulation (`--network <profile>`)

Both `bots-app run` and `bots-app gen` accept `--network <profile>`, and meeting-config YAML files accept `network:` at both the meeting level and per-bot. When set, the bot's meeting URL is rewritten to include `?netsim=<profile>` before navigation — the in-tab `videocall-client` (built with `--features netsim`) installs the matching shim on its WT + WS send paths to mimic a degraded peer. Without that build flag the URL param is parsed by the browser but silently ignored. Valid profiles: `none`, `good_wifi`, `good_4g`, `congested_wifi`, `lossy_mobile`, `satellite`, `dialup`. See discussion [#793](https://github01.hclpnp.com/labs-projects/videocall/discussions/793) phase 3.

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

## Preparing assets (prep-assets)

PR-1c adds a `prep-assets` subcommand that builds the per-participant audio + video files Chrome's `--use-file-for-fake-{audio,video}-capture` flags need. Run it once before launching bots that should send realistic media:

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
  --auth <backend>             Override auth backend: "jwt" or "storage-state" (default: auto by hostname)
  --storage-state-file <path>  Explicit storage-state JSON path (default: <assets-dir>/auth/<participant>.json)
  --sso-state-file <path>      HCL SSO state path (default: <assets-dir>/auth/hcl-sso.json; loaded only if present)
  --ctl-port <port|auto>       Phase 4: bind a local HTTP control API. "auto" lets the kernel pick a free port. Token file written to run/ctl-<pid>.token (mode 0600).

bots-app ctl <subcommand>      Phase 4 control client; auto-discovers the most recent run/ctl-*.token (override with --state-file / --port + --token).
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
```

## Roadmap

| Phase        | Status                            | What it adds                                                             |
| ------------ | --------------------------------- | ------------------------------------------------------------------------ |
| 1a           | :white_check_mark: done           | Scaffold + minimal CLI                                                   |
| 1b           | :white_check_mark: done           | `--ttl <duration>` flag + clean leave-meeting on TTL/SIGTERM             |
| 1c           | :white_check_mark: done           | Asset prep (costume MP4 → y4m, audio stitching from `bot/conversation/`) |
| 1d           | :white_check_mark: done           | Fake camera + mic wired into Chrome launch                               |
| 1e (this PR) | :construction_worker: in progress | Storage-state auth backend for `app.videocall.rs` + `bots-app login`     |
| 2            | :white_check_mark: done           | `--users N` multi-bot + `bots-app gen` random matrix                     |
| 3            | :white_check_mark: done           | Network simulation via WASM-injected `netsim.rs`                         |
| 4 (this PR)  | :white_check_mark: done           | Stateful orchestrator (`--ctl-port` + `bots-app ctl` subcommand family)  |
| 5            | pending                           | UX dashboard                                                             |

## Architecture (current)

```
e2e/
  helpers/auth.ts           ← existing; mints JWT session tokens
  bots-app/
    src/
      cli.ts                ← commander-based CLI (`run` + `prep-assets`)
      bot.ts                ← Playwright launch + cookie inject + navigate + leaveMeeting helper
      ttl.ts                ← parse "<int>s|m|h" / "infinite"; setTimeout-based scheduler
      ttl.test.ts           ← vitest unit tests for the duration parser + scheduler
      manifest.ts           ← typed loader for bot/conversation/manifest.yaml
      manifest.test.ts      ← vitest unit tests for the manifest parser
      stitcher.ts           ← ffmpeg-driven per-participant WAV stitcher (idempotent)
      costumes.ts           ← ffmpeg-driven MP4 → y4m converter (idempotent)
      assets.ts             ← resolves participant → {audioPath?, videoPath?} from run-dir
      assets.test.ts        ← vitest unit tests for the assets resolver
      meeting-join.ts       ← post-goto: fills display-name form, clicks Join Meeting, enables mic + camera
      meeting-join.test.ts  ← vitest placeholder (smoke test only — real coverage is the manual run)
      orchestrator.ts       ← runBotsToCompletion — Phase 4: Map<botId, Promise> wait loop + registry + control server wiring
      meeting-config.ts     ← parse / emit meeting-config YAML + seeded random-N generator
      meeting-config.test.ts ← vitest unit tests (22) for parse/emit, seeded RNG, shuffle, generate
      auth/
        jwt-cookie.ts       ← thin wrapper over helpers/auth.ts injectSessionCookie
        storage-state.ts    ← backend picker + captured-session path resolver (incl. HCL SSO state)
        storage-state.test.ts ← vitest unit tests
      control/              ← Phase 4: HTTP control surface + ctl client
        registry.ts         ← BotRegistryEntry + snapshot + retention sweeper
        registry.test.ts    ← vitest unit tests
        auth.ts             ← token generation + token-file IO + bearer header parsing
        auth.test.ts        ← vitest unit tests (mode 0600, round-trip, token-file discovery)
        server.ts           ← Node http.createServer routes (`/healthz`, `/bots`, `/bots/:id/*`)
        server.test.ts      ← in-process integration tests with mock OrchestratorControlSurface
        client.ts           ← thin node:http JSON client used by ctl subcommands
        client.test.ts      ← vitest unit tests
        ctl.ts              ← registerCtlCommands(program, runDir) — wires `bots-app ctl <subcmd>` family
    scripts/
      setup-assets.sh       ← thin wrapper over `npm run bot -- prep-assets`
    run/                    ← gitignored; per-participant stitched WAVs + costume y4m caches + Phase 4 ctl-<pid>.token files
    README.md               ← this file
```
