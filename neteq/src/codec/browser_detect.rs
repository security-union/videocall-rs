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

//! Browser capability detection for audio decoding
//!
//! Works in both main thread and Web Worker contexts

use js_sys::Reflect;
use std::sync::OnceLock;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

static IS_IOS: OnceLock<bool> = OnceLock::new();

/// Get the global object (works in both window and worker contexts)
fn get_global() -> JsValue {
    js_sys::global().into()
}

/// Get navigator object (works in both window and worker contexts)
fn get_navigator() -> Option<web_sys::Navigator> {
    let global = get_global();

    // Try to get navigator from global scope
    match Reflect::get(&global, &JsValue::from_str("navigator")) {
        Ok(nav_val) if !nav_val.is_undefined() => nav_val.dyn_into::<web_sys::Navigator>().ok(),
        _ => None,
    }
}

/// Detects if the current environment is iOS/Safari
pub fn is_ios() -> bool {
    *IS_IOS.get_or_init(|| {
        if let Some(navigator) = get_navigator() {
            if let Ok(ua) = navigator.user_agent() {
                let ua_lower = ua.to_lowercase();
                let is_ios = ua_lower.contains("iphone")
                    || ua_lower.contains("ipad")
                    || ua_lower.contains("ipod")
                    || (ua_lower.contains("safari") && !ua_lower.contains("chrome"));

                log::info!("Platform detection: UA='{ua}', is_ios={is_ios}");
                return is_ios;
            }
        }
        log::warn!("Platform detection: Could not access navigator, assuming non-iOS");
        false
    })
}

/// Checks if WebCodecs AudioDecoder constructor is available
/// Note: This only checks if the API exists, not if Opus is supported
/// Actual Opus support is verified during decoder initialization
fn has_webcodecs_api() -> bool {
    let global = get_global();

    // Check if AudioDecoder exists (available in both window and workers)
    if let Ok(true) = Reflect::has(&global, &JsValue::from_str("AudioDecoder")) {
        if let Ok(constructor) = Reflect::get(&global, &JsValue::from_str("AudioDecoder")) {
            if !constructor.is_undefined() && !constructor.is_null() {
                return true;
            }
        }
    }
    false
}

/// Async function to check if WebCodecs AudioDecoder supports Opus codec
/// Uses the AudioDecoder.isConfigSupported() API
pub async fn check_webcodecs_opus_support() -> bool {
    let global = get_global();

    // Get AudioDecoder constructor
    let audio_decoder = match Reflect::get(&global, &JsValue::from_str("AudioDecoder")) {
        Ok(ad) if !ad.is_undefined() && !ad.is_null() => ad,
        _ => {
            log::info!("WebCodecs AudioDecoder not available");
            return false;
        }
    };

    // Get isConfigSupported static method
    let is_config_supported =
        match Reflect::get(&audio_decoder, &JsValue::from_str("isConfigSupported")) {
            Ok(method) if method.is_function() => method,
            _ => {
                log::warn!("AudioDecoder.isConfigSupported not available");
                return false;
            }
        };

    // Create test config for Opus
    let test_config = js_sys::Object::new();
    if Reflect::set(
        &test_config,
        &JsValue::from_str("codec"),
        &JsValue::from_str("opus"),
    )
    .is_err()
    {
        return false;
    }
    if Reflect::set(
        &test_config,
        &JsValue::from_str("sampleRate"),
        &JsValue::from_f64(48000.0),
    )
    .is_err()
    {
        return false;
    }
    if Reflect::set(
        &test_config,
        &JsValue::from_str("numberOfChannels"),
        &JsValue::from_f64(1.0),
    )
    .is_err()
    {
        return false;
    }

    // Call AudioDecoder.isConfigSupported(config)
    let is_config_fn = match is_config_supported.dyn_into::<js_sys::Function>() {
        Ok(f) => f,
        Err(_) => return false,
    };

    let promise = match is_config_fn.call1(&audio_decoder, &test_config) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("Failed to call isConfigSupported: {e:?}");
            return false;
        }
    };

    // Await the promise
    let result = match JsFuture::from(js_sys::Promise::from(promise)).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("isConfigSupported promise rejected: {e:?}");
            return false;
        }
    };

    // Extract the 'supported' field from the result
    match Reflect::get(&result, &JsValue::from_str("supported")) {
        Ok(supported) => {
            let is_supported = supported.as_bool().unwrap_or(false);
            if is_supported {
                log::info!("WebCodecs AudioDecoder supports Opus codec");
            } else {
                log::warn!("WebCodecs AudioDecoder does NOT support Opus codec");
            }
            is_supported
        }
        Err(_) => {
            log::warn!("Failed to read 'supported' field from isConfigSupported result");
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackend {
    WebCodecs,
    JsLibrary,
}

/// Determines the best audio decoder backend for the current browser
/// This async version properly tests for Opus codec support
pub async fn detect_audio_backend() -> AudioBackend {
    // iOS/Safari must use JS library (WebCodecs not supported or crashes)
    if is_ios() {
        log::info!("Selected audio backend: JsLibrary (iOS/Safari)");
        return AudioBackend::JsLibrary;
    }

    // Chrome/Edge/Android - check if WebCodecs API exists first
    if !has_webcodecs_api() {
        log::info!("Selected audio backend: JsLibrary (WebCodecs API not available)");
        return AudioBackend::JsLibrary;
    }

    // WebCodecs API exists - now check if it supports Opus
    if check_webcodecs_opus_support().await {
        log::info!("Selected audio backend: WebCodecs (hardware-accelerated with Opus support)");
        return AudioBackend::WebCodecs;
    }

    // WebCodecs exists but doesn't support Opus
    log::info!("Selected audio backend: JsLibrary (WebCodecs doesn't support Opus)");
    AudioBackend::JsLibrary
}
