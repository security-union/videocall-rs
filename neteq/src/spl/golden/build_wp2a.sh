#!/usr/bin/env bash
# Builds golden_gen_wp2a (BackgroundNoise / Expand / Normal / Merge goldens)
# against a WebRTC checkout ($1, default $WEBRTC_SRC) and runs it.
#
# Unlike build.sh (WP1), this compiles NetEq headers that include api/ and
# abseil, so it uses the real rtc_base/checks.h and needs an abseil newer than
# 2025 (for absl_nullable). Point ABSL_INCLUDE at its include dir, or run inside
# `nix-shell neteq/parity/shell.nix`, where pkg-config finds it.
set -euo pipefail
. "$(dirname "$0")/common.sh"
if [ -z "$ABSL_INCLUDE" ]; then
  echo "error: abseil not found. Set ABSL_INCLUDE or run inside nix-shell neteq/parity/shell.nix." >&2
  exit 1
fi
PLATFORM=-DWEBRTC_POSIX
case "$(uname)" in
  Darwin) PLATFORM="$PLATFORM -DWEBRTC_MAC" ;;
  *) PLATFORM="$PLATFORM -DWEBRTC_LINUX" ;;
esac
# shellcheck disable=SC2206
FLAGS=($PLATFORM -DNDEBUG $HARDENING -DRTC_DISABLE_LOGGING -DRTC_DISABLE_TRACE_EVENTS
  -O1 -fwrapv -ffp-contract=off -I"$W" -I"$ABSL_INCLUDE" -w)
for f in min_max_operations cross_correlation downsample_fast copy_set_operations division_operations spl_sqrt vector_scaling_operations filter_ar_fast_q12 filter_ma_fast_q12 levinson_durbin spl_inl spl_init vector_operations; do
  "$CC" -std=c11 "${FLAGS[@]}" -c "$SPL/$f.c" -o "$OUT/$f.o"
done
"$CC" -std=c11 "${FLAGS[@]}" -c "$W/common_audio/third_party/spl_sqrt_floor/spl_sqrt_floor.c" -o "$OUT/spl_sqrt_floor.o"
"$CXX" -std=c++20 "${FLAGS[@]}" -c "$SPL/dot_product_with_scale.cc" -o "$OUT/dot_product_with_scale.o"
for f in audio_vector audio_multi_vector sync_buffer dsp_helper cross_correlation random_vector background_noise expand normal merge statistics_calculator expand_uma_logger; do
  "$CXX" -std=c++20 "${FLAGS[@]}" -c "$NQ/$f.cc" -o "$OUT/nq_$f.o"
done
for f in api/neteq/tick_timer rtc_base/checks system_wrappers/source/metrics; do
  "$CXX" -std=c++20 "${FLAGS[@]}" -c "$W/$f.cc" -o "$OUT/$(basename "$f").o"
done
"$CXX" -std=c++20 "${FLAGS[@]}" "$HERE/golden_gen_wp2a.cc" "$OUT"/*.o -o "$OUT/golden_gen_wp2a"
"$OUT/golden_gen_wp2a"
