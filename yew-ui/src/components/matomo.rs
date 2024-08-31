use gloo_utils::window;
use js_sys::Array;
use js_sys::Reflect;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;
use web_sys::js_sys;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = window)]
    static _PAQ: Array;
}

pub struct MatomoTracker {}

impl MatomoTracker {
    pub fn new() -> Self {
        Self {}
    }

    pub fn push(&self, args: &JsValue) {
        let method: js_sys::Function = js_sys::Reflect::get(&_PAQ, &"push".into()).unwrap().into();
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