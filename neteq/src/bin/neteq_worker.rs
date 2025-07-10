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

/*
 * NetEq worker for browser: receives RTP-like Opus packets, feeds them to NetEq,
 * periodically pulls PCM frames and posts them back to main thread.
 */
#![cfg_attr(target_arch = "wasm32", no_main)]

#[cfg(target_arch = "wasm32")]
mod wasm_worker {
    use neteq::WebNetEq;
    use serde::{Deserialize, Serialize};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{console, DedicatedWorkerGlobalScope, MessageEvent};

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "cmd", rename_all = "camelCase")]
    enum WorkerMsg {
        Init {
            sample_rate: u32,
            channels: u8,
        },
        /// Insert an encoded packet
        Insert {
            seq: u16,
            timestamp: u32,
            #[serde(with = "serde_bytes")]
            payload: Vec<u8>,
        },
        Flush,
        Clear,
        Close,
        /// Mute/unmute audio output
        Mute {
            muted: bool,
        },
        /// Enable/disable diagnostics reporting
        SetDiagnostics {
            enabled: bool,
        },
    }

    thread_local! {
        static NETEQ: std::cell::RefCell<Option<WebNetEq>> = const { std::cell::RefCell::new(None) };
        static IS_MUTED: std::cell::RefCell<bool> = const { std::cell::RefCell::new(true) }; // Start muted by default
        static DIAGNOSTICS_ENABLED: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) }; // Diagnostics enabled by default
    }

    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
        console::log_1(&"[neteq-worker] starting".into());
        let self_scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
        let self_scope_clone = self_scope.clone();
        let self_scope_clone_2 = self_scope.clone();
        let self_scope_clone_3 = self_scope.clone();
        let on_message = Closure::wrap(Box::new(move |evt: MessageEvent| {
            match serde_wasm_bindgen::from_value::<WorkerMsg>(evt.data()) {
                Ok(msg) => handle_message(&self_scope_clone, msg),
                Err(e) => console::error_1(&format!("[neteq-worker] bad msg: {:?}", e).into()),
            }
        }) as Box<dyn FnMut(_)>);
        console::log_1(&"[neteq-worker] onmessage".into());

        self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        // Eagerly create a default NetEq (48 kHz / mono). If the main thread later sends an
        // explicit Init message, that path will simply be ignored because `NETEQ` is already
        // populated.
        NETEQ.with(|cell| {
            if cell.borrow().is_none() {
                match WebNetEq::new(48_000, 1) {
                    Ok(eq) => {
                        *cell.borrow_mut() = Some(eq);
                        console::log_1(
                            &"[neteq-worker] NetEq auto-initialised (48 kHz/mono)".into(),
                        );

                        // Log initial mute state
                        IS_MUTED.with(|muted_cell| {
                            let is_muted = *muted_cell.borrow();
                            console::log_1(
                                &format!(
                                    "🔇 NetEq worker auto-initialized with muted: {}",
                                    is_muted
                                )
                                .into(),
                            );
                        });
                    }
                    Err(e) => {
                        console::error_2(&"[neteq-worker] auto-init error:".into(), &e);
                    }
                }
            }
        });

        // === Stats interval (1 Hz) ===
        console::log_1(&"[neteq-worker] stats interval".into());
        let stats_cb = Closure::wrap(Box::new(move || {
            DIAGNOSTICS_ENABLED.with(|enabled_cell| {
                let is_enabled = *enabled_cell.borrow();
                if !is_enabled {
                    return; // Skip stats reporting if diagnostics are disabled
                }

                NETEQ.with(|cell| {
                    if let Some(eq) = cell.borrow().as_ref() {
                        if let Ok(js_val) = eq.get_statistics() {
                            // Build { cmd: "stats", stats: <object> }
                            let obj = js_sys::Object::new();
                            let _ = js_sys::Reflect::set(
                                &obj,
                                &JsValue::from_str("cmd"),
                                &JsValue::from_str("stats"),
                            );
                            let _ =
                                js_sys::Reflect::set(&obj, &JsValue::from_str("stats"), &js_val);
                            let _ = js_sys::global()
                                .unchecked_into::<DedicatedWorkerGlobalScope>()
                                .post_message(&obj);
                        }
                    }
                });
            });
        }) as Box<dyn FnMut()>);
        let _ = self_scope_clone_2.set_interval_with_callback_and_timeout_and_arguments_0(
            stats_cb.as_ref().unchecked_ref(),
            1000,
        );
        stats_cb.forget();

        // Timer to pull audio every 10 ms.
        let cb = Closure::wrap(Box::new(move || {
            IS_MUTED.with(|muted_cell| {
                let is_muted = *muted_cell.borrow();
                if !is_muted {
                    NETEQ.with(|cell| {
                        if let Some(eq) = cell.borrow().as_ref() {
                            if let Ok(pcm) = eq.get_audio() {
                                let sab = js_sys::Array::of1(&pcm.buffer());
                                let _ = js_sys::global()
                                    .unchecked_into::<DedicatedWorkerGlobalScope>()
                                    .post_message_with_transfer(&pcm, &sab);
                            }
                        }
                    });
                } else {
                    // Debug: Log when audio is skipped due to muting (but only occasionally to avoid spam)
                    static mut SKIP_COUNTER: u32 = 0;
                    unsafe {
                        SKIP_COUNTER += 1;
                        if SKIP_COUNTER % 100 == 0 {
                            // Log every 100 skips (every 1 second)
                            console::log_1(
                                &format!(
                                    "🔇 Skipped audio production {} times (muted)",
                                    SKIP_COUNTER
                                )
                                .into(),
                            );
                        }
                    }
                }
                // If muted, we don't call get_audio() so NetEq doesn't produce expand packets
            });
        }) as Box<dyn FnMut()>);
        let _ = self_scope_clone_3.set_interval_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            10,
        );
        cb.forget();

        on_message.forget();
    }

    fn handle_message(scope: &DedicatedWorkerGlobalScope, msg: WorkerMsg) {
        // console::log_1(&format!("[neteq-worker] received message: {:?}", msg).into());
        match msg {
            WorkerMsg::Init {
                sample_rate,
                channels,
            } => {
                console::log_2(
                    &"[neteq-worker] Init received, sr=".into(),
                    &JsValue::from_f64(sample_rate as f64),
                );

                // NOTE: We don't set up a second timer here! The main timer in start() already handles audio production
                // and respects the mute state. Setting up a second timer would bypass mute functionality.
            }
            WorkerMsg::Insert {
                seq,
                timestamp,
                payload,
            } => {
                // console::log_1(&"[neteq-worker] insert_packet".into());
                NETEQ.with(|cell| {
                    if let Some(eq) = cell.borrow().as_ref() {
                        if let Err(e) = eq.insert_packet(seq, timestamp, &payload) {
                            console::error_1(
                                &format!("[neteq-worker] insert_packet error: {:?}", e).into(),
                            );
                        }
                    }
                });
            }
            WorkerMsg::Flush => {
                NETEQ.with(|cell| {
                    if let Some(eq) = cell.borrow().as_ref() {
                        // Flush is handled by the NetEq instance
                        console::log_1(&"[neteq-worker] flush".into());
                    }
                });
            }
            WorkerMsg::Clear => {
                NETEQ.with(|cell| cell.borrow_mut().take());
            }
            WorkerMsg::Close => {
                scope.close();
            }
            WorkerMsg::Mute { muted } => {
                IS_MUTED.with(|muted_cell| {
                    *muted_cell.borrow_mut() = muted;
                    console::log_2(
                        &"[neteq-worker] audio muted:".into(),
                        &JsValue::from_bool(muted),
                    );
                    console::log_1(
                        &format!("🔇 NetEq worker received mute message: {}", muted).into(),
                    );
                });
            }
            WorkerMsg::SetDiagnostics { enabled } => {
                DIAGNOSTICS_ENABLED.with(|enabled_cell| {
                    *enabled_cell.borrow_mut() = enabled;
                    console::log_2(
                        &"[neteq-worker] diagnostics enabled:".into(),
                        &JsValue::from_bool(enabled),
                    );
                });
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("neteq_worker is only compiled for wasm32 target");
}
