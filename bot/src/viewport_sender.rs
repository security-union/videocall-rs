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
    /// Set when off-viewport VIDEO has been observed since the last accepted
    /// send — the observable "the relay forgot my viewport subscription" signal.
    ///
    /// The relay viewport-filters VIDEO only: on a healthy filtered connection a
    /// receiver must NOT receive VIDEO from a source outside its last-sent
    /// viewport set. If it does, the relay's copy of the viewport is gone
    /// (re-election / failover → fail-open, forwarding ALL video), which is
    /// exactly the recovery scenario [`Self::resend_on_reconnect`] exists for.
    /// AUDIO and SCREEN are NEVER viewport-filtered, so an off-viewport AUDIO or
    /// SCREEN packet is expected on a healthy connection and must NOT arm this —
    /// the arming condition is VIDEO-specific. Cleared on any successful send
    /// (change or reconnect), since re-asserting `last_sent` to the relay heals
    /// the fail-open.
    off_viewport_video_seen: bool,
    /// Channel to send outbound packets.
    packet_tx: Sender<OutboundFrame>,
    /// Counter for total VIEWPORT packets sent.
    pub viewports_sent: Arc<AtomicU64>,
    /// When the last *reconnect re-assert* (via [`Self::resend_on_reconnect`])
    /// went out. This is now a SECONDARY burst guard: the primary steady-state
    /// silence comes from the `off_viewport_video_seen` gate (a steady
    /// connection observes no off-viewport video, so it never re-asserts). This
    /// bound only collapses a burst of re-asserts that arm in quick succession
    /// to roughly one packet per interval. `None` until the first re-assert.
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
            off_viewport_video_seen: false,
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

    /// Record a source `session_id` observed on an inbound media packet, along
    /// with whether that packet was VIDEO (`is_video`). When a newly observed
    /// source changes the visible subset, a fresh VIEWPORT is emitted. A
    /// `session_id` of 0 is the proto-3 default / unstamped sentinel and is
    /// ignored — only relay-stamped sources participate in the viewport.
    ///
    /// `is_video` arms the reconnect re-assert: the relay viewport-filters VIDEO
    /// only, so inbound VIDEO from a source NOT in `last_sent` means the relay's
    /// copy of our viewport is gone (fail-open). That observation arms
    /// [`Self::off_viewport_video_seen`] so the next reset-hook re-asserts the
    /// current set. AUDIO / SCREEN are never viewport-filtered, so an
    /// off-viewport packet of those kinds is expected on a healthy connection
    /// and does NOT arm. A successful change emit clears the flag, since it
    /// re-asserts `last_sent` to the relay (healing any fail-open).
    pub fn on_source_seen(&mut self, session_id: u64, is_video: bool) {
        // Legacy mode: never emit. Skip the unstamped sentinel.
        if self.visible_count.is_none() || session_id == 0 {
            return;
        }

        // Arm the reconnect re-assert when off-viewport VIDEO is observed. This
        // must run BEFORE the duplicate-source early-return below so that a
        // KNOWN source arriving as off-`last_sent` video (the relay fail-opened
        // and is forwarding video it should have dropped) still arms. session_id
        // != 0 is already guaranteed by the guard above; visible_count.is_some()
        // is too, but it is kept explicit for clarity/robustness.
        if is_video
            && self.has_sent
            && self.visible_count.is_some()
            && !self.last_sent.contains(&session_id)
        {
            self.off_viewport_video_seen = true;
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
        // `flush_pending` clears `off_viewport_video_seen` on success: a change
        // emit re-asserts `last_sent` to the relay, healing any fail-open.
        self.flush_pending("change");
    }

    /// Re-assert the CURRENT viewport after a reconnect / re-election — but ONLY
    /// when off-viewport VIDEO has been observed since the last accepted send.
    ///
    /// On disconnect the relay drops this bot's viewport subscription and a
    /// reconnect allocates a fresh empty viewport (fail-open → the bot starts
    /// receiving ALL video again, silently under-filtered). The browser client
    /// re-sends its viewport on the `Connected` state edge for exactly this
    /// reason (`video_call_client.rs::reset_for_reconnect`); the bot mirrors
    /// that intent here. The bot has no connection-state event of its own, so
    /// this is driven off the periodic 10s reset hook — which also fires on a
    /// perfectly healthy connection. To avoid re-asserting an UNCHANGED viewport
    /// every window (which blunts `relay_viewport_updates_total{outcome=accepted}`
    /// as the "client re-subscribed after a flap" signal — HCL #1006), the
    /// re-assert is gated on the observable fail-open symptom: inbound VIDEO from
    /// a source outside the last-sent viewport, recorded in
    /// [`Self::off_viewport_video_seen`]. On a steady connection with an
    /// unchanged visible set, the relay forwards no off-viewport video, the flag
    /// is never armed, and this emits NOTHING after the initial send.
    ///
    /// This is a no-op when:
    ///   - legacy mode (`visible_count == None`) — the bot never filters,
    ///   - nothing has ever been sent and no failed send is pending — there is no
    ///     prior viewport to restore, so a first-connect caller never double-sends,
    ///   - no off-viewport VIDEO has been observed (the steady-state silence gate),
    ///     and
    ///   - the current visible subset is empty — an empty VIEWPORT is a relay
    ///     no-op (fail-open).
    ///
    /// Unlike the change-driven [`Self::on_source_seen`], this re-sends the
    /// current set *unconditionally once armed* (the relay's copy is stale even
    /// though the local set is unchanged), and it also retries a pending send
    /// that previously failed because the outbound channel was full — that retry
    /// runs BEFORE the arming gate (a genuine first-send retry must not require
    /// the fail-open symptom). [`MIN_RESEND_INTERVAL`] is now only a SECONDARY
    /// burst guard, collapsing re-asserts that arm in quick succession. The
    /// `known_sources` set is deliberately preserved across the reconnect, so the
    /// re-send reflects exactly what the bot was rendering before the drop. Any
    /// successful send (here or via the retry branch) clears the armed flag in
    /// [`Self::flush_pending`].
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
        // viewport has ever made it onto the wire yet. This must run BEFORE the
        // arming gate below: a genuine first-send retry is needed regardless of
        // whether off-viewport video has been observed.
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

        // Steady-state silence gate (HCL #1006): the unconditional re-assert only
        // proceeds when off-viewport VIDEO has been observed (the relay fail-open
        // symptom). A steady connection never arms this, so it stays silent.
        if !self.off_viewport_video_seen {
            return;
        }

        let visible = self.compute_visible();
        if visible.is_empty() {
            return;
        }

        self.pending = Some(visible);
        // `flush_pending` clears the armed flag on success.
        if self.flush_pending("reconnect") {
            self.last_resend = Some(Instant::now());
        }
    }

    /// Try to queue the current pending viewport. The pending value is cleared
    /// only after a successful send, so transient channel backpressure is
    /// retried by the periodic reset/reconnect hook. On a successful send the
    /// armed [`Self::off_viewport_video_seen`] flag is also cleared: any emit
    /// (change, retry, or reconnect) re-asserts the current set to the relay and
    /// updates `last_sent`, healing the fail-open the flag was tracking. Doing
    /// the clear here (the single send chokepoint) keeps every send path
    /// consistent — a successful retry of a stale-superset set heals the relay
    /// just as a reconnect re-assert does, so it must not leave the flag armed
    /// (which would cause one redundant re-send on the next reset).
    fn flush_pending(&mut self, reason: &str) -> bool {
        let Some(visible) = self.pending.clone() else {
            return false;
        };
        if self.emit_viewport(visible, reason) {
            self.pending = None;
            self.off_viewport_video_seen = false;
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
            vs.on_source_seen(sid, true);
        }
        assert!(rx.try_recv().is_err(), "legacy mode must not emit VIEWPORT");
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn zero_session_id_ignored() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);
        vs.on_source_seen(0, true);
        assert!(rx.try_recv().is_err(), "unstamped sentinel must be ignored");
        assert!(vs.known_sources.is_empty());
    }

    #[tokio::test]
    async fn picks_first_n_sorted_deterministically() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        // Observe out of order: 30, 10, 20. Visible set should be {10, 20}.
        vs.on_source_seen(30, true); // visible = [30]
        let (uid, ids) = parse_sent(&rx.try_recv().expect("first emit").bytes);
        assert_eq!(uid, b"bot-1");
        assert_eq!(ids, vec![30]);

        vs.on_source_seen(10, true); // visible = [10, 30] (changed)
        let (_, ids) = parse_sent(&rx.try_recv().expect("second emit").bytes);
        assert_eq!(ids, vec![10, 30]);

        vs.on_source_seen(20, true); // visible = [10, 20] (changed, 30 dropped past N)
        let (_, ids) = parse_sent(&rx.try_recv().expect("third emit").bytes);
        assert_eq!(ids, vec![10, 20]);

        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn no_emit_when_visible_unchanged() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.on_source_seen(1, true); // [1]
        assert!(rx.try_recv().is_ok());
        vs.on_source_seen(2, true); // [1, 2]
        assert!(rx.try_recv().is_ok());

        // A new peer (3) past the visible_count cutoff must NOT move the set.
        vs.on_source_seen(3, true); // visible still [1, 2]
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

        vs.on_source_seen(5, true);
        assert!(rx.try_recv().is_ok());
        // Seeing the same source again is a no-op.
        vs.on_source_seen(5, true);
        assert!(rx.try_recv().is_err());
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn reconnect_resends_current_viewport_after_off_viewport_video() {
        // After a viewport is established AND off-viewport VIDEO is observed (the
        // relay-forgot-my-subscription fail-open symptom), a reconnect re-asserts
        // the SAME current visible subset unconditionally even though the local
        // set did not change. This is the #988 fidelity fix, now gated by the
        // #1006 fail-open symptom so a steady connection stays silent.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.on_source_seen(10, true); // [10]
        vs.on_source_seen(20, true); // [10, 20]
        let (_, ids) = parse_sent(&rx.try_recv().expect("emit 1").bytes);
        assert_eq!(ids, vec![10]);
        let (_, ids) = parse_sent(&rx.try_recv().expect("emit 2").bytes);
        assert_eq!(ids, vec![10, 20]);
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);

        // Off-viewport VIDEO from source 30 (past the visible cutoff, so the
        // visible set stays [10, 20] and NO change emit fires) arms the
        // reconnect re-assert.
        vs.on_source_seen(30, true);
        assert!(
            rx.try_recv().is_err(),
            "off-viewport video past cutoff must not change-emit"
        );

        // Reconnect: known_sources is preserved (mirrors InboundStats::reset
        // take/restore), so the re-assert re-sends the current [10, 20] subset.
        vs.resend_on_reconnect();
        let (uid, ids) = parse_sent(&rx.try_recv().expect("reconnect re-send").bytes);
        assert_eq!(uid, b"bot-1");
        assert_eq!(ids, vec![10, 20]);
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn reconnect_resends_after_off_viewport_video() {
        // Recovery path + flag-clear proof. A steady connection is silent; an
        // off-viewport VIDEO arms the re-assert; the next reset fires; and the
        // flag is cleared so an immediate second reset is silent again.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.on_source_seen(10, true); // [10]
        vs.on_source_seen(20, true); // [10, 20]
        let _ = rx.try_recv().expect("emit 1");
        let _ = rx.try_recv().expect("emit 2");

        // Steady: no off-viewport video → reset is a no-op.
        vs.resend_on_reconnect();
        assert!(rx.try_recv().is_err(), "steady connection must be silent");

        // Off-viewport VIDEO from 30 (past cutoff → visible set stays [10, 20],
        // change path suppressed) ARMS the flag. FAILS if the arming assignment
        // is removed.
        vs.on_source_seen(30, true);
        assert!(
            rx.try_recv().is_err(),
            "off-viewport video past cutoff must not change-emit"
        );

        // Reset now fires the re-assert of the current [10, 20] set.
        vs.resend_on_reconnect();
        let (_, ids) = parse_sent(&rx.try_recv().expect("armed re-assert").bytes);
        assert_eq!(ids, vec![10, 20]);

        // Flag cleared behaviorally: an immediate second reset is silent again
        // (also covered by the MIN_RESEND_INTERVAL guard, but the clear is the
        // primary mechanism — without it the flag would stay armed forever).
        vs.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "armed flag must clear after a successful re-assert"
        );
    }

    #[tokio::test]
    async fn known_off_viewport_source_arms_reassert() {
        // The genuine fail-open recovery case: the relay re-forwards VIDEO from
        // an ALREADY-KNOWN source that is not in `last_sent`. Because the source
        // is already in `known_sources`, `known_sources.insert()` returns false
        // and the change-path early-returns — so arming MUST happen BEFORE that
        // early-return, or this known-source fail-open would never arm and
        // recovery would be silently missed.
        //
        // Mutation guard: moving the arming block below the
        // `if !self.known_sources.insert(session_id) { return; }` line makes this
        // test FAIL (the second sighting of 30 would not arm). The existing
        // recovery tests only arm via a *new* source, which passes `insert()`
        // before the early-return, so they do NOT cover this ordering.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.on_source_seen(10, true); // [10]
        vs.on_source_seen(20, true); // [10, 20]
        let _ = rx.try_recv().expect("emit 1");
        let _ = rx.try_recv().expect("emit 2");

        // Source 30 becomes KNOWN (past the cutoff, so visible stays [10, 20]
        // and no change-emit fires). This first sighting also arms the flag, so
        // clear it via a successful re-assert to isolate the known-source path.
        vs.on_source_seen(30, true);
        assert!(
            rx.try_recv().is_err(),
            "30 past cutoff must not change-emit"
        );
        vs.resend_on_reconnect(); // consumes the arm from 30's first sighting
        let (_, ids) = parse_sent(&rx.try_recv().expect("re-assert clears flag").bytes);
        assert_eq!(ids, vec![10, 20]);
        assert!(
            !vs.off_viewport_video_seen,
            "flag cleared by the re-assert above"
        );

        // Now the relay fail-opens and re-forwards VIDEO from the ALREADY-KNOWN
        // source 30. `known_sources.insert(30)` returns false (it is known), so
        // the change path early-returns — but the arming (which runs first) must
        // still set the flag.
        vs.on_source_seen(30, true);
        assert!(
            vs.off_viewport_video_seen,
            "known off-last_sent source arriving as video must arm (arming runs before the dup early-return)"
        );

        // Bypass the MIN_RESEND_INTERVAL secondary guard so this asserts the GATE,
        // not the rate-limit: the armed flag alone must drive the re-assert.
        vs.last_resend = None;
        vs.resend_on_reconnect();
        let (_, ids) = parse_sent(&rx.try_recv().expect("known-source armed re-assert").bytes);
        assert_eq!(ids, vec![10, 20]);
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

        // First send fails (channel full). has_sent stays false, so the flag is
        // NOT armed — the pending-retry branch must fire regardless.
        vs.on_source_seen(7, true);
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 0);
        assert!(!vs.has_sent);
        assert!(!vs.off_viewport_video_seen);
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
            vs.on_source_seen(sid, true);
        }
        vs.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "legacy mode must not emit on reconnect"
        );
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn no_resend_spam_in_steady_state() {
        // HCL #1006 acceptance criterion: on a stable connection with an
        // unchanged visible set, the bot emits NO VIEWPORT re-assert after its
        // initial send. The reset hook fires every 10s (> MIN_RESEND_INTERVAL),
        // so the rate-limit alone never suppresses it — the off_viewport_video
        // gate is what keeps steady state silent. FAILS on the un-fixed code,
        // which fires on the first call because last_resend is None.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.on_source_seen(10, true); // [10]
        vs.on_source_seen(20, true); // [10, 20]
        let _ = rx.try_recv().expect("emit 1");
        let _ = rx.try_recv().expect("emit 2");
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);

        // First reset-hook tick on a steady connection (no off-viewport video):
        // must NOT re-assert.
        vs.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "steady connection must not re-assert on first reset hook"
        );
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);

        // Many subsequent ticks: still silent. relay_viewport_updates_total
        // stops incrementing in steady state.
        for _ in 0..50 {
            vs.resend_on_reconnect();
            assert!(
                rx.try_recv().is_err(),
                "steady connection must stay silent every reset window"
            );
            assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);
        }
    }

    #[tokio::test]
    async fn off_viewport_audio_does_not_arm() {
        // The relay viewport-filters VIDEO only. Off-viewport AUDIO (or SCREEN)
        // is EXPECTED on a healthy connection and must NOT arm the re-assert.
        // Load-bearing: reverting `is_video` so any media arms makes this FAIL.
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut vs = ViewportSender::new("bot-1".to_string(), Some(2), tx);

        vs.on_source_seen(10, true); // [10]
        vs.on_source_seen(20, true); // [10, 20]
        let _ = rx.try_recv().expect("emit 1");
        let _ = rx.try_recv().expect("emit 2");
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);

        // AUDIO from off-viewport source 30 (past the cutoff so no change emit
        // either). is_video = false → must NOT arm.
        vs.on_source_seen(30, false);
        assert!(
            rx.try_recv().is_err(),
            "off-viewport audio must not change-emit"
        );
        assert!(
            !vs.off_viewport_video_seen,
            "off-viewport audio must not arm the re-assert"
        );

        vs.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "off-viewport audio must not trigger a reconnect re-assert"
        );
        assert_eq!(vs.viewports_sent.load(Ordering::Relaxed), 2);
    }
}
