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

//! The entry point and main loop for the Web Worker.

use console_error_panic_hook;
use std::cell::RefCell;
use videocall_codecs::{decoder::DecodedFrame, frame::FrameBuffer, frame::FrameType};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    console, DedicatedWorkerGlobalScope, EncodedVideoChunk, EncodedVideoChunkInit,
    EncodedVideoChunkType, VideoDecoder, VideoDecoderConfig, VideoDecoderInit, VideoFrame,
};

// Use a thread-local static RefCell to hold the decoder.
// This is a common pattern for managing state in a WASM worker.
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
    log::info!("Starting worker");
    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    // Global error handler
    let on_global_error = Closure::wrap(Box::new(move |err: JsValue| {
        console::error_1(&"[WORKER] Uncaught error:".into());
        console::error_1(&err);
    }) as Box<dyn FnMut(JsValue)>);
    self_scope.set_onerror(Some(on_global_error.as_ref().unchecked_ref()));
    on_global_error.forget();

    let on_message = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
        let frame: FrameBuffer = serde_wasm_bindgen::from_value(event.data()).unwrap();

        WAITING_FOR_KEY.with(|waiting_cell| {
            let mut waiting_for_key = waiting_cell.borrow_mut();

            DECODER.with(|decoder_cell| {
                let mut decoder_opt = decoder_cell.borrow_mut();
                if decoder_opt.is_none() {
                    console::log_1(&"[WORKER] Initializing decoder".into());
                    let decoder_private =
                        initialize_decoder().expect("[WORKER] Failed to initialize decoder");
                    *decoder_opt = Some(decoder_private);
                }
                let decoder = decoder_opt.as_ref().unwrap();

                let chunk_type = match frame.frame.frame_type {
                    FrameType::KeyFrame => EncodedVideoChunkType::Key,
                    FrameType::DeltaFrame => EncodedVideoChunkType::Delta,
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
                let init =
                    EncodedVideoChunkInit::new(&data.into(), frame.frame.timestamp, chunk_type);

                let chunk = EncodedVideoChunk::new(&init).unwrap();
                if let Err(e) = decoder.decode(&chunk) {
                    log::info!("[WORKER] Decoder error: {:?}. Resetting.", e);
                    *decoder_opt = None;
                    *waiting_for_key = true;
                }
            });
        });
    }) as Box<dyn FnMut(_)>);

    self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
}

fn initialize_decoder() -> anyhow::Result<VideoDecoder> {
    log::info!("Initializing decoder");
    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let on_output = Closure::wrap(Box::new(move |video_frame: JsValue| {
        let video_frame = video_frame.dyn_into::<VideoFrame>().unwrap();
        self_scope.post_message(&video_frame).unwrap();
        video_frame.close();
    }) as Box<dyn FnMut(_)>);

    let on_error = Closure::wrap(Box::new(move |e: JsValue| {
        console::error_1(&"[WORKER] on error Decoder error:".into());
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

    let decoder = VideoDecoder::new(&init).unwrap();
    let config = VideoDecoderConfig::new("vp09.00.10.08");
    if let Err(e) = decoder.configure(&config) {
        return Err(anyhow::anyhow!(
            "[WORKER] Failed to configure decoder: {:?}",
            e
        ));
    };

    on_output.forget();
    on_error.forget();

    Ok(decoder)
}

async fn copy_video_frame_data(video_frame: &VideoFrame) -> Result<Vec<u8>, JsValue> {
    let size = video_frame.allocation_size()? as usize;
    let mut buffer = vec![0; size];
    let promise = video_frame.copy_to_with_u8_array(&buffer_to_uint8array(&mut buffer));
    JsFuture::from(promise).await?;
    Ok(buffer)
}

pub fn buffer_to_uint8array(buf: &mut [u8]) -> js_sys::Uint8Array {
    // Convert &mut [u8] to a Uint8Array
    unsafe { js_sys::Uint8Array::view_mut_raw(buf.as_mut_ptr(), buf.len()) }
}
