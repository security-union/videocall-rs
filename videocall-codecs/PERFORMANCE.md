# VP9 encoder performance

Whole-frame encode time, **90-frame `moving_box` sequence, release build, Apple
Silicon (M-series)**. Numbers are Criterion medians; expect ~±3% run-to-run
variance. `libvpx` is the mature C reference (hand-written SIMD + threads); the
Rust columns are the pure-Rust encoder in this crate.

## Median encode time per frame

| Resolution | libvpx (C, 2 threads) | libvpx (C, 1 thread) | Rust — scalar, single-tile | Rust — + tile-parallel (rayon) | Rust — + SIMD SAD¹ |
|---|---|---|---|---|---|
| 640×480  | 0.39 ms | 0.63 ms | 2.00 ms | 1.10 ms | 0.98 ms |
| 1280×720 | 0.89 ms | — | 5.71 ms | 1.75 ms | 1.59 ms |

¹ SIMD so far covers **motion-search SAD only**. Forward-DCT + quantize SIMD is
in progress and will lower the last column further.

## How to read it

Two independent axes separate the Rust encoder from libvpx, and we're closing
them one at a time:

- **Threading.** `Rust scalar single-tile → + tile-parallel` is the rayon
  tile-column work. 640×480 gets **1.81×** (2 tiles — VP9's 256 px minimum tile
  width caps 640 at 2 columns, the same ceiling that gives libvpx its ~1.9×
  from 2 threads); 720p gets **3.26×** (4 tiles).
- **SIMD (scalar → vectorized kernels).** `+ tile-parallel → + SIMD SAD` is the
  first SIMD kernel: NEON on aarch64, SSE2/AVX2 (runtime-detected) on x86_64,
  scalar fallback on wasm/other. **−11.9%** at 640×480, **−9.6%** at 720p, and
  it's **byte-for-byte identical** output (bit-exact SAD → same motion vectors →
  same bitstream).

## Gap to libvpx

| Resolution | before (scalar, single-tile) | now (rayon + SIMD SAD) |
|---|---|---|
| 640×480  | 5.1× slower | 2.5× slower |
| 1280×720 | 6.4× slower | 1.8× slower |

The remaining gap is almost entirely the rest of the SIMD axis — libvpx
vectorizes the forward DCT and quantize kernels that the Rust encoder still runs
scalar. Those are the next kernels (Stage 2). Beyond that, `row-mt` (a bit-exact
row wavefront within a tile) is the only way past the 2-tile ceiling at 640×480.

_Reproduce:_ `cargo bench -p videocall-codecs --features "libvpx test-utils" --bench vp9_encode`
