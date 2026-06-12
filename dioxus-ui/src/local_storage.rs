/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Tiny `localStorage` helpers for boolean UI preferences.
//!
//! These are intentionally small and untyped — they exist to avoid duplicating
//! the same `web_sys::window().and_then(...)` chain across components that
//! persist a single boolean (e.g. collapsible-section expand/collapse state).
//!
//! Both functions are defensive: if `localStorage` is unavailable (Safari
//! private mode, SSR, sandboxed iframes, etc.) `load_bool` returns the default
//! and `save_bool` silently no-ops. Failures are never surfaced to the caller.
//!
//! The serialised form is the literal string `"true"` or `"false"`. Any other
//! value (or a missing key) maps to `default`.
//!
//! For richer state shapes use a typed helper like the
//! `load_transport_preference` / `save_transport_preference` pair in
//! `context.rs` instead.
//!
//! # Key naming
//!
//! Keys should be stable, namespaced strings. The home-page meetings list
//! uses dot-namespaced keys like `home.meetings.expanded`.

/// Read a boolean preference from `localStorage`.
///
/// Returns `default` if `localStorage` is unavailable, the key is missing, or
/// the stored value is anything other than `"true"` / `"false"`.
pub fn load_bool(key: &str, default: bool) -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(key).ok().flatten())
        .map(|v| match v.as_str() {
            "true" => true,
            "false" => false,
            _ => default,
        })
        .unwrap_or(default)
}

/// Persist a boolean preference to `localStorage`. Silently ignores failures.
pub fn save_bool(key: &str, value: bool) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(key, if value { "true" } else { "false" });
    }
}

/// Load a JSON-serialised preference from `localStorage`, falling back to the
/// supplied `default` on any failure path: storage unavailable, key missing, or
/// a stored value that no longer parses (corrupt / written by an incompatible
/// older release). Mirrors the resilience of [`load_bool`] for richer state
/// shapes such as the meetings-list `FilterState` / `SortState`.
///
/// Chosen over the `FromStr`-based `load/save_transport_preference` pattern in
/// `context.rs` because these states are multi-field structs/enums where a
/// derived `serde_json` round-trip is far less error-prone than a hand-written
/// string codec.
pub fn load_json<T>(key: &str, default: T) -> T
where
    T: serde::de::DeserializeOwned,
{
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(key).ok().flatten())
        .and_then(|raw| serde_json::from_str::<T>(&raw).ok())
        .unwrap_or(default)
}

/// Persist a value to `localStorage` as JSON. Silently ignores serialisation
/// and storage failures (Safari private mode, quota, etc.).
pub fn save_json<T>(key: &str, value: &T)
where
    T: serde::Serialize,
{
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        if let Ok(json) = serde_json::to_string(value) {
            let _ = storage.set_item(key, &json);
        }
    }
}

/// Read an `f64` preference from `localStorage`. Returns `default` if storage is
/// unavailable, the key is missing, the value no longer parses, or the parsed
/// value is non-finite (NaN or ±infinity). The non-finite guard prevents a
/// tampered or corrupt stored value (e.g. `"NaN"`) from propagating into layout
/// math where `NaN` would silently collapse the grid (`avail_w = vw - NaN = NaN
/// → .max(0.0) = 0.0`). Call sites may additionally clamp the returned value
/// to enforce domain-specific bounds.
pub fn load_f64(key: &str, default: f64) -> f64 {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(key).ok().flatten())
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}

/// Persist an `f64` preference to `localStorage`. Silently ignores failures.
pub fn save_f64(key: &str, value: f64) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(key, &value.to_string());
    }
}
