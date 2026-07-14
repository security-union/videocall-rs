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

//! Frame encode orchestration (keyframe and inter).
//!
//! Both [`encode_keyframe`] and [`encode_inter_frame`] run the two-phase pipeline
//! of `vp9/encoder`: a superblock walk that predicts, transforms, quantizes and
//! *tokenizes* every block (the reconstruction it leaves behind is bit-identical
//! to the decoder's), followed by a re-walk that packs the frame header,
//! partition tree, modes and tokens into the bitstream. Both walks descend
//! superblocks in the identical recursive order, so the entropy, partition and
//! mode-info contexts they maintain agree with the decoder's.
//!
//! The frame is split into `1..=4` VP9 tile columns (a pure function of width);
//! each tile is encoded and packed independently — in parallel via rayon on
//! native targets, sequentially on wasm — then their payloads are concatenated
//! with per-tile size prefixes and their reconstructions stitched into one frame.
//!
//! Scope: Profile 0, error-resilient, fixed full split to 8x8,
//! `tx_mode = ALLOW_8X8` (luma 8x8 / chroma 4x4 transforms). Keyframes use
//! DC_PRED; inter frames use ZEROMV / NEARESTMV / NEWMV against a single LAST
//! reference with integer-pel motion compensation.

use crate::vp9::common::block::{
    sb64_cols_from_mi, tile_cols_log2_range, BlockSize, Partition, PredictionMode, TxMode, TxSize,
    SUBSIZE_LOOKUP,
};
use crate::vp9::common::bool_coder::BoolWriter;
use crate::vp9::common::frame_buffer::FrameBuffer;
use crate::vp9::common::generated::{DEFAULT_PARTITION_PROBS, KF_PARTITION_PROBS};
use crate::vp9::common::inter_pred::{predict_inter_block, Plane as McPlane};
use crate::vp9::common::mvref::{
    find_mv_refs, Mv, MvRefGeom, MvRefInfo, INTRA_FRAME, LAST_FRAME, NONE_FRAME,
};
use crate::vp9::enc::bitstream::{pack_frame_header, UncompressedHeader};
use crate::vp9::enc::block_encode::{encode_intra_block, residual_transform_reconstruct, BlockCtx};
use crate::vp9::enc::encodemv::encode_mv;
use crate::vp9::enc::mcomp::{search_block, RefPlane, SrcBlock};
use crate::vp9::enc::pack::{
    b_width_log2, pack_mb_tokens, write_inter_mode, write_intra_inter, write_kf_dc_modes,
    write_partition, write_single_ref_last, write_skip, InterNeighbor, PartitionContext,
};
use crate::vp9::enc::quantize::QuantParams;
use crate::vp9::enc::speed::SpeedFeatures;
use crate::vp9::enc::tokenize::{
    combine_entropy_contexts, tokenize_block, PlaneType, Token, REF_TYPE_INTER, REF_TYPE_INTRA,
};

/// `INTER_OFFSET(mode)` = `mode - NEARESTMV`.
#[inline]
fn inter_offset(mode: PredictionMode) -> usize {
    (mode as u8 - PredictionMode::NearestMv as u8) as usize
}

/// One coded 8x8 mode-info block: coefficient tokens, skip flag, and the
/// mode/reference/motion needed by neighbor context derivations and the pack
/// walk. Keyframe blocks are intra DC_PRED; inter blocks carry a real mode/MV.
#[derive(Clone)]
struct Mi {
    skip: bool,
    tokens: Vec<Token>,
    is_inter: bool,
    mode: PredictionMode,
    ref_frame: [i8; 2],
    mv: Mv,
    /// `mode_context` derived at encode time and reused by the pack walk.
    mode_context: u8,
    /// True on all four mode-info units of a `BLOCK_16X16` leaf (a 16x16 block
    /// coded as one unit). The pack walk reads it on the top-left unit to decide
    /// `PARTITION_NONE` (16x16 leaf) vs `PARTITION_SPLIT` (four 8x8 leaves).
    is_leaf16: bool,
}

impl Default for Mi {
    fn default() -> Self {
        Mi {
            skip: false,
            tokens: Vec::new(),
            is_inter: false,
            mode: PredictionMode::DcPred,
            ref_frame: [INTRA_FRAME, NONE_FRAME],
            mv: Mv::ZERO,
            mode_context: 0,
            is_leaf16: false,
        }
    }
}

/// The result of a `BLOCK_16X16` MV-reference derivation + motion search, shared
/// between the partition decision and the leaf encode.
#[derive(Clone, Copy)]
struct Search16 {
    /// Inter-mode probability context from the two nearest coded neighbors.
    mode_context: u8,
    /// Nearest reference MV (`best_ref_mvs[0]`), the NEWMV coding center.
    nearest: Mv,
    /// The winning MV (a multiple of 16 in 1/8-pel).
    mv: Mv,
    /// The winning MV's luma SAD (prediction error), for the split decision.
    sad: u32,
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
/// walks. `src`/`recon`/`reference` are passed separately so their borrows never
/// alias.
struct Frame {
    mi_rows: u32,
    mi_cols: u32,
    qp: QuantParams,
    grid: Vec<Mi>,
    /// `cpu_used`-derived motion-search knobs (inter frames only).
    sf: SpeedFeatures,
    /// Left mi-column bound of the tile being coded (inclusive). Blocks at this
    /// column have no left neighbor: intra prediction, the entropy/partition
    /// contexts and the MV-reference scan all treat the tile's left edge as a
    /// frame edge (`set_mi_row_col`: `left_mi = mi_col > tile->mi_col_start`).
    /// `0` for a single-tile frame, restoring the original behavior exactly.
    tile_col_start: u32,
    /// Right mi-column bound of the tile (exclusive) — one past the last coded
    /// column. Caps the MV-reference scan; equals `mi_cols` for a single tile.
    tile_col_end: u32,
}

impl Frame {
    #[inline]
    fn block_mut(&mut self, mi_row: u32, mi_col: u32) -> &mut Mi {
        &mut self.grid[(mi_row * self.mi_cols + mi_col) as usize]
    }

    #[inline]
    fn block(&self, mi_row: u32, mi_col: u32) -> &Mi {
        &self.grid[(mi_row * self.mi_cols + mi_col) as usize]
    }

    /// Whether a `BLOCK_16X16` at `(mi_row, mi_col)` fits entirely within the mi
    /// grid — the condition for coding it as a `PARTITION_NONE` leaf. When it
    /// straddles the frame's right/bottom edge, VP9 forces a split, so the walk
    /// falls through to four 8x8 leaves instead (matching `write_partition`'s
    /// `has_rows`/`has_cols` handling and the decoder's `decode_partition`).
    #[inline]
    fn fits16(&self, mi_row: u32, mi_col: u32) -> bool {
        mi_row + 2 <= self.mi_rows && mi_col + 2 <= self.mi_cols
    }

    /// Store one 16x16 leaf's mode info into all four mode-info units of its 2x2
    /// region (neighbor context lookups read mi units, so the decoder's single
    /// coded block must appear replicated). Coefficient `tokens` live only on the
    /// top-left unit, which is the one the pack walk reads.
    fn store_leaf16(&mut self, mi_row: u32, mi_col: u32, tl: Mi) {
        // The other three units carry the same metadata but no tokens.
        let meta = Mi {
            tokens: Vec::new(),
            ..tl.clone()
        };
        *self.block_mut(mi_row, mi_col + 1) = meta.clone();
        *self.block_mut(mi_row + 1, mi_col) = meta.clone();
        *self.block_mut(mi_row + 1, mi_col + 1) = meta;
        *self.block_mut(mi_row, mi_col) = tl;
    }

    // --- Keyframe (intra) walks --------------------------------------------

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
        // A keyframe 16x16 leaf's luma reconstruction is bit-identical to the
        // 8x8-split path (same four 8x8 DC transforms in the same raster order),
        // so splitting an intra 16x16 only spends more bits: always keep it when
        // it fits the frame.
        if bsize == BlockSize::B16X16 && self.sf.max_partition_16x16 && self.fits16(mi_row, mi_col)
        {
            self.encode_leaf16_kf(src, recon, ec, mi_row, mi_col);
            return;
        }
        let bs = (1u32 << b_width_log2(bsize)) / 4;
        let sub = split_child(bsize);
        self.encode_sb(src, recon, ec, mi_row, mi_col, sub);
        self.encode_sb(src, recon, ec, mi_row, mi_col + bs, sub);
        self.encode_sb(src, recon, ec, mi_row + bs, mi_col, sub);
        self.encode_sb(src, recon, ec, mi_row + bs, mi_col + bs, sub);
    }

    /// Encode + tokenize one intra `BLOCK_16X16` leaf: four 8x8 luma transforms
    /// (each DC-predicted from its own reconstructed neighbors, in raster order)
    /// and one 8x8 transform per chroma plane. Updates the per-plane entropy
    /// contexts exactly as `foreach_transformed_block` + `vp9_set_contexts` do,
    /// and records the replicated mode info.
    fn encode_leaf16_kf(
        &mut self,
        src: &FrameBuffer,
        recon: &mut FrameBuffer,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
    ) {
        let up_blk = mi_row > 0;
        let left_blk = mi_col > self.tile_col_start;

        // --- Luma: four 8x8 transforms in raster order (TL, TR, BL, BR). ---
        let base_ay = (mi_col * 2) as usize;
        let base_ly = ((mi_row & 7) * 2) as usize;
        let mut tokens_y = Vec::new();
        let mut any_eob = false;
        for sr in 0..2u32 {
            for sc in 0..2u32 {
                let ay = base_ay + (sc * 2) as usize;
                let ly = base_ly + (sr * 2) as usize;
                let pt = combine_entropy_contexts(
                    ec.above_y[ay] != 0 || ec.above_y[ay + 1] != 0,
                    ec.left_y[ly] != 0 || ec.left_y[ly + 1] != 0,
                );
                let (eob, qc) = intra_dc_tx(
                    src,
                    recon,
                    &self.qp,
                    TxSize::Tx8X8,
                    Plane::Y,
                    (mi_row * 8 + sr * 8) as usize,
                    (mi_col * 8 + sc * 8) as usize,
                    sr > 0 || up_blk,
                    sc > 0 || left_blk,
                );
                tokens_y.extend(tokenize_block(
                    &qc,
                    eob,
                    TxSize::Tx8X8,
                    PlaneType::Y,
                    REF_TYPE_INTRA,
                    pt,
                ));
                let he = (eob > 0) as u8;
                any_eob |= eob > 0;
                ec.above_y[ay] = he;
                ec.above_y[ay + 1] = he;
                ec.left_y[ly] = he;
                ec.left_y[ly + 1] = he;
            }
        }

        // --- Chroma: one 8x8 transform per plane (4:2:0 → 8x8 chroma block). ---
        let au = mi_col as usize;
        let lu = (mi_row & 7) as usize;
        let cy = (mi_row * 4) as usize;
        let cx = (mi_col * 4) as usize;
        let mut tokens_u = Vec::new();
        let mut tokens_v = Vec::new();
        for (plane, above, left, out) in [
            (Plane::U, &mut ec.above_u, &mut ec.left_u, &mut tokens_u),
            (Plane::V, &mut ec.above_v, &mut ec.left_v, &mut tokens_v),
        ] {
            let pt = combine_entropy_contexts(
                above[au] != 0 || above[au + 1] != 0,
                left[lu] != 0 || left[lu + 1] != 0,
            );
            let (eob, qc) = intra_dc_tx(
                src,
                recon,
                &self.qp,
                TxSize::Tx8X8,
                plane,
                cy,
                cx,
                up_blk,
                left_blk,
            );
            out.extend(tokenize_block(
                &qc,
                eob,
                TxSize::Tx8X8,
                PlaneType::Uv,
                REF_TYPE_INTRA,
                pt,
            ));
            let he = (eob > 0) as u8;
            any_eob |= eob > 0;
            above[au] = he;
            above[au + 1] = he;
            left[lu] = he;
            left[lu + 1] = he;
        }

        let skip = !any_eob;
        let tokens = concat_tokens(skip, tokens_y, tokens_u, tokens_v);
        self.store_leaf16(
            mi_row,
            mi_col,
            Mi {
                skip,
                tokens,
                is_leaf16: true,
                ..Mi::default()
            },
        );
    }

    /// Encode + tokenize one intra 8x8 mode-info block (luma 8x8, chroma 4x4),
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
        let left = mi_col > self.tile_col_start;

        // --- Luma 8x8 ---
        let ay = (mi_col * 2) as usize;
        let ly = ((mi_row & 7) * 2) as usize;
        let pt_y = combine_entropy_contexts(
            ec.above_y[ay] != 0 || ec.above_y[ay + 1] != 0,
            ec.left_y[ly] != 0 || ec.left_y[ly + 1] != 0,
        );
        let (eob_y, qc_y) =
            encode_intra_plane_block(src, recon, &self.qp, Plane::Y, mi_row, mi_col, up, left);
        let tokens_y = tokenize_block(
            &qc_y,
            eob_y,
            TxSize::Tx8X8,
            PlaneType::Y,
            REF_TYPE_INTRA,
            pt_y,
        );
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
            encode_intra_plane_block(src, recon, &self.qp, Plane::U, mi_row, mi_col, up, left);
        let tokens_u = tokenize_block(
            &qc_u,
            eob_u,
            TxSize::Tx4X4,
            PlaneType::Uv,
            REF_TYPE_INTRA,
            pt_u,
        );
        let he_u = (eob_u > 0) as u8;
        ec.above_u[au] = he_u;
        ec.left_u[lu] = he_u;

        // --- Chroma V 4x4 ---
        let pt_v = combine_entropy_contexts(ec.above_v[au] != 0, ec.left_v[lu] != 0);
        let (eob_v, qc_v) =
            encode_intra_plane_block(src, recon, &self.qp, Plane::V, mi_row, mi_col, up, left);
        let tokens_v = tokenize_block(
            &qc_v,
            eob_v,
            TxSize::Tx4X4,
            PlaneType::Uv,
            REF_TYPE_INTRA,
            pt_v,
        );
        let he_v = (eob_v > 0) as u8;
        ec.above_v[au] = he_v;
        ec.left_v[lu] = he_v;

        let skip = eob_y == 0 && eob_u == 0 && eob_v == 0;
        let tokens = concat_tokens(skip, tokens_y, tokens_u, tokens_v);
        *self.block_mut(mi_row, mi_col) = Mi {
            skip,
            tokens,
            ..Mi::default()
        };
    }

    // --- Inter walks --------------------------------------------------------

    /// Recursive inter tokenize walk (same descent order as the keyframe walk).
    #[allow(clippy::too_many_arguments)]
    fn encode_inter_sb(
        &mut self,
        src: &FrameBuffer,
        reference: &FrameBuffer,
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
            self.encode_inter_leaf(src, reference, recon, ec, mi_row, mi_col);
            return;
        }
        // A 16x16 that fits the frame is coded as one leaf unless its best
        // motion-compensated luma SAD is above the split valve, in which case it
        // falls through to four 8x8 leaves (the validated per-8x8 path) for the
        // extra prediction/quality that a finer motion field buys.
        if bsize == BlockSize::B16X16 && self.sf.max_partition_16x16 && self.fits16(mi_row, mi_col)
        {
            let d = self.search_16x16(reference, src, mi_row, mi_col);
            if d.sad <= self.sf.split_16x16_sad {
                self.encode_leaf16_inter(src, reference, recon, ec, mi_row, mi_col, d);
                return;
            }
        }
        let bs = (1u32 << b_width_log2(bsize)) / 4;
        let sub = split_child(bsize);
        self.encode_inter_sb(src, reference, recon, ec, mi_row, mi_col, sub);
        self.encode_inter_sb(src, reference, recon, ec, mi_row, mi_col + bs, sub);
        self.encode_inter_sb(src, reference, recon, ec, mi_row + bs, mi_col, sub);
        self.encode_inter_sb(src, reference, recon, ec, mi_row + bs, mi_col + bs, sub);
    }

    /// Run the spatial MV-reference derivation and integer-pel motion search for
    /// a `BLOCK_16X16` at `(mi_row, mi_col)`, returning the mode context, nearest
    /// reference MV and the winning MV/SAD. Shared by the partition decision and
    /// the leaf encode so the search runs once.
    fn search_16x16(
        &self,
        reference: &FrameBuffer,
        src: &FrameBuffer,
        mi_row: u32,
        mi_col: u32,
    ) -> Search16 {
        let (mi_rows, mi_cols) = (self.mi_rows as i32, self.mi_cols as i32);
        let (mode_context, refs) = {
            let grid = &self.grid;
            let cols = self.mi_cols;
            // MV-reference candidates are clamped to the tile's column span
            // (`is_inside`); rows span the whole frame (tile_rows == 1).
            let (tcs, tce) = (self.tile_col_start as i32, self.tile_col_end as i32);
            let geom = MvRefGeom {
                mi_rows,
                mi_cols,
                ref_frame: LAST_FRAME,
                ref_sign_bias: [0; 4],
                allow_hp: false,
            };
            find_mv_refs(
                |r, c| {
                    if r < 0 || c < tcs || r >= mi_rows || c >= tce {
                        None
                    } else {
                        let mi = &grid[(r as u32 * cols + c as u32) as usize];
                        Some(MvRefInfo {
                            mode: mi.mode as u8,
                            ref_frame: mi.ref_frame,
                            mv: [mi.mv, Mv::ZERO],
                        })
                    }
                },
                mi_row as i32,
                mi_col as i32,
                BlockSize::B16X16,
                &geom,
            )
        };
        let nearest = refs[0];
        let best = {
            let (rdata, rorg, rstride, _, _) = reference.y();
            let (sdata, sorg, sstride, _, _) = src.y();
            let refp = RefPlane {
                data: rdata,
                origin: rorg,
                stride: rstride,
            };
            let srcb = SrcBlock {
                data: sdata,
                off: sorg + (mi_row * 8) as usize * sstride + (mi_col * 8) as usize,
                stride: sstride,
            };
            search_block(
                &refp,
                &srcb,
                mi_row as i32,
                mi_col as i32,
                nearest,
                &self.sf,
                16,
                16,
            )
        };
        Search16 {
            mode_context,
            nearest,
            mv: best.mv,
            sad: best.sad,
        }
    }

    /// Motion-compensate, residual, transform and tokenize one inter
    /// `BLOCK_16X16` leaf using the precomputed [`Search16`]: one MV drives a
    /// 16x16 luma / 8x8 chroma block copy, then four 8x8 luma transforms and one
    /// 8x8 transform per chroma plane, with the same entropy-context bookkeeping
    /// as the intra leaf.
    #[allow(clippy::too_many_arguments)]
    fn encode_leaf16_inter(
        &mut self,
        src: &FrameBuffer,
        reference: &FrameBuffer,
        recon: &mut FrameBuffer,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
        d: Search16,
    ) {
        let (mi_rows, mi_cols) = (self.mi_rows as i32, self.mi_cols as i32);
        let mv = d.mv;
        let mode = if mv == Mv::ZERO {
            PredictionMode::ZeroMv
        } else if mv == d.nearest {
            PredictionMode::NearestMv
        } else {
            PredictionMode::NewMv
        };

        // Motion-compensate the whole 16x16 leaf (bsize_mi = 2) per plane.
        for plane in [McPlane::Y, McPlane::U, McPlane::V] {
            predict_inter_block(
                reference,
                recon,
                plane,
                mi_row as i32,
                mi_col as i32,
                mv,
                2,
                mi_rows,
                mi_cols,
            );
        }

        // --- Luma: four 8x8 residual transforms in raster order. ---
        let base_ay = (mi_col * 2) as usize;
        let base_ly = ((mi_row & 7) * 2) as usize;
        let mut tokens_y = Vec::new();
        let mut any_eob = false;
        for sr in 0..2u32 {
            for sc in 0..2u32 {
                let ay = base_ay + (sc * 2) as usize;
                let ly = base_ly + (sr * 2) as usize;
                let pt = combine_entropy_contexts(
                    ec.above_y[ay] != 0 || ec.above_y[ay + 1] != 0,
                    ec.left_y[ly] != 0 || ec.left_y[ly + 1] != 0,
                );
                let (eob, qc) = inter_residual_tx(
                    src,
                    recon,
                    &self.qp,
                    TxSize::Tx8X8,
                    Plane::Y,
                    (mi_row * 8 + sr * 8) as usize,
                    (mi_col * 8 + sc * 8) as usize,
                );
                tokens_y.extend(tokenize_block(
                    &qc,
                    eob,
                    TxSize::Tx8X8,
                    PlaneType::Y,
                    REF_TYPE_INTER,
                    pt,
                ));
                let he = (eob > 0) as u8;
                any_eob |= eob > 0;
                ec.above_y[ay] = he;
                ec.above_y[ay + 1] = he;
                ec.left_y[ly] = he;
                ec.left_y[ly + 1] = he;
            }
        }

        // --- Chroma: one 8x8 residual transform per plane. ---
        let au = mi_col as usize;
        let lu = (mi_row & 7) as usize;
        let cy = (mi_row * 4) as usize;
        let cx = (mi_col * 4) as usize;
        let mut tokens_u = Vec::new();
        let mut tokens_v = Vec::new();
        for (plane, above, left, out) in [
            (Plane::U, &mut ec.above_u, &mut ec.left_u, &mut tokens_u),
            (Plane::V, &mut ec.above_v, &mut ec.left_v, &mut tokens_v),
        ] {
            let pt = combine_entropy_contexts(
                above[au] != 0 || above[au + 1] != 0,
                left[lu] != 0 || left[lu + 1] != 0,
            );
            let (eob, qc) = inter_residual_tx(src, recon, &self.qp, TxSize::Tx8X8, plane, cy, cx);
            out.extend(tokenize_block(
                &qc,
                eob,
                TxSize::Tx8X8,
                PlaneType::Uv,
                REF_TYPE_INTER,
                pt,
            ));
            let he = (eob > 0) as u8;
            any_eob |= eob > 0;
            above[au] = he;
            above[au + 1] = he;
            left[lu] = he;
            left[lu + 1] = he;
        }

        let skip = !any_eob;
        let tokens = concat_tokens(skip, tokens_y, tokens_u, tokens_v);
        self.store_leaf16(
            mi_row,
            mi_col,
            Mi {
                skip,
                tokens,
                is_inter: true,
                mode,
                ref_frame: [LAST_FRAME, NONE_FRAME],
                mv,
                mode_context: d.mode_context,
                is_leaf16: true,
            },
        );
    }

    /// Motion-estimate, predict, transform and tokenize one inter 8x8 block.
    fn encode_inter_leaf(
        &mut self,
        src: &FrameBuffer,
        reference: &FrameBuffer,
        recon: &mut FrameBuffer,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
    ) {
        let (mi_rows, mi_cols) = (self.mi_rows as i32, self.mi_cols as i32);

        // 1. Spatial MV reference + inter-mode context from coded neighbors.
        let (mode_context, refs) = {
            let grid = &self.grid;
            let cols = self.mi_cols;
            // MV-reference candidates are clamped to the tile's column span
            // (`is_inside`); rows span the whole frame (tile_rows == 1).
            let (tcs, tce) = (self.tile_col_start as i32, self.tile_col_end as i32);
            let geom = MvRefGeom {
                mi_rows,
                mi_cols,
                ref_frame: LAST_FRAME,
                ref_sign_bias: [0; 4],
                allow_hp: false,
            };
            find_mv_refs(
                |r, c| {
                    if r < 0 || c < tcs || r >= mi_rows || c >= tce {
                        None
                    } else {
                        let mi = &grid[(r as u32 * cols + c as u32) as usize];
                        Some(MvRefInfo {
                            mode: mi.mode as u8,
                            ref_frame: mi.ref_frame,
                            mv: [mi.mv, Mv::ZERO],
                        })
                    }
                },
                mi_row as i32,
                mi_col as i32,
                BlockSize::B8X8,
                &geom,
            )
        };
        let nearest = refs[0];

        // 2. Motion search on luma; classify the winner into a coded mode.
        let best = {
            let (rdata, rorg, rstride, _, _) = reference.y();
            let (sdata, sorg, sstride, _, _) = src.y();
            let refp = RefPlane {
                data: rdata,
                origin: rorg,
                stride: rstride,
            };
            let srcb = SrcBlock {
                data: sdata,
                off: sorg + (mi_row * 8) as usize * sstride + (mi_col * 8) as usize,
                stride: sstride,
            };
            search_block(
                &refp,
                &srcb,
                mi_row as i32,
                mi_col as i32,
                nearest,
                &self.sf,
                8,
                8,
            )
        };
        let mv = best.mv;
        let mode = if mv == Mv::ZERO {
            PredictionMode::ZeroMv
        } else if mv == nearest {
            PredictionMode::NearestMv
        } else {
            PredictionMode::NewMv
        };

        // 3. Motion-compensate the prediction into `recon` for all planes.
        for plane in [McPlane::Y, McPlane::U, McPlane::V] {
            predict_inter_block(
                reference,
                recon,
                plane,
                mi_row as i32,
                mi_col as i32,
                mv,
                1,
                mi_rows,
                mi_cols,
            );
        }

        // 4. Residual, transform, tokenize per plane (prediction already placed).
        let ay = (mi_col * 2) as usize;
        let ly = ((mi_row & 7) * 2) as usize;
        let pt_y = combine_entropy_contexts(
            ec.above_y[ay] != 0 || ec.above_y[ay + 1] != 0,
            ec.left_y[ly] != 0 || ec.left_y[ly + 1] != 0,
        );
        let (eob_y, qc_y) = inter_plane_residual(src, recon, &self.qp, Plane::Y, mi_row, mi_col);
        let tokens_y = tokenize_block(
            &qc_y,
            eob_y,
            TxSize::Tx8X8,
            PlaneType::Y,
            REF_TYPE_INTER,
            pt_y,
        );
        let he_y = (eob_y > 0) as u8;
        ec.above_y[ay] = he_y;
        ec.above_y[ay + 1] = he_y;
        ec.left_y[ly] = he_y;
        ec.left_y[ly + 1] = he_y;

        let au = mi_col as usize;
        let lu = (mi_row & 7) as usize;
        let pt_u = combine_entropy_contexts(ec.above_u[au] != 0, ec.left_u[lu] != 0);
        let (eob_u, qc_u) = inter_plane_residual(src, recon, &self.qp, Plane::U, mi_row, mi_col);
        let tokens_u = tokenize_block(
            &qc_u,
            eob_u,
            TxSize::Tx4X4,
            PlaneType::Uv,
            REF_TYPE_INTER,
            pt_u,
        );
        let he_u = (eob_u > 0) as u8;
        ec.above_u[au] = he_u;
        ec.left_u[lu] = he_u;

        let pt_v = combine_entropy_contexts(ec.above_v[au] != 0, ec.left_v[lu] != 0);
        let (eob_v, qc_v) = inter_plane_residual(src, recon, &self.qp, Plane::V, mi_row, mi_col);
        let tokens_v = tokenize_block(
            &qc_v,
            eob_v,
            TxSize::Tx4X4,
            PlaneType::Uv,
            REF_TYPE_INTER,
            pt_v,
        );
        let he_v = (eob_v > 0) as u8;
        ec.above_v[au] = he_v;
        ec.left_v[lu] = he_v;

        let skip = eob_y == 0 && eob_u == 0 && eob_v == 0;
        let tokens = concat_tokens(skip, tokens_y, tokens_u, tokens_v);
        *self.block_mut(mi_row, mi_col) = Mi {
            skip,
            tokens,
            is_inter: true,
            mode,
            ref_frame: [LAST_FRAME, NONE_FRAME],
            mv,
            mode_context,
            is_leaf16: false,
        };
    }

    // --- Pack walks ---------------------------------------------------------

    /// Recursive pack walk: partition tree + modes + tokens (`write_modes_sb`).
    fn pack_sb(
        &self,
        w: &mut BoolWriter,
        pc: &mut PartitionContext,
        mi_row: u32,
        mi_col: u32,
        bsize: BlockSize,
        is_keyframe: bool,
    ) {
        if mi_row >= self.mi_rows || mi_col >= self.mi_cols {
            return;
        }
        let bs = (1u32 << b_width_log2(bsize)) / 4;
        // A 16x16 that fits and was coded as a leaf writes PARTITION_NONE; an 8x8
        // block always does. Everything else (64/32, or a 16x16 that was split or
        // straddles the frame edge) writes PARTITION_SPLIT and recurses. The
        // decision is read back from the top-left mi's `is_leaf16`, set by the
        // encode walk, so both walks descend identically.
        let is_leaf16 = bsize == BlockSize::B16X16
            && self.fits16(mi_row, mi_col)
            && self.block(mi_row, mi_col).is_leaf16;
        let partition = if bsize == BlockSize::B8X8 || is_leaf16 {
            Partition::None
        } else {
            Partition::Split
        };
        // Keyframes use the fixed KF partition probabilities; inter frames use the
        // (default, no-update) frame-context partition probabilities.
        let partition_probs = if is_keyframe {
            &KF_PARTITION_PROBS
        } else {
            &DEFAULT_PARTITION_PROBS
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
            partition_probs,
        );
        let subsize = subsize_of(bsize, partition);

        if partition == Partition::None {
            // A leaf (8x8 or 16x16). Keyframe leaves share one pack path; inter
            // leaves differ only in the mv-ref block size.
            if is_keyframe {
                self.pack_leaf_kf(w, mi_row, mi_col);
            } else {
                self.pack_leaf_inter(w, mi_row, mi_col, bsize);
            }
        } else {
            let sub = split_child(bsize);
            self.pack_sb(w, pc, mi_row, mi_col, sub, is_keyframe);
            self.pack_sb(w, pc, mi_row, mi_col + bs, sub, is_keyframe);
            self.pack_sb(w, pc, mi_row + bs, mi_col, sub, is_keyframe);
            self.pack_sb(w, pc, mi_row + bs, mi_col + bs, sub, is_keyframe);
        }

        // update_partition_context runs at every non-split node (leaves and the
        // 16x16 leaf); split parents leave it to their recursed children.
        if partition != Partition::Split {
            pc.update(mi_row, mi_col, subsize, bsize);
        }
    }

    /// Pack one keyframe leaf: skip flag, DC modes, coefficient tokens.
    fn pack_leaf_kf(&self, w: &mut BoolWriter, mi_row: u32, mi_col: u32) {
        let block = self.block(mi_row, mi_col);
        let above_skip = mi_row > 0 && self.block(mi_row - 1, mi_col).skip;
        let left_skip = mi_col > self.tile_col_start && self.block(mi_row, mi_col - 1).skip;
        write_skip(w, above_skip, left_skip, block.skip);
        write_kf_dc_modes(w);
        pack_mb_tokens(w, &block.tokens);
    }

    /// Pack one inter leaf (`pack_inter_mode_mvs`): skip, is_inter, single-ref,
    /// inter mode, the MV for NEWMV, then coefficient tokens. `bsize` is the coded
    /// block size (`BLOCK_8X8` or `BLOCK_16X16`), which selects the
    /// `mv_ref_blocks` neighborhood for the NEWMV reference.
    fn pack_leaf_inter(&self, w: &mut BoolWriter, mi_row: u32, mi_col: u32, bsize: BlockSize) {
        let block = self.block(mi_row, mi_col);
        let above = self.inter_neighbor(mi_row.checked_sub(1), Some(mi_col));
        // The tile's left edge is a frame edge: no left neighbor there.
        let left = if mi_col > self.tile_col_start {
            self.inter_neighbor(Some(mi_row), Some(mi_col - 1))
        } else {
            None
        };

        // skip.
        let above_skip = above.is_some() && self.block(mi_row - 1, mi_col).skip;
        let left_skip = left.is_some() && self.block(mi_row, mi_col - 1).skip;
        write_skip(w, above_skip, left_skip, block.skip);

        // is_inter (always 1 for this encoder's inter frames).
        write_intra_inter(w, above, left, block.is_inter);

        // tx_size: ALLOW_8X8 is not TX_MODE_SELECT → no bits.

        // reference frame (single ref, LAST).
        write_single_ref_last(w, above, left);

        // inter mode.
        write_inter_mode(w, block.mode_context as usize, inter_offset(block.mode));

        // interp filter: fixed EIGHTTAP (not SWITCHABLE) → no bits.

        // NEWMV codes the MV difference against the nearest reference MV.
        if block.mode == PredictionMode::NewMv {
            let (_ctx, refs) = self.mv_refs_at(mi_row, mi_col, bsize);
            encode_mv(w, block.mv, refs[0], false);
        }

        pack_mb_tokens(w, &block.tokens);
    }

    /// Neighbor descriptor for the inter prediction contexts, or `None` at a
    /// frame edge (mirrors `above_mi` / `left_mi == NULL`).
    fn inter_neighbor(&self, mi_row: Option<u32>, mi_col: Option<u32>) -> Option<InterNeighbor> {
        let (r, c) = (mi_row?, mi_col?);
        if r >= self.mi_rows || c >= self.mi_cols {
            return None;
        }
        let mi = self.block(r, c);
        Some(InterNeighbor {
            is_inter: mi.is_inter,
            ref0: mi.ref_frame[0],
            ref1: mi.ref_frame[1],
        })
    }

    /// Recompute `find_mv_refs` at `(mi_row, mi_col)` for block size `bsize` from
    /// the finished grid (the pack walk needs the nearest MV to code a NEWMV
    /// difference). All `mv_ref_blocks` offsets are causal in z-order, so the
    /// finished grid holds the same neighbor values the encode walk saw.
    fn mv_refs_at(&self, mi_row: u32, mi_col: u32, bsize: BlockSize) -> (u8, [Mv; 2]) {
        let (mi_rows, mi_cols) = (self.mi_rows as i32, self.mi_cols as i32);
        let cols = self.mi_cols;
        // MV-reference candidates are clamped to the tile's column span.
        let (tcs, tce) = (self.tile_col_start as i32, self.tile_col_end as i32);
        let grid = &self.grid;
        let geom = MvRefGeom {
            mi_rows,
            mi_cols,
            ref_frame: LAST_FRAME,
            ref_sign_bias: [0; 4],
            allow_hp: false,
        };
        find_mv_refs(
            |r, c| {
                if r < 0 || c < tcs || r >= mi_rows || c >= tce {
                    None
                } else {
                    let mi = &grid[(r as u32 * cols + c as u32) as usize];
                    Some(MvRefInfo {
                        mode: mi.mode as u8,
                        ref_frame: mi.ref_frame,
                        mv: [mi.mv, Mv::ZERO],
                    })
                }
            },
            mi_row as i32,
            mi_col as i32,
            bsize,
            &geom,
        )
    }
}

/// Concatenate the per-plane token lists (Y, U, V) unless the block is skipped.
fn concat_tokens(skip: bool, y: Vec<Token>, u: Vec<Token>, v: Vec<Token>) -> Vec<Token> {
    if skip {
        Vec::new()
    } else {
        let mut t = y;
        t.extend(u);
        t.extend(v);
        t
    }
}

/// Run [`encode_intra_block`] for one plane's transform block, returning its
/// end-of-block count and quantized coefficients.
#[allow(clippy::too_many_arguments)]
fn encode_intra_plane_block(
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

/// Transform + reconstruct one plane's transform block whose motion-compensated
/// prediction already sits in `recon`.
fn inter_plane_residual(
    src: &FrameBuffer,
    recon: &mut FrameBuffer,
    qp: &QuantParams,
    plane: Plane,
    mi_row: u32,
    mi_col: u32,
) -> (usize, Vec<i16>) {
    let (tx_size, px) = match plane {
        Plane::Y => (TxSize::Tx8X8, 8u32),
        Plane::U | Plane::V => (TxSize::Tx4X4, 4u32),
    };
    let (src_data, src_org, src_stride, _, _) = match plane {
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
    let res = residual_transform_reconstruct(
        rdata, rstride, src_data, src_off, src_stride, tx_size, off, qp,
    );
    (res.eob as usize, res.qcoeff)
}

/// DC-predict, residual, transform and reconstruct one intra transform block of
/// `tx_size` in `plane`, whose top-left sits at plane pixel `(y_px, x_px)`.
/// `up`/`left` are the neighbor-availability flags for this transform block (true
/// when a reconstructed neighbor exists, i.e. a sub-block edge inside the coded
/// block or a frame-interior block edge). Returns the end-of-block count and the
/// quantized coefficients for tokenization.
#[allow(clippy::too_many_arguments)]
fn intra_dc_tx(
    src: &FrameBuffer,
    recon: &mut FrameBuffer,
    qp: &QuantParams,
    tx_size: TxSize,
    plane: Plane,
    y_px: usize,
    x_px: usize,
    up: bool,
    left: bool,
) -> (usize, Vec<i16>) {
    let (src_data, src_org, src_stride, fw, fh) = match plane {
        Plane::Y => src.y(),
        Plane::U => src.u(),
        Plane::V => src.v(),
    };
    let src_off = src_org + y_px * src_stride + x_px;
    let (rdata, rorg, rstride) = match plane {
        Plane::Y => recon.y_mut(),
        Plane::U => recon.u_mut(),
        Plane::V => recon.v_mut(),
    };
    let off = rorg + y_px * rstride + x_px;
    let ctx = BlockCtx {
        tx_size,
        off,
        up_available: up,
        left_available: left,
        x0: x_px as i32,
        y0: y_px as i32,
        frame_w: fw as i32,
        frame_h: fh as i32,
    };
    let res = encode_intra_block(rdata, rstride, src_data, src_off, src_stride, &ctx, qp);
    (res.eob as usize, res.qcoeff)
}

/// Residual, transform and reconstruct one inter transform block of `tx_size` in
/// `plane` at plane pixel `(y_px, x_px)`, whose motion-compensated prediction is
/// already in `recon`. Returns the end-of-block count and quantized coefficients.
#[allow(clippy::too_many_arguments)]
fn inter_residual_tx(
    src: &FrameBuffer,
    recon: &mut FrameBuffer,
    qp: &QuantParams,
    tx_size: TxSize,
    plane: Plane,
    y_px: usize,
    x_px: usize,
) -> (usize, Vec<i16>) {
    let (src_data, src_org, src_stride, _, _) = match plane {
        Plane::Y => src.y(),
        Plane::U => src.u(),
        Plane::V => src.v(),
    };
    let src_off = src_org + y_px * src_stride + x_px;
    let (rdata, rorg, rstride) = match plane {
        Plane::Y => recon.y_mut(),
        Plane::U => recon.u_mut(),
        Plane::V => recon.v_mut(),
    };
    let off = rorg + y_px * rstride + x_px;
    let res = residual_transform_reconstruct(
        rdata, rstride, src_data, src_off, src_stride, tx_size, off, qp,
    );
    (res.eob as usize, res.qcoeff)
}

/// Maximum tile-column split (`log2`) this encoder will request. Caps the
/// resolution-derived legal maximum so the tile count is a pure function of frame
/// width — independent of the machine's core count — which keeps the emitted
/// bitstream identical regardless of how many threads encode it. `2` permits up
/// to 4 tile columns (reached at 1280 px / 720p and wider); 640-wide frames top
/// out at the legal maximum of 1 (2 columns). Frames up to 448 px wide
/// (`mi_cols <= 56`, so `vp9_get_tile_n_bits` yields `max_log2 = 0`) resolve to a
/// single tile, byte-for-byte identical to the pre-tiling encoder; the first
/// 2-column width is 449 px (`mi_cols = 57`).
const MAX_TILE_COLS_LOG2: u32 = 2;

/// One tile column's mi-column span `[col_start, col_end)` (SB64-aligned).
#[derive(Clone, Copy)]
struct TileGeom {
    col_start: u32,
    col_end: u32,
}

/// `log2` of the tile-column count for a frame of `mi_cols`: the resolution's
/// legal maximum (`vp9_get_tile_n_bits`) capped by [`MAX_TILE_COLS_LOG2`].
fn tile_cols_log2_for(mi_cols: u32) -> u32 {
    let (_min, max) = tile_cols_log2_range(mi_cols);
    max.min(MAX_TILE_COLS_LOG2)
}

/// `get_tile_offset`: the SB64-aligned mi column at which tile `idx` starts.
fn tile_offset(idx: u32, mi_cols: u32, log2: u32) -> u32 {
    let sb_cols = sb64_cols_from_mi(mi_cols);
    let offset = ((idx * sb_cols) >> log2) << 3; // << MI_BLOCK_SIZE_LOG2
    offset.min(mi_cols)
}

/// The `1 << log2` tile columns spanning `mi_cols`.
fn tile_geometry(mi_cols: u32, log2: u32) -> Vec<TileGeom> {
    (0..(1u32 << log2))
        .map(|i| TileGeom {
            col_start: tile_offset(i, mi_cols, log2),
            col_end: tile_offset(i + 1, mi_cols, log2),
        })
        .collect()
}

/// Encode and pack one tile column, returning its bool-coded byte payload and a
/// reconstruction buffer whose `[col_start, col_end)` mi band is populated.
///
/// Fully self-contained: it reads only its own band of `src` and the shared,
/// read-only `reference`, and writes only its own private buffers, so tile
/// columns can run concurrently with no synchronization.
#[allow(clippy::too_many_arguments)]
fn encode_pack_tile(
    src: &FrameBuffer,
    reference: Option<&FrameBuffer>,
    base_qindex: u8,
    sf: &SpeedFeatures,
    mi_rows: u32,
    mi_cols: u32,
    mi_cols_aligned: usize,
    geom: TileGeom,
    is_keyframe: bool,
) -> (Vec<u8>, FrameBuffer) {
    let mut recon = FrameBuffer::new(src.crop_width, src.crop_height);
    let mut frame = Frame {
        mi_rows,
        mi_cols,
        qp: QuantParams::new(base_qindex as i32, 0, 0),
        grid: vec![Mi::default(); (mi_rows * mi_cols) as usize],
        sf: *sf,
        tile_col_start: geom.col_start,
        tile_col_end: geom.col_end,
    };

    // --- Phase 1: encode + tokenize this tile's SB columns, top to bottom. ---
    let mut ec = EntropyContext::new(mi_cols_aligned);
    let mut mi_row = 0;
    while mi_row < mi_rows {
        ec.reset_left();
        let mut mi_col = geom.col_start;
        while mi_col < geom.col_end {
            if is_keyframe {
                frame.encode_sb(src, &mut recon, &mut ec, mi_row, mi_col, BlockSize::B64X64);
            } else {
                frame.encode_inter_sb(
                    src,
                    reference.expect("inter frame requires a reference"),
                    &mut recon,
                    &mut ec,
                    mi_row,
                    mi_col,
                    BlockSize::B64X64,
                );
            }
            mi_col += 8;
        }
        mi_row += 8;
    }

    // --- Phase 2: pack this tile's mode/token stream. ---
    let mut w = BoolWriter::new();
    let mut pc = PartitionContext::new(mi_cols_aligned);
    let mut mi_row = 0;
    while mi_row < mi_rows {
        pc.reset_left();
        let mut mi_col = geom.col_start;
        while mi_col < geom.col_end {
            frame.pack_sb(
                &mut w,
                &mut pc,
                mi_row,
                mi_col,
                BlockSize::B64X64,
                is_keyframe,
            );
            mi_col += 8;
        }
        mi_row += 8;
    }

    (w.finalize(), recon)
}

/// Encode every tile column and assemble the frame: the uncompressed header
/// (carrying the tile-column count), then each tile's payload prefixed with a
/// 4-byte big-endian size (all but the last, per `vp9_bitstream.c:encode_tiles`),
/// with the per-tile reconstructions stitched back into one frame buffer.
///
/// Tile columns are independent (VP9 severs the left neighbor at a tile's left
/// edge), so on native they run in parallel via rayon; wasm runs the identical
/// sequential loop. The tile count is a pure function of frame width, so both
/// paths — and any thread count — emit byte-for-byte the same bitstream.
fn encode_frame_tiled(
    src: &FrameBuffer,
    reference: Option<&FrameBuffer>,
    base_qindex: u8,
    sf: &SpeedFeatures,
    is_keyframe: bool,
) -> (Vec<u8>, FrameBuffer) {
    let width = src.crop_width;
    let height = src.crop_height;
    let mi_rows = src.mi_rows();
    let mi_cols = src.mi_cols();
    let mi_cols_aligned = ((mi_cols + 7) & !7) as usize;

    let log2_tile_cols = tile_cols_log2_for(mi_cols);
    let tiles = tile_geometry(mi_cols, log2_tile_cols);

    let encode_one = |geom: TileGeom| {
        encode_pack_tile(
            src,
            reference,
            base_qindex,
            sf,
            mi_rows,
            mi_cols,
            mi_cols_aligned,
            geom,
            is_keyframe,
        )
    };

    #[cfg(not(target_arch = "wasm32"))]
    let mut results: Vec<(Vec<u8>, FrameBuffer)> = {
        use rayon::prelude::*;
        tiles.par_iter().copied().map(encode_one).collect()
    };
    #[cfg(target_arch = "wasm32")]
    let mut results: Vec<(Vec<u8>, FrameBuffer)> = tiles.iter().copied().map(encode_one).collect();

    // Header carrying the tile-column count, then the concatenated tiles.
    let mut header = if is_keyframe {
        UncompressedHeader::keyframe(width, height, base_qindex)
    } else {
        UncompressedHeader::inter(width, height, base_qindex)
    };
    header.log2_tile_cols = log2_tile_cols;
    let (mut out, _) = pack_frame_header(&header, TxMode::Allow8X8);

    // Single tile (frames ≤ ~504 wide, and the wasm/single-core common case): the
    // one tile band *is* the whole frame, so return its reconstruction directly —
    // no fresh full-frame allocation and no per-row stitch copy. No size prefix is
    // written for the last (here, only) tile.
    if results.len() == 1 {
        let (bytes, recon) = results.pop().expect("exactly one tile");
        out.extend_from_slice(&bytes);
        return (out, recon);
    }

    let mut recon = FrameBuffer::new(width, height);
    let n = results.len();
    for (i, (bytes, tile_recon)) in results.iter().enumerate() {
        // All tiles but the last carry a 4-byte big-endian size prefix.
        if i + 1 < n {
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        }
        out.extend_from_slice(bytes);
        recon.copy_tile_band(tile_recon, tiles[i].col_start, tiles[i].col_end);
    }
    (out, recon)
}

/// Encode a single keyframe of `src` at `base_qindex`.
///
/// Returns the complete VP9 frame bitstream and the reconstruction buffer (which
/// equals what a conformant decoder produces, and becomes the reference for later
/// inter frames). `src` must already hold the imported, mi-padded frame.
pub fn encode_keyframe(src: &FrameBuffer, base_qindex: u8) -> (Vec<u8>, FrameBuffer) {
    // Keyframes do no motion search; the speed knobs are unused.
    encode_frame_tiled(src, None, base_qindex, &SpeedFeatures::default(), true)
}

/// Encode a single inter frame of `src` against `reference` (the previous
/// reconstruction, borders already extended) at `base_qindex`.
///
/// Returns the VP9 frame bitstream and this frame's reconstruction (the next
/// reference). Uses a single LAST reference, integer-pel motion compensation, and
/// ZEROMV / NEARESTMV / NEWMV modes. `sf` supplies the `cpu_used`-derived motion
/// search range and ZEROMV early-exit threshold.
pub fn encode_inter_frame(
    src: &FrameBuffer,
    reference: &FrameBuffer,
    base_qindex: u8,
    sf: &SpeedFeatures,
) -> (Vec<u8>, FrameBuffer) {
    encode_frame_tiled(src, Some(reference), base_qindex, sf, false)
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

    #[test]
    fn inter_frame_is_deterministic_and_shows_inter_marker() {
        let key = encode_keyframe(&make_src(128, 96), 128);
        let mut reference = key.1;
        reference.extend_borders();
        let src = make_src(128, 96);
        let a = encode_inter_frame(&src, &reference, 128, &SpeedFeatures::default()).0;
        let b = encode_inter_frame(&src, &reference, 128, &SpeedFeatures::default()).0;
        assert_eq!(a, b, "inter encode must be deterministic");
        // frame_type bit (inter = 1) sits after marker(2)+profile(2)+show_existing(1).
        assert_eq!(a[0] >> 6, 0b10, "frame marker");
    }

    #[test]
    fn inter_of_identical_frame_is_small() {
        // Encoding a frame identical to its reference should predict everything
        // with ZEROMV + skip, yielding a tiny frame relative to the keyframe.
        let key = encode_keyframe(&make_src(128, 96), 128);
        let mut reference = key.1;
        reference.extend_borders();
        // The source for the inter frame equals the *reconstruction* so ZEROMV is
        // near-perfect (residual near zero).
        let src_i420 = reference.export_i420();
        let mut src = FrameBuffer::new(128, 96);
        src.import_i420(&src_i420, 128, 96).unwrap();
        let inter = encode_inter_frame(&src, &reference, 128, &SpeedFeatures::default());
        assert!(
            inter.0.len() < key.0.len(),
            "inter frame ({}) should be smaller than keyframe ({})",
            inter.0.len(),
            key.0.len()
        );
    }

    #[test]
    fn inter_odd_size_encodes_without_panic() {
        let key = encode_keyframe(&make_src(636, 476), 150);
        let mut reference = key.1;
        reference.extend_borders();
        let (bytes, recon) = encode_inter_frame(
            &make_src(636, 476),
            &reference,
            150,
            &SpeedFeatures::default(),
        );
        assert!(!bytes.is_empty());
        assert_eq!((recon.crop_width, recon.crop_height), (636, 476));
    }

    #[test]
    fn tile_count_is_a_pure_function_of_width() {
        use crate::vp9::common::block::mi_cols;
        // Tiny frames stay single-tile (byte-identical to the pre-tiling path).
        assert_eq!(tile_cols_log2_for(mi_cols(64)), 0);
        assert_eq!(tile_cols_log2_for(mi_cols(128)), 0);
        assert_eq!(tile_cols_log2_for(mi_cols(176)), 0);
        // 640-wide tops out at the legal maximum of 1 (2 tile columns).
        assert_eq!(tile_cols_log2_for(mi_cols(640)), 1);
        assert_eq!(tile_cols_log2_for(mi_cols(636)), 1);
        // 720p reaches 4 tile columns; the cap holds it there.
        assert_eq!(tile_cols_log2_for(mi_cols(1280)), 2);

        // The two 640-wide tiles are contiguous, SB64-aligned, and cover the frame.
        let tiles = tile_geometry(mi_cols(640), 1);
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].col_start, 0);
        assert_eq!(tiles[0].col_end, tiles[1].col_start);
        assert_eq!(tiles[1].col_end, mi_cols(640));
        assert_eq!(tiles[0].col_end % 8, 0, "tile edge must be SB64-aligned");
    }

    /// The emitted bitstream must not depend on how many threads encode it: the
    /// tile count is fixed by resolution and each tile is independent, so a
    /// 1-thread and a 4-thread rayon pool must produce identical bytes. Uses a
    /// 640×480 frame (2 tile columns → real parallelism).
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn tiled_output_is_independent_of_thread_count() {
        fn encode_kf_on(threads: usize, src: &FrameBuffer) -> Vec<u8> {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| encode_keyframe(src, 96).0)
        }
        fn encode_inter_on(threads: usize, src: &FrameBuffer, r: &FrameBuffer) -> Vec<u8> {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| encode_inter_frame(src, r, 96, &SpeedFeatures::default()).0)
        }

        let src = make_src(640, 480);
        assert_eq!(
            encode_kf_on(1, &src),
            encode_kf_on(4, &src),
            "keyframe bytes must be thread-count independent"
        );

        let key = encode_keyframe(&src, 96);
        let mut reference = key.1;
        reference.extend_borders();
        assert_eq!(
            encode_inter_on(1, &src, &reference),
            encode_inter_on(4, &src, &reference),
            "inter bytes must be thread-count independent"
        );
    }
}
