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

use js_sys::Uint8Array;
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
}

#[wasm_bindgen]
pub fn main() {
    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let on_message = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
        let frame: FrameBuffer = serde_wasm_bindgen::from_value(event.data()).unwrap();

        DECODER.with(|decoder_cell| {
            let mut decoder = decoder_cell.borrow_mut();
            if decoder.is_none() {
                let decoder_private =
                    initialize_decoder().expect("[WORKER] Failed to initialize decoder");
                *decoder = Some(decoder_private);
            }
            let decoder = decoder.as_ref().unwrap();

            let chunk_type = match frame.frame.frame_type {
                FrameType::KeyFrame => EncodedVideoChunkType::Key,
                FrameType::DeltaFrame => EncodedVideoChunkType::Delta,
            };

            let data = js_sys::Uint8Array::from(frame.frame.data.as_slice());
            let init = EncodedVideoChunkInit::new(
                &data.into(),
                frame.frame.sequence_number as f64,
                chunk_type,
            );

            let chunk = EncodedVideoChunk::new(&init).unwrap();
            if let Err(e) = decoder.decode(&chunk) {
                log::info!("[WORKER] Decoder error: {:?}", e);
            }
        });
    }) as Box<dyn FnMut(_)>);

    self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
}

fn initialize_decoder() -> anyhow::Result<VideoDecoder> {
    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let on_output = Closure::wrap(Box::new(move |video_frame: JsValue| {
        let video_frame = video_frame.dyn_into::<VideoFrame>().unwrap();
        let self_scope_clone = self_scope.clone();

        let future = async move {
            let frame_data = copy_video_frame_data(&video_frame).await.unwrap();
            let decoded_frame = DecodedFrame {
                sequence_number: video_frame.timestamp().unwrap_or(0.0) as u64,
                width: video_frame.coded_width(),
                height: video_frame.coded_height(),
                data: frame_data,
            };
            let js_decoded = serde_wasm_bindgen::to_value(&decoded_frame).unwrap();
            self_scope_clone.post_message(&js_decoded).unwrap();
        };
        wasm_bindgen_futures::spawn_local(future);
    }) as Box<dyn FnMut(_)>);

    let on_error = Closure::wrap(Box::new(move |e: JsValue| {
        console::error_1(&"[WORKER] Decoder error:".into());
        console::error_1(&e);
    }) as Box<dyn FnMut(_)>);

    let init = VideoDecoderInit::new(
        on_output.as_ref().unchecked_ref(),
        on_error.as_ref().unchecked_ref(),
    );

    let decoder = VideoDecoder::new(&init).unwrap();
    let config = VideoDecoderConfig::new("vp9");
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

pub fn buffer_to_uint8array(buf: &mut [u8]) -> Uint8Array {
    // Convert &mut [u8] to a Uint8Array
    unsafe { Uint8Array::view_mut_raw(buf.as_mut_ptr(), buf.len()) }
}
