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
use std::borrow::Cow;
use std::ops::Deref;

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
    /// Text metric value.
    ///
    /// Holds a [`Cow<'static, str>`] so static string literals (e.g. the
    /// `"webtransport"` / `"websocket"` / `"unknown"` `peer_transport` labels
    /// emitted at ~2 Hz per peer in `broadcast_peer_status`) can be stored as
    /// [`Cow::Borrowed`] with **zero heap allocation** per emit, while dynamic
    /// or computed strings stay [`Cow::Owned`].
    ///
    /// # Wire format
    ///
    /// Serde treats `Cow<'static, str>` transparently: it serializes byte-for-byte
    /// identically to a `String` and deserializes into [`Cow::Owned`]. The wire
    /// format is therefore unchanged by the switch from `String` to `Cow`.
    Text(Cow<'static, str>),
}

impl MetricValue {
    /// Construct a zero-allocation [`MetricValue::Text`] from a `&'static str`.
    ///
    /// Use this for static string literals and `&'static str` values (such as
    /// the `peer_transport` label) so the value is stored as [`Cow::Borrowed`]
    /// and no heap allocation occurs on the hot emit path. For dynamic strings,
    /// use [`MetricValue::from`] (via `String`) which yields [`Cow::Owned`].
    #[inline]
    pub const fn text_static(value: &'static str) -> Self {
        MetricValue::Text(Cow::Borrowed(value))
    }
}

// === Simple global broadcast bus ===

use async_broadcast::{broadcast, Receiver, Sender};

static SENDER: Lazy<Sender<DiagEvent>> = Lazy::new(|| {
    let (mut s, r) = broadcast(1024);

    // When the buffer is full, drop the oldest message instead of rejecting
    // the newest. For real-time state (peer mute/video status), the latest
    // event is always more relevant than stale ones.
    s.set_overflow(true);

    // Create a background task that keeps a receiver active
    // This prevents the channel from closing when there are no active receivers
    #[cfg(target_arch = "wasm32")]
    {
        let mut receiver = r;
        wasm_bindgen_futures::spawn_local(async move {
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

/// Obtain a sender that can publish diagnostics events.
pub fn global_sender() -> Sender<DiagEvent> {
    SENDER.deref().clone()
}

/// Subscribe to the diagnostics stream. Each subscriber receives **all** future events.
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
// NOTE: `From<&str>` stays ALLOCATING on purpose.
//
// A `&str` is not necessarily `'static`, so it cannot be borrowed into a
// `Cow<'static, str>` without copying — we must own it. There is also no way to
// distinguish `&'static str` from a borrowed-with-shorter-lifetime `&str` at the
// trait level (they are the same `From<&str>` impl), so this impl must be safe
// for the general (non-static) case and therefore allocates.
//
// Call sites that want the zero-allocation `Cow::Borrowed` path for a static
// literal or `&'static str` must construct it explicitly via
// [`MetricValue::text_static`] (or `MetricValue::Text(Cow::Borrowed(..))`),
// NOT via this `From<&str>` impl.
impl From<&str> for MetricValue {
    fn from(v: &str) -> Self {
        MetricValue::Text(Cow::Owned(v.to_string()))
    }
}
impl From<String> for MetricValue {
    fn from(v: String) -> Self {
        MetricValue::Text(Cow::Owned(v))
    }
}
impl From<Cow<'static, str>> for MetricValue {
    fn from(v: Cow<'static, str>) -> Self {
        MetricValue::Text(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire format of `MetricValue::Text` MUST stay byte-identical to what
    /// it was when the variant held a plain `String`. `serde` treats
    /// `Cow<'static, str>` transparently, so both a `Cow::Borrowed` and a
    /// `Cow::Owned` value must serialize to the exact same JSON bytes that a
    /// `String` of the same contents would have produced.
    ///
    /// This is the load-bearing safety check for issue #1421: the diagnostics
    /// pipeline consumes this wire format, so any drift here is a regression.
    #[test]
    fn text_metric_wire_format_is_byte_identical() {
        // The historical (pre-#1421) `String`-backed encoding for this content.
        // With `#[serde(tag = "t", content = "v")]` a `Text("webtransport")`
        // value encodes as this exact byte sequence.
        let expected = r#"{"t":"Text","v":"webtransport"}"#;

        // Zero-alloc static-literal path (Cow::Borrowed).
        let borrowed = MetricValue::Text(Cow::Borrowed("webtransport"));
        let borrowed_json = serde_json::to_string(&borrowed).unwrap();
        assert_eq!(
            borrowed_json, expected,
            "Cow::Borrowed Text must serialize byte-identically to the old String form"
        );

        // Dynamic/owned path (Cow::Owned) with identical contents.
        let owned = MetricValue::Text(Cow::Owned("webtransport".to_string()));
        let owned_json = serde_json::to_string(&owned).unwrap();
        assert_eq!(
            owned_json, expected,
            "Cow::Owned Text must serialize byte-identically to the old String form"
        );

        // Borrowed and Owned must be indistinguishable on the wire.
        assert_eq!(
            borrowed_json, owned_json,
            "Borrowed and Owned Text variants must produce identical wire bytes"
        );
    }

    /// Deserialization must reproduce the value and yield an owned `Cow`
    /// (serde has no borrowed source to lend from when decoding into a
    /// `Cow<'static, str>`), and the round-trip must re-serialize identically.
    #[test]
    fn text_metric_round_trips() {
        for original in [
            MetricValue::Text(Cow::Borrowed("websocket")),
            MetricValue::Text(Cow::Owned("peer-abc-123".to_string())),
        ] {
            let json = serde_json::to_string(&original).unwrap();
            let decoded: MetricValue = serde_json::from_str(&json).unwrap();

            match (&original, &decoded) {
                (MetricValue::Text(orig_s), MetricValue::Text(dec_s)) => {
                    assert_eq!(orig_s, dec_s, "decoded text must equal original");
                    // Deserializing a `Cow<'static, str>` always produces Owned.
                    assert!(
                        matches!(dec_s, Cow::Owned(_)),
                        "deserialized Text must be Cow::Owned"
                    );
                }
                _ => panic!("expected Text variant after round-trip"),
            }

            // Re-serialization must match the first serialization exactly.
            assert_eq!(
                serde_json::to_string(&decoded).unwrap(),
                json,
                "re-serialized value must be byte-identical to the original encoding"
            );
        }
    }

    /// A static literal routed through [`MetricValue::text_static`] must be a
    /// `Cow::Borrowed` — i.e. statically zero-allocation. This pins the
    /// headline acceptance criterion: the `peer_transport` label path never
    /// heap-allocates per emit.
    #[test]
    fn text_static_is_zero_alloc_borrowed() {
        let v = MetricValue::text_static("webtransport");
        match v {
            MetricValue::Text(Cow::Borrowed(s)) => assert_eq!(s, "webtransport"),
            MetricValue::Text(Cow::Owned(_)) => {
                panic!("text_static must yield Cow::Borrowed (zero-alloc), not Cow::Owned")
            }
            other => panic!("expected Text variant, got {other:?}"),
        }
    }

    /// The other wire-format variants must also remain stable.
    #[test]
    fn numeric_metric_wire_format_is_stable() {
        assert_eq!(
            serde_json::to_string(&MetricValue::I64(-7)).unwrap(),
            r#"{"t":"I64","v":-7}"#
        );
        assert_eq!(
            serde_json::to_string(&MetricValue::U64(42)).unwrap(),
            r#"{"t":"U64","v":42}"#
        );
        assert_eq!(
            serde_json::to_string(&MetricValue::F64(1.5)).unwrap(),
            r#"{"t":"F64","v":1.5}"#
        );
    }

    /// `From<&str>` allocates (general non-static case); `MetricValue::text_static`
    /// borrows. Confirm the documented contract holds.
    #[test]
    fn from_str_owns_text_static_borrows() {
        // Non-static path: must be Owned.
        let dynamic = String::from("peer-xyz");
        let from_dynamic = MetricValue::from(dynamic.as_str());
        assert!(
            matches!(from_dynamic, MetricValue::Text(Cow::Owned(_))),
            "From<&str> must allocate (Cow::Owned) for the general non-static case"
        );

        // Static path: must be Borrowed.
        assert!(
            matches!(
                MetricValue::text_static("websocket"),
                MetricValue::Text(Cow::Borrowed(_))
            ),
            "text_static must borrow (Cow::Borrowed) for a &'static str"
        );
    }
}
