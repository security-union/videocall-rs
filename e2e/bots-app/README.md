# bots-app

Browser-driven bot CLI for videocall meetings. Runs a real Chrome instance via Playwright so the bot exercises the same WASM / WebCodecs / WebTransport code path a human peer would — used to recreate real-life issues against the deployed meeting stack while a human peer evaluates the result.

See discussion [#793](https://github01.hclpnp.com/labs-projects/videocall/discussions/793) for the design and implementation plan.

## Status — phase 1b

The bot now launches headed Chrome, injects a JWT session cookie, navigates to a meeting URL, holds the session for the configured `--ttl` (default 5 minutes), then cleanly clicks the meeting's "Hang Up" button and exits. SIGINT / SIGTERM trigger the same clean-leave path. **No fake audio/video yet (PR-1c/1d), no support for `app.videocall.rs` yet (PR-1e).**

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
| 1b (this PR) | :construction_worker: in progress | `--ttl <duration>` flag + clean leave-meeting on TTL/SIGTERM              |
| 1c           | pending                           | Asset prep (costume MP4 → y4m, audio stitching from `bot/conversation/`)  |
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
      cli.ts                ← commander-based CLI entry point + TTL scheduler
      bot.ts                ← Playwright launch + cookie inject + navigate + leaveMeeting helper
      ttl.ts                ← parse "<int>s|m|h" / "infinite"; setTimeout-based scheduler
      ttl.test.ts           ← vitest unit tests for the duration parser + scheduler
      auth/
        jwt-cookie.ts       ← thin wrapper over helpers/auth.ts injectSessionCookie
    README.md               ← this file
```
