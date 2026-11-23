// SPDX-License-Identifier: MIT OR Apache-2.0

//! The few browser calls the gallery needs. Every one fails soft: with no
//! window (or an API missing) it returns an empty answer instead of panicking.

use std::collections::HashMap;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// Reads the computed value of each custom property on `<html>`.
///
/// An unregistered custom property computes to its declared text with every
/// `var()` already substituted, so `--color-accent` reads as the blue it
/// points at.
pub fn read_properties(names: &[&'static str]) -> HashMap<&'static str, String> {
    let mut values = HashMap::with_capacity(names.len());
    let Some(window) = web_sys::window() else {
        return values;
    };
    let Some(root) = window
        .document()
        .and_then(|document| document.document_element())
    else {
        return values;
    };
    let Ok(Some(style)) = window.get_computed_style(&root) else {
        return values;
    };
    for name in names {
        if let Ok(value) = style.get_property_value(name) {
            values.insert(*name, value.trim().to_string());
        }
    }
    values
}

/// Puts `class` on `<html>` (or takes it off), so a tokens.css pin such as
/// `.theme-dark` applies to the whole page.
pub fn set_root_class(class: &str, on: bool) {
    let Some(root) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.document_element())
    else {
        return;
    };
    let list = root.class_list();
    let _ = if on {
        list.add_1(class)
    } else {
        list.remove_1(class)
    };
}

/// True when `query` matches right now.
pub fn media_matches(query: &str) -> bool {
    web_sys::window()
        .and_then(|window| window.match_media(query).ok().flatten())
        .is_some_and(|list| list.matches())
}

/// Calls `on_change` whenever any of `queries` flips.
///
/// The closure is leaked on purpose: it has to outlive this call, and the
/// gallery page never tears it down.
pub fn watch_media(queries: &[&str], on_change: impl Fn() + 'static) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = Closure::<dyn Fn()>::new(on_change);
    for query in queries {
        if let Ok(Some(list)) = window.match_media(query) {
            list.set_onchange(Some(callback.as_ref().unchecked_ref()));
        }
    }
    callback.forget();
}

/// Calls `on_change` on every `hashchange` (index clicks, back and forward,
/// a pasted link). Leaked for the same reason as [`watch_media`].
pub fn watch_hash(on_change: impl Fn() + 'static) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = Closure::<dyn Fn()>::new(on_change);
    window.set_onhashchange(Some(callback.as_ref().unchecked_ref()));
    callback.forget();
}

/// The URL fragment without `#`, or the first section when there is none.
pub fn current_hash() -> String {
    let hash = web_sys::window()
        .and_then(|window| window.location().hash().ok())
        .unwrap_or_default();
    let hash = hash.trim_start_matches('#');
    if hash.is_empty() {
        crate::SECTIONS[0].0.to_string()
    } else {
        hash.to_string()
    }
}
