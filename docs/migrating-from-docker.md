# Migrating from the Docker dev environment

You were running the old workflow: `docker compose up` with every service in a
`nixos/nix` container, per-service `nix-store-*` volumes, bind-mounted repo,
cargo building inside Docker. That's gone. Development is native now — this is
how to switch. (Why we did it: [the vlog post](https://vlog.videocall.rs/posts/we-deleted-docker/).)

## TL;DR

```bash
git pull
make dev
```

`make dev` installs Nix if you don't have it (official nixos.org installer),
then boots the whole stack — postgres, NATS, prometheus, grafana, and every
service under cargo-watch — as native processes in a process-compose TUI.
First run compiles the toolchain caches (minutes, once); after that startup is
seconds. Works on macOS, Linux, and Windows via WSL2.

## What replaced what

| you used to run | now run |
|---|---|
| `docker compose up` (dev stack) | `make dev` |
| `docker compose down` | quit the TUI, or `make dev-down` |
| `docker compose up postgres nats …` | `make dev-middleware` |
| `docker compose build` | nothing — no dev images exist anymore |
| shell inside a service container | `make shell` (pinned toolchain, on your host) |
| integration tests via compose | `make tests_run` (native, throwaway state) |
| `docker compose logs -f <svc>` | the TUI shows per-process logs; `F` to follow |

Docker itself is now only needed for two things: running the *production*
images locally (`make up`, built by Nix and loaded into Docker) and the e2e
Playwright stack (`make e2e-ci`). If you don't use those, you don't need
Docker running at all.

## One-time setup

1. **Nix** — let `make dev` install it, or do it yourself first:
   `curl -L https://nixos.org/nix/install | sh -s -- --daemon`
   (Windows: inside WSL2; without systemd use `--no-daemon`.)
   Open a new terminal afterward so `nix-shell` is on PATH.

2. **Binary cache** — add yourself to `trusted-users` or the build ignores the
   project cache and compiles everything locally:
   ```bash
   echo "trusted-users = root $USER" | sudo tee -a /etc/nix/nix.conf
   sudo pkill nix-daemon
   ```
   Symptom you skipped this: `warning: ignoring untrusted substituter
   'https://videocall-rs.cachix.org'` and a very long first build.

3. **Your `.env` still works.** The same file is read by the native stack, now
   layered as *defaults* < `.env` < `.env.local` (put secrets in `.env.local`,
   it's gitignored).

## Clean up the old Docker footprint

The old setup left heavy residue — per-service Nix stores were multi-GB
volumes. Once you're on `make dev`:

```bash
docker compose -f docker/docker-compose.yaml down --remove-orphans
docker volume ls | grep nix-store        # the old per-service stores
docker system prune -a --volumes         # reclaims tens of GB — removes ALL unused images/volumes
```

`docker system prune -a --volumes` deletes every unused image and volume on
your machine, not just this project's — check `docker volume ls` first if you
have other projects.

## Data: your old dev database does not migrate

The old postgres lived in a Docker volume; the native stack keeps state in
`.data/postgres` (postgres 18) and starts fresh. Dev databases are throwaway
by design — migrations recreate the schema on first boot. If you had dev data
you actually care about, `pg_dump` it from the old container before pruning.

## Troubleshooting

- **`make dev` dies with a TUI/tty error** — it needs an interactive
  terminal. Headless/scripted: `nix-shell default.nix -A shells.dev --run
  "dev-stack -t=false"`.
- **Port already in use (5432/4222/8080/…)** — the old Docker stack is still
  running. `docker compose -f docker/docker-compose.yaml down` first.
- **`websocket` can't bind / UI can't connect on 8080** — something else owns
  the port; the process-compose API deliberately sits on 28080 to stay out of
  the way.
- **postgres refuses to start after a pull** — `.data/postgres` from an older
  major version; the stack only accepts postgres 18 data dirs. Delete
  `.data/postgres` (dev data is throwaway) and re-run.
