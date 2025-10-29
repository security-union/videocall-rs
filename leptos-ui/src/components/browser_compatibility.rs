// SPDX-License-Identifier: MIT OR Apache-2.0

use leptos::prelude::*;
use leptos::web_sys;
use wasm_bindgen::JsValue;

#[component]
pub fn BrowserCompatibility() -> impl IntoView {
    let error = create_memo(|_| check_browser_compatibility());

    view! {
        {move || if let Some(err) = error.get() {
            view! {
                <div class="error-container">
                    <p class="error-message">{err}</p>
                    <img src="/assets/street_fighter.gif" alt="Permission instructions" class="instructions-gif" />
                </div>
            }.into_any()
        } else {
            ().into_any()
        }}
    }
}

fn check_browser_compatibility() -> Option<String> {
    let window = web_sys::window().unwrap();

    if is_firefox() {
        return Some(
            "🦊 Firefox Detected! Unfortunately, videocall.rs doesn't support Firefox due to incomplete MediaStreamTrackProcessor implementation. Please use Desktop Chrome, Chromium, Brave, or Edge for the best experience. 🚀".to_string(),
        );
    }

    let mut missing_features = Vec::new();

    // Check for MediaStreamTrackProcessor
    if js_sys::Reflect::get(&window, &JsValue::from_str("MediaStreamTrackProcessor"))
        .unwrap()
        .is_undefined()
    {
        missing_features.push("MediaStreamTrackProcessor");
    }

    // Check for VideoEncoder
    if js_sys::Reflect::get(&window, &JsValue::from_str("VideoEncoder"))
        .unwrap()
        .is_undefined()
    {
        missing_features.push("VideoEncoder");
    }

    if !missing_features.is_empty() {
        Some(format!(
            "Hey friend! 👋 Thanks for trying videocall.rs! We're working hard to support your browser, but we need a few more modern features to make the magic happen. Your browser is missing: {}. We recommend using Desktop Chrome, Chromium, Brave, or Edge for the best experience. 🚀",
            missing_features.join(", ")
        ))
    } else {
        None
    }
}

fn is_firefox() -> bool {
    if let Some(window) = web_sys::window() {
        if let Ok(user_agent) = window.navigator().user_agent() {
            let ua_lower = user_agent.to_lowercase();
            let has_firefox = ua_lower.contains("firefox");
            let has_gecko = ua_lower.contains("gecko");
            let has_chrome = ua_lower.contains("chrome");
            let has_safari = ua_lower.contains("safari");
            let has_like_gecko = ua_lower.contains("like gecko");
            return has_firefox || (has_gecko && !has_chrome && !has_safari && !has_like_gecko);
        }
    }
    false
}
