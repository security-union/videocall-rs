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

//! Microphone Opus encoder worker. One per `MicrophoneEncoder`, reused by
//! each capture session (unmute, device switch), so its wasm is fetched and
//! compiled once.
//!
//! Glue only: the encoding lives in `videocall_client::encode::mic_opus`.
//!
//! Message protocol:
//!
//! - Worker to main thread: `{type: "ready"}` once its wasm is loaded and
//!   `onmessage` is set (messages posted earlier would be lost); then
//!   `{type: "packet", session, frame, data}` per Opus packet, where `data`
//!   is a `Uint8Array` (buffer transferred) and `frame` the encoder's frame
//!   index counting DTX-dropped frames; and `{type: "error", session,
//!   message}` if a session's setup or encoding fails.
//! - Main thread to worker:
//!   - `{cmd: "init", session, inputSampleRate, bitrateBps, dtx}` with one
//!     transferred `MessagePort` whose other end belongs to the session's
//!     capture AudioWorklet (`micCaptureWorklet.js`). Replaces any previous
//!     session; frame indices start again at 0.
//!   - `{cmd: "reconfigure", session, bitrateBps}`: new target bitrate.
//!   - `{cmd: "stop", session}`: close the session's port, drop its encoder.
//! - Worklet to worker, over the port: `Float32Array` blocks of mono PCM at
//!   `inputSampleRate`. Each is sent back over the port after its samples
//!   are copied, so the worklet can reuse it. A plain number instead is a
//!   gap: that many input samples the worklet dropped while this worker
//!   was behind. The encoder's frame index skips over them.
//!
//! The main thread terminates the worker when the `MicrophoneEncoder` goes.
#![cfg_attr(target_arch = "wasm32", no_main)]

#[cfg(target_arch = "wasm32")]
mod wasm_worker {
    use std::cell::RefCell;

    use js_sys::{Array, Float32Array, Object, Reflect, Uint8Array};
    use serde::Deserialize;
    use videocall_client::encode::mic_opus::{MicEncoderConfig, MicOpusEncoder};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{DedicatedWorkerGlobalScope, MessageEvent, MessagePort};

    #[derive(Debug, Deserialize)]
    #[serde(tag = "cmd", rename_all = "camelCase")]
    enum WorkerMsg {
        #[serde(rename_all = "camelCase")]
        Init {
            session: u32,
            input_sample_rate: u32,
            bitrate_bps: u32,
            dtx: bool,
        },
        #[serde(rename_all = "camelCase")]
        Reconfigure {
            session: u32,
            bitrate_bps: u32,
        },
        Stop {
            session: u32,
        },
    }

    struct Session {
        id: u32,
        encoder: MicOpusEncoder,
        /// Samples of the current block, copied out of the JS array.
        samples: Vec<f32>,
        port: MessagePort,
        _port_handler: Closure<dyn FnMut(MessageEvent)>,
        /// Report only the first encode error; the rest would flood the log.
        reported_error: bool,
    }

    impl Session {
        fn close(self) {
            self.port.set_onmessage(None);
            self.port.close();
        }
    }

    thread_local! {
        static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    }

    fn scope() -> DedicatedWorkerGlobalScope {
        js_sys::global().unchecked_into()
    }

    fn post_ready() {
        let object = Object::new();
        let _ = Reflect::set(&object, &"type".into(), &"ready".into());
        let _ = scope().post_message(&object);
    }

    fn post_error(session: u32, message: &str) {
        let object = Object::new();
        let _ = Reflect::set(&object, &"type".into(), &"error".into());
        let _ = Reflect::set(&object, &"session".into(), &session.into());
        let _ = Reflect::set(&object, &"message".into(), &message.into());
        let _ = scope().post_message(&object);
    }

    fn post_packet(session: u32, frame: u64, packet: &[u8]) {
        let data = Uint8Array::from(packet);
        let object = Object::new();
        let _ = Reflect::set(&object, &"type".into(), &"packet".into());
        let _ = Reflect::set(&object, &"session".into(), &session.into());
        // f64 holds frame indices exactly for 285,000 years of audio.
        let _ = Reflect::set(&object, &"frame".into(), &(frame as f64).into());
        let _ = Reflect::set(&object, &"data".into(), &data);
        let transfer = Array::of1(&data.buffer());
        if let Err(e) = scope().post_message_with_transfer(&object, &transfer) {
            web_sys::console::error_2(&"audio_encoder_worker: post failed".into(), &e);
        }
    }

    #[wasm_bindgen(start)]
    pub fn start() {
        let on_message = Closure::wrap(Box::new(on_main_message) as Box<dyn FnMut(MessageEvent)>);
        scope().set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        // Lives as long as the worker; terminate() frees everything.
        on_message.forget();
        post_ready();
    }

    fn on_main_message(event: MessageEvent) {
        match serde_wasm_bindgen::from_value::<WorkerMsg>(event.data()) {
            Ok(WorkerMsg::Init {
                session,
                input_sample_rate,
                bitrate_bps,
                dtx,
            }) => init(&event, session, input_sample_rate, bitrate_bps, dtx),
            Ok(WorkerMsg::Reconfigure {
                session,
                bitrate_bps,
            }) => SESSION.with(|cell| {
                if let Some(current) = cell.borrow_mut().as_mut().filter(|s| s.id == session) {
                    if let Err(e) = current.encoder.set_bitrate(bitrate_bps) {
                        post_error(session, &e.to_string());
                    }
                }
            }),
            Ok(WorkerMsg::Stop { session }) => SESSION.with(|cell| {
                let mut slot = cell.borrow_mut();
                if slot.as_ref().is_some_and(|s| s.id == session) {
                    if let Some(old) = slot.take() {
                        old.close();
                    }
                }
            }),
            Err(e) => {
                web_sys::console::error_1(&format!("audio_encoder_worker: bad message: {e}").into())
            }
        }
    }

    fn init(event: &MessageEvent, id: u32, input_sample_rate: u32, bitrate_bps: u32, dtx: bool) {
        // Whatever happens, the previous session is over.
        SESSION.with(|cell| {
            if let Some(old) = cell.borrow_mut().take() {
                old.close();
            }
        });
        let Some(port) = event.ports().get(0).dyn_into::<MessagePort>().ok() else {
            post_error(id, "init without a worklet port");
            return;
        };
        let encoder = match MicOpusEncoder::new(MicEncoderConfig {
            input_sample_rate,
            bitrate_bps,
            dtx,
        }) {
            Ok(encoder) => encoder,
            Err(e) => {
                port.close();
                post_error(id, &e.to_string());
                return;
            }
        };
        let handler = Closure::wrap(Box::new(on_worklet_message) as Box<dyn FnMut(MessageEvent)>);
        port.set_onmessage(Some(handler.as_ref().unchecked_ref()));
        SESSION.with(|cell| {
            *cell.borrow_mut() = Some(Session {
                id,
                encoder,
                samples: Vec::with_capacity(512),
                port,
                _port_handler: handler,
                reported_error: false,
            });
        });
    }

    fn on_worklet_message(event: MessageEvent) {
        let data = event.data();
        if let Some(dropped) = data.as_f64() {
            // Cap hostile or corrupt values at an hour of 384 kHz audio.
            if dropped.is_finite() && dropped > 0.0 {
                SESSION.with(|cell| {
                    if let Some(session) = cell.borrow_mut().as_mut() {
                        session
                            .encoder
                            .skip_input(dropped.min(384_000.0 * 3_600.0) as usize);
                    }
                });
            }
            return;
        }
        let Ok(block) = data.dyn_into::<Float32Array>() else {
            return;
        };
        SESSION.with(|cell| {
            let mut guard = cell.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            let len = block.length() as usize;
            session.samples.resize(len, 0.0);
            block.copy_to(&mut session.samples);
            // Hand the block back for reuse before encoding.
            let transfer = Array::of1(&block.buffer());
            let _ = session
                .port
                .post_message_with_transferable(&block, &transfer);

            let Session {
                id,
                encoder,
                samples,
                reported_error,
                ..
            } = session;
            let id = *id;
            if let Err(e) = encoder.push(samples, |frame, packet| post_packet(id, frame, packet)) {
                if !*reported_error {
                    *reported_error = true;
                    post_error(id, &e.to_string());
                }
            }
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("audio_encoder_worker is only compiled for the wasm32 target");
}
