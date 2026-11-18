# Sourced by build*.sh. Resolves the WebRTC checkout, abseil include dir,
# compilers and a scratch output dir. POSIX-ish, no platform-specific flags.
HERE="$(cd "$(dirname "$0")" && pwd)"
W="${1:-${WEBRTC_SRC:-}}"
if [ -z "$W" ] || [ ! -d "$W/common_audio" ]; then
  echo "error: WebRTC sources not found. Set WEBRTC_SRC (or pass the path as \$1)." >&2
  echo "       Easiest: nix-shell neteq/parity/shell.nix --run '$0'" >&2
  exit 1
fi
SPL=$W/common_audio/signal_processing
NQ=$W/modules/audio_coding/neteq
CC="${CC:-cc}"
CXX="${CXX:-c++}"
# Build outputs go to a scratch dir (override with OUT=...), removed on exit.
if [ -z "${OUT:-}" ]; then
  OUT="$(mktemp -d "${TMPDIR:-/tmp}/neteq-golden.XXXXXX")"
  trap 'rm -rf "$OUT"' EXIT
fi
mkdir -p "$OUT"
rm -f "$OUT"/*.o
# Abseil include dir: ABSL_INCLUDE overrides, else pkg-config (nix shell).
# Only build_wp2a.sh requires it.
if [ -z "${ABSL_INCLUDE:-}" ]; then
  ABSL_INCLUDE="$(pkg-config --variable=includedir absl_base 2>/dev/null || true)"
fi
# libc++ hardening (on by default in newer toolchains) traps on the harmless
# one-past-the-end &vec[size] that upstream AudioVector::CopyTo takes for an
# empty copy. Turn the checks off; libstdc++ ignores the macro.
HARDENING=-D_LIBCPP_HARDENING_MODE=_LIBCPP_HARDENING_MODE_NONE
