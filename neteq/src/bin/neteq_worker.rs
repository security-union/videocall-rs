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
    use log::LevelFilter;
    #[cfg(feature = "matomo-logger")]
    use matomo_logger::worker as matomo_worker;
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

    /// Messages sent from worker back to main thread
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "camelCase")]
    enum WorkerResponse {
        WorkerReady {
            mute_state: bool,
        },
        Stats {
            #[serde(skip_serializing, skip_deserializing)]
            stats: JsValue, // Will be set manually since JsValue doesn't serialize
        },
    }

    thread_local! {
        static NETEQ: std::cell::RefCell<Option<WebNetEq>> = const { std::cell::RefCell::new(None) };
        static IS_MUTED: std::cell::RefCell<bool> = const { std::cell::RefCell::new(true) }; // Start muted by default
        static DIAGNOSTICS_ENABLED: std::cell::RefCell<bool> = const { std::cell::RefCell::new(true) }; // Diagnostics enabled by default
    }

    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
        console::log_1(&"[neteq-worker] starting".into());

        // Initialize worker-side logger bridge: forward WARN+ to main thread (Matomo)
        // Keep console at INFO for local visibility inside the worker
        #[cfg(feature = "matomo-logger")]
        {
            if let Err(_e) =
                matomo_worker::init_with_bridge(LevelFilter::Info, LevelFilter::Warn, {
                    // The bridge expects the object as 'arguments[0]'
                    js_sys::Function::new_no_args("self.postMessage(arguments[0]);")
                })
            {
                console::error_1(
                    &"[neteq-worker] Failed to initialize matomo worker bridge".into(),
                );
            }
        }

        // Load opus-decoder library in worker context by evaluating the script directly
        let global_scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();

        // Instead of importing external file, evaluate the opus-decoder script directly
        let opus_decoder_script = include_str!("../scripts/opus-decoder.min.js");
        if let Ok(eval_fn) = js_sys::Reflect::get(&global_scope, &JsValue::from_str("eval")) {
            if eval_fn.is_function() {
                let eval_function = eval_fn.unchecked_into::<js_sys::Function>();
                if let Err(e) =
                    eval_function.call1(&global_scope, &JsValue::from_str(opus_decoder_script))
                {
                    console::warn_2(&"[neteq-worker] Failed to load opus-decoder:".into(), &e);
                } else {
                    console::log_1(
                        &"[neteq-worker] Successfully loaded opus-decoder library".into(),
                    );
                }
            }
        }

        // Note: PCM AudioWorklet is registered against the main thread AudioContext.
        // We embed/register it there instead of evaluating it inside this worker.

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
                        // Spawn async initialization
                        wasm_bindgen_futures::spawn_local(async move {
                            match eq.init().await {
                                Ok(()) => {
                                    NETEQ.with(|cell| {
                                        *cell.borrow_mut() = Some(eq);
                                    });
                                    console::log_1(
                                        &"[neteq-worker] NetEq auto-initialised (48 kHz/mono)"
                                            .into(),
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

                                        // Send WorkerReady confirmation to main thread
                                        let ready_msg = WorkerResponse::WorkerReady {
                                            mute_state: is_muted,
                                        };
                                        if let Ok(js_msg) = serde_wasm_bindgen::to_value(&ready_msg)
                                        {
                                            let _ = js_sys::global()
                                                .unchecked_into::<DedicatedWorkerGlobalScope>()
                                                .post_message(&js_msg);
                                            console::log_1(
                                                &"✅ Sent WorkerReady confirmation to main thread"
                                                    .into(),
                                            );
                                        } else {
                                            console::error_1(
                                                &"❌ Failed to serialize WorkerReady message"
                                                    .into(),
                                            );
                                        }
                                    });
                                }
                                Err(e) => {
                                    console::error_2(&"[neteq-worker] auto-init error:".into(), &e);
                                }
                            }
                        });
                    }
                    Err(e) => {
                        console::error_2(&"[neteq-worker] WebNetEq::new error:".into(), &e);
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
                            // Manual construction since JsValue doesn't serialize properly
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

        // High-frequency timer (5ms) for precise audio timing decisions (2x rate approach)
        let cb = Closure::wrap(Box::new(move || {
            IS_MUTED.with(|muted_cell| {
                let is_muted = *muted_cell.borrow();

                // High-precision timing tracking
                static mut START_TIME: f64 = 0.0;
                static mut LAST_PRODUCTION_TIME: f64 = 0.0;
                static mut TOTAL_FRAMES_PRODUCED: u64 = 0;
                static mut TIMING_ADJUSTMENTS: i32 = 0;
                static mut LAST_TIMING_LOG: f64 = 0.0;

                unsafe {
                    let now = js_sys::Date::now();
                    // Initialize timing on first call
                    if START_TIME == 0.0 {
                        START_TIME = now;
                        LAST_PRODUCTION_TIME = now;
                        console::log_1(&"🎵 NetEq: Starting high-frequency timing checks (5ms rate)".into());
                        // Produce first frame immediately
                        if !is_muted {
                            NETEQ.with(|cell| {
                                if let Some(eq) = cell.borrow().as_ref() {
                                    if let Ok(pcm) = eq.get_audio() {
                                        TOTAL_FRAMES_PRODUCED += 1;
                                        let sab = js_sys::Array::of1(&pcm.buffer());
                                        let _ = js_sys::global()
                                            .unchecked_into::<DedicatedWorkerGlobalScope>()
                                            .post_message_with_transfer(&pcm, &sab);
                                    }
                                }
                            });
                        }
                        return;
                    }

                    // Calculate timing metrics
                    let total_elapsed_ms = now - START_TIME;
                    let interval_since_last = now - LAST_PRODUCTION_TIME;
                    let expected_total_frames = (total_elapsed_ms / 10.0) as u64;
                    let frames_behind = expected_total_frames.saturating_sub(TOTAL_FRAMES_PRODUCED) as i32;
                                         // Decide whether to produce audio this cycle
                     let should_produce = if is_muted {
                         // When muted, keep frame count in sync but don't produce audio
                         TOTAL_FRAMES_PRODUCED = expected_total_frames;
                         false
                     } else {
                         // Produce audio if we're behind or if a full 10ms period has passed
                         frames_behind > 0 || interval_since_last >= 10.0
                     };

                    // Log timing statistics every 5 seconds
                    if now - LAST_TIMING_LOG > 5000.0 {
                        let actual_production_rate = TOTAL_FRAMES_PRODUCED as f64 / (total_elapsed_ms / 1000.0);
                        let expected_rate = 100.0; // 100 Hz target
                        let timing_error_ms = (TOTAL_FRAMES_PRODUCED as f64 * 10.0) - total_elapsed_ms;
                        log::debug!(
                            "🎯 NetEq (5ms checks): {actual_production_rate:.1}Hz actual, {expected_rate:.1}Hz expected, {timing_error_ms:.1}ms timing error, {frames_behind} behind, muted={is_muted}"
                         );
                        LAST_TIMING_LOG = now;
                    }

                    // Produce audio if needed
                    if should_produce {
                        NETEQ.with(|cell| {
                            if let Some(eq) = cell.borrow().as_ref() {
                                if let Ok(pcm) = eq.get_audio() {
                                    TOTAL_FRAMES_PRODUCED += 1;
                                    LAST_PRODUCTION_TIME = now;
                                    let sab = js_sys::Array::of1(&pcm.buffer());
                                    let _ = js_sys::global()
                                        .unchecked_into::<DedicatedWorkerGlobalScope>()
                                        .post_message_with_transfer(&pcm, &sab);
                                    // Track timing adjustments for debugging
                                    if frames_behind > 1 {
                                        TIMING_ADJUSTMENTS += 1;
                                        log::debug!(
                                            "⚡ Timing adjustment: {} frames behind, interval was {:.1}ms",
                                            frames_behind, interval_since_last
                                        );
                                    }
                                } else {
                                    // NetEq couldn't provide audio - this is expected sometimes
                                    console::log_1(&"📭 NetEq: No audio available this cycle".into());
                                }
                            }
                        });
                    }
                }
            });
        }) as Box<dyn FnMut()>);
        let _ = self_scope_clone_3
            .set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 5);
        cb.forget();

        on_message.forget();
    }

    fn handle_message(scope: &DedicatedWorkerGlobalScope, msg: WorkerMsg) {
        // console::log_1(&format!("[neteq-worker] received message: {:?}", msg).into());
        match msg {
            WorkerMsg::Init {
                sample_rate,
                channels: _,
            } => {
                console::log_2(
                    &"[neteq-worker] Init received, sr=".into(),
                    &JsValue::from_f64(sample_rate as f64),
                );

                // NOTE: We don't set up a second timer here! The main timer in start() already handles audio production
                // with time-based logic to handle Safari's irregular intervals, and respects the mute state.
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
                    if let Some(_eq) = cell.borrow().as_ref() {
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
                    let old_state = *muted_cell.borrow();
                    *muted_cell.borrow_mut() = muted;
                    let now = js_sys::Date::now();

                    console::log_2(
                        &"[neteq-worker] audio muted:".into(),
                        &JsValue::from_bool(muted),
                    );
                    log::info!(
                        "🔇 NetEq worker mute state: {} -> {} at {:.0}ms",
                        old_state,
                        muted,
                        now
                    );

                    if old_state != muted {
                        log::info!("⚡ Mute state CHANGED for worker at {:.0}ms", now);
                    } else {
                        log::info!(
                            "🔄 Mute state unchanged (redundant message) at {:.0}ms",
                            now
                        );
                    }
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
