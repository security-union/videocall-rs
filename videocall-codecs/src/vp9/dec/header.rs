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

/// `REFS_PER_FRAME` — LAST / GOLDEN / ALTREF reference index slots.
const REFS_PER_FRAME: usize = 3;
/// `REF_FRAMES` — number of reference-buffer slots the refresh mask selects.
pub const REF_FRAMES: usize = 8;
/// `REF_FRAMES_LOG2` — bit width of one reference-slot index.
const REF_FRAMES_LOG2: u32 = 3;

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
    /// 8-bit reference-slot refresh mask (`refresh_frame_flags`). Keyframes
    /// refresh every slot (`0xFF`); this encoder's inter frames refresh slot 0.
    pub refresh_frame_flags: u8,
    /// Reference-slot indices for LAST/GOLDEN/ALTREF (inter only; `[0; 3]` on a
    /// keyframe). `ref_frame_idx[0]` is the LAST slot this encoder motion
    /// compensates against.
    pub ref_frame_idx: [u8; REFS_PER_FRAME],
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
/// `bytes` is the whole frame. `ref_sizes` supplies the `(width, height)` of each
/// of the eight reference-buffer slots (or `None` for an empty slot); it is
/// required to parse an inter frame, which inherits its size from a reference
/// rather than coding it, and is ignored for keyframes. Returns the recovered
/// [`FrameHeader`] or a [`DecodeError`] when the stream is truncated, references
/// an empty slot, or uses a feature outside the supported subset.
pub fn parse_frame_header(
    bytes: &[u8],
    ref_sizes: Option<&[Option<(u32, u32)>; REF_FRAMES]>,
) -> Result<FrameHeader, DecodeError> {
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
    let show_frame = rb.read_bit() != 0;
    let error_resilient = rb.read_bit() != 0;
    // Both frame types in this subset are always coded error-resilient, which
    // determines the header bit layout (no reset_frame_context / refresh /
    // frame_parallel bits). Reject anything else before reading further.
    if !error_resilient {
        return Err(DecodeError::Unsupported("non-error-resilient stream"));
    }

    let (width, height);
    let mut refresh_frame_flags = 0xFFu8;
    let mut ref_frame_idx = [0u8; REFS_PER_FRAME];
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
            // A keyframe refreshes every reference slot (the `0xFF` default).
        }
        FrameType::Inter => {
            // show_frame == 1 ⇒ no intra_only bit; error_resilient ⇒ no
            // reset_frame_context bit. This encoder always shows inter frames.
            if !show_frame {
                return Err(DecodeError::Unsupported("hidden inter frame"));
            }
            let sizes = ref_sizes.ok_or(DecodeError::Corrupt(
                "inter frame decoded without reference sizes",
            ))?;
            refresh_frame_flags = rb.read_literal(8) as u8;
            for idx in ref_frame_idx.iter_mut() {
                *idx = rb.read_literal(REF_FRAMES_LOG2) as u8;
                let _ref_sign_bias = rb.read_bit();
            }
            // write_frame_size_with_refs: a `1` bit means "size taken from the
            // first reference"; this encoder always sets it. The size is inherited
            // from that reference's buffer, not coded in the header.
            if rb.read_bit() != 1 {
                return Err(DecodeError::Unsupported("inter frame codes its own size"));
            }
            let (w, h) = sizes[ref_frame_idx[0] as usize]
                .ok_or(DecodeError::Corrupt("inter frame references an empty slot"))?;
            width = w;
            height = h;
            if rb.read_bit() != 0 {
                return Err(DecodeError::Unsupported("render size != frame size"));
            }
            // This encoder always codes integer-pel MVs (allow_high_precision_mv
            // = 0). A foreign hp=1 stream reads its MV components with an extra
            // high-precision bit each, which would silently desync the boolean
            // reader from this decoder's fixed `usehp = false` MV reader — reject
            // it loudly at the subset boundary instead.
            if rb.read_bit() != 0 {
                return Err(DecodeError::Unsupported("high-precision MV"));
            }
            // interp filter: a `0` bit (not SWITCHABLE) then a 2-bit literal.
            if rb.read_bit() != 0 {
                return Err(DecodeError::Unsupported("switchable interp filter"));
            }
            let _interp_filter = rb.read_literal(2);
        }
    }

    // Trust boundary: reject oversized dimensions from the untrusted header BEFORE
    // the caller allocates any dimension-derived buffer. This transitively bounds
    // the reconstruction buffer and every context/grid vector. (Inter sizes come
    // from an already-validated reference, so this only ever triggers on a
    // keyframe, but the check is unconditional for defense in depth.)
    if width > MAX_DIM || height > MAX_DIM {
        return Err(DecodeError::TooLarge);
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
        refresh_frame_flags,
        ref_frame_idx,
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
