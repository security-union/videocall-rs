// videocall-client/src/utils.rs
use wasm_bindgen::prelude::*;
use web_sys::window;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = window, js_name = MediaStreamTrackProcessor)]
    type MediaStreamTrackProcessor;

    #[wasm_bindgen(catch, js_name = MediaStreamTrackProcessor)]
    fn try_get_media_stream_track_processor() -> Result<JsValue, JsValue>;
}

// Cached result to avoid repeated checks
use std::sync::OnceLock;
static IS_IOS: OnceLock<bool> = OnceLock::new();

/// Detects if the current environment is likely iOS Safari.
/// Checks user agent and the absence of MediaStreamTrackProcessor.
pub fn is_ios() -> bool {
    *IS_IOS.get_or_init(|| {
        if let Some(window) = window() {
            if let Ok(ua) = window.navigator().user_agent() {
                let ua_lower = ua.to_lowercase();
                let likely_ios = ua_lower.contains("iphone") || ua_lower.contains("ipad") || ua_lower.contains("ipod");

                // Check for specific API absence known on iOS Safari
                let processor_missing = try_get_media_stream_track_processor().is_err();

                // More robust check: likely iOS UA AND missing the specific API
                let result = likely_ios && processor_missing;
                log::info!("iOS detection: User Agent='{}', LikelyiOS={}, ProcessorMissing={}, FinalResult={}", ua, likely_ios, processor_missing, result);
                return result;
            }
        }
        log::warn!("Could not determine platform, assuming not iOS.");
        false // Default to false if detection fails
    })
}