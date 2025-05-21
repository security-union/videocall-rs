use wasm_bindgen::JsValue;
use web_sys::*;
use yew::prelude::*;

#[derive(Properties, Debug, PartialEq)]
pub struct BrowserCompatibilityProps {}

pub struct BrowserCompatibility {
    error: Option<String>,
}

impl Component for BrowserCompatibility {
    type Message = ();
    type Properties = BrowserCompatibilityProps;

    fn create(_ctx: &Context<Self>) -> Self {
        log::info!("Checking browser compatibility");
        let error = Self::check_browser_compatibility();
        if let Some(error) = &error {
            log::error!("Browser compatibility check failed: {}", error);
        } else {
            log::info!("Browser compatibility check passed");
        }
        Self { error }
    }

    fn view(&self, _ctx: &Context<Self>) -> Html {
        if let Some(error) = &self.error {
            html! {
                <div class="error-container">
                    <p class="error-message">{ error }</p>
                    <img src="/assets/street_fighter.gif" alt="Permission instructions" class="instructions-gif" />
                </div>
            }
        } else {
            html! {}
        }
    }
}

impl BrowserCompatibility {
    fn check_browser_compatibility() -> Option<String> {
        let window = web_sys::window().unwrap();
        let mut missing_features = Vec::new();

        // Check for MediaStreamTrackProcessor
        if js_sys::Reflect::get(&window, &JsValue::from_str("MediaStreamTrackProcessor"))
            .unwrap()
            .is_undefined()
        {
            missing_features.push("MediaStreamTrackProcessor");
        }

        // Check for VideoEncoder
        if js_sys::Reflect::get(&window, &JsValue::from_str("VideoEncoder"))
            .unwrap()
            .is_undefined()
        {
            missing_features.push("VideoEncoder");
        }

        // Check for AudioEncoder
        // if js_sys::Reflect::get(&window, &JsValue::from_str("AudioEncoder"))
        //     .unwrap()
        //     .is_undefined()
        // {
        //     missing_features.push("AudioEncoder");
        // }

        if !missing_features.is_empty() {
            Some(format!(
                "Hey friend! 👋 Thanks for trying videocall.rs! We're working hard to support your browser, but we need a few more modern features to make the magic happen. Your browser is missing: {}. We recommend using Desktop Chrome, Chromium, Brave, or Edge for the best experience. 🚀",
                missing_features.join(", ")
            ))
        } else {
            None
        }
    }
}
