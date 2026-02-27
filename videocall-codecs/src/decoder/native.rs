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

//! The native decoder implementation using `std::thread`.

use super::{Decodable, DecodedFrame};
use crate::frame::FrameBuffer;
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use vpx_sys::{
    vpx_codec_ctx_t, vpx_codec_dec_init_ver, vpx_codec_decode, vpx_codec_destroy,
    vpx_codec_get_frame, vpx_codec_vp9_dx, VPX_CODEC_OK, VPX_DECODER_ABI_VERSION,
};

// --- Vp9Decoder implementation, now living inside the native module ---

/// A VP9 decoder using libvpx.
struct Vp9Decoder {
    context: vpx_codec_ctx_t,
}

impl Vp9Decoder {
    fn new() -> Result<Self, String> {
        let mut context = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            vpx_codec_dec_init_ver(
                &mut context,
                vpx_codec_vp9_dx(),
                ptr::null_mut(),
                0,
                VPX_DECODER_ABI_VERSION as i32,
            )
        };
        if ret != VPX_CODEC_OK {
            return Err(format!("Failed to initialize VP9 decoder: {:?}", ret));
        }
        Ok(Self { context })
    }
}

impl Drop for Vp9Decoder {
    fn drop(&mut self) {
        unsafe {
            vpx_codec_destroy(&mut self.context);
        }
    }
}
// --- End Vp9Decoder implementation ---

// A wrapper to make the Vp9Decoder Send-able.
// This is safe because we are only ever accessing the decoder from a single thread.
struct SendableVp9Decoder(Vp9Decoder);
unsafe impl Send for SendableVp9Decoder {}

// A mock decoder that does nothing.
struct MockDecoder;
impl MockDecoder {
    fn new() -> Self {
        Self
    }
}

/// A trait for any decoder that can run on the internal thread.
trait ThreadDecodable: Send {
    fn decode_frame(&mut self, frame_buffer: &FrameBuffer) -> Result<Vec<DecodedFrame>, String>;
}

impl ThreadDecodable for SendableVp9Decoder {
    fn decode_frame(&mut self, frame_buffer: &FrameBuffer) -> Result<Vec<DecodedFrame>, String> {
        let mut decoded_frames = Vec::new();

        let ret = unsafe {
            vpx_codec_decode(
                &mut self.0.context,
                frame_buffer.frame.data.as_ptr(),
                frame_buffer.frame.data.len() as u32,
                ptr::null_mut(),
                0,
            )
        };
        if ret != VPX_CODEC_OK {
            let error_msg = unsafe {
                let error_cstr = vpx_sys::vpx_codec_err_to_string(ret);
                if error_cstr.is_null() {
                    "Unknown codec error".to_string()
                } else {
                    std::ffi::CStr::from_ptr(error_cstr)
                        .to_string_lossy()
                        .into_owned()
                }
            };
            return Err(format!("VPX Decode failed: {}", error_msg));
        }

        let mut iter = ptr::null_mut::<c_void>();
        loop {
            let img = unsafe {
                vpx_codec_get_frame(
                    &mut self.0.context,
                    &mut iter as *mut _ as *mut *const c_void,
                )
            };
            if img.is_null() {
                break;
            }

            let image_data = unsafe {
                let width = (*img).d_w as usize;
                let height = (*img).d_h as usize;

                // For I420 format, the U and V planes are half the width and height.
                let uv_width = width / 2;
                let uv_height = height / 2;

                // Total size = Y plane + U plane + V plane
                let mut buffer = Vec::with_capacity(width * height + 2 * uv_width * uv_height);

                // Copy Y plane
                copy_plane_to_buffer(
                    (*img).planes[0],
                    (*img).stride[0],
                    width,
                    height,
                    &mut buffer,
                );
                // Copy U plane
                copy_plane_to_buffer(
                    (*img).planes[1],
                    (*img).stride[1],
                    uv_width,
                    uv_height,
                    &mut buffer,
                );
                // Copy V plane
                copy_plane_to_buffer(
                    (*img).planes[2],
                    (*img).stride[2],
                    uv_width,
                    uv_height,
                    &mut buffer,
                );

                buffer
            };

            decoded_frames.push(DecodedFrame {
                sequence_number: frame_buffer.sequence_number(),
                width: 0,
                height: 0,
                data: image_data,
            });
        }
        Ok(decoded_frames)
    }
}

/// Helper to copy a plane from a vpx_image_t to a buffer, accounting for stride.
unsafe fn copy_plane_to_buffer(
    plane: *const u8,
    stride: i32,
    width: usize,
    height: usize,
    buffer: &mut Vec<u8>,
) {
    let mut current_ptr = plane;
    for _ in 0..height {
        buffer.extend_from_slice(std::slice::from_raw_parts(current_ptr, width));
        current_ptr = current_ptr.offset(stride as isize);
    }
}

impl ThreadDecodable for MockDecoder {
    fn decode_frame(&mut self, frame_buffer: &FrameBuffer) -> Result<Vec<DecodedFrame>, String> {
        println!(
            "[MOCK_DECODER] Pretending to decode frame {}",
            frame_buffer.sequence_number()
        );
        Ok(Vec::new())
    }
}

/// A message sent to the native decoder thread.
enum DecoderMessage {
    /// A frame to be decoded.
    Frame(FrameBuffer),
    /// A signal to shut down the thread.
    Shutdown,
}

pub struct NativeDecoder {
    thread_handle: Option<JoinHandle<()>>,
    sender: Sender<DecoderMessage>,
}

impl Decodable for NativeDecoder {
    /// The decoded frame type for native decoding.
    type Frame = DecodedFrame;

    fn new(
        codec: crate::decoder::VideoCodec,
        on_decoded_frame: Box<dyn Fn(Self::Frame) + Send + Sync>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();

        let thread_handle = Some(thread::spawn(move || {
            let mut decoder: Box<dyn ThreadDecodable> = match codec {
                crate::decoder::VideoCodec::Vp9Profile0Level10Bit8 => Box::new(SendableVp9Decoder(
                    Vp9Decoder::new().expect("Failed to create Vp9Decoder"),
                )),
                crate::decoder::VideoCodec::Vp8 => {
                    // VP8 uses the same libvpx decoder
                    Box::new(SendableVp9Decoder(
                        Vp9Decoder::new().expect("Failed to create Vp9Decoder"),
                    ))
                }
                crate::decoder::VideoCodec::Mock => Box::new(MockDecoder::new()),
                crate::decoder::VideoCodec::Unspecified => {
                    panic!("Cannot create decoder for unspecified codec")
                }
            };

            // This is the decoder thread loop.
            while let Ok(message) = receiver.recv() {
                match message {
                    DecoderMessage::Frame(frame_buffer) => {
                        println!(
                            "[DECODER_THREAD] Decoding frame {}",
                            frame_buffer.sequence_number()
                        );

                        match decoder.decode_frame(&frame_buffer) {
                            Ok(images) => {
                                for img in images {
                                    on_decoded_frame(img);
                                }
                            }
                            Err(e) => {
                                eprintln!("[DECODER_THREAD] Decode error: {}", e);
                            }
                        }
                    }
                    DecoderMessage::Shutdown => {
                        println!("[DECODER_THREAD] Shutting down.");
                        break;
                    }
                }
            }
        }));

        NativeDecoder {
            thread_handle,
            sender,
        }
    }

    fn decode(&self, frame: FrameBuffer) {
        if let Err(e) = self.sender.send(DecoderMessage::Frame(frame)) {
            eprintln!(
                "[NativeDecoder] Failed to send frame to decoder thread: {}",
                e
            );
        }
    }
}

impl Drop for NativeDecoder {
    fn drop(&mut self) {
        println!("[NativeDecoder] Dropping decoder. Signaling shutdown.");
        // Signal the thread to shut down.
        if self.sender.send(DecoderMessage::Shutdown).is_err() {
            eprintln!("[NativeDecoder] Decoder thread already shut down.");
        }

        // Wait for the thread to finish.
        if let Some(handle) = self.thread_handle.take() {
            handle.join().expect("Decoder thread failed to join");
        }
    }
}
