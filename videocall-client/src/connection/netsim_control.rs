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
//! // Synthetically bump the publisher-uplink-distress counters the encoders
//! // read, so the single-layer audio uplink-distress detector fires (#1398):
//! window.__vcNetsim.bumpUplinkStall(8); // WT slow-ready() saturation events
//! window.__vcNetsim.bumpWsDrop(6);      // WS send-buffer drops
//! window.__vcNetsim.bumpWtDrop(6);      // WT write-drop (teardown) events
//! window.__vcNetsim.bumpStaleDeltaDrop(15); // camera stale-delta age-drops
//! window.__vcNetsim.bumpWsStaleDeltaDrop(15); // camera WS freshness-gate drops
//! window.__vcNetsim.bumpCameraUplinkStall(8); // camera WT slow-ready() events
//! window.__vcNetsim.setWsBufferedOverride(131072); // stand-in WS bufferedAmount; null clears
//! window.__vcNetsim.forceCameraKeyframe(); // raise the camera PLI flag
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

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use log::{info, warn};
use videocall_netsim::{resolve_profile, Direction, NetSimShim};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;

use super::netsim_hook::{clear_hook, install_hook_for_direction};
use super::webmedia::MediaStreamKey;

thread_local! {
    static WS_BUFFERED_OVERRIDE: Cell<Option<u64>> = const { Cell::new(None) };
    static CAMERA_FORCE_KEYFRAME: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Registered by `VideoCallClient::new`; the last registration wins.
pub(crate) fn register_camera_force_keyframe_for_netsim(flag: Arc<AtomicBool>) {
    CAMERA_FORCE_KEYFRAME.with(|cell| *cell.borrow_mut() = Some(flag));
}

pub(crate) fn force_camera_keyframe_for_netsim() -> bool {
    CAMERA_FORCE_KEYFRAME.with(|cell| match cell.borrow().as_ref() {
        Some(flag) => {
            flag.store(true, Ordering::Release);
            true
        }
        None => false,
    })
}

pub(crate) fn ws_buffered_override_for_netsim() -> Option<u64> {
    WS_BUFFERED_OVERRIDE.with(Cell::get)
}

pub(crate) fn set_ws_buffered_override_for_netsim(bytes: Option<u64>) {
    WS_BUFFERED_OVERRIDE.with(|cell| cell.set(bytes));
}

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

    // bumpUplinkStall(n: number) -> bool  (issue #1398)
    // Synthetically records `n` audio-attributed WT ready stalls. The helper also
    // preserves the aggregate counter used by camera/screen AQ. The real
    // increment happens on a slow `writer.ready()` deep in the `.await`-blocking
    // media send path, which a localhost-loopback e2e cannot reliably induce;
    // this lets netsim drive the SAME audio slot the mic-side single-layer
    // uplink-distress detector reads. `n` is coerced from a JS number; a
    // non-number / negative is treated as 0 (a no-op bump → false).
    let bump_uplink_stall = Closure::<dyn Fn(JsValue) -> JsValue>::new(|n: JsValue| -> JsValue {
        let count = n.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64);
        match count {
            Some(c) => {
                videocall_transport::webtransport::force_unistream_ready_stall(c);
                info!("__vcNetsim.bumpUplinkStall: +{c} WT ready-stall events");
                JsValue::from_bool(true)
            }
            None => {
                warn!("__vcNetsim.bumpUplinkStall: argument must be a non-negative number");
                JsValue::from_bool(false)
            }
        }
    });

    // bumpWsDrop(n: number) -> bool  (issue #1398)
    // The WebSocket analogue of `bumpUplinkStall`: records `n` audio-attributed
    // WS send-buffer drops while preserving the aggregate counter, so the
    // detector's WS axis can be exercised on a WS-transport e2e run.
    let bump_ws_drop = Closure::<dyn Fn(JsValue) -> JsValue>::new(|n: JsValue| -> JsValue {
        let count = n.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64);
        match count {
            Some(c) => {
                videocall_transport::websocket::force_websocket_drop(c);
                info!("__vcNetsim.bumpWsDrop: +{c} WS send-buffer drops");
                JsValue::from_bool(true)
            }
            None => {
                warn!("__vcNetsim.bumpWsDrop: argument must be a non-negative number");
                JsValue::from_bool(false)
            }
        }
    });

    // bumpWtDrop(n: number) -> bool  (issue #1616, follow-up to #1398)
    // The third uplink-distress axis: records `n` audio-attributed WT write
    // drops while preserving the aggregate counter. The real increment happens
    // when an established unistream write fails (a teardown-class drop) deep in
    // the `.await`-blocking media send path, which a localhost-loopback e2e
    // cannot reliably induce; this drives the SAME audio slot the mic detector
    // reads so its WT-drop axis can be exercised in isolation. Same coercion
    // contract as the siblings above: a non-number / negative is a no-op.
    let bump_wt_drop = Closure::<dyn Fn(JsValue) -> JsValue>::new(|n: JsValue| -> JsValue {
        let count = n.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64);
        match count {
            Some(c) => {
                videocall_transport::webtransport::force_unistream_drop(c);
                info!("__vcNetsim.bumpWtDrop: +{c} WT write-drop events");
                JsValue::from_bool(true)
            }
            None => {
                warn!("__vcNetsim.bumpWtDrop: argument must be a non-negative number");
                JsValue::from_bool(false)
            }
        }
    });

    // bumpStaleDeltaDrop(n: number) -> bool  (issue #1737)
    // Camera sender-side age-drop axis: increments the WT stale-delta drop
    // counter (`unistream_stale_delta_drop_count`) by `n` so the camera AQ
    // monitor can exercise the same counter it reads after send-path age drops.
    // Same coercion contract as the siblings above: a non-number / negative is
    // treated as 0 (a no-op -> false).
    let bump_stale_delta_drop =
        Closure::<dyn Fn(JsValue) -> JsValue>::new(|n: JsValue| -> JsValue {
            let count = n.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64);
            match count {
                Some(c) => {
                    videocall_transport::webtransport::force_unistream_stale_delta_drop(c);
                    info!("__vcNetsim.bumpStaleDeltaDrop: +{c} camera stale-delta age-drops");
                    JsValue::from_bool(true)
                }
                None => {
                    warn!("__vcNetsim.bumpStaleDeltaDrop: argument must be a non-negative number");
                    JsValue::from_bool(false)
                }
            }
        });

    let bump_ws_stale_delta_drop =
        Closure::<dyn Fn(JsValue) -> JsValue>::new(|n: JsValue| -> JsValue {
            let count = n.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64);
            match count {
                Some(c) => {
                    crate::encode::camera_encoder::bump_camera_ws_stale_delta_drops_for_netsim(c);
                    info!("__vcNetsim.bumpWsStaleDeltaDrop: +{c} camera WS freshness-gate drops");
                    JsValue::from_bool(true)
                }
                None => {
                    warn!(
                        "__vcNetsim.bumpWsStaleDeltaDrop: argument must be a non-negative number"
                    );
                    JsValue::from_bool(false)
                }
            }
        });

    let bump_camera_uplink_stall =
        Closure::<dyn Fn(JsValue) -> JsValue>::new(|n: JsValue| -> JsValue {
            let count = n.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64);
            match count {
                Some(c) => {
                    videocall_transport::webtransport::force_unistream_ready_stall_for_stream(
                        MediaStreamKey::Video.as_u8(),
                        c,
                    );
                    info!("__vcNetsim.bumpCameraUplinkStall: +{c} camera WT ready-stall events");
                    JsValue::from_bool(true)
                }
                None => {
                    warn!(
                        "__vcNetsim.bumpCameraUplinkStall: argument must be a non-negative number"
                    );
                    JsValue::from_bool(false)
                }
            }
        });

    let set_ws_buffered_override = Closure::<dyn Fn(JsValue) -> JsValue>::new(
        |bytes: JsValue| -> JsValue {
            if bytes.is_null() || bytes.is_undefined() {
                set_ws_buffered_override_for_netsim(None);
                info!("__vcNetsim.setWsBufferedOverride: cleared");
                return JsValue::from_bool(true);
            }
            match bytes.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64) {
                Some(b) => {
                    set_ws_buffered_override_for_netsim(Some(b));
                    info!("__vcNetsim.setWsBufferedOverride: {b} bytes");
                    JsValue::from_bool(true)
                }
                None => {
                    warn!(
                        "__vcNetsim.setWsBufferedOverride: argument must be a non-negative number or null"
                    );
                    JsValue::from_bool(false)
                }
            }
        },
    );

    let force_camera_keyframe = Closure::<dyn Fn() -> JsValue>::new(|| -> JsValue {
        let raised = force_camera_keyframe_for_netsim();
        if raised {
            info!("__vcNetsim.forceCameraKeyframe: camera PLI flag raised");
        } else {
            warn!("__vcNetsim.forceCameraKeyframe: no VideoCallClient has registered its flag");
        }
        JsValue::from_bool(raised)
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
    .and_then(|_| {
        js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("bumpUplinkStall"),
            bump_uplink_stall.as_ref().unchecked_ref(),
        )
    })
    .and_then(|_| {
        js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("bumpWsDrop"),
            bump_ws_drop.as_ref().unchecked_ref(),
        )
    })
    .and_then(|_| {
        js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("bumpWtDrop"),
            bump_wt_drop.as_ref().unchecked_ref(),
        )
    })
    .and_then(|_| {
        js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("bumpStaleDeltaDrop"),
            bump_stale_delta_drop.as_ref().unchecked_ref(),
        )
    })
    .and_then(|_| {
        js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("bumpWsStaleDeltaDrop"),
            bump_ws_stale_delta_drop.as_ref().unchecked_ref(),
        )
    })
    .and_then(|_| {
        js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("bumpCameraUplinkStall"),
            bump_camera_uplink_stall.as_ref().unchecked_ref(),
        )
    })
    .and_then(|_| {
        js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("setWsBufferedOverride"),
            set_ws_buffered_override.as_ref().unchecked_ref(),
        )
    })
    .and_then(|_| {
        js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("forceCameraKeyframe"),
            force_camera_keyframe.as_ref().unchecked_ref(),
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
    // any time. This is a one-time, bounded leak (ten closures per tab,
    // installed once via the `Once` in `install_window_hook`).
    install.forget();
    clear.forget();
    bump_uplink_stall.forget();
    bump_ws_drop.forget();
    bump_wt_drop.forget();
    bump_stale_delta_drop.forget();
    bump_ws_stale_delta_drop.forget();
    bump_camera_uplink_stall.forget();
    set_ws_buffered_override.forget();
    force_camera_keyframe.forget();

    info!(
        "netsim: window.__vcNetsim installed (install/clear/bumpUplinkStall/bumpWsDrop/bumpWtDrop/bumpStaleDeltaDrop/bumpWsStaleDeltaDrop/bumpCameraUplinkStall/setWsBufferedOverride/forceCameraKeyframe)"
    );
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

    #[test]
    fn force_camera_keyframe_raises_the_registered_flag() {
        assert!(!force_camera_keyframe_for_netsim());
        let flag = Arc::new(AtomicBool::new(false));
        register_camera_force_keyframe_for_netsim(flag.clone());
        assert!(force_camera_keyframe_for_netsim());
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn force_camera_keyframe_raises_only_the_last_registered_flag() {
        let first = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        register_camera_force_keyframe_for_netsim(first.clone());
        register_camera_force_keyframe_for_netsim(second.clone());
        assert!(force_camera_keyframe_for_netsim());
        assert!(!first.load(Ordering::Acquire));
        assert!(second.load(Ordering::Acquire));
    }
}
