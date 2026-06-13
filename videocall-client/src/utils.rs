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
static IS_WEBKIT: OnceLock<bool> = OnceLock::new();

/// Pure user-agent check: returns `true` if the UA string indicates an
/// iPhone, iPad, or iPod.  Extracted so it can be unit-tested without a
/// browser window.
pub fn ua_is_ios(ua: &str) -> bool {
    let ua_lower = ua.to_lowercase();
    ua_lower.contains("iphone") || ua_lower.contains("ipad") || ua_lower.contains("ipod")
}

/// Pure user-agent check: returns `true` if the UA string indicates a
/// **WebKit** engine — i.e. desktop Safari **or** any iOS browser (Safari,
/// Chrome/CriOS, Firefox/FxiOS, Edge/EdgiOS), all of which are WebKit-backed
/// on Apple platforms.
///
/// ## Why this exists (issue #1286)
///
/// The browser Long Tasks API (`PerformanceObserver` with the `"longtask"`
/// entry type) is **not implemented on WebKit**. On those browsers the
/// observer is never installed (see `long_tasks::LongTaskObserver::start`,
/// which returns `None`), so `client_longtask_duration_ms` is never emitted
/// and the decode-budget control loop sees a perpetual `0.0` long-task bucket
/// — indistinguishable from a genuinely idle main thread. Reading that blind
/// `0.0` as "healthy" let the receiver-side DecodeBudget cap ratchet UP on a
/// weak iPhone until it collapsed. This detector is the discriminator that
/// lets the construction site emit `None` ("no telemetry") instead of
/// `Some(0.0)` ("idle") on every WebKit browser.
///
/// ## Detection logic
///
/// - Any iOS device (`iphone`/`ipad`/`ipod`) is WebKit, regardless of the
///   in-app browser brand (CriOS, FxiOS, EdgiOS) — Apple mandates WebKit for
///   all iOS browser engines.
/// - Desktop Safari contains both `safari` and `applewebkit` in its UA but
///   does NOT contain a Chromium/Blink brand token (`chrome`, `chromium`,
///   `crios`, `edg`/`edge`, `opr`/`opera`, `samsungbrowser`). Chromium-based
///   browsers (Chrome, Edge, Opera, Samsung) also ship `safari` +
///   `applewebkit` tokens for compatibility, so the brand-token exclusion is
///   required to avoid mis-classifying them as WebKit.
///
/// ## Known limitation
///
/// This does NOT catch Firefox < 127 (desktop Gecko), which also lacked the
/// `"longtask"` entry type. That is a rare, ageing edge case and is out of
/// scope for #1286 (the named bug is mobile WebKit). Firefox >= 127 supports
/// `"longtask"` and is correctly treated as non-WebKit here.
pub fn ua_is_webkit(ua: &str) -> bool {
    let ua_lower = ua.to_lowercase();

    // All iOS browsers are WebKit-backed, including CriOS/FxiOS/EdgiOS.
    if ua_is_ios(&ua_lower) {
        return true;
    }

    // Desktop Safari: WebKit + Safari brand, but NOT a Chromium/Blink brand
    // (those carry `applewebkit`/`safari` tokens too for legacy reasons).
    let is_chromium_brand = ua_lower.contains("chrome")
        || ua_lower.contains("chromium")
        || ua_lower.contains("crios")
        || ua_lower.contains("edg") // matches both "edge" and "edg/" (modern Edge)
        || ua_lower.contains("opr")
        || ua_lower.contains("opera")
        || ua_lower.contains("samsungbrowser");

    ua_lower.contains("applewebkit") && ua_lower.contains("safari") && !is_chromium_brand
}

/// Detects whether the current browser engine is WebKit (desktop Safari or
/// any iOS browser). Cached for the session. Used to decide that the Long
/// Tasks API is unavailable (issue #1286). See [`ua_is_webkit`] for the
/// detection rationale and known limitations.
pub fn is_webkit() -> bool {
    *IS_WEBKIT.get_or_init(|| {
        if let Some(window) = window() {
            if let Ok(ua) = window.navigator().user_agent() {
                let webkit = ua_is_webkit(&ua);
                log::info!("WebKit detection: User Agent='{ua}', IsWebKit={webkit}");
                return webkit;
            }
        }
        log::warn!("Could not determine browser engine, assuming not WebKit.");
        false
    })
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

/// Logical CPU core count from `navigator.hardwareConcurrency`, cached for the
/// session. Returns `0` when the count is unavailable or implausible (the
/// caller treats `0` as the most conservative tier). Centralised here so the
/// decode-budget device-class ceiling (issue #1286) and other platform-gating
/// logic share ONE reader instead of duplicating the
/// `hardware_concurrency()`-finite-clamp dance (it already appears in
/// `capability_check.rs` and `health_reporter.rs`, both of which compute it
/// inline at their own call sites for unrelated reasons).
pub fn hardware_concurrency_cores() -> u32 {
    static CORES: OnceLock<u32> = OnceLock::new();
    *CORES.get_or_init(|| {
        if let Some(window) = window() {
            let cores_f64 = window.navigator().hardware_concurrency();
            if cores_f64.is_finite() && cores_f64 >= 1.0 {
                return cores_f64.min(u32::MAX as f64) as u32;
            }
        }
        0
    })
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
    use super::{ua_is_ios, ua_is_webkit};

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

    // ── ua_is_webkit (issue #1286) ───────────────────────────────────────────
    //
    // The contract these pin: the Long Tasks API is unavailable on WebKit, so
    // `ua_is_webkit` must return `true` for every browser whose decode-budget
    // long-task signal would be perpetually blind (desktop Safari + ALL iOS
    // browsers), and `false` for browsers that DO support `"longtask"`
    // (desktop/Android Chrome, modern Firefox). If these flip, the #1286
    // longtask-blind handling either over-applies (suppressing growth on
    // Chromium-idle) or under-applies (re-exposing the iPhone ratchet bug).

    #[test]
    fn ios_safari_is_webkit() {
        assert!(ua_is_webkit(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 \
             Mobile/15E148 Safari/604.1"
        ));
    }

    #[test]
    fn ios_chrome_crios_is_webkit() {
        // iOS Chrome is WebKit-backed (Apple mandates it) and DOES carry a
        // "CriOS" Chromium brand token — the iOS short-circuit must win so it
        // is still classed WebKit (longtask-blind), not Chromium.
        assert!(ua_is_webkit(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/120.0.0.0 \
             Mobile/15E148 Safari/604.1"
        ));
    }

    #[test]
    fn ios_firefox_fxios_is_webkit() {
        assert!(ua_is_webkit(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/120.0 \
             Mobile/15E148 Safari/605.1.15"
        ));
    }

    #[test]
    fn ipad_safari_is_webkit() {
        assert!(ua_is_webkit(
            "Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 \
             Mobile/15E148 Safari/604.1"
        ));
    }

    #[test]
    fn desktop_safari_is_webkit() {
        assert!(ua_is_webkit(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 \
             Safari/605.1.15"
        ));
    }

    #[test]
    fn macos_chrome_is_not_webkit() {
        // Chromium carries applewebkit + safari tokens for legacy reasons; the
        // "chrome" brand exclusion must keep it OUT of the WebKit class so its
        // (working) longtask telemetry is honoured.
        assert!(!ua_is_webkit(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 \
             Safari/537.36"
        ));
    }

    #[test]
    fn windows_chrome_is_not_webkit() {
        assert!(!ua_is_webkit(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 \
             Safari/537.36"
        ));
    }

    #[test]
    fn android_chrome_is_not_webkit() {
        assert!(!ua_is_webkit(
            "Mozilla/5.0 (Linux; Android 13; Pixel 7) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 \
             Mobile Safari/537.36"
        ));
    }

    #[test]
    fn macos_edge_is_not_webkit() {
        // Modern Edge (Chromium) carries an "Edg/" token plus chrome/safari.
        assert!(!ua_is_webkit(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 \
             Safari/537.36 Edg/120.0.0.0"
        ));
    }

    #[test]
    fn desktop_firefox_is_not_webkit() {
        // Modern desktop Firefox (Gecko, >=127) supports "longtask"; it is not
        // WebKit and must not be treated as blind.
        assert!(!ua_is_webkit(
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) \
             Gecko/20100101 Firefox/128.0"
        ));
    }
}
