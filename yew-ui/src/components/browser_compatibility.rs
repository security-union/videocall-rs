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

        // First check if this is Firefox and block it specifically
        if Self::is_firefox() {
            return Some(
                "🦊 Firefox Detected! Unfortunately, videocall.rs doesn't support Firefox due to incomplete MediaStreamTrackProcessor implementation. Please use Desktop Chrome, Chromium, Brave, or Edge for the best experience. 🚀".to_string()
            );
        }

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

        if !missing_features.is_empty() {
            Some(format!(
                "Hey friend! 👋 Thanks for trying videocall.rs! We're working hard to support your browser, but we need a few more modern features to make the magic happen. Your browser is missing: {}. We recommend using Desktop Chrome, Chromium, Brave, or Edge for the best experience. 🚀",
                missing_features.join(", ")
            ))
        } else {
            None
        }
    }

    fn is_firefox() -> bool {
        if let Some(window) = web_sys::window() {
            if let Ok(user_agent) = window.navigator().user_agent() {
                let ua_lower = user_agent.to_lowercase();

                // Check for Firefox user agent patterns
                let has_firefox = ua_lower.contains("firefox");
                let has_gecko = ua_lower.contains("gecko");
                let has_chrome = ua_lower.contains("chrome");
                let has_safari = ua_lower.contains("safari");
                let has_like_gecko = ua_lower.contains("like gecko");

                // Firefox detection: has "firefox" OR (has "gecko" but NOT "chrome" AND NOT "safari" AND NOT "like gecko")
                // The "like gecko" check is important because Safari and Chrome include "like Gecko" in their user agents
                let is_firefox =
                    has_firefox || (has_gecko && !has_chrome && !has_safari && !has_like_gecko);

                log::info!("Firefox detection: UA='{}', HasFirefox={}, HasGecko={}, HasChrome={}, HasSafari={}, HasLikeGecko={}, IsFirefox={}", 
                    user_agent, has_firefox, has_gecko, has_chrome, has_safari, has_like_gecko, is_firefox);

                return is_firefox;
            }
        }
        false
    }

    // Helper function for testing Firefox detection with custom user agent
    #[cfg(test)]
    fn is_firefox_from_ua(user_agent: &str) -> bool {
        let ua_lower = user_agent.to_lowercase();
        let has_firefox = ua_lower.contains("firefox");
        let has_gecko = ua_lower.contains("gecko");
        let has_chrome = ua_lower.contains("chrome");
        let has_safari = ua_lower.contains("safari");
        let has_like_gecko = ua_lower.contains("like gecko");

        has_firefox || (has_gecko && !has_chrome && !has_safari && !has_like_gecko)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_firefox_detection() {
        // Test Firefox user agents
        assert!(BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:91.0) Gecko/20100101 Firefox/91.0"
        ));

        assert!(BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:91.0) Gecko/20100101 Firefox/91.0"
        ));

        assert!(BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (X11; Linux x86_64; rv:91.0) Gecko/20100101 Firefox/91.0"
        ));

        // Test Chrome user agents (should not be detected as Firefox)
        assert!(!BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"
        ));

        assert!(!BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"
        ));

        // Test Edge user agents (should not be detected as Firefox)
        assert!(!BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36 Edg/91.0.864.59"
        ));

        // Test Safari user agents (should not be detected as Firefox)
        assert!(!BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/14.1.1 Safari/605.1.15"
        ));

        // Test the specific Safari user agent that was incorrectly detected as Firefox
        assert!(!BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Safari/605.1.15"
        ));

        // Test more Safari variants to ensure robustness
        assert!(!BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 15_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.0 Mobile/15E148 Safari/604.1"
        ));

        assert!(!BrowserCompatibility::is_firefox_from_ua(
            "Mozilla/5.0 (iPad; CPU OS 15_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.0 Mobile/15E148 Safari/604.1"
        ));
    }
}
