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
//! Keys should be stable, namespaced strings. The home-page collapsible
//! sections use dot-namespaced keys like `home.previously-joined.expanded`.

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
