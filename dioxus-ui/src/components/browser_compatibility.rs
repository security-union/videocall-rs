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
use wasm_bindgen::JsValue;

#[component]
pub fn BrowserCompatibility() -> Element {
    let error = use_signal(|| check_browser_compatibility());

    if let Some(error_msg) = error.read().as_ref() {
        rsx! {
            div { class: "error-container",
                p { class: "error-message", "{error_msg}" }
                img {
                    src: "/assets/street_fighter.gif",
                    alt: "Permission instructions",
                    class: "instructions-gif"
                }
            }
        }
    } else {
        rsx! {}
    }
}

fn check_browser_compatibility() -> Option<String> {
    log::info!("Checking browser compatibility");
    let window = web_sys::window()?;

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

        log::error!(
            "Browser compatibility check failed: missing {}",
            missing_features.join(", ")
        );

        Some(format!(
            "Hey friend! Thanks for trying videocall.rs! We're working hard to support your browser, but we need a few more modern features to make the magic happen. Your browser is missing: {}. {}",
            missing_features.join(", "),
            browser_hint
        ))
    } else {
        log::info!("Browser compatibility check passed");
        None
    }
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
            let is_firefox =
                has_firefox || (has_gecko && !has_chrome && !has_safari && !has_like_gecko);

            log::info!(
                "Firefox detection: UA='{}', IsFirefox={}",
                user_agent,
                is_firefox
            );

            return is_firefox;
        }
    }
    false
}
