// SPDX-License-Identifier: MIT OR Apache-2.0

//! Username context and localStorage helpers for Leptos UI.

use leptos::prelude::*;
use leptos::web_sys;
use once_cell::sync::Lazy;
use regex::Regex;

pub type UsernameSignal = RwSignal<Option<String>>;

const STORAGE_KEY: &str = "vc_username";

pub fn provide_username_context() -> UsernameSignal {
    let initial = load_username_from_storage();
    let signal = RwSignal::new(initial);
    provide_context(signal);
    signal
}

pub fn use_username_context() -> UsernameSignal {
    use_context::<UsernameSignal>().expect("Username context missing")
}

pub fn load_username_from_storage() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(STORAGE_KEY).ok().flatten())
}

pub fn save_username_to_storage(username: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(STORAGE_KEY, username);
    }
}

static USERNAME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9_]+$").unwrap());

pub fn is_valid_username(name: &str) -> bool {
    !name.is_empty() && USERNAME_RE.is_match(name)
}
