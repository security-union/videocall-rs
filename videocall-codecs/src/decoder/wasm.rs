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

//! The WASM decoder implementation using a Web Worker and WebCodecs.

use super::Decodable;
use crate::frame::FrameBuffer;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{console, VideoFrame, Worker};

unsafe impl Send for WasmDecoder {}
unsafe impl Sync for WasmDecoder {}

pub struct WasmDecoder {
    worker: Worker,
    // The closure that handles messages from the worker.
    // We must store it to keep it alive.
    _on_message_closure: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

impl Decodable for WasmDecoder {
    /// The decoded frame type for WASM decoding (a JS VideoFrame).
    type Frame = VideoFrame;

    fn new(
        _codec: crate::decoder::VideoCodec,
        on_decoded_frame: Box<dyn Fn(Self::Frame) + Send + Sync>,
    ) -> Self {
        log::info!("Creating WASM decoder");
        // Find the worker script URL from the link tag added by Trunk.
        let worker_url = web_sys::window()
            .expect("no window")
            .document()
            .expect("no document")
            .get_element_by_id("codecs-worker")
            .expect("worker link tag with id 'codecs-worker' not found")
            .get_attribute("href")
            .expect("worker link tag has no href attribute");

        // Create the worker.
        let worker = Worker::new(&worker_url).expect("Failed to create worker");

        // Create a closure to handle messages from the worker.
        let on_message_closure = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
            log::info!("[MAIN] Received message");
            // event.data() is a transferred VideoFrame
            let js_val = event.data();
            let video_frame: VideoFrame = js_val.dyn_into().expect("Expected VideoFrame");
            on_decoded_frame(video_frame);
        }) as Box<dyn FnMut(_)>);

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        WasmDecoder {
            worker,
            _on_message_closure: on_message_closure,
        }
    }

    fn decode(&self, frame: FrameBuffer) {
        // log::info!("Decoding frame");
        match serde_wasm_bindgen::to_value(&frame) {
            Ok(js_frame) => {
                // log::info!("Posting message to worker");
                if let Err(e) = self.worker.post_message(&js_frame) {
                    log::error!("Error posting message to worker: {:?}", e);
                }
            }
            Err(e) => {
                log::error!("Error serializing frame: {:?}", e);
            }
        }
    }
}

impl Drop for WasmDecoder {
    fn drop(&mut self) {
        console::log_1(&"Terminating worker".into());
        self.worker.terminate();
    }
}
