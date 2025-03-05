#![allow(non_upper_case_globals)]

use gloo_utils::window;
use js_sys::Array;
use js_sys::Reflect;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;
use web_sys::js_sys;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = window, thread_local_v2)]
    static _paq: Array;
}

pub struct MatomoTracker {}

impl MatomoTracker {
    pub fn new() -> Self {
        Self {}
    }

    pub fn push(&self, args: &JsValue) {
        // Get the _paq array from the window object since we can't directly use the thread_local variable
        let window = web_sys::window().expect("no global window exists");
        let paq_value = Reflect::get(&window, &JsValue::from_str("_paq")).unwrap();
        let method: js_sys::Function = js_sys::Reflect::get(&paq_value, &"push".into())
            .unwrap()
            .into();
        let _ = method.call1(&JsValue::NULL, args);
    }

    pub fn track_page_view(&self, title: &str, url: &str) {
        if !Reflect::has(&window(), &"_paq".into()).unwrap_or(false) {
            return;
        }
        // Create an array with commands
        let array = js_sys::Array::new();

        array.push(&JsValue::from_str("setCustomUrl"));
        array.push(&JsValue::from_str(url));
        self.push(&array.into());

        let array = js_sys::Array::new();
        array.push(&JsValue::from_str("setDocumentTitle"));
        array.push(&JsValue::from_str(title));
        self.push(&array.into());

        let array = js_sys::Array::new();
        array.push(&JsValue::from_str("trackPageView"));
        // Call the push method with the command array
        self.push(&array.into());
    }
}
