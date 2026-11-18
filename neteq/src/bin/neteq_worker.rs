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

//! NetEq worker for the browser. One worker per remote peer.
//!
//! Message protocol:
//!
//! - Main thread to worker (`WorkerMsg`, serde, tagged by `cmd`): `init`,
//!   `insert`, `flush`, `clear`, `close`, `mute`, `setDiagnostics`, and
//!   `attachWorklet`, which carries a transferred `MessagePort` whose other
//!   end belongs to the peer's AudioWorklet.
//! - Worker to main thread: `{type: "workerReady", mute_state}` once,
//!   `{type: "audioLevel", rms}` every 50 ms while audio plays,
//!   `{type: "stats", json, buffer_ms, jitter_buffer_delay_ms,
//!   jitter_buffer_target_delay_ms, packets_awaiting_decode}` (see
//!   [`post_stats`]), and the Matomo log bridge messages (WARN and above).
//!   `json` is the `telemetry::NetEqStats` schema, already a string.
//! - Worklet to worker, over the port: "I need `n` more 10 ms frames", sent
//!   either as the number `n` or as a `Float32Array` whose element 0 holds
//!   `n`. The array is a buffer the worker sent earlier, handed back so the
//!   worker can reuse it. The worker runs `GetAudio` `n` times and answers
//!   with either a `Float32Array` of `n` frames (its buffer transferred) or
//!   the number `n`, meaning `n` frames of silence.
//!
//! Recycling the reply buffers means steady playback allocates no sample
//! memory on either side: the same couple of `ArrayBuffer`s go back and
//! forth. SharedArrayBuffer would avoid even the messages, but it needs
//! cross-origin isolation (COOP/COEP), which the app does not enable.
//!
//! `GetAudio` is paced by the worklet's demand, which is the audio device
//! clock, instead of a free-running timer. NetEq measures arrival delay in
//! `GetAudio` ticks, so a timer drifting from 10 ms would skew its delay
//! estimate (PARITY_PLAN.md 8.4). The worklet asks for two or three frames
//! at a time, so ticks come in small bursts, but their count always equals
//! the audio played.
#![cfg_attr(target_arch = "wasm32", no_main)]

#[cfg(target_arch = "wasm32")]
mod wasm_worker {
    use std::cell::RefCell;

    use neteq::web::{init_net_eq, OPUS_SAMPLE_RATE_HZ};
    use neteq::WebNetEq;
    use serde::{Deserialize, Serialize};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{console, DedicatedWorkerGlobalScope, MessageEvent, MessagePort};

    /// Cap on frames per pull. The worklet asks for at most a few frames;
    /// this only bounds the work after a long stall.
    const MAX_FRAMES_PER_PULL: u32 = 8;

    /// Reply buffers kept for reuse. Steady playback cycles one or two.
    const MAX_POOLED_BUFFERS: usize = 4;

    /// A gap between pulls longer than this means playback stopped (the
    /// AudioContext was suspended, or the worklet just attached). The packets
    /// queued meanwhile are stale, so NetEq is flushed and restarts at the
    /// next packet instead of slowly accelerating through seconds of backlog.
    const STALL_RESET_MS: f64 = 500.0;

    /// Frames per audio level report: 50 ms.
    const LEVEL_WINDOW_FRAMES: u32 = 5;

    /// Stats timer period.
    const STATS_INTERVAL_MS: i32 = 1000;

    /// With diagnostics off, stats go out on every Nth timer tick (5 s).
    /// That still feeds the health reporter, which samples every 5 s by
    /// default, without paying for 1 Hz updates nobody is looking at.
    const STATS_TICKS_WHEN_HIDDEN: u32 = 5;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "cmd", rename_all = "camelCase")]
    enum WorkerMsg {
        Init {
            sample_rate: u32,
            channels: u8,
        },
        /// Insert an encoded packet. `timestamp` is in 48 kHz samples.
        Insert {
            seq: u16,
            timestamp: u32,
            #[serde(with = "serde_bytes")]
            payload: Vec<u8>,
        },
        /// Drop all buffered audio (the peer stopped sending).
        Flush,
        /// Replace NetEq with a fresh instance.
        Clear,
        Close,
        /// The peer's microphone is off: keep NetEq running, play silence.
        Mute {
            muted: bool,
        },
        /// The diagnostics UI is open: post stats every second instead of
        /// every [`STATS_TICKS_WHEN_HIDDEN`] seconds.
        SetDiagnostics {
            enabled: bool,
        },
        /// The message's first transferred port leads to the AudioWorklet.
        AttachWorklet,
    }

    /// Messages sent from worker back to main thread
    #[derive(Debug, Serialize)]
    #[serde(tag = "type", rename_all = "camelCase")]
    enum WorkerResponse {
        WorkerReady { mute_state: bool },
        AudioLevel { rms: f32 },
    }

    struct State {
        neteq: WebNetEq,
        muted: bool,
        diagnostics: bool,
        stats_ticks: u32,
        port: Option<MessagePort>,
        /// Keeps the port's `onmessage` closure alive while attached.
        _port_handler: Option<Closure<dyn FnMut(MessageEvent)>>,
        last_pull_ms: Option<f64>,
        /// Reused output buffer for one pull.
        pcm: Vec<f32>,
        /// Reply buffers the worklet handed back.
        pool: Vec<js_sys::ArrayBuffer>,
        /// Reused one-element transfer list for replies.
        transfer: js_sys::Array,
        level_peak: f32,
        level_frames: u32,
        last_level_posted: f32,
        get_audio_errors: u32,
    }

    impl State {
        fn new(neteq: WebNetEq) -> Self {
            let capacity = neteq.frame_len() * MAX_FRAMES_PER_PULL as usize;
            Self {
                neteq,
                muted: true, // Start muted until the main thread says otherwise.
                diagnostics: false,
                stats_ticks: 0,
                port: None,
                _port_handler: None,
                last_pull_ms: None,
                pcm: Vec::with_capacity(capacity),
                pool: Vec::with_capacity(MAX_POOLED_BUFFERS),
                transfer: js_sys::Array::new_with_length(1),
                level_peak: 0.0,
                level_frames: 0,
                last_level_posted: 0.0,
                get_audio_errors: 0,
            }
        }

        /// A reply buffer of at least `bytes` bytes: a pooled one if one is
        /// big enough, else a new one. Pooled buffers too small are dropped.
        fn take_buffer(&mut self, bytes: usize) -> js_sys::ArrayBuffer {
            while let Some(buffer) = self.pool.pop() {
                if buffer.byte_length() as usize >= bytes {
                    return buffer;
                }
            }
            js_sys::ArrayBuffer::new(bytes as u32)
        }

        /// Keep a buffer the worklet returned, up to [`MAX_POOLED_BUFFERS`].
        fn recycle(&mut self, buffer: js_sys::ArrayBuffer) {
            if self.pool.len() < MAX_POOLED_BUFFERS {
                self.pool.push(buffer);
            }
        }

        /// Answer a worklet pull of `frames` 10 ms frames.
        fn pull(&mut self, frames: u32) {
            let now = js_sys::Date::now();
            let stalled = self
                .last_pull_ms
                .is_none_or(|last| now - last > STALL_RESET_MS);
            self.last_pull_ms = Some(now);
            if stalled {
                self.neteq.flush();
            }

            let frame_len = self.neteq.frame_len();
            self.pcm.clear();
            let mut audible = false;
            for _ in 0..frames {
                let start = self.pcm.len();
                let frame_audible = match self.neteq.get_audio_into(&mut self.pcm) {
                    Ok(frame_audible) => frame_audible,
                    Err(e) => {
                        // Rate-limited: one log per 100 errors (1 s of ticks).
                        if self.get_audio_errors.is_multiple_of(100) {
                            log::warn!("[neteq-worker] get_audio error: {e}");
                        }
                        self.get_audio_errors = self.get_audio_errors.wrapping_add(1);
                        false
                    }
                };
                let frame_audible = frame_audible && !self.muted;
                audible |= frame_audible;
                let rms = if frame_audible {
                    rms(&self.pcm[start..start + frame_len])
                } else {
                    0.0
                };
                self.track_level(rms);
            }

            if self.port.is_none() {
                return;
            }
            let posted = if audible {
                let samples = self.pcm.len();
                let buffer = self.take_buffer(samples * std::mem::size_of::<f32>());
                let pcm = js_sys::Float32Array::new_with_byte_offset_and_length(
                    &buffer,
                    0,
                    samples as u32,
                );
                pcm.copy_from(&self.pcm);
                self.transfer.set(0, buffer.into());
                let posted = self
                    .port
                    .as_ref()
                    .map(|port| port.post_message_with_transferable(&pcm, &self.transfer));
                // Do not keep the (now detached) buffer alive from here.
                self.transfer.set(0, JsValue::UNDEFINED);
                posted
            } else {
                self.port
                    .as_ref()
                    .map(|port| port.post_message(&JsValue::from_f64(f64::from(frames))))
            };
            if let Some(Err(e)) = posted {
                console::error_2(&"[neteq-worker] post to worklet failed:".into(), &e);
            }
        }

        /// Fold one frame's RMS into the level window; post at window end.
        fn track_level(&mut self, rms: f32) {
            self.level_peak = self.level_peak.max(rms);
            self.level_frames += 1;
            if self.level_frames < LEVEL_WINDOW_FRAMES {
                return;
            }
            let peak = self.level_peak;
            self.level_peak = 0.0;
            self.level_frames = 0;
            // Silence after silence carries no news; skip it.
            if peak == 0.0 && self.last_level_posted == 0.0 {
                return;
            }
            self.last_level_posted = peak;
            post_response(&WorkerResponse::AudioLevel { rms: peak });
        }
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_squares: f32 = samples.iter().map(|sample| sample * sample).sum();
        (sum_squares / samples.len() as f32).sqrt()
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    fn scope() -> DedicatedWorkerGlobalScope {
        js_sys::global().unchecked_into()
    }

    fn post_response(response: &WorkerResponse) {
        match serde_wasm_bindgen::to_value(response) {
            Ok(js) => {
                if let Err(e) = scope().post_message(&js) {
                    console::error_2(&"[neteq-worker] post to main failed:".into(), &e);
                }
            }
            Err(e) => console::error_1(&format!("[neteq-worker] serialize: {e:?}").into()),
        }
    }

    fn new_neteq() -> Option<WebNetEq> {
        match WebNetEq::try_new(OPUS_SAMPLE_RATE_HZ, 1) {
            Ok(neteq) => Some(neteq),
            Err(e) => {
                console::error_1(&format!("[neteq-worker] NetEq init failed: {e}").into());
                None
            }
        }
    }

    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
        init_net_eq();

        let self_scope = scope();
        let on_message = Closure::wrap(Box::new(move |evt: MessageEvent| {
            match serde_wasm_bindgen::from_value::<WorkerMsg>(evt.data()) {
                Ok(msg) => handle_message(msg, &evt),
                Err(e) => console::error_1(&format!("[neteq-worker] bad msg: {e:?}").into()),
            }
        }) as Box<dyn FnMut(_)>);
        self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();

        // NetEq is built eagerly (48 kHz mono Opus); `init` is accepted for
        // protocol compatibility but changes nothing.
        let Some(neteq) = new_neteq() else {
            return;
        };
        let state = State::new(neteq);
        let mute_state = state.muted;
        STATE.with(|cell| *cell.borrow_mut() = Some(state));
        log::info!("[neteq-worker] NetEq ready (48 kHz mono Opus, ropus decoder)");
        post_response(&WorkerResponse::WorkerReady { mute_state });

        let stats_cb = Closure::wrap(Box::new(on_stats_tick) as Box<dyn FnMut()>);
        if let Err(e) = self_scope.set_interval_with_callback_and_timeout_and_arguments_0(
            stats_cb.as_ref().unchecked_ref(),
            STATS_INTERVAL_MS,
        ) {
            console::error_2(&"[neteq-worker] stats timer failed:".into(), &e);
        }
        stats_cb.forget();
    }

    /// Every second: post stats if diagnostics are on, else every
    /// [`STATS_TICKS_WHEN_HIDDEN`] ticks.
    fn on_stats_tick() {
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            let Some(state) = state.as_mut() else {
                return;
            };
            state.stats_ticks = state.stats_ticks.wrapping_add(1);
            if state.diagnostics || state.stats_ticks.is_multiple_of(STATS_TICKS_WHEN_HIDDEN) {
                post_stats(state);
            }
        });
    }

    /// Post `{type: "stats", json, ...}`. `json` is the full
    /// `telemetry::NetEqStats` serialized here, off the main thread; the
    /// numeric fields repeat the four values the main thread turns into
    /// metrics, so it never has to parse `json`.
    fn post_stats(state: &State) {
        let stats = state.neteq.stats();
        let json = match serde_json::to_string(&stats) {
            Ok(json) => json,
            Err(e) => {
                console::error_1(&format!("[neteq-worker] stats serialize: {e}").into());
                return;
            }
        };
        let obj = js_sys::Object::new();
        let fields: [(&str, JsValue); 6] = [
            ("type", "stats".into()),
            ("json", json.into()),
            (
                "buffer_ms",
                f64::from(stats.network.current_buffer_size_ms).into(),
            ),
            (
                "jitter_buffer_delay_ms",
                (stats.lifetime.jitter_buffer_delay_ms as f64).into(),
            ),
            (
                "jitter_buffer_target_delay_ms",
                (stats.lifetime.jitter_buffer_target_delay_ms as f64).into(),
            ),
            (
                "packets_awaiting_decode",
                (stats.packets_awaiting_decode as f64).into(),
            ),
        ];
        for (key, value) in fields {
            let _ = js_sys::Reflect::set(&obj, &key.into(), &value);
        }
        if let Err(e) = scope().post_message(&obj) {
            console::error_2(&"[neteq-worker] post stats failed:".into(), &e);
        }
    }

    /// A worklet pull: the number `n`, or a returned buffer carrying `n` in
    /// element 0.
    fn on_worklet_message(evt: MessageEvent) {
        let data = evt.data();
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            let Some(state) = state.as_mut() else {
                return;
            };
            let frames = if let Some(frames) = data.as_f64() {
                frames
            } else if let Some(returned) = data.dyn_ref::<js_sys::Float32Array>() {
                let frames = if returned.length() > 0 {
                    f64::from(returned.get_index(0))
                } else {
                    f64::NAN
                };
                state.recycle(returned.buffer());
                frames
            } else {
                return;
            };
            // NaN and values below one frame are ignored.
            if frames.is_nan() || frames < 1.0 {
                return;
            }
            state.pull((frames as u32).min(MAX_FRAMES_PER_PULL));
        });
    }

    fn attach_worklet(state: &mut State, evt: &MessageEvent) {
        let Some(port) = evt.ports().get(0).dyn_into::<MessagePort>().ok() else {
            console::error_1(&"[neteq-worker] attachWorklet without a port".into());
            return;
        };
        if let Some(old) = state.port.take() {
            old.set_onmessage(None);
            old.close();
        }
        let handler = Closure::wrap(Box::new(on_worklet_message) as Box<dyn FnMut(_)>);
        port.set_onmessage(Some(handler.as_ref().unchecked_ref()));
        state.port = Some(port);
        state._port_handler = Some(handler);
        state.last_pull_ms = None;
        log::info!("[neteq-worker] AudioWorklet attached");
    }

    fn handle_message(msg: WorkerMsg, evt: &MessageEvent) {
        if let WorkerMsg::Close = msg {
            STATE.with(|cell| {
                if let Some(port) = cell.borrow_mut().as_mut().and_then(|s| s.port.take()) {
                    port.set_onmessage(None);
                    port.close();
                }
            });
            scope().close();
            return;
        }
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            let Some(state) = state.as_mut() else {
                return;
            };
            match msg {
                WorkerMsg::Init { .. } | WorkerMsg::Close => {}
                WorkerMsg::Insert {
                    seq,
                    timestamp,
                    payload,
                } => {
                    if let Err(e) = state.neteq.insert(seq, timestamp, &payload) {
                        log::debug!("[neteq-worker] insert_packet seq {seq}: {e}");
                    }
                }
                WorkerMsg::Flush => state.neteq.flush(),
                WorkerMsg::Clear => {
                    if let Some(neteq) = new_neteq() {
                        state.neteq = neteq;
                    }
                }
                WorkerMsg::Mute { muted } => {
                    if state.muted != muted {
                        log::info!("[neteq-worker] muted: {} -> {muted}", state.muted);
                    }
                    state.muted = muted;
                }
                WorkerMsg::SetDiagnostics { enabled } => {
                    let opened = enabled && !state.diagnostics;
                    state.diagnostics = enabled;
                    // Fill the panel now rather than up to a second later.
                    if opened {
                        post_stats(state);
                    }
                }
                WorkerMsg::AttachWorklet => attach_worklet(state, evt),
            }
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("neteq_worker is only compiled for wasm32 target");
}
