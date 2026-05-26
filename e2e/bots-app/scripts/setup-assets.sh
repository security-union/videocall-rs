#!/usr/bin/env bash
# One-shot script that prepares per-participant audio (stitched WAVs) and
# costume video (y4m files) for the bots-app browser bot.
#
# This is a thin wrapper over `npm run bot -- prep-assets` so the entry
# point can be invoked from any cwd. Both forms are idempotent — re-runs
# only ffmpeg work when the source files have changed.
#
# Prereqs:
#   - `python3 bot/generate-conversation-edge.py` has been run, producing
#     bot/conversation/{manifest.yaml,lines/*.wav}.
#   - `costume-videos.zip` has been unpacked into bot/assets/costumes/
#     (so each costume has its talking.mp4 alongside the I420 cache).
#   - ffmpeg is on PATH.

set -euo pipefail

cd "$(dirname "$0")/.."
cd ../..  # repo root

exec npm --prefix e2e run bot -- prep-assets "$@"
