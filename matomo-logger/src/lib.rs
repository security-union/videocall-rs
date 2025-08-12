use log::LevelFilter;

#[cfg(target_arch = "wasm32")]
use log::{Level, Log, Metadata, Record};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
#[cfg(target_arch = "wasm32")]
use web_sys::{console, window};

#[derive(Clone, Debug)]
pub struct MatomoConfig {
    pub base_url: Option<String>, // e.g. https://matomo.videocall.rs/
    pub site_id: Option<u32>,
    pub console_level: LevelFilter,
    pub matomo_level: LevelFilter,
    pub inject_snippet: bool,
}

impl Default for MatomoConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            site_id: None,
            console_level: LevelFilter::Info,
            matomo_level: LevelFilter::Warn,
            inject_snippet: true,
        }
    }
}

pub struct MatomoLogger {
    #[allow(dead_code)]
    min_console_level: LevelFilter,
    #[allow(dead_code)]
    min_matomo_level: LevelFilter,
}

// Public facade: works on all targets by delegating to the platform implementation.
impl MatomoLogger {
    pub fn init(config: MatomoConfig) -> Result<(), log::SetLoggerError> {
        imp::init(config)
    }
}

pub fn track_page_view(title: &str, url: &str) {
    imp::track_page_view_impl(title, url)
}
pub fn set_user_id(user_id: &str) {
    imp::set_user_id_impl(user_id)
}

#[cfg(target_arch = "wasm32")]
fn log_to_console(threshold: LevelFilter, record: &Record) {
    if record.level().to_level_filter() > threshold {
        return;
    }
    let msg = format!(
        "{}: {} — {}",
        record.level(),
        record.target(),
        record.args()
    );
    match record.level() {
        Level::Error => console::error_1(&JsValue::from_str(&msg)),
        Level::Warn => console::warn_1(&JsValue::from_str(&msg)),
        Level::Info => console::info_1(&JsValue::from_str(&msg)),
        Level::Debug => console::log_1(&JsValue::from_str(&msg)),
        Level::Trace => console::debug_1(&JsValue::from_str(&msg)),
    }
}

#[cfg(target_arch = "wasm32")]
fn log_to_matomo(threshold: LevelFilter, record: &Record) {
    if record.level().to_level_filter() > threshold {
        return;
    }
    let mut name = format!("{} — {}", record.target(), record.args());
    if name.len() > 300 {
        name.truncate(300);
    }

    let arr = js_sys::Array::new();
    arr.push(&JsValue::from_str("trackEvent"));
    arr.push(&JsValue::from_str("RustLog"));
    arr.push(&JsValue::from_str(record.level().as_str()));
    arr.push(&JsValue::from_str(&name));
    let value = match record.level() {
        Level::Error => 50,
        Level::Warn => 40,
        Level::Info => 30,
        Level::Debug => 20,
        Level::Trace => 10,
    } as f64;
    arr.push(&JsValue::from_f64(value));

    if has_paq() {
        push_to_paq(&arr.into());
    }
}

#[cfg(target_arch = "wasm32")]
fn has_paq() -> bool {
    if let Some(w) = window() {
        js_sys::Reflect::has(&w, &JsValue::from_str("_paq")).unwrap_or(false)
    } else {
        false
    }
}

#[cfg(target_arch = "wasm32")]
fn push_to_paq(args: &JsValue) {
    if let Some(w) = window() {
        if let Ok(paq) = js_sys::Reflect::get(&w, &JsValue::from_str("_paq")) {
            if let Ok(push) = js_sys::Reflect::get(&paq, &JsValue::from_str("push")) {
                let func: js_sys::Function = push.into();
                // Use the _paq array as the 'this' value for push
                if let Err(e) = func.call1(&paq, args) {
                    console::error_2(&JsValue::from_str("_paq.push error"), &e);
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn maybe_inject_snippet(cfg: &MatomoConfig) {
    // Only in main thread with window
    let Some(w) = window() else {
        return;
    };
    if has_paq() {
        return;
    }
    let Some(base) = cfg.base_url.as_ref() else {
        return;
    };
    let Some(site) = cfg.site_id else {
        return;
    };

    // Create window._paq and push basic setup
    let paq = js_sys::Array::new();
    let _ = js_sys::Reflect::set(&w, &JsValue::from_str("_paq"), &paq);

    let set_tracker = js_sys::Array::new();
    set_tracker.push(&JsValue::from_str("setTrackerUrl"));
    set_tracker.push(&JsValue::from_str(&(base.to_string() + "matomo.php")));
    push_to_paq(&set_tracker.into());

    let set_site = js_sys::Array::new();
    set_site.push(&JsValue::from_str("setSiteId"));
    set_site.push(&JsValue::from_str(&site.to_string()));
    push_to_paq(&set_site.into());

    let enable = js_sys::Array::new();
    enable.push(&JsValue::from_str("enableLinkTracking"));
    push_to_paq(&enable.into());

    // Inject matomo.js script tag
    if let Some(doc) = w.document() {
        if let Ok(script) = doc.create_element("script") {
            script.set_attribute("async", "true").ok();
            script
                .set_attribute("src", &(base.to_string() + "matomo.js"))
                .ok();
            if let Some(head) = doc.head() {
                let _ = head.append_child(&script);
            }
        }
    }
}

// Worker bridge API (simple): serialize log event and let host push it to Matomo.
#[cfg(all(target_arch = "wasm32", feature = "worker"))]
pub mod worker {
    use super::*;
    use web_sys::DedicatedWorkerGlobalScope;

    /// Install a worker-side logger that forwards WARN+ records to the main thread via postMessage.
    /// The `sender` argument is accepted for compatibility but is ignored; the logger uses postMessage directly.
    pub fn init_with_bridge(
        console_level: LevelFilter,
        matomo_level: LevelFilter,
        _sender: js_sys::Function,
    ) -> Result<(), log::SetLoggerError> {
        let logger = BridgeLogger {
            console_level,
            matomo_level,
        };
        // Leak to obtain 'static lifetime required by log::set_logger
        let leaked: &'static BridgeLogger = Box::leak(Box::new(logger));
        log::set_logger(leaked)?;
        log::set_max_level(console_level.max(matomo_level));
        Ok(())
    }

    struct BridgeLogger {
        console_level: LevelFilter,
        matomo_level: LevelFilter,
    }
    impl Log for BridgeLogger {
        fn enabled(&self, _metadata: &Metadata) -> bool {
            true
        }
        fn log(&self, record: &Record) {
            super::log_to_console(self.console_level, record);
            if record.level().to_level_filter() > self.matomo_level {
                return;
            }
            // Build a compact log object and post to main thread
            let obj = js_sys::Object::new();
            let _ =
                js_sys::Reflect::set(&obj, &JsValue::from_str("type"), &JsValue::from_str("log"));
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("level"),
                &JsValue::from_str(record.level().as_str()),
            );
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("target"),
                &JsValue::from_str(record.target()),
            );
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("message"),
                &JsValue::from_str(&format!("{}", record.args())),
            );
            let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
            let _ = scope.post_message(&obj);
        }
        fn flush(&self) {}
    }
}

// Host build (non-wasm or no `worker` feature): provide a stub so root clippy/fmt succeeds
#[cfg(not(all(target_arch = "wasm32", feature = "worker")))]
pub mod worker {
    use super::*;
    // Accept the same signature but ignore the sender type via a generic to avoid js_sys import
    pub fn init_with_bridge<T>(
        _console_level: LevelFilter,
        _matomo_level: LevelFilter,
        _sender: T,
    ) -> Result<(), log::SetLoggerError> {
        Ok(())
    }
}

// ---------------- Platform-specific implementations -----------------

// WASM implementation
#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;

    impl Log for MatomoLogger {
        fn enabled(&self, _metadata: &Metadata) -> bool {
            true
        }
        fn log(&self, record: &Record) {
            super::log_to_console(self.min_console_level, record);
            super::log_to_matomo(self.min_matomo_level, record);
        }
        fn flush(&self) {}
    }

    pub fn init(config: MatomoConfig) -> Result<(), log::SetLoggerError> {
        if config.inject_snippet {
            super::maybe_inject_snippet(&config);
        }
        let logger = MatomoLogger {
            min_console_level: config.console_level,
            min_matomo_level: config.matomo_level,
        };
        let leaked: &'static MatomoLogger = Box::leak(Box::new(logger));
        log::set_logger(leaked)?;
        log::set_max_level(config.console_level.max(config.matomo_level));
        Ok(())
    }

    pub fn track_page_view_impl(title: &str, url: &str) {
        // Inline implementation to avoid recursion with the public facade
        // setCustomUrl
        let a = js_sys::Array::new();
        a.push(&JsValue::from_str("setCustomUrl"));
        a.push(&JsValue::from_str(url));
        if super::has_paq() {
            super::push_to_paq(&a.into());
        }

        // setDocumentTitle
        let a = js_sys::Array::new();
        a.push(&JsValue::from_str("setDocumentTitle"));
        a.push(&JsValue::from_str(title));
        if super::has_paq() {
            super::push_to_paq(&a.into());
        }

        // trackPageView
        let a = js_sys::Array::new();
        a.push(&JsValue::from_str("trackPageView"));
        if super::has_paq() {
            super::push_to_paq(&a.into());
        }
    }
    pub fn set_user_id_impl(user_id: &str) {
        // Inline implementation to avoid recursion with the public facade
        let a = js_sys::Array::new();
        a.push(&JsValue::from_str("setUserId"));
        a.push(&JsValue::from_str(user_id));
        if super::has_paq() {
            super::push_to_paq(&a.into());
        }
    }
}

// Native/no-op implementation (for clippy/fmt host builds)
#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;
    pub fn init(_config: MatomoConfig) -> Result<(), log::SetLoggerError> {
        log::set_max_level(LevelFilter::Info);
        Ok(())
    }
    pub fn track_page_view_impl(_title: &str, _url: &str) {}
    pub fn set_user_id_impl(_user_id: &str) {}
}
