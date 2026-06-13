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

//! Shared packet handling logic for session actors.
//!
//! This module provides common packet classification and processing
//! used by both `WsChatSession` and `WtChatSession`.

use protobuf::rt::WireType;
use protobuf::CodedInputStream;
use protobuf::Enum;
use protobuf::Message as ProtobufMessage;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
use videocall_types::protos::packet_wrapper::PacketWrapper;

use crate::constants::{
    KEYFRAME_LIMITER_CLEANUP_INTERVAL, KEYFRAME_REQUEST_MAX_LAYER_ID, KEYFRAME_REQUEST_MAX_PER_SEC,
    KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER, KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED,
    KEYFRAME_REQUEST_STILL_WAITING_MIN_RETRY_MS, KEYFRAME_REQUEST_WINDOW_MS,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Media-kind discriminator for KEYFRAME_REQUEST rate limiting (#1297).
///
/// This is a tiny relay-local enum, NOT the proto
/// [`videocall_types::...::MediaKind`]. It exists so the REQUEST side (which
/// learns the kind from the inner `MediaPacket.data` byte-string the client
/// sends — see [`KeyframeMediaKind::from_request_data`]) and the DELIVERY side
/// (which learns the kind from the OUTER cleartext `PacketWrapper.media_kind` —
/// see [`KeyframeMediaKind::from_outer`]) map onto the SAME three buckets, so a
/// request and the matching delivered media JOIN on identical limiter keys.
///
/// Only VIDEO and SCREEN are keyframe-bearing media kinds the client ever
/// requests (AUDIO has no keyframe concept and the client never sends a request
/// for it — see `peer_decode_manager::send_keyframe_request`, whose `_ =>
/// return` arm covers AUDIO and everything else). `Other` is the fail-open
/// catch-all for AUDIO, `MEDIA_KIND_UNSPECIFIED`, an unrecognised request
/// byte-string, or any future kind; folding them into one bucket keeps the
/// per-target bucket count bounded at 3 (Video, Screen, Other).
///
/// SPLITTING VIDEO from SCREEN (the core of fix part 2) means a SCREEN recovery
/// request is no longer starved out of the same 1/sec bucket by a flurry of
/// VIDEO requests in the same second — the previous behaviour collided both
/// into one `(target, layer)` bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyframeMediaKind {
    /// Camera video.
    Video,
    /// Screen share.
    Screen,
    /// AUDIO / UNSPECIFIED / unknown — fail-open single bucket.
    Other,
}

impl KeyframeMediaKind {
    /// Derive the requested kind from the inner `MediaPacket.data` bytes the
    /// client populates on a KEYFRAME_REQUEST.
    ///
    /// CLIENT TRUTH (`videocall-client/src/decode/peer_decode_manager.rs`):
    /// the client writes the literal ASCII `b"VIDEO"` or `b"SCREEN"` into the
    /// inner `MediaPacket.data` field; it sends NO request for any other kind.
    /// The outer `PacketWrapper.media_kind` is left UNSPECIFIED on requests, so
    /// the discriminator lives ONLY in these inner bytes — there is no client
    /// companion change required for this fix. Anything else (older client,
    /// forged/garbage bytes) maps to [`KeyframeMediaKind::Other`] (fail-open).
    fn from_request_data(data: &[u8]) -> Self {
        match data {
            b"VIDEO" => KeyframeMediaKind::Video,
            b"SCREEN" => KeyframeMediaKind::Screen,
            _ => KeyframeMediaKind::Other,
        }
    }

    /// Derive the delivery kind from the OUTER cleartext
    /// `PacketWrapper.media_kind` of a forwarded MEDIA frame.
    ///
    /// Publishers DO set the outer `media_kind` on real media (the #988/#989
    /// filters depend on it), so a delivered VIDEO/SCREEN frame maps to the SAME
    /// bucket the matching request set. AUDIO and `MEDIA_KIND_UNSPECIFIED` map to
    /// [`KeyframeMediaKind::Other`]: a publisher delivering media with an
    /// UNSPECIFIED outer `media_kind` only clears the `Other`/fail-open waiting
    /// bucket (documented degrade — it cannot clear a Video/Screen wait), so a
    /// request that landed in the Video/Screen bucket simply keeps its
    /// delivery-aware relaxation until a properly-tagged frame arrives.
    fn from_outer(kind: MediaKind) -> Self {
        match kind {
            MediaKind::VIDEO => KeyframeMediaKind::Video,
            MediaKind::SCREEN => KeyframeMediaKind::Screen,
            MediaKind::AUDIO | MediaKind::MEDIA_KIND_UNSPECIFIED => KeyframeMediaKind::Other,
        }
    }
}

/// Classification of an incoming packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketKind {
    /// RTT (Round-Trip Time) packet - should be echoed back to sender
    Rtt,
    /// Health diagnostics packet - should be processed for metrics
    Health,
    /// Normal data packet - should be forwarded to ChatServer
    Data,
    /// Packet that should be silently dropped (e.g., client-originated CONGESTION or MEETING)
    Dropped,
    /// KEYFRAME_REQUEST packet - subject to per-(receiver, target_sender,
    /// layer) rate limiting. The embedded `target_user_id` is the user_id of
    /// the peer whose video the receiver wants a keyframe from, taken from the
    /// inner `MediaPacket.user_id` field. May be empty if the client sent a
    /// malformed request, in which case the limiter still enforces a key (the
    /// empty target acts as a single bucket).
    ///
    /// `target_session_id` is the inner `MediaPacket.target_session_id` (#1124)
    /// — the specific publishing SESSION the receiver wants a keyframe from.
    /// `0` means the requesting client is older / did not populate it, in which
    /// case the limiter falls back to keying by `target_user_id` (preserving
    /// the pre-#1124 behaviour). When non-zero it is the limiter key, so two
    /// concurrent sessions of the same participant get independent budgets.
    ///
    /// `layer` is the cleartext `PacketWrapper.simulcast_layer_id` (#989,
    /// Phase 1b) the request targets — 0 = base/unspecified. It is part of the
    /// limiter key so a receiver switching the simulcast layer it wants from a
    /// sender is not rate-limited as "already requested" (which would freeze
    /// the newly-selected layer's tile until the window elapsed).
    ///
    /// `kind` is the requested media kind (#1297), derived from the inner
    /// `MediaPacket.data` byte-string (`b"VIDEO"`/`b"SCREEN"`, else `Other`).
    /// It is part of the limiter key so VIDEO and SCREEN recovery requests no
    /// longer collide into one rate-limit bucket (SCREEN recovery starved by
    /// VIDEO requests in the same second).
    KeyframeRequest {
        target_user_id: Vec<u8>,
        target_session_id: u64,
        layer: u32,
        kind: KeyframeMediaKind,
    },
}

/// Classify a packet based on its contents.
///
/// Parses the `PacketWrapper` exactly once and uses the `packet_type` field
/// to classify the packet. For MEDIA packets, the inner `MediaPacket` is
/// parsed at most once to distinguish RTT and KEYFRAME_REQUEST from regular
/// media data.
///
/// # Arguments
/// * `data` - Raw packet bytes
///
/// # Returns
/// The classification of the packet
pub fn classify_packet(data: &[u8]) -> PacketKind {
    let packet_wrapper = match PacketWrapper::parse_from_bytes(data) {
        Ok(pw) => pw,
        Err(_) => return PacketKind::Data, // unparseable, treat as opaque data
    };

    // Drop client-originated CONGESTION packets.
    // CONGESTION signals must only originate from the server's CongestionTracker,
    // never from clients. A malicious client could craft a CONGESTION packet with
    // a victim's session_id to force them to degrade video quality.
    if packet_wrapper.packet_type == PacketType::CONGESTION.into() {
        return PacketKind::Dropped;
    }

    // Drop client-originated LAYER_HINT packets (#1119).
    // LAYER_HINT is symmetric to CONGESTION: a RELAY-authored, self-addressed
    // control packet emitted only by the relay's per-source layer aggregator
    // (`emit_layer_hint`) onto a publisher's own subject so the client can cap its
    // simulcast ladder. The relay NEVER parses an inbound LAYER_HINT (see the proto
    // doc on `PacketType::LAYER_HINT`), so a client-sent one is always forged.
    // It is harmless today — it never touches the relay's union state, and every
    // recipient rejects it via the client self-targeting check — but reflecting a
    // relay-authored-only control type client→room is an avoidable broadcast vector.
    // Drop it here, fail-closed, so the "relay never reflects relay-authored control
    // packets" invariant is explicit.
    if packet_wrapper.packet_type == PacketType::LAYER_HINT.into() {
        return PacketKind::Dropped;
    }

    // Drop client-originated MEETING packets.
    // MEETING events (HOST_MUTE_PARTICIPANT, MEETING_ENDED, etc.) are
    // server-authoritative: they are published exclusively by meeting-api
    // via NATS on the room.{id}.system subject.  A client-originated
    // MEETING packet is always forged and must be dropped to prevent
    // participants from broadcasting fake host actions.
    if packet_wrapper.packet_type == PacketType::MEETING.into() {
        return PacketKind::Dropped;
    }

    // Check if it's a MEDIA packet (RTT, keyframe request, or regular media).
    if packet_wrapper.packet_type == PacketType::MEDIA.into() {
        // Try to parse inner MediaPacket to distinguish control sub-types.
        // For encrypted payloads this parse will fail, correctly falling
        // through to PacketKind::Data.
        if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
            if media_packet.media_type == MediaType::RTT.into() {
                return PacketKind::Rtt;
            }
            if media_packet.media_type == MediaType::KEYFRAME_REQUEST.into() {
                // The inner MediaPacket.user_id identifies the target peer
                // (the sender whose video should produce a keyframe). The
                // inner MediaPacket.target_session_id (#1124) identifies the
                // specific target SESSION; the limiter keys on it when present
                // so two concurrent sessions of one participant do not collide
                // into a single rate-limit bucket. The outer wrapper's
                // session_id is the SOURCE (the requester) and must not be
                // reused for the target, so the target session travels in the
                // inner packet, which is sent in cleartext for KEYFRAME_REQUEST
                // (relay-readable even under E2EE). When `target_session_id` is
                // 0 (older client), the limiter falls back to keying by
                // `user_id` — stable across reconnects of the same participant,
                // preserving the pre-#1124 behaviour for those clients.
                //
                // The cleartext outer `simulcast_layer_id` (#989, Phase 1b)
                // identifies which simulcast layer the receiver wants a
                // keyframe for. It is part of the limiter key (see
                // `PacketKind::KeyframeRequest`) so switching layers is not
                // throttled as a duplicate request.
                //
                // #1297: the requested media kind (VIDEO vs SCREEN) lives in
                // the inner `MediaPacket.data` byte-string (the client sets
                // `b"VIDEO"`/`b"SCREEN"` there — the OUTER `media_kind` is left
                // UNSPECIFIED on requests). We classify it here so VIDEO and
                // SCREEN keyframe requests do not share a rate-limit bucket.
                // The inner MediaPacket is already parsed above (cleartext on
                // a KEYFRAME_REQUEST even under E2EE), so this is free.
                let kind = KeyframeMediaKind::from_request_data(&media_packet.data);
                return PacketKind::KeyframeRequest {
                    target_user_id: media_packet.user_id,
                    target_session_id: media_packet.target_session_id,
                    layer: packet_wrapper.simulcast_layer_id,
                    kind,
                };
            }
        }
        return PacketKind::Data;
    }

    // Check health packet.
    if packet_wrapper.packet_type == PacketType::HEALTH.into() {
        return PacketKind::Health;
    }

    PacketKind::Data
}

/// Identity of the keyframe-request target, for the per-pair limiter key
/// (#1124).
///
/// Preferred form is [`KeyframeTarget::Session`] — the specific publishing
/// session the receiver wants a keyframe from — so two concurrent sessions of
/// the SAME participant get independent rate-limit budgets. When the requesting
/// client does not populate the target session (older client; inner
/// `MediaPacket.target_session_id == 0`), we fall back to
/// [`KeyframeTarget::User`], the participant's stable `user_id`, preserving the
/// pre-#1124 behaviour for those clients. The two variants never alias: a
/// session-keyed entry and a user-keyed entry for the same participant are
/// distinct buckets, which is correct — a meeting is either all-new-clients or
/// mixed, and a mixed pair simply double-budgets the same target briefly, which
/// is harmless (it only ever ALLOWS slightly more, never throttles legit
/// traffic).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyframeTarget {
    /// Target publishing session (#1124) — the preferred per-session key.
    Session(u64),
    /// Fallback: target participant's `user_id` (older clients that do not
    /// send `target_session_id`).
    User(Vec<u8>),
}

impl KeyframeTarget {
    /// Build the target key from a `(target_user_id, target_session_id)` pair:
    /// session when non-zero, else the user_id fallback (#1124).
    pub fn from_request(target_user_id: &[u8], target_session_id: u64) -> Self {
        if target_session_id != 0 {
            KeyframeTarget::Session(target_session_id)
        } else {
            KeyframeTarget::User(target_user_id.to_vec())
        }
    }
}

/// Tumbling-window counter for one rate-limit bucket.
///
/// Used both for the global per-receiver cap and for each
/// `(receiver, target_sender, kind, layer)` pair entry inside the limiter.
///
/// `waiting_since` (#1297) is the delivery-awareness state. It is `Some(t)`
/// when this bucket's receiver has issued a keyframe request and the relay has
/// NOT yet observed a qualifying keyframe-bearing frame DELIVERED for that
/// `(target, kind)` since `t`. While it is `Some`, the delivery-aware
/// relaxation path admits a retry (bounded by
/// [`KEYFRAME_REQUEST_STILL_WAITING_MIN_RETRY_MS`] and the global cap) even
/// when the strict per-pair budget is exhausted — this is the WS/TCP recovery
/// path that congestion-relaxation cannot reach on a lossless link. It lives
/// in the SAME map entry as the rate-limit counter so it prunes via the SAME
/// `cleanup_stale_entries` pass and #1068 layer clamp — no second structure,
/// no second prune.
struct WindowCounter {
    count: u32,
    window_start: Instant,
    /// #1297 delivery-awareness: when this bucket's receiver is still waiting
    /// for a keyframe to be delivered, the `Instant` of its last admitted
    /// request; `None` once a qualifying frame has been delivered (waiting
    /// cleared) or before any request. See struct doc.
    waiting_since: Option<Instant>,
}

impl WindowCounter {
    fn new(now: Instant) -> Self {
        Self {
            count: 0,
            window_start: now,
            waiting_since: None,
        }
    }

    /// Try to consume one slot from this bucket within the configured
    /// `window` and `max` capacity. Returns true if accepted, false if the
    /// bucket is saturated for the current window.
    fn try_consume(&mut self, now: Instant, window: Duration, max: u32) -> bool {
        if now.duration_since(self.window_start) > window {
            self.count = 0;
            self.window_start = now;
        }
        if self.count < max {
            self.count += 1;
            true
        } else {
            false
        }
    }

    /// Non-consuming peek: would a [`try_consume`](Self::try_consume) admit right
    /// now? Mirrors the window-roll logic (an elapsed window is effectively empty)
    /// WITHOUT mutating, so the caller can ask "is there a free slot?" before
    /// deciding to do work. Used to gate per-target bucket creation on the global
    /// cap (issue #1303) without burning a slot.
    fn has_capacity(&self, now: Instant, window: Duration, max: u32) -> bool {
        if now.duration_since(self.window_start) > window {
            // A try_consume would reset the count to 0 first, so the window is
            // effectively empty: capacity exists iff the cap is non-zero.
            return max > 0;
        }
        self.count < max
    }
}

/// Per-receiver, per-target-sender rate limiter for KEYFRAME_REQUEST packets.
///
/// Each receiver session owns one `KeyframeRequestLimiter`. The limiter
/// enforces two layers of throttling:
///
/// 1. **Per-target-sender** (primary): each `(receiver, target_sender)` pair
///    gets its own [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`] budget per
///    [`KEYFRAME_REQUEST_WINDOW_MS`]. This is what allows a fresh joiner to
///    request keyframes from many existing senders simultaneously without
///    being clipped by a single global counter.
/// 2. **Global per-receiver** (defense-in-depth): a coarse cap of
///    [`KEYFRAME_REQUEST_MAX_PER_SEC`] across all targets in the same
///    window. This bounds total fan-out from any single receiver as a
///    safety net against bursty or malicious behaviour.
///
/// Memory bound (two layers): (1) a NEW per-target bucket is only opened while
/// the global per-receiver cap has a free slot this window (issue #1303) — a
/// globally-rejected request cannot be forwarded, so it must not cost a map
/// entry; this caps new-bucket creation at ~[`KEYFRAME_REQUEST_MAX_PER_SEC`] per
/// window and closes the forged-`target_session_id` amplification vector. (2) The
/// table additionally cleans up entries that have not been touched for
/// `KEYFRAME_REQUEST_WINDOW_MS * 10` (10 seconds), running every
/// [`KEYFRAME_LIMITER_CLEANUP_INTERVAL`] calls to amortize the O(n)
/// `retain()` cost. This mirrors `CongestionTracker::record_drop` so the
/// strategy is consistent across the relay.
pub struct KeyframeRequestLimiter {
    /// Global counter across all target senders for this receiver.
    global: WindowCounter,
    /// Per-(target-sender, media-kind, layer) counters, keyed by the target
    /// identity ([`KeyframeTarget`]: the target's session_id when known, else
    /// its user_id — #1124), the requested media kind ([`KeyframeMediaKind`] —
    /// #1297) and the simulcast layer the request targets (#989, Phase 1b).
    /// Keying on the layer as well as the target means a receiver switching
    /// layers for the same sender gets a fresh budget instead of being
    /// throttled as a duplicate — otherwise the newly-selected layer's tile
    /// would stay frozen until the window elapsed. Keying on the SESSION (not
    /// the participant) means two concurrent sessions of one identity get
    /// independent budgets (#1124). Keying on the media KIND means a SCREEN
    /// recovery request is not starved out of the bucket by VIDEO requests in
    /// the same second (#1297). The global per-receiver cap (below) is
    /// unaffected, so total fan-out stays bounded (OSS #814).
    ///
    /// #1068: the `u32` layer component is CLAMPED to
    /// `0..=KEYFRAME_REQUEST_MAX_LAYER_ID` before it becomes a key, so the
    /// number of distinct per-layer buckets per target is bounded (an attacker
    /// cycling out-of-ladder layer ids cannot open unbounded buckets). Adding
    /// the media-kind dimension multiplies the bucket ceiling per target by at
    /// most 3 (Video/Screen/Other), so the worst-case bucket count per target
    /// stays `3 × (KEYFRAME_REQUEST_MAX_LAYER_ID + 1)` (= 9 today) — still well
    /// below the global cap. See `allow_with_congestion`.
    per_target: HashMap<(KeyframeTarget, KeyframeMediaKind, u32), WindowCounter>,
    /// Total `allow()` calls since the last cleanup. Cleanup runs every
    /// [`KEYFRAME_LIMITER_CLEANUP_INTERVAL`] calls.
    calls_since_cleanup: u32,
}

impl Default for KeyframeRequestLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeRequestLimiter {
    pub fn new() -> Self {
        Self {
            global: WindowCounter::new(Instant::now()),
            per_target: HashMap::new(),
            calls_since_cleanup: 0,
        }
    }

    /// Check whether a KEYFRAME_REQUEST aimed at `target` of `kind` for simulcast
    /// `layer` should be allowed through, using the strict steady-state per-pair
    /// budget (plus the delivery-aware relaxation — #1297).
    ///
    /// Equivalent to [`KeyframeRequestLimiter::allow_with_congestion`] with
    /// `congested = false`. Retained as the simple entry point for callers
    /// (and tests) that have no congestion signal.
    pub fn allow(&mut self, target: KeyframeTarget, kind: KeyframeMediaKind, layer: u32) -> bool {
        self.allow_with_congestion(target, kind, layer, false)
    }

    /// Check whether a KEYFRAME_REQUEST aimed at `target_user_id` should be
    /// allowed through. Both the per-pair budget and the global cap must
    /// admit the request.
    ///
    /// `congested` indicates the requesting receiver is in active congestion
    /// (issue #979): the relay has recently had to drop inbound media
    /// destined for it, so its decoder is likely frozen and in genuine need
    /// of a fresh keyframe. When set, the **per-pair** budget is relaxed from
    /// [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`] to
    /// [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED`] so recovery is
    /// possible even if some keyframe responses are themselves lost. The
    /// global per-receiver ceiling ([`KEYFRAME_REQUEST_MAX_PER_SEC`]) is
    /// **never** relaxed, so the pre-existing keyframe-storm risk (OSS #814)
    /// stays bounded — this relaxes the cap, it does not remove it.
    ///
    /// ## #1297 — delivery-aware relaxation (the lossless-WS recovery path)
    ///
    /// The `congested` relaxation above can ONLY fire when the relay observed
    /// inbound-media loss for this receiver, which on a lossless WS/TCP path
    /// NEVER happens. So before #1297, a genuinely frozen receiver on the common
    /// all-WS deployment was throttled identically to a flooder and stayed
    /// frozen. The delivery-aware path fixes that: when the strict per-pair
    /// budget would DENY *and* this bucket is STILL WAITING for a keyframe
    /// (no qualifying frame delivered since the last request — see
    /// [`KeyframeRequestLimiter::observe_delivery`]) *and* at least
    /// [`KEYFRAME_REQUEST_STILL_WAITING_MIN_RETRY_MS`] has elapsed since the
    /// waiting flag was last (re)stamped, the request is ADMITTED — STILL
    /// subject to the unchanged global cap. Once a qualifying frame is delivered
    /// the waiting flag clears, so the strict budget re-engages and a receiver
    /// that keeps requesting AFTER recovery is throttled again
    /// (spammer-after-delivery cannot reopen the storm). This is the OPPOSITE of
    /// #1287 (publisher-side emit coalescing) — there is no publisher coalescer
    /// here.
    ///
    /// Behaviour:
    /// - If the per-pair bucket is full and the bucket is NOT still-waiting (or
    ///   the min-retry interval has not elapsed), returns `false` and does not
    ///   consume the global slot (so a deny on one target does not eat
    ///   budget intended for others).
    /// - If the per-pair bucket admits (strict/congested budget OR the
    ///   delivery-aware still-waiting path) but the global bucket is full,
    ///   returns `false`; any per-pair slot consumed is refunded so the
    ///   legitimate next pair retains its allowance, and the waiting flag is
    ///   NOT re-stamped (the request did not actually go through).
    /// - On a successful admit, the bucket is marked still-waiting
    ///   (`waiting_since = now`), bounding the next still-waiting allow to the
    ///   min-retry interval.
    ///
    /// Stale entries (target senders that have not been requested from
    /// for `KEYFRAME_REQUEST_WINDOW_MS * 10`) are cleaned up every
    /// [`KEYFRAME_LIMITER_CLEANUP_INTERVAL`] calls to bound memory; the
    /// waiting-state lives in the same entry so it prunes with it.
    pub fn allow_with_congestion(
        &mut self,
        target: KeyframeTarget,
        kind: KeyframeMediaKind,
        layer: u32,
        congested: bool,
    ) -> bool {
        let now = Instant::now();
        let window = Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);
        let min_retry = Duration::from_millis(KEYFRAME_REQUEST_STILL_WAITING_MIN_RETRY_MS);

        let per_pair_max = if congested {
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED
        } else {
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER
        };

        self.calls_since_cleanup = self.calls_since_cleanup.wrapping_add(1);
        if self.calls_since_cleanup >= KEYFRAME_LIMITER_CLEANUP_INTERVAL {
            self.calls_since_cleanup = 0;
            self.cleanup_stale_entries(now, window);
        }

        // Per-(pair, kind, layer) check first: this is the dimension that
        // actually discriminates a 16-sender fan-out — and a deliberate layer
        // switch (#989) / a SCREEN-vs-VIDEO recovery (#1297) — from sustained
        // abuse.
        //
        // #1068: CLAMP the layer dimension of the key to the realistic ladder
        // ceiling. `layer` is the cleartext, attacker-controllable
        // `simulcast_layer_id` (an unbounded `u32`), so without this a malicious
        // receiver could cycle DISTINCT ids against ONE sender to open unbounded
        // fresh per-layer buckets — each with its own per-pair budget — and
        // concentrate up to the global cap of keyframe pressure on that single
        // victim. Clamping to `0..=KEYFRAME_REQUEST_MAX_LAYER_ID` bounds the
        // buckets per target to `MAX + 1`; ids beyond the real ladder share the
        // top bucket's budget rather than opening new ones. Every REAL layer
        // switch (ids 0..=2 today) still gets its own bucket, so legitimate
        // clients are unaffected. The global per-receiver cap is unchanged.
        let key_layer = layer.min(KEYFRAME_REQUEST_MAX_LAYER_ID);
        let key = (target, kind, key_layer);

        // #1303: forged-target memory-amplification guard. `or_insert_with` below
        // opens a fresh bucket on every NEW key, and the global cap further down
        // only bounds FORWARDING, not map growth — so a receiver spraying
        // KEYFRAME_REQUESTs with distinct, client-controllable `target_session_id`s
        // could open one bucket per forged target, none stale for 10s (a memory
        // amplification vector, not a packet flood). Gate NEW-bucket creation on the
        // global cap: if this receiver's global window is already saturated, the
        // request cannot be forwarded regardless of target, so opening a bucket for
        // it would cost memory and accomplish nothing. EXISTING buckets bypass this
        // (so an established sender's strict/congested/#1297-still-waiting budgets
        // and refund path are all untouched), and a legitimate FRESH sender is only
        // refused a bucket while the receiver is already at its own
        // KEYFRAME_REQUEST_MAX_PER_SEC ceiling — where its request would be denied
        // forwarding anyway, and it opens a bucket on the next window. This bounds
        // the map at ~KEYFRAME_REQUEST_MAX_PER_SEC new buckets per window.
        if !self.per_target.contains_key(&key)
            && !self
                .global
                .has_capacity(now, window, KEYFRAME_REQUEST_MAX_PER_SEC)
        {
            return false;
        }

        let per_pair_entry = self
            .per_target
            .entry(key.clone())
            .or_insert_with(|| WindowCounter::new(now));

        // Try the strict/congested budget first. `try_consume` increments
        // `count` only when it admits.
        let consumed_per_pair = per_pair_entry.try_consume(now, window, per_pair_max);

        if !consumed_per_pair {
            // Strict/congested budget exhausted. #1297 delivery-aware path:
            // admit a retry ONLY while this bucket is still waiting for a
            // keyframe to be delivered, and no faster than the min-retry
            // interval. This is the relaxation a lossless WS/TCP path can reach
            // (the `congested` relaxation cannot fire there). It deliberately
            // does NOT consume the (already-full) per-pair counter; it is
            // bounded instead by `min_retry` here and the global cap below.
            let still_waiting_ok = match per_pair_entry.waiting_since {
                Some(since) => now.duration_since(since) >= min_retry,
                None => false,
            };
            if !still_waiting_ok {
                return false;
            }
            // Fall through to the global cap with consumed_per_pair == false
            // (nothing to refund on the per-pair side).
        }

        // Global cap as a defense-in-depth ceiling — NEVER relaxed, applies to
        // the delivery-aware path too. If exceeded, refund any per-pair slot we
        // consumed so legitimate distinct-target requests are not penalized for
        // hitting the global ceiling, and do NOT re-stamp the waiting flag (the
        // request did not go through).
        if !self
            .global
            .try_consume(now, window, KEYFRAME_REQUEST_MAX_PER_SEC)
        {
            // Refund the per-pair slot ONLY if the strict budget consumed one.
            // The delivery-aware path did not increment `count` (the per-pair
            // budget was already full), so there is nothing to refund there.
            if consumed_per_pair {
                // The entry is guaranteed to exist because we just
                // inserted/incremented it.
                if let Some(entry) = self.per_target.get_mut(&key) {
                    entry.count = entry.count.saturating_sub(1);
                }
            }
            return false;
        }

        // Admitted. Mark this bucket as (still) waiting for a keyframe so the
        // delivery-aware path can relax the next retry until a frame arrives,
        // and so the min-retry interval bounds that next still-waiting allow.
        // The entry is guaranteed to exist (we inserted it above).
        if let Some(entry) = self.per_target.get_mut(&key) {
            entry.waiting_since = Some(now);
        }

        true
    }

    /// Record that a qualifying keyframe-bearing MEDIA frame for `(target,
    /// kind)` was DELIVERED downstream (#1297), clearing the still-waiting flag
    /// so the strict per-pair budget re-engages on the next request.
    ///
    /// THE LAYER-JOIN (critical): a KEYFRAME_REQUEST always arrives with outer
    /// `simulcast_layer_id == 0` (the client never sets it on requests — see
    /// `classify_packet`), so the request consumed and set its waiting flag on
    /// the `(target, kind, 0)` bucket. Delivered simulcast media, by contrast,
    /// spans layers 0/1/2. To JOIN with the request bucket, the delivery clear
    /// NORMALIZES the layer to 0 and clears the `(target, kind, 0)` entry —
    /// regardless of which layer the delivered frame was on. This is correct for
    /// today's client (request layer always 0) and matches the client truth.
    /// (Forward concern, NOT built speculatively: if a future client sends a
    /// non-zero request layer, the waiting-set in `allow_with_congestion` and
    /// this clear must be reconciled to the same layer.)
    ///
    /// O(1): a single HashMap lookup + a field clear. It does NOT create an
    /// entry — only clears an existing one. Creating on delivery would be an
    /// unbounded-growth vector keyed by the attacker-forgeable outer
    /// `session_id` (delivery key option A — see `handle_outbound`); refusing to
    /// insert closes that.
    pub fn observe_delivery(&mut self, target: KeyframeTarget, kind: KeyframeMediaKind) {
        if let Some(entry) = self.per_target.get_mut(&(target, kind, 0u32)) {
            entry.waiting_since = None;
        }
    }

    /// Drop per-target entries whose window has been silent for
    /// `window * 10` to keep the table size bounded. The `waiting_since`
    /// state (#1297) lives in the same entries, so it prunes with them — no
    /// separate structure and no second prune pass.
    fn cleanup_stale_entries(&mut self, now: Instant, window: Duration) {
        let stale_threshold = window * 10;
        self.per_target
            .retain(|_, entry| now.duration_since(entry.window_start) <= stale_threshold);
    }
}

/// Cheap delivery-observation peek for an OUTBOUND forwarded frame (#1297).
///
/// Returns `Some((target, kind))` ONLY for a MEDIA packet whose OUTER cleartext
/// `media_kind` is VIDEO or SCREEN — the only deliveries that can clear a
/// keyframe wait. Returns `None` for everything else (non-MEDIA, AUDIO,
/// UNSPECIFIED, unparseable), so the caller does no per-frame map work for the
/// vast majority of traffic that cannot satisfy a keyframe request.
///
/// ## Why a partial decode (performance — this is the relay's hottest path)
///
/// `handle_outbound` runs once per forwarded frame PER RECEIVER. Each transport
/// handler ALREADY does one full `PacketWrapper::parse_from_bytes` per outbound
/// frame, and that full parse COPIES the multi-KB `data` (field 3) payload. A
/// second full parse here would DOUBLE that per-frame copy on the busiest path
/// in the system. Instead we walk the outer wrapper with the protobuf library's
/// own [`CodedInputStream`] primitives, reading only the three scalar fields we
/// need — `packet_type` (1), `session_id` (4), `media_kind` (6) — plus
/// `user_id` (2) for the [`KeyframeTarget::User`] fallback, and SKIPPING every
/// other field (crucially `data`, field 3) WITHOUT copying it. This is NOT a
/// hand-rolled byte scanner: tag reads, varint reads, and length-delimited
/// skips are all done by the library, so wire-format correctness is the
/// library's responsibility. The only manual step is the standard tag unpack
/// (`field_number = tag >> 3`, `wire_type = tag & 7`).
///
/// proto3 last-wins: if a (malformed) frame repeats a scalar field, the LAST
/// value wins, exactly matching `parse_from_bytes`. We loop to EOF rather than
/// stopping at the first match, so field order on the wire does not matter.
///
/// On ANY decode error we return `None` (fail-safe): the worst consequence of a
/// missed observation is that a receiver keeps its delivery-aware relaxation a
/// little longer (≤ the min-retry rate, still under the global cap) — never a
/// wrongful throttle and never a storm.
///
/// ## Delivery-key trust (option A — outer `session_id`, forgeable, bounded)
///
/// The authoritative publisher identity is the NATS subject (set by the relay,
/// unforgeable — see `chat_server::handle_msg` ~4199), NOT the outer
/// `session_id` (which a publisher can forge — ingress only stamps it when the
/// client sends 0). We key the delivery observation off the outer
/// `session_id`/`user_id` here (mirroring [`KeyframeTarget::from_request`]) to
/// keep `handle_outbound` self-contained and off the `Message` hot struct. The
/// abuse bound holds regardless of key fidelity: a publisher forging its OWN
/// media's outer `session_id` can only mis-key the waiting-flag CLEAR on the
/// receivers it sends to — at worst leaving a receiver's OWN waiting flag set so
/// that receiver's re-requests stay in the delivery-aware path. That is still
/// bounded by [`KEYFRAME_REQUEST_STILL_WAITING_MIN_RETRY_MS`] (≤ ~5/sec) AND the
/// unchanged global cap ([`KEYFRAME_REQUEST_MAX_PER_SEC`], 32/sec), and each
/// receiver has its OWN limiter, so it cannot starve or attack ANOTHER receiver.
/// `observe_delivery` also never CREATES an entry, so a forged key cannot grow
/// the map.
pub fn outbound_keyframe_observation(data: &[u8]) -> Option<(KeyframeTarget, KeyframeMediaKind)> {
    // Field numbers on PacketWrapper (see packet_wrapper.proto).
    const FIELD_PACKET_TYPE: u32 = 1;
    const FIELD_USER_ID: u32 = 2;
    const FIELD_SESSION_ID: u32 = 4;
    const FIELD_MEDIA_KIND: u32 = 6;

    let mut is = CodedInputStream::from_bytes(data);

    let mut packet_type: i32 = 0;
    let mut media_kind: i32 = 0;
    let mut session_id: u64 = 0;
    let mut user_id: Vec<u8> = Vec::new();

    loop {
        let raw_tag = match is.read_raw_tag_or_eof() {
            Ok(Some(t)) => t,
            Ok(None) => break, // clean EOF
            Err(_) => return None,
        };
        let field_number = raw_tag >> 3;
        // Unknown wire type (3/4 = legacy groups, or a malformed tag) → bail
        // (fail-safe None). `?` returns None on the `None` case.
        let wire_type = WireType::new(raw_tag & 0x7)?;
        match (field_number, wire_type) {
            (FIELD_PACKET_TYPE, WireType::Varint) => {
                packet_type = match is.read_enum_or_unknown::<PacketType>() {
                    Ok(v) => v.value(),
                    Err(_) => return None,
                };
            }
            (FIELD_USER_ID, WireType::LengthDelimited) => {
                user_id = match is.read_bytes() {
                    Ok(v) => v,
                    Err(_) => return None,
                };
            }
            (FIELD_SESSION_ID, WireType::Varint) => {
                session_id = match is.read_uint64() {
                    Ok(v) => v,
                    Err(_) => return None,
                };
            }
            (FIELD_MEDIA_KIND, WireType::Varint) => {
                media_kind = match is.read_enum_or_unknown::<MediaKind>() {
                    Ok(v) => v.value(),
                    Err(_) => return None,
                };
            }
            // Every other field — crucially `data` (field 3), the multi-KB
            // payload — is skipped WITHOUT copying. `skip_field` consumes the
            // value per its wire type (length-delimited → skip N bytes).
            (_, wt) => {
                if is.skip_field(wt).is_err() {
                    return None;
                }
            }
        }
    }

    // Only MEDIA deliveries can satisfy a keyframe request.
    if packet_type != PacketType::MEDIA.value() {
        return None;
    }
    // Map the outer cleartext media_kind to the relay-local kind. Only
    // VIDEO/SCREEN can clear a keyframe wait; AUDIO/UNSPECIFIED → None (no
    // observation), matching the documented degrade (an UNSPECIFIED-tagged
    // publisher simply never clears a Video/Screen wait).
    let media_kind = MediaKind::from_i32(media_kind).unwrap_or(MediaKind::MEDIA_KIND_UNSPECIFIED);
    let kind = match KeyframeMediaKind::from_outer(media_kind) {
        kind @ (KeyframeMediaKind::Video | KeyframeMediaKind::Screen) => kind,
        KeyframeMediaKind::Other => return None,
    };

    // Mirror the request-side target construction so the delivery key JOINS the
    // request key (session when set, else user_id — #1124).
    let target = KeyframeTarget::from_request(&user_id, session_id);
    Some((target, kind))
}

/// Maximum payload size for WebTransport datagrams (bytes).
///
/// Datagrams are used for control packets (heartbeats, RTT probes,
/// diagnostics) that are periodic and expendable. Media packets always use
/// reliable unidirectional streams. Control packets larger than this limit
/// also fall back to reliable streams.
///
/// Must match the client-side `DATAGRAM_MAX_SIZE` constant.
pub const DATAGRAM_MAX_SIZE: usize = 1200;

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Test-only helper functions
    //
    // These standalone is_* functions are used only by their own unit tests.
    // Production code uses `classify_packet()` instead.
    // =========================================================================

    /// Check if a packet is a CONGESTION packet (test-only helper).
    fn is_congestion_packet(data: &[u8]) -> bool {
        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            return packet_wrapper.packet_type == PacketType::CONGESTION.into();
        }
        false
    }

    /// Check if a packet is an RTT measurement packet (test-only helper).
    fn is_rtt_packet(data: &[u8]) -> bool {
        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            if packet_wrapper.packet_type == PacketType::MEDIA.into() {
                if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
                    return media_packet.media_type == MediaType::RTT.into();
                }
            }
        }
        false
    }

    /// Check if a MEDIA packet contains a KEYFRAME_REQUEST (test-only helper).
    fn is_keyframe_request(data: &[u8]) -> bool {
        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            if packet_wrapper.packet_type == PacketType::MEDIA.into() {
                if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
                    return media_packet.media_type == MediaType::KEYFRAME_REQUEST.into();
                }
            }
        }
        false
    }

    /// Test-only helper that replicates the datagram routing logic from
    /// `WtChatSession::send_auto`. Control packets (non-media) that fit
    /// within the datagram MTU use datagrams; media packets always use
    /// reliable streams. Empty/unparseable inputs are never routed via
    /// datagram.
    fn should_use_datagram(data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        if let Ok(pw) = PacketWrapper::parse_from_bytes(data) {
            let is_media = pw.packet_type == PacketType::MEDIA.into();
            return !is_media && data.len() <= DATAGRAM_MAX_SIZE;
        }
        false
    }

    #[test]
    fn test_classify_empty_packet_as_data() {
        assert_eq!(classify_packet(&[]), PacketKind::Data);
    }

    #[test]
    fn test_classify_garbage_as_data() {
        assert_eq!(classify_packet(&[1, 2, 3, 4, 5]), PacketKind::Data);
    }

    #[test]
    fn test_is_rtt_packet_with_garbage() {
        assert!(!is_rtt_packet(&[1, 2, 3, 4, 5]));
    }

    #[test]
    fn test_is_rtt_packet_with_empty() {
        assert!(!is_rtt_packet(&[]));
    }

    #[test]
    fn test_should_use_datagram_empty() {
        assert!(!should_use_datagram(&[]));
    }

    #[test]
    fn test_should_use_datagram_garbage() {
        assert!(!should_use_datagram(&[1, 2, 3, 4, 5]));
    }

    #[test]
    fn test_should_use_datagram_media_packet() {
        // MEDIA packets always use reliable streams (avoids artifacts)
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: vec![1, 2, 3], // small payload
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(bytes.len() <= DATAGRAM_MAX_SIZE);
        assert!(!should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_oversized_media_packet() {
        // Oversized MEDIA packets also use reliable streams
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: vec![0u8; DATAGRAM_MAX_SIZE + 100], // exceeds MTU
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_non_media_packet() {
        // Small AES_KEY packets use datagrams (control, expendable)
        let wrapper = PacketWrapper {
            packet_type: PacketType::AES_KEY.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_diagnostics_packet() {
        // Small DIAGNOSTICS packets use datagrams (periodic, expendable)
        let wrapper = PacketWrapper {
            packet_type: PacketType::DIAGNOSTICS.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_health_packet() {
        // Small HEALTH packets use datagrams (periodic, expendable)
        let wrapper = PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(should_use_datagram(&bytes));
    }

    #[test]
    fn test_should_use_datagram_oversized_control_packet() {
        // Control packets exceeding DATAGRAM_MAX_SIZE fall back to reliable stream
        let wrapper = PacketWrapper {
            packet_type: PacketType::DIAGNOSTICS.into(),
            data: vec![0u8; DATAGRAM_MAX_SIZE + 100],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!should_use_datagram(&bytes));
    }

    #[test]
    fn test_classify_congestion_packet_as_dropped() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::CONGESTION.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Dropped);
    }

    #[test]
    fn test_classify_layer_hint_packet_as_dropped() {
        // #1119: a client-sent LAYER_HINT is always forged (LAYER_HINT is
        // relay-authored-only) and must be dropped at ingest, never reflected to
        // the room — symmetric with the CONGESTION drop above.
        let wrapper = PacketWrapper {
            packet_type: PacketType::LAYER_HINT.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Dropped);
    }

    #[test]
    fn test_classify_keyframe_request() {
        // Build a KEYFRAME_REQUEST aimed at "alice" so we can also verify
        // that the inner MediaPacket.user_id is propagated through to the
        // PacketKind variant. This is what feeds the per-pair limiter key.
        let media = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            user_id: b"alice".to_vec(),
            target_session_id: 7777,
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::KeyframeRequest {
                target_user_id: b"alice".to_vec(),
                // #1124: the inner target_session_id must flow through so the
                // limiter can key per-session.
                target_session_id: 7777,
                // No simulcast_layer_id set on the wrapper → base/unspecified 0.
                layer: 0,
                // No inner `data` byte-string → Other (#1297).
                kind: KeyframeMediaKind::Other,
            }
        );
    }

    #[test]
    fn test_classify_keyframe_request_propagates_layer() {
        // The cleartext outer `simulcast_layer_id` (#989) must flow through to
        // the PacketKind so the limiter key is per-(target, layer).
        let media = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            user_id: b"alice".to_vec(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            simulcast_layer_id: 2,
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::KeyframeRequest {
                target_user_id: b"alice".to_vec(),
                target_session_id: 0,
                layer: 2,
                kind: KeyframeMediaKind::Other,
            }
        );
    }

    #[test]
    fn test_classify_keyframe_request_with_empty_target() {
        // A malformed KEYFRAME_REQUEST without a target user_id is still
        // classified as KeyframeRequest. The limiter then uses the empty
        // key, treating all such packets as a single bucket.
        let media = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::KeyframeRequest {
                target_user_id: Vec::new(),
                target_session_id: 0,
                layer: 0,
                kind: KeyframeMediaKind::Other,
            }
        );
    }

    #[test]
    fn test_classify_rtt_packet() {
        let media = MediaPacket {
            media_type: MediaType::RTT.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Rtt);
    }

    #[test]
    fn test_classify_health_packet() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Health);
    }

    #[test]
    fn test_classify_regular_media_as_data() {
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Data);
    }

    #[test]
    fn test_is_congestion_packet_true() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::CONGESTION.into(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(is_congestion_packet(&bytes));
    }

    #[test]
    fn test_is_congestion_packet_false_for_media() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!is_congestion_packet(&bytes));
    }

    #[test]
    fn test_is_keyframe_request_with_valid_packet() {
        let media = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(is_keyframe_request(&bytes));
    }

    #[test]
    fn test_is_keyframe_request_false_for_video() {
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!is_keyframe_request(&bytes));
    }

    #[test]
    fn test_is_keyframe_request_false_for_non_media() {
        let wrapper = PacketWrapper {
            packet_type: PacketType::AES_KEY.into(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(!is_keyframe_request(&bytes));
    }

    // =====================================================================
    // KeyframeRequestLimiter — per-pair behaviour
    // =====================================================================

    /// Test helper: a user-keyed target (the older-client fallback path).
    /// Most limiter-mechanics tests use this since they exercise the sliding
    /// window / global cap / cleanup regardless of which key variant is used.
    fn user_target(id: &[u8]) -> KeyframeTarget {
        KeyframeTarget::User(id.to_vec())
    }

    /// The media kind most limiter-mechanics tests pin against. These tests
    /// exercise the sliding window / global cap / cleanup, which are kind- and
    /// layer-agnostic; using a single fixed kind keeps them readable. Tests
    /// that specifically pin the VIDEO/SCREEN bucket SPLIT (#1297) name both
    /// kinds explicitly instead of using these helpers.
    const TEST_KIND: KeyframeMediaKind = KeyframeMediaKind::Video;

    /// `allow` with the default test kind ([`TEST_KIND`]). Forwards to the real
    /// `KeyframeRequestLimiter::allow`, so it still pins production behaviour.
    fn allow_v(limiter: &mut KeyframeRequestLimiter, target: KeyframeTarget, layer: u32) -> bool {
        limiter.allow(target, TEST_KIND, layer)
    }

    /// `allow_with_congestion` with the default test kind ([`TEST_KIND`]).
    /// Forwards to the real method, so it still pins production behaviour.
    fn allow_cong_v(
        limiter: &mut KeyframeRequestLimiter,
        target: KeyframeTarget,
        layer: u32,
        congested: bool,
    ) -> bool {
        limiter.allow_with_congestion(target, TEST_KIND, layer, congested)
    }

    /// A `WindowCounter` with no waiting state, for tests that synthesize map
    /// entries directly. Mirrors a fresh per-pair entry that has never issued a
    /// still-waiting allow.
    fn counter(count: u32, window_start: Instant) -> WindowCounter {
        WindowCounter {
            count,
            window_start,
            waiting_since: None,
        }
    }

    #[test]
    fn test_keyframe_limiter_allows_first_request_per_target() {
        let mut limiter = KeyframeRequestLimiter::new();
        assert!(allow_v(&mut limiter, user_target(b"sender-a"), 0));
    }

    #[test]
    fn test_keyframe_limiter_blocks_second_request_within_window_same_target() {
        // Same target, second request inside the window must be denied.
        // This is the classic per-pair throttle on a single relationship.
        let mut limiter = KeyframeRequestLimiter::new();
        assert!(allow_v(&mut limiter, user_target(b"sender-a"), 0));
        assert!(
            !allow_v(&mut limiter, user_target(b"sender-a"), 0),
            "second request to the same sender within 1s must be denied"
        );
    }

    #[test]
    fn test_keyframe_limiter_allows_fanout_across_distinct_targets() {
        // The frozen-video-on-join repro: a fresh joiner needs keyframes
        // from all 16 existing senders simultaneously. With the per-pair
        // limiter all 16 must succeed within the same second.
        let mut limiter = KeyframeRequestLimiter::new();
        for i in 0..16 {
            let target = format!("sender-{}", i);
            assert!(
                allow_v(&mut limiter, user_target(target.as_bytes()), 0),
                "first request to sender-{} should be allowed (i={})",
                i,
                i
            );
        }
    }

    #[test]
    fn test_keyframe_limiter_allows_same_target_after_window_elapses() {
        // Force the per-pair window to look elapsed by manually rewinding
        // the bucket's window_start. We avoid `tokio::time::sleep` so the
        // test stays cheap and deterministic.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = user_target(b"sender-x");
        assert!(allow_v(&mut limiter, target.clone(), 0));

        // Push the bucket's window_start ~1.5s into the past.
        let entry = limiter
            .per_target
            .get_mut(&(target.clone(), TEST_KIND, 0u32))
            .unwrap();
        entry.window_start =
            Instant::now() - Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS + 500);

        assert!(
            allow_v(&mut limiter, target, 0),
            "second request to the same sender after the window elapses must be allowed"
        );
    }

    #[test]
    fn test_keyframe_limiter_per_target_is_independent() {
        // Exhausting one (receiver, target) pair must not affect another.
        let mut limiter = KeyframeRequestLimiter::new();
        assert!(allow_v(&mut limiter, user_target(b"sender-a"), 0));
        assert!(!allow_v(&mut limiter, user_target(b"sender-a"), 0));

        // sender-b is a fresh pair — must still admit its first request.
        assert!(allow_v(&mut limiter, user_target(b"sender-b"), 0));
    }

    // =====================================================================
    // #1124: per-SESSION keying — the core acceptance proof
    // =====================================================================

    #[test]
    fn test_keyframe_target_from_request_prefers_session_then_user() {
        // The production builder (used at session_logic.rs's KEYFRAME_REQUEST
        // branch): a non-zero target_session_id keys by Session; 0 (older
        // client) falls back to User. Pins both branches directly.
        assert_eq!(
            KeyframeTarget::from_request(b"alice", 7),
            KeyframeTarget::Session(7),
            "a non-zero target_session_id must key by Session (#1124)"
        );
        assert_eq!(
            KeyframeTarget::from_request(b"alice", 0),
            KeyframeTarget::User(b"alice".to_vec()),
            "target_session_id == 0 (older client) must fall back to User"
        );
    }

    #[test]
    fn test_keyframe_limiter_concurrent_sessions_same_user_have_independent_budgets() {
        // #1124: two concurrent publishing SESSIONS of the SAME participant
        // must NOT collide into one rate-limit bucket. With per-session keying
        // (KeyframeTarget::Session), exhausting session A's per-pair budget
        // must leave session B's budget untouched.
        //
        // ADVERSARIAL (CLAUDE.md check #2): if the limiter reverted to keying
        // by user_id, both sessions would map to the same bucket and the
        // second assertion (session B admitted) would FAIL — so this test is
        // pinned to the real per-session behaviour, not a tautology.
        let mut limiter = KeyframeRequestLimiter::new();
        let session_a = KeyframeTarget::Session(1001);
        let session_b = KeyframeTarget::Session(1002);

        // Session A: first request admitted, second within the window denied
        // (strict per-pair budget) — exhausts A's bucket.
        assert!(allow_v(&mut limiter, session_a.clone(), 0));
        assert!(
            !allow_v(&mut limiter, session_a, 0),
            "session A's per-pair budget must be exhausted by its 2nd request"
        );

        // Session B (a DIFFERENT session of the same identity) must still be
        // admitted — independent budget. This is exactly what #1124 fixes.
        assert!(
            allow_v(&mut limiter, session_b, 0),
            "a concurrent session of the same user must have an INDEPENDENT \
             keyframe budget (#1124) — collision here means per-user keying"
        );
    }

    #[test]
    fn test_keyframe_limiter_session_and_user_targets_are_distinct_buckets() {
        // A session-keyed target and a user-keyed fallback are distinct keys,
        // so a new-client request (Session) and an old-client request (User)
        // for nominally the same participant do not share a bucket. This is
        // the documented, harmless consequence of the fallback (it can only
        // ever allow slightly more, never throttle legitimate traffic).
        let mut limiter = KeyframeRequestLimiter::new();
        assert!(allow_v(&mut limiter, KeyframeTarget::Session(2001), 0));
        // Exhaust the session bucket.
        assert!(!allow_v(&mut limiter, KeyframeTarget::Session(2001), 0));
        // The user-keyed fallback bucket is independent.
        assert!(
            allow_v(&mut limiter, user_target(b"some-user"), 0),
            "user-keyed fallback must not share a bucket with a session key"
        );
    }

    #[test]
    fn test_keyframe_limiter_per_layer_is_independent() {
        // #989, Phase 1b: the limiter key is (target, layer). Exhausting the
        // budget for one layer of a sender MUST NOT throttle a request for a
        // DIFFERENT layer of the SAME sender — otherwise a receiver switching
        // the simulcast layer it wants would have the newly-selected layer's
        // tile frozen until the window elapsed.
        let mut limiter = KeyframeRequestLimiter::new();
        // Saturate layer 1 for sender-a.
        assert!(allow_v(&mut limiter, user_target(b"sender-a"), 1));
        assert!(
            !allow_v(&mut limiter, user_target(b"sender-a"), 1),
            "second request for (sender-a, layer 1) within the window must be denied"
        );
        // A request for a DIFFERENT layer of the same sender is a fresh bucket.
        assert!(
            allow_v(&mut limiter, user_target(b"sender-a"), 2),
            "switching to layer 2 of the same sender must admit a fresh request \
             (not throttled as a duplicate)"
        );
        // Layer 0 (base) is also its own independent bucket.
        assert!(
            allow_v(&mut limiter, user_target(b"sender-a"), 0),
            "base layer 0 of the same sender must admit a fresh request"
        );
    }

    #[test]
    fn test_keyframe_limiter_layer_clamp_bounds_per_victim_pressure() {
        // #1068: a malicious receiver must NOT be able to cycle distinct
        // out-of-ladder `simulcast_layer_id`s against ONE sender to open
        // unbounded fresh per-layer buckets and drive per-victim keyframe
        // pressure up toward the global cap. The layer dimension of the key is
        // clamped to `0..=KEYFRAME_REQUEST_MAX_LAYER_ID`, so a single target has
        // at most `KEYFRAME_REQUEST_MAX_LAYER_ID + 1` distinct buckets — well
        // below the global cap of `KEYFRAME_REQUEST_MAX_PER_SEC` (~32).
        //
        // Sanity-check the test's own premise: without the clamp this attack
        // WOULD reach the global cap, so the constants must leave headroom for
        // the clamp to be the binding limit.
        let realistic_buckets = KEYFRAME_REQUEST_MAX_LAYER_ID + 1;
        assert!(
            realistic_buckets < KEYFRAME_REQUEST_MAX_PER_SEC,
            "clamp must bind BELOW the global cap, else this test proves nothing"
        );

        let mut limiter = KeyframeRequestLimiter::new();
        let victim = user_target(b"victim-sender");

        // Each distinct CLAMPED layer (0..=MAX) admits exactly one request in
        // the window (per-pair budget is 1/sec). All of these are real ladder
        // ids, so they map to distinct buckets and must all be admitted.
        let mut admitted = 0u32;
        for layer in 0..=KEYFRAME_REQUEST_MAX_LAYER_ID {
            assert!(
                allow_v(&mut limiter, victim.clone(), layer),
                "first request for clamped layer {layer} of the victim must be admitted"
            );
            admitted += 1;
        }

        // Now cycle MANY distinct OUT-OF-LADDER layer ids against the same
        // victim. Every one of these clamps onto the top bucket
        // (KEYFRAME_REQUEST_MAX_LAYER_ID), whose 1/sec budget was just consumed
        // above — so they must ALL be denied. Without the clamp each distinct id
        // would open a fresh bucket and admit, marching toward the global cap.
        for forged_layer in
            (KEYFRAME_REQUEST_MAX_LAYER_ID + 1)..=(KEYFRAME_REQUEST_MAX_LAYER_ID + 100)
        {
            assert!(
                !allow_v(&mut limiter, victim.clone(), forged_layer),
                "forged out-of-ladder layer {forged_layer} must collapse onto the clamped \
                 top bucket and be denied (no new per-layer budget)"
            );
        }

        // Per-victim pressure is therefore bounded to the clamped bucket count,
        // NOT the global cap.
        assert_eq!(
            admitted, realistic_buckets,
            "exactly KEYFRAME_REQUEST_MAX_LAYER_ID + 1 distinct layer buckets may admit per victim"
        );
        assert!(
            admitted < KEYFRAME_REQUEST_MAX_PER_SEC,
            "per-victim keyframe pressure must stay well under the global per-receiver cap"
        );
    }

    #[test]
    fn test_keyframe_limiter_global_cap_blocks_runaway_fanout() {
        // The defense-in-depth global cap kicks in when a single receiver
        // requests from more distinct targets than KEYFRAME_REQUEST_MAX_PER_SEC.
        let mut limiter = KeyframeRequestLimiter::new();
        for i in 0..KEYFRAME_REQUEST_MAX_PER_SEC {
            let target = format!("t-{}", i);
            assert!(allow_v(&mut limiter, user_target(target.as_bytes()), 0));
        }
        // One more distinct target inside the same window must be denied
        // by the global cap.
        let extra = format!("t-{}", KEYFRAME_REQUEST_MAX_PER_SEC);
        assert!(
            !allow_v(&mut limiter, user_target(extra.as_bytes()), 0),
            "global per-receiver cap must clamp runaway fan-out"
        );
    }

    #[test]
    fn test_keyframe_limiter_global_cap_does_not_consume_per_pair_budget_on_deny() {
        // When the global cap denies, the per-pair budget for the denied
        // target must be refunded so the legitimate next call (after the
        // global window elapses) is admitted.
        //
        // #1303: the new-bucket creation gate only applies to BRAND-NEW keys, so
        // the refund path now protects an ESTABLISHED pair. Establish `pair` FIRST
        // so the later globally-denied request is an existing key (bypasses the
        // gate) and actually reaches the per-pair consume → refund — otherwise the
        // gate would short-circuit before any per-pair slot is consumed.
        let mut limiter = KeyframeRequestLimiter::new();
        let pair = user_target(b"t-victim");

        // Establish the pair (consumes 1 global slot + its 1/window per-pair slot).
        assert!(allow_v(&mut limiter, pair.clone(), 0));

        // Fill the REST of the global cap with distinct targets (1 + (MAX-1) == MAX).
        for i in 0..(KEYFRAME_REQUEST_MAX_PER_SEC - 1) {
            let target = format!("t-{}", i);
            assert!(allow_v(&mut limiter, user_target(target.as_bytes()), 0));
        }

        // Rewind ONLY the pair's per-pair window so it has fresh per-pair budget,
        // while the global window stays full. The pair's next request admits
        // per-pair but is denied by the (full) global cap → the per-pair slot it
        // consumed must be refunded.
        let entry = limiter
            .per_target
            .get_mut(&(pair.clone(), TEST_KIND, 0u32))
            .unwrap();
        entry.window_start =
            Instant::now() - Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS + 500);

        assert!(
            !allow_v(&mut limiter, pair.clone(), 0),
            "an established pair must be denied by the full global cap"
        );

        // Manually expire only the global window (simulating ~1s passing
        // for the global cap while the per-pair entry was just refunded).
        limiter.global.window_start =
            Instant::now() - Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS + 500);

        // The per-pair budget was refunded, so the pair's first legitimate
        // request after the global cap reopens must be allowed.
        assert!(
            allow_v(&mut limiter, pair, 0),
            "per-pair budget must be refunded when global cap denies"
        );
    }

    #[test]
    fn test_keyframe_limiter_per_target_map_bounded_under_forged_target_spray() {
        // #1303: forged-target memory amplification. A receiver sprays
        // KEYFRAME_REQUESTs with MANY distinct, client-controllable target
        // session-ids in ONE window. Each forged target is a NEW key; without the
        // new-bucket creation gate each would open a fresh `per_target` entry (none
        // stale for 10s) and the map would grow to the spray volume — a memory
        // amplification vector (forwarding stays capped, so it is not a flood).
        // The gate refuses to open a bucket once the global cap is saturated, so the
        // map is bounded by the global cap regardless of how many targets are forged.
        let mut limiter = KeyframeRequestLimiter::new();

        const SPRAY: u64 = 1000;
        let mut admitted = 0usize;
        for i in 0..SPRAY {
            // Distinct forged target session-ids, all within one window (no time
            // passes between calls in a unit test).
            if limiter.allow(KeyframeTarget::Session(i), TEST_KIND, 0) {
                admitted += 1;
            }
        }

        // Only the global cap's worth of requests are admitted (and forwarded)...
        assert_eq!(
            admitted, KEYFRAME_REQUEST_MAX_PER_SEC as usize,
            "exactly the global cap's worth of distinct targets may be admitted in one window"
        );
        // ...and crucially the map did NOT grow with the spray: a globally-rejected
        // request opens no bucket. The bound is the global cap, not SPRAY.
        assert_eq!(
            limiter.per_target.len(),
            KEYFRAME_REQUEST_MAX_PER_SEC as usize,
            "per_target map must stay bounded by the global cap under a forged-target \
             spray, not grow to the spray volume"
        );
        assert!(
            (limiter.per_target.len() as u64) < SPRAY,
            "the map must not grow with the spray volume"
        );
    }

    // =====================================================================
    // KeyframeRequestLimiter — congestion-relaxed budget (issue #979)
    // =====================================================================

    #[test]
    fn test_keyframe_limiter_congested_admits_request_strict_would_deny() {
        // The core acceptance proof for issue #979: a per-pair request that
        // the strict steady-state budget (1/sec) would reject must be
        // admitted when the requesting receiver is flagged congested.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = user_target(b"frozen-sender");

        // First request always admitted under either budget.
        assert!(allow_cong_v(&mut limiter, target.clone(), 0, false));

        // Second request to the same target within the window is denied by
        // the strict per-pair budget...
        assert!(
            !allow_cong_v(&mut limiter, target.clone(), 0, false),
            "strict per-pair budget must deny the 2nd request within the window"
        );

        // ...but is admitted under the relaxed congested budget, letting a
        // frozen receiver re-request a keyframe to recover.
        assert!(
            allow_cong_v(&mut limiter, target, 0, true),
            "congested receiver must be allowed a relaxed retry (issue #979)"
        );
    }

    #[test]
    fn test_keyframe_limiter_congested_still_bounded_by_relaxed_per_pair() {
        // Relaxing the per-pair budget must NOT uncap it — the keyframe-storm
        // risk (OSS #814) requires the per-pair budget stay bounded. Exactly
        // KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED requests are
        // admitted within the window; the next is denied.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = user_target(b"sender-c");

        for i in 0..KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED {
            assert!(
                allow_cong_v(&mut limiter, target.clone(), 0, true),
                "congested request {i} within the relaxed budget must be admitted"
            );
        }
        assert!(
            !allow_cong_v(&mut limiter, target, 0, true),
            "relaxed per-pair budget must still be bounded (no uncapping — OSS #814)"
        );
    }

    #[test]
    fn test_keyframe_limiter_congested_does_not_relax_global_cap() {
        // The global per-receiver ceiling must NOT be relaxed by congestion:
        // it is the storm safety net. Saturate the global cap with distinct
        // targets, then verify a congested request to a fresh target is still
        // denied by the global ceiling.
        let mut limiter = KeyframeRequestLimiter::new();
        for i in 0..KEYFRAME_REQUEST_MAX_PER_SEC {
            let target = format!("g-{i}");
            assert!(allow_cong_v(
                &mut limiter,
                user_target(target.as_bytes()),
                0,
                true
            ));
        }
        let extra = format!("g-{KEYFRAME_REQUEST_MAX_PER_SEC}");
        assert!(
            !allow_cong_v(&mut limiter, user_target(extra.as_bytes()), 0, true),
            "global per-receiver cap must hold even under congestion (OSS #814)"
        );
    }

    #[test]
    fn test_keyframe_limiter_allow_matches_uncongested_path() {
        // `allow()` must behave identically to `allow_with_congestion(.., 0, false)`.
        let mut a = KeyframeRequestLimiter::new();
        let mut b = KeyframeRequestLimiter::new();
        let target = user_target(b"sender-eq");
        assert_eq!(
            allow_v(&mut a, target.clone(), 0),
            allow_cong_v(&mut b, target.clone(), 0, false)
        );
        assert_eq!(
            allow_v(&mut a, target.clone(), 0),
            allow_cong_v(&mut b, target, 0, false)
        );
    }

    // =====================================================================
    // #1297: delivery-aware relaxation + VIDEO/SCREEN bucket split
    // =====================================================================

    /// Rewind a per-pair bucket's `waiting_since` so the still-waiting min-retry
    /// interval looks elapsed, mirroring how the other tests rewind
    /// `window_start`. Avoids `sleep`, keeping the test cheap and deterministic.
    fn rewind_waiting(
        limiter: &mut KeyframeRequestLimiter,
        key: &(KeyframeTarget, KeyframeMediaKind, u32),
    ) {
        let entry = limiter
            .per_target
            .get_mut(key)
            .expect("bucket must exist (a request was admitted for it)");
        let since = entry
            .waiting_since
            .expect("bucket must be in the waiting state");
        entry.waiting_since =
            Some(since - Duration::from_millis(KEYFRAME_REQUEST_STILL_WAITING_MIN_RETRY_MS + 50));
    }

    #[test]
    fn test_keyframe_limiter_still_waiting_admits_retry_on_lossless_path() {
        // #1297 (a) — the core fix. On a LOSSLESS path (congested == false, so
        // the #979 relaxation can NEVER fire) a still-frozen receiver whose
        // strict 1/sec budget is exhausted must STILL be able to re-request once
        // the min-retry interval elapses, because no qualifying media has been
        // delivered to it (waiting flag still set). Before #1297 this second
        // request was dropped and the receiver stayed frozen.
        //
        // ADVERSARIAL (mutation): delete the `still_waiting_ok` branch in
        // `allow_with_congestion` (revert delivery-awareness) and the final
        // assertion FAILS — the strict budget alone denies the retry.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = user_target(b"frozen-on-ws");
        let key = (target.clone(), TEST_KIND, 0u32);

        // First request: admitted, sets the waiting flag.
        assert!(allow_cong_v(&mut limiter, target.clone(), 0, false));

        // Immediate second request: strict budget exhausted AND the min-retry
        // interval has NOT elapsed → denied. Proves the relaxation is not a
        // blanket "always allow when waiting".
        assert!(
            !allow_cong_v(&mut limiter, target.clone(), 0, false),
            "an immediate retry (before min-retry elapses) must still be denied"
        );

        // Simulate the min-retry interval elapsing while still waiting (no
        // delivery observed) by rewinding the waiting timestamp.
        rewind_waiting(&mut limiter, &key);

        // Now the still-waiting, lossless-path retry must be ADMITTED.
        assert!(
            allow_cong_v(&mut limiter, target, 0, false),
            "a still-waiting receiver on a lossless path must be allowed to \
             re-request once the min-retry interval elapses (#1297)"
        );
    }

    #[test]
    fn test_keyframe_limiter_video_and_screen_do_not_share_a_bucket() {
        // #1297 (b) — VIDEO and SCREEN keyframe requests must NOT collide into
        // one rate-limit bucket, or a SCREEN recovery is starved by VIDEO
        // requests in the same second.
        //
        // ADVERSARIAL (mutation): collapse the `kind` dimension out of the
        // per_target key (key = (target, layer)) and the SCREEN request below
        // maps to the now-exhausted shared bucket → the final assertion FAILS.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = user_target(b"sender-with-cam-and-screen");

        // Fill the VIDEO bucket for this sender.
        assert!(limiter.allow(target.clone(), KeyframeMediaKind::Video, 0));
        // A second VIDEO request within the window is denied (its bucket is
        // full and min-retry has not elapsed).
        assert!(
            !limiter.allow(target.clone(), KeyframeMediaKind::Video, 0),
            "second VIDEO request within the window must be denied"
        );
        // A SCREEN request is a DIFFERENT bucket and must be admitted even
        // though the VIDEO bucket is exhausted.
        assert!(
            limiter.allow(target, KeyframeMediaKind::Screen, 0),
            "SCREEN recovery must NOT be starved by a full VIDEO bucket (#1297)"
        );
    }

    #[test]
    fn test_keyframe_limiter_delivery_reengages_strict_budget() {
        // #1297 (c) — once a qualifying frame is DELIVERED, the waiting flag
        // clears and the strict per-pair budget re-engages, so a receiver that
        // keeps requesting AFTER recovery is throttled again. A
        // spammer-after-delivery cannot stay in the relaxed path.
        //
        // ADVERSARIAL (mutation): make `observe_delivery` a no-op (delivery
        // never re-engages the limiter) and the post-delivery request below
        // would be ALLOWED via the still-waiting path → the assertion FAILS.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = user_target(b"recovered-sender");
        let key = (target.clone(), TEST_KIND, 0u32);

        // Request admitted, waiting flag set.
        assert!(allow_cong_v(&mut limiter, target.clone(), 0, false));

        // Even after the min-retry interval elapses, a DELIVERED frame clears
        // the wait — so the strict budget re-engages and the next request is
        // denied. Rewind waiting first to prove it is the DELIVERY (not the
        // interval) that throttles: without the delivery clear, the rewound
        // wait would re-admit.
        rewind_waiting(&mut limiter, &key);
        limiter.observe_delivery(target.clone(), TEST_KIND);

        assert!(
            !allow_cong_v(&mut limiter, target, 0, false),
            "after a keyframe is delivered, the strict budget must re-engage and \
             throttle a receiver that keeps requesting (#1297)"
        );
    }

    #[test]
    fn test_keyframe_limiter_still_waiting_allow_bounded_by_global_cap() {
        // #1297 HARD CONSTRAINT — the still-waiting relaxation must REMAIN
        // subject to the unchanged global per-receiver cap
        // (KEYFRAME_REQUEST_MAX_PER_SEC). Set every distinct target into the
        // still-waiting state, elapse their min-retry, then drive still-waiting
        // retries: the total admitted across all targets in one window must not
        // exceed the global cap.
        //
        // ADVERSARIAL (mutation): if the still-waiting path skipped the global
        // `try_consume`, this would admit far more than the cap → the final
        // assertion FAILS.
        let mut limiter = KeyframeRequestLimiter::new();

        // Seed `2 * cap` distinct still-waiting targets with their first
        // (admitted) request. The first `cap` of these consume the global cap
        // for this window; the rest are denied at the global cap on their first
        // request (and so are NOT waiting). We only need the first `cap` to be
        // waiting for the retry phase.
        let cap = KEYFRAME_REQUEST_MAX_PER_SEC;
        for i in 0..cap {
            let t = user_target(format!("w-{i}").as_bytes());
            assert!(
                allow_cong_v(&mut limiter, t.clone(), 0, false),
                "seeding request {i} (within the global cap) must be admitted"
            );
            // Elapse the min-retry so each is eligible for a still-waiting retry.
            rewind_waiting(&mut limiter, &(t, TEST_KIND, 0u32));
        }

        // The global window is now full (cap admits consumed it). Every
        // still-waiting retry in the SAME window must be denied by the global
        // cap — proving the relaxation does not bypass the ceiling.
        let mut extra_admitted = 0u32;
        for i in 0..cap {
            let t = user_target(format!("w-{i}").as_bytes());
            if allow_cong_v(&mut limiter, t, 0, false) {
                extra_admitted += 1;
            }
        }
        assert_eq!(
            extra_admitted, 0,
            "still-waiting retries must be denied once the global per-receiver \
             cap is exhausted for the window (HARD CONSTRAINT, OSS #814)"
        );
    }

    #[test]
    fn test_keyframe_limiter_still_waiting_throttled_faster_than_min_retry() {
        // #1297 (e) — the min-retry interval bounds the still-waiting allow: a
        // receiver hammering FASTER than the interval is still throttled.
        //
        // ADVERSARIAL (mutation): remove the
        // `now.duration_since(since) >= min_retry` check (admit whenever
        // waiting) and the immediate re-request below would be ALLOWED → the
        // final assertion FAILS.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = user_target(b"hammering-receiver");
        let key = (target.clone(), TEST_KIND, 0u32);

        // First request admitted (waiting set). Strict budget now exhausted.
        assert!(allow_cong_v(&mut limiter, target.clone(), 0, false));

        // Elapse min-retry and take ONE still-waiting allow (this re-stamps
        // waiting_since = now).
        rewind_waiting(&mut limiter, &key);
        assert!(
            allow_cong_v(&mut limiter, target.clone(), 0, false),
            "the first still-waiting retry after min-retry must be admitted"
        );

        // Immediately hammer again: still waiting, but min-retry has NOT elapsed
        // since the re-stamp → must be denied.
        assert!(
            !allow_cong_v(&mut limiter, target, 0, false),
            "a still-waiting receiver hammering faster than the min-retry \
             interval must be throttled (#1297)"
        );
    }

    #[test]
    fn test_keyframe_limiter_waiting_state_pruned_by_cleanup() {
        // #1297 (d) — the waiting-state lives in the SAME per_target entry, so
        // it must prune via the SAME `cleanup_stale_entries` pass: a stale
        // entry that still carries a `waiting_since` flag must be removed (no
        // waiting-state leak).
        //
        // ADVERSARIAL (mutation): move `waiting_since` into a separate map that
        // cleanup does not touch and this entry's waiting-state would survive →
        // the assertion FAILS (here it can't even compile against a separate
        // map, which is the point: one structure, one prune).
        let mut limiter = KeyframeRequestLimiter::new();
        let now = Instant::now();
        let window = Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);

        // A stale entry (silent > 10*window) that is ALSO still flagged waiting.
        let stale_key = (
            KeyframeTarget::User(b"stale-waiter".to_vec()),
            TEST_KIND,
            0u32,
        );
        limiter.per_target.insert(
            stale_key.clone(),
            WindowCounter {
                count: 1,
                window_start: now - (window * 20),
                waiting_since: Some(now - (window * 20)),
            },
        );

        // Force cleanup on the next call.
        limiter.calls_since_cleanup = KEYFRAME_LIMITER_CLEANUP_INTERVAL - 1;
        assert!(allow_v(&mut limiter, user_target(b"trigger"), 0));

        assert!(
            !limiter.per_target.contains_key(&stale_key),
            "a stale entry must be pruned even though it still carried a \
             waiting flag — waiting-state must not leak (#1297)"
        );
    }

    // =====================================================================
    // #1297: KeyframeMediaKind mapping + delivery-observation parse
    // =====================================================================

    #[test]
    fn test_keyframe_media_kind_from_request_data() {
        // The request kind comes from the inner MediaPacket.data byte-string
        // (client truth). VIDEO/SCREEN map to their kinds; everything else is
        // the fail-open Other bucket.
        assert_eq!(
            KeyframeMediaKind::from_request_data(b"VIDEO"),
            KeyframeMediaKind::Video
        );
        assert_eq!(
            KeyframeMediaKind::from_request_data(b"SCREEN"),
            KeyframeMediaKind::Screen
        );
        assert_eq!(
            KeyframeMediaKind::from_request_data(b""),
            KeyframeMediaKind::Other,
            "empty/older-client data must fail open to Other"
        );
        assert_eq!(
            KeyframeMediaKind::from_request_data(b"AUDIO"),
            KeyframeMediaKind::Other,
            "AUDIO (never requested) and any unknown bytes map to Other"
        );
    }

    #[test]
    fn test_classify_keyframe_request_carries_kind_from_inner_data() {
        // End-to-end: classify_packet must lift the requested kind out of the
        // inner data byte-string. This pins the wire contract the client
        // already ships (no client companion change required).
        for (bytes, expect) in [
            (&b"VIDEO"[..], KeyframeMediaKind::Video),
            (&b"SCREEN"[..], KeyframeMediaKind::Screen),
        ] {
            let media = MediaPacket {
                media_type: MediaType::KEYFRAME_REQUEST.into(),
                user_id: b"alice".to_vec(),
                data: bytes.to_vec(),
                ..Default::default()
            };
            let wrapper = PacketWrapper {
                packet_type: PacketType::MEDIA.into(),
                data: media.write_to_bytes().unwrap(),
                ..Default::default()
            };
            let raw = wrapper.write_to_bytes().unwrap();
            assert_eq!(
                classify_packet(&raw),
                PacketKind::KeyframeRequest {
                    target_user_id: b"alice".to_vec(),
                    target_session_id: 0,
                    layer: 0,
                    kind: expect,
                },
                "classify_packet must carry the requested kind from inner data"
            );
        }
    }

    #[test]
    fn test_outbound_observation_matches_request_target_and_kind() {
        // The delivery observation must JOIN the request: a request derives its
        // (target, kind) from the INNER bytes; a delivered frame derives the
        // SAME (target, kind) from the OUTER session_id + media_kind. Build a
        // delivered VIDEO frame from publisher session 555 and confirm the peek
        // yields the same key a request for that publisher+VIDEO would set.
        let delivered = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            session_id: 555,
            media_kind: MediaKind::VIDEO.into(),
            // A realistic multi-KB payload to prove the peek skips `data`
            // without choking and without needing to copy it.
            data: vec![0u8; 4096],
            ..Default::default()
        };
        let raw = delivered.write_to_bytes().unwrap();
        assert_eq!(
            outbound_keyframe_observation(&raw),
            Some((KeyframeTarget::Session(555), KeyframeMediaKind::Video)),
            "a delivered VIDEO frame must observe (Session(publisher), Video)"
        );

        // And that join actually CLEARS a wait set by the matching request.
        let mut limiter = KeyframeRequestLimiter::new();
        // Request for publisher session 555, VIDEO, layer 0 (as the client
        // always sends — outer layer 0).
        assert!(limiter.allow(KeyframeTarget::Session(555), KeyframeMediaKind::Video, 0));
        let (target, kind) = outbound_keyframe_observation(&raw).unwrap();
        limiter.observe_delivery(target, kind);
        // The waiting flag for (Session(555), Video, 0) must now be cleared.
        let entry = limiter
            .per_target
            .get(&(KeyframeTarget::Session(555), KeyframeMediaKind::Video, 0u32))
            .expect("the request must have created the bucket");
        assert!(
            entry.waiting_since.is_none(),
            "delivery of matching VIDEO media must clear the request's waiting flag"
        );
    }

    #[test]
    fn test_outbound_observation_layer_zero_join_for_simulcast_delivery() {
        // THE LAYER-JOIN TRAP: a request always arrives at outer layer 0, so it
        // sets its waiting flag on the (target, kind, 0) bucket. Delivered
        // simulcast media may carry a NON-ZERO simulcast_layer_id (layer 1/2).
        // The observation must STILL clear the layer-0 request bucket — the
        // clear normalizes to layer 0. If it keyed off the delivered layer, the
        // flag would never clear and the feature would be inert.
        //
        // ADVERSARIAL (mutation): change `observe_delivery` to key off a
        // non-zero layer and this assertion FAILS.
        let mut limiter = KeyframeRequestLimiter::new();
        // Request: publisher 777, SCREEN, outer layer 0 (client truth).
        assert!(limiter.allow(KeyframeTarget::Session(777), KeyframeMediaKind::Screen, 0));

        // Delivered SCREEN media on simulcast layer 2.
        let delivered = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            session_id: 777,
            media_kind: MediaKind::SCREEN.into(),
            simulcast_layer_id: 2,
            data: vec![1u8; 1024],
            ..Default::default()
        };
        let raw = delivered.write_to_bytes().unwrap();
        let (target, kind) = outbound_keyframe_observation(&raw).unwrap();
        limiter.observe_delivery(target, kind);

        let entry = limiter
            .per_target
            .get(&(
                KeyframeTarget::Session(777),
                KeyframeMediaKind::Screen,
                0u32,
            ))
            .expect("the request must have created the layer-0 bucket");
        assert!(
            entry.waiting_since.is_none(),
            "delivery on layer 2 must clear the layer-0 request bucket (#1297 join)"
        );
    }

    #[test]
    fn test_outbound_observation_ignores_non_qualifying_frames() {
        // Non-MEDIA, AUDIO, UNSPECIFIED, and unparseable frames must yield no
        // observation (None) so the hot path does no map work for them and an
        // UNSPECIFIED-tagged publisher cannot clear a Video/Screen wait.
        let audio = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            session_id: 9,
            media_kind: MediaKind::AUDIO.into(),
            data: vec![0u8; 256],
            ..Default::default()
        };
        assert_eq!(
            outbound_keyframe_observation(&audio.write_to_bytes().unwrap()),
            None,
            "AUDIO delivery cannot satisfy a keyframe request"
        );

        let unspecified = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            session_id: 9,
            // media_kind left UNSPECIFIED (0)
            data: vec![0u8; 256],
            ..Default::default()
        };
        assert_eq!(
            outbound_keyframe_observation(&unspecified.write_to_bytes().unwrap()),
            None,
            "UNSPECIFIED outer media_kind yields no observation (documented degrade)"
        );

        let non_media = PacketWrapper {
            packet_type: PacketType::AES_KEY.into(),
            session_id: 9,
            data: vec![0u8; 256],
            ..Default::default()
        };
        assert_eq!(
            outbound_keyframe_observation(&non_media.write_to_bytes().unwrap()),
            None,
            "non-MEDIA frames are never a keyframe delivery"
        );

        assert_eq!(
            outbound_keyframe_observation(&[1, 2, 3, 0xff]),
            None,
            "unparseable bytes fail safe to no observation"
        );
    }

    #[test]
    fn test_keyframe_limiter_cleanup_removes_only_stale_entries() {
        // Insert a synthetic stale entry (silent for >10*window) and a
        // synthetic fresh entry. After cleanup runs only the fresh one
        // (and any newly active pair) survives.
        let mut limiter = KeyframeRequestLimiter::new();
        let now = Instant::now();
        let window = Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);

        limiter.per_target.insert(
            (KeyframeTarget::User(b"stale".to_vec()), TEST_KIND, 0u32),
            counter(0, now - (window * 20)),
        );
        limiter.per_target.insert(
            (KeyframeTarget::User(b"fresh".to_vec()), TEST_KIND, 0u32),
            counter(0, now),
        );

        // Force the next allow() call to trigger cleanup.
        limiter.calls_since_cleanup = KEYFRAME_LIMITER_CLEANUP_INTERVAL - 1;
        assert!(allow_v(&mut limiter, user_target(b"trigger"), 0));

        assert!(
            !limiter.per_target.contains_key(&(
                KeyframeTarget::User(b"stale".to_vec()),
                TEST_KIND,
                0u32
            )),
            "stale entry must be removed by cleanup"
        );
        assert!(
            limiter.per_target.contains_key(&(
                KeyframeTarget::User(b"fresh".to_vec()),
                TEST_KIND,
                0u32
            )),
            "fresh entry must be retained by cleanup"
        );
        assert!(
            limiter.per_target.contains_key(&(
                KeyframeTarget::User(b"trigger".to_vec()),
                TEST_KIND,
                0u32
            )),
            "the active pair that triggered cleanup must remain"
        );
    }

    #[test]
    fn test_keyframe_limiter_cleanup_does_not_evict_active_pair_state() {
        // Required by the change spec: cleanup must not prematurely clear
        // active-pair state. Specifically, an entry whose window_start is
        // `now - window * 5` (well within the 10x boundary) must survive.
        let mut limiter = KeyframeRequestLimiter::new();
        let now = Instant::now();
        let window = Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);

        limiter.per_target.insert(
            (KeyframeTarget::User(b"active".to_vec()), TEST_KIND, 0u32),
            // count: 1 — mid-window allowance already consumed.
            counter(1, now - (window * 5)),
        );

        limiter.calls_since_cleanup = KEYFRAME_LIMITER_CLEANUP_INTERVAL - 1;
        assert!(allow_v(&mut limiter, user_target(b"unrelated"), 0));

        let entry = limiter
            .per_target
            .get(&(KeyframeTarget::User(b"active".to_vec()), TEST_KIND, 0u32))
            .expect("active pair must survive cleanup");
        assert_eq!(
            entry.count, 1,
            "active pair's count must not be reset by cleanup"
        );
    }

    #[test]
    fn test_keyframe_limiter_cleanup_amortized_not_every_call() {
        // The cleanup pass must run only every KEYFRAME_LIMITER_CLEANUP_INTERVAL
        // calls, not on every single allow(). Insert a stale entry and verify
        // it survives until the cleanup boundary is crossed.
        let mut limiter = KeyframeRequestLimiter::new();
        let now = Instant::now();
        let window = Duration::from_millis(KEYFRAME_REQUEST_WINDOW_MS);

        limiter.per_target.insert(
            (KeyframeTarget::User(b"stale".to_vec()), TEST_KIND, 0u32),
            counter(0, now - (window * 20)),
        );

        // Issue strictly fewer calls than the cleanup threshold. Use a
        // distinct fresh target each call to avoid global cap denial.
        for i in 0..(KEYFRAME_LIMITER_CLEANUP_INTERVAL - 1) {
            let target = format!("tick-{}", i);
            // Some calls will be denied by the global cap once it fills;
            // we don't care about return value, only that we drove the
            // call counter close to the boundary.
            let _ = allow_v(&mut limiter, user_target(target.as_bytes()), 0);
        }

        assert!(
            limiter.per_target.contains_key(&(
                KeyframeTarget::User(b"stale".to_vec()),
                TEST_KIND,
                0u32
            )),
            "stale entry must persist below the cleanup threshold (amortized)"
        );
    }
}
