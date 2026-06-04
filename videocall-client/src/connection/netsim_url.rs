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

//! URL-param shim for the browser bot's network simulator
//! (phase 3c of discussion #793).
//!
//! When `videocall-client` is built with `--features netsim` and the
//! browser tab's URL carries `?netsim=<profile>`, this module
//! constructs a [`videocall_netsim::NetSimShim`] from the named
//! profile and parks it in the per-tab hook slot via
//! [`super::netsim_hook::install_hook`]. The shim then shapes all
//! outbound media on this tab.
//!
//! Phase 3d will have the bots-app harness launch a Chrome instance
//! at `<meeting-url>?netsim=lossy_mobile` and rely on this module to
//! install the profile during the client's first
//! [`super::connection::Connection::connect`] call. No other caller
//! needs to invoke [`try_install_from_url`] directly.
//!
//! ## Compile-out guarantee
//!
//! The entire module is gated by `#[cfg(feature = "netsim")]`.
//! Default builds (no `netsim` feature) never see this code, never
//! touch `window.location`, and never link `videocall-netsim`. The
//! production code path is byte-for-byte equivalent to pre-3c.

use std::sync::Arc;

use log::{info, warn};
use videocall_netsim::{resolve_profile, Direction, NetSimShim};

use super::netsim_hook::install_hook;

/// Inspect `window.location.search` for `?netsim=<profile>` and, if
/// the profile name resolves via [`videocall_netsim::resolve_profile`],
/// build a [`NetSimShim`] in [`Direction::Up`] and install it in the
/// per-tab hook slot.
///
/// Returns `true` when a shim was successfully installed, `false`
/// otherwise. Reasons to return `false`:
/// - `window` is undefined (worker / SSR-ish context),
/// - `window.location.search` is missing or empty,
/// - no `netsim=` key is present among the query params,
/// - the param value does not resolve to a known preset.
///
/// Never panics. Callers should not depend on this returning
/// anything other than completion — the installed hook is the
/// observable effect.
pub(super) fn try_install_from_url() -> bool {
    let Some(window) = web_sys::window() else {
        // Worker / non-browser context. No `window`, nothing to do.
        return false;
    };

    let search = match window.location().search() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let Some(raw_value) = find_param(&search, "netsim") else {
        return false;
    };

    // URL-decode (cheap; preset names are ASCII identifiers so this
    // is realistically a no-op, but doing it right costs nothing
    // and tolerates a stray `%20` etc. from a script-generated URL).
    let decoded = match js_sys::decode_uri_component(&raw_value) {
        Ok(js_str) => js_str.as_string().unwrap_or(raw_value.clone()),
        Err(_) => raw_value.clone(),
    };
    let name = decoded.trim().to_ascii_lowercase();

    let Some(profile) = resolve_profile(&name) else {
        warn!("netsim: unknown profile '{name}' in ?netsim=, ignoring");
        return false;
    };

    info!("netsim: installing profile '{name}' from URL");
    let shim = NetSimShim::new(profile, Direction::Up);
    // `NetSimShim` is `!Sync` on wasm32 (uses `RefCell` for poison
    // safety — see PR #811 finding 2 and the `NetSimShim` doc
    // comment) but the wasm runtime is single-threaded, so an
    // `Arc<NetSimShim>` here is safe. The thread-local in
    // `netsim_hook` stores `Option<Arc<NetSimShim>>` so it can hand
    // out `Arc` clones without taking ownership; switching to `Rc`
    // would force every consumer to change, when the actual
    // multi-thread sharing risk is zero on wasm32.
    #[allow(clippy::arc_with_non_send_sync)]
    let arc = Arc::new(shim);
    install_hook(Some(arc));
    true
}

/// Find the value of `key` in a `?k1=v1&k2=v2`-style query string.
/// The leading `?` is optional and stripped. Returns the raw
/// (still URL-encoded) value, or `None` if absent.
///
/// Pulling in a URL crate for this is overkill — preset names are
/// ASCII identifiers and the search string is short.
fn find_param(search: &str, key: &str) -> Option<String> {
    let stripped = search.strip_prefix('?').unwrap_or(search);
    if stripped.is_empty() {
        return None;
    }
    for pair in stripped.split('&') {
        let mut parts = pair.splitn(2, '=');
        let k = parts.next()?;
        if k == key {
            // `key=` with no value → empty string (still "present").
            return Some(parts.next().unwrap_or("").to_string());
        }
    }
    None
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::find_param;

    #[test]
    fn find_param_picks_named_key() {
        assert_eq!(
            find_param("?meeting=abc&netsim=lossy_mobile", "netsim"),
            Some("lossy_mobile".to_string())
        );
    }

    #[test]
    fn find_param_handles_leading_question_optional() {
        assert_eq!(
            find_param("netsim=good_wifi", "netsim"),
            Some("good_wifi".to_string())
        );
    }

    #[test]
    fn find_param_missing_returns_none() {
        assert_eq!(find_param("?meeting=abc", "netsim"), None);
    }

    #[test]
    fn find_param_empty_search_returns_none() {
        assert_eq!(find_param("", "netsim"), None);
        assert_eq!(find_param("?", "netsim"), None);
    }

    #[test]
    fn find_param_value_can_be_empty() {
        assert_eq!(find_param("?netsim=", "netsim"), Some("".to_string()));
    }

    #[test]
    fn find_param_ignores_partial_key_match() {
        // `netsim_other` must not match key `netsim`.
        assert_eq!(find_param("?netsim_other=x", "netsim"), None);
    }
}
