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

//! Simple web worker decoder that directly processes frames without complex jitter buffer for now.

use console_error_panic_hook;
use std::cell::RefCell;
use videocall_codecs::frame::FrameBuffer;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    console, DedicatedWorkerGlobalScope, EncodedVideoChunk, EncodedVideoChunkInit,
    EncodedVideoChunkType, VideoDecoder, VideoDecoderConfig, VideoDecoderInit, VideoFrame,
};

// Thread-local storage for the decoder
thread_local! {
    static DECODER: RefCell<Option<VideoDecoder>> = const { RefCell::new(None) };
    static WAITING_FOR_KEY: RefCell<bool> = const { RefCell::new(true) };
}

#[wasm_bindgen(start)]
pub fn main() {
    // Set up panic hook to get Rust panics in the console
    console_error_panic_hook::set_once();
    // Initialize Rust log to console logging
    log::set_max_level(log::LevelFilter::Debug);
    log::info!("Starting simple worker decoder");

    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let on_message = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
        let frame: FrameBuffer = serde_wasm_bindgen::from_value(event.data()).unwrap();
        decode_frame_direct(frame);
    }) as Box<dyn FnMut(_)>);

    self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
}

fn decode_frame_direct(frame: FrameBuffer) {
    DECODER.with(|decoder_cell| {
        let mut decoder_opt = decoder_cell.borrow_mut();
        if decoder_opt.is_none() {
            console::log_1(&"[WORKER] Initializing decoder".into());
            match initialize_decoder() {
                Ok(decoder) => *decoder_opt = Some(decoder),
                Err(e) => {
                    console::error_1(
                        &format!("[WORKER] Failed to initialize decoder: {:?}", e).into(),
                    );
                    return;
                }
            }
        }

        WAITING_FOR_KEY.with(|waiting_cell| {
            let mut waiting_for_key = waiting_cell.borrow_mut();

            let chunk_type = match frame.frame.frame_type {
                videocall_codecs::frame::FrameType::KeyFrame => EncodedVideoChunkType::Key,
                videocall_codecs::frame::FrameType::DeltaFrame => EncodedVideoChunkType::Delta,
            };

            if *waiting_for_key {
                if chunk_type == EncodedVideoChunkType::Key {
                    *waiting_for_key = false;
                } else {
                    console::log_1(&"[WORKER] Waiting for key frame, dropping delta.".into());
                    return;
                }
            }

            let data = js_sys::Uint8Array::from(frame.frame.data.as_slice());
            let init = EncodedVideoChunkInit::new(&data.into(), frame.frame.timestamp, chunk_type);

            match EncodedVideoChunk::new(&init) {
                Ok(chunk) => {
                    // Get a fresh reference to the decoder inside the closure to avoid borrowing conflicts
                    if let Some(decoder) = decoder_opt.as_ref() {
                        if let Err(e) = decoder.decode(&chunk) {
                            console::log_1(
                                &format!("[WORKER] Decoder error: {:?}. Resetting.", e).into(),
                            );
                            *decoder_opt = None;
                            *waiting_for_key = true;
                        } else {
                            console::log_1(
                                &format!("[WORKER] Decoded frame: {}", frame.sequence_number())
                                    .into(),
                            );
                        }
                    }
                }
                Err(e) => {
                    console::error_1(&format!("[WORKER] Failed to create chunk: {:?}", e).into());
                }
            }
        });
    });
}

fn initialize_decoder() -> Result<VideoDecoder, String> {
    log::info!("Initializing decoder");
    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let on_output = {
        let global_scope = self_scope.clone();
        Closure::wrap(Box::new(move |video_frame: JsValue| {
            let video_frame = video_frame.dyn_into::<VideoFrame>().unwrap();

            // Post the VideoFrame back to the main thread
            if let Err(e) = global_scope.post_message(&video_frame) {
                console::error_1(&format!("[WORKER] Error posting decoded frame: {:?}", e).into());
            }
            video_frame.close();
        }) as Box<dyn FnMut(_)>)
    };

    let on_error = Closure::wrap(Box::new(move |e: JsValue| {
        console::error_1(&"[WORKER] Decoder error:".into());
        console::error_1(&e);
        DECODER.with(|decoder_cell| {
            *decoder_cell.borrow_mut() = None;
        });
        WAITING_FOR_KEY.with(|waiting_cell| {
            *waiting_cell.borrow_mut() = true;
        });
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

    Ok(decoder)
}
