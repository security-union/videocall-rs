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

//! Keyframe encode orchestration.
//!
//! [`encode_keyframe`] runs the two-phase pipeline of `vp9/encoder`: a superblock
//! walk that predicts, transforms, quantizes and *tokenizes* every block (the
//! reconstruction it leaves behind is bit-identical to the decoder's), followed
//! by a re-walk that packs the frame header, partition tree, modes and tokens
//! into the bitstream. Both walks descend superblocks in the identical recursive
//! order, so the entropy and partition contexts they maintain agree with the
//! decoder's.
//!
//! M1 scope: Profile 0, error-resilient, single tile, fixed full split to 8x8,
//! `tx_mode = ALLOW_8X8` (luma 8x8 / chroma 4x4 transforms), DC_PRED everywhere.

use crate::vp9::common::block::{BlockSize, Partition, TxMode, TxSize, SUBSIZE_LOOKUP};
use crate::vp9::common::bool_coder::BoolWriter;
use crate::vp9::common::frame_buffer::FrameBuffer;
use crate::vp9::enc::bitstream::{pack_frame_header, UncompressedHeader};
use crate::vp9::enc::block_encode::{encode_intra_block, BlockCtx};
use crate::vp9::enc::pack::{
    b_width_log2, pack_mb_tokens, write_kf_dc_modes, write_partition, write_skip, PartitionContext,
};
use crate::vp9::enc::quantize::QuantParams;
use crate::vp9::enc::tokenize::{combine_entropy_contexts, tokenize_block, PlaneType, Token};

/// The coefficient tokens and skip flag of one coded 8x8 mode-info block.
#[derive(Clone, Default)]
struct CodedBlock {
    skip: bool,
    tokens: Vec<Token>,
}

/// Per-plane entropy contexts (`above_context`/`left_context`) maintained across
/// the tokenize walk. Luma is indexed in 4x4 units at full resolution; chroma at
/// half resolution.
struct EntropyContext {
    above_y: Vec<u8>,
    above_u: Vec<u8>,
    above_v: Vec<u8>,
    left_y: [u8; 16],
    left_u: [u8; 8],
    left_v: [u8; 8],
}

impl EntropyContext {
    fn new(mi_cols_aligned: usize) -> Self {
        Self {
            above_y: vec![0u8; 2 * mi_cols_aligned],
            above_u: vec![0u8; mi_cols_aligned],
            above_v: vec![0u8; mi_cols_aligned],
            left_y: [0u8; 16],
            left_u: [0u8; 8],
            left_v: [0u8; 8],
        }
    }

    fn reset_left(&mut self) {
        self.left_y = [0u8; 16];
        self.left_u = [0u8; 8];
        self.left_v = [0u8; 8];
    }
}

/// Which plane a block belongs to.
#[derive(Clone, Copy)]
enum Plane {
    Y,
    U,
    V,
}

/// The block size one partition level below `bsize` for the fixed full-split
/// structure (64→32→16→8).
fn split_child(bsize: BlockSize) -> BlockSize {
    match bsize {
        BlockSize::B64X64 => BlockSize::B32X32,
        BlockSize::B32X32 => BlockSize::B16X16,
        BlockSize::B16X16 => BlockSize::B8X8,
        _ => unreachable!("split_child only descends 64→32→16→8"),
    }
}

/// `get_subsize(bsize, partition)` for the reachable block sizes.
fn subsize_of(bsize: BlockSize, partition: Partition) -> BlockSize {
    const BLOCK_SIZE_FROM_U8: [BlockSize; 13] = [
        BlockSize::B4X4,
        BlockSize::B4X8,
        BlockSize::B8X4,
        BlockSize::B8X8,
        BlockSize::B8X16,
        BlockSize::B16X8,
        BlockSize::B16X16,
        BlockSize::B16X32,
        BlockSize::B32X16,
        BlockSize::B32X32,
        BlockSize::B32X64,
        BlockSize::B64X32,
        BlockSize::B64X64,
    ];
    let v = SUBSIZE_LOOKUP[partition as usize][bsize as usize];
    BLOCK_SIZE_FROM_U8[v as usize]
}

/// Frame geometry, the coded-block grid, and quantizer, shared across the two
/// walks. `src`/`recon` are passed separately so their borrows never alias.
struct Frame {
    mi_rows: u32,
    mi_cols: u32,
    qp: QuantParams,
    grid: Vec<CodedBlock>,
}

impl Frame {
    #[inline]
    fn block_mut(&mut self, mi_row: u32, mi_col: u32) -> &mut CodedBlock {
        &mut self.grid[(mi_row * self.mi_cols + mi_col) as usize]
    }

    #[inline]
    fn block(&self, mi_row: u32, mi_col: u32) -> &CodedBlock {
        &self.grid[(mi_row * self.mi_cols + mi_col) as usize]
    }

    /// Recursive tokenize walk (mirrors `write_modes_sb`'s descent order).
    fn encode_sb(
        &mut self,
        src: &FrameBuffer,
        recon: &mut FrameBuffer,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
        bsize: BlockSize,
    ) {
        if mi_row >= self.mi_rows || mi_col >= self.mi_cols {
            return;
        }
        if bsize == BlockSize::B8X8 {
            self.encode_leaf(src, recon, ec, mi_row, mi_col);
            return;
        }
        let bs = (1u32 << b_width_log2(bsize)) / 4;
        let sub = split_child(bsize);
        self.encode_sb(src, recon, ec, mi_row, mi_col, sub);
        self.encode_sb(src, recon, ec, mi_row, mi_col + bs, sub);
        self.encode_sb(src, recon, ec, mi_row + bs, mi_col, sub);
        self.encode_sb(src, recon, ec, mi_row + bs, mi_col + bs, sub);
    }

    /// Encode + tokenize one 8x8 mode-info block (luma 8x8, chroma 4x4 U/V),
    /// updating entropy contexts and recording the coded block.
    fn encode_leaf(
        &mut self,
        src: &FrameBuffer,
        recon: &mut FrameBuffer,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
    ) {
        let up = mi_row > 0;
        let left = mi_col > 0;

        // --- Luma 8x8 ---
        let ay = (mi_col * 2) as usize;
        let ly = ((mi_row & 7) * 2) as usize;
        let pt_y = combine_entropy_contexts(
            ec.above_y[ay] != 0 || ec.above_y[ay + 1] != 0,
            ec.left_y[ly] != 0 || ec.left_y[ly + 1] != 0,
        );
        let (eob_y, qc_y) =
            encode_plane_block(src, recon, &self.qp, Plane::Y, mi_row, mi_col, up, left);
        let tokens_y = tokenize_block(&qc_y, eob_y, TxSize::Tx8X8, PlaneType::Y, pt_y);
        let he_y = (eob_y > 0) as u8;
        ec.above_y[ay] = he_y;
        ec.above_y[ay + 1] = he_y;
        ec.left_y[ly] = he_y;
        ec.left_y[ly + 1] = he_y;

        // --- Chroma U 4x4 ---
        let au = mi_col as usize;
        let lu = (mi_row & 7) as usize;
        let pt_u = combine_entropy_contexts(ec.above_u[au] != 0, ec.left_u[lu] != 0);
        let (eob_u, qc_u) =
            encode_plane_block(src, recon, &self.qp, Plane::U, mi_row, mi_col, up, left);
        let tokens_u = tokenize_block(&qc_u, eob_u, TxSize::Tx4X4, PlaneType::Uv, pt_u);
        let he_u = (eob_u > 0) as u8;
        ec.above_u[au] = he_u;
        ec.left_u[lu] = he_u;

        // --- Chroma V 4x4 ---
        let pt_v = combine_entropy_contexts(ec.above_v[au] != 0, ec.left_v[lu] != 0);
        let (eob_v, qc_v) =
            encode_plane_block(src, recon, &self.qp, Plane::V, mi_row, mi_col, up, left);
        let tokens_v = tokenize_block(&qc_v, eob_v, TxSize::Tx4X4, PlaneType::Uv, pt_v);
        let he_v = (eob_v > 0) as u8;
        ec.above_v[au] = he_v;
        ec.left_v[lu] = he_v;

        let skip = eob_y == 0 && eob_u == 0 && eob_v == 0;
        let tokens = if skip {
            Vec::new()
        } else {
            let mut t = tokens_y;
            t.extend(tokens_u);
            t.extend(tokens_v);
            t
        };
        *self.block_mut(mi_row, mi_col) = CodedBlock { skip, tokens };
    }

    /// Recursive pack walk: partition tree + modes + tokens (`write_modes_sb`).
    fn pack_sb(
        &self,
        w: &mut BoolWriter,
        pc: &mut PartitionContext,
        mi_row: u32,
        mi_col: u32,
        bsize: BlockSize,
    ) {
        if mi_row >= self.mi_rows || mi_col >= self.mi_cols {
            return;
        }
        let bs = (1u32 << b_width_log2(bsize)) / 4;
        let partition = if bsize == BlockSize::B8X8 {
            Partition::None
        } else {
            Partition::Split
        };
        write_partition(
            w,
            pc,
            bs,
            mi_row,
            mi_col,
            partition,
            bsize,
            self.mi_rows,
            self.mi_cols,
        );
        let subsize = subsize_of(bsize, partition);

        if bsize == BlockSize::B8X8 {
            self.pack_leaf(w, mi_row, mi_col);
        } else {
            let sub = split_child(bsize);
            self.pack_sb(w, pc, mi_row, mi_col, sub);
            self.pack_sb(w, pc, mi_row, mi_col + bs, sub);
            self.pack_sb(w, pc, mi_row + bs, mi_col, sub);
            self.pack_sb(w, pc, mi_row + bs, mi_col + bs, sub);
        }

        // update_partition_context (only at leaves for the full-split structure).
        if bsize == BlockSize::B8X8 || partition != Partition::Split {
            pc.update(mi_row, mi_col, subsize, bsize);
        }
    }

    /// Pack one leaf's skip flag, modes and coefficient tokens (`write_modes_b`).
    fn pack_leaf(&self, w: &mut BoolWriter, mi_row: u32, mi_col: u32) {
        let block = self.block(mi_row, mi_col);
        let above_skip = mi_row > 0 && self.block(mi_row - 1, mi_col).skip;
        let left_skip = mi_col > 0 && self.block(mi_row, mi_col - 1).skip;
        write_skip(w, above_skip, left_skip, block.skip);
        write_kf_dc_modes(w);
        pack_mb_tokens(w, &block.tokens);
    }
}

/// Run [`encode_intra_block`] for one plane's transform block, returning its
/// end-of-block count and quantized coefficients.
#[allow(clippy::too_many_arguments)]
fn encode_plane_block(
    src: &FrameBuffer,
    recon: &mut FrameBuffer,
    qp: &QuantParams,
    plane: Plane,
    mi_row: u32,
    mi_col: u32,
    up: bool,
    left: bool,
) -> (usize, Vec<i16>) {
    let (tx_size, px) = match plane {
        Plane::Y => (TxSize::Tx8X8, 8u32),
        Plane::U | Plane::V => (TxSize::Tx4X4, 4u32),
    };
    let (src_data, src_org, src_stride, fw, fh) = match plane {
        Plane::Y => src.y(),
        Plane::U => src.u(),
        Plane::V => src.v(),
    };
    let src_off = src_org + (mi_row * px) as usize * src_stride + (mi_col * px) as usize;
    let (rdata, rorg, rstride) = match plane {
        Plane::Y => recon.y_mut(),
        Plane::U => recon.u_mut(),
        Plane::V => recon.v_mut(),
    };
    let off = rorg + (mi_row * px) as usize * rstride + (mi_col * px) as usize;
    let ctx = BlockCtx {
        tx_size,
        off,
        up_available: up,
        left_available: left,
        x0: (mi_col * px) as i32,
        y0: (mi_row * px) as i32,
        frame_w: fw as i32,
        frame_h: fh as i32,
    };
    let res = encode_intra_block(rdata, rstride, src_data, src_off, src_stride, &ctx, qp);
    (res.eob as usize, res.qcoeff)
}

/// Encode a single keyframe of `src` at `base_qindex`.
///
/// Returns the complete VP9 frame bitstream and the reconstruction buffer (which
/// equals what a conformant decoder produces, and becomes the reference for
/// later inter frames). `src` must already hold the imported, mi-padded frame.
pub fn encode_keyframe(src: &FrameBuffer, base_qindex: u8) -> (Vec<u8>, FrameBuffer) {
    let width = src.crop_width;
    let height = src.crop_height;
    let mi_rows = src.mi_rows();
    let mi_cols = src.mi_cols();
    let mi_cols_aligned = ((mi_cols + 7) & !7) as usize;

    let mut recon = FrameBuffer::new(width, height);
    let qp = QuantParams::new(base_qindex as i32, 0, 0);
    let grid = vec![CodedBlock::default(); (mi_rows * mi_cols) as usize];

    let mut frame = Frame {
        mi_rows,
        mi_cols,
        qp,
        grid,
    };

    // --- Phase 1: tokenize walk (also builds the reconstruction). ---
    let mut ec = EntropyContext::new(mi_cols_aligned);
    let mut mi_row = 0;
    while mi_row < mi_rows {
        ec.reset_left();
        let mut mi_col = 0;
        while mi_col < mi_cols {
            frame.encode_sb(src, &mut recon, &mut ec, mi_row, mi_col, BlockSize::B64X64);
            mi_col += 8;
        }
        mi_row += 8;
    }

    // --- Phase 2: pack the frame. ---
    let header = UncompressedHeader::keyframe(width, height, base_qindex);
    let (mut out, _) = pack_frame_header(&header, TxMode::Allow8X8);

    let mut tile = BoolWriter::new();
    let mut pc = PartitionContext::new(mi_cols_aligned);
    let mut mi_row = 0;
    while mi_row < mi_rows {
        pc.reset_left();
        let mut mi_col = 0;
        while mi_col < mi_cols {
            frame.pack_sb(&mut tile, &mut pc, mi_row, mi_col, BlockSize::B64X64);
            mi_col += 8;
        }
        mi_row += 8;
    }
    out.extend_from_slice(&tile.finalize());

    (out, recon)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_i420(w: u32, h: u32) -> Vec<u8> {
        let (yw, yh) = (w as usize, h as usize);
        let (cw, ch) = ((w.div_ceil(2)) as usize, (h.div_ceil(2)) as usize);
        let mut v = Vec::new();
        for r in 0..yh {
            for c in 0..yw {
                v.push((((r + c) / 2 + ((r ^ c) & 0x7)) & 0xff) as u8);
            }
        }
        v.extend(std::iter::repeat_n(128u8, 2 * cw * ch));
        v
    }

    fn make_src(w: u32, h: u32) -> FrameBuffer {
        let mut fb = FrameBuffer::new(w, h);
        fb.import_i420(&gradient_i420(w, h), w, h).unwrap();
        fb
    }

    #[test]
    fn keyframe_is_deterministic() {
        let a = encode_keyframe(&make_src(64, 64), 128).0;
        let b = encode_keyframe(&make_src(64, 64), 128).0;
        assert_eq!(a, b);
    }

    #[test]
    fn keyframe_has_plausible_structure() {
        // Frame marker (0b10) in the top two bits and a non-empty payload.
        let (bytes, _) = encode_keyframe(&make_src(128, 96), 96);
        assert!(bytes.len() > 16, "frame too small: {}", bytes.len());
        assert_eq!(bytes[0] >> 6, 0b10, "frame marker missing");
    }

    #[test]
    fn odd_size_encodes_without_panic() {
        // 636x476 exercises mi padding (→640x480) and the partial bottom SB row.
        let (bytes, recon) = encode_keyframe(&make_src(636, 476), 150);
        assert!(!bytes.is_empty());
        assert_eq!((recon.crop_width, recon.crop_height), (636, 476));
    }
}
