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
use crate::messages::{VideoStatsMessage, WorkerMessage};
#[cfg(feature = "wasm")]
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
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
    _on_decoded_frame: Box<dyn Fn(DecodedFrame)>,
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
            // We need to use Rc<RefCell<>> to share the callback since trait objects can't be cloned
            use std::cell::RefCell;
            use std::rc::Rc;
            let callback_rc = Rc::new(RefCell::new(callback));
            let callback_for_closure = callback_rc.clone();

            Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
                let js_val = event.data();

                // Clone js_val before trying to convert it to avoid move issues
                match js_val.clone().dyn_into::<VideoFrame>() {
                    Ok(video_frame) => {
                        // Convert VideoFrame to DecodedFrame for consistency
                        let decoded_frame = DecodedFrame {
                            sequence_number: 0, // Note: sequence number tracking happens in jitter buffer
                            width: video_frame.display_width(),
                            height: video_frame.display_height(),
                            data: vec![], // For now, we don't copy the actual video data
                        };

                        // Call the callback through RefCell
                        if let Ok(cb) = callback_for_closure.try_borrow() {
                            cb(decoded_frame);
                        }
                        video_frame.close();
                    }
                    Err(_) => {
                        if !handle_worker_diag_message(&js_val) {
                            log::warn!("Received unexpected message from worker: {js_val:?}");
                        }
                    }
                }
            }) as Box<dyn FnMut(_)>)
        };

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        // Create a dummy callback for the struct field since the real one is in Rc<RefCell<>>
        let dummy_callback = Box::new(|_: DecodedFrame| {
            // The actual callback is handled through the Rc<RefCell<>> in the closure
        });

        WasmDecoder {
            worker,
            _on_message_closure: on_message_closure,
            _on_decoded_frame: dummy_callback,
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

                // Clone js_val before trying to convert it to avoid move issues
                match js_val.clone().dyn_into::<VideoFrame>() {
                    Ok(video_frame) => {
                        callback(video_frame);
                    }
                    Err(_) => {
                        if !handle_worker_diag_message(&js_val) {
                            log::warn!("Received unexpected message from worker: {js_val:?}");
                        }
                    }
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
            _on_decoded_frame: dummy_callback,
        }
    }

    /// New ergonomic API: simply push a frame and let the decoder handle the rest
    pub fn push_frame(&self, frame: FrameBuffer) {
        let message = WorkerMessage::DecodeFrame(frame);
        match serde_wasm_bindgen::to_value(&message) {
            Ok(js_message) => {
                if let Err(e) = self.worker.post_message(&js_message) {
                    log::error!("Error posting message to worker: {e:?}");
                }
            }
            Err(e) => {
                log::error!("Error serializing message: {e:?}");
            }
        }
    }

    /// Provide diagnostic context to the worker so that metrics include original peer IDs
    pub fn set_context(&self, from_peer: String, to_peer: String) {
        let message = WorkerMessage::SetContext { from_peer, to_peer };
        match serde_wasm_bindgen::to_value(&message) {
            Ok(js_message) => {
                if let Err(e) = self.worker.post_message(&js_message) {
                    log::error!("Error posting context message to worker: {e:?}");
                } else {
                    log::debug!("Sent context to worker");
                }
            }
            Err(e) => log::error!("Error serializing context message: {e:?}"),
        }
    }

    /// Check if the decoder is waiting for a keyframe
    /// Note: This is now handled internally by the jitter buffer in the worker
    pub fn is_waiting_for_keyframe(&self) -> bool {
        // Since the jitter buffer is in the worker, we can't easily check this
        // For now, return false and let the worker handle keyframe logic
        false
    }

    /// Flush the internal decoder buffer
    pub fn flush(&self) {
        let message = WorkerMessage::Flush;
        match serde_wasm_bindgen::to_value(&message) {
            Ok(js_message) => {
                if let Err(e) = self.worker.post_message(&js_message) {
                    log::error!("Error posting flush message to worker: {e:?}");
                } else {
                    log::debug!("Sent flush message to worker");
                }
            }
            Err(e) => {
                log::error!("Error serializing flush message: {e:?}");
            }
        }
    }

    /// Reset the decoder to initial state (waiting for keyframe)
    pub fn reset(&self) {
        let message = WorkerMessage::Reset;
        match serde_wasm_bindgen::to_value(&message) {
            Ok(js_message) => {
                if let Err(e) = self.worker.post_message(&js_message) {
                    log::error!("Error posting reset message to worker: {e:?}");
                } else {
                    log::debug!("Sent reset message to worker");
                }
            }
            Err(e) => {
                log::error!("Error serializing reset message: {e:?}");
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

/// Handle diagnostics objects posted by the worker. Returns true if handled.
fn handle_worker_diag_message(js_val: &JsValue) -> bool {
    // Try to deserialize the JavaScript object using serde
    match serde_wasm_bindgen::from_value::<VideoStatsMessage>(js_val.clone()) {
        Ok(stats_msg) => {
            // Only handle video_stats messages
            if stats_msg.kind != "video_stats" {
                return false;
            }

            #[cfg(feature = "wasm")]
            {
                let evt = DiagEvent {
                    subsystem: "video",
                    stream_id: None,
                    ts_ms: now_ms(),
                    metrics: vec![
                        metric!("from_peer", stats_msg.from_peer.unwrap_or_default()),
                        metric!("to_peer", stats_msg.to_peer.unwrap_or_default()),
                        metric!("frames_buffered", stats_msg.frames_buffered.unwrap_or(0)),
                    ],
                };
                let _ = global_sender().try_broadcast(evt);
            }
            true
        }
        Err(_) => {
            // Not a recognized diagnostic message
            log::debug!("Received unexpected message from worker: {js_val:?}");
            false
        }
    }
}
