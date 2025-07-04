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

//! Lightweight diagnostics event bus shared across the code-base.
//! Works on both native and `wasm32` targets (no Tokio required).

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

// === Diagnostic data structures ===

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagEvent {
    /// Sub-system that produced this event (e.g. "neteq", "codec", "transport").
    pub subsystem: &'static str,
    /// Optional stream identifier (peer or media stream).
    pub stream_id: Option<String>,
    /// Unix time in milliseconds when the metric was captured.
    pub ts_ms: u64,
    /// Arbitrary key/value metrics.
    pub metrics: Vec<Metric>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metric {
    pub name: &'static str,
    pub value: MetricValue,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum MetricValue {
    I64(i64),
    U64(u64),
    F64(f64),
    Text(String),
}

// === Simple global broadcast bus (flume multi-producer multi-consumer) ===

use flume::{Receiver, Sender};

static BUS: Lazy<(Sender<DiagEvent>, Receiver<DiagEvent>)> = Lazy::new(|| flume::unbounded());

/// Obtain a sender that can publish diagnostics events.
pub fn global_sender() -> &'static Sender<DiagEvent> {
    &BUS.0
}

/// Subscribe to the diagnostics stream. Each subscriber receives **all** future events.
pub fn subscribe() -> Receiver<DiagEvent> {
    BUS.1.clone()
}

// === Helper utilities ===

/// Current wall-clock time in milliseconds.
#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

// === metric! helper macro ===

/// Shorthand for constructing a [`Metric`].
#[macro_export]
macro_rules! metric {
    ($name:expr, $value:expr) => {
        $crate::Metric {
            name: $name,
            value: $crate::MetricValue::from($value),
        }
    };
}

// Implement `From` conversions so `metric!("fps", 30)` works for common types.
impl From<i64> for MetricValue {
    fn from(v: i64) -> Self {
        MetricValue::I64(v)
    }
}
impl From<u64> for MetricValue {
    fn from(v: u64) -> Self {
        MetricValue::U64(v)
    }
}
impl From<f64> for MetricValue {
    fn from(v: f64) -> Self {
        MetricValue::F64(v)
    }
}
impl From<&str> for MetricValue {
    fn from(v: &str) -> Self {
        MetricValue::Text(v.to_string())
    }
}
impl From<String> for MetricValue {
    fn from(v: String) -> Self {
        MetricValue::Text(v)
    }
}
