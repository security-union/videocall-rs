// SPDX-License-Identifier: MIT OR Apache-2.0

//! Username context helpers
//!
//! This module centralises everything related to persisting the chosen
//! username in `localStorage` and sharing it across the component tree
//! through Yew's `ContextProvider`.

use wasm_bindgen::JsCast;
use web_sys::HtmlInputElement;
use yew::prelude::*;

/// Type alias used throughout the app when accessing the username context.
///
/// `UseStateHandle<Option<String>>` allows both read-only access (via
/// deref) and mutation by calling `.set(Some("new_name".into()))`.
pub type UsernameCtx = UseStateHandle<Option<String>>;

// -----------------------------------------------------------------------------
// Local-storage helpers
// -----------------------------------------------------------------------------

const STORAGE_KEY: &str = "vc_username";

/// Read the username from `window.localStorage` (if present).
pub fn load_username_from_storage() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten())
}

/// Persist the username to `localStorage` so that it survives page reloads.
pub fn save_username_to_storage(username: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(STORAGE_KEY, username);
    }
}

/// Remove the cached username (used by the "Change username" button).
pub fn clear_username_from_storage() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item(STORAGE_KEY);
    }
}

// -----------------------------------------------------------------------------
// Validation helpers
// -----------------------------------------------------------------------------

/// Regex for `^[a-zA-Z0-9_]*$` compiled once at start-up.
lazy_static::lazy_static! {
    static ref USERNAME_RE: regex::Regex = regex::Regex::new(r"^[A-Za-z0-9_]+$").unwrap();
}

/// Returns `true` iff the supplied username is non-empty and matches the
/// allowed pattern.
pub fn is_valid_username(name: &str) -> bool {
    !name.is_empty() && USERNAME_RE.is_match(name)
}

/// Helper that reads the string value out of an `InputEvent`.
pub fn extract_input_value(e: InputEvent) -> String {
    e.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|input| input.value())
        .unwrap_or_default()
}
