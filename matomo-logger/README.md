# matomo-logger

Global logger for WASM apps that forwards Rust `log` records to both the browser console and Matomo (`_paq`). Provides a one-line initialization and an SPA-friendly `track_page_view` helper. Optional worker bridge API lets you forward logs from Web Workers to the main thread, where they are pushed into Matomo.

## Features

- Single global logger (implements `log::Log`)
- Console + Matomo multiplexing with independent level thresholds
- Safe Matomo snippet injection when `_paq` is absent (configurable)
- SPA navigation helper: `track_page_view(title, url)`
- Web Worker bridge (optional feature) to forward logs to main thread
- Small, dependency-light; optimized for Yew and other WASM front-ends

## Quick start

```rust
use matomo_logger::{MatomoConfig, MatomoLogger};

// Call as early as possible in your `main()`
let _ = MatomoLogger::init(MatomoConfig {
    base_url: Some("https://matomo.example.com/".into()),
    site_id: Some(1),
    console_level: log::LevelFilter::Info,
    matomo_level: log::LevelFilter::Warn, // keep Matomo volume sane
    ..Default::default()
});

// Later, on SPA route change:
matomo_logger::track_page_view("/home", "/home");

// From anywhere in your code:
log::warn!("Network jitter high: {} ms", 120);
```

The logger writes to the browser console at `console_level` and forwards records to Matomo at `matomo_level`. If Matomo is blocked or not available, logging continues to console without errors.

## Configuration

```rust
#[derive(Clone, Debug)]
pub struct MatomoConfig {
    pub base_url: Option<String>, // e.g. https://matomo.example.com/
    pub site_id: Option<u32>,
    pub console_level: log::LevelFilter, // default: Info
    pub matomo_level: log::LevelFilter,   // default: Warn
    pub inject_snippet: bool,             // default: true
    pub queue_capacity: usize,            // reserved for future queueing
    pub max_event_len: usize,             // truncate long messages (default 300)
}
```

- `inject_snippet`: If true and `_paq` is missing, the crate injects the Matomo snippet (`setTrackerUrl`, `setSiteId`, `enableLinkTracking`, and `matomo.js`). If your HTML already includes the Matomo snippet, you can keep it; the crate detects `_paq` and won’t inject again.
- `matomo_level`: strongly recommend `Warn` or higher to avoid analytics noise. Use `Info` only for short experiments.

## SPA page views

Use the built-in helper:

```rust
matomo_logger::track_page_view("/meeting/123", "/meeting/123");
```

It sends `setCustomUrl`, `setDocumentTitle`, and `trackPageView` via `_paq` if available. Calls are safe even when Matomo is blocked.

## Web Worker logs (optional)

Workers do not have access to `window._paq`. Use the bridge API to forward worker logs to the main thread:

```rust
// In worker (requires feature = "worker")
use matomo_logger::worker;
let send = /* a js_sys::Function that posts messages to main thread */;
worker::init_with_bridge(log::LevelFilter::Info, log::LevelFilter::Warn, send)?;

// In the main thread, handle worker messages and push to Matomo
// (Implement a tiny router that converts {type:"log", level, target, message} to
// _paq.push(['trackEvent', 'RustLog', level, `${target} — ${message}`, value]))
```

## Best practices

- Keep `matomo_level` at `Warn` or above in production.
- Consider removing the Matomo snippet from HTML to avoid double page views if you rely on `MatomoLogger::init`.
- Be mindful of PII: avoid logging sensitive data; messages are truncated to `max_event_len`.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 (`LICENSE-APACHE` or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license (`LICENSE-MIT` or <http://opensource.org/licenses/MIT>)

at your option.

## Authors

- Dario Lencina <dario@securityunion.dev>

## Changelog

See the repository’s main CHANGELOG for release notes.
