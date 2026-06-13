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
    classify_packet, outbound_keyframe_observation, KeyframeRequestLimiter, KeyframeTarget,
    PacketKind,
};
use crate::client_diagnostics::health_processor;
use crate::constants::{
    CONGESTION_DROP_THRESHOLD, CONGESTION_NOTIFY_MIN_INTERVAL, CONGESTION_WINDOW,
    KEYFRAME_CONGESTION_RELAX_WINDOW,
};
use crate::messages::server::{ClientMessage, Connect, Disconnect, JoinRoom, Packet};
use crate::messages::session::Message;
use crate::metrics::{RELAY_ACTIVE_SESSIONS_PER_ROOM, RELAY_ROOM_BYTES_TOTAL};
use crate::server_diagnostics::{
    send_connection_ended, send_connection_started, DataTracker, TrackerSender,
};
use crate::session_manager::SessionManager;
use actix::Addr;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

pub type SessionId = u64;
pub type RoomId = String;
pub type UserId = String;

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
    /// Most recent instant at which this receiver crossed the drop threshold
    /// for *any* sender. Used by [`CongestionTracker::is_actively_congested`]
    /// to relax the KEYFRAME_REQUEST rate limiter so a frozen receiver can
    /// recover (issue #979).
    last_congestion: Option<Instant>,
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
/// [`CLEANUP_INTERVAL`] drops, so between passes the map could in principle
/// accumulate one entry per distinct `sender_session_id` seen — a slow
/// memory-amplification vector if a peer churns join/leave to cycle through
/// server-assigned session ids (lower severity than #1303's client-forged
/// KEYFRAME_REQUEST targets, since the id here is server-assigned, not in the
/// packet payload). This cap is ~5–10× the largest realistic room, so it never
/// constrains legitimate traffic; it only backstops the pathological case.
/// At ~40 bytes per [`SenderDropState`], 256 entries is ~10 KB per receiver.
const MAX_TRACKED_SENDERS: usize = 256;

impl CongestionTracker {
    pub fn new() -> Self {
        Self {
            senders: HashMap::new(),
            total_drops: 0,
            last_congestion: None,
        }
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
    /// avoiding an O(n) `retain()` on every single drop.
    /// Remove sender entries idle longer than `CONGESTION_WINDOW * 10` (10s of
    /// no recorded drops). Shared by the amortized cleanup and the #1320
    /// cap-pressure sweep.
    fn evict_stale_senders(&mut self, now: Instant) {
        let stale_threshold = CONGESTION_WINDOW * 10;
        self.senders
            .retain(|_, state| now.duration_since(state.window_start) <= stale_threshold);
    }

    pub fn record_drop(&mut self, sender_session_id: u64) -> Option<u64> {
        let now = Instant::now();

        // Amortized cleanup of stale sender entries.
        self.total_drops = self.total_drops.wrapping_add(1);
        if self.total_drops.is_multiple_of(CLEANUP_INTERVAL) {
            self.evict_stale_senders(now);
        }

        // #1320: hard entry-count bound as defense-in-depth on top of the
        // amortized time-based cleanup above. If we are at the cap and this is a
        // NEW sender, force an immediate stale sweep first; if STILL at the cap
        // afterward, skip tracking this drop rather than grow the map unbounded.
        // An ALREADY-tracked sender is never refused (its window/notify state and
        // the #979 keyframe-relax path it feeds are untouched). Refusing a brand
        // new sender only when the cap is genuinely full is harmless: at that
        // point this receiver is already tracking MAX_TRACKED_SENDERS congested
        // sources, `last_congestion` is already being driven, and
        // `is_actively_congested()` already returns true — so the dropped
        // tracking for one more sender costs nothing the relax path needs.
        if self.senders.len() >= MAX_TRACKED_SENDERS
            && !self.senders.contains_key(&sender_session_id)
        {
            self.evict_stale_senders(now);
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
        }
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
        }
    }

    /// Create ClientMessage for forwarding a packet to ChatServer (NATS broadcast).
    pub fn create_client_message(&self, msg: Packet) -> ClientMessage {
        ClientMessage {
            session: self.id,
            user: self.user_id.clone(),
            room: self.room.clone(),
            msg,
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
        // LEAK-PROOF (issue #1090): we iterate the FULL fixed `kind` taxonomy
        // [`crate::metrics::RELAY_DROP_KINDS`] UNCONDITIONALLY rather than a
        // per-session "kinds I emitted" tracking set. `remove_label_values` on a
        // `(…, kind)` tuple that was never created returns a benign `Err`, so
        // the discarded result is intentional. This removes the dependency on
        // tracking-set completeness: even if a future drop site introduces a new
        // `kind`, adding it to `RELAY_DROP_KINDS` (the single source of truth the
        // emit sites are documented against) keeps cleanup exhaustive — there is
        // no second bookkeeping structure that could silently fall out of sync.
        let session_id = self.id.to_string();
        for kind in crate::metrics::RELAY_DROP_KINDS {
            let _ = crate::metrics::RELAY_SESSION_DROPS_TOTAL.remove_label_values(&[
                &self.room,
                &self.transport,
                &session_id,
                kind,
            ]);
        }
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

    /// Handle an inbound packet from the client.
    ///
    /// Returns the action the transport should take.
    /// Observer sessions can still send RTT and health packets but all media
    /// data packets are silently dropped.
    ///
    /// This is the **inbound** half of the waiting-room isolation enforcement.
    /// The **outbound** half lives in `ChatServer::handle_msg()` which drops all
    /// non-allowlisted packets before they reach observer sessions.
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
                trace!("Health packet from {}", self.user_id);
                health_processor::process_health_packet_bytes(data, self.nats_client.clone());
                InboundAction::Processed
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
                // link); `handle_outbound` clears that waiting flag when the
                // matching media is actually delivered.
                if !self
                    .keyframe_limiter
                    .allow_with_congestion(target, kind, layer, congested)
                {
                    warn!(
                        "Rate-limiting KEYFRAME_REQUEST from session {} (user {}) targeting user {} session {}",
                        self.id,
                        self.user_id,
                        String::from_utf8_lossy(&target_user_id),
                        target_session_id,
                    );
                    return InboundAction::Processed;
                }
                InboundAction::Forward(Arc::new(data.to_vec()))
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
        }
    }

    /// Handle an outbound message from ChatServer (to be sent to client).
    ///
    /// Returns the bytes to send and tracks metrics.
    ///
    /// #1297: this is also the DELIVERY-OBSERVATION point for the keyframe
    /// limiter. Every forwarded frame this receiver is about to be sent is the
    /// delivery side of the keyframe-request/keyframe-delivery loop. When the
    /// frame is a MEDIA VIDEO/SCREEN packet, we clear THIS receiver's
    /// still-waiting flag for that `(publisher, kind)` bucket so the strict
    /// per-pair budget re-engages on its next request (a receiver that keeps
    /// requesting after recovery is throttled again). This runs for BOTH
    /// transports because both `WsChatSession` and `WtChatSession` route every
    /// outbound frame through here. It is `&mut self` purely so the observation
    /// can mutate `self.keyframe_limiter` — the SAME limiter instance the
    /// inbound KEYFRAME_REQUEST arm reads, so request-set and delivery-clear act
    /// on one map.
    ///
    /// DELIVERY semantics (honest contract): like the `RELAY_ROOM_BYTES_TOTAL`
    /// "outbound" accounting above, this hook runs when a frame is HANDED to the
    /// transport — BEFORE the per-transport priority-drop / channel-full check
    /// (both callers invoke `handle_outbound` first). So a keyframe that is
    /// subsequently priority-dropped under outbound-channel saturation still
    /// clears the wait. That imprecision is benign: a drop here means the
    /// receiver's outbound channel is SATURATED, which fires `on_outbound_drop`
    /// → the receiver is then flagged congested, so the #979 congested
    /// relaxation (not the delivery-aware path) covers its next recovery
    /// request. On the healthy, unsaturated path #1297 targets — the common
    /// all-WS deployment — the frame IS delivered and clearing the wait is
    /// correct.
    pub fn handle_outbound(&mut self, msg: &Message) -> Vec<u8> {
        RELAY_ROOM_BYTES_TOTAL
            .with_label_values(&[&self.room, "outbound"])
            .inc_by(msg.msg.len() as f64);
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_sent(self.id, msg.msg.len() as u64);

        // #1297 delivery observation: cheap partial decode (no `data` copy —
        // see `outbound_keyframe_observation`). Only MEDIA VIDEO/SCREEN frames
        // return Some; everything else (the bulk of traffic) is a no-op here.
        // Observers never request keyframes, so skip the work for them.
        if !self.observer {
            if let Some((target, kind)) = outbound_keyframe_observation(&msg.msg) {
                self.keyframe_limiter.observe_delivery(target, kind);
            }
        }

        // `msg.msg` is a shared `bytes::Bytes` (#1063): the single NATS payload
        // allocation is refcounted across all fan-out receivers. The
        // per-transport outbound channel (`Sender<Vec<u8>>` for WS, or
        // `Sender<Bytes>` for WT via `send_auto`) still needs owned bytes, so
        // materialize ONCE here per receiver — the same single copy that used
        // to live at the fan-out `Message` construction, just moved downstream.
        msg.msg.to_vec()
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
    pub fn on_outbound_drop(&mut self, sender_session_id: u64, sender_user_id: &[u8]) {
        if let Some(sender_sid) = self.congestion_tracker.record_drop(sender_session_id) {
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
        // Forward, Processed, KeepAlive should activate.
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::Forward(Arc::new(vec![]))
        ));
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::Processed
        ));
        assert!(SessionLogic::should_activate_on_action(
            &InboundAction::KeepAlive
        ));
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
            false,
            None,
            "websocket",
            false,
            false,
        )
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
}
