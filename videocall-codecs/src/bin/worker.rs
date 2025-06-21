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

use std::cell::RefCell;
use video_decoder::{frame::FrameBuffer, libvpx_decoder::DecodedFrame};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;
use web_sys::{
    console, EncodedVideoChunk, EncodedVideoChunkInit, VideoDecoder, VideoDecoderConfig,
    VideoDecoderInit, VideoFrame,
};

// Thread-local static variables to hold the state within the worker.
thread_local! {
    // The WebCodecs decoder.
    static DECODER: RefCell<Option<VideoDecoder>> = RefCell::new(None);
}

/// The main entry point for the worker, called from JavaScript.
#[wasm_bindgen]
pub fn worker_entry(js_frame: JsValue) {
    // Deserialize the frame from the main thread.
    let frame: FrameBuffer = match serde_wasm_bindgen::from_value(js_frame) {
        Ok(frame) => frame,
        Err(e) => {
            console::error_1(&format!("[WORKER] Failed to deserialize frame: {:?}", e).into());
            return;
        }
    };

    // If the decoder isn't initialized, do it now.
    DECODER.with(|decoder_cell| {
        let mut decoder_opt = decoder_cell.borrow_mut();
        if decoder_opt.is_none() {
            *decoder_opt = Some(initialize_decoder());
        }

        // Now we know the decoder exists.
        if let Some(decoder) = decoder_opt.as_ref() {
            let chunk = EncodedVideoChunk::new(&EncodedVideoChunkInit::new(
                &frame.frame.data,
                frame.frame.sequence_number as f64, // timestamp
                frame.frame.frame_type == video_decoder::frame::FrameType::KeyFrame,
            ))
            .expect("Failed to create EncodedVideoChunk");

            decoder.decode(&chunk);
        }
    });
}

fn initialize_decoder() -> VideoDecoder {
    // Define the output handler for the decoder.
    let on_output = Closure::wrap(Box::new(move |chunk: JsValue| {
        let video_frame = VideoFrame::from(chunk);
        let decoded_frame = DecodedFrame {
            sequence_number: video_frame.timestamp() as u64,
            // In a real scenario, you'd copy the YUV data out.
            // For this simulation, we'll just send back an empty Vec.
            data: Vec::new(),
        };

        if let Ok(js_decoded) = serde_wasm_bindgen::to_value(&decoded_frame) {
            // Post the decoded frame back to the main thread.
            js_sys::global()
                .dyn_into::<web_sys::WorkerGlobalScope>()
                .unwrap()
                .post_message(&js_decoded)
                .unwrap();
        }
    }) as Box<dyn FnMut(JsValue)>);

    // Define the error handler.
    let on_error = Closure::wrap(Box::new(move |e: JsValue| {
        console::error_1(&"[WORKER] Decoder error:".into());
        console::error_1(&e);
    }) as Box<dyn FnMut(JsValue)>);

    let config = VideoDecoderConfig::new(
        "vp8",
        on_output.as_ref().unchecked_ref(),
        on_error.as_ref().unchecked_ref(),
    );

    // The closures must be forgotten to keep them alive.
    on_output.forget();
    on_error.forget();

    VideoDecoder::new(&config).expect("Failed to create VideoDecoder")
}
