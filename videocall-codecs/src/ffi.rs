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

//! UniFFI Swift/Kotlin bindings for the pure-Rust VP9 codec.
//!
//! This is a thin, native-only wrapper (zero libvpx, zero C) exposing the
//! from-scratch VP9 encoder ([`crate::vp9::Vp9Encoder`]) and stateful decoder
//! ([`crate::vp9::dec::Vp9Decoder`]) to Swift/Kotlin via UniFFI. It is compiled
//! only with the `uniffi` cargo feature and never on `wasm32`.
//!
//! ## Thread-safety model
//!
//! UniFFI objects are shared as `Arc<T>` and therefore must be `Send + Sync`.
//!
//! - The **encoder** is `Send`, so it is guarded by a plain [`Mutex`]; every
//!   `encode`/`update_bitrate` call is serialized, which matches the encoder's
//!   inherently sequential (reference-carrying) nature.
//! - The **decoder**'s reference-buffer slots are `Rc<FrameBuffer>` (chosen for
//!   the single-threaded wasm decode path), which makes [`crate::vp9::dec::Vp9Decoder`]
//!   `!Send`. Rather than change the decoder, we confine it to a dedicated worker
//!   thread and drive it over channels: the FFI object holds only `Send + Sync`
//!   channel handles, and the `Rc` never crosses a thread boundary. A [`Mutex`]
//!   around the sender serializes calls and preserves decode order (essential —
//!   inter frames depend on the previously decoded reference).
//!
//! No path panics across the FFI boundary: every fallible operation surfaces a
//! thrown [`CodecError`].

use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::encoder::{Encodable, EncoderConfig};

/// An error crossing the Swift/Kotlin boundary. Every fallible codec operation
/// maps its failure onto one of these variants instead of panicking.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CodecError {
    /// The encoder/decoder was constructed with invalid parameters (e.g. odd or
    /// zero frame dimensions).
    #[error("invalid configuration: {message}")]
    InvalidConfig { message: String },
    /// Encoding a frame failed.
    #[error("encode failed: {message}")]
    Encode { message: String },
    /// Decoding a frame failed (truncated, corrupt, or out-of-subset stream).
    #[error("decode failed: {message}")]
    Decode { message: String },
    /// The internal worker/lock was lost (e.g. a poisoned mutex after a panic).
    #[error("codec worker unavailable: {message}")]
    Internal { message: String },
}

/// A decoded picture: tightly-packed 8-bit I420 (`data`) at the given cropped
/// dimensions. `data.len()` == `width*height + 2*ceil(width/2)*ceil(height/2)`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DecodedFrame {
    /// Planar I420 pixels (Y plane, then U, then V), tightly packed.
    pub data: Vec<u8>,
    /// Cropped frame width in pixels.
    pub width: u32,
    /// Cropped frame height in pixels.
    pub height: u32,
}

/// Pure-Rust VP9 encoder handle for Swift/Kotlin.
///
/// Accepts full 8-bit I420 frames and returns the compressed VP9 frame bytes.
/// The first frame (and every `keyframe_interval`-th frame) is a keyframe.
#[derive(uniffi::Object)]
pub struct Vp9Encoder {
    inner: Mutex<crate::vp9::Vp9Encoder>,
}

#[uniffi::export]
impl Vp9Encoder {
    /// Create an encoder for `width`x`height` I420 frames.
    ///
    /// `width` and `height` must be non-zero and even. `min_quantizer` /
    /// `max_quantizer` are in VP9's 0-63 quality window (lower = better). Throws
    /// [`CodecError::InvalidConfig`] on invalid dimensions.
    #[uniffi::constructor]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        keyframe_interval: u32,
        min_quantizer: u32,
        max_quantizer: u32,
        cpu_used: u8,
    ) -> Result<Arc<Self>, CodecError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(CodecError::InvalidConfig {
                message: format!("width/height must be non-zero and even, got {width}x{height}"),
            });
        }
        let config = EncoderConfig {
            width,
            height,
            framerate: fps,
            bitrate_kbps,
            keyframe_interval,
            min_quantizer,
            max_quantizer,
            cpu_used,
        };
        let enc = crate::vp9::Vp9Encoder::new(config).map_err(|e| CodecError::InvalidConfig {
            message: e.to_string(),
        })?;
        Ok(Arc::new(Self {
            inner: Mutex::new(enc),
        }))
    }

    /// Encode one I420 frame at presentation timestamp `pts`.
    ///
    /// Returns the compressed VP9 frame bytes, or `None` if the encoder buffered
    /// the frame and produced no output this call. Throws [`CodecError::Encode`]
    /// on a bad input buffer.
    pub fn encode(&self, pts: i64, i420: Vec<u8>) -> Result<Option<Vec<u8>>, CodecError> {
        let mut enc = self.inner.lock().map_err(|_| CodecError::Internal {
            message: "encoder mutex poisoned".to_string(),
        })?;
        let frame = enc.encode(pts, &i420).map_err(|e| CodecError::Encode {
            message: e.to_string(),
        })?;
        Ok(frame.map(|f| f.data))
    }

    /// Update the target bitrate (kbps) at runtime.
    pub fn update_bitrate(&self, kbps: u32) -> Result<(), CodecError> {
        let mut enc = self.inner.lock().map_err(|_| CodecError::Internal {
            message: "encoder mutex poisoned".to_string(),
        })?;
        enc.update_bitrate_kbps(kbps)
            .map_err(|e| CodecError::Encode {
                message: e.to_string(),
            })
    }
}

/// A unit of work handed to the decoder worker thread: the compressed frame plus
/// a rendezvous channel to return the result on.
type DecodeJob = (Vec<u8>, SyncSender<Result<DecodedFrame, CodecError>>);

/// Pure-Rust VP9 decoder handle for Swift/Kotlin.
///
/// Stateful: feed a keyframe first, then subsequent inter frames in order. The
/// underlying [`crate::vp9::dec::Vp9Decoder`] is `!Send` (its reference slots are
/// `Rc`), so it lives on a dedicated worker thread; this handle only forwards
/// frames to it and reads back results.
#[derive(uniffi::Object)]
pub struct Vp9Decoder {
    /// Sends frames to the worker. The [`Mutex`] serializes calls so frames are
    /// decoded in submission order (inter frames depend on prior references).
    sender: Mutex<Sender<DecodeJob>>,
    /// Owns the worker thread; joined-on-drop is unnecessary — dropping `sender`
    /// ends the worker's receive loop. Kept alive for the handle's lifetime.
    _worker: JoinHandle<()>,
}

#[uniffi::export]
impl Vp9Decoder {
    /// Create a decoder with no reference history. The first decoded frame must
    /// be a keyframe.
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        let (tx, rx) = channel::<DecodeJob>();
        let worker = std::thread::spawn(move || {
            // The `Rc`-bearing decoder is created and used only on this thread.
            let mut decoder = crate::vp9::dec::Vp9Decoder::new();
            while let Ok((bytes, respond)) = rx.recv() {
                let result = decoder
                    .decode_frame(&bytes)
                    .map(|fb| DecodedFrame {
                        data: fb.export_i420(),
                        width: fb.crop_width,
                        height: fb.crop_height,
                    })
                    .map_err(|e| CodecError::Decode {
                        message: e.to_string(),
                    });
                // If the caller gave up (channel closed), just drop the result.
                let _ = respond.send(result);
            }
        });
        Arc::new(Self {
            sender: Mutex::new(tx),
            _worker: worker,
        })
    }

    /// Decode one compressed VP9 frame into an I420 [`DecodedFrame`].
    ///
    /// Throws [`CodecError::Decode`] for a truncated, corrupt, or out-of-subset
    /// stream (e.g. an inter frame before any keyframe).
    pub fn decode(&self, frame: Vec<u8>) -> Result<DecodedFrame, CodecError> {
        // Rendezvous channel (capacity 0): the worker blocks until we receive.
        let (respond_tx, respond_rx) = sync_channel::<Result<DecodedFrame, CodecError>>(0);
        // Hold the lock across send+recv so concurrent callers are fully
        // serialized and frames decode in submission order.
        let sender = self.sender.lock().map_err(|_| CodecError::Internal {
            message: "decoder mutex poisoned".to_string(),
        })?;
        sender
            .send((frame, respond_tx))
            .map_err(|_| CodecError::Internal {
                message: "decoder worker thread is gone".to_string(),
            })?;
        respond_rx.recv().map_err(|_| CodecError::Internal {
            message: "decoder worker dropped the response".to_string(),
        })?
    }
}
