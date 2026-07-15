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

//! VP9 frame-header writers (error-resilient, Profile 0 subset).
//!
//! Ports the header syntax of `vp9/encoder/vp9_bitstream.c`
//! (`write_uncompressed_header`, `write_compressed_header`, and their helpers),
//! restricted to the encoder's target: Profile 0, 8-bit I420, error-resilient,
//! single reference, single tile, segmentation off, loop filter level 0, no
//! backward probability adaptation (every forward update written as "no
//! update"). Field order was cross-checked against the decoder read path
//! `read_uncompressed_header` (`vp9/decoder/vp9_decodeframe.c`), which is the
//! ground truth.

use crate::vp9::common::bit_buffer::BitBufferWriter;
use crate::vp9::common::block::{mi_cols, tile_cols_log2_range, TxMode};
use crate::vp9::common::bool_coder::BoolWriter;

/// `VP9_FRAME_MARKER` (`0b10`).
const FRAME_MARKER: u32 = 2;
/// Frame sync code bytes (`VP9_SYNC_CODE_0..2`).
const SYNC_CODE: [u32; 3] = [0x49, 0x83, 0x42];
/// `VPX_CS_UNKNOWN`.
const CS_UNKNOWN: u32 = 0;
/// `FRAME_CONTEXTS_LOG2`.
const FRAME_CONTEXTS_LOG2: u32 = 2;
/// `REF_FRAMES` (refresh mask width).
const REF_FRAMES: u32 = 8;
/// `REF_FRAMES_LOG2` (per-ref slot index width).
const REF_FRAMES_LOG2: u32 = 3;
/// `QINDEX_BITS`.
const QINDEX_BITS: u32 = 8;
/// `REFS_PER_FRAME`.
const REFS_PER_FRAME: usize = 3;
/// `DIFF_UPDATE_PROB` / `MV_UPDATE_PROB` — probability for "no update" flags.
pub(crate) const DIFF_UPDATE_PROB: u8 = 252;

/// `EIGHTTAP` interpolation filter (the only one this encoder signals).
pub const INTERP_EIGHTTAP: u8 = 0;

/// Frame type (`FRAME_TYPE`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameType {
    Key,
    Inter,
}

/// Parameters for the uncompressed header, restricted to the encoder's subset.
#[derive(Clone, Debug)]
pub struct UncompressedHeader {
    pub frame_type: FrameType,
    pub show_frame: bool,
    /// Frame width in pixels (1..=65536).
    pub width: u32,
    /// Frame height in pixels (1..=65536).
    pub height: u32,
    pub base_qindex: u8,
    /// Inter only: 8-bit refresh mask (`refresh_frame_flags`).
    pub refresh_frame_flags: u8,
    /// Inter only: reference slot indices for LAST/GOLDEN/ALTREF (0..=7).
    pub ref_frame_idx: [u8; REFS_PER_FRAME],
    /// Inter only: per-reference sign bias bits.
    pub ref_sign_bias: [bool; REFS_PER_FRAME],
    /// Inter only: `allow_high_precision_mv`.
    pub allow_high_precision_mv: bool,
    /// Inter only: interpolation filter (`INTERP_EIGHTTAP`).
    pub interp_filter: u8,
    /// `log2` of the number of tile columns (0 = single tile). Emitted by
    /// `write_tile_info`; the encoder derives it from the frame width.
    pub log2_tile_cols: u32,
}

impl UncompressedHeader {
    /// A minimal keyframe header for `width`×`height` at `base_qindex`.
    pub fn keyframe(width: u32, height: u32, base_qindex: u8) -> Self {
        UncompressedHeader {
            frame_type: FrameType::Key,
            show_frame: true,
            width,
            height,
            base_qindex,
            refresh_frame_flags: 0xFF,
            ref_frame_idx: [0; REFS_PER_FRAME],
            ref_sign_bias: [false; REFS_PER_FRAME],
            allow_high_precision_mv: false,
            interp_filter: INTERP_EIGHTTAP,
            log2_tile_cols: 0,
        }
    }

    /// A minimal single-reference inter header (all three refs → slot 0,
    /// refresh slot 0).
    pub fn inter(width: u32, height: u32, base_qindex: u8) -> Self {
        UncompressedHeader {
            frame_type: FrameType::Inter,
            show_frame: true,
            width,
            height,
            base_qindex,
            refresh_frame_flags: 0x01,
            ref_frame_idx: [0; REFS_PER_FRAME],
            ref_sign_bias: [false; REFS_PER_FRAME],
            allow_high_precision_mv: false,
            interp_filter: INTERP_EIGHTTAP,
            log2_tile_cols: 0,
        }
    }
}

fn write_sync_code(wb: &mut BitBufferWriter) {
    for b in SYNC_CODE {
        wb.write_literal(b, 8);
    }
}

/// `write_bitdepth_colorspace_sampling` for Profile 0: color space (3 bits) then,
/// since it is not sRGB, a color range bit. No bit-depth or subsampling bits.
fn write_color_config_profile0(wb: &mut BitBufferWriter) {
    wb.write_literal(CS_UNKNOWN, 3);
    // color_space != VPX_CS_SRGB → write color_range (0 = studio range).
    wb.write_bit(0);
}

/// `write_render_size`: render size equals frame size, so a single 0 bit.
fn write_render_size_same(wb: &mut BitBufferWriter) {
    wb.write_bit(0);
}

fn write_frame_size(wb: &mut BitBufferWriter, width: u32, height: u32) {
    wb.write_literal(width - 1, 16);
    wb.write_literal(height - 1, 16);
    write_render_size_same(wb);
}

/// `write_interp_filter`. The literal map `{EIGHTTAP→1, EIGHTTAP_SMOOTH→0,
/// EIGHTTAP_SHARP→2, BILINEAR→3}` is inverted from the enum order (surprising),
/// mirroring libvpx `filter_to_literal[] = {1, 0, 2, 3}`.
fn write_interp_filter(wb: &mut BitBufferWriter, filter: u8) {
    const FILTER_TO_LITERAL: [u32; 4] = [1, 0, 2, 3];
    // Not SWITCHABLE → write bit 0 then the 2-bit literal.
    wb.write_bit(0);
    wb.write_literal(FILTER_TO_LITERAL[filter as usize], 2);
}

/// `encode_loopfilter` with everything off: level 0 (6 bits), sharpness 0
/// (3 bits), `mode_ref_delta_enabled` = 0.
fn write_loopfilter_off(wb: &mut BitBufferWriter) {
    wb.write_literal(0, 6);
    wb.write_literal(0, 3);
    wb.write_bit(0);
}

/// `encode_quantization`: base qindex then three zero delta-q flags.
fn write_quantization(wb: &mut BitBufferWriter, base_qindex: u8) {
    wb.write_literal(base_qindex as u32, QINDEX_BITS);
    // write_delta_q(0) → single 0 bit, three times (y_dc, uv_dc, uv_ac).
    wb.write_bit(0);
    wb.write_bit(0);
    wb.write_bit(0);
}

/// `write_tile_info` for `log2_tile_cols` tile columns and a single tile row
/// (`log2_tile_rows = 0`). Port of `vp9_bitstream.c:write_tile_info`: emit
/// `log2_tile_cols - min_log2` one-bits, then a `0` terminator unless we are at
/// the maximum, then the single-tile-row flag.
fn write_tile_info(wb: &mut BitBufferWriter, width: u32, log2_tile_cols: u32) {
    let (min_log2, max_log2) = tile_cols_log2_range(mi_cols(width));
    // The tile-column cap must never drop the count below the legal minimum for
    // this width, or the `saturating_sub` below would silently emit zero
    // increment bits for an under-minimum count and desync the decoder.
    debug_assert!(
        log2_tile_cols >= min_log2,
        "log2_tile_cols ({log2_tile_cols}) < min_log2 ({min_log2}) for width {width}"
    );
    // columns: log2_tile_cols - min_log2 increment ones, then a 0 terminator
    // if we are below max.
    let ones = log2_tile_cols.saturating_sub(min_log2);
    for _ in 0..ones {
        wb.write_bit(1);
    }
    if log2_tile_cols < max_log2 {
        wb.write_bit(0);
    }
    // rows: log2_tile_rows == 0.
    wb.write_bit(0);
}

/// Write the uncompressed header through the tile info (everything before the
/// 16-bit compressed-header-size field). Always error-resilient.
pub fn write_uncompressed_header(wb: &mut BitBufferWriter, p: &UncompressedHeader) {
    wb.write_literal(FRAME_MARKER, 2);
    wb.write_literal(0, 2); // profile 0
    wb.write_bit(0); // show_existing_frame = 0

    wb.write_bit(u8::from(p.frame_type == FrameType::Inter)); // frame_type
    wb.write_bit(u8::from(p.show_frame));
    wb.write_bit(1); // error_resilient_mode = 1

    match p.frame_type {
        FrameType::Key => {
            write_sync_code(wb);
            write_color_config_profile0(wb);
            write_frame_size(wb, p.width, p.height);
        }
        FrameType::Inter => {
            // show_frame = 1 ⇒ intra_only bit not written (implicitly 0).
            // error_resilient ⇒ reset_frame_context not written.
            wb.write_literal(p.refresh_frame_flags as u32, REF_FRAMES);
            for i in 0..REFS_PER_FRAME {
                wb.write_literal(p.ref_frame_idx[i] as u32, REF_FRAMES_LOG2);
                wb.write_bit(u8::from(p.ref_sign_bias[i]));
            }
            // write_frame_size_with_refs: first ref has same size → write bit 1,
            // stop; then render size.
            wb.write_bit(1);
            write_render_size_same(wb);
            wb.write_bit(u8::from(p.allow_high_precision_mv));
            write_interp_filter(wb, p.interp_filter);
        }
    }

    // error_resilient ⇒ refresh_frame_context / frame_parallel not written.
    wb.write_literal(0, FRAME_CONTEXTS_LOG2); // frame_context_idx = 0
    write_loopfilter_off(wb);
    write_quantization(wb, p.base_qindex);
    wb.write_bit(0); // segmentation enabled = 0
    write_tile_info(wb, p.width, p.log2_tile_cols);
}

/// `tx_mode_to_biggest_tx_size`: largest TX size (0..=3) permitted by `tx_mode`.
fn biggest_tx_size(tx_mode: TxMode) -> u8 {
    match tx_mode {
        TxMode::Only4X4 => 0,
        TxMode::Allow8X8 => 1,
        TxMode::Allow16X16 => 2,
        TxMode::Allow32X32 | TxMode::TxModeSelect => 3,
    }
}

/// Write a run of `n` "no update" flags (each `write(0, 252)`).
fn no_updates(w: &mut BoolWriter, n: usize) {
    for _ in 0..n {
        w.write(0, DIFF_UPDATE_PROB);
    }
}

/// `vp9_write_nmv_probs` under the no-update regime (all flags 0). `usehp`
/// mirrors `allow_high_precision_mv`.
fn write_nmv_no_update(w: &mut BoolWriter, usehp: bool) {
    no_updates(w, 3); // joints: MV_JOINTS - 1
    for _comp in 0..2 {
        no_updates(w, 1); // sign
        no_updates(w, 10); // classes: MV_CLASSES - 1
        no_updates(w, 1); // class0: CLASS0_SIZE - 1
        no_updates(w, 10); // bits: MV_OFFSET_BITS
    }
    for _comp in 0..2 {
        // class0_fp: CLASS0_SIZE * (MV_FP_SIZE - 1)
        no_updates(w, 2 * 3);
        // fp: MV_FP_SIZE - 1
        no_updates(w, 3);
    }
    if usehp {
        for _comp in 0..2 {
            no_updates(w, 1); // class0_hp
            no_updates(w, 1); // hp
        }
    }
}

/// Write the compressed header (bool-coded) for the encoder's subset: transform
/// mode, then every probability group as "no update". `is_intra_only` is true for
/// keyframes; `allow_hp` mirrors `allow_high_precision_mv`.
pub fn write_compressed_header(
    w: &mut BoolWriter,
    tx_mode: TxMode,
    is_intra_only: bool,
    allow_hp: bool,
) {
    // encode_txfm_probs (non-lossless): tx_mode literal, +1 selector bit for
    // ALLOW_32X32/TX_MODE_SELECT. TX_MODE_SELECT emits per-context prob updates,
    // which this encoder never uses.
    let m = (tx_mode as u8).min(TxMode::Allow32X32 as u8);
    w.write_literal(m as u32, 2);
    if (tx_mode as u8) >= TxMode::Allow32X32 as u8 {
        w.write_bit(u8::from(tx_mode == TxMode::TxModeSelect));
    }

    // update_coef_probs: one plain bit(0) per tx size ≤ biggest for tx_mode.
    let max_tx = biggest_tx_size(tx_mode);
    for _ in 0..=max_tx {
        w.write_bit(0);
    }

    // update_skip_probs: SKIP_CONTEXTS = 3.
    no_updates(w, 3);

    if !is_intra_only {
        // inter_mode: INTER_MODE_CONTEXTS(7) * (INTER_MODES - 1 = 3).
        no_updates(w, 7 * 3);
        // interp filter is EIGHTTAP (not SWITCHABLE) → no switchable prob updates.
        // intra_inter: INTRA_INTER_CONTEXTS = 4.
        no_updates(w, 4);
        // allow_comp_inter_inter = false → reference_mode / comp blocks omitted.
        // single_ref: REF_CONTEXTS(5) * 2.
        no_updates(w, 5 * 2);
        // comp_ref omitted (SINGLE_REFERENCE).
        // y_mode: BLOCK_SIZE_GROUPS(4) * (INTRA_MODES - 1 = 9).
        no_updates(w, 4 * 9);
        // partition: PARTITION_CONTEXTS(16) * (PARTITION_TYPES - 1 = 3).
        no_updates(w, 16 * 3);
        // mv probs.
        write_nmv_no_update(w, allow_hp);
    }
}

/// Assemble the frame header bytes: uncompressed header, the 16-bit
/// compressed-header size (backpatched), and the compressed header. Returns the
/// concatenated bytes and the byte offset at which the compressed header begins.
pub fn pack_frame_header(p: &UncompressedHeader, tx_mode: TxMode) -> (Vec<u8>, usize) {
    let is_intra_only = p.frame_type == FrameType::Key;

    // Compressed header is independent of its own size.
    let mut bw = BoolWriter::new();
    write_compressed_header(&mut bw, tx_mode, is_intra_only, p.allow_high_precision_mv);
    let compressed = bw.finalize();

    // Uncompressed header + 16-bit size placeholder, then backpatch.
    let mut wb = BitBufferWriter::new();
    write_uncompressed_header(&mut wb, p);
    let size_bit_offset = wb.bit_offset();
    wb.write_literal(0, 16); // placeholder
    wb.backpatch_literal(size_bit_offset, compressed.len() as u32, 16);

    let mut out = wb.finalize();
    let compressed_offset = out.len();
    out.extend_from_slice(&compressed);
    (out, compressed_offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::debug::parser;

    fn check_roundtrip(p: &UncompressedHeader, tx_mode: TxMode) {
        let (bytes, comp_off) = pack_frame_header(p, tx_mode);
        // Inter frames inherit their size from the reference buffer; supply it.
        let ref_size = match p.frame_type {
            FrameType::Inter => Some((p.width, p.height)),
            FrameType::Key => None,
        };
        let parsed = parser::parse_uncompressed_header(&bytes, ref_size);

        assert_eq!(parsed.frame_type, p.frame_type);
        assert_eq!(parsed.show_frame, p.show_frame);
        assert!(parsed.error_resilient);
        assert_eq!(parsed.width, p.width);
        assert_eq!(parsed.height, p.height);
        assert_eq!(parsed.base_qindex, p.base_qindex);
        assert_eq!(parsed.log2_tile_cols, p.log2_tile_cols);
        assert_eq!(parsed.profile, 0);
        assert!(!parsed.show_existing_frame);
        assert_eq!(parsed.frame_context_idx, 0);
        assert_eq!(parsed.loop_filter_level, 0);
        assert!(!parsed.segmentation_enabled);
        if p.frame_type == FrameType::Inter {
            assert_eq!(parsed.refresh_frame_flags, p.refresh_frame_flags);
            assert_eq!(parsed.ref_frame_idx, p.ref_frame_idx);
            assert_eq!(parsed.ref_sign_bias, p.ref_sign_bias);
            assert_eq!(parsed.allow_high_precision_mv, p.allow_high_precision_mv);
            assert_eq!(parsed.interp_filter, p.interp_filter);
        } else {
            assert_eq!(parsed.refresh_frame_flags, 0xFF);
        }

        // The 16-bit size field must equal the actual compressed header length.
        assert_eq!(
            parsed.compressed_header_size as usize,
            bytes.len() - comp_off
        );
        // Compressed header begins on the byte boundary the parser reports.
        assert_eq!(parsed.uncompressed_header_bytes, comp_off);

        // Parse the compressed header; all flags must be "no update".
        let is_intra = p.frame_type == FrameType::Key;
        let cp = parser::parse_compressed_header(
            &bytes[comp_off..],
            is_intra,
            p.allow_high_precision_mv,
        );
        assert_eq!(cp.tx_mode, tx_mode);
        assert!(cp.all_no_update, "a compressed-header flag was not zero");
    }

    #[test]
    fn keyframe_headers_round_trip() {
        for &(w, h) in &[(640u32, 480u32), (636, 476), (176, 144), (1280, 720)] {
            for &q in &[0u8, 40, 128, 255] {
                let p = UncompressedHeader::keyframe(w, h, q);
                check_roundtrip(&p, TxMode::Allow8X8);
            }
        }
    }

    #[test]
    fn keyframe_all_tx_modes() {
        let p = UncompressedHeader::keyframe(640, 480, 64);
        for tx in [
            TxMode::Only4X4,
            TxMode::Allow8X8,
            TxMode::Allow16X16,
            TxMode::Allow32X32,
        ] {
            check_roundtrip(&p, tx);
        }
    }

    #[test]
    fn inter_headers_round_trip() {
        for &(w, h) in &[(640u32, 480u32), (636, 476), (176, 144), (1280, 720)] {
            let mut p = UncompressedHeader::inter(w, h, 96);
            p.ref_frame_idx = [0, 1, 2];
            p.ref_sign_bias = [false, true, false];
            check_roundtrip(&p, TxMode::Allow8X8);
        }
    }

    #[test]
    fn inter_high_precision_mv() {
        let mut p = UncompressedHeader::inter(640, 480, 100);
        p.allow_high_precision_mv = true;
        check_roundtrip(&p, TxMode::Allow16X16);
    }

    #[test]
    fn multi_tile_header_round_trips() {
        // 640-wide legally allows up to `log2_tile_cols == 1` (2 columns); the
        // header must encode the tile-column count and parse back to the same
        // value, exercising the increment-bit path `write_tile_info` emits for
        // multi-tile streams (and `parse_tile_info`'s inverse).
        for &(w, h) in &[(640u32, 480u32), (636, 476)] {
            let mut kf = UncompressedHeader::keyframe(w, h, 96);
            kf.log2_tile_cols = 1;
            check_roundtrip(&kf, TxMode::Allow8X8);

            let mut inter = UncompressedHeader::inter(w, h, 96);
            inter.log2_tile_cols = 1;
            check_roundtrip(&inter, TxMode::Allow8X8);
        }
    }
}
