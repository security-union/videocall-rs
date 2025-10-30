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
use std::ops::Deref;

#[cfg(all(target_arch = "wasm32", feature = "diagnostics"))]
use wasm_bindgen_futures::spawn_local;

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

// === Simple global broadcast bus ===

use async_broadcast::{broadcast, Receiver, Sender};

#[cfg(feature = "diagnostics")]
static SENDER: Lazy<Sender<DiagEvent>> = Lazy::new(|| {
    let (s, r) = broadcast(256); // Capacity of 256 messages.

    // Create a background task that keeps a receiver active
    // This prevents the channel from closing when there are no active receivers
    #[cfg(target_arch = "wasm32")]
    {
        let mut receiver = r;
        spawn_local(async move {
            // Keep the receiver alive to prevent channel closure
            // This receiver will consume messages but not process them
            while (receiver.recv().await).is_ok() {
                // Intentionally discard messages in the background receiver
                // This keeps the channel open for other receivers
            }
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        // For native targets, we could use tokio::spawn here if needed
        // For now, just drop the receiver and let the channel close if no receivers
        std::mem::drop(r);
    }

    s
});

#[cfg(not(feature = "diagnostics"))]
static SENDER: Lazy<Sender<DiagEvent>> = Lazy::new(|| {
    // Create a dummy channel that will be dropped immediately
    // The sender will never successfully send
    let (s, _r) = broadcast(1);
    s
});

/// Obtain a sender that can publish diagnostics events.
///
/// When the "diagnostics" feature is disabled, this returns a sender that will fail to send (low overhead).
pub fn global_sender() -> Sender<DiagEvent> {
    SENDER.deref().clone()
}

/// Subscribe to the diagnostics stream. Each subscriber receives **all** future events.
///
/// When the "diagnostics" feature is disabled, this returns a receiver that will never receive events.
pub fn subscribe() -> Receiver<DiagEvent> {
    SENDER.deref().new_receiver()
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
///
/// When the "diagnostics" feature is disabled, this becomes a no-op for zero overhead.
#[cfg(feature = "diagnostics")]
#[macro_export]
macro_rules! metric {
    ($name:expr, $value:expr) => {
        $crate::Metric {
            name: $name,
            value: $crate::MetricValue::from($value),
        }
    };
}

/// No-op version of metric! when diagnostics are disabled
#[cfg(not(feature = "diagnostics"))]
#[macro_export]
macro_rules! metric {
    ($name:expr, $value:expr) => {
        // Evaluates to nothing - completely removed by compiler
        ()
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
