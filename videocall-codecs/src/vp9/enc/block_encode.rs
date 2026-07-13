/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! The per-block intra encode kernel.
//!
//! [`encode_intra_block`] runs the full pixel-domain pipeline for one transform
//! block: DC-predict from already-reconstructed neighbors, form the residual
//! against the source, forward transform, quantize, dequantize, and add the
//! inverse transform back into the reconstruction buffer in place. The
//! reconstruction it leaves behind is bit-identical to what the decoder will
//! produce from the emitted coefficients — this is the kernel the stage-4
//! superblock walk will call per block.

use crate::vp9::common::block::TxSize;
use crate::vp9::common::idct::{idct4x4_add, idct8x8_add};
use crate::vp9::common::intra_pred::build_intra_dc;
use crate::vp9::enc::fdct::{fdct4x4, fdct8x8};
use crate::vp9::enc::quantize::{quantize_fp, QuantParams};

/// Result of encoding one transform block.
#[derive(Clone, Debug)]
pub struct BlockResult {
    /// Quantized coefficients in raster order (`bs²` entries).
    pub qcoeff: Vec<i16>,
    /// End-of-block count (0 means the block quantized to all zeros).
    pub eob: u16,
    /// True when the block carries no residual (`eob == 0`).
    pub skip: bool,
}

/// Position and geometry of a transform block within its plane.
pub struct BlockCtx {
    pub tx_size: TxSize,
    /// Block top-left offset into the (bordered) plane buffers.
    pub off: usize,
    /// Neighbor availability (frame/tile relative).
    pub up_available: bool,
    pub left_available: bool,
    /// Block top-left position in plane pixels, for edge replication.
    pub x0: i32,
    pub y0: i32,
    /// mi-aligned plane dimensions.
    pub frame_w: i32,
    pub frame_h: i32,
}

/// Encode one intra transform block.
///
/// `recon`/`recon_stride` is the reconstruction plane (predicted from and
/// written to in place); `src`/`src_off`/`src_stride` locate the matching
/// source block. On return the `recon` block holds the final reconstruction.
pub fn encode_intra_block(
    recon: &mut [u8],
    recon_stride: usize,
    src: &[u8],
    src_off: usize,
    src_stride: usize,
    ctx: &BlockCtx,
    qp: &QuantParams,
) -> BlockResult {
    let bs = 4usize << (ctx.tx_size as usize);
    let n = bs * bs;

    // 1. DC prediction into the reconstruction block.
    build_intra_dc(
        recon,
        ctx.off,
        recon_stride,
        ctx.tx_size,
        ctx.up_available,
        ctx.left_available,
        ctx.x0,
        ctx.y0,
        ctx.frame_w,
        ctx.frame_h,
    );

    // 2. Residual = source - prediction (prediction currently in `recon`).
    let mut residual = [0i16; 64];
    for r in 0..bs {
        for c in 0..bs {
            let s = src[src_off + r * src_stride + c] as i16;
            let p = recon[ctx.off + r * recon_stride + c] as i16;
            residual[r * bs + c] = s - p;
        }
    }

    // 3. Forward transform.
    let mut coeff = [0i16; 64];
    match ctx.tx_size {
        TxSize::Tx4X4 => {
            let mut c4 = [0i16; 16];
            fdct4x4(&residual[..n], bs, &mut c4);
            coeff[..16].copy_from_slice(&c4);
        }
        TxSize::Tx8X8 => {
            let mut c8 = [0i16; 64];
            fdct8x8(&residual[..n], bs, &mut c8);
            coeff.copy_from_slice(&c8);
        }
        _ => unreachable!("encode_intra_block only supports 4x4 and 8x8 transforms"),
    }

    // 4. Quantize / dequantize.
    let mut qcoeff = [0i16; 64];
    let mut dqcoeff = [0i16; 64];
    let eob = quantize_fp(
        &coeff[..n],
        ctx.tx_size,
        qp,
        &mut qcoeff[..n],
        &mut dqcoeff[..n],
    );
    let skip = eob == 0;

    // 5. Inverse transform back into the reconstruction (recon still holds the
    //    prediction, so this yields prediction + residual). Skipped blocks keep
    //    the prediction untouched.
    if !skip {
        match ctx.tx_size {
            TxSize::Tx4X4 => {
                let dq: [i16; 16] = dqcoeff[..16].try_into().unwrap();
                idct4x4_add(&dq, &mut recon[ctx.off..], recon_stride);
            }
            TxSize::Tx8X8 => {
                let dq: [i16; 64] = dqcoeff;
                idct8x8_add(&dq, &mut recon[ctx.off..], recon_stride);
            }
            _ => unreachable!(),
        }
    }

    BlockResult {
        qcoeff: qcoeff[..n].to_vec(),
        eob,
        skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::common::frame_buffer::FrameBuffer;

    /// Encode every 8x8 luma block of a `dim`x`dim` frame in raster order at
    /// `qindex`, returning the reconstruction as a fresh frame buffer.
    fn encode_luma_8x8(src_fb: &FrameBuffer, dim: usize, qindex: i32) -> FrameBuffer {
        let mut recon = FrameBuffer::new(dim as u32, dim as u32);
        let qp = QuantParams::new(qindex, 0, 0);

        let (src, src_off0, src_stride, fw, fh) = src_fb.y();
        let (rdata, roff0, rstride) = recon.y_mut();

        let cols = dim / 8;
        let rows = dim / 8;
        for by in 0..rows {
            for bx in 0..cols {
                let ctx = BlockCtx {
                    tx_size: TxSize::Tx8X8,
                    off: roff0 + by * 8 * rstride + bx * 8,
                    up_available: by > 0,
                    left_available: bx > 0,
                    x0: (bx * 8) as i32,
                    y0: (by * 8) as i32,
                    frame_w: fw as i32,
                    frame_h: fh as i32,
                };
                let src_off = src_off0 + by * 8 * src_stride + bx * 8;
                encode_intra_block(rdata, rstride, src, src_off, src_stride, &ctx, &qp);
            }
        }
        recon
    }

    fn psnr_y(a: &FrameBuffer, b: &FrameBuffer, dim: usize) -> f64 {
        let (da, oa, sa, _, _) = a.y();
        let (db, ob, sb, _, _) = b.y();
        let mut mse = 0f64;
        for r in 0..dim {
            for c in 0..dim {
                let d = da[oa + r * sa + c] as f64 - db[ob + r * sb + c] as f64;
                mse += d * d;
            }
        }
        mse /= (dim * dim) as f64;
        if mse == 0.0 {
            return 99.0;
        }
        10.0 * (255.0 * 255.0 / mse).log10()
    }

    fn gradient_frame(dim: usize) -> FrameBuffer {
        let mut fb = FrameBuffer::new(dim as u32, dim as u32);
        let (yw, yh) = (dim, dim);
        let (cw, ch) = (dim.div_ceil(2), dim.div_ceil(2));
        let mut src = Vec::new();
        for r in 0..yh {
            for c in 0..yw {
                // Smooth gradient plus a mild texture so blocks carry residual.
                let v = (r + c) / 2 + ((r ^ c) & 0x7);
                src.push((v & 0xff) as u8);
            }
        }
        src.extend(std::iter::repeat_n(128u8, 2 * cw * ch));
        fb.import_i420(&src, dim as u32, dim as u32).unwrap();
        fb
    }

    #[test]
    fn recon_psnr_above_bound_at_mid_q() {
        let dim = 64;
        let src = gradient_frame(dim);
        let recon = encode_luma_8x8(&src, dim, 100);
        let psnr = psnr_y(&src, &recon, dim);
        assert!(psnr > 30.0, "PSNR {psnr:.2} dB below 30 dB bound");
    }

    #[test]
    fn deterministic() {
        let dim = 64;
        let src = gradient_frame(dim);
        let a = encode_luma_8x8(&src, dim, 100);
        let b = encode_luma_8x8(&src, dim, 100);
        assert_eq!(a.export_i420(), b.export_i420());
    }

    #[test]
    fn flat_image_skips_every_block() {
        // A flat image at the DC-128 fill value: the first block predicts 128
        // from its (absent) neighbors with zero residual and reconstructs
        // exactly; every subsequent block then predicts 128 from perfectly
        // reconstructed neighbors, so all blocks quantize to eob 0 (skip).
        let dim: usize = 64;
        let mut fb = FrameBuffer::new(dim as u32, dim as u32);
        let (cw, ch) = (dim.div_ceil(2), dim.div_ceil(2));
        let mut src = vec![128u8; dim * dim];
        src.extend(std::iter::repeat_n(128u8, 2 * cw * ch));
        fb.import_i420(&src, dim as u32, dim as u32).unwrap();

        let qp = QuantParams::new(100, 0, 0);
        let mut recon = FrameBuffer::new(dim as u32, dim as u32);
        let (sdata, soff0, sstride, fw, fh) = fb.y();
        let (rdata, roff0, rstride) = recon.y_mut();

        for by in 0..dim / 8 {
            for bx in 0..dim / 8 {
                let ctx = BlockCtx {
                    tx_size: TxSize::Tx8X8,
                    off: roff0 + by * 8 * rstride + bx * 8,
                    up_available: by > 0,
                    left_available: bx > 0,
                    x0: (bx * 8) as i32,
                    y0: (by * 8) as i32,
                    frame_w: fw as i32,
                    frame_h: fh as i32,
                };
                let src_off = soff0 + by * 8 * sstride + bx * 8;
                let res = encode_intra_block(rdata, rstride, sdata, src_off, sstride, &ctx, &qp);
                assert!(res.skip, "flat block ({bx},{by}) should skip");
                assert_eq!(res.eob, 0);
            }
        }
        // The reconstruction is exactly the flat source.
        assert_eq!(recon.export_i420()[..dim * dim], src[..dim * dim]);
    }
}
