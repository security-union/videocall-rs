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

#[cfg(any(target_arch = "wasm32", test))]
const MAX_FRAMES_PER_TICK: u64 = 2;
#[cfg(any(target_arch = "wasm32", test))]
const MAX_OWED_FRAMES: u64 = 10;

/// One tick: (counter after mute sync or backlog forgiveness, frames behind, frames to pull).
#[cfg(any(target_arch = "wasm32", test))]
fn pace_tick(
    now_ms: f64,
    start_ms: f64,
    produced: u64,
    last_production_ms: f64,
    muted: bool,
) -> (u64, u64, u64) {
    let expected = ((now_ms - start_ms) / 10.0) as u64;
    if muted {
        return (expected, 0, 0);
    }
    let produced = produced.max(expected.saturating_sub(MAX_OWED_FRAMES));
    let behind = expected.saturating_sub(produced);
    let frames = if behind > 0 {
        behind.min(MAX_FRAMES_PER_TICK)
    } else if now_ms - last_production_ms >= 10.0 {
        1
    } else {
        0
    };
    (produced, behind, frames)
}

/// Pulls up to `frames` frames, stopping at the first failed `pull`; returns frames emitted.
#[cfg(any(target_arch = "wasm32", test))]
fn emit_frames(
    frames: u64,
    now_ms: f64,
    produced: &mut u64,
    last_production_ms: &mut f64,
    mut pull: impl FnMut() -> bool,
) -> u64 {
    let mut emitted = 0;
    for _ in 0..frames {
        if !pull() {
            break;
        }
        *produced += 1;
        *last_production_ms = now_ms;
        emitted += 1;
    }
    emitted
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, PartialEq)]
struct PaceState {
    start_ms: f64,
    produced: u64,
    last_production_ms: f64,
}

#[cfg(any(target_arch = "wasm32", test))]
impl PaceState {
    /// First tick: pulls one frame unless muted.
    fn start(now_ms: f64, muted: bool, pull: impl FnMut() -> bool) -> Self {
        let mut state = Self {
            start_ms: now_ms,
            produced: 0,
            last_production_ms: now_ms,
        };
        if !muted {
            emit_frames(
                1,
                now_ms,
                &mut state.produced,
                &mut state.last_production_ms,
                pull,
            );
        }
        state
    }

    /// Every later tick; returns (frames behind, frames emitted).
    fn tick(&mut self, now_ms: f64, muted: bool, pull: impl FnMut() -> bool) -> (u64, u64) {
        let (produced, behind, frames) = pace_tick(
            now_ms,
            self.start_ms,
            self.produced,
            self.last_production_ms,
            muted,
        );
        self.produced = produced;
        let emitted = emit_frames(
            frames,
            now_ms,
            &mut self.produced,
            &mut self.last_production_ms,
            pull,
        );
        (behind, emitted)
    }
}

/// Frames per level message to the main thread: 100 frames/s -> 20 Hz.
#[cfg(any(target_arch = "wasm32", test))]
const LEVEL_BATCH_FRAMES: u8 = 5;

/// The loudest frame RMS since the last level message (direct path only).
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct LevelBatch {
    loudest: f32,
    frames: u8,
}

#[cfg(any(target_arch = "wasm32", test))]
impl LevelBatch {
    /// Adds one frame; returns the batch's loudest level once it holds `LEVEL_BATCH_FRAMES`.
    fn push(&mut self, rms: f32) -> Option<f32> {
        self.loudest = self.loudest.max(rms);
        self.frames += 1;
        if self.frames >= LEVEL_BATCH_FRAMES {
            self.flush()
        } else {
            None
        }
    }

    /// The partial batch's loudest level, if any frame is waiting; empties the batch.
    fn flush(&mut self) -> Option<f32> {
        let level = (self.frames > 0).then_some(self.loudest);
        *self = Self::default();
        level
    }
}

/// A mute's messages: the partial batch's level, then `pause` to the worklet, or to the main
/// thread when `post_direct` has no port (`None`) or its post fails (`Some(false)`).
#[cfg(any(target_arch = "wasm32", test))]
fn pause(
    batch: &mut LevelBatch,
    post_level: impl FnOnce(f32),
    post_direct: impl FnOnce() -> Option<bool>,
    post_main: impl FnOnce(),
) {
    if let Some(level) = batch.flush() {
        post_level(level);
    }
    if post_direct() != Some(true) {
        post_main();
    }
}

/// At most one warning per `WARN_INTERVAL_MS`; the next one carries how many were held back.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
struct WarnLimiter {
    last_ms: f64,
    held_back: u32,
}

#[cfg(any(target_arch = "wasm32", test))]
const WARN_INTERVAL_MS: f64 = 1000.0;

#[cfg(any(target_arch = "wasm32", test))]
impl WarnLimiter {
    const fn new() -> Self {
        Self {
            last_ms: f64::NEG_INFINITY,
            held_back: 0,
        }
    }

    /// `Some(held back since the last warning)` if a warning may go out at `now_ms`.
    fn allow(&mut self, now_ms: f64) -> Option<u32> {
        if now_ms - self.last_ms < WARN_INTERVAL_MS {
            self.held_back += 1;
            return None;
        }
        let held_back = self.held_back;
        *self = Self {
            last_ms: now_ms,
            held_back: 0,
        };
        Some(held_back)
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_worker {
    const AUDIO_PRODUCTION_INTERVAL_MS: i32 = 5;

    use super::{pause, LevelBatch, PaceState, WarnLimiter};

    use neteq::web::init_net_eq;
    use neteq::WebNetEq;
    use serde::{Deserialize, Serialize};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{console, DedicatedWorkerGlobalScope, MessageEvent, MessagePort};

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
            /// 2: understands `attachOutput` and posts `pause`/`handoff`. Absent (0) before that.
            protocol: u32,
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
        static BOOTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The AudioWorklet's end of a MessageChannel: PCM goes there instead of to the main thread.
        static OUTPUT_PORT: std::cell::RefCell<Option<MessagePort>> = const { std::cell::RefCell::new(None) };
        static LEVEL_BATCH: std::cell::RefCell<LevelBatch> = const { std::cell::RefCell::new(LevelBatch { loudest: 0.0, frames: 0 }) };
        static DROP_WARN: std::cell::Cell<WarnLimiter> = const { std::cell::Cell::new(WarnLimiter::new()) };
        /// Made once: every inbound message is checked against these.
        static ATTACH_KEYS: (JsValue, JsValue, JsValue) = (
            JsValue::from_str("cmd"),
            JsValue::from_str("attachOutput"),
            JsValue::from_str("port"),
        );
        /// Made once: every direct frame is `{command: "play", pcm}`.
        static PLAY_KEYS: (JsValue, JsValue, JsValue) = (
            JsValue::from_str("command"),
            JsValue::from_str("play"),
            JsValue::from_str("pcm"),
        );
    }

    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
        console::log_1(&"[neteq-worker] starting".into());

        init_net_eq();

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
                    preset_opus_decoder_module(&global_scope);
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
            if attach_output(&evt.data()) {
                return;
            }
            match serde_wasm_bindgen::from_value::<WorkerMsg>(evt.data()) {
                Ok(msg) => handle_message(&self_scope_clone, msg),
                Err(e) => console::error_1(&format!("[neteq-worker] bad msg: {:?}", e).into()),
            }
        }) as Box<dyn FnMut(_)>);
        self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let on_boot_rejection = Closure::wrap(Box::new(move |event: JsValue| {
            if BOOTED.with(|b| b.get()) {
                return;
            }
            let reason =
                js_sys::Reflect::get(&event, &JsValue::from_str("reason")).unwrap_or(event);
            console::error_2(&"[neteq-worker] boot failed:".into(), &reason);
            rethrow_to_parent(reason);
        }) as Box<dyn FnMut(JsValue)>);
        let _ = self_scope.add_event_listener_with_callback(
            "unhandledrejection",
            on_boot_rejection.as_ref().unchecked_ref(),
        );
        on_boot_rejection.forget();

        // Eagerly create a default NetEq (48 kHz / mono). If the main thread later sends an
        // explicit Init message, that path will simply be ignored because `NETEQ` is already
        // populated.
        NETEQ.with(|cell| {
            if cell.borrow().is_none() {
                match WebNetEq::new(48_000, 1, 80) {
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
                                        BOOTED.with(|b| b.set(true));
                                        let ready_msg = WorkerResponse::WorkerReady {
                                            mute_state: is_muted,
                                            protocol: 2,
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
                                    rethrow_to_parent(e);
                                }
                            }
                        });
                    }
                    Err(e) => {
                        console::error_2(&"[neteq-worker] WebNetEq::new error:".into(), &e);
                        rethrow_to_parent(e);
                    }
                }
            }
        });

        // === Stats interval (1 Hz) ===
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

        // Audio production timer (AUDIO_PRODUCTION_INTERVAL_MS) for 100Hz frame rate
        let mut pace: Option<PaceState> = None;
        let mut last_timing_log = 0.0;
        let cb = Closure::wrap(Box::new(move || {
            let is_muted = IS_MUTED.with(|muted_cell| *muted_cell.borrow());
            let now = js_sys::Date::now();
            let Some(state) = pace.as_mut() else {
                console::log_1(
                    &format!(
                        "🎵 NetEq: Starting audio production timer ({}ms interval)",
                        AUDIO_PRODUCTION_INTERVAL_MS
                    )
                    .into(),
                );
                pace = Some(PaceState::start(now, is_muted, || pull_pcm(false)));
                return;
            };

            let (frames_behind, emitted) = state.tick(now, is_muted, || pull_pcm(true));

            if now - last_timing_log > 5000.0 {
                let total_elapsed_ms = now - state.start_ms;
                // Counter after pace_tick, before this tick's pulls.
                let produced = state.produced - emitted;
                let actual_production_rate = produced as f64 / (total_elapsed_ms / 1000.0);
                let expected_rate = 100.0; // 100 Hz target
                let timing_error_ms = (produced as f64 * 10.0) - total_elapsed_ms;
                log::debug!(
                    "🎯 NetEq ({}ms timer): {actual_production_rate:.1}Hz actual, {expected_rate:.1}Hz expected, {timing_error_ms:.1}ms timing error, {frames_behind} behind, muted={is_muted}",
                    AUDIO_PRODUCTION_INTERVAL_MS
                );
                last_timing_log = now;
            }
        }) as Box<dyn FnMut()>);
        let _ = self_scope_clone_3.set_interval_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            AUDIO_PRODUCTION_INTERVAL_MS,
        );
        cb.forget();

        on_message.forget();
    }

    /// Hands opus-decoder its Opus module already inflated, so its `puff` inflater never takes a
    /// wasm memory of the tab's (#3210).
    fn preset_opus_decoder_module(global_scope: &DedicatedWorkerGlobalScope) {
        let constructor = js_sys::Reflect::get(global_scope, &JsValue::from_str("opus-decoder"))
            .and_then(|ns| js_sys::Reflect::get(&ns, &JsValue::from_str("OpusDecoder")));
        let Some(constructor) = constructor.ok().filter(|c| c.is_function()) else {
            return;
        };
        let bytes = js_sys::Uint8Array::from(neteq::codec::OPUS_DECODER_WASM);
        let module = js_sys::WebAssembly::compile(&bytes);
        let _ = js_sys::Reflect::set(&constructor, &JsValue::from_str("module"), &module);
    }

    /// A failed init is fatal (no `WorkerReady` follows), so surface it as the parent's
    /// `Worker.onerror` (#3210). A throw from a timer is uncaught; one from a promise is not.
    fn rethrow_to_parent(e: JsValue) {
        let throw = Closure::once_into_js(move || -> Result<(), JsValue> { Err(e) });
        let _ = js_sys::global()
            .unchecked_into::<DedicatedWorkerGlobalScope>()
            .set_timeout_with_callback_and_timeout_and_arguments_0(throw.unchecked_ref(), 0);
    }

    /// `{cmd: "attachOutput", port}` from the main thread; true if `data` was that message. PCM
    /// moves to the port only once the worklet answers `inputReady` on it, so a port that never
    /// delivers leaves the main-thread path in place.
    fn attach_output(data: &JsValue) -> bool {
        let Some(port) = ATTACH_KEYS.with(|(cmd, attach, port)| {
            js_sys::Reflect::get(data, cmd)
                .is_ok_and(|c| c == *attach)
                .then(|| js_sys::Reflect::get(data, port).ok())
        }) else {
            return false;
        };
        match port.and_then(|p| p.dyn_into::<MessagePort>().ok()) {
            Some(port) => {
                let ready_port = port.clone();
                let on_ready = Closure::wrap(Box::new(move |evt: MessageEvent| {
                    let ready = js_sys::Reflect::get(&evt.data(), &JsValue::from_str("type"))
                        .ok()
                        .and_then(|t| t.as_string())
                        .is_some_and(|t| t == "inputReady");
                    if ready {
                        // After every frame already sent the old way, so the worklet can order them.
                        let _ = js_sys::global()
                            .unchecked_into::<DedicatedWorkerGlobalScope>()
                            .post_message(&command("handoff", "type"));
                        OUTPUT_PORT.with(|cell| *cell.borrow_mut() = Some(ready_port.clone()));
                        console::log_1(
                            &"[neteq-worker] PCM output attached to the AudioWorklet".into(),
                        );
                    }
                }) as Box<dyn FnMut(MessageEvent)>);
                port.set_onmessage(Some(on_ready.as_ref().unchecked_ref()));
                on_ready.forget();
            }
            None => console::warn_1(&"[neteq-worker] attachOutput without a MessagePort".into()),
        }
        true
    }

    fn command(name: &str, key: &str) -> js_sys::Object {
        let msg = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&msg, &JsValue::from_str(key), &JsValue::from_str(name));
        msg
    }

    fn post_level(level: Option<f32>) {
        if let Some(rms) = level {
            let _ = js_sys::global()
                .unchecked_into::<DedicatedWorkerGlobalScope>()
                .post_message(&JsValue::from_f64(f64::from(rms)));
        }
    }

    /// The worklet plays `pcm` directly; the main thread only gets its level for the VAD. A frame
    /// whose post fails is dropped: the worklet no longer plays frames from the main thread.
    fn post_pcm_direct(port: &MessagePort, pcm: &js_sys::Float32Array, rms: f32) {
        let msg = js_sys::Object::new();
        PLAY_KEYS.with(|(command, play, pcm_key)| {
            let _ = js_sys::Reflect::set(&msg, command, play);
            let _ = js_sys::Reflect::set(&msg, pcm_key, pcm);
        });
        if let Err(e) =
            port.post_message_with_transferable(&msg, &js_sys::Array::of1(&pcm.buffer()))
        {
            let mut limiter = DROP_WARN.with(|w| w.get());
            if let Some(held_back) = limiter.allow(js_sys::Date::now()) {
                console::warn_3(
                    &"[neteq-worker] dropped a PCM frame:".into(),
                    &e,
                    &format!("({held_back} more dropped since the last warning)").into(),
                );
            }
            DROP_WARN.with(|w| w.set(limiter));
        }
        post_level(LEVEL_BATCH.with(|b| b.borrow_mut().push(rms)));
    }

    /// Tell playback that no frame follows.
    fn post_pause() {
        LEVEL_BATCH.with(|b| {
            pause(
                &mut b.borrow_mut(),
                |rms| post_level(Some(rms)),
                || {
                    OUTPUT_PORT.with(|cell| {
                        cell.borrow()
                            .as_ref()
                            .map(|port| port.post_message(&command("pause", "command")).is_ok())
                    })
                },
                || {
                    let _ = js_sys::global()
                        .unchecked_into::<DedicatedWorkerGlobalScope>()
                        .post_message(&command("pause", "type"));
                },
            )
        });
    }

    /// Posts one PCM frame (to the worklet when attached, else the main thread); false if NetEq
    /// is absent or `get_audio` errs.
    fn pull_pcm(log_no_audio: bool) -> bool {
        NETEQ.with(|cell| {
            let neteq = cell.borrow();
            let Some(eq) = neteq.as_ref() else {
                return false;
            };
            let post_to_main = |pcm: &js_sys::Float32Array| {
                let sab = js_sys::Array::of1(&pcm.buffer());
                let _ = js_sys::global()
                    .unchecked_into::<DedicatedWorkerGlobalScope>()
                    .post_message_with_transfer(pcm, &sab);
            };
            // Only the direct path needs the worker's RMS; the old path's main thread computes it.
            let produced = match OUTPUT_PORT.with(|port| port.borrow().clone()) {
                Some(port) => eq
                    .get_audio_with_rms()
                    .map(|(pcm, rms)| post_pcm_direct(&port, &pcm, rms)),
                None => eq.get_audio().map(|pcm| post_to_main(&pcm)),
            };
            if produced.is_err() && log_no_audio {
                console::log_1(&"📭 NetEq: No audio available this cycle".into());
            }
            produced.is_ok()
        })
    }

    fn handle_message(scope: &DedicatedWorkerGlobalScope, msg: WorkerMsg) {
        // console::log_1(&format!("[neteq-worker] received message: {:?}", msg).into());
        match msg {
            WorkerMsg::Init {
                sample_rate,
                channels: _,
            } => {
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
                // Issue #1402: actually drain/reset the NetEq buffer. Previously
                // this only logged (the "handled by the NetEq instance" comment was
                // inaccurate — nothing downstream flushed), so a peer audio-off left
                // the buffer emitting expand/hiss concealment on the un-muted path.
                NETEQ.with(|cell| {
                    if let Some(eq) = cell.borrow().as_ref() {
                        eq.flush();
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
                if muted {
                    post_pause();
                }
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

#[cfg(test)]
mod tests {
    use super::{
        emit_frames, pace_tick, pause, LevelBatch, PaceState, WarnLimiter, LEVEL_BATCH_FRAMES,
        WARN_INTERVAL_MS,
    };
    use std::cell::RefCell;

    /// What `pause` posts, in order, with a direct port that answers `direct`.
    fn pause_posts(batch: &mut LevelBatch, direct: Option<bool>) -> Vec<String> {
        let posts = RefCell::new(Vec::new());
        pause(
            batch,
            |level| posts.borrow_mut().push(format!("level {level}")),
            || {
                posts.borrow_mut().push("pause to the worklet".to_string());
                direct
            },
            || {
                posts
                    .borrow_mut()
                    .push("pause to the main thread".to_string())
            },
        );
        posts.into_inner()
    }

    #[test]
    fn a_pause_flushes_the_partial_level_first_and_falls_back_to_the_main_thread() {
        let mut batch = LevelBatch::default();
        batch.push(0.2);
        batch.push(0.7);
        assert_eq!(
            pause_posts(&mut batch, Some(true)),
            ["level 0.7", "pause to the worklet"]
        );
        assert_eq!(batch, LevelBatch::default(), "the next batch starts empty");
        assert_eq!(
            pause_posts(&mut batch, None),
            ["pause to the worklet", "pause to the main thread"],
            "no level when no frame is waiting; no port means the main thread"
        );
        batch.push(0.1);
        assert_eq!(
            pause_posts(&mut batch, Some(false)),
            [
                "level 0.1",
                "pause to the worklet",
                "pause to the main thread"
            ]
        );
    }

    #[test]
    fn dropped_frame_warnings_go_out_at_most_once_a_second_with_a_count() {
        let mut limiter = WarnLimiter::new();
        assert_eq!(limiter.allow(0.0), Some(0), "the first drop is reported");
        let held: Vec<_> = (1..100)
            .map(|i| limiter.allow(f64::from(i) * 10.0))
            .collect();
        assert!(
            held.iter().all(Option::is_none),
            "100 drops/s give one warning"
        );
        assert_eq!(limiter.allow(WARN_INTERVAL_MS), Some(99));
        assert_eq!(limiter.allow(WARN_INTERVAL_MS + 1.0), None);
        assert_eq!(limiter.allow(2.0 * WARN_INTERVAL_MS), Some(1));
    }

    #[test]
    fn a_level_goes_out_once_per_five_frames_carrying_the_loudest() {
        let mut batch = LevelBatch::default();
        let levels = [0.1, 0.4, 0.2, 0.0, 0.3, 0.05, 0.06];
        let posted: Vec<_> = levels.iter().map(|&rms| batch.push(rms)).collect();
        assert_eq!(posted, [None, None, None, None, Some(0.4), None, None]);
        assert_eq!(
            LEVEL_BATCH_FRAMES, 5,
            "100 frames/s / 5 = 20 level messages/s"
        );
        let per_second = (0..100).filter_map(|_| batch.push(0.01)).count();
        assert_eq!(per_second, 20);
    }

    #[test]
    fn a_pause_flushes_the_partial_batch_once() {
        let mut batch = LevelBatch::default();
        assert_eq!(batch.flush(), None, "nothing waiting, nothing sent");
        batch.push(0.2);
        batch.push(0.7);
        assert_eq!(batch.flush(), Some(0.7));
        assert_eq!(batch.flush(), None);
        assert_eq!(batch.push(0.1), None, "a flushed batch starts over");
    }

    const START: f64 = 1_700_000_000_007.0;

    /// Returns frames emitted and per-tick `(behind, emitted)`.
    fn run_with(
        tick_times_ms: &[f64],
        muted: bool,
        mut pull: impl FnMut() -> bool,
    ) -> (u64, Vec<(u64, u64)>) {
        let mut state = PaceState::start(START, muted, &mut pull);
        let mut emitted = state.produced;
        let mut trace = Vec::new();
        for &t in tick_times_ms {
            let (behind, n) = state.tick(START + t, muted, &mut pull);
            emitted += n;
            trace.push((behind, n));
        }
        (emitted, trace)
    }

    fn state(produced: u64, last_ms: f64) -> PaceState {
        PaceState {
            start_ms: START,
            produced,
            last_production_ms: START + last_ms,
        }
    }

    #[test]
    fn start_pulls_one_frame_unless_muted() {
        for (muted, ok, produced, pulls) in [
            (false, true, 1, 1),
            (false, false, 0, 1),
            (true, true, 0, 0),
        ] {
            let mut n = 0;
            let s = PaceState::start(START, muted, || {
                n += 1;
                ok
            });
            assert_eq!(
                (s, n),
                (state(produced, 0.0), pulls),
                "muted={muted} ok={ok}"
            );
        }
    }

    #[test]
    fn tick_emits_owed_frames_up_to_cap_and_stores_counter() {
        let mut s = state(1, 0.0);
        let mut pulls = 0;
        let out = s.tick(START + 30.0, false, || {
            pulls += 1;
            true
        });
        assert_eq!((out, pulls, s), ((2, 2), 2, state(3, 30.0)));
    }

    #[test]
    fn tick_stops_at_failed_pull() {
        let mut s = state(1, 0.0);
        let mut pulls = 0;
        let out = s.tick(START + 30.0, false, || {
            pulls += 1;
            false
        });
        assert_eq!((out, pulls, s), ((2, 0), 1, state(1, 0.0)));
    }

    #[test]
    fn tick_stores_forgiven_backlog() {
        let mut s = state(1, 0.0);
        assert_eq!(s.tick(START + 1_000.0, false, || true), (10, 2));
        assert_eq!(s, state(92, 1_000.0));
    }

    #[test]
    fn tick_fallback_uses_stored_last_production_time() {
        let mut s = state(2, 10.0);
        assert_eq!(s.tick(START + 20.0, false, || true), (0, 1));
        assert_eq!(s, state(3, 20.0));
        assert_eq!(s.tick(START + 25.0, false, || true), (0, 0));
        let mut s = state(2, 10.1);
        assert_eq!(s.tick(START + 20.0, false, || true), (0, 0));
    }

    #[test]
    fn muted_ticks_keep_counter_in_step_through_unmute() {
        let mut s = state(1, 0.0);
        assert_eq!(s.tick(START + 1_000.0, true, || panic!("pulled")), (0, 0));
        assert_eq!(s, state(100, 0.0));
        assert_eq!(s.tick(START + 1_005.0, false, || true), (0, 1));
        assert_eq!(s, state(101, 1_005.0));
    }

    fn run(tick_times_ms: &[f64], muted: bool) -> (u64, Vec<(u64, u64)>) {
        run_with(tick_times_ms, muted, || true)
    }

    fn ticks(from_ms: f64, to_ms: f64, step_ms: f64) -> Vec<f64> {
        (1..)
            .map(|i| from_ms + i as f64 * step_ms)
            .take_while(|&t| t <= to_ms)
            .collect()
    }

    #[test]
    fn five_ms_ticks_produce_one_frame_at_a_time_at_100_per_sec() {
        let (emitted, trace) = run(&ticks(0.0, 10_000.0, 5.0), false);
        assert_eq!(emitted, 1001);
        assert!(trace.iter().all(|&(_, n)| n <= 1));
    }

    #[test]
    fn throttled_timer_still_supplies_100_frames_per_sec() {
        let mut pulls = 0;
        let (emitted, _) = run_with(&ticks(0.0, 10_000.0, 10.5), false, || {
            pulls += 1;
            true
        });
        assert!(emitted >= 990, "only {emitted} frames in 10 s");
        assert!(pulls >= 990, "only {pulls} pulls in 10 s");
    }

    #[test]
    fn emit_frames_pulls_every_frame_it_is_asked_for() {
        let (mut produced, mut last, mut pulls) = (1, START, 0);
        let n = emit_frames(2, START + 30.0, &mut produced, &mut last, || {
            pulls += 1;
            true
        });
        assert_eq!((n, pulls, produced, last), (2, 2, 3, START + 30.0));
    }

    #[test]
    fn emit_frames_stops_at_first_failed_pull() {
        for (fail_on, expected) in [(1, (0, 1, 1, START)), (2, (1, 2, 2, START + 30.0))] {
            let (mut produced, mut last, mut pulls) = (1, START, 0);
            let n = emit_frames(2, START + 30.0, &mut produced, &mut last, || {
                pulls += 1;
                pulls != fail_on
            });
            assert_eq!((n, pulls, produced, last), expected, "fail_on={fail_on}");
        }
    }

    #[test]
    fn stall_forgives_backlog_and_recovers_without_flooding() {
        let mut times = ticks(0.0, 1_000.0, 5.0);
        times.push(1_155.0);
        let stall = times.len() - 1;
        times.extend(ticks(1_155.0, 3_000.0, 5.0));
        let (_, trace) = run(&times, false);
        assert!(trace
            .iter()
            .all(|&(behind, n)| n <= behind.max(1) && n <= 2));
        assert_eq!(trace[stall].0, 10, "owed backlog after the stall");

        let caught_up = (stall..trace.len()).find(|&i| trace[i].0 == 0).unwrap();
        assert!(caught_up - stall <= 10, "took {} ticks", caught_up - stall);
        let last_second: u64 = trace[trace.len() - 200..].iter().map(|&(_, n)| n).sum();
        assert_eq!(last_second, 100);
    }

    #[test]
    fn muted_ticks_keep_counter_in_sync_and_produce_nothing() {
        for (t, expected) in [(5.0, 0), (155.0, 15), (1_000.0, 100), (10_007.0, 1_000)] {
            assert_eq!(
                pace_tick(START + t, START, 1, START, true),
                (expected, 0, 0)
            );
        }
        let (_, trace) = run(&ticks(0.0, 2_000.0, 5.0), true);
        assert!(trace.iter().all(|&(_, n)| n == 0));
    }

    #[test]
    fn single_owed_frame_and_ten_ms_fallback_boundaries() {
        assert_eq!(
            pace_tick(START + 21.0, START, 1, START + 15.0, false),
            (1, 1, 1)
        );
        assert_eq!(
            pace_tick(START + 20.0, START, 2, START + 10.1, false),
            (2, 0, 0)
        );
        assert_eq!(
            pace_tick(START + 20.0, START, 2, START + 10.0, false),
            (2, 0, 1)
        );
    }
}
