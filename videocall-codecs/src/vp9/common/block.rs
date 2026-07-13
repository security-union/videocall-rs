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

//! VP9 block-geometry enums and lookup tables.
//!
//! Enum discriminants and the lookup tables are transcribed from libvpx
//! `vp9/common/vp9_enums.h` and `vp9/common/vp9_common_data.c`. Sentinels use
//! [`INVALID`]. The mode-info (`mi`, 8×8 unit) and superblock (`sb64`, 64×64)
//! geometry helpers mirror `vp9/common/vp9_onyxc_int.h` / `vp9_tile_common.c`.

/// Sentinel for `BLOCK_INVALID` / `PARTITION_INVALID` table entries.
pub const INVALID: u8 = 255;

/// Number of block sizes (`BLOCK_SIZES`).
pub const BLOCK_SIZES: usize = 13;
/// Number of transform sizes (`TX_SIZES`).
pub const TX_SIZES: usize = 4;
/// Number of partition types (`PARTITION_TYPES`).
pub const PARTITION_TYPES: usize = 4;

/// `BLOCK_SIZE` (`vp9/common/vp9_enums.h`). Discriminant is the table index.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
#[repr(u8)]
pub enum BlockSize {
    B4X4 = 0,
    B4X8 = 1,
    B8X4 = 2,
    B8X8 = 3,
    B8X16 = 4,
    B16X8 = 5,
    B16X16 = 6,
    B16X32 = 7,
    B32X16 = 8,
    B32X32 = 9,
    B32X64 = 10,
    B64X32 = 11,
    B64X64 = 12,
}

/// `TX_SIZE` (`vp9/common/vp9_enums.h`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
#[repr(u8)]
pub enum TxSize {
    Tx4X4 = 0,
    Tx8X8 = 1,
    Tx16X16 = 2,
    Tx32X32 = 3,
}

/// `TX_MODE` (`vp9/common/vp9_enums.h`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
#[allow(clippy::enum_variant_names)] // names mirror the libvpx TX_MODE enum
pub enum TxMode {
    Only4X4 = 0,
    Allow8X8 = 1,
    Allow16X16 = 2,
    Allow32X32 = 3,
    TxModeSelect = 4,
}

/// `PARTITION_TYPE` (`vp9/common/vp9_enums.h`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Partition {
    None = 0,
    Horz = 1,
    Vert = 2,
    Split = 3,
}

/// `PREDICTION_MODE` (`vp9/common/vp9_enums.h`). Intra modes `0..=9`, inter
/// modes `10..=13`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PredictionMode {
    DcPred = 0,
    VPred = 1,
    HPred = 2,
    D45Pred = 3,
    D135Pred = 4,
    D117Pred = 5,
    D153Pred = 6,
    D207Pred = 7,
    D63Pred = 8,
    TmPred = 9,
    NearestMv = 10,
    NearMv = 11,
    ZeroMv = 12,
    NewMv = 13,
}

impl PredictionMode {
    /// True for inter prediction modes (`NEARESTMV..=NEWMV`).
    pub fn is_inter(self) -> bool {
        (self as u8) >= PredictionMode::NearestMv as u8
    }
}

// --- Lookup tables (vp9/common/vp9_common_data.c), indexed by BlockSize -----

/// `b_width_log2_lookup`.
pub const B_WIDTH_LOG2: [u8; BLOCK_SIZES] = [0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4];
/// `b_height_log2_lookup`.
pub const B_HEIGHT_LOG2: [u8; BLOCK_SIZES] = [0, 1, 0, 1, 2, 1, 2, 3, 2, 3, 4, 3, 4];
/// `num_4x4_blocks_wide_lookup`.
pub const NUM_4X4_BLOCKS_WIDE: [u8; BLOCK_SIZES] = [1, 1, 2, 2, 2, 4, 4, 4, 8, 8, 8, 16, 16];
/// `num_4x4_blocks_high_lookup`.
pub const NUM_4X4_BLOCKS_HIGH: [u8; BLOCK_SIZES] = [1, 2, 1, 2, 4, 2, 4, 8, 4, 8, 16, 8, 16];
/// `mi_width_log2_lookup`.
pub const MI_WIDTH_LOG2: [u8; BLOCK_SIZES] = [0, 0, 0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3];
/// `num_8x8_blocks_wide_lookup`.
pub const NUM_8X8_BLOCKS_WIDE: [u8; BLOCK_SIZES] = [1, 1, 1, 1, 1, 2, 2, 2, 4, 4, 4, 8, 8];
/// `num_8x8_blocks_high_lookup`.
pub const NUM_8X8_BLOCKS_HIGH: [u8; BLOCK_SIZES] = [1, 1, 1, 1, 2, 1, 2, 4, 2, 4, 8, 4, 8];
/// `size_group_lookup`.
pub const SIZE_GROUP: [u8; BLOCK_SIZES] = [0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 3];
/// `num_pels_log2_lookup`.
pub const NUM_PELS_LOG2: [u8; BLOCK_SIZES] = [4, 5, 5, 6, 7, 7, 8, 9, 9, 10, 11, 11, 12];

/// `max_txsize_lookup`: largest transform for each block size (as [`TxSize`]
/// discriminants).
pub const MAX_TXSIZE: [u8; BLOCK_SIZES] = [0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 3];

/// `subsize_lookup[PARTITION_TYPES][BLOCK_SIZES]`: resulting block size for a
/// partition of a given block. [`INVALID`] where the split is illegal.
#[rustfmt::skip]
pub const SUBSIZE_LOOKUP: [[u8; BLOCK_SIZES]; PARTITION_TYPES] = [
    // PARTITION_NONE
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
    // PARTITION_HORZ
    [INVALID, INVALID, INVALID, 2, INVALID, INVALID, 5, INVALID, INVALID, 8, INVALID, INVALID, 11],
    // PARTITION_VERT
    [INVALID, INVALID, INVALID, 1, INVALID, INVALID, 4, INVALID, INVALID, 7, INVALID, INVALID, 10],
    // PARTITION_SPLIT
    [INVALID, INVALID, INVALID, 0, INVALID, INVALID, 3, INVALID, INVALID, 6, INVALID, INVALID, 9],
];

/// `partition_lookup[sb_index][BLOCK_SIZES]`: partition implied when a superblock
/// of the row's size contains a sub-block of the given size. `sb_index` runs
/// 4X4→0, 8X8→1, 16X16→2, 32X32→3, 64X64→4. [`INVALID`] where illegal.
#[rustfmt::skip]
pub const PARTITION_LOOKUP: [[u8; BLOCK_SIZES]; 5] = [
    // 4X4
    [0, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID],
    // 8X8: SPLIT, VERT, HORZ, NONE, ...
    [3, 2, 1, 0, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID],
    // 16X16
    [3, 3, 3, 3, 2, 1, 0, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID],
    // 32X32
    [3, 3, 3, 3, 3, 3, 3, 2, 1, 0, INVALID, INVALID, INVALID],
    // 64X64
    [3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 2, 1, 0],
];

// --- Frame / tile geometry (mi = 8px unit, sb64 = 64px superblock) ----------

/// Number of 8×8 mode-info columns for a frame `width` (rounds up).
/// `mi_cols = (width + 7) >> 3`.
pub fn mi_cols(width: u32) -> u32 {
    (width + 7) >> 3
}

/// Number of 8×8 mode-info rows for a frame `height` (rounds up).
pub fn mi_rows(height: u32) -> u32 {
    (height + 7) >> 3
}

/// Round a mi count up to a whole superblock (`mi_cols_aligned_to_sb`).
pub fn mi_aligned_to_sb(mi: u32) -> u32 {
    (mi + 7) & !7
}

/// Number of 64×64 superblock columns spanning `mi_cols` mode-info columns.
pub fn sb64_cols_from_mi(mi_cols: u32) -> u32 {
    mi_aligned_to_sb(mi_cols) >> 3
}

/// `(min, max)` log2 tile-column counts for a frame `mi_cols`, matching
/// `vp9_get_tile_n_bits`.
pub fn tile_cols_log2_range(mi_cols: u32) -> (u32, u32) {
    const MIN_TILE_WIDTH_B64: u32 = 4;
    const MAX_TILE_WIDTH_B64: u32 = 64;
    let sb64_cols = sb64_cols_from_mi(mi_cols);

    let mut min_log2 = 0u32;
    while (MAX_TILE_WIDTH_B64 << min_log2) < sb64_cols {
        min_log2 += 1;
    }
    let mut max_log2 = 1u32;
    while (sb64_cols >> max_log2) >= MIN_TILE_WIDTH_B64 {
        max_log2 += 1;
    }
    (min_log2, max_log2 - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_size_discriminants() {
        assert_eq!(BlockSize::B64X64 as u8, 12);
        assert_eq!(NUM_4X4_BLOCKS_WIDE[BlockSize::B16X16 as usize], 4);
        assert_eq!(NUM_8X8_BLOCKS_HIGH[BlockSize::B64X64 as usize], 8);
        assert_eq!(MAX_TXSIZE[BlockSize::B8X8 as usize], TxSize::Tx8X8 as u8);
        assert_eq!(
            MAX_TXSIZE[BlockSize::B64X64 as usize],
            TxSize::Tx32X32 as u8
        );
    }

    #[test]
    fn subsize_split_of_64x64_is_32x32() {
        assert_eq!(
            SUBSIZE_LOOKUP[Partition::Split as usize][BlockSize::B64X64 as usize],
            BlockSize::B32X32 as u8
        );
        assert_eq!(
            SUBSIZE_LOOKUP[Partition::None as usize][BlockSize::B8X8 as usize],
            BlockSize::B8X8 as u8
        );
        assert_eq!(
            SUBSIZE_LOOKUP[Partition::Horz as usize][BlockSize::B4X4 as usize],
            INVALID
        );
    }

    #[test]
    fn prediction_mode_inter_split() {
        assert!(!PredictionMode::DcPred.is_inter());
        assert!(!PredictionMode::TmPred.is_inter());
        assert!(PredictionMode::ZeroMv.is_inter());
        assert!(PredictionMode::NewMv.is_inter());
    }

    #[test]
    fn mi_geometry() {
        assert_eq!(mi_cols(640), 80);
        assert_eq!(mi_rows(480), 60);
        // Odd size rounds up.
        assert_eq!(mi_cols(636), 80); // (636+7)>>3 = 80
        assert_eq!(mi_rows(476), 60); // (476+7)>>3 = 60
        assert_eq!(mi_cols(176), 22);
        assert_eq!(mi_rows(144), 18);
    }

    #[test]
    fn tile_ranges_match_libvpx() {
        // 640x480: mi_cols=80, sb64_cols=10 -> (0, 1).
        assert_eq!(tile_cols_log2_range(mi_cols(640)), (0, 1));
        // 176x144: mi_cols=22, sb64_cols=3 -> (0, 0).
        assert_eq!(tile_cols_log2_range(mi_cols(176)), (0, 0));
        // 1280x720: mi_cols=160, sb64_cols=20 -> (0, 2).
        assert_eq!(tile_cols_log2_range(mi_cols(1280)), (0, 2));
    }
}
