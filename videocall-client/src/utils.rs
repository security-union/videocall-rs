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

use js_sys::Reflect;
use wasm_bindgen::JsValue;
use web_sys::window;

// Cached result to avoid repeated checks
use std::sync::OnceLock;
static IS_IOS: OnceLock<bool> = OnceLock::new();

/// Detects if the current environment is likely iOS Safari.
/// Checks user agent and the absence of AudioEncoder API which causes crashes on iOS.
pub fn is_ios() -> bool {
    *IS_IOS.get_or_init(|| {
        if let Some(window) = window() {
            // Check if AudioEncoder exists in window
            let audio_encoder_exists = is_audio_encoder_available();
            if let Ok(ua) = window.navigator().user_agent() {
                let ua_lower = ua.to_lowercase();
                let likely_ios = ua_lower.contains("iphone") || ua_lower.contains("ipad") || ua_lower.contains("ipod");
                // Consider it iOS if the user agent suggests iOS OR if AudioEncoder is missing
                // Audio Encoder may be missing on older browsers too, so we check both conditions
                let result = likely_ios || !audio_encoder_exists;
                log::info!(
                    "Platform detection: User Agent='{}', LikelyiOS={}, AudioEncoderAvailable={}, FinalResult={}",
                    ua, likely_ios, audio_encoder_exists, result
                );
                return result;
            }
        }
        log::warn!("Could not determine platform, assuming not iOS.");
        false // Default to false if detection fails
    })
}

/// Safely check if AudioEncoder is available without crashing
fn is_audio_encoder_available() -> bool {
    // Use reflection to safely check if AudioEncoder exists on the window object
    if let Some(window) = window() {
        let global = JsValue::from(window);

        // First check if AudioEncoder exists on the window object
        match Reflect::has(&global, &JsValue::from_str("AudioEncoder")) {
            Ok(exists) => {
                if !exists {
                    return false;
                }

                // Try to access it to make sure it's properly supported
                match Reflect::get(&global, &JsValue::from_str("AudioEncoder")) {
                    Ok(constructor) => {
                        // Check if it's a function/constructor by verifying it's not undefined/null
                        !constructor.is_undefined() && !constructor.is_null()
                    }
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    } else {
        false
    }
}
