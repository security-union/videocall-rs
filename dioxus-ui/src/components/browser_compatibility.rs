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

use crate::constants::firefox_enabled;
use dioxus::prelude::*;
use videocall_theme::Card;
use wasm_bindgen::{JsCast, JsValue};

#[component]
pub fn BrowserCompatibility() -> Element {
    let error = use_hook(|| {
        log::info!("Checking browser compatibility");
        let error = check_browser_compatibility();
        if let Some(error) = &error {
            log::error!("Browser compatibility check failed: {error}");
        } else {
            log::info!("Browser compatibility check passed");
        }
        error
    });

    // A card centered over the page, not a full-screen takeover: the page
    // around it stays visible and usable, as it was before the restyle.
    // `.browser-compat-notice` is also styled without cascade layers
    // (styles/unsupported-browser.css), so it stays readable in a browser
    // that drops every layered rule. `.error-container` is a test hook.
    if let Some(error) = &error {
        rsx! {
            Card { class: "error-container browser-compat-notice status-card", role: "alert",
                p { class: "text-headline", "{error}" }
                img {
                    src: "/assets/street_fighter.gif",
                    alt: "Permission instructions",
                    class: "instructions-gif",
                }
            }
        }
    } else {
        rsx! {}
    }
}

fn check_browser_compatibility() -> Option<String> {
    let window = match web_sys::window() {
        Some(w) => w,
        None => return Some("No browser window available".to_string()),
    };

    // Check Firefox feature flag - block Firefox unless explicitly enabled
    if is_firefox() {
        let ff_enabled = firefox_enabled().unwrap_or(false);
        log::info!("Firefox detected, firefoxEnabled={ff_enabled}");
        if !ff_enabled {
            return Some(
                "Hey friend! Firefox support is currently experimental and disabled. \
                Please use Chrome, Edge, Brave, or Safari for the best experience. \
                Firefox support can be enabled via the firefoxEnabled configuration flag."
                    .to_string(),
            );
        }
    }

    let mut missing_features = Vec::new();

    // Check for MediaStreamTrackProcessor (native or polyfill from index.html)
    if js_sys::Reflect::get(&window, &JsValue::from_str("MediaStreamTrackProcessor"))
        .unwrap_or(JsValue::UNDEFINED)
        .is_undefined()
    {
        missing_features.push("MediaStreamTrackProcessor");
    }

    // Check for VideoEncoder (WebCodecs API - supported in Firefox 130+, Chrome 94+)
    if js_sys::Reflect::get(&window, &JsValue::from_str("VideoEncoder"))
        .unwrap_or(JsValue::UNDEFINED)
        .is_undefined()
    {
        missing_features.push("VideoEncoder");
    }

    // Check for VideoDecoder (WebCodecs API)
    if js_sys::Reflect::get(&window, &JsValue::from_str("VideoDecoder"))
        .unwrap_or(JsValue::UNDEFINED)
        .is_undefined()
    {
        missing_features.push("VideoDecoder");
    }

    // Check for OffscreenCanvas (required by MediaStreamTrackProcessor polyfill)
    if js_sys::Reflect::get(&window, &JsValue::from_str("OffscreenCanvas"))
        .unwrap_or(JsValue::UNDEFINED)
        .is_undefined()
    {
        missing_features.push("OffscreenCanvas");
    }

    if !missing_features.is_empty() {
        let browser_hint = if is_firefox() {
            "Firefox 130+ is required for WebCodecs support."
        } else {
            "We recommend using Desktop Chrome, Chromium, Brave, Edge, or Firefox 130+."
        };

        Some(format!(
            "Hey friend! Thanks for trying videocall.rs! We're working hard to support your browser, but we need a few more modern features to make the magic happen. Your browser is missing: {}. {}",
            missing_features.join(", "),
            browser_hint
        ))
    } else {
        css_floor_message(supports_css_floor(&window))
    }
}

/// The CSS probe for the style floor. Every sheet sits in `@layer` (Chrome
/// 99, Firefox 97, Safari 15.4) and the speaking ring uses `color-mix()`
/// (Chrome 111, Firefox 113, Safari 16.2). Each engine shipped color-mix
/// after cascade layers, so one probe covers both.
const CSS_FLOOR_PROPERTY: &str = "color";
const CSS_FLOOR_VALUE: &str = "color-mix(in srgb, red, blue)";

/// Asks `CSS.supports` about the floor. A browser without `CSS.supports`
/// is older than the floor, so a missing function or a failed call counts
/// as unsupported.
fn supports_css_floor(window: &web_sys::Window) -> bool {
    let Ok(css) = js_sys::Reflect::get(window, &JsValue::from_str("CSS")) else {
        return false;
    };
    let Ok(supports) = js_sys::Reflect::get(&css, &JsValue::from_str("supports")) else {
        return false;
    };
    let Some(supports) = supports.dyn_ref::<js_sys::Function>() else {
        return false;
    };
    supports
        .call2(
            &css,
            &JsValue::from_str(CSS_FLOOR_PROPERTY),
            &JsValue::from_str(CSS_FLOOR_VALUE),
        )
        .map(|result| result.is_truthy())
        .unwrap_or(false)
}

/// The notice for a browser that runs the media code but not the styles.
fn css_floor_message(supported: bool) -> Option<String> {
    if supported {
        return None;
    }
    Some(
        "Hey friend! Your browser can run calls, but it is too old for this app's styles,         so the page would show up unstyled. Please update to Chrome or Edge 111+,         Firefox 113+, or Safari 16.2+."
            .to_string(),
    )
}

fn is_firefox() -> bool {
    if let Some(window) = web_sys::window() {
        if let Ok(user_agent) = window.navigator().user_agent() {
            let ua_lower = user_agent.to_lowercase();

            // Check for Firefox user agent patterns
            let has_firefox = ua_lower.contains("firefox");
            let has_gecko = ua_lower.contains("gecko");
            let has_chrome = ua_lower.contains("chrome");
            let has_safari = ua_lower.contains("safari");
            let has_like_gecko = ua_lower.contains("like gecko");

            // Firefox detection: has "firefox" OR (has "gecko" but NOT "chrome" AND NOT "safari" AND NOT "like gecko")
            // The "like gecko" check is important because Safari and Chrome include "like Gecko" in their user agents
            let is_firefox =
                has_firefox || (has_gecko && !has_chrome && !has_safari && !has_like_gecko);

            log::info!("Firefox detection: UA='{user_agent}', HasFirefox={has_firefox}, HasGecko={has_gecko}, HasChrome={has_chrome}, HasSafari={has_safari}, HasLikeGecko={has_like_gecko}, IsFirefox={is_firefox}");

            return is_firefox;
        }
    }
    false
}

// Helper function for testing Firefox detection with custom user agent
#[cfg(test)]
fn is_firefox_from_ua(user_agent: &str) -> bool {
    let ua_lower = user_agent.to_lowercase();
    let has_firefox = ua_lower.contains("firefox");
    let has_gecko = ua_lower.contains("gecko");
    let has_chrome = ua_lower.contains("chrome");
    let has_safari = ua_lower.contains("safari");
    let has_like_gecko = ua_lower.contains("like gecko");

    has_firefox || (has_gecko && !has_chrome && !has_safari && !has_like_gecko)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_css_floor_message() {
        assert_eq!(css_floor_message(true), None);

        let message = css_floor_message(false).expect("an old browser gets a notice");
        assert!(message.contains("Chrome or Edge 111+"));
        assert!(message.contains("Firefox 113+"));
        assert!(message.contains("Safari 16.2+"));
    }

    #[test]
    fn test_firefox_detection() {
        // Test Firefox user agents
        assert!(is_firefox_from_ua(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:91.0) Gecko/20100101 Firefox/91.0"
        ));

        assert!(is_firefox_from_ua(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:91.0) Gecko/20100101 Firefox/91.0"
        ));

        assert!(is_firefox_from_ua(
            "Mozilla/5.0 (X11; Linux x86_64; rv:91.0) Gecko/20100101 Firefox/91.0"
        ));

        // Test Chrome user agents (should not be detected as Firefox)
        assert!(!is_firefox_from_ua(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"
        ));

        assert!(!is_firefox_from_ua(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"
        ));

        // Test Edge user agents (should not be detected as Firefox)
        assert!(!is_firefox_from_ua(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36 Edg/91.0.864.59"
        ));

        // Test Safari user agents (should not be detected as Firefox)
        assert!(!is_firefox_from_ua(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/14.1.1 Safari/605.1.15"
        ));

        // Test the specific Safari user agent that was incorrectly detected as Firefox
        assert!(!is_firefox_from_ua(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Safari/605.1.15"
        ));

        // Test more Safari variants to ensure robustness
        assert!(!is_firefox_from_ua(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 15_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.0 Mobile/15E148 Safari/604.1"
        ));

        assert!(!is_firefox_from_ua(
            "Mozilla/5.0 (iPad; CPU OS 15_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.0 Mobile/15E148 Safari/604.1"
        ));
    }
}
