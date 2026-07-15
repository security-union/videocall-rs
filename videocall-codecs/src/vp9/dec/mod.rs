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

//! Pure-Rust VP9 decoder for our own encoder's subset (Milestones 0 and 1).
//!
//! Mirrors the pure-Rust encoder in [`crate::vp9::enc`]: it inverts the exact
//! bitstream subset the encoder emits (Profile 0, error-resilient, `ALLOW_8X8`
//! transforms, 16x16 max partition, tile columns) and reuses the bit-exact
//! [`crate::vp9::common`] machinery — the boolean reader, token trees, dequant
//! tables, inverse transforms, DC intra predictor, motion-vector reference
//! derivation, integer-pel motion compensation, and the shared partition /
//! entropy contexts — so the reconstruction it produces is byte-identical to the
//! encoder's own reconstruction buffer.
//!
//! The decoder walks each tile column's superblock tree in the identical
//! recursive z-order as the encoder's pack walk, keeping the partition and
//! per-plane entropy contexts in lockstep. Being pure Rust (no C libvpx), it
//! compiles for `wasm32` and will back a future UniFFI/iOS wrapper.
//!
//! - **M0 (keyframe):** DC_PRED intra blocks, 8x8 luma / 4x4 chroma transforms.
//! - **M1 (inter):** single-reference (LAST) integer-pel motion compensation with
//!   ZEROMV / NEARESTMV / NEWMV modes, decoded via the stateful [`Vp9Decoder`],
//!   which maintains the reference-buffer slots across a keyframe-plus-inter
//!   sequence.
//!
//! Not yet covered: non-DC intra modes, sub-pel motion, compound prediction,
//! loop filtering, larger transforms, and arbitrary-profile / non-error-resilient
//! streams. Anything outside the subset is rejected with a [`DecodeError`] rather
//! than decoded — this parses untrusted network input, so every path either
//! reconstructs in-bounds pixels or returns an error; none panics.

mod detokenize;
mod header;

use std::rc::Rc;

use crate::vp9::common::block::{
    mi_cols as mi_cols_of, mi_rows as mi_rows_of, tile_offset, BlockSize, Partition,
    PredictionMode, B_WIDTH_LOG2,
};
use crate::vp9::common::block::{TxMode, TxSize};
use crate::vp9::common::bool_coder::BoolReader;
use crate::vp9::common::frame_buffer::FrameBuffer;
use crate::vp9::common::generated::{
    DEFAULT_INTER_MODE_PROBS, DEFAULT_INTRA_INTER_PROBS, DEFAULT_PARTITION_PROBS,
    DEFAULT_SINGLE_REF_PROBS, DEFAULT_SKIP_PROBS, KF_PARTITION_PROBS, KF_UV_MODE_PROBS,
    KF_Y_MODE_PROBS,
};
use crate::vp9::common::idct::{idct4x4_add, idct8x8_add};
use crate::vp9::common::inter_ctx::{intra_inter_context, single_ref_p1_context, InterNeighbor};
use crate::vp9::common::inter_pred::{predict_inter_block, Plane as McPlane};
use crate::vp9::common::intra_pred::build_intra_dc;
use crate::vp9::common::mvref::{
    find_mv_refs, Mv, MvRefGeom, MvRefInfo, INTRA_FRAME, LAST_FRAME, NONE_FRAME,
};
use crate::vp9::common::partition::{read_partition, PartitionContext};
use crate::vp9::common::quant::{ac_quant, dc_quant};
use crate::vp9::common::trees::{read_tree, INTER_MODE_TREE, INTRA_MODE_TREE};
use detokenize::{decode_coefs, PlaneType, REF_TYPE_INTER, REF_TYPE_INTRA};
use header::{parse_frame_header, FrameHeader, FrameType, REF_FRAMES};

mod readmv;
use readmv::read_mv;

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

/// DC_PRED discriminant (the only intra mode this decoder supports).
const DC_PRED: u8 = 0;

/// Which plane a transform block belongs to.
#[derive(Clone, Copy)]
enum Plane {
    Y,
    U,
    V,
}

/// Per-mi decoded mode info the neighbor-context and MV-reference derivations
/// read. Keyframe blocks are intra DC_PRED; inter blocks carry a real
/// mode/reference/motion. Replicated across all mi units of a 16x16 leaf, exactly
/// as the encoder stores its grid, so later neighbor lookups agree.
#[derive(Clone)]
struct DecMi {
    /// Block skip flag (no coded residual).
    skip: bool,
    /// True for an inter (motion-compensated) block.
    is_inter: bool,
    /// Prediction mode (intra `DC_PRED` for keyframe blocks; an inter mode
    /// otherwise). Feeds `find_mv_refs`'s neighbor mode counter.
    mode: PredictionMode,
    /// `ref_frame[0..2]` (`[INTRA_FRAME, NONE]` for intra, `[LAST_FRAME, NONE]`
    /// for this encoder's inter blocks).
    ref_frame: [i8; 2],
    /// Block motion vector in 1/8-pel units (`ZERO` for intra / ZEROMV).
    mv: Mv,
    /// Intra luma mode (for the keyframe neighbor-mode context); `DC_PRED` only.
    ymode: u8,
}

impl Default for DecMi {
    fn default() -> Self {
        DecMi {
            skip: false,
            is_inter: false,
            mode: PredictionMode::DcPred,
            ref_frame: [INTRA_FRAME, NONE_FRAME],
            mv: Mv::ZERO,
            ymode: DC_PRED,
        }
    }
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

/// A stateful VP9 decoder for the encoder's subset.
///
/// Maintains the eight reference-buffer slots across a frame sequence so inter
/// frames can motion-compensate against the previous reconstruction. Decode a
/// keyframe first (which populates every slot), then feed each subsequent inter
/// frame to [`Vp9Decoder::decode_frame`].
pub struct Vp9Decoder {
    /// `REF_FRAMES` reference-buffer slots. `Rc` so a keyframe can refresh all
    /// eight without cloning the pixels; each holds a border-extended
    /// reconstruction ready for motion compensation.
    refs: [Option<Rc<FrameBuffer>>; REF_FRAMES],
}

impl Default for Vp9Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Vp9Decoder {
    /// A decoder with no reference history. The first frame must be a keyframe.
    pub fn new() -> Self {
        Self {
            refs: std::array::from_fn(|_| None),
        }
    }

    /// Decode one frame (keyframe or inter) from `bytes`, update the reference
    /// slots per the frame's refresh mask, and return the reconstruction (borders
    /// already extended, so it can serve directly as a later reference).
    ///
    /// An inter frame decoded before any keyframe — or one referencing an empty
    /// slot — is rejected with [`DecodeError::Corrupt`] rather than reading
    /// uninitialized state.
    pub fn decode_frame(&mut self, bytes: &[u8]) -> Result<Rc<FrameBuffer>, DecodeError> {
        // Reference sizes for the header parser: an inter frame inherits its
        // dimensions from `ref_frame_idx[0]`'s buffer rather than coding them.
        let ref_sizes: [Option<(u32, u32)>; REF_FRAMES] =
            std::array::from_fn(|i| self.refs[i].as_ref().map(|f| (f.crop_width, f.crop_height)));
        let hdr = parse_frame_header(bytes, Some(&ref_sizes))?;

        let reference: Option<Rc<FrameBuffer>> = match hdr.frame_type {
            FrameType::Key => None,
            FrameType::Inter => Some(
                self.refs[hdr.ref_frame_idx[0] as usize]
                    .clone()
                    .ok_or(DecodeError::Corrupt("inter frame references an empty slot"))?,
            ),
        };

        let mut recon = decode_frame_inner(&hdr, bytes, reference.as_deref())?;
        // Extend borders so this reconstruction can be motion-compensated against
        // by the next inter frame, matching the encoder's `extend_borders` on the
        // reference before the following frame is coded.
        recon.extend_borders();
        let frame = Rc::new(recon);

        // Install the new frame into every slot its refresh mask selects.
        for (i, slot) in self.refs.iter_mut().enumerate() {
            if hdr.refresh_frame_flags & (1u8 << i) != 0 {
                *slot = Some(Rc::clone(&frame));
            }
        }
        Ok(frame)
    }
}

/// Decode a single VP9 keyframe of the encoder's subset into a reconstruction
/// buffer whose exported I420 equals the encoder's own reconstruction.
///
/// A convenience wrapper over [`Vp9Decoder`] for a standalone keyframe; inter
/// frames require the stateful decoder so it can reach the reference history.
pub fn decode_keyframe(bytes: &[u8]) -> Result<FrameBuffer, DecodeError> {
    let hdr = parse_frame_header(bytes, None)?;
    if hdr.frame_type != FrameType::Key {
        return Err(DecodeError::Unsupported("non-keyframe (use Vp9Decoder)"));
    }
    decode_frame_inner(&hdr, bytes, None)
}

/// Decode one frame's tile payloads into a fresh reconstruction buffer, given its
/// already-parsed header and (for inter frames) the LAST reference.
fn decode_frame_inner(
    hdr: &FrameHeader,
    bytes: &[u8],
    reference: Option<&FrameBuffer>,
) -> Result<FrameBuffer, DecodeError> {
    if hdr.tx_mode != TxMode::Allow8X8 {
        return Err(DecodeError::Unsupported("tx_mode != ALLOW_8X8"));
    }
    let is_keyframe = hdr.frame_type == FrameType::Key;
    if !is_keyframe && reference.is_none() {
        return Err(DecodeError::Corrupt("inter frame without a reference"));
    }

    let mi_rows = mi_rows_of(hdr.height);
    let mi_cols = mi_cols_of(hdr.width);
    let mi_cols_aligned = ((mi_cols + 7) & !7) as usize;
    let q = hdr.base_qindex as i32;
    let dequant = [dc_quant(q, 0), ac_quant(q, 0)];

    let mut dec = FrameDecoder {
        mi_rows,
        mi_cols,
        mi_cols_aligned,
        dequant,
        recon: FrameBuffer::new(hdr.width, hdr.height),
        grid: vec![DecMi::default(); (mi_rows * mi_cols) as usize],
        reference,
        is_keyframe,
        tile_col_start: 0,
        tile_col_end: mi_cols,
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

/// One frame's decode state: geometry, quantizer, the reconstruction buffer, the
/// per-mi decoded mode-info grid the neighbor-context derivations read, and (for
/// inter frames) the LAST reference the motion compensation reads.
struct FrameDecoder<'a> {
    mi_rows: u32,
    mi_cols: u32,
    mi_cols_aligned: usize,
    dequant: [i16; 2],
    recon: FrameBuffer,
    grid: Vec<DecMi>,
    /// The LAST reference (border-extended) for inter motion compensation, or
    /// `None` on a keyframe.
    reference: Option<&'a FrameBuffer>,
    is_keyframe: bool,
    /// Left mi-column bound of the tile being decoded (inclusive): its left edge
    /// is a frame edge for intra prediction, entropy/partition contexts, and the
    /// MV-reference column clamp.
    tile_col_start: u32,
    /// Right mi-column bound of the tile (exclusive): caps the MV-reference scan.
    tile_col_end: u32,
}

impl FrameDecoder<'_> {
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
        self.tile_col_start = col_start;
        self.tile_col_end = col_end;

        let mut r = BoolReader::new(bytes);
        let mut pc = PartitionContext::new(self.mi_cols_aligned);
        let mut ec = EntropyContext::new(self.mi_cols_aligned);

        let mut mi_row = 0;
        while mi_row < self.mi_rows {
            pc.reset_left();
            ec.reset_left();
            let mut mi_col = col_start;
            while mi_col < col_end {
                self.decode_sb(&mut r, &mut pc, &mut ec, mi_row, mi_col, BlockSize::B64X64)?;
                mi_col += 8;
            }
            mi_row += 8;
        }
        Ok(())
    }

    /// Recursive superblock decode (inverse of `pack_sb`): read the partition,
    /// then either decode a leaf or recurse into four children in z-order.
    fn decode_sb(
        &mut self,
        r: &mut BoolReader,
        pc: &mut PartitionContext,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
        bsize: BlockSize,
    ) -> Result<(), DecodeError> {
        if mi_row >= self.mi_rows || mi_col >= self.mi_cols {
            return Ok(());
        }
        let bs = (1u32 << B_WIDTH_LOG2[bsize as usize]) / 4;
        // Keyframes use the fixed KF partition probabilities; inter frames use the
        // (default, no-update) frame-context partition probabilities.
        let partition_probs = if self.is_keyframe {
            &KF_PARTITION_PROBS
        } else {
            &DEFAULT_PARTITION_PROBS
        };
        let partition = read_partition(
            r,
            pc,
            bs,
            mi_row,
            mi_col,
            bsize,
            self.mi_rows,
            self.mi_cols,
            partition_probs,
        );

        match partition {
            Partition::None => {
                match bsize {
                    BlockSize::B8X8 => {
                        if self.is_keyframe {
                            self.decode_leaf_8x8(r, ec, mi_row, mi_col)?
                        } else {
                            self.decode_inter_leaf_8x8(r, ec, mi_row, mi_col)?
                        }
                    }
                    BlockSize::B16X16 => {
                        if self.is_keyframe {
                            self.decode_leaf16(r, ec, mi_row, mi_col)?
                        } else {
                            self.decode_inter_leaf16(r, ec, mi_row, mi_col)?
                        }
                    }
                    // Our encoder never codes a 32x32/64x64 leaf.
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
                self.decode_sb(r, pc, ec, mi_row, mi_col, sub)?;
                self.decode_sb(r, pc, ec, mi_row, mi_col + bs, sub)?;
                self.decode_sb(r, pc, ec, mi_row + bs, mi_col, sub)?;
                self.decode_sb(r, pc, ec, mi_row + bs, mi_col + bs, sub)?;
            }
            // The encoder emits only NONE or SPLIT.
            Partition::Horz | Partition::Vert => {
                return Err(DecodeError::Unsupported("HORZ/VERT partition (M2)"))
            }
        }
        Ok(())
    }

    // --- Keyframe (intra) leaves -------------------------------------------

    /// Read a keyframe leaf's skip flag and DC intra modes, storing them into the
    /// given mi units. Returns the skip flag. Supports DC_PRED only.
    fn read_leaf_modes(
        &mut self,
        r: &mut BoolReader,
        mi_row: u32,
        mi_col: u32,
        units: &[(u32, u32)],
    ) -> Result<bool, DecodeError> {
        let above_skip = mi_row > 0 && self.grid[self.idx(mi_row - 1, mi_col)].skip;
        let left_skip =
            mi_col > self.tile_col_start && self.grid[self.idx(mi_row, mi_col - 1)].skip;
        let ctx = above_skip as usize + left_skip as usize;
        let skip = r.read(DEFAULT_SKIP_PROBS[ctx]) != 0;

        let a_mode = if mi_row > 0 {
            self.grid[self.idx(mi_row - 1, mi_col)].ymode
        } else {
            DC_PRED
        };
        let l_mode = if mi_col > self.tile_col_start {
            self.grid[self.idx(mi_row, mi_col - 1)].ymode
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
            return Err(DecodeError::Unsupported("non-DC intra mode"));
        }

        let mi = DecMi {
            skip,
            ymode: y_mode,
            ..DecMi::default()
        };
        for &(dr, dc) in units {
            let i = self.idx(mi_row + dr, mi_col + dc);
            self.grid[i] = mi.clone();
        }
        Ok(skip)
    }

    /// Decode one intra 8x8 mode-info block (luma 8x8, chroma 4x4).
    fn decode_leaf_8x8(
        &mut self,
        r: &mut BoolReader,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
    ) -> Result<(), DecodeError> {
        let skip = self.read_leaf_modes(r, mi_row, mi_col, &[(0, 0)])?;
        let up = mi_row > 0;
        let left = mi_col > self.tile_col_start;

        // Luma 8x8.
        let ay = (mi_col * 2) as usize;
        let ly = ((mi_row & 7) * 2) as usize;
        let pt_y = ctx_combine(
            ec.above_y[ay] != 0 || ec.above_y[ay + 1] != 0,
            ec.left_y[ly] != 0 || ec.left_y[ly + 1] != 0,
        );
        let he_y = self.intra_recon_tx(
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
        let he_u = self.intra_recon_tx(
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
        let he_v = self.intra_recon_tx(
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
        mi_row: u32,
        mi_col: u32,
    ) -> Result<(), DecodeError> {
        let units = [(0, 0), (0, 1), (1, 0), (1, 1)];
        let skip = self.read_leaf_modes(r, mi_row, mi_col, &units)?;
        let up_blk = mi_row > 0;
        let left_blk = mi_col > self.tile_col_start;

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
                let he = self.intra_recon_tx(
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

        self.decode_leaf16_chroma(
            r,
            ec,
            mi_row,
            mi_col,
            up_blk,
            left_blk,
            skip,
            REF_TYPE_INTRA,
        );
        Ok(())
    }

    /// DC-predict one transform block into `recon`, then (unless the block is
    /// skipped) decode its coefficients and add the inverse transform. Returns the
    /// entropy-context flag (`1` if the block has any non-zero coefficient).
    #[allow(clippy::too_many_arguments)]
    fn intra_recon_tx(
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
        add_residual(tx_size, eob, &dq, &mut rdata[off..], stride);
        (eob > 0) as u8
    }

    // --- Inter leaves ------------------------------------------------------

    /// Read one inter block's mode info (`pack_leaf_inter` inverse): skip,
    /// `is_inter`, single-ref, the inter mode, and the NEWMV difference. `bsize`
    /// selects the `mv_ref_blocks` neighborhood. Returns `(skip, mode, mv)`.
    ///
    /// Only this encoder's subset is accepted: an intra block, a non-LAST
    /// reference, or a non-integer (non-multiple-of-16) motion vector is rejected.
    /// Rejecting non-integer MVs both matches the encoder (whose emitted MVs are
    /// always integer-pel) and keeps motion compensation a pure block copy whose
    /// reads the umv-border clamp proves in-bounds — so a hostile MV can never
    /// index out of the reference's 64-pixel border.
    fn read_inter_mode_info(
        &self,
        r: &mut BoolReader,
        mi_row: u32,
        mi_col: u32,
        bsize: BlockSize,
    ) -> Result<(bool, PredictionMode, Mv), DecodeError> {
        let above = self.inter_neighbor(mi_row.checked_sub(1), Some(mi_col));
        let left = if mi_col > self.tile_col_start {
            self.inter_neighbor(Some(mi_row), Some(mi_col - 1))
        } else {
            None
        };

        // skip.
        let above_skip = above.is_some() && self.grid[self.idx(mi_row - 1, mi_col)].skip;
        let left_skip = left.is_some() && self.grid[self.idx(mi_row, mi_col - 1)].skip;
        let skip = r.read(DEFAULT_SKIP_PROBS[above_skip as usize + left_skip as usize]) != 0;

        // is_inter — always 1 for this encoder's inter frames.
        let is_inter = r.read(DEFAULT_INTRA_INTER_PROBS[intra_inter_context(above, left)]) != 0;
        if !is_inter {
            return Err(DecodeError::Unsupported("intra block in inter frame"));
        }

        // tx_size: ALLOW_8X8 is not TX_MODE_SELECT → no bits.

        // reference frame: single_ref_p1 = 0 selects LAST; a 1 bit would select a
        // reference this encoder never emits.
        if r.read(DEFAULT_SINGLE_REF_PROBS[single_ref_p1_context(above, left)][0]) != 0 {
            return Err(DecodeError::Unsupported("non-LAST reference"));
        }

        // inter mode, using the MV-reference-derived mode context.
        let (mode_context, refs) = self.mv_refs_at(mi_row, mi_col, bsize);
        let inter_offset = read_tree(
            r,
            &INTER_MODE_TREE,
            &DEFAULT_INTER_MODE_PROBS[mode_context as usize],
        );
        // INTER_OFFSET: NEARESTMV→0, NEARMV→1, ZEROMV→2, NEWMV→3.
        let mode = match inter_offset {
            0 => PredictionMode::NearestMv,
            1 => PredictionMode::NearMv,
            2 => PredictionMode::ZeroMv,
            _ => PredictionMode::NewMv,
        };

        // interp filter: fixed EIGHTTAP (not SWITCHABLE) → no bits.

        let mv = match mode {
            PredictionMode::ZeroMv => Mv::ZERO,
            PredictionMode::NearestMv => refs[0],
            PredictionMode::NearMv => refs[1],
            // NEWMV codes the MV difference against the nearest reference MV.
            _ => read_mv(r, refs[0]),
        };

        // Integer-pel invariant: every MV this encoder emits (and every reference
        // candidate it selects for NEAREST/ZERO) is a multiple of 16 in 1/8-pel
        // units. Anything else is a malformed stream; reject it before motion
        // compensation so the block copy stays on integer sample positions.
        if mv.row % 16 != 0 || mv.col % 16 != 0 {
            return Err(DecodeError::Corrupt("non-integer motion vector"));
        }

        Ok((skip, mode, mv))
    }

    /// Neighbor descriptor for the inter prediction contexts, or `None` at a
    /// frame edge (mirrors `above_mi` / `left_mi == NULL`).
    fn inter_neighbor(&self, mi_row: Option<u32>, mi_col: Option<u32>) -> Option<InterNeighbor> {
        let (r, c) = (mi_row?, mi_col?);
        if r >= self.mi_rows || c >= self.mi_cols {
            return None;
        }
        let mi = &self.grid[self.idx(r, c)];
        Some(InterNeighbor {
            is_inter: mi.is_inter,
            ref0: mi.ref_frame[0],
            ref1: mi.ref_frame[1],
        })
    }

    /// `find_mv_refs` at `(mi_row, mi_col)` for block size `bsize`, reading the
    /// decoded neighbor grid exactly as the encoder does (rows span the frame,
    /// columns the current tile). Returns `(mode_context, [nearest, near])`.
    fn mv_refs_at(&self, mi_row: u32, mi_col: u32, bsize: BlockSize) -> (u8, [Mv; 2]) {
        let (mi_rows, mi_cols) = (self.mi_rows as i32, self.mi_cols as i32);
        let cols = self.mi_cols;
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

    /// Store one inter leaf's decoded mode info into its mi unit(s). A 16x16 leaf
    /// replicates the info across all four units (as the encoder does) so later
    /// neighbor lookups agree.
    fn store_inter_mi(
        &mut self,
        mi_row: u32,
        mi_col: u32,
        units: &[(u32, u32)],
        skip: bool,
        mode: PredictionMode,
        mv: Mv,
    ) {
        let mi = DecMi {
            skip,
            is_inter: true,
            mode,
            ref_frame: [LAST_FRAME, NONE_FRAME],
            mv,
            ymode: DC_PRED,
        };
        for &(dr, dc) in units {
            let i = self.idx(mi_row + dr, mi_col + dc);
            self.grid[i] = mi.clone();
        }
    }

    /// Motion-compensate every plane of a leaf (`bsize_mi` mi units square) from
    /// the reference into `recon`, a pure integer-pel block copy.
    fn motion_compensate(&mut self, mi_row: u32, mi_col: u32, mv: Mv, bsize_mi: i32) {
        // `reference` is a shared borrow independent of `self.recon`, so copy it
        // out before mutably borrowing the reconstruction buffer.
        let reference = self
            .reference
            .expect("inter leaf requires a reference (validated at frame entry)");
        let (mi_rows, mi_cols) = (self.mi_rows as i32, self.mi_cols as i32);
        for plane in [McPlane::Y, McPlane::U, McPlane::V] {
            predict_inter_block(
                reference,
                &mut self.recon,
                plane,
                mi_row as i32,
                mi_col as i32,
                mv,
                bsize_mi,
                mi_rows,
                mi_cols,
            );
        }
    }

    /// Decode one inter 8x8 leaf: single-MV block copy (8x8 luma / 4x4 chroma)
    /// then one 8x8 luma and one 4x4 residual per chroma plane.
    fn decode_inter_leaf_8x8(
        &mut self,
        r: &mut BoolReader,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
    ) -> Result<(), DecodeError> {
        let (skip, mode, mv) = self.read_inter_mode_info(r, mi_row, mi_col, BlockSize::B8X8)?;
        self.store_inter_mi(mi_row, mi_col, &[(0, 0)], skip, mode, mv);
        self.motion_compensate(mi_row, mi_col, mv, 1);

        // Luma 8x8.
        let ay = (mi_col * 2) as usize;
        let ly = ((mi_row & 7) * 2) as usize;
        let pt_y = ctx_combine(
            ec.above_y[ay] != 0 || ec.above_y[ay + 1] != 0,
            ec.left_y[ly] != 0 || ec.left_y[ly + 1] != 0,
        );
        let he_y = self.inter_recon_tx(
            r,
            Plane::Y,
            TxSize::Tx8X8,
            (mi_row * 8) as usize,
            (mi_col * 8) as usize,
            skip,
            pt_y,
            PlaneType::Y,
        );
        ec.above_y[ay] = he_y;
        ec.above_y[ay + 1] = he_y;
        ec.left_y[ly] = he_y;
        ec.left_y[ly + 1] = he_y;

        // Chroma U/V 4x4.
        let au = mi_col as usize;
        let lu = (mi_row & 7) as usize;
        let cy = (mi_row * 4) as usize;
        let cx = (mi_col * 4) as usize;

        let pt_u = ctx_combine(ec.above_u[au] != 0, ec.left_u[lu] != 0);
        let he_u = self.inter_recon_tx(
            r,
            Plane::U,
            TxSize::Tx4X4,
            cy,
            cx,
            skip,
            pt_u,
            PlaneType::Uv,
        );
        ec.above_u[au] = he_u;
        ec.left_u[lu] = he_u;

        let pt_v = ctx_combine(ec.above_v[au] != 0, ec.left_v[lu] != 0);
        let he_v = self.inter_recon_tx(
            r,
            Plane::V,
            TxSize::Tx4X4,
            cy,
            cx,
            skip,
            pt_v,
            PlaneType::Uv,
        );
        ec.above_v[au] = he_v;
        ec.left_v[lu] = he_v;

        Ok(())
    }

    /// Decode one inter 16x16 leaf: one 16x16-luma / 8x8-chroma block copy, then
    /// four 8x8 luma residual transforms and one 8x8 transform per chroma plane.
    fn decode_inter_leaf16(
        &mut self,
        r: &mut BoolReader,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
    ) -> Result<(), DecodeError> {
        let (skip, mode, mv) = self.read_inter_mode_info(r, mi_row, mi_col, BlockSize::B16X16)?;
        let units = [(0, 0), (0, 1), (1, 0), (1, 1)];
        self.store_inter_mi(mi_row, mi_col, &units, skip, mode, mv);
        self.motion_compensate(mi_row, mi_col, mv, 2);

        // Luma: four 8x8 residual transforms in raster order (TL, TR, BL, BR).
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
                let he = self.inter_recon_tx(
                    r,
                    Plane::Y,
                    TxSize::Tx8X8,
                    (mi_row * 8 + sr * 8) as usize,
                    (mi_col * 8 + sc * 8) as usize,
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

        self.decode_leaf16_chroma(r, ec, mi_row, mi_col, false, false, skip, REF_TYPE_INTER);
        Ok(())
    }

    /// Decode the two chroma planes of a 16x16 leaf: one 8x8 transform each,
    /// covering the two 4x4 entropy entries. Shared by the intra and inter 16x16
    /// paths, which differ only in the coefficient reference type and whether the
    /// prediction already sits in `recon` (inter, `up`/`left` unused) or is
    /// DC-predicted here (intra).
    #[allow(clippy::too_many_arguments)]
    fn decode_leaf16_chroma(
        &mut self,
        r: &mut BoolReader,
        ec: &mut EntropyContext,
        mi_row: u32,
        mi_col: u32,
        up_blk: bool,
        left_blk: bool,
        skip: bool,
        ref_type: usize,
    ) {
        let au = mi_col as usize;
        let lu = (mi_row & 7) as usize;
        let cy = (mi_row * 4) as usize;
        let cx = (mi_col * 4) as usize;

        for (plane, above, left) in [
            (Plane::U, &mut ec.above_u, &mut ec.left_u),
            (Plane::V, &mut ec.above_v, &mut ec.left_v),
        ] {
            let pt = ctx_combine(
                above[au] != 0 || above[au + 1] != 0,
                left[lu] != 0 || left[lu + 1] != 0,
            );
            let he = if ref_type == REF_TYPE_INTRA {
                self.intra_recon_tx(
                    r,
                    plane,
                    TxSize::Tx8X8,
                    cy,
                    cx,
                    up_blk,
                    left_blk,
                    skip,
                    pt,
                    PlaneType::Uv,
                )
            } else {
                self.inter_recon_tx(r, plane, TxSize::Tx8X8, cy, cx, skip, pt, PlaneType::Uv)
            };
            above[au] = he;
            above[au + 1] = he;
            left[lu] = he;
            left[lu + 1] = he;
        }
    }

    /// Add one inter transform block's residual onto its already-placed
    /// motion-compensated prediction. Returns the entropy-context flag.
    #[allow(clippy::too_many_arguments)]
    fn inter_recon_tx(
        &mut self,
        r: &mut BoolReader,
        plane: Plane,
        tx_size: TxSize,
        y_px: usize,
        x_px: usize,
        skip: bool,
        pt: usize,
        ptype: PlaneType,
    ) -> u8 {
        if skip {
            return 0;
        }
        let dequant = self.dequant;
        let (org, stride, _, _) = self.plane_ro(plane);
        let off = org + y_px * stride + x_px;
        let (eob, dq) = decode_coefs(r, tx_size, ptype, REF_TYPE_INTER, pt, dequant);
        let rdata = self.plane_mut(plane);
        add_residual(tx_size, eob, &dq, &mut rdata[off..], stride);
        (eob > 0) as u8
    }
}

/// Inverse-transform a decoded coefficient block onto `dest` (at the block's
/// top-left) when it has any non-zero coefficient. A no-op for an empty block.
fn add_residual(tx_size: TxSize, eob: usize, dq: &[i16; 64], dest: &mut [u8], stride: usize) {
    if eob == 0 {
        return;
    }
    match tx_size {
        TxSize::Tx4X4 => {
            let block: [i16; 16] = dq[..16].try_into().expect("4x4 block is 16 coeffs");
            idct4x4_add(&block, dest, stride);
        }
        TxSize::Tx8X8 => idct8x8_add(dq, dest, stride),
        _ => unreachable!("decoder only supports 4x4 and 8x8 transforms"),
    }
}

// The oracle round-trip tests build synthetic sources from `crate::testing`,
// which only exists under `test-utils`.
#[cfg(all(test, feature = "test-utils"))]
mod tests;
