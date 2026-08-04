// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue #1768: the `experimentalReducedLadder` runtime flag must resolve to the
// right `LadderVariant` through the PRODUCTION accessor
// (`dioxus_ui::constants::camera_ladder_variant`), reading a real
// `window.__APP_CONFIG` the way the browser does.
//
// WHY THIS FILE EXISTS (pre-submit gate finding). The unit tests around this
// feature stop one step short of the production path on BOTH sides:
//   * the `RuntimeConfig` parse tests apply `videocall_types::truthy` themselves,
//     so they pin the FIELD and the truthiness table but never call the accessor;
//   * the `videocall-client` encoder/controller tests are handed a
//     `LadderVariant` directly, so they pin the CONSUMER but never the flag.
// Neither would fail if `camera_ladder_variant()` were hardcoded to
// `LadderVariant::Default` — i.e. if the whole gate were silently dead. This
// closes that seam: it is the only test that fails when the config→variant
// resolution itself breaks.
//
// It runs in a real browser (`wasm_bindgen_test`), which is required — the
// accessor reads `window.__APP_CONFIG` via the memoized `app_config()`.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use wasm_bindgen_test::*;

use videocall_client::adaptive_quality_constants::LadderVariant;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Install a minimal valid `__APP_CONFIG`. When `reduced` is `Some(v)` the
/// `experimentalReducedLadder` key is set to `v`; when `None` the key is OMITTED
/// entirely — the config.js bind-mount case (a deployed config predating this
/// key must still parse and must mean "off").
fn inject_app_config(reduced: Option<&str>) {
    let config = js_sys::Object::new();
    let set = |key: &str, val: &wasm_bindgen::JsValue| {
        js_sys::Reflect::set(&config, &key.into(), val).unwrap();
    };
    set("apiBaseUrl", &"http://test:8080".into());
    set("wsUrl", &"ws://test:8080".into());
    set("webTransportHost", &"https://test:4433".into());
    set("oauthEnabled", &"false".into());
    set("e2eeEnabled", &"false".into());
    set("webTransportEnabled", &"false".into());
    set("usersAllowedToStream", &"".into());
    set("serverElectionPeriodMs", &wasm_bindgen::JsValue::from(2000));
    if let Some(v) = reduced {
        set("experimentalReducedLadder", &v.into());
    }

    let frozen = js_sys::Object::freeze(&config);
    let window = gloo_utils::window();
    js_sys::Reflect::set(&window, &"__APP_CONFIG".into(), &frozen).unwrap();
    // `app_config()` memoizes the first successful parse (#1492), and this harness
    // reuses one wasm module across cases — clear the cache so THIS config is read.
    dioxus_ui::constants::reset_config_cache_for_test();
}

fn remove_app_config() {
    let window = gloo_utils::window();
    let _ = js_sys::Reflect::delete_property(&window.into(), &"__APP_CONFIG".into());
    dioxus_ui::constants::reset_config_cache_for_test();
}

/// `experimentalReducedLadder: "true"` must select the REDUCED ladder.
///
/// MUTATION: hardcode `camera_ladder_variant()` to `LadderVariant::Default` (i.e.
/// disable the feature) and this is the ONLY test in the repo that fails.
#[wasm_bindgen_test]
fn truthy_flag_selects_the_reduced_ladder() {
    // `videocall_types::truthy` accepts exactly `"true"`/`"1"` (case-insensitive).
    for raw in ["true", "1", "TRUE"] {
        inject_app_config(Some(raw));
        assert_eq!(
            dioxus_ui::constants::camera_ladder_variant(),
            LadderVariant::Reduced,
            "experimentalReducedLadder={raw:?} must select the reduced ladder"
        );
    }
    remove_app_config();
}

/// A falsy value must leave the SHIPPED ladder in place — the gate is default-OFF.
///
/// MUTATION: invert the branch in `camera_ladder_variant()` and this fails.
#[wasm_bindgen_test]
fn falsy_flag_keeps_the_default_ladder() {
    for raw in ["false", "0", "", "no", "yes"] {
        inject_app_config(Some(raw));
        assert_eq!(
            dioxus_ui::constants::camera_ladder_variant(),
            LadderVariant::Default,
            "experimentalReducedLadder={raw:?} must keep the shipped ladder"
        );
    }
    remove_app_config();
}

/// The config.js bind-mount trap: a deployed `config.js` that PREDATES this key
/// must still parse, and must mean "off" — never a startup-bricking parse failure
/// and never an accidental opt-in.
///
/// MUTATION: drop `#[serde(default)]` from `experimental_reduced_ladder` and the
/// parse fails, so `app_config()` returns Err; the accessor's `.unwrap_or(false)`
/// still yields Default here, but the companion assertion below (that the config
/// parses at all) catches it.
#[wasm_bindgen_test]
fn absent_key_parses_and_keeps_the_default_ladder() {
    inject_app_config(None);
    assert!(
        dioxus_ui::constants::app_config().is_ok(),
        "a config.js predating experimentalReducedLadder must still PARSE \
         (serde(default)); a parse failure would brick startup"
    );
    assert_eq!(
        dioxus_ui::constants::camera_ladder_variant(),
        LadderVariant::Default,
        "an absent key must mean the shipped ladder"
    );
    remove_app_config();
}

/// With NO `__APP_CONFIG` at all (the pre-config.js cold-load window, and the
/// native/headless case), the accessor must FAIL OPEN to the shipped ladder
/// rather than panic.
///
/// This also documents a real operational caveat: because the resolution is
/// fail-open, a call made before `config.js` installs `__APP_CONFIG` would
/// silently yield `Default` even with the flag set. `Host` reads it inside
/// `use_hook` (after config.js has run and frozen the object), so production is
/// safe — but a future caller moving the read earlier would silently disable the
/// gate, which is why the fail-open direction is pinned here.
#[wasm_bindgen_test]
fn missing_config_fails_open_to_the_default_ladder() {
    remove_app_config();
    assert_eq!(
        dioxus_ui::constants::camera_ladder_variant(),
        LadderVariant::Default,
        "an unreadable config must fail OPEN to the shipped ladder, not panic"
    );
}
