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

//! NetEq worker implementation for browser

mod initialization;
mod messages;
mod state;
mod timing;

use log::LevelFilter;
#[cfg(feature = "matomo-logger")]
use matomo_logger::worker as matomo_worker;
use messages::WorkerMsg;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{console, DedicatedWorkerGlobalScope, MessageEvent};

/// Initialize and start the neteq worker
pub fn start_worker() {
    console_error_panic_hook::set_once();
    console::log_1(&"[neteq-worker] starting".into());

    initialize_logging();

    let global_scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();

    initialization::load_opus_decoder(&global_scope);
    setup_message_handler(&global_scope);
    initialization::initialize_neteq();
    setup_stats_interval(&global_scope);
    setup_audio_production_timer(&global_scope);
}

/// Initialize worker logging
fn initialize_logging() {
    #[cfg(feature = "matomo-logger")]
    {
        let bridge_fn = js_sys::Function::new_no_args("self.postMessage(arguments[0]);");
        if let Err(_e) =
            matomo_worker::init_with_bridge(LevelFilter::Info, LevelFilter::Warn, bridge_fn)
        {
            console::error_1(&"[neteq-worker] Failed to initialize matomo worker bridge".into());
        }
    }
}

/// Setup message handler for worker
fn setup_message_handler(scope: &DedicatedWorkerGlobalScope) {
    let scope_clone = scope.clone();
    let on_message = Closure::wrap(Box::new(move |evt: MessageEvent| {
        match serde_wasm_bindgen::from_value::<WorkerMsg>(evt.data()) {
            Ok(msg) => handle_message(&scope_clone, msg),
            Err(e) => console::error_1(&format!("[neteq-worker] bad msg: {:?}", e).into()),
        }
    }) as Box<dyn FnMut(_)>);

    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
}

/// Setup statistics reporting interval (1 Hz)
fn setup_stats_interval(scope: &DedicatedWorkerGlobalScope) {
    let stats_cb = Closure::wrap(Box::new(|| {
        if !state::is_diagnostics_enabled() {
            return;
        }

        publish_statistics();
    }) as Box<dyn FnMut()>);

    let _ = scope.set_interval_with_callback_and_timeout_and_arguments_0(
        stats_cb.as_ref().unchecked_ref(),
        1000,
    );
    stats_cb.forget();
}

/// Setup audio production timer
fn setup_audio_production_timer(scope: &DedicatedWorkerGlobalScope) {
    let audio_cb = Closure::wrap(Box::new(|| {
        produce_audio_frame();
    }) as Box<dyn FnMut()>);

    let _ = scope.set_interval_with_callback_and_timeout_and_arguments_0(
        audio_cb.as_ref().unchecked_ref(),
        timing::AUDIO_PRODUCTION_INTERVAL_MS,
    );
    audio_cb.forget();
}

/// Publish statistics to main thread
fn publish_statistics() {
    let stats_result = state::with_neteq(|eq| eq.get_statistics());

    let js_val = match stats_result {
        Some(Ok(val)) => val,
        _ => return,
    };

    let obj = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("cmd"), &JsValue::from_str("stats"));
    let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("stats"), &js_val);

    let _ = js_sys::global()
        .unchecked_into::<DedicatedWorkerGlobalScope>()
        .post_message(&obj);
}

/// Produce and send an audio frame
fn produce_audio_frame() {
    let is_muted = state::is_muted();
    let now = js_sys::Date::now();

    let mut timing = timing::get_timing_state();

    // Initialize on first call
    if timing.is_uninitialized() {
        handle_first_audio_frame(now, is_muted, &mut timing);
        timing::set_timing_state(timing);
        return;
    }

    // Calculate timing metrics
    let total_elapsed_ms = now - timing.start_time;
    let interval_since_last =
        timing::calculate_interval_since_last(now, timing.last_production_time);
    let expected_frames = (total_elapsed_ms / timing::FRAME_DURATION_MS) as u64;
    let frames_behind =
        timing::calculate_frames_behind(total_elapsed_ms, timing.total_frames_produced);

    // Sync frame count when muted
    if is_muted {
        timing.sync_muted_frames(expected_frames);
        timing::set_timing_state(timing);
        return;
    }

    // Log timing stats periodically
    if timing::should_log_timing_stats(&timing, now) {
        timing::log_timing_stats(&timing, is_muted);
        timing.update_last_log(now);
    }

    // Produce audio if needed - with multi-frame catch-up
    if timing::should_produce_audio_frame(frames_behind, interval_since_last, is_muted) {
        // Limit catch-up to prevent audio thread starvation
        let max_catchup_frames = timing::max_catchup_frames(frames_behind);
        let mut frames_produced = 0;

        for _ in 0..max_catchup_frames {
            if produce_and_send_audio(&mut timing, now, frames_behind) {
                frames_produced += 1;
            } else {
                // NetEq has no more audio available
                break;
            }
        }

        if frames_produced > 1 {
            log::debug!(
                "⚡ NetEq catch-up: produced {} frames (was {} behind)",
                frames_produced,
                frames_behind
            );
        }
    }

    timing::set_timing_state(timing);
}

/// Handle first audio frame production
fn handle_first_audio_frame(now: f64, is_muted: bool, timing: &mut timing::TimingState) {
    *timing = timing::TimingState::initialize(now);
    console::log_1(
        &format!(
            "🎵 NetEq: Starting audio production timer ({}ms interval)",
            timing::AUDIO_PRODUCTION_INTERVAL_MS
        )
        .into(),
    );

    if !is_muted {
        produce_and_send_audio(timing, now, 0);
    }
}

/// Produce audio from NetEq and send to main thread
/// Returns true if a frame was successfully produced, false otherwise
fn produce_and_send_audio(timing: &mut timing::TimingState, now: f64, frames_behind: i32) -> bool {
    let pcm_result = state::with_neteq(|eq| eq.get_audio());

    let pcm = match pcm_result {
        Some(Ok(audio)) => audio,
        Some(Err(_)) => {
            return false;
        }
        None => return false,
    };

    timing.record_frame_production(now);

    if frames_behind > 1 {
        timing.record_timing_adjustment();
    }

    let sab = js_sys::Array::of1(&pcm.buffer());
    let _ = js_sys::global()
        .unchecked_into::<DedicatedWorkerGlobalScope>()
        .post_message_with_transfer(&pcm, &sab);

    true
}

/// Handle incoming messages from main thread
fn handle_message(scope: &DedicatedWorkerGlobalScope, msg: WorkerMsg) {
    match msg {
        WorkerMsg::Insert {
            seq,
            timestamp,
            payload,
        } => handle_insert_packet(seq, timestamp, &payload),
        WorkerMsg::Flush => handle_flush(),
        WorkerMsg::Clear => handle_clear(),
        WorkerMsg::Close => handle_close(scope),
        WorkerMsg::Mute { muted } => handle_mute(muted),
        WorkerMsg::SetDiagnostics { enabled } => handle_set_diagnostics(enabled),
    }
}

/// Handle packet insertion
fn handle_insert_packet(seq: u16, timestamp: u32, payload: &[u8]) {
    state::with_neteq(|eq| {
        if let Err(e) = eq.insert_packet(seq, timestamp, payload) {
            console::error_1(&format!("[neteq-worker] insert_packet error: {:?}", e).into());
        }
    });
}

/// Handle flush command
fn handle_flush() {
    if state::is_neteq_initialized() {
        console::log_1(&"[neteq-worker] flush".into());
    }
}

/// Handle clear command
fn handle_clear() {
    state::clear_neteq();
}

/// Handle close command
fn handle_close(scope: &DedicatedWorkerGlobalScope) {
    scope.close();
}

/// Handle mute/unmute command
fn handle_mute(muted: bool) {
    let old_state = state::is_muted();
    state::set_muted(muted);
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
}

/// Handle diagnostics enable/disable command
fn handle_set_diagnostics(enabled: bool) {
    state::set_diagnostics_enabled(enabled);
    console::log_2(
        &"[neteq-worker] diagnostics enabled:".into(),
        &JsValue::from_bool(enabled),
    );
}
