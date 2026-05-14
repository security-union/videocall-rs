# bots-app dashboard (phase 5)

Browser-based UX dashboard for launching and managing browser-driven videocall bots.
Layers on top of the phase-4 stateful orchestrator + HTTP control API (see
[discussion #793](https://github01.hclpnp.com/labs-projects/videocall/discussions/793)).

## Quick start

```
cd <repo>/e2e
npm run bot -- dashboard
```

That's it. The dashboard now spawns the orchestrator + ctl server in-process —
no separate `bots-app run` terminal needed. The launch form on the Bots page
adds bots; the table below shows running ones.

The dashboard listens on `http://127.0.0.1:5174/` (built mode) or
`http://127.0.0.1:5173/` (Vite dev mode). The Layout header shows a
**Self-hosted daemon** badge with the PID so you know which mode is active.

### Attach mode

If you already have a `bots-app run --ctl-port auto` running and want the
dashboard to talk to it instead of spawning its own daemon, pass one of:

```
npm run bot -- dashboard --ctl-token-file run/ctl-12345.token
# or
npm run bot -- dashboard --ctl-port 4321 --ctl-token <token>
```

The Layout header switches to **ctl :<port>** in that mode.

## Architecture

```
+-------------------+         /api/*           +-------------------+        Bearer <token>          +-----------------------+
|  Browser tab      |  ----------------------> |  Dashboard sidecar | -----------------------------> |  bots-app run         |
|  (React + Tailwind)|  <----------------------|  (Node http server)| <----------------------------- |  ctl HTTP API (4xxxx) |
+-------------------+    JSON, no auth header  +-------------------+    Token injected server-side   +-----------------------+
                                                       |
                                                       +-- /api/assets/audio        listed from run/audio
                                                       +-- /api/assets/costumes     listed from run/costumes
                                                       +-- /api/daemon              { port, pid, startedAt }
```

### Security model

- The dashboard's Node sidecar binds to `127.0.0.1` only — never exposed over the network.
- The phase-4 ctl-API bearer token lives only in the Node sidecar process. The browser
  fetches `/api/*` without any `Authorization` header; the sidecar attaches
  `Authorization: Bearer <token>` server-side before forwarding to the ctl API.
- The token never reaches the browser tab and is never logged.
- All proxied endpoints are 127.0.0.1 ⇄ 127.0.0.1, never crossing the network.

### Why a proxy (not "token in the page")?

Two reasons:

1. The dashboard tab can run third-party scripts (a Tailwind plugin gone rogue,
   a Vite import-analysis stub, a browser extension), and a token in
   `window.__BOOTSTRAP__` would be readable by all of them.
2. The token rotates every time `bots-app run` is restarted. Keeping it on the
   server side means a long-lived dashboard tab doesn't end up holding a stale
   bearer; the next dashboard request just goes through the new token attached
   by the sidecar's latest discovery.

## CLI flags

| Flag | Default | Notes |
|---|---|---|
| `--port <port>` | `5174` | Dashboard Node sidecar port. `0` lets the kernel pick. |
| `--ctl-token-file <path>` | (self-hosted mode) | Attach to an existing daemon. Setting this (or `--ctl-port`/`--ctl-token`) disables in-process orchestrator spawn. |
| `--ctl-port <port>` | (self-hosted mode) | Attach mode; combine with `--ctl-token`. |
| `--ctl-token <token>` | (self-hosted mode) | Attach mode; combine with `--ctl-port`. |
| `--run-dir <dir>` | `e2e/bots-app/run` | Token files, asset listings, and `<runDir>/profiles/*.json`. |
| `--no-open` | (open) | Skip auto-opening the dashboard URL in the operator's browser. |
| `--dist-dir <dir>` | `e2e/bots-app/dashboard/dist` | Where to serve the built UI from. |

If `--dist-dir` exists and contains an `index.html`, the dashboard runs in
**built mode** and serves static files directly from Node. Otherwise it falls
back to **dev mode**: it spawns `npm run dev` (Vite) inside `dashboard/` and
proxies `/api/*` from Vite to the Node sidecar.

## Tech stack

- Vite + React 18 + TypeScript
- Tailwind CSS v3
- Radix UI primitives (Dialog, Select, RadioGroup, Switch, Tooltip, Toast)
- Lucide-react icons
- TanStack Query for ctl-API state

All dependencies are local to `dashboard/package.json` — the parent
`e2e/package.json` is untouched.

## Development

```
cd e2e/bots-app/dashboard
npm install
npm run dev          # Vite dev server (with /api proxy)
npm run typecheck    # tsc --noEmit
npm run lint         # eslint src
npm run test         # vitest (run --run for CI mode)
npm run build        # produce ./dist
```

The sidecar speaks to a real running orchestrator, so `npm run dev` by itself
won't show any bots — start `bots-app run --ctl-port auto` in another terminal
first, then `bots-app dashboard` (which boots Vite for you).

## What's covered today

- Launch form with all phase-4 launch options (meeting URL, participant, display
  name, TTL with suggestion chips, network preset, headless toggle, auth
  backend, storage-state file, costume + audio asset hints).
- Running-bots table with status badges, live TTL countdown, meeting link,
  network profile, and per-row actions: Extend / Set TTL, Mute, Toggle camera,
  Toggle share, Duplicate, Leave meeting, Force kill.
- Auto-collapse of the launch form once at least one bot is running.
- Duplicate-bot flow pre-fills the launch form with the source bot's settings.
- Run-location pick list with "Local machine" enabled and "Cloud VM", "SSH host",
  "Docker container" disabled with a "Future feature — see discussion #793" tooltip.

## Deferred (future PRs)

- Run-location backends other than `local` (cloud VM, SSH-able host, Docker).
- Cookieless / auto-rotating tokens (today the token rotates when the
  orchestrator restarts; the dashboard discovers the new file but doesn't
  notify the operator).
- Dark mode + responsive table layout for narrow viewports.
- Server-pushed bot updates (SSE / WebSocket) so the bot list reflects
  reality faster than the 2.5s poll.
