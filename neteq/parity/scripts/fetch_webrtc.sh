#!/usr/bin/env bash
# Sparse, shallow, blobless clone of Google WebRTC at the pinned revision the
# NetEq parity harness is validated against. Only the directories build.rs
# compiles from are checked out (~40 MB instead of several GB).
#
#   neteq/parity/scripts/fetch_webrtc.sh [DEST]
#   export WEBRTC_SRC=DEST
#
# DEST defaults to neteq/parity/third_party/webrtc (gitignored), which
# build.rs also uses when WEBRTC_SRC is unset. Re-running is idempotent.
set -euo pipefail

REV=6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
URL=https://webrtc.googlesource.com/src
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST="${1:-$SCRIPT_DIR/../third_party/webrtc}"

SPARSE_DIRS=(
  api
  common_audio
  modules/audio_coding/codecs
  modules/audio_coding/neteq
  modules/include
  modules/rtp_rtcp/include
  modules/rtp_rtcp/source
  rtc_base
  system_wrappers
)

if [[ -d "$DEST/.git" ]]; then
  current="$(git -C "$DEST" rev-parse HEAD 2>/dev/null || true)"
  if [[ "$current" == "$REV" ]]; then
    git -C "$DEST" sparse-checkout set "${SPARSE_DIRS[@]}"
    echo "WebRTC already at $REV in $DEST"
    exit 0
  fi
else
  mkdir -p "$DEST"
  git -C "$DEST" init -q
  git -C "$DEST" remote add origin "$URL"
fi

git -C "$DEST" config remote.origin.promisor true
git -C "$DEST" config remote.origin.partialclonefilter blob:none
git -C "$DEST" sparse-checkout set "${SPARSE_DIRS[@]}"
git -C "$DEST" fetch --depth 1 --filter=blob:none origin "$REV"
git -C "$DEST" checkout -q --detach FETCH_HEAD

got="$(git -C "$DEST" rev-parse HEAD)"
if [[ "$got" != "$REV" ]]; then
  echo "error: expected $REV, got $got" >&2
  exit 1
fi
echo "WebRTC $REV ready in $DEST"
echo "export WEBRTC_SRC=$(cd "$DEST" && pwd)"
