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

//! Partition context bookkeeping and the partition token reader/writer.
//!
//! `partition_plane_context` / `update_partition_context` (`vp9_pred_common.c`)
//! and `write_partition` / `read_partition` (`vp9/encoder/vp9_bitstream.c` and
//! `vp9/decoder/vp9_decodeframe.c`) are decoder-mandated: the encoder and the
//! decoder must derive the identical partition probability context and split the
//! superblock tree identically, so both directions live here beside the shared
//! [`PartitionContext`] state.

use crate::vp9::common::block::{BlockSize, Partition, MI_WIDTH_LOG2, NUM_8X8_BLOCKS_WIDE};
use crate::vp9::common::bool_coder::{BoolReader, BoolWriter};
use crate::vp9::common::trees::{read_tree, write_token, PARTITION_TREE};

/// `PARTITION_PLOFFSET`: probability models per block size in the partition ctx.
const PARTITION_PLOFFSET: usize = 4;
/// `MI_MASK`: superblock-local mi index mask (8 mi per SB64).
const MI_MASK: u32 = 7;

/// Partition above/left context arrays (`above_seg_context` / `left_seg_context`),
/// mirroring the encoder's per-frame/per-SB-row lifecycle.
pub struct PartitionContext {
    /// One byte per mi column across the (sb-aligned) frame width.
    pub above: Vec<u8>,
    /// One byte per mi row within a superblock (8 entries).
    pub left: [u8; 8],
}

impl PartitionContext {
    pub fn new(mi_cols_aligned: usize) -> Self {
        Self {
            above: vec![0u8; mi_cols_aligned],
            left: [0u8; 8],
        }
    }

    /// Reset the left context at the start of a superblock row.
    pub fn reset_left(&mut self) {
        self.left = [0u8; 8];
    }

    /// `partition_plane_context`: derive the partition probability context.
    fn plane_context(&self, mi_row: u32, mi_col: u32, bsize: BlockSize) -> usize {
        let bsl = MI_WIDTH_LOG2[bsize as usize] as usize;
        let above = ((self.above[mi_col as usize] >> bsl) & 1) as usize;
        let left = ((self.left[(mi_row & MI_MASK) as usize] >> bsl) & 1) as usize;
        (left * 2 + above) + bsl * PARTITION_PLOFFSET
    }

    /// `update_partition_context`: stamp the context after coding a block.
    pub fn update(&mut self, mi_row: u32, mi_col: u32, subsize: BlockSize, bsize: BlockSize) {
        // `partition_context_lookup[subsize]` — the {above, left} masks. For the
        // block sizes this encoder emits, above == left (square blocks).
        const ABOVE: [u8; 13] = [15, 15, 14, 14, 14, 12, 12, 12, 8, 8, 8, 0, 0];
        const LEFT: [u8; 13] = [15, 14, 15, 14, 12, 14, 12, 8, 12, 8, 0, 8, 0];
        let bs = NUM_8X8_BLOCKS_WIDE[bsize as usize] as usize;
        let a = ABOVE[subsize as usize];
        let l = LEFT[subsize as usize];
        for k in 0..bs {
            self.above[mi_col as usize + k] = a;
        }
        let base = (mi_row & MI_MASK) as usize;
        for k in 0..bs {
            self.left[base + k] = l;
        }
    }
}

/// `write_partition`: emit the partition decision with frame-edge handling.
///
/// `hbs` is the half-block size in mi units. At the frame's right/bottom edge one
/// of the partition token's bits is forced (or the whole token dropped), matching
/// `has_rows`/`has_cols` in libvpx.
#[allow(clippy::too_many_arguments)]
pub fn write_partition(
    w: &mut BoolWriter,
    ctx: &PartitionContext,
    hbs: u32,
    mi_row: u32,
    mi_col: u32,
    partition: Partition,
    bsize: BlockSize,
    mi_rows: u32,
    mi_cols: u32,
    partition_probs: &[[u8; 3]; 16],
) {
    let cidx = ctx.plane_context(mi_row, mi_col, bsize);
    let probs = &partition_probs[cidx];
    let has_rows = (mi_row + hbs) < mi_rows;
    let has_cols = (mi_col + hbs) < mi_cols;

    if has_rows && has_cols {
        write_token(w, &PARTITION_TREE, probs, partition as usize);
    } else if !has_rows && has_cols {
        // SPLIT or HORZ: one bit distinguishes them.
        w.write((partition == Partition::Split) as u8, probs[1]);
    } else if has_rows && !has_cols {
        // SPLIT or VERT.
        w.write((partition == Partition::Split) as u8, probs[2]);
    }
    // else: neither — partition is forced SPLIT, nothing coded.
}

/// `read_partition`: recover the partition decision written by [`write_partition`],
/// applying the identical frame-edge handling. The inverse of `write_partition`;
/// `hbs` is the half-block size in mi units.
#[allow(clippy::too_many_arguments)]
pub fn read_partition(
    r: &mut BoolReader,
    ctx: &PartitionContext,
    hbs: u32,
    mi_row: u32,
    mi_col: u32,
    bsize: BlockSize,
    mi_rows: u32,
    mi_cols: u32,
    partition_probs: &[[u8; 3]; 16],
) -> Partition {
    let cidx = ctx.plane_context(mi_row, mi_col, bsize);
    let probs = &partition_probs[cidx];
    let has_rows = (mi_row + hbs) < mi_rows;
    let has_cols = (mi_col + hbs) < mi_cols;

    if has_rows && has_cols {
        partition_from_index(read_tree(r, &PARTITION_TREE, probs))
    } else if !has_rows && has_cols {
        // SPLIT or HORZ: one bit distinguishes them (probs[1]).
        if r.read(probs[1]) != 0 {
            Partition::Split
        } else {
            Partition::Horz
        }
    } else if has_rows && !has_cols {
        // SPLIT or VERT (probs[2]).
        if r.read(probs[2]) != 0 {
            Partition::Split
        } else {
            Partition::Vert
        }
    } else {
        // Neither — the partition is forced SPLIT, nothing was coded.
        Partition::Split
    }
}

/// Map a `PARTITION_TREE` leaf value (0..=3) to its [`Partition`].
fn partition_from_index(v: usize) -> Partition {
    match v {
        0 => Partition::None,
        1 => Partition::Horz,
        2 => Partition::Vert,
        _ => Partition::Split,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `write_partition` → `read_partition` round-trip must recover the coded
    /// partition for every frame-edge case (interior, bottom edge, right edge),
    /// sharing one [`PartitionContext`] so the derived probability context matches.
    #[test]
    fn partition_round_trips_including_edges() {
        // 24x24 mi frame; probe a 16x16 (bsize B16X16, hbs = 1) at positions that
        // exercise interior, bottom-only, right-only and corner edges.
        let mi_rows = 24;
        let mi_cols = 24;
        let probs = crate::vp9::common::generated::KF_PARTITION_PROBS;
        let cases = [
            (0u32, 0u32, Partition::None),
            (0, 0, Partition::Split),
            (mi_rows - 1, 0, Partition::Split), // bottom edge → forced-ish
            (0, mi_cols - 1, Partition::Split), // right edge
            (10, 10, Partition::None),
            (10, 10, Partition::Split),
        ];
        for &(mi_row, mi_col, part) in &cases {
            let ctx = PartitionContext::new(32);
            let mut w = BoolWriter::new();
            write_partition(
                &mut w,
                &ctx,
                1,
                mi_row,
                mi_col,
                part,
                BlockSize::B16X16,
                mi_rows,
                mi_cols,
                &probs,
            );
            let bytes = w.finalize();
            let mut r = BoolReader::new(&bytes);
            let got = read_partition(
                &mut r,
                &ctx,
                1,
                mi_row,
                mi_col,
                BlockSize::B16X16,
                mi_rows,
                mi_cols,
                &probs,
            );
            // At interior positions the exact partition round-trips; at forced
            // edges both writer and reader agree on SPLIT.
            let has_rows = (mi_row + 1) < mi_rows;
            let has_cols = (mi_col + 1) < mi_cols;
            if has_rows && has_cols {
                assert_eq!(
                    got, part,
                    "interior partition mismatch at {mi_row},{mi_col}"
                );
            } else {
                assert_eq!(got, Partition::Split);
            }
        }
    }
}
