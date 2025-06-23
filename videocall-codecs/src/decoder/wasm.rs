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

//! The WASM decoder implementation using a Web Worker with internal JitterBuffer.

use super::{Decodable, DecodedFrame};
use crate::frame::FrameBuffer;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{console, window, VideoFrame, Worker};

unsafe impl Send for WasmDecoder {}
unsafe impl Sync for WasmDecoder {}

pub struct WasmDecoder {
    worker: Worker,
    // The closure that handles messages from the worker.
    // We must store it to keep it alive.
    _on_message_closure: Closure<dyn FnMut(web_sys::MessageEvent)>,
    // Store the user's callback
    on_decoded_frame: Box<dyn Fn(DecodedFrame)>,
}

impl Decodable for WasmDecoder {
    /// The decoded frame type for WASM decoding (now consistent with native).
    type Frame = DecodedFrame;

    fn new(
        _codec: crate::decoder::VideoCodec,
        on_decoded_frame: Box<dyn Fn(Self::Frame) + Send + Sync>,
    ) -> Self {
        log::info!("Creating WASM decoder with internal jitter buffer");
        // Find the worker script URL from the link tag added by Trunk.
        let worker_url = window()
            .expect("no window")
            .document()
            .expect("no document")
            .get_element_by_id("codecs-worker")
            .expect("worker link tag with id 'codecs-worker' not found")
            .get_attribute("href")
            .expect("worker link tag has no href attribute");

        // Create the worker.
        let worker = Worker::new(&worker_url).expect("Failed to create worker");

        // Convert the Send + Sync callback to a non-Send one for WASM
        let callback: Box<dyn Fn(DecodedFrame)> = unsafe { std::mem::transmute(on_decoded_frame) };

        // Create a closure to handle messages from the worker.
        let on_message_closure = {
            let callback = callback.clone();
            Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
                let js_val = event.data();

                // Try to convert to VideoFrame (the actual decoded frame)
                if let Ok(video_frame) = js_val.dyn_into::<VideoFrame>() {
                    // Convert VideoFrame to DecodedFrame for consistency
                    let decoded_frame = DecodedFrame {
                        sequence_number: 0, // Note: sequence number tracking happens in jitter buffer
                        width: video_frame.display_width(),
                        height: video_frame.display_height(),
                        data: vec![], // For now, we don't copy the actual video data
                    };

                    callback(decoded_frame);
                    video_frame.close();
                } else {
                    log::warn!("Received unexpected message from worker: {:?}", js_val);
                }
            }) as Box<dyn FnMut(_)>)
        };

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        WasmDecoder {
            worker,
            _on_message_closure: on_message_closure,
            on_decoded_frame: callback,
        }
    }

    fn decode(&self, frame: FrameBuffer) {
        self.push_frame(frame);
    }
}

impl WasmDecoder {
    /// Create a WasmDecoder with VideoFrame callback for direct canvas rendering
    pub fn new_with_video_frame_callback(
        _codec: crate::decoder::VideoCodec,
        on_video_frame: Box<dyn Fn(VideoFrame)>,
    ) -> Self {
        log::info!("Creating WASM decoder with VideoFrame callback");
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
        let on_message_closure = {
            let callback = on_video_frame;
            Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
                let js_val = event.data();

                // Try to convert to VideoFrame (the actual decoded frame)
                if let Ok(video_frame) = js_val.dyn_into::<VideoFrame>() {
                    callback(video_frame);
                } else {
                    log::warn!("Received unexpected message from worker: {:?}", js_val);
                }
            }) as Box<dyn FnMut(_)>)
        };

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        // Create a dummy DecodedFrame callback since we need it for the trait
        let dummy_callback = Box::new(|_: DecodedFrame| {
            // This won't be called when using VideoFrame callback
        });

        WasmDecoder {
            worker,
            _on_message_closure: on_message_closure,
            on_decoded_frame: dummy_callback,
        }
    }

    /// New ergonomic API: simply push a frame and let the decoder handle the rest
    pub fn push_frame(&self, frame: FrameBuffer) {
        match serde_wasm_bindgen::to_value(&frame) {
            Ok(js_frame) => {
                if let Err(e) = self.worker.post_message(&js_frame) {
                    log::error!("Error posting frame to worker: {:?}", e);
                }
            }
            Err(e) => {
                log::error!("Error serializing frame: {:?}", e);
            }
        }
    }

    /// Check if the decoder is waiting for a keyframe
    /// Note: This is now handled internally by the jitter buffer in the worker
    pub fn is_waiting_for_keyframe(&self) -> bool {
        // Since the jitter buffer is in the worker, we can't easily check this
        // For now, return false and let the worker handle keyframe logic
        false
    }

    /// Flush the internal jitter buffer
    /// Note: This could be implemented by sending a special message to the worker
    pub fn flush(&self) {
        // TODO: Implement by sending a "flush" message to the worker
        log::warn!("flush() not yet implemented for WasmDecoder");
    }
}

impl Drop for WasmDecoder {
    fn drop(&mut self) {
        console::log_1(&"Terminating worker".into());
        self.worker.terminate();
    }
}
