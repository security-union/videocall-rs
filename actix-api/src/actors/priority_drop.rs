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

//! Priority-aware drop policy for per-session outbound channels.
//!
//! The relay buffers cross-peer packets in a bounded `mpsc` channel per
//! receiver session. When that channel fills up, the prior policy dropped
//! the *next* packet to arrive regardless of media type. A single 1-2 Mbps
//! video frame buffer is equivalent to ~200 50-byte audio frames, so a
//! uniform drop wastes audio frames disproportionately. This module
//! implements the priority-aware variant requested in discussion #699:
//!
//! 1. **VIDEO / SCREEN** frames are dropped first, starting at
//!    [`PRIORITY_DROP_VIDEO_FILL_RATIO`] (80% full). Brief video freezes
//!    are tolerable; audio loss is catastrophic.
//! 2. **AUDIO** frames are preserved until
//!    [`PRIORITY_DROP_AUDIO_FILL_RATIO`] (95% full). Audio is ~50 kbps
//!    and far cheaper than video, so a few extra audio frames in the
//!    queue buy more UX than a few extra video frames.
//! 3. **CONTROL** packets are never preemptively dropped. They are
//!    admitted up to the point the channel is 100% full, at which the
//!    transport-level `try_send` returns `Full` and the existing drop
//!    counter still fires (with the new `overflow_critical` label so the
//!    new policy can be distinguished from the legacy uniform drops).
//!    A subset of control packets — `SESSION_ASSIGNED`, `CONGESTION`,
//!    `RSA_PUB_KEY`, `AES_KEY`, `MEETING` — are extra-critical to
//!    session lifecycle (reconnection, E2EE handshake, host
//!    transitions) and *must* still be attempted on overflow even if
//!    the policy were later tightened. Both halves of the E2EE
//!    handshake (RSA_PUB_KEY + AES_KEY) are Critical because dropping
//!    either silently breaks encrypted communication for the affected
//!    peer pair with no page-able alert.
//!
//! The decision is per-session: no global state is introduced, and the
//! drop policy is identical for the WebTransport and WebSocket transports.
//! Both call into [`evaluate`] with their own resolved channel capacity.
//!
//! ### Why drop at the enqueue site instead of inside the bridge?
//!
//! The bridge consumes the channel sequentially; once a packet is enqueued
//! it has already cost a slot. We want to free that slot for higher-priority
//! traffic before it is consumed. Hence the policy lives at the producer
//! side (the `Handler<Message>` for both transports). The receiver bridge
//! does not need to change.
//!
//! ### Behaviour on dropped packets
//!
//! When this module returns [`PriorityDropDecision::Drop`], the caller
//! must still invoke `SessionLogic::on_outbound_drop`. Post-#1219 that no
//! longer emits a sender-keyed CONGESTION signal (one slow receiver must not
//! collapse the publisher's encode for the whole room); the publisher's own
//! uplink distress is now detected client-side. What `on_outbound_drop` still
//! does is call `CongestionTracker::record_drop`, which updates the
//! per-RECEIVER `last_congestion` timestamp that relaxes this receiver's
//! KEYFRAME_REQUEST rate limiter (#979) so its own frozen video can recover
//! faster. So the preempt-drop here still feeds a per-receiver response — it
//! just no longer drives the removed sender-keyed CONGESTION path.

use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};

/// Channel-fill ratio at which VIDEO and SCREEN media packets begin
/// being dropped to make room for higher-priority audio and control.
///
/// 80% of the bounded outbound channel. Below this, video/audio/control
/// are all admitted. Tuned together with [`PRIORITY_DROP_AUDIO_FILL_RATIO`]
/// to give audio a 15-percentage-point cushion over video.
pub const PRIORITY_DROP_VIDEO_FILL_RATIO: f32 = 0.80;

/// Channel-fill ratio at which AUDIO media packets begin being dropped.
///
/// 95% of the bounded outbound channel. Audio is ~50 kbps versus video
/// at ~1-2 Mbps, so a single audio packet in the queue is cheap. We
/// preserve it until the channel is nearly saturated — losing audio
/// frames is catastrophic for the call experience.
pub const PRIORITY_DROP_AUDIO_FILL_RATIO: f32 = 0.95;

/// Classification of an outbound packet for priority-drop purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundPriority {
    /// Critical lifecycle / key-exchange packet that must never be
    /// preemptively dropped: `SESSION_ASSIGNED`, `CONGESTION`,
    /// `RSA_PUB_KEY`, `AES_KEY`, `MEETING`. These ride the outbound
    /// channel like any other packet, but the priority policy will
    /// admit them regardless of channel fill.
    Critical,
    /// Generic control / non-media packet (DIAGNOSTICS, HEALTH,
    /// KEYFRAME_REQUEST relayed back to senders, …). Not preemptively
    /// dropped; only fails on actual channel overflow.
    Control,
    /// Audio media frame. Dropped when fill ratio reaches
    /// [`PRIORITY_DROP_AUDIO_FILL_RATIO`].
    Audio,
    /// Video media frame. Dropped when fill ratio reaches
    /// [`PRIORITY_DROP_VIDEO_FILL_RATIO`].
    Video,
    /// Screen-share media frame. Same drop threshold as `Video`.
    Screen,
}

impl OutboundPriority {
    /// Map an outer [`PacketType`] plus the inner [`MediaType`] (when
    /// the outer is `MEDIA`) to an [`OutboundPriority`].
    ///
    /// `parsed = false` means the outer wrapper failed to parse;
    /// treated as `Control` so we never proactively drop something we
    /// could not classify (matching the WS-site fallback for the
    /// drop-kind metric label).
    pub fn classify(parsed: bool, packet_type: PacketType, media_type: Option<MediaType>) -> Self {
        if !parsed {
            return OutboundPriority::Control;
        }
        match packet_type {
            // Media is the bulk of traffic. Refine on inner MediaType.
            PacketType::MEDIA => match media_type {
                Some(MediaType::AUDIO) => OutboundPriority::Audio,
                Some(MediaType::VIDEO) => OutboundPriority::Video,
                Some(MediaType::SCREEN) => OutboundPriority::Screen,
                // HEARTBEAT, RTT echo replays, KEYFRAME_REQUEST relayed
                // to senders, encrypted/unparseable inner — treat as
                // control: low-volume, valuable, never preemptively
                // dropped. (RTT echo never reaches Handler<Message>; it
                // is handled inline in the inbound path. We include it
                // here for completeness in case the classification is
                // ever reused on that hot path.)
                _ => OutboundPriority::Control,
            },
            // Non-media wrappers share their Critical/Control split with
            // `classify_outer` (single source of truth for the Critical
            // set).
            other => Self::classify_non_media(other),
        }
    }

    /// Map an outer [`PacketType`] plus the OUTER cleartext [`MediaKind`]
    /// (`PacketWrapper.media_kind`, wire field 5) to an [`OutboundPriority`],
    /// WITHOUT parsing the inner [`MediaPacket`].
    ///
    /// This is the variant used at the relay's **inbound NATS fan-out hop**
    /// (`chat_server.rs` `handle_msg`), where the only thing available for
    /// free is the already-parsed outer wrapper. Crucially, the inner
    /// `MediaPacket.media_type` is AES-sealed when E2EE is enabled, so the
    /// fan-out path MUST NOT depend on it — it classifies off the cleartext
    /// outer `media_kind` the relay already reads for the #988/#989 filters.
    ///
    /// Fail-open contract (matches the #988/#989 filter convention):
    /// * `parsed = false` (outer wrapper failed to decode) → `Control`
    ///   (never preemptively shed something we could not classify).
    /// * `media_kind` `UNSPECIFIED` or any unknown value → `Control`
    ///   (older clients / non-discriminated packets are never shed).
    /// * any non-`MEDIA` packet type → the SAME Critical/Control split as
    ///   [`classify`] (so lifecycle/E2EE/CONGESTION packets are never shed
    ///   at the fan-out hop either).
    ///
    /// Only `MEDIA` packets carrying a concrete VIDEO / AUDIO / SCREEN
    /// `media_kind` map to the droppable [`OutboundPriority::Video`] /
    /// [`OutboundPriority::Audio`] / [`OutboundPriority::Screen`] buckets.
    ///
    /// NOTE: this maps the OUTER `MediaKind` (VIDEO/AUDIO/SCREEN) onto the
    /// same `OutboundPriority` variants `classify` derives from the INNER
    /// `MediaType` — the two enums share VIDEO/AUDIO/SCREEN semantics, so a
    /// well-formed publisher's outer `media_kind` agrees with its inner
    /// `media_type`. The priority is only ever used to decide *which kind to
    /// sacrifice first*; a (malicious) mismatch can only mis-bucket the
    /// FORGER's own packet, never another peer's (same trust boundary as the
    /// #989 layer filter).
    pub fn classify_outer(parsed: bool, packet_type: PacketType, media_kind: MediaKind) -> Self {
        if !parsed {
            return OutboundPriority::Control;
        }
        match packet_type {
            // Media is the bulk of traffic. Refine on the OUTER media_kind
            // (cleartext, E2EE-safe) — never the inner MediaType.
            PacketType::MEDIA => match media_kind {
                MediaKind::AUDIO => OutboundPriority::Audio,
                MediaKind::VIDEO => OutboundPriority::Video,
                MediaKind::SCREEN => OutboundPriority::Screen,
                // UNSPECIFIED (0) or any future/unknown kind → fail-open
                // Control. A MEDIA packet without a usable discriminator is
                // never preemptively sacrificed (matches the #988 viewport
                // filter, which fails OPEN on UNSPECIFIED media_kind).
                MediaKind::MEDIA_KIND_UNSPECIFIED => OutboundPriority::Control,
            },
            // Non-media wrappers: identical Critical/Control split to
            // `classify` via the shared helper.
            other => Self::classify_non_media(other),
        }
    }

    /// Critical/Control classification for every NON-`MEDIA` [`PacketType`].
    ///
    /// Shared by [`classify`] and [`classify_outer`] so the Critical set is
    /// defined in exactly ONE place: a future packet type promoted to (or
    /// demoted from) Critical changes both the outbound-channel policy and
    /// the inbound fan-out policy together, with no risk of drift.
    fn classify_non_media(packet_type: PacketType) -> Self {
        match packet_type {
            // SESSION_ASSIGNED is sent at most once per session at
            // start-up, but losing it would deny the client its session
            // identifier and break reconnection logic — keep it
            // protected even though it normally never collides with
            // outbound saturation.
            PacketType::SESSION_ASSIGNED => OutboundPriority::Critical,
            // CONGESTION remains a Critical control packet for
            // compatibility with client-originated or externally injected
            // packets. The relay no longer emits sender-keyed CONGESTION
            // from receiver-downlink overflow, but if such a packet exists
            // it must not be shed by the priority policy.
            PacketType::CONGESTION => OutboundPriority::Critical,
            // DOWNLINK_CONGESTION is the relay-authored receiver-directed
            // congestion signal (#1219 Half 2). It carries the critical
            // "your downlink is saturated — step down" instruction; losing
            // it delays the client's layer-chooser step-down response.
            PacketType::DOWNLINK_CONGESTION => OutboundPriority::Critical,
            // RSA_PUB_KEY and AES_KEY are both halves of the E2EE
            // handshake — RSA_PUB_KEY initiates the asymmetric step and
            // AES_KEY delivers the symmetric session key encrypted under
            // it. Dropping either one silently breaks encrypted
            // communication for the affected peer pair, with no
            // page-able label surfaced to operators. They are small and
            // infrequent, so the cost of always admitting them is
            // negligible, and the cost of losing one is catastrophic.
            PacketType::RSA_PUB_KEY => OutboundPriority::Critical,
            PacketType::AES_KEY => OutboundPriority::Critical,
            // MEETING packets carry server-authoritative events
            // (MEETING_STARTED, MEETING_ENDED, PARTICIPANT_LEFT,
            // HOST_MUTE_PARTICIPANT, …). Losing them desyncs the
            // host/participant UI from the server's authoritative state.
            PacketType::MEETING => OutboundPriority::Critical,
            // Remaining wrappers: CONNECTION, DIAGNOSTICS, HEALTH,
            // PEER_EVENT, KEYFRAME_REQUEST, PACKET_TYPE_UNKNOWN, … Treated
            // as Control: never preemptively dropped, only fail on actual
            // overflow. This matches the prior uniform behaviour for these
            // types.
            _ => OutboundPriority::Control,
        }
    }

    /// Stable metric label for the drop-reason counter.
    /// Returns `None` for priorities that cannot trigger a priority
    /// drop (they only fail on overflow, which is recorded separately).
    pub fn priority_drop_label(self) -> Option<&'static str> {
        match self {
            OutboundPriority::Audio => Some("priority_drop_audio"),
            OutboundPriority::Video | OutboundPriority::Screen => Some("priority_drop_video"),
            OutboundPriority::Critical | OutboundPriority::Control => None,
        }
    }
}

/// Outcome of the priority-drop evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityDropDecision {
    /// Admit the packet — call `try_send` as normal.
    Admit,
    /// Drop the packet before enqueuing. The caller is responsible for
    /// incrementing the drop metrics with the embedded reason label and
    /// for invoking `on_outbound_drop` so the drop is recorded for metrics
    /// and the #979 keyframe-relax path.
    Drop {
        /// Stable metric label suitable for the
        /// `videocall_outbound_channel_drops_total{drop_reason=…}`
        /// counter (e.g. `"priority_drop_video"`).
        reason: &'static str,
    },
}

/// Decide whether to admit or preemptively drop an outbound packet
/// based on its priority and the current channel fill ratio.
///
/// `free_capacity` is the value returned by `tokio::sync::mpsc::Sender::capacity`
/// (number of unused slots). `total_capacity` is the constant used to
/// construct the channel. Both are passed in explicitly so the policy
/// can be unit-tested without instantiating a real `mpsc::Sender`.
///
/// Special cases:
/// * `total_capacity == 0` is treated as "always admit" (the channel
///   constructor would panic for zero capacity; this defensive branch
///   prevents a div-by-zero if a future caller forgets).
/// * `free_capacity > total_capacity` is treated as "fully empty" —
///   should never happen but cannot panic.
pub fn evaluate(
    priority: OutboundPriority,
    free_capacity: usize,
    total_capacity: usize,
) -> PriorityDropDecision {
    // Critical and Control never preempt — they admit and let
    // `try_send` fail naturally on actual overflow.
    if matches!(
        priority,
        OutboundPriority::Critical | OutboundPriority::Control
    ) {
        return PriorityDropDecision::Admit;
    }

    if total_capacity == 0 {
        return PriorityDropDecision::Admit;
    }
    let used = total_capacity.saturating_sub(free_capacity);
    // f32 is sufficient — we are comparing against constants with two
    // significant digits of precision (0.80, 0.95).
    let fill_ratio = used as f32 / total_capacity as f32;

    match priority {
        OutboundPriority::Audio => {
            if fill_ratio >= PRIORITY_DROP_AUDIO_FILL_RATIO {
                PriorityDropDecision::Drop {
                    reason: "priority_drop_audio",
                }
            } else {
                PriorityDropDecision::Admit
            }
        }
        OutboundPriority::Video | OutboundPriority::Screen => {
            if fill_ratio >= PRIORITY_DROP_VIDEO_FILL_RATIO {
                PriorityDropDecision::Drop {
                    reason: "priority_drop_video",
                }
            } else {
                PriorityDropDecision::Admit
            }
        }
        OutboundPriority::Critical | OutboundPriority::Control => {
            // Already handled above; pattern is unreachable but
            // exhaustive matching guards against future variants.
            PriorityDropDecision::Admit
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- classify() coverage --------------------------------------------

    #[test]
    fn classify_parse_fail_is_control() {
        // Parse failure must never be classified as droppable media.
        assert_eq!(
            OutboundPriority::classify(false, PacketType::MEDIA, Some(MediaType::VIDEO)),
            OutboundPriority::Control,
        );
        assert_eq!(
            OutboundPriority::classify(false, PacketType::CONGESTION, None),
            OutboundPriority::Control,
        );
    }

    #[test]
    fn classify_session_assigned_is_critical() {
        assert_eq!(
            OutboundPriority::classify(true, PacketType::SESSION_ASSIGNED, None),
            OutboundPriority::Critical,
        );
    }

    #[test]
    fn classify_congestion_is_critical() {
        assert_eq!(
            OutboundPriority::classify(true, PacketType::CONGESTION, None),
            OutboundPriority::Critical,
        );
    }

    #[test]
    fn classify_rsa_pub_key_is_critical() {
        assert_eq!(
            OutboundPriority::classify(true, PacketType::RSA_PUB_KEY, None),
            OutboundPriority::Critical,
        );
    }

    #[test]
    fn classify_meeting_is_critical() {
        assert_eq!(
            OutboundPriority::classify(true, PacketType::MEETING, None),
            OutboundPriority::Critical,
        );
    }

    #[test]
    fn classify_audio_media_is_audio() {
        assert_eq!(
            OutboundPriority::classify(true, PacketType::MEDIA, Some(MediaType::AUDIO)),
            OutboundPriority::Audio,
        );
    }

    #[test]
    fn classify_video_media_is_video() {
        assert_eq!(
            OutboundPriority::classify(true, PacketType::MEDIA, Some(MediaType::VIDEO)),
            OutboundPriority::Video,
        );
    }

    #[test]
    fn classify_screen_media_is_screen() {
        assert_eq!(
            OutboundPriority::classify(true, PacketType::MEDIA, Some(MediaType::SCREEN)),
            OutboundPriority::Screen,
        );
    }

    #[test]
    fn classify_media_without_inner_type_is_control() {
        // Encrypted / unparseable inner — must NOT be preemptively
        // dropped. We treat it as Control (legacy behaviour).
        assert_eq!(
            OutboundPriority::classify(true, PacketType::MEDIA, None),
            OutboundPriority::Control,
        );
    }

    #[test]
    fn classify_media_heartbeat_is_control() {
        // HEARTBEAT and KEYFRAME_REQUEST are MEDIA-wrapped control
        // signals — keep them in the never-preempt bucket.
        assert_eq!(
            OutboundPriority::classify(true, PacketType::MEDIA, Some(MediaType::HEARTBEAT)),
            OutboundPriority::Control,
        );
        assert_eq!(
            OutboundPriority::classify(true, PacketType::MEDIA, Some(MediaType::KEYFRAME_REQUEST)),
            OutboundPriority::Control,
        );
    }

    #[test]
    fn classify_aes_key_is_critical() {
        // AES_KEY is the symmetric half of the E2EE handshake — it
        // delivers the session key encrypted under RSA_PUB_KEY. Losing
        // it silently breaks encrypted communication between the
        // affected peer pair with no `overflow_critical` label, so it
        // is classified Critical alongside RSA_PUB_KEY.
        assert_eq!(
            OutboundPriority::classify(true, PacketType::AES_KEY, None),
            OutboundPriority::Critical,
        );
    }

    #[test]
    fn classify_diagnostics_is_control() {
        assert_eq!(
            OutboundPriority::classify(true, PacketType::DIAGNOSTICS, None),
            OutboundPriority::Control,
        );
    }

    // ----- classify_outer() coverage (inbound fan-out hop, #1145) ---------
    //
    // `classify_outer` keys off the OUTER cleartext `media_kind`, never the
    // (E2EE-sealed) inner MediaType. These tests pin the shed taxonomy used
    // at the relay's NATS fan-out hop. Each asserts a CONCRETE expected
    // priority (not `X == X`), so flipping any single classification in the
    // source (e.g. VIDEO → Control "never shed") makes the matching test
    // fail loudly.

    #[test]
    fn classify_outer_video_kind_is_video_droppable() {
        // VIDEO media is the first thing shed under fan-out pressure.
        assert_eq!(
            OutboundPriority::classify_outer(true, PacketType::MEDIA, MediaKind::VIDEO),
            OutboundPriority::Video,
        );
        // ...and it carries a priority-drop label (is actually droppable).
        assert_eq!(
            OutboundPriority::classify_outer(true, PacketType::MEDIA, MediaKind::VIDEO)
                .priority_drop_label(),
            Some("priority_drop_video"),
        );
    }

    #[test]
    fn classify_outer_screen_kind_is_screen_droppable() {
        // SCREEN shares the droppable video band (same label).
        assert_eq!(
            OutboundPriority::classify_outer(true, PacketType::MEDIA, MediaKind::SCREEN),
            OutboundPriority::Screen,
        );
        assert_eq!(
            OutboundPriority::classify_outer(true, PacketType::MEDIA, MediaKind::SCREEN)
                .priority_drop_label(),
            Some("priority_drop_video"),
        );
    }

    #[test]
    fn classify_outer_audio_kind_is_audio_protected_band() {
        // AUDIO classifies to the Audio band — droppable only as a last
        // resort (95% on the outbound channel); at the inbound hop it is
        // still attributed distinctly from video so audio loss is alertable.
        assert_eq!(
            OutboundPriority::classify_outer(true, PacketType::MEDIA, MediaKind::AUDIO),
            OutboundPriority::Audio,
        );
        assert_eq!(
            OutboundPriority::classify_outer(true, PacketType::MEDIA, MediaKind::AUDIO)
                .priority_drop_label(),
            Some("priority_drop_audio"),
        );
    }

    #[test]
    fn classify_outer_unspecified_kind_fails_open_to_control() {
        // UNSPECIFIED media_kind (older clients / no discriminator) must
        // NEVER be preemptively shed — fail OPEN to Control, matching the
        // #988 viewport filter convention. A Control priority has no
        // priority-drop label (it is never preemptively dropped).
        assert_eq!(
            OutboundPriority::classify_outer(
                true,
                PacketType::MEDIA,
                MediaKind::MEDIA_KIND_UNSPECIFIED
            ),
            OutboundPriority::Control,
        );
        assert_eq!(
            OutboundPriority::classify_outer(
                true,
                PacketType::MEDIA,
                MediaKind::MEDIA_KIND_UNSPECIFIED
            )
            .priority_drop_label(),
            None,
        );
    }

    #[test]
    fn classify_outer_parse_fail_is_control() {
        // Unparseable outer wrapper (`parsed=false`) → Control: never shed
        // something we could not classify. The `media_kind` arg is ignored
        // when `parsed=false`, but we pass VIDEO to prove the parse-fail
        // guard wins over the media_kind (it would otherwise be droppable).
        assert_eq!(
            OutboundPriority::classify_outer(false, PacketType::MEDIA, MediaKind::VIDEO),
            OutboundPriority::Control,
        );
    }

    #[test]
    fn classify_outer_keyframe_request_is_never_shed() {
        // KEYFRAME_REQUEST is an INNER MediaType wrapped in a MEDIA packet
        // (there is no `PacketType::KEYFRAME_REQUEST`). On the wire it is a
        // MEDIA wrapper whose OUTER `media_kind` is UNSPECIFIED (keyframe
        // requests carry no media_kind discriminator). Since classify_outer
        // never parses the inner type, it sees `MEDIA + UNSPECIFIED` and must
        // fail OPEN to Control — a dropped keyframe request leaves a receiver
        // frozen, so it must never be preemptively shed. This is the
        // E2EE-safe behaviour: even when the inner MediaType is sealed, the
        // outer UNSPECIFIED kind protects it.
        assert_eq!(
            OutboundPriority::classify_outer(
                true,
                PacketType::MEDIA,
                MediaKind::MEDIA_KIND_UNSPECIFIED
            ),
            OutboundPriority::Control,
        );
    }

    #[test]
    fn classify_outer_lifecycle_and_e2ee_packets_are_critical() {
        // The Critical set must be IDENTICAL between classify_outer (inbound
        // hop) and classify (outbound channel). Dropping any of these at the
        // fan-out hop would break reconnection (SESSION_ASSIGNED), congestion
        // control (CONGESTION), E2EE (RSA_PUB_KEY / AES_KEY), or host/UI state
        // (MEETING). Pin each to Critical; a demotion to Control breaks this.
        for pt in [
            PacketType::SESSION_ASSIGNED,
            PacketType::CONGESTION,
            PacketType::RSA_PUB_KEY,
            PacketType::AES_KEY,
            PacketType::MEETING,
        ] {
            assert_eq!(
                OutboundPriority::classify_outer(true, pt, MediaKind::MEDIA_KIND_UNSPECIFIED),
                OutboundPriority::Critical,
                "{pt:?} must be Critical at the inbound fan-out hop",
            );
        }
    }

    #[test]
    fn classify_outer_critical_set_matches_classify() {
        // Lockstep: classify_outer and classify share `classify_non_media`,
        // so for every NON-media packet type they MUST agree. This guards
        // the refactor — if a future edit forks the two Critical sets, this
        // fails. We sweep the lifecycle/handshake/control types explicitly
        // (comparing the two FUNCTIONS against each other, not a literal).
        for pt in [
            PacketType::SESSION_ASSIGNED,
            PacketType::CONGESTION,
            PacketType::RSA_PUB_KEY,
            PacketType::AES_KEY,
            PacketType::MEETING,
            PacketType::DIAGNOSTICS,
            PacketType::HEALTH,
            PacketType::CONNECTION,
            PacketType::PEER_EVENT,
            PacketType::VIEWPORT,
            PacketType::LAYER_PREFERENCE,
            PacketType::LAYER_HINT,
            PacketType::PACKET_TYPE_UNKNOWN,
        ] {
            assert_eq!(
                OutboundPriority::classify_outer(true, pt, MediaKind::MEDIA_KIND_UNSPECIFIED),
                OutboundPriority::classify(true, pt, None),
                "classify_outer and classify disagree on non-media {pt:?}",
            );
        }
    }

    // ----- evaluate(): video / screen drop at 80% -------------------------

    #[test]
    fn video_admit_below_80_percent_full() {
        // 79% used (211/256) — still admit.
        let total = 256usize;
        let used = (total as f32 * 0.79) as usize;
        let free = total - used;
        assert_eq!(
            evaluate(OutboundPriority::Video, free, total),
            PriorityDropDecision::Admit,
        );
    }

    #[test]
    fn video_dropped_at_exactly_80_percent_full() {
        let total = 100usize; // round number so 80% is exact
        let used = 80;
        let free = total - used;
        match evaluate(OutboundPriority::Video, free, total) {
            PriorityDropDecision::Drop { reason } => {
                assert_eq!(reason, "priority_drop_video");
            }
            other => panic!("expected Drop at 80%, got {other:?}"),
        }
    }

    #[test]
    fn screen_dropped_at_exactly_80_percent_full() {
        // SCREEN must share the same threshold as VIDEO.
        let total = 100usize;
        let used = 80;
        let free = total - used;
        match evaluate(OutboundPriority::Screen, free, total) {
            PriorityDropDecision::Drop { reason } => {
                assert_eq!(reason, "priority_drop_video");
            }
            other => panic!("expected Drop at 80% for SCREEN, got {other:?}"),
        }
    }

    #[test]
    fn video_dropped_above_80_percent_full() {
        let total = 100usize;
        let used = 90;
        let free = total - used;
        match evaluate(OutboundPriority::Video, free, total) {
            PriorityDropDecision::Drop { reason } => {
                assert_eq!(reason, "priority_drop_video");
            }
            other => panic!("expected Drop above 80%, got {other:?}"),
        }
    }

    // ----- evaluate(): audio preserved until 95% --------------------------

    #[test]
    fn audio_admit_at_80_percent_full_when_video_drops() {
        // The whole point of the policy: audio survives a fill at
        // which video is already being dropped.
        let total = 100usize;
        let used = 85;
        let free = total - used;
        assert_eq!(
            evaluate(OutboundPriority::Audio, free, total),
            PriorityDropDecision::Admit,
        );
        // And video would be dropped at the same fill.
        assert!(matches!(
            evaluate(OutboundPriority::Video, free, total),
            PriorityDropDecision::Drop { .. }
        ));
    }

    #[test]
    fn audio_admit_just_below_95_percent_full() {
        let total = 100usize;
        let used = 94;
        let free = total - used;
        assert_eq!(
            evaluate(OutboundPriority::Audio, free, total),
            PriorityDropDecision::Admit,
        );
    }

    #[test]
    fn audio_dropped_at_exactly_95_percent_full() {
        let total = 100usize;
        let used = 95;
        let free = total - used;
        match evaluate(OutboundPriority::Audio, free, total) {
            PriorityDropDecision::Drop { reason } => {
                assert_eq!(reason, "priority_drop_audio");
            }
            other => panic!("expected Drop at 95% for AUDIO, got {other:?}"),
        }
    }

    #[test]
    fn audio_dropped_above_95_percent_full() {
        let total = 100usize;
        let used = 99;
        let free = total - used;
        match evaluate(OutboundPriority::Audio, free, total) {
            PriorityDropDecision::Drop { reason } => {
                assert_eq!(reason, "priority_drop_audio");
            }
            other => panic!("expected Drop above 95% for AUDIO, got {other:?}"),
        }
    }

    // ----- evaluate(): control / critical never preempt -------------------

    #[test]
    fn control_admit_even_at_99_percent_full() {
        // Control / Critical never preempt — only `try_send` can fail
        // on real overflow.
        let total = 100usize;
        let used = 99;
        let free = total - used;
        assert_eq!(
            evaluate(OutboundPriority::Control, free, total),
            PriorityDropDecision::Admit,
        );
        assert_eq!(
            evaluate(OutboundPriority::Critical, free, total),
            PriorityDropDecision::Admit,
        );
    }

    #[test]
    fn control_admit_when_channel_is_completely_full() {
        // At zero free capacity we still admit — the try_send will
        // observe the full state and the legacy overflow metric will
        // fire (with the new `overflow_critical` label for Critical).
        let total = 100usize;
        let free = 0;
        assert_eq!(
            evaluate(OutboundPriority::Control, free, total),
            PriorityDropDecision::Admit,
        );
        assert_eq!(
            evaluate(OutboundPriority::Critical, free, total),
            PriorityDropDecision::Admit,
        );
    }

    // ----- evaluate(): defensive corners ----------------------------------

    #[test]
    fn evaluate_handles_zero_total_capacity_without_panic() {
        // Channel with zero capacity is invalid (mpsc::channel panics),
        // but we must not div-by-zero if a future caller forgets.
        // Defensive branch admits everything in that degenerate case.
        assert_eq!(
            evaluate(OutboundPriority::Video, 0, 0),
            PriorityDropDecision::Admit,
        );
        assert_eq!(
            evaluate(OutboundPriority::Audio, 0, 0),
            PriorityDropDecision::Admit,
        );
    }

    #[test]
    fn evaluate_handles_free_greater_than_total_without_panic() {
        // Defensive: should never happen, but `used = total - free`
        // saturates to zero so fill_ratio is 0 — admit.
        assert_eq!(
            evaluate(OutboundPriority::Video, 1024, 256),
            PriorityDropDecision::Admit,
        );
    }

    // ----- label coverage -------------------------------------------------

    #[test]
    fn priority_drop_label_buckets() {
        assert_eq!(
            OutboundPriority::Audio.priority_drop_label(),
            Some("priority_drop_audio"),
        );
        assert_eq!(
            OutboundPriority::Video.priority_drop_label(),
            Some("priority_drop_video"),
        );
        assert_eq!(
            OutboundPriority::Screen.priority_drop_label(),
            Some("priority_drop_video"),
        );
        assert_eq!(OutboundPriority::Control.priority_drop_label(), None);
        assert_eq!(OutboundPriority::Critical.priority_drop_label(), None);
    }

    // ----- realistic capacity sanity checks -------------------------------
    //
    // The WT default is 512 (issue #979) and WS is 128. Verify the
    // thresholds map sensibly to slot counts on both. (The constants in
    // crate::constants are not imported here to keep this module
    // self-contained, but the slot maths match those values.)

    #[test]
    fn wt_512_thresholds_make_sense() {
        // 512 slots (issue #979 fail-fast cap): video starts dropping at
        // ~410 used (512 * 0.80), audio at ~486 (512 * 0.95).
        let total = 512usize;

        // 408 used → fill 79.7%, video admit.
        let free_at_408 = total - 408;
        assert_eq!(
            evaluate(OutboundPriority::Video, free_at_408, total),
            PriorityDropDecision::Admit,
        );

        // 410 used → fill 80.1%, video drop.
        let free_at_410 = total - 410;
        assert!(matches!(
            evaluate(OutboundPriority::Video, free_at_410, total),
            PriorityDropDecision::Drop { .. }
        ));

        // 410 used → audio still admit (protected until ~486).
        assert_eq!(
            evaluate(OutboundPriority::Audio, free_at_410, total),
            PriorityDropDecision::Admit,
        );

        // 485 used → fill 94.7%, audio still admit.
        let free_at_485 = total - 485;
        assert_eq!(
            evaluate(OutboundPriority::Audio, free_at_485, total),
            PriorityDropDecision::Admit,
        );

        // 488 used → fill 95.3%, audio drop.
        let free_at_488 = total - 488;
        assert!(matches!(
            evaluate(OutboundPriority::Audio, free_at_488, total),
            PriorityDropDecision::Drop { .. }
        ));
    }

    #[test]
    fn ws_128_thresholds_make_sense() {
        // 128 slots: video starts dropping at ~102 used, audio at ~122.
        let total = 128usize;

        // 100 used → fill 78%, video admit.
        let free_at_100 = total - 100;
        assert_eq!(
            evaluate(OutboundPriority::Video, free_at_100, total),
            PriorityDropDecision::Admit,
        );

        // 105 used → fill 82%, video drop, audio admit.
        let free_at_105 = total - 105;
        assert!(matches!(
            evaluate(OutboundPriority::Video, free_at_105, total),
            PriorityDropDecision::Drop { .. }
        ));
        assert_eq!(
            evaluate(OutboundPriority::Audio, free_at_105, total),
            PriorityDropDecision::Admit,
        );

        // 125 used → fill 97.6%, audio drop.
        let free_at_125 = total - 125;
        assert!(matches!(
            evaluate(OutboundPriority::Audio, free_at_125, total),
            PriorityDropDecision::Drop { .. }
        ));
    }

    // ----- Spec acceptance tests from discussion #699 -----------------
    //
    // These four tests lock in the exact behaviour requested in the
    // meeting analysis. If the policy is ever softened or thresholds
    // adjusted accidentally, the relevant test will fail with a clear
    // message tying it back to the spec.

    #[test]
    fn spec_video_dropped_first_at_80_percent() {
        // "VIDEO / SCREEN frames first — start dropping when channel
        //  is ≥80% full."
        // Uses the real 512-slot default (issue #979); video drops at
        // ~410 used.
        let total = 512usize;
        let used = (total as f32 * 0.81) as usize;
        let free = total - used;

        // Video and screen drop at 81% fill...
        assert!(
            matches!(
                evaluate(OutboundPriority::Video, free, total),
                PriorityDropDecision::Drop {
                    reason: "priority_drop_video"
                }
            ),
            "spec: VIDEO must drop at >=80% fill"
        );
        assert!(
            matches!(
                evaluate(OutboundPriority::Screen, free, total),
                PriorityDropDecision::Drop {
                    reason: "priority_drop_video"
                }
            ),
            "spec: SCREEN must drop at >=80% fill (same threshold as VIDEO)"
        );
        // ...while audio still passes through.
        assert_eq!(
            evaluate(OutboundPriority::Audio, free, total),
            PriorityDropDecision::Admit,
            "spec: AUDIO must be preserved at 81% fill"
        );
    }

    #[test]
    fn spec_audio_preserved_until_95_percent() {
        // "AUDIO frames — only when channel is ≥95% full (critical)"
        //
        // Two probes:
        //  - At the last sub-threshold slot, audio is admitted (proves the
        //    policy gives audio the requested cushion).
        //  - At the first slot that reaches 95% fill, audio drops with the
        //    documented label.
        // Uses the real 512-slot default (issue #979). 95% of 512 = 486.4,
        // so 486 used is still admitted (486/512 = 0.949 < 0.95) and 487 is
        // the first slot that drops (487/512 = 0.951 >= 0.95). Probes are
        // explicit slot counts, NOT `(total * ratio) as usize`, whose
        // truncation lands just under the threshold at this capacity.
        let total = 512usize;

        let used_admit = 486usize; // last protected slot (0.949 fill)
        assert_eq!(
            evaluate(OutboundPriority::Audio, total - used_admit, total),
            PriorityDropDecision::Admit,
            "spec: AUDIO must be admitted at the last sub-95% slot (486/512)"
        );

        let used_drop = 487usize; // first slot at/over 95% (0.951 fill)
        assert!(
            matches!(
                evaluate(OutboundPriority::Audio, total - used_drop, total),
                PriorityDropDecision::Drop {
                    reason: "priority_drop_audio"
                }
            ),
            "spec: AUDIO must drop at >=95% fill (487/512) with label priority_drop_audio"
        );
    }

    #[test]
    fn spec_control_never_dropped_except_at_100_percent() {
        // "CONTROL packets never dropped unless channel is 100%; even
        //  then, preserve SESSION_*, CONGESTION, RSA_PUB_KEY, MEETING_*."
        //
        // The policy evaluator itself NEVER preempts Control or
        // Critical packets — they are always admitted by this layer.
        // The 100%-overflow behaviour is the transport-level try_send
        // outcome (covered by the per-transport tests). Here we lock
        // in the policy-side invariant.
        let total = 100usize;
        for fill_pct in [50, 80, 90, 95, 99] {
            let free = total - fill_pct;
            assert_eq!(
                evaluate(OutboundPriority::Control, free, total),
                PriorityDropDecision::Admit,
                "spec: Control packet at {fill_pct}% fill must be admitted",
            );
            assert_eq!(
                evaluate(OutboundPriority::Critical, free, total),
                PriorityDropDecision::Admit,
                "spec: Critical packet at {fill_pct}% fill must be admitted",
            );
        }
    }

    #[test]
    fn spec_critical_set_is_session_congestion_rsa_aes_meeting() {
        // "preserve SESSION_*, CONGESTION, RSA_PUB_KEY, AES_KEY,
        //  MEETING_*"
        //
        // Lock in the Critical set so a future change can't silently
        // demote one of them into the Control bucket. AES_KEY is the
        // symmetric half of the E2EE handshake and is paired with
        // RSA_PUB_KEY — dropping either silently breaks encryption for
        // the affected peer pair.
        assert_eq!(
            OutboundPriority::classify(true, PacketType::SESSION_ASSIGNED, None),
            OutboundPriority::Critical,
        );
        assert_eq!(
            OutboundPriority::classify(true, PacketType::CONGESTION, None),
            OutboundPriority::Critical,
        );
        assert_eq!(
            OutboundPriority::classify(true, PacketType::RSA_PUB_KEY, None),
            OutboundPriority::Critical,
        );
        assert_eq!(
            OutboundPriority::classify(true, PacketType::AES_KEY, None),
            OutboundPriority::Critical,
        );
        assert_eq!(
            OutboundPriority::classify(true, PacketType::MEETING, None),
            OutboundPriority::Critical,
        );
    }

    // ----- ordering invariant ----------------------------------------
    //
    // At any single fill level, if VIDEO is dropped then AUDIO is
    // either admitted or also dropped — *never* the other way around.
    // Locks in the policy's monotonic priority ordering.

    #[test]
    fn audio_never_dropped_while_video_admits_at_same_fill() {
        let total = 1024usize;
        // Sweep a range of fills around the interesting region.
        for fill_pct_x10 in 700u32..1000 {
            let used = total * (fill_pct_x10 as usize) / 1000;
            let free = total - used;
            let video = evaluate(OutboundPriority::Video, free, total);
            let audio = evaluate(OutboundPriority::Audio, free, total);
            // If audio is dropped, video must also be dropped.
            if matches!(audio, PriorityDropDecision::Drop { .. }) {
                assert!(
                    matches!(video, PriorityDropDecision::Drop { .. }),
                    "ordering violation at fill {}%: audio dropped while video admitted",
                    fill_pct_x10 as f32 / 10.0,
                );
            }
        }
    }

    // ----- Real-channel integration -----------------------------------
    //
    // These tests build the same `tokio::sync::mpsc::channel` that both
    // transport sessions use and verify that the policy interacts
    // correctly with `Sender::capacity()` on a real channel — locking
    // in the wiring assumption that `total - sender.capacity() = used`.

    use tokio::sync::mpsc;

    /// Helper: build a fresh `(Sender, Receiver)` pair with a known
    /// capacity. Returns the sender (drained into the test) and a
    /// receiver (held so the channel does not get closed).
    fn channel_at_fill(
        total: usize,
        used: usize,
    ) -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(total);
        for _ in 0..used {
            tx.try_send(vec![0; 1]).expect("pre-fill should succeed");
        }
        assert_eq!(tx.capacity(), total - used);
        (tx, rx)
    }

    #[test]
    fn realchannel_video_dropped_at_80_percent_audio_admitted() {
        // Construct a real channel, pre-fill 81/100 slots, and verify
        // the policy's view of fill (using the sender's capacity) drops
        // video while admitting audio. This is the most direct
        // integration test of the wiring at both transport sites.
        let total = 100;
        let used = 81;
        let (tx, _rx) = channel_at_fill(total, used);
        let free = tx.capacity();

        match evaluate(OutboundPriority::Video, free, total) {
            PriorityDropDecision::Drop { reason } => {
                assert_eq!(reason, "priority_drop_video");
            }
            other => panic!("expected Drop for VIDEO at 81% fill, got {other:?}"),
        }
        assert_eq!(
            evaluate(OutboundPriority::Audio, free, total),
            PriorityDropDecision::Admit,
            "AUDIO must survive while VIDEO is dropped",
        );
    }

    #[test]
    fn realchannel_audio_dropped_at_96_percent() {
        let total = 100;
        let used = 96;
        let (tx, _rx) = channel_at_fill(total, used);
        let free = tx.capacity();

        match evaluate(OutboundPriority::Audio, free, total) {
            PriorityDropDecision::Drop { reason } => {
                assert_eq!(reason, "priority_drop_audio");
            }
            other => panic!("expected Drop for AUDIO at 96% fill, got {other:?}"),
        }
    }

    #[test]
    fn realchannel_critical_admit_even_at_99_percent_fill() {
        // The Critical bucket guards lifecycle and E2EE-handshake
        // packets (SESSION_ASSIGNED, CONGESTION, RSA_PUB_KEY, AES_KEY,
        // MEETING). Even when the channel is 99% full, the policy must
        // admit. The real try_send may then succeed (1 slot left) or
        // fail (raced to fill) — the policy itself is not the gating
        // layer here.
        let total = 100;
        let used = 99;
        let (tx, _rx) = channel_at_fill(total, used);
        let free = tx.capacity();

        for packet_type in [
            PacketType::SESSION_ASSIGNED,
            PacketType::CONGESTION,
            PacketType::RSA_PUB_KEY,
            PacketType::AES_KEY,
            PacketType::MEETING,
        ] {
            let priority = OutboundPriority::classify(true, packet_type, None);
            assert_eq!(
                priority,
                OutboundPriority::Critical,
                "{packet_type:?} must be Critical",
            );
            assert_eq!(
                evaluate(priority, free, total),
                PriorityDropDecision::Admit,
                "Critical {packet_type:?} must be admitted at 99% fill",
            );
        }
    }

    #[test]
    fn aes_key_is_critical_and_never_dropped() {
        // AES_KEY symmetry with RSA_PUB_KEY: both halves of the E2EE
        // handshake must survive the priority policy at every fill
        // level. Dropping AES_KEY silently breaks encrypted
        // communication for the affected peer pair with no
        // `overflow_critical` label surfaced to operators — exactly the
        // failure mode the priority policy is supposed to prevent for
        // RSA_PUB_KEY. The two packet types must be treated
        // symmetrically.
        let priority = OutboundPriority::classify(true, PacketType::AES_KEY, None);
        assert_eq!(
            priority,
            OutboundPriority::Critical,
            "AES_KEY must classify as Critical, mirroring RSA_PUB_KEY",
        );

        let total = 100usize;
        for fill_pct in [0, 50, 80, 90, 95, 99] {
            let free = total - fill_pct;
            assert_eq!(
                evaluate(priority, free, total),
                PriorityDropDecision::Admit,
                "AES_KEY must be admitted at {fill_pct}% fill (no preemptive drop)",
            );
        }
    }

    #[test]
    fn realchannel_priority_drop_does_not_consume_slot() {
        // Critical contract: when the policy decides to drop, it must
        // NOT have called try_send yet — so the slot count is unchanged.
        // This is the whole reason we evaluate before enqueue.
        let total = 100;
        let used = 85;
        let (tx, _rx) = channel_at_fill(total, used);
        let before_free = tx.capacity();

        let decision = evaluate(OutboundPriority::Video, before_free, total);
        assert!(matches!(decision, PriorityDropDecision::Drop { .. }));

        // Capacity unchanged — the evaluator does not touch the channel.
        assert_eq!(
            tx.capacity(),
            before_free,
            "evaluate must be side-effect-free"
        );
    }
}
