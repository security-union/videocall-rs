#!/usr/bin/env bash
# Builds golden_gen against a WebRTC checkout ($1, default ~/Documents/webrtc-src).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
W="${1:-$HOME/Documents/webrtc-src}"
SPL=$W/common_audio/signal_processing
NQ=$W/modules/audio_coding/neteq
OUT="${OUT:-$HERE/obj}"
FLAGS=(-O1 -fwrapv -ffp-contract=off -DNDEBUG -I"$HERE/stub" -I"$W" -I/opt/homebrew/include)
mkdir -p "$OUT"; rm -f "$OUT"/*.o
for f in min_max_operations cross_correlation downsample_fast copy_set_operations division_operations spl_sqrt vector_scaling_operations vector_operations filter_ar_fast_q12 filter_ma_fast_q12 filter_ar levinson_durbin randomization_functions energy get_scaling_square get_hanning_window spl_inl; do
  cc -std=c11 "${FLAGS[@]}" -c "$SPL/$f.c" -o "$OUT/$f.o"
done
cc -std=c11 "${FLAGS[@]}" -c "$W/common_audio/third_party/spl_sqrt_floor/spl_sqrt_floor.c" -o "$OUT/spl_sqrt_floor.o"
for f in dot_product_with_scale auto_correlation; do
  c++ -std=c++20 "${FLAGS[@]}" -c "$SPL/$f.cc" -o "$OUT/$f.o"
done
for f in audio_vector audio_multi_vector sync_buffer dsp_helper cross_correlation random_vector; do
  c++ -std=c++20 "${FLAGS[@]}" -c "$NQ/$f.cc" -o "$OUT/nq_$f.o"
done
c++ -std=c++20 "${FLAGS[@]}" "$HERE/golden_gen.cc" "$OUT"/*.o -o "$OUT/golden_gen"
"$OUT/golden_gen"
