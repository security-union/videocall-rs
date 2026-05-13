# bots-app

Browser-driven bot CLI for videocall meetings. Runs a real Chrome instance via Playwright so the bot exercises the same WASM / WebCodecs / WebTransport code path a human peer would — used to recreate real-life issues against the deployed meeting stack while a human peer evaluates the result.

See discussion [#793](https://github01.hclpnp.com/labs-projects/videocall/discussions/793) for the design and implementation plan.

## Status — phase 1c

The bot launches headed Chrome with `--ttl`-bounded lifetime and clean leave-on-expiry (phase 1a + 1b), and there is now a `prep-assets` subcommand that prepares the per-participant fake audio (stitched WAVs from `bot/conversation/lines/*.wav`) and fake video (y4m files from `bot/assets/costumes/<name>/talking.mp4`) that Chrome's `--use-file-for-fake-*-capture` flags consume. **The bot itself does not yet wire those files into the Chrome launch** — that's PR-1d. **No support for `app.videocall.rs` yet** — that's PR-1e.

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

For HCL daily (`videocall.fnxlabs.com`), pull the secret from the cluster:

```bash
JWT_SECRET=$(kubectl --kubeconfig=$HCL_KUBECONFIG -n videocall get secret jwt-secret -o jsonpath='{.data.secret}' | base64 -d) \
  npm run bot -- run --meeting-url https://app.videocall.fnxlabs.com/meeting/TonyBots --participant alice
```

## Flags

```
bots-app run
  --meeting-url <url>          Full meeting URL (required)
  --participant <name>         Handle (alice/bob/...) or full email (required)
  --display-name <name>        Display name shown in the meeting (default: capitalized participant)
  --headless                   Run Chrome headless (default: headed)
  --ttl <duration>             Bot lifespan — "<int>s|m|h" or "infinite" (default: 5m)
```

## Development

```bash
cd e2e
npm run ci:lint               # eslint + prettier + tsc
npm run test:unit             # vitest unit tests for bots-app/
```

## Roadmap

| Phase        | Status                            | What it adds                                                              |
| ------------ | --------------------------------- | ------------------------------------------------------------------------- |
| 1a           | :white_check_mark: done           | Scaffold + minimal CLI                                                    |
| 1b           | :white_check_mark: done           | `--ttl <duration>` flag + clean leave-meeting on TTL/SIGTERM              |
| 1c (this PR) | :construction_worker: in progress | Asset prep (costume MP4 → y4m, audio stitching from `bot/conversation/`)  |
| 1d           | pending                           | Fake camera + mic wired into Chrome launch                                |
| 1e           | pending                           | Storage-state auth backend for `app.videocall.rs`                         |
| 2            | pending                           | `--users N` multi-bot + `bots-app gen` random matrix                      |
| 3            | pending                           | Network simulation via WASM-injected `netsim.rs`                          |
| 4            | pending                           | Stateful orchestrator (`bots-app orchestrator` daemon + `ctl` subcommand) |
| 5            | pending                           | UX dashboard                                                              |

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
      auth/
        jwt-cookie.ts       ← thin wrapper over helpers/auth.ts injectSessionCookie
    scripts/
      setup-assets.sh       ← thin wrapper over `npm run bot -- prep-assets`
    run/                    ← gitignored; per-participant stitched WAVs + costume y4m caches
    README.md               ← this file
```
