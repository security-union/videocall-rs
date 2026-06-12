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

//! Runtime JS control surface for the network simulator (issue #1080).
//!
//! The per-receiver simulcast-divergence e2e test drives a
//! climb → impair → heal sequence MID-CALL, so a static `?netsim=`
//! URL param (installed once at first connect) is not enough — the test
//! needs to flip impairment on and off after the call is already up.
//!
//! This module installs a `window.__vcNetsim` object that Playwright can
//! reach from `page.evaluate(...)`:
//!
//! ```js
//! // Install a downlink impairment (returns true on success):
//! window.__vcNetsim.install("crushed_downlink", "down");
//! // Remove ALL netsim shaping (uplink + downlink):
//! window.__vcNetsim.clear();
//! ```
//!
//! ### Why `window.*` registration, not a `#[wasm_bindgen]` export
//!
//! A plain `#[wasm_bindgen]` export lands on the wasm-bindgen JS glue
//! MODULE, not on `window`, so Playwright's `page.evaluate` (which runs
//! in the page's global scope) cannot reach it reliably across bundlers.
//! We therefore register the functions explicitly on `window` via
//! `js_sys::Reflect::set` + `wasm_bindgen::Closure`, mirroring how the
//! rest of the app exposes browser-facing hooks.
//!
//! ### When it becomes available
//!
//! [`install_window_hook`] is invoked once at app startup (from
//! `dioxus-ui`'s `main`, before any meeting is joined) so the test can
//! pre-arm or impair at any point in the call lifecycle. It is idempotent
//! via a [`std::sync::Once`]; re-registering would leak the previous
//! `Closure`s.
//!
//! ## Compile-out guarantee
//!
//! Gated by `#[cfg(feature = "netsim")]` like the rest of the netsim
//! plumbing. Default builds never register the hook and never touch
//! `window`.

use std::sync::Arc;

use log::{info, warn};
use videocall_netsim::{resolve_profile, Direction, NetSimShim};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;

use super::netsim_hook::{clear_hook, install_hook_for_direction};

/// Parse the `"up"` / `"down"` direction string. Case-insensitive,
/// trimmed. Returns `None` for anything else.
fn parse_direction(s: &str) -> Option<Direction> {
    match s.trim().to_ascii_lowercase().as_str() {
        "up" => Some(Direction::Up),
        "down" => Some(Direction::Down),
        _ => None,
    }
}

/// Core install logic shared by the window hook and the URL plumbing.
/// Resolves `profile_name` against the built-in presets and installs a
/// shim in the slot for `direction`. Returns `true` on success.
///
/// A `"none"` / passthrough profile is a valid install (it stores a
/// passthrough shim that [`super::netsim_hook::consult`] short-circuits)
/// — callers that want to REMOVE shaping should use [`clear_hook`] (or
/// the JS `clear()`), which empties both slots.
pub(super) fn install_profile(profile_name: &str, direction: Direction) -> bool {
    let name = profile_name.trim().to_ascii_lowercase();
    let Some(profile) = resolve_profile(&name) else {
        warn!("netsim: unknown profile '{name}' requested, ignoring");
        return false;
    };
    if let Err(e) = profile.validate() {
        warn!("netsim: profile '{name}' failed validation: {e}");
        return false;
    }
    info!("netsim: installing profile '{name}' direction={direction:?} (runtime)");
    // `NetSimShim` is `!Sync` on wasm32 (RefCell interior) but the wasm
    // runtime is single-threaded, so an `Arc<NetSimShim>` is safe — the
    // thread-local stores `Option<Arc<_>>` so it can hand out clones.
    #[allow(clippy::arc_with_non_send_sync)]
    let arc = Arc::new(NetSimShim::new(profile, direction));
    install_hook_for_direction(arc);
    true
}

/// Register `window.__vcNetsim` with `install(profile, direction)` and
/// `clear()`. Idempotent (first call per tab wins).
///
/// Returns `false` when there is no `window` (worker / non-browser
/// context) or registration failed; `true` when the object is present
/// on `window` after the call.
pub fn install_window_hook() -> bool {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut ok = false;
    ONCE.call_once(|| {
        ok = register_on_window();
    });
    // On subsequent calls `ONCE` is already consumed; report whether the
    // object is currently present rather than re-registering.
    if !ok {
        if let Some(window) = web_sys::window() {
            if let Ok(existing) = js_sys::Reflect::get(&window, &JsValue::from_str("__vcNetsim")) {
                return existing.is_object();
            }
        }
    }
    ok
}

fn register_on_window() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };

    let obj = js_sys::Object::new();

    // install(profileName: string, direction: "up"|"down") -> bool
    let install = Closure::<dyn Fn(JsValue, JsValue) -> JsValue>::new(
        |profile: JsValue, direction: JsValue| -> JsValue {
            let Some(profile) = profile.as_string() else {
                warn!("__vcNetsim.install: profile name must be a string");
                return JsValue::from_bool(false);
            };
            let dir_str = direction.as_string().unwrap_or_default();
            let Some(dir) = parse_direction(&dir_str) else {
                warn!("__vcNetsim.install: direction must be \"up\" or \"down\", got {dir_str:?}");
                return JsValue::from_bool(false);
            };
            JsValue::from_bool(install_profile(&profile, dir))
        },
    );

    // clear() -> void  (clears BOTH uplink and downlink slots)
    let clear = Closure::<dyn Fn()>::new(|| {
        info!("netsim: clearing all shaping (runtime)");
        clear_hook();
    });

    let set_ok = js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("install"),
        install.as_ref().unchecked_ref(),
    )
    .and_then(|_| {
        js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("clear"),
            clear.as_ref().unchecked_ref(),
        )
    })
    .and_then(|_| js_sys::Reflect::set(&window, &JsValue::from_str("__vcNetsim"), &obj))
    .is_ok();

    if !set_ok {
        warn!("netsim: failed to register window.__vcNetsim");
        return false;
    }

    // Leak the closures so they outlive this function for the tab's
    // lifetime — the window object now holds them and may call back at
    // any time. This is a one-time, bounded leak (two closures per tab,
    // installed once via the `Once` in `install_window_hook`).
    install.forget();
    clear.forget();

    info!("netsim: window.__vcNetsim installed (install/clear)");
    true
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn parse_direction_accepts_up_and_down_case_insensitive() {
        assert_eq!(parse_direction("up"), Some(Direction::Up));
        assert_eq!(parse_direction("DOWN"), Some(Direction::Down));
        assert_eq!(parse_direction("  Down  "), Some(Direction::Down));
    }

    #[test]
    fn parse_direction_rejects_garbage() {
        assert_eq!(parse_direction(""), None);
        assert_eq!(parse_direction("sideways"), None);
        assert_eq!(parse_direction("updown"), None);
    }
}
