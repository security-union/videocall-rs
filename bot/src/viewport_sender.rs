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
 */

//! Viewport / "desired streams" emitter for the synthetic bot (HCL issue #988).
//!
//! A real browser only decodes the peers whose tiles are on-screen. It tells
//! the relay which peers those are via a `VIEWPORT` control packet (a list of
//! source `session_id`s); the relay then stops forwarding VIDEO from the
//! off-screen peers, cutting downstream fan-out. Load-test bots historically
//! rendered *everyone*, so they measured an optimistic relay that never got to
//! apply viewport filtering — masking the very bandwidth saving #988 exists to
//! deliver.
//!
//! This module makes the bot emit a VIEWPORT just like a browser. It is driven
//! by inbound media: every time the bot observes a packet from a source
//! `session_id` (read from the cleartext `PacketWrapper.session_id` the relay
//! stamps on forwarded media), it updates its known-peer set and recomputes the
//! visible subset. When the visible subset *changes* it sends a fresh VIEWPORT.
//!
//! The visible subset is the first `visible_count` source session_ids in
//! ascending numeric order. Sorting makes the choice deterministic so two runs
//! of the same scenario produce identical viewport sets and identical relay
//! filtering decisions — essential for reproducible load tests.
//!
//! Wire format matches the browser client at
//! `videocall-client/src/client/video_call_client.rs::send_viewport_via`:
//!
//! ```text
//! ViewportPacket { session_ids: <visible source session_ids> }
//! PacketWrapper {
//!     packet_type: VIEWPORT,
//!     user_id: self_user_id.as_bytes(), // who is rendering
//!     data: <serialized ViewportPacket>,
//!     ..
//! }
//! ```

use protobuf::Message;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use tracing::{info, warn};

use crate::transport::{MediaTypeLabel, OutboundFrame};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::viewport_packet::ViewportPacket;

/// Tracks the bot's known peers and emits a `VIEWPORT` packet whenever the
/// visible subset changes.
pub struct ViewportSender {
    /// Our own user id (the renderer), stamped into the VIEWPORT wrapper.
    self_user_id: String,
    /// How many peers this bot "renders". `None` disables VIEWPORT entirely
    /// (legacy behaviour — relay fails open and forwards every stream).
    visible_count: Option<usize>,
    /// Every distinct source session_id seen so far, kept sorted ascending so
    /// the visible subset is deterministic across runs.
    known_sources: BTreeSet<u64>,
    /// The visible subset most recently sent to the relay. Used to suppress
    /// duplicate VIEWPORT packets when the visible set is unchanged.
    last_sent: Vec<u64>,
    /// Visible subset that should be sent once the outbound channel accepts it.
    /// Set before an emit attempt and cleared only after the packet is queued,
    /// so a transient full channel cannot permanently lose the first viewport.
    pending: Option<Vec<u64>>,
    /// Whether we have emitted at least one VIEWPORT this connection.
    has_sent: bool,
    /// Channel to send outbound packets.
    packet_tx: Sender<OutboundFrame>,
    /// Counter for total VIEWPORT packets sent.
    pub viewports_sent: Arc<AtomicU64>,
    /// When the last *reconnect re-assert* (via [`Self::resend_on_reconnect`])
    /// went out. Rate-limits the re-assert so a frequent reconnect/diagnostic
    /// hook cannot spam identical VIEWPORTs. `None` until the first re-assert.
    /// The change-driven [`Self::on_source_seen`] path is NOT gated by this —
    /// it only fires when the visible set genuinely changes.
    last_resend: Option<Instant>,
}

/// Minimum spacing between two reconnect re-asserts. The bot has no
/// connection-state event (unlike the browser client), so the re-assert is
/// driven off a periodic hook; this bound keeps an unconditional re-send from
/// exceeding roughly one packet per interval even if that hook fires often.
const MIN_RESEND_INTERVAL: Duration = Duration::from_secs(5);

impl ViewportSender {
    /// Create a new viewport sender. `visible_count == None` disables emission.
    pub fn new(
        self_user_id: String,
        visible_count: Option<usize>,
        packet_tx: Sender<OutboundFrame>,
    ) -> Self {
        Self {
            self_user_id,
            visible_count,
            known_sources: BTreeSet::new(),
            last_sent: Vec::new(),
            pending: None,
            has_sent: false,
            packet_tx,
            viewports_sent: Arc::new(AtomicU64::new(0)),
            last_resend: None,
        }
    }

    /// Returns the shared atomic counter for VIEWPORT packets sent.
    pub fn viewports_sent_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.viewports_sent)
    }

    /// Whether this sender will ever emit a VIEWPORT (i.e. it was configured
    /// with a `visible_count`). Used to gate logging at construction time.
    pub fn is_enabled(&self) -> bool {
        self.visible_count.is_some()
    }

    /// Record a source `session_id` observed on an inbound media packet. When a
    /// newly observed source changes the visible subset, a fresh VIEWPORT is
    /// emitted. A `session_id` of 0 is the proto-3 default / unstamped sentinel
    /// and is ignored — only relay-stamped sources participate in the viewport.
    pub fn on_source_seen(&mut self, session_id: u64) {
        // Legacy mode: never emit. Skip the unstamped sentinel.
        if self.visible_count.is_none() || session_id == 0 {
            return;
        }

        // First time we see this source — adding it may change the visible set.
        if !self.known_sources.insert(session_id) {
            return;
        }

        let visible = self.compute_visible();

        // Only emit when the visible subset genuinely changed (a new peer past
        // the visible_count cutoff does not move the set), and never emit an
        // empty viewport — the relay treats empty as "no signal" (fail-open),
        // so an empty packet would be a no-op at best.
        if visible.is_empty() || (self.has_sent && visible == self.last_sent) {
            return;
        }

        self.pending = Some(visible);
        self.flush_pending("change");
    }

    /// Re-assert the CURRENT viewport after a reconnect / re-election.
    ///
    /// On disconnect the relay drops this bot's viewport subscription and a
    /// reconnect allocates a fresh empty viewport (fail-open → the bot starts
    /// receiving ALL video again, silently under-filtered). The browser client
    /// re-sends its viewport on the `Connected` state edge for exactly this
    /// reason (`video_call_client.rs::reset_for_reconnect`); the bot mirrors
    /// that intent here.
    ///
    /// This is a no-op when:
    ///   - legacy mode (`visible_count == None`) — the bot never filters,
    ///   - nothing has ever been sent and no failed send is pending — there is no
    ///     prior viewport to restore, so a first-connect caller never double-sends,
    ///     and
    ///   - the current visible subset is empty — an empty VIEWPORT is a relay
    ///     no-op (fail-open).
    ///
    /// Unlike the change-driven [`Self::on_source_seen`], this re-sends the
    /// current set *unconditionally* (the relay's copy is stale even though the
    /// local set is unchanged), and it also retries a pending send that previously
    /// failed because the outbound channel was full. Both paths are rate-limited
    /// by [`MIN_RESEND_INTERVAL`] so a periodic reconnect hook cannot spam
    /// identical packets. The `known_sources` set is deliberately preserved
    /// across the reconnect, so the re-send reflects exactly what the bot was
    /// rendering before the drop.
    pub fn resend_on_reconnect(&mut self) {
        // Legacy mode → nothing to restore.
        if self.visible_count.is_none() {
            return;
        }

        // Rate-limit: skip if a re-assert went out within the last interval.
        if let Some(last) = self.last_resend {
            if last.elapsed() < MIN_RESEND_INTERVAL {
                return;
            }
        }

        // A previous change-driven send may have failed because the outbound
        // channel was full. Retry that exact pending set here even if no
        // viewport has ever made it onto the wire yet.
        if self.pending.is_some() {
            if self.flush_pending("retry") {
                self.last_resend = Some(Instant::now());
            }
            return;
        }

        // No viewport ever established and no failed pending send → nothing to
        // restore, so a first-connect caller never double-sends.
        if !self.has_sent {
            return;
        }

        let visible = self.compute_visible();
        if visible.is_empty() {
            return;
        }

        self.pending = Some(visible);
        if self.flush_pending("reconnect") {
            self.last_resend = Some(Instant::now());
        }
    }

    /// Try to queue the current pending viewport. The pending value is cleared
    /// only after a successful send, so transient channel backpressure is
    /// retried by the periodic reset/reconnect hook.
    fn flush_pending(&mut self, reason: &str) -> bool {
        let Some(visible) = self.pending.clone() else {
            return false;
        };
        if self.emit_viewport(visible, reason) {
            self.pending = None;
            true
        } else {
            false
        }
    }

    /// Build and send a VIEWPORT for `visible`, updating `last_sent` / counters
    /// on success. `reason` only labels the log line ("change" vs "reconnect").
    /// Returns `true` if the packet was queued, `false` on build/channel error.
    fn emit_viewport(&mut self, visible: Vec<u64>, reason: &str) -> bool {
        match build_viewport(&self.self_user_id, &visible) {
            Ok(bytes) => {
                let frame = OutboundFrame::new(MediaTypeLabel::Other, bytes);
                if self.packet_tx.try_send(frame).is_err() {
                    warn!(
                        "[{}] Failed to send VIEWPORT ({} sessions, channel full, will retry on reset/source change)",
                        self.self_user_id,
                        visible.len()
                    );
                    false
                } else {
                    self.last_sent = visible.clone();
                    self.has_sent = true;
                    self.viewports_sent.fetch_add(1, Ordering::Relaxed);
                    info!(
                        "[{}] Sent VIEWPORT ({}) rendering {} of {} known peer(s): {:?}",
                        self.self_user_id,
                        reason,
                        visible.len(),
                        self.known_sources.len(),
                        visible
                    );
                    true
                }
            }
            Err(e) => {
                warn!(
                    "[{}] Failed to build VIEWPORT packet: {}",
                    self.self_user_id, e
                );
                false
            }
        }
    }

    /// The first `visible_count` known source session_ids in ascending order.
    fn compute_visible(&self) -> Vec<u64> {
        let n = self.visible_count.unwrap_or(0);
        self.known_sources.iter().take(n).copied().collect()
    }
}

/// Build a serialized `VIEWPORT` `PacketWrapper` for `session_ids`.
fn build_viewport(self_user_id: &str, session_ids: &[u64]) -> anyhow::Result<Vec<u8>> {
    let viewport = ViewportPacket {
        session_ids: session_ids.to_vec(),
        ..Default::default()
    };
    let wrapper = PacketWrapper {
        packet_type: PacketType::VIEWPORT.into(),
        user_id: self_user_id.as_bytes().to_vec(),
        data: viewport.write_to_bytes()?,
        ..Default::default()
    };
    Ok(wrapper.write_to_bytes()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn parse_sent(bytes: &[u8]) -> (Vec<u8>, Vec<u64>) {
        let wrapper = PacketWrapper::parse_from_bytes(bytes).expect("parse wrapper");
        assert_eq!(wrapper.packet_type.enum_value(), Ok(PacketType::VIEWPORT));
        let vp = ViewportPacket::parse_from_bytes(&wrapper.data).expect("parse viewport");
        (wrapper.user_id, vp.session_ids)
    }

    #[tokio::test]
    async fn legacy_mode_never_sends() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), None, tx);
        assert!(!vs.is_enabled());
        for sid in 1..=5 {
            vs.on_source_seen(sid);
        }
        assert!(rx.try_recv().is_err(), "legacy mode must not emit VIEWPORT");
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn zero_session_id_ignored() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);
        vs.on_source_seen(0);
        assert!(rx.try_recv().is_err(), "unstamped sentinel must be ignored");
        assert!(vs.known_sources.is_empty());
    }

    #[tokio::test]
    async fn picks_first_n_sorted_deterministically() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        // Observe out of order: 30, 10, 20. Visible set should be {10, 20}.
        vs.on_source_seen(30); // visible = [30]
        let (uid, ids) = parse_sent(&rx.try_recv().expect("first emit").bytes);
        assert_eq!(uid, b"bot-1");
        assert_eq!(ids, vec![30]);

        vs.on_source_seen(10); // visible = [10, 30] (changed)
        let (_, ids) = parse_sent(&rx.try_recv().expect("second emit").bytes);
        assert_eq!(ids, vec![10, 30]);

        vs.on_source_seen(20); // visible = [10, 20] (changed, 30 dropped past N)
        let (_, ids) = parse_sent(&rx.try_recv().expect("third emit").bytes);
        assert_eq!(ids, vec![10, 20]);

        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn no_emit_when_visible_unchanged() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.on_source_seen(1); // [1]
        assert!(rx.try_recv().is_ok());
        vs.on_source_seen(2); // [1, 2]
        assert!(rx.try_recv().is_ok());

        // A new peer (3) past the visible_count cutoff must NOT move the set.
        vs.on_source_seen(3); // visible still [1, 2]
        assert!(
            rx.try_recv().is_err(),
            "peer past cutoff must not re-emit VIEWPORT"
        );
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn duplicate_source_does_not_re_emit() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(3), tx);

        vs.on_source_seen(5);
        assert!(rx.try_recv().is_ok());
        // Seeing the same source again is a no-op.
        vs.on_source_seen(5);
        assert!(rx.try_recv().is_err());
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn reconnect_resends_current_viewport() {
        // After a viewport is established, a reconnect must re-assert the SAME
        // current visible subset unconditionally (the relay forgot it on the
        // drop), even though the local set did not change. This is the #988
        // fidelity fix: without it the bot silently receives all video again.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.on_source_seen(10); // [10]
        vs.on_source_seen(20); // [10, 20]
        let (_, ids) = parse_sent(&rx.try_recv().expect("emit 1").bytes);
        assert_eq!(ids, vec![10]);
        let (_, ids) = parse_sent(&rx.try_recv().expect("emit 2").bytes);
        assert_eq!(ids, vec![10, 20]);
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);

        // Reconnect: known_sources is preserved (mirrors InboundStats::reset
        // take/restore), so the re-assert re-sends the current [10, 20] subset.
        vs.resend_on_reconnect();
        let (uid, ids) = parse_sent(&rx.try_recv().expect("reconnect re-send").bytes);
        assert_eq!(uid, b"bot-1");
        assert_eq!(ids, vec![10, 20]);
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn reconnect_no_resend_when_never_sent() {
        // A bot that has just connected and never rendered anyone must NOT emit
        // on a reconnect hook — this is the first-connect double-send guard.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "no viewport established yet → reconnect must be a no-op"
        );
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn failed_initial_send_is_retried_on_reconnect_hook() {
        // If the first VIEWPORT enqueue fails, the source is already known. The
        // retry must not depend on seeing a new source or on has_sent=true.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(1);
        tx.try_send(OutboundFrame::new(MediaTypeLabel::Other, vec![0]))
            .expect("prefill channel");
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(1), tx);

        vs.on_source_seen(7);
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 0);
        assert!(!vs.has_sent);
        assert_eq!(vs.pending, Some(vec![7]));

        let _ = rx.try_recv().expect("remove prefilled frame");
        vs.resend_on_reconnect();

        let (uid, ids) = parse_sent(&rx.try_recv().expect("retried viewport").bytes);
        assert_eq!(uid, b"bot-1");
        assert_eq!(ids, vec![7]);
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 1);
        assert!(vs.pending.is_none());
    }

    #[tokio::test]
    async fn reconnect_no_resend_in_legacy_mode() {
        // Legacy mode (visible_count == None) never filters, so a reconnect
        // re-assert must stay silent.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), None, tx);
        for sid in 1..=3 {
            vs.on_source_seen(sid);
        }
        vs.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "legacy mode must not emit on reconnect"
        );
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn reconnect_resend_is_rate_limited() {
        // Two reconnect re-asserts in quick succession (well under
        // MIN_RESEND_INTERVAL) must collapse to a single packet on the wire —
        // a frequent reconnect/diagnostic hook cannot spam identical VIEWPORTs.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.on_source_seen(7); // [7]
        assert!(rx.try_recv().is_ok());
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 1);

        vs.resend_on_reconnect();
        let (_, ids) = parse_sent(&rx.try_recv().expect("first re-assert").bytes);
        assert_eq!(ids, vec![7]);
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);

        // Immediately again: inside the rate-limit window → suppressed.
        vs.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "re-assert within MIN_RESEND_INTERVAL must be suppressed"
        );
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);
    }
}
