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
use videocall_types::protos::meeting_timer_packet::MeetingTimerPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::raise_hand_packet::RaiseHandPacket;
use videocall_types::protos::reaction_packet::reaction_packet::ReactionType;
use videocall_types::protos::reaction_packet::ReactionPacket;

use crate::constants::{
    KEYFRAME_LIMITER_CLEANUP_INTERVAL, KEYFRAME_REQUEST_MAX_LAYER_ID, KEYFRAME_REQUEST_MAX_PER_SEC,
    KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER, KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED,
    KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN,
    KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN_CONGESTED,
    KEYFRAME_REQUEST_STILL_WAITING_MIN_RETRY_MS, KEYFRAME_REQUEST_WINDOW_MS,
    MEETING_TIMER_MAX_DURATION_MS, MEETING_TIMER_MAX_PER_WINDOW, MEETING_TIMER_PACKET_MAX_BYTES,
    MEETING_TIMER_WINDOW_MS, RAISE_HAND_MAX_PER_WINDOW, RAISE_HAND_PACKET_MAX_BYTES,
    RAISE_HAND_WINDOW_MS, REACTION_CUSTOM_EMOJI_MAX_BYTES, REACTION_MAX_PER_WINDOW,
    REACTION_WINDOW_MS,
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

/// The per-`(receiver, target_sender)` KEYFRAME_REQUEST budget that applies to a
/// request of `kind`, given whether the receiver is currently `congested`
/// (issue #1899).
///
/// This is the SINGLE source of truth for the per-pair cap: both the enforcement
/// site ([`KeyframeRequestLimiter::allow_with_congestion`]) and the rate-limit
/// `warn!` log in `SessionLogic::handle_inbound` call it, so the value the log
/// reports can never drift from the value the limiter actually enforced.
///
/// - **SCREEN** gets the raised budgets ([`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN`]
///   / [`..._SCREEN_CONGESTED`]) because a static screen share has no
///   inter-frame fallback — a missed keyframe freezes the tile indefinitely, so
///   its steady-state cap must permit prompt re-request (see the constants' docs
///   for the field rationale).
/// - **VIDEO** keeps the original camera budgets
///   ([`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`] / [`..._CONGESTED`])
///   byte-for-byte — the #1479 PLI-storm protection for camera is unchanged.
/// - **Other** is the fail-open bucket for AUDIO / `MEDIA_KIND_UNSPECIFIED` / an
///   unrecognised (older-client, forged, or — should it ever become unreadable —
///   E2EE-obscured) request byte-string. It maps to the CAMERA budget: the
///   conservative default is to treat an unknown kind as the tighter,
///   storm-safe camera cap, never to hand out SCREEN's wider budget on an
///   unverified kind. (Today a KEYFRAME_REQUEST's inner `MediaPacket` — including
///   the `b"VIDEO"`/`b"SCREEN"` marker — is cleartext even under E2EE, so a
///   current client's SCREEN request is correctly classified; `Other` is the
///   documented, safe-by-construction fallback for everything else.)
pub(crate) fn keyframe_per_pair_budget(kind: KeyframeMediaKind, congested: bool) -> u32 {
    match (kind, congested) {
        (KeyframeMediaKind::Screen, false) => KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN,
        (KeyframeMediaKind::Screen, true) => {
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN_CONGESTED
        }
        (_, false) => KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER,
        (_, true) => KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED,
    }
}

/// Classification of an incoming packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketKind {
    /// RTT (Round-Trip Time) packet - should be echoed back to sender
    Rtt,
    /// Health diagnostics packet - should be processed for metrics
    Health,
    /// Normal opaque data packet - should be forwarded to ChatServer
    Data,
    /// Regular inbound MEDIA frame - should be forwarded to ChatServer.
    ///
    /// Carries only bounded relay-readable observation metadata. The cleartext
    /// outer `media_kind` is available even under E2EE; `frame_kind` is `Unknown`
    /// when the inner `MediaPacket` is sealed or otherwise unreadable.
    Media {
        media_kind: MediaKind,
        frame_kind: InboundFrameKind,
    },
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
    /// REACTION packet (#1884) that PASSED closed-enum ingress validation in
    /// [`classify_packet`] (the inner cleartext `ReactionPacket` parsed and its
    /// `reaction` is a defined, non-`UNSPECIFIED` value). It is subject to the
    /// per-sender [`ReactionRateLimiter`] in `SessionLogic::handle_inbound`;
    /// within budget it is FORWARDED on the standard media fan-out (re-broadcast
    /// to the room, sender self-skipped), over budget it is dropped as Processed.
    ///
    /// A distinct variant (rather than plain [`PacketKind::Data`]) is what lets
    /// the stateful per-session limiter run in `handle_inbound` while the
    /// stateless enum validation stays here — so an INVALID reaction is
    /// classified [`PacketKind::Dropped`] and never reaches the limiter, and a
    /// flood of invalid reactions cannot consume a sender's valid budget.
    Reaction,
    /// RAISE_HAND packet (#2135) that PASSED ingress validation in
    /// [`classify_packet`] (the raw inner payload is within
    /// [`RAISE_HAND_PACKET_MAX_BYTES`] and parses as a `RaiseHandPacket`). It is
    /// subject to the per-sender [`RaiseHandRateLimiter`] in
    /// `SessionLogic::handle_inbound`; within budget it is FORWARDED on the
    /// standard media fan-out (re-broadcast to the room, sender self-skipped),
    /// over budget it is dropped as Processed.
    ///
    /// Mirrors [`PacketKind::Reaction`]'s split of responsibilities — stateless
    /// validation here, stateful metering in `handle_inbound` — so a flood of
    /// OVERSIZED or unparseable raise-hand packets cannot consume a sender's
    /// valid budget.
    ///
    /// Unlike a REACTION there is no closed enum to allowlist: the payload is a
    /// `bool` plus two cosmetic/advisory fields, so "valid" here means
    /// "well-formed and bounded", and a `raised = false` default (from an empty
    /// or truncated payload) is the SAFE degradation — see the proto doc.
    RaiseHand,
    /// MEETING_TIMER packet (#2136) that PASSED ingress validation in
    /// [`classify_packet`] (raw payload within
    /// [`MEETING_TIMER_PACKET_MAX_BYTES`], inner `MeetingTimerPacket` parses,
    /// and `duration_ms` within [`MEETING_TIMER_MAX_DURATION_MS`]).
    ///
    /// It is subject to the per-sender [`MeetingTimerRateLimiter`] in
    /// `SessionLogic::handle_inbound`; within budget it becomes an
    /// `InboundAction::ForwardHostOnly`, which the `chat_server` fan-out funnel
    /// admits ONLY if the sending session is the room's current host.
    ///
    /// AUTHORITY IS NOT CHECKED HERE, AND CANNOT BE. `classify_packet` is a free
    /// function over raw bytes with no session context, and the session-local
    /// `SessionLogic::is_host` it could otherwise reach is a JWT-claim snapshot
    /// that goes stale across a transfer-host. See `PacketKind::MeetingTimer`'s
    /// handling in `session_logic.rs` and `session_is_room_host` in
    /// `chat_server.rs` for where the real gate lives and why.
    MeetingTimer,
}

/// Bounded frame-kind label for publisher-leg inbound-arrival instrumentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundFrameKind {
    Key,
    Delta,
    Unknown,
}

impl InboundFrameKind {
    pub fn as_label(self) -> &'static str {
        match self {
            InboundFrameKind::Key => "key",
            InboundFrameKind::Delta => "delta",
            InboundFrameKind::Unknown => "unknown",
        }
    }
}

/// True iff `bytes` is a valid CUSTOM-reaction `custom_emoji` payload: a single
/// standard Unicode emoji on the exact `emojis` allowlist, within the byte cap
/// (issue 1884).
///
/// This is the RELAY half of the CUSTOM allowlist and MUST stay in lockstep with
/// the client's `validate_custom_emoji` in
/// `videocall-client/src/client/reactions.rs` — the SAME predicate
/// (`len <= cap && emojis::get(s).is_some()`) over the SAME `emojis` 0.9 table.
/// The relay adds exactly ONE term the client cannot need: a UTF-8
/// well-formedness gate, because the client only ever validates a Rust `&str`
/// (already valid UTF-8) whereas the relay validates RAW wire bytes a malicious
/// or old sender may have left as invalid UTF-8. Empty rejects (the empty string
/// is not a table entry — same as the client). `emojis::get` is an EXACT lookup,
/// so a within-cap ZWJ sequence or flag validates, but two concatenated emoji or
/// trailing markup do not; the `len <= cap` term independently rejects the
/// 35-byte full-table skin-tone variants the picker never offers (see the
/// `REACTION_CUSTOM_EMOJI_MAX_BYTES` doc for the byte budget).
///
/// Fail-closed by construction: any failed term returns `false` and the caller
/// ([`classify_packet`]) drops the packet. The relay is the sole ingress
/// enforcement point that protects OLD clients and non-conforming senders — the
/// proto threat model puts the allowlist HERE.
pub fn custom_emoji_is_valid(bytes: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(bytes) else {
        return false;
    };
    s.len() <= REACTION_CUSTOM_EMOJI_MAX_BYTES && emojis::get(s).is_some()
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

    // Drop client-originated DOWNLINK_CONGESTION packets (#1219 Half 2).
    // DOWNLINK_CONGESTION is relay-authored (emitted by the relay when a
    // receiver's outbound channel overflows, as observed by the windowed
    // CongestionTracker via on_outbound_drop). A client-sent one is always
    // forged. Drop it to prevent a client from injecting fake congestion
    // signals that would trick OTHER receivers into stepping down their layers.
    if packet_wrapper.packet_type == PacketType::DOWNLINK_CONGESTION.into() {
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

    // Validate and classify client-originated REACTION packets (#1884).
    //
    // A REACTION is the ONLY client-authored packet the relay RE-BROADCASTS to
    // the whole room (on the media fan-out) — unlike VIEWPORT/LAYER_PREFERENCE,
    // which the relay consumes and never re-broadcasts. That broadcast reach is
    // why its content is validated at ingress: the inner `ReactionPacket` is
    // CLEARTEXT (never AES-sealed, even under E2EE — see the proto doc)
    // precisely so the relay can enforce the closed-enum allowlist HERE. Parse
    // it; a reaction that is UNSPECIFIED(0), an unknown/reserved value, or
    // unparseable is dropped as Processed (no fan-out). A CUSTOM(12) reaction
    // additionally must carry a valid `custom_emoji` (the SAME exact-emoji
    // allowlist the client enforces — see `custom_emoji_is_valid`), while a
    // built-in glyph must NOT carry one; both are enforced in the match below so
    // the invariant "custom_emoji is meaningful iff CUSTOM" holds room-wide.
    //
    // Validation runs HERE, BEFORE the per-sender `ReactionRateLimiter` in
    // `SessionLogic::handle_inbound`, so a flood of INVALID reactions is
    // discarded without ever reaching (and thus without consuming) a sender's
    // valid-budget window. A VALID reaction becomes `PacketKind::Reaction`; the
    // stateful limiter then decides forward-vs-drop. Both the WS and WT paths
    // route inbound through the shared `handle_inbound` → `classify_packet`, so
    // this single arm is the sole enforcement point across transports.
    if packet_wrapper.packet_type == PacketType::REACTION.into() {
        let Ok(reaction) = ReactionPacket::parse_from_bytes(&packet_wrapper.data) else {
            return PacketKind::Dropped;
        };
        // `enum_value()` returns `Err(raw)` for an unknown/reserved wire value
        // (the closed-enum drop signal); `UNSPECIFIED(0)` is the proto3 default
        // and is not a real reaction. Both drop. Every OTHER defined value can
        // forward, but CUSTOM(12) and the built-in glyphs have DIFFERENT
        // `custom_emoji` contracts, enforced by the two arms below (issue 1884).
        return match reaction.reaction.enum_value() {
            Ok(ReactionType::REACTION_TYPE_UNSPECIFIED) | Err(_) => PacketKind::Dropped,
            // CUSTOM carries a picker-selected emoji in `custom_emoji`. The relay
            // enforces the SAME exact-emoji allowlist the client does, fail-closed
            // for OLD/forged senders: empty, markup, multi-emoji, invalid UTF-8,
            // or over-cap all drop here.
            Ok(ReactionType::CUSTOM) if custom_emoji_is_valid(&reaction.custom_emoji) => {
                PacketKind::Reaction
            }
            Ok(ReactionType::CUSTOM) => PacketKind::Dropped,
            // A built-in glyph (1..=11) has NO legitimate `custom_emoji`. A
            // non-empty field here is smuggling — drop to keep the invariant
            // "custom_emoji is meaningful IFF CUSTOM" true for every downstream
            // consumer (renderer, stamp path, future features). OLD clients never
            // set field 3, so this cannot affect them.
            Ok(_) if !reaction.custom_emoji.is_empty() => PacketKind::Dropped,
            Ok(_) => PacketKind::Reaction,
        };
    }

    // Validate and classify client-originated RAISE_HAND packets (#2135).
    //
    // Like REACTION this is a client-authored packet the relay RE-BROADCASTS to
    // the whole room, so its content is validated at ingress. It has no closed
    // enum to allowlist (the payload is a `bool` plus two cosmetic/advisory
    // fields), so validation is two terms:
    //
    //  1. SIZE. The raw inner payload must be within
    //     `RAISE_HAND_PACKET_MAX_BYTES`. This is checked FIRST, on the raw
    //     bytes, BEFORE the parse — both because it is the cheap term and
    //     because it is the ONLY term that bounds the packet at all.
    //     rust-protobuf preserves UNKNOWN FIELDS across parse/serialize (needed
    //     so a newer client's field survives an older relay), which means the
    //     `display_name` cap applied later does NOT bound the payload; without
    //     this check a forged RAISE_HAND stuffed with megabytes of unknown
    //     fields would be re-broadcast verbatim to every participant. See the
    //     constant's doc for the byte budget.
    //  2. WELL-FORMEDNESS. It must parse as a `RaiseHandPacket`; unparseable
    //     drops (fail-closed), so the fan-out never carries bytes the relay
    //     could not decode.
    //
    // There is deliberately NO validation of `raised`, `raised_at_ms`, or
    // `display_name` CONTENT here:
    //   * `raised` is a bool — every wire value is meaningful, and the proto3
    //     default (`false`, which an empty payload decodes to) is the SAFE
    //     degradation: combined with the relay-stamped envelope session_id
    //     (#2124), the worst a corrupt payload can do is lower the SENDER'S OWN
    //     hand.
    //   * `raised_at_ms` is any u64 — there is no syntactically invalid value,
    //     and every rewrite rule that would blunt a forged one (clamp to the
    //     future, clamp to a floor) ALSO breaks legitimate re-announce of a hand
    //     raised minutes ago. It is documented as an advisory display-ordering
    //     hint, never authorization. Adding a clamp here would be inert code
    //     that looks like security.
    //   * `display_name` is BOUNDED rather than validated, in
    //     `stamp_raise_hand_for_broadcast` on the forward path — truncated, not
    //     dropped, because the hand STATE is valid and discarding it over a
    //     cosmetic field would leave the room's view of that participant wrong.
    //
    // Validation runs HERE, BEFORE the per-sender `RaiseHandRateLimiter` in
    // `SessionLogic::handle_inbound`, so a flood of oversized/garbage raise-hand
    // packets is discarded without consuming a sender's valid-budget window.
    // Both the WS and WT paths route inbound through the shared
    // `handle_inbound` → `classify_packet`, so this single arm is the sole
    // enforcement point across transports.
    if packet_wrapper.packet_type == PacketType::RAISE_HAND.into() {
        if packet_wrapper.data.len() > RAISE_HAND_PACKET_MAX_BYTES {
            return PacketKind::Dropped;
        }
        return match RaiseHandPacket::parse_from_bytes(&packet_wrapper.data) {
            Ok(_) => PacketKind::RaiseHand,
            Err(_) => PacketKind::Dropped,
        };
    }

    // Validate and classify host-originated MEETING_TIMER packets (#2136).
    //
    // NOT the MEETING arm above, despite the adjacent name — the two have
    // OPPOSITE trust models and this arm sits directly below the drop so the
    // contrast is impossible to miss. MEETING (7) is server-authored by
    // meeting-api and a client-sent one is ALWAYS forged, hence the
    // unconditional drop. MEETING_TIMER (19) is CLIENT-authored by the meeting
    // host and re-broadcast, like REACTION (17).
    //
    // Two things are validated here, and one deliberately is not:
    //
    //  * A SIZE CAP on the inner payload, checked BEFORE the inner parse.
    //    rust-protobuf preserves unknown fields across parse/serialize
    //    (deliberately — a newer client's field must survive an older relay), so
    //    a forged MEETING_TIMER stuffed with megabytes of unknown fields would
    //    otherwise be re-broadcast verbatim to every participant. See
    //    `MEETING_TIMER_PACKET_MAX_BYTES` for what this does and does NOT bound
    //    (it is the inner payload only; the outer wrapper is bounded by
    //    MAX_FRAME_SIZE, as it is for every packet class).
    //  * `duration_ms`, bounded by its own MAGNITUDE. See
    //    `MEETING_TIMER_MAX_DURATION_MS`: this is arithmetic hygiene for the
    //    client's progress-proportion math, not authorization.
    //  * `ends_at_ms >= duration_ms`, an INTERNAL-CONSISTENCY check between two
    //    fields of the same packet. The wire contract has every client compute
    //    `started_at_ms = ends_at_ms - duration_ms` to render a progress
    //    proportion; without this, `running=true, ends_at_ms=0,
    //    duration_ms=300_000` underflows that subtraction on every receiver —
    //    a panic in a debug wasm build, which aborts the module and takes the
    //    whole call down for that tab (the same blast radius #2095 documents),
    //    and a ~1.8e19 garbage proportion in release.
    //
    // NOT validated: the MAGNITUDE of `ends_at_ms`. Sanity-checking an ABSOLUTE
    // instant means comparing it to the RELAY's clock, and a relay whose clock
    // stepped backwards would then reject legitimate timers — a fail-closed
    // wedge of the #2122 shape — while buying nothing, since the sender is the
    // authorized host and may set any end time. Note the consistency check above
    // is NOT that: it compares two fields of the packet to each other, involves
    // no clock, and therefore cannot wedge. This arm performs no clock
    // arithmetic at all, which is what makes it wedge-proof.
    //
    // Validation runs HERE, BEFORE the per-sender `MeetingTimerRateLimiter` in
    // `SessionLogic::handle_inbound`, so a flood of INVALID packets is discarded
    // without ever consuming a sender's valid-budget window. Both WS and WT
    // route inbound through the shared `handle_inbound` → `classify_packet`, so
    // this single arm is the sole ingress enforcement point across transports.
    //
    // AUTHORITY (host-only) is NOT enforced here — see `PacketKind::MeetingTimer`.
    if packet_wrapper.packet_type == PacketType::MEETING_TIMER.into() {
        if packet_wrapper.data.len() > MEETING_TIMER_PACKET_MAX_BYTES {
            return PacketKind::Dropped;
        }
        return match MeetingTimerPacket::parse_from_bytes(&packet_wrapper.data) {
            Ok(timer)
                if timer.duration_ms <= MEETING_TIMER_MAX_DURATION_MS
                    && timer.ends_at_ms >= timer.duration_ms =>
            {
                PacketKind::MeetingTimer
            }
            _ => PacketKind::Dropped,
        };
    }

    // Check if it's a MEDIA packet (RTT, keyframe request, or regular media).
    if packet_wrapper.packet_type == PacketType::MEDIA.into() {
        // Try to parse inner MediaPacket to distinguish control sub-types.
        // When the inner bytes do not parse (as with ordinary encrypted
        // payloads), the frame kind falls back to `Unknown`; the cleartext
        // outer media kind still distinguishes video/screen from other data.
        let frame_kind =
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
                // Map only the two relay-readable literals; an empty/unexpected
                // `frame_type` (older/malformed publisher, or opaque bytes that
                // happen to parse as a proto) is NOT a relay-readable kind and must
                // fall to `Unknown` per the metric contract — mapping it to `Delta`
                // would pollute the delta bucket with frames whose kind we don't
                // actually know.
                match media_packet.frame_type.as_str() {
                    "key" => InboundFrameKind::Key,
                    "delta" => InboundFrameKind::Delta,
                    _ => InboundFrameKind::Unknown,
                }
            } else {
                InboundFrameKind::Unknown
            };
        return match packet_wrapper.media_kind.enum_value() {
            Ok(media_kind @ (MediaKind::VIDEO | MediaKind::SCREEN)) => PacketKind::Media {
                media_kind,
                frame_kind,
            },
            _ => PacketKind::Data,
        };
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
    /// The per-pair budget is chosen by [`keyframe_per_pair_budget`] from the
    /// request `kind` and `congested`. `kind` (issue #1899): a **SCREEN** stream
    /// has no inter-frame fallback, so a missed keyframe freezes the tile
    /// indefinitely; it gets a higher per-pair budget than **VIDEO** (whose
    /// camera budget is unchanged — the #1479 storm protection is intact) so a
    /// frozen static share can re-request promptly. `congested` (issue #979):
    /// the relay has recently had to drop inbound media destined for this
    /// receiver, so its decoder is likely frozen and in genuine need of a fresh
    /// keyframe; when set, the **per-pair** budget is relaxed to that kind's
    /// congested budget so recovery is possible even if some keyframe responses
    /// are themselves lost. The global per-receiver ceiling
    /// ([`KEYFRAME_REQUEST_MAX_PER_SEC`]) is **never** relaxed for any kind, so
    /// the pre-existing keyframe-storm risk (OSS #814) stays bounded — this
    /// relaxes the per-pair cap, it does not remove the ceiling.
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

        // #1899: the per-pair budget is kind-aware. SCREEN gets a higher cap
        // (no inter-frame fallback → a missed keyframe freezes the tile), VIDEO
        // and the fail-open Other bucket keep the camera budgets unchanged. The
        // congested relaxation still layers on top per kind. `keyframe_per_pair_budget`
        // is the shared source of truth so the rate-limit log cannot misreport it.
        let per_pair_max = keyframe_per_pair_budget(kind, congested);

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
    /// entry — only clears an existing one. Creating on delivery would have been
    /// an unbounded-growth vector keyed by the then-forgeable outer `session_id`
    /// (delivery key option A — see `handle_outbound`); refusing to insert
    /// closes that independently of #2095's relay-side stamp, which now also
    /// makes that key unforgeable.
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

/// Per-session tumbling-window rate limiter for REACTION packets (#1884).
///
/// Each SENDING session owns one `ReactionRateLimiter`. A REACTION is a
/// client-authored packet the relay RE-BROADCASTS to the whole room on the
/// media fan-out, so — like the KEYFRAME_REQUEST path — it needs an abuse
/// ceiling. Here that is a SINGLE per-sender bucket of
/// [`REACTION_MAX_PER_WINDOW`] per [`REACTION_WINDOW_MS`]: there is no
/// per-target dimension (a reaction is aimed at the room, not one peer), no
/// global/relaxation layer, and no delivery-awareness — one bucket, one window.
///
/// This is the RELAY ceiling. The browser client self-throttles STRICTLY below
/// it (≤3 per rolling 1000ms AND ≥350ms between sends — see videocall-client's
/// reaction self-throttle), so a well-behaved client never reaches this cap; it
/// clamps a misbehaving or forged client, and it is enforced identically on
/// both transports because WS and WT both route inbound through the shared
/// `SessionLogic::handle_inbound`.
///
/// Closed-enum validation runs in [`classify_packet`] BEFORE a reaction ever
/// reaches this limiter (an invalid reaction is classified
/// [`PacketKind::Dropped`]), so a flood of invalid reactions cannot consume a
/// sender's valid-budget window here.
///
/// Reuses [`WindowCounter`] for the tumbling-window math; its keyframe-only
/// `waiting_since` field stays `None` (unused for reactions) and is untouched by
/// [`WindowCounter::try_consume`].
pub struct ReactionRateLimiter {
    window: WindowCounter,
}

impl Default for ReactionRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl ReactionRateLimiter {
    pub fn new() -> Self {
        Self {
            window: WindowCounter::new(Instant::now()),
        }
    }

    /// Try to consume one reaction slot for this sender. Returns `true` when the
    /// reaction is within budget for the current window (forward it), `false`
    /// when the per-sender budget is saturated (drop it as Processed). The
    /// window slides on its own: once [`REACTION_WINDOW_MS`] elapses with the
    /// next call, the count resets and the budget refills.
    pub fn allow(&mut self) -> bool {
        self.window.try_consume(
            Instant::now(),
            Duration::from_millis(REACTION_WINDOW_MS),
            REACTION_MAX_PER_WINDOW,
        )
    }
}

/// Per-session tumbling-window rate limiter for RAISE_HAND packets (#2135).
///
/// Structurally identical to [`ReactionRateLimiter`] — one per SENDING session,
/// one bucket, one window, no per-target dimension (a raised hand is aimed at
/// the room) — but with its OWN budget ([`RAISE_HAND_MAX_PER_WINDOW`] per
/// [`RAISE_HAND_WINDOW_MS`]) because the legitimate traffic shape differs: a
/// hand toggle is human-paced and rare, but a raised-hand client ALSO
/// re-announces its state on peer-join, which produces a short burst during a
/// join wave that a reaction never does. See the constants' docs.
///
/// A SEPARATE type rather than a shared/parameterised limiter, deliberately: the
/// two surfaces are metered for different reasons at different budgets, and
/// collapsing them would make a future tuning change to one silently retune the
/// other. Both are thin wrappers over the SAME [`WindowCounter`], which is where
/// the tumbling-window math actually lives, so there is no duplicated logic —
/// only a duplicated (and independently documented) budget.
///
/// CONSEQUENCE OF A DROP, stated plainly because it differs from REACTION's:
/// dropping a reaction loses an ephemeral float; dropping a RAISE_HAND loses a
/// STATE TRANSITION, and the relay holds no hand registry to repair it from. The
/// budget is therefore sized so a well-behaved client cannot reach it, and the
/// wire contract asks the client to re-announce on the next peer-join (the
/// packet is idempotent state, so a repeat is always safe).
///
/// Ingress validation (size cap + parse) runs in [`classify_packet`] BEFORE a
/// packet ever reaches this limiter, so a flood of oversized/garbage raise-hand
/// packets cannot consume a sender's valid-budget window here.
///
/// Reuses [`WindowCounter`]; its keyframe-only `waiting_since` field stays
/// `None` (unused here) and is untouched by [`WindowCounter::try_consume`].
pub struct RaiseHandRateLimiter {
    window: WindowCounter,
}

impl Default for RaiseHandRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RaiseHandRateLimiter {
    pub fn new() -> Self {
        Self {
            window: WindowCounter::new(Instant::now()),
        }
    }

    /// Try to consume one raise-hand slot for this sender. Returns `true` when
    /// the packet is within budget for the current window (forward it), `false`
    /// when the per-sender budget is saturated (drop it as Processed). The
    /// window slides on its own: once [`RAISE_HAND_WINDOW_MS`] elapses with the
    /// next call, the count resets and the budget refills.
    pub fn allow(&mut self) -> bool {
        self.window.try_consume(
            Instant::now(),
            Duration::from_millis(RAISE_HAND_WINDOW_MS),
            RAISE_HAND_MAX_PER_WINDOW,
        )
    }

    /// Test-only: push this limiter's window start `by` into the past so the
    /// next [`allow`](Self::allow) rolls the window and refills the budget,
    /// without a real sleep.
    ///
    /// Exists because `WindowCounter::window_start` is private to this module,
    /// so `session_logic`'s call-site test — which must drive the REAL
    /// `handle_inbound` arm, not the limiter in isolation — has no other way to
    /// prove the budget RECOVERS. That recovery is load-bearing for #2135: a
    /// raise-hand carries persistent state with no relay-side registry to repair
    /// from, so a limiter that could not refill would wedge a participant's hand
    /// state for the rest of the meeting.
    #[cfg(test)]
    pub fn rewind_window_for_test(&mut self, by: Duration) {
        self.window.window_start = Instant::now() - by;
    }
}

/// Per-session tumbling-window rate limiter for MEETING_TIMER packets (#2136).
///
/// Each SENDING session owns one. A MEETING_TIMER is a client-authored packet
/// the relay RE-BROADCASTS to the whole room, so — like REACTION — it needs an
/// abuse ceiling: a SINGLE per-sender bucket of
/// [`MEETING_TIMER_MAX_PER_WINDOW`] per [`MEETING_TIMER_WINDOW_MS`].
///
/// It has its OWN budget rather than reusing [`ReactionRateLimiter`]'s because
/// the legitimate traffic shape is not click-driven at all — a ~5s heartbeat
/// while a timer runs, plus a 3-packet repeat burst on each transition. See
/// [`MEETING_TIMER_MAX_PER_WINDOW`] for the sizing and for why the margin
/// matters more here than for reactions (a dropped CANCEL leaves the room
/// counting down to an audible expiry the host already called off).
///
/// The limiter runs BEFORE the host gate, because the relay cannot know whether
/// a sender is the host until the packet reaches the `chat_server` fan-out
/// funnel. A non-host flood is therefore metered here and rejected there: each
/// forged sender is bounded to its own [`MEETING_TIMER_MAX_PER_WINDOW`] packets
/// of at most [`MEETING_TIMER_PACKET_MAX_BYTES`] each, and none of them reach
/// another participant.
///
/// Ingress validation runs in [`classify_packet`] BEFORE this limiter (an
/// oversized or unparseable packet is [`PacketKind::Dropped`]), so a flood of
/// garbage cannot consume a sender's valid-budget window here.
///
/// Reuses [`WindowCounter`] for the tumbling-window math; its keyframe-only
/// `waiting_since` field stays `None` and is untouched by
/// [`WindowCounter::try_consume`].
pub struct MeetingTimerRateLimiter {
    window: WindowCounter,
}

impl Default for MeetingTimerRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl MeetingTimerRateLimiter {
    pub fn new() -> Self {
        Self {
            window: WindowCounter::new(Instant::now()),
        }
    }

    /// Try to consume one MEETING_TIMER slot for this sender. `true` = within
    /// budget (forward it), `false` = per-sender budget saturated (drop it as
    /// Processed). The window is TUMBLING, not sliding: once
    /// [`MEETING_TIMER_WINDOW_MS`] has elapsed at the next call, the count
    /// resets wholesale and the budget refills — so the limiter cannot
    /// permanently wedge a host out of controlling the room's timer. (The
    /// tumbling reset means a burst straddling a window boundary can pass up to
    /// 2× the budget within one window-length SPAN; see
    /// [`MEETING_TIMER_MAX_PER_WINDOW`], whose stated rate is the sustained
    /// average, not an instantaneous bound.)
    pub fn allow(&mut self) -> bool {
        self.window.try_consume(
            Instant::now(),
            Duration::from_millis(MEETING_TIMER_WINDOW_MS),
            MEETING_TIMER_MAX_PER_WINDOW,
        )
    }

    /// Test-only: pretend the current window started `by` ago, so a test can
    /// prove the budget REFILLS without sleeping for a real window.
    #[cfg(test)]
    pub fn rewind_window_for_test(&mut self, by: Duration) {
        self.window.window_start = Instant::now() - by;
    }
}

/// Return the largest index `<= max` at which `bytes` can be truncated without
/// splitting a UTF-8 codepoint (a byte-slice analogue of the unstable
/// `str::floor_char_boundary`). If `bytes[max]` is a UTF-8 continuation byte
/// (`0b10xx_xxxx`) we back up to the leading byte of the straddling codepoint so
/// the partial codepoint is dropped whole, never emitted split. For bytes that
/// are not valid UTF-8 this still returns a bound `<= max`.
fn floor_utf8_boundary(bytes: &[u8], max: usize) -> usize {
    if max >= bytes.len() {
        return bytes.len();
    }
    let mut end = max;
    while end > 0 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    end
}

/// Re-stamp a validated REACTION `PacketWrapper` for room fan-out (#1884,
/// security). This is the point at which the relay makes reaction attribution
/// TRUSTWORTHY:
///
/// 1. It overwrites `PacketWrapper.session_id` UNCONDITIONALLY with
///    `authenticated_session` (the relay's own session id for this sender),
///    NOT only when the client sent `0` like the generic publish-path stamp
///    (`chat_server::handle` for `ClientMessage`). Because a REACTION is the one
///    client-authored packet the relay RE-BROADCASTS cleartext to the whole
///    room, a client-supplied non-zero `session_id` is an impersonation vector:
///    a malicious participant could stamp a victim's session (learned from
///    presence) and have the reaction attributed to the victim's real display
///    name and screen-reader announcement — with no E2EE backstop (unlike
///    MEDIA). Overwriting here closes that: attribution now anchors on the
///    relay-authenticated session for every REACTION, and the downstream
///    self-echo suppression (which keys on this session_id) becomes trustworthy
///    for REACTION as a side effect. Well-behaved clients send `0`, so this is
///    behavior-transparent for legitimate traffic.
/// 2. It bounds the cosmetic `display_name` to `max_name_bytes`, truncating at a
///    UTF-8 char boundary. The name is attacker-controlled and fanned out to
///    every participant, so an oversized value is an egress-amplification
///    surface; the client caps at 64 chars but the relay must not rely on client
///    cooperation. Truncate (not drop): the reaction itself is valid and the
///    name is only a cosmetic fallback.
///
/// Runs on the shared `SessionLogic::handle_inbound` REACTION path (both WS and
/// WT), AFTER closed-enum validation (`classify_packet`) and the per-sender
/// rate limiter — so the ordering "validate enum → meter → stamp → fan out" is
/// preserved. Returns `None` (→ caller drops the reaction, fail-closed, never
/// fanning out an unstamped packet) if the wrapper or inner packet cannot be
/// parsed/reserialized; that is unreachable for a packet `classify_packet`
/// already accepted as `PacketKind::Reaction`, but the fail-closed default is
/// the safe one.
pub fn stamp_reaction_for_broadcast(
    data: &[u8],
    authenticated_session: u64,
    max_name_bytes: usize,
) -> Option<Vec<u8>> {
    let mut wrapper = PacketWrapper::parse_from_bytes(data).ok()?;
    wrapper.session_id = authenticated_session;

    // Bound the cosmetic display_name. Parse the inner packet only to measure
    // it; re-serialize the inner (and thus rewrite `wrapper.data`) ONLY when we
    // actually truncate, so the common in-bound case pays no re-encode cost.
    let mut inner = ReactionPacket::parse_from_bytes(&wrapper.data).ok()?;
    if inner.display_name.len() > max_name_bytes {
        let end = floor_utf8_boundary(&inner.display_name, max_name_bytes);
        inner.display_name.truncate(end);
        wrapper.data = inner.write_to_bytes().ok()?;
    }

    wrapper.write_to_bytes().ok()
}

/// Re-stamp a validated RAISE_HAND `PacketWrapper` for room fan-out (#2135).
///
/// The exact analogue of [`stamp_reaction_for_broadcast`], and for the same two
/// reasons:
///
/// 1. It overwrites `PacketWrapper.session_id` UNCONDITIONALLY with
///    `authenticated_session`. Since #2124 (issue #2095) the generic broadcast
///    stamp (`stamp_wrapper_for_broadcast`, in `chat_server`'s
///    `Handler<ClientMessage>`) already does this for EVERY forwarded packet, so
///    this write is REDUNDANT on the live path — and it is retained anyway, on
///    purpose. Attribution for a raised hand is the whole feature (a forged
///    session_id would raise a victim's hand, cleartext, with no E2EE backstop),
///    and that guarantee must not depend on an invariant enforced in a different
///    module that a future refactor could move. This is the same
///    defense-in-depth reasoning the `MAX_TRACKED_SENDERS` doc states for the
///    congestion-tracker cap. Cost is one `u64` store.
/// 2. It bounds the cosmetic `display_name` to `max_name_bytes`, truncating at a
///    UTF-8 char boundary. The name is attacker-controlled and fanned out to
///    every participant, so an oversized value is an egress-amplification
///    surface. Truncate (not drop): the hand STATE is valid and the name is only
///    a cosmetic fallback, so discarding a real state transition over an
///    oversized cosmetic field would leave the room's view of that participant
///    wrong — with no server-side registry to repair it from.
///
/// Note the division of labour with [`classify_packet`]: the SIZE cap
/// (`RAISE_HAND_PACKET_MAX_BYTES`, on the raw bytes) is what bounds the packet
/// as a whole — including unknown fields, which rust-protobuf preserves — and it
/// runs at ingress. This function only bounds the one KNOWN field the relay can
/// meaningfully shorten.
///
/// Runs on the shared `SessionLogic::handle_inbound` RAISE_HAND path (both WS
/// and WT), AFTER ingress validation and the per-sender rate limiter, so the
/// ordering "validate → meter → stamp → fan out" matches REACTION's. Returns
/// `None` (→ caller drops the packet, fail-closed, never fanning out an
/// unstamped one) if the wrapper or inner packet cannot be parsed/reserialized;
/// that is unreachable for a packet `classify_packet` already accepted as
/// `PacketKind::RaiseHand`, but the fail-closed default is the safe one.
pub fn stamp_raise_hand_for_broadcast(
    data: &[u8],
    authenticated_session: u64,
    max_name_bytes: usize,
) -> Option<Vec<u8>> {
    let mut wrapper = PacketWrapper::parse_from_bytes(data).ok()?;
    wrapper.session_id = authenticated_session;

    // Bound the cosmetic display_name. Parse the inner packet only to measure
    // it; re-serialize the inner (and thus rewrite `wrapper.data`) ONLY when we
    // actually truncate, so the common in-bound case pays no re-encode cost —
    // and, importantly, so an in-bound packet's UNKNOWN FIELDS survive byte-for
    // -byte (forward compatibility with a newer client's added field).
    let mut inner = RaiseHandPacket::parse_from_bytes(&wrapper.data).ok()?;
    if inner.display_name.len() > max_name_bytes {
        let end = floor_utf8_boundary(&inner.display_name, max_name_bytes);
        inner.display_name.truncate(end);
        wrapper.data = inner.write_to_bytes().ok()?;
    }

    wrapper.write_to_bytes().ok()
}

/// Re-stamp EVERY room-fan-out `PacketWrapper` with the relay-authenticated
/// identity of the session that published it (#2095, security).
///
/// This is the generic counterpart to [`stamp_reaction_for_broadcast`]: it runs
/// once per packet in `ChatServer`'s `Handler<ClientMessage>`, the single funnel
/// through which every forwarded packet from every transport reaches NATS and
/// therefore every peer.
///
/// ## What it fixes
///
/// The pre-#2095 code stamped `session_id` FILL-IF-ZERO
/// (`if wrapper.session_id == 0 { wrapper.session_id = session }`) and never
/// stamped `user_id` at all, so a client that supplied a NONZERO `session_id` or
/// any `user_id` had those values forwarded to every peer untouched. Both outer
/// scalars are consumed by the receiving client as ATTRIBUTION:
///
/// * `session_id` keys `set_peer_device_info` in
///   `videocall-client/src/client/video_call_client.rs`. Because
///   `client_diagnostics::trim_health_packet_for_peers` deliberately strips the
///   inner identity scalars from the peer-facing HEALTH copy, this outer field is
///   the ONLY attribution the peer UI has — a forged value matching a live peer
///   overwrote THAT peer's rendered device info.
/// * `session_id` + `user_id` together feed `ensure_peer`, which mints the peer
///   entry (and its user-id/email fallback label) on the first packet seen from a
///   session.
///
/// Stamping the ENVELOPE — rather than patching the HEALTH device-info consumer —
/// closes both at the one place all fan-out traffic passes, for every packet type
/// at once.
///
/// ## Unconditional, not fill-if-zero
///
/// `session_id` is written unconditionally: a `u64` store is cheaper than the
/// compare it replaces, and "only when the client sent 0" is exactly the hole.
///
/// `user_id` is written only when it DIFFERS from the authenticated value. That is
/// observationally identical to an unconditional write — on exit
/// `wrapper.user_id == authenticated_user_id.as_bytes()` holds on both branches,
/// and `Vec<u8>` equality is byte equality, so the skipped write could only have
/// stored bytes already present. The branch exists purely to avoid a heap
/// allocation: `user_id` is `Vec<u8>`, so an unconditional write would `to_vec()`
/// a fresh buffer FOR EVERY PACKET on the relay's hottest loop. A well-behaved
/// client already sends its own id here (see `videocall-client`'s
/// `transform_video_chunk` / `transform_screen_chunk` / `transform_audio_chunk`),
/// so the common case is a ≤64-byte `memcmp` and zero allocations.
///
/// ## Guests / empty identities
///
/// The authenticated `user_id` is the session's JWT `sub` (or the sanitized path
/// segment on the deprecated endpoint). If it is EMPTY, this writes empty — which
/// is correct: an empty relay-side identity means the relay knows of no user id
/// for this session, and forwarding a client's self-asserted one instead would be
/// precisely the trust the fix removes. The receiving client already handles an
/// empty `user_id` (it renders the display name resolved from the
/// server-authored PARTICIPANT_JOINED, falling back to the session id).
///
/// A session with an empty identity should never exist in the first place —
/// `Handler<JoinRoom>` now refuses one, alongside the reserved `SYSTEM_USER_ID`
/// — so this arm is defense in depth for a session that somehow reaches the
/// broadcast path without a join, not a supported configuration.
///
/// ## Fail-CLOSED: `None` means DROP, exactly like the REACTION stamp
///
/// Returns `None` — never a fallback payload — when the input does not parse as
/// a `PacketWrapper`, or when the stamped wrapper will not re-serialize. The
/// caller MUST skip the publish entirely on `None`; returning `Vec::new()`
/// instead would still publish, and empty bytes parse as a DEFAULT
/// `PacketWrapper` (session_id 0, user_id empty, packet_type UNSPECIFIED) — a
/// forwarded packet with no identity at all.
///
/// Two distinct reasons, one behavior:
///
/// 1. **Unparseable input is a remote DoS, not opaque data.** `classify_packet`
///    routes bytes that FAIL to parse as a `PacketWrapper` to `PacketKind::Data`
///    ("unparseable, treat as opaque data"), so they reach here. The pre-#2095
///    code forwarded them verbatim. That is NOT safe on the default transport:
///    `videocall-types`' `From<Binary> for PacketWrapper`
///    (`videocall-types/src/lib.rs` ~132-136) calls `parse_from_bytes(..).unwrap()`,
///    and a wasm panic ABORTS — trapping the module and killing the call for
///    that tab. WebTransport drops a bad datagram cleanly, but WebSocket has
///    been the DEFAULT transport since #2045. `handle_msg`'s outbound filters do
///    not save an ADMITTED participant either: its packet-type predicates are all
///    `parsed.map(..).unwrap_or(false)`, and each is a "relay-authored control
///    packet" EXEMPTION from the self-echo guard, so `false` means "forward
///    normally" — an unparseable frame (`parsed == None`) matches no drop
///    condition and the viewport VIDEO filter needs a `media_kind` it also
///    cannot read. (An OBSERVER receiver is safe: its allowlist is an ALLOW
///    predicate, so the same `unwrap_or(false)` drops there. Observers were
///    never the target.) Net effect before this change: one 4-byte frame from
///    any authenticated participant (a guest suffices) crashed every OTHER
///    participant's tab — relay-amplified, unrate-limited. Dropping at the relay
///    is the fix that belongs here; the client-side `.unwrap()` is a separate
///    hardening item on a shared type.
///
/// 2. **A serialize failure would forward an UNSTAMPED identity.** These bytes
///    DID parse, so the attacker-controlled `session_id`/`user_id` in them are
///    readable by every peer. Falling back to `data.to_vec()` would hand the
///    forgery through the one function whose entire job is to remove it.
///    Effectively unreachable (rust-protobuf writes into a `Vec`, and proto3 has
///    no required-field check that could fail), but "unreachable" is not a
///    reason to leave the unsafe arm in place.
///
/// This costs nothing legitimate: every packet a real client emits on this path
/// is a serialized `PacketWrapper` by construction, so no well-behaved frame can
/// take either arm.
pub fn stamp_wrapper_for_broadcast(
    data: &[u8],
    authenticated_session: u64,
    authenticated_user_id: &str,
) -> Option<Vec<u8>> {
    let mut wrapper = PacketWrapper::parse_from_bytes(data).ok()?;

    wrapper.session_id = authenticated_session;
    if wrapper.user_id != authenticated_user_id.as_bytes() {
        wrapper.user_id = authenticated_user_id.as_bytes().to_vec();
    }

    wrapper.write_to_bytes().ok()
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
/// ## Delivery-key trust (option A — outer `session_id`, bounded)
///
/// The authoritative publisher identity is the NATS subject (set by the relay,
/// unforgeable — see `chat_server::handle_msg` ~4199), NOT the outer
/// `session_id`. We key the delivery observation off the outer
/// `session_id`/`user_id` here (mirroring [`KeyframeTarget::from_request`]) to
/// keep `handle_outbound` self-contained and off the `Message` hot struct.
///
/// Since #2095 those outer scalars are no longer publisher-forgeable at all:
/// the broadcast path stamps BOTH with the publisher's authenticated identity
/// (`stamp_wrapper_for_broadcast`) before anything is published, so by the time
/// an OUTBOUND frame reaches this function the key already agrees with the
/// subject. The bound below is kept because it does not depend on that: a
/// publisher forging its OWN
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
    // #1306: capture the user_id byte RANGE within `data` rather than copying it.
    // This runs per outbound media frame; the user_id is only consumed (via
    // `from_request`'s `to_vec`) in the rare `session_id == 0` fallback, so a
    // per-frame `read_bytes` allocation is pure waste in the common session-keyed
    // path. Materialized as a slice only at the end, and only when needed.
    let mut user_id_range: Option<(usize, usize)> = None;

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
                // Read the length prefix and record the slice bounds WITHOUT
                // copying (vs `read_bytes`, which allocates a Vec). The length is
                // read with `read_raw_varint32` and consumed via `skip_raw_bytes`
                // as a `u32` with NO truncation cast — byte-for-byte identical to
                // what `read_bytes_into` does internally (`len = read_raw_varint32`
                // then `read_raw_bytes_into(len, ..)`, both `u32`), minus the
                // allocation (#1306, #1350). Bounds-check against `data` before
                // trusting the attacker-controllable length, then skip the bytes
                // to keep parsing.
                let len = match is.read_raw_varint32() {
                    Ok(v) => v,
                    Err(_) => return None,
                };
                let start = is.pos() as usize;
                let end = match start.checked_add(len as usize) {
                    Some(end) if end <= data.len() => end,
                    _ => return None,
                };
                if is.skip_raw_bytes(len).is_err() {
                    return None;
                }
                user_id_range = Some((start, end));
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
    // request key (session when set, else user_id — #1124). The user_id slice is
    // materialized (and copied by `from_request`) ONLY in the `session_id == 0`
    // fallback; the common session-keyed path never touches it.
    let user_id: &[u8] = match user_id_range {
        Some((start, end)) => &data[start..end],
        None => &[],
    };
    let target = KeyframeTarget::from_request(user_id, session_id);
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
    fn test_classify_downlink_congestion_packet_as_dropped() {
        // #1219 Half 2: DOWNLINK_CONGESTION is relay-authored-only (the relay
        // emits it on a receiver's own subject when that receiver's outbound
        // channel overflows, as observed by the windowed CongestionTracker).
        // A client-sent one is always forged and is
        // the PRIMARY trust boundary for the signal: if accepted and reflected,
        // a malicious client could trick OTHER receivers into stepping their
        // video layers down (denial-of-quality). It must be dropped at ingest —
        // symmetric with the CONGESTION and LAYER_HINT drops above. This test
        // pins that drop: deleting the guard in `classify_packet` must fail here.
        let wrapper = PacketWrapper {
            packet_type: PacketType::DOWNLINK_CONGESTION.into(),
            data: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(classify_packet(&bytes), PacketKind::Dropped);
    }

    #[test]
    fn test_classify_meeting_packet_as_dropped() {
        // #1704: a client-sent MEETING packet is always forged — MEETING events
        // (HOST_MUTE_PARTICIPANT, MEETING_ENDED, etc.) are server-authoritative,
        // published exclusively by meeting-api over NATS. This drop became
        // LOAD-BEARING for a SECOND consumer in #1703 (the #1202 membership
        // mirror): that PR widened the mirror's NATS subject gate to also observe
        // the receiver's own per-session subject `room.{room}.{self}` — a subject a
        // client CAN publish to. That widening is safe ONLY because this guard
        // drops every client-authored `PacketType::MEETING` packet before it can
        // reach NATS, so any `PacketType::MEETING` + `SYSTEM_USER_ID` packet seen on
        // that subject is necessarily server-authored. Remove this arm and the
        // widening silently becomes a real attack surface: a malicious participant
        // could plant inert→Stage-B-counted `Remote` rows, un-pinning a publisher's
        // simulcast union = bandwidth amplification. Symmetric with the CONGESTION,
        // LAYER_HINT, and DOWNLINK_CONGESTION drops above, mirroring the #1219
        // trust-boundary reasoning. BOTH the ws and wt paths route through the
        // shared `SessionLogic::handle_inbound` → `classify_packet`, so one test on
        // the shared `classify_packet` is sufficient. This test pins that drop:
        // deleting the MEETING arm in `classify_packet` must fail here.
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
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
    fn test_classify_regular_video_media_as_media() {
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            frame_type: "delta".to_string(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            media_kind: MediaKind::VIDEO.into(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::Media {
                media_kind: MediaKind::VIDEO,
                frame_kind: InboundFrameKind::Delta,
            }
        );
    }

    #[test]
    fn test_classify_unparseable_inner_video_and_screen_as_unknown_media() {
        for media_kind in [MediaKind::VIDEO, MediaKind::SCREEN] {
            let wrapper = PacketWrapper {
                packet_type: PacketType::MEDIA.into(),
                data: vec![0x08, 0x80],
                media_kind: media_kind.into(),
                ..Default::default()
            };
            let bytes = wrapper.write_to_bytes().unwrap();
            assert_eq!(
                classify_packet(&bytes),
                PacketKind::Media {
                    media_kind,
                    frame_kind: InboundFrameKind::Unknown,
                },
                "an unparseable inner frame must retain its outer {media_kind:?} classification"
            );
        }
    }

    #[test]
    fn test_classify_audio_and_unspecified_media_as_data() {
        let media = MediaPacket {
            media_type: MediaType::AUDIO.into(),
            frame_type: "delta".to_string(),
            ..Default::default()
        };
        for media_kind in [MediaKind::AUDIO, MediaKind::MEDIA_KIND_UNSPECIFIED] {
            let wrapper = PacketWrapper {
                packet_type: PacketType::MEDIA.into(),
                data: media.write_to_bytes().unwrap(),
                media_kind: media_kind.into(),
                ..Default::default()
            };
            let bytes = wrapper.write_to_bytes().unwrap();
            assert_eq!(
                classify_packet(&bytes),
                PacketKind::Data,
                "outer {media_kind:?} media must not enter the video/screen gap metric"
            );
        }
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
    fn test_outbound_observation_user_id_skip_preserves_trailing_data_common_path() {
        // #1350 (common session-keyed path): a NON-zero session_id frame carries
        // BOTH a non-empty user_id (field 2) AND a multi-KB `data` (field 3)
        // serialized AFTER it. The session-keyed path captures the user_id range
        // and SKIPS the bytes without copying (the line #1350 made varint32-exact);
        // it must advance the stream by EXACTLY the user_id length so the trailing
        // `data` field — and then the outer session_id (4) / media_kind (6) — still
        // parse. Field-number order serializes user_id(2) < data(3) < session_id(4)
        // < media_kind(6), so the multi-KB payload sits directly after the user_id
        // whose skip we are exercising.
        //
        // ADVERSARIAL (mutation): if the user_id length read or skip desynced by
        // even one byte (e.g. a truncating cast or an off-by-one varint width),
        // the subsequent `data` length prefix would be misread, the parse would
        // either bail (fail-safe None) or land on the wrong session_id, and the
        // `Session(98_765)` assertion below would FAIL.
        let session_id = 98_765u64;
        let delivered = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            session_id,
            // Non-empty user_id whose multi-byte varint length-prefix the skip
            // must consume exactly (>127 bytes forces a 2-byte length varint,
            // exercising the varint32 width, not just a 1-byte length).
            user_id: vec![b'u'; 200],
            media_kind: MediaKind::VIDEO.into(),
            // Multi-KB payload AFTER user_id on the wire; the user_id skip must
            // not desync the parse of this field.
            data: vec![0u8; 8192],
            ..Default::default()
        };
        let raw = delivered.write_to_bytes().unwrap();
        assert_eq!(
            outbound_keyframe_observation(&raw),
            Some((
                KeyframeTarget::Session(session_id),
                KeyframeMediaKind::Video
            )),
            "a non-zero-session frame with a non-empty user_id followed by multi-KB \
             data must observe Session(session_id)+Video — proving the user_id \
             range-capture/skip did not desync parsing of the trailing data field"
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
    fn test_outbound_observation_user_fallback_when_session_zero() {
        // #1124: when the outer session_id is 0 (legacy user-id-wide case) the
        // observation falls back to KeyframeTarget::User(user_id). This is the
        // ONLY path that materializes the user_id bytes, so it pins the #1306
        // zero-copy range capture: an off-by-one or wrong offset in the recorded
        // (start, end) range would corrupt the extracted user_id and fail the
        // equality below. The multi-KB `data` (field 3) is serialized AFTER
        // user_id (field 2), proving the recorded range stays valid across the
        // skip of the payload.
        let user_id = b"publisher@example.com".to_vec();
        let delivered = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            session_id: 0,
            user_id: user_id.clone(),
            media_kind: MediaKind::VIDEO.into(),
            data: vec![7u8; 4096],
            ..Default::default()
        };
        let raw = delivered.write_to_bytes().unwrap();
        assert_eq!(
            outbound_keyframe_observation(&raw),
            Some((KeyframeTarget::User(user_id), KeyframeMediaKind::Video)),
            "session_id == 0 must fall back to the EXACT user_id bytes (zero-copy range capture)"
        );

        // An empty/absent user_id with session 0 must still parse and yield the
        // empty-user target (the `None` range arm), never panic.
        let no_user = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            session_id: 0,
            media_kind: MediaKind::VIDEO.into(),
            data: vec![0u8; 16],
            ..Default::default()
        };
        assert_eq!(
            outbound_keyframe_observation(&no_user.write_to_bytes().unwrap()),
            Some((KeyframeTarget::User(Vec::new()), KeyframeMediaKind::Video)),
            "session 0 with no user_id yields the empty-user target without panicking"
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

    // =====================================================================
    // #1899: SCREEN gets a higher per-pair keyframe budget than camera
    // =====================================================================

    #[test]
    fn test_keyframe_per_pair_budget_screen_higher_camera_unchanged() {
        // Pins the budget-selection SOURCE OF TRUTH (`keyframe_per_pair_budget`),
        // the same fn both the limiter and the rate-limit log call.
        //
        // ADVERSARIAL (mutation): flip the `Screen` arm to return a camera
        // constant (revert #1899) → the two "> camera" assertions FAIL. Widen the
        // `(_, false)` camera arm to the SCREEN constant → the "camera unchanged"
        // assertion FAILS.

        // VIDEO keeps the camera budgets byte-for-byte (#1479 unchanged).
        assert_eq!(
            keyframe_per_pair_budget(KeyframeMediaKind::Video, false),
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER,
            "VIDEO steady-state budget must stay the camera value"
        );
        assert_eq!(
            keyframe_per_pair_budget(KeyframeMediaKind::Video, true),
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED,
            "VIDEO congested budget must stay the camera value"
        );

        // Other (AUDIO / UNSPECIFIED / unparseable / E2EE-obscured request) maps
        // to the CAMERA budget — the conservative fallback (never SCREEN's wider
        // budget on an unverified kind).
        assert_eq!(
            keyframe_per_pair_budget(KeyframeMediaKind::Other, false),
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER,
            "Other (E2EE/unparseable fallback) must get the camera steady-state budget"
        );
        assert_eq!(
            keyframe_per_pair_budget(KeyframeMediaKind::Other, true),
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED,
            "Other congested must get the camera congested budget"
        );

        // SCREEN gets the raised budgets.
        assert_eq!(
            keyframe_per_pair_budget(KeyframeMediaKind::Screen, false),
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN,
        );
        assert_eq!(
            keyframe_per_pair_budget(KeyframeMediaKind::Screen, true),
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN_CONGESTED,
        );

        // Load-bearing design invariants (fail if SCREEN were reverted to the
        // camera budget, or the congested relaxation collapsed):
        assert!(
            keyframe_per_pair_budget(KeyframeMediaKind::Screen, false)
                > keyframe_per_pair_budget(KeyframeMediaKind::Video, false),
            "SCREEN steady-state must exceed camera steady-state (#1899)"
        );
        assert!(
            keyframe_per_pair_budget(KeyframeMediaKind::Screen, true)
                > keyframe_per_pair_budget(KeyframeMediaKind::Screen, false),
            "SCREEN congested must be strictly more permissive than SCREEN steady-state"
        );
    }

    #[test]
    fn test_keyframe_limiter_screen_admits_burst_camera_would_block() {
        // THE #1899 CORE PROOF, driven through the real limiter. A SCREEN sender
        // must admit a burst up to its raised per-pair budget within ONE window;
        // the same burst on the camera (VIDEO) budget is clipped after the first
        // request. This is the frozen-static-share recovery the field bug needed.
        //
        // ADVERSARIAL (mutation): revert the SCREEN arm of `keyframe_per_pair_budget`
        // to the camera budget (1/sec) → only the FIRST screen request is admitted
        // and the `admitted == SCREEN budget` assertion FAILS.
        let mut limiter = KeyframeRequestLimiter::new();
        let screen_target = KeyframeTarget::Session(4201);

        let mut screen_admitted = 0u32;
        // Drive one more request than the budget to also prove the ceiling bites.
        for _ in 0..(KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN + 1) {
            if limiter.allow(screen_target.clone(), KeyframeMediaKind::Screen, 0) {
                screen_admitted += 1;
            }
        }
        assert_eq!(
            screen_admitted, KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN,
            "a SCREEN sender must admit exactly its raised per-pair budget in one \
             window (the old camera budget of {} would starve it — #1899)",
            KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER
        );

        // Same burst on a FRESH camera (VIDEO) target: only the first is admitted.
        let camera_target = KeyframeTarget::Session(4202);
        let mut camera_admitted = 0u32;
        for _ in 0..(KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN + 1) {
            if limiter.allow(camera_target.clone(), KeyframeMediaKind::Video, 0) {
                camera_admitted += 1;
            }
        }
        assert_eq!(
            camera_admitted, KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER,
            "camera (VIDEO) must still admit only its unchanged 1/sec budget"
        );
    }

    #[test]
    fn test_keyframe_limiter_camera_budget_unchanged_by_screen_raise() {
        // Guards against the SCREEN raise accidentally widening the camera path.
        // A VIDEO sender must admit EXACTLY its (unchanged) steady-state budget in
        // one window, then be denied — regardless of the SCREEN budget's value.
        //
        // ADVERSARIAL (mutation): widen the `(_, false)` camera arm to the SCREEN
        // constant → the second VIDEO request is admitted and the `!admitted`
        // assertion FAILS.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = KeyframeTarget::Session(4301);

        let mut admitted = 0u32;
        for _ in 0..KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER {
            if limiter.allow(target.clone(), KeyframeMediaKind::Video, 0) {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER,
            "camera steady-state budget must be exactly the unchanged camera value"
        );
        // The immediately-following request (strict budget exhausted, min-retry
        // not elapsed) must be denied — the camera cap is NOT the wider SCREEN cap.
        assert!(
            !limiter.allow(target, KeyframeMediaKind::Video, 0),
            "a VIDEO request past the camera budget must be denied within the window"
        );
    }

    #[test]
    fn test_keyframe_limiter_screen_flood_ceiling_still_engages() {
        // The raised SCREEN budget is a HIGHER ceiling, not an exemption: a flood
        // beyond the (steady-state and congested) SCREEN budget in one window is
        // still denied. This is what keeps the publisher-side coalescer — not an
        // unbounded relay — as the sole storm backstop under a reconnection wave.
        //
        // ADVERSARIAL (mutation): make SCREEN exempt (return e.g. u32::MAX from the
        // Screen arm) → the over-budget request is admitted and the `!` assertion
        // FAILS.

        // Steady-state ceiling.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = KeyframeTarget::Session(4401);
        for _ in 0..KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN {
            assert!(limiter.allow(target.clone(), KeyframeMediaKind::Screen, 0));
        }
        assert!(
            !limiter.allow(target, KeyframeMediaKind::Screen, 0),
            "a SCREEN request past the raised steady-state budget must still be denied"
        );

        // Congested ceiling (a screen share on a lossy link): higher, but bounded.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = KeyframeTarget::Session(4402);
        for _ in 0..KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN_CONGESTED {
            assert!(limiter.allow_with_congestion(
                target.clone(),
                KeyframeMediaKind::Screen,
                0,
                true
            ));
        }
        assert!(
            !limiter.allow_with_congestion(target, KeyframeMediaKind::Screen, 0, true),
            "a SCREEN request past the raised CONGESTED budget must still be denied \
             (the ceiling engages at the new higher bound, not never)"
        );
    }

    #[test]
    fn test_keyframe_limiter_unparseable_kind_uses_camera_budget() {
        // E2EE / older-client / forged request whose inner data byte-string is not
        // `b"VIDEO"`/`b"SCREEN"` classifies to `KeyframeMediaKind::Other`
        // (see `from_request_data`). The documented fallback is the CAMERA budget,
        // never SCREEN's wider one — so an unverified kind cannot be used to obtain
        // the higher screen budget.
        //
        // ADVERSARIAL (mutation): map `Other` to the SCREEN budget → the second
        // Other request is admitted and the `!admitted` assertion FAILS.
        let mut limiter = KeyframeRequestLimiter::new();
        let target = KeyframeTarget::Session(4501);

        let mut admitted = 0u32;
        for _ in 0..KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER {
            if limiter.allow(target.clone(), KeyframeMediaKind::Other, 0) {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER,
            "an unparseable-kind (Other) request must get the camera budget"
        );
        assert!(
            !limiter.allow(target, KeyframeMediaKind::Other, 0),
            "Other must be throttled at the camera budget, not handed SCREEN's wider budget"
        );
        // And confirm the classification an E2EE/older request produces IS Other,
        // so the budget this test pins is the one such a request receives.
        assert_eq!(
            KeyframeMediaKind::from_request_data(b""),
            KeyframeMediaKind::Other,
            "an empty/unparseable request kind classifies to Other (the fallback bucket)"
        );
    }

    // =====================================================================
    // #1884: REACTION classify validation + per-sender rate limiter
    // =====================================================================

    /// Build the raw bytes of a `PacketWrapper{REACTION}` whose inner cleartext
    /// `ReactionPacket` carries `reaction`. Exercises the REAL wire path
    /// `classify_packet` parses (not an in-memory struct), so these tests pin
    /// the production ingress-validation contract.
    fn reaction_wrapper_bytes(reaction: ::protobuf::EnumOrUnknown<ReactionType>) -> Vec<u8> {
        let inner = ReactionPacket {
            reaction,
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::REACTION.into(),
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap()
    }

    #[test]
    fn test_classify_valid_reaction_is_forwardable() {
        // Every defined reaction (1..=7) classifies as `Reaction` — the
        // forwardable class the per-sender limiter meters before the media
        // fan-out.
        //
        // ADVERSARIAL (mutation): delete the REACTION arm in `classify_packet`
        // and a REACTION falls through to the default `Data` tail → `Reaction !=
        // Data` fails this. Delete only the validation (always return Reaction)
        // and the UNSPECIFIED/unknown tests below fail instead.
        for r in [
            ReactionType::THUMBS_UP,
            ReactionType::THUMBS_DOWN,
            ReactionType::LAUGH,
            ReactionType::APPLAUSE,
            ReactionType::HEART,
            ReactionType::THINKING,
            ReactionType::PARTY,
        ] {
            let bytes = reaction_wrapper_bytes(::protobuf::EnumOrUnknown::new(r));
            assert_eq!(
                classify_packet(&bytes),
                PacketKind::Reaction,
                "a valid reaction {r:?} must classify as Reaction (forwarded), not Data or Dropped"
            );
        }
    }

    #[test]
    fn test_classify_unspecified_reaction_dropped() {
        // UNSPECIFIED(0) is the proto3 default, never a real reaction → Dropped
        // (Processed, no fan-out). Pins the "invalid → Processed-dropped" half of
        // the ingress contract; fails if the validation arm is removed (then it
        // would classify as Data, not Dropped).
        let bytes = reaction_wrapper_bytes(::protobuf::EnumOrUnknown::new(
            ReactionType::REACTION_TYPE_UNSPECIFIED,
        ));
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::Dropped,
            "an UNSPECIFIED reaction must be dropped as Processed, never forwarded"
        );
    }

    #[test]
    fn test_classify_unknown_reaction_value_dropped() {
        // An unknown/reserved wire value (e.g. a newer or forged client sending
        // 99) decodes as `EnumOrUnknown::Unknown` → Dropped. This is the
        // closed-enum allowlist the relay enforces at ingress so an attacker
        // cannot broadcast arbitrary content through this type.
        //
        // ADVERSARIAL (mutation): drop the `Err(_)` arm (forward unknowns) and 99
        // would classify as Reaction → this fails.
        let bytes = reaction_wrapper_bytes(::protobuf::EnumOrUnknown::from_i32(99));
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::Dropped,
            "an unknown/reserved reaction value must be dropped (closed-enum allowlist)"
        );
    }

    #[test]
    fn test_classify_unparseable_reaction_dropped_fail_closed() {
        // A REACTION envelope whose inner bytes are NOT a parseable
        // ReactionPacket (e.g. a client that wrongly AES-sealed it, or garbage)
        // is dropped fail-closed — never forwarded as opaque Data. The inner
        // bytes are a field-1 varint tag (0x08) followed by an unterminated
        // varint (0x80 with the continuation bit set, then EOF), which forces a
        // protobuf parse error.
        let wrapper = PacketWrapper {
            packet_type: PacketType::REACTION.into(),
            data: vec![0x08, 0x80],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::Dropped,
            "an unparseable inner ReactionPacket must be dropped fail-closed, not forwarded"
        );
    }

    /// Build a `PacketWrapper{REACTION}` whose inner `ReactionPacket` carries a
    /// `reaction` value AND a raw `custom_emoji` byte payload — the exact wire
    /// shape `classify_packet` validates for CUSTOM (issue 1884).
    fn reaction_wrapper_with_emoji(reaction: ReactionType, custom_emoji: Vec<u8>) -> Vec<u8> {
        let inner = ReactionPacket {
            reaction: reaction.into(),
            custom_emoji,
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::REACTION.into(),
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap()
    }

    #[test]
    fn test_classify_new_vocabulary_reactions_forwardable() {
        // The proto regen (issue 1884) added CRY(8), DISAGREE(9), SAD(10),
        // HEART_BROKEN(11) as fixed built-in glyphs. Each with NO custom_emoji
        // must classify as a forwardable Reaction — they take the `Ok(_)` tail
        // arm exactly like the original 1..=7 vocabulary.
        //
        // ADVERSARIAL: before the regen these were unknown wire values
        // (`Err(_) => Dropped`); this pins that they now forward. It also guards
        // the smuggling arm — if `Ok(_) if !custom_emoji.is_empty()` wrongly
        // matched an EMPTY field, these (empty) would drop and this fails.
        for r in [
            ReactionType::CRY,
            ReactionType::DISAGREE,
            ReactionType::SAD,
            ReactionType::HEART_BROKEN,
        ] {
            let bytes = reaction_wrapper_with_emoji(r, Vec::new());
            assert_eq!(
                classify_packet(&bytes),
                PacketKind::Reaction,
                "a new built-in reaction {r:?} (no custom_emoji) must forward"
            );
        }
    }

    #[test]
    fn test_classify_custom_reaction_with_valid_emoji_forwardable() {
        // CUSTOM(12) + a single standard emoji on the allowlist forwards. The
        // cases span an emoji's structural range: a plain 4-byte emoji, an 8-byte
        // regional-indicator flag, and an 18-byte ZWJ family sequence — the
        // longest, which doubles as the byte-cap sensor.
        //
        // ADVERSARIAL: making CUSTOM unconditionally Dropped fails all three;
        // SHRINKING REACTION_CUSTOM_EMOJI_MAX_BYTES below 18 rejects the family
        // sequence and fails that case (the cap-mutation receipt — every valid
        // emoji is <= 32 bytes, so only a shrink of the cap is observable).
        for emoji in ["😭", "🇲🇽", "🧑‍🤝‍🧑"] {
            let bytes =
                reaction_wrapper_with_emoji(ReactionType::CUSTOM, emoji.as_bytes().to_vec());
            assert_eq!(
                classify_packet(&bytes),
                PacketKind::Reaction,
                "CUSTOM + valid emoji {emoji:?} must classify as a forwardable Reaction"
            );
        }
    }

    #[test]
    fn test_classify_custom_reaction_with_invalid_emoji_dropped() {
        // CUSTOM(12) whose custom_emoji is NOT a single allowlisted emoji is
        // dropped fail-closed. HEADLINE mutation receipt: delete the
        // `custom_emoji_is_valid(..)` guard (CUSTOM always -> Reaction) and every
        // case below flips to Reaction -> all fail. Each case isolates one term:
        //   ""            -> empty (not a table entry)
        //   "hello"       -> a word (not an emoji)
        //   "<script>"    -> markup (XSS-shaped, not an emoji)
        //   "👍👍"        -> two concatenated emoji (not a single table entry)
        //   80-byte emoji -> over the byte cap
        //   0xff 0xfe     -> invalid UTF-8 (guards the from_utf8 gate)
        let invalid: [Vec<u8>; 6] = [
            b"".to_vec(),
            b"hello".to_vec(),
            b"<script>".to_vec(),
            "👍👍".as_bytes().to_vec(),
            "👍".repeat(20).into_bytes(),
            vec![0xff, 0xfe],
        ];
        for payload in invalid {
            let bytes = reaction_wrapper_with_emoji(ReactionType::CUSTOM, payload.clone());
            assert_eq!(
                classify_packet(&bytes),
                PacketKind::Dropped,
                "CUSTOM + invalid custom_emoji {payload:?} must be dropped fail-closed"
            );
        }
    }

    #[test]
    fn test_classify_non_custom_reaction_with_custom_emoji_dropped() {
        // A built-in glyph must NOT carry a custom_emoji. Even a VALID emoji on a
        // NON-CUSTOM reaction is field-smuggling and drops — isolating the
        // smuggling guard (the emoji itself is valid, so the ONLY reason to drop
        // is that the reaction is not CUSTOM).
        //
        // ADVERSARIAL: delete the `Ok(_) if !custom_emoji.is_empty()` arm and
        // THUMBS_UP + "👍" would forward -> this fails.
        for r in [
            ReactionType::THUMBS_UP,
            ReactionType::HEART,
            ReactionType::CRY,
        ] {
            let bytes = reaction_wrapper_with_emoji(r, "👍".as_bytes().to_vec());
            assert_eq!(
                classify_packet(&bytes),
                PacketKind::Dropped,
                "a non-CUSTOM reaction {r:?} carrying a custom_emoji must be dropped (smuggling)"
            );
        }
    }

    #[test]
    fn test_custom_emoji_is_valid_unit() {
        // Direct unit coverage of the pure validator that `classify_packet` and
        // the client's `validate_custom_emoji` must agree on (lockstep). Accept: a
        // plain emoji, a flag, a ZWJ family. Reject: empty, a word, markup, two
        // emoji, over-cap, invalid UTF-8. Mirrors the client's reactions.rs tests.
        assert!(custom_emoji_is_valid("😭".as_bytes()));
        assert!(custom_emoji_is_valid("🇲🇽".as_bytes()));
        assert!(custom_emoji_is_valid("🧑‍🤝‍🧑".as_bytes()));
        assert!(!custom_emoji_is_valid(b""));
        assert!(!custom_emoji_is_valid(b"hello"));
        assert!(!custom_emoji_is_valid(b"<script>"));
        assert!(!custom_emoji_is_valid("👍👍".as_bytes()));
        assert!(!custom_emoji_is_valid(&"👍".repeat(20).into_bytes()));
        assert!(!custom_emoji_is_valid(&[0xff, 0xfe]));
    }

    #[test]
    fn test_reaction_limiter_admits_up_to_max_then_drops() {
        // Up to REACTION_MAX_PER_WINDOW reactions in one window are admitted; the
        // next is dropped. All calls happen within microseconds so the window
        // never slides — deterministic without a sleep.
        //
        // ADVERSARIAL (mutation): raising the cap or removing the `count < max`
        // check in `try_consume` would admit the (MAX+1)th → the final assert
        // fails.
        let mut limiter = ReactionRateLimiter::new();
        for i in 0..REACTION_MAX_PER_WINDOW {
            assert!(
                limiter.allow(),
                "reaction {i} within the per-sender budget must be admitted"
            );
        }
        assert!(
            !limiter.allow(),
            "the reaction after REACTION_MAX_PER_WINDOW ({REACTION_MAX_PER_WINDOW}) in one \
             window must be dropped (over budget)"
        );
    }

    #[test]
    fn test_reaction_limiter_window_slides_and_resets() {
        // Once the window elapses the per-sender budget refills. Rewind the
        // internal `window_start` (same-module access, exactly as the keyframe
        // limiter tests do) to avoid a real sleep.
        //
        // ADVERSARIAL (mutation): remove the window-roll reset in `try_consume`
        // and the post-slide allow would stay denied → this fails.
        let mut limiter = ReactionRateLimiter::new();
        for _ in 0..REACTION_MAX_PER_WINDOW {
            assert!(limiter.allow());
        }
        assert!(
            !limiter.allow(),
            "budget must be exhausted within a single window"
        );

        // Push window_start past REACTION_WINDOW_MS into the past so the next
        // call rolls the window.
        limiter.window.window_start =
            Instant::now() - Duration::from_millis(REACTION_WINDOW_MS + 50);

        assert!(
            limiter.allow(),
            "after the window slides, the per-sender reaction budget must refill"
        );
    }

    // ---------------------------------------------------------------------
    // #1884 (security): unconditional session_id stamp + display_name bound
    // via `stamp_reaction_for_broadcast` (the relay-side attribution fix).
    // ---------------------------------------------------------------------

    /// Build the raw bytes of a `PacketWrapper{REACTION}` carrying an inner
    /// `ReactionPacket` with the given envelope `session_id`, reaction, and raw
    /// `display_name` bytes — the exact wire shape `stamp_reaction_for_broadcast`
    /// re-stamps.
    fn reaction_wrapper_with(
        session_id: u64,
        reaction: ReactionType,
        display_name: Vec<u8>,
    ) -> Vec<u8> {
        let inner = ReactionPacket {
            reaction: reaction.into(),
            display_name,
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::REACTION.into(),
            session_id,
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap()
    }

    #[test]
    fn test_stamp_reaction_overwrites_forged_session_id() {
        // SECURITY (#1884): a REACTION arriving with a NON-ZERO session_id is a
        // forge — a malicious authenticated client stamping a victim's session
        // (learned from presence) to have the reaction attributed to the
        // victim's real display name / SR announcement, cleartext, with no E2EE
        // backstop. The relay must overwrite it with THIS sender's authenticated
        // session before fan-out.
        //
        // ADVERSARIAL (mutation): revert the stamp to the old publish-path
        // "only if session_id == 0" behavior (or delete the
        // `wrapper.session_id = authenticated_session` line) and the forged 9999
        // survives → this fails.
        const FORGED_VICTIM: u64 = 9999;
        const AUTHENTICATED: u64 = 42;
        let bytes = reaction_wrapper_with(FORGED_VICTIM, ReactionType::THUMBS_UP, Vec::new());

        let stamped = stamp_reaction_for_broadcast(
            &bytes,
            AUTHENTICATED,
            crate::constants::REACTION_DISPLAY_NAME_MAX_BYTES,
        )
        .expect("a valid REACTION must re-stamp");
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.session_id, AUTHENTICATED,
            "a forged non-zero session_id must be overwritten with the authenticated session"
        );
    }

    #[test]
    fn test_stamp_reaction_stamps_zero_session_id() {
        // Happy path: a well-behaved client sends session_id = 0; the relay
        // stamps its authenticated session. ADVERSARIAL: remove the stamp and 0
        // survives → fails.
        const AUTHENTICATED: u64 = 7;
        let bytes = reaction_wrapper_with(0, ReactionType::HEART, Vec::new());

        let stamped = stamp_reaction_for_broadcast(
            &bytes,
            AUTHENTICATED,
            crate::constants::REACTION_DISPLAY_NAME_MAX_BYTES,
        )
        .unwrap();
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.session_id, AUTHENTICATED,
            "session_id=0 must be stamped with the authenticated session (no happy-path regression)"
        );
    }

    #[test]
    fn test_stamp_reaction_truncates_overlong_display_name() {
        // An oversized cosmetic display_name (egress-amplification surface) is
        // truncated to the server bound at ingress. ADVERSARIAL: remove the cap
        // branch and the 400-byte name survives → fails.
        const AUTHENTICATED: u64 = 1;
        let cap = crate::constants::REACTION_DISPLAY_NAME_MAX_BYTES;
        let bytes = reaction_wrapper_with(0, ReactionType::PARTY, "x".repeat(400).into_bytes());

        let stamped = stamp_reaction_for_broadcast(&bytes, AUTHENTICATED, cap).unwrap();
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        let out_inner = ReactionPacket::parse_from_bytes(&out.data).unwrap();
        assert!(
            out_inner.display_name.len() <= cap,
            "display_name must be truncated to the server bound"
        );
        assert!(
            !out_inner.display_name.is_empty(),
            "an all-ASCII overlong name must truncate to a non-empty bound"
        );
        assert!(
            std::str::from_utf8(&out_inner.display_name).is_ok(),
            "truncation must leave valid UTF-8"
        );
        assert_eq!(
            out_inner.reaction.enum_value(),
            Ok(ReactionType::PARTY),
            "the reaction itself must survive the name truncation"
        );
    }

    #[test]
    fn test_stamp_reaction_truncation_never_splits_a_codepoint() {
        // 100 × 'あ' (E3 81 82 = 3 bytes) = 300 bytes. A cap of 256 lands at byte
        // 256, which is mid-codepoint (256 = 3*85 + 1), so a naive byte truncate
        // would split 'あ' and yield invalid UTF-8. floor_utf8_boundary must back
        // up to byte 255 (85 whole codepoints).
        //
        // ADVERSARIAL (mutation): replace floor_utf8_boundary with a plain
        // `truncate(max)` and the result ends in a split sequence → from_utf8
        // fails → this fails.
        const AUTHENTICATED: u64 = 1;
        let bytes = reaction_wrapper_with(0, ReactionType::LAUGH, "あ".repeat(100).into_bytes());

        let stamped = stamp_reaction_for_broadcast(&bytes, AUTHENTICATED, 256).unwrap();
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        let out_inner = ReactionPacket::parse_from_bytes(&out.data).unwrap();
        assert!(out_inner.display_name.len() <= 256);
        let s = std::str::from_utf8(&out_inner.display_name)
            .expect("truncation must produce valid UTF-8 (no split codepoint)");
        assert!(
            !s.is_empty() && s.chars().all(|c| c == 'あ'),
            "only whole codepoints may survive truncation"
        );
    }

    #[test]
    fn test_stamp_reaction_preserves_enum_and_short_name_and_still_classifies() {
        // A within-bound reaction is preserved unchanged (enum + name), only the
        // session_id is stamped, and the stamped bytes STILL classify as a
        // forwardable Reaction — tying the stamp output back to the ingress
        // contract (a stamp that corrupted the packet would break re-classify).
        const AUTHENTICATED: u64 = 5;
        let bytes = reaction_wrapper_with(0, ReactionType::APPLAUSE, b"Bob".to_vec());

        let stamped = stamp_reaction_for_broadcast(
            &bytes,
            AUTHENTICATED,
            crate::constants::REACTION_DISPLAY_NAME_MAX_BYTES,
        )
        .unwrap();
        assert_eq!(
            classify_packet(&stamped),
            PacketKind::Reaction,
            "stamped bytes must still classify as a forwardable Reaction"
        );
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(out.session_id, AUTHENTICATED);
        let out_inner = ReactionPacket::parse_from_bytes(&out.data).unwrap();
        assert_eq!(out_inner.reaction.enum_value(), Ok(ReactionType::APPLAUSE));
        assert_eq!(
            out_inner.display_name,
            b"Bob".to_vec(),
            "a within-bound display_name must be preserved unchanged"
        );
    }

    #[test]
    fn test_stamp_reaction_preserves_custom_emoji_through_rewrite() {
        // The stamp path re-serializes the inner packet ONLY when it truncates an
        // overlong display_name. A CUSTOM reaction that hits that re-serialize
        // MUST round-trip its custom_emoji byte-for-byte — otherwise the relay
        // would fan out a CUSTOM reaction with a blanked-out glyph.
        //
        // ADVERSARIAL: if the stamp rebuilt a FRESH ReactionPacket (dropping
        // field 3) instead of mutating the PARSED `inner`, custom_emoji would be
        // lost on the truncation path -> this fails. The overlong display_name
        // FORCES the re-serialize branch so preservation is actually exercised.
        const AUTHENTICATED: u64 = 3;
        let emoji = "🧑‍🤝‍🧑".as_bytes().to_vec();
        let inner = ReactionPacket {
            reaction: ReactionType::CUSTOM.into(),
            display_name: "x".repeat(400).into_bytes(),
            custom_emoji: emoji.clone(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::REACTION.into(),
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();

        let cap = crate::constants::REACTION_DISPLAY_NAME_MAX_BYTES;
        let stamped = stamp_reaction_for_broadcast(&bytes, AUTHENTICATED, cap).unwrap();
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        let out_inner = ReactionPacket::parse_from_bytes(&out.data).unwrap();
        assert_eq!(
            out_inner.custom_emoji, emoji,
            "custom_emoji must survive the display_name-truncation re-serialize byte-for-byte"
        );
        assert!(
            out_inner.display_name.len() <= cap,
            "the overlong display_name must still be truncated (re-serialize path was taken)"
        );
        assert_eq!(
            out_inner.reaction.enum_value(),
            Ok(ReactionType::CUSTOM),
            "the CUSTOM reaction value must survive the rewrite"
        );
        // And the stamped bytes still pass ingress re-classification as a
        // forwardable Reaction (the stamp did not corrupt the packet).
        assert_eq!(classify_packet(&stamped), PacketKind::Reaction);
    }

    #[test]
    fn test_stamp_reaction_preserves_custom_emoji_common_path() {
        // Common in-bound path: a within-cap display_name means the stamp does
        // NOT re-serialize the inner packet — wrapper.data stays the ORIGINAL
        // bytes, so custom_emoji is trivially preserved. Pins that the no-truncate
        // fast path also keeps a CUSTOM reaction's glyph intact after the
        // session_id stamp.
        const AUTHENTICATED: u64 = 4;
        let emoji = "😭".as_bytes().to_vec();
        let inner = ReactionPacket {
            reaction: ReactionType::CUSTOM.into(),
            display_name: b"Ann".to_vec(),
            custom_emoji: emoji.clone(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::REACTION.into(),
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();

        let stamped = stamp_reaction_for_broadcast(
            &bytes,
            AUTHENTICATED,
            crate::constants::REACTION_DISPLAY_NAME_MAX_BYTES,
        )
        .unwrap();
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(out.session_id, AUTHENTICATED);
        let out_inner = ReactionPacket::parse_from_bytes(&out.data).unwrap();
        assert_eq!(
            out_inner.custom_emoji, emoji,
            "custom_emoji must be preserved on the no-truncate fast path"
        );
    }

    // =====================================================================
    // #2135: RAISE_HAND classify validation, per-sender limiter, and the
    // relay-side attribution stamp.
    //
    // Every test here is a pure function over real wire bytes — no broker, no
    // actor — so they EXECUTE in every environment rather than silently
    // skipping when NATS is unreachable.
    // =====================================================================

    /// Build the raw bytes of a `PacketWrapper{RAISE_HAND}` carrying an inner
    /// cleartext `RaiseHandPacket`. Exercises the REAL wire path
    /// `classify_packet` / `stamp_raise_hand_for_broadcast` parse (not an
    /// in-memory struct), so these tests pin the production contract.
    fn raise_hand_wrapper_with(
        session_id: u64,
        raised: bool,
        raised_at_ms: u64,
        display_name: Vec<u8>,
    ) -> Vec<u8> {
        let inner = RaiseHandPacket {
            raised,
            raised_at_ms,
            display_name,
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::RAISE_HAND.into(),
            session_id,
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap()
    }

    #[test]
    fn test_2135_classify_wellformed_raise_hand_is_forwardable() {
        // Both hand states classify as the forwardable `RaiseHand` class the
        // per-sender limiter meters before the media fan-out. A LOWER
        // (`raised = false`) matters as much as a RAISE: proto3 omits default
        // values, so a lower serializes to an EMPTY inner payload — it must not
        // be mistaken for a malformed packet and dropped, or a user could raise
        // their hand and never be able to put it down.
        //
        // ADVERSARIAL (mutation): delete the whole `PacketType::RAISE_HAND` arm
        // from `classify_packet` and these fall through to the wrapper's
        // catch-all `PacketKind::Data` -> both asserts fail. Flip the accepting
        // arm to `PacketKind::Dropped` -> both fail.
        assert_eq!(
            classify_packet(&raise_hand_wrapper_with(
                0,
                true,
                1_700_000_000_000,
                b"Ada".to_vec()
            )),
            PacketKind::RaiseHand,
            "a well-formed RAISE (raised=true) must classify as forwardable"
        );
        let lower = raise_hand_wrapper_with(0, false, 0, Vec::new());
        assert_eq!(
            PacketWrapper::parse_from_bytes(&lower).unwrap().data.len(),
            0,
            "sanity: proto3 default-elision makes a bare LOWER an EMPTY inner payload — \
             the exact shape the next assert proves is still accepted"
        );
        assert_eq!(
            classify_packet(&lower),
            PacketKind::RaiseHand,
            "a LOWER (empty inner payload after proto3 default-elision) must still be \
             forwardable — otherwise a raised hand could never be put down"
        );
    }

    #[test]
    fn test_2135_classify_drops_oversized_raise_hand_payload() {
        // The size cap is the ONLY term that bounds a RAISE_HAND as a whole.
        // rust-protobuf PRESERVES unknown fields across parse/serialize (needed
        // for forward compatibility), so the display_name bound applied later on
        // the stamp path does NOT bound the payload: without this check a forged
        // RAISE_HAND stuffed with unknown fields would be re-broadcast verbatim
        // to every participant.
        //
        // The oversized packet is deliberately WELL-FORMED (it parses fine), so
        // the ONLY thing that can reject it is the size check — the parse arm
        // cannot accidentally cover for a deleted cap.
        //
        // ADVERSARIAL (mutation): delete the
        // `if packet_wrapper.data.len() > RAISE_HAND_PACKET_MAX_BYTES` guard and
        // this classifies as `RaiseHand` -> fails.
        let huge = raise_hand_wrapper_with(0, true, 1, vec![b'x'; RAISE_HAND_PACKET_MAX_BYTES * 4]);
        let inner_len = PacketWrapper::parse_from_bytes(&huge).unwrap().data.len();
        assert!(
            inner_len > RAISE_HAND_PACKET_MAX_BYTES,
            "sanity: the fixture's inner payload ({inner_len}B) must exceed the \
             {RAISE_HAND_PACKET_MAX_BYTES}B cap or the test proves nothing"
        );
        assert!(
            RaiseHandPacket::parse_from_bytes(
                &PacketWrapper::parse_from_bytes(&huge).unwrap().data
            )
            .is_ok(),
            "sanity: the oversized fixture PARSES cleanly, so only the size cap can reject it"
        );
        assert_eq!(
            classify_packet(&huge),
            PacketKind::Dropped,
            "an over-cap RAISE_HAND payload must be dropped at ingress, never re-broadcast"
        );
    }

    #[test]
    fn test_2135_classify_accepts_raise_hand_exactly_at_the_cap() {
        // Boundary: the check is `> cap`, so a payload of EXACTLY
        // RAISE_HAND_PACKET_MAX_BYTES is admitted. Pins the comparison operator.
        //
        // ADVERSARIAL (mutation): change `>` to `>=` and this at-cap packet is
        // dropped -> fails.
        //
        // The inner payload is grown to land exactly on the cap: a `display_name`
        // of N bytes costs `2 + N` (tag + length for N <= 127, tag + 2-byte
        // varint length above), so we search rather than hardcode the arithmetic.
        let name_len = (1..RAISE_HAND_PACKET_MAX_BYTES)
            .find(|n| {
                RaiseHandPacket {
                    display_name: vec![b'x'; *n],
                    ..Default::default()
                }
                .write_to_bytes()
                .unwrap()
                .len()
                    == RAISE_HAND_PACKET_MAX_BYTES
            })
            .expect("some display_name length must serialize to exactly the cap");
        let at_cap = raise_hand_wrapper_with(0, false, 0, vec![b'x'; name_len]);
        assert_eq!(
            PacketWrapper::parse_from_bytes(&at_cap).unwrap().data.len(),
            RAISE_HAND_PACKET_MAX_BYTES,
            "sanity: the fixture must sit exactly ON the cap"
        );
        assert_eq!(
            classify_packet(&at_cap),
            PacketKind::RaiseHand,
            "a payload of exactly RAISE_HAND_PACKET_MAX_BYTES must be admitted (the check is `>`)"
        );
    }

    #[test]
    fn test_2135_classify_drops_unparseable_raise_hand_inner() {
        // Well-formedness is the second ingress term: a within-cap payload that
        // is NOT a decodable RaiseHandPacket is dropped fail-closed, so the
        // fan-out never carries bytes the relay could not decode.
        //
        // The fixture is a TRUNCATED length-delimited field 3 (`display_name`):
        // tag 0x1a, claimed length 5, but only 1 byte follows. That is a hard
        // protobuf decode error, not an unknown field that would be skipped.
        //
        // ADVERSARIAL (mutation): change the `Err(_) => PacketKind::Dropped` arm
        // to `PacketKind::RaiseHand` -> fails.
        let wrapper = PacketWrapper {
            packet_type: PacketType::RAISE_HAND.into(),
            data: vec![0x1a, 0x05, 0x61],
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert!(
            RaiseHandPacket::parse_from_bytes(&[0x1a, 0x05, 0x61]).is_err(),
            "sanity: the fixture must genuinely fail to parse, or the test proves nothing"
        );
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::Dropped,
            "an unparseable RaiseHandPacket must be dropped at ingress"
        );
    }

    #[test]
    fn test_2135_raise_hand_limiter_admits_up_to_max_then_drops() {
        // Up to RAISE_HAND_MAX_PER_WINDOW announces in one window are admitted;
        // the next is dropped. All calls happen within microseconds so the window
        // never slides — deterministic without a sleep.
        //
        // ADVERSARIAL (mutation): raise the cap, or point `RaiseHandRateLimiter`
        // at REACTION's constants, and the (MAX+1)th is admitted -> the final
        // assert fails.
        let mut limiter = RaiseHandRateLimiter::new();
        for i in 0..RAISE_HAND_MAX_PER_WINDOW {
            assert!(
                limiter.allow(),
                "raise-hand announce {i} within the per-sender budget must be admitted"
            );
        }
        assert!(
            !limiter.allow(),
            "the announce after RAISE_HAND_MAX_PER_WINDOW ({RAISE_HAND_MAX_PER_WINDOW}) in one \
             window must be dropped (over budget)"
        );
    }

    #[test]
    fn test_2135_raise_hand_limiter_window_slides_and_resets() {
        // Once the window elapses the per-sender budget refills. This is the
        // property that keeps a rate-limit drop RECOVERABLE: a raise-hand carries
        // persistent state with no relay-side registry to repair from, so a
        // limiter that could not refill would pin a participant's hand state
        // wrong for the rest of the meeting.
        //
        // Rewind the internal `window_start` (same-module access, exactly as the
        // reaction and keyframe limiter tests do) to avoid a real sleep.
        //
        // ADVERSARIAL (mutation): remove the window-roll reset in `try_consume`,
        // or give `RaiseHandRateLimiter::allow` a window LONGER than the rewind
        // (e.g. leaving it pointed at a much larger constant), and the post-slide
        // allow stays denied -> fails.
        let mut limiter = RaiseHandRateLimiter::new();
        for _ in 0..RAISE_HAND_MAX_PER_WINDOW {
            assert!(limiter.allow());
        }
        assert!(
            !limiter.allow(),
            "budget must be exhausted within a single window"
        );

        limiter.window.window_start =
            Instant::now() - Duration::from_millis(RAISE_HAND_WINDOW_MS + 50);

        assert!(
            limiter.allow(),
            "after the window slides, the per-sender raise-hand budget must refill"
        );
    }

    #[test]
    fn test_2135_stamp_raise_hand_overwrites_forged_session_id() {
        // SECURITY (#2135): attribution IS the feature. A RAISE_HAND arriving
        // with a NON-ZERO session_id is a forge — an authenticated participant
        // stamping a victim's session (learned from presence) to raise the
        // VICTIM's hand, cleartext, with no E2EE backstop. Unlike a reaction this
        // is not a transient float: it plants a durable, named entry in every
        // participant's raised-hands list until the victim notices and lowers a
        // hand they never raised.
        //
        // ADVERSARIAL (mutation): delete the
        // `wrapper.session_id = authenticated_session` line (or make it
        // fill-if-zero) and the forged 9999 survives -> fails.
        const FORGED_VICTIM: u64 = 9999;
        const AUTHENTICATED: u64 = 42;
        let bytes = raise_hand_wrapper_with(FORGED_VICTIM, true, 1_700_000_000_000, Vec::new());

        let stamped = stamp_raise_hand_for_broadcast(
            &bytes,
            AUTHENTICATED,
            crate::constants::RAISE_HAND_DISPLAY_NAME_MAX_BYTES,
        )
        .expect("a valid RAISE_HAND must re-stamp");
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.session_id, AUTHENTICATED,
            "a forged non-zero session_id must be overwritten with the authenticated session"
        );
    }

    #[test]
    fn test_2135_stamp_raise_hand_preserves_state_on_the_truncation_path() {
        // The stamp re-serializes the inner packet ONLY when it truncates an
        // overlong display_name. The STATE fields must round-trip through that
        // rewrite: a stamp that dropped them would silently convert a RAISE into
        // a LOWER (proto3 elides `false`) for every peer — the worst possible
        // failure for this feature, and one no display_name assertion catches.
        //
        // ADVERSARIAL (mutation): rebuild a FRESH `RaiseHandPacket` carrying only
        // the truncated display_name instead of mutating the PARSED `inner`, and
        // `raised` comes back false / `raised_at_ms` comes back 0 -> fails.
        // The overlong name FORCES the re-serialize branch, so preservation is
        // actually exercised rather than trivially true.
        const AUTHENTICATED: u64 = 3;
        const RAISED_AT: u64 = 1_700_000_000_123;
        let cap = crate::constants::RAISE_HAND_DISPLAY_NAME_MAX_BYTES;
        let bytes = raise_hand_wrapper_with(0, true, RAISED_AT, vec![b'x'; cap + 100]);

        let stamped = stamp_raise_hand_for_broadcast(&bytes, AUTHENTICATED, cap).unwrap();
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        let out_inner = RaiseHandPacket::parse_from_bytes(&out.data).unwrap();
        assert!(
            out_inner.display_name.len() <= cap,
            "the overlong display_name must be truncated (proving the re-serialize path ran)"
        );
        assert!(
            out_inner.raised,
            "`raised` must survive the display_name-truncation re-serialize — losing it \
             would turn a RAISE into a LOWER room-wide"
        );
        assert_eq!(
            out_inner.raised_at_ms, RAISED_AT,
            "`raised_at_ms` must survive the rewrite verbatim, or the room's ordering \
             collapses for anyone whose name happened to be overlong"
        );
    }

    #[test]
    fn test_2135_stamp_raise_hand_truncation_never_splits_a_codepoint() {
        // 100 x 'あ' (E3 81 82 = 3 bytes) = 300 bytes. A cap of 256 lands at byte
        // 256, which is mid-codepoint (256 = 3*85 + 1), so a naive byte truncate
        // would split 'あ' and yield invalid UTF-8. `floor_utf8_boundary` must
        // back up to byte 255 (85 whole codepoints).
        //
        // ADVERSARIAL (mutation): replace `floor_utf8_boundary` with a plain
        // `truncate(max)` and the result ends in a split sequence -> from_utf8
        // fails -> this fails.
        const AUTHENTICATED: u64 = 1;
        let bytes = raise_hand_wrapper_with(0, true, 7, "あ".repeat(100).into_bytes());

        let stamped = stamp_raise_hand_for_broadcast(&bytes, AUTHENTICATED, 256).unwrap();
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        let out_inner = RaiseHandPacket::parse_from_bytes(&out.data).unwrap();
        assert!(out_inner.display_name.len() <= 256);
        let s = std::str::from_utf8(&out_inner.display_name)
            .expect("truncation must produce valid UTF-8 (no split codepoint)");
        assert!(
            !s.is_empty() && s.chars().all(|c| c == 'あ'),
            "only whole codepoints may survive truncation"
        );
    }

    #[test]
    fn test_2135_stamp_raise_hand_preserves_unknown_fields_on_the_fast_path() {
        // FORWARD COMPATIBILITY, and the reason the size cap (not the stamp) is
        // what bounds this packet: on the common in-bound path the stamp does NOT
        // re-serialize the inner packet, so a NEWER client's added field survives
        // an OLDER relay byte-for-byte instead of being silently stripped.
        //
        // The "future field" is simulated as an unknown field 9 (varint) appended
        // to a current-shape payload — exactly what a `uint64 something = 9;`
        // added later would put on the wire.
        //
        // ADVERSARIAL (mutation): make the stamp ALWAYS rebuild `wrapper.data`
        // from a fresh `RaiseHandPacket` (dropping `special_fields`) and the
        // unknown field is gone -> fails.
        const AUTHENTICATED: u64 = 11;
        let mut inner_bytes = RaiseHandPacket {
            raised: true,
            raised_at_ms: 5,
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap();
        // field 9, wiretype 0 (varint) = (9 << 3) | 0 = 0x48; value 1.
        inner_bytes.extend_from_slice(&[0x48, 0x01]);
        let wrapper = PacketWrapper {
            packet_type: PacketType::RAISE_HAND.into(),
            data: inner_bytes.clone(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();
        assert_eq!(
            classify_packet(&bytes),
            PacketKind::RaiseHand,
            "sanity: a packet carrying an unknown (future) field must still pass ingress"
        );

        let stamped = stamp_raise_hand_for_broadcast(
            &bytes,
            AUTHENTICATED,
            crate::constants::RAISE_HAND_DISPLAY_NAME_MAX_BYTES,
        )
        .unwrap();
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(out.session_id, AUTHENTICATED);
        assert_eq!(
            out.data, inner_bytes,
            "on the no-truncate fast path the inner payload — including a future \
             client's unknown field — must be forwarded byte-for-byte"
        );
    }

    #[test]
    fn test_2135_stamp_raise_hand_preserves_unknown_fields_through_truncation() {
        // The companion to the fast-path test above, and the one that actually
        // VERIFIES the mechanism claimed in `RAISE_HAND_PACKET_MAX_BYTES`'s doc:
        // rust-protobuf carries unknown fields in `special_fields` and RE-EMITS
        // them on `write_to_bytes`. That is what makes the size cap load-bearing
        // (the display_name bound cannot bound what it does not know about) and
        // what keeps forward compatibility on the truncation path too.
        //
        // Asserting it here rather than asserting it in prose: the overlong
        // display_name FORCES the re-serialize branch, so if rust-protobuf ever
        // stopped preserving unknown fields — or if this function were changed to
        // rebuild a canonical packet — the future field would vanish and this
        // fails.
        //
        // ADVERSARIAL (mutation): rebuild `wrapper.data` from a fresh
        // `RaiseHandPacket` (copying only the known fields) instead of
        // re-serializing the PARSED `inner`, and the unknown field is stripped
        // -> fails.
        const AUTHENTICATED: u64 = 12;
        let cap = crate::constants::RAISE_HAND_DISPLAY_NAME_MAX_BYTES;
        let mut inner_bytes = RaiseHandPacket {
            raised: true,
            raised_at_ms: 5,
            display_name: vec![b'x'; cap + 100],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap();
        // field 9, wiretype 0 (varint) = (9 << 3) | 0 = 0x48; value 1.
        inner_bytes.extend_from_slice(&[0x48, 0x01]);
        let wrapper = PacketWrapper {
            packet_type: PacketType::RAISE_HAND.into(),
            data: inner_bytes,
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();

        let stamped = stamp_raise_hand_for_broadcast(&bytes, AUTHENTICATED, cap).unwrap();
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        let out_inner = RaiseHandPacket::parse_from_bytes(&out.data).unwrap();
        assert!(
            out_inner.display_name.len() <= cap,
            "sanity: the overlong name must be truncated, proving the re-serialize path ran"
        );
        assert!(
            out_inner
                .special_fields
                .unknown_fields()
                .get(9)
                .is_some_and(|v| v == protobuf::UnknownValueRef::Varint(1)),
            "the future client's unknown field 9 must survive the display_name-truncation \
             re-serialize — this is the `special_fields` round-trip the size cap's doc relies on"
        );
    }

    #[test]
    fn test_2135_stamp_raise_hand_output_still_classifies() {
        // Ties the stamp output back to the ingress contract: the stamped bytes
        // must STILL classify as a forwardable RaiseHand. A stamp that corrupted
        // the packet (or pushed it over the size cap) would break re-classify.
        const AUTHENTICATED: u64 = 5;
        let bytes = raise_hand_wrapper_with(0, true, 99, b"Bob".to_vec());

        let stamped = stamp_raise_hand_for_broadcast(
            &bytes,
            AUTHENTICATED,
            crate::constants::RAISE_HAND_DISPLAY_NAME_MAX_BYTES,
        )
        .unwrap();
        assert_eq!(
            classify_packet(&stamped),
            PacketKind::RaiseHand,
            "stamped bytes must still classify as a forwardable RaiseHand"
        );
        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        let out_inner = RaiseHandPacket::parse_from_bytes(&out.data).unwrap();
        assert!(out_inner.raised);
        assert_eq!(out_inner.raised_at_ms, 99);
        assert_eq!(
            out_inner.display_name,
            b"Bob".to_vec(),
            "a within-bound display_name must be preserved unchanged"
        );
    }

    #[test]
    fn test_2135_stamp_raise_hand_is_fail_closed_on_garbage() {
        // Fail-closed default: unparseable input returns `None`, and the caller
        // DROPS rather than fanning out an unstamped packet. Unreachable for a
        // packet `classify_packet` already accepted, but the default must be the
        // safe one.
        //
        // ADVERSARIAL (mutation): change either `.ok()?` to a
        // `.unwrap_or_default()` and this returns `Some(..)` -> fails.
        assert!(
            stamp_raise_hand_for_broadcast(&[0xff, 0xff, 0xff, 0xff], 1, 256).is_none(),
            "an unparseable wrapper must fail closed (None => caller drops)"
        );
    }

    // ---------------------------------------------------------------------
    // #2095 (security): unconditional envelope identity stamp on the generic
    // broadcast path via `stamp_wrapper_for_broadcast`.
    //
    // Every one of these tests runs with NO broker and NO actor — the stamp is
    // a pure function precisely so its guarantees are pinned by tests that
    // actually EXECUTE in every environment, not ones that silently skip when
    // NATS is unreachable.
    // ---------------------------------------------------------------------

    /// The relay-authenticated identity used across the #2095 tests. Distinct
    /// from every forged value below so a mix-up cannot pass by coincidence.
    const AUTH_SESSION: u64 = 4242;
    const AUTH_USER: &str = "attacker@example.com";
    /// A live peer's identity the attacker learned from presence.
    const VICTIM_SESSION: u64 = 777;
    const VICTIM_USER: &str = "victim@example.com";

    /// Build the raw bytes of a peer-facing HEALTH `PacketWrapper` — the exact
    /// wire shape that carries device info to every participant's UI. The inner
    /// packet holds ONLY the seven device fields, mirroring what
    /// `client_diagnostics::trim_health_packet_for_peers` emits before fan-out
    /// (it strips the inner identity scalars, which is WHY the outer envelope is
    /// the only attribution the peer UI has — the premise of this whole fix).
    fn health_wrapper_with(session_id: u64, user_id: &str, cores: u32) -> Vec<u8> {
        use videocall_types::protos::health_packet::HealthPacket;

        let inner = HealthPacket {
            client_cores: Some(cores),
            client_os: Some("macOS".to_string()),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            session_id,
            user_id: user_id.as_bytes().to_vec(),
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap()
    }

    #[test]
    fn stamp_wrapper_overwrites_forged_nonzero_session_id() {
        // SECURITY (#2095), the core of the fix. Pre-#2095 the broadcast path
        // stamped session_id FILL-IF-ZERO, so a client that supplied a NON-ZERO
        // value had it forwarded to every peer untouched. Because the peer UI
        // keys `set_peer_device_info` off this outer field (the inner identity
        // scalars are stripped by `trim_health_packet_for_peers`), a value
        // matching a live peer overwrote THAT peer's rendered device info.
        //
        // MUTATION PROOF: wrap the assignment in the old
        // `if wrapper.session_id == 0 { .. }` guard — or delete the
        // `wrapper.session_id = authenticated_session;` line — and the forged
        // VICTIM_SESSION survives, so this assert fails.
        let bytes = health_wrapper_with(VICTIM_SESSION, AUTH_USER, 8);

        let stamped = stamp_wrapper_for_broadcast(&bytes, AUTH_SESSION, AUTH_USER)
            .expect("a well-formed PacketWrapper must stamp, not drop");

        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.session_id, AUTH_SESSION,
            "a forged NON-ZERO envelope session_id must be replaced by the \
             sender's authenticated session before peer fan-out"
        );
    }

    #[test]
    fn stamp_wrapper_overwrites_forged_user_id() {
        // SECURITY (#2095): `PacketWrapper.user_id` was never stamped at all —
        // not even fill-if-empty. The receiving client feeds BOTH outer scalars
        // to `ensure_peer(session_id, user_id)`, which mints the peer entry and
        // its user-id/email fallback label from the first packet seen for a
        // session, so an unstamped user_id let a sender label its own tile with
        // another participant's identity.
        //
        // MUTATION PROOF: delete the `wrapper.user_id = ...` branch and the
        // forged VICTIM_USER survives, so this assert fails.
        let bytes = health_wrapper_with(0, VICTIM_USER, 8);

        let stamped = stamp_wrapper_for_broadcast(&bytes, AUTH_SESSION, AUTH_USER)
            .expect("a well-formed PacketWrapper must stamp, not drop");

        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.user_id,
            AUTH_USER.as_bytes(),
            "a client-supplied envelope user_id must be replaced by the \
             sender's authenticated user id before peer fan-out"
        );
    }

    #[test]
    fn stamp_wrapper_health_device_info_survives_identity_rewrite() {
        // The user-visible half of #2095, end to end on the REAL peer-facing
        // shape: run the relay's peer trim (which is what strips the inner
        // identity scalars) and then the stamp, and assert BOTH that the forged
        // envelope identity is gone AND that the device payload the peer UI
        // renders is untouched. A "fix" that closed the attribution hole by
        // mangling the device fields would break the Device panel for everyone.
        //
        // MUTATION PROOF: revert either stamp and the identity asserts fail;
        // rebuild the wrapper from scratch instead of mutating the parsed one
        // and the device-field asserts fail.
        use videocall_types::protos::health_packet::HealthPacket;

        let forged = health_wrapper_with(VICTIM_SESSION, VICTIM_USER, 12);
        let trimmed =
            crate::client_diagnostics::health_processor::trim_health_packet_for_peers(&forged);

        let stamped = stamp_wrapper_for_broadcast(&trimmed, AUTH_SESSION, AUTH_USER)
            .expect("a well-formed PacketWrapper must stamp, not drop");

        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.session_id, AUTH_SESSION,
            "peer device info must be attributed to the sender's authenticated session"
        );
        assert_eq!(
            out.user_id,
            AUTH_USER.as_bytes(),
            "peer device info must be attributed to the sender's authenticated user id"
        );
        assert_eq!(
            out.packet_type.enum_value(),
            Ok(PacketType::HEALTH),
            "the packet type must be untouched — the relay stamps identity, not routing"
        );
        let inner = HealthPacket::parse_from_bytes(&out.data).unwrap();
        assert_eq!(
            inner.client_cores,
            Some(12),
            "the device payload the peer UI renders must survive the identity rewrite"
        );
        assert_eq!(inner.client_os.as_deref(), Some("macOS"));
    }

    #[test]
    fn stamp_wrapper_still_fills_a_zero_session_id() {
        // Happy-path neutrality: on the media path the well-behaved client
        // leaves session_id at the proto3 default 0 (`transform_video_chunk` /
        // `transform_screen_chunk` / `transform_audio_chunk` never set it), which
        // the pre-#2095 fill-if-zero stamp already handled. That behavior must be
        // preserved, otherwise every legitimate packet loses its attribution.
        //
        // NOT a claim that no client packet carries a non-zero outer session_id:
        // `videocall-client`'s `build_heartbeat_packet`
        // (connection/connection.rs ~572-573) DOES set it, from the connection's
        // OWN server-assigned id. That packet is `PacketType::MEDIA` with
        // `media_kind` unset, so `classify_packet` returns `PacketKind::Data` and
        // it is forwarded through this stamp — where the write is a genuine
        // no-op, because the value it carries already equals the authenticated
        // session. `stamp_wrapper_overwrites_forged_nonzero_session_id` covers
        // the case where those two DISAGREE, which is the attack.
        let bytes = health_wrapper_with(0, AUTH_USER, 4);

        let stamped = stamp_wrapper_for_broadcast(&bytes, AUTH_SESSION, AUTH_USER)
            .expect("a well-formed PacketWrapper must stamp, not drop");

        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.session_id, AUTH_SESSION,
            "session_id = 0 must still be stamped with the authenticated session"
        );
    }

    #[test]
    fn stamp_wrapper_preserves_payload_and_cleartext_discriminators() {
        // WIRE NEUTRALITY: the stamp touches fields 2 and 4 ONLY. Every other
        // field is load-bearing downstream — `packet_type` (1) drives
        // `classify_packet` and the client's packet dispatch, `data` (3) is the
        // AES-sealed media payload, `simulcast_layer_id` (5) drives the relay's
        // per-receiver layer filter, and `media_kind` (6) drives viewport-aware
        // VIDEO filtering. Clobbering any of them would break media delivery
        // while the security asserts above stayed green.
        //
        // MUTATION PROOF: build a fresh `PacketWrapper` inside the stamp instead
        // of mutating the parsed one and all four asserts below fail.
        let payload = b"sealed-media-bytes".to_vec();
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            media_kind: MediaKind::SCREEN.into(),
            simulcast_layer_id: 2,
            session_id: VICTIM_SESSION,
            user_id: VICTIM_USER.as_bytes().to_vec(),
            data: payload.clone(),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().unwrap();

        let stamped = stamp_wrapper_for_broadcast(&bytes, AUTH_SESSION, AUTH_USER)
            .expect("a well-formed PacketWrapper must stamp, not drop");

        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.data, payload,
            "the sealed media payload must be untouched"
        );
        assert_eq!(out.packet_type.enum_value(), Ok(PacketType::MEDIA));
        assert_eq!(out.media_kind.enum_value(), Ok(MediaKind::SCREEN));
        assert_eq!(
            out.simulcast_layer_id, 2,
            "the cleartext simulcast layer id must survive the stamp"
        );
        // ...and the stamped bytes still classify identically at ingress, tying
        // the stamp output back to the relay's own routing contract.
        assert!(matches!(
            classify_packet(&stamped),
            PacketKind::Media { .. }
        ));
    }

    #[test]
    fn stamp_wrapper_writes_empty_when_the_session_has_no_user_id() {
        // GUEST / EMPTY-IDENTITY case. The authenticated user id is the JWT
        // `sub`; if the relay has no user id for a session, the correct stamp is
        // EMPTY. Forwarding the client's self-asserted id instead would be
        // exactly the trust this fix removes, so "empty overwrites non-empty"
        // has to hold rather than degrading to fill-if-empty.
        //
        // MUTATION PROOF: change the write to `if wrapper.user_id.is_empty()`
        // (fill-if-empty) and the forged VICTIM_USER survives -> this fails.
        let bytes = health_wrapper_with(0, VICTIM_USER, 4);

        let stamped = stamp_wrapper_for_broadcast(&bytes, AUTH_SESSION, "")
            .expect("a well-formed PacketWrapper must stamp, not drop");

        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert!(
            out.user_id.is_empty(),
            "an empty authenticated user id must CLEAR a client-supplied one, \
             not fall back to it"
        );
        assert_eq!(
            out.session_id, AUTH_SESSION,
            "the session stamp is independent of the user_id stamp"
        );
    }

    #[test]
    fn stamp_wrapper_skipped_user_id_write_is_observationally_identical() {
        // PERFORMANCE-CORRECTNESS PIN. `user_id` is `Vec<u8>`, so an
        // unconditional write would allocate a fresh buffer for EVERY packet on
        // the relay's hottest loop. The stamp therefore writes only when the
        // value DIFFERS. This test pins that the branch is observationally
        // identical to an unconditional write: two inputs that differ ONLY in
        // whether the client already sent the authenticated user_id must produce
        // byte-identical output.
        //
        // MUTATION PROOF: make the branch skip the write when the value differs
        // (e.g. invert the `!=`) and the two outputs diverge -> this fails.
        let already_correct = health_wrapper_with(0, AUTH_USER, 4);
        let forged = health_wrapper_with(0, VICTIM_USER, 4);

        // `.expect` on BOTH so the assert cannot pass vacuously on `None == None`
        // if the function ever started dropping well-formed input.
        let from_correct = stamp_wrapper_for_broadcast(&already_correct, AUTH_SESSION, AUTH_USER)
            .expect("a well-formed PacketWrapper must stamp, not drop");
        let from_forged = stamp_wrapper_for_broadcast(&forged, AUTH_SESSION, AUTH_USER)
            .expect("a well-formed PacketWrapper must stamp, not drop");

        assert_eq!(
            from_correct, from_forged,
            "skipping the write when the value already matches must yield the \
             same bytes as rewriting it"
        );
    }

    #[test]
    fn stamp_wrapper_drops_unparseable_bytes_instead_of_forwarding_them() {
        // SECURITY (#2095 review, HIGH): fail CLOSED, like
        // `stamp_reaction_for_broadcast`. `classify_packet` routes bytes that
        // FAIL to parse as a PacketWrapper to `PacketKind::Data` ("unparseable,
        // treat as opaque data"), so they reach this function; the pre-#2095
        // code forwarded them verbatim, and so did the first cut of this stamp.
        //
        // That is a relay-amplified remote DoS on the DEFAULT transport. The
        // receiving client's `From<Binary> for PacketWrapper`
        // (videocall-types/src/lib.rs ~132-136) does
        // `parse_from_bytes(..).unwrap()`, and a wasm panic ABORTS — trapping the
        // module and killing the call for that tab. WebTransport drops a bad
        // datagram cleanly, but WebSocket has been the default since #2045, and
        // `handle_msg`'s outbound filters are all `parsed.map(..).unwrap_or(false)`
        // so an unparseable frame matches no drop condition. One 4-byte frame
        // from any authenticated participant crashed every other tab in the call.
        //
        // The assert is deliberately `is_none()` and NOT "the output is empty":
        // an empty `Vec` would still be published and parses as a DEFAULT
        // wrapper, so only the `None`/skip-the-publish contract is safe.
        //
        // MUTATION PROOF: restore `return data.to_vec()` (or `Some(data.to_vec())`,
        // or `Some(Vec::new())`) on the parse-failure arm and this fails.
        let garbage = vec![0xFF_u8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(
            PacketWrapper::parse_from_bytes(&garbage).is_err(),
            "precondition: these bytes must not parse as a PacketWrapper"
        );

        assert!(
            stamp_wrapper_for_broadcast(&garbage, AUTH_SESSION, AUTH_USER).is_none(),
            "unparseable bytes must be DROPPED, never forwarded: the receiving \
             client unwraps its parse and a wasm panic aborts the whole tab"
        );

        // The 4-byte frame from the report, verbatim — the cheapest form of the
        // attack, and the one that must classify as `Data` to reach this function
        // at all. Pinning the classification here ties the drop to the actual
        // reachable path rather than to a hand-picked byte string.
        let four_bytes = vec![0xFF_u8, 0xFF, 0xFF, 0xFF];
        assert!(
            matches!(classify_packet(&four_bytes), PacketKind::Data),
            "precondition: unparseable bytes reach the broadcast path as Data"
        );
        assert!(
            stamp_wrapper_for_broadcast(&four_bytes, AUTH_SESSION, AUTH_USER).is_none(),
            "the 4-byte crash frame must be dropped at the relay"
        );
    }

    #[test]
    fn stamp_wrapper_defeats_an_appended_duplicate_field_forgery() {
        // The E2E companion, at unit level. A malicious client does not have to
        // build a whole wrapper: protobuf takes LAST-WINS for a repeated scalar,
        // so it can take a legitimate serialized packet and simply APPEND another
        // `user_id` (field 2, wire type 2) and `session_id` (field 4, wire type 0)
        // to override what it already wrote. This is byte-for-byte the forgery
        // the `peer-device-metrics` E2E performs inside the browser, so pinning it
        // here means the relay-side guarantee is proven even when the browser
        // harness cannot be run.
        //
        // It also pins the premise the E2E depends on: that appending really does
        // override (i.e. protobuf last-wins), which would silently change if the
        // `protobuf` crate ever altered that semantic.
        //
        // MUTATION PROOF: revert either stamp and the appended values survive, so
        // both asserts fail. Remove the `precondition` assert and a protobuf
        // behaviour change would make this test vacuous instead of loud.
        let mut bytes = health_wrapper_with(0, AUTH_USER, 8);

        let victim = VICTIM_USER.as_bytes();
        bytes.push(0x12); // field 2 (user_id), wire type 2 (length-delimited)
        bytes.push(victim.len() as u8);
        bytes.extend_from_slice(victim);
        bytes.push(0x20); // field 4 (session_id), wire type 0 (varint)
                          // LEB128 for VICTIM_SESSION (777 = 6 * 128 + 9): low group 9 with the
                          // continuation bit set, then 6.
        bytes.push(0x89);
        bytes.push(0x06);

        // Precondition: the append really is a forgery — an UNSTAMPED parse of
        // these bytes yields the attacker's values, not the client's originals.
        let raw = PacketWrapper::parse_from_bytes(&bytes).expect("appended fields must parse");
        assert_eq!(
            raw.session_id, VICTIM_SESSION,
            "precondition: protobuf last-wins must let the appended session_id override"
        );
        assert_eq!(
            raw.user_id, victim,
            "precondition: protobuf last-wins must let the appended user_id override"
        );

        let stamped = stamp_wrapper_for_broadcast(&bytes, AUTH_SESSION, AUTH_USER)
            .expect("a well-formed PacketWrapper must stamp, not drop");

        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.session_id, AUTH_SESSION,
            "an appended-duplicate session_id forgery must not survive the stamp"
        );
        assert_eq!(
            out.user_id,
            AUTH_USER.as_bytes(),
            "an appended-duplicate user_id forgery must not survive the stamp"
        );
    }

    #[test]
    fn stamp_wrapper_drops_an_earlier_duplicate_when_the_user_id_write_is_skipped() {
        // The REVERSE ordering of the test above, and the one that exercises the
        // SKIPPED `user_id` write. There the appended copy was the victim's, so
        // the parsed value DIFFERED from the authenticated one and the write ran.
        // Here the appended copy is the AUTHENTICATED id: protobuf last-wins makes
        // the parsed value already equal to it, so `wrapper.user_id !=
        // authenticated_user_id.as_bytes()` is FALSE and the stamp writes nothing
        // — while an EARLIER victim copy is still sitting on the input wire.
        //
        // That is the one shape where "skip the write" could plausibly differ from
        // an unconditional write, so it is asserted rather than argued. It holds
        // because the stamp re-SERIALIZES from the parsed message: rust-protobuf
        // emits each scalar exactly once, so the earlier duplicate is dropped on
        // write-out and cannot reach a peer's parser to win a different last-wins
        // race. Same for the session_id half, appended in the same direction.
        //
        // MUTATION PROOF (verified, not asserted): returning the INPUT bytes
        // instead of `write_to_bytes()` output on the success path (e.g.
        // `Some(data.to_vec())`) leaves the victim copy on the wire -> the "no
        // victim bytes" assert and the byte-identity assert both fail.
        //
        // COVERAGE LIMIT, stated so no one over-reads this test: inverting the
        // `!=` guard does NOT fail here, and cannot. This input is constructed so
        // the parsed `user_id` ALREADY equals the authenticated one, so an
        // inverted guard just rewrites the same bytes — that is precisely the
        // "skipped write is a no-op" property being pinned. The inverted guard is
        // caught by `stamp_wrapper_overwrites_forged_user_id` and
        // `stamp_wrapper_writes_empty_when_the_session_has_no_user_id`, whose
        // inputs DIFFER from the authenticated value.
        let mut bytes = health_wrapper_with(VICTIM_SESSION, VICTIM_USER, 8);

        let auth = AUTH_USER.as_bytes();
        bytes.push(0x12); // field 2 (user_id), wire type 2 (length-delimited)
        bytes.push(auth.len() as u8);
        bytes.extend_from_slice(auth);
        bytes.push(0x20); // field 4 (session_id), wire type 0 (varint)
                          // LEB128 for AUTH_SESSION (4242 = 33 * 128 + 18): low group
                          // 18 with the continuation bit set, then 33.
        bytes.push(0x92);
        bytes.push(0x21);

        // Precondition: last-wins really does make the PARSED values already equal
        // to the authenticated ones, i.e. the stamp's `!=` guard is FALSE and the
        // user_id write is genuinely skipped on this input. Without this the test
        // would silently degrade into a copy of the previous one.
        let raw = PacketWrapper::parse_from_bytes(&bytes).expect("appended fields must parse");
        assert_eq!(
            raw.user_id, auth,
            "precondition: the appended authenticated user_id must win, so the \
             stamp's conditional write is SKIPPED for this input"
        );
        assert_eq!(raw.session_id, AUTH_SESSION, "precondition: last-wins");
        // ...and the victim's copy really is still present in the INPUT bytes, so
        // the post-stamp absence assert below is meaningful.
        assert!(
            bytes
                .windows(VICTIM_USER.len())
                .any(|w| w == VICTIM_USER.as_bytes()),
            "precondition: the earlier victim user_id must still be on the input wire"
        );

        let stamped = stamp_wrapper_for_broadcast(&bytes, AUTH_SESSION, AUTH_USER)
            .expect("a well-formed PacketWrapper must stamp, not drop");

        let out = PacketWrapper::parse_from_bytes(&stamped).unwrap();
        assert_eq!(
            out.user_id, auth,
            "the skipped write must still leave the authenticated user_id in place"
        );
        assert_eq!(out.session_id, AUTH_SESSION);
        assert!(
            !stamped
                .windows(VICTIM_USER.len())
                .any(|w| w == VICTIM_USER.as_bytes()),
            "the earlier duplicate user_id must be GONE from the published bytes — \
             re-serializing from the parsed message is what removes it"
        );

        // Strongest form of the equivalence: the output is byte-identical to
        // stamping a clean wrapper that already carried the authenticated
        // identity, so a peer cannot tell the forgery attempt happened at all.
        let clean = stamp_wrapper_for_broadcast(
            &health_wrapper_with(0, AUTH_USER, 8),
            AUTH_SESSION,
            AUTH_USER,
        )
        .expect("a well-formed PacketWrapper must stamp, not drop");
        assert_eq!(
            stamped, clean,
            "a duplicate-field forgery must serialize to exactly the same bytes as \
             an honest packet from the same session"
        );
    }

    // ---------------------------------------------------------------------
    // #2136 — MEETING_TIMER ingress validation + rate limiting
    // ---------------------------------------------------------------------

    /// Build the raw bytes of a `PacketWrapper{MEETING_TIMER}` carrying an inner
    /// `MeetingTimerPacket` — the exact wire shape `classify_packet` validates.
    fn meeting_timer_wrapper_with(
        running: bool,
        ends_at_ms: u64,
        duration_ms: u64,
        updated_at_ms: u64,
    ) -> Vec<u8> {
        use videocall_types::protos::meeting_timer_packet::MeetingTimerPacket;
        let inner = MeetingTimerPacket {
            running,
            ends_at_ms,
            duration_ms,
            updated_at_ms,
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING_TIMER.into(),
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap()
    }

    /// A well-formed START and a well-formed CANCEL must both be forwardable.
    ///
    /// The CANCEL half is the one that earns its keep. proto3 elides default
    /// values, so a bare cancel (`running = false` and nothing else) serializes
    /// to an EMPTY inner payload — a shape it is very easy to reject by accident
    /// (an `is_empty()` guard, a "must have an end time" check). If a cancel
    /// cannot cross the relay, a host can start a timer it can never stop, and
    /// the room hears the expiry sound anyway.
    ///
    /// MUTATION PROOF: delete the `PacketType::MEETING_TIMER` arm from
    /// `classify_packet` and both asserts fail (the packet falls through to
    /// `PacketKind::Data`).
    #[test]
    fn test_2136_classify_wellformed_meeting_timer_is_forwardable() {
        assert_eq!(
            classify_packet(&meeting_timer_wrapper_with(
                true,
                1_700_000_300_000,
                300_000,
                1_700_000_000_000
            )),
            PacketKind::MeetingTimer,
            "a well-formed START (running = true) must classify as forwardable"
        );

        let cancel = meeting_timer_wrapper_with(false, 0, 0, 0);
        assert_eq!(
            PacketWrapper::parse_from_bytes(&cancel).unwrap().data.len(),
            0,
            "sanity: proto3 default-elision makes a bare CANCEL an EMPTY inner payload — \
             the exact shape the next assert proves is still accepted"
        );
        assert_eq!(
            classify_packet(&cancel),
            PacketKind::MeetingTimer,
            "a CANCEL (empty inner payload after proto3 default-elision) must still be \
             forwardable — otherwise a started timer could never be stopped"
        );
    }

    /// An over-cap raw payload is dropped at ingress, never re-broadcast.
    ///
    /// The size cap is the ONLY bound on this packet's total size: rust-protobuf
    /// round-trips unknown fields, so without it a forged MEETING_TIMER stuffed
    /// with megabytes of unknown fields is amplified to every participant.
    ///
    /// MUTATION PROOF: remove the `data.len() > MEETING_TIMER_PACKET_MAX_BYTES`
    /// check and the oversized packet classifies as `MeetingTimer` -> fails. The
    /// second sanity assert is what makes this specific: the fixture PARSES
    /// cleanly and its `duration_ms` is in range, so the size cap is the only
    /// thing that can reject it.
    #[test]
    fn test_2136_classify_drops_oversized_meeting_timer_payload() {
        use videocall_types::protos::meeting_timer_packet::MeetingTimerPacket;

        // Unknown-field padding, since this message has no string field to
        // inflate — which is exactly the amplification shape the cap exists for.
        let mut inner = MeetingTimerPacket {
            running: true,
            ends_at_ms: 1_700_000_300_000,
            duration_ms: 300_000,
            updated_at_ms: 1_700_000_000_000,
            ..Default::default()
        };
        inner
            .special_fields
            .mut_unknown_fields()
            .add_length_delimited(9999, vec![b'x'; MEETING_TIMER_PACKET_MAX_BYTES * 4]);
        let huge = PacketWrapper {
            packet_type: PacketType::MEETING_TIMER.into(),
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap();

        let inner_len = PacketWrapper::parse_from_bytes(&huge).unwrap().data.len();
        assert!(
            inner_len > MEETING_TIMER_PACKET_MAX_BYTES,
            "sanity: the fixture's inner payload ({inner_len}B) must exceed the \
             {MEETING_TIMER_PACKET_MAX_BYTES}B cap or the test proves nothing"
        );
        let reparsed = MeetingTimerPacket::parse_from_bytes(
            &PacketWrapper::parse_from_bytes(&huge).unwrap().data,
        )
        .expect("sanity: the oversized fixture PARSES cleanly");
        assert!(
            reparsed.duration_ms <= MEETING_TIMER_MAX_DURATION_MS,
            "sanity: the fixture's duration is IN range, so only the size cap can reject it"
        );

        assert_eq!(
            classify_packet(&huge),
            PacketKind::Dropped,
            "an over-cap MEETING_TIMER payload must be dropped at ingress, never re-broadcast"
        );
    }

    /// The size cap is inclusive: a payload of EXACTLY the cap is accepted.
    ///
    /// MUTATION PROOF: change the check from `>` to `>=` and this fails, while
    /// every other test in this file stays green.
    #[test]
    fn test_2136_classify_accepts_meeting_timer_exactly_at_the_cap() {
        use videocall_types::protos::meeting_timer_packet::MeetingTimerPacket;

        let mut inner = MeetingTimerPacket {
            running: true,
            ends_at_ms: 1_700_000_300_000,
            duration_ms: 300_000,
            updated_at_ms: 1_700_000_000_000,
            ..Default::default()
        };
        let base_len = inner.write_to_bytes().unwrap().len();
        // Pad with an unknown field until the inner payload is exactly the cap.
        // `add_length_delimited` costs (tag + length varint + payload), so solve
        // for the payload length by search rather than by arithmetic guesswork.
        let pad = (0..MEETING_TIMER_PACKET_MAX_BYTES)
            .find(|n| {
                let mut probe = inner.clone();
                probe
                    .special_fields
                    .mut_unknown_fields()
                    .add_length_delimited(9999, vec![b'x'; *n]);
                probe.write_to_bytes().unwrap().len() == MEETING_TIMER_PACKET_MAX_BYTES
            })
            .unwrap_or_else(|| {
                panic!(
                    "some padding length must serialize to exactly the {MEETING_TIMER_PACKET_MAX_BYTES}B \
                     cap (base payload is {base_len}B)"
                )
            });
        inner
            .special_fields
            .mut_unknown_fields()
            .add_length_delimited(9999, vec![b'x'; pad]);
        let at_cap = PacketWrapper {
            packet_type: PacketType::MEETING_TIMER.into(),
            data: inner.write_to_bytes().unwrap(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap();

        assert_eq!(
            PacketWrapper::parse_from_bytes(&at_cap).unwrap().data.len(),
            MEETING_TIMER_PACKET_MAX_BYTES,
            "sanity: the fixture must sit EXACTLY on the cap or it pins the wrong boundary"
        );
        assert_eq!(
            classify_packet(&at_cap),
            PacketKind::MeetingTimer,
            "a payload of exactly MEETING_TIMER_PACKET_MAX_BYTES must be ACCEPTED — the cap \
             is an inclusive upper bound, not an exclusive one"
        );
    }

    /// An out-of-range `duration_ms` drops the whole packet; the cap itself is
    /// inclusive.
    ///
    /// MUTATION PROOF: delete the `timer.duration_ms <= MEETING_TIMER_MAX_DURATION_MS`
    /// guard and the first assert fails. Change `<=` to `<` and the second
    /// (at-cap) assert fails.
    #[test]
    fn test_2136_classify_bounds_meeting_timer_duration() {
        let over = meeting_timer_wrapper_with(
            true,
            1_700_000_300_000,
            MEETING_TIMER_MAX_DURATION_MS + 1,
            1_700_000_000_000,
        );
        assert!(
            PacketWrapper::parse_from_bytes(&over).unwrap().data.len()
                <= MEETING_TIMER_PACKET_MAX_BYTES,
            "sanity: the over-duration fixture is WITHIN the size cap, so only the duration \
             bound can reject it"
        );
        assert_eq!(
            classify_packet(&over),
            PacketKind::Dropped,
            "a duration beyond MEETING_TIMER_MAX_DURATION_MS must be dropped at ingress — an \
             unbounded u64 would overflow the client's progress arithmetic"
        );

        assert_eq!(
            classify_packet(&meeting_timer_wrapper_with(
                true,
                1_700_000_300_000,
                MEETING_TIMER_MAX_DURATION_MS,
                1_700_000_000_000
            )),
            PacketKind::MeetingTimer,
            "a duration of EXACTLY the cap must be accepted — the bound is inclusive"
        );
    }

    /// `ends_at_ms` is deliberately NOT range-checked, however absurd.
    ///
    /// This pins a DELIBERATE non-decision, and it is the #2122 lesson in test
    /// form. Bounding an absolute instant means comparing it against the RELAY's
    /// clock; a relay whose clock stepped backwards would then start refusing
    /// legitimate timers — a fail-closed wedge — while buying nothing, since the
    /// sender is the authorized host and may set any end time.
    ///
    /// MUTATION PROOF: add any `ends_at_ms` sanity check to the arm (e.g. reject
    /// values more than a day ahead of `SystemTime::now()`) and the first assert
    /// fails. It also fails if someone "helpfully" rejects `ends_at_ms == 0` on a
    /// running timer.
    #[test]
    fn test_2136_classify_does_no_clock_arithmetic_on_ends_at_ms() {
        assert_eq!(
            classify_packet(&meeting_timer_wrapper_with(true, u64::MAX, 300_000, 1)),
            PacketKind::MeetingTimer,
            "a far-future ends_at_ms must be forwarded: validating it requires the RELAY's \
             clock, and a backwards relay clock step would then wedge legitimate timers"
        );
        // An already-EXPIRED timer must still forward. Kept internally consistent
        // (`ends_at_ms >= duration_ms`) so this pins the no-clock-arithmetic
        // property and NOT the consistency check, which has its own test below.
        assert_eq!(
            classify_packet(&meeting_timer_wrapper_with(true, 300_000, 300_000, 1)),
            PacketKind::MeetingTimer,
            "a long-past ends_at_ms must be forwarded too — 'already expired' is a state \
             the client renders, not one the relay adjudicates"
        );
    }

    /// `ends_at_ms < duration_ms` is rejected: it underflows the
    /// `started_at_ms = ends_at_ms - duration_ms` subtraction the wire contract
    /// asks EVERY receiver to perform.
    ///
    /// This is NOT the clock check the sibling test forbids, and the distinction
    /// is the whole point: it compares two fields of the SAME packet to each
    /// other, reads no clock, and so cannot wedge if any clock steps. Without
    /// it, one forged packet panics a debug wasm build on every receiver —
    /// aborting the module and dropping the whole call for that tab.
    ///
    /// MUTATION PROOF: remove `&& timer.ends_at_ms >= timer.duration_ms` from
    /// the arm and the first assert fails. Change `>=` to `>` and the
    /// equal-values assert fails (a zero-length span is degenerate but
    /// consistent, and a CANCEL sends 0/0).
    #[test]
    fn test_2136_classify_rejects_internally_inconsistent_timer() {
        assert_eq!(
            classify_packet(&meeting_timer_wrapper_with(true, 0, 300_000, 1)),
            PacketKind::Dropped,
            "ends_at_ms < duration_ms must be dropped: every receiver computes \
             `ends_at_ms - duration_ms`, and this packet underflows it"
        );
        assert_eq!(
            classify_packet(&meeting_timer_wrapper_with(true, 300_000, 300_001, 1)),
            PacketKind::Dropped,
            "the check must bite on an underflow of ONE, not just on an obvious zero"
        );
        assert_eq!(
            classify_packet(&meeting_timer_wrapper_with(true, 300_000, 300_000, 1)),
            PacketKind::MeetingTimer,
            "equal values are consistent (a zero-length elapsed span) and must be accepted — \
             the bound is inclusive"
        );
        assert_eq!(
            classify_packet(&meeting_timer_wrapper_with(false, 0, 0, 0)),
            PacketKind::MeetingTimer,
            "a CANCEL sends 0/0 and must survive this check — rejecting it would leave a \
             host able to start a timer it can never stop"
        );
    }

    /// MEETING_TIMER and MEETING are adjacent in name and OPPOSITE in trust
    /// model. This pins the contrast their doc comments claim.
    ///
    /// MUTATION PROOF: renumber MEETING_TIMER to 7, or move the MEETING_TIMER
    /// arm above the MEETING drop and widen it, and the two asserts collide.
    /// Deleting the MEETING drop makes the second assert fail — which is the
    /// far more serious direction, since a client-forged MEETING packet
    /// broadcasts fake host actions.
    #[test]
    fn test_2136_meeting_timer_is_forwarded_where_meeting_is_dropped() {
        assert_eq!(
            classify_packet(&meeting_timer_wrapper_with(
                true,
                1_700_000_300_000,
                300_000,
                1
            )),
            PacketKind::MeetingTimer,
            "MEETING_TIMER (19) is CLIENT-authored and re-broadcast"
        );

        let client_meeting = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap();
        assert_eq!(
            classify_packet(&client_meeting),
            PacketKind::Dropped,
            "MEETING (7) is SERVER-authored: a client-originated one is always forged and \
             must stay dropped"
        );
    }

    /// The per-sender budget both BINDS and REFILLS.
    ///
    /// The refill half matters more than it looks: the relay holds no timer
    /// registry, so a limiter that could permanently wedge a host would leave a
    /// room with a countdown nobody can cancel.
    ///
    /// MUTATION PROOF: change `MEETING_TIMER_MAX_PER_WINDOW` (either direction)
    /// and the first loop or the "one past budget" assert fails. Make
    /// `WindowCounter::try_consume` never reset the window and the refill assert
    /// fails.
    #[test]
    fn test_2136_meeting_timer_rate_limiter_binds_and_refills() {
        let mut limiter = MeetingTimerRateLimiter::new();
        for i in 0..MEETING_TIMER_MAX_PER_WINDOW {
            assert!(
                limiter.allow(),
                "packet {i} must be within the per-sender budget of \
                 {MEETING_TIMER_MAX_PER_WINDOW}"
            );
        }
        assert!(
            !limiter.allow(),
            "one packet past MEETING_TIMER_MAX_PER_WINDOW must be refused"
        );

        // Pretend a full window elapsed: the budget must come back, so a host
        // cannot be locked out of controlling its own room's timer.
        limiter.rewind_window_for_test(Duration::from_millis(MEETING_TIMER_WINDOW_MS + 1));
        assert!(
            limiter.allow(),
            "the budget must REFILL once the window elapses — a limiter that could wedge \
             would leave the room with a timer nobody can cancel"
        );
    }

    /// The budget must comfortably admit the traffic shape the wire contract
    /// asks the client for: a 3-packet transition burst plus a heartbeat inside
    /// one window.
    ///
    /// This is the test that would catch someone "tidying" the budget down to
    /// REACTION's value. It asserts against the CONSTANT, not a literal, so it
    /// pins the relationship rather than a number.
    ///
    /// MUTATION PROOF: set `MEETING_TIMER_MAX_PER_WINDOW` to 3 (or to REACTION's
    /// value) and this fails.
    #[test]
    fn test_2136_meeting_timer_budget_admits_a_transition_burst_plus_heartbeat() {
        // Wire contract: 3 repeats per transition (rule 2) + 1 heartbeat (rule 1)
        // can land in the same 2s window.
        const WORST_CASE_WELL_BEHAVED_BURST: u32 = 4;
        // A `const` block, so shrinking the budget is a COMPILE error rather than
        // a test failure — this relationship is a wire-contract invariant, and
        // catching it at build time is strictly stronger than catching it at
        // test time.
        const {
            assert!(
                MEETING_TIMER_MAX_PER_WINDOW > WORST_CASE_WELL_BEHAVED_BURST,
                "MEETING_TIMER_MAX_PER_WINDOW must exceed the 4-packet worst case a \
                 WELL-BEHAVED host produces (a 3-packet cancel repeat overlapping a \
                 heartbeat) — a dropped CANCEL leaves the room counting down to a sound \
                 the host already called off"
            );
        }

        let mut limiter = MeetingTimerRateLimiter::new();
        for i in 0..WORST_CASE_WELL_BEHAVED_BURST {
            assert!(
                limiter.allow(),
                "packet {i} of a well-behaved host's worst-case burst must not be throttled"
            );
        }
    }
}
