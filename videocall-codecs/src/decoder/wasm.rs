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

use super::{Decodable, DecodedFrame};
use crate::frame::FrameBuffer;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;
use web_sys::{console, Worker};

pub struct WasmDecoder {
    worker: Worker,
    // The closure that handles messages from the worker.
    // We must store it to keep it alive.
    _on_message_closure: Closure<dyn FnMut(JsValue)>,
}

impl Decodable for WasmDecoder {
    fn new(on_decoded_frame: Box<dyn Fn(DecodedFrame) + Send + Sync>) -> Self {
        // Create the worker.
        let worker = Worker::new("./worker.js").expect("Failed to create worker");

        // Create a closure to handle messages from the worker.
        let on_message_closure = Closure::new(move |event: JsValue| {
            match serde_wasm_bindgen::from_value::<DecodedFrame>(event) {
                Ok(frame) => on_decoded_frame(frame),
                Err(e) => console::error_1(&format!("Error deserializing frame: {:?}", e).into()),
            }
        });

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        WasmDecoder {
            worker,
            _on_message_closure: on_message_closure,
        }
    }

    fn decode(&self, frame: FrameBuffer) {
        match serde_wasm_bindgen::to_value(&frame) {
            Ok(js_frame) => {
                if let Err(e) = self.worker.post_message(&js_frame) {
                    console::error_1(&format!("Error posting message to worker: {:?}", e).into());
                }
            }
            Err(e) => {
                console::error_1(&format!("Error serializing frame: {:?}", e).into());
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
