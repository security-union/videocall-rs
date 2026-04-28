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

/// Pure user-agent check: returns `true` if the UA string indicates an
/// iPhone, iPad, or iPod.  Extracted so it can be unit-tested without a
/// browser window.
pub fn ua_is_ios(ua: &str) -> bool {
    let ua_lower = ua.to_lowercase();
    ua_lower.contains("iphone") || ua_lower.contains("ipad") || ua_lower.contains("ipod")
}

/// Detects if the current environment is iOS (iPhone/iPad/iPod).
/// Uses only the user-agent string — AudioEncoder availability is checked
/// separately where needed because macOS Safari also lacks it.
pub fn is_ios() -> bool {
    *IS_IOS.get_or_init(|| {
        if let Some(window) = window() {
            if let Ok(ua) = window.navigator().user_agent() {
                let likely_ios = ua_is_ios(&ua);
                let audio_encoder_exists = is_audio_encoder_available();
                log::info!(
                    "Platform detection: User Agent='{ua}', LikelyiOS={likely_ios}, AudioEncoderAvailable={audio_encoder_exists}"
                );
                return likely_ios;
            }
        }
        log::warn!("Could not determine platform, assuming not iOS.");
        false
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

#[cfg(test)]
mod tests {
    use super::ua_is_ios;

    #[test]
    fn iphone_user_agent_is_ios() {
        assert!(ua_is_ios(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 \
             Mobile/15E148 Safari/604.1"
        ));
    }

    #[test]
    fn ipad_user_agent_is_ios() {
        assert!(ua_is_ios(
            "Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 \
             Mobile/15E148 Safari/604.1"
        ));
    }

    #[test]
    fn ipod_user_agent_is_ios() {
        assert!(ua_is_ios(
            "Mozilla/5.0 (iPod touch; CPU iPhone OS 15_0 like Mac OS X) \
             AppleWebKit/605.1.15"
        ));
    }

    #[test]
    fn macos_safari_is_not_ios() {
        assert!(!ua_is_ios(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 \
             Safari/605.1.15"
        ));
    }

    #[test]
    fn macos_chrome_is_not_ios() {
        assert!(!ua_is_ios(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 \
             Safari/537.36"
        ));
    }

    #[test]
    fn windows_chrome_is_not_ios() {
        assert!(!ua_is_ios(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 \
             Safari/537.36"
        ));
    }

    #[test]
    fn linux_firefox_is_not_ios() {
        assert!(!ua_is_ios(
            "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) \
             Gecko/20100101 Firefox/120.0"
        ));
    }
}
