# VP9 encoder performance

Whole-frame encode time, **90-frame `moving_box` sequence, release build, Apple
Silicon (M-series, aarch64)**. Criterion medians; ~±3–10% run-to-run variance
(machine-load sensitive). `libvpx` is the mature C reference (hand-written SIMD
+ threads); the Rust columns are the pure-Rust encoder in this crate.

## Median encode time per frame

| Resolution | libvpx (2 threads) | Rust — scalar, single-tile | Rust — + tile-parallel (rayon) | Rust — + SIMD kernels¹ |
|---|---|---|---|---|
| 640×480  | 0.39 ms | 2.00 ms | 1.10 ms | **0.76 ms** |
| 1280×720 | 0.89 ms | 5.71 ms | 1.75 ms | **1.26 ms** |

¹ SIMD = SAD + forward-DCT + quantize. **These are aarch64 (NEON) numbers where
all three kernels are vectorized.** On x86_64, SAD and quantize are SIMD
(SSE4.1/AVX2) but **fdct is still scalar** (see TODO), so x86 currently sits
between the "tile-parallel" and "SIMD" columns.

## Gap to libvpx

| Resolution | scalar, single-tile | now (rayon + SIMD, aarch64) |
|---|---|---|
| 640×480  | 5.1× slower | **1.9× slower** |
| 1280×720 | 6.4× slower | **1.4× slower** |

## The two axes, closed one at a time

- **Threading** (scalar single-tile → rayon tiles): 1.81× @640×480 (2-tile
  ceiling — VP9's 256 px min tile width), 3.26× @720p (4 tiles).
- **SIMD** (rayon → + kernels): SAD −11.9%/−9.6%, then fdct+quantize
  −10.6%/−10.7% on top. Every kernel is **bit-identical to scalar** (proven by a
  debug SIMD==scalar assert across the full oracle suite), so the emitted
  bitstream is byte-for-byte unchanged. NEON (aarch64), SSE4.1/AVX2
  (x86_64, runtime-detected), scalar fallback on wasm/other. Bonus: the quantize
  scalar refactor made the scalar/wasm path ~1.9× faster on its own.

_Reproduce:_ `cargo bench -p videocall-codecs --features "libvpx test-utils" --bench vp9_encode`

---

# TODO / next steps

Ordered by leverage. The encoder is realtime with wide margin already; these are
about closing the remaining gap to libvpx and enabling new consumers.

### Performance
1. **x86 forward-DCT SIMD.** fdct is NEON-only; x86 keeps the exact scalar
   transform because bit-exactness couldn't be verified without an x86 host.
   Implement SSE4.1/AVX2 fdct kernels and verify bit-identity on x86 (Rosetta
   runs SSE but not AVX2; needs real x86 CI). Until then x86 leaves ~10% on the
   table vs aarch64.
2. **x86 CI coverage for the SIMD kernels.** CI runs on aarch64, so the
   SSE4.1/AVX2 paths are compile-checked but not *run* in CI. Add an x86_64
   runner (or a QEMU/Rosetta step) that executes the `simd_*_matches_scalar`
   tests on the real x86 kernels — otherwise an x86-only SIMD bug ships unseen.
3. **wasm SIMD128.** The wasm (browser) path is fully scalar. `core::arch::wasm32`
   SIMD128 kernels for SAD/fdct/quantize would speed up the client encoder;
   gate behind `#[cfg(target_feature = "simd128")]` + build flag.
4. **row-mt (row-wavefront) within a tile.** Tile-column parallelism caps at 2×
   @640×480 (the 256 px min-tile-width ceiling). A bit-exact SB-row wavefront
   (libvpx's `row_mt`) is the only way past that at low res — but it needs
   unsafe/fine-grained sync (disjoint `&mut` bands + per-row atomics). High
   effort; defer until the SIMD axis is exhausted.
5. **Allocation churn.** ~18k allocs/frame (per-block `qcoeff.to_vec`, token
   Vecs) + per-tile full-frame recon/grid buffers. Scratch-buffer reuse /
   arena — matters for Raspberry-Pi-class devices, invisible on desktop.
6. **32×32 / 64×64 partitions.** Signaling floor is ~290 kbps @640×480 (58% of a
   500 kbps budget). Larger partitions for static regions → est. 3–8× floor
   reduction. Quality/bitrate lever, orthogonal to speed.

### Quality (deferred, per the original scope: correctness + realtime first)
7. Sub-pel motion comp (convolve8), loop filter, V/H/TM intra modes, ADST.

### iOS / Swift (UniFFI) — the encoder is ready; the DECODER is the blocker
The repo already ships an iOS pattern in `videocall-sdk` (`src/videocall.udl` +
`build_ios.sh` → xcframework). Reuse it:

8. **Pure-Rust VP9 ENCODER on iOS — READY.** `videocall_codecs::vp9::Vp9Encoder`
   is pure Rust and builds for `aarch64-apple-ios` / `-sim` with **zero C deps**.
   Expose it to Swift via a **new thin UniFFI wrapper crate** (e.g.
   `videocall-codecs-ffi`) that depends on `videocall-codecs`, NOT by adding
   UniFFI to `videocall-codecs` itself (that crate targets wasm32 + `cdylib`
   for the browser; bolting on `uniffi`/`staticlib`/iOS would pollute it).
   Mirror `videocall-sdk`: `.udl` or `#[uniffi::export]` surface (encoder handle
   + `encode(pts, bytes) -> Option<bytes>`), `crate-type = ["staticlib"]`,
   `build_ios.sh` → xcframework + Swift bindings.
9. **Pure-Rust VP9 DECODER — DOES NOT EXIST.** `decoder/native.rs` is C libvpx
   (`vpx_sys`); `decoder/wasm.rs` is browser WebCodecs. Neither is a pure-Rust
   decoder, and iOS has no reliable system VP9 decoder (VideoToolbox VP9 HW
   decode is absent on most iPhones). So a pure-Rust encode+decode iOS story
   needs a **new pure-Rust VP9 decoder** — a substantial project on the order of
   the encoder (bitstream parse + inverse transforms + intra/inter recon + loop
   filter). Until then, iOS decode means shipping C libvpx (works on iOS, but
   reintroduces the C dependency) or a non-VP9 codec.
