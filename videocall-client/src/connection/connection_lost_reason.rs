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

/// Distinguishes _why_ a connection was lost so that logs, health packets,
/// and future diagnostics dashboards can differentiate handshake failures
/// (server unreachable, TLS mismatch, QUIC version negotiation, auth
/// rejection) from mid-session drops (network path change, idle timeout,
/// server restart).
#[derive(Debug, Clone)]
pub enum ConnectionLostReason {
    /// WebTransport `ready()` or WebSocket `open` event never fired —
    /// connection lost during the handshake phase.
    HandshakeFailed(String),
    /// Connection was fully established (`ready`/`open` fired) before it
    /// was lost.
    SessionDropped(String),
}

impl ConnectionLostReason {
    /// Human-readable message carried by this reason.
    pub fn message(&self) -> &str {
        match self {
            Self::HandshakeFailed(m) | Self::SessionDropped(m) => m,
        }
    }

    /// Short, Prometheus-friendly label for this reason category.
    pub fn label(&self) -> &'static str {
        match self {
            Self::HandshakeFailed(_) => "handshake_failed",
            Self::SessionDropped(_) => "session_dropped",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_failed_label_and_message() {
        let r = ConnectionLostReason::HandshakeFailed("tls error".to_string());
        assert_eq!(r.label(), "handshake_failed");
        assert_eq!(r.message(), "tls error");
    }

    #[test]
    fn session_dropped_label_and_message() {
        let r = ConnectionLostReason::SessionDropped("idle timeout".to_string());
        assert_eq!(r.label(), "session_dropped");
        assert_eq!(r.message(), "idle timeout");
    }

    #[test]
    fn clone_preserves_variant() {
        let r = ConnectionLostReason::HandshakeFailed("x".to_string());
        let c = r.clone();
        assert_eq!(c.label(), "handshake_failed");
        assert_eq!(c.message(), "x");
    }
}
