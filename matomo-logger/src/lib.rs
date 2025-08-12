use log::LevelFilter;

#[cfg(target_arch = "wasm32")]
use log::{Level, LevelFilter, Log, Metadata, Record};

#[derive(Clone, Debug)]
pub struct MatomoConfig {
    pub base_url: Option<String>, // e.g. https://matomo.videocall.rs/
    pub site_id: Option<u32>,
    pub console_level: LevelFilter,
    pub matomo_level: LevelFilter,
    pub inject_snippet: bool,
    pub queue_capacity: usize,
    pub max_event_len: usize,
}

impl Default for MatomoConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            site_id: None,
            console_level: LevelFilter::Info,
            matomo_level: LevelFilter::Warn,
            inject_snippet: true,
            queue_capacity: 256,
            max_event_len: 300,
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
                if let Ok(arr) = args.clone().dyn_into::<js_sys::Array>() {
                    let cmd = arr.get(0).as_string().unwrap_or_else(|| "<unknown>".into());
                    debug_log(&format!("_paq.push({})", cmd));
                }
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
            debug_log("inject_snippet: appended matomo.js script tag");
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn debug_log(msg: &str) {
    web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
        "[matomo-logger] {}",
        msg
    )));
}
#[cfg(not(target_arch = "wasm32"))]
fn debug_log(_msg: &str) {}

// Worker bridge API (simple): serialize log event and let host push it to Matomo.
#[cfg(feature = "worker")]
pub mod worker {
    use super::*;
    use wasm_bindgen::JsCast;

    pub fn init_with_bridge(
        console_level: LevelFilter,
        matomo_level: LevelFilter,
        send: js_sys::Function,
    ) -> Result<(), log::SetLoggerError> {
        let logger = BridgeLogger {
            console_level,
            matomo_level,
            sender: send,
        };
        log::set_boxed_logger(Box::new(logger))?;
        log::set_max_level(console_level.max(matomo_level));
        Ok(())
    }

    struct BridgeLogger {
        console_level: LevelFilter,
        matomo_level: LevelFilter,
        sender: js_sys::Function,
    }
    impl Log for BridgeLogger {
        fn enabled(&self, _metadata: &Metadata) -> bool {
            true
        }
        fn log(&self, record: &Record) {
            log_to_console(self.console_level, record);
            if record.level().to_level_filter() > self.matomo_level {
                return;
            }
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
            let _ = self.sender.call1(&JsValue::NULL, &obj);
        }
        fn flush(&self) {}
    }
}

// ---------------- Platform-specific implementations -----------------

// WASM implementation
#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use wasm_bindgen::prelude::*;
    use web_sys::{console, window};

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
        super::debug_log(&format!(
            "init: inject_snippet={}, base_url={:?}, site_id={:?}, console_level={:?}, matomo_level={:?}, has_paq_pre={}",
            config.inject_snippet, config.base_url, config.site_id, config.console_level, config.matomo_level, super::has_paq()
        ));
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
        super::debug_log(&format!(
            "init: logger installed, has_paq_post={}",
            super::has_paq()
        ));
        Ok(())
    }

    pub fn track_page_view_impl(title: &str, url: &str) {
        super::track_page_view(title, url);
    }
    pub fn set_user_id_impl(user_id: &str) {
        super::set_user_id(user_id);
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
