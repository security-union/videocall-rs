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

#![no_main]
/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Web worker decoder that handles both frame data and control messages using a JitterBuffer.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use videocall_codecs::decoder::{Decodable, DecodedFrame, VideoCodec};
use videocall_codecs::frame::{FrameBuffer, VideoFrame};
use videocall_codecs::jitter_buffer::JitterBuffer;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    console, DedicatedWorkerGlobalScope, EncodedVideoChunk, EncodedVideoChunkInit,
    EncodedVideoChunkType, VideoDecoder, VideoDecoderConfig, VideoDecoderInit,
    VideoFrame as WebVideoFrame,
};

/// Messages that can be sent to the web worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerMessage {
    /// Decode a frame using the jitter buffer
    DecodeFrame(FrameBuffer),
    /// Flush the jitter buffer and decoder
    Flush,
    /// Reset jitter buffer and decoder to initial state
    Reset,
}

/// WebDecoder implementation that wraps WebCodecs VideoDecoder
struct WebDecoder {
    decoder: RefCell<Option<VideoDecoder>>,
    self_scope: DedicatedWorkerGlobalScope,
}

// Safety: These are safe because we're in a single-threaded web worker context
unsafe impl Send for WebDecoder {}
unsafe impl Sync for WebDecoder {}

impl WebDecoder {
    fn new(self_scope: DedicatedWorkerGlobalScope) -> Self {
        Self {
            decoder: RefCell::new(None),
            self_scope,
        }
    }

    fn initialize_decoder(&self) -> Result<(), String> {
        let mut decoder_ref = self.decoder.borrow_mut();
        if decoder_ref.is_some() {
            return Ok(());
        }

        let self_scope = self.self_scope.clone();
        let on_output = {
            let global_scope = self_scope.clone();
            Closure::wrap(Box::new(move |video_frame: JsValue| {
                let video_frame = video_frame.dyn_into::<WebVideoFrame>().unwrap();
                // Post the VideoFrame back to the main thread
                if let Err(e) = global_scope.post_message(&video_frame) {
                    console::error_1(
                        &format!("[WORKER] Error posting decoded frame: {:?}", e).into(),
                    );
                }
                video_frame.close();
            }) as Box<dyn FnMut(_)>)
        };

        let on_error = Closure::wrap(Box::new(move |e: JsValue| {
            console::error_1(&"[WORKER] WebCodecs decoder error:".into());
            console::error_1(&e);
        }) as Box<dyn FnMut(_)>);

        let init = VideoDecoderInit::new(
            on_error.as_ref().unchecked_ref(),
            on_output.as_ref().unchecked_ref(),
        );

        let decoder =
            VideoDecoder::new(&init).map_err(|e| format!("Failed to create decoder: {:?}", e))?;
        let config = VideoDecoderConfig::new("vp09.00.10.08");
        decoder
            .configure(&config)
            .map_err(|e| format!("Failed to configure decoder: {:?}", e))?;

        on_output.forget();
        on_error.forget();

        *decoder_ref = Some(decoder);
        console::log_1(&"[WORKER] WebCodecs decoder initialized".into());
        Ok(())
    }

    /// Tear down the current decoder instance entirely, releasing all resources so that the next
    /// decode call will create a fresh `VideoDecoder`. This is required when the decoder enters an
    /// `InvalidStateError` that cannot be recovered from with `reset()`.
    fn destroy_decoder(&self) {
        // Acquire a mutable reference so we can replace the Option with `None`.
        let mut decoder_ref = self.decoder.borrow_mut();

        if let Some(decoder) = decoder_ref.take() {
            // Attempt to close the decoder. If it is already closed the call may return an
            // `InvalidStateError`; we simply log and continue.
            if let Err(e) = decoder.close() {
                console::error_1(
                    &format!("[WORKER] Failed to close decoder cleanly: {:?}", e).into(),
                );
            } else {
                console::log_1(&"[WORKER] Video decoder closed".into());
            }

            console::log_1(&"[WORKER] Video decoder destroyed".into());
        }
    }

    /// High-level helper that tears down the decoder and schedules a jitter-buffer reset on the
    /// next event-loop tick. This avoids nested borrows and ensures we always start clean, waiting
    /// for a keyframe.
    fn reset_pipeline(&self) {
        // First, drop the current decoder instance (if any)
        self.destroy_decoder();

        // Schedule jitter-buffer reset asynchronously to avoid borrow conflicts with whatever
        // call stack triggered this reset.
        let self_scope = self.self_scope.clone();

        let cb = Closure::once_into_js(move || {
            reset_jitter_buffer();
        });

        // Ignore errors from setTimeout – if scheduling fails we'll try again on next frame.
        let _ = self_scope
            .set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 0);
        // `cb` moved into JS runtime, no need to forget.
    }
}

impl Decodable for WebDecoder {
    type Frame = DecodedFrame;

    fn new(_codec: VideoCodec, _on_decoded_frame: Box<dyn Fn(Self::Frame) + Send + Sync>) -> Self {
        // This is not used in the worker context, decoder is created manually
        panic!("Use WebDecoder::new(self_scope) in worker context");
    }

    fn decode(&self, frame: FrameBuffer) {
        // Initialize decoder if needed
        if self.decoder.borrow().is_none() {
            if let Err(e) = self.initialize_decoder() {
                console::error_1(&format!("[WORKER] Failed to initialize decoder: {:?}", e).into());
                return;
            }
        }

        let decoder_ref = self.decoder.borrow();
        if let Some(decoder) = decoder_ref.as_ref() {
            let chunk_type = match frame.frame.frame_type {
                videocall_codecs::frame::FrameType::KeyFrame => EncodedVideoChunkType::Key,
                videocall_codecs::frame::FrameType::DeltaFrame => EncodedVideoChunkType::Delta,
            };

            let data = js_sys::Uint8Array::from(frame.frame.data.as_slice());
            let init = EncodedVideoChunkInit::new(&data.into(), frame.frame.timestamp, chunk_type);

            match EncodedVideoChunk::new(&init) {
                Ok(chunk) => {
                    if let Err(e) = decoder.decode(&chunk) {
                        console::error_1(&format!("[WORKER] Decoder error: {:?}", e).into());

                        // Release the immutable borrow so we can safely mutate within
                        // `reset_pipeline()`.
                        drop(decoder_ref);

                        // Completely reset decoder + jitter buffer in a single abstraction.
                        self.reset_pipeline();
                    }
                }
                Err(e) => {
                    console::error_1(&format!("[WORKER] Failed to create chunk: {:?}", e).into());
                }
            }
        }
    }
}

// Thread-local storage for the jitter buffer and related state
thread_local! {
    static JITTER_BUFFER: RefCell<Option<JitterBuffer<DecodedFrame>>> = const { RefCell::new(None) };
    static INTERVAL_ID: RefCell<Option<i32>> = const { RefCell::new(None) };
}

const JITTER_BUFFER_CHECK_INTERVAL_MS: i32 = 10; // Check every 10ms for frames ready to decode

#[wasm_bindgen(start)]
pub fn main() {
    // Set up panic hook to get Rust panics in the console
    console_error_panic_hook::set_once();
    // Initialize Rust log to console logging
    log::set_max_level(log::LevelFilter::Debug);
    log::info!("Starting worker decoder with jitter buffer and message handling");

    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let on_message = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
        match serde_wasm_bindgen::from_value::<WorkerMessage>(event.data()) {
            Ok(message) => handle_worker_message(message),
            Err(e) => {
                console::error_1(
                    &format!("[WORKER] Failed to deserialize message: {:?}", e).into(),
                );
            }
        }
    }) as Box<dyn FnMut(_)>);

    self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    // Start the jitter buffer check interval
    start_jitter_buffer_interval();
}

fn handle_worker_message(message: WorkerMessage) {
    match message {
        WorkerMessage::DecodeFrame(frame) => {
            insert_frame_to_jitter_buffer(frame);
        }
        WorkerMessage::Flush => {
            console::log_1(&"[WORKER] Flushing jitter buffer and decoder".into());
            flush_jitter_buffer();
        }
        WorkerMessage::Reset => {
            console::log_1(&"[WORKER] Resetting jitter buffer and decoder state".into());
            reset_jitter_buffer();
        }
    }
}

fn insert_frame_to_jitter_buffer(frame: FrameBuffer) {
    JITTER_BUFFER.with(|jb_cell| {
        let mut jb_opt = jb_cell.borrow_mut();

        if jb_opt.is_none() {
            match initialize_jitter_buffer() {
                Ok(jb) => *jb_opt = Some(jb),
                Err(e) => {
                    console::error_1(
                        &format!("[WORKER] Failed to initialize jitter buffer: {:?}", e).into(),
                    );
                    return;
                }
            }
        }

        if let Some(jb) = jb_opt.as_mut() {
            // Convert FrameBuffer to VideoFrame
            let video_frame = VideoFrame {
                sequence_number: frame.sequence_number(),
                frame_type: frame.frame.frame_type,
                data: frame.frame.data.clone(),
                timestamp: frame.frame.timestamp,
            };

            // Get current time in milliseconds
            let current_time_ms = js_sys::Date::now() as u128;
            jb.insert_frame(video_frame, current_time_ms);
        }
    });
}

fn start_jitter_buffer_interval() {
    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let interval_callback = Closure::wrap(Box::new(move || {
        check_jitter_buffer_for_ready_frames();
    }) as Box<dyn FnMut()>);

    let interval_id = self_scope
        .set_interval_with_callback_and_timeout_and_arguments_0(
            interval_callback.as_ref().unchecked_ref(),
            JITTER_BUFFER_CHECK_INTERVAL_MS,
        )
        .expect("Failed to set interval");

    interval_callback.forget();

    INTERVAL_ID.with(|id_cell| {
        *id_cell.borrow_mut() = Some(interval_id);
    });

    console::log_1(
        &format!(
            "[WORKER] Started jitter buffer check interval ({}ms)",
            JITTER_BUFFER_CHECK_INTERVAL_MS
        )
        .into(),
    );
}

fn check_jitter_buffer_for_ready_frames() {
    JITTER_BUFFER.with(|jb_cell| {
        let mut jb_opt = jb_cell.borrow_mut();
        if let Some(jb) = jb_opt.as_mut() {
            let current_time_ms = js_sys::Date::now() as u128;
            jb.find_and_move_continuous_frames(current_time_ms);
        }
    });
}

fn initialize_jitter_buffer() -> Result<JitterBuffer<DecodedFrame>, String> {
    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let web_decoder = WebDecoder::new(self_scope);
    let boxed_decoder = Box::new(web_decoder);

    console::log_1(&"[WORKER] Initializing jitter buffer with WebCodecs decoder".into());
    Ok(JitterBuffer::new(boxed_decoder))
}

fn flush_jitter_buffer() {
    JITTER_BUFFER.with(|jb_cell| {
        let mut jb_opt = jb_cell.borrow_mut();
        if let Some(jb) = jb_opt.as_mut() {
            jb.flush();
            console::log_1(&"[WORKER] Jitter buffer flushed".into());
        } else {
            console::log_1(&"[WORKER] No jitter buffer to flush".into());
        }
    });
}

fn reset_jitter_buffer() {
    JITTER_BUFFER.with(|jb_cell| {
        *jb_cell.borrow_mut() = None;
    });
    console::log_1(&"[WORKER] Jitter buffer reset to initial state".into());
}
