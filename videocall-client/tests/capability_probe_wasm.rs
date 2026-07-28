/*
 * Copyright 2026 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

#![cfg(target_arch = "wasm32")]

// Keep these in an integration target: the library wasm test binary is
// browser-configured and is intentionally skipped by CI's `wasm-pack test --node`.

use js_sys::{Object, Reflect};
use videocall_client::capability_probe::{parse_support, resolved_hw_hint};
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

fn set_property(target: &JsValue, key: &str, value: &JsValue) {
    Reflect::set(target, &JsValue::from_str(key), value)
        .expect("synthetic result property must be writable");
}

fn support_result(supported: bool, hardware_acceleration: Option<JsValue>) -> JsValue {
    let result: JsValue = Object::new().into();
    set_property(&result, "supported", &JsValue::from_bool(supported));

    if let Some(hardware_acceleration) = hardware_acceleration {
        let config: JsValue = Object::new().into();
        set_property(&config, "hardwareAcceleration", &hardware_acceleration);
        set_property(&result, "config", &config);
    }

    result
}

#[wasm_bindgen_test]
fn parses_supported_prefer_hardware_result() {
    let result = support_result(true, Some(JsValue::from_str("prefer-hardware")));

    assert_eq!(parse_support(&result), Some(true));
    assert!(resolved_hw_hint(&result));
}

#[wasm_bindgen_test]
fn parses_supported_prefer_software_result() {
    let result = support_result(true, Some(JsValue::from_str("prefer-software")));

    assert_eq!(parse_support(&result), Some(true));
    assert!(!resolved_hw_hint(&result));
}

#[wasm_bindgen_test]
fn parses_unsupported_result_without_config() {
    let result = support_result(false, None);

    assert_eq!(parse_support(&result), Some(false));
    assert!(!resolved_hw_hint(&result));
}

#[wasm_bindgen_test]
fn hardware_hint_defaults_false_for_missing_and_non_string_values() {
    let missing_config = support_result(true, None);
    assert!(!resolved_hw_hint(&missing_config));

    let missing_hint: JsValue = Object::new().into();
    let empty_config: JsValue = Object::new().into();
    set_property(&missing_hint, "config", &empty_config);
    assert!(!resolved_hw_hint(&missing_hint));

    let non_string = support_result(true, Some(JsValue::from_f64(1.0)));
    assert!(!resolved_hw_hint(&non_string));
}

#[wasm_bindgen_test]
fn support_parser_rejects_missing_and_non_boolean_values() {
    let missing: JsValue = Object::new().into();
    assert_eq!(parse_support(&missing), None);

    let non_boolean: JsValue = Object::new().into();
    set_property(&non_boolean, "supported", &JsValue::from_str("true"));
    assert_eq!(parse_support(&non_boolean), None);
}
