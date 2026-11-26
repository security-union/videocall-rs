# macOS Apple Silicon Setup Guide

Setup on an M-series Mac is two commands — no Docker Desktop, no VM, everything
runs as native processes.

## Setup

```bash
git clone https://github.com/security-union/videocall-rs.git
cd videocall-rs
make dev
```

`make dev` installs [Nix](https://nixos.org/download/) if you don't have it
(official installer — open a new terminal afterward and re-run), then boots the
whole stack in a process-compose TUI: postgres, NATS, prometheus, grafana, and
every service under cargo-watch with hot reload.

One-time speedup — add yourself to Nix's `trusted-users` so builds pull the
project's public binary cache instead of compiling locally:

```bash
echo "trusted-users = root $USER" | sudo tee -a /etc/nix/nix.conf
sudo pkill nix-daemon
```

## Use

- Open http://localhost:3001 and navigate to
  `http://localhost:3001/meeting/<username>/<meeting-id>`.
- Stop with `F10`/`Ctrl-C` in the TUI, or `make dev-down` from another terminal.
- `make help` lists everything else (tests, single-service watchers, e2e).

## Troubleshooting

- **Slow first build with `ignoring untrusted substituter` warnings** — you
  skipped the `trusted-users` step above.
- **TUI startup error about `/dev/tty`** — `make dev` needs an interactive
  terminal; for scripted use run
  `nix-shell default.nix -A shells.dev --run "dev-stack -t=false"`.
- **Ports 5432/4222/8080/3001 already in use** — something else owns them
  (an old Docker stack? `docker ps`). `make status` shows which native stacks
  are running.

Coming from the old Docker-based setup? See
[migrating-from-docker.md](migrating-from-docker.md).
