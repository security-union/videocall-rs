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

//! NetEq initialization logic

use super::messages::WorkerResponse;
use super::state;
use crate::WebNetEq;
use wasm_bindgen::JsCast;
use web_sys::{console, DedicatedWorkerGlobalScope};

const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_CHANNELS: u8 = 1;

/// Load opus-decoder library in worker context
pub fn load_opus_decoder(global_scope: &DedicatedWorkerGlobalScope) {
    let opus_decoder_script = include_str!("../scripts/opus-decoder.min.js");

    let eval_fn = match js_sys::Reflect::get(global_scope, &wasm_bindgen::JsValue::from_str("eval"))
    {
        Ok(f) => f,
        Err(e) => {
            console::error_2(&"[neteq-worker] Failed to get eval function:".into(), &e);
            return;
        }
    };

    if !eval_fn.is_function() {
        console::error_1(&"[neteq-worker] eval is not a function".into());
        return;
    }

    let eval_function = eval_fn.unchecked_into::<js_sys::Function>();
    match eval_function.call1(
        global_scope,
        &wasm_bindgen::JsValue::from_str(opus_decoder_script),
    ) {
        Ok(_) => {
            console::log_1(&"[neteq-worker] Successfully loaded opus-decoder library".into());
        }
        Err(e) => {
            console::warn_2(&"[neteq-worker] Failed to load opus-decoder:".into(), &e);
        }
    }
}

/// Initialize NetEq with default settings (48kHz mono)
pub fn initialize_neteq() {
    if state::is_neteq_initialized() {
        return;
    }

    let neteq = match WebNetEq::new(DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS) {
        Ok(eq) => eq,
        Err(e) => {
            console::error_2(&"[neteq-worker] WebNetEq::new error:".into(), &e);
            return;
        }
    };

    wasm_bindgen_futures::spawn_local(async move {
        initialize_neteq_async(neteq).await;
    });
}

/// Async initialization of NetEq
async fn initialize_neteq_async(neteq: WebNetEq) {
    if let Err(e) = neteq.init().await {
        console::error_2(&"[neteq-worker] auto-init error:".into(), &e);
        return;
    }

    state::store_neteq(neteq);
    log_initialization_success();
    send_worker_ready_message();
}

/// Log successful initialization
fn log_initialization_success() {
    console::log_1(
        &format!(
            "[neteq-worker] NetEq auto-initialised ({} Hz/{} ch)",
            DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS
        )
        .into(),
    );

    let is_muted = state::is_muted();
    console::log_1(&format!("🔇 NetEq worker auto-initialized with muted: {}", is_muted).into());
}

/// Send WorkerReady message to main thread
fn send_worker_ready_message() {
    let is_muted = state::is_muted();
    let ready_msg = WorkerResponse::WorkerReady {
        mute_state: is_muted,
    };

    let js_msg = match serde_wasm_bindgen::to_value(&ready_msg) {
        Ok(msg) => msg,
        Err(_) => {
            console::error_1(&"❌ Failed to serialize WorkerReady message".into());
            return;
        }
    };

    let result = js_sys::global()
        .unchecked_into::<DedicatedWorkerGlobalScope>()
        .post_message(&js_msg);

    if result.is_ok() {
        console::log_1(&"✅ Sent WorkerReady confirmation to main thread".into());
    } else {
        console::error_1(&"❌ Failed to post WorkerReady message".into());
    }
}
