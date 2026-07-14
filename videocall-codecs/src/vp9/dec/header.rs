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

//! VP9 frame-header parsing for the decoder (Profile-0, error-resilient subset).
//!
//! Ports the decoder read path `read_uncompressed_header`
//! (`vp9/decoder/vp9_decodeframe.c`) restricted to what our pure-Rust encoder
//! emits: Profile 0, 8-bit I420, error-resilient, single reference, loop filter
//! off, segmentation off, no forward probability updates. Unlike the test-only
//! `debug::parser`, this is compiled into every build (including wasm) and
//! returns typed errors instead of panicking on malformed input.

use crate::vp9::common::bit_buffer::BitBufferReader;
use crate::vp9::common::block::{mi_cols, tile_cols_log2_range, TxMode};
use crate::vp9::common::bool_coder::BoolReader;
use crate::vp9::dec::{DecodeError, MAX_DIM};

/// Frame type recovered from the uncompressed header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameType {
    Key,
    Inter,
}

/// The header fields the decoder needs to reconstruct a frame of the encoder's
/// subset.
#[derive(Clone, Debug)]
pub struct FrameHeader {
    pub frame_type: FrameType,
    pub width: u32,
    pub height: u32,
    pub base_qindex: u8,
    /// `log2` of the number of tile columns (0 = single tile).
    pub log2_tile_cols: u32,
    /// Transform mode from the compressed header (`ALLOW_8X8` for this encoder).
    pub tx_mode: TxMode,
    /// Byte offset at which the compressed header begins (end of the uncompressed
    /// header including the 16-bit size field).
    pub compressed_header_offset: usize,
    /// Length in bytes of the compressed header.
    pub compressed_header_size: usize,
}

impl FrameHeader {
    /// Byte offset at which the tile payloads begin.
    pub fn tile_data_offset(&self) -> usize {
        self.compressed_header_offset + self.compressed_header_size
    }
}

/// Frame sync code bytes (`VP9_SYNC_CODE_0..2`) concatenated as a 24-bit literal.
const SYNC_CODE: u32 = 0x0049_8342 & 0x00FF_FFFF;

/// Parse the uncompressed header and the compressed header's transform mode.
///
/// `bytes` is the whole frame. Returns the recovered [`FrameHeader`] or a
/// [`DecodeError`] when the stream is truncated or uses a feature outside the
/// supported subset.
pub fn parse_frame_header(bytes: &[u8]) -> Result<FrameHeader, DecodeError> {
    let mut rb = BitBufferReader::new(bytes);

    if rb.read_literal(2) != 2 {
        return Err(DecodeError::Corrupt("bad frame marker"));
    }
    // Profile: two bits (low then high); only profile 0 is supported.
    let profile = rb.read_bit() | (rb.read_bit() << 1);
    if profile != 0 {
        return Err(DecodeError::Unsupported("only VP9 profile 0"));
    }
    if rb.read_bit() != 0 {
        return Err(DecodeError::Unsupported("show_existing_frame"));
    }

    let frame_type = if rb.read_bit() == 0 {
        FrameType::Key
    } else {
        FrameType::Inter
    };
    let _show_frame = rb.read_bit() != 0;
    let error_resilient = rb.read_bit() != 0;

    let (width, height);
    match frame_type {
        FrameType::Key => {
            if rb.read_literal(24) != SYNC_CODE {
                return Err(DecodeError::Corrupt("bad sync code"));
            }
            // Profile-0 color config: color_space (3), color_range (1).
            let _color_space = rb.read_literal(3);
            let _color_range = rb.read_bit();
            width = rb.read_literal(16) + 1;
            height = rb.read_literal(16) + 1;
            if rb.read_bit() != 0 {
                return Err(DecodeError::Unsupported("render size != frame size"));
            }
        }
        FrameType::Inter => {
            // Inter frames are decoded in a later milestone (they inherit their
            // size from a reference buffer, which M0 has no keyframe history for).
            return Err(DecodeError::Unsupported("inter frame decode (M1)"));
        }
    }

    // Trust boundary: reject oversized dimensions from the untrusted header BEFORE
    // the caller allocates any dimension-derived buffer. This transitively bounds
    // the reconstruction buffer and every context/grid vector.
    if width > MAX_DIM || height > MAX_DIM {
        return Err(DecodeError::TooLarge);
    }

    if !error_resilient {
        return Err(DecodeError::Unsupported("non-error-resilient stream"));
    }

    // error_resilient ⇒ no refresh_frame_context / frame_parallel bits.
    let _frame_context_idx = rb.read_literal(2);

    // Loop filter: level (6), sharpness (3), mode_ref_delta_enabled (1).
    let loop_filter_level = rb.read_literal(6);
    let _sharpness = rb.read_literal(3);
    if rb.read_bit() != 0 {
        return Err(DecodeError::Unsupported("loop filter mode/ref deltas"));
    }
    if loop_filter_level != 0 {
        return Err(DecodeError::Unsupported("loop filter level != 0"));
    }

    // Quantization: base qindex (8) + three delta flags (all zero).
    let base_qindex = rb.read_literal(8) as u8;
    for _ in 0..3 {
        if rb.read_bit() != 0 {
            return Err(DecodeError::Unsupported("delta_q"));
        }
    }

    // Segmentation.
    if rb.read_bit() != 0 {
        return Err(DecodeError::Unsupported("segmentation"));
    }

    // Tile info.
    let log2_tile_cols = parse_tile_info(&mut rb, width)?;

    // 16-bit compressed header size, then the header ends on a byte boundary.
    let compressed_header_size = rb.read_literal(16) as usize;
    let compressed_header_offset = rb.bytes_read();

    if compressed_header_offset + compressed_header_size > bytes.len() {
        return Err(DecodeError::Truncated);
    }

    // Compressed header: recover the transform mode (the rest is all "no update"
    // for this encoder and does not affect the keyframe decode).
    let tx_mode = parse_tx_mode(
        &bytes[compressed_header_offset..compressed_header_offset + compressed_header_size],
    )?;

    Ok(FrameHeader {
        frame_type,
        width,
        height,
        base_qindex,
        log2_tile_cols,
        tx_mode,
        compressed_header_offset,
        compressed_header_size,
    })
}

/// Inverse of `write_tile_info`: columns are coded as increment one-bits from
/// `min_log2_tile_cols` upward, stopping at a `0` terminator or on reaching
/// `max_log2_tile_cols`; a single tile row (`log2_tile_rows == 0`) follows.
fn parse_tile_info(rb: &mut BitBufferReader, width: u32) -> Result<u32, DecodeError> {
    let (min_log2, max_log2) = tile_cols_log2_range(mi_cols(width));
    let mut log2_tile_cols = min_log2;
    while log2_tile_cols < max_log2 {
        if rb.read_bit() != 0 {
            log2_tile_cols += 1;
        } else {
            break;
        }
    }
    if rb.read_bit() != 0 {
        return Err(DecodeError::Unsupported("tile rows"));
    }
    Ok(log2_tile_cols)
}

/// Read only the transform mode from the compressed header (`read_tx_mode`).
fn parse_tx_mode(bytes: &[u8]) -> Result<TxMode, DecodeError> {
    let mut r = BoolReader::new(bytes);
    let m = r.read_literal(2) as u8;
    let tx_mode = if m < TxMode::Allow32X32 as u8 {
        match m {
            0 => TxMode::Only4X4,
            1 => TxMode::Allow8X8,
            _ => TxMode::Allow16X16,
        }
    } else if r.read_bit() != 0 {
        TxMode::TxModeSelect
    } else {
        TxMode::Allow32X32
    };
    Ok(tx_mode)
}
