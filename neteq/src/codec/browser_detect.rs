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

use js_sys::Reflect;
use std::sync::OnceLock;
use wasm_bindgen::JsValue;
use web_sys::window;

static IS_IOS: OnceLock<bool> = OnceLock::new();
static SUPPORTS_WEBCODECS: OnceLock<bool> = OnceLock::new();

/// Detects if the current environment is iOS/Safari
pub fn is_ios() -> bool {
    *IS_IOS.get_or_init(|| {
        if let Some(window) = window() {
            if let Ok(ua) = window.navigator().user_agent() {
                let ua_lower = ua.to_lowercase();
                let is_ios = ua_lower.contains("iphone")
                    || ua_lower.contains("ipad")
                    || ua_lower.contains("ipod")
                    || (ua_lower.contains("safari") && !ua_lower.contains("chrome"));

                log::info!("Platform detection: UA='{ua}', is_ios={is_ios}");
                return is_ios;
            }
        }
        false
    })
}

/// Checks if WebCodecs AudioDecoder is available and supports Opus
pub fn supports_webcodecs_audio() -> bool {
    *SUPPORTS_WEBCODECS.get_or_init(|| {
        if let Some(window) = window() {
            let global = JsValue::from(window);

            // Check if AudioDecoder exists
            match Reflect::has(&global, &JsValue::from_str("AudioDecoder")) {
                Ok(true) => match Reflect::get(&global, &JsValue::from_str("AudioDecoder")) {
                    Ok(constructor) if !constructor.is_undefined() && !constructor.is_null() => {
                        log::info!("WebCodecs AudioDecoder available");
                        return true;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        log::info!("WebCodecs AudioDecoder not available");
        false
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackend {
    WebCodecs,
    JsLibrary,
}

/// Determines the best audio decoder backend for the current browser
pub fn detect_audio_backend() -> AudioBackend {
    // iOS/Safari must use JS library (WebCodecs not supported or crashes)
    if is_ios() {
        log::info!("Selected audio backend: JsLibrary (iOS/Safari)");
        return AudioBackend::JsLibrary;
    }

    // Chrome/Edge/Android - prefer WebCodecs if available
    if supports_webcodecs_audio() {
        log::info!("Selected audio backend: WebCodecs (hardware-accelerated)");
        return AudioBackend::WebCodecs;
    }

    // Fallback for older browsers
    log::info!("Selected audio backend: JsLibrary (fallback)");
    AudioBackend::JsLibrary
}
