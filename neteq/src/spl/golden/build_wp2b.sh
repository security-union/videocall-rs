#!/usr/bin/env bash
# WP2b golden generator: builds golden_gen_wp2b against a WebRTC checkout
# ($1, default $WEBRTC_SRC) and prints the hashes the WP2b tests
# check (time_stretch.rs, accelerate.rs, preemptive_expand.rs,
# comfort_noise.rs, codecs/cng.rs). Same flags as build.sh (generic C, no NEON).
set -euo pipefail
. "$(dirname "$0")/common.sh"
# stub_wp2b shadows decoder_database.h / packet.h / logging.h for ComfortNoise.
FLAGS=(-O1 -fwrapv -ffp-contract=off -DNDEBUG $HARDENING -I"$HERE/stub_wp2b" -I"$HERE/stub" -I"$W")
for f in min_max_operations cross_correlation downsample_fast copy_set_operations division_operations spl_sqrt vector_scaling_operations vector_operations filter_ar_fast_q12 filter_ma_fast_q12 filter_ar levinson_durbin randomization_functions energy get_scaling_square get_hanning_window spl_inl; do
  "$CC" -std=c11 "${FLAGS[@]}" -c "$SPL/$f.c" -o "$OUT/$f.o"
done
"$CC" -std=c11 "${FLAGS[@]}" -c "$W/common_audio/third_party/spl_sqrt_floor/spl_sqrt_floor.c" -o "$OUT/spl_sqrt_floor.o"
for f in dot_product_with_scale auto_correlation; do
  "$CXX" -std=c++20 "${FLAGS[@]}" -c "$SPL/$f.cc" -o "$OUT/$f.o"
done
for f in audio_vector audio_multi_vector sync_buffer dsp_helper cross_correlation background_noise time_stretch accelerate preemptive_expand comfort_noise; do
  "$CXX" -std=c++20 "${FLAGS[@]}" -c "$NQ/$f.cc" -o "$OUT/nq_$f.o"
done
"$CXX" -std=c++20 "${FLAGS[@]}" -c "$W/modules/audio_coding/codecs/cng/webrtc_cng.cc" -o "$OUT/webrtc_cng.o"
"$CXX" -std=c++20 "${FLAGS[@]}" "$HERE/golden_gen_wp2b.cc" "$OUT"/*.o -o "$OUT/golden_gen_wp2b"
"$OUT/golden_gen_wp2b"
