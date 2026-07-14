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

//! Minimal VP9 header parsers for our own round-trip validation.
//!
//! These mirror the decoder field order (`read_uncompressed_header` in
//! `vp9/decoder/vp9_decodeframe.c`) for the encoder's Profile-0 error-resilient
//! subset only — they are NOT a general VP9 decoder. They exist so header
//! writers can be verified without libvpx in the fast unit-test loop.

use crate::vp9::common::bit_buffer::BitBufferReader;
use crate::vp9::common::block::TxMode;
use crate::vp9::common::bool_coder::BoolReader;
use crate::vp9::enc::bitstream::{FrameType, DIFF_UPDATE_PROB};

/// Fields recovered from the uncompressed header.
#[derive(Clone, Debug)]
pub struct ParsedUncompressed {
    pub profile: u8,
    pub show_existing_frame: bool,
    pub frame_type: FrameType,
    pub show_frame: bool,
    pub error_resilient: bool,
    pub width: u32,
    pub height: u32,
    pub refresh_frame_flags: u8,
    pub ref_frame_idx: [u8; 3],
    pub ref_sign_bias: [bool; 3],
    pub allow_high_precision_mv: bool,
    pub interp_filter: u8,
    pub frame_context_idx: u8,
    pub loop_filter_level: u8,
    pub base_qindex: u8,
    pub segmentation_enabled: bool,
    /// `log2` of the number of tile columns decoded from `tile_info`.
    pub log2_tile_cols: u32,
    /// Value of the 16-bit compressed-header-size field.
    pub compressed_header_size: u16,
    /// Byte offset at which the compressed header begins.
    pub uncompressed_header_bytes: usize,
}

/// Parse the uncompressed header of a frame produced by
/// [`crate::vp9::enc::bitstream::write_uncompressed_header`] plus the trailing
/// 16-bit compressed-header size.
///
/// Inter frames do not carry their own size (it is taken from the reference
/// buffer via the same-size flag), so `ref_frame_size` supplies the dimensions
/// the real decoder would inherit; it is required to parse the tile info and is
/// ignored for keyframes.
pub fn parse_uncompressed_header(
    bytes: &[u8],
    ref_frame_size: Option<(u32, u32)>,
) -> ParsedUncompressed {
    let mut rb = BitBufferReader::new(bytes);

    let marker = rb.read_literal(2);
    assert_eq!(marker, 2, "bad frame marker");
    // Profile: two bits low+high (profile 0 = 00; higher profiles unused here).
    let profile_low = rb.read_bit();
    let profile_high = rb.read_bit();
    let profile = profile_low | (profile_high << 1);

    let show_existing_frame = rb.read_bit() != 0;
    // show_existing_frame path is never produced by our encoder.
    assert!(!show_existing_frame, "unexpected show_existing_frame");

    let frame_type = if rb.read_bit() == 0 {
        FrameType::Key
    } else {
        FrameType::Inter
    };
    let show_frame = rb.read_bit() != 0;
    let error_resilient = rb.read_bit() != 0;

    let width;
    let height;
    let refresh_frame_flags;
    let mut ref_frame_idx = [0u8; 3];
    let mut ref_sign_bias = [false; 3];
    let mut allow_high_precision_mv = false;
    let mut interp_filter = 0u8;

    match frame_type {
        FrameType::Key => {
            let sync = rb.read_literal(24);
            assert_eq!(sync, 0x498342, "bad sync code");
            // Profile-0 color config: color_space (3), color_range (1).
            let _color_space = rb.read_literal(3);
            let _color_range = rb.read_bit();
            width = rb.read_literal(16) + 1;
            height = rb.read_literal(16) + 1;
            let render_diff = rb.read_bit();
            assert_eq!(render_diff, 0, "unexpected render size");
            refresh_frame_flags = 0xFF;
        }
        FrameType::Inter => {
            // show_frame = 1 ⇒ no intra_only bit. error_resilient ⇒ no
            // reset_frame_context.
            assert!(show_frame, "inter frames must be shown in this subset");
            refresh_frame_flags = rb.read_literal(8) as u8;
            for i in 0..3 {
                ref_frame_idx[i] = rb.read_literal(3) as u8;
                ref_sign_bias[i] = rb.read_bit() != 0;
            }
            // frame_size_with_refs: first ref same-size bit = 1. The size is
            // inherited from the reference buffer, not coded in the header.
            let found = rb.read_bit();
            assert_eq!(found, 1, "expected same-size ref");
            let (rw, rh) = ref_frame_size.expect("inter frame parsing requires ref_frame_size");
            width = rw;
            height = rh;
            let render_diff = rb.read_bit();
            assert_eq!(render_diff, 0, "unexpected render size");
            allow_high_precision_mv = rb.read_bit() != 0;
            // interp filter: not-switchable bit then 2-bit literal.
            let is_switchable = rb.read_bit();
            assert_eq!(is_switchable, 0, "unexpected switchable filter");
            let literal = rb.read_literal(2);
            const LITERAL_TO_FILTER: [u8; 4] = [1, 0, 2, 3]; // EIGHTTAP_SMOOTH, EIGHTTAP, SHARP, BILINEAR
            interp_filter = LITERAL_TO_FILTER[literal as usize];
        }
    }
    assert!(error_resilient, "this subset always sets error_resilient");

    // error_resilient ⇒ no refresh_frame_context / frame_parallel bits.
    let frame_context_idx = rb.read_literal(2) as u8;

    // Loop filter: level (6), sharpness (3), mode_ref_delta_enabled (1).
    let loop_filter_level = rb.read_literal(6) as u8;
    let _sharpness = rb.read_literal(3);
    let mode_ref_delta_enabled = rb.read_bit();
    assert_eq!(mode_ref_delta_enabled, 0, "unexpected lf deltas");

    // Quantization: base qindex (8) + three delta flags.
    let base_qindex = rb.read_literal(8) as u8;
    for _ in 0..3 {
        let has_delta = rb.read_bit();
        assert_eq!(has_delta, 0, "unexpected delta_q");
    }

    // Segmentation.
    let segmentation_enabled = rb.read_bit() != 0;
    assert!(!segmentation_enabled, "unexpected segmentation");

    // Tile info.
    let log2_tile_cols = parse_tile_info(&mut rb, width);

    // 16-bit compressed header size.
    let compressed_header_size = rb.read_literal(16) as u16;
    let uncompressed_header_bytes = rb.bytes_read();

    ParsedUncompressed {
        profile,
        show_existing_frame,
        frame_type,
        show_frame,
        error_resilient,
        width,
        height,
        refresh_frame_flags,
        ref_frame_idx,
        ref_sign_bias,
        allow_high_precision_mv,
        interp_filter,
        frame_context_idx,
        loop_filter_level,
        base_qindex,
        segmentation_enabled,
        log2_tile_cols,
        compressed_header_size,
        uncompressed_header_bytes,
    }
}

/// Read `write_tile_info`'s bits and return the decoded `log2_tile_cols` (the
/// inverse of `bitstream.rs::write_tile_info`). Columns are coded as increment
/// one-bits from `min_log2_tile_cols` upward, stopping at a `0` terminator or on
/// reaching `max_log2_tile_cols`; tile rows are `0` for this encoder's subset.
fn parse_tile_info(rb: &mut BitBufferReader, width: u32) -> u32 {
    use crate::vp9::common::block::{mi_cols, tile_cols_log2_range};
    let (min_log2, max_log2) = tile_cols_log2_range(mi_cols(width));
    // columns: keep reading increment bits until a 0 or until we hit max_log2.
    let mut log2_tile_cols = min_log2;
    while log2_tile_cols < max_log2 {
        if rb.read_bit() != 0 {
            log2_tile_cols += 1;
        } else {
            break;
        }
    }
    // rows: log2_tile_rows == 0 (a single 0 bit; no increment for our subset).
    let has_rows = rb.read_bit();
    assert_eq!(has_rows, 0, "expected 0 tile rows");
    log2_tile_cols
}

/// Fields recovered from the compressed header.
#[derive(Clone, Debug)]
pub struct ParsedCompressed {
    pub tx_mode: TxMode,
    /// True if every probability-update flag was "no update" (0).
    pub all_no_update: bool,
}

/// Parse the compressed header of a frame produced by
/// [`crate::vp9::enc::bitstream::write_compressed_header`]. `is_intra_only` and
/// `allow_hp` must match the values used when writing.
pub fn parse_compressed_header(
    bytes: &[u8],
    is_intra_only: bool,
    allow_hp: bool,
) -> ParsedCompressed {
    let mut r = BoolReader::new(bytes);
    let mut all_no_update = true;
    let flag = |r: &mut BoolReader, all: &mut bool| {
        if r.read(DIFF_UPDATE_PROB) != 0 {
            *all = false;
        }
    };

    // tx mode.
    let m = r.read_literal(2) as u8;
    let tx_mode = if m < TxMode::Allow32X32 as u8 {
        match m {
            0 => TxMode::Only4X4,
            1 => TxMode::Allow8X8,
            _ => TxMode::Allow16X16,
        }
    } else {
        // Selector bit distinguishes ALLOW_32X32 from TX_MODE_SELECT.
        if r.read_bit() != 0 {
            TxMode::TxModeSelect
        } else {
            TxMode::Allow32X32
        }
    };

    // coef "no update": one plain bit(0) per tx size ≤ biggest.
    let biggest = match tx_mode {
        TxMode::Only4X4 => 0,
        TxMode::Allow8X8 => 1,
        TxMode::Allow16X16 => 2,
        TxMode::Allow32X32 | TxMode::TxModeSelect => 3,
    };
    for _ in 0..=biggest {
        if r.read_bit() != 0 {
            all_no_update = false;
        }
    }

    // skip: 3.
    for _ in 0..3 {
        flag(&mut r, &mut all_no_update);
    }

    if !is_intra_only {
        for _ in 0..(7 * 3) {
            flag(&mut r, &mut all_no_update); // inter_mode
        }
        for _ in 0..4 {
            flag(&mut r, &mut all_no_update); // intra_inter
        }
        for _ in 0..(5 * 2) {
            flag(&mut r, &mut all_no_update); // single_ref
        }
        for _ in 0..(4 * 9) {
            flag(&mut r, &mut all_no_update); // y_mode
        }
        for _ in 0..(16 * 3) {
            flag(&mut r, &mut all_no_update); // partition
        }
        // nmv probs.
        for _ in 0..3 {
            flag(&mut r, &mut all_no_update); // joints
        }
        for _comp in 0..2 {
            for _ in 0..(1 + 10 + 1 + 10) {
                flag(&mut r, &mut all_no_update); // sign, classes, class0, bits
            }
        }
        for _comp in 0..2 {
            for _ in 0..(2 * 3 + 3) {
                flag(&mut r, &mut all_no_update); // class0_fp, fp
            }
        }
        if allow_hp {
            for _comp in 0..2 {
                for _ in 0..2 {
                    flag(&mut r, &mut all_no_update); // class0_hp, hp
                }
            }
        }
    }

    ParsedCompressed {
        tx_mode,
        all_no_update,
    }
}
