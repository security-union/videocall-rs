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

// Cached results to avoid repeated checks
use std::sync::OnceLock;
static IS_IOS: OnceLock<bool> = OnceLock::new();
static IS_FIREFOX: OnceLock<bool> = OnceLock::new();

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
                    "Platform detection: User Agent='{ua}', LikelyiOS={likely_ios}, AudioEncoderAvailable={audio_encoder_exists}, FinalResult={result}"
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

/// Detects if the current browser is Firefox.
/// Firefox uses software VP9 encoding which is slow, so we use VP8 instead.
pub fn is_firefox() -> bool {
    *IS_FIREFOX.get_or_init(|| {
        if let Some(window) = window() {
            if let Ok(ua) = window.navigator().user_agent() {
                let ua_lower = ua.to_lowercase();
                // Firefox user agent contains "firefox" but not "seamonkey" (which also uses Gecko)
                let is_ff = ua_lower.contains("firefox") && !ua_lower.contains("seamonkey");
                log::info!("Firefox detection: User Agent='{ua}', IsFirefox={is_ff}");
                return is_ff;
            }
        }
        log::warn!("Could not determine browser, assuming not Firefox.");
        false
    })
}

/// Returns a monotonic, high-resolution timestamp in milliseconds using
/// `performance.now()`. This is immune to NTP adjustments, DST changes, and
/// user clock manipulation, unlike `js_sys::Date::now()`, making it safe
/// for RTT, elapsed-time and rate-control calculations.
///
/// Falls back to `js_sys::Date::now()` when the Performance API is
/// unavailable (e.g. some headless WASM runtimes).
///
/// The `Performance` object is looked up once per thread: this runs for
/// every received camera frame.
pub(crate) fn monotonic_now_ms() -> f64 {
    thread_local! {
        static PERFORMANCE: Option<web_sys::Performance> =
            window().and_then(|w| w.performance());
    }
    PERFORMANCE
        .with(|performance| performance.as_ref().map(|p| p.now()))
        .unwrap_or_else(js_sys::Date::now)
}

/// Query parameter names whose values must never appear in logs because they
/// carry credentials or session secrets (e.g. the JWT `token` embedded in the
/// server URL).
const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "token",
    "key",
    "password",
    "passwd",
    "secret",
    "signature",
    "auth",
];

/// Returns a copy of `url` with the values of sensitive query parameters
/// replaced by `<redacted>` so the URL is safe to write to logs.
///
/// Non-sensitive parameters, ordering, and fragments are preserved so the
/// logged URL still identifies the endpoint and transport.
///
/// ```
/// use videocall_client::utils::redact_url_for_logging;
/// assert_eq!(
///     redact_url_for_logging("wss://host/lobby?token=abc123&room=x"),
///     "wss://host/lobby?token=<redacted>&room=x"
/// );
/// ```
pub fn redact_url_for_logging(url: &str) -> String {
    let Some(query_start) = url.find('?') else {
        return url.to_string();
    };
    let (base_with_sep, rest) = url.split_at(query_start + 1);
    let (query, fragment) = match rest.find('#') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let redacted = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _)) if is_sensitive_param(name) => format!("{name}=<redacted>"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base_with_sep}{redacted}{fragment}")
}

fn is_sensitive_param(name: &str) -> bool {
    SENSITIVE_QUERY_PARAMS
        .iter()
        .any(|s| name.eq_ignore_ascii_case(s))
}

#[cfg(test)]
mod tests {
    use super::redact_url_for_logging;

    #[test]
    fn redacts_token_param() {
        assert_eq!(
            redact_url_for_logging("wss://host/lobby?token=eyJ0eXA.eyJzdWI.sig"),
            "wss://host/lobby?token=<redacted>"
        );
    }

    #[test]
    fn preserves_non_sensitive_params_and_order() {
        assert_eq!(
            redact_url_for_logging("wss://host/lobby?room=infra&token=abc&n=1"),
            "wss://host/lobby?room=infra&token=<redacted>&n=1"
        );
    }

    #[test]
    fn matches_param_name_case_insensitively() {
        assert_eq!(
            redact_url_for_logging("wss://host/lobby?TOKEN=abc"),
            "wss://host/lobby?TOKEN=<redacted>"
        );
    }

    #[test]
    fn preserves_fragment() {
        assert_eq!(
            redact_url_for_logging("wss://host/lobby?token=abc#frag"),
            "wss://host/lobby?token=<redacted>#frag"
        );
    }

    #[test]
    fn leaves_param_without_value_alone() {
        assert_eq!(
            redact_url_for_logging("wss://host/lobby?token&room=x"),
            "wss://host/lobby?token&room=x"
        );
    }

    #[test]
    fn does_not_redact_substring_names() {
        assert_eq!(
            redact_url_for_logging("wss://host/lobby?csrf_token=keepme"),
            "wss://host/lobby?csrf_token=keepme"
        );
    }

    #[test]
    fn passes_through_url_without_query() {
        assert_eq!(
            redact_url_for_logging("wss://host/lobby"),
            "wss://host/lobby"
        );
    }
}
