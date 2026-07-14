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

//! Pure-Rust VP9 decoder (Milestone 0: keyframe decode of our own encoder).
//!
//! Mirrors the pure-Rust encoder in [`crate::vp9::enc`]: it inverts the exact
//! bitstream subset the encoder emits (Profile 0, error-resilient, `ALLOW_8X8`
//! transforms, DC_PRED-only intra, 16x16 max partition, tile columns) and reuses
//! the bit-exact [`crate::vp9::common`] machinery — the boolean reader, token
//! trees, dequant tables, inverse transforms, DC intra predictor, and the shared
//! partition-context bookkeeping — so the reconstruction it produces is
//! byte-identical to the encoder's own reconstruction buffer.
//!
//! The decoder walks each tile column's superblock tree in the identical
//! recursive z-order as the encoder's pack walk, keeping the partition and
//! per-plane entropy contexts in lockstep. Being pure Rust (no C libvpx), it
//! compiles for `wasm32` and will back a future UniFFI/iOS wrapper.
//!
//! M0 scope: keyframes only. Inter frames, non-DC intra modes, loop filtering,
//! larger transforms and arbitrary-profile streams arrive with later milestones.

mod detokenize;
mod header;

use crate::vp9::common::block::{
    mi_cols as mi_cols_of, mi_rows as mi_rows_of, tile_offset, BlockSize, Partition, B_WIDTH_LOG2,
};
use crate::vp9::common::block::{TxMode, TxSize};
use crate::vp9::common::bool_coder::BoolReader;
use crate::vp9::common::frame_buffer::FrameBuffer;
use crate::vp9::common::generated::{
    DEFAULT_SKIP_PROBS, KF_PARTITION_PROBS, KF_UV_MODE_PROBS, KF_Y_MODE_PROBS,
};
use crate::vp9::common::idct::{idct4x4_add, idct8x8_add};
use crate::vp9::common::intra_pred::build_intra_dc;
use crate::vp9::common::partition::{read_partition, PartitionContext};
use crate::vp9::common::quant::{ac_quant, dc_quant};
use crate::vp9::common::trees::{read_tree, INTRA_MODE_TREE};
use detokenize::{decode_coefs, PlaneType, REF_TYPE_INTRA};
use header::{parse_frame_header, FrameType};

/// A VP9 decode failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The bitstream ended before a required field/payload.
    Truncated,
    /// A syntactically valid stream using a feature outside the supported subset.
    Unsupported(&'static str),
    /// A malformed field (bad marker, sync code, or partition/leaf structure).
    Corrupt(&'static str),
    /// Frame dimensions exceed [`MAX_DIM`]. Rejected at the header (before any
    /// allocation) so an untrusted ~20-byte header cannot request a multi-gigabyte
    /// buffer or overflow the 32-bit plane stride arithmetic on wasm32.
    TooLarge,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "truncated VP9 bitstream"),
            DecodeError::Unsupported(what) => write!(f, "unsupported VP9 feature: {what}"),
            DecodeError::Corrupt(what) => write!(f, "corrupt VP9 bitstream: {what}"),
            DecodeError::TooLarge => {
                write!(f, "frame dimensions exceed the maximum of {MAX_DIM}")
            }
        }
    }
}

/// Maximum decodable frame width/height in pixels. Comfortably covers 4K/UHD and
/// any realistic call resolution; bounds every dimension-derived allocation.
pub const MAX_DIM: u32 = 8192;

impl std::error::Error for DecodeError {}

/// DC_PRED discriminant (the only intra mode M0 supports).
const DC_PRED: u8 = 0;

/// Which plane a transform block belongs to.
#[derive(Clone, Copy)]
enum Plane {
    Y,
    U,
    V,
}

/// Per-plane entropy contexts (`above_context` / `left_context`) maintained
/// across the coefficient decode, mirroring the encoder's `EntropyContext`. Luma
/// is indexed in 4x4 units at full resolution; chroma at half resolution.
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

/// `combine_entropy_contexts(a, b)` = `(a != 0) + (b != 0)`.
#[inline]
fn ctx_combine(a: bool, b: bool) -> usize {
    a as usize + b as usize
}

/// Decode a single VP9 keyframe of the encoder's subset into a reconstruction
/// buffer whose exported I420 equals the encoder's own reconstruction.
pub fn decode_keyframe(bytes: &[u8]) -> Result<FrameBuffer, DecodeError> {
    let hdr = parse_frame_header(bytes)?;
    if hdr.frame_type != FrameType::Key {
        return Err(DecodeError::Unsupported("non-keyframe (M1)"));
    }
    if hdr.tx_mode != TxMode::Allow8X8 {
        return Err(DecodeError::Unsupported("tx_mode != ALLOW_8X8"));
    }

    let mi_rows = mi_rows_of(hdr.height);
    let mi_cols = mi_cols_of(hdr.width);
    let mi_cols_aligned = ((mi_cols + 7) & !7) as usize;
    let q = hdr.base_qindex as i32;
    let dequant = [dc_quant(q, 0), ac_quant(q, 0)];

    let mut dec = Decoder {
        mi_rows,
        mi_cols,
        mi_cols_aligned,
        dequant,
        recon: FrameBuffer::new(hdr.width, hdr.height),
        skip: vec![false; (mi_rows * mi_cols) as usize],
        ymode: vec![DC_PRED; (mi_rows * mi_cols) as usize],
    };

    // Tile columns: all but the last carry a 4-byte big-endian size prefix.
    let tiles = tile_geometry(mi_cols, hdr.log2_tile_cols);
    let tile_data = &bytes[hdr.tile_data_offset()..];
    let n = tiles.len();
    let mut pos = 0usize;
    for (i, &(col_start, col_end)) in tiles.iter().enumerate() {
        let size = if i + 1 < n {
            if pos + 4 > tile_data.len() {
                return Err(DecodeError::Truncated);
            }
            let s = u32::from_be_bytes([
                tile_data[pos],
                tile_data[pos + 1],
                tile_data[pos + 2],
                tile_data[pos + 3],
            ]) as usize;
            pos += 4;
            s
        } else {
            tile_data.len() - pos
        };
        if pos + size > tile_data.len() {
            return Err(DecodeError::Truncated);
        }
        let tile_bytes = &tile_data[pos..pos + size];
        pos += size;
        dec.decode_tile(tile_bytes, col_start, col_end)?;
    }

    Ok(dec.recon)
}

/// The `1 << log2` tile-column mi spans `[col_start, col_end)` for `mi_cols`.
fn tile_geometry(mi_cols: u32, log2: u32) -> Vec<(u32, u32)> {
    (0..(1u32 << log2))
        .map(|i| {
            (
                tile_offset(i, mi_cols, log2),
                tile_offset(i + 1, mi_cols, log2),
            )
        })
        .collect()
}

/// The block one partition level below `bsize` for the fixed full split.
fn split_child(bsize: BlockSize) -> BlockSize {
    match bsize {
        BlockSize::B64X64 => BlockSize::B32X32,
        BlockSize::B32X32 => BlockSize::B16X16,
        BlockSize::B16X16 => BlockSize::B8X8,
        _ => unreachable!("split_child only descends 64→32→16→8"),
    }
}

/// Keyframe decode state: geometry, quantizer, the reconstruction buffer, and the
/// per-mi skip / Y-mode grids the neighbor-context derivations read.
struct Decoder {
    mi_rows: u32,
    mi_cols: u32,
    mi_cols_aligned: usize,
    dequant: [i16; 2],
    recon: FrameBuffer,
    skip: Vec<bool>,
    ymode: Vec<u8>,
}

impl Decoder {
    #[inline]
    fn idx(&self, mi_row: u32, mi_col: u32) -> usize {
        (mi_row * self.mi_cols + mi_col) as usize
    }

    fn plane_ro(&self, p: Plane) -> (usize, usize, i32, i32) {
        let (_d, o, s, w, h) = match p {
            Plane::Y => self.recon.y(),
            Plane::U => self.recon.u(),
            Plane::V => self.recon.v(),
        };
        (o, s, w as i32, h as i32)
    }

    fn plane_mut(&mut self, p: Plane) -> &mut [u8] {
        match p {
            Plane::Y => self.recon.y_mut().0,
            Plane::U => self.recon.u_mut().0,
            Plane::V => self.recon.v_mut().0,
        }
    }

    /// Decode one tile column into the shared reconstruction buffer.
    fn decode_tile(
        &mut self,
        bytes: &[u8],
        col_start: u32,
        col_end: u32,
    ) -> Result<(), DecodeError> {
        let mut r = BoolReader::new(bytes);
        let mut pc = PartitionContext::new(self.mi_cols_aligned);
        let mut ec = EntropyContext::new(self.mi_cols_aligned);

        let mut mi_row = 0;
        while mi_row < self.mi_rows {
            pc.reset_left();
            ec.reset_left();
            let mut mi_col = col_start;
            while mi_col < col_end {
                self.decode_sb(
                    &mut r,
                    &mut pc,
                    &mut ec,
                    col_start,
                    mi_row,
                    mi_col,
                    BlockSize::B64X64,
                )?;
                mi_col += 8;
            }
            mi_row += 8;
        }
        Ok(())
    }

    /// Recursive superblock decode (inverse of `pack_sb`): read the partition,
    /// then either decode a leaf or recurse into four children in z-order.
    #[allow(clippy::too_many_arguments)]
    fn decode_sb(
        &mut self,
        r: &mut BoolReader,
        pc: &mut PartitionContext,
        ec: &mut EntropyContext,
        tile_col_start: u32,
        mi_row: u32,
        mi_col: u32,
        bsize: BlockSize,
    ) -> Result<(), DecodeError> {
        if mi_row >= self.mi_rows || mi_col >= self.mi_cols {
            return Ok(());
        }
        let bs = (1u32 << B_WIDTH_LOG2[bsize as usize]) / 4;
        let partition = read_partition(
            r,
            pc,
            bs,
            mi_row,
            mi_col,
            bsize,
            self.mi_rows,
            self.mi_cols,
            &KF_PARTITION_PROBS,
        );

        match partition {
            Partition::None => {
                match bsize {
                    BlockSize::B8X8 => {
                        self.decode_leaf_8x8(r, ec, tile_col_start, mi_row, mi_col)?
                    }
                    BlockSize::B16X16 => {
                        self.decode_leaf16(r, ec, tile_col_start, mi_row, mi_col)?
                    }
                    // Our encoder never codes a 32x32/64x64 leaf on a keyframe.
                    _ => return Err(DecodeError::Corrupt("unexpected leaf block size")),
                }
                // subsize == bsize for PARTITION_NONE.
                pc.update(mi_row, mi_col, bsize, bsize);
            }
            Partition::Split => {
                if bsize == BlockSize::B8X8 {
                    return Err(DecodeError::Corrupt("split of an 8x8 block"));
                }
                let sub = split_child(bsize);
                self.decode_sb(r, pc, ec, tile_col_start, mi_row, mi_col, sub)?;
                self.decode_sb(r, pc, ec, tile_col_start, mi_row, mi_col + bs, sub)?;
                self.decode_sb(r, pc, ec, tile_col_start, mi_row + bs, mi_col, sub)?;
                self.decode_sb(r, pc, ec, tile_col_start, mi_row + bs, mi_col + bs, sub)?;
            }
            // The keyframe encoder emits only NONE or SPLIT.
            Partition::Horz | Partition::Vert => {
                return Err(DecodeError::Unsupported("HORZ/VERT partition (M2)"))
            }
        }
        Ok(())
    }

    /// Read a leaf's skip flag and DC intra modes, storing them into the given mi
    /// units. Returns the skip flag. M0 supports DC_PRED only.
    #[allow(clippy::too_many_arguments)]
    fn read_leaf_modes(
        &mut self,
        r: &mut BoolReader,
        tile_col_start: u32,
        mi_row: u32,
        mi_col: u32,
        units: &[(u32, u32)],
    ) -> Result<bool, DecodeError> {
        let above_skip = mi_row > 0 && self.skip[self.idx(mi_row - 1, mi_col)];
        let left_skip = mi_col > tile_col_start && self.skip[self.idx(mi_row, mi_col - 1)];
        let ctx = above_skip as usize + left_skip as usize;
        let skip = r.read(DEFAULT_SKIP_PROBS[ctx]) != 0;

        let a_mode = if mi_row > 0 {
            self.ymode[self.idx(mi_row - 1, mi_col)]
        } else {
            DC_PRED
        };
        let l_mode = if mi_col > tile_col_start {
            self.ymode[self.idx(mi_row, mi_col - 1)]
        } else {
            DC_PRED
        };
        let y_mode = read_tree(
            r,
            &INTRA_MODE_TREE,
            &KF_Y_MODE_PROBS[a_mode as usize][l_mode as usize],
        ) as u8;
        let _uv_mode = read_tree(r, &INTRA_MODE_TREE, &KF_UV_MODE_PROBS[y_mode as usize]) as u8;
        if y_mode != DC_PRED {
            return Err(DecodeError::Unsupported("non-DC intra mode (M0)"));
        }

        for &(dr, dc) in units {
            let i = self.idx(mi_row + dr, mi_col + dc);
            self.skip[i] = skip;
            self.ymode[i] = y_mode;
        }
        Ok(skip)
    }

    /// Decode one intra 8x8 mode-info block (luma 8x8, chroma 4x4).
    fn decode_leaf_8x8(
        &mut self,
        r: &mut BoolReader,
        ec: &mut EntropyContext,
        tile_col_start: u32,
        mi_row: u32,
        mi_col: u32,
    ) -> Result<(), DecodeError> {
        let skip = self.read_leaf_modes(r, tile_col_start, mi_row, mi_col, &[(0, 0)])?;
        let up = mi_row > 0;
        let left = mi_col > tile_col_start;

        // Luma 8x8.
        let ay = (mi_col * 2) as usize;
        let ly = ((mi_row & 7) * 2) as usize;
        let pt_y = ctx_combine(
            ec.above_y[ay] != 0 || ec.above_y[ay + 1] != 0,
            ec.left_y[ly] != 0 || ec.left_y[ly + 1] != 0,
        );
        let he_y = self.recon_tx(
            r,
            Plane::Y,
            TxSize::Tx8X8,
            (mi_row * 8) as usize,
            (mi_col * 8) as usize,
            up,
            left,
            skip,
            pt_y,
            PlaneType::Y,
        );
        ec.above_y[ay] = he_y;
        ec.above_y[ay + 1] = he_y;
        ec.left_y[ly] = he_y;
        ec.left_y[ly + 1] = he_y;

        // Chroma U/V 4x4 (single 4x4 unit → single entropy entry).
        let au = mi_col as usize;
        let lu = (mi_row & 7) as usize;
        let cy = (mi_row * 4) as usize;
        let cx = (mi_col * 4) as usize;

        let pt_u = ctx_combine(ec.above_u[au] != 0, ec.left_u[lu] != 0);
        let he_u = self.recon_tx(
            r,
            Plane::U,
            TxSize::Tx4X4,
            cy,
            cx,
            up,
            left,
            skip,
            pt_u,
            PlaneType::Uv,
        );
        ec.above_u[au] = he_u;
        ec.left_u[lu] = he_u;

        let pt_v = ctx_combine(ec.above_v[au] != 0, ec.left_v[lu] != 0);
        let he_v = self.recon_tx(
            r,
            Plane::V,
            TxSize::Tx4X4,
            cy,
            cx,
            up,
            left,
            skip,
            pt_v,
            PlaneType::Uv,
        );
        ec.above_v[au] = he_v;
        ec.left_v[lu] = he_v;

        Ok(())
    }

    /// Decode one intra 16x16 leaf: four 8x8 luma transforms in raster order plus
    /// one 8x8 transform per chroma plane, replicated across its 2x2 mi units.
    fn decode_leaf16(
        &mut self,
        r: &mut BoolReader,
        ec: &mut EntropyContext,
        tile_col_start: u32,
        mi_row: u32,
        mi_col: u32,
    ) -> Result<(), DecodeError> {
        let units = [(0, 0), (0, 1), (1, 0), (1, 1)];
        let skip = self.read_leaf_modes(r, tile_col_start, mi_row, mi_col, &units)?;
        let up_blk = mi_row > 0;
        let left_blk = mi_col > tile_col_start;

        // Luma: four 8x8 transforms in raster order (TL, TR, BL, BR).
        let base_ay = (mi_col * 2) as usize;
        let base_ly = ((mi_row & 7) * 2) as usize;
        for sr in 0..2u32 {
            for sc in 0..2u32 {
                let ay = base_ay + (sc * 2) as usize;
                let ly = base_ly + (sr * 2) as usize;
                let pt = ctx_combine(
                    ec.above_y[ay] != 0 || ec.above_y[ay + 1] != 0,
                    ec.left_y[ly] != 0 || ec.left_y[ly + 1] != 0,
                );
                let he = self.recon_tx(
                    r,
                    Plane::Y,
                    TxSize::Tx8X8,
                    (mi_row * 8 + sr * 8) as usize,
                    (mi_col * 8 + sc * 8) as usize,
                    sr > 0 || up_blk,
                    sc > 0 || left_blk,
                    skip,
                    pt,
                    PlaneType::Y,
                );
                ec.above_y[ay] = he;
                ec.above_y[ay + 1] = he;
                ec.left_y[ly] = he;
                ec.left_y[ly + 1] = he;
            }
        }

        // Chroma: one 8x8 transform per plane (covers two 4x4 entropy entries).
        let au = mi_col as usize;
        let lu = (mi_row & 7) as usize;
        let cy = (mi_row * 4) as usize;
        let cx = (mi_col * 4) as usize;

        let pt_u = ctx_combine(
            ec.above_u[au] != 0 || ec.above_u[au + 1] != 0,
            ec.left_u[lu] != 0 || ec.left_u[lu + 1] != 0,
        );
        let he_u = self.recon_tx(
            r,
            Plane::U,
            TxSize::Tx8X8,
            cy,
            cx,
            up_blk,
            left_blk,
            skip,
            pt_u,
            PlaneType::Uv,
        );
        ec.above_u[au] = he_u;
        ec.above_u[au + 1] = he_u;
        ec.left_u[lu] = he_u;
        ec.left_u[lu + 1] = he_u;

        let pt_v = ctx_combine(
            ec.above_v[au] != 0 || ec.above_v[au + 1] != 0,
            ec.left_v[lu] != 0 || ec.left_v[lu + 1] != 0,
        );
        let he_v = self.recon_tx(
            r,
            Plane::V,
            TxSize::Tx8X8,
            cy,
            cx,
            up_blk,
            left_blk,
            skip,
            pt_v,
            PlaneType::Uv,
        );
        ec.above_v[au] = he_v;
        ec.above_v[au + 1] = he_v;
        ec.left_v[lu] = he_v;
        ec.left_v[lu + 1] = he_v;

        Ok(())
    }

    /// DC-predict one transform block into `recon`, then (unless the block is
    /// skipped) decode its coefficients and add the inverse transform. Returns the
    /// entropy-context flag (`1` if the block has any non-zero coefficient).
    #[allow(clippy::too_many_arguments)]
    fn recon_tx(
        &mut self,
        r: &mut BoolReader,
        plane: Plane,
        tx_size: TxSize,
        y_px: usize,
        x_px: usize,
        up: bool,
        left: bool,
        skip: bool,
        pt: usize,
        ptype: PlaneType,
    ) -> u8 {
        let dequant = self.dequant;
        let (org, stride, fw, fh) = self.plane_ro(plane);
        let off = org + y_px * stride + x_px;
        let rdata = self.plane_mut(plane);

        build_intra_dc(
            rdata,
            off,
            stride,
            tx_size,
            up,
            left,
            x_px as i32,
            y_px as i32,
            fw,
            fh,
        );

        if skip {
            return 0;
        }

        let (eob, dq) = decode_coefs(r, tx_size, ptype, REF_TYPE_INTRA, pt, dequant);
        if eob > 0 {
            match tx_size {
                TxSize::Tx4X4 => {
                    let block: [i16; 16] = dq[..16].try_into().expect("4x4 block is 16 coeffs");
                    idct4x4_add(&block, &mut rdata[off..], stride);
                }
                TxSize::Tx8X8 => idct8x8_add(&dq, &mut rdata[off..], stride),
                _ => unreachable!("recon_tx only supports 4x4 and 8x8 transforms"),
            }
        }
        (eob > 0) as u8
    }
}

// The oracle round-trip tests build synthetic sources from `crate::testing`,
// which only exists under `test-utils`.
#[cfg(all(test, feature = "test-utils"))]
mod tests;
