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

//! Shared session logic for chat sessions.
//!
//! This module contains transport-agnostic session logic used by both
//! `WsChatSession` and `WtChatSession`. The actors become thin transport
//! adapters while all business logic lives here.

use crate::actors::chat_server::ChatServer;
use crate::actors::packet_handler::{
    classify_packet, keyframe_per_pair_budget, outbound_keyframe_observation,
    stamp_raise_hand_for_broadcast, stamp_reaction_for_broadcast, InboundFrameKind,
    KeyframeMediaKind, KeyframeRequestLimiter, KeyframeRequestOutcome, KeyframeTarget,
    MeetingTimerRateLimiter, PacketKind, RaiseHandRateLimiter, ReactionRateLimiter,
};
use crate::client_diagnostics::health_processor::{self, AuthenticatedReporter};
use crate::constants::{
    CONGESTION_DROP_THRESHOLD, CONGESTION_NOTIFY_MIN_INTERVAL, CONGESTION_WINDOW,
    KEYFRAME_CONGESTION_RELAX_WINDOW, RAISE_HAND_DISPLAY_NAME_MAX_BYTES,
    REACTION_DISPLAY_NAME_MAX_BYTES,
};
use crate::messages::server::{ClientMessage, Connect, Disconnect, JoinRoom, Packet};
use crate::messages::session::Message;
use crate::metrics::{
    RELAY_ACTIVE_SESSIONS_PER_ROOM, RELAY_KEYFRAME_REQUESTS_TOTAL,
    RELAY_PUBLISHER_INBOUND_FRAME_GAP_MS, RELAY_ROOM_BYTES_TOTAL,
};
use crate::server_diagnostics::{
    send_connection_ended, send_connection_started, DataTracker, TrackerSender,
};
use crate::session_manager::SessionManager;
use actix::Addr;
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;
use videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind;

pub type SessionId = u64;
pub type RoomId = String;
pub type UserId = String;

lazy_static! {
    /// Process-global monotonic origin shared by the receiver-downlink relief
    /// signal (#1219 Half 2). The transport actor (which owns the per-receiver
    /// [`CongestionTracker`]) and the per-receiver NATS fan-out closure in
    /// `chat_server::handle_msg` run on different tasks, so they cannot share an
    /// `Instant` directly. Instead each side measures time as
    /// `PROCESS_START.elapsed()` and exchanges a `u64` millis "epoch" through a
    /// shared `AtomicU64` (see [`SessionLogic::downlink_congested_epoch`] /
    /// [`downlink_congested_epoch_now`]). Both sides agree on the same origin so
    /// the closure's windowed-decay read matches the writer's clock.
    static ref PROCESS_START: Instant = Instant::now();
}

#[derive(Default)]
struct PublisherInboundFrameGapTracker {
    video: Option<Instant>,
    screen: Option<Instant>,
}

impl PublisherInboundFrameGapTracker {
    fn observe(&mut self, transport: &str, media_kind: MediaKind, frame_kind: InboundFrameKind) {
        let now = Instant::now();
        let (last_arrival, media_kind_label) = match media_kind {
            MediaKind::VIDEO => (&mut self.video, "video"),
            MediaKind::SCREEN => (&mut self.screen, "screen"),
            MediaKind::AUDIO | MediaKind::MEDIA_KIND_UNSPECIFIED => return,
        };

        if let Some(previous) = last_arrival.replace(now) {
            let gap_ms = now.duration_since(previous).as_secs_f64() * 1000.0;
            RELAY_PUBLISHER_INBOUND_FRAME_GAP_MS
                .with_label_values(&[transport, media_kind_label, frame_kind.as_label()])
                .observe(gap_ms);
        }
    }
}

/// Sentinel for [`SessionLogic::downlink_congested_epoch`] meaning "this
/// receiver has never crossed downlink congestion". Stored values are always
/// `>= 1` (see [`downlink_congested_epoch_now`]), so `0` can never collide with
/// a real epoch even in the first millisecond of process life.
pub const DOWNLINK_EPOCH_NEVER: u64 = 0;

/// Current monotonic epoch (millis since the process-global `PROCESS_START`
/// origin, offset by `+1`) for stamping the shared receiver-downlink congestion
/// signal. The `+1` keeps every real epoch strictly greater than the
/// [`DOWNLINK_EPOCH_NEVER`] sentinel.
pub fn downlink_congested_epoch_now() -> u64 {
    (PROCESS_START.elapsed().as_millis() as u64).saturating_add(1)
}

/// Whether a receiver whose last downlink-congestion crossing was stamped at
/// `epoch` (a value produced by [`downlink_congested_epoch_now`], or
/// [`DOWNLINK_EPOCH_NEVER`]) is still inside the relief window `window` as of
/// now. This is the READ side of the shared signal used by the fan-out closure:
/// it makes shed-entry and shed-exit BOTH a time-based decay of the most recent
/// real receiver-downlink drop, so a healthy link recovers automatically once
/// `window` elapses with no fresh crossing (no consecutive-success counter that
/// a single stray drop could reset — the #1219 Half-2 B2 wedge).
pub fn downlink_epoch_is_active(epoch: u64, window: std::time::Duration) -> bool {
    if epoch == DOWNLINK_EPOCH_NEVER {
        return false;
    }
    let now = downlink_congested_epoch_now();
    now.saturating_sub(epoch) <= window.as_millis() as u64
}

/// Connection state for session management during election
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connection is in testing phase (during election)
    Testing,
    /// Connection is active and should broadcast to NATS
    Active,
}

/// Result of handling an inbound packet
#[derive(Debug)]
pub enum InboundAction {
    /// Echo the packet back to sender (RTT measurement)
    Echo(Arc<Vec<u8>>),
    /// Forward to ChatServer for room routing
    Forward(Arc<Vec<u8>>),
    /// Forward to ChatServer for room routing, but ONLY if the sending session
    /// is the room's current HOST (issue #2136).
    ///
    /// A separate variant rather than a flag on [`InboundAction::Forward`] so
    /// that every existing `Forward` site — including the transports and their
    /// tests — keeps its meaning unchanged, and so that "this packet class needs
    /// an authority check" is visible in the type rather than buried in a bool.
    ///
    /// WHY THE CHECK IS NOT MADE HERE, next to the observer guard where every
    /// other authorization in this function lives. `SessionLogic` holds an
    /// `is_host` field, but it is a snapshot of the room JWT's `is_host` claim
    /// taken once at construction and NEVER refreshed. After a transfer-host it
    /// is stale in BOTH directions:
    ///
    ///   * the DEMOTED ex-host still carries `is_host = true` until it
    ///     reconnects, so gating here would let it keep driving the room's
    ///     timer after losing the role; and
    ///   * the PROMOTED new host still carries `is_host = false`, and the UI
    ///     explicitly does NOT reconnect on promotion (it re-fetches `/status`,
    ///     re-signs the room token and re-renders in place), so gating here
    ///     would show the new host a timer control whose every packet the relay
    ///     silently dropped.
    ///
    /// The relay's own presence mirror — `ChatServer.room_members[..].is_host`,
    /// kept in step with the authoritative `meeting_participants.is_host` column
    /// by the `internal.meeting_host_changed` NATS fanout, and reconciled
    /// against a stale re-presented JWT on reconnect — is correct in both
    /// directions. It lives in the `ChatServer` actor, so the check happens
    /// there, on the single fan-out funnel every packet already passes through.
    /// See `session_is_room_host` in `chat_server.rs`.
    ///
    /// The check is deliberately NOT duplicated here as "defense in depth":
    /// AND-ing a stale flag with a fresh one reintroduces the false NEGATIVE
    /// (the promoted host stays blocked), which is the worse of the two
    /// failures.
    ForwardHostOnly(Arc<Vec<u8>>),
    /// Already processed (health packet), no further action
    Processed,
    /// Keep-alive ping, no action needed
    KeepAlive,
}

// =========================================================================
// Congestion Tracking
// =========================================================================

/// Per-sender drop tracking state for congestion feedback.
struct SenderDropState {
    /// Number of drops in the current window.
    drop_count: u32,
    /// Start of the current counting window.
    window_start: Instant,
    /// Last time this sender crossed the drop threshold (i.e. `record_drop`
    /// returned `Some`). Used only to rate-limit threshold crossings; since
    /// #1219 no CONGESTION notification is emitted on a crossing.
    last_notify: Option<Instant>,
}

/// Tracks outbound packet drops per sender for this receiver's downlink.
///
/// Each receiver session has its own `CongestionTracker`. When the receiver's
/// outbound channel is full, the transport layer calls
/// [`CongestionTracker::record_drop`] with the sender's session ID. When enough
/// drops accumulate within the configured window the tracker records that this
/// receiver is *actively congested* (see `last_congestion` /
/// [`CongestionTracker::is_actively_congested`]), which relaxes the
/// KEYFRAME_REQUEST rate limiter so a frozen receiver can recover (#979).
///
/// As of #1219 (Half 1) crossing the threshold no longer authors a sender-keyed
/// CONGESTION `PacketWrapper`: a single slow receiver's full downlink is a
/// per-receiver problem and must not collapse the publisher's encode for the
/// whole room. The publisher's own uplink distress is detected client-side
/// instead (see [`SessionLogic::on_outbound_drop`]).
pub struct CongestionTracker {
    /// Drop state keyed by sender session ID.
    senders: HashMap<u64, SenderDropState>,
    /// Total drops since the last stale-entry cleanup. Cleanup runs every
    /// [`CLEANUP_INTERVAL`] drops to amortize the cost of `retain()`.
    total_drops: u32,
    /// Value of [`total_drops`](Self::total_drops) the last time the #1320
    /// cap-pressure forced sweep ran. Gates that sweep to the same amortized
    /// [`CLEANUP_INTERVAL`] cadence as the unconditional cleanup so that under a
    /// sustained flood of distinct new `sender_session_id`s on an already-full
    /// receiver we do NOT run an O(n) `retain()` on every dropped packet (#1349).
    last_forced_sweep_drops: u32,
    /// Most recent instant at which this receiver crossed the drop threshold
    /// for *any* sender. Used by [`CongestionTracker::is_actively_congested`]
    /// to relax the KEYFRAME_REQUEST rate limiter so a frozen receiver can
    /// recover (issue #979).
    last_congestion: Option<Instant>,
    /// Test-only count of how many times the #1320 cap-pressure forced sweep was
    /// actually invoked. Lets the #1349 gating test assert the sweep does NOT run
    /// on every at-cap drop without depending on internal eviction side effects.
    #[cfg(test)]
    forced_sweep_count: u32,
}

impl Default for CongestionTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of drops between stale-entry cleanup passes. Amortizes the
/// O(n) `retain()` cost so it does not run on every single drop.
const CLEANUP_INTERVAL: u32 = 100;

/// Hard upper bound on the number of distinct senders tracked per receiver
/// (issue #1320). The amortized time-based cleanup runs only every
/// [`CLEANUP_INTERVAL`] drops, so between passes the map could accumulate one
/// entry per distinct `sender_session_id` seen — a memory-amplification vector
/// in the SAME client-forgeable trust class as #1303. The key is the outer
/// `PacketWrapper.session_id` of the forwarded media packet (see
/// `on_outbound_drop`).
///
/// #2095 CLOSED the forgery this cap was sized against: the broadcast path now
/// stamps that field UNCONDITIONALLY with the publisher's authenticated session
/// (`packet_handler::stamp_wrapper_for_broadcast`, called from
/// `chat_server::Handler<ClientMessage>`), so a publisher can no longer mint a
/// distinct key per packet — it contributes exactly ONE entry, as a legitimate
/// publisher always did. The cap is RETAINED as defense-in-depth (it also
/// bounds honest join/leave churn between amortized passes, and it must not
/// depend on a guarantee enforced in a different module). It is ~5–10× the
/// largest realistic room (≈13× a 20-user room),
/// so it never constrains legitimate traffic and only backstops the abuse case.
/// At ~40 bytes per [`SenderDropState`], 256 entries is ~10 KB per receiver.
const MAX_TRACKED_SENDERS: usize = 256;

impl CongestionTracker {
    pub fn new() -> Self {
        Self {
            senders: HashMap::new(),
            total_drops: 0,
            last_forced_sweep_drops: 0,
            last_congestion: None,
            #[cfg(test)]
            forced_sweep_count: 0,
        }
    }

    /// Remove sender entries idle longer than `CONGESTION_WINDOW * 10` (10s of
    /// no recorded drops). Shared by the amortized cleanup and the #1320
    /// cap-pressure sweep.
    fn evict_stale_senders(&mut self, now: Instant) {
        let stale_threshold = CONGESTION_WINDOW * 10;
        self.senders
            .retain(|_, state| now.duration_since(state.window_start) <= stale_threshold);
    }

    /// Record a dropped outbound packet from the given sender.
    ///
    /// Returns `Some(sender_session_id)` when the drop threshold has just been
    /// crossed (rate-limited per sender), otherwise `None`. Since #1219 the
    /// `Some` arm no longer drives a CONGESTION emit; crossing the threshold
    /// updates `last_congestion` so [`CongestionTracker::is_actively_congested`]
    /// can relax the KEYFRAME_REQUEST limiter (#979).
    ///
    /// Performs amortized cleanup of stale entries every [`CLEANUP_INTERVAL`]
    /// drops: any sender whose `window_start` is older than
    /// `CONGESTION_WINDOW * 10` (10 seconds of inactivity) is removed. This
    /// prevents unbounded growth when transient participants leave while
    /// avoiding an O(n) `retain()` on every single drop. As a defense-in-depth
    /// backstop the map is ALSO hard-capped at [`MAX_TRACKED_SENDERS`] (#1320):
    /// at the cap a NEW sender is refused so the map cannot grow unbounded
    /// between amortized passes. The cap-pressure reclaim sweep is itself gated
    /// to the [`CLEANUP_INTERVAL`] cadence (#1349) so a flood of distinct new
    /// senders against a saturated receiver stays O(1) per drop rather than
    /// triggering a full `retain()` on each packet.
    pub fn record_drop(&mut self, sender_session_id: u64) -> Option<u64> {
        let now = Instant::now();

        // Amortized cleanup of stale sender entries.
        self.total_drops = self.total_drops.wrapping_add(1);
        if self.total_drops.is_multiple_of(CLEANUP_INTERVAL) {
            self.evict_stale_senders(now);
        }

        // #1320: hard entry-count bound as defense-in-depth on top of the
        // amortized time-based cleanup above. If we are at the cap and this is a
        // NEW sender, try once to reclaim space, then either admit (if room was
        // freed) or skip tracking this drop rather than grow the map unbounded.
        // An ALREADY-tracked sender is never refused (its window/notify state and
        // the #979 keyframe-relax path it feeds are untouched). Refusing a brand
        // new sender only when the cap is genuinely full is harmless: at that
        // point this receiver is already tracking MAX_TRACKED_SENDERS congested
        // sources, `last_congestion` is already being driven, and
        // `is_actively_congested()` already returns true — so the dropped
        // tracking for one more sender costs nothing the relax path needs.
        //
        // #1349: the reclaim sweep is GATED to the same amortized CLEANUP_INTERVAL
        // cadence as the unconditional cleanup above, keyed off `total_drops`.
        // `sender_session_id` is the publisher-controlled outer
        // `PacketWrapper.session_id` (see the MAX_TRACKED_SENDERS doc), so a
        // malicious publisher can present a distinct NEW id on every dropped
        // packet to a saturated receiver. Without the gate that adversarial path
        // would run a full O(<=MAX_TRACKED_SENDERS) `retain()` PER PACKET — a
        // self-inflicted O(n)-per-drop on exactly the case the cap backstops.
        // Gating makes the steady-state at-cap drop O(1): the sweep runs at most
        // once per CLEANUP_INTERVAL drops, and between sweeps an over-cap new
        // sender is simply refused. The memory bound is unaffected — the map only
        // ever grows when a sweep has actually freed a slot, never on the
        // gated-off path.
        if self.senders.len() >= MAX_TRACKED_SENDERS
            && !self.senders.contains_key(&sender_session_id)
        {
            let due_for_sweep =
                self.total_drops.wrapping_sub(self.last_forced_sweep_drops) >= CLEANUP_INTERVAL;
            if due_for_sweep {
                self.last_forced_sweep_drops = self.total_drops;
                #[cfg(test)]
                {
                    self.forced_sweep_count += 1;
                }
                self.evict_stale_senders(now);
            }
            if self.senders.len() >= MAX_TRACKED_SENDERS
                && !self.senders.contains_key(&sender_session_id)
            {
                return None;
            }
        }

        let state = self
            .senders
            .entry(sender_session_id)
            .or_insert_with(|| SenderDropState {
                drop_count: 0,
                window_start: now,
                last_notify: None,
            });

        // Reset window if it has elapsed.
        if now.duration_since(state.window_start) > CONGESTION_WINDOW {
            state.drop_count = 0;
            state.window_start = now;
        }

        state.drop_count += 1;

        if state.drop_count >= CONGESTION_DROP_THRESHOLD {
            // Rate-limit notifications.
            if let Some(last) = state.last_notify {
                if now.duration_since(last) < CONGESTION_NOTIFY_MIN_INTERVAL {
                    return None;
                }
            }
            state.last_notify = Some(now);
            state.drop_count = 0;
            state.window_start = now;
            // Record that this receiver is now actively congested so the
            // KEYFRAME_REQUEST limiter can relax its per-pair budget and let
            // a frozen receiver recover (issue #979).
            self.last_congestion = Some(now);
            Some(sender_session_id)
        } else {
            None
        }
    }

    /// Whether this receiver crossed the congestion drop threshold recently
    /// enough (within [`KEYFRAME_CONGESTION_RELAX_WINDOW`]) to be considered
    /// in **active congestion** (issue #979).
    ///
    /// Used by the inbound KEYFRAME_REQUEST handler to decide whether to use
    /// the relaxed per-pair keyframe budget. A receiver is "actively
    /// congested" precisely when the relay has had to drop inbound media
    /// destined for it — the scenario in which its decoder is most likely
    /// frozen and genuinely needs fresh keyframes to recover.
    pub fn is_actively_congested(&self) -> bool {
        self.last_congestion
            .is_some_and(|t| Instant::now().duration_since(t) <= KEYFRAME_CONGESTION_RELAX_WINDOW)
    }
}

/// Shared session logic, transport-agnostic.
///
/// This struct contains all the business logic for a chat session.
/// The transport-specific actors (`WsChatSession`, `WtChatSession`)
/// own an instance of this and delegate to it.
pub struct SessionLogic {
    pub id: u64,
    pub room: RoomId,
    pub user_id: UserId,
    /// Participant's chosen display name (from JWT claims).
    /// Falls back to `user_id` when no display name is available.
    pub display_name: String,
    /// Server-authoritative guest flag (JWT `is_guest` claim).
    pub is_guest: bool,
    pub addr: Addr<ChatServer>,
    pub nats_client: async_nats::client::Client,
    pub tracker_sender: TrackerSender,
    pub session_manager: SessionManager,
    /// When true, this session is observer-only: it can receive messages
    /// but cannot publish media to the room.
    pub observer: bool,
    /// Stable client instance identifier (UUID). Survives reconnects within
    /// the same tab/meeting join. Used by the server to correlate reconnections.
    pub instance_id: Option<String>,
    /// Transport type for this session ("websocket" or "webtransport")
    pub transport: String,
    /// Whether this participant is the meeting host.
    pub is_host: bool,
    /// Whether the meeting should end when the host leaves.
    pub end_on_host_leave: bool,
    /// Tracks this receiver's outbound packet drops per sender; feeds the
    /// #979 keyframe-relax path (no longer a CONGESTION emit, see #1219).
    pub congestion_tracker: CongestionTracker,
    /// Per-session rate limiter for KEYFRAME_REQUEST packets.
    pub keyframe_limiter: KeyframeRequestLimiter,
    /// Per-session rate limiter for client-authored REACTION broadcasts (#1884).
    /// Meters this SENDER's reactions before they are re-broadcast to the room.
    pub reaction_limiter: ReactionRateLimiter,
    /// Per-session rate limiter for client-authored RAISE_HAND broadcasts
    /// (#2135). Meters this SENDER's hand-state announces (including the
    /// re-announces it emits when a new peer joins) before they are re-broadcast
    /// to the room. Separate budget from `reaction_limiter` — see
    /// [`RaiseHandRateLimiter`].
    pub raise_hand_limiter: RaiseHandRateLimiter,
    /// Per-session rate limiter for client-authored MEETING_TIMER broadcasts
    /// (#2136). Meters this SENDER's timer state announces — including the ~5s
    /// heartbeat while a timer runs and the repeat burst on each transition —
    /// before they are re-broadcast to the room. Separate budget from
    /// `reaction_limiter`; see [`MeetingTimerRateLimiter`].
    pub meeting_timer_limiter: MeetingTimerRateLimiter,
    /// Shared receiver-downlink-congestion signal for #1219 Half 2.
    ///
    /// Written by THIS transport actor in [`SessionLogic::on_outbound_drop`]
    /// (the REAL per-receiver downlink backpressure surface — the bounded
    /// `outbound_tx` channel overflow that the windowed [`CongestionTracker`]
    /// observes), and READ by the per-receiver NATS fan-out closure in
    /// `chat_server::handle_msg` to decide emergency layer shedding + the
    /// one-shot DOWNLINK_CONGESTION emit. The same `Arc` is handed to the
    /// closure via [`JoinRoom`](crate::messages::server::JoinRoom).
    ///
    /// Holds a monotonic millis epoch ([`downlink_congested_epoch_now`]) of the
    /// most recent crossing, or [`DOWNLINK_EPOCH_NEVER`]. The closure applies a
    /// time-decaying window to it, so this is a level (not edge) signal: it
    /// never needs an explicit "clear" write when the link recovers.
    pub downlink_congested_epoch: Arc<AtomicU64>,
    /// Last publisher-to-relay inbound VIDEO/SCREEN arrivals for this session.
    ///
    /// Fresh `SessionLogic` actors are constructed on reconnect/re-election, so
    /// the first frame after a transition intentionally has no previous sample.
    publisher_inbound_frame_gap_tracker: PublisherInboundFrameGapTracker,
}

impl SessionLogic {
    /// Create a new session logic instance
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        user_id: String,
        display_name: String,
        is_guest: bool,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
        observer: bool,
        instance_id: Option<String>,
        transport: &str,
        is_host: bool,
        end_on_host_leave: bool,
    ) -> Self {
        let id = (Uuid::new_v4().as_u128() & 0xffffffffffffffff) as u64;
        info!(
            "new session: room={} user_id={} display_name={} is_guest={} session_id={} observer={} is_host={} transport={}",
            room, user_id, display_name, is_guest, id, observer, is_host, transport
        );

        SessionLogic {
            id,
            room,
            user_id,
            display_name,
            is_guest,
            addr,
            nats_client,
            tracker_sender,
            session_manager,
            observer,
            instance_id,
            transport: transport.to_string(),
            is_host,
            end_on_host_leave,
            congestion_tracker: CongestionTracker::new(),
            keyframe_limiter: KeyframeRequestLimiter::new(),
            reaction_limiter: ReactionRateLimiter::new(),
            raise_hand_limiter: RaiseHandRateLimiter::new(),
            meeting_timer_limiter: MeetingTimerRateLimiter::new(),
            downlink_congested_epoch: Arc::new(AtomicU64::new(DOWNLINK_EPOCH_NEVER)),
            publisher_inbound_frame_gap_tracker: PublisherInboundFrameGapTracker::default(),
        }
    }

    fn observe_publisher_inbound_frame_gap(
        &mut self,
        media_kind: MediaKind,
        frame_kind: InboundFrameKind,
    ) {
        self.publisher_inbound_frame_gap_tracker
            .observe(&self.transport, media_kind, frame_kind);
    }

    /// Record a per-session outbound drop on `relay_session_drops_total`
    /// (dashboard audit Tier B #1).
    ///
    /// Called from both transport actors' drop sites (priority-preempt and
    /// real channel-full) with the same `kind` label they pass to the
    /// protocol-wide `videocall_outbound_channel_drops_total` counter, so the
    /// two stay in lock-step. `kind` MUST be a `'static` string from the bounded
    /// drop-kind taxonomy ([`crate::metrics::RELAY_DROP_KINDS`]); the actors
    /// only ever pass string literals / the `priority_drop.rs` reason labels.
    ///
    /// No per-session bookkeeping of which kinds were emitted is needed:
    /// [`on_stopping`] GCs the FULL fixed taxonomy unconditionally (issue #1090),
    /// so the cleanup is leak-proof regardless of which subset this session
    /// happened to increment.
    pub fn record_session_drop(&self, kind: &'static str) {
        let session_id = self.id.to_string();
        crate::metrics::RELAY_SESSION_DROPS_TOTAL
            .with_label_values(&[&self.room, &self.transport, &session_id, kind])
            .inc();
    }

    // =========================================================================
    // Lifecycle
    // =========================================================================

    /// Track connection start for metrics
    pub fn track_connection_start(&self) {
        RELAY_ACTIVE_SESSIONS_PER_ROOM
            .with_label_values(&[&self.room, &self.transport])
            .inc();
        send_connection_started(
            &self.tracker_sender,
            self.id,
            self.user_id.clone(),
            self.room.clone(),
            self.transport.clone(),
        );
    }

    /// Build MEETING_STARTED packet
    pub fn build_meeting_started(&self, start_time_ms: u64, creator_id: &str) -> Vec<u8> {
        SessionManager::build_meeting_started_packet(&self.room, start_time_ms, creator_id)
    }

    /// Build SESSION_ASSIGNED packet for this session
    pub fn build_session_assigned(&self) -> Vec<u8> {
        SessionManager::build_session_assigned_packet(self.id)
    }

    /// This session's relay-authenticated identity, for stamping onto telemetry
    /// the client authored (issue 2047).
    ///
    /// Deliberately takes ONLY `&self`: the inbound packet is not in scope here,
    /// so the returned identity is session-derived BY CONSTRUCTION and cannot be
    /// sourced from client-supplied bytes. Callers must use this rather than
    /// assembling an [`AuthenticatedReporter`] inline — an inline literal at the
    /// call site is the one edit that reintroduces the telemetry-impersonation
    /// vulnerability while leaving every stamping test green.
    ///
    /// Read fresh on each call, never cached: a reconnect or re-election builds a
    /// NEW `SessionLogic` with a new [`Self::id`], and none of the three fields is
    /// mutated in place for the life of a session, so this is always the CURRENT
    /// identity.
    ///
    /// See [`AuthenticatedReporter`] for the provenance of each field (and the
    /// caveat that on the deprecated path-based endpoint the identity is
    /// URL-chosen rather than JWT-authenticated).
    pub fn authenticated_reporter(&self) -> AuthenticatedReporter {
        Self::reporter_from_session_fields(&self.room, self.id, &self.user_id)
    }

    /// Field mapping for [`Self::authenticated_reporter`], split out as a pure
    /// function so a unit test can pin WHICH session field feeds WHICH telemetry
    /// label without standing up an actor (a real `SessionLogic` needs a live
    /// NATS connection). Transposing `room` and `user_id` here would silently
    /// relabel every dashboard.
    fn reporter_from_session_fields(
        room: &str,
        session_id: u64,
        user_id: &str,
    ) -> AuthenticatedReporter {
        AuthenticatedReporter {
            meeting_id: room.to_string(),
            session_id,
            user_id: user_id.to_string(),
        }
    }

    /// Build MEETING_ENDED packet (for errors)
    pub fn build_meeting_ended(&self, reason: &str) -> Vec<u8> {
        SessionManager::build_meeting_ended_packet(&self.room, reason)
    }

    /// Create Connect message for ChatServer registration
    pub fn create_connect_message<R>(&self, recipient: R) -> Connect
    where
        R: Into<actix::Recipient<Message>>,
    {
        Connect {
            id: self.id,
            addr: recipient.into(),
        }
    }

    /// Create JoinRoom message for ChatServer
    pub fn create_join_room_message(&self) -> JoinRoom {
        JoinRoom {
            room: self.room.clone(),
            session: self.id,
            user_id: self.user_id.clone(),
            display_name: self.display_name.clone(),
            is_guest: self.is_guest,
            observer: self.observer,
            instance_id: self.instance_id.clone(),
            is_host: self.is_host,
            end_on_host_leave: self.end_on_host_leave,
            transport: self.transport.clone(),
            // #1219 Half 2: hand the per-receiver downlink-congestion signal to
            // the fan-out closure. The closure reads it to drive emergency
            // shedding; this actor writes it from `on_outbound_drop`.
            downlink_congested_epoch: Arc::clone(&self.downlink_congested_epoch),
        }
    }

    /// Create ClientMessage for forwarding a packet to ChatServer (NATS broadcast).
    pub fn create_client_message(&self, msg: Packet) -> ClientMessage {
        ClientMessage {
            session: self.id,
            user: self.user_id.clone(),
            room: self.room.clone(),
            msg,
            requires_host: false,
        }
    }

    /// Build a [`ClientMessage`] that the `ChatServer` funnel must refuse to fan
    /// out unless this session is the room's current HOST (issue #2136).
    ///
    /// The ONLY producer is the [`InboundAction::ForwardHostOnly`] branch in the
    /// transports, which `handle_inbound` returns exclusively for
    /// [`PacketKind::MeetingTimer`]. Read [`InboundAction::ForwardHostOnly`] for
    /// why the authority check lives at the funnel rather than here.
    ///
    /// Identity is taken from `&self` session state (`self.id`), never from the
    /// packet — the funnel resolves host-ness by that session id against the
    /// room's presence mirror, so a client cannot present another participant's
    /// session and borrow their authority.
    pub fn create_host_gated_client_message(&self, msg: Packet) -> ClientMessage {
        ClientMessage {
            session: self.id,
            user: self.user_id.clone(),
            room: self.room.clone(),
            msg,
            requires_host: true,
        }
    }

    /// Build the right [`ClientMessage`] for an outgoing [`Packet`], preserving
    /// its `requires_host` flag (issue #2136).
    ///
    /// THE SINGLE SITE both transports call from their `Handler<Packet>`. It
    /// exists specifically so the WS and WT paths cannot drift: the flag is what
    /// decides whether `ChatServer` applies the host authorization, so a
    /// mirrored `if` in each transport would mean a one-line edit in either file
    /// could silently disable the gate on THAT TRANSPORT ONLY — the hardest
    /// class of bug to notice, because the feature keeps working for whichever
    /// transport the tester happens to be on. One function, one test.
    pub fn client_message_for(&self, msg: Packet) -> ClientMessage {
        if msg.requires_host {
            self.create_host_gated_client_message(msg)
        } else {
            self.create_client_message(msg)
        }
    }

    /// Handle JoinRoom response. Returns true if the session should stop (error case).
    pub fn handle_join_room_result(
        &self,
        result: Result<Result<(), String>, actix::MailboxError>,
    ) -> bool {
        match result {
            Ok(Ok(())) => {
                info!(
                    "Successfully joined room {} for session {}",
                    self.room, self.id
                );
                false
            }
            Ok(Err(e)) => {
                error!("Failed to join room: {}", e);
                true
            }
            Err(err) => {
                error!("Error sending JoinRoom: {:?}", err);
                true
            }
        }
    }

    /// Handle actor stopping - cleanup
    pub fn on_stopping(&self) {
        info!("Session stopping: {} in room {}", self.id, self.room);
        RELAY_ACTIVE_SESSIONS_PER_ROOM
            .with_label_values(&[&self.room, &self.transport])
            .dec();

        // GC the per-session drop series (Tier B #1). `relay_session_drops_total`
        // carries an unbounded-over-time `session_id` label; removing every
        // `(room, transport, session_id, kind)` tuple the moment this session
        // disconnects keeps the live series count bounded to active sessions.
        //
        // LEAK-PROOF (issue #1090): `forget_session_drops` iterates the FULL fixed
        // `kind` taxonomy [`crate::metrics::RELAY_DROP_KINDS`] UNCONDITIONALLY
        // rather than a per-session "kinds I emitted" tracking set, so a session
        // that only ever incremented a subset of kinds is still fully cleaned.
        // The sweep lives in `metrics` as the single source of truth (issue #1186)
        // so the #1090 GC test pins the HELPER's full-taxonomy behavior rather than
        // an inline copy. NOTE (issue #1380): that test calls `forget_session_drops`
        // directly and does NOT exercise this call site — reverting THIS line to an
        // inline per-session-subset loop would still pass CI. Keep this call wired to
        // the full-taxonomy helper; it is the only thing standing between #1090 and a
        // re-regression here.
        let session_id = self.id.to_string();
        crate::metrics::forget_session_drops(&self.room, &self.transport, &session_id);
        crate::metrics::forget_outbound_queue_depth_by_session(
            &self.room,
            &self.transport,
            &session_id,
        );
        crate::metrics::forget_outbound_queue_bytes_by_session(
            &self.room,
            &self.transport,
            &session_id,
        );
        send_connection_ended(&self.tracker_sender, self.id);
        self.addr.do_send(Disconnect {
            session: self.id,
            room: self.room.clone(),
            user_id: self.user_id.clone(),
            display_name: self.display_name.clone(),
            is_guest: self.is_guest,
            observer: self.observer,
            is_host: self.is_host,
            end_on_host_leave: self.end_on_host_leave,
        });
    }

    // =========================================================================
    // Packet Handling
    // =========================================================================

    /// Returns true if this action should trigger connection activation.
    /// RTT probes (Echo) do not activate; any other packet does.
    pub fn should_activate_on_action(action: &InboundAction) -> bool {
        !matches!(action, InboundAction::Echo(_))
    }

    /// Run one KEYFRAME_REQUEST through `limiter` and record the decision on
    /// `relay_keyframe_requests_total{room, kind, outcome}` (#2394).
    fn admit_keyframe_request(
        limiter: &mut KeyframeRequestLimiter,
        room: &str,
        target: KeyframeTarget,
        kind: KeyframeMediaKind,
        layer: u32,
        congested: bool,
    ) -> KeyframeRequestOutcome {
        let outcome = limiter.classify_with_congestion(target, kind, layer, congested);
        RELAY_KEYFRAME_REQUESTS_TOTAL
            .with_label_values(&[room, kind.metric_label(), outcome.metric_label()])
            .inc();
        outcome
    }

    /// Handle an inbound packet from the client.
    ///
    /// Returns the action the transport should take.
    /// An observer session may still send RTT (which the relay ECHOES back to
    /// the sender and never fans out); everything that would reach another
    /// participant — Data, Media, KeyframeRequest, Reaction, RaiseHand,
    /// MeetingTimer, and Health — is silently dropped.
    ///
    /// This is the **inbound** half of the waiting-room isolation enforcement.
    /// The **outbound** half lives in `chat_server::handle_msg()` (a free
    /// function, not a method on the actor) which drops all non-allowlisted
    /// packets before they reach observer sessions.
    /// See `handle_msg` doc comment for the full three-layer enforcement model.
    pub fn handle_inbound(&mut self, data: &[u8]) -> InboundAction {
        // Track received data
        RELAY_ROOM_BYTES_TOTAL
            .with_label_values(&[&self.room, "inbound"])
            .inc_by(data.len() as f64);
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_received(self.id, data.len() as u64);

        // Classify and handle
        match classify_packet(data) {
            PacketKind::Dropped => {
                debug!(
                    "Dropping disallowed packet from session {} (user {})",
                    self.id, self.user_id
                );
                InboundAction::Processed
            }
            PacketKind::Rtt => {
                trace!("RTT packet from {}, echoing back", self.user_id);
                let data_tracker = DataTracker::new(self.tracker_sender.clone());
                data_tracker.track_sent(self.id, data.len() as u64);
                InboundAction::Echo(Arc::new(data.to_vec()))
            }
            PacketKind::Health => {
                // WAITING-ROOM ISOLATION (#2095 review, MEDIUM). An observer sits
                // in the waiting room and has NOT been admitted, so nothing it
                // sends may reach the meeting. Every other `Forward` arm already
                // guards on this (Data, Media, KeyframeRequest, Reaction); HEALTH
                // did not, and #2095 turned that omission into a real leak:
                //
                //  * The relay now STAMPS the fanned-out envelope with the
                //    sender's AUTHENTICATED `sub` (`stamp_wrapper_for_broadcast`),
                //    so an observer's HEALTH disclosed its server-side identity —
                //    email, or `guest:{uuid}` — to every participant. That is an
                //    identity PARTICIPANT_JOINED deliberately does NOT broadcast
                //    for observers: they are not tracked in `room_members`
                //    (chat_server.rs, `Handler<JoinRoom>`).
                //  * HEALTH is not in the client's
                //    `suppresses_peer_creation_for_packet` allowlist, so the
                //    forwarded packet calls `ensure_peer` in every admitted
                //    participant's browser — a never-admitted client could mint
                //    peer tiles and their decoder Workers room-wide.
                //
                // The guard belongs HERE, on the relay, not in the UI: the stock
                // client already sets `enable_health_reporting: false` for
                // observers, which is precisely why a MODIFIED client is the
                // threat and why a client-side check enforces nothing. This is
                // also the single site both transports share (`WsChatSession` and
                // `WtChatSession` both delegate to `handle_inbound`).
                //
                // The return is BEFORE `process_health_packet_bytes`, so an
                // observer's HEALTH is not fanned out AND does not feed the
                // server-side Prometheus telemetry either. No legitimate observer
                // sends HEALTH at all, so nothing real is lost; admitting one to
                // the telemetry path would only let an unadmitted client write
                // operator dashboards.
                if self.observer {
                    trace!(
                        "Observer session {} dropping health packet from {}",
                        self.id,
                        self.user_id
                    );
                    return InboundAction::Processed;
                }

                trace!("Health packet from {}", self.user_id);
                // #1482: process for server-side NATS telemetry AND forward to peers
                // so each peer's client can read the sender's self-reported
                // device/hardware metrics (peer_device_info). Previously returned
                // Processed, which consumed the packet and starved the per-peer
                // Device UI of data. HEALTH carries no media and is unencrypted; the
                // outbound observer allowlist in chat_server::handle_msg drops it on
                // the RECEIVE side for waiting-room observers, and the guard above
                // now closes the SEND side.
                //
                // #1543: the FULL packet goes to the server-side NATS telemetry path
                // FIRST (operators rely on the heavy per-peer `peer_stats` map). Then
                // the PEER fan-out is TRIMMED to only the device fields the peer UI
                // reads (`trim_health_packet_for_peers`), dropping `peer_stats` and
                // every other unread field — this breaks the O(N²) relay egress at
                // scale. The trim parses + re-serializes ONCE here (O(1) per inbound
                // HEALTH), BEFORE the relay's per-recipient fan-out, so it is never
                // repeated per recipient.
                //
                // Issue 2047 (SECURITY): the packet's self-declared
                // meeting_id/session_id/reporting_user_id become Prometheus
                // LABELS downstream, so the relay stamps them with THIS session's
                // authenticated identity before publishing (mirrors the #1884
                // REACTION stamp). Both transports reach this one arm
                // (`WsChatSession` and `WtChatSession` both delegate inbound
                // frames to `handle_inbound`), so there is a single stamping site.
                //
                // The identity comes from [`Self::authenticated_reporter`], which
                // takes ONLY `&self` — the inbound `data` is not in scope inside
                // it, so the reporter cannot be sourced from the packet even by
                // accident. Do NOT inline a struct literal here: that is exactly
                // the edit that would reintroduce the vulnerability while every
                // stamping test still passed.
                health_processor::process_health_packet_bytes(
                    data,
                    self.nats_client.clone(),
                    self.authenticated_reporter(),
                );
                let trimmed = health_processor::trim_health_packet_for_peers(data);
                InboundAction::Forward(Arc::new(trimmed))
            }
            PacketKind::KeyframeRequest {
                target_user_id,
                target_session_id,
                layer,
                kind,
            } => {
                if self.observer {
                    return InboundAction::Processed;
                }
                // Rate-limit KEYFRAME_REQUEST packets per
                // `(receiver, target_sender)` pair. The per-pair dimension
                // is what allows a fresh joiner into a populated room to
                // request keyframes from every existing sender within the
                // first second after joining without being clipped — the
                // fix for the frozen-video-on-join bug. A coarser global
                // cap inside the limiter still bounds total fan-out as a
                // defense against abuse.
                //
                // Issue #979: if the relay has recently had to drop inbound
                // media destined for this receiver (active congestion), its
                // decoder is likely frozen and genuinely needs fresh
                // keyframes to recover. In that case relax the per-pair
                // budget so the strict 1/sec steady-state limit does not
                // hold the receiver frozen. The global per-receiver ceiling
                // is unchanged, so the keyframe-storm risk (OSS #814) stays
                // bounded — the cap is relaxed, not removed.
                let congested = self.congestion_tracker.is_actively_congested();
                // #1124: key the limiter by the target SESSION when the client
                // populated it (independent budgets for concurrent sessions of
                // one identity), else fall back to the target user_id (older
                // clients). `KeyframeTarget::from_request` encodes that choice.
                let target = KeyframeTarget::from_request(&target_user_id, target_session_id);
                // #1297: `kind` (derived from the inner request bytes in
                // `classify_packet`) splits VIDEO and SCREEN into separate
                // rate-limit buckets so SCREEN recovery is not starved by VIDEO
                // requests in the same second. The delivery-aware relaxation
                // inside `allow_with_congestion` lets a still-frozen receiver on
                // a lossless WS path re-request even when the strict budget is
                // exhausted (the `congested` path cannot fire on a lossless
                // link); `observe_outbound_delivery` clears it on delivery.
                let outcome = Self::admit_keyframe_request(
                    &mut self.keyframe_limiter,
                    &self.room,
                    target,
                    kind,
                    layer,
                    congested,
                );
                if !outcome.admitted() {
                    // #1899: log the media kind, congestion state, and the
                    // per-pair budget that applied (via the SAME
                    // `keyframe_per_pair_budget` the limiter enforced, so the log
                    // cannot misreport it). The field diagnosis of the
                    // screen-freeze bug depended on knowing SCREEN requests were
                    // being throttled at the camera budget — surfacing kind +
                    // budget here makes the next diagnosis a single grep.
                    warn!(
                        "Rate-limiting KEYFRAME_REQUEST from session {} (user {}) targeting user {} session {} kind={:?} congested={} per_pair_budget={} outcome={:?}",
                        self.id,
                        self.user_id,
                        String::from_utf8_lossy(&target_user_id),
                        target_session_id,
                        kind,
                        congested,
                        keyframe_per_pair_budget(kind, congested),
                        outcome,
                    );
                    return InboundAction::Processed;
                }
                InboundAction::Forward(Arc::new(data.to_vec()))
            }
            PacketKind::Reaction => {
                // Waiting-room isolation: an observer must NOT broadcast a
                // reaction into the meeting. A REACTION is re-broadcast on the
                // media fan-out, so an observer-sent one would reach every
                // active participant — the same isolation break the Data and
                // KeyframeRequest observer guards prevent. Mirror them here.
                if self.observer {
                    return InboundAction::Processed;
                }
                // Per-sender rate limit (#1884). The closed-enum validation
                // already ran in `classify_packet` (an invalid reaction is
                // `PacketKind::Dropped` and never reaches here), so this meters
                // ONLY valid reactions against this sender's window — a flood of
                // invalid reactions cannot consume the budget. Over budget →
                // drop as Processed (no fan-out); within budget → forward on the
                // standard media fan-out (re-broadcast room-wide; the relay
                // self-skips the sender, which renders its own local echo).
                //
                // No sealing: the inner packet stays CLEARTEXT. Before fan-out
                // the relay UNCONDITIONALLY re-stamps the envelope session_id to
                // THIS sender's authenticated session (`self.id`) and bounds the
                // cosmetic display_name (see `stamp_reaction_for_broadcast`).
                // That is what makes reaction attribution trustworthy: it anchors
                // on a relay-authenticated session_id, never a client-supplied
                // one (a forged non-zero session_id would otherwise impersonate a
                // victim, cleartext, with no E2EE backstop), and the downstream
                // self-echo suppression that keys on session_id becomes reliable
                // for REACTION.
                //
                // `debug!` (not `warn!` like the keyframe path): a well-behaved
                // client self-throttles STRICTLY below this relay cap, so a hit
                // is only reachable by a misbehaving or forged client — logging
                // every drop at warn would hand a reaction flood a
                // log-amplification lever. The drop itself is the enforcement.
                if !self.reaction_limiter.allow() {
                    debug!(
                        "Rate-limiting REACTION from session {} (user {})",
                        self.id, self.user_id
                    );
                    return InboundAction::Processed;
                }
                // SECURITY (#1884, web-security-auditor BLOCKER): this arm MUST
                // forward ONLY the output of `stamp_reaction_for_broadcast` —
                // never the raw inbound `data`. A REACTION is the one
                // client-authored packet re-broadcast CLEARTEXT to the whole
                // room, so its envelope session_id must be the relay's
                // authenticated session, not a client-supplied (forgeable) one;
                // forwarding raw `data` here (e.g. `Forward(Arc::new(data.to_vec()))`)
                // reopens the victim-impersonation vector — and would do so with
                // every test still GREEN, because
                // `test_stamp_reaction_overwrites_forged_session_id` guards the
                // stamping fn, not this call site. To make that mistake
                // impossible-by-accident we SHADOW `data` with the stamped
                // `Option<Vec<u8>>` below: the raw `&[u8]` is no longer reachable
                // in this scope, so an accidental passthrough will not compile.
                //
                // Fail-closed: an (already enum-validated) packet that will not
                // re-serialize is dropped, never fanned out unstamped.
                let data =
                    stamp_reaction_for_broadcast(data, self.id, REACTION_DISPLAY_NAME_MAX_BYTES);
                match data {
                    Some(bytes) => InboundAction::Forward(Arc::new(bytes)),
                    None => InboundAction::Processed,
                }
            }
            PacketKind::RaiseHand => {
                // WAITING-ROOM ISOLATION (#2135). An observer sits in the
                // waiting room and has NOT been admitted, so nothing it sends
                // may reach the meeting. EVERY other `Forward` arm guards on
                // this (Data, Media, KeyframeRequest, Reaction, and — since
                // #2124 — Health), and RAISE_HAND is the arm most in need of it:
                //
                //  * It is re-broadcast on the media fan-out, so an
                //    observer-sent one reaches every admitted participant.
                //  * The relay STAMPS the fanned-out envelope with the sender's
                //    AUTHENTICATED session_id and `sub`
                //    (`stamp_wrapper_for_broadcast`, #2124), so it would
                //    disclose an unadmitted client's server-side identity —
                //    email, or `guest:{uuid}` — to the whole room. That is
                //    exactly the leak the #2124 HEALTH guard closed, and it is
                //    WORSE here: a raised hand is rendered as a named, durable
                //    entry in the participants list rather than a transient
                //    metric.
                //  * The client-side `suppresses_peer_creation_for_packet` entry
                //    added alongside this (videocall-client) stops an inbound
                //    RAISE_HAND from minting a ghost TILE, but that is UI
                //    hygiene, not isolation — it does nothing about the state
                //    itself, which would still land in every participant's
                //    raised-hands list. Only this guard prevents that, and it
                //    must live on the relay: the stock client already refuses to
                //    send while un-admitted, which is precisely why a MODIFIED
                //    client is the threat and a client-side check enforces
                //    nothing.
                //
                // The RECEIVE side is already fail-closed independently: the
                // outbound observer allowlist in `chat_server::handle_msg`
                // forwards only MEETING and SESSION_ASSIGNED to an observer, so
                // a waiting-room client never SEES a raised hand either. This
                // guard closes the SEND side, giving the same two-sided
                // isolation the other packet types have.
                if self.observer {
                    trace!(
                        "Observer session {} dropping raise-hand packet from {}",
                        self.id,
                        self.user_id
                    );
                    return InboundAction::Processed;
                }
                // Per-sender rate limit (#2135). Ingress validation (size cap +
                // parse) already ran in `classify_packet` (an oversized or
                // unparseable packet is `PacketKind::Dropped` and never reaches
                // here), so this meters ONLY well-formed packets against this
                // sender's window — a flood of garbage cannot consume the
                // budget.
                //
                // A DROP HERE IS MORE COSTLY THAN A DROPPED REACTION, and that
                // asymmetry is why the budget is sized generously (see
                // `RAISE_HAND_MAX_PER_WINDOW`): a reaction is an ephemeral
                // float, but a raise-hand carries persistent STATE and the relay
                // keeps no hand registry to repair from, so a dropped transition
                // leaves the room's view of this participant wrong until the
                // client announces again. The packet is idempotent state, so the
                // client's next re-announce (on the next peer-join) repairs it.
                //
                // `debug!` (not `warn!`), mirroring the REACTION arm: a
                // well-behaved client self-throttles strictly below this ceiling
                // and coalesces its join-wave re-announces, so a hit is only
                // reachable by a misbehaving or forged client — logging every
                // drop at warn would hand a flood a log-amplification lever.
                if !self.raise_hand_limiter.allow() {
                    debug!(
                        "Rate-limiting RAISE_HAND from session {} (user {})",
                        self.id, self.user_id
                    );
                    return InboundAction::Processed;
                }
                // SECURITY: like the REACTION arm, this MUST forward ONLY the
                // output of `stamp_raise_hand_for_broadcast` — never the raw
                // inbound `data`. Attribution for a raised hand IS the feature; a
                // client-supplied envelope session_id would let a participant
                // raise a VICTIM's hand, cleartext, with no E2EE backstop. We
                // SHADOW `data` with the stamped `Option<Vec<u8>>` so the raw
                // `&[u8]` is no longer reachable in this scope and an accidental
                // passthrough will not compile.
                //
                // Fail-closed: a packet that will not re-serialize is dropped,
                // never fanned out unstamped.
                let data = stamp_raise_hand_for_broadcast(
                    data,
                    self.id,
                    RAISE_HAND_DISPLAY_NAME_MAX_BYTES,
                );
                match data {
                    Some(bytes) => InboundAction::Forward(Arc::new(bytes)),
                    None => InboundAction::Processed,
                }
            }
            PacketKind::MeetingTimer => {
                // WAITING-ROOM ISOLATION (#2136). An observer sits in the
                // waiting room and has NOT been admitted, so nothing it sends
                // may reach the meeting. EVERY other forwarding arm guards on
                // this (Data, Media, KeyframeRequest, Reaction, and — since
                // #2124 — Health), and this arm is no exception.
                //
                // It is also NOT redundant with the host gate downstream, even
                // though no observer should ever hold the host role. That
                // coupling is an emergent property of how tokens are minted
                // (a waiting-room token carries `observer = true`), not an
                // invariant anything enforces, and a feature whose isolation
                // depends on "the two flags can't both be set" is one policy
                // change away from breaking silently. Guard explicitly, and
                // guard FIRST so an unadmitted client's packet is discarded
                // before it costs a limiter slot or reaches the fan-out actor
                // at all.
                if self.observer {
                    trace!(
                        "Observer session {} dropping meeting-timer packet from {}",
                        self.id,
                        self.user_id
                    );
                    return InboundAction::Processed;
                }
                // Per-sender rate limit (#2136). Ingress validation (size cap +
                // parse + duration bound) already ran in `classify_packet` (an
                // invalid packet is `PacketKind::Dropped` and never reaches
                // here), so this meters ONLY well-formed packets against this
                // sender's window — a flood of garbage cannot consume the
                // budget.
                //
                // This runs BEFORE the host gate, because host-ness is not
                // knowable here (see `InboundAction::ForwardHostOnly`). A
                // non-host flood is therefore metered here and rejected at the
                // funnel: bounded to `MEETING_TIMER_MAX_PER_WINDOW` packets of
                // at most `MEETING_TIMER_PACKET_MAX_BYTES` per sender, none of
                // which reach another participant.
                //
                // A DROP HERE IS COSTLY, which is why the budget is generous
                // rather than tight: the relay keeps no timer registry, so a
                // dropped transition leaves the room's view wrong until the
                // host announces again. A dropped START self-repairs on the next
                // ~5s heartbeat; a dropped CANCEL has no heartbeat behind it and
                // relies on the client's transition repeat burst — which is
                // exactly why `MEETING_TIMER_MAX_PER_WINDOW` is sized to admit
                // that whole burst plus a heartbeat.
                //
                // `debug!` (not `warn!`), mirroring the REACTION arm: a
                // well-behaved host stays far below this ceiling, so a hit means
                // a misbehaving or forged client, and logging every drop at warn
                // would hand a flood a log-amplification lever.
                if !self.meeting_timer_limiter.allow() {
                    debug!(
                        "Rate-limiting MEETING_TIMER from session {} (user {})",
                        self.id, self.user_id
                    );
                    return InboundAction::Processed;
                }
                // No stamp function, unlike REACTION and RAISE_HAND — a
                // deliberate divergence, not an oversight. Those two carry a
                // bounded, attacker-controlled cosmetic `display_name` that must
                // be truncated before room-wide fan-out, and their whole feature
                // is per-sender ATTRIBUTION ("who reacted", "whose hand"), so
                // they re-stamp the envelope session_id themselves rather than
                // depend on an invariant enforced in another module.
                //
                // A MeetingTimerPacket has neither property. It contains no
                // string field at all, and a meeting timer is room-global state
                // ("5:00 remaining"), not a per-participant attribution — the
                // envelope session_id matters here only for the fan-out's
                // self-skip. Adding a stamp would mean a parse + reserialize on
                // every heartbeat for a guarantee `stamp_wrapper_for_broadcast`
                // (#2124) already makes unconditionally on this exact path, and
                // which is separately load-bearing for the host gate that
                // follows. Forwarding the validated bytes is correct here.
                InboundAction::ForwardHostOnly(Arc::new(data.to_vec()))
            }
            PacketKind::Data => {
                if self.observer {
                    trace!(
                        "Observer session {} dropping media packet from {}",
                        self.id,
                        self.user_id
                    );
                    return InboundAction::Processed;
                }

                InboundAction::Forward(Arc::new(data.to_vec()))
            }
            PacketKind::Media {
                media_kind,
                frame_kind,
            } => {
                if self.observer {
                    trace!(
                        "Observer session {} dropping media packet from {}",
                        self.id,
                        self.user_id
                    );
                    return InboundAction::Processed;
                }

                // Observe AFTER the observer guard so the metric counts only
                // publisher→relay frames that are actually forwarded to the room
                // — an observer/waiting-room session's inbound media is dropped
                // (never reaches receivers) and must not pollute the gap series.
                self.observe_publisher_inbound_frame_gap(media_kind, frame_kind);
                InboundAction::Forward(Arc::new(data.to_vec()))
            }
        }
    }

    /// Handle an outbound message from ChatServer (to be sent to client).
    ///
    /// Returns the bytes to send and tracks metrics.
    pub fn handle_outbound(&mut self, msg: &Message) -> Vec<u8> {
        RELAY_ROOM_BYTES_TOTAL
            .with_label_values(&[&self.room, "outbound"])
            .inc_by(msg.msg.len() as f64);
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_sent(self.id, msg.msg.len() as u64);

        // `msg.msg` is a shared `bytes::Bytes` (#1063): the single NATS payload
        // allocation is refcounted across all fan-out receivers. The
        // per-transport outbound channel (`Sender<Vec<u8>>` for WS, or
        // `Sender<Bytes>` for WT via `send_auto`) still needs owned bytes, so
        // materialize ONCE here per receiver — the same single copy that used
        // to live at the fan-out `Message` construction, just moved downstream.
        msg.msg.to_vec()
    }

    /// Clear this receiver's keyframe wait (#1297) for a frame the transport
    /// ACCEPTED for the socket: both transports call this only after the
    /// priority-drop pre-check and a successful enqueue on the outbound channel.
    pub fn observe_outbound_delivery(&mut self, msg: &Message) {
        if self.observer {
            return;
        }
        if let Some((target, kind)) = outbound_keyframe_observation(&msg.msg) {
            self.keyframe_limiter.observe_delivery(target, kind);
        }
    }

    // =========================================================================
    // Congestion Feedback
    // =========================================================================

    /// Record that an outbound packet from `sender_session_id` was dropped
    /// because the outbound channel to THIS receiver was full.
    ///
    /// ## #1219 (Half 1) — this no longer emits a sender-keyed CONGESTION signal
    ///
    /// This callback fires ONLY on receiver-DOWNLINK overflow: every caller
    /// (`ws_chat_session.rs` and `wt_chat_session.rs`, both the priority-drop
    /// preempt and the real channel-full branch) invokes it when the bounded
    /// outbound channel to this ONE receiver is saturated. There is NO
    /// server-side caller that fires for a publisher's OWN uplink distress.
    ///
    /// Historically this published a CONGESTION `PacketWrapper` keyed to the
    /// SENDER's session (subject `room.{room}.{sender_sid}`), which drove the
    /// publisher's client-side `force_congestion_cut` — the HARD 2-tier global
    /// encoder collapse (#702) for the WHOLE room. For a broadcast relay (NOT
    /// an SFU) that is the wrong response: a single slow receiver's full
    /// downlink channel is a per-RECEIVER problem and must never collapse the
    /// stream that every OTHER receiver is happily getting. Field evidence
    /// (#1217) showed 338–510 cuts/publisher, pinning publishers at 400kbps
    /// 166–273×, caused by exactly this path.
    ///
    /// The publisher's OWN uplink distress is instead detected entirely
    /// CLIENT-SIDE, by THREE complementary compensating signals in the
    /// encoders (`camera_encoder.rs` / `screen_encoder.rs`), each feeding the
    /// gentle single-rung `force_video_step_down` via
    /// `videocall_aq::constants::evaluate_self_congestion`:
    ///   1. WS: browser TCP send-buffer (`bufferedAmount`) drops via
    ///      `websocket::websocket_drop_count()` (#1178).
    ///   2. WT teardown: `webtransport::unistream_drop_count()` — increments
    ///      only on stream/connection TEARDOWN (STOP_SENDING / RESET_STREAM /
    ///      close), so it stays FLAT on a slow-but-alive uplink cliff (#1178).
    ///   3. WT saturation: `webtransport::unistream_ready_stall_count()` —
    ///      increments when `writer.ready().await` blocks past
    ///      `READY_STALL_THRESHOLD_MS` (250ms), gated by the videocall-aq
    ///      `WT_SATURATION_STALL_THRESHOLD` / `WT_SATURATION_WINDOW_MS`
    ///      constants. This is the ACTUAL WT bandwidth-cliff detector (#1219
    ///      prerequisite): signal #2 alone could never self-shed a saturated
    ///      WT uplink. This relay path is deliberately SUBTRACTED in favour of
    ///      those three.
    ///
    /// HALF-1 SCOPE / KNOWN GAP: this is the subtraction only. The
    /// receiver-scoped downlink-relief signal (the replacement that lets a
    /// slow receiver shed to a lower simulcast layer for ITSELF without
    /// touching the publisher's encode) is "Half 2", deferred — its consumer
    /// is the already-merged #1179 client chooser. Until Half 2 lands, a slow
    /// receiver's tile may freeze or degrade. That is ACCEPTABLE and strictly
    /// better than a room-wide collapse: only the congested receiver is
    /// affected, not every participant.
    ///
    /// We STILL call [`CongestionTracker::record_drop`] (and ignore its return
    /// value) because that is what updates `last_congestion`, which
    /// [`CongestionTracker::is_actively_congested`] reads to RELAX the
    /// KEYFRAME_REQUEST rate limiter (#979) so a congested receiver can recover
    /// its own frozen video faster. That is a per-receiver downlink response
    /// and is correct to keep; only the sender-keyed CONGESTION emit is removed.
    ///
    /// Record an outbound drop for this receiver and refresh the shared
    /// downlink-congestion epoch UNCONDITIONALLY (#1481).
    ///
    /// This callback is the GENUINE receiver-downlink backpressure surface: it
    /// fires only when the bounded `outbound_tx` channel to THIS one receiver
    /// overflows (a slow socket / parked event loop) — NOT the relay-side
    /// actor-mailbox `Full` (room-wide fan-out burst). The fan-out closure
    /// (`chat_server::handle_msg`) reads the epoch against
    /// [`RECEIVER_DOWNLINK_RELIEF_WINDOW`] to (a) shed non-base camera VIDEO
    /// layers — SCREEN is protected per issue 1977 (the shared content outranks
    /// cameras; its own relief is the priority_drop 90% fill backstop, one rung
    /// above camera VIDEO's 80%) — and (b) emit one DOWNLINK_CONGESTION packet so
    /// the client steps down. Recovery is automatic once the window elapses with
    /// no fresh drops.
    /// Receiver-only scope: this never touches the publisher's encoder.
    ///
    /// Every drop — regardless of whether the windowed `CongestionTracker` is
    /// above its threshold — refreshes the epoch. The relief window
    /// (`RECEIVER_DOWNLINK_RELIEF_WINDOW`) provides the natural decay: shedding
    /// turns off on its own once the window elapses with no fresh drops. The
    /// previous `is_actively_congested()` gate caused flapping on WebTransport:
    /// shed works → drops stop → tracker decays below threshold → gate closes →
    /// epoch ages → shed off → buffer refills → repeat (#1481).
    pub fn on_outbound_drop(&mut self, sender_session_id: u64, sender_user_id: &[u8]) {
        let crossed = self.congestion_tracker.record_drop(sender_session_id);

        // #1481: stamp on EVERY drop unconditionally. The relief window provides
        // decay — shedding turns off after RECEIVER_DOWNLINK_RELIEF_WINDOW with
        // no fresh drops. The is_actively_congested() gate caused the shed to
        // flap on WT where drops cluster then go quiet (shed works → drops stop
        // → gate closes → epoch decays → shed off → buffer refills → repeat).
        self.downlink_congested_epoch
            .store(downlink_congested_epoch_now(), Ordering::Relaxed);

        if let Some(sender_sid) = crossed {
            // #1219 (Half 1): intentionally do NOT publish a sender-keyed
            // CONGESTION signal here. See the doc comment above. `record_drop`
            // still ran (updating `last_congestion` for the #979 keyframe-relax
            // path); we only log for observability and drop the signal.
            warn!(
                "Receiver-downlink overflow: session {} dropping packets from sender {} (user: {}); \
                 CONGESTION cut SUPPRESSED (#1219 Half 1 — per-receiver downlink, not publisher uplink)",
                self.id, sender_sid, String::from_utf8_lossy(sender_user_id),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_inbound_action_debug() {
        let action = InboundAction::KeepAlive;
        assert_eq!(format!("{action:?}"), "KeepAlive");
    }

    #[test]
    fn test_congestion_tracker_cleans_stale_entries() {
        let mut tracker = CongestionTracker::new();

        // Insert a stale entry by manually inserting with an old window_start.
        let stale_id = 1000;
        tracker.senders.insert(
            stale_id,
            SenderDropState {
                drop_count: 0,
                // 20 seconds ago — well past the 10 * CONGESTION_WINDOW threshold
                window_start: Instant::now() - (CONGESTION_WINDOW * 20),
                last_notify: None,
            },
        );

        // Insert a fresh entry.
        let fresh_id = 2000;
        tracker.senders.insert(
            fresh_id,
            SenderDropState {
                drop_count: 0,
                window_start: Instant::now(),
                last_notify: None,
            },
        );

        assert_eq!(tracker.senders.len(), 2);

        // Set total_drops so the next record_drop triggers cleanup.
        tracker.total_drops = CLEANUP_INTERVAL - 1;

        // Recording a drop for a new sender should trigger cleanup.
        let trigger_id = 3000;
        tracker.record_drop(trigger_id);

        // The stale entry should have been removed.
        assert!(
            !tracker.senders.contains_key(&stale_id),
            "stale sender entry should be cleaned up"
        );
        // Fresh and trigger entries should remain.
        assert!(tracker.senders.contains_key(&fresh_id));
        assert!(tracker.senders.contains_key(&trigger_id));
    }

    #[test]
    fn test_congestion_tracker_retains_active_entries() {
        let mut tracker = CongestionTracker::new();

        // Record drops for two senders.
        tracker.record_drop(100);
        tracker.record_drop(200);

        assert_eq!(tracker.senders.len(), 2);

        // Record another drop — both entries are fresh, nothing should be cleaned.
        tracker.record_drop(100);

        assert_eq!(tracker.senders.len(), 2);
        assert!(tracker.senders.contains_key(&100));
        assert!(tracker.senders.contains_key(&200));
    }

    /// #1320: the senders map must be hard-bounded at MAX_TRACKED_SENDERS. A NEW
    /// sender beyond the cap (with no stale entries to evict) is refused so the
    /// map cannot grow unbounded; an ALREADY-tracked sender is never refused.
    ///
    /// Mutation coverage: removing the cap gate inserts `over_cap_sender`, growing
    /// the map to MAX+1 and failing the size/containment asserts. Dropping the
    /// `!contains_key` term (refusing existing senders too) makes the established
    /// sender's threshold-crossing return `None`, failing the final assert.
    #[test]
    fn test_congestion_tracker_bounds_senders_map_at_cap() {
        let mut tracker = CongestionTracker::new();
        let now = Instant::now();

        // Fill to the cap with FRESH (non-stale) entries so the stale sweep
        // cannot make room.
        for id in 0..MAX_TRACKED_SENDERS as u64 {
            tracker.senders.insert(
                id,
                SenderDropState {
                    drop_count: 0,
                    window_start: now,
                    last_notify: None,
                },
            );
        }
        assert_eq!(tracker.senders.len(), MAX_TRACKED_SENDERS);

        // A NEW sender beyond the cap must be REFUSED: no growth, returns None.
        let over_cap_sender = MAX_TRACKED_SENDERS as u64 + 1;
        assert_eq!(
            tracker.record_drop(over_cap_sender),
            None,
            "a new sender at the cap must not be admitted"
        );
        assert!(
            !tracker.senders.contains_key(&over_cap_sender),
            "the over-cap sender must not be inserted"
        );
        assert_eq!(
            tracker.senders.len(),
            MAX_TRACKED_SENDERS,
            "the map must not grow past MAX_TRACKED_SENDERS"
        );

        // An ALREADY-tracked sender is still recorded at the cap (never refused):
        // prove the drop lands by crossing the congestion threshold.
        let established = 0u64;
        tracker.senders.get_mut(&established).unwrap().drop_count = CONGESTION_DROP_THRESHOLD - 1;
        assert_eq!(
            tracker.record_drop(established),
            Some(established),
            "an already-tracked sender must keep being recorded at the cap"
        );
        assert_eq!(tracker.senders.len(), MAX_TRACKED_SENDERS);
    }

    /// #1349: the cap-pressure forced sweep must NOT run on every at-cap drop.
    /// Under a flood of distinct NEW (publisher-forgeable) `sender_session_id`s
    /// against an already-full receiver, the forced `retain()` is gated to the
    /// amortized `CLEANUP_INTERVAL` cadence so the steady-state at-cap drop is
    /// O(1) — never an O(n) sweep per packet.
    ///
    /// Mutation coverage: if the `due_for_sweep` gate is removed (sweep runs
    /// unconditionally, the pre-#1349 behavior), `forced_sweep_count` would equal
    /// the number of at-cap new-sender drops and the `<= 1` assert below fails.
    /// The map-bound asserts also pin the #1320 invariant the gate must preserve.
    #[test]
    fn test_congestion_tracker_forced_sweep_gated_at_cap() {
        let mut tracker = CongestionTracker::new();
        let now = Instant::now();

        // Fill to the cap with FRESH (non-stale) entries so the stale sweep can
        // never reclaim a slot: every new sender below is genuinely refused.
        for id in 0..MAX_TRACKED_SENDERS as u64 {
            tracker.senders.insert(
                id,
                SenderDropState {
                    drop_count: 0,
                    window_start: now,
                    last_notify: None,
                },
            );
        }
        assert_eq!(tracker.senders.len(), MAX_TRACKED_SENDERS);
        assert_eq!(tracker.forced_sweep_count, 0);

        // Flood the saturated receiver with a burst of DISTINCT new senders that
        // stays strictly below CLEANUP_INTERVAL. With the #1349 gate the forced
        // sweep fires at most once across the whole burst; without it (sweep per
        // packet) it would fire `burst` times.
        let burst = CLEANUP_INTERVAL - 1;
        let base = MAX_TRACKED_SENDERS as u64;
        for i in 0..burst as u64 {
            assert_eq!(
                tracker.record_drop(base + i),
                None,
                "an over-cap new sender must always be refused"
            );
        }

        assert!(
            tracker.forced_sweep_count <= 1,
            "forced sweep ran {} times over {} at-cap drops; it must be gated to \
             the CLEANUP_INTERVAL cadence (<= 1), not run per packet",
            tracker.forced_sweep_count,
            burst
        );
        // The bound the gate must preserve: the map never grew past the cap and
        // no over-cap sender was admitted.
        assert_eq!(
            tracker.senders.len(),
            MAX_TRACKED_SENDERS,
            "the map must remain bounded at MAX_TRACKED_SENDERS under flood"
        );
    }

    // =====================================================================
    // Active-congestion flag for relaxed keyframe budget (issue #979)
    // =====================================================================

    #[test]
    fn test_is_actively_congested_false_before_any_threshold_cross() {
        let mut tracker = CongestionTracker::new();
        assert!(
            !tracker.is_actively_congested(),
            "a tracker with no drops must not report active congestion"
        );
        // A few drops below the threshold must not flip the flag.
        for _ in 0..(CONGESTION_DROP_THRESHOLD - 1) {
            tracker.record_drop(1);
        }
        assert!(
            !tracker.is_actively_congested(),
            "sub-threshold drops must not flag active congestion"
        );
    }

    #[test]
    fn test_is_actively_congested_true_after_threshold_cross() {
        let mut tracker = CongestionTracker::new();
        // Cross the threshold so `record_drop` returns `Some` (threshold
        // crossing). Since #1219 this no longer emits a notification.
        let mut crossed = false;
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            crossed |= tracker.record_drop(1).is_some();
        }
        assert!(crossed, "threshold cross must return Some");
        assert!(
            tracker.is_actively_congested(),
            "tracker must report active congestion right after a threshold cross"
        );
    }

    #[test]
    fn test_is_actively_congested_expires_after_relax_window() {
        let mut tracker = CongestionTracker::new();
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            tracker.record_drop(1);
        }
        assert!(tracker.is_actively_congested());

        // Rewind the last_congestion timestamp past the relax window.
        tracker.last_congestion =
            Some(Instant::now() - (KEYFRAME_CONGESTION_RELAX_WINDOW + Duration::from_millis(50)));
        assert!(
            !tracker.is_actively_congested(),
            "active congestion must lapse once the relax window elapses"
        );
    }

    // =====================================================================
    // Drop recording and counting
    // =====================================================================

    #[test]
    fn test_drop_recording_increments_count() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 42;

        // Record a single drop — should not yet trigger notification.
        let result = tracker.record_drop(sender_id);
        assert!(
            result.is_none(),
            "single drop should not trigger notification"
        );

        // The internal count should be 1.
        let state = tracker.senders.get(&sender_id).unwrap();
        assert_eq!(state.drop_count, 1);
    }

    #[test]
    fn test_drop_recording_multiple_senders_independent() {
        let mut tracker = CongestionTracker::new();

        // Record drops for two different senders.
        for _ in 0..3 {
            tracker.record_drop(100);
        }
        for _ in 0..2 {
            tracker.record_drop(200);
        }

        // Each sender should have independent counts.
        assert_eq!(tracker.senders.get(&100).unwrap().drop_count, 3);
        assert_eq!(tracker.senders.get(&200).unwrap().drop_count, 2);
    }

    #[test]
    fn test_drop_window_resets_after_expiry() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 50;

        // Manually insert a sender with a window that started in the past
        // (just beyond CONGESTION_WINDOW) so the next record_drop resets it.
        tracker.senders.insert(
            sender_id,
            SenderDropState {
                drop_count: 3,
                window_start: Instant::now() - (CONGESTION_WINDOW + Duration::from_millis(10)),
                last_notify: None,
            },
        );

        // record_drop should reset the window and set count to 1 (not 4).
        tracker.record_drop(sender_id);
        let state = tracker.senders.get(&sender_id).unwrap();
        assert_eq!(
            state.drop_count, 1,
            "drop count should reset to 1 after window expiry"
        );
    }

    // =====================================================================
    // Congestion notification triggering
    // =====================================================================

    #[test]
    fn test_notification_triggers_at_threshold() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 99;

        // Record drops up to one less than threshold — no notification.
        for _ in 0..(CONGESTION_DROP_THRESHOLD - 1) {
            let result = tracker.record_drop(sender_id);
            assert!(result.is_none());
        }

        // The threshold-th drop should trigger a notification.
        let result = tracker.record_drop(sender_id);
        assert_eq!(
            result,
            Some(sender_id),
            "should return sender_id when threshold is reached"
        );
    }

    #[test]
    fn test_notification_resets_count_after_trigger() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 77;

        // Reach threshold to trigger notification.
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            tracker.record_drop(sender_id);
        }

        // After triggering, count should be reset to 0.
        let state = tracker.senders.get(&sender_id).unwrap();
        assert_eq!(
            state.drop_count, 0,
            "drop count should reset after notification"
        );
    }

    #[test]
    fn test_rate_limiting_suppresses_rapid_notifications() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 55;

        // First burst: trigger notification.
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            tracker.record_drop(sender_id);
        }
        // The last call above returned Some(55). Now the last_notify is set.

        // Second burst immediately after: should be rate-limited because
        // CONGESTION_NOTIFY_MIN_INTERVAL has not elapsed.
        for i in 0..CONGESTION_DROP_THRESHOLD {
            let result = tracker.record_drop(sender_id);
            if i < CONGESTION_DROP_THRESHOLD - 1 {
                // Below threshold — always None.
                assert!(result.is_none());
            } else {
                // At threshold — rate-limited, so still None.
                assert!(
                    result.is_none(),
                    "notification should be suppressed by rate limiter"
                );
            }
        }
    }

    // =====================================================================
    // Stale entry cleanup
    // =====================================================================

    #[test]
    fn test_stale_cleanup_removes_multiple_stale_entries() {
        let mut tracker = CongestionTracker::new();

        // Insert several stale entries.
        for id in 1..=5 {
            tracker.senders.insert(
                id,
                SenderDropState {
                    drop_count: 0,
                    window_start: Instant::now() - (CONGESTION_WINDOW * 20),
                    last_notify: None,
                },
            );
        }

        // Insert one fresh entry.
        tracker.senders.insert(
            100,
            SenderDropState {
                drop_count: 0,
                window_start: Instant::now(),
                last_notify: None,
            },
        );

        assert_eq!(tracker.senders.len(), 6);

        // Set total_drops so the next record_drop triggers cleanup.
        tracker.total_drops = CLEANUP_INTERVAL - 1;

        // Trigger cleanup by recording a drop.
        tracker.record_drop(200);

        // All stale entries (1-5) should be gone; fresh (100) and new (200) remain.
        assert_eq!(tracker.senders.len(), 2);
        assert!(tracker.senders.contains_key(&100));
        assert!(tracker.senders.contains_key(&200));
    }

    #[test]
    fn test_entry_just_under_boundary_is_retained() {
        let mut tracker = CongestionTracker::new();

        // Insert an entry slightly under the stale boundary (10 * CONGESTION_WINDOW).
        // Use a 500ms margin to account for time elapsed between insertion and
        // the `retain` call inside `record_drop`.
        tracker.senders.insert(
            1,
            SenderDropState {
                drop_count: 2,
                window_start: Instant::now() - (CONGESTION_WINDOW * 10)
                    + Duration::from_millis(500),
                last_notify: None,
            },
        );

        // Set total_drops so the next record_drop triggers cleanup.
        tracker.total_drops = CLEANUP_INTERVAL - 1;

        tracker.record_drop(2);

        // Entry 1 is within the boundary — should be retained.
        assert!(
            tracker.senders.contains_key(&1),
            "entry just under stale boundary should be retained"
        );
    }

    // =====================================================================
    // should_notify_sender() — tested indirectly through record_drop
    // =====================================================================

    #[test]
    fn test_first_notification_for_sender_has_no_rate_limit() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 10;

        // First time reaching threshold — no prior last_notify, should fire.
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            tracker.record_drop(sender_id);
        }

        // Verify last_notify was set.
        let state = tracker.senders.get(&sender_id).unwrap();
        assert!(
            state.last_notify.is_some(),
            "last_notify should be set after first notification"
        );
    }

    #[test]
    fn test_notification_allowed_after_rate_limit_expires() {
        let mut tracker = CongestionTracker::new();
        let sender_id = 30;

        // Simulate a previous notification that happened long enough ago
        // that the rate limit has expired.
        tracker.senders.insert(
            sender_id,
            SenderDropState {
                drop_count: 0,
                window_start: Instant::now(),
                last_notify: Some(
                    Instant::now() - CONGESTION_NOTIFY_MIN_INTERVAL - Duration::from_millis(10),
                ),
            },
        );

        // Record enough drops to hit threshold.
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            tracker.record_drop(sender_id);
        }

        // Should trigger because rate limit has expired.
        // The last record_drop was the threshold-th, which was the one that returned.
        // We need to check the return value of the last call.
        // Let's redo this more carefully.
        let mut tracker2 = CongestionTracker::new();
        tracker2.senders.insert(
            sender_id,
            SenderDropState {
                drop_count: 0,
                window_start: Instant::now(),
                last_notify: Some(
                    Instant::now() - CONGESTION_NOTIFY_MIN_INTERVAL - Duration::from_millis(10),
                ),
            },
        );

        let mut triggered = false;
        for _ in 0..CONGESTION_DROP_THRESHOLD {
            if tracker2.record_drop(sender_id).is_some() {
                triggered = true;
            }
        }
        assert!(
            triggered,
            "notification should fire after rate-limit window expires"
        );
    }

    #[test]
    fn test_default_trait_impl() {
        // Verify Default trait works and produces an empty tracker.
        let tracker = CongestionTracker::default();
        assert!(tracker.senders.is_empty());
    }

    #[test]
    fn test_should_activate_on_action() {
        // Echo (RTT probe) should NOT activate.
        assert!(!SessionLogic::should_activate_on_action(
            &InboundAction::Echo(Arc::new(vec![]))
        ));
        // Forward, ForwardHostOnly, Processed, KeepAlive should activate.
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::Forward(Arc::new(vec![]))
        ));
        // #2136: `ForwardHostOnly` must activate exactly like `Forward`. The
        // predicate is written as "everything except Echo", so a new variant
        // inherits the right answer silently — this asserts that the silence was
        // CORRECT rather than merely unnoticed. If it did not activate, the
        // host's very first timer packet would be swallowed by the
        // `ConnectionState::Testing` gate in `Handler<ClientMessage>`.
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::ForwardHostOnly(Arc::new(vec![]))
        ));
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::Processed
        ));
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::KeepAlive
        ));
    }

    /// Issue 2047: pins WHICH session field feeds WHICH telemetry identity label.
    ///
    /// The three values are all strings-or-ids of the same shape, so a
    /// transposition (`room` into `user_id`, say) compiles cleanly and would
    /// relabel every dashboard while every stamping test stayed green — those
    /// tests hand-build their own reporter and never see this mapping.
    ///
    /// This calls the production mapping used by
    /// [`SessionLogic::authenticated_reporter`]; it does NOT re-implement it.
    ///
    /// MUTATION PROOF: swap the `room` and `user_id` arms of
    /// `reporter_from_session_fields` and the first two asserts fail.
    ///
    /// COVERAGE LIMIT (stated so no one over-reads this test): it pins the field
    /// MAPPING, not the CALL SITE. Proving `handle_inbound` passes session state
    /// rather than packet state needs a real `SessionLogic`, which needs a live
    /// NATS connection — that is
    /// `health_packet_publishes_under_session_identity_not_claimed_identity`
    /// below, which SKIPS when no broker is reachable.
    #[test]
    fn authenticated_reporter_maps_session_fields_to_telemetry_labels() {
        let reporter =
            SessionLogic::reporter_from_session_fields("room-alpha", 4242, "alice@example.com");

        assert_eq!(
            reporter.meeting_id, "room-alpha",
            "the session's room must become the telemetry meeting_id"
        );
        assert_eq!(
            reporter.user_id, "alice@example.com",
            "the session's user_id must become the telemetry reporting user"
        );
        assert_eq!(
            reporter.session_id, 4242,
            "the relay-assigned session id must be carried verbatim"
        );
    }

    /// #1699 Phase 1: publisher-leg keyframe-arrival instrumentation must be
    /// wired through the REAL production path — `handle_inbound` classifying a
    /// MEDIA `PacketWrapper` as `PacketKind::Media` and calling the per-session
    /// observer. This drives two real VIDEO `PacketWrapper`s (a delta then a
    /// keyframe) through `SessionLogic::handle_inbound` and asserts one
    /// inter-arrival sample landed on the arriving keyframe's `{websocket, video,
    /// key}` label set, AND that both were still forwarded (behavior neutrality).
    ///
    /// MUTATION PROOF: removing the `PacketKind::Media` arm's
    /// `self.observe_publisher_inbound_frame_gap(...)` call in `handle_inbound`
    /// (or deleting the `.observe(...)` in `PublisherInboundFrameGapTracker`)
    /// leaves the sample count flat and fails the `after > before` assert.
    /// Because it goes through `classify_packet`, it also guards the new
    /// `PacketKind::Media` classification itself.
    #[actix_rt::test]
    #[serial_test::serial(relay_publisher_inbound_frame_gap)]
    async fn handle_inbound_media_frame_observes_publisher_gap_and_forwards() {
        use protobuf::Message as _;
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::media_packet::MediaPacket;
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        // Build a real MEDIA PacketWrapper: outer media_kind = VIDEO (cleartext),
        // inner MediaPacket.frame_type distinguishes delta vs key.
        let media_bytes = |frame_type: &str| -> Vec<u8> {
            let inner = MediaPacket {
                media_type: MediaType::VIDEO.into(),
                frame_type: frame_type.to_string(),
                ..Default::default()
            };
            let wrapper = PacketWrapper {
                packet_type: PacketType::MEDIA.into(),
                media_kind: MediaKind::VIDEO.into(),
                data: inner.write_to_bytes().expect("inner encode"),
                ..Default::default()
            };
            wrapper.write_to_bytes().expect("wrapper encode")
        };

        let mut logic = build_test_receiver_logic(nats_client, "room-1699-inbound").await;

        let labels = ["websocket", "video", "key"];
        let before = RELAY_PUBLISHER_INBOUND_FRAME_GAP_MS
            .with_label_values(&labels)
            .get_sample_count();

        // First VIDEO frame (delta): seeds the tracker's last-arrival, no gap yet.
        // It must still FORWARD (behavior neutrality — same as the old Data arm).
        let delta = media_bytes("delta");
        assert!(
            matches!(logic.handle_inbound(&delta), InboundAction::Forward(_)),
            "a regular MEDIA frame must still be forwarded (behavior-neutral)"
        );

        // Ensure a measurable gap, then a VIDEO keyframe: this produces exactly one
        // inter-arrival sample on {websocket, video, key} and must also forward.
        std::thread::sleep(Duration::from_millis(1));
        let key = media_bytes("key");
        assert!(
            matches!(logic.handle_inbound(&key), InboundAction::Forward(_)),
            "a keyframe MEDIA frame must still be forwarded (behavior-neutral)"
        );

        let after = RELAY_PUBLISHER_INBOUND_FRAME_GAP_MS
            .with_label_values(&labels)
            .get_sample_count();
        assert!(
            after > before,
            "handle_inbound must observe one keyframe-arrival gap sample for \
             {labels:?} via the PacketKind::Media path (before={before}, after={after})"
        );
    }

    /// #2394 CALL-SITE proof. Requires NATS (constructing `SessionLogic` needs a
    /// live client); SKIPS when the broker is unreachable, like its neighbours.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn handle_inbound_keyframe_request_records_outcome_metric() {
        use protobuf::Message as _;
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::media_packet::MediaPacket;
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let inner = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            user_id: b"target-user-2394".to_vec(),
            target_session_id: 909_090,
            data: b"VIDEO".to_vec(),
            ..Default::default()
        };
        let request = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: inner.write_to_bytes().expect("inner encode"),
            ..Default::default()
        }
        .write_to_bytes()
        .expect("wrapper encode");

        let room = "room-2394-keyframe-outcome";
        let admitted = [room, "video", "admitted_strict"];
        let denied = [room, "video", "denied_still_waiting"];
        let before_admitted = RELAY_KEYFRAME_REQUESTS_TOTAL
            .with_label_values(&admitted)
            .get();
        let before_denied = RELAY_KEYFRAME_REQUESTS_TOTAL
            .with_label_values(&denied)
            .get();

        let mut logic = build_test_receiver_logic(nats_client, room).await;

        assert!(
            matches!(logic.handle_inbound(&request), InboundAction::Forward(_)),
            "the first KEYFRAME_REQUEST must be forwarded"
        );
        assert!(
            matches!(logic.handle_inbound(&request), InboundAction::Processed),
            "the immediate retry must be rate-limited, not forwarded"
        );

        assert_eq!(
            RELAY_KEYFRAME_REQUESTS_TOTAL
                .with_label_values(&admitted)
                .get()
                - before_admitted,
            1.0,
            "handle_inbound must record the admitted request on \
             relay_keyframe_requests_total{{room, kind=video, outcome=admitted_strict}} (#2394)"
        );
        assert_eq!(
            RELAY_KEYFRAME_REQUESTS_TOTAL
                .with_label_values(&denied)
                .get()
                - before_denied,
            1.0,
            "the frozen-receiver denial must be recorded as denied_still_waiting, the outcome \
             that distinguishes it from a correct denied_budget (#2394)"
        );
    }

    // =====================================================================
    // #1219 — receiver-downlink overflow must NOT emit sender-keyed CONGESTION
    // =====================================================================
    //
    // These tests need NATS (they subscribe to the sender's self-subject to
    // prove no CONGESTION is published). They are `#[serial]` + `#[actix_rt::test]`
    // to match the other NATS-backed tests in this crate.

    /// Build a `SessionLogic` for a receiver in `room` over `nats_client`.
    /// (Test helper — mirrors the construction in `chat_server.rs` tests but
    /// needs no DB pool.)
    #[cfg(test)]
    async fn build_test_receiver_logic(
        nats_client: async_nats::client::Client,
        room: &str,
    ) -> SessionLogic {
        build_test_logic(nats_client, room, false).await
    }

    /// Same, with the waiting-room `observer` flag under the caller's control.
    /// The flag is the LAST positional `bool`-heavy argument of
    /// `SessionLogic::new`'s middle block, so it is passed here by name rather
    /// than duplicating the whole constructor at each call site.
    #[cfg(test)]
    async fn build_test_logic(
        nats_client: async_nats::client::Client,
        room: &str,
        observer: bool,
    ) -> SessionLogic {
        use crate::actors::chat_server::ChatServer;
        use crate::server_diagnostics::{TrackerMessage, TrackerSender};
        use actix::Actor;
        use tokio::sync::mpsc;

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let (tx, _rx) = mpsc::unbounded_channel::<TrackerMessage>();
        let tracker_sender: TrackerSender = tx;
        SessionLogic::new(
            chat_server,
            room.to_string(),
            "receiver-user".to_string(),
            "receiver-user".to_string(),
            false,
            nats_client,
            tracker_sender,
            SessionManager::new(),
            observer,
            None,
            "websocket",
            false,
            false,
        )
    }

    const KEYFRAME_2393_PUBLISHER: u64 = 555;

    async fn connect_test_nats() -> Option<async_nats::client::Client> {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        match async_nats::connect(&nats_url).await {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                None
            }
        }
    }

    /// The sleep lets `ChatServer::started` run inside the runtime — its
    /// `tokio::spawn` panics without a reactor.
    async fn build_receiver(nats_client: async_nats::client::Client, room: &str) -> SessionLogic {
        let logic = build_test_receiver_logic(nats_client, room).await;
        actix_rt::time::sleep(Duration::from_millis(20)).await;
        logic
    }

    fn keyframe_delivery_message() -> Message {
        use bytes::Bytes;
        use protobuf::Message as _;
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::media_packet::MediaPacket;
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let inner = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            frame_type: "key".to_string(),
            data: vec![0xABu8; 256],
            ..Default::default()
        };
        let raw = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            session_id: KEYFRAME_2393_PUBLISHER,
            media_kind: MediaKind::VIDEO.into(),
            data: inner
                .write_to_bytes()
                .expect("inner MediaPacket serializes"),
            ..Default::default()
        }
        .write_to_bytes()
        .expect("PacketWrapper serializes");
        Message {
            session: 1,
            msg: Bytes::from(raw),
        }
    }

    fn classify_request(logic: &mut SessionLogic) -> KeyframeRequestOutcome {
        logic.keyframe_limiter.classify_with_congestion(
            KeyframeTarget::Session(KEYFRAME_2393_PUBLISHER),
            KeyframeMediaKind::Video,
            0,
            false,
        )
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn test_handle_outbound_alone_does_not_clear_the_keyframe_wait() {
        let nats_client = match connect_test_nats().await {
            Some(c) => c,
            None => return,
        };
        let mut logic = build_receiver(nats_client, "keyframe_2393_handoff").await;

        assert_eq!(
            classify_request(&mut logic),
            KeyframeRequestOutcome::AdmittedStrict,
            "the first request must be admitted by the strict budget and arm the wait"
        );

        let _bytes = logic.handle_outbound(&keyframe_delivery_message());

        assert_eq!(
            classify_request(&mut logic),
            KeyframeRequestOutcome::DeniedStillWaiting,
            "hand-off alone must leave the wait armed — clearing it here strands a \
             receiver whose keyframe was dropped before the socket"
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn test_observe_outbound_delivery_clears_the_keyframe_wait() {
        let nats_client = match connect_test_nats().await {
            Some(c) => c,
            None => return,
        };
        let mut logic = build_receiver(nats_client, "keyframe_2393_delivered").await;

        assert_eq!(
            classify_request(&mut logic),
            KeyframeRequestOutcome::AdmittedStrict,
            "the first request must be admitted by the strict budget and arm the wait"
        );

        let msg = keyframe_delivery_message();
        let _bytes = logic.handle_outbound(&msg);
        logic.observe_outbound_delivery(&msg);

        assert_eq!(
            classify_request(&mut logic),
            KeyframeRequestOutcome::DeniedBudget,
            "an accepted keyframe must disarm the wait so the strict budget re-engages"
        );
    }

    /// #1219 (Half 1): when the relay drops outbound packets to ONE receiver
    /// (receiver-downlink overflow) past the congestion threshold,
    /// `on_outbound_drop` must NOT publish a sender-keyed CONGESTION packet to
    /// the sender's self-subject. (Before #1219 it did — driving the publisher's
    /// whole-room `force_congestion_cut`.) We subscribe to the sender's subject
    /// and assert SILENCE.
    ///
    /// MUTATION PROOF: reverting #1219 (restoring the `nc.publish(subject, ..)`
    /// of the CONGESTION packet in `on_outbound_drop`) makes a CONGESTION arrive
    /// on the subscription, so `received` becomes 1 and the `== 0` assert FAILS.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn test_on_outbound_drop_does_not_emit_sender_keyed_congestion() {
        use futures::StreamExt;

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let room = "congestion_1219_room";
        let sender_sid: u64 = 424242;
        // The relay publishes CONGESTION on `room.{room}.{sender_sid}`.
        let subject = format!("room.{room}.{sender_sid}");
        let mut sub = nats_client
            .subscribe(subject.clone())
            .await
            .expect("subscribe should succeed");
        // Ensure the subscription is registered server-side before we drive drops.
        nats_client.flush().await.expect("flush should succeed");

        let mut logic = build_test_receiver_logic(nats_client.clone(), room).await;

        // Drive enough drops to cross the threshold MULTIPLE times (and past the
        // rate-limit interval would still only ever publish, never not-publish).
        // record_drop returns Some at the threshold; on_outbound_drop used to
        // publish on that. We call well past threshold to be unambiguous.
        let sender_user_id = b"sender-user";
        for _ in 0..(CONGESTION_DROP_THRESHOLD * 3) {
            logic.on_outbound_drop(sender_sid, sender_user_id);
        }

        // The surviving #979 behavior: the receiver IS now actively congested
        // (record_drop still ran and crossed the threshold). This proves we did
        // not gut record_drop — only the emit.
        assert!(
            logic.congestion_tracker.is_actively_congested(),
            "#979 keyframe-relax path must survive: record_drop still flags active congestion"
        );

        // #1481: after 15 drops the epoch must be stamped (both gated and
        // ungated code would stamp here since the tracker IS congested).
        assert_ne!(
            logic.downlink_congested_epoch.load(Ordering::Relaxed),
            DOWNLINK_EPOCH_NEVER,
            "#1481: on_outbound_drop must stamp the epoch after drops"
        );

        // Allow any (erroneously) spawned publish task to land on the wire.
        let mut received = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, sub.next()).await {
                Ok(Some(_msg)) => received += 1,
                _ => break,
            }
        }

        assert_eq!(
            received, 0,
            "#1219 Half 1: receiver-downlink overflow must NOT publish a \
             sender-keyed CONGESTION packet (got {received} on {subject})"
        );
    }

    // =====================================================================
    // #1481 — downlink-epoch stamp is now UNCONDITIONAL on every drop
    // =====================================================================

    /// THE regression test for #1481: a SINGLE sub-threshold drop through the
    /// real `on_outbound_drop` must stamp the epoch. On the old gated code
    /// (where `stamp_downlink_epoch_if_congested` required
    /// `is_actively_congested() == true`), 1 drop does NOT cross the threshold
    /// (needs 5), so the epoch stays NEVER → this assert FAILS. On the fixed
    /// code it stamps unconditionally → passes.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn epoch_stamps_on_single_sub_threshold_drop() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let mut logic = build_test_receiver_logic(nats_client.clone(), "epoch_1481_room").await;

        // ONE drop — below CONGESTION_DROP_THRESHOLD (5).
        logic.on_outbound_drop(9999, b"some-sender");

        // Premise: the tracker must NOT be congested after a single drop.
        assert!(
            !logic.congestion_tracker.is_actively_congested(),
            "test premise: a single drop must NOT cross the threshold"
        );

        // The fix: epoch is stamped DESPITE being below threshold.
        assert_ne!(
            logic.downlink_congested_epoch.load(Ordering::Relaxed),
            DOWNLINK_EPOCH_NEVER,
            "#1481: a single sub-threshold drop must still stamp the epoch \
             (fails on gated code, passes on unconditional stamp)"
        );
    }

    // =====================================================================
    // Issue 2047 — the WIRING test: what `handle_inbound` actually publishes
    // =====================================================================

    /// End-to-end proof that a forged HEALTH packet is published to NATS under
    /// THIS SESSION's identity — driven through the real production path
    /// (`handle_inbound` -> `process_health_packet_bytes` ->
    /// `build_health_payload_for_publish`) and asserted on the bytes that land on
    /// the health subject.
    ///
    /// This is the one test that covers the CALL SITE rather than the stamping
    /// function. Every other issue-2047 test hand-builds an
    /// `AuthenticatedReporter`, so replacing
    /// `self.authenticated_reporter()` in the HEALTH arm with a struct literal
    /// fed from the parsed packet would leave them all green — and reintroduce
    /// the vulnerability. Only this test observes which identity the relay
    /// actually chose.
    ///
    /// MUTATION PROOF: build the reporter at the `PacketKind::Health` call site
    /// from the inbound packet's own `meeting_id`/`session_id`/
    /// `reporting_user_id`; the three "authenticated" asserts below fail because
    /// the forged values reach NATS.
    ///
    /// Requires a broker (it subscribes to the health subject) and SKIPS when
    /// none is reachable, like the #1219 tests above.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn health_packet_publishes_under_session_identity_not_claimed_identity() {
        use futures::StreamExt;
        use protobuf::Message as _;
        use videocall_types::protos::health_packet::HealthPacket as PbHealthPacket;
        use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        // The relay publishes health on
        // `health.diagnostics.{REGION}.{SERVICE_TYPE}.{SERVER_ID}`; subscribe to
        // the whole tree so this does not depend on the env defaults.
        let mut sub = nats_client
            .subscribe("health.diagnostics.>")
            .await
            .expect("subscribe should succeed");
        nats_client.flush().await.expect("flush should succeed");

        let room = "identity_2047_room";
        let logic = build_test_receiver_logic(nats_client.clone(), room).await;
        let authenticated_session = logic.id;

        // A health packet claiming a DIFFERENT meeting, session and user.
        let mut hp = PbHealthPacket::new();
        hp.meeting_id = "victim-boardroom".to_string();
        hp.session_id = "9999999999999999999".to_string();
        hp.reporting_user_id = b"ceo@example.com".to_vec();
        hp.timestamp_ms = 1_700_000_000_000;
        let mut wrapper = PacketWrapper::new();
        wrapper.packet_type = PacketType::HEALTH.into();
        wrapper.data = hp.write_to_bytes().expect("serialize inner health packet");
        let bytes = wrapper.write_to_bytes().expect("serialize health wrapper");

        // Drive the REAL inbound path.
        let mut logic = logic;
        let action = logic.handle_inbound(&bytes);
        assert!(
            matches!(action, InboundAction::Forward(_)),
            "HEALTH must still be forwarded to peers (behavior neutrality)"
        );

        // The publish is spawned, so wait for it with a bound.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .expect("health publish should arrive within 5s")
            .expect("subscription should yield a message");

        let published =
            PbHealthPacket::parse_from_bytes(&msg.payload).expect("published payload must parse");

        assert_eq!(
            published.meeting_id, room,
            "meeting_id must be the session's authenticated room, not the claimed one"
        );
        assert_eq!(
            published.session_id,
            authenticated_session.to_string(),
            "session_id must be the relay-assigned session, not the claimed one"
        );
        assert_eq!(
            published.reporting_user_id,
            b"receiver-user".to_vec(),
            "reporting_user_id must be the session's authenticated user, not the claimed one"
        );
    }

    /// #2095 review (MEDIUM): a waiting-room OBSERVER's HEALTH must not escape
    /// the waiting room — not to peers, and not to the server-side telemetry.
    ///
    /// Before this guard the HEALTH arm was the only `Forward` arm with no
    /// `self.observer` check, and #2095 made that omission load-bearing: the
    /// relay now stamps the fan-out envelope with the sender's AUTHENTICATED
    /// `sub`, so an unadmitted observer's HEALTH disclosed its server-side
    /// identity to every participant (an identity PARTICIPANT_JOINED does not
    /// broadcast for observers), and — HEALTH not being in the client's
    /// `suppresses_peer_creation_for_packet` set — called `ensure_peer` in every
    /// participant's browser, minting a tile and its decoder Workers.
    ///
    /// Both halves are asserted:
    ///   1. the action is `Processed`, i.e. nothing is fanned out;
    ///   2. nothing lands on `health.diagnostics.>`, i.e. the guard sits BEFORE
    ///      `process_health_packet_bytes` and an unadmitted client cannot write
    ///      operator dashboards either.
    ///
    /// A NON-observer session is then driven through the SAME code with the SAME
    /// bytes and must still Forward AND publish. Without that control, deleting
    /// the whole HEALTH arm would pass this test.
    ///
    /// MUTATION PROOF: remove the `if self.observer { .. }` guard from the
    /// `PacketKind::Health` arm and the observer's packet becomes
    /// `Forward(_)` -> assert 1 fails, and its telemetry lands -> assert 2 fails.
    /// Move the guard to AFTER `process_health_packet_bytes` and assert 2 alone
    /// fails.
    ///
    /// Requires a broker (a `SessionLogic` needs a real `ChatServer`) and SKIPS
    /// when none is reachable, like the sibling above.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn observer_health_packet_is_neither_forwarded_nor_published() {
        use futures::StreamExt;
        use protobuf::Message as _;
        use videocall_types::protos::health_packet::HealthPacket as PbHealthPacket;
        use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let mut sub = nats_client
            .subscribe("health.diagnostics.>")
            .await
            .expect("subscribe should succeed");
        nats_client.flush().await.expect("flush should succeed");

        let mut hp = PbHealthPacket::new();
        hp.client_cores = Some(8);
        hp.timestamp_ms = 1_700_000_000_000;
        let mut wrapper = PacketWrapper::new();
        wrapper.packet_type = PacketType::HEALTH.into();
        wrapper.data = hp.write_to_bytes().expect("serialize inner health packet");
        let bytes = wrapper.write_to_bytes().expect("serialize health wrapper");

        // --- the observer: dropped on both legs ---------------------------------
        let mut observer_logic =
            build_test_logic(nats_client.clone(), "observer_2095_room", true).await;
        assert!(
            matches!(
                observer_logic.handle_inbound(&bytes),
                InboundAction::Processed
            ),
            "an observer's HEALTH must be consumed, never fanned out to the meeting"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(750), sub.next())
                .await
                .is_err(),
            "an observer's HEALTH must not reach the server-side telemetry either: \
             the guard belongs BEFORE process_health_packet_bytes"
        );

        // --- the admitted participant: unchanged (control) ----------------------
        let mut member_logic =
            build_test_logic(nats_client.clone(), "observer_2095_room", false).await;
        assert!(
            matches!(
                member_logic.handle_inbound(&bytes),
                InboundAction::Forward(_)
            ),
            "control: an admitted participant's HEALTH must still be forwarded"
        );
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .expect("control: a participant's health publish should arrive within 5s")
            .expect("control: subscription should yield a message");
        let published =
            PbHealthPacket::parse_from_bytes(&msg.payload).expect("published payload must parse");
        assert_eq!(
            published.reporting_user_id,
            b"receiver-user".to_vec(),
            "control: the participant's telemetry must still carry its authenticated identity"
        );
    }

    /// Build the raw bytes of a `PacketWrapper{RAISE_HAND}` for the #2135
    /// call-site tests below. `session_id` is what a (possibly malicious) client
    /// put on the wire — the whole point is that the relay must not honour it.
    #[cfg(test)]
    fn raise_hand_bytes(session_id: u64, raised: bool, display_name: &[u8]) -> Vec<u8> {
        use protobuf::Message as _;
        use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};
        use videocall_types::protos::raise_hand_packet::RaiseHandPacket;

        let inner = RaiseHandPacket {
            raised,
            raised_at_ms: 1_700_000_000_000,
            display_name: display_name.to_vec(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::RAISE_HAND.into(),
            session_id,
            data: inner.write_to_bytes().expect("serialize inner raise-hand"),
            ..Default::default()
        };
        wrapper
            .write_to_bytes()
            .expect("serialize raise-hand wrapper")
    }

    /// #2135 (waiting-room isolation): an OBSERVER's RAISE_HAND must not escape
    /// the waiting room.
    ///
    /// This mirrors the #2095 HEALTH guard directly above, and the stakes are the
    /// same or higher. A RAISE_HAND is re-broadcast on the media fan-out and the
    /// relay stamps the fanned-out envelope with the sender's AUTHENTICATED
    /// session_id and `sub` (`stamp_wrapper_for_broadcast`, #2124) — so without
    /// this guard an unadmitted waiting-room client could plant a NAMED, durable
    /// entry in every participant's raised-hands list, disclosing its server-side
    /// identity (email, or `guest:{uuid}`) in the process. That identity is
    /// precisely what PARTICIPANT_JOINED deliberately does NOT broadcast for
    /// observers.
    ///
    /// A NON-observer session is then driven through the SAME code with the SAME
    /// bytes and must still Forward. Without that control, deleting the whole
    /// `PacketKind::RaiseHand` arm would pass this test.
    ///
    /// MUTATION PROOF: remove the `if self.observer { .. }` guard from the
    /// `PacketKind::RaiseHand` arm and the observer's packet becomes
    /// `Forward(_)` -> the first assert fails. Delete the arm entirely and the
    /// control assert fails instead.
    ///
    /// Requires a broker (a `SessionLogic` needs a real `ChatServer`) and SKIPS
    /// when none is reachable, like the siblings above.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn observer_raise_hand_is_not_forwarded() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let bytes = raise_hand_bytes(0, true, b"Waiting Wendy");

        let mut observer_logic =
            build_test_logic(nats_client.clone(), "raise_hand_2135_room", true).await;
        assert!(
            matches!(
                observer_logic.handle_inbound(&bytes),
                InboundAction::Processed
            ),
            "an observer's RAISE_HAND must be consumed, never fanned out to the meeting"
        );

        let mut member_logic =
            build_test_logic(nats_client.clone(), "raise_hand_2135_room", false).await;
        assert!(
            matches!(
                member_logic.handle_inbound(&bytes),
                InboundAction::Forward(_)
            ),
            "control: an admitted participant's RAISE_HAND must still be forwarded"
        );
    }

    /// #2135 (security): the RAISE_HAND arm must forward the STAMPED bytes, not
    /// the raw inbound ones.
    ///
    /// This is a CALL-SITE test, deliberately. The pure-function tests in
    /// `packet_handler` pin `stamp_raise_hand_for_broadcast` itself, but they
    /// cannot catch the mistake that actually matters here: an arm that computes
    /// the stamp and then forwards `data.to_vec()` anyway. The REACTION arm
    /// documents that exact gap in its own comment ("the stamping test guards the
    /// stamping fn, not this call site") and relies solely on a `let data =`
    /// shadow to make it a compile error. The shadow is good, but a test that
    /// reads the FORWARDED bytes is what proves the guarantee end to end — so
    /// this closes for RAISE_HAND what is still only structural for REACTION.
    ///
    /// MUTATION PROOF: replace the arm's
    /// `stamp_raise_hand_for_broadcast(..)` + `match` with
    /// `InboundAction::Forward(Arc::new(data.to_vec()))` and the forged
    /// `FORGED_VICTIM` session_id survives into the forwarded bytes -> fails.
    /// Also fails if the display_name bound is dropped from the call (the second
    /// assert), since an unbounded name reaches the fan-out.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn raise_hand_forward_carries_stamped_not_raw_bytes() {
        use protobuf::Message as _;
        use videocall_types::protos::packet_wrapper::PacketWrapper;
        use videocall_types::protos::raise_hand_packet::RaiseHandPacket;

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        // A malicious participant claims a live peer's session and an oversized
        // cosmetic name in the SAME packet — both must be neutralised on the way
        // out.
        const FORGED_VICTIM: u64 = 9999;
        let overlong = vec![b'x'; RAISE_HAND_DISPLAY_NAME_MAX_BYTES + 200];
        let bytes = raise_hand_bytes(FORGED_VICTIM, true, &overlong);

        let mut logic = build_test_logic(nats_client.clone(), "raise_hand_2135_stamp", false).await;
        let authenticated = logic.id;
        assert_ne!(
            authenticated, FORGED_VICTIM,
            "sanity: the relay-assigned session must differ from the forged one"
        );

        let forwarded = match logic.handle_inbound(&bytes) {
            InboundAction::Forward(b) => b,
            other => panic!("expected a Forward for a valid RAISE_HAND, got {other:?}"),
        };

        let out = PacketWrapper::parse_from_bytes(&forwarded)
            .expect("the forwarded bytes must be a parseable wrapper");
        assert_eq!(
            out.session_id, authenticated,
            "the FORWARDED envelope must carry the relay-authenticated session, not the \
             client-supplied one — otherwise a participant can raise a victim's hand"
        );
        let out_inner = RaiseHandPacket::parse_from_bytes(&out.data)
            .expect("the forwarded inner packet must parse");
        assert!(
            out_inner.display_name.len() <= RAISE_HAND_DISPLAY_NAME_MAX_BYTES,
            "the FORWARDED cosmetic display_name must be bounded before room-wide fan-out"
        );
        assert!(
            out_inner.raised,
            "the hand STATE must survive the stamp — a stamp that lost it would turn a \
             RAISE into a LOWER for the whole room"
        );
    }

    /// #2135: an over-budget RAISE_HAND is dropped, and the sender's budget
    /// REFILLS — i.e. the limiter cannot wedge a participant's hand state.
    ///
    /// This is the recovery half of the rate limit, and it matters more here than
    /// for reactions: the relay holds no hand registry, so a limiter that could
    /// not refill would leave the room's view of this participant permanently
    /// wrong. Driven through `handle_inbound` (not the limiter in isolation) so
    /// it pins the ARM's use of the limiter, including that the arm meters at all.
    ///
    /// MUTATION PROOF: delete the `if !self.raise_hand_limiter.allow()` block and
    /// the (MAX+1)th packet Forwards -> the second assert fails. Point the arm at
    /// `self.reaction_limiter` instead and the budget changes from
    /// RAISE_HAND_MAX_PER_WINDOW (6) to REACTION_MAX_PER_WINDOW (4) -> the
    /// in-budget loop fails at i=4.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn raise_hand_over_budget_is_dropped_then_budget_refills() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let bytes = raise_hand_bytes(0, true, b"Ada");
        let mut logic = build_test_logic(nats_client.clone(), "raise_hand_2135_limit", false).await;

        for i in 0..crate::constants::RAISE_HAND_MAX_PER_WINDOW {
            assert!(
                matches!(logic.handle_inbound(&bytes), InboundAction::Forward(_)),
                "announce {i} within the per-sender budget must be forwarded"
            );
        }
        assert!(
            matches!(logic.handle_inbound(&bytes), InboundAction::Processed),
            "the announce past RAISE_HAND_MAX_PER_WINDOW must be dropped, not fanned out"
        );

        // Rewind the limiter's window so the budget refills without a real sleep.
        // A hand-state drop is only tolerable because this recovery exists.
        logic
            .raise_hand_limiter
            .rewind_window_for_test(Duration::from_millis(
                crate::constants::RAISE_HAND_WINDOW_MS + 50,
            ));
        assert!(
            matches!(logic.handle_inbound(&bytes), InboundAction::Forward(_)),
            "once the window slides the sender's budget must refill — a rate-limit drop \
             must be recoverable, or a participant's hand state is wedged for the meeting"
        );
    }

    // ---------------------------------------------------------------------
    // #2136 — MEETING_TIMER call-site behaviour
    // ---------------------------------------------------------------------

    /// Build the raw bytes of a `PacketWrapper{MEETING_TIMER}` for the #2136
    /// call-site tests below.
    #[cfg(test)]
    fn meeting_timer_bytes(running: bool, ends_at_ms: u64, duration_ms: u64) -> Vec<u8> {
        use protobuf::Message as _;
        use videocall_types::protos::meeting_timer_packet::MeetingTimerPacket;
        use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

        let inner = MeetingTimerPacket {
            running,
            ends_at_ms,
            duration_ms,
            updated_at_ms: 1_700_000_000_000,
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING_TIMER.into(),
            data: inner
                .write_to_bytes()
                .expect("serialize inner meeting-timer"),
            ..Default::default()
        };
        wrapper
            .write_to_bytes()
            .expect("serialize meeting-timer wrapper")
    }

    /// #2136 (SECURITY): `client_message_for` must PRESERVE `requires_host` in
    /// both directions.
    ///
    /// This link had no test until a review pointed out that the suite bracketed
    /// it on both sides: the `handle_inbound` tests stop at
    /// `matches!(action, ForwardHostOnly(_))`, and the `chat_server` gate tests
    /// start by hand-constructing a `ClientMessage { requires_host: true }`.
    /// Nothing joined the two, so flipping the `true` in
    /// `create_host_gated_client_message` to `false` left the entire suite green
    /// — with the host gate silently disabled in production and any participant
    /// able to drive the room's timer.
    ///
    /// Both transports route through this ONE function (rather than mirroring
    /// the branch), so this also covers the WS and WT paths together — the
    /// mirrored version could have been disabled on one transport only, which is
    /// the harder failure to notice.
    ///
    /// MUTATION PROOF: flip either literal in
    /// `create_host_gated_client_message` / `create_client_message`, or make
    /// `client_message_for` ignore `msg.requires_host` and always take one
    /// branch, and one of the two asserts fails.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn client_message_for_preserves_the_host_gate_flag() {
        use crate::messages::server::Packet;

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let logic = build_test_logic(nats_client.clone(), "meeting_timer_2136_flag", false).await;

        let gated = logic.client_message_for(Packet {
            data: Arc::new(vec![]),
            requires_host: true,
        });
        assert!(
            gated.requires_host,
            "a host-gated Packet must produce a host-gated ClientMessage — losing the flag \
             here removes the relay's ONLY host authorization for MEETING_TIMER"
        );
        assert_eq!(
            gated.session, logic.id,
            "identity must come from the session, never the packet — the funnel resolves \
             host-ness by this session id"
        );

        let plain = logic.client_message_for(Packet {
            data: Arc::new(vec![]),
            requires_host: false,
        });
        assert!(
            !plain.requires_host,
            "an ordinary Packet must NOT be host-gated — otherwise every media frame from a \
             non-host would be dropped at the funnel"
        );
    }

    /// #2136 (waiting-room isolation): an OBSERVER's MEETING_TIMER must not
    /// escape the waiting room.
    ///
    /// This mirrors the #2095 HEALTH guard above. It is NOT made redundant by the
    /// host gate downstream: no observer should hold the host role, but that is
    /// an emergent property of how tokens are minted rather than an invariant
    /// anything enforces, and this guard also stops an unadmitted client's packet
    /// from consuming a limiter slot or reaching the fan-out actor at all.
    ///
    /// A NON-observer session is then driven through the SAME code with the SAME
    /// bytes and must still forward. Without that control, deleting the whole
    /// `PacketKind::MeetingTimer` arm would pass this test.
    ///
    /// MUTATION PROOF: remove the `if self.observer { .. }` guard from the
    /// `PacketKind::MeetingTimer` arm and the observer's packet becomes
    /// `ForwardHostOnly(_)` -> the first assert fails. Delete the arm entirely
    /// and the control assert fails instead.
    ///
    /// Requires a broker (a `SessionLogic` needs a real `ChatServer`) and SKIPS
    /// when none is reachable, like the siblings above.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn observer_meeting_timer_is_not_forwarded() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let bytes = meeting_timer_bytes(true, 1_700_000_300_000, 300_000);

        let mut observer_logic =
            build_test_logic(nats_client.clone(), "meeting_timer_2136_room", true).await;
        assert!(
            matches!(
                observer_logic.handle_inbound(&bytes),
                InboundAction::Processed
            ),
            "an observer's MEETING_TIMER must be consumed, never handed to the fan-out"
        );

        let mut member_logic =
            build_test_logic(nats_client.clone(), "meeting_timer_2136_room", false).await;
        assert!(
            matches!(
                member_logic.handle_inbound(&bytes),
                InboundAction::ForwardHostOnly(_)
            ),
            "control: an admitted participant's MEETING_TIMER must still reach the host gate"
        );
    }

    /// #2136 (SECURITY, the load-bearing one): the arm must return
    /// `ForwardHostOnly`, never a plain `Forward`.
    ///
    /// `ForwardHostOnly` is the ONLY thing that routes this packet through
    /// `ChatServer`'s host authorization. A plain `Forward` looks harmless,
    /// compiles, delivers the timer correctly in every manual test — and
    /// silently removes the authority check entirely, letting ANY participant
    /// drive the room's timer and trigger the expiry sound on every device. No
    /// other test in this suite would notice: the `chat_server` gate tests would
    /// still pass, because they construct their `ClientMessage` directly.
    ///
    /// The second half pins the other side of the same design decision. This
    /// `SessionLogic` is built with `is_host = false` (see `build_test_logic`),
    /// and it must STILL forward — because the arm deliberately does NOT consult
    /// `self.is_host`, which is a JWT-claim snapshot that goes stale in both
    /// directions across a transfer-host. If someone "hardens" the arm by adding
    /// an `if !self.is_host { return Processed }`, this assert fails, and that is
    /// the point: the promoted host would otherwise be locked out.
    ///
    /// MUTATION PROOF: change the arm's tail to
    /// `InboundAction::Forward(Arc::new(data.to_vec()))` and the first assert
    /// fails. Add a `self.is_host` guard to the arm and the same assert fails.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn meeting_timer_forwards_host_gated_and_ignores_the_stale_jwt_claim() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let mut logic =
            build_test_logic(nats_client.clone(), "meeting_timer_2136_gate", false).await;
        assert!(
            !logic.is_host,
            "sanity: this fixture's JWT-claim snapshot is false — the assert below is only \
             meaningful because the arm forwards ANYWAY, deferring authority to the live flag"
        );

        let action = logic.handle_inbound(&meeting_timer_bytes(true, 1_700_000_300_000, 300_000));
        assert!(
            matches!(action, InboundAction::ForwardHostOnly(_)),
            "a valid MEETING_TIMER must return ForwardHostOnly — a plain Forward would bypass \
             the relay's host authorization entirely, and a `self.is_host` guard here would \
             lock out a host promoted by transfer-host. Got {action:?}"
        );

        // The forwarded bytes must be the validated packet, unchanged: unlike
        // REACTION/RAISE_HAND this class carries no cosmetic string to bound, so
        // there is deliberately no stamp step to lose the state in.
        let forwarded = match action {
            InboundAction::ForwardHostOnly(b) => b,
            other => panic!("expected ForwardHostOnly, got {other:?}"),
        };
        use protobuf::Message as _;
        use videocall_types::protos::meeting_timer_packet::MeetingTimerPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;
        let out = PacketWrapper::parse_from_bytes(&forwarded)
            .expect("the forwarded bytes must be a parseable wrapper");
        let out_inner =
            MeetingTimerPacket::parse_from_bytes(&out.data).expect("inner packet must parse");
        assert!(
            out_inner.running,
            "the timer STATE must survive the arm — dropping it would turn a START into a \
             CANCEL for the whole room"
        );
        assert_eq!(
            out_inner.ends_at_ms, 1_700_000_300_000,
            "ends_at_ms must be forwarded VERBATIM: it is the countdown every peer computes \
             its own remaining time from, and the relay must never rewrite it"
        );
    }

    /// #2136: an over-budget MEETING_TIMER is dropped, and the sender's budget
    /// REFILLS — the limiter cannot wedge a host out of its own room's timer.
    ///
    /// The recovery half matters more here than for reactions: the relay keeps no
    /// timer registry, so a permanently-throttled host could neither cancel a
    /// running timer nor start a new one.
    ///
    /// MUTATION PROOF: delete the `if !self.meeting_timer_limiter.allow()` block
    /// from the arm and the over-budget assert fails. Make the window never reset
    /// and the refill assert fails.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn meeting_timer_over_budget_is_dropped_then_refills() {
        use crate::constants::{MEETING_TIMER_MAX_PER_WINDOW, MEETING_TIMER_WINDOW_MS};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = match async_nats::connect(&nats_url).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                return;
            }
        };

        let mut logic =
            build_test_logic(nats_client.clone(), "meeting_timer_2136_budget", false).await;
        let bytes = meeting_timer_bytes(true, 1_700_000_300_000, 300_000);

        for i in 0..MEETING_TIMER_MAX_PER_WINDOW {
            assert!(
                matches!(
                    logic.handle_inbound(&bytes),
                    InboundAction::ForwardHostOnly(_)
                ),
                "packet {i} must be within budget at the CALL SITE, not just in the limiter"
            );
        }
        assert!(
            matches!(logic.handle_inbound(&bytes), InboundAction::Processed),
            "one packet past the budget must be dropped as Processed, never fanned out"
        );

        logic
            .meeting_timer_limiter
            .rewind_window_for_test(std::time::Duration::from_millis(
                MEETING_TIMER_WINDOW_MS + 1,
            ));
        assert!(
            matches!(
                logic.handle_inbound(&bytes),
                InboundAction::ForwardHostOnly(_)
            ),
            "the budget must REFILL: a host locked out permanently could neither cancel a \
             running timer nor start a new one"
        );
    }
}
