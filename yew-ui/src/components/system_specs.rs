/*
 * System specs collection utilities for videocall.rs diagnostics
 *
 * Gathers information about the browser and host device that is available
 * from standard Web APIs.  All fields are optional so that we can work on
 * every platform without failing if an API is missing / denied.
 */

use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::{Navigator, Screen, Window};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SystemSpecs {
    /// Raw browser user-agent string
    pub user_agent: Option<String>,
    /// e.g. "Win32", "MacIntel", "Linux armv8l", "iPhone" …
    pub platform: Option<String>,
    /// Number of logical CPU cores (`navigator.hardwareConcurrency`)
    pub cpu_cores: Option<u32>,
    /// Approximate device memory in **GB** (`navigator.deviceMemory` – experimental)
    pub device_memory_gb: Option<f64>,
    /// Preferred UI languages (`navigator.languages`)
    pub languages: Vec<String>,
    /// Screen width in CSS pixels
    pub screen_width: Option<u32>,
    /// Screen height in CSS pixels
    pub screen_height: Option<u32>,
    /// Color depth (bits-per-pixel)
    pub color_depth: Option<u32>,
    /// Device-pixel-ratio (`window.devicePixelRatio`)
    pub pixel_ratio: Option<f64>,
    /// Effective network type (4g / 3g / 2g / slow-2g)
    pub network_type: Option<String>,
    /// Estimated down-link bandwidth in megabits (`navigator.connection.downlink`)
    pub network_downlink_mbps: Option<f64>,
}

impl Default for SystemSpecs {
    fn default() -> Self {
        Self {
            user_agent: None,
            platform: None,
            cpu_cores: None,
            device_memory_gb: None,
            languages: vec![],
            screen_width: None,
            screen_height: None,
            color_depth: None,
            pixel_ratio: None,
            network_type: None,
            network_downlink_mbps: None,
        }
    }
}

/// Collect the specs that are synchronously available in the browser.
pub fn gather_system_specs() -> anyhow::Result<SystemSpecs> {
    let window: Window = web_sys::window().ok_or(anyhow::anyhow!("No window found"))?;
    let navigator: Navigator = window.navigator();
    let screen: Screen = window
        .screen()
        .map_err(|e| anyhow::anyhow!("No screen found: {e:?}"))?;

    let mut specs = SystemSpecs::default();

    // User agent & platform
    specs.user_agent = navigator
        .user_agent()
        .map_err(|e| anyhow::anyhow!("No user agent found: {e:?}"))
        .ok();
    specs.platform = navigator
        .platform()
        .map_err(|e| anyhow::anyhow!("No platform found: {e:?}"))
        .ok();

    // CPU cores & device memory (the latter is non-standard, use JS reflection)
    let cores = navigator.hardware_concurrency();
    if cores > 0.0 {
        specs.cpu_cores = Some(cores as u32);
    }

    specs.device_memory_gb = js_sys::Reflect::get(&navigator, &JsValue::from_str("deviceMemory"))
        .ok()
        .and_then(|v| v.as_f64());

    // Languages – `languages()` is not yet in web_sys, fallback to primary language
    let langs_val = js_sys::Reflect::get(&navigator, &JsValue::from_str("languages")).ok();
    if let Some(val) = langs_val {
        if val.is_object() {
            let arr = js_sys::Array::from(&val);
            specs.languages = arr.iter().filter_map(|v| v.as_string()).collect();
        }
    }
    if specs.languages.is_empty() {
        if let Some(lang) = navigator.language() {
            specs.languages.push(lang);
        }
    }

    specs.screen_width = screen.width().ok().map(|v| v as u32);
    specs.screen_height = screen.height().ok().map(|v| v as u32);
    specs.color_depth = screen.color_depth().ok().map(|v| v as u32);

    specs.pixel_ratio = Some(window.device_pixel_ratio());

    // Network Information API (experimental – guarded)
    if let Ok(conn) = js_sys::Reflect::get(&navigator, &JsValue::from_str("connection")) {
        if !conn.is_undefined() && !conn.is_null() {
            let conn_obj = conn;
            specs.network_type =
                js_sys::Reflect::get(&conn_obj, &JsValue::from_str("effectiveType"))
                    .ok()
                    .and_then(|v| v.as_string());
            specs.network_downlink_mbps =
                js_sys::Reflect::get(&conn_obj, &JsValue::from_str("downlink"))
                    .ok()
                    .and_then(|v| v.as_f64());
        }
    }

    Ok(specs)
}
