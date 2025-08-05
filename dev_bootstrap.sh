#!/usr/bin/env bash
# -----------------------------------------------------------------------------
# dev_bootstrap.sh
# -----------------------------------------------------------------------------
# Convenience launcher for local development.
# 1. Ensures a Docker daemon is running (Docker Desktop or Colima).
# 2. Starts the full container stack via `make up` with streaming logs.
#
# Run from repo root:
#   ./dev_bootstrap.sh
# -----------------------------------------------------------------------------
set -euo pipefail

bold="\033[1m"; reset="\033[0m"

log()   { echo -e "${bold}[dev-bootstrap]${reset} $*"; }
error() { echo -e "${bold}[dev-bootstrap][ERROR]${reset} $*" >&2; }

have_docker() {
  docker info >/dev/null 2>&1
}

start_docker_desktop_macos() {
  if [[ "$OSTYPE" == "darwin"* ]]; then
    if ! pgrep -f "Docker Desktop.app" >/dev/null 2>&1; then
      log "Attempting to launch Docker Desktop…"
      open -g -a "Docker" || true
    fi
  fi
}

# 1. Ensure Docker daemon
start_docker_desktop_macos
if ! have_docker; then
  log "Docker daemon not running."
  start_colima_if_available
fi

if ! have_docker; then
  error "Docker daemon still not available. Please start Docker Desktop or Colima then re-run this script."
  exit 1
fi

log "Docker daemon is running. Bringing up containers…"

# 2. Stream compose logs (foreground)
make up
