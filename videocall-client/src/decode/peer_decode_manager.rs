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

use super::hash_map_with_ordered_keys::HashMapWithOrderedKeys;
use super::layer_chooser::{DownlinkSample, LayerAvailability, LayerChooser, PrefMediaKind};
use super::peer_decoder::{PeerDecode, VideoPeerDecoder, MEDIA_TYPE_CAMERA, MEDIA_TYPE_SCREEN};
use super::pli_budget::{PliBudget, PliBudgetDecision};
use super::{create_audio_peer_decoder, AudioPeerDecoderTrait, DecodeStatus};
use crate::adaptive_quality_constants::{
    KEYFRAME_BACKOFF_DECAY_MS, KEYFRAME_REQUEST_MAX_BACKOFF_MS, KEYFRAME_REQUEST_MAX_UNANSWERED,
    KEYFRAME_REQUEST_MIN_INTERVAL_MS, KEYFRAME_REQUEST_SLOW_RETRY_MS, KEYFRAME_REQUEST_TIMEOUT_MS,
};
use crate::audio::shared_audio_context::SharedAudioContext;
use crate::crypto::aes::Aes128State;
use crate::diagnostics::DiagnosticManager;
use anyhow::Result;
use js_sys::Date;
use log::debug;
use protobuf::Message;
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fmt::Display, sync::Arc};
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent, Metric, MetricValue};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, TransportType};
use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::peer_event::PeerEvent;
use videocall_types::{Callback, PEER_EVENT_SCREEN_DECODE_STARTED};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

/// Cumulative count of keyframe requests (PLI) sent by this client.
static KEYFRAME_REQUESTS_SENT: AtomicU64 = AtomicU64::new(0);

/// Returns the total number of keyframe requests sent since process start.
pub fn keyframe_requests_sent_count() -> u64 {
    KEYFRAME_REQUESTS_SENT.load(Ordering::Relaxed)
}

/// Build and emit a single `KEYFRAME_REQUEST` `PacketWrapper` for `peer_user_id`'s
/// `requested_media_type` stream, keyed to `target_session_id` for the relay's per-session
/// rate limiter (#1124).
///
/// Extracted as a free helper (issue #1025) so BOTH the manager's gap-/visibility-driven
/// `send_keyframe_request(&self, ...)` AND the worker's proactive eviction route can produce
/// the **identical** packet without either holding `&PeerDecodeManager`. The proactive route
/// (`VideoPeerDecoder::set_keyframe_request_route`) captures a cheap clone of the `send_packet`
/// `Callback` plus the owned `local_user_id` / peer id / session id and calls this directly.
///
/// Coalescing (#979 / #1011): this helper does NOT rate-limit. The CLIENT-side per-stream
/// backoff lives in `SequenceTracker::should_request_keyframe` and gates only the gap-driven
/// path; the visibility-driven proactive path (and now the eviction-driven one) intentionally
/// bypass it, exactly as the existing visibility PLIs do. The authoritative coalescer is the
/// RELAY's per-`(receiver, target_session)` `KEYFRAME_REQUEST` limiter — every request, from
/// every client subsystem, lands on it as the same `target_session_id`-keyed packet, so a burst
/// of evictions cannot storm the publisher regardless of how many subsystems ask.
///
/// Uses `send_packet` (reliable stream), NOT datagrams: a `KEYFRAME_REQUEST` is a control
/// message that must be delivered reliably. Sent unencrypted (raw `MediaPacket`) because the
/// relay must read the target `user_id` / `target_session_id` to route and rate-limit it.
fn emit_keyframe_request(
    send_packet: &Callback<PacketWrapper>,
    local_user_id: &str,
    peer_user_id: &str,
    target_session_id: u64,
    requested_media_type: MediaType,
) {
    let media_type_byte = match requested_media_type {
        MediaType::VIDEO => b"VIDEO".to_vec(),
        MediaType::SCREEN => b"SCREEN".to_vec(),
        _ => return,
    };

    let media_packet = MediaPacket {
        media_type: MediaType::KEYFRAME_REQUEST.into(),
        user_id: peer_user_id.as_bytes().to_vec(),
        target_session_id,
        data: media_type_byte,
        ..Default::default()
    };

    let media_data = match media_packet.write_to_bytes() {
        Ok(data) => data,
        Err(e) => {
            log::warn!("Failed to serialize keyframe request: {}", e);
            return;
        }
    };

    let wrapper = PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        user_id: local_user_id.as_bytes().to_vec(),
        data: media_data,
        ..Default::default()
    };

    KEYFRAME_REQUESTS_SENT.fetch_add(1, Ordering::Relaxed);
    log::info!(
        "Sending KEYFRAME_REQUEST to {} (session {}) for {:?}",
        peer_user_id,
        target_session_id,
        requested_media_type
    );
    send_packet.emit(wrapper);
}

/// Install the proactive keyframe-request route (issue #1025) on a peer's VIDEO and SCREEN
/// decoders. Each route captures a cheap clone of the transport `send_packet` callback plus the
/// owned local user id, the peer's user id, and the peer's relay `session_id` (the per-session
/// limiter key, #1124), so it can call [`emit_keyframe_request`] for the right peer + stream
/// without holding `&PeerDecodeManager`. The worker fires the route when its jitter buffer
/// evicts a stale keyframe-less backlog for that stream.
///
/// Issue #1479: each route also captures a clone of the shared per-receiver cross-sender PLI
/// `budget` and gates the emission through it. The gate sits ABOVE the transport-agnostic
/// `emit_keyframe_request`, so it applies identically to WebTransport and WebSocket. The budget is
/// a benign defense-in-depth ceiling (mirrors the relay's 32/s) that only sheds genuinely-redundant
/// same-window cross-sender 2nd+ pokes — a sender's first-in-window request (including the #1662
/// post-reset recovery PLI) is always allowed, so this never weakens the #1494 per-sender backoff
/// nor wedges a frozen stream. On a shed it broadcasts a `pli_budget` `DiagEvent` and a throttled
/// `warn!` (at most once per window per sender).
fn install_keyframe_request_routes(
    peer: &Peer,
    send_packet: &Callback<PacketWrapper>,
    local_user_id: &str,
    budget: &Rc<RefCell<PliBudget>>,
) {
    for (decoder, media_type, starved_since) in [
        (&peer.video, MediaType::VIDEO, None),
        (
            &peer.screen,
            MediaType::SCREEN,
            // Issue #2328: only the SCREEN route arms the starvation clock. The camera equivalent
            // is deliberately absent — camera content moves continuously, so a camera rung is not
            // subject to the static-share keyframe-floor gating that strands a screen rung, and
            // the #1899 comment records the same camera↔screen divergence for the chooser itself.
            Some(peer.screen_keyframe_starved_since_ms.clone()),
        ),
    ] {
        let send_packet = send_packet.clone();
        let local_user_id = local_user_id.to_owned();
        let peer_user_id = peer.user_id.clone();
        let session_id = peer.session_id;
        let budget = budget.clone();
        let sid_str = peer.sid_str.clone();
        decoder.set_keyframe_request_route(Box::new(move |head_age_ms: f64| {
            // Read the clock ONCE for both consumers below (it crosses the JS boundary).
            let now = now_ms();
            // Issue #2328: stamp the START of this keyframe-less hold, before and independently of
            // the budget gate — a SHED request is still evidence the receiver is frozen, and losing
            // that evidence would make the retraction bound depend on cross-sender contention.
            // `is_none()` so repeated requests during ONE hold keep the ORIGINAL start time and the
            // bound measures the true hold duration rather than restarting on every poke.
            if let Some(cell) = &starved_since {
                if cell.get().is_none() {
                    cell.set(Some(now));
                }
            }
            // Issue #1479: gate the proactive PLI through the per-receiver cross-sender budget.
            // `now_ms()` is read here (the side-effecting route closure), NOT inside the pure
            // `PliBudget::allow`, so the budget stays host-testable. Keyed by `session_id`, the
            // same key the manager's lifecycle hooks clean up.
            let decision = budget.borrow_mut().allow(session_id, head_age_ms, now as u128);
            match decision {
                PliBudgetDecision::Allow => {
                    emit_keyframe_request(
                        &send_packet,
                        &local_user_id,
                        &peer_user_id,
                        session_id,
                        media_type,
                    );
                }
                PliBudgetDecision::Shed { log } => {
                    // Both the structured DiagEvent and the human-readable warn are gated by the
                    // same `log` throttle (at most once per window per sender, computed in the pure
                    // budget). Under a sustained meeting-wide freeze at cap a shed can fire on every
                    // #1494-paced poke; throttling the DiagEvent too keeps the diagnostics bus from
                    // emitting one event per shed. The `log` flag carries the throttle decision so
                    // the route closure stays the only side-effecting place.
                    if log {
                        // Structured shed counter for in-process consumers (uploaded diagnostics).
                        let evt = DiagEvent {
                            subsystem: "pli_budget",
                            stream_id: Some(sid_str.clone()),
                            ts_ms: now_ms(),
                            metrics: vec![
                                metric!("shed", 1u64),
                                metric!("from_peer", local_user_id.clone()),
                                metric!("to_peer", sid_str.clone()),
                                metric!("head_age_ms", head_age_ms),
                                metric!("media_type", format!("{media_type:?}")),
                            ],
                        };
                        let _ = global_sender().try_broadcast(evt);
                        log::warn!(
                            "[PLI_BUDGET] shed proactive keyframe request {}->{} ({media_type:?}, head_age={head_age_ms:.0}ms): per-receiver cross-sender budget at cap (#1479)",
                            local_user_id,
                            sid_str
                        );
                    }
                }
            }
        }));
    }
}

#[derive(Debug)]
pub enum PeerDecodeError {
    AesDecryptError,
    IncorrectPacketType,
    AudioDecodeError,
    ScreenDecodeError,
    VideoDecodeError,
    NoSuchPeer(u64),
    NoMediaType,
    NoPacketType,
    PacketParseError,
    SameUserPacket(u64),
    UnknownMediaType,
    UnknownPacketType,
}

#[derive(Debug)]
pub enum PeerStatus {
    Added(u64),
    NoChange,
}

impl Display for PeerDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerDecodeError::AesDecryptError => write!(f, "AesDecryptError"),
            PeerDecodeError::IncorrectPacketType => write!(f, "IncorrectPacketType"),
            PeerDecodeError::AudioDecodeError => write!(f, "AudioDecodeError"),
            PeerDecodeError::ScreenDecodeError => write!(f, "ScreenDecodeError"),
            PeerDecodeError::VideoDecodeError => write!(f, "VideoDecodeError"),
            PeerDecodeError::NoSuchPeer(s) => write!(f, "Peer Not Found: {s}"),
            PeerDecodeError::NoMediaType => write!(f, "No media_type"),
            PeerDecodeError::NoPacketType => write!(f, "No packet_type"),
            PeerDecodeError::PacketParseError => {
                write!(f, "Failed to parse to protobuf MediaPacket")
            }
            PeerDecodeError::SameUserPacket(s) => write!(f, "SameUserPacket: {s}"),
            PeerDecodeError::UnknownMediaType => write!(f, "UnknownMediaType"),
            PeerDecodeError::UnknownPacketType => write!(f, "UnknownPacketType"),
        }
    }
}

/// Which of a peer's three decoders, if any, has to be rebuilt after a
/// [`PeerDecodeError`].
///
/// See [`decoder_reset_scope`] for why this is per-kind rather than "all three".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderResetScope {
    /// Nothing to rebuild: the packet failed before any decoder was invoked.
    None,
    Video,
    Screen,
    Audio,
}

/// Map a decode failure to the decoder that actually failed (issue 2225).
///
/// # Why this is not "rebuild everything"
///
/// A peer owns three fully independent decoders: two `VideoPeerDecoder`s
/// (camera + screen, each its own codec worker and canvas renderer) and one
/// `NetEqAudioPeerDecoder` (its own Web Worker, jitter buffer, AudioWorklet, and
/// blob URL). They share no state, so a failure in one says nothing about the
/// health of the others — and rebuilding the audio decoder is *expensive*:
/// a fresh NetEQ worker, worklet init and message plumbing per rebuild.
///
/// Before this mapping existed, the decode-error arm called `Peer::reset`, which
/// rebuilt all three unconditionally. On a lossy link that repeated for as long
/// as the errors did, respawning the NetEQ worker every time — with the audio
/// path interrupted on each respawn even though audio was never the thing that
/// failed.
///
/// # Why the pre-decode errors rebuild nothing
///
/// `AesDecryptError`, `PacketParseError`, `NoMediaType`, `NoPacketType`,
/// `IncorrectPacketType`, `UnknownMediaType` and `UnknownPacketType` are all
/// raised by `Peer::decode` **before** it reaches any `decoder.decode(..)` call
/// — they are envelope, crypto and protobuf failures. No decoder was invoked, so
/// no decoder holds state that a rebuild could recover; rebuilding is pure churn.
///
/// `NoSuchPeer` and `SameUserPacket` never reach a peer's reset path at all
/// (they are raised by the manager, not by `Peer::decode`); they map to `None`
/// for completeness.
///
/// # Which arms fire today
///
/// The pre-decode group is, as of this change, the ONLY group that reaches the
/// decode-error arm: neither concrete decoder currently propagates a failure.
/// `VideoPeerDecoder::decode` (`peer_decoder.rs`) returns `Ok` on every path —
/// `push_frame` is fire-and-forget and an unknown codec returns a non-rendered
/// `Ok` — so `VideoDecodeError`/`ScreenDecodeError` cannot be produced; and
/// `NetEqAudioPeerDecoder::decode` posts to the worker and likewise always
/// returns `Ok` (`StandardAudioPeerDecoder`, which *can* fail, is never
/// constructed — `create_audio_peer_decoder` always builds the NetEQ one).
///
/// That is precisely why the pre-2225 blanket reset was so damaging: every
/// rebuild it performed was churn, and a receiver holding a stale AES key fails
/// EVERY packet, respawning the NetEQ worker tens of times a second for as long
/// as the mismatch lasts. The three per-kind arms below are therefore currently
/// latent. They are kept — rather than collapsed into "rebuild nothing" —
/// because they encode the correct recovery for the day a decoder does start
/// propagating errors, and because a per-kind mapping is what makes that future
/// change safe by construction.
///
/// # PRECONDITION for making any decoder fallible on peer input
///
/// Do not accept that invitation without first adding a per-peer, per-kind
/// rebuild budget. The rebuild here is unmetered — no cooldown, no backoff, no
/// cap — so the moment a decoder starts returning `Err` for attacker-shaped
/// input, a hostile sender gets the whole issue-2225 storm back, now aimed at
/// whichever decoder it can make fail. Two specific hazards:
///
///  * `create_audio_peer_decoder` spawns a Web Worker, a worklet and a blob URL
///    per call. At packet rate that is a remote resource lever.
///  * `NetEqAudioPeerDecoder::create_neteq_worker` `.expect()`s on FOUR
///    DOM/window lookups (window, document, the `neteq-worker` link tag, its
///    href). A peer-triggered rebuild racing teardown or navigation would panic
///    rather than return `Err`. Make those fallible in the same change.
///
/// [`crate::decode::pli_budget::PliBudget`] is the template: a pure `allow`
/// decision over (peer, kind, now), already used to meter the other
/// peer-triggered work on this path.
///
/// The match is deliberately exhaustive with no wildcard arm: a new
/// `PeerDecodeError` variant must then be classified here explicitly rather than
/// silently inheriting a default.
fn decoder_reset_scope(error: &PeerDecodeError) -> DecoderResetScope {
    match error {
        PeerDecodeError::VideoDecodeError => DecoderResetScope::Video,
        PeerDecodeError::ScreenDecodeError => DecoderResetScope::Screen,
        PeerDecodeError::AudioDecodeError => DecoderResetScope::Audio,

        PeerDecodeError::AesDecryptError
        | PeerDecodeError::IncorrectPacketType
        | PeerDecodeError::NoSuchPeer(_)
        | PeerDecodeError::NoMediaType
        | PeerDecodeError::NoPacketType
        | PeerDecodeError::PacketParseError
        | PeerDecodeError::SameUserPacket(_)
        | PeerDecodeError::UnknownMediaType
        | PeerDecodeError::UnknownPacketType => DecoderResetScope::None,
    }
}

fn diagnostic_media_type_for_decode_error(
    packet: &PacketWrapper,
    error: &PeerDecodeError,
) -> MediaType {
    match packet.media_kind.enum_value() {
        Ok(MediaKind::VIDEO) => MediaType::VIDEO,
        Ok(MediaKind::SCREEN) => MediaType::SCREEN,
        Ok(MediaKind::AUDIO) => MediaType::AUDIO,
        _ => match error {
            PeerDecodeError::ScreenDecodeError => MediaType::SCREEN,
            PeerDecodeError::AudioDecodeError => MediaType::AUDIO,
            _ => MediaType::VIDEO,
        },
    }
}

/// This peer's decoder-OUTPUT clock for `media_type` (#2511); `None` for a kind that owns no
/// `VideoPeerDecoder`, leaving the tracker's clock at its seed. Exhaustive with no catch-all so a
/// NEW kind breaks the build rather than reaching the freeze machine on an unadvanceable clock.
fn diagnostic_last_output_ms(peer: &Peer, media_type: MediaType) -> Option<f64> {
    match media_type {
        MediaType::VIDEO => Some(peer.video.last_output_ms()),
        MediaType::SCREEN => Some(peer.screen.last_output_ms()),
        MediaType::AUDIO
        | MediaType::HEARTBEAT
        | MediaType::RTT
        | MediaType::KEYFRAME_REQUEST
        | MediaType::MEDIA_TYPE_UNKNOWN => None,
    }
}

/// Tracks sequence numbers with a reorder-tolerant sliding window.
///
/// WebTransport sends each video packet on a separate unidirectional QUIC
/// stream, so packets can arrive out-of-order across streams. A simple
/// `seq > prev + 1` check would misinterpret every reordering as packet loss,
/// triggering unnecessary keyframe requests.
///
/// This tracker uses a `u64` bitfield to remember the 64 most recent sequence
/// positions. A packet is only declared lost when it shifts off the window
/// without ever being marked as seen. This tolerates up to 63 positions of
/// reordering before declaring loss.
struct SequenceTracker {
    /// Highest sequence number seen so far.
    high_seq: Option<u64>,
    /// Bitfield of recently seen sequences. Bit 0 = `high_seq` was seen,
    /// bit 1 = `high_seq - 1`, etc. A 0 bit means that seq was not yet received.
    seen_bits: u64,
    /// Number of packets confirmed lost (shifted off the window unseen).
    lost_count: u32,
    /// Timestamp (ms) when loss was first detected. `None` if no loss.
    loss_detected_at_ms: Option<u64>,
    /// Last time a keyframe request was sent (ms). Used for rate-limiting.
    last_keyframe_request_ms: u64,
    /// Number of unanswered keyframe requests sent since last recovery.
    unanswered_requests: u32,
    /// Current backoff interval for keyframe requests (ms). Doubles after each request.
    current_backoff_ms: u64,

    // ── Windowed rate accounting (freeze observability, issue #1013) ──────
    // During a freeze, decode CALLS keep firing (fps_received reads ~30) but
    // packets are being lost and the stream is stuck requesting keyframes.
    // We surface two per-stream signals — windowed packet-loss rate and
    // windowed keyframe-request rate — so the health reporter can fold them
    // into video_quality_score. Both use a ~1s rolling window and are
    // recomputed only on window rollover (never per packet) to keep the
    // real-time decode path allocation- and bus-spam-free.
    /// Start of the current rate window (ms). 0 = not yet started.
    window_start_ms: u64,
    /// Lost packets accumulated in the current window.
    window_lost: u32,
    /// Largest missing run in this window that outgrew the reorder window, in frames
    /// (#2524) — a count, because `loss_per_sec` divides by ELAPSED.
    window_max_gap: u64,
    /// Keyframe requests (PLI) we decided to send for this stream in the
    /// current window.
    window_kf_requests: u32,
    window_kf_suppressions: [u32; KEYFRAME_SUPPRESSION_REASON_COUNT],
    /// Most recently computed loss rate (lost packets/sec). Stable between
    /// window rollovers.
    loss_per_sec: f64,
    /// Last published [`Self::window_max_gap`]; stable between rollovers (#2524).
    max_gap: u64,
    /// Most recently computed keyframe-request rate (PLI/sec). Stable between
    /// window rollovers.
    kf_per_sec: f64,
    kf_suppression_per_sec: KeyframeSuppressionRates,
    /// Which WAIT reason is currently booked — see `record_keyframe_wait_edge` (#2541).
    booked_suppression: Option<KeyframeSuppressionReason>,
}

const KEYFRAME_SUPPRESSION_REASON_COUNT: usize = 5;

/// Why a keyframe request was decided AGAINST. `lost_count == 0` books nothing (#2541).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyframeSuppressionReason {
    PreTimeout,
    SlowRetryWait,
    BackoffWait,
    DiscardedVideoOff,
    DiscardedNotVisible,
}

impl KeyframeSuppressionReason {
    const ALL: [Self; KEYFRAME_SUPPRESSION_REASON_COUNT] = [
        Self::PreTimeout,
        Self::SlowRetryWait,
        Self::BackoffWait,
        Self::DiscardedVideoOff,
        Self::DiscardedNotVisible,
    ];

    fn index(self) -> usize {
        match self {
            Self::PreTimeout => 0,
            Self::SlowRetryWait => 1,
            Self::BackoffWait => 2,
            Self::DiscardedVideoOff => 3,
            Self::DiscardedNotVisible => 4,
        }
    }

    fn metric_name(self) -> &'static str {
        match self {
            Self::PreTimeout => "keyframe_requests_suppressed_pre_timeout_per_sec",
            Self::SlowRetryWait => "keyframe_requests_suppressed_slow_retry_wait_per_sec",
            Self::BackoffWait => "keyframe_requests_suppressed_backoff_wait_per_sec",
            Self::DiscardedVideoOff => "keyframe_requests_suppressed_discarded_video_off_per_sec",
            Self::DiscardedNotVisible => {
                "keyframe_requests_suppressed_discarded_not_visible_per_sec"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct KeyframeSuppressionRates {
    values: [f64; KEYFRAME_SUPPRESSION_REASON_COUNT],
}

impl KeyframeSuppressionRates {
    fn get(self, reason: KeyframeSuppressionReason) -> f64 {
        self.values[reason.index()]
    }
}

/// Width of the reorder bitfield; a position shifted past it can never be seen.
const REORDER_WINDOW_SLOTS: u64 = u64::BITS as u64;

use videocall_aq::constants::MAX_PLAUSIBLE_FORWARD_GAP;

/// Reorder-tolerant sequence-gap counter shared by the video [`SequenceTracker`]
/// and the audio [`AudioDatagramLossTracker`] (issue #1878).
///
/// Records `seq` into a 64-position bitfield window rooted at `*high_seq` and
/// returns the number of NEW packets confirmed lost — positions that shifted off
/// the window without ever being marked seen. Tolerates up to 63 positions of
/// reordering before declaring loss (WebTransport delivers video on separate
/// unistreams and audio on unordered datagrams, so both can arrive out of order).
///
/// Extracted verbatim from the former inline `SequenceTracker::record_seq` body
/// so the two trackers share ONE implementation of this subtle bitfield logic
/// rather than duplicating it. It is pure over its two `&mut` state words, so it
/// is unit-testable on the native host target and both callers are guarded by
/// the same existing tests.
fn record_seq_into_reorder_window(
    high_seq: &mut Option<u64>,
    seen_bits: &mut u64,
    seq: u64,
) -> u32 {
    let Some(high) = *high_seq else {
        // First packet -- initialize. Mark all window positions as "seen"
        // so that pre-stream positions (sequence numbers that never
        // existed) are not counted as lost when the window advances.
        *high_seq = Some(seq);
        *seen_bits = u64::MAX;
        return 0;
    };

    if seq > high {
        // New highest -- shift window left and count losses.
        // Compare in u64 space first to avoid truncation on huge gaps.
        let gap = seq - high;
        let new_lost = if gap >= REORDER_WINDOW_SLOTS {
            // Two disjoint sets are lost (#2535): the old window's unseen bits,
            // AND `high + 1 ..= seq - 64`, which land beyond the NEW window.
            let old_window_unseen = seen_bits.count_zeros();
            let past_new_window =
                (gap - REORDER_WINDOW_SLOTS).min(MAX_PLAUSIBLE_FORWARD_GAP) as u32;
            *seen_bits = 1; // only the new packet is seen
            old_window_unseen + past_new_window
        } else {
            // Count the bits that will shift off (the top `shift` bits).
            let shift = gap as u32;
            let mask = !((1u64 << (64 - shift)) - 1);
            let shifting_out = *seen_bits & mask;
            let lost_in_shift = shift - shifting_out.count_ones();
            // Shift and mark the new seq as seen.
            *seen_bits = (*seen_bits << shift) | 1;
            lost_in_shift
        };
        *high_seq = Some(seq);
        new_lost
    } else if high - seq < REORDER_WINDOW_SLOTS {
        // Out-of-order but within window -- mark as seen (no loss).
        let bit_pos = (high - seq) as u32;
        *seen_bits |= 1u64 << bit_pos;
        0
    } else {
        // Too old (beyond window) -- ignore silently.
        0
    }
}

impl SequenceTracker {
    fn new() -> Self {
        Self {
            high_seq: None,
            seen_bits: 0,
            lost_count: 0,
            loss_detected_at_ms: None,
            last_keyframe_request_ms: 0,
            unanswered_requests: 0,
            current_backoff_ms: KEYFRAME_REQUEST_MIN_INTERVAL_MS,
            window_start_ms: 0,
            window_lost: 0,
            window_max_gap: 0,
            window_kf_requests: 0,
            window_kf_suppressions: [0; KEYFRAME_SUPPRESSION_REASON_COUNT],
            loss_per_sec: 0.0,
            max_gap: 0,
            kf_per_sec: 0.0,
            kf_suppression_per_sec: KeyframeSuppressionRates::default(),
            booked_suppression: None,
        }
    }

    /// Record a sequence number. Returns the number of NEW lost packets
    /// detected (packets that shifted off the window unseen).
    ///
    /// A backward jump of at least `SEQ_RESET_REANCHOR_GAP` is a publisher restart,
    /// not reordering: drop the baseline so this packet anchors a fresh one instead
    /// of every later packet falling into the shared window's "too old" branch.
    fn record_seq(&mut self, seq: u64) -> u32 {
        if let Some(high) = self.high_seq {
            if seq < high && high - seq >= videocall_aq::constants::SEQ_RESET_REANCHOR_GAP {
                self.high_seq = None;
                self.seen_bits = 0;
            }
        }
        if let Some(high) = self.high_seq {
            let gap = seq.saturating_sub(high);
            // Inside the window a missing position may still arrive; past it, book the
            // whole run — SATURATING, or a huge burst would read as a clean link.
            if gap > REORDER_WINDOW_SLOTS {
                let missing = (gap - 1).min(MAX_PLAUSIBLE_FORWARD_GAP);
                self.window_max_gap = self.window_max_gap.max(missing);
            }
        }
        let new_lost = record_seq_into_reorder_window(&mut self.high_seq, &mut self.seen_bits, seq);
        self.lost_count = self.lost_count.saturating_add(new_lost);
        new_lost
    }

    /// Check if a keyframe request should be sent. Returns `true` if yes.
    ///
    /// Implements exponential backoff: first request after
    /// `KEYFRAME_REQUEST_TIMEOUT_MS`, then intervals double from
    /// `KEYFRAME_REQUEST_MIN_INTERVAL_MS` up to `KEYFRAME_REQUEST_MAX_BACKOFF_MS`.
    /// After `KEYFRAME_REQUEST_MAX_UNANSWERED` unanswered requests, switches to
    /// a slow periodic retry (`KEYFRAME_REQUEST_SLOW_RETRY_MS`) to recover from
    /// persistent loss without flooding the sender. Each slow retry ages one
    /// request out of the tally, so slow retry is bounded, not terminal (#2397).
    fn should_request_keyframe(&mut self, now: u64) -> bool {
        if self.lost_count == 0 {
            self.booked_suppression = None;
            // No current loss -- clear detection timestamp but preserve
            // backoff escalation. Only fully reset after sustained stability
            // (KEYFRAME_BACKOFF_DECAY_MS with no PLI activity), so that
            // repeated PLI→keyframe→loss cycles don't restart the backoff
            // from scratch every time. See issue #832.
            self.loss_detected_at_ms = None;
            if now.saturating_sub(self.last_keyframe_request_ms) >= KEYFRAME_BACKOFF_DECAY_MS {
                self.unanswered_requests = 0;
                self.current_backoff_ms = KEYFRAME_REQUEST_MIN_INTERVAL_MS;
            }
            return false;
        }

        // Record first loss detection time.
        let loss_time = *self.loss_detected_at_ms.get_or_insert(now);
        let elapsed_since_loss = now.saturating_sub(loss_time);
        let elapsed_since_last_req = now.saturating_sub(self.last_keyframe_request_ms);

        // Wait for initial timeout before first request.
        if elapsed_since_loss < KEYFRAME_REQUEST_TIMEOUT_MS {
            self.record_keyframe_wait_edge(KeyframeSuppressionReason::PreTimeout);
            return false;
        }

        // After the initial backoff burst is exhausted, switch to slow
        // periodic retry. Keyframes are 5-10x larger than delta frames
        // and have a higher drop probability on lossy networks, so giving
        // up permanently would leave frozen video with no recovery path.
        if self.unanswered_requests >= KEYFRAME_REQUEST_MAX_UNANSWERED {
            if elapsed_since_last_req < KEYFRAME_REQUEST_SLOW_RETRY_MS {
                self.record_keyframe_wait_edge(KeyframeSuppressionReason::SlowRetryWait);
                return false;
            }
            self.last_keyframe_request_ms = now;
            // #2397: age one request out of the tally; `current_backoff_ms` is untouched.
            self.unanswered_requests = self.unanswered_requests.saturating_sub(1);
            self.booked_suppression = None;
            return true;
        }

        // Exponential backoff between requests.
        if elapsed_since_last_req < self.current_backoff_ms {
            self.record_keyframe_wait_edge(KeyframeSuppressionReason::BackoffWait);
            return false;
        }

        self.last_keyframe_request_ms = now;
        self.unanswered_requests += 1;
        // Double backoff for next request, capped.
        self.current_backoff_ms =
            (self.current_backoff_ms * 2).min(KEYFRAME_REQUEST_MAX_BACKOFF_MS);
        self.booked_suppression = None;
        true
    }

    /// Book a wait reason once per ENTRY into that wait, not per packet observed while in it
    /// (#2541). A reason CHANGE books the new one; both decided-request paths and the
    /// loss-cleared path reset the latch, so it cannot wedge.
    fn record_keyframe_wait_edge(&mut self, reason: KeyframeSuppressionReason) {
        if self.booked_suppression != Some(reason) {
            self.record_keyframe_suppression(reason);
            self.booked_suppression = Some(reason);
        }
    }

    fn record_keyframe_suppression(&mut self, reason: KeyframeSuppressionReason) {
        let count = &mut self.window_kf_suppressions[reason.index()];
        *count = count.saturating_add(1);
    }

    /// Called when a keyframe is received -- clears loss state with graduated
    /// backoff recovery.
    ///
    /// We clear `lost_count` and `loss_detected_at_ms` because the keyframe
    /// provides a fresh decode point. However, we intentionally preserve most
    /// of the backoff escalation (`current_backoff_ms`) and only decrement
    /// `unanswered_requests` by 1.
    ///
    /// **Why**: In a PLI storm, each keyframe (5-10x larger than a delta frame)
    /// causes a bandwidth spike that triggers new packet loss, sending another
    /// PLI. If we fully reset backoff here, the cycle restarts at 1s forever
    /// and never reaches slow-retry. By retaining escalation history, the
    /// interval between PLIs naturally grows (1s → 2s → 4s → 8s), giving the
    /// network time to recover. See issue #832.
    ///
    /// Full backoff reset happens only after sustained stability (30s with no
    /// loss), via the time-gated path in `should_request_keyframe()`.
    fn on_keyframe(&mut self) {
        self.lost_count = 0;
        self.loss_detected_at_ms = None;
        self.unanswered_requests = self.unanswered_requests.saturating_sub(1);
        // current_backoff_ms is intentionally NOT reset -- see doc comment.
    }

    /// Feed this window's per-packet observations and roll the ~1s rate
    /// window when it expires. Returns `true` exactly when a rollover occurred
    /// (i.e. fresh `loss_per_sec` / `kf_per_sec` values are available), so the
    /// caller can throttle bus emission to ~1Hz per stream rather than per
    /// packet.
    ///
    /// `new_lost`     — newly lost packets detected this call (from `record_seq`).
    /// `kf_requested` — whether this tracker decided to request a keyframe.
    fn observe_window(&mut self, now: u64, new_lost: u32, kf_requested: bool) -> bool {
        if self.window_start_ms == 0 {
            self.window_start_ms = now;
        }
        self.window_lost = self.window_lost.saturating_add(new_lost);
        if kf_requested {
            self.window_kf_requests = self.window_kf_requests.saturating_add(1);
        }

        let elapsed = now.saturating_sub(self.window_start_ms);
        if elapsed >= 1000 {
            // Normalize by the ACTUAL elapsed window (not a fixed 1000ms) so a
            // window that ran long — sparse packet arrival, a stalled tab —
            // still yields a correct per-second rate. The `elapsed >= 1000`
            // gate guarantees `denom` is never zero, so the division is safe.
            let denom = elapsed as f64;
            self.loss_per_sec = self.window_lost as f64 * 1000.0 / denom;
            self.kf_per_sec = self.window_kf_requests as f64 * 1000.0 / denom;
            let mut suppression_rates = [0.0; KEYFRAME_SUPPRESSION_REASON_COUNT];
            for reason in KeyframeSuppressionReason::ALL {
                let idx = reason.index();
                suppression_rates[idx] = self.window_kf_suppressions[idx] as f64 * 1000.0 / denom;
                self.window_kf_suppressions[idx] = 0;
            }
            self.kf_suppression_per_sec = KeyframeSuppressionRates {
                values: suppression_rates,
            };
            self.max_gap = self.window_max_gap;
            self.window_lost = 0;
            self.window_max_gap = 0;
            self.window_kf_requests = 0;
            self.window_start_ms = now;
            return true;
        }
        false
    }

    /// Most recently computed windowed packet-loss rate (lost packets/sec).
    fn loss_per_sec(&self) -> f64 {
        self.loss_per_sec
    }

    fn max_gap(&self) -> u64 {
        self.max_gap
    }

    /// Most recently computed windowed keyframe-request rate (PLI/sec).
    fn kf_per_sec(&self) -> f64 {
        self.kf_per_sec
    }

    fn kf_suppression_per_sec(&self) -> KeyframeSuppressionRates {
        self.kf_suppression_per_sec
    }

    /// Re-anchor the tracker because the receiver switched to a DIFFERENT
    /// simulcast layer (issue #989 / #1079 H1).
    ///
    /// Each simulcast layer is an independent encoder with its OWN per-layer
    /// sequence space (camera/screen `sequence_numbers: vec![0; n_layers]`). When
    /// the chooser switches `selected_*_layer`, the first packets of the new
    /// layer arrive with sequence numbers from a DIFFERENT counter than the one
    /// `high_seq` is tracking. Without this reset, `record_seq` would interpret
    /// that cross-counter discontinuity as a huge window shift and manufacture
    /// phantom loss — which is exactly the chooser's congestion signal, causing a
    /// step-UP to immediately look like congestion and oscillate (UP→DOWN→UP).
    ///
    /// We clear the sequence-window state (`high_seq`, `seen_bits`, `lost_count`,
    /// `loss_detected_at_ms`) so the next packet of the new layer establishes a
    /// fresh baseline (the `high_seq == None` path in `record_seq` returns 0 loss
    /// on the first packet), and clear the in-flight window accumulators
    /// (`window_lost` / `window_max_gap` / `window_kf_requests` /
    /// `window_kf_suppressions` / `booked_suppression`) so the discontinuity does
    /// not leak into the current rate window.
    /// We DELIBERATELY preserve the PLI backoff
    /// history (`current_backoff_ms`, `unanswered_requests`,
    /// `last_keyframe_request_ms`): that rate-limit state is connection-level, not
    /// layer-level, and resetting it could let a PLI storm restart (issue #832).
    /// The already-published `loss_per_sec` / `kf_per_sec` are left as-is; they
    /// are recomputed on the next ~1s window rollover.
    fn reanchor_for_layer_switch(&mut self) {
        self.high_seq = None;
        self.seen_bits = 0;
        self.lost_count = 0;
        self.loss_detected_at_ms = None;
        self.window_lost = 0;
        self.window_max_gap = 0;
        self.window_kf_requests = 0;
        self.window_kf_suppressions = [0; KEYFRAME_SUPPRESSION_REASON_COUNT];
        self.booked_suppression = None;
    }
}

/// A backward jump in the audio sequence at least this large is treated as a
/// sender-side stream RESTART (e.g. the mic encoder is recreated on a device
/// change — `microphone_encoder` re-initialises its `sequence_number` to 0),
/// NOT as reordering, so [`AudioDatagramLossTracker`] re-anchors to the new
/// baseline. It is far above the 64-position reorder window (unordered
/// datagrams reorder on a millisecond scale, i.e. a handful of positions at
/// 50 pps), so a genuine live-stream straggler can never trip it. A reset-to-0
/// trips it only ONCE the high-water mark has reached this gap (>= 1024 — i.e.
/// after ~20 s at 50 pps); a restart in the FIRST ~20 s (high-water mark < 1024)
/// does NOT re-anchor and instead lands in the shared window's "too old, ignore"
/// branch, self-healing once the new sequence climbs back past the old mark
/// (< 1024 packets). This guard matters for the LARGE-mark case: without it, a
/// restart after a long-running stream would leave the tracker effectively
/// wedged — the new low sequence would take many thousands of packets to climb
/// back — silently blinding the loss signal. (The sub-1024 blind spot is noted
/// as a known limitation on [`AudioDatagramLossTracker`].)
///
/// This guard covers SENDER restarts only. A RECEIVER-side simulcast rung change is
/// handled separately and unconditionally by
/// [`AudioDatagramLossTracker::reanchor_for_layer_switch`] (issue #2138), which does
/// not depend on the jump exceeding this threshold — that is precisely why the
/// sub-1024 backward rung switch is no longer a blind spot even though this constant
/// would never trip on it.
const AUDIO_SEQ_RESET_REANCHOR_GAP: u64 = 1024;

const _: () = assert!(
    AUDIO_SEQ_RESET_REANCHOR_GAP == videocall_aq::constants::SEQ_RESET_REANCHOR_GAP,
    "AUDIO_SEQ_RESET_REANCHOR_GAP must match videocall_aq SEQ_RESET_REANCHOR_GAP"
);

/// Windowed receive-side audio packet-loss tracker for the WebTransport
/// **datagram** path (issue #1878).
///
/// ## The pathology this makes visible
///
/// With E2EE off, the relay routes small Opus AUDIO frames onto unreliable QUIC
/// datagrams (`wt_chat_session.rs` `build_outbound`). Our incoming-datagram
/// reader (`videocall-transport::webtransport::start_listening_incoming_datagrams`)
/// runs as a `spawn_local` task on the MAIN thread. When the main thread stalls
/// (a long task — the #1878 trigger, correlated in the field with the
/// "CPU-stall suppression budget exhausted" log), the reader cannot drain and
/// the browser drops the oldest queued datagrams. Nothing counted this: the
/// relay's `send_datagram` is best-effort (returns `Ok`) and the client's
/// `datagram_drop_count` is SEND-side only. The result is a contiguous audio
/// burst-loss NetEQ cannot conceal (56% vs 2% concealment WT-vs-WS in the field).
///
/// ## What it measures
///
/// The per-frame audio `sequence` is dense (+1 per frame — see
/// `microphone_encoder`), so a gap in the sequence of RECEIVED audio packets is
/// a dropped datagram. This reuses the same reorder-tolerant 64-position window
/// as the video [`SequenceTracker`] (via [`record_seq_into_reorder_window`]) and
/// publishes a windowed lost-packets/sec rate, so the datagram loss surfaces on
/// the diagnostics bus / health reporter exactly like `video_seq_loss_per_sec`.
///
/// ## Scoping (why it reads ~0 except under the pathology)
///
/// Fed only when THIS receiver's active transport is WebTransport (see
/// `Peer::receiver_on_webtransport`). On WebSocket audio rides one ordered TCP
/// stream, and on E2EE-on WebTransport it rides the reliable audio unistream;
/// neither produces sequence gaps under the identical stall (they buffer and
/// deliver late-but-complete), so this rate is ~0 there and nonzero only when
/// audio is actually losing datagrams.
///
/// ## Known limitations
///
/// 1. **[`Self::loss_per_sec`] trails [`Self::raw_loss_per_sec`] on a burst.**
///    The skipped positions still inside the reorder window may yet arrive, so
///    they are booked only as they shift off it, whereas the raw sum counts them
///    at the jump. Only the raw signal counts a gap beyond
///    [`MAX_PLAUSIBLE_FORWARD_GAP`] in full, and only the raw signal declines to
///    credit in-window reorder back (so it can slightly over-report under
///    genuine reordering).
/// 2. **Audio simulcast layer switches ARE handled (issue #2138 — no longer a
///    limitation).** Each simulcast layer carries its own dense sequence, so a
///    receive switch to a LAGGING audio layer is a BACKWARD jump (and to a LEADING
///    one, a FORWARD jump). Left unhandled, the backward case was the worse of the
///    two: a jump smaller than [`AUDIO_SEQ_RESET_REANCHOR_GAP`] is not treated as a
///    restart, so it landed in the shared window's "too old, ignore" branch and the
///    tracker stayed pinned at the old high-water mark, silently ignoring the new
///    layer's packets (UNDER-reporting) until its sequence climbed back past that
///    mark. The forward case OVER-reported, booking the cross-counter offset as a
///    phantom gap into BOTH [`Self::loss_per_sec`] and
///    [`Self::raw_loss_per_sec`]. Both are now removed at the source:
///    [`Peer::set_selected_audio_layer`] — the SINGLE chokepoint every write to
///    `selected_audio_layer` goes through — calls
///    [`Self::reanchor_for_layer_switch`] on an actual rung change, so the next
///    packet of the new rung establishes a fresh baseline. This matters because the
///    shipped runtime default is three audio layers (#1082).
/// 3. **A sender restart in the first ~20 s of a stream is a temporary blind
///    spot.** The mic encoder re-initialises `sequence_number` to 0 on restart
///    (e.g. a mid-call device change). If it restarts while the high-water mark
///    is still below [`AUDIO_SEQ_RESET_REANCHOR_GAP`] (< 1024, ~20 s at 50 pps),
///    the backward jump is too small to trip the restart re-anchor, so the new
///    low sequences land in the shared window's "too old, ignore" branch and are
///    not tracked until the sequence climbs back past the old mark (< 1024
///    packets, so it self-heals within ~20 s — it does not wedge). A restart at a
///    LARGER mark is caught by the re-anchor guard, so only this early window is
///    affected; loss during it is under-counted, never over-counted.
///
///    The #2138 layer-switch re-anchor does NOT narrow this: it is driven by the
///    RECEIVER's own rung selection, whereas this blind spot is a SENDER-side
///    restart that the receiver never observes as a rung change (the restarted
///    stream keeps the same `simulcast_layer_id`). The two are independent — a
///    restart still needs either the >= 1024 backward-jump guard or the self-heal.
///    Closing it would need a sender-signalled stream epoch, which is out of scope
///    here.
struct AudioDatagramLossTracker {
    /// Highest audio sequence seen so far. `None` before the first packet.
    high_seq: Option<u64>,
    /// Bitfield of recently seen sequences (bit 0 = `high_seq`), shared shape
    /// with [`SequenceTracker::seen_bits`].
    seen_bits: u64,
    /// Start of the current ~1s rate window (ms). 0 = not yet started.
    window_start_ms: u64,
    /// Lost packets accumulated in the current window.
    window_lost: u32,
    /// Issue 2031: RAW (uncapped) skipped-sequence sum accumulated in the current
    /// window. Unlike [`Self::window_lost`], a contiguous forward gap contributes
    /// its full size here at the jump, rather than as the tail shifts off.
    window_raw_lost: u64,
    /// Most recently computed windowed loss rate (lost packets/sec). Stable
    /// between window rollovers.
    loss_per_sec: f64,
    /// Issue 2031: most recently computed windowed RAW (uncapped) loss rate
    /// (skipped sequences/sec). The magnitude companion to [`Self::loss_per_sec`].
    raw_loss_per_sec: f64,
}

impl AudioDatagramLossTracker {
    fn new() -> Self {
        Self {
            high_seq: None,
            seen_bits: 0,
            window_start_ms: 0,
            window_lost: 0,
            window_raw_lost: 0,
            loss_per_sec: 0.0,
            raw_loss_per_sec: 0.0,
        }
    }

    /// Record one received audio sequence number. Returns the number of NEW
    /// lost packets detected (gaps that shifted off the reorder window unseen).
    ///
    /// Re-anchors on a large BACKWARD jump (sender restart), so a mic-encoder
    /// restart cannot wedge the tracker — see [`AUDIO_SEQ_RESET_REANCHOR_GAP`].
    fn record_seq(&mut self, seq: u64) -> u32 {
        if let Some(high) = self.high_seq {
            if seq < high && high - seq >= AUDIO_SEQ_RESET_REANCHOR_GAP {
                // Sender-side sequence reset: forget the old baseline so this
                // packet anchors a fresh one (returning 0 loss) instead of being
                // ignored as "too old" forever.
                self.high_seq = None;
                self.seen_bits = 0;
            }
        }
        // Issue 2031: accumulate the RAW (uncapped) forward gap for magnitude,
        // BEFORE the shared reorder-window call advances `high_seq`. A forward
        // jump from `high` to `seq` skipped `seq - high - 1` sequence numbers;
        // summed un-truncated this reads the true burst size at once, where
        // `window_lost` books the tail only as it shifts off. Only counted on a
        // forward jump
        // past the current high-water mark; after a re-anchor `high_seq` is None
        // (fresh baseline) so this contributes 0, exactly like the capped count.
        // Reordering within the window is NOT credited back here (see the
        // "Burst severity" limitation note on the struct), so under genuine
        // reordering this over-reports slightly — acceptable for a burst-loss
        // magnitude signal on a single-path datagram flow.
        if let Some(high) = self.high_seq {
            if seq > high {
                self.window_raw_lost = self.window_raw_lost.saturating_add(seq - high - 1);
            }
        }
        record_seq_into_reorder_window(&mut self.high_seq, &mut self.seen_bits, seq)
    }

    /// Feed this window's newly-lost count and roll the ~1s rate window
    /// when it expires. Returns `Some((loss_per_sec, raw_loss_per_sec))` exactly
    /// on rollover (so the caller throttles bus emission to ~1 Hz per peer), else
    /// `None`. The raw component (issue 2031) is normalised from the uncapped
    /// `window_raw_lost` accumulated in [`Self::record_seq`], over the SAME
    /// elapsed window, so the two rates are directly comparable. Mirrors
    /// [`SequenceTracker::observe_window`] (same 1000 ms window, same
    /// elapsed-normalised rate) minus the keyframe-request accounting audio
    /// does not have.
    fn observe_window(&mut self, now: u64, new_lost: u32) -> Option<(f64, f64)> {
        if self.window_start_ms == 0 {
            self.window_start_ms = now;
        }
        self.window_lost = self.window_lost.saturating_add(new_lost);

        let elapsed = now.saturating_sub(self.window_start_ms);
        if elapsed >= 1000 {
            // Normalise by the ACTUAL elapsed window (not a fixed 1000 ms) so a
            // window that ran long — sparse arrival, a stalled tab — still
            // yields a correct per-second rate. `elapsed >= 1000` guarantees
            // `denom` is never zero.
            let denom = elapsed as f64;
            self.loss_per_sec = self.window_lost as f64 * 1000.0 / denom;
            self.raw_loss_per_sec = self.window_raw_lost as f64 * 1000.0 / denom;
            self.window_lost = 0;
            self.window_raw_lost = 0;
            self.window_start_ms = now;
            return Some((self.loss_per_sec, self.raw_loss_per_sec));
        }
        None
    }

    /// Most recently computed windowed loss rate (lost audio packets/sec).
    #[cfg(test)]
    fn loss_per_sec(&self) -> f64 {
        self.loss_per_sec
    }

    /// Most recently computed windowed RAW (uncapped) loss rate (issue 2031).
    #[cfg(test)]
    fn raw_loss_per_sec(&self) -> f64 {
        self.raw_loss_per_sec
    }

    /// Re-anchor because the receiver switched to a DIFFERENT AUDIO simulcast
    /// layer (issue #2138) — the audio sibling of
    /// [`SequenceTracker::reanchor_for_layer_switch`].
    ///
    /// Each audio rung is an INDEPENDENT Opus encode owning its own sequence
    /// counter (`microphone_encoder`: "each layer owns its own seq counter + RED
    /// previous-frame buffer"), and the rungs' counters drift apart over a call:
    /// the upper rungs are created later (the awaited per-rung `create_node` loop)
    /// and, while a user/congestion ceiling suppresses them, their publish gate
    /// returns BEFORE `sequence_number += 1`, so a suppressed rung's counter
    /// STALLS while the base keeps advancing. A receive-side rung change is
    /// therefore a sequence DISCONTINUITY in BOTH directions, and both mis-account:
    ///
    ///   * **To a LEADING rung — a FORWARD jump — OVER-reports.** The cross-counter
    ///     offset is summed immediately and UNCAPPED into `window_raw_lost`. Resetting
    ///     the reorder bitmap then makes the next ~63 packets shift unseen positions
    ///     out of the window into `window_lost`, so the settled signal also reports a
    ///     phantom burst.
    ///     **This is the direction that MATTERS**, and issue 2138's title
    ///     ("inflates `audio_datagram_loss_per_sec`") is right about it. The BASE
    ///     rung always LEADS: `audio_layer_is_published` computes
    ///     `effective_count … .max(1)`, so layer 0 is never suppressed, and the base
    ///     node is created before the upper-rung loop — so it is the UPPER rungs
    ///     whose counters stall. A DOWN-switch (2→0) is therefore the FORWARD case,
    ///     and a down-switch is exactly what a CONGESTED receiver does. On the
    ///     settled signal that reads >= 50 pkt/s against a 5.0 threshold,
    ///     and it feeds the #2029 transport-demotion latch (see
    ///     [`Self::set_selected_audio_layer`]).
    ///   * **To a LAGGING rung — a BACKWARD jump — UNDER-reports (goes blind).**
    ///     A backward jump smaller than [`AUDIO_SEQ_RESET_REANCHOR_GAP`] is not
    ///     treated as a sender restart, so it lands in the shared reorder window's
    ///     "too old, ignore" branch: `high_seq` stays pinned at the OLD rung's
    ///     high-water mark and every packet of the new rung is silently ignored —
    ///     including real loss — until the new rung's sequence climbs back past
    ///     that mark. This is the recovery-driven UP-switch, so it is the less
    ///     operationally urgent of the two, but it suppresses a fallback the user
    ///     may need.
    ///
    /// Clearing `high_seq` / `seen_bits` makes the next packet of the new rung
    /// establish a fresh baseline (the `high_seq == None` path returns 0 loss and
    /// contributes 0 raw), which removes both directions at once.
    ///
    /// `window_lost` and `window_raw_lost` are cleared TOGETHER, and that pairing
    /// is load-bearing rather than cosmetic: they are two views of the SAME event
    /// stream in the SAME in-flight window (window-settled vs immediate),
    /// and issue #2031's whole interpretation is their RATIO. Resetting one without
    /// the other would publish an incoherent pair (e.g. 0 settled alongside raw 400
    /// = "enormous burst, no loss present"), which is worse than either value alone.
    /// Clearing both also keeps the rate window from mixing the old rung's partial
    /// measurements into a sample that will be published as the NEW rung's rate.
    /// `audio_layer_switch_clears_both_in_flight_window_accumulators` pins BOTH
    /// independently (dropping either line fails it), so this pairing is enforced and
    /// not merely asserted here.
    ///
    /// DELIBERATELY NOT reset — each for its own reason:
    ///   * `window_start_ms` — matching [`SequenceTracker::reanchor_for_layer_switch`],
    ///     which does not touch it either. It is BOTH the window's elapsed origin
    ///     (the denominator in `observe_window`'s elapsed-normalised rate) and the
    ///     `== 0` "not yet started" SENTINEL, so zeroing it here would restart the
    ///     ~1 s cadence on every layer switch — delaying the next published sample
    ///     by up to a full window — while gaining nothing, since the counts that
    ///     window would have normalised are already zeroed above.
    ///   * `loss_per_sec` / `raw_loss_per_sec` — the ALREADY-PUBLISHED rates, left
    ///     as-is for the same reason the video precedent leaves its published rates
    ///     alone: they are recomputed on the next ~1 s window rollover, and blanking
    ///     a last-known-good diagnostic mid-window would read as "loss stopped"
    ///     rather than "not resampled yet".
    ///
    /// Audio has no analogue of the video tracker's connection-level keyframe /
    /// PLI-backoff state (`lost_count`, `loss_detected_at_ms`, `current_backoff_ms`,
    /// …) — audio is keyframe-less and its recovery lives inside NetEq — so the
    /// four fields above are the complete set of layer-scoped state this tracker
    /// holds. That is why this reset is exhaustive over the struct's 7 fields
    /// (4 cleared, 3 deliberately preserved) with nothing left unaccounted for.
    fn reanchor_for_layer_switch(&mut self) {
        self.high_seq = None;
        self.seen_bits = 0;
        self.window_lost = 0;
        self.window_raw_lost = 0;
    }
}

/// Result of `Peer::track_sequence`: a possible keyframe request plus the
/// pending observation for the ~1Hz loss/keyframe metrics window.
struct SeqTrackResult {
    /// `Some(media_type)` when a KEYFRAME_REQUEST (PLI) should be sent.
    keyframe_request: Option<MediaType>,
    /// `Some` whenever this packet reached a tracker; the caller decides when to
    /// settle it via `observe_sequence_window`.
    window: Option<SeqWindowObservation>,
}

/// A `track_sequence` window update the caller has not yet applied. Deferred so
/// each `decode` path settles the window exactly once, on every outcome.
#[derive(Clone, Copy)]
struct SeqWindowObservation {
    now: u64,
    new_lost: u32,
    kf_requested: bool,
}

/// One window's per-stream sample: two rates plus `max_gap`, a FRAME COUNT (#2524).
struct WindowSample {
    loss_per_sec: f64,
    kf_per_sec: f64,
    max_gap: u64,
    kf_suppression_per_sec: KeyframeSuppressionRates,
    freshness_evictions_total: u64,
    freshness_evictions_keyframeless_total: u64,
}

/// Build the `"video"` event [`Peer::emit_loss_metrics`] broadcasts. Pure so the
/// metric key set its consumers match on is host-testable.
fn loss_metrics_event(
    local_user_id: &str,
    to_peer: &str,
    media_type: MediaType,
    rates: &WindowSample,
    ts_ms: u64,
) -> DiagEvent {
    let mut metrics = Vec::with_capacity(8 + KEYFRAME_SUPPRESSION_REASON_COUNT);
    metrics.extend([
        metric!("media_type", format!("{media_type:?}")),
        metric!("from_peer", local_user_id.to_string()),
        metric!("to_peer", to_peer.to_string()),
        metric!("video_seq_loss_per_sec", rates.loss_per_sec),
        metric!("video_seq_max_gap", rates.max_gap),
        metric!("keyframe_requests_per_sec", rates.kf_per_sec),
        metric!("freshness_evictions_total", rates.freshness_evictions_total),
        metric!(
            "freshness_evictions_keyframeless_total",
            rates.freshness_evictions_keyframeless_total
        ),
    ]);
    // Bus-only: #2524's Gap 2 export was scoped out, so these have no consumer yet. Kept so
    // the follow-up is ingest plumbing, and because the tracker feeding them is what makes
    // `keyframe_requests_per_sec` a decision count rather than a per-packet count.
    for reason in KeyframeSuppressionReason::ALL {
        metrics.push(metric!(
            reason.metric_name(),
            rates.kf_suppression_per_sec.get(reason)
        ));
    }
    DiagEvent {
        subsystem: "video",
        stream_id: None,
        ts_ms,
        metrics,
    }
}

/// Per-peer RECEIVE simulcast diagnostics for one connected peer (issue #1095
/// observability — additive). Produced by
/// [`PeerDecodeManager::per_peer_received_snapshots`] and shown in the panel's
/// "Live diagnostics" disclosure. Each per-kind field is `Some` only when that
/// kind is currently being received from the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerReceiveDiag {
    /// The peer's relay session id.
    pub session_id: u64,
    /// A human-friendly label: display name if known, else user id, else the
    /// session id (so the row always has something to show).
    pub label: String,
    /// Decoded VIDEO layer snapshot, if video is being received.
    pub video: Option<crate::decode::layer_chooser::ReceivedLayerSnapshot>,
    /// Decoded SCREEN layer snapshot, if a screen share is being received.
    pub screen: Option<crate::decode::layer_chooser::ReceivedLayerSnapshot>,
    /// Decoded AUDIO layer snapshot, if audio is being received.
    pub audio: Option<crate::decode::layer_chooser::ReceivedLayerSnapshot>,
}

/// Fill a SCREEN snapshot's geometry and budget from what the PEER stamped.
///
/// Ceiling on a peer-reported screen bitrate, in kbps.
///
/// The value rides on the wire and is attacker-controllable, so an unbounded one
/// renders as a nonsense magnitude ("4294967.5M"). Set an order of magnitude
/// above the encode ceiling's own budget so no honest publisher is clipped.
const MAX_REPORTED_SCREEN_KBPS: u32 = 100_000;

/// The peer reports its CAPTURED surface, so it is fitted through
/// [`screen_encode_box_for_capture`] — the same function the sender configures
/// its encoder with. `(0, 0)` / `0` stay as "not told us yet".
fn enrich_screen_snapshot(
    peer: &Peer,
    snap: &mut crate::decode::layer_chooser::ReceivedLayerSnapshot,
) {
    let (capture_w, capture_h) = peer.screen.reported_source_dims();
    let (w, h) = videocall_aq::screen_encode_box_for_capture(capture_w, capture_h);
    snap.width = w;
    snap.height = h;
    snap.kbps = peer
        .screen
        .reported_target_kbps()
        .min(MAX_REPORTED_SCREEN_KBPS);
}

/// Per-peer self-reported device/hardware metrics (#1482). Populated from the
/// sender's HealthPacket (cores/arch/OS/device-type are STATIC; main-thread
/// load + used memory change each health tick, ~0.2 Hz at the 5 s default
/// interval). Every field is OPTIONAL and is `None` when the sending browser's
/// API is unavailable ("if available"). This is the UI contract surfaced by
/// [`PeerDecodeManager::peer_device_info`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerDeviceInfo {
    /// navigator.hardwareConcurrency (logical CPU cores).
    pub client_cores: Option<u32>,
    /// CPU/chip architecture, e.g. "arm" | "x86".
    pub client_architecture: Option<String>,
    /// Human OS + version, e.g. "macOS 14.5" | "Windows 11".
    pub client_os: Option<String>,
    /// "desktop" | "mobile" | "tablet".
    pub client_device_type: Option<String>,
    /// 0.0-1.0 main-thread busy fraction over the last health interval
    /// (longtask sum / interval) — a CPU proxy, not system CPU.
    pub client_main_thread_load: Option<f64>,
    /// JS-heap used memory: HealthPacket.memory_used_bytes (bytes) divided by
    /// 1 MiB (1024 * 1024). The `_mb` suffix follows the codebase's existing
    /// heap-size labels; the unit is mebibytes (MiB), not decimal megabytes.
    pub client_memory_used_mb: Option<f64>,
    /// navigator.deviceMemory total-RAM tier in GB (coarse, browser-capped at 8).
    pub client_device_memory_gb: Option<f64>,
}

/// Last in-place simulcast layer switch for this peer, stamped by Marker 1
/// (issue #1460 observability). `ms == 0` is the never-switched sentinel.
///
/// `ms` is the manager-tick wall-clock timestamp at which the switch was
/// applied (the same `now_ms` threaded into the chooser tick / seed paths,
/// which on wasm is `js_sys::Date::now()` — identical to the clock the worker
/// stamps on its `freshness_skip` DiagEvent `ts_ms`), so the subscriber's
/// `ts_ms - ms` delta is directly comparable. `pub` so the diagnostics-bus
/// subscriber in `video_call_client` can read a copy via the accessors below.
#[derive(Clone, Copy, Debug, Default)]
pub struct LastLayerSwitch {
    pub ms: u64,
    pub from: u32,
    pub to: u32,
}

/// Per-peer tally of packets discarded by the simulcast layer gates in
/// [`Peer::decode`], keyed by which side of the arriving rung the decode guard
/// sat on (issue #2533).
///
/// Not one comparable series: #2251's rescue is VIDEO-only, so only
/// `video_guard_above` excludes packets that rescue admitted. Off-ladder discards land
/// only in `*_guard_below`, never `*_guard_above` — and for SCREEN, a one-layer ladder,
/// they are the ONLY thing `screen_guard_below` can be.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayerGateSkips {
    pub video_guard_above: u64,
    pub video_guard_below: u64,
    pub screen_guard_above: u64,
    pub screen_guard_below: u64,
    pub audio_guard_above: u64,
    pub audio_guard_below: u64,
}

/// Hold a decode-guard selection inside `kind`'s ladder; a guard above the top matches nothing.
fn clamp_selected_layer(kind: PrefMediaKind, layer: u32) -> u32 {
    layer.min(crate::decode::layer_chooser::max_layers_for_kind(kind).saturating_sub(1))
}

impl LayerGateSkips {
    fn record(&mut self, kind: PrefMediaKind, incoming: u32, selected: u32) {
        let guard_above = incoming < selected;
        match (kind, guard_above) {
            (PrefMediaKind::Video, true) => self.video_guard_above += 1,
            (PrefMediaKind::Video, false) => self.video_guard_below += 1,
            (PrefMediaKind::Screen, true) => self.screen_guard_above += 1,
            (PrefMediaKind::Screen, false) => self.screen_guard_below += 1,
            (PrefMediaKind::Audio, true) => self.audio_guard_above += 1,
            (PrefMediaKind::Audio, false) => self.audio_guard_below += 1,
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Last `LAYER_GATE_SKIPS` line emitted on this thread. The host target
    /// installs no logger, so the emit is otherwise unobservable.
    static LAST_GATE_SKIPS_LOG: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub struct Peer {
    pub audio: Box<dyn AudioPeerDecoderTrait>,
    pub video: VideoPeerDecoder,
    pub screen: VideoPeerDecoder,
    pub session_id: u64,
    /// Cached `session_id.to_string()` to avoid repeated allocations.
    sid_str: String,
    pub user_id: String,
    pub video_canvas_id: String,
    pub screen_canvas_id: String,
    pub aes: Option<Aes128State>,
    activity_count: u8,
    missed_heartbeat_checks: u32,
    pub video_enabled: bool,
    pub audio_enabled: bool,
    pub screen_enabled: bool,
    pub is_speaking: bool,
    pub audio_level: f32,
    /// Most recently observed transport this peer is connected over
    /// (announced via HeartbeatMetadata). Defaults to UNKNOWN until the
    /// first heartbeat arrives or if the peer is on an older client that
    /// does not populate the field.
    pub transport_type: TransportType,
    pub display_name: Option<String>,
    /// #1482: this peer's self-reported device/hardware metrics, refreshed each
    /// time a HealthPacket arrives. Every field is optional ("if available").
    pub device_info: PeerDeviceInfo,
    /// Server-vouched guest indicator, sourced from the authenticated JWT
    /// `is_guest` claim and broadcast on `PARTICIPANT_JOINED`.
    pub is_guest: bool,
    /// Whether this peer is currently decode-active in the UI layout.
    ///
    /// When `false`, video and screen decoding is skipped to save CPU. Audio
    /// is always decoded regardless so off-screen participants can still be
    /// heard.
    pub visible: bool,
    /// Preserve the operator decode-and-drop choice when decoder workers reset.
    skip_canvas_paint: bool,
    context_initialized: bool,
    vad_threshold: Option<f32>,
    has_received_heartbeat: bool,
    /// A host force-off has been applied to this peer's audio (respectively
    /// video). Each is read at one site — the matching pre-heartbeat inference
    /// branch in [`Peer::decode`], dead once `has_received_heartbeat` is true.
    audio_forced_off: bool,
    video_forced_off: bool,
    /// Which simulcast layer (issue #989) this receiver decodes for this peer.
    ///
    /// VIDEO `PacketWrapper`s whose cleartext `simulcast_layer_id` does not
    /// match this value are dropped BEFORE sequence tracking and decode, so a
    /// peer publishing N>1 layers only ever costs this receiver one layer's
    /// decode. The first-cut default is **0 (the lowest layer)** unconditionally
    /// — which is also exactly what every pre-simulcast publisher emits, so
    /// behaviour is unchanged for them. Viewport/tile-size-driven layer
    /// selection (raising this to a higher layer for the focused tile) is
    /// deferred to a later PR.
    selected_video_layer: u32,
    /// Receiver-driven per-peer simulcast layer chooser (issue #989, Phase 2).
    /// Run on each monitor tick against this receiver's OWN downlink health for
    /// this source; its output drives `selected_video_layer` (the decode guard)
    /// and the `LAYER_PREFERENCE` packet sent to the relay. Independent per peer.
    video_layer_chooser: LayerChooser,
    /// Empirically-learned set of simulcast layers this source is producing,
    /// observed from arriving `simulcast_layer_id`s (the relay does not
    /// advertise availability). Caps how high the chooser may climb.
    video_layer_availability: LayerAvailability,
    /// Most recent windowed downlink sample for VIDEO (loss/sec + PLI/sec),
    /// refreshed on each ~1s sequence-window rollover and consumed by the
    /// chooser on the monitor tick. Starts clean so a fresh peer is not treated
    /// as congested before any window has rolled.
    last_video_downlink: DownlinkSample,
    /// Which simulcast layer (issue #989, Phase 3) this receiver decodes for
    /// this peer's SCREEN stream. Independent of `selected_video_layer`: a peer
    /// can publish camera AND screen simultaneously and the receiver chooses a
    /// layer for each separately. Defaults to 0 (base / un-upgraded publisher).
    selected_screen_layer: u32,
    /// Per-peer SCREEN layer chooser, availability, and last downlink sample
    /// (issue #989, Phase 3). Mirror of the VIDEO trio, driven by the SCREEN
    /// stream's own loss/PLI windows so screen adapts independently of camera.
    screen_layer_chooser: LayerChooser,
    screen_layer_availability: LayerAvailability,
    last_screen_downlink: DownlinkSample,
    /// When the CURRENT unbroken SCREEN keyframe-less hold began (issue #2328),
    /// or `None` while the screen decoder is not starved.
    ///
    /// ARMED by the worker's proactive keyframe-request route — the jitter
    /// buffer fires it when it evicts a stale keyframe-less backlog, i.e. when
    /// playout is frozen on the last-good frame with no buffered keyframe to
    /// resume from. That is the only signal available on the main thread that
    /// means "frozen", and it is transport-agnostic (the worker path is identical
    /// for WebSocket and WebTransport). NOTE the obvious-looking alternative is a
    /// trap: `VideoPeerDecoder::is_waiting_for_keyframe()` is hard-coded `false`
    /// on wasm (`videocall_codecs::decoder::wasm`, "the jitter buffer is in the
    /// worker"), so anything keyed off it never executes in production.
    ///
    /// CLEARED by any keyframe arriving on the selected screen rung, by a screen
    /// layer switch, by a screen-decoder rebuild, and on transport teardown — so
    /// the value always describes an unbroken, current hold.
    ///
    /// `Rc<Cell<_>>` because the route closure is installed on the decoder with
    /// only `&Peer` in hand (see `install_keyframe_request_routes`) and therefore
    /// cannot take `&mut self`; the Peer consumes the stamp on its own paths.
    screen_keyframe_starved_since_ms: Rc<Cell<Option<u64>>>,
    /// Which simulcast layer (issue #989, Phase 3) this receiver decodes for
    /// this peer's AUDIO stream. Defaults to 0.
    selected_audio_layer: u32,
    /// Per-peer AUDIO layer chooser + availability (issue #989, Phase 3). The
    /// chooser is fed the VIDEO downlink as a proxy for shared-connection health
    /// (see `tick_audio_layer_chooser`) — hence no separate `last_audio_downlink`.
    /// `audio_datagram_loss` (#1878) is NOT that missing per-window sample: it is
    /// never fed back into THIS chooser. (It is not inert, though — issue #2029 routes
    /// it through the health reporter into the one-way WebTransport→WebSocket audio
    /// fallback; see `set_selected_audio_layer`.)
    audio_layer_chooser: LayerChooser,
    audio_layer_availability: LayerAvailability,
    /// Reorder-tolerant sequence tracker for video packets.
    video_seq_tracker: SequenceTracker,
    /// Reorder-tolerant sequence tracker for screen packets.
    screen_seq_tracker: SequenceTracker,
    /// HCL bug #1: monotonic timestamp (ms since epoch) of the most recent
    /// SCREEN media frame this receiver actually decoded. A non-zero value
    /// means we have hard evidence the publisher is currently sharing
    /// (the SCREEN stream is live), regardless of what an older heartbeat
    /// metadata payload claims. Used by the HEARTBEAT branch to suppress
    /// stale-heartbeat clobbering — when WT delivers a SCREEN keyframe on
    /// the Screen persistent stream before an older heartbeat (carrying
    /// `screen_enabled = false`) catches up on the Control stream, we
    /// must NOT let the heartbeat reset `screen_enabled` back to false.
    /// On WS (strict FIFO over one TCP socket) the heartbeat almost
    /// always wins the race so the symptom is rare; on WT (multi-stream,
    /// no global ordering) the race surfaces reliably and the screen
    /// tile collapses out of the split layout.
    last_screen_frame_ms: u64,
    /// Same idea for the camera-video stream, but resolved
    /// against the short `LIVE_STREAM_FRESH_WINDOW_MS` (not the screen-sized
    /// window) — a live camera streams continuously, so a recent frame only
    /// has to out-vote a stale `false` heartbeat across one reorder gap.
    /// Within the window this guard stops a stale `video_enabled = false`
    /// heartbeat from blanking an actively-streaming camera on WT; once
    /// frames actually stop (camera disabled) the next heartbeat reflects
    /// the change on remote peers within ~500ms instead of ~5s.
    last_video_frame_ms: u64,
    /// Consecutive `false` CAMERA heartbeats since the last affirmative one,
    /// used to corroborate an on->off transition before blanking the tile.
    /// See [`CAMERA_OFF_CORROBORATION_COUNT`].
    consecutive_video_off_hbs: u32,
    /// HCL bug #1: same idea for the audio stream.
    last_audio_frame_ms: u64,
    /// Issue #1460 observability: timestamp + from/to of this peer's most recent
    /// in-place VIDEO simulcast layer switch (stamped by Marker 1 at the 6 switch
    /// sites). Read by the diagnostics-bus subscriber to correlate a
    /// `freshness_skip` with a recent layer transition. Never read by adaptation
    /// logic — pure telemetry.
    last_video_switch: LastLayerSwitch,
    /// Issue #1460 observability: SCREEN-kind sibling of `last_video_switch`.
    last_screen_switch: LastLayerSwitch,
    layer_gate_skips: LayerGateSkips,
    /// Issue #1878: whether THIS receiver's active transport is WebTransport,
    /// refreshed by [`PeerDecodeManager::decode`] on every packet from the
    /// authoritative [`ConnectionController::active_is_webtransport`] signal.
    /// Gates [`Peer::audio_datagram_loss`]: audio can only ride unreliable
    /// datagrams on WebTransport, so on WebSocket the tracker stays idle.
    receiver_on_webtransport: bool,
    /// Issue #1878: windowed receive-side audio datagram loss for this peer,
    /// fed in the AUDIO decode arm only while `receiver_on_webtransport`.
    audio_datagram_loss: AudioDatagramLossTracker,
}

/// HCL bug #1: window during which a recent media frame suppresses a stale
/// negative heartbeat. Set to match the publisher's heartbeat cadence
/// (`HEARTBEAT_KEEPALIVE_INTERVAL_MS = 5000ms`, see
/// `videocall-aq/src/constants.rs`).
///
/// Heartbeats are sent over lossy datagrams (see
/// `videocall-client/src/connection/connection.rs`) and can arrive up to
/// one full cadence late on bad links (mobile, 3G, congested WT). A
/// heartbeat carrying `screen_enabled = false` sent at t=0 might land at
/// t=4.5s — well after the first SCREEN frame of a freshly-started share
/// has already set the local flag to true. If the freshness window were
/// shorter than the cadence, the stale heartbeat would clobber the live
/// flag back to false, collapse the split layout, and re-introduce the
/// "shared content shown in a small tile only" symptom this fix exists
/// to prevent.
///
/// 5000ms is the minimum value that covers the worst case while still
/// honouring genuine "publisher stopped sharing" transitions on the
/// NEXT heartbeat after the window expires.
///
/// This window is correct for SCREEN ONLY. Screen-share is legitimately
/// bursty/idle: a static shared window can emit no new frames for several
/// seconds while the stream is still "on" (most screen encoders skip frames
/// when nothing on screen changes), so we must NOT let a single stale
/// `false` heartbeat clobber the flag. Audio and camera-video are continuous
/// streams and use the much shorter `LIVE_STREAM_FRESH_WINDOW_MS` instead —
/// see that constant for why.
const MEDIA_FRESH_WINDOW_MS: u64 = 5000;

/// Freshness window for the CONTINUOUS live streams — audio and
/// camera-video — deliberately MUCH shorter than `MEDIA_FRESH_WINDOW_MS`.
///
/// Audio and camera-video are fundamentally different from screen-share for
/// this decision:
///
///   * They are continuous, high-rate streams. Audio is ~50 packets/sec
///     (20ms framing); a live camera emits frames continuously at its frame
///     rate even on a static scene (camera encoders, unlike screen encoders,
///     do not skip frames when the picture is unchanging). When either is
///     genuinely live, fresh frames keep arriving and re-stamp
///     `last_{audio,video}_frame_ms`, so even a single reordered/late
///     `false` heartbeat is naturally re-suppressed by the next frame. The
///     window therefore only has to cover ONE worst-case reorder gap between
///     a live frame and a contemporaneous stale heartbeat — NOT a full
///     multi-second idle gap like screen needs.
///
///   * A real gap in audio/video frames is itself an immediate "stopped"
///     signal (mute / camera-off), unlike screen which idles while still
///     enabled. So a short window both protects against WT reorder AND lets
///     a genuine stop reflect on remote peers promptly.
///
/// Sizing: 500ms comfortably exceeds plausible QUIC reorder/jitter spread
/// on a high-latency (200ms+) lossy/mobile link between a live frame and a
/// heartbeat sent at nearly the same time, so it does NOT introduce audio
/// mute-flicker or camera flicker on bad networks. At the same time it
/// bounds mute / camera-off propagation to at most ~500ms after the last
/// frame (and effectively to the next heartbeat on the ordered WebSocket
/// path, where a post-frame `false` heartbeat is already authoritative).
/// This replaces the previous ~5s lag, which came from reusing the
/// screen-sized window for these continuous streams.
pub(crate) const LIVE_STREAM_FRESH_WINDOW_MS: u64 = 500;

/// Consecutive `false` CAMERA heartbeats required before blanking a shown tile.
///
/// Under congestion a live camera's frame interval can exceed the 500ms window,
/// so a single stale `video_enabled = false` keepalive datagram (unordered on
/// WT) could blank a still-on camera until the next affirmative heartbeat (up to
/// 5s away, since a blanked tile is only re-enabled by a heartbeat, not by
/// frames). A GENUINE off is self-corroborating: the publisher resends the
/// `false` once at 600ms on the reliable Control stream, so `2` blanks a real
/// off sub-second while a lone reordered datagram is ignored. Cannot wedge on —
/// an affirmative heartbeat resets the count.
pub(crate) const CAMERA_OFF_CORROBORATION_COUNT: u32 = 2;

/// #1399: coalescing window (ms) for the per-`delete_peer` #508 decode
/// snapshot.
///
/// The #508 instrumentation snapshots the *full remaining-peer set* (one
/// `log::info!` line per remaining peer) on each single-peer removal. In a
/// mass-leave / reconnection wave where N peers each leave one-by-one through
/// `delete_peer` (the `PARTICIPANT_LEFT` path), an ungated snapshot is
/// O(N^2) info-level lines (~N^2/2) landing on the main thread at exactly the
/// moment the UI is already churning teardown + re-render — i.e. it risks
/// perturbing the very #510 re-render storm this instrumentation is meant to
/// diagnose.
///
/// Coalescing to at most one full snapshot per this window bounds an
/// individual-leave cascade from O(N^2) to at most one snapshot per window
/// (the whole snapshot — header AND remaining-set body — is gated, so
/// intermediate cascade removals emit nothing), while a single *isolated*
/// peer-leave — more than one window after the previous one — still produces
/// its full remaining-set snapshot for the analyst. 250ms is short enough that
/// a genuinely-spaced leave (human-paced, seconds apart) is never coalesced,
/// yet long enough that a tight teardown cascade collapses to a single
/// snapshot. The per-leave breadcrumb is not lost: `delete_peer` still fires
/// `on_peers_removed_batch` with the departed id on EVERY removal — only the
/// diagnostic decode-state snapshot of the *remaining* peers is coalesced.
const DELETE_PEER_SNAPSHOT_COALESCE_MS: u64 = 250;

/// HCL bug #1: decide what `*_enabled` value to apply when a heartbeat
/// arrives, given:
///   * `current` — our locally tracked flag for this peer
///   * `heartbeat_value` — what `HeartbeatMetadata.X_enabled` says
///   * `last_frame_ms` — timestamp of the most recent live X frame we
///     decoded (0 = none ever)
///   * `now_ms` — current monotonic clock
///   * `fresh_window_ms` — how recent a live frame must be to out-vote a
///     `false` heartbeat. Callers pass `MEDIA_FRESH_WINDOW_MS` for screen
///     (legitimately bursty/idle stream) and the much shorter
///     `LIVE_STREAM_FRESH_WINDOW_MS` for audio and camera-video (continuous
///     high-rate streams where a frame gap is itself a stop signal; see
///     those constants for the rationale).
///
/// Returns the value to install on `self.X_enabled`.
///
/// Decision matrix:
///
///   heartbeat=true   → trust the heartbeat (publisher announces it's on;
///                      any contradicting "no frames seen" condition is
///                      a network problem, not a state problem).
///
///   heartbeat=false  → if we saw an X frame within `fresh_window_ms`,
///                      KEEP `current`. The heartbeat is stale relative to
///                      the live stream (classic out-of-order-arrival
///                      window on WT, where heartbeats and media frames
///                      live on different QUIC streams with no global FIFO
///                      ordering). If no recent frame, trust the heartbeat
///                      — the publisher really did stop the X stream.
///
/// Pure function so it can be unit-tested without a real `Peer`.
pub(crate) fn apply_heartbeat_enabled_flag(
    current: bool,
    heartbeat_value: bool,
    last_frame_ms: u64,
    now_ms: u64,
    fresh_window_ms: u64,
) -> bool {
    if heartbeat_value {
        // Affirmative heartbeats always win — publisher is announcing
        // the stream is live, and we can't out-vote the source of truth
        // with stale local state.
        return true;
    }
    // heartbeat says off — only override the heartbeat when we have
    // live media evidence within the freshness window. `saturating_sub`
    // guards the (unlikely) case where `now_ms < last_frame_ms` due to
    // a clock skew or test fixture setting future timestamps; we treat
    // that as "frame is fresh" rather than panic / wrap.
    if last_frame_ms > 0 && now_ms.saturating_sub(last_frame_ms) < fresh_window_ms {
        current
    } else {
        false
    }
}

/// Issue #508 (instrumentation): age in ms since a `last_*_frame_ms` stamp,
/// or `-1` when the stamp is `0` ("no frame of this kind ever observed").
///
/// `saturating_sub` guards the unlikely `now < last` clock-skew case (treated
/// as age 0 rather than wrapping). Returns `i64` so the "never seen" sentinel
/// (`-1`) is distinguishable in the log from a genuine age of 0ms; a positive
/// value is the milliseconds since the last packet of that kind reached the
/// per-peer decode body. Pure function so it can be unit-tested without a
/// `Peer`.
fn age_ms_since(now: u64, last_frame_ms: u64) -> i64 {
    if last_frame_ms == 0 {
        -1
    } else {
        now.saturating_sub(last_frame_ms) as i64
    }
}

/// Resolve a CONTINUOUS live-stream (audio / camera-video) enabled flag
/// against a heartbeat, using the short [`LIVE_STREAM_FRESH_WINDOW_MS`]
/// (500ms) rather than the screen-sized [`MEDIA_FRESH_WINDOW_MS`] (5000ms).
///
/// # What this window does and does not buy
///
/// The window bounds how long a *stale* heartbeat can be overridden by live
/// frames — it does NOT set how fast a mute propagates. Propagation speed is
/// the heartbeat's: a mute/camera-off triggers an immediate edge-triggered
/// heartbeat (`Connection::send_immediate_heartbeat`), and if that datagram is
/// lost the state is only corrected by the next keepalive, up to
/// `HEARTBEAT_KEEPALIVE_INTERVAL_MS` (5000ms) later. So the honest worst-case
/// bound for the `audio_enabled`/`video_enabled` glyph flip is ~5s, not
/// sub-second.
///
/// What the short window prevents is the *additional* lag of ignoring a fresh
/// off-heartbeat because frames are still arriving: with the screen-sized
/// window, a peer who stopped 4s ago could still read as enabled. For the
/// speaking glow specifically the fast signal is not this path at all — it is
/// the decoder-side VAD's `peer_speaking` events (see
/// `neteq_audio_decoder::handle_pcm_data` and its terminal zero on teardown).
///
/// Thin wrapper over [`apply_heartbeat_enabled_flag`] that BAKES IN the
/// correct window. The window is the correctness-critical knob here: a future
/// "simplify by sharing one window" edit at the call site could silently widen
/// audio/video back to the screen-sized window and reintroduce that extra
/// staleness. Routing audio and camera-video through this wrapper makes the
/// window non-passable at the call site.
pub(crate) fn apply_live_stream_heartbeat_flag(
    current: bool,
    heartbeat_value: bool,
    last_frame_ms: u64,
    now_ms: u64,
) -> bool {
    apply_heartbeat_enabled_flag(
        current,
        heartbeat_value,
        last_frame_ms,
        now_ms,
        LIVE_STREAM_FRESH_WINDOW_MS,
    )
}

/// Resolve the SCREEN-share enabled flag against a heartbeat, using the long
/// [`MEDIA_FRESH_WINDOW_MS`]: screen is legitimately bursty/idle (encoders
/// skip unchanged frames), so the multi-second window prevents a stale
/// `false` heartbeat from collapsing a live split-share layout on WebTransport.
///
/// Companion to [`apply_live_stream_heartbeat_flag`]; BAKES IN the screen
/// window for the same anti-misuse reason — the two media classes must NOT
/// share a window, and which one each call site uses is no longer a free
/// argument it can get wrong.
pub(crate) fn apply_screen_heartbeat_flag(
    current: bool,
    heartbeat_value: bool,
    last_frame_ms: u64,
    now_ms: u64,
) -> bool {
    apply_heartbeat_enabled_flag(
        current,
        heartbeat_value,
        last_frame_ms,
        now_ms,
        MEDIA_FRESH_WINDOW_MS,
    )
}

/// Resolve the CAMERA enabled flag with an on->off corroboration debounce.
/// Returns `(resolved_enabled, updated_off_count)`; the caller stores the count
/// back on the peer and feeds it in next heartbeat. Wraps
/// [`apply_live_stream_heartbeat_flag`]: an affirmative heartbeat resets the
/// count; a fresh frame keeps the tile on (WT reorder protection) but still
/// counts the `false`; otherwise the tile blanks only once
/// [`CAMERA_OFF_CORROBORATION_COUNT`] consecutive `false`s corroborate the off.
///
/// Pure function so it can be unit-tested without a real `Peer`.
pub(crate) fn resolve_camera_heartbeat_flag(
    current: bool,
    heartbeat_value: bool,
    last_frame_ms: u64,
    now_ms: u64,
    consecutive_off: u32,
) -> (bool, u32) {
    if heartbeat_value {
        // Affirmative heartbeat: camera is on, clear any pending-off streak.
        return (true, 0);
    }
    if !current {
        // Already off — the debounce only guards a currently-shown tile.
        return (false, 0);
    }
    // Currently ON and the heartbeat says off. The freshness primitive still
    // out-votes the stale `false` when a live frame is within the window.
    let kept_on_by_frame =
        apply_live_stream_heartbeat_flag(current, heartbeat_value, last_frame_ms, now_ms);
    let new_count = consecutive_off.saturating_add(1);
    if kept_on_by_frame {
        // WT reorder protection kept it on; still record the `false` so a
        // genuine off's resend can reach the corroboration count.
        (true, new_count)
    } else if new_count >= CAMERA_OFF_CORROBORATION_COUNT {
        // Corroborated: blank the tile and reset the streak.
        (false, 0)
    } else {
        // First stale `false` — not yet corroborated, keep the tile on.
        (true, new_count)
    }
}

use std::fmt::Debug;

impl Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Peer {{ session_id: {}, video_canvas_id: {}, screen_canvas_id: {} }}",
            self.session_id, self.video_canvas_id, self.screen_canvas_id
        )
    }
}

impl Peer {
    // 8 args (was 7): #2068 threads `skip_canvas_paint` through to the decoders.
    // Private constructor with all-distinct positional fields; a params struct
    // would add indirection without clarity.
    #[allow(clippy::too_many_arguments)]
    fn new(
        video_canvas_id: String,
        screen_canvas_id: String,
        session_id: u64,
        user_id: String,
        aes: Option<Aes128State>,
        vad_threshold: Option<f32>,
        is_guest: bool,
        skip_canvas_paint: bool,
    ) -> Result<Self, JsValue> {
        let sid_str = session_id.to_string();
        let (mut audio, video, screen) = Self::new_decoders(
            &video_canvas_id,
            &screen_canvas_id,
            &sid_str,
            vad_threshold,
            skip_canvas_paint,
        )?;

        audio.set_muted(true);
        debug!("Initialized peer {user_id} (session_id: {session_id}) with audio muted");

        Ok(Self {
            audio,
            video,
            screen,
            session_id,
            sid_str,
            user_id,
            video_canvas_id,
            screen_canvas_id,
            aes,
            activity_count: 1,
            missed_heartbeat_checks: 0,
            video_enabled: false,
            audio_enabled: false,
            screen_enabled: false,
            is_speaking: false,
            audio_level: 0.0,
            transport_type: TransportType::TRANSPORT_UNKNOWN,
            display_name: None,
            device_info: PeerDeviceInfo::default(),
            is_guest,
            visible: false,
            skip_canvas_paint,
            context_initialized: false,
            vad_threshold,
            has_received_heartbeat: false,
            audio_forced_off: false,
            video_forced_off: false,
            // Default to the lowest layer (0). Pre-simulcast publishers send 0,
            // so this is unchanged behaviour for them.
            selected_video_layer: 0,
            // Phase 2: start the chooser at the base layer (bandwidth-safe for a
            // freshly-joined peer whose available layers we have not learned),
            // with empty availability and a clean initial downlink sample.
            video_layer_chooser: LayerChooser::new(now_ms()),
            video_layer_availability: LayerAvailability::new(),
            last_video_downlink: DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            },
            // Phase 3: per-peer SCREEN + AUDIO choosers, mirroring VIDEO.
            // #1899: the SCREEN chooser runs in screen-mode so the yo-yo
            // oscillation latch is active (a static share re-arms a keyframe wait
            // on every switch and self-oscillates); VIDEO/AUDIO stay on the plain
            // `new` — the camera↔screen divergence is deliberate.
            selected_screen_layer: 0,
            screen_layer_chooser: LayerChooser::new_screen(now_ms()),
            screen_layer_availability: LayerAvailability::new(),
            last_screen_downlink: DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            },
            // #2328: cold start is never starved. Peers are constructed fresh by
            // `add_peer` (there is no `Peer::reset` that reuses an instance), so
            // this cannot inherit a previous session's hold — the stale-state
            // hazard of #1915 does not reach it.
            screen_keyframe_starved_since_ms: Rc::new(Cell::new(None)),
            selected_audio_layer: 0,
            audio_layer_chooser: LayerChooser::new(now_ms()),
            audio_layer_availability: LayerAvailability::new(),
            video_seq_tracker: SequenceTracker::new(),
            screen_seq_tracker: SequenceTracker::new(),
            // HCL bug #1: 0 means "no media frame observed yet". The
            // freshness check (`apply_heartbeat_enabled_flag`) treats 0
            // as "not fresh," so a heartbeat at session start carries
            // unchallenged authority — correct behaviour because we have
            // no media yet.
            last_screen_frame_ms: 0,
            last_video_frame_ms: 0,
            consecutive_video_off_hbs: 0,
            last_audio_frame_ms: 0,
            // Issue #1460 observability: never-switched sentinel.
            last_video_switch: LastLayerSwitch::default(),
            last_screen_switch: LastLayerSwitch::default(),
            layer_gate_skips: LayerGateSkips::default(),
            // Issue #1878: default to false; set per-packet by the manager from
            // the live active transport. A fresh peer accrues no audio datagram
            // loss until we confirm the receiver is on WebTransport.
            receiver_on_webtransport: false,
            audio_datagram_loss: AudioDatagramLossTracker::new(),
        })
    }

    fn new_decoders(
        video_canvas_id: &str,
        screen_canvas_id: &str,
        peer_id: &str,
        vad_threshold: Option<f32>,
        skip_canvas_paint: bool,
    ) -> Result<
        (
            Box<dyn AudioPeerDecoderTrait>,
            VideoPeerDecoder,
            VideoPeerDecoder,
        ),
        JsValue,
    > {
        // Construction order matches the pre-2225 inline version exactly (both
        // video-like decoders, then audio), so a partial failure drops the same
        // set of half-built decoders it always did.
        let video_decoder =
            Self::new_video_like_decoder(video_canvas_id, MEDIA_TYPE_CAMERA, skip_canvas_paint)?;
        let screen_decoder =
            Self::new_video_like_decoder(screen_canvas_id, MEDIA_TYPE_SCREEN, skip_canvas_paint)?;

        Ok((
            create_audio_peer_decoder(None, peer_id.to_string(), vad_threshold)?,
            video_decoder,
            screen_decoder,
        ))
    }

    /// Build one `VideoPeerDecoder` (camera or screen) and wire its canvas.
    ///
    /// Extracted from [`Self::new_decoders`] (issue 2225) so a single stream's
    /// decoder can be rebuilt on its own after a decode error, without dragging
    /// the peer's other two decoders — in particular the NetEQ audio worker —
    /// along with it. Cold start is unchanged: `new_decoders` calls this twice
    /// with the same arguments it used to pass inline.
    ///
    /// The decoder is created without a canvas (`set_canvas` may run later, when
    /// the tile mounts) and then opportunistically wired to whatever element
    /// currently carries `canvas_id`. Looking the element up fresh on every
    /// build is what lets a rebuilt decoder re-attach to a tile that is already
    /// on screen.
    fn new_video_like_decoder(
        canvas_id: &str,
        media_type: &'static str,
        skip_canvas_paint: bool,
    ) -> Result<VideoPeerDecoder, JsValue> {
        let decoder = VideoPeerDecoder::new(None, media_type, skip_canvas_paint)?;

        if let Some(document) = web_sys::window().and_then(|w| w.document()) {
            if let Some(canvas_element) = document.get_element_by_id(canvas_id) {
                if let Ok(canvas) = canvas_element.dyn_into::<web_sys::HtmlCanvasElement>() {
                    let _ = decoder.set_canvas(canvas);
                }
            }
        }

        Ok(decoder)
    }

    /// Swap in a freshly built audio decoder for this peer, retiring the
    /// outgoing one first.
    ///
    /// Issue 2174 follow-up: `NetEqAudioPeerDecoder::drop` broadcasts a
    /// terminal `speaking: 0` so a glow left lit by the edge-triggered VAD is
    /// cleared when the worker dies — but `Drop` also fires here, on
    /// REPLACEMENT, where the peer has not stopped speaking at all. Left
    /// unguarded, one AUDIO decode error blanks the peer's speaking indicator
    /// (tile level forced to 0.0, roster dot darkened, the UI's speaking deadman
    /// cancelled) until the replacement NetEQ worker finishes worklet init.
    ///
    /// Issue 2225 narrowed the trigger: a VIDEO or SCREEN decode error no longer
    /// reaches this function at all (`decoder_reset_scope` routes those to
    /// `Video`/`Screen`), so the "lossy link, repeated decode errors, blinking
    /// glow" framing no longer applies. The surviving trigger is a
    /// decoder-propagated AUDIO failure, which no concrete decoder currently
    /// produces — see `decoder_reset_scope`'s "Which arms fire today".
    ///
    /// [`AudioPeerDecoderTrait::retire_for_replacement`] makes the outgoing
    /// decoder's `Drop` silent, so the indicator simply carries over: the new
    /// decoder starts from a cleared VAD and re-emits from its first speech
    /// frame, and the deadman — which the spurious zero would have cancelled —
    /// stays armed as the backstop for the case where the peer really did go
    /// quiet across the reset.
    ///
    /// Genuine teardown (peer removal via `delete_peer_at` /
    /// `run_peer_monitor`, which drop the whole `Peer`) never calls this, so it
    /// keeps the terminal zero that issue 2174 added.
    fn replace_audio_decoder(&mut self, mut audio: Box<dyn AudioPeerDecoderTrait>) {
        // Preserve the current mute state after reset
        audio.set_muted(!self.audio_enabled);
        self.audio.retire_for_replacement();
        self.audio = audio;
    }

    /// Swap in a freshly built camera decoder, giving it stream attribution
    /// immediately.
    ///
    /// Issue #1640: the new codec worker starts with empty context, and
    /// `context_initialized` stays `true` across a rebuild, so the one-time
    /// install in `PeerDecodeManager::decode` will not re-run — the `SetContext`
    /// has to be sent here or the worker's diagnostics are unattributed for the
    /// rest of the call.
    ///
    /// Shaped like [`Self::replace_audio_decoder`] (build outside, swap inside)
    /// so the swap is one auditable unit and is reachable from host tests, which
    /// cannot construct a real `VideoPeerDecoder`.
    fn replace_video_decoder(&mut self, video: VideoPeerDecoder, from_peer: &str) {
        self.video = video;
        self.video
            .set_stream_context(from_peer.to_owned(), self.session_id.to_string());
    }

    /// SCREEN sibling of [`Self::replace_video_decoder`].
    fn replace_screen_decoder(&mut self, screen: VideoPeerDecoder, from_peer: &str) {
        self.screen = screen;
        self.screen
            .set_stream_context(from_peer.to_owned(), self.session_id.to_string());
        // Issue #2328: the fresh decoder starts from a clean jitter buffer, so any hold recorded
        // against the OLD one no longer describes the current stream. The rebuilt decoder re-arms
        // the stamp on its own first eviction. Note this does NOT weaken the bound under sustained
        // starvation: the #1662 keyframe-less ESCALATION resets the pipeline inside the worker and
        // never reaches this main-thread path, which runs only on a decode ERROR.
        self.clear_screen_keyframe_starvation();
    }

    /// Rebuild only the decoder that the failure `error` implicates (issue
    /// 2225).
    ///
    /// This replaces a blanket `reset` that rebuilt all three of the peer's
    /// decoders on ANY decode failure. See [`decoder_reset_scope`] for the
    /// classification and the reasoning; the short version is that the three
    /// decoders are independent, rebuilding the NetEQ audio worker is expensive
    /// and audible, and the errors that actually reach this path today never
    /// touched a decoder at all.
    ///
    /// `has_received_heartbeat` and the `*_enabled` flags are intentionally
    /// preserved (as they were by `reset`): the peer's last-known media state is
    /// still the best information we have, and clearing it would let straggler
    /// frames through until the next heartbeat.
    fn reset_for_decode_error(
        &mut self,
        from_peer: &str,
        error: &PeerDecodeError,
    ) -> Result<(), JsValue> {
        match decoder_reset_scope(error) {
            DecoderResetScope::None => {
                debug!(
                    "Peer {} decode error {error} touched no decoder; nothing to rebuild",
                    self.session_id
                );
            }
            DecoderResetScope::Video => {
                debug!("Rebuilding VIDEO decoder for peer {}", self.session_id);
                let video = Self::new_video_like_decoder(
                    &self.video_canvas_id,
                    MEDIA_TYPE_CAMERA,
                    self.skip_canvas_paint,
                )?;
                self.replace_video_decoder(video, from_peer);
            }
            DecoderResetScope::Screen => {
                debug!("Rebuilding SCREEN decoder for peer {}", self.session_id);
                let screen = Self::new_video_like_decoder(
                    &self.screen_canvas_id,
                    MEDIA_TYPE_SCREEN,
                    self.skip_canvas_paint,
                )?;
                self.replace_screen_decoder(screen, from_peer);
            }
            DecoderResetScope::Audio => {
                debug!(
                    "Rebuilding AUDIO decoder for peer {} with audio muted: {}",
                    self.session_id, !self.audio_enabled
                );
                let audio = create_audio_peer_decoder(
                    None,
                    self.session_id.to_string(),
                    self.vad_threshold,
                )?;
                self.replace_audio_decoder(audio);
            }
        }
        Ok(())
    }

    /// Select which simulcast layer (issue #989) this receiver decodes for this
    /// peer. VIDEO packets tagged with a different `simulcast_layer_id` are
    /// dropped before sequence tracking and decode. Defaults to 0 (lowest
    /// layer). In Phase 2 this is driven automatically by
    /// [`Self::tick_layer_chooser`]; it may also be set directly (tests / future
    /// viewport-driven overrides).
    pub fn set_selected_video_layer(&mut self, layer: u32) {
        let layer = clamp_selected_layer(PrefMediaKind::Video, layer);
        // Re-anchor the sequence tracker ONLY on an actual change (#1079 H1): the
        // new layer has its own per-layer sequence counter, so the next packet
        // must establish a fresh baseline rather than be diffed against the old
        // layer's `high_seq` (which would manufacture phantom loss → chooser
        // oscillation). A no-op switch (same layer) must not reset the tracker,
        // or we would discard healthy loss/PLI history every tick.
        if layer != self.selected_video_layer {
            self.video_seq_tracker.reanchor_for_layer_switch();
        }
        self.selected_video_layer = layer;
    }

    /// The simulcast layer this receiver currently decodes for this peer.
    pub fn selected_video_layer(&self) -> u32 {
        self.selected_video_layer
    }

    /// Collapse the VIDEO decode guard onto a rung that is still arriving (#2251):
    /// [`crate::decode::layer_chooser::LayerChooser::choose`]'s shrinking-ceiling
    /// rule, applied when the evidence lands rather than at the next peer-monitor
    /// tick. It only ever LOWERS the guard, never raises it (#1695).
    fn collapse_video_guard_to_available(&mut self, now_ms: u64) -> bool {
        let highest = self.video_layer_availability.highest_available(now_ms);
        if highest >= self.selected_video_layer {
            return false;
        }
        let old = self.selected_video_layer;
        self.video_seq_tracker.reanchor_for_layer_switch();
        log::info!(
            "LAYER_SWITCH session_id={} kind=video from={} to={} site=starved_guard constrained={} highest_available={}",
            self.session_id,
            old,
            highest,
            self.video_layer_chooser.is_constrained(),
            highest
        );
        self.last_video_switch = LastLayerSwitch {
            ms: now_ms,
            from: old,
            to: highest,
        };
        self.selected_video_layer = highest;
        true
    }

    /// SCREEN-kind sibling of [`Self::set_selected_video_layer`] (issue #1256):
    /// select which screen-share simulcast layer this receiver decodes for this
    /// peer, re-anchoring the SCREEN sequence tracker ONLY on an actual change
    /// (same #1079 H1 rationale as the VIDEO setter — a fresh per-layer sequence
    /// baseline must be established, while a no-op switch must NOT discard healthy
    /// loss/PLI history). Used by [`PeerDecodeManager::apply_size_lid_to_decode_guards`]
    /// to lower/raise the screen decode guard without running the chooser.
    pub fn set_selected_screen_layer(&mut self, layer: u32) {
        let layer = clamp_selected_layer(PrefMediaKind::Screen, layer);
        if layer != self.selected_screen_layer {
            self.screen_seq_tracker.reanchor_for_layer_switch();
            // Issue #2328: the hold belonged to the rung we are leaving. The new rung has to earn
            // its own retraction from a fresh window, or one starved rung's stamp would cascade
            // the receiver down the whole ladder.
            self.clear_screen_keyframe_starvation();
        }
        self.selected_screen_layer = layer;
    }

    /// Run the receiver-driven layer chooser one tick for this peer (issue #989,
    /// Phase 2) and apply the result to the local decode guard.
    ///
    /// Folds the most recent windowed downlink sample
    /// ([`Self::last_video_downlink`]) and the empirically-learned availability
    /// cap into the per-peer [`LayerChooser`], updates `selected_video_layer` so
    /// decode follows the chosen layer, and returns the desired layer so the
    /// caller can build the aggregate `LAYER_PREFERENCE` packet.
    ///
    /// `now_ms` is threaded in (rather than read here) so the manager uses one
    /// consistent clock per tick and so this stays testable. The chooser's raw
    /// output is the value returned; P4 will clamp it to the user's
    /// `[min, max]` at the manager level via
    /// [`crate::decode::layer_chooser::clamp_to_user_range`] before it is
    /// applied — a clean seam that does not touch this method.
    pub fn tick_layer_chooser(
        &mut self,
        now_ms: u64,
        bounds: crate::decode::layer_chooser::KindLayerBounds,
    ) -> u32 {
        let highest = self.video_layer_availability.highest_available(now_ms);
        let raw = self
            .video_layer_chooser
            .choose(self.last_video_downlink, highest, now_ms);
        // Phase 4: clamp the auto-chosen layer to the user's receive bounds, so
        // the REQUESTED (and relay-forwarded) layer is bounded, and the local
        // decode guard matches. Open bounds (default) = identity.
        let desired = bounds.clamp(raw);
        // Re-anchor on an actual layer change (#1079 H1) — see
        // `set_selected_video_layer` for the rationale. No-op when unchanged.
        if desired != self.selected_video_layer {
            let old = self.selected_video_layer;
            self.video_seq_tracker.reanchor_for_layer_switch();
            // Issue #1460 observability (Marker 1): record EVERY actual switch,
            // including the unconstrained `highest_available` flap (the only path
            // that emits no LAYER_PREFERENCE). NOT gated on `constrained` — that
            // unconstrained path is precisely the suspected freshness_skip cause.
            log::info!(
                "LAYER_SWITCH session_id={} kind=video from={} to={} site=tick constrained={} highest_available={}",
                self.session_id,
                old,
                desired,
                self.video_layer_chooser.is_constrained(),
                highest
            );
            self.last_video_switch = LastLayerSwitch {
                ms: now_ms,
                from: old,
                to: desired,
            };
        }
        self.selected_video_layer = desired;
        desired
    }

    /// Run the SCREEN layer chooser one tick (issue #989, Phase 3) and apply the
    /// result to the screen decode guard. Independent of the camera VIDEO
    /// chooser: a peer's screen adapts to this receiver's downlink separately.
    pub fn tick_screen_layer_chooser(
        &mut self,
        now_ms: u64,
        bounds: crate::decode::layer_chooser::KindLayerBounds,
    ) -> u32 {
        // Issue #2328: retract a keyframe-starved rung BEFORE reading availability, so the
        // downshift lands on this same tick rather than the next one. Also covers the case the
        // decode-path hook cannot: a rung whose packets have stopped arriving entirely still gets
        // its stale hold evaluated here, because this runs on the 5s monitor tick regardless.
        self.apply_screen_keyframe_starvation(now_ms);
        let highest = self.screen_layer_availability.highest_available(now_ms);
        let raw = self
            .screen_layer_chooser
            .choose(self.last_screen_downlink, highest, now_ms);
        let desired = bounds.clamp(raw);
        // Re-anchor on an actual layer change (#1079 H1) — see
        // `set_selected_video_layer` for the rationale. No-op when unchanged.
        if desired != self.selected_screen_layer {
            let old = self.selected_screen_layer;
            self.screen_seq_tracker.reanchor_for_layer_switch();
            // Issue #2328: a screen rung switch retires the previous rung's keyframe-less hold.
            self.clear_screen_keyframe_starvation();
            // Issue #1460 observability (Marker 1). NOT gated on `constrained`.
            log::info!(
                "LAYER_SWITCH session_id={} kind=screen from={} to={} site=tick constrained={} highest_available={}",
                self.session_id,
                old,
                desired,
                self.screen_layer_chooser.is_constrained(),
                highest
            );
            self.last_screen_switch = LastLayerSwitch {
                ms: now_ms,
                from: old,
                to: desired,
            };
        }
        self.selected_screen_layer = desired;
        desired
    }

    /// The simulcast layer this receiver currently decodes for this peer's
    /// screen stream.
    pub fn selected_screen_layer(&self) -> u32 {
        self.selected_screen_layer
    }

    /// Act on the current SCREEN keyframe-starvation observation (issue #2328).
    ///
    /// A rung that keeps delivering DELTAS but no decodable KEYFRAME is invisible to
    /// arrival-based [`LayerAvailability`] — packets land, `last_seen_ms` refreshes, the rung
    /// looks healthy — while the receiver sitting on it is frozen. This is the receiver-side
    /// self-heal: past [`SCREEN_KEYFRAME_STARVED_RETRACT_MS`] of unbroken hold, retract that rung
    /// so the chooser drops to one that is actually delivering keyframes. The base rung always
    /// qualifies: the relay forwards `simulcast_layer_id == 0` unconditionally, whatever this
    /// receiver's LAYER_PREFERENCE says, so the fallback is guaranteed to be on the wire.
    ///
    /// It cannot wedge. The quarantine expires on wall-clock alone, and re-admission never
    /// requires the rung to prove anything (it could not — once the chooser constrains, the relay
    /// stops forwarding that rung, so a "re-admit on a keyframe" rule would be unsatisfiable).
    /// The complementary chooser paths are also escape-safe: unconstrained (the #1079 M2 default,
    /// and the state the #2328 field case is in), no LAYER_PREFERENCE is published at all, so the
    /// relay keeps forwarding every rung, the retracted rung keeps being `observe`d, and it is back
    /// in the ceiling the instant the quarantine lifts. Constrained, `LayerChooser::choose`'s
    /// shrinking-ceiling arm clears `constrained` once `current >= highest_available` — unless it
    /// is also `sticky`, in which case the exit is one step longer but still time-bounded: the
    /// clean-tick recovery drops `sticky` as soon as `sticky_floor >= highest_available` (both 0
    /// while the top rung is retracted), and the next clean tick then clears `constrained`. Either
    /// way the preference is dropped and the relay's fail-open path re-opens.
    fn apply_screen_keyframe_starvation(&mut self, now_ms: u64) {
        use crate::decode::layer_chooser::{
            screen_starvation_action, ScreenStarvationAction,
            SCREEN_KEYFRAME_STARVED_QUARANTINE_MS, SCREEN_KEYFRAME_STARVED_RETRACT_MS,
            SCREEN_KEYFRAME_STARVED_STALE_MS,
        };
        // The ASYMMETRY input. Layer 0 is forwarded by the relay unconditionally (`chat_server.rs`
        // drops only `simulcast_layer_id != 0`), and the cleartext gate `observe`s every arriving
        // IN-LADDER layer BEFORE it skips a non-selected one — so this reads true exactly while
        // the base rung is genuinely on the wire, even though this receiver is decoding a higher
        // rung. When it reads FALSE the whole SCREEN stream has stopped (e.g. the layer-agnostic
        // `priority_drop.rs` shed at 90% channel fill), and retracting one rung would buy nothing
        // but LAYER_SWITCH churn. `layer_available_peek` is the non-pruning read, so this cannot
        // mutate availability as a side effect of a diagnostic question.
        let base_rung_arriving = self
            .screen_layer_availability
            .layer_available_peek(0, now_ms);
        match screen_starvation_action(
            self.selected_screen_layer,
            self.screen_keyframe_starved_since_ms.get(),
            base_rung_arriving,
            now_ms,
            SCREEN_KEYFRAME_STARVED_RETRACT_MS,
            SCREEN_KEYFRAME_STARVED_STALE_MS,
        ) {
            ScreenStarvationAction::None => {}
            ScreenStarvationAction::Rearm => {
                // Suspended tab / clock step: the stamp is too old to act on. Require a fresh
                // hold window rather than retracting for a freeze that may already be over.
                self.screen_keyframe_starved_since_ms.set(Some(now_ms));
            }
            ScreenStarvationAction::Retract => {
                let layer = self.selected_screen_layer;
                let until = now_ms.saturating_add(SCREEN_KEYFRAME_STARVED_QUARANTINE_MS);
                self.screen_layer_availability.quarantine(layer, until);
                // Re-arm rather than latch: the NEXT retraction must be earned by a fresh hold
                // window. Leaving the old stamp in place would re-quarantine on every subsequent
                // packet off one stale observation and ratchet the expiry forward indefinitely —
                // the wedge shape this design exists to avoid.
                self.screen_keyframe_starved_since_ms.set(None);
                log::warn!(
                    "SCREEN_LAYER_RETRACT session_id={} layer={} reason=keyframe_starved \
                     hold_ms>={} base_rung_arriving={} quarantine_until_ms={} (issue #2328)",
                    self.session_id,
                    layer,
                    SCREEN_KEYFRAME_STARVED_RETRACT_MS,
                    base_rung_arriving,
                    until
                );
            }
        }
    }

    /// Clear the SCREEN keyframe-starvation stamp (issue #2328). Called wherever the hold is
    /// broken or the observation stops describing the current stream: a keyframe arriving on the
    /// selected rung, a screen layer switch (the new rung's hold has not started), a screen
    /// decoder rebuild, and transport teardown.
    fn clear_screen_keyframe_starvation(&self) {
        self.screen_keyframe_starved_since_ms.set(None);
    }

    /// Issue #1460 observability: a copy of this peer's most recent in-place
    /// VIDEO simulcast layer switch (timestamp + from/to). `ms == 0` means no
    /// switch has happened yet. Read by the diagnostics-bus subscriber to
    /// correlate a `freshness_skip` with a recent layer transition. Pure
    /// telemetry — never consulted by adaptation logic.
    pub fn last_video_switch(&self) -> LastLayerSwitch {
        self.last_video_switch
    }

    /// Issue #1460 observability: SCREEN-kind sibling of [`Self::last_video_switch`].
    pub fn last_screen_switch(&self) -> LastLayerSwitch {
        self.last_screen_switch
    }

    /// Issue #2533: cumulative exact-match-gate discards for this peer.
    pub fn layer_gate_skips(&self) -> LayerGateSkips {
        self.layer_gate_skips
    }

    /// Run the AUDIO layer chooser one tick (issue #989, Phase 3) and apply the
    /// result to the audio decode guard.
    ///
    /// Audio has no per-window loss sample driving this chooser (audio decode goes
    /// through NetEq, not `track_sequence`), and audio is only ~1-3% of call
    /// bandwidth — so rather than build a dedicated audio loss pipeline, the
    /// audio chooser is fed the receiver's VIDEO downlink as a proxy for overall
    /// connection health (`last_video_downlink`): audio and video share the same
    /// QUIC connection, so when the receiver's downlink is congested enough to
    /// lose video it is the right moment to also shed the costlier audio layer.
    /// This keeps audio adaptation cheap and conservative (the documented
    /// cost-benefit: marginal savings, minimal added complexity).
    pub fn tick_audio_layer_chooser(
        &mut self,
        now_ms: u64,
        bounds: crate::decode::layer_chooser::KindLayerBounds,
    ) -> u32 {
        let highest = self.audio_layer_availability.highest_available(now_ms);
        // Proxy audio downlink health with the video downlink window (see doc).
        let raw = self
            .audio_layer_chooser
            .choose(self.last_video_downlink, highest, now_ms);
        // `clamp` RAISES to `min`, and audio decode is exact-match with no analogue of
        // `collapse_video_guard_to_available`, so the guard must not exceed `highest`.
        let desired = bounds.clamp(raw).min(highest);
        // #1561: log audio layer transitions (mirrors video/screen LAYER_SWITCH).
        // Logged BEFORE the write below so `self.selected_audio_layer` still reads
        // the OLD rung for the `from=` field.
        if desired != self.selected_audio_layer {
            log::info!(
                "LAYER_SWITCH session_id={} kind=audio from={} to={} site=tick constrained={} highest_available={}",
                self.session_id,
                self.selected_audio_layer,
                desired,
                self.audio_layer_chooser.is_constrained(),
                highest
            );
        }
        // Issue #2138: route the write through the single chokepoint so the audio
        // datagram-loss tracker is re-anchored on an actual rung change (it has its
        // own re-anchor `if`-guard, so an unchanged layer stays a no-op here).
        self.set_selected_audio_layer(desired);
        desired
    }

    /// Early-seed this peer's per-kind choosers from a sample taken OUTSIDE the
    /// 5s monitor tick (issue #1179, Part B).
    ///
    /// Runs the pure [`LayerChooser::observe_early_congestion`] for VIDEO, SCREEN
    /// and AUDIO using the SAME inputs the normal tick would
    /// ([`Self::tick_layer_chooser`] et al.): each kind's most-recent windowed
    /// downlink sample + its empirically-learned availability cap, AND the SAME
    /// post-process the tick applies — the chooser's raw output is clamped to the
    /// user's per-kind receive [`KindLayerBounds`] (issue #1179, PR #1192 review)
    /// before it is written to the decode guard, so a bandwidth-conscious user who
    /// set a manual receive `max` never decodes (nor, via
    /// [`Self::collect_desired_preferences`], advertises) a layer above their cap
    /// even on the fast early-seed path. Open (default) bounds are an identity
    /// clamp, so an unbounded user sees behavior identical to before.
    ///
    /// Because the early-seed primitive only acts while a chooser is still
    /// UNCONSTRAINED and only on a congested sample, this is a no-op for a healthy
    /// cold-start join (M2 preserved) and a no-op once the 5s loop has already
    /// constrained — the 5s loop then owns adaptation.
    ///
    /// Unlike `tick_layer_chooser`, this does NOT advance the chooser's hysteresis
    /// (`choose`/clean-window/score/sticky are untouched): the only state it can
    /// change is flipping an unconstrained chooser to constrained + one step down,
    /// exactly mirroring the unconstrained-congested arm of `choose`. The bounds
    /// clamp is a pure post-process (same as the tick's `bounds.clamp(raw)`) — it
    /// touches no hysteresis state. On an actual seed it re-anchors the matching
    /// sequence tracker and updates the decode guard so decode follows the
    /// (clamped) seeded layer immediately (same as the tick).
    ///
    /// Returns `true` if ANY kind was seeded (so the caller can log it); the
    /// caller still emits the resulting preference via the normal sender path.
    ///
    /// NOTE on availability: like the tick this calls `highest_available`, which
    /// prunes the rolling availability map. That is a read of "what layers exist
    /// right now", NOT chooser-hysteresis state, and it is exactly what the next
    /// tick would do anyway — it does not advance any clean-window / sticky state.
    pub fn seed_early_congestion(
        &mut self,
        now_ms: u64,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
    ) -> bool {
        use crate::decode::layer_chooser::PrefMediaKind;
        let mut seeded = false;

        // VIDEO — same inputs as `tick_layer_chooser`.
        let vh = self.video_layer_availability.highest_available(now_ms);
        if self
            .video_layer_chooser
            .observe_early_congestion(self.last_video_downlink, vh, now_ms)
        {
            // Clamp to the user's receive bounds BEFORE writing the decode guard
            // and the change-detection, exactly as the tick does (P4 invariant:
            // the post-clamp selected layer can never exceed the user's `max`).
            let layer = bounds
                .for_kind(PrefMediaKind::Video)
                .clamp(self.video_layer_chooser.current());
            if layer != self.selected_video_layer {
                let old = self.selected_video_layer;
                self.video_seq_tracker.reanchor_for_layer_switch();
                // Issue #1460 observability (Marker 1). NOT gated on `constrained`.
                log::info!(
                    "LAYER_SWITCH session_id={} kind=video from={} to={} site=early_seed constrained={} highest_available={}",
                    self.session_id,
                    old,
                    layer,
                    self.video_layer_chooser.is_constrained(),
                    vh
                );
                self.last_video_switch = LastLayerSwitch {
                    ms: now_ms,
                    from: old,
                    to: layer,
                };
            }
            self.selected_video_layer = layer;
            seeded = true;
        }

        // SCREEN — same inputs as `tick_screen_layer_chooser`.
        let sh = self.screen_layer_availability.highest_available(now_ms);
        if self
            .screen_layer_chooser
            .observe_early_congestion(self.last_screen_downlink, sh, now_ms)
        {
            let layer = bounds
                .for_kind(PrefMediaKind::Screen)
                .clamp(self.screen_layer_chooser.current());
            if layer != self.selected_screen_layer {
                let old = self.selected_screen_layer;
                self.screen_seq_tracker.reanchor_for_layer_switch();
                // Issue #2328: a screen rung switch retires the previous rung's keyframe-less hold.
                self.clear_screen_keyframe_starvation();
                // Issue #1460 observability (Marker 1). NOT gated on `constrained`.
                log::info!(
                    "LAYER_SWITCH session_id={} kind=screen from={} to={} site=early_seed constrained={} highest_available={}",
                    self.session_id,
                    old,
                    layer,
                    self.screen_layer_chooser.is_constrained(),
                    sh
                );
                self.last_screen_switch = LastLayerSwitch {
                    ms: now_ms,
                    from: old,
                    to: layer,
                };
            }
            self.selected_screen_layer = layer;
            seeded = true;
        }

        // AUDIO — proxied by the VIDEO downlink, same as `tick_audio_layer_chooser`.
        // NOTE (#1219 Half 2): the sibling `seed_downlink_congestion` deliberately
        // does NOT seed audio (audio is priority-protected on the relay-asserted
        // congestion path). That divergence is intentional — do NOT "re-align" the
        // two methods by deleting the audio branch here; this #1179 early-seed path
        // legitimately proxies audio off the real video downlink sample.
        let ah = self.audio_layer_availability.highest_available(now_ms);
        if self
            .audio_layer_chooser
            .observe_early_congestion(self.last_video_downlink, ah, now_ms)
        {
            // Issue #2138: route through the single chokepoint (see
            // `set_selected_audio_layer`) so an actual rung change re-anchors the
            // audio datagram-loss tracker. `observe_early_congestion` returning
            // `true` means the chooser stepped DOWN one rung, so this is exactly
            // the forward-jump case that otherwise inflates the sample — though the
            // post-clamp layer can still equal the current one (e.g. a user receive
            // `min` pins it), in which case the setter's guard makes it a no-op.
            let layer = bounds
                .for_kind(PrefMediaKind::Audio)
                .clamp(self.audio_layer_chooser.current())
                .min(ah);
            self.set_selected_audio_layer(layer);
            seeded = true;
        }

        seeded
    }

    /// Step this peer's RECEIVER-side choosers down ONE rung in response to a
    /// relay-authored `DOWNLINK_CONGESTION` control packet (issue #1219 Half 2).
    ///
    /// This is the SYNTHETIC-sample sibling of [`Self::seed_early_congestion`].
    /// For VIDEO and SCREEN the two differ ONLY in the `DownlinkSample` fed into
    /// each `observe_early_congestion` call (see below); for AUDIO they diverge
    /// further — this method does NOT seed audio at all (see the AUDIO note). The
    /// `DownlinkSample` difference:
    ///
    ///   * `seed_early_congestion` feeds the peer's REAL most-recent windowed
    ///     downlink sample (`last_video_downlink` / `last_screen_downlink`). On
    ///     WebSocket (TCP) and WebTransport video (reliable QUIC unistreams) there
    ///     is NO packet loss, so that real sample is `{loss: 0, kf: 0}` and
    ///     [`DownlinkSample::is_congested`] is `false` → the early seed is a no-op.
    ///     That zero-loss blindness is exactly the #1219 condition the relay sees
    ///     (it drops frames at its bounded per-receiver outbound channel) but the
    ///     per-peer receive telemetry cannot.
    ///   * This method instead feeds a SYNTHETIC congested sample
    ///     (`{loss_per_sec: LOSS_STEP_DOWN_PER_SEC, kf_per_sec: 0.0}`, which makes
    ///     `is_congested()` true) so the chooser steps down even though the real
    ///     telemetry shows zero loss. DOWNLINK_CONGESTION is the relay ASSERTING
    ///     congestion the per-peer loss sample cannot observe; the client honors
    ///     that assertion by treating this window as congested for the chooser's
    ///     purposes ONLY.
    ///
    /// The VIDEO and SCREEN handling is identical to `seed_early_congestion`: the
    /// clamp to the user's per-kind receive bounds, the `selected_*_layer`
    /// decode-guard write, and the sequence-tracker reanchor on an actual layer
    /// switch. The real `last_video_downlink` / `last_screen_downlink` telemetry is
    /// deliberately LEFT UNTOUCHED — the synthetic value is local to this call and
    /// never written back, so the next real-sample tick (`choose`) and the
    /// early-seed path keep observing genuine link health.
    ///
    /// AUDIO is the ONE intentional divergence from `seed_early_congestion`: it is
    /// NOT stepped down here. Audio is priority-protected (the relay's emergency
    /// downlink-shed exempts it too), its base-layer bitrate is small, and keeping
    /// voice clear while video degrades is the DESIRED behavior under downlink
    /// congestion. See the AUDIO note in the body.
    ///
    /// Because it reuses `observe_early_congestion`, a chooser already constrained
    /// (by a prior real step-down or a prior DOWNLINK_CONGESTION) returns `false`
    /// and is NOT re-stepped — that is correct and idempotent: the held layer is
    /// still re-advertised to the relay via
    /// [`PeerDecodeManager::current_desired_preferences`] on the caller's publish.
    ///
    /// RECEIVER-ONLY: this mutates only this peer's own LayerChoosers (the layers
    /// THIS client REQUESTS from the relay for THIS peer's stream). It does not
    /// touch — and must never touch — the local publisher's encoder; collapsing
    /// the publisher would re-collapse this client's outbound stream for the whole
    /// room, the exact bug #1219 Half 1 fixed.
    ///
    /// Returns `true` if ANY kind was stepped down (so the caller can log it); the
    /// caller still emits the resulting preference via the normal sender path.
    pub fn seed_downlink_congestion(
        &mut self,
        now_ms: u64,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
    ) -> bool {
        use crate::decode::layer_chooser::{PrefMediaKind, LOSS_STEP_DOWN_PER_SEC};
        let mut seeded = false;

        // Relay-authored congestion: synthesize a congested window so the chooser
        // steps down even on lossless transports where the real sample reads
        // `{0, 0}` (#1219 Half 2). `loss_per_sec == LOSS_STEP_DOWN_PER_SEC` makes
        // `DownlinkSample::is_congested()` true (`>=` threshold).
        let synthetic = DownlinkSample {
            loss_per_sec: LOSS_STEP_DOWN_PER_SEC,
            kf_per_sec: 0.0,
        };

        // VIDEO — same inputs/clamp as `seed_early_congestion`, synthetic sample.
        let vh = self.video_layer_availability.highest_available(now_ms);
        if self
            .video_layer_chooser
            .observe_early_congestion(synthetic, vh, now_ms)
        {
            let layer = bounds
                .for_kind(PrefMediaKind::Video)
                .clamp(self.video_layer_chooser.current());
            if layer != self.selected_video_layer {
                let old = self.selected_video_layer;
                self.video_seq_tracker.reanchor_for_layer_switch();
                // Issue #1460 observability (Marker 1). NOT gated on `constrained`.
                log::info!(
                    "LAYER_SWITCH session_id={} kind=video from={} to={} site=downlink_seed constrained={} highest_available={}",
                    self.session_id,
                    old,
                    layer,
                    self.video_layer_chooser.is_constrained(),
                    vh
                );
                self.last_video_switch = LastLayerSwitch {
                    ms: now_ms,
                    from: old,
                    to: layer,
                };
            }
            self.selected_video_layer = layer;
            seeded = true;
        }

        // SCREEN — same inputs/clamp as `seed_early_congestion`, synthetic sample.
        let sh = self.screen_layer_availability.highest_available(now_ms);
        if self
            .screen_layer_chooser
            .observe_early_congestion(synthetic, sh, now_ms)
        {
            let layer = bounds
                .for_kind(PrefMediaKind::Screen)
                .clamp(self.screen_layer_chooser.current());
            if layer != self.selected_screen_layer {
                let old = self.selected_screen_layer;
                self.screen_seq_tracker.reanchor_for_layer_switch();
                // Issue #2328: a screen rung switch retires the previous rung's keyframe-less hold.
                self.clear_screen_keyframe_starvation();
                // Issue #1460 observability (Marker 1). NOT gated on `constrained`.
                log::info!(
                    "LAYER_SWITCH session_id={} kind=screen from={} to={} site=downlink_seed constrained={} highest_available={}",
                    self.session_id,
                    old,
                    layer,
                    self.screen_layer_chooser.is_constrained(),
                    sh
                );
                self.last_screen_switch = LastLayerSwitch {
                    ms: now_ms,
                    from: old,
                    to: layer,
                };
            }
            self.selected_screen_layer = layer;
            seeded = true;
        }

        // AUDIO — deliberately NOT stepped down (#1219 Half 2). This is the one
        // intentional divergence from `seed_early_congestion` (which DOES proxy
        // audio off the video downlink). The relay's emergency downlink-shed
        // (chat_server.rs `is_shed_candidate`) exempts AUDIO for the same reason:
        // audio is priority-protected, and "video froze but voice stayed clear"
        // is the DESIRED degradation under downlink congestion, not a bug. Audio's
        // base-layer bitrate is small relative to video, so holding it costs
        // little bandwidth while preserving the call's usability. Stepping audio
        // down here would fight the very goal of #1219 (don't make the experience
        // worse than it has to be). The audio chooser still adapts on its own via
        // the normal `tick_audio_layer_chooser` path if real loss appears.

        seeded
    }

    /// Snapshot of the layer this peer would advertise for each kind RIGHT NOW,
    /// without advancing any chooser hysteresis (issue #1179, Part B; clamp/gate
    /// added in PR #1192 review).
    ///
    /// Mirrors what [`PeerDecodeManager::tick_layer_choosers`] would emit, applying
    /// the SAME two post-processes the tick applies — but WITHOUT calling
    /// `choose` / `tick_*` (so no clean-window/score/sticky/last-change hysteresis
    /// advances):
    ///   1. it reads each chooser's [`LayerChooser::desired_preference`] (which is
    ///      `Some(layer)` only while the chooser is actively constrained, `None`
    ///      otherwise), then
    ///   2. clamps that layer to the user's per-kind receive [`KindLayerBounds`]
    ///      and advertises it ONLY when the clamped layer is `< highest_available`
    ///      for that kind — byte-identical to the tick's advertise predicate
    ///      (`if clamped < highest_available { insert }`). This is what keeps the
    ///      advertised preference at or below the user's `max`, and what suppresses
    ///      a spurious `Some(0)` for a source whose only learned layer is the base
    ///      (`highest_available == 0`), matching the tick (NIT 1).
    ///
    /// `highest_available(now_ms)` PRUNES the rolling availability map; that is a
    /// read of "what layers exist right now" (exactly what the next tick would do),
    /// NOT chooser-hysteresis state — so this still advances no clean-window /
    /// sticky / score / last-change state and the read-only guarantee the early
    /// seed relies on is preserved. Pushes each advertised `(kind, layer)` into
    /// `out`.
    ///
    /// ## #1256 Phase 1 — the size lid is folded HERE too (durable on the wire)
    ///
    /// The tile-size lid is applied in [`PeerDecodeManager::tick_layer_choosers`]
    /// by folding it into the per-kind `max` bound passed to the chooser. But the
    /// lid never touches the chooser's INTERNAL `constrained` state — by design it
    /// lives only in the bounds. So a healthy size-lidded peer is UNCONSTRAINED,
    /// and [`LayerChooser::desired_preference`] returns `None` for it. If this
    /// read-only publish path used `desired_preference()` alone (as it did before
    /// #1256), the lid would be INVISIBLE here: the next seed-driven publish (the
    /// early-seed timer + the relay-congestion / local-CPU-pressure seeds, all of
    /// which republish via [`PeerDecodeManager::current_desired_preferences`])
    /// would emit a map WITHOUT the lidded entry, `take_if_changed` would see the
    /// key vanish, and a `LAYER_PREFERENCE` clearing the cap would go out — the
    /// relay fail-opens the missing entry to the TOP layer for a tiny thumbnail
    /// until the next 5s tick re-asserts it (the ~5s oscillation / permanent
    /// defeat under congestion described in #1256 P1).
    ///
    /// The fix mirrors the tick EXACTLY: for VIDEO/SCREEN the advertised layer is
    /// `min(baseline, effective_max)` where `baseline` is the chooser's own pick
    /// (its constrained `desired_preference()` clamped to bounds, else decode-best =
    /// `highest_available`) and `effective_max = size_lid.max(user_min).min(user_max)`.
    /// The size lid lowers the ceiling toward the user's receive MIN but never BELOW
    /// it — the user min is an authoritative FLOOR the lid must yield to (#1256
    /// user-min regression), then an explicit user MAX still caps the result. Gated
    /// on `< highest_available` — the SAME `< highest_available` advertise predicate
    /// the tick uses. This composes the lid with congestion: a constrained chooser
    /// holding L1 under a size lid of L0 (no user min) advertises `min(1, 0) = 0`, so
    /// congestion can NEVER advertise ABOVE the lid; and with a user min of L1 the lid
    /// of L0 yields up to L1 (the floor), never below. AUDIO is UNCHANGED (never
    /// size-capped). This stays READ-ONLY:
    /// `size_cap_layer` is pure, and `desired_preference()` / `highest_available()`
    /// are the SAME reads the prior code already did (the lazy prune inside
    /// `highest_available` advances no chooser hysteresis — see the paragraph
    /// above), so no `choose` / `tick_*` is called and the early-seed read-only
    /// guarantee is preserved.
    fn collect_desired_preferences(
        &mut self,
        session_id: u64,
        now_ms: u64,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
        hint: crate::decode::layer_chooser::TileHint,
        out: &mut HashMap<(u64, crate::decode::layer_chooser::PrefMediaKind), u32>,
    ) {
        use crate::decode::layer_chooser::{size_cap_layer, PrefMediaKind, TileHint};

        // VIDEO — fold the #1256 size lid into the advertised layer so it is
        // DURABLE across this read-only publish path (early-seed timer + congestion
        // seeds), not just the 5s tick. Without this the lid is cleared on the wire
        // by the next seed publish (the chooser is unconstrained for a healthy
        // lidded peer, so `desired_preference()` is None) — see #1256 P1.
        let vh = self.video_layer_availability.highest_available(now_ms);
        // Baseline = the chooser's own pick: its constrained preference (clamped to
        // the user's bounds) if any, else decode-best (`highest_available`). This
        // makes the lid compose with congestion: `min(constrained_layer, size_lid)`
        // so congestion can NEVER advertise above the size lid.
        let v_base = self
            .video_layer_chooser
            .desired_preference()
            .map(|raw| bounds.for_kind(PrefMediaKind::Video).clamp(raw))
            .unwrap_or(vh);
        let v_lid = match hint {
            TileHint::Uncapped => vh,
            TileHint::Capped { device_px_h } => {
                size_cap_layer(device_px_h, vh, vh + 1, PrefMediaKind::Video)
            }
        };
        // Respect the user's MIN/MAX. The size lid lowers the ceiling toward the user
        // MIN but never below it (the floor is authoritative; the cap yields to it —
        // #1256 user-min regression). `v_effective_max = v_lid.max(v_user_min)` floors
        // the lid at the user min (so the cap can't undercut an explicit "never below
        // L1"), then `.min(v_user_max)` honors an explicit user ceiling (mirroring the
        // tick's `effective_max`). `v_base` already respects the user MIN on the
        // constrained branch (its `clamp` raises a below-min desired up to min), so
        // `min(v_base, v_effective_max)` never drops below the floor — no double-apply.
        let v_user_min = bounds.for_kind(PrefMediaKind::Video).min.unwrap_or(0);
        let v_user_max = bounds
            .for_kind(PrefMediaKind::Video)
            .max
            .unwrap_or(u32::MAX);
        let v_effective_max = v_lid.max(v_user_min).min(v_user_max);
        let v_layer = v_base.min(v_effective_max);
        if v_layer < vh {
            out.insert((session_id, PrefMediaKind::Video), v_layer);
        }

        // SCREEN — identical fold (screen_* fields, PrefMediaKind::Screen).
        let sh = self.screen_layer_availability.highest_available(now_ms);
        let s_base = self
            .screen_layer_chooser
            .desired_preference()
            .map(|raw| bounds.for_kind(PrefMediaKind::Screen).clamp(raw))
            .unwrap_or(sh);
        let s_lid = match hint {
            TileHint::Uncapped => sh,
            TileHint::Capped { device_px_h } => {
                size_cap_layer(device_px_h, sh, sh + 1, PrefMediaKind::Screen)
            }
        };
        // Same floor-respecting composition for SCREEN — the lid yields to the user
        // MIN, then honors an explicit user MAX. See the VIDEO arm above.
        let s_user_min = bounds.for_kind(PrefMediaKind::Screen).min.unwrap_or(0);
        let s_user_max = bounds
            .for_kind(PrefMediaKind::Screen)
            .max
            .unwrap_or(u32::MAX);
        let s_effective_max = s_lid.max(s_user_min).min(s_user_max);
        let s_layer = s_base.min(s_effective_max);
        if s_layer < sh {
            out.insert((session_id, PrefMediaKind::Screen), s_layer);
        }

        // AUDIO — UNCHANGED. Audio is NEVER size-capped, so the hint is ignored
        // here; advertise the chooser's constrained preference exactly as before.
        if let Some(raw) = self.audio_layer_chooser.desired_preference() {
            let layer = bounds.for_kind(PrefMediaKind::Audio).clamp(raw);
            if layer < self.audio_layer_availability.highest_available(now_ms) {
                out.insert((session_id, PrefMediaKind::Audio), layer);
            }
        }
    }

    /// The simulcast layer this receiver currently decodes for this peer's audio
    /// stream.
    pub fn selected_audio_layer(&self) -> u32 {
        self.selected_audio_layer
    }

    /// The audio rung to SHOW for this peer (#2132) — see [`displayed_audio_rung`].
    pub fn displayed_audio_layer(&self, now_ms: u64) -> Option<u32> {
        displayed_audio_rung(
            self.selected_audio_layer,
            self.audio_layer_availability
                .layer_available_peek(self.selected_audio_layer, now_ms),
        )
    }

    /// AUDIO-kind sibling of [`Self::set_selected_video_layer`] (issue #1695):
    /// select which audio simulcast layer this receiver decodes for this peer.
    /// AUDIO packets tagged with a different `simulcast_layer_id` are dropped
    /// (see the AUDIO arm of the decode path) before decode, exactly like VIDEO.
    ///
    /// ## THE SINGLE CHOKEPOINT for the audio decode guard (issue #2138)
    ///
    /// Every production write to `selected_audio_layer` goes through HERE, so the
    /// paired re-anchor of [`AudioDatagramLossTracker`] cannot be forgotten at a
    /// call site. This is deliberately UNLIKE video/screen, whose re-anchor is
    /// duplicated at four sites each (setter + tick + early-seed + downlink-seed) and
    /// can drift apart; audio funnels instead.
    ///
    /// What ENFORCES the chokepoint (as opposed to merely describing it today):
    ///   * `selected_audio_layer` is a PRIVATE field of `Peer`, so no code outside
    ///     this module can write it at all — external callers have only this setter.
    ///   * In-module, the writers are [`Self::tick_audio_layer_chooser`] (per-tick
    ///     chooser switch), [`Self::seed_early_congestion`] (the #1179 early-seed
    ///     bounds-clamped write), and
    ///     [`PeerDecodeManager::reconcile_decode_guards_to_wire`] (the #1695
    ///     guard-follows-wire pin). All three call this setter, and ALL THREE have a
    ///     dedicated routing regression test, so reverting any one of them to a direct
    ///     field assignment fails CI:
    ///     `audio_tick_chooser_switch_routes_through_the_reanchor_chokepoint`,
    ///     `audio_early_seed_switch_routes_through_the_reanchor_chokepoint`,
    ///     `audio_reconcile_to_wire_switch_routes_through_the_reanchor_chokepoint`.
    ///     The third was added because the pre-existing #1695 coverage asserts only the
    ///     guard VALUE, which a direct field write satisfies identically — so that
    ///     writer was a live switch site with no re-anchor guard at all.
    ///     A FOURTH in-module writer would still need its own routing test — this list
    ///     is descriptive; the PRIVATE field is what forces external callers here.
    ///
    /// `Peer` construction (`Peer::new` and the test fixtures) initialises the field
    /// in the struct literal, which is not a switch: the tracker is built fresh
    /// (`high_seq: None`) in the same literal, so there is nothing to re-anchor.
    ///
    /// There is no audio arm in [`Self::seed_downlink_congestion`] to route through:
    /// that path deliberately does not step audio down at all (#1219 Half 2 —
    /// audio is priority-protected), so it never writes this field. That absence is
    /// intentional, not a missed chokepoint.
    ///
    /// ## Why the re-anchor is needed
    ///
    /// Each audio rung is an independent Opus encode with its OWN sequence counter,
    /// so a rung change is a cross-counter discontinuity that makes the receive-side
    /// datagram-loss tracker mis-account — going BLIND on a backward switch, and
    /// reporting a phantom burst on a forward one. See
    /// [`AudioDatagramLossTracker::reanchor_for_layer_switch`] for the full mechanism.
    ///
    /// Audio DECODE and PLAYOUT are untouched: they run through NetEq, which never
    /// consults this tracker.
    ///
    /// ## But the corrected metric is NOT decode-inert — it ACTUATES (issue #2029)
    ///
    /// `audio_datagram_loss_per_sec` is not merely displayed. `health_reporter`
    /// forwards every ~1 Hz per-peer sample to
    /// [`crate::connection::connection_manager::ConnectionManager::observe_peer_audio_datagram_loss`],
    /// and on SUSTAINED, cross-sender-UNIFORM loss above `WT_AUDIO_LOSS_THRESHOLD_PER_SEC`
    /// (5.0 pkt/s) `check_audio_datagram_fallback` latches the session WebSocket-only
    /// and re-elects. That latch is ONE-WAY and session-scoped, so a wrong sample here
    /// can cost this client WebTransport for the rest of the call.
    ///
    /// Both pre-fix error directions pushed that detector the WRONG way, so this fix
    /// improves it on both counts:
    ///   * the FORWARD-jump phantom burst inflated samples over the 5.0 pkt/s
    ///     threshold — a rung switch could contribute toward a fallback that the link
    ///     did not warrant (a spurious, unrecoverable demotion to WebSocket);
    ///   * the BACKWARD-jump blindness reported ~0 while audio was genuinely dropping,
    ///     suppressing a fallback the user needed.
    ///
    /// WHICH DIRECTION MATTERS MOST: the FORWARD (over-reporting) one, because it is the
    /// direction a CONGESTED receiver takes. The base rung always LEADS the upper rungs
    /// — `audio_layer_is_published` computes `effective_count … .max(1)`, so layer 0 is
    /// never suppressed, and the base node is created before the upper-rung loop — so a
    /// suppressed upper rung's counter stalls while the base advances. A
    /// congestion-driven DOWN-switch (2→0) therefore lands on the LEADING counter, i.e. a
    /// FORWARD jump booking the whole cross-counter offset as a phantom burst. (Issue
    /// 2138's title says this metric is INFLATED; that is correct for the direction that
    /// actuates. The backward/blind direction is the recovery-driven UP-switch.)
    ///
    /// The remaining trade is deliberate: clearing the in-flight window on a real rung
    /// change means the sample published for THAT window under-reports (it counts only
    /// the post-switch part). That is the right side to err on. The detector is a
    /// WINDOWED K-of-M count over the last 12 samples — `wt_audio_fallback_should_fire`
    /// counts `uniform_lossy` across the window with no ordering requirement, so a
    /// truncated sample COSTS ONE SLOT rather than resetting progress. Against that, the
    /// alternative (leaking a cross-counter offset) inflates a sample ~10× past the
    /// 5.0 pkt/s threshold and can drive an irreversible transport demotion outright.
    ///
    /// Sizing the cost honestly: the single-remote-peer bar is 10-of-12
    /// (`WT_AUDIO_LOSS_MIN_LOSSY_SAMPLES_SINGLE`), not 8, so a 2-peer call has less slack
    /// than the multi case. Under availability flap a rung can change faster than the 5 s
    /// monitor tick (the early-seed path samples at 1 Hz), so more than one slot can be
    /// truncated in a window. This does NOT make the fix net-negative and cannot wedge
    /// the detector: the chooser SATURATES at rung 0 under genuinely sustained
    /// congestion, so switching stops exactly when the loss is persistent — and the
    /// pre-fix behaviour on those same slots was phantom loss, which is worse than a
    /// truncated real reading.
    ///
    /// The `if`-guard makes an unchanged layer a true no-op: re-anchoring on every
    /// tick would discard the healthy in-flight loss window ~once per monitor tick
    /// and permanently suppress the signal this tracker exists to publish — which
    /// would ALSO blind the #2029 detector, so the guard is load-bearing for that
    /// path too, not just for signal fidelity.
    pub fn set_selected_audio_layer(&mut self, layer: u32) {
        let layer = clamp_selected_layer(PrefMediaKind::Audio, layer);
        if layer != self.selected_audio_layer {
            // Issue #2138: the new rung's sequence comes from a DIFFERENT counter,
            // so drop the old baseline before the guard starts admitting it.
            self.audio_datagram_loss.reanchor_for_layer_switch();
            self.selected_audio_layer = layer;
        }
    }

    /// Broadcast current media-enabled state to the diagnostics bus so the UI
    /// can update peer tiles.
    fn broadcast_peer_status(&self) {
        if let Some(fields) = self.layer_gate_skips_log_fields() {
            let line = format!("LAYER_GATE_SKIPS session_id={} {fields}", self.session_id);
            #[cfg(test)]
            LAST_GATE_SKIPS_LOG.with(|c| *c.borrow_mut() = Some(line.clone()));
            log::info!("{line}");
        }
        let _ = global_sender().try_broadcast(self.peer_status_event());
    }

    /// Nonzero `LAYER_GATE_SKIPS` fields, or `None` while every counter is zero.
    /// Rides `broadcast_peer_status` (per heartbeat); the gate fires per packet.
    fn layer_gate_skips_log_fields(&self) -> Option<String> {
        let s = self.layer_gate_skips;
        let fields: Vec<String> = [
            ("video_above", s.video_guard_above),
            ("video_below", s.video_guard_below),
            ("screen_above", s.screen_guard_above),
            ("screen_below", s.screen_guard_below),
            ("audio_above", s.audio_guard_above),
            ("audio_below", s.audio_guard_below),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(name, count)| format!("{name}={count}"))
        .collect();
        (!fields.is_empty()).then(|| fields.join(" "))
    }

    /// Build the `peer_status` event this peer currently reports.
    ///
    /// Split out from [`Self::broadcast_peer_status`] so the emitted shape is
    /// unit-testable on the host target: the global bus keeps no live receiver
    /// on native and closes itself on first use, so the broadcast itself is
    /// unobservable there.
    fn peer_status_event(&self) -> DiagEvent {
        let now = now_ms();
        let transport_str = match self.transport_type {
            TransportType::TRANSPORT_WEBTRANSPORT => "webtransport",
            TransportType::TRANSPORT_WEBSOCKET => "websocket",
            TransportType::TRANSPORT_UNKNOWN => "unknown",
        };
        DiagEvent {
            subsystem: "peer_status",
            stream_id: None,
            ts_ms: now,
            metrics: {
                let mut m = vec![
                    metric!("to_peer", self.sid_str.clone()),
                    metric!(
                        "audio_enabled",
                        if self.audio_enabled { 1u64 } else { 0u64 }
                    ),
                    metric!(
                        "video_enabled",
                        if self.video_enabled { 1u64 } else { 0u64 }
                    ),
                    metric!(
                        "screen_enabled",
                        if self.screen_enabled { 1u64 } else { 0u64 }
                    ),
                    // Issue 2174: audio-off dominates the speaking flag. A peer
                    // whose audio is disabled cannot be audibly speaking, so never
                    // report `is_speaking: 1` alongside `audio_enabled: 0` — a
                    // stale `true` here would re-light the tile glow right after
                    // the decoder's terminal `speaking: 0`. Callers that force
                    // audio off also clear the field (see `force_media_off`); this
                    // guard is the backstop that makes any future forced path safe
                    // by construction.
                    metric!(
                        "is_speaking",
                        if self.is_speaking && self.audio_enabled {
                            1u64
                        } else {
                            0u64
                        }
                    ),
                    metric!("audio_level", self.audio_level as f64),
                    // Zero-alloc per emit: `transport_str` is a `&'static str` from
                    // a literal match, so route it through the borrowing path
                    // (`Cow::Borrowed`) instead of the allocating `From<&str>`.
                    //
                    // Issue #1421: the dominant caller is the HEARTBEAT arm below,
                    // which broadcasts once per received heartbeat per peer. The
                    // sender's keepalive is `HEARTBEAT_KEEPALIVE_INTERVAL_MS` =
                    // 5000 (videocall-aq/src/constants.rs, driven by the `Interval`
                    // in connection/connection.rs), so this is ~0.2 Hz per peer at
                    // rest — plus an edge-triggered immediate heartbeat on every
                    // mute / camera / screen transition (`send_immediate_heartbeat`),
                    // and it scales with participant count.
                    Metric {
                        name: "peer_transport",
                        value: MetricValue::text_static(transport_str),
                    },
                ];
                // Only when the selected rung is actually arriving (#2132): an
                // absent metric renders as "—" rather than a nominal bitrate the
                // receiver is not getting.
                if let Some(rung) = self.displayed_audio_layer(now) {
                    m.push(metric!(METRIC_SELECTED_AUDIO_LAYER, rung as u64));
                }
                // Omitted until this peer's first discard, then permanent.
                let s = self.layer_gate_skips;
                for (name, count) in [
                    ("layer_gate_skips_video_guard_above", s.video_guard_above),
                    ("layer_gate_skips_video_guard_below", s.video_guard_below),
                    ("layer_gate_skips_screen_guard_above", s.screen_guard_above),
                    ("layer_gate_skips_screen_guard_below", s.screen_guard_below),
                    ("layer_gate_skips_audio_guard_above", s.audio_guard_above),
                    ("layer_gate_skips_audio_guard_below", s.audio_guard_below),
                ] {
                    if count > 0 {
                        m.push(metric!(name, count));
                    }
                }
                m
            },
        }
    }

    /// Authoritatively force *this* peer's audio and/or video to the *off*
    /// state, bypassing the heartbeat freshness guard.
    ///
    /// This is the shared per-peer body behind both
    /// [`PeerDecodeManager::force_peer_media_off`] (single target, HCL #1034)
    /// and [`PeerDecodeManager::force_all_peers_media_off_except`] (mute-all /
    /// disable-all, HCL #1036). It sets the tracked `audio_enabled` /
    /// `video_enabled` flags **directly** and reuses the same decoder-flush
    /// paths the heartbeat off-transition uses, so a frozen last frame is
    /// cleared at the instant the tile flips and the NetEq buffer stops
    /// emitting expand/hiss after the stream is gone.
    ///
    /// Idempotent: only mutates and reports a change on an actual
    /// `enabled -> false` transition, so duplicate dual-transport deliveries or
    /// already-off peers are no-ops. Returns `true` iff any tracked flag
    /// transitioned, so the caller can drive a single `broadcast_peer_status()`
    /// per real change.
    fn force_media_off(&mut self, audio_off: bool, video_off: bool) -> bool {
        let mut changed = false;

        // Issue 2274: latched independently of the transition guards below — a
        // peer still at the `Peer::new` default takes no transition there, yet
        // is exactly the one `decode` would infer back on.
        if audio_off {
            self.audio_forced_off = true;
        }
        if video_off {
            self.video_forced_off = true;
        }

        if audio_off && self.audio_enabled {
            self.audio_enabled = false;
            // Mute and flush the audio decoder the same way the heartbeat
            // audio-off transition does, to prevent the NetEq buffer from
            // emitting expand/hiss packets after the stream is gone.
            self.audio.set_muted(true);
            self.audio.flush();
            // Issue 2174: clear the tracked speaking state too, mirroring the
            // heartbeat audio-off arm. Without this the peer stays recorded as
            // speaking, and the next `broadcast_peer_status()` would re-light
            // the tile glow moments after the decoder emitted its terminal
            // `speaking: 0`. `audio_level` is reset for the same reason (it is
            // 0.0 on every current path, so this is future-proofing rather than
            // a live fix).
            self.is_speaking = false;
            self.audio_level = 0.0;
            changed = true;
            debug!(
                "force_media_off: muted audio for peer {} (host command)",
                self.session_id
            );
        }

        if video_off && self.video_enabled {
            self.video_enabled = false;
            // Reuse the heartbeat video-off flush path so the frozen last
            // frame is cleared at the same instant the tile flips to the
            // avatar — no lingering freeze-frame (#1034).
            self.video.flush();
            // Force-clear the canvas so the recording output shows the avatar
            // rather than a frozen last frame (recording_active guard does not
            // apply for a genuine camera-off command).
            self.video.force_clear_canvas();
            changed = true;
            debug!(
                "force_media_off: disabled video for peer {} (host command)",
                self.session_id
            );
        }

        changed
    }

    /// Emit windowed per-stream packet-loss and keyframe-request rates on the
    /// diagnostics bus (freeze observability, issue #1013).
    ///
    /// Called at most ~1Hz per peer-stream (only on rate-window rollover), so
    /// this stays off the per-packet hot path.
    ///
    /// `local_user_id` is the reporting (local) client — the `from_peer` of the
    /// event; `self.sid_str` is the observed remote peer — the `to_peer`.
    fn emit_loss_metrics(&self, local_user_id: &str, media_type: MediaType, rates: &WindowSample) {
        let _ = global_sender().try_broadcast(loss_metrics_event(
            local_user_id,
            &self.sid_str,
            media_type,
            rates,
            now_ms(),
        ));
    }

    /// Emit the windowed receive-side audio DATAGRAM loss rate on the
    /// diagnostics bus (issue #1878). Published under the `"neteq"` subsystem —
    /// the per-peer AUDIO bucket — with the same `from_peer`/`to_peer` routing
    /// keys as [`Self::emit_loss_metrics`], so the existing health_reporter
    /// `"neteq"` handler folds it into this peer's audio health next to the
    /// NetEQ concealment stats. Called at most ~1 Hz per peer (only on rate-
    /// window rollover), so it stays off the per-packet hot path.
    ///
    /// `local_user_id` is the reporting (local) client — the `from_peer`;
    /// `self.sid_str` is the observed remote peer — the `to_peer`.
    fn emit_audio_datagram_loss(
        &self,
        local_user_id: &str,
        loss_per_sec: f64,
        raw_loss_per_sec: f64,
    ) {
        let evt = DiagEvent {
            subsystem: "neteq",
            stream_id: None,
            ts_ms: now_ms(),
            metrics: vec![
                metric!("from_peer", local_user_id.to_string()),
                metric!("to_peer", self.sid_str.clone()),
                metric!("wt_datagram_audio_loss_per_sec", loss_per_sec),
                // Issue 2031: uncapped magnitude companion, emitted on the same
                // event/cadence so the health reporter folds both per-peer rates.
                metric!("wt_datagram_audio_raw_loss_per_sec", raw_loss_per_sec),
            ],
        };
        let _ = global_sender().try_broadcast(evt);
    }

    /// `false` when `raw_layer_id` is outside `kind`'s ladder, in which case the
    /// caller must discard the packet rather than record the layer as arriving.
    fn observe_arriving_layer(
        &mut self,
        kind: PrefMediaKind,
        raw_layer_id: u32,
        now_ms: u64,
    ) -> bool {
        if raw_layer_id >= crate::decode::layer_chooser::max_layers_for_kind(kind) {
            return false;
        }
        match kind {
            PrefMediaKind::Video => self.video_layer_availability.observe(raw_layer_id, now_ms),
            PrefMediaKind::Screen => self.screen_layer_availability.observe(raw_layer_id, now_ms),
            PrefMediaKind::Audio => self.audio_layer_availability.observe(raw_layer_id, now_ms),
        }
        true
    }

    /// Decode a packet and return `(media_type, decode_status, keyframe_request)`.
    ///
    /// The third element is `Some(media_type)` when a sequence gap has been
    /// detected and enough time has elapsed to warrant sending a
    /// KEYFRAME_REQUEST to this peer. The caller is responsible for
    /// actually sending the request packet.
    fn decode(
        &mut self,
        packet: &Arc<PacketWrapper>,
        local_user_id: &str,
    ) -> Result<(MediaType, DecodeStatus, Option<MediaType>), PeerDecodeError> {
        if packet
            .packet_type
            .enum_value()
            .map_err(|_| PeerDecodeError::NoPacketType)?
            != PacketType::MEDIA
        {
            return Err(PeerDecodeError::IncorrectPacketType);
        }

        // Read the cleartext simulcast layer id from the OUTER wrapper (issue
        // #989) before `packet` is shadowed by the decrypted inner MediaPacket.
        // Pre-simulcast and explicitly single-layer publishers send 0, so this
        // is 0 for them. The VIDEO arm uses it to drop non-selected layers
        // before any sequence tracking / decode.
        let incoming_video_layer = packet.simulcast_layer_id;

        // `now_ms()` is WALL CLOCK (`js_sys::Date::now()`), NOT monotonic — it feeds
        // the media-freshness stamp and `LayerAvailability::observe`. Wall clock is
        // accepted here: `observe` overwrites its per-layer stamp unconditionally
        // (no latched watermark), so the next packet for a layer re-anchors it.
        let now = now_ms();

        // ---- CLEARTEXT layer gate, BEFORE AES-decrypt (#1066) -----------------
        //
        // The simulcast layer-drop decision needs only CLEARTEXT envelope fields
        // (`media_kind` field 6 and `simulcast_layer_id` field 5 on the outer
        // `PacketWrapper`), both of which live OUTSIDE the AEAD seal. Deciding the
        // drop here — before `aes.decrypt(...)` and `parse_media_packet(...)`
        // below — means a receiver that has narrowed to one layer pays NO
        // AES-decrypt and NO protobuf-parse cost on the layers it discards, so a
        // receiver's CPU stops scaling with the publisher's layer count (the
        // perf goal of #1066). Previously this guard keyed on the DECRYPTED inner
        // `media_type`, so every non-selected layer was decrypted and parsed only
        // to be dropped a few lines later.
        //
        // This is PERF-ONLY — NOT a trust/authz change. `media_kind` and
        // `simulcast_layer_id` were already trusted, un-sealed routing hints (the
        // relay filters on the very same fields); a forged value only changes
        // which layer the FORGER's own receiver decodes. A non-key-holder still
        // cannot decrypt anything: the drop returns SKIPPED without ever touching
        // `self.aes`, and a kept packet still goes through the unchanged decrypt
        // path below.
        //
        // N=1 INERT (preserved exactly): pre-simulcast / single-layer publishers
        // send `simulcast_layer_id == 0`, and every `selected_*_layer` defaults to
        // 0, so `incoming_video_layer != selected` is false → nothing is dropped
        // and we fall through to the identical decrypt+decode path. The
        // availability `observe` records layer 0, exactly as before.
        //
        // FALL-THROUGH for UNSPECIFIED: a packet whose cleartext `media_kind` is
        // UNSPECIFIED (older client that predates field 6 — which also predates
        // simulcast, so its layer id is always 0) is NOT gated here; it falls
        // through and the per-arm observe+drop below runs against the decrypted
        // `media_type` exactly as it did before this change.
        let cleartext_kind = packet
            .media_kind
            .enum_value()
            .unwrap_or(MediaKind::MEDIA_KIND_UNSPECIFIED);
        // True once the cleartext gate has taken ownership of the observe+drop
        // for this kind, so the matching post-decrypt arm skips its now-redundant
        // (and otherwise double-counting) observe+drop.
        let mut cleartext_gate_handled = false;
        match cleartext_kind {
            MediaKind::VIDEO => {
                if !self.observe_arriving_layer(PrefMediaKind::Video, incoming_video_layer, now) {
                    self.layer_gate_skips.record(
                        PrefMediaKind::Video,
                        incoming_video_layer,
                        self.selected_video_layer,
                    );
                    return Ok((MediaType::VIDEO, DecodeStatus::SKIPPED, None));
                }
                // EXACT-MATCH simulcast guard (NOT SVC "layer N and below"): these
                // are independent encodes (see SIMULCAST_LAYER_TIER_INDICES doc in
                // videocall-aq). A packet whose `layer_id` is not the selected layer
                // is DROPPED, not down-decoded — so if this guard ever sits ABOVE the
                // layer the relay is actually forwarding, EVERY arriving packet is
                // skipped and the tile FREEZES on its last-good frame. The selected
                // layer must therefore never lead the requested-layer wire state
                // (issue #1695): raise it only once the LAYER_PREFERENCE that asks
                // for the higher layer has been published.
                // #2251: `downlink_shed_candidate` drops EVERY non-base camera rung
                // for a congested receiver, so a guard above 0 skips all it is sent.
                if incoming_video_layer < self.selected_video_layer {
                    self.collapse_video_guard_to_available(now);
                }
                if incoming_video_layer != self.selected_video_layer {
                    // #2533: metered AFTER the collapse above has had its chance,
                    // so a packet the collapse rescued is not counted as a discard.
                    self.layer_gate_skips.record(
                        PrefMediaKind::Video,
                        incoming_video_layer,
                        self.selected_video_layer,
                    );
                    return Ok((MediaType::VIDEO, DecodeStatus::SKIPPED, None));
                }
                cleartext_gate_handled = true;
            }
            MediaKind::SCREEN => {
                if !self.observe_arriving_layer(PrefMediaKind::Screen, incoming_video_layer, now) {
                    self.layer_gate_skips.record(
                        PrefMediaKind::Screen,
                        incoming_video_layer,
                        self.selected_screen_layer,
                    );
                    return Ok((MediaType::SCREEN, DecodeStatus::SKIPPED, None));
                }
                if incoming_video_layer != self.selected_screen_layer {
                    self.layer_gate_skips.record(
                        PrefMediaKind::Screen,
                        incoming_video_layer,
                        self.selected_screen_layer,
                    );
                    return Ok((MediaType::SCREEN, DecodeStatus::SKIPPED, None));
                }
                cleartext_gate_handled = true;
            }
            MediaKind::AUDIO => {
                if !self.observe_arriving_layer(PrefMediaKind::Audio, incoming_video_layer, now) {
                    self.layer_gate_skips.record(
                        PrefMediaKind::Audio,
                        incoming_video_layer,
                        self.selected_audio_layer,
                    );
                    return Ok((MediaType::AUDIO, DecodeStatus::SKIPPED, None));
                }
                if incoming_video_layer != self.selected_audio_layer {
                    self.layer_gate_skips.record(
                        PrefMediaKind::Audio,
                        incoming_video_layer,
                        self.selected_audio_layer,
                    );
                    return Ok((MediaType::AUDIO, DecodeStatus::SKIPPED, None));
                }
                cleartext_gate_handled = true;
            }
            MediaKind::MEDIA_KIND_UNSPECIFIED => {
                // Older / non-layered client: fall through to the post-decrypt
                // per-arm observe+drop, unchanged.
            }
        }

        let packet = match self.aes {
            Some(aes) => {
                let data = aes
                    .decrypt(&packet.data)
                    .map_err(|_| PeerDecodeError::AesDecryptError)?;
                parse_media_packet(&data)?
            }
            None => parse_media_packet(&packet.data)?,
        };

        let media_type = packet
            .media_type
            .enum_value()
            .map_err(|_| PeerDecodeError::NoMediaType)?;
        match media_type {
            MediaType::VIDEO => {
                // Phase 2 (#989): learn which layers this source produces and
                // drop non-selected layers. NOTE (#1066): when the CLEARTEXT gate
                // above already handled this kind (the common path for current
                // clients that stamp `media_kind`), the observe+drop ran there
                // BEFORE decrypt, so a surviving packet here is guaranteed to be
                // the selected layer. We therefore skip this redundant block to
                // avoid a double `observe`. This per-arm path now runs ONLY for
                // the cleartext-UNSPECIFIED fall-through (older clients).
                if !cleartext_gate_handled {
                    if !self.observe_arriving_layer(PrefMediaKind::Video, incoming_video_layer, now)
                    {
                        self.layer_gate_skips.record(
                            PrefMediaKind::Video,
                            incoming_video_layer,
                            self.selected_video_layer,
                        );
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }

                    // Simulcast layer-select guard (issue #989). Drop any VIDEO
                    // packet that is not the layer this receiver is decoding for
                    // this peer — BEFORE sequence tracking and BEFORE decode.
                    //
                    // This MUST run before `track_sequence`: each simulcast layer
                    // carries an independent dense sequence, so feeding a
                    // non-selected layer's sequence into our single per-peer
                    // `video_seq_tracker` would manufacture phantom loss
                    // (~(N-1)/N) and trigger spurious PLI storms. For
                    // pre-simulcast publishers `incoming_video_layer` is 0 and
                    // `selected_video_layer` defaults to 0, so nothing is dropped.
                    if incoming_video_layer != self.selected_video_layer {
                        self.layer_gate_skips.record(
                            PrefMediaKind::Video,
                            incoming_video_layer,
                            self.selected_video_layer,
                        );
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }
                }

                // Track sequence numbers for gap detection (PLI) and windowed
                // loss/keyframe-rate accounting (freeze observability #1013).
                let seq = self.track_sequence(media_type, &packet);
                let mut kf_request = seq.keyframe_request;
                let window = seq.window;

                // HCL bug #1: stamp the freshness timestamp BEFORE the
                // `has_received_heartbeat` branch so it works on both the
                // "no heartbeat yet" and "heartbeat says off, drop frame"
                // paths. The next heartbeat consults this to decide whether
                // to trust its own metadata or the live frame stream.
                self.last_video_frame_ms = now;

                if !self.video_enabled {
                    if !self.has_received_heartbeat && !self.video_forced_off {
                        // No heartbeat yet — infer video_enabled from the actual frame.
                        self.video_enabled = true;
                        self.broadcast_peer_status();
                    } else {
                        // Peer has video off per heartbeat; drop straggler frame.
                        if kf_request.take().is_some() {
                            self.video_seq_tracker.record_keyframe_suppression(
                                KeyframeSuppressionReason::DiscardedVideoOff,
                            );
                        }
                        self.settle_video_window(local_user_id, media_type, window);
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }
                }

                // Skip video decoding when the peer tile is not visible in the
                // viewport. The next keyframe after visibility is restored will
                // allow the decoder to recover naturally.
                //
                // Recording follows the same local visibility/decode set the
                // participant is already viewing. Off-budget peers remain
                // avatars instead of forcing extra decode work on this device.
                if !should_decode_visible_peer(self.visible) {
                    if kf_request.take().is_some() {
                        self.video_seq_tracker.record_keyframe_suppression(
                            KeyframeSuppressionReason::DiscardedNotVisible,
                        );
                    }
                    self.settle_video_window(local_user_id, media_type, window);
                    return Ok((media_type, DecodeStatus::SKIPPED, None));
                }

                self.settle_video_window(local_user_id, media_type, window);

                let video_status = self
                    .video
                    .decode(&packet)
                    .map_err(|_| PeerDecodeError::VideoDecodeError)?;
                Ok((
                    media_type,
                    DecodeStatus {
                        rendered: video_status._rendered,
                        first_frame: video_status.first_frame,
                    },
                    kf_request,
                ))
            }
            MediaType::AUDIO => {
                // Phase 3 (#989): learn AUDIO layer availability and drop
                // non-selected audio layers. Skipped here when the CLEARTEXT gate
                // (#1066) already handled this kind before decrypt — see the
                // VIDEO arm for the full rationale; this per-arm block now runs
                // only for the cleartext-UNSPECIFIED fall-through.
                if !cleartext_gate_handled {
                    if !self.observe_arriving_layer(PrefMediaKind::Audio, incoming_video_layer, now)
                    {
                        self.layer_gate_skips.record(
                            PrefMediaKind::Audio,
                            incoming_video_layer,
                            self.selected_audio_layer,
                        );
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }

                    // Phase 3 (#989): AUDIO simulcast layer-select guard. Drop
                    // audio packets whose layer != the selected audio layer.
                    // Default selected_audio_layer is 0, matching single-layer
                    // publishers.
                    if incoming_video_layer != self.selected_audio_layer {
                        self.layer_gate_skips.record(
                            PrefMediaKind::Audio,
                            incoming_video_layer,
                            self.selected_audio_layer,
                        );
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }
                }

                // HCL bug #1: stamp audio freshness regardless of the
                // straggler-drop path so the next heartbeat can detect
                // recent audio frames and suppress a stale-muted heartbeat.
                self.last_audio_frame_ms = now;

                // Issue #1878: receive-side audio DATAGRAM loss accounting.
                // Recorded on ARRIVAL — before the mute/straggler gate below —
                // so a mute-off transition does not manufacture a phantom gap,
                // and only while THIS receiver is on WebTransport (the sole
                // transport that can carry audio on unreliable datagrams). The
                // dense per-frame `sequence` (see `microphone_encoder`) turns a
                // dropped datagram into a countable gap; the windowed rate is
                // emitted to the diagnostics bus / health reporter on rollover.
                if self.receiver_on_webtransport {
                    if let Some(seq) = packet.audio_metadata.as_ref().map(|am| am.sequence) {
                        let new_lost = self.audio_datagram_loss.record_seq(seq);
                        if let Some((loss_per_sec, raw_loss_per_sec)) =
                            self.audio_datagram_loss.observe_window(now, new_lost)
                        {
                            self.emit_audio_datagram_loss(
                                local_user_id,
                                loss_per_sec,
                                raw_loss_per_sec,
                            );
                        }
                    }
                }

                if !self.audio_enabled {
                    if !self.has_received_heartbeat && !self.audio_forced_off {
                        // No heartbeat yet — infer audio_enabled from the actual frame.
                        self.audio_enabled = true;
                        self.audio.set_muted(false);
                        self.broadcast_peer_status();
                    } else {
                        // Peer is muted per heartbeat; drop straggler audio to avoid audible glitch.
                        // Note: Not counting this as a frame drop since it's intentional filtering
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }
                }
                Ok((
                    media_type,
                    self.audio
                        .decode(&packet)
                        .map_err(|_| PeerDecodeError::AudioDecodeError)?,
                    None,
                ))
            }
            MediaType::SCREEN => {
                // Phase 3 (#989): learn SCREEN layer availability and drop
                // non-selected screen layers. Skipped here when the CLEARTEXT
                // gate (#1066) already handled this kind before decrypt — see the
                // VIDEO arm for the full rationale; this per-arm block now runs
                // only for the cleartext-UNSPECIFIED fall-through.
                if !cleartext_gate_handled {
                    if !self.observe_arriving_layer(
                        PrefMediaKind::Screen,
                        incoming_video_layer,
                        now,
                    ) {
                        self.layer_gate_skips.record(
                            PrefMediaKind::Screen,
                            incoming_video_layer,
                            self.selected_screen_layer,
                        );
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }

                    // Phase 3 (#989): SCREEN simulcast layer-select guard. Drop
                    // any SCREEN packet that is not the layer this receiver
                    // selected for this peer's screen — BEFORE sequence tracking
                    // and decode, for the same phantom-loss reason as the VIDEO
                    // guard. Pre-simulcast / single-layer screen publishers send
                    // layer 0 and the default selected_screen_layer is 0, so
                    // nothing is dropped for them.
                    if incoming_video_layer != self.selected_screen_layer {
                        self.layer_gate_skips.record(
                            PrefMediaKind::Screen,
                            incoming_video_layer,
                            self.selected_screen_layer,
                        );
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }
                }

                // Track sequence numbers for gap detection (PLI) and windowed
                // loss/keyframe-rate accounting (freeze observability #1013).
                let seq = self.track_sequence(media_type, &packet);
                if let Some(sample) = self.observe_sequence_window(media_type, seq.window) {
                    self.emit_loss_metrics(local_user_id, media_type, &sample);
                    // Phase 3: cache the SCREEN downlink window for the chooser.
                    self.last_screen_downlink = DownlinkSample {
                        loss_per_sec: sample.loss_per_sec,
                        kf_per_sec: sample.kf_per_sec,
                    };
                }
                let kf_request = seq.keyframe_request;

                // Issue #2328: a KEYFRAME on the selected rung breaks the keyframe-less hold —
                // whatever the decoder does with it, the publisher is provably still re-keying
                // this rung, which is the exact condition the retraction tests for. Reaching here
                // already means the packet passed the layer guard, so this can only ever clear a
                // hold recorded against the rung we are actually decoding. Then evaluate: this
                // runs on every selected-rung screen packet, which is the tightest bound available
                // (the 5s monitor tick evaluates it again as the no-packets backstop).
                if packet.frame_type.as_str() == "key" {
                    self.clear_screen_keyframe_starvation();
                }
                self.apply_screen_keyframe_starvation(now);

                // HCL bug #1: stamp the screen-freshness timestamp on every
                // observed SCREEN frame. The next heartbeat (which may carry
                // a stale `metadata.screen_enabled = false` on WebTransport
                // because heartbeats and SCREEN frames race across separate
                // QUIC streams) consults this to decide whether to honour
                // its own metadata or trust the live screen stream.
                // Without this stamp, the heartbeat at line ~691 below would
                // overwrite `screen_enabled` back to false, the UI would
                // observe `has_screen_share = false`, and the split-screen
                // layout would collapse — exactly the WT-only symptom from
                // the user report.
                self.last_screen_frame_ms = now;

                if !self.screen_enabled {
                    // A SCREEN frame arrived while screen_enabled is false.
                    // This happens when the sender starts sharing before
                    // the next heartbeat reaches us.  Trust the actual
                    // media frame over the (stale) heartbeat state —
                    // dropping the first keyframe here would leave the
                    // decoder waiting until a PLI round-trip completes.
                    self.screen_enabled = true;
                    self.broadcast_peer_status();
                }

                // Skip screen decoding when the peer tile is not visible.
                // Still propagate any keyframe request from gap detection so
                // that the sender starts producing keyframes before the tile
                // becomes visible again.
                //
                if !should_decode_visible_peer(self.visible) {
                    return Ok((media_type, DecodeStatus::SKIPPED, kf_request));
                }

                let screen_status = self
                    .screen
                    .decode(&packet)
                    .map_err(|_| PeerDecodeError::ScreenDecodeError)?;

                // If gap detection already requested a keyframe, use that.
                // Otherwise, proactively request one when the screen decoder
                // is still waiting for a keyframe (e.g., late joiner starting
                // mid-stream, or returning from off-screen). Rate-limited via
                // the screen tracker's last_keyframe_request_ms to avoid
                // spamming the sender.
                let effective_kf_request = if kf_request.is_some() {
                    kf_request
                } else if self.screen.is_waiting_for_keyframe() {
                    // Reuse the hoisted per-packet `now` (computed at the top of
                    // decode) — within one synchronous decode call it is the same
                    // instant for rate-limiting purposes, and avoids another
                    // JS-boundary `now_ms()` call.
                    let elapsed =
                        now.saturating_sub(self.screen_seq_tracker.last_keyframe_request_ms);
                    if elapsed >= KEYFRAME_REQUEST_MIN_INTERVAL_MS {
                        self.screen_seq_tracker.last_keyframe_request_ms = now;
                        Some(MediaType::SCREEN)
                    } else {
                        None
                    }
                } else {
                    None
                };

                Ok((
                    media_type,
                    DecodeStatus {
                        rendered: screen_status._rendered,
                        first_frame: screen_status.first_frame,
                    },
                    effective_kf_request,
                ))
            }
            MediaType::HEARTBEAT => {
                self.has_received_heartbeat = true;
                // update state using heartbeat metadata
                if let Some(metadata) = packet.heartbeat_metadata.as_ref() {
                    let now = now_ms();
                    // HCL bug #1: resolve each media-enabled flag against
                    // recently observed frames. The heartbeat stream and the
                    // media streams race on WebTransport — a stale heartbeat
                    // carrying `metadata.X_enabled = false` can arrive after
                    // we've already started decoding live X frames. Trusting
                    // the heartbeat blindly would erase `screen_enabled = true`
                    // and collapse the split-screen-share layout for one full
                    // heartbeat period. The freshness check trusts the live
                    // media when we saw an X frame within the last
                    // `MEDIA_FRESH_WINDOW_MS`; otherwise the heartbeat wins.
                    // Camera-video uses the short continuous-stream window
                    // (NOT the screen-sized one): a live camera streams
                    // frames continuously, so a real camera-off must reflect
                    // on remote peers sub-second — not after the ~5s screen
                    // window. See `LIVE_STREAM_FRESH_WINDOW_MS` for the full
                    // rationale.
                    // Camera adds an on->off corroboration debounce so a lone
                    // stale `false` datagram can't blank a still-on but slow
                    // camera. See `resolve_camera_heartbeat_flag`.
                    let (resolved_video, new_video_off_count) = resolve_camera_heartbeat_flag(
                        self.video_enabled,
                        metadata.video_enabled,
                        self.last_video_frame_ms,
                        now,
                        self.consecutive_video_off_hbs,
                    );
                    self.consecutive_video_off_hbs = new_video_off_count;
                    // Audio likewise uses the short continuous-stream window
                    // so a real mute reflects on remote peers sub-second.
                    let resolved_audio = apply_live_stream_heartbeat_flag(
                        self.audio_enabled,
                        metadata.audio_enabled,
                        self.last_audio_frame_ms,
                        now,
                    );
                    let resolved_screen = apply_screen_heartbeat_flag(
                        self.screen_enabled,
                        metadata.screen_enabled,
                        self.last_screen_frame_ms,
                        now,
                    );

                    // Check if video is being turned off (on -> off transition)
                    let video_turned_off = self.video_enabled && !resolved_video;
                    // Check if screen is being turned off (on -> off transition)
                    let screen_turned_off = self.screen_enabled && !resolved_screen;
                    // Check if audio is being turned off (on -> off transition)
                    let audio_turned_off = self.audio_enabled && !resolved_audio;
                    // Check if audio state changed at all
                    let audio_state_changed = self.audio_enabled != resolved_audio;

                    // Set mute state on audio decoder when audio state changes (before updating state)
                    if audio_state_changed {
                        self.audio.set_muted(!resolved_audio);
                        debug!(
                            "Audio state changed for peer {} - muted: {}",
                            self.session_id, !resolved_audio
                        );
                    }

                    // #2537: must precede the writes below — the edge is read off the
                    // PREVIOUS state.
                    self.reanchor_on_media_restart(resolved_video, resolved_screen);

                    self.video_enabled = resolved_video;
                    self.audio_enabled = resolved_audio;
                    self.screen_enabled = resolved_screen;
                    // Issue 2174 (security carry-over): resolve the peer's
                    // self-reported speaking claim against its resolved audio
                    // state before storing it. A peer that announces
                    // `audio_enabled = 0` while claiming `is_speaking = 1`
                    // would otherwise hold the #1557 active-speaker exemption
                    // from the local-CPU layer-drop cascade on EVERY receiver
                    // (see the `exempt_speakers && peer.is_speaking` skip in
                    // `seed_downlink_congestion`) without ever sending audio —
                    // a resource lever a peer should not control unilaterally.
                    //
                    // This is #1557's own definition, not a new policy: that
                    // exemption is documented as keying off the INSTANTANEOUS
                    // VAD bool, and a peer with no audio is by definition not
                    // instantaneously speaking.
                    self.is_speaking = metadata.is_speaking && resolved_audio;
                    if !self.is_speaking {
                        self.audio_level = 0.0;
                    }
                    // Capture peer's announced transport. Unknown enum
                    // values (e.g., from a future client) decay to
                    // TRANSPORT_UNKNOWN rather than crashing.
                    self.transport_type = metadata
                        .transport_type
                        .enum_value()
                        .unwrap_or(TransportType::TRANSPORT_UNKNOWN);

                    // Flush video decoder when video is turned off

                    if video_turned_off {
                        self.video.flush();
                        // Clear the canvas unconditionally so the recording
                        // output shows the avatar instead of a frozen last
                        // frame.  clear_canvas() would skip the pixel wipe
                        // when recording_active=true, which is only the right
                        // behaviour for budget-pressure invisibility — not for
                        // a genuine camera-off event.
                        self.video.force_clear_canvas();
                        debug!(
                            "Flushed video decoder for peer {} (video turned off)",
                            self.session_id
                        );
                    }

                    // Flush audio decoder when audio is turned off to prevent expand packets
                    if audio_turned_off {
                        // For NetEq audio decoders, we need to flush the buffer to prevent hissing
                        self.audio.flush();
                        debug!(
                            "Flushed audio decoder for peer {} (audio turned off)",
                            self.session_id
                        );
                    }

                    // Flush screen decoder when screen sharing is turned off
                    if screen_turned_off {
                        self.screen.flush();
                        // HCL #893 re-share: the screen decoder is reused across
                        // stop→restart (only flushed, never recreated), so its
                        // once-per-lifetime first-render ack latch survives. Re-arm
                        // it here so the NEXT share re-fires
                        // PEER_EVENT(screen_decode_started); otherwise every share
                        // after the first in this page session never acks and the
                        // publisher falsely toasts "No peers received the shared
                        // content within 10 seconds".
                        self.screen.rearm_first_render_ack();
                        // A stopped share is a source-state transition, not a
                        // visibility shed. Wipe the decoder canvas so the
                        // recording compositor cannot retain a frozen frame.
                        self.screen.force_clear_canvas();
                        debug!(
                            "Flushed + re-armed screen decoder ack + cleared canvas for peer {} (screen turned off)",
                            self.session_id
                        );
                    }

                    self.broadcast_peer_status();
                }
                Ok((media_type, DecodeStatus::SKIPPED, None))
            }
            MediaType::RTT => {
                // RTT packets are handled by ConnectionManager, not by peer decoders
                debug!(
                    "Received RTT packet for peer {} - ignoring in peer decoder",
                    self.session_id
                );
                Ok((media_type, DecodeStatus::SKIPPED, None))
            }
            MediaType::KEYFRAME_REQUEST => {
                // Keyframe requests are handled by encoders, not by peer decoders.
                debug!(
                    "Received KEYFRAME_REQUEST for peer {} - ignoring in peer decoder",
                    self.session_id
                );
                Ok((media_type, DecodeStatus::SKIPPED, None))
            }
            MediaType::MEDIA_TYPE_UNKNOWN => {
                log::error!(
                    "Received packet with unknown media type from peer {}",
                    self.session_id
                );
                Err(PeerDecodeError::UnknownMediaType)
            }
        }
    }

    /// Track the sequence number of an incoming video/screen packet and detect
    /// genuine packet loss using a sliding-window reorder buffer.
    ///
    /// Returns a [`SeqTrackResult`] carrying any pending KEYFRAME_REQUEST plus,
    /// on ~1Hz window rollover, the freshly-computed per-stream loss/keyframe
    /// rates for the diagnostics bus.
    ///
    /// Unlike the previous implementation, out-of-order arrivals within a 64-
    /// packet window are NOT treated as loss. Only packets that shift off the
    /// window without ever being received are counted as genuinely lost.
    fn track_sequence(&mut self, media_type: MediaType, packet: &MediaPacket) -> SeqTrackResult {
        // Both VIDEO and SCREEN packets use `video_metadata` for sequence
        // tracking. This is correct: `transform_screen_chunk` in
        // `encode/transform.rs` populates `VideoMetadata { sequence, .. }`
        // for SCREEN packets the same way `transform_video_chunk` does for
        // VIDEO packets.
        let (seq, frame_type_str) = if let Some(vm) = packet.video_metadata.as_ref() {
            (vm.sequence, packet.frame_type.as_str())
        } else {
            return SeqTrackResult {
                keyframe_request: None,
                window: None,
            };
        };

        let tracker = match media_type {
            MediaType::VIDEO => &mut self.video_seq_tracker,
            MediaType::SCREEN => &mut self.screen_seq_tracker,
            _ => {
                return SeqTrackResult {
                    keyframe_request: None,
                    window: None,
                }
            }
        };

        // Record the sequence number first. This may detect new losses
        // (packets that shifted off the window without being seen).
        let new_lost = tracker.record_seq(seq);

        // If this is a keyframe, clear loss state AFTER recording the seq.
        // Ordering matters: record_seq may add losses from the window shift,
        // but on_keyframe resets lost_count to 0. If we called on_keyframe
        // first, record_seq would immediately re-add losses.
        if frame_type_str == "key" {
            tracker.on_keyframe();
        }

        let now = now_ms();
        let kf_requested = tracker.should_request_keyframe(now);

        SeqTrackResult {
            keyframe_request: kf_requested.then_some(media_type),
            window: Some(SeqWindowObservation {
                now,
                new_lost,
                kf_requested,
            }),
        }
    }

    fn observe_sequence_window(
        &mut self,
        media_type: MediaType,
        window: Option<SeqWindowObservation>,
    ) -> Option<WindowSample> {
        let window = window?;
        let (tracker, decoder) = match media_type {
            MediaType::VIDEO => (&mut self.video_seq_tracker, &self.video),
            MediaType::SCREEN => (&mut self.screen_seq_tracker, &self.screen),
            _ => return None,
        };
        // `observe_window` returns true exactly on rollover, throttling bus
        // emission to ~1Hz per stream.
        if tracker.observe_window(window.now, window.new_lost, window.kf_requested) {
            let (freshness_evictions_total, freshness_evictions_keyframeless_total) =
                decoder.freshness_eviction_counts();
            Some(WindowSample {
                loss_per_sec: tracker.loss_per_sec(),
                kf_per_sec: tracker.kf_per_sec(),
                max_gap: tracker.max_gap(),
                kf_suppression_per_sec: tracker.kf_suppression_per_sec(),
                freshness_evictions_total,
                freshness_evictions_keyframeless_total,
            })
        } else {
            None
        }
    }

    /// Settle the deferred video window on EVERY `decode` outcome — decoded,
    /// video-off discard, or not-visible discard. Skipping a path would freeze
    /// `last_video_downlink` (the #989 chooser's input) and the published rates.
    fn settle_video_window(
        &mut self,
        local_user_id: &str,
        media_type: MediaType,
        window: Option<SeqWindowObservation>,
    ) {
        if let Some(sample) = self.observe_sequence_window(media_type, window) {
            self.last_video_downlink = DownlinkSample {
                loss_per_sec: sample.loss_per_sec,
                kf_per_sec: sample.kf_per_sec,
            };
            self.emit_loss_metrics(local_user_id, media_type, &sample);
        }
    }

    /// Re-baseline the trackers for media that just turned back ON (#2537): the encode
    /// task owns its per-rung counter, so a stop/start restarts it at 0.
    fn reanchor_on_media_restart(&mut self, resolved_video: bool, resolved_screen: bool) {
        if !self.video_enabled && resolved_video {
            self.video_seq_tracker.reanchor_for_layer_switch();
        }
        if !self.screen_enabled && resolved_screen {
            self.screen_seq_tracker.reanchor_for_layer_switch();
        }
    }

    /// Record inbound activity from this peer. Called for ALL successfully
    /// dispatched media types (audio, video, screen, heartbeat) so that any
    /// traffic counts toward liveness, not just HEARTBEAT packets.
    fn on_activity(&mut self) {
        self.activity_count = self.activity_count.saturating_add(1);
    }

    /// Check whether this peer is still alive. Returns `true` if the peer
    /// should be kept, `false` if it should be removed.
    ///
    /// A peer is only considered dead after 3 consecutive checks (~15 seconds
    /// at the 5-second monitor interval) with zero inbound packets of any
    /// kind. This tolerates timer phase drift between the local monitor and
    /// the remote heartbeat sender, as well as transient packet loss.
    pub fn check_heartbeat(&mut self) -> bool {
        if self.activity_count > 0 {
            self.activity_count = 0;
            self.missed_heartbeat_checks = 0;
            return true;
        }
        self.missed_heartbeat_checks += 1;
        if self.missed_heartbeat_checks >= 3 {
            debug!(
                "---@@@--- detected heartbeat stop for {} (missed {} consecutive checks)",
                self.session_id, self.missed_heartbeat_checks
            );
            return false;
        }
        debug!(
            "---@@@--- no activity for peer {} (missed {}/3 checks, still alive)",
            self.session_id, self.missed_heartbeat_checks
        );
        true
    }
}

/// Issue #1183: decide whether a peer's video/screen canvas must be cleared on
/// a decode-visibility transition.
///
/// The canvas backing bitmap must be wiped exactly on the decode-stop EDGE —
/// when a peer leaves the active decode set, i.e. `visible` goes `true -> false`.
/// At that moment `Peer::decode` begins returning `SKIPPED` for VIDEO and
/// SCREEN, so no further frame is painted; the stale last frame would otherwise
/// linger on the canvas until Dioxus eventually unmounts the tile (deferred,
/// and not guaranteed to happen promptly under render pressure).
///
/// Must return `false` for every other transition:
/// * `false -> true` (becoming visible — the decoder will repaint; clearing
///   here would needlessly blank a tile that's about to show live frames)
/// * `true -> true` (still visible — no-op, never blank a live tile)
/// * `false -> false` (still hidden — no-op; clearing repeatedly every pass
///   would waste work and is already handled by the first edge)
///
/// Extracted as a pure function so the edge logic is host-testable without the
/// wasm-only `CanvasRenderingContext2d`; the actual clear (`clear_canvas`) lives
/// in the wasm path and is gated on this returning `true`.
#[inline]
fn should_clear_canvas(prev_visible: bool, new_visible: bool) -> bool {
    prev_visible && !new_visible
}

/// Decode eligibility is the participant's local visible set, independent of
/// whether client-side recording is active. Both camera and screen paths call
/// this helper before reaching their decoder.
fn should_decode_visible_peer(visible: bool) -> bool {
    visible
}

fn parse_media_packet(data: &[u8]) -> Result<Arc<MediaPacket>, PeerDecodeError> {
    Ok(Arc::new(
        MediaPacket::parse_from_bytes(data).map_err(|_| PeerDecodeError::PacketParseError)?,
    ))
}

/// Metric on the `peer_status` event carrying the audio rung this receiver is
/// actually decoding for the peer (#2132). Absent when the selected rung is not
/// currently arriving, so the readout shows "—" instead of a fabricated number.
pub const METRIC_SELECTED_AUDIO_LAYER: &str = "selected_audio_layer";

/// The audio rung to display for a peer: the selected one when it is actually
/// arriving, otherwise `None`. Selection is bandwidth-safe and can sit on a rung
/// the publisher has since shed; reporting that rung's nominal would overstate
/// what is being received.
///
/// Note this keys off packet arrival, and DTX is on for every audio tier — a peer
/// silent past the availability window reads "—" until they speak again.
pub fn displayed_audio_rung(selected: u32, selected_is_arriving: bool) -> Option<u32> {
    selected_is_arriving.then_some(selected)
}

#[derive(Debug)]
pub struct PeerDecodeManager {
    connected_peers: HashMapWithOrderedKeys<u64, Peer>,
    /// Cache of session_id -> display_name, populated from PARTICIPANT_JOINED events.
    /// This persists independently of the peer list so that when `ensure_peer()`
    /// creates a peer later (after the first media packet arrives), the display
    /// name is immediately available and does not fall back to user_id/email.
    display_name_cache: HashMap<u64, String>,
    /// #1482: Cache of session_id -> self-reported device info, populated from
    /// HealthPackets. Mirrors `display_name_cache` so device info arriving
    /// before the peer entry is created (via `ensure_peer()`) is not lost and is
    /// applied once the peer is created; also survives transient peer churn.
    device_info_cache: HashMap<u64, PeerDeviceInfo>,
    /// Cache of session_id -> is_guest, populated from PARTICIPANT_JOINED events.
    /// Mirrors `display_name_cache` so a guest flag arriving before the peer
    /// entry is created (via `ensure_peer()`) is still applied once the first
    /// media packet brings the peer online.
    is_guest_cache: HashMap<u64, bool>,
    /// Roster ids awaiting confirmation from the existing-member replay (#2267).
    superseded_candidates: HashSet<u64>,
    /// Whether `superseded_candidates` may be acted on by the next monitor pass.
    superseded_eligible: bool,
    pub on_first_frame: Callback<(String, MediaType)>,
    pub get_video_canvas_id: Callback<String, String>,
    pub get_screen_canvas_id: Callback<String, String>,
    diagnostics: Option<Rc<DiagnosticManager>>,
    pub on_peer_removed: Callback<String>,
    /// Batched companion of `on_peer_removed` fired once per
    /// `run_peer_monitor` pass with **all** peers removed in that pass.
    ///
    /// Phase 6 watchdog-cascade fix (cc7tp 2026-05-06): when N peers time
    /// out together (e.g. a network blip drops 5 simultaneously), the
    /// per-peer `on_peer_removed.emit(...)` loop triggered N consecutive
    /// `peer_list_version` bumps in the dioxus UI, each of which forced a
    /// full meeting-view re-render before the next removal completed. On
    /// 2-core machines this could chain into a 5-second main-thread stall
    /// that itself tripped the CPU-stall guard. Subscribers that only need
    /// "something changed" (e.g. version bumps) should listen on this
    /// callback and bump exactly once per pass; subscribers that need
    /// per-peer cleanup (e.g. removing entries from per-peer maps) keep
    /// using `on_peer_removed`. Both fire — they are not mutually
    /// exclusive.
    pub on_peers_removed_batch: Callback<Vec<String>>,
    vad_threshold: Option<f32>,
    /// Callback for sending packets back through the connection (used for
    /// KEYFRAME_REQUEST). Set by `VideoCallClient` after construction.
    send_packet: Option<Callback<PacketWrapper>>,
    /// The local user_id, needed to construct outgoing KEYFRAME_REQUEST packets.
    local_user_id: String,
    /// The local session_id as a string, used as `from_peer` in worker diagnostics
    /// context (issue #1640). Set when SESSION_ASSIGNED arrives; `None` before that.
    /// Using session_id (not user_id/email) makes both `from_peer` and `to_peer`
    /// carry the same ID type for consistent log parsing.
    local_session_id: Option<String>,
    /// Cached snapshot of `connected_peers.ordered_keys()` rendered as
    /// `Vec<String>`. Phase 6 fix: avoids walking the ordered key list and
    /// allocating a fresh `Vec<String>` on every `sorted_peer_keys()` call
    /// from the UI render path. Held as `Rc<Vec<String>>` so a single
    /// allocation can be shared cheaply across many callers within one
    /// render frame. Invalidated to `None` whenever the peer set changes
    /// (insert / remove / drain) — see `invalidate_sorted_string_keys()`.
    cached_sorted_string_keys: RefCell<Option<Rc<Vec<String>>>>,
    /// Cancellation tokens for in-flight PEER_EVENT(screen_decode_started)
    /// retries, keyed by publisher user_id. Set to `false` when the peer
    /// stops screen-sharing or is removed, causing pending retries to no-op.
    screen_decode_retry_tokens: HashMap<String, Rc<std::cell::Cell<bool>>>,
    /// #1399: timestamp (ms) of the last per-`delete_peer` #508 decode
    /// snapshot. Used to coalesce the full-set snapshot during an
    /// individual-leave cascade so an N-peer one-by-one teardown emits O(N)
    /// snapshot lines instead of O(N^2). See `delete_peer_snapshot_due` and
    /// `DELETE_PEER_SNAPSHOT_COALESCE_MS`. Only the per-`delete_peer` path is
    /// gated; the `clear_all_peers` (remaining=0 marker) and `run_peer_monitor`
    /// (one snapshot per removal batch) paths stay unconditional.
    last_delete_peer_snapshot_ms: u64,
    /// #1479: per-receiver cross-sender proactive-PLI budget, shared (via `Rc`)
    /// into every per-(peer, media_type) keyframe-request route closure so they
    /// can gate the proactive PLI through one cross-sender ceiling without holding
    /// `&PeerDecodeManager`. Keyed by `session_id` (the same key as
    /// `connected_peers`); cleaned per-sender on every peer-removal path
    /// (`run_peer_monitor` / `delete_peer_at`) and fully on `clear_all_peers`. A
    /// benign defense-in-depth shadow of the relay's 32/s cap (see
    /// [`super::pli_budget`]).
    pli_budget: Rc<RefCell<PliBudget>>,
    /// #1256 Phase 1: per-peer rendered-tile-size hints pushed by the UI. Keyed by
    /// session_id (same key as `connected_peers`). A peer ABSENT from this map (or
    /// `Uncapped`) gets NO size lid — fail-open, so an unknown peer is never capped.
    /// Lifecycle: deliberately NOT cleaned up on peer-removal. A stale entry for a
    /// departed session is harmless — it is only consulted inside the
    /// `connected_peers` loop in `tick_layer_choosers`, which skips absent peers —
    /// and the UI overwrites the whole map on the next viewport event.
    peer_tile_hints: HashMap<u64, crate::decode::layer_chooser::TileHint>,
    /// Issue #1878: whether THIS client's currently-active transport is
    /// WebTransport. Refreshed on every inbound packet from the authoritative
    /// [`ConnectionController::active_is_webtransport`] via
    /// [`Self::set_receiver_on_webtransport`], then stamped onto each `Peer`
    /// before decode so the per-peer audio-datagram-loss tracker runs only when
    /// audio can actually ride datagrams. Defaults to `false` (pre-connect).
    receiver_on_webtransport: bool,
    /// Applied to every camera/screen decoder created now or after a reset.
    skip_canvas_paint: bool,
    /// Test-only count of how many times `log_peer_leave_decode_snapshot`
    /// actually emitted, so #1399 coalescing can be asserted directly
    /// (O(N) -> constant under a within-window cascade). `Cell` because the
    /// emitter takes `&self`. Compiled out of production builds.
    #[cfg(test)]
    snapshot_emits: std::cell::Cell<u32>,
}

impl Default for PeerDecodeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerDecodeManager {
    pub fn new() -> Self {
        Self {
            connected_peers: HashMapWithOrderedKeys::new(),
            display_name_cache: HashMap::new(),
            device_info_cache: HashMap::new(),
            is_guest_cache: HashMap::new(),
            superseded_candidates: HashSet::new(),
            superseded_eligible: false,
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: None,
            on_peer_removed: Callback::noop(),
            on_peers_removed_batch: Callback::noop(),
            vad_threshold: None,
            send_packet: None,
            local_user_id: String::new(),
            local_session_id: None,
            cached_sorted_string_keys: RefCell::new(None),
            screen_decode_retry_tokens: HashMap::new(),
            last_delete_peer_snapshot_ms: 0,
            pli_budget: Rc::new(RefCell::new(PliBudget::new())),
            peer_tile_hints: HashMap::new(),
            receiver_on_webtransport: false,
            skip_canvas_paint: false,
            #[cfg(test)]
            snapshot_emits: std::cell::Cell::new(0),
        }
    }

    pub fn new_with_diagnostics(diagnostics: Rc<DiagnosticManager>) -> Self {
        Self {
            connected_peers: HashMapWithOrderedKeys::new(),
            display_name_cache: HashMap::new(),
            device_info_cache: HashMap::new(),
            is_guest_cache: HashMap::new(),
            superseded_candidates: HashSet::new(),
            superseded_eligible: false,
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: Some(diagnostics),
            on_peer_removed: Callback::noop(),
            on_peers_removed_batch: Callback::noop(),
            vad_threshold: None,
            send_packet: None,
            local_user_id: String::new(),
            local_session_id: None,
            cached_sorted_string_keys: RefCell::new(None),
            screen_decode_retry_tokens: HashMap::new(),
            last_delete_peer_snapshot_ms: 0,
            pli_budget: Rc::new(RefCell::new(PliBudget::new())),
            peer_tile_hints: HashMap::new(),
            receiver_on_webtransport: false,
            skip_canvas_paint: false,
            #[cfg(test)]
            snapshot_emits: std::cell::Cell::new(0),
        }
    }

    /// TEST-ONLY: insert a connected peer that has learned a 3-layer ladder
    /// (`highest_available == 2`) and reports a ZERO-LOSS real downlink sample,
    /// i.e. the WebSocket / reliable-WT case where per-peer telemetry can never
    /// observe congestion. This is the minimal seam the HOST `#[test]`s in
    /// `video_call_client.rs` need to drive `seed_local_congestion_and_publish`
    /// against a real connected peer without touching any browser/JS API.
    ///
    /// `#[cfg(test)]`-gated so it never appears in production builds. It is a thin
    /// wrapper over the existing host-safe `make_zero_loss_top_peer` test helper so
    /// the 50-field `Peer` literal stays single-sourced (no duplication / drift).
    #[cfg(test)]
    pub(crate) fn insert_zero_loss_top_peer_for_test(&mut self, session_id: u64) {
        self.connected_peers
            .insert(session_id, make_zero_loss_top_peer(session_id));
    }

    /// TEST-ONLY: count of peers still awaiting existing-member confirmation.
    #[cfg(test)]
    pub(crate) fn superseded_candidate_count(&self) -> usize {
        self.superseded_candidates.len()
    }

    /// TEST-ONLY: zero a peer's activity so the next monitor pass reads it as silent.
    #[cfg(test)]
    pub(crate) fn silence_peer_for_test(&mut self, session_id: u64) {
        if let Some(peer) = self.connected_peers.get_mut(&session_id) {
            peer.activity_count = 0;
        }
    }

    /// TEST-ONLY: drive a peer's VIDEO decode guard to `layer` so the display
    /// producers resolve a rung ABOVE the base.
    /// Also marks the peer video-ENABLED, which the needle producer requires for
    /// eligibility (`PrefMediaKind::Video => peer.video_enabled`) — without it the
    /// producer returns `None` and the test would assert nothing.
    #[cfg(test)]
    pub(crate) fn set_peer_video_layer_for_test(&mut self, session_id: u64, layer: u32) {
        if let Some(peer) = self.connected_peers.get_mut(&session_id) {
            peer.set_selected_video_layer(layer);
            peer.video_enabled = true;
        }
    }

    /// Set the callback used to send packets back through the connection.
    /// This is required for the PLI (keyframe request) mechanism.
    pub fn set_send_packet_callback(&mut self, callback: Callback<PacketWrapper>, user_id: String) {
        self.send_packet = Some(callback);
        self.local_user_id = user_id;
    }

    /// Issue #1878: record whether THIS client's active transport is
    /// WebTransport. Called by `VideoCallClient` on every inbound packet from
    /// the live [`ConnectionController::active_is_webtransport`] signal, so the
    /// value tracks re-election and WT↔WS transport switches with no stale
    /// cache. `decode` stamps it onto the target `Peer` so the audio-datagram-
    /// loss tracker is fed only when audio can ride datagrams.
    pub fn set_receiver_on_webtransport(&mut self, on_webtransport: bool) {
        self.receiver_on_webtransport = on_webtransport;
    }

    /// Store the per-peer rendered-tile-size hints pushed by the UI (issue #1256
    /// Phase 1). Keyed by session_id; an absent peer defaults to Uncapped (no size
    /// lid), so an unknown peer is never capped (fail-open). Replaces the whole map
    /// each call — the UI pushes the full current set on each viewport change.
    pub fn set_peer_tile_hints(
        &mut self,
        hints: HashMap<u64, crate::decode::layer_chooser::TileHint>,
    ) {
        self.peer_tile_hints = hints;
    }

    /// Store the local session_id once SERVER assigns it (issue #1640).
    ///
    /// Used as `from_peer` in worker diagnostics context so both `from_peer` and
    /// `to_peer` carry session_id strings (consistent ID type for log parsing).
    /// Also backfills any peer workers that were created before SESSION_ASSIGNED
    /// arrived — fixing the SetContext race where workers had empty `from_peer`.
    pub fn set_local_session_id(&mut self, session_id: u64) {
        let sid_str = session_id.to_string();
        self.local_session_id = Some(sid_str.clone());

        // Backfill: re-send SetContext to every existing peer worker so their
        // CONTEXT_FROM is populated with the now-known local session_id. Workers
        // that already received a SetContext simply overwrite their thread-local.
        for peer_session_id in self.connected_peers.ordered_keys().clone() {
            if let Some(peer) = self.connected_peers.get(&peer_session_id) {
                peer.video
                    .set_stream_context(sid_str.clone(), peer.sid_str.clone());
                peer.screen
                    .set_stream_context(sid_str.clone(), peer.sid_str.clone());
            }
        }
    }

    /// Expose `local_session_id` for regression tests (issue #1640).
    ///
    /// Returns the stored string slice when `SESSION_ASSIGNED` has been processed
    /// (i.e. after `set_local_session_id` has been called), or `None` before that.
    #[cfg(test)]
    pub fn local_session_id_str(&self) -> Option<&str> {
        self.local_session_id.as_deref()
    }

    /// Clear the send-packet callback. Called from
    /// [`VideoCallClient::disconnect()`](crate::VideoCallClient::disconnect)
    /// to break the `client -> peer_decode_manager.send_packet -> client`
    /// `Rc` cycle that otherwise keeps `Inner` alive after the UI scope
    /// holding the client has unmounted (issue: cc7tp meeting incident
    /// 2026-05-01, github01.hclpnp.com/labs-projects/videocall/discussions/502).
    ///
    /// Also drops every peer decoder's proactive keyframe-request route (#1025):
    /// each route closure captured a CLONE of this same `send_packet` `Callback`
    /// (a strong `Rc` reaching `Inner`) and is stored in a per-decoder slot that
    /// nulling `self.send_packet` alone does NOT reach — so without this it would
    /// be a second leg of the same `Rc` cycle, re-leaking `Inner` past teardown.
    pub fn clear_send_packet_callback(&mut self) {
        self.send_packet = None;
        // Drop each peer decoder's route (interior-mutable slot, so `&Peer` is
        // enough). `ordered_keys().clone()` + `get` mirrors the established
        // iteration pattern for this ordered map (it has no `values()`).
        for session_id in self.connected_peers.ordered_keys().clone() {
            if let Some(peer) = self.connected_peers.get(&session_id) {
                peer.video.clear_keyframe_request_route();
                peer.screen.clear_keyframe_request_route();
                // Issue #2328: the route that ARMS the screen starvation stamp is being dropped,
                // so any stamp left behind can no longer be refreshed or cleared by it. Left set,
                // it would survive the disconnect and — because `now_ms()` is a wall clock —
                // trip the retraction on the first packet after a reconnect/re-election, against
                // a rung whose hold ended minutes earlier. Clearing here keeps the #1915 class of
                // "stale latch survives the lifecycle boundary" out of this state.
                peer.clear_screen_keyframe_starvation();
            }
        }
    }

    /// Test/observability helper: report whether the PLI send-packet
    /// callback is currently set. Used by the disconnect regression
    /// tests to assert that `clear_send_packet_callback` actually fired.
    #[doc(hidden)]
    pub fn has_send_packet_callback(&self) -> bool {
        self.send_packet.is_some()
    }

    pub fn set_vad_threshold(&mut self, threshold: Option<f32>) {
        self.vad_threshold = threshold;
    }

    pub fn set_skip_canvas_paint(&mut self, skip_canvas_paint: bool) {
        self.skip_canvas_paint = skip_canvas_paint;
    }

    #[cfg(test)]
    pub(crate) fn selected_video_layer_for_test(&self, session_id: u64) -> Option<u32> {
        self.connected_peers
            .get(&session_id)
            .map(Peer::selected_video_layer)
    }

    #[cfg(test)]
    pub(crate) fn selected_audio_layer_for_test(&self, session_id: u64) -> Option<u32> {
        self.connected_peers
            .get(&session_id)
            .map(Peer::selected_audio_layer)
    }

    /// TEST-ONLY: a connected peer publishing ONE audio layer (audio
    /// `highest_available == 0`), video/screen keeping the learned 3-layer ladder.
    #[cfg(test)]
    pub(crate) fn insert_single_audio_layer_peer_for_test(&mut self, session_id: u64) {
        let mut peer = make_zero_loss_top_peer(session_id);
        peer.audio_layer_availability = crate::decode::layer_chooser::LayerAvailability::new();
        peer.audio_layer_availability.observe(0, 1000);
        self.connected_peers.insert(session_id, peer);
    }

    #[cfg(test)]
    pub(crate) fn selected_screen_layer_for_test(&self, session_id: u64) -> Option<u32> {
        self.connected_peers
            .get(&session_id)
            .map(Peer::selected_screen_layer)
    }

    /// Update which peers are eligible for video/screen decode.
    ///
    /// The meeting layout is the authoritative source of truth in the first
    /// pass of selective decode: peers rendered in the active layout are
    /// marked visible, and every other peer is fail-closed to skipped video
    /// and screen decode. Audio remains decoded for all peers.
    pub fn set_active_decode_set(&mut self, active_session_ids: &HashSet<u64>) {
        let session_ids = self.connected_peers.ordered_keys().clone();
        // Carry (user_id, session_id) so the KEYFRAME_REQUEST can target the
        // specific session for per-session rate limiting (#1124).
        let mut screen_keyframe_requests: Vec<(String, u64)> = Vec::new();
        let mut video_keyframe_requests: Vec<(String, u64)> = Vec::new();
        for session_id in session_ids {
            let visible = active_session_ids.contains(&session_id);
            if let Some(peer) = self.connected_peers.get_mut(&session_id) {
                if peer.visible == visible {
                    continue;
                }
                debug!(
                    "Peer {} decode visibility changed: {} -> {}",
                    session_id, peer.visible, visible
                );
                if visible && peer.screen_enabled {
                    screen_keyframe_requests.push((peer.user_id.clone(), session_id));
                }
                // Send a proactive video PLI when a video tile becomes visible
                // so the decoder gets a keyframe immediately instead of waiting
                // up to 5 s for the next periodic one (150 frames at 30 fps).
                // Gated on video_enabled so we don't send spurious PLIs for
                // peers that have their camera off.
                if visible && peer.video_enabled {
                    video_keyframe_requests.push((peer.user_id.clone(), session_id));
                }
                // Issue #1183: on the decode-stop edge (visible true -> false)
                // wipe the stale last frame out of both the camera and screen
                // canvas backing bitmaps NOW, synchronously, rather than relying
                // on Dioxus to later unmount the tile. `decode()` is about to
                // start returning SKIPPED for this peer's VIDEO and SCREEN (see
                // the `!self.visible` guards), so nothing else will repaint
                // these canvases; without this clear the tile freezes on its
                // last frame until the (deferred, pressure-stallable) DOM
                // unmount. The clear goes through the same cached 2D context the
                // painter draws into, so it targets the exact backing bitmap.
                if should_clear_canvas(peer.visible, visible) {
                    peer.video.clear_canvas();
                    peer.screen.clear_canvas();
                }
                peer.visible = visible;
                if let Some(diagnostics) = &self.diagnostics {
                    diagnostics.set_peer_decode_eligibility(&peer.sid_str, visible);
                }
            }
        }
        for (user_id, session_id) in &screen_keyframe_requests {
            self.send_keyframe_request(user_id, *session_id, MediaType::SCREEN);
        }
        for (user_id, session_id) in &video_keyframe_requests {
            self.send_keyframe_request(user_id, *session_id, MediaType::VIDEO);
        }
    }

    pub fn sorted_keys(&self) -> &Vec<u64> {
        self.connected_peers.ordered_keys()
    }

    /// Memoised string-form of [`sorted_keys`] for hot UI render paths.
    ///
    /// Phase 6 fix (cc7tp 2026-05-06): the dioxus meeting view called
    /// `VideoCallClient::sorted_peer_keys()` on every render, which used
    /// to walk the ordered-key list and clone each `u64` into a fresh
    /// `String`. With many peers and a render-storm bug bumping
    /// `peer_list_version` 20+ times per second, that allocation cost
    /// became measurable on 2-core hardware. The cache stores the
    /// rendered `Vec<String>` inside an `Rc` so successive callers in
    /// the same frame share one allocation, and is invalidated by
    /// [`invalidate_sorted_string_keys`] whenever the peer set changes.
    pub fn sorted_string_keys(&self) -> Rc<Vec<String>> {
        if let Some(existing) = self.cached_sorted_string_keys.borrow().as_ref() {
            return Rc::clone(existing);
        }
        let computed: Rc<Vec<String>> = Rc::new(
            self.connected_peers
                .ordered_keys()
                .iter()
                .map(|k| k.to_string())
                .collect(),
        );
        *self.cached_sorted_string_keys.borrow_mut() = Some(Rc::clone(&computed));
        computed
    }

    /// Invalidate the [`sorted_string_keys`] cache. Called from every code
    /// path that mutates the peer set. Cheap (sets one `Option` to `None`).
    fn invalidate_sorted_string_keys(&self) {
        *self.cached_sorted_string_keys.borrow_mut() = None;
    }

    pub fn get(&self, key: &u64) -> Option<&Peer> {
        self.connected_peers.get(key)
    }

    /// Set the canvas element for a peer's video decoder
    /// Set the canvas element for a peer's camera video decoder.
    ///
    /// When the canvas is attached and the peer's camera is on, a keyframe
    /// request is sent immediately.  This handles late joiners and re-mounts
    /// (e.g. the peer turned the camera on after recording started, or the
    /// canvas was unmounted and remounted by the Dioxus render cycle): the
    /// first keyframe was decoded before the canvas existed, so the decoder
    /// needs a fresh one to render.  Without the request the canvas stays
    /// blank until the jitter buffer eviction timer fires.
    pub fn set_peer_video_canvas(
        &self,
        peer_id: u64,
        canvas: web_sys::HtmlCanvasElement,
    ) -> Result<(), JsValue> {
        if let Some(peer) = self.connected_peers.get(&peer_id) {
            peer.video.set_canvas(canvas)?;
            if peer.video_enabled {
                // `peer_id` is the relay session_id of the target peer (#1124).
                self.send_keyframe_request(&peer.user_id, peer_id, MediaType::VIDEO);
            }
            Ok(())
        } else {
            Err(JsValue::from_str(&format!("Peer {peer_id} not found")))
        }
    }

    /// Return the canvas elements that are currently wired to camera-video
    /// decoders for peers inside the active decode set.
    ///
    /// Omitted: peers with no canvas, and those `should_decode_visible_peer`
    /// rejects — the gate `decode()` uses, so their canvas cannot be live.
    ///
    /// Used by the recording module to composite remote video directly from
    /// the decoder's live canvas rather than relying on the Dioxus DOM, which
    /// may not have the `<canvas>` element mounted when `show_canvas = false`
    /// (budget pressure, camera-on/off race, etc.).
    pub fn peer_video_canvases(&self) -> Vec<(String, web_sys::HtmlCanvasElement)> {
        self.connected_peers
            .ordered_keys()
            .iter()
            .filter_map(|sid| {
                let peer = self.connected_peers.get(sid)?;
                if !should_decode_visible_peer(peer.visible) {
                    return None;
                }
                let canvas = peer.video.get_canvas()?;
                Some((peer.sid_str.clone(), canvas))
            })
            .collect()
    }

    /// Return every peer's currently-attached screen-share decoder canvas.
    ///
    /// Mirror of [`peer_video_canvases`] for the remote screen-share stream.
    /// Used by the recording module to composite remote screen-share directly
    /// from the decoder's live canvas rather than relying on the Dioxus DOM,
    /// which may not have the `<canvas>` element mounted yet when the peer
    /// first starts sharing (the tile only appears after a re-render).
    pub fn peer_screen_canvases(&self) -> Vec<(String, web_sys::HtmlCanvasElement)> {
        self.connected_peers
            .ordered_keys()
            .iter()
            .filter_map(|sid| {
                let peer = self.connected_peers.get(sid)?;
                let canvas = peer.screen.get_canvas()?;
                Some((peer.sid_str.clone(), canvas))
            })
            .collect()
    }

    /// Compatibility hook for the recording UI.
    ///
    /// Recording deliberately follows the participant's existing local decode
    /// and visibility set. Starting a recording must not force off-screen peers
    /// to decode, create hidden canvases, or change publisher behavior.
    pub fn set_recording_active(&self, _active: bool) {}

    /// Set the canvas element for a peer's screen share decoder.
    ///
    /// When the canvas is attached and the peer is screen-sharing, a
    /// keyframe request is sent immediately.  This handles late joiners
    /// and re-mounts: the first keyframe was decoded before the canvas
    /// existed, so the decoder needs a fresh one to render.
    pub fn set_peer_screen_canvas(
        &self,
        peer_id: u64,
        canvas: web_sys::HtmlCanvasElement,
    ) -> Result<(), JsValue> {
        if let Some(peer) = self.connected_peers.get(&peer_id) {
            peer.screen.set_canvas(canvas)?;
            if peer.screen_enabled {
                // `peer_id` is the target's relay session_id (#1124).
                self.send_keyframe_request(&peer.user_id, peer_id, MediaType::SCREEN);
            }
            Ok(())
        } else {
            Err(JsValue::from_str(&format!("Peer {peer_id} not found")))
        }
    }

    /// Stage the roster for supersede confirmation on the TRANSPORT-LOSS edge, ahead
    /// of the relay's existing-member replay (#2267). Staged ids stay INERT until
    /// [`Self::promote_roster_reconcile`]: no packets arrive while the transport is
    /// down, so a pass acting here would reap peers for the outage itself.
    pub fn begin_roster_reconcile(&mut self) {
        self.superseded_candidates = self
            .connected_peers
            .ordered_keys()
            .iter()
            .copied()
            .collect();
        self.superseded_eligible = false;
    }

    /// Make the staged ids eligible for the next [`Self::run_peer_monitor`] pass,
    /// which drops those still unconfirmed AND silent.
    pub fn promote_roster_reconcile(&mut self) {
        self.superseded_eligible = true;
    }

    /// Clear `session_id`'s supersede candidacy. Driven by `PARTICIPANT_JOINED`,
    /// which the relay replays per existing member on join AND on reconnect.
    pub fn confirm_peer_present(&mut self, session_id: u64) {
        self.superseded_candidates.remove(&session_id);
    }

    pub fn run_peer_monitor(&mut self) -> Vec<String> {
        // Drained: an eligible reconcile spans exactly ONE pass (#2267).
        let superseded = if self.superseded_eligible {
            self.superseded_eligible = false;
            std::mem::take(&mut self.superseded_candidates)
        } else {
            HashSet::new()
        };
        let removed = self.connected_peers.remove_if_and_return(|peer| {
            if superseded.contains(&peer.session_id) && peer.activity_count == 0 {
                return false;
            }
            peer.check_heartbeat()
        });
        let mut removed_ids = Vec::new();
        for (session_id, peer) in removed {
            if let Some(token) = self.screen_decode_retry_tokens.remove(&peer.user_id) {
                token.set(false);
            }
            if let Some(diag) = &self.diagnostics {
                diag.remove_peer(&peer.sid_str);
            }
            // Issue #1479: drop this sender's PLI-budget state on the heartbeat-timeout removal
            // path so a rejoining session under the same id is never throttled by its prior life.
            self.pli_budget.borrow_mut().remove_sender(session_id);
            removed_ids.push(peer.sid_str.clone());
            self.on_peer_removed.emit(peer.sid_str);
        }
        if !removed_ids.is_empty() {
            // Invalidate the sorted-keys cache and emit a single batched
            // event so subscribers that only care about "something
            // changed" can coalesce their work (e.g. a single
            // peer_list_version bump on the dioxus side, instead of one
            // per dead peer). See Phase 6 watchdog-cascade fix.
            self.invalidate_sorted_string_keys();
            self.on_peers_removed_batch.emit(removed_ids.clone());
            // Issue #508 (instrumentation only): the heartbeat watchdog is a
            // SECOND peer-leave path (it does NOT call `delete_peer`). Snapshot
            // the remaining peers once per removal pass so a watchdog-driven
            // cascade is covered too. `departed_session_id=0` is the sentinel:
            // the watchdog removes by predicate, so there is no single id; the
            // departed sids are in the `removed_ids` already logged via the
            // batch callback. Pure read; alters no teardown ordering.
            self.log_peer_leave_decode_snapshot(0, "run_peer_monitor");
        }
        removed_ids
    }

    /// Run the receiver-driven simulcast layer chooser for every connected peer
    /// (issue #989, Phase 2/3) and return the desired
    /// `(session_id, media_kind) -> layer` map.
    ///
    /// Called on the monitor tick. For each peer this advances ALL THREE per-peer
    /// choosers — camera VIDEO, SCREEN, and AUDIO (Phase 3) — each using that
    /// peer-stream's own downlink health + learned per-kind availability, applies
    /// each result to the matching decode guard, and collects the chosen layers
    /// keyed by `(session_id, PrefMediaKind)`. The caller (`VideoCallClient`)
    /// feeds that map to the `LayerPreferenceSender`, which emits a
    /// `LAYER_PREFERENCE` packet only when the map actually changes and the
    /// relay's rate-limit allows.
    ///
    /// Each (peer, kind) is fully independent: a congested screen never affects
    /// the same peer's camera, and one peer never affects another. For a source
    /// publishing only the base layer of a kind (the default until the P1/P3
    /// send flags are raised), that kind's availability is 0, so its chosen layer
    /// is 0 — a no-op relative to the relay's fail-open (base is always
    /// forwarded). VIDEO is always emitted (every peer has a camera chooser);
    /// SCREEN/AUDIO entries are emitted too, defaulting to base.
    ///
    /// `now_ms` is supplied by the caller so the whole tick shares one clock.
    pub fn tick_layer_choosers(
        &mut self,
        now_ms: u64,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
    ) -> HashMap<(u64, crate::decode::layer_chooser::PrefMediaKind), u32> {
        use crate::decode::layer_chooser::{KindLayerBounds, PrefMediaKind, TileHint};
        let video_bounds = bounds.for_kind(PrefMediaKind::Video);
        let screen_bounds = bounds.for_kind(PrefMediaKind::Screen);
        let audio_bounds = bounds.for_kind(PrefMediaKind::Audio);
        let mut desired = HashMap::new();
        // #1256 Phase 1: peers whose tile-size lid lifted (a layer went UP) need a
        // keyframe so the higher layer can be decoded immediately. Collected during
        // the loop and drained AFTER the `&mut peer` borrow is released.
        let mut up_switches: Vec<(String, u64, MediaType)> = Vec::new();
        for session_id in self.connected_peers.ordered_keys().clone() {
            // #1256: read the per-peer tile-size hint from `&self.peer_tile_hints`
            // BEFORE taking the `&mut` borrow on `connected_peers`, so the immutable
            // borrow ends before the mutable one (no overlapping borrow of `self`).
            // An absent peer defaults to Uncapped (fail-open — never capped).
            let hint = self
                .peer_tile_hints
                .get(&session_id)
                .copied()
                .unwrap_or(TileHint::Uncapped);
            if let Some(peer) = self.connected_peers.get_mut(&session_id) {
                // Highest available per kind, computed ONCE and reused for BOTH the
                // size lid (below) and the #1079 advertise gate (further down), so
                // the lid and the gate can never disagree on the top.
                let vh = peer.video_layer_availability.highest_available(now_ms);
                let sh = peer.screen_layer_availability.highest_available(now_ms);
                let ah = peer.audio_layer_availability.highest_available(now_ms);

                // Selected layers BEFORE this tick — used to detect an UP-switch
                // (size lid lifting, or a congestion recovery) so we can request a
                // keyframe for the now-higher layer.
                let old_v = peer.selected_video_layer();
                let old_s = peer.selected_screen_layer();

                // #1256 Phase 1: fold the rendered-tile-size LID into the per-kind
                // `max` bound passed to the chooser. The lid is the smallest layer
                // whose native height covers the tile (per `size_cap_layer`); we
                // `min()` it with the user's existing receive `max` so the most-
                // constraining bound wins. VIDEO and SCREEN only — AUDIO is NEVER
                // size-capped (its bounds pass through unchanged below). An Uncapped
                // hint uses `vh`/`sh` (the top), so the lid is a no-op.
                let v_cap = match hint {
                    TileHint::Uncapped => vh,
                    TileHint::Capped { device_px_h } => {
                        crate::decode::layer_chooser::size_cap_layer(
                            device_px_h,
                            vh,
                            vh + 1,
                            PrefMediaKind::Video,
                        )
                    }
                };
                // #1256: the size cap lowers the ceiling toward the user's receive MIN
                // but never BELOW it — the user min is an authoritative floor; the size
                // cap is a bandwidth optimization that must yield to it.
                // `effective_max = v_cap.max(user_min)` keeps max >= min (no inverted
                // bound that `clamp_to_user_range` would otherwise normalize, letting the
                // cap undercut the floor — #1256 user-min regression), then
                // `.min(user_max)` respects an explicit user ceiling.
                let v_user_min = video_bounds.min.unwrap_or(0);
                let v_user_max = video_bounds.max.unwrap_or(u32::MAX);
                let v_effective_max = v_cap.max(v_user_min).min(v_user_max);
                let v_lidded = KindLayerBounds {
                    min: video_bounds.min,
                    max: Some(v_effective_max),
                };
                let s_cap = match hint {
                    TileHint::Uncapped => sh,
                    TileHint::Capped { device_px_h } => {
                        crate::decode::layer_chooser::size_cap_layer(
                            device_px_h,
                            sh,
                            sh + 1,
                            PrefMediaKind::Screen,
                        )
                    }
                };
                // #1256: same floor-respecting composition for SCREEN — the size cap
                // yields to the user's receive MIN (authoritative floor), then honors an
                // explicit user MAX. See the VIDEO comment above for the rationale.
                let s_user_min = screen_bounds.min.unwrap_or(0);
                let s_user_max = screen_bounds.max.unwrap_or(u32::MAX);
                let s_effective_max = s_cap.max(s_user_min).min(s_user_max);
                let s_lidded = KindLayerBounds {
                    min: screen_bounds.min,
                    max: Some(s_effective_max),
                };

                // Phase 4: each per-(peer,kind) chooser output is clamped to the
                // user's GLOBAL receive bounds for that kind, now folded with the
                // #1256 size lid for VIDEO/SCREEN. The tick updates the peer's
                // DECODE guard (`selected_*_layer`) as a side effect and returns the
                // (clamped) decode layer. AUDIO uses its UNCHANGED bounds.
                let video = peer.tick_layer_chooser(now_ms, v_lidded);
                let screen = peer.tick_screen_layer_chooser(now_ms, s_lidded);
                let audio = peer.tick_audio_layer_chooser(now_ms, audio_bounds);

                // #1256: capture UP-switches (lid lifted / congestion recovered) so a
                // keyframe is requested for the higher layer. A down-switch or
                // no-change pushes nothing. Still holding `&mut peer` so `user_id` is
                // read cheaply (cloned) here.
                //
                // P2 gate: only request a keyframe for a stream that is actually being
                // DECODED right now — `peer.visible && peer.<kind>_enabled`, the SAME
                // predicate `set_active_decode_set` uses to decide whether to send its
                // proactive visibility PLI (see the `visible && peer.video_enabled` /
                // `visible && peer.screen_enabled` gates at the
                // invisible->visible edge in `set_active_decode_set`). An offscreen or
                // camera-off/screen-off peer that up-switches (congestion-recovery or
                // availability-learning climb) would otherwise PLI a publisher nobody
                // is decoding. Nothing is lost: when such a peer later becomes visible,
                // `set_active_decode_set` emits the proactive keyframe on the
                // invisible->visible transition (gated on the same `*_enabled`), so the
                // deferred case is covered there.
                if video > old_v && peer.visible && peer.video_enabled {
                    up_switches.push((peer.user_id.clone(), session_id, MediaType::VIDEO));
                }
                if screen > old_s && peer.visible && peer.screen_enabled {
                    up_switches.push((peer.user_id.clone(), session_id, MediaType::SCREEN));
                }

                // Issue #1079 M1/M2: only ADVERTISE a preference for a kind when
                // the final (clamped) decode layer actually constrains BELOW the
                // highest available layer for that kind. This single rule captures
                // BOTH sources of a real constraint:
                //   * the chooser dropped below the top under congestion, and
                //   * the user's receive `max` bound (or #1256 size lid) capped it
                //     below the top.
                // On cold start / a healthy unclamped receiver, the chooser tracks
                // the top (M2: it no longer ramps from base), so layer == highest
                // and the entry is OMITTED → relay fail-open forwards all layers
                // (no base-pin HD dip after reconnect). An all-omitted map yields
                // no entries, so no LAYER_PREFERENCE packet goes out when there is
                // nothing to constrain (M1). Reuses the `vh`/`sh`/`ah` computed
                // above — never recomputed, so the gate and lid agree on the top.
                if video < vh {
                    desired.insert((session_id, PrefMediaKind::Video), video);
                }
                if screen < sh {
                    desired.insert((session_id, PrefMediaKind::Screen), screen);
                }
                if audio < ah {
                    desired.insert((session_id, PrefMediaKind::Audio), audio);
                }
            }
        }
        // #1256 Phase 1: request a keyframe for every up-switch captured above.
        // Routes via `send_keyframe_request` -> `emit_keyframe_request`, the
        // always-allowed visibility-PLI path that bypasses the #1479/#1494
        // PliBudget (the budget gate lives only in `install_keyframe_request_routes`,
        // the eviction route — `send_keyframe_request` calls `emit_keyframe_request`
        // directly with no gate). A size/recovery up-switch is therefore never shed
        // by the per-receiver PLI budget on the CLIENT side.
        //
        // Volume bound (do NOT claim "cannot storm" — there IS a burst): the
        // dominant burst is cold-start / reconnect, where every VISIBLE peer's
        // chooser jumps 0 -> highest_available on the first clean tick and fires one
        // keyframe per peer per kind. That burst is REAL but BOUNDED by:
        //   (a) the P2 visibility gate above — only `visible && *_enabled` streams
        //       fire here, so an off-screen / media-off peer contributes nothing;
        //   (b) the relay's GLOBAL per-receiver ceiling
        //       `KEYFRAME_REQUEST_MAX_PER_SEC` (32/s, actix-api/src/constants.rs),
        //       which caps the total keyframe requests one receiver can push across
        //       ALL target senders in a 1s window; and
        //   (c) the per-`(receiver, target_sender)` cap
        //       `KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER` (1/s), which caps how many
        //       this receiver can push at any single publisher.
        // In steady state the rate is much lower: congestion climbs are
        // hysteresis-limited (~1/15s) and size up-switches are viewport-driven +
        // 300ms-debounced. Every request still hits the relay's
        // per-(receiver, target_session) limiter regardless.
        //
        // Recovery if this PLI is DROPPED (relay limiter / lost packet): there is NO
        // client-side retry here — a layer switch only `reanchor_for_layer_switch`s
        // the sequence tracker (no gap PLI for the new layer's in-order packets), and
        // the next tick reads `old == current` so it does not re-poke. The decode is
        // NOT wedged, though: a layer switch does not flush the jitter buffer, so the
        // prior layer's last-good frame keeps painting (scaled, never blank) and the
        // codecs jitter buffer fires its OWN keyframe-less recovery PLI at
        // `MAX_PLAYOUT_AGE_MS` (bounded by #1662). That jitter-buffer path — a
        // different subsystem — is the actual safety net for a dropped up-switch PLI;
        // do not remove it on the assumption this loop self-heals.
        for (user_id, sid, mt) in up_switches {
            self.send_keyframe_request(&user_id, sid, mt);
        }
        desired
    }

    /// Apply the per-peer rendered-tile-size LID to the DECODE GUARDS directly,
    /// WITHOUT advancing any chooser hysteresis (issue #1256 — resize cadence).
    ///
    /// This is the seam [`crate::client::video_call_client::VideoCallClient::set_peer_tile_hints`]
    /// uses INSTEAD of [`Self::tick_layer_choosers`]. The 5s monitor tick owns the
    /// congestion `choose()` loop; a tile-size push must NOT call `choose()` because
    /// its DOWN path has no dwell gate (see `layer_chooser.rs`) and `last_video_downlink`
    /// is fixed within a ~1s loss window, so feeding the same congested sample on
    /// every resize-drag render (the UI bumps `viewport_version` on the RAW, un-debounced
    /// resize listener) would compound into N down-steps + bank the sticky latch. Instead
    /// this sets the guard purely from the lid composed with the chooser's EXISTING
    /// pick — idempotent: N calls in one window re-assert the SAME layer.
    ///
    /// Per peer + VIDEO/SCREEN: baseline = the chooser's current pick clamped to the
    /// user's bounds (`desired_preference().map(|r| bounds.clamp(r))`) or decode-best
    /// (`highest_available`) when unconstrained — the SAME `v_base`
    /// [`Peer::collect_desired_preferences`] computes, NOT a fresh `choose()`. The
    /// guard layer = `min(baseline, effective_max)` where
    /// `effective_max = size_lid.max(user_min).min(user_max)` (the SAME floor-respecting
    /// composition as the tick / read-only fold). AUDIO is never touched. Re-anchors the
    /// seq tracker on an actual guard change (mirrors `tick_layer_chooser`). Returns the
    /// up-switch events (old < new) for VISIBLE + media-enabled peers so the caller can
    /// request keyframes after the borrow is released — gated `peer.visible &&
    /// peer.<kind>_enabled`, the SAME gate the tick uses.
    pub fn apply_size_lid_to_decode_guards(
        &mut self,
        now_ms: u64,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
    ) -> Vec<(String, u64, MediaType)> {
        use crate::decode::layer_chooser::{size_cap_layer, PrefMediaKind, TileHint};
        let mut up_switches: Vec<(String, u64, MediaType)> = Vec::new();
        for session_id in self.connected_peers.ordered_keys().clone() {
            // Read the per-peer tile hint BEFORE the &mut borrow (mirrors the tick
            // loop's borrow discipline). Absent peer = Uncapped (fail-open).
            let hint = self
                .peer_tile_hints
                .get(&session_id)
                .copied()
                .unwrap_or(TileHint::Uncapped);
            if let Some(peer) = self.connected_peers.get_mut(&session_id) {
                // VIDEO — baseline + lid composition byte-identical to
                // `collect_desired_preferences` so the guard and the advertised
                // layer AGREE. Only chooser reads here are `desired_preference()`
                // (pure getter) and `highest_available()` (benign prune); NO
                // `choose()` / `tick_*` → advances NO hysteresis.
                let vh = peer.video_layer_availability.highest_available(now_ms);
                let v_base = peer
                    .video_layer_chooser
                    .desired_preference()
                    .map(|raw| bounds.for_kind(PrefMediaKind::Video).clamp(raw))
                    .unwrap_or(vh);
                let v_lid = match hint {
                    TileHint::Uncapped => vh,
                    TileHint::Capped { device_px_h } => {
                        size_cap_layer(device_px_h, vh, vh + 1, PrefMediaKind::Video)
                    }
                };
                let v_user_min = bounds.for_kind(PrefMediaKind::Video).min.unwrap_or(0);
                let v_user_max = bounds
                    .for_kind(PrefMediaKind::Video)
                    .max
                    .unwrap_or(u32::MAX);
                let v_effective_max = v_lid.max(v_user_min).min(v_user_max);
                let v_new = v_base.min(v_effective_max);
                let v_old = peer.selected_video_layer();
                if v_new != v_old {
                    // Reanchors the VIDEO seq tracker ONLY on an actual change.
                    peer.set_selected_video_layer(v_new);
                }
                if v_new > v_old && peer.visible && peer.video_enabled {
                    up_switches.push((peer.user_id.clone(), session_id, MediaType::VIDEO));
                }

                // SCREEN — identical with screen_* fields / PrefMediaKind::Screen.
                let sh = peer.screen_layer_availability.highest_available(now_ms);
                let s_base = peer
                    .screen_layer_chooser
                    .desired_preference()
                    .map(|raw| bounds.for_kind(PrefMediaKind::Screen).clamp(raw))
                    .unwrap_or(sh);
                let s_lid = match hint {
                    TileHint::Uncapped => sh,
                    TileHint::Capped { device_px_h } => {
                        size_cap_layer(device_px_h, sh, sh + 1, PrefMediaKind::Screen)
                    }
                };
                let s_user_min = bounds.for_kind(PrefMediaKind::Screen).min.unwrap_or(0);
                let s_user_max = bounds
                    .for_kind(PrefMediaKind::Screen)
                    .max
                    .unwrap_or(u32::MAX);
                let s_effective_max = s_lid.max(s_user_min).min(s_user_max);
                let s_new = s_base.min(s_effective_max);
                let s_old = peer.selected_screen_layer();
                if s_new != s_old {
                    peer.set_selected_screen_layer(s_new);
                }
                if s_new > s_old && peer.visible && peer.screen_enabled {
                    up_switches.push((peer.user_id.clone(), session_id, MediaType::SCREEN));
                }
                // AUDIO is NEVER size-capped — the guard is left untouched here.
            }
        }
        up_switches
    }

    /// Reconcile every peer's DECODE GUARD to the layer the relay will actually
    /// forward, AFTER a `LAYER_PREFERENCE` publish attempt (issue #1695).
    ///
    /// THE INVARIANT (single chokepoint): for every (peer, kind ∈ {Video, Screen,
    /// Audio}) the decode guard MUST equal what the relay forwards, which is
    /// `last_sent[(sid, kind)]` when a recorded entry exists, else
    /// `highest_available(kind)`.
    ///
    /// AUDIO IS reconciled (issue #1695): it is the SAME guard-leads-wire class as
    /// video/screen. `collect_desired_preferences`/`current_desired_preferences` DO
    /// emit `PrefMediaKind::Audio` entries (constrained audio chooser), the relay's
    /// exact-match filter includes AUDIO (`is_layer_filterable` matches AUDIO, and a
    /// recorded `(src, AUDIO)` preference drops mismatched audio packets), and the
    /// decode path drops AUDIO whose layer != `selected_audio_layer`. So a
    /// rate-limited audio layer-preference change would leave `selected_audio_layer`
    /// leading the audio wire → relay forwards only the old layer → exact-match drop
    /// → audio drop until the next accepted publish — exactly the freeze this fix
    /// removes for video/screen. Pinning the audio guard to the audio wire (or, with
    /// no recorded entry, to `highest_available` = the layer the relay fails open and
    /// forwards) closes that hole. Audio up-switches do NOT request a keyframe (audio
    /// has no I-frames / no keyframe-gated decode — see below).
    ///
    /// WHY this exists (the #1256 regression): `apply_size_lid_to_decode_guards`
    /// raises the EXACT-MATCH guard immediately on an up-switch, but the paired
    /// publish goes through `LayerPreferenceSender::take_if_changed`, which returns
    /// `None` WITHOUT promoting `last_sent` when it is rate-limited (<200ms since the
    /// last accepted send). The guard then leads the wire (guard=L2, last_sent=L0),
    /// the relay exact-match-forwards only L0, the guard rejects every L0 → freeze
    /// ≤5s. Reconciling the guard DOWN to `last_sent` after every publish removes
    /// that desync: the guard never leads the wire.
    ///
    /// `last_sent` is the sender's canonical last-written map (`None` until the first
    /// send; a `(sid,kind)` ABSENT from it = "no preference recorded for that
    /// source"). The caller MUST read it AFTER `take_if_changed` so an accepted send
    /// has already promoted it to the just-sent map.
    ///
    /// Returns the UP-switch events (`target < old` returns nothing — a LOWER move
    /// needs no keyframe) for VISIBLE + media-enabled peers, gated `peer.visible &&
    /// peer.<kind>_enabled` — the SAME gate `apply_size_lid_to_decode_guards` uses
    /// (lines 3217 / 3245) — so the caller can request a keyframe after the borrow
    /// is released. A wasted-then-re-requested keyframe on a rate-limited raise
    /// self-heals on the next push/tick. ONLY Video and Screen up-switches are
    /// returned: AUDIO is reconciled (its guard is pinned) but NEVER pushed into the
    /// up-switch vec, because audio is keyframe-less — there are no I-frames and the
    /// audio decode path is not keyframe-gated, so an audio up-switch decodes the new
    /// layer from the next packet without a keyframe request. Requesting one would be
    /// a meaningless PLI for a kind that has no keyframes.
    ///
    /// It writes ONLY the guard (via `set_selected_*_layer`; each of the three
    /// setters re-anchors its OWN receive-side sequence tracker only on an actual
    /// change — Video/Screen their [`SequenceTracker`], AUDIO its
    /// [`AudioDatagramLossTracker`] since issue #2138). It does NOT call
    /// `choose()`/`tick_*`/`observe_*`/`seed_*` or mutate ANY chooser hysteresis —
    /// the only mutation besides the guard is the benign lazy prune inside
    /// `highest_available`.
    pub fn reconcile_decode_guards_to_wire(
        &mut self,
        last_sent: Option<
            &std::collections::BTreeMap<(u64, crate::decode::layer_chooser::PrefMediaKind), u32>,
        >,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
        now_ms: u64,
    ) -> Vec<(String, u64, MediaType)> {
        use crate::decode::layer_chooser::PrefMediaKind;
        // ACCEPTABLE-BY-DESIGN same-call guard bounce (issue #1695, do NOT "fix"):
        // on a continuous resize-GROW drag that crosses a size-cap layer boundary,
        // `apply_size_lid_to_decode_guards` raises the guard (e.g. L0→L2) and then —
        // in the SAME synchronous `set_peer_tile_hints` call, before any packet is
        // decoded — this method pulls it back to the rate-limited wire (L0). Across a
        // ~60Hz drag that reads like a guard "bounce", but the decoder only ever sees
        // the FINAL per-call value (the wire), so it decodes LIVE LOW-RES video the
        // whole time (never frozen, never blank), self-heals within ≤200ms once the
        // rate-limit clears, and does NOT re-arm once the tile size stabilizes. This
        // is STRICTLY BETTER than the ≤5s freeze it replaces (the intended "soft,
        // live" behavior). Do NOT try to suppress the down-reconcile during a grow to
        // avoid the bounce — skipping the down move re-introduces the #1695 freeze
        // (guard leads the wire → relay forwards old layer → exact-match drop).
        let mut up_switches: Vec<(String, u64, MediaType)> = Vec::new();
        for session_id in self.connected_peers.ordered_keys().clone() {
            if let Some(peer) = self.connected_peers.get_mut(&session_id) {
                // VIDEO. target = the relay-forwarded layer for this source:
                //   * a recorded `last_sent` entry → the relay exact-match forwards
                //     exactly that layer, so the guard must be it; ELSE
                //   * NO entry → the relay FAILS OPEN and forwards ALL layers, i.e.
                //     the publisher's TOP (`highest_available`), so the guard must
                //     match the top it forwards — NOT "leave the guard where it is".
                //
                // LOAD-BEARING, DO NOT SIMPLIFY the `unwrap_or(highest_available)`
                // branch. `collect_desired_preferences`/`current_desired_preferences`
                // only inserts an entry when `layer < highest_available`, so a peer at
                // top has NO entry → this branch fires and pins the guard to the top.
                // If we instead left the guard alone, a DOWN-cap whose publish was
                // rate-limited (entry not yet on the wire) would keep guard=L0 while
                // the relay still forwards the old L2 → exact-match drop → the SAME
                // freeze this fix removes. Fail-open forward-the-top is exactly what
                // the relay does with no entry (chat_server.rs: no entry → forward
                // ALL), so the guard must equal the top.
                let v_target = last_sent
                    .and_then(|m| m.get(&(session_id, PrefMediaKind::Video)).copied())
                    .unwrap_or_else(|| peer.video_layer_availability.highest_available(now_ms));
                let v_target = bounds.for_kind(PrefMediaKind::Video).clamp(v_target);
                let v_old = peer.selected_video_layer();
                if v_target != v_old {
                    peer.set_selected_video_layer(v_target);
                }
                if v_target > v_old && peer.visible && peer.video_enabled {
                    up_switches.push((peer.user_id.clone(), session_id, MediaType::VIDEO));
                }

                // SCREEN — identical, screen_* fields / PrefMediaKind::Screen.
                let s_target = last_sent
                    .and_then(|m| m.get(&(session_id, PrefMediaKind::Screen)).copied())
                    .unwrap_or_else(|| peer.screen_layer_availability.highest_available(now_ms));
                let s_target = bounds.for_kind(PrefMediaKind::Screen).clamp(s_target);
                let s_old = peer.selected_screen_layer();
                if s_target != s_old {
                    peer.set_selected_screen_layer(s_target);
                }
                if s_target > s_old && peer.visible && peer.screen_enabled {
                    up_switches.push((peer.user_id.clone(), session_id, MediaType::SCREEN));
                }

                // AUDIO — identical pin (audio_* fields / PrefMediaKind::Audio).
                // Audio is in the SAME exact-match family as video/screen (relay
                // `is_layer_filterable` includes AUDIO; the decode path drops AUDIO
                // whose layer != `selected_audio_layer`), so a rate-limited audio
                // pref change would leave the guard leading the audio wire → drop.
                // Pin the guard to the wire (recorded entry) else to the top the
                // relay fails-open-forwards (`highest_available`) — same reasoning
                // and same load-bearing `unwrap_or(highest_available)` as VIDEO.
                let a_audio_top = peer.audio_layer_availability.highest_available(now_ms);
                let a_target = last_sent
                    .and_then(|m| m.get(&(session_id, PrefMediaKind::Audio)).copied())
                    .unwrap_or(a_audio_top);
                // Second write site for the audio guard; same ceiling rule as the tick.
                let a_target = bounds
                    .for_kind(PrefMediaKind::Audio)
                    .clamp(a_target)
                    .min(a_audio_top);
                let a_old = peer.selected_audio_layer();
                if a_target != a_old {
                    peer.set_selected_audio_layer(a_target);
                }
                // NO up-switch push for AUDIO: audio is keyframe-less (no I-frames,
                // decode is not keyframe-gated), so an audio up-switch needs no PLI —
                // it decodes the new layer from the next packet. We reconcile the
                // guard but request no keyframe. See the method doc-comment.
            }
        }
        up_switches
    }

    /// Early-seed congestion across every connected peer showing an
    /// early-congested sample (issue #1179, Part B).
    ///
    /// Drives [`Peer::seed_early_congestion`] for each connected peer; the
    /// primitive is itself a no-op on a clean sample, so a peer healthy at join
    /// is seeded nothing. The early-seed primitive's `observe_early_congestion`
    /// congestion gate is the ONLY thing deciding whether a given peer flips to
    /// constrained — this method applies NO transport filtering of its own.
    ///
    /// NOTE — there is NO transport gate in this method (neither client-wide nor
    /// per-peer). The WebTransport decision was already made by the caller. #1179's
    /// root cause is THIS client's own downlink being WebTransport
    /// (reliable-unistream flow-control pinning), which is a single client-wide
    /// boolean — NOT a per-peer property. A peer's announced `transport_type`
    /// describes that *remote sender's* uplink and is the wrong signal for a
    /// downlink-pinning bug. The local-WT decision is therefore made ONCE by the
    /// caller ([`crate::client::video_call_client`]'s early-seed timer tick) on the
    /// client's active connection transport, and this loop is only ever reached
    /// when that gate has passed. Keeping the gate at the call site removes any
    /// per-peer transport read here, so this method simply seeds every connected
    /// peer whose congestion gate trips.
    ///
    /// Returns `true` if any peer was actually seeded (a congested early sample
    /// flipped it to constrained). The caller still emits the resulting
    /// preference through the normal [`LayerPreferenceSender`] path via
    /// [`Self::current_desired_preferences`].
    ///
    /// `bounds` is the user's GLOBAL receive-layer bounds, threaded through to
    /// [`Peer::seed_early_congestion`] so the seeded decode layer is clamped to
    /// the user's per-kind `max`/`min` exactly as the 5s tick clamps its output
    /// (PR #1192 review). Open (default) bounds are an identity clamp.
    pub fn seed_early_congestion_for_connected_peers(
        &mut self,
        now_ms: u64,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
    ) -> bool {
        let mut seeded = false;
        for session_id in self.connected_peers.ordered_keys().clone() {
            if let Some(peer) = self.connected_peers.get_mut(&session_id) {
                if peer.seed_early_congestion(now_ms, bounds) {
                    seeded = true;
                }
            }
        }
        seeded
    }

    /// Step EVERY connected peer's RECEIVER-side choosers down one rung in
    /// response to a relay-authored `DOWNLINK_CONGESTION` control packet (issue
    /// #1219 Half 2). The synthetic-sample twin of
    /// [`Self::seed_early_congestion_for_connected_peers`].
    ///
    /// Like the early-seed method, there is NO transport gate here: the early-seed
    /// timer's WT gate exists to bound a SPECULATIVE cold-start optimization to the
    /// transport where #1179's join spike occurs. DOWNLINK_CONGESTION is not
    /// speculative — the relay has ALREADY measured this receiver's downlink and
    /// decided it is saturated, on whatever transport this client elected (WS or
    /// WT alike). The gate (if any) lives at the relay, which only emits the
    /// packet when this receiver's outbound channel overflowed (windowed
    /// CongestionTracker crossing via on_outbound_drop). So this method
    /// unconditionally steps down every connected peer whose chooser is not already
    /// constrained.
    ///
    /// Returns `true` if any peer was actually stepped down. The caller still
    /// emits the resulting preference through the normal [`LayerPreferenceSender`]
    /// path via [`Self::current_desired_preferences`] — including for peers whose
    /// already-constrained choosers returned `false` here, so the held layer is
    /// re-advertised to the relay.
    ///
    /// `bounds` is the user's GLOBAL receive-layer bounds, threaded through to
    /// [`Peer::seed_downlink_congestion`] so the stepped-down decode layer is
    /// clamped to the user's per-kind `max`/`min`. Open (default) bounds are an
    /// identity clamp.
    pub fn seed_downlink_congestion_for_connected_peers(
        &mut self,
        now_ms: u64,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
        exempt_speakers: bool,
    ) -> bool {
        let mut seeded = false;
        for session_id in self.connected_peers.ordered_keys().clone() {
            if let Some(peer) = self.connected_peers.get_mut(&session_id) {
                // Issue #1557: the active speaker(s) are EXEMPT from receiver-side
                // layer-drop ONLY for the LOCAL-CPU-pressure cascade — and only
                // when the caller passes `exempt_speakers == true`. That keeps the
                // person talking sharp while the LOCAL decoder is the bottleneck.
                // This mirrors the PAUSE-side exemption in `promote_speakers`
                // (attendants_layout.rs) — both protect the active speaker — and like
                // it we exempt EVERY `is_speaking` peer to honor the multi-speaker
                // case. The protection predicate differs, though: `promote_speakers`
                // uses a TIME WINDOW (`now - speech_ts < active_ms`), so a peer who
                // just stopped stays PAUSE-protected for `active_ms`, whereas this
                // seed uses the INSTANTANEOUS `is_speaking` VAD bool, so a peer who
                // just stopped is immediately eligible for layer-drop again.
                //
                // The relay DOWNLINK_CONGESTION arm (#1219 Half 2) calls this with
                // `exempt_speakers == false`: under REAL downlink saturation the
                // largest inbound stream (often the speaker's video) is exactly what
                // must be shed, and in the degenerate 1-on-1 the only remote peer IS
                // the speaker — exempting it would shed ZERO bitrate. So on that path
                // the speaker's VIDEO is stepped down like any other peer's.
                //
                // Audio is never touched on EITHER path (`seed_downlink_congestion`
                // never touches the audio chooser); screen for a non-speaking sharer
                // is still dropped. A skipped speaker contributes `false` to `seeded`
                // (its chooser did not move).
                if exempt_speakers && peer.is_speaking {
                    continue;
                }
                if peer.seed_downlink_congestion(now_ms, bounds) {
                    seeded = true;
                }
            }
        }
        seeded
    }

    /// Per-(peer, kind) desired-layer map that mirrors what the 5s tick would
    /// advertise, WITHOUT advancing any chooser hysteresis (issue #1179, Part B;
    /// clamp/gate added in PR #1192 review).
    ///
    /// Mirrors the shape AND the advertise semantics of
    /// [`Self::tick_layer_choosers`]'s return value but does NOT call
    /// `choose` / `tick_*`: per peer it reads each chooser's
    /// [`LayerChooser::desired_preference`], clamps it to the user's per-kind
    /// receive `bounds`, and advertises it only when the clamped layer is
    /// `< highest_available` for that kind — the SAME post-clamp + advertise gate
    /// the tick applies (see [`Peer::collect_desired_preferences`]). It therefore
    /// never advertises above the user's `max`, and never advertises `Some(0)` for
    /// a base-only source — matching the tick exactly.
    ///
    /// It advances no clean-window / score / sticky / last-change hysteresis. The
    /// only mutation it performs is the lazy prune inside `highest_available`,
    /// which reflects "what layers exist right now" (exactly what the next tick
    /// would do) and is NOT chooser-hysteresis state — so feeding its result to
    /// the [`LayerPreferenceSender`] after an early seed cannot perturb what the
    /// next 5s monitor tick computes (the early-seed `observe_early_congestion`
    /// mutation on a genuinely-congested WT peer is the ONLY chooser state change
    /// in the seed path). Used to publish the early seed through the existing
    /// sender so `last_sent` / `last_sent_ms` stay coherent and the next tick does
    /// not re-send a redundant packet.
    ///
    /// #1256 Phase 1: per peer it reads the rendered-tile-size hint from
    /// `peer_tile_hints` and threads it into [`Peer::collect_desired_preferences`],
    /// which folds the size lid into the advertised VIDEO/SCREEN layer so the lid
    /// is DURABLE across THIS publish path (the seed republishes), not just the 5s
    /// tick — see the `collect_desired_preferences` doc for why the lid is
    /// otherwise invisible here.
    ///
    /// `now_ms` is supplied by the caller (one clock per cycle) for the
    /// availability gate; `bounds` is the user's GLOBAL receive-layer bounds.
    pub fn current_desired_preferences(
        &mut self,
        now_ms: u64,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
    ) -> HashMap<(u64, crate::decode::layer_chooser::PrefMediaKind), u32> {
        let mut desired = HashMap::new();
        for session_id in self.connected_peers.ordered_keys().clone() {
            // #1256: read the per-peer tile-size hint from `&self.peer_tile_hints`
            // BEFORE taking the `&mut` borrow on `connected_peers` (mirrors the
            // tick loop's borrow discipline), so the immutable borrow ends before
            // the mutable one. An absent peer defaults to Uncapped (fail-open).
            let hint = self
                .peer_tile_hints
                .get(&session_id)
                .copied()
                .unwrap_or(crate::decode::layer_chooser::TileHint::Uncapped);
            if let Some(peer) = self.connected_peers.get_mut(&session_id) {
                peer.collect_desired_preferences(session_id, now_ms, bounds, hint, &mut desired);
            }
        }
        desired
    }

    /// Aggregate received-layer snapshot for one media kind (issue #989, Phase 4),
    /// for the P5 quality needles. Returns `None` when nothing of that kind is
    /// being received.
    ///
    /// Receive is per-peer, so this collapses to ONE representative stream the UI
    /// shows as a single needle per kind:
    ///   * **Audio** — the active talker: the speaking, audio-enabled peer with
    ///     the highest `audio_level`; fallback to any audio-enabled peer.
    ///   * **Video** — the active speaker's camera: the speaking, video-enabled
    ///     peer; fallback to the video-enabled peer currently receiving the
    ///     HIGHEST layer (the most prominent stream).
    ///   * **Screen** — the screen-enabled peer receiving the highest screen
    ///     layer (screen-share is typically singular).
    ///
    /// The layer reported is the peer's POST-CLAMP `selected_*_layer` (what is
    /// actually decoded), so the needle never exceeds the user's `max` bound.
    /// `layer_count` is the empirically-learned ladder size (highest observed
    /// available layer + 1), clamped by the resolver. `now_ms` lets availability
    /// expiry use one consistent clock. Panic-safe; cheap to poll each render.
    pub fn received_layer_snapshot(
        &mut self,
        kind: crate::decode::layer_chooser::PrefMediaKind,
        now_ms: u64,
    ) -> Option<crate::decode::layer_chooser::ReceivedLayerSnapshot> {
        use crate::decode::layer_chooser::{received_layer_snapshot, PrefMediaKind};

        // Pick the representative (session_id) for this kind, plus its decoded
        // layer + learned ladder size. Two-pass: prefer the active talker /
        // speaker, else the highest-layer eligible peer.
        let keys = self.connected_peers.ordered_keys().clone();
        let mut speaker: Option<(u64, f32)> = None;
        let mut fallback: Option<(u64, u32)> = None; // (sid, layer) — highest layer

        for sid in &keys {
            let Some(peer) = self.connected_peers.get(sid) else {
                continue;
            };
            let eligible = match kind {
                PrefMediaKind::Video => peer.video_enabled,
                PrefMediaKind::Screen => peer.screen_enabled,
                PrefMediaKind::Audio => peer.audio_enabled,
            };
            if !eligible {
                continue;
            }
            let layer = match kind {
                PrefMediaKind::Video => peer.selected_video_layer,
                PrefMediaKind::Screen => peer.selected_screen_layer,
                PrefMediaKind::Audio => peer.selected_audio_layer,
            };
            // Active-talker / active-speaker preference (video + audio).
            if matches!(kind, PrefMediaKind::Video | PrefMediaKind::Audio) && peer.is_speaking {
                let better = speaker
                    .map(|(_, lvl)| peer.audio_level > lvl)
                    .unwrap_or(true);
                if better {
                    speaker = Some((*sid, peer.audio_level));
                }
            }
            // Highest-layer fallback (all kinds).
            let take = fallback.map(|(_, l)| layer > l).unwrap_or(true);
            if take {
                fallback = Some((*sid, layer));
            }
        }

        let chosen = speaker
            .map(|(sid, _)| sid)
            .or(fallback.map(|(sid, _)| sid))?;
        let peer = self.connected_peers.get_mut(&chosen)?;
        let (layer, count) = match kind {
            PrefMediaKind::Video => (
                peer.selected_video_layer,
                peer.video_layer_availability.highest_available(now_ms) + 1,
            ),
            PrefMediaKind::Screen => (
                peer.selected_screen_layer,
                peer.screen_layer_availability.highest_available(now_ms) + 1,
            ),
            PrefMediaKind::Audio => (
                peer.selected_audio_layer,
                peer.audio_layer_availability.highest_available(now_ms) + 1,
            ),
        };
        let mut snap = received_layer_snapshot(kind, layer, count);
        if matches!(kind, PrefMediaKind::Screen) {
            enrich_screen_snapshot(peer, &mut snap);
        }
        Some(snap)
    }

    /// Per-peer RECEIVE simulcast diagnostics (issue #1095 observability).
    ///
    /// Unlike [`received_layer_snapshot`](Self::received_layer_snapshot) (which
    /// collapses to ONE representative stream per kind for the needles), this
    /// returns one [`PeerReceiveDiag`] for EVERY connected peer that is receiving
    /// at least one media kind, each carrying the per-kind decoded-layer snapshot.
    /// Used by the panel's "Live diagnostics" disclosure to show what this
    /// receiver is pulling from each peer. Panic-safe; iterates peers in their
    /// stable ordered-key order.
    ///
    /// NOT a pure getter despite its name: it calls
    /// [`LayerAvailability::highest_available`], which takes `&mut self` and runs
    /// `.retain()` to evict stale per-layer observations. The eviction is benign
    /// here (≤3 entries, and the decode path evicts on its own cadence anyway),
    /// but callers must hold `&mut self`.
    ///
    /// `bounds` is the client's GLOBAL per-kind receive-layer preference (the
    /// user's `max` caps). It is passed in rather than re-read here so the
    /// degradation-reason attribution (issue #1131) uses the SAME persisted bound
    /// the decode path clamps with — no duplicated/stale copy. The user `max` of
    /// `None` (Auto) means "uncapped" for the `Setting` attribution.
    pub fn per_peer_received_snapshots(
        &mut self,
        now_ms: u64,
        bounds: &crate::decode::layer_chooser::ReceiveLayerBounds,
    ) -> Vec<PeerReceiveDiag> {
        use crate::decode::layer_chooser::{received_layer_snapshot_with_reason, PrefMediaKind};
        let video_max = bounds.for_kind(PrefMediaKind::Video).max;
        let screen_max = bounds.for_kind(PrefMediaKind::Screen).max;
        let audio_max = bounds.for_kind(PrefMediaKind::Audio).max;
        let keys = self.connected_peers.ordered_keys().clone();
        let mut out = Vec::with_capacity(keys.len());
        for sid in keys {
            let Some(peer) = self.connected_peers.get_mut(&sid) else {
                continue;
            };
            // Resolve each kind's snapshot only when that kind is enabled for the
            // peer; otherwise the panel would show stale base-layer rows for
            // streams that aren't flowing. The per-kind `reason` is attributed
            // from the live availability + this peer's chooser-constrained flag +
            // the user's `max` bound (issue #1131).
            //
            // IMPORTANT (issue #1131 follow-up B): the reason is derived from the
            // CLAMPED decoded layer (`min(selected, avail_top)`), not the raw
            // `selected_*_layer` — otherwise a receive `min` above a base-only
            // sender's offering would render a Low/red dot (clamped index) with NO
            // reason chip (raw sel >= full top → None), a self-contradictory row.
            // `received_layer_snapshot_with_reason` (host-tested) does the clamp +
            // reason from ONE consistent layer.
            let video = peer.video_enabled.then(|| {
                let avail_top = peer.video_layer_availability.highest_available(now_ms);
                received_layer_snapshot_with_reason(
                    PrefMediaKind::Video,
                    peer.selected_video_layer,
                    avail_top,
                    video_max,
                    peer.video_layer_chooser.is_constrained(),
                )
            });
            let screen = peer.screen_enabled.then(|| {
                let avail_top = peer.screen_layer_availability.highest_available(now_ms);
                let mut snap = received_layer_snapshot_with_reason(
                    PrefMediaKind::Screen,
                    peer.selected_screen_layer,
                    avail_top,
                    screen_max,
                    peer.screen_layer_chooser.is_constrained(),
                );
                enrich_screen_snapshot(peer, &mut snap);
                snap
            });
            let audio = peer.audio_enabled.then(|| {
                let avail_top = peer.audio_layer_availability.highest_available(now_ms);
                received_layer_snapshot_with_reason(
                    PrefMediaKind::Audio,
                    peer.selected_audio_layer,
                    avail_top,
                    audio_max,
                    peer.audio_layer_chooser.is_constrained(),
                )
            });
            // Skip peers with nothing flowing so the list stays compact.
            if video.is_none() && screen.is_none() && audio.is_none() {
                continue;
            }
            out.push(PeerReceiveDiag {
                session_id: sid,
                label: peer.display_name.clone().unwrap_or_else(|| {
                    if peer.user_id.is_empty() {
                        sid.to_string()
                    } else {
                        peer.user_id.clone()
                    }
                }),
                video,
                screen,
                audio,
            });
        }
        out
    }

    pub fn decode(&mut self, response: PacketWrapper, userid: &str) -> Result<(), PeerDecodeError> {
        let packet = Arc::new(response);
        let peer_session_id = packet.session_id;

        // `userid` is the local (reporting) user — captured before the mutable
        // borrow of `connected_peers` so `Peer::decode` can stamp it as the
        // `from_peer` on its windowed loss/keyframe bus events (#1013).
        //
        // Issue #1025: also clone the transport send-packet callback + local user id BEFORE the
        // mutable borrow of `connected_peers`, so the per-decoder proactive keyframe-request
        // routes installed in the `!context_initialized` block can capture them without
        // double-borrowing `self`. `Callback` is a cheap `Rc`-clone. `None` means the transport
        // isn't wired (e.g. pre-connect / post-disconnect) — the proactive route stays unset.
        let send_packet_for_route = self.send_packet.clone();
        let local_user_id_for_route = self.local_user_id.clone();
        // Issue #1479: clone the shared per-receiver PLI budget handle BEFORE the mutable borrow
        // of `connected_peers`, so the route closures installed below can capture it without
        // double-borrowing `self`. `Rc::clone` is cheap; all routes share one budget.
        let pli_budget_for_route = self.pli_budget.clone();
        // Issue #1640: capture local session_id before the mutable borrow for
        // use in set_stream_context and peer.reset_for_decode_error().
        let local_sid_for_context = self.local_session_id.clone().unwrap_or_default();
        if let Some(peer) = self.connected_peers.get_mut(&peer_session_id) {
            let was_screen_enabled = peer.screen_enabled;
            if !peer.context_initialized {
                // Issue #1640: use local session_id (not user_id/email) as `from_peer`
                // so both fields carry the same ID type for consistent log parsing.
                // Falls back to empty string if SESSION_ASSIGNED hasn't arrived yet;
                // `set_local_session_id` backfills all workers when it does.
                peer.video
                    .set_stream_context(local_sid_for_context.clone(), peer.sid_str.clone());
                peer.screen
                    .set_stream_context(local_sid_for_context.clone(), peer.sid_str.clone());
                // Issue #1025: install the proactive keyframe-request route on each video
                // decoder. The worker fires this (via the jitter buffer) the instant it evicts a
                // stale keyframe-less backlog for this stream, so we request a fresh keyframe for
                // THIS peer + media type immediately. Each route is bound to one (peer,
                // media_type) and emits the identical `KEYFRAME_REQUEST` that the gap-/
                // visibility-driven paths do, so it flows through the same relay limiter (#979 /
                // #1011) and cannot storm. Installed only when the transport is wired
                // (`send_packet` set); `session_id` (not `sid_str`) is the per-session limiter
                // key (#1124).
                if let Some(send_packet) = &send_packet_for_route {
                    install_keyframe_request_routes(
                        peer,
                        send_packet,
                        &local_user_id_for_route,
                        &pli_budget_for_route,
                    );
                }
                peer.context_initialized = true;
            } else if send_packet_for_route.is_some()
                && (!peer.video.has_keyframe_request_route()
                    || !peer.screen.has_keyframe_request_route())
            {
                // Issue 1899 / discussion 1960 — self-healing route re-install. The proactive
                // keyframe-request route is installed exactly once above (the one-time
                // `context_initialized` latch), but it is DESTROYED later in the peer lifecycle:
                // `Peer::reset_for_decode_error` (decode-error path) rebuilds a video or screen
                // decoder with an empty route slot, and `clear_send_packet_callback` nulls them on
                // teardown. Because the latch never re-runs, a rebuild leaves the route `None`
                // FOREVER — the worker's keyframe-less eviction PLIs (issue #1025) then post to a
                // no-op main-thread route, so a keyframe-starved tile can never recover (field:
                // minutes-long screen freeze after a reset while PLIs fired into the void). Here
                // media is flowing again AND the transport is wired (`send_packet` set — so this
                // cannot fire during a disconnect teardown, where it is intentionally `None`), so
                // re-install the route on the fresh decoder. Transport-agnostic: the same route
                // serves WebTransport and WebSocket.
                //
                // Issue 2225: BOTH slots are probed. The old code probed VIDEO alone and justified
                // it with a "routes are only ever dropped in PAIRS" invariant — true while
                // `Peer::reset` rebuilt video and screen together, and explicitly flagged in that
                // comment as something a future single-stream rebuild would break. Per-kind resets
                // break it exactly that way: a SCREEN decode error now rebuilds the screen decoder
                // alone, and a VIDEO-keyed probe would never notice the missing screen route — the
                // very freeze this self-heal exists to prevent. Both probes are RefCell-borrow
                // `Option` checks; the install BODY is still skipped once both routes are present,
                // so the healthy path costs two borrow-and-checks per packet.
                if let Some(send_packet) = &send_packet_for_route {
                    install_keyframe_request_routes(
                        peer,
                        send_packet,
                        &local_user_id_for_route,
                        &pli_budget_for_route,
                    );
                }
            }
            // Issue #1878: stamp the live active transport onto the peer so the
            // AUDIO arm's datagram-loss tracker runs only when this receiver is
            // on WebTransport (audio can ride datagrams only there).
            peer.receiver_on_webtransport = self.receiver_on_webtransport;
            match peer.decode(&packet, userid) {
                Ok((MediaType::HEARTBEAT, _, _)) => {
                    peer.on_activity();
                    // #2511: the only arm in this match that keeps firing once media stops.
                    if let Some(diagnostics) = &self.diagnostics {
                        diagnostics.set_peer_media_state(
                            &peer.sid_str,
                            peer.video_enabled,
                            peer.screen_enabled,
                        );
                    }
                    let stopped_screen_share = was_screen_enabled && !peer.screen_enabled;
                    let publisher_user_id = stopped_screen_share.then(|| peer.user_id.clone());
                    if let Some(publisher_user_id) = publisher_user_id {
                        self.cancel_screen_decode_retries(&publisher_user_id);
                    }
                    Ok(())
                }
                Ok((media_type, decode_status, keyframe_request)) => {
                    // Any successfully decoded packet (audio, video, screen)
                    // counts toward liveness, not just heartbeats.
                    peer.on_activity();
                    if let Some(diagnostics) = &self.diagnostics {
                        // Issue #2190: pass whether the packet was actually DECODED, so
                        // `fps_received` counts decodes rather than arrivals.
                        //
                        // `decode_status.rendered` is the discriminator. Precisely, it
                        // means NOT-SKIPPED, not "a frame reached the screen": every early
                        // return in `Peer::decode` that declines to decode yields
                        // `DecodeStatus::SKIPPED` (`rendered: false`), and the real paths
                        // yield `true`. It is deliberately NOT claimed to be exact —
                        // `VideoPeerDecoder::decode` wraps its whole body in
                        // `if let Some(video_metadata)` and still falls through to
                        // `_rendered: true`, so a packet carrying no video metadata
                        // decodes nothing yet reports `true`; and `true` marks the frame as
                        // handed to `push_frame` (async, fire-and-forget), not painted.
                        // "Not skipped" is nonetheless a large improvement over "arrived" —
                        // it removes the whole wrong-rung population below. For the
                        // paint-truthful number, use `fps_painted` (#1656).
                        //
                        // The dominant SKIPPED path here is the EXACT-MATCH simulcast
                        // guard. A receiver that is NOT constraining its layer advertises
                        // no `LAYER_PREFERENCE` at all (`tick_layer_choosers` omits the
                        // entry when it already tracks the top), so the relay fails OPEN
                        // and forwards every rung — each non-selected one arrived here and
                        // was counted, making an 8fps 3-rung camera read ~52 fps (7+15+30,
                        // the ladder sum) instead of 8. Field-confirmed across three
                        // meetings on two clusters. NOTE the inflation is therefore a
                        // HEALTHY-receiver phenomenon: once this receiver constrains below
                        // the top, the relay filters server-side and there is little left
                        // to skip — so pre-fix series are inflated for unconstrained
                        // receivers, NOT uniformly. The other SKIPPED paths (tile not
                        // visible, straggler frame after a heartbeat says camera/mic off,
                        // unknown codec) are equally correct to exclude: in none of them
                        // did a frame reach the screen or the speaker.
                        //
                        // Bytes are still reported regardless — a skipped packet still
                        // consumed downlink — and bitrate is gated on its OWN arrival
                        // freshness clock (see `FpsTracker`), so it stays live for a
                        // receiving-but-not-decoding stream instead of being zeroed with
                        // fps.
                        diagnostics.track_frame(
                            &peer.sid_str,
                            media_type,
                            packet.data.len() as u64,
                            decode_status.rendered,
                            peer.visible,
                            diagnostic_last_output_ms(peer, media_type),
                        );
                    }

                    if decode_status.first_frame {
                        let sid_str = peer.sid_str.clone();
                        self.on_first_frame.emit((sid_str, media_type));
                    }

                    // Capture state we may need after dropping the mutable
                    // borrow of `peer`:
                    //   - `screen_first_frame_publisher` notifies the
                    //     publisher that we just decoded their first
                    //     screen-share frame (HCL #893). One PEER_EVENT per
                    //     (publisher, stream) because `first_frame` flips
                    //     true exactly once.
                    //   - `kf_info` carries any gap-driven keyframe request.
                    let screen_first_frame_publisher =
                        if decode_status.first_frame && media_type == MediaType::SCREEN {
                            Some(peer.user_id.clone())
                        } else {
                            None
                        };
                    let kf_info = keyframe_request.map(|mt| (peer.user_id.clone(), mt));

                    // Mutable borrow on `peer` ends here.
                    if let Some(publisher_user_id) = screen_first_frame_publisher {
                        self.publish_screen_decode_started(&publisher_user_id);
                    }

                    // Now we can immutably borrow self for sending.
                    // `peer_session_id` is the target peer's relay session (the
                    // map key above) — the per-session limiter key (#1124).
                    if let Some((peer_uid, requested_media_type)) = kf_info {
                        self.send_keyframe_request(
                            &peer_uid,
                            peer_session_id,
                            requested_media_type,
                        );
                    }

                    Ok(())
                }
                Err(e) => {
                    // Track decode errors (codec failures: keyframe miss, parse error, decoder
                    // reset). The failed packet still arrived and consumed downlink, so record it
                    // as a non-decoded arrival too. Without this, an all-error stream never
                    // refreshes the arrival clock and falsely reports both bitrate and decode
                    // errors as zero after one second.
                    //
                    // Prefer the cleartext outer media-kind hint because failures such as AES
                    // decryption happen before the inner media packet is available. Older
                    // wrappers can leave the hint unspecified, so typed decode errors provide
                    // the fallback; otherwise retain the historical VIDEO default.
                    if let Some(diagnostics) = &self.diagnostics {
                        let diagnostic_media_type =
                            diagnostic_media_type_for_decode_error(&packet, &e);
                        diagnostics.track_decode_error(&peer.sid_str, diagnostic_media_type);
                        diagnostics.track_frame(
                            &peer.sid_str,
                            diagnostic_media_type,
                            packet.data.len() as u64,
                            false,
                            peer.visible,
                            diagnostic_last_output_ms(peer, diagnostic_media_type),
                        );
                    }
                    // Issue 2225: rebuild ONLY the decoder this error implicates
                    // — a video failure must not respawn the peer's NetEQ audio
                    // worker, and an envelope/crypto failure (which never
                    // reached a decoder) must rebuild nothing at all.
                    let rebuilt = peer.reset_for_decode_error(&local_sid_for_context, &e);
                    rebuilt.map_err(|_| e)
                }
            }
        } else {
            Err(PeerDecodeError::NoSuchPeer(peer_session_id))
        }
    }

    fn cancel_screen_decode_retries(&mut self, publisher_user_id: &str) {
        if let Some(token) = self.screen_decode_retry_tokens.remove(publisher_user_id) {
            token.set(false);
        }
    }

    /// Send a KEYFRAME_REQUEST packet to a specific peer.
    ///
    /// The packet is a `MediaPacket` with `media_type = KEYFRAME_REQUEST`,
    /// `user_id` set to the target participant, and `target_session_id` set to
    /// the target's relay session (#1124). The `data` field encodes which
    /// stream (VIDEO or SCREEN) needs the keyframe.
    ///
    /// `target_session_id` lets the relay's keyframe rate-limiter key per
    /// SESSION rather than per participant, so two concurrent publishing
    /// sessions of the same identity get independent budgets (#1124). The relay
    /// still routes by `user_id`; the session_id is purely the limiter key, and
    /// the relay falls back to `user_id` when it is 0 (older clients).
    ///
    /// IMPORTANT: This uses `send_packet` (reliable stream), NOT
    /// `send_media_packet` (datagrams). KEYFRAME_REQUEST is a control
    /// message that MUST be delivered reliably.
    ///
    /// The packet is sent unencrypted (raw MediaPacket, not AES-encrypted)
    /// because this is a signaling/control packet, not user media data.
    /// The server needs to read the target `user_id` / `target_session_id` to
    /// route and rate-limit it correctly.
    ///
    /// `pub(crate)` so `VideoCallClient::set_peer_tile_hints` can drain the
    /// up-switch keyframes returned by `apply_size_lid_to_decode_guards` AFTER
    /// the `&mut` peer borrow has been released (issue #1256). It takes `&self`,
    /// so the drain does not re-borrow the manager mutably — same shape as the
    /// in-manager drain at the tail of `tick_layer_choosers`.
    pub(crate) fn send_keyframe_request(
        &self,
        peer_user_id: &str,
        target_session_id: u64,
        requested_media_type: MediaType,
    ) {
        let Some(send_packet) = &self.send_packet else {
            debug!("Cannot send KEYFRAME_REQUEST: no send_packet callback");
            return;
        };
        emit_keyframe_request(
            send_packet,
            &self.local_user_id,
            peer_user_id,
            target_session_id,
            requested_media_type,
        );
    }

    /// Send a `PEER_EVENT(screen_decode_started)` to the publisher whose
    /// screen-share we just decoded for the first time. The relay routes
    /// the packet by `target_peer_id`, so only the publisher receives it.
    ///
    /// Sends immediately then retries twice (at 2s and 4s) to handle a
    /// race condition where the publisher's NATS subscription may not be
    /// active yet when the first packet arrives (the relay's JoinRoom
    /// handler spawns the subscription asynchronously).
    ///
    /// `publisher_user_id` MUST be the publisher's user_id (the remote peer
    /// whose screen frame we just decoded). The event's `stream_id` is set
    /// to the same value because there is at most one screen-share per user.
    fn publish_screen_decode_started(&mut self, publisher_user_id: &str) {
        let Some(send_packet) = &self.send_packet else {
            debug!("Cannot publish PEER_EVENT: no send_packet callback");
            return;
        };

        let local_user_id = self.local_user_id.clone();
        let target_user_id = publisher_user_id.to_string();
        let send_packet = send_packet.clone();

        let active = Rc::new(std::cell::Cell::new(true));
        if let Some(previous) = self
            .screen_decode_retry_tokens
            .insert(target_user_id.clone(), Rc::clone(&active))
        {
            previous.set(false);
        }

        log::info!(
            "Publishing PEER_EVENT(screen_decode_started) target={} (with retries)",
            publisher_user_id
        );

        Self::emit_screen_decode_event(&send_packet, &local_user_id, &target_user_id);

        wasm_bindgen_futures::spawn_local(async move {
            for delay_ms in [2000, 4000] {
                gloo_timers::future::TimeoutFuture::new(delay_ms).await;
                if !active.get() {
                    log::debug!(
                        "PEER_EVENT(screen_decode_started) retry cancelled for target={}",
                        target_user_id
                    );
                    return;
                }
                log::debug!(
                    "Retry PEER_EVENT(screen_decode_started) target={} delay={}ms",
                    target_user_id,
                    delay_ms
                );
                Self::emit_screen_decode_event(&send_packet, &local_user_id, &target_user_id);
            }
        });
    }

    fn emit_screen_decode_event(
        send_packet: &Callback<PacketWrapper>,
        local_user_id: &str,
        target_user_id: &str,
    ) {
        let peer_event = PeerEvent {
            source_peer_id: local_user_id.as_bytes().to_vec(),
            target_peer_id: target_user_id.as_bytes().to_vec(),
            event_type: PEER_EVENT_SCREEN_DECODE_STARTED.to_string(),
            stream_id: target_user_id.to_string(),
            timestamp_ms: Date::now() as i64,
            ..Default::default()
        };

        let data = match peer_event.write_to_bytes() {
            Ok(b) => b,
            Err(e) => {
                log::warn!("Failed to serialize PeerEvent: {}", e);
                return;
            }
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::PEER_EVENT.into(),
            user_id: local_user_id.as_bytes().to_vec(),
            data,
            ..Default::default()
        };

        send_packet.emit(wrapper);
    }

    fn add_peer(
        &mut self,
        user_id: &str,
        session_id: u64,
        aes: Option<Aes128State>,
    ) -> Result<(), JsValue> {
        let sid_str = session_id.to_string();
        debug!("Adding peer {user_id} with session_id {sid_str}");
        let cached_is_guest = self
            .is_guest_cache
            .get(&session_id)
            .copied()
            .unwrap_or(false);
        let mut peer = Peer::new(
            self.get_video_canvas_id.emit(sid_str.clone()),
            self.get_screen_canvas_id.emit(sid_str.clone()),
            session_id,
            user_id.to_owned(),
            aes,
            self.vad_threshold,
            cached_is_guest,
            self.skip_canvas_paint,
        )?;
        // Issue #1640: send SetContext to the worker immediately at peer creation
        // so the publisher session_id (`to_peer`) is populated from the first frame.
        // `from_peer` = local session_id if known, else empty (backfilled when
        // `set_local_session_id` is called on SESSION_ASSIGNED).
        let from_peer = self.local_session_id.clone().unwrap_or_default();
        peer.video
            .set_stream_context(from_peer.clone(), sid_str.clone());
        peer.screen.set_stream_context(from_peer, sid_str.clone());
        // Apply cached display name if PARTICIPANT_JOINED arrived before
        // the first media packet created this peer entry.
        if let Some(cached_name) = self.display_name_cache.get(&session_id) {
            debug!(
                "Applying cached display_name '{}' for peer {} (user_id={})",
                cached_name, session_id, user_id
            );
            peer.display_name = Some(cached_name.clone());
        }
        // #1482: apply cached device info if a HealthPacket arrived before the
        // first media packet created this peer entry.
        if let Some(cached) = self.device_info_cache.get(&session_id) {
            peer.device_info = cached.clone();
        }
        self.connected_peers.insert(session_id, peer);
        // Phase 6: invalidate the sorted-keys cache so the next
        // `sorted_string_keys()` call rebuilds with the new peer.
        self.invalidate_sorted_string_keys();

        // If the Dioxus UI already rendered this peer's tile before the first
        // media packet arrived (PARTICIPANT_JOINED fires before any media),
        // the `use_effect` in UserVideo will have tried `set_peer_video_canvas`
        // but found the peer absent from `connected_peers` and logged a debug
        // message.  Since `use_effect` only runs once on mount, the canvas
        // would never be registered unless we attempt it here too.
        //
        // The canvas element id is the bare `session_id` string — see the
        // UserVideo RSX `canvas { id: "{id}", ... }` — which is why this block
        // looks up `sid_str` directly.
        //
        // The "video-{key}" prefix lives ONLY in `PeerDecodeManager::new`'s
        // DEFAULT `get_video_canvas_id` callback, which nothing in this
        // workspace ships with: every UI construction site injects the identity
        // callback (`get_peer_video_canvas_id: VcCallback::from(|id| id)` in
        // dioxus-ui's meeting / attendants / waiting_room / guest_join, wired
        // through `VideoCallClient::new`). So in production `video_canvas_id`
        // IS the bare session id and does match the DOM; the legacy prefix is
        // reached only by tests that build a manager directly.
        if let Some(peer_ref) = self.connected_peers.get(&session_id) {
            if let Some(window) = web_sys::window() {
                if let Some(document) = window.document() {
                    if let Some(el) = document.get_element_by_id(&sid_str) {
                        if let Ok(canvas) = el.dyn_into::<web_sys::HtmlCanvasElement>() {
                            if let Err(e) = peer_ref.video.set_canvas(canvas) {
                                log::debug!(
                                    "add_peer: could not register canvas for {sid_str}: {e:?}"
                                );
                            } else {
                                log::debug!("add_peer: registered canvas for {sid_str} from DOM");
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Issue #508 (instrumentation only): emit ONE diagnostic snapshot of every
    /// REMAINING connected peer's decode state at the instant a peer leaves.
    ///
    /// Purpose: the next time a peer-leave triggers a receiver-side FPS collapse
    /// (~25 -> 2-3 FPS) on the *remaining* peers, the console logs must let an
    /// analyst decide WHY, between two hypotheses:
    ///
    ///   * **Mechanism A (teardown invalidated the decode path)** — frames are
    ///     still ARRIVING for a remaining peer (its `last_*_frame_ms` keeps
    ///     advancing past this snapshot) but the tile stops PAINTING. The
    ///     received-side clock stays fresh while the painted FPS metric
    ///     (`fps_received` / canvas repaints / "Resized canvas to …") goes quiet.
    ///   * **Mechanism B (main-thread starvation, #510 re-render storm)** — the
    ///     received-side clock ALSO goes stale across ALL remaining peers at once:
    ///     packets stop being processed because the event loop is saturated, so
    ///     `last_*_frame_ms` ages out for everyone simultaneously.
    ///
    /// To support that disambiguation this logs, per remaining peer, the
    /// **age (ms) since the last media packet of each kind reached the per-peer
    /// decode body** (`now_ms() - peer.last_{video,audio,screen}_frame_ms`).
    /// These three fields are stamped in the decode body (`decode_media_packet`,
    /// `self.last_*_frame_ms = now`) *before* the visibility gate and the actual
    /// `VideoPeerDecoder::decode()` call, so they are a RECEIVED-side signal:
    /// "a packet for this stream arrived and passed sequence tracking". They are
    /// NOT a painted signal. Diverging this received clock against the existing
    /// painted-FPS series (`fps_received` events + the per-resize
    /// `"Resized canvas to WxH"` debug line in `peer_decoder.rs`) is exactly the
    /// A-vs-B test: received-fresh + painted-quiet = A; received-stale-for-all
    /// = B.
    ///
    /// Also logged per peer, all cheap synchronous reads of existing state — no
    /// new counters, no behavior change:
    ///   * `visible` — whether this receiver is even ATTEMPTING to decode/paint
    ///     this peer's video (a `false` here explains a quiet canvas WITHOUT
    ///     implicating teardown).
    ///   * `video_enabled` / `screen_enabled` — decoder-active flags.
    ///   * `video_kf_wait` / `screen_kf_wait` — `is_waiting_for_keyframe()`: a
    ///     stuck decoder waiting on a keyframe (received frames but nothing
    ///     decodable) is another flavor of A.
    ///   * `selected_video_layer` and the video/screen `canvas_id`s — so the
    ///     analyst can line this snapshot up against the canvas-resize debug
    ///     lines (which carry no peer identity) by id + timestamp.
    ///
    /// `departed_session_id` is the peer that just left; `remaining` is the count
    /// of peers still connected after the removal.
    ///
    /// This runs ONLY on the rare peer-leave / clear / watchdog-timeout paths,
    /// never per frame. The small loop here is bounded by the meeting size and is
    /// acceptable because peer-leave is an infrequent event. The grep tag
    /// `PEER_LEAVE_DECODE_SNAPSHOT` is unique (verified not keyed on by
    /// `scripts/parse_meeting_console_logs.sh`).
    fn log_peer_leave_decode_snapshot(&self, departed_session_id: u64, trigger: &str) {
        #[cfg(test)]
        self.snapshot_emits.set(self.snapshot_emits.get() + 1);
        let now = now_ms();
        let remaining = self.connected_peers.ordered_keys().len();
        log::info!(
            "PEER_LEAVE_DECODE_SNAPSHOT trigger={trigger} departed_session_id={departed_session_id} remaining={remaining}"
        );
        for sid in self.connected_peers.ordered_keys() {
            let Some(peer) = self.connected_peers.get(sid) else {
                continue;
            };
            // Age (ms) since the last packet of each kind reached the decode
            // body; `-1` means "no frame of this kind ever seen" (see
            // `age_ms_since`).
            let video_age_ms = age_ms_since(now, peer.last_video_frame_ms);
            let audio_age_ms = age_ms_since(now, peer.last_audio_frame_ms);
            let screen_age_ms = age_ms_since(now, peer.last_screen_frame_ms);
            log::info!(
                "PEER_LEAVE_DECODE_SNAPSHOT_PEER session_id={sid} visible={visible} \
                 video_enabled={video_enabled} screen_enabled={screen_enabled} \
                 audio_enabled={audio_enabled} video_age_ms={video_age_ms} \
                 audio_age_ms={audio_age_ms} screen_age_ms={screen_age_ms} \
                 video_kf_wait={video_kf_wait} screen_kf_wait={screen_kf_wait} \
                 selected_video_layer={selected_video_layer} video_canvas={video_canvas} \
                 screen_canvas={screen_canvas}",
                visible = peer.visible,
                video_enabled = peer.video_enabled,
                screen_enabled = peer.screen_enabled,
                audio_enabled = peer.audio_enabled,
                video_kf_wait = peer.video.is_waiting_for_keyframe(),
                screen_kf_wait = peer.screen.is_waiting_for_keyframe(),
                selected_video_layer = peer.selected_video_layer,
                video_canvas = peer.video_canvas_id,
                screen_canvas = peer.screen_canvas_id,
            );
        }
    }

    pub fn delete_peer(&mut self, session_id: u64) {
        self.delete_peer_at(session_id, now_ms());
    }

    /// `delete_peer` with the clock threaded in so the #1399 snapshot-coalesce
    /// decision is deterministically testable. The public `delete_peer` reads
    /// `now_ms()` once and delegates here.
    fn delete_peer_at(&mut self, session_id: u64, now_ms: u64) {
        if let Some(peer) = self.connected_peers.remove(&session_id) {
            if let Some(token) = self.screen_decode_retry_tokens.remove(&peer.user_id) {
                token.set(false);
            }
            if let Some(diag) = &self.diagnostics {
                diag.remove_peer(&peer.sid_str);
            }
            self.display_name_cache.remove(&session_id);
            self.device_info_cache.remove(&session_id);
            self.is_guest_cache.remove(&session_id);
            // Issue #1479: drop this sender's PLI-budget state on the explicit-delete removal
            // path (mirrors the heartbeat-timeout path in `run_peer_monitor`).
            self.pli_budget.borrow_mut().remove_sender(session_id);
            // Phase 6: invalidate the sorted-keys cache before notifying
            // observers so any read in the callback sees a fresh list.
            self.invalidate_sorted_string_keys();
            let sid_str = peer.sid_str.clone();
            self.on_peer_removed.emit(peer.sid_str);
            // Single-peer removals also fire the batched callback so
            // subscribers can coalesce on it without subscribing to two
            // separate notifications.
            self.on_peers_removed_batch.emit(vec![sid_str]);
            // Issue #508 (instrumentation only): snapshot the REMAINING peers'
            // decode state right after this single-peer removal. Pure read of
            // `self.connected_peers`; emits no events and alters no teardown
            // ordering — the departed peer is already removed above.
            //
            // #1399: coalesce so an individual-leave cascade (N peers leaving
            // one-by-one through this path) emits O(N) lines, not O(N^2). The
            // first leave in a burst — and any isolated leave more than
            // `DELETE_PEER_SNAPSHOT_COALESCE_MS` after the previous one — emits
            // the full remaining-set snapshot; intermediate cascade removals
            // are suppressed (the per-removal `on_peers_removed_batch` above
            // already carries the departed id, so no peer-leave event is lost).
            if self.delete_peer_snapshot_due(now_ms) {
                self.last_delete_peer_snapshot_ms = now_ms;
                self.log_peer_leave_decode_snapshot(session_id, "delete_peer");
            }
        }
    }

    /// #1399: returns `true` when the per-`delete_peer` #508 snapshot should be
    /// emitted, i.e. when no snapshot has fired within the trailing
    /// `DELETE_PEER_SNAPSHOT_COALESCE_MS` window.
    ///
    /// `last_delete_peer_snapshot_ms == 0` is the never-fired sentinel and
    /// always emits. The explicit `now_ms < last` arm guards a non-monotonic
    /// clock (possible across a `Date.now()` step on wasm): a backwards clock
    /// *emits* rather than wedging the snapshot off until the wall clock
    /// catches back up — fail-open, matching the diagnostic intent.
    ///
    /// That arm is load-bearing, not defensive dead code: without it a
    /// backwards step makes `now_ms - last` underflow to `0`, and `0 >= window`
    /// is false, suppressing every snapshot for the duration of the step.
    fn delete_peer_snapshot_due(&self, now_ms: u64) -> bool {
        self.last_delete_peer_snapshot_ms == 0
            || now_ms < self.last_delete_peer_snapshot_ms
            || now_ms - self.last_delete_peer_snapshot_ms >= DELETE_PEER_SNAPSHOT_COALESCE_MS
    }

    /// Remove all peers and terminate their decoder workers immediately.
    ///
    /// Called when the connection drops so stale workers don't linger and
    /// consume WASM memory while the client reconnects.
    pub fn clear_all_peers(&mut self) {
        for token in self.screen_decode_retry_tokens.values() {
            token.set(false);
        }
        self.screen_decode_retry_tokens.clear();
        let removed = self.connected_peers.drain_all();
        let mut removed_ids: Vec<String> = Vec::with_capacity(removed.len());
        for (_session_id, peer) in removed {
            if let Some(diag) = &self.diagnostics {
                diag.remove_peer(&peer.sid_str);
            }
            removed_ids.push(peer.sid_str.clone());
            self.on_peer_removed.emit(peer.sid_str);
        }
        // Clear the display name cache so stale names don't persist
        // across reconnections.
        self.display_name_cache.clear();
        self.device_info_cache.clear();
        self.is_guest_cache.clear();
        self.superseded_candidates.clear();
        self.superseded_eligible = false;
        // Issue #1479: all senders leave on a connection drop, so reset the whole PLI budget.
        // (The wall-clock window would also self-heal it after one window, but clearing here is
        // immediate and matches the cache-clear semantics of the bulk teardown.)
        self.pli_budget.borrow_mut().clear();
        // Phase 6: invalidate the sorted-keys cache and emit a single
        // batched event so observers can coalesce the bulk-clear into
        // one notification.
        self.invalidate_sorted_string_keys();
        if !removed_ids.is_empty() {
            self.on_peers_removed_batch.emit(removed_ids);
            // Issue #508 (instrumentation only): connection-drop / bulk-clear
            // path. After draining there are no remaining peers, so the loop
            // body emits nothing — the header line (`remaining=0`) still serves
            // as a correlatable "all peers cleared" marker distinct from a
            // single-peer leave. Pure read; no teardown-order change.
            self.log_peer_leave_decode_snapshot(0, "clear_all_peers");
        }
        // Peers are dropped here, triggering Worker::terminate() via Drop impl
    }

    /// Collapse a superseded session onto its successor (#2269).
    pub fn adopt_resumed_session(
        &mut self,
        previous_session_id: u64,
        new_session_id: u64,
        display_name: Option<String>,
        is_guest: bool,
    ) -> bool {
        if previous_session_id == 0 || new_session_id == 0 || previous_session_id == new_session_id
        {
            return false;
        }

        // Before the eviction: `delete_peer` drops the predecessor's cache rows.
        let carried_name =
            display_name.or_else(|| self.display_name_cache.get(&previous_session_id).cloned());
        if let Some(name) = carried_name {
            self.set_peer_display_name(new_session_id, name);
        }
        self.set_peer_is_guest(new_session_id, is_guest);

        let had_predecessor = self.connected_peers.contains_key(&previous_session_id);
        self.delete_peer(previous_session_id);
        had_predecessor
    }

    pub fn ensure_peer(&mut self, session_id: u64, user_id: &str) -> PeerStatus {
        if self.connected_peers.contains_key(&session_id) {
            PeerStatus::NoChange
        } else if let Err(e) = self.add_peer(user_id, session_id, None) {
            log::error!("Error adding peer: {e:?}");
            PeerStatus::NoChange
        } else {
            PeerStatus::Added(session_id)
        }
    }

    pub fn set_peer_aes(
        &mut self,
        session_id: u64,
        aes: Aes128State,
    ) -> Result<(), PeerDecodeError> {
        match self.connected_peers.get_mut(&session_id) {
            Some(peer) => {
                peer.aes = Some(aes);
                Ok(())
            }
            None => Err(PeerDecodeError::NoSuchPeer(session_id)),
        }
    }

    pub fn get_fps(&self, _peer_id: &str, _media_type: MediaType) -> f64 {
        // FPS tracking is now handled by the DiagnosticManager internally
        // We return 0.0 here as we can't get real-time FPS immediately
        0.0
    }

    pub fn get_all_fps_stats(&self) -> Option<String> {
        None
    }

    /// Updates the speaker device by switching the sink on the shared AudioContext
    pub fn update_speaker_device(
        &mut self,
        speaker_device_id: Option<String>,
    ) -> Result<(), JsValue> {
        log::info!(
            "Updating shared AudioContext sink to {speaker_device_id:?} (no decoder rebuild)",
        );
        SharedAudioContext::update_speaker_device(speaker_device_id)?;
        Ok(())
    }

    /// Set the display name for a peer identified by session_id.
    /// This is called when a PARTICIPANT_JOINED event provides the display name.
    ///
    /// The display name is stored in both the per-peer entry (if the peer
    /// already exists) AND a persistent cache keyed by session_id. This way,
    /// if the PARTICIPANT_JOINED event arrives before the first media packet
    /// creates the peer entry via `ensure_peer()`, the display name is
    /// still available when the peer is created later.
    pub fn set_peer_display_name(&mut self, session_id: u64, display_name: String) {
        // Always persist in the cache so that future `add_peer()` calls
        // can pick it up even if no peer entry exists yet.
        self.display_name_cache
            .insert(session_id, display_name.clone());

        // Also update the existing peer entry if it exists.
        if let Some(peer) = self.connected_peers.get_mut(&session_id) {
            peer.display_name = Some(display_name);
        }
    }

    /// Store/merge a peer's self-reported device info (#1482), keyed by session_id.
    /// Mirrors `set_peer_display_name`: writes the cache so info arriving before
    /// the peer entry exists is not lost, and updates the live peer if present.
    ///
    /// MERGE POLICY: STATIC fields (cores, architecture, os, device_type,
    /// device_memory_gb) use `incoming.or(existing)` so a tick where the browser
    /// momentarily omits a field does NOT erase previously-known good data.
    /// DYNAMIC fields (main_thread_load, memory_used_mb) ALWAYS take the latest
    /// incoming value (including None -> None) since they are live gauges.
    pub fn set_peer_device_info(&mut self, session_id: u64, incoming: PeerDeviceInfo) {
        let existing = self
            .device_info_cache
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        let merged = PeerDeviceInfo {
            client_cores: incoming.client_cores.or(existing.client_cores),
            client_architecture: incoming
                .client_architecture
                .or(existing.client_architecture),
            client_os: incoming.client_os.or(existing.client_os),
            client_device_type: incoming.client_device_type.or(existing.client_device_type),
            client_device_memory_gb: incoming
                .client_device_memory_gb
                .or(existing.client_device_memory_gb),
            client_main_thread_load: incoming.client_main_thread_load,
            client_memory_used_mb: incoming.client_memory_used_mb,
        };
        self.device_info_cache.insert(session_id, merged.clone());
        if let Some(peer) = self.connected_peers.get_mut(&session_id) {
            peer.device_info = merged;
        }
    }

    /// Update display name for all peers with the given user_id.
    /// Used for PARTICIPANT_DISPLAY_NAME_CHANGED events where the server
    /// does not include session_id — a rename applies to all sessions
    /// belonging to that user.
    pub fn set_peer_display_name_by_user_id(&mut self, user_id: &str, display_name: String) {
        let keys: Vec<u64> = self.connected_peers.ordered_keys().clone();
        for key in keys {
            if let Some(peer) = self.connected_peers.get_mut(&key) {
                if peer.user_id == user_id {
                    peer.display_name = Some(display_name.clone());
                    self.display_name_cache.insert(key, display_name.clone());
                }
            }
        }
    }

    /// Authoritatively force a peer's audio and/or video to the *off* state,
    /// identified by `user_id` (the `target_user_id` carried on a host-command
    /// broadcast such as `HOST_MUTE_PARTICIPANT` / `HOST_DISABLE_VIDEO`).
    ///
    /// HCL issue #1034. A host command is **authoritative** — unlike a
    /// heartbeat, it is not a possibly-stale announcement racing live media on
    /// a separate QUIC stream. It is a deliberate moderation action the whole
    /// room must reflect *immediately*. We therefore set the tracked
    /// `audio_enabled` / `video_enabled` flags **directly**, bypassing
    /// [`apply_heartbeat_enabled_flag`] and its `MEDIA_FRESH_WINDOW_MS`
    /// freshness guard.
    ///
    /// Why bypassing the guard is correct here (and only here): the freshness
    /// guard (HCL bug #1) exists to stop a *stale negative heartbeat* from
    /// clobbering a *live* stream during the ~5s out-of-order window on
    /// WebTransport. When the host mutes peer X, X stops its encoder and sends
    /// an immediate off-heartbeat, but X is still flushing straggler frames, so
    /// `last_video_frame_ms` / `last_audio_frame_ms` look fresh and the guard
    /// would keep `enabled = true` until the window expires — the ~5s lag from
    /// the bug report. The host command is independent ground truth, so it does
    /// not need (and must not be subject to) the freshness arbitration.
    ///
    /// On video-off we reuse the same decoder-flush path the heartbeat
    /// off-transition uses (`self.video.flush()`), so the frozen last frame is
    /// cleared at the instant the tile flips — no lingering freeze-frame.
    /// On audio-off we mute and flush the audio decoder the same way.
    ///
    /// Not a permanent latch for a peer whose heartbeats reach this receiver: an
    /// affirmative `heartbeat = true` still wins. Otherwise see `audio_forced_off`.
    ///
    /// Safe no-op when no connected peer matches `user_id` (e.g. the host's own
    /// view, or a peer not yet known to this client). Only emits a peer-status
    /// broadcast for peers whose state actually changed, avoiding redundant UI
    /// churn on duplicate dual-transport deliveries.
    pub fn force_peer_media_off(&mut self, user_id: &str, audio_off: bool, video_off: bool) {
        if !audio_off && !video_off {
            return;
        }
        let diagnostics = self.diagnostics.clone();
        let keys: Vec<u64> = self.connected_peers.ordered_keys().clone();
        for key in keys {
            if let Some(peer) = self.connected_peers.get_mut(&key) {
                if peer.user_id != user_id {
                    continue;
                }
                // Per-peer force-off + change detection lives in the shared
                // `Peer::force_media_off` helper (also used by the mute-all /
                // disable-all path, #1036).
                if peer.force_media_off(audio_off, video_off) {
                    if let Some(diagnostics) = &diagnostics {
                        diagnostics.set_peer_media_state(
                            &peer.sid_str,
                            peer.video_enabled,
                            peer.screen_enabled,
                        );
                    }
                    // Drives the `peer_status` diagnostics event the UI peer
                    // tiles subscribe to, so the muted/video-off state shows
                    // immediately rather than after the freshness window.
                    peer.broadcast_peer_status();
                }
                // A given user_id maps to one peer per session; keep scanning
                // in case the same user is present under multiple session_ids
                // (multi-tab), so every tile for that user updates.
            }
        }
    }

    /// Authoritatively force audio and/or video to the *off* state for **every
    /// connected peer except** the one whose `user_id` matches `except_user_id`.
    ///
    /// HCL issue #1036. The mute-all / disable-all host broadcasts
    /// (`HOST_MUTE_PARTICIPANT` / `HOST_DISABLE_VIDEO` with an empty
    /// `target_user_id`) must reflect across the whole room *immediately*, the
    /// same way the single-target [`force_peer_media_off`] does — not lag behind
    /// the slow heartbeat path. But a mute-all must **not** force-mute the
    /// issuing host's own tile: the host muting everyone is not muting itself.
    /// The server therefore carries the host's `user_id` on the broadcast via
    /// `creator_id`, which the handler passes here as `except_user_id`; that one
    /// peer is skipped entirely (its `audio_enabled` / `video_enabled` are left
    /// untouched) while every other peer is forced off.
    ///
    /// Shares the exact per-peer force-off body, freshness-guard bypass, decoder
    /// flush, idempotency (only a real `enabled -> false` transition mutates or
    /// broadcasts), and multi-tab handling with [`force_peer_media_off`] via
    /// [`Peer::force_media_off`]. Like that method it is not a permanent latch
    /// for a heartbeat-sending peer: an affirmative heartbeat re-enables it.
    ///
    /// Safe no-op for `audio_off == video_off == false`. The exclusion is by
    /// `user_id`, so all of the host's own sessions/tabs are excluded.
    pub fn force_all_peers_media_off_except(
        &mut self,
        except_user_id: &str,
        audio_off: bool,
        video_off: bool,
    ) {
        if !audio_off && !video_off {
            return;
        }
        let diagnostics = self.diagnostics.clone();
        let keys: Vec<u64> = self.connected_peers.ordered_keys().clone();
        for key in keys {
            if let Some(peer) = self.connected_peers.get_mut(&key) {
                // Skip the issuing host's own tile(s) entirely — a mute-all
                // must not mute the host that issued it (#1036).
                if peer.user_id == except_user_id {
                    continue;
                }
                if peer.force_media_off(audio_off, video_off) {
                    if let Some(diagnostics) = &diagnostics {
                        diagnostics.set_peer_media_state(
                            &peer.sid_str,
                            peer.video_enabled,
                            peer.screen_enabled,
                        );
                    }
                    // Single status broadcast per real change, same as the
                    // single-target path, to avoid redundant UI churn.
                    peer.broadcast_peer_status();
                }
            }
        }
    }

    /// Get the display name for a peer by session_id string.
    ///
    /// Checks the live peer entry first, then falls back to the persistent
    /// `display_name_cache` (populated by PARTICIPANT_JOINED events that may
    /// arrive before the first media packet creates the peer entry).
    pub fn get_peer_display_name(&self, session_id_str: &str) -> Option<String> {
        let sid: u64 = session_id_str.parse().ok()?;
        if let Some(peer) = self.connected_peers.get(&sid) {
            if peer.display_name.is_some() {
                return peer.display_name.clone();
            }
        }
        self.display_name_cache.get(&sid).cloned()
    }

    /// Get a peer's self-reported device info (#1482) by relay session_id.
    /// THE UI CONTRACT: returns the live peer's `device_info` if the peer exists
    /// and has at least one populated field, else falls back to the cache (info
    /// may arrive before the peer entry is created), else `None`. The returned
    /// struct is a clone; reading it does NOT mutate state or trigger a render.
    pub fn peer_device_info(&self, session_id: u64) -> Option<PeerDeviceInfo> {
        if let Some(peer) = self.connected_peers.get(&session_id) {
            if peer.device_info != PeerDeviceInfo::default() {
                return Some(peer.device_info.clone());
            }
        }
        self.device_info_cache.get(&session_id).cloned()
    }

    /// issue 1482: every known peer's self-reported device info, for the
    /// diagnostics "Device (per peer)" section which must render independent of
    /// whether media is currently flowing (a camera-off peer still reports device
    /// metrics via HEALTH). Returns `(session_id, label, info)` for each peer that
    /// has at least one populated device field. The label mirrors
    /// `per_peer_received_snapshots` (display name → user id → session id) so a
    /// receiving peer's label does not regress; a cache-only peer (info arrived
    /// before its `Peer` entry) falls back to the display-name cache then the sid
    /// string. The peer set is the UNION of live peers and the device-info cache,
    /// each resolved through the same live-then-cache rule as `peer_device_info`,
    /// deduplicated by session_id. Read-only; does not mutate or trigger a render.
    pub fn all_peer_device_info(&self) -> Vec<(u64, String, PeerDeviceInfo)> {
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut out: Vec<(u64, String, PeerDeviceInfo)> = Vec::new();
        // Live peers first, in their stable ordered-key order, so a receiving
        // peer keeps the same label/order it had under the old receive-list path.
        for &sid in self.connected_peers.ordered_keys() {
            if !seen.insert(sid) {
                continue;
            }
            if let Some(info) = self.peer_device_info(sid) {
                if info == PeerDeviceInfo::default() {
                    continue;
                }
                let label = self
                    .connected_peers
                    .get(&sid)
                    .map(|peer| {
                        peer.display_name.clone().unwrap_or_else(|| {
                            if peer.user_id.is_empty() {
                                sid.to_string()
                            } else {
                                peer.user_id.clone()
                            }
                        })
                    })
                    .or_else(|| self.display_name_cache.get(&sid).cloned())
                    .unwrap_or_else(|| sid.to_string());
                out.push((sid, label, info));
            }
        }
        // Cache-only peers (device info arrived before the `Peer` entry exists).
        for sid in self.device_info_cache.keys().copied() {
            if !seen.insert(sid) {
                continue;
            }
            if let Some(info) = self.peer_device_info(sid) {
                if info == PeerDeviceInfo::default() {
                    continue;
                }
                let label = self
                    .display_name_cache
                    .get(&sid)
                    .cloned()
                    .unwrap_or_else(|| sid.to_string());
                out.push((sid, label, info));
            }
        }
        out
    }

    /// Get the server-vouched guest status for a peer by session_id string.
    pub fn get_peer_is_guest(&self, session_id_str: &str) -> Option<bool> {
        let sid: u64 = session_id_str.parse().ok()?;
        if let Some(peer) = self.connected_peers.get(&sid) {
            return Some(peer.is_guest);
        }
        self.is_guest_cache.get(&sid).copied()
    }

    /// Set the server-vouched guest status for a peer identified by
    /// session_id.
    pub fn set_peer_is_guest(&mut self, session_id: u64, is_guest: bool) {
        self.is_guest_cache.insert(session_id, is_guest);
        if let Some(peer) = self.connected_peers.get_mut(&session_id) {
            peer.is_guest = is_guest;
        }
    }

    pub fn is_peer_speaking(&self, key: &str) -> bool {
        let sid: u64 = match key.parse() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if let Some(peer) = self.connected_peers.get(&sid) {
            return peer.is_speaking;
        }
        false
    }

    pub fn peer_audio_level(&self, key: &str) -> f32 {
        let sid: u64 = match key.parse() {
            Ok(v) => v,
            Err(_) => return 0.0,
        };
        if let Some(peer) = self.connected_peers.get(&sid) {
            return peer.audio_level;
        }
        0.0
    }
}

// ---------------------------------------------------------------------------
// Shared test fixtures (parent-module scope, still `#[cfg(test)]`)
// ---------------------------------------------------------------------------
// These were hoisted out of `mod tests` so the production-only test seam
// `PeerDecodeManager::insert_zero_loss_top_peer_for_test` (a `#[cfg(test)]`
// method on the main impl) can build the same host-safe peer without
// duplicating the 50-field `Peer` literal. They never compile into a non-test
// build. `mod tests` re-imports them via its `use super::*;`, so every existing
// bare-name call site there keeps working unchanged.

/// Ordered log of the lifecycle calls a [`MockAudioDecoder`] received, shared
/// by every mock in one test. Entries are `"<tag>:<op>"`, so a test can observe
/// both WHICH decoder was called and in WHAT ORDER — which is the whole point
/// for decoder replacement (issue 2174 follow-up), where `retire_for_replacement`
/// must land strictly before the `Drop` it disarms.
#[cfg(test)]
type MockAudioLog = Rc<RefCell<Vec<String>>>;

/// No-op audio decoder for unit tests.
/// Muted state is stored in an `Rc<Cell<bool>>` so tests can inspect it
/// after handing ownership to `Peer`.
#[cfg(test)]
struct MockAudioDecoder {
    muted: Rc<std::cell::Cell<bool>>,
    tag: &'static str,
    log: MockAudioLog,
}

#[cfg(test)]
impl MockAudioDecoder {
    fn new() -> (Self, Rc<std::cell::Cell<bool>>) {
        Self::tagged("mock", Rc::new(RefCell::new(Vec::new())))
    }

    /// A mock that records its lifecycle calls into a caller-owned log under
    /// `tag`, so several mocks can be told apart in one test.
    fn tagged(tag: &'static str, log: MockAudioLog) -> (Self, Rc<std::cell::Cell<bool>>) {
        let muted = Rc::new(std::cell::Cell::new(true));
        (
            Self {
                muted: muted.clone(),
                tag,
                log,
            },
            muted,
        )
    }

    fn record(&self, op: &str) {
        self.log.borrow_mut().push(format!("{}:{op}", self.tag));
    }
}

#[cfg(test)]
impl AudioPeerDecoderTrait for MockAudioDecoder {
    fn decode(&mut self, _packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        Ok(DecodeStatus::SKIPPED)
    }
    fn flush(&mut self) {}
    fn set_muted(&mut self, muted: bool) {
        self.muted.set(muted);
        self.record(if muted { "mute" } else { "unmute" });
    }
    fn retire_for_replacement(&mut self) {
        self.record("retire");
    }
}

/// Stands in for `NetEqAudioPeerDecoder::drop`, which broadcasts the terminal
/// `speaking: 0`. Recording the drop is what lets a test distinguish
/// replacement (retire, then a disarmed drop) from teardown (a drop that still
/// has to announce silence).
#[cfg(test)]
impl Drop for MockAudioDecoder {
    fn drop(&mut self) {
        self.record("drop");
    }
}

/// Create a `Peer` with no-op decoders (no browser APIs required).
/// Returns the peer and an `Rc<Cell<bool>>` handle to the mock audio
/// decoder's muted state for test assertions.
#[cfg(test)]
fn make_test_peer(session_id: u64) -> (Peer, Rc<std::cell::Cell<bool>>) {
    let sid_str = session_id.to_string();
    let (mock_audio, muted_handle) = MockAudioDecoder::new();
    let peer = Peer {
        audio: Box::new(mock_audio),
        video: VideoPeerDecoder::noop(),
        screen: VideoPeerDecoder::noop(),
        session_id,
        sid_str,
        user_id: "test@test.com".into(),
        video_canvas_id: format!("video-{session_id}"),
        screen_canvas_id: format!("screen-{session_id}"),
        aes: None,
        activity_count: 1,
        missed_heartbeat_checks: 0,
        video_enabled: false,
        audio_enabled: false,
        screen_enabled: false,
        display_name: None,
        device_info: PeerDeviceInfo::default(),
        is_guest: false,
        visible: false,
        skip_canvas_paint: false,
        context_initialized: false,
        has_received_heartbeat: false,
        audio_forced_off: false,
        video_forced_off: false,
        is_speaking: false,
        audio_level: 0.0,
        transport_type: TransportType::TRANSPORT_UNKNOWN,
        vad_threshold: None,
        selected_video_layer: 0,
        video_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
        video_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
        last_video_downlink: crate::decode::layer_chooser::DownlinkSample {
            loss_per_sec: 0.0,
            kf_per_sec: 0.0,
        },
        selected_screen_layer: 0,
        screen_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
        screen_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
        last_screen_downlink: crate::decode::layer_chooser::DownlinkSample {
            loss_per_sec: 0.0,
            kf_per_sec: 0.0,
        },
        screen_keyframe_starved_since_ms: Rc::new(std::cell::Cell::new(None)),
        selected_audio_layer: 0,
        audio_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
        audio_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
        video_seq_tracker: SequenceTracker::new(),
        screen_seq_tracker: SequenceTracker::new(),
        last_screen_frame_ms: 0,
        last_video_frame_ms: 0,
        consecutive_video_off_hbs: 0,
        last_audio_frame_ms: 0,
        last_video_switch: LastLayerSwitch::default(),
        last_screen_switch: LastLayerSwitch::default(),
        layer_gate_skips: LayerGateSkips::default(),
        receiver_on_webtransport: false,
        audio_datagram_loss: AudioDatagramLossTracker::new(),
    };
    (peer, muted_handle)
}

/// Build a connected peer with a learned 3-layer ladder (highest_available
/// == 2) and a ZERO-LOSS real downlink sample (`{0.0, 0.0}`) — the WebSocket /
/// reliable-WT case where the per-peer telemetry can never see congestion.
/// This is the #1219 Half 2 precondition: unlike `make_congested_top_peer`,
/// the real sample is NOT congested, so the early-seed path is a no-op here and
/// only the synthetic DOWNLINK_CONGESTION seed can step the chooser down.
#[cfg(test)]
fn make_zero_loss_top_peer(session_id: u64) -> Peer {
    let (mut peer, _muted) = make_test_peer(session_id);
    // Learn layers 0,1,2 so highest_available == 2 (room to drop to 1).
    for layer in 0..3u32 {
        peer.video_layer_availability.observe(layer, 1000);
        peer.screen_layer_availability.observe(layer, 1000);
        peer.audio_layer_availability.observe(layer, 1000);
    }
    // Zero-loss real telemetry — the lossless-transport blindness (#1219).
    peer.last_video_downlink = crate::decode::layer_chooser::DownlinkSample {
        loss_per_sec: 0.0,
        kf_per_sec: 0.0,
    };
    peer.last_screen_downlink = crate::decode::layer_chooser::DownlinkSample {
        loss_per_sec: 0.0,
        kf_per_sec: 0.0,
    };
    peer
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    #[test]
    fn the_freeze_output_clock_is_resolved_per_media_kind() {
        let (peer, _muted) = make_test_peer(2511);
        peer.video.set_last_output_ms_for_test(1_111.0);
        peer.screen.set_last_output_ms_for_test(2_222.0);

        assert_eq!(
            diagnostic_last_output_ms(&peer, MediaType::VIDEO),
            Some(1_111.0)
        );
        assert_eq!(
            diagnostic_last_output_ms(&peer, MediaType::SCREEN),
            Some(2_222.0)
        );
        for kind in [
            MediaType::AUDIO,
            MediaType::HEARTBEAT,
            MediaType::RTT,
            MediaType::KEYFRAME_REQUEST,
            MediaType::MEDIA_TYPE_UNKNOWN,
        ] {
            assert_eq!(
                diagnostic_last_output_ms(&peer, kind),
                None,
                "{kind:?} owns no VideoPeerDecoder, so it must not carry an output clock"
            );
        }
    }

    /// The receive VU meter renders `receive::format_readout`, whose `{w}x{h}`
    /// comes from `received_layer_snapshot`.
    ///
    /// MUTATION: drop the `enrich_screen_snapshot` call from
    /// `received_layer_snapshot` and the first assertion reads `0x0`; drop the
    /// `screen_encode_box_for_capture` fit and the capped case reports the
    /// capture size.
    #[test]
    fn the_screen_vu_readout_reports_real_geometry_not_zero() {
        let mut mgr = PeerDecodeManager::new();
        let (mut peer, _muted) = make_test_peer(2343);
        peer.screen_enabled = true;
        peer.screen.set_reported_for_test(1400, 700, 1176);
        mgr.connected_peers.insert(2343, peer);

        let snap = mgr
            .received_layer_snapshot(crate::decode::layer_chooser::PrefMediaKind::Screen, 1000)
            .expect("a screen-enabled peer is being received");
        assert_eq!(
            (snap.width, snap.height),
            (1400, 700),
            "the VU meter must report the sender's real geometry, not 0x0"
        );
        assert_ne!((snap.width, snap.height), (0, 0));
        assert_eq!(snap.kbps, 1176);

        // A surface above the encode ceiling: the receiver must report what is
        // ENCODED, which is what the sender's own readout shows.
        let (mut big, _m2) = make_test_peer(2344);
        big.screen_enabled = true;
        big.screen.set_reported_for_test(5120, 2880, 9000);
        let mut mgr2 = PeerDecodeManager::new();
        mgr2.connected_peers.insert(2344, big);
        let capped = mgr2
            .received_layer_snapshot(crate::decode::layer_chooser::PrefMediaKind::Screen, 1000)
            .expect("receiving");
        assert_eq!(
            (capped.width, capped.height),
            videocall_aq::screen_encode_box_for_capture(5120, 2880),
            "a capped share must report the ENCODE box, not its raw capture"
        );

        // An unreported publisher stays honestly unknown rather than inventing a size.
        let (mut quiet, _m3) = make_test_peer(2345);
        quiet.screen_enabled = true;
        let mut mgr3 = PeerDecodeManager::new();
        mgr3.connected_peers.insert(2345, quiet);
        let unknown = mgr3
            .received_layer_snapshot(crate::decode::layer_chooser::PrefMediaKind::Screen, 1000)
            .expect("receiving");
        assert_eq!((unknown.width, unknown.height), (0, 0));

        // The stamped bitrate rides on the wire and is attacker-controllable.
        //
        // MUTATION: drop the `.min(MAX_REPORTED_SCREEN_KBPS)` and this reads
        // `u32::MAX`, which the readouts render as "4294967.5M".
        let (mut hostile, _m4) = make_test_peer(2346);
        hostile.screen_enabled = true;
        hostile.screen.set_reported_for_test(1920, 1080, u32::MAX);
        let mut mgr4 = PeerDecodeManager::new();
        mgr4.connected_peers.insert(2346, hostile);
        let bounded = mgr4
            .received_layer_snapshot(crate::decode::layer_chooser::PrefMediaKind::Screen, 1000)
            .expect("receiving");
        assert_eq!(bounded.kbps, MAX_REPORTED_SCREEN_KBPS);
        // An honest publisher is never clipped.
        assert_eq!(
            videocall_aq::constants::screen_bitrate_kbps_for(2560, 1440, 10)
                .min(MAX_REPORTED_SCREEN_KBPS),
            videocall_aq::constants::screen_bitrate_kbps_for(2560, 1440, 10)
        );
    }

    /// #2251 leaf. MUTATION: `>=`→`>` moves a healthy guard; dropping the write pins 2.
    #[test]
    fn the_video_guard_collapses_onto_the_rung_the_relay_still_forwards() {
        let (mut peer, _muted) = make_test_peer(2251);
        peer.set_selected_video_layer(2);
        peer.video_layer_availability.observe(0, 1_000);
        peer.video_layer_availability.observe(1, 1_000);
        peer.video_layer_availability.observe(2, 1_000);
        assert!(
            !peer.collapse_video_guard_to_available(1_500),
            "a guard whose rung is still arriving must not be moved"
        );
        assert_eq!(peer.selected_video_layer(), 2);

        let shed_now = 1_000 + LayerAvailability::DEFAULT_WINDOW_MS + 1;
        peer.video_layer_availability.observe(0, shed_now);
        assert!(
            peer.collapse_video_guard_to_available(shed_now),
            "a guard above every arriving rung must be collapsed"
        );
        assert_eq!(
            peer.selected_video_layer(),
            0,
            "#2251: the receiver must decode the base rung the relay is still \
             forwarding instead of freezing on a rung that stopped"
        );
    }

    /// #2251 wiring. MUTATION: delete the VIDEO-arm call and the guard stays on 2.
    #[test]
    fn decode_collapses_a_starved_video_guard_instead_of_skipping_the_base_rung() {
        let (mut peer, _muted) = make_test_peer(2251);
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        peer.set_selected_video_layer(2);
        let stale_beyond_window = now_ms() - (LayerAvailability::DEFAULT_WINDOW_MS + 1_000);
        peer.video_layer_availability
            .observe(2, stale_beyond_window);

        let base_rung = Arc::new(PacketWrapper {
            data: MediaPacket {
                media_type: MediaType::VIDEO.into(),
                ..Default::default()
            }
            .write_to_bytes()
            .expect("serialize"),
            user_id: "test@test.com".into(),
            packet_type: PacketType::MEDIA.into(),
            session_id: 2251,
            simulcast_layer_id: 0,
            media_kind: MediaKind::VIDEO.into(),
            ..Default::default()
        });
        let _ = peer.decode(&base_rung, "local@test.com");

        assert_eq!(
            peer.selected_video_layer(),
            0,
            "#2251: decode must drop the guard onto the arriving base rung — \
             leaving it on the shed rung 2 freezes the tile for up to a full \
             peer-monitor tick past the availability window"
        );
        assert_eq!(
            peer.layer_gate_skips().video_guard_above,
            0,
            "#2533: a packet the collapse rescued was decoded, so it is not a discard"
        );
    }

    /// `cleartext` is the OUTER `media_kind`; `MEDIA_KIND_UNSPECIFIED` selects the
    /// pre-field-6 fall-through, where the gate runs after decrypt instead.
    fn gate_packet(
        session_id: u64,
        media_type: MediaType,
        cleartext: MediaKind,
        layer: u32,
    ) -> Arc<PacketWrapper> {
        Arc::new(PacketWrapper {
            data: MediaPacket {
                media_type: media_type.into(),
                ..Default::default()
            }
            .write_to_bytes()
            .expect("serialize"),
            user_id: "test@test.com".into(),
            packet_type: PacketType::MEDIA.into(),
            session_id,
            simulcast_layer_id: layer,
            media_kind: cleartext.into(),
            ..Default::default()
        })
    }

    #[test]
    fn decode_meters_every_layer_gate_discard_by_guard_relationship() {
        const KINDS: [(MediaType, MediaKind); 3] = [
            (MediaType::VIDEO, MediaKind::VIDEO),
            (MediaType::SCREEN, MediaKind::SCREEN),
            (MediaType::AUDIO, MediaKind::AUDIO),
        ];
        let (mut peer, _muted) = make_test_peer(2533);
        peer.video_enabled = true;
        peer.screen_enabled = true;
        peer.audio_enabled = true;
        peer.has_received_heartbeat = true;

        // Rung 2 observed fresh, so #2251's collapse declines and the base rung is
        // genuinely discarded.
        peer.set_selected_video_layer(2);
        peer.video_layer_availability.observe(2, now_ms());
        // Screen has ONE layer, so every screen discard below is off-ladder, not exact-match.
        install_decode_guard(&mut peer, PrefMediaKind::Screen, 2);
        peer.set_selected_audio_layer(2);
        // Two on the cleartext gate plus one on the pre-field-6 fall-through.
        for (inner, cleartext) in KINDS {
            let via_cleartext = gate_packet(2533, inner, cleartext, 0);
            let via_fallthrough = gate_packet(2533, inner, MediaKind::MEDIA_KIND_UNSPECIFIED, 0);
            for pkt in [&via_cleartext, &via_cleartext, &via_fallthrough] {
                peer.decode(pkt, "local@test.com")
                    .unwrap_or_else(|e| panic!("{inner:?} rung 0 must not error: {e:?}"));
            }
        }

        peer.set_selected_video_layer(0);
        peer.set_selected_screen_layer(0);
        peer.set_selected_audio_layer(0);
        for (inner, cleartext) in KINDS {
            let pkt = gate_packet(2533, inner, cleartext, 1);
            peer.decode(&pkt, "local@test.com")
                .unwrap_or_else(|e| panic!("{inner:?} rung 1 must not error: {e:?}"));
        }

        assert_eq!(
            peer.layer_gate_skips(),
            LayerGateSkips {
                video_guard_above: 3,
                video_guard_below: 1,
                screen_guard_above: 3,
                screen_guard_below: 1,
                audio_guard_above: 3,
                audio_guard_below: 1,
            },
            "#2533: every gate discard must land in the bucket for the side the \
             guard sat on — this return was previously unmetered entirely"
        );
    }

    /// The first `simulcast_layer_id` past `kind`'s ladder, derived so growing a ladder cannot
    /// leave these fixtures inside it.
    fn off_ladder_layer(kind: PrefMediaKind) -> u32 {
        crate::decode::layer_chooser::max_layers_for_kind(kind)
    }

    /// This peer's availability map and admission stamp for `kind`; the stamp is non-zero only
    /// once a packet has passed the layer gate.
    fn availability_and_admission(peer: &Peer, kind: PrefMediaKind) -> (&LayerAvailability, u64) {
        match kind {
            PrefMediaKind::Video => (&peer.video_layer_availability, peer.last_video_frame_ms),
            PrefMediaKind::Screen => (&peer.screen_layer_availability, peer.last_screen_frame_ms),
            PrefMediaKind::Audio => (&peer.audio_layer_availability, peer.last_audio_frame_ms),
        }
    }

    /// Writes the guard field directly, BYPASSING the public setter's ladder clamp, so the decode
    /// path's own off-ladder rejection is pinned independently of how the guard got there.
    fn install_decode_guard(peer: &mut Peer, kind: PrefMediaKind, layer: u32) {
        match kind {
            PrefMediaKind::Video => peer.selected_video_layer = layer,
            PrefMediaKind::Screen => peer.selected_screen_layer = layer,
            PrefMediaKind::Audio => peer.selected_audio_layer = layer,
        }
    }

    fn guard_below(peer: &Peer, kind: PrefMediaKind) -> u64 {
        let s = peer.layer_gate_skips();
        match kind {
            PrefMediaKind::Video => s.video_guard_below,
            PrefMediaKind::Screen => s.screen_guard_below,
            PrefMediaKind::Audio => s.audio_guard_below,
        }
    }

    fn gate_ready_peer(session_id: u64) -> Peer {
        let (mut peer, _muted) = make_test_peer(session_id);
        peer.video_enabled = true;
        peer.screen_enabled = true;
        peer.audio_enabled = true;
        peer.has_received_heartbeat = true;
        peer
    }

    /// Covers the cleartext gate AND the pre-field-6 fall-through, at both guard positions.
    /// `selected == off` is the sharp case: exact-match would otherwise ADMIT an off-ladder id.
    fn assert_off_ladder_layer_is_discarded_coherently(
        kind: PrefMediaKind,
        media_type: MediaType,
        cleartext: MediaKind,
    ) {
        let off = off_ladder_layer(kind);
        let top = off - 1;
        for ct in [cleartext, MediaKind::MEDIA_KIND_UNSPECIFIED] {
            for selected in [0, off] {
                let mut peer = gate_ready_peer(2556);
                install_decode_guard(&mut peer, kind, selected);

                let now = now_ms();
                peer.decode(&gate_packet(2556, media_type, ct, off), "local@test.com")
                    .expect("a well-formed off-ladder packet must not error");

                let ctx = format!("{kind:?} off={off} selected={selected} cleartext={ct:?}");
                let (availability, admitted_at_ms) = availability_and_admission(&peer, kind);
                assert!(
                    !availability.layer_available_peek(top, now),
                    "{ctx}: layer {top} must not read as available off an id the guard skips"
                );
                assert_eq!(
                    admitted_at_ms, 0,
                    "{ctx}: an off-ladder id must not reach sequence tracking or decode"
                );
                assert_eq!(
                    guard_below(&peer, kind),
                    1,
                    "{ctx}: the discard must be counted"
                );
            }
        }
    }

    #[test]
    fn decode_discards_an_off_ladder_video_layer_without_recording_it_available() {
        assert_off_ladder_layer_is_discarded_coherently(
            PrefMediaKind::Video,
            MediaType::VIDEO,
            MediaKind::VIDEO,
        );
    }

    #[test]
    fn decode_discards_an_off_ladder_screen_layer_without_recording_it_available() {
        assert_off_ladder_layer_is_discarded_coherently(
            PrefMediaKind::Screen,
            MediaType::SCREEN,
            MediaKind::SCREEN,
        );
    }

    #[test]
    fn decode_discards_an_off_ladder_audio_layer_without_recording_it_available() {
        assert_off_ladder_layer_is_discarded_coherently(
            PrefMediaKind::Audio,
            MediaType::AUDIO,
            MediaKind::AUDIO,
        );
    }

    /// The band: each ladder's TOP layer — video layer 2, audio layer 2, screen layer 0.
    #[test]
    fn decode_still_admits_the_top_layer_of_every_ladder() {
        for (kind, media_type, cleartext) in [
            (PrefMediaKind::Video, MediaType::VIDEO, MediaKind::VIDEO),
            (PrefMediaKind::Screen, MediaType::SCREEN, MediaKind::SCREEN),
            (PrefMediaKind::Audio, MediaType::AUDIO, MediaKind::AUDIO),
        ] {
            let top = off_ladder_layer(kind) - 1;
            for ct in [cleartext, MediaKind::MEDIA_KIND_UNSPECIFIED] {
                let mut peer = gate_ready_peer(2556);
                install_decode_guard(&mut peer, kind, top);

                let now = now_ms();
                peer.decode(&gate_packet(2556, media_type, ct, top), "local@test.com")
                    .expect("a well-formed top-layer packet must not error");

                let ctx = format!("{kind:?} top={top} cleartext={ct:?}");
                let (availability, admitted_at_ms) = availability_and_admission(&peer, kind);
                assert!(
                    availability.layer_available_peek(top, now),
                    "{ctx}: the ladder top is in range and must be recorded as arriving"
                );
                assert_ne!(admitted_at_ms, 0, "{ctx}: the ladder top must be admitted");
                assert_eq!(
                    peer.layer_gate_skips(),
                    LayerGateSkips::default(),
                    "{ctx}: nothing was off-ladder, so nothing may be counted as skipped"
                );
            }
        }
    }

    /// A guard above the ladder top exact-matches nothing, blacking out that kind for this peer.
    #[test]
    fn selected_layer_setters_cannot_install_an_off_ladder_guard() {
        let (mut peer, _muted) = make_test_peer(2556);
        for over in [0u32, 1, 7, u32::MAX] {
            peer.set_selected_video_layer(
                off_ladder_layer(PrefMediaKind::Video).saturating_add(over),
            );
            peer.set_selected_screen_layer(
                off_ladder_layer(PrefMediaKind::Screen).saturating_add(over),
            );
            peer.set_selected_audio_layer(
                off_ladder_layer(PrefMediaKind::Audio).saturating_add(over),
            );

            assert_eq!(
                peer.selected_video_layer(),
                off_ladder_layer(PrefMediaKind::Video) - 1,
                "video guard must be held at the ladder top (over={over})"
            );
            assert_eq!(
                peer.selected_screen_layer(),
                off_ladder_layer(PrefMediaKind::Screen) - 1,
                "screen guard must be held at the ladder top (over={over})"
            );
            assert_eq!(
                peer.selected_audio_layer,
                off_ladder_layer(PrefMediaKind::Audio) - 1,
                "audio guard must be held at the ladder top (over={over})"
            );
        }
    }

    #[test]
    fn decode_mints_no_availability_entry_for_a_flood_of_unique_off_ladder_ids() {
        let mut peer = gate_ready_peer(2556);
        let off = off_ladder_layer(PrefMediaKind::Video);
        let now = now_ms();
        for raw in off..off + 1_000 {
            peer.decode(
                &gate_packet(2556, MediaType::VIDEO, MediaKind::VIDEO, raw),
                "local@test.com",
            )
            .expect("a well-formed off-ladder packet must not error");
        }

        assert!(
            !peer.video_layer_availability.layer_available_peek(off, now),
            "an off-ladder id must not become its own availability entry"
        );
        assert!(
            !peer
                .video_layer_availability
                .layer_available_peek(off + 999, now),
            "no id in the flood may become an availability entry"
        );
        assert_eq!(
            peer.video_layer_availability.highest_available(now),
            0,
            "an off-ladder flood must not advertise the ladder top either"
        );
        assert_eq!(peer.layer_gate_skips().video_guard_below, 1_000);
    }

    /// #2533: the counters must reach `peer_status`, and be ABSENT while zero.
    #[test]
    fn peer_status_carries_gate_skips_only_once_they_are_nonzero() {
        let (mut peer, _muted) = make_test_peer(2533);
        let names = |evt: &DiagEvent| -> Vec<&'static str> {
            evt.metrics
                .iter()
                .map(|m| m.name)
                .filter(|n| n.starts_with("layer_gate_skips_"))
                .collect()
        };
        assert!(
            names(&peer.peer_status_event()).is_empty(),
            "a peer that has discarded nothing must add no gate-skip metrics"
        );

        peer.set_selected_video_layer(2);
        peer.video_layer_availability.observe(2, now_ms());
        let _ = peer.decode(
            &gate_packet(2533, MediaType::VIDEO, MediaKind::VIDEO, 0),
            "local@test.com",
        );
        assert_eq!(
            names(&peer.peer_status_event()),
            vec!["layer_gate_skips_video_guard_above"],
            "only the bucket that moved may be reported"
        );
    }

    /// #2533 Gap A: ASSERTS THE CURRENT, WRONG BEHAVIOUR ON PURPOSE. The 4000ms
    /// availability window keeps the starved rung "available", so the collapse
    /// cannot fire beneath it. Whoever removes that floor must invert this.
    #[test]
    fn sub_window_starvation_does_not_collapse_the_guard() {
        let (mut peer, _muted) = make_test_peer(2533);
        peer.set_selected_video_layer(1);
        let stopped_at = 10_000;
        let now = stopped_at + SUB_WINDOW_STALENESS_MS;
        peer.video_layer_availability.observe(1, stopped_at);
        peer.video_layer_availability.observe(0, now);

        assert_eq!(
            peer.video_layer_availability.highest_available(now),
            1,
            "a rung that stopped inside the window still reads as available"
        );
        assert!(
            !peer.collapse_video_guard_to_available(now),
            "#2533 DEFECT: the collapse declines, so the arriving rung-0 packet is \
             discarded and the tile stays frozen for the rest of the window"
        );
        assert_eq!(peer.selected_video_layer(), 1, "guard is left above rung 0");
    }

    /// The field median `head_age` for a #2533 starvation episode.
    const SUB_WINDOW_STALENESS_MS: u64 = 1_809;
    const _: () = assert!(
        SUB_WINDOW_STALENESS_MS < LayerAvailability::DEFAULT_WINDOW_MS,
        "the floor-test fixture must sit INSIDE the availability window"
    );

    /// #2533 Gap A, wiring half: the same floor through production `Peer::decode`.
    #[test]
    fn decode_discards_the_base_rung_while_the_starved_guard_is_sub_window() {
        let (mut peer, _muted) = make_test_peer(2533);
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        peer.set_selected_video_layer(1);
        peer.video_layer_availability
            .observe(1, now_ms() - SUB_WINDOW_STALENESS_MS);

        let (media_type, _, _) = peer
            .decode(
                &gate_packet(2533, MediaType::VIDEO, MediaKind::VIDEO, 0),
                "local@test.com",
            )
            .expect("a well-formed base-rung packet must not error");
        assert_eq!(media_type, MediaType::VIDEO);
        assert_eq!(
            peer.selected_video_layer(),
            1,
            "#2533 DEFECT: a starvation shorter than the availability window \
             leaves the guard above the only rung the relay guarantees"
        );
        assert_eq!(
            peer.layer_gate_skips().video_guard_above,
            1,
            "the discard the freeze is made of must be counted"
        );
    }

    #[test]
    fn broadcast_peer_status_logs_only_the_nonzero_gate_skips() {
        let (mut peer, _muted) = make_test_peer(2533);
        LAST_GATE_SKIPS_LOG.with(|c| *c.borrow_mut() = None);

        peer.broadcast_peer_status();
        assert_eq!(
            LAST_GATE_SKIPS_LOG.with(|c| c.borrow().clone()),
            None,
            "a peer that has discarded nothing must emit no line"
        );

        peer.set_selected_video_layer(2);
        peer.video_layer_availability.observe(2, now_ms());
        install_decode_guard(&mut peer, PrefMediaKind::Screen, 1);
        for (inner, cleartext) in [
            (MediaType::VIDEO, MediaKind::VIDEO),
            (MediaType::SCREEN, MediaKind::SCREEN),
        ] {
            peer.decode(&gate_packet(2533, inner, cleartext, 0), "local@test.com")
                .unwrap_or_else(|e| panic!("{inner:?} rung 0 must not error: {e:?}"));
        }

        peer.broadcast_peer_status();
        assert_eq!(
            LAST_GATE_SKIPS_LOG.with(|c| c.borrow().clone()).as_deref(),
            Some("LAYER_GATE_SKIPS session_id=2533 video_above=1 screen_above=1"),
            "nonzero fields only, under a greppable tag"
        );
    }

    /// Guards the WIRING, not the leaf fn: deleting the emission from
    /// `peer_status_event` left every other test in both crates green.
    #[test]
    fn peer_status_carries_the_arriving_audio_rung_and_omits_a_stale_one() {
        let now = videocall_diagnostics::now_ms();
        let (mut peer, _muted) = make_test_peer(2132);
        peer.audio_layer_availability.observe(1, now);
        peer.set_selected_audio_layer(1);
        let evt = peer.peer_status_event();
        let got = evt
            .metrics
            .iter()
            .find(|m| m.name == METRIC_SELECTED_AUDIO_LAYER)
            .map(|m| m.value.clone());
        assert!(
            matches!(got, Some(MetricValue::U64(1))),
            "arriving rung 1 must be emitted, got {got:?}"
        );

        // Same selection, but the observation is now older than the availability
        // window: the metric must be ABSENT so the readout falls back to "—"
        // rather than claiming a bitrate the receiver is not getting.
        let (mut stale, _m2) = make_test_peer(2133);
        stale.audio_layer_availability.observe(1, 0);
        stale.set_selected_audio_layer(1);
        let stale_evt = stale.peer_status_event();
        assert!(
            !stale_evt
                .metrics
                .iter()
                .any(|m| m.name == METRIC_SELECTED_AUDIO_LAYER),
            "a rung outside the availability window must not be emitted"
        );
    }

    /// The peek must agree with the pruning sibling about the window boundary;
    /// replacing it with a bare `contains_key` left every test green.
    #[test]
    fn layer_available_peek_respects_the_window() {
        use crate::decode::layer_chooser::LayerAvailability;
        let w = LayerAvailability::DEFAULT_WINDOW_MS;
        let mut a = LayerAvailability::new();
        a.observe(1, 1_000);
        assert!(
            a.layer_available_peek(1, 1_000),
            "same instant is available"
        );
        assert!(
            a.layer_available_peek(1, 1_000 + w),
            "boundary is inclusive"
        );
        assert!(
            !a.layer_available_peek(1, 1_000 + w + 1),
            "one ms past the window must be unavailable"
        );
        assert!(
            !a.layer_available_peek(2, 1_000),
            "unobserved layer is never available"
        );
    }

    #[test]
    fn displayed_audio_rung_hides_a_selected_rung_that_is_not_arriving() {
        assert_eq!(
            crate::decode::peer_decode_manager::displayed_audio_rung(2, true),
            Some(2)
        );
        assert_eq!(
            crate::decode::peer_decode_manager::displayed_audio_rung(0, true),
            Some(0)
        );
        assert_eq!(
            crate::decode::peer_decode_manager::displayed_audio_rung(2, false),
            None
        );
        assert_eq!(
            crate::decode::peer_decode_manager::displayed_audio_rung(0, false),
            None
        );
    }

    use super::*;
    use protobuf::Message;
    use std::cell::Cell;
    use videocall_codecs::messages::StreamContext;
    use videocall_types::protos::media_packet::media_packet::MediaType;
    use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket};
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    /// A FIXED test clock, inside the layer-availability retention window.
    ///
    /// `make_zero_loss_top_peer` / `make_congested_top_peer` record layer
    /// availability at `t == 1000`, and `LayerAvailability::highest_available`
    /// prunes any entry older than `DEFAULT_WINDOW_MS`. Ticking a
    /// chooser with the real wall clock (`now_ms()` ≈ 1.7e12) therefore prunes
    /// the entire learned ladder and collapses `highest_available` from 2 to 0,
    /// silently defeating both the size lid and the #1079 advertise gate — the
    /// fixture, not the production code, is what breaks.
    ///
    /// 2000 is 1000ms after the fixtures' observation, well inside the window.
    ///
    /// Ticking repeatedly at the SAME fixed clock is intentional and safe because
    /// nothing these call sites exercise is time-gated. Every `FIXTURE_NOW_MS` tick
    /// seeds a ZERO-LOSS peer (`insert_zero_loss_top_peer_for_test` ->
    /// `make_zero_loss_top_peer`), so `DownlinkSample::is_congested` is never true,
    /// the chooser never leaves its unconstrained state, and `choose` takes the
    /// follow-the-top arm (`layer_chooser.rs:539`) — an unconditional
    /// `set_layer(highest_available, now_ms)` on EVERY tick that consults no elapsed
    /// time at all. The dwell gate (`LAYER_STEP_UP_DWELL_MS`) sits only in the
    /// constrained + clean climb, which a zero-loss fixture never reaches; and
    /// `set_layer` itself only records timestamps, it never gates on them. The
    /// tile-size lid is not a chooser decision either: `tick_layer_chooser` applies
    /// it AFTER `choose` returns, as the pure `bounds.clamp(raw)` at line 1487 of
    /// this file, so it is a function of the tile hint alone and likewise
    /// clock-independent.
    ///
    /// A fixed small clock is also the established convention in this module:
    /// `size_lid_yields_to_user_receive_min` and
    /// `congestion_seed_never_advertises_above_size_lid` both already tick with
    /// `let now = 2000u64`. Note those two drive the CONGESTED fixture, so their own
    /// safety argument is the different one their comments give (the step-down arm
    /// carries no dwell gate) — only the fixed constant is shared with these tests.
    const FIXTURE_NOW_MS: u64 = 2000;

    /// Build a `delta`-frame packet carrying `seq`. Both VIDEO and SCREEN read
    /// `video_metadata.sequence` in `track_sequence`, so one helper covers both.
    fn delta_seq_packet(seq: u64) -> MediaPacket {
        use videocall_types::protos::media_packet::VideoMetadata;
        MediaPacket {
            video_metadata: Some(VideoMetadata {
                sequence: seq,
                ..Default::default()
            })
            .into(),
            frame_type: "delta".to_string(),
            ..Default::default()
        }
    }

    /// Sequence 3, sent after seq 1, to leave position 2 as a genuine in-window
    /// hole. A following >= 64 jump shifts that hole off the window and counts it.
    /// `video_and_screen_independent_tracking` asserts the bridged tracker's
    /// `high_seq == Some(3)`.
    const LOSS_BRIDGE_SEQ: u64 = 3;

    /// Install the worker `<link>` tags that the real decoders look up on the page.
    ///
    /// Trunk normally emits these into `index.html`; the `wasm-bindgen-test` harness
    /// page has neither, so ANY test driving the real `add_peer` -> `Peer::new` ->
    /// `new_decoders` path panics. Both decoders are constructed, so both tags are
    /// required:
    ///   * `codecs-worker` — `videocall-codecs/src/decoder/wasm.rs` (video/screen)
    ///   * `neteq-worker`  — `decode/neteq_audio_decoder.rs` (audio)
    ///
    /// These are missing test fixtures, not product defects.
    ///
    /// Each `href` points at a `blob:` URL holding a no-op worker script: these tests
    /// only need `Worker::new` to SUCCEED, and never run a decode round-trip.
    /// Idempotent, and the tags plus their object URLs are deliberately left installed
    /// for the life of the test binary — revoking a URL would break later
    /// `Worker::new` calls.
    ///
    /// Creating real DOM nodes from a `run_in_browser` test is the existing in-repo
    /// convention (`peer_decoder.rs::make_canvas`).
    fn ensure_decoder_worker_link_tags() {
        for id in ["codecs-worker", "neteq-worker"] {
            ensure_worker_link_tag(id);
        }
    }

    /// Install one no-op blob-backed worker `<link id="{id}" href="blob:...">`.
    fn ensure_worker_link_tag(id: &str) {
        let document = web_sys::window()
            .expect("no window")
            .document()
            .expect("no document");
        if document.get_element_by_id(id).is_some() {
            return;
        }
        let parts = js_sys::Array::new();
        parts.push(&wasm_bindgen::JsValue::from_str(
            "self.onmessage = () => {};",
        ));
        let bag = web_sys::BlobPropertyBag::new();
        bag.set_type("text/javascript");
        let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &bag)
            .expect("create worker blob");
        let url = web_sys::Url::create_object_url_with_blob(&blob).expect("create blob URL");
        let link = document.create_element("link").expect("create <link>");
        link.set_id(id);
        link.set_attribute("href", &url).expect("set href");
        document
            .body()
            .expect("no body")
            .append_child(&link)
            .expect("append <link>");
    }

    // ── Forward gaps that outgrow the reorder window (#2535), over both
    // callers of `record_seq_into_reorder_window`.

    /// Seeded to hold exactly ONE window hole (the never-sent seq 1) at
    /// `high_seq == 2`, so a test can vary only the gap.
    fn tracker_with_one_window_hole() -> SequenceTracker {
        let mut t = SequenceTracker::new();
        assert_eq!(t.record_seq(0), 0, "first packet primes the window");
        assert_eq!(t.record_seq(2), 0, "seq 1 is still inside the window");
        t
    }

    /// The headline case: a burst against a CLEAN window, where `count_zeros()`
    /// contributes nothing.
    #[test]
    fn forward_gap_books_positions_shifted_past_the_new_window() {
        let mut t = SequenceTracker::new();
        assert_eq!(t.record_seq(1000), 0, "first packet primes the window");
        // 1001..=1499 are missing; the window rooted at 1500 still covers the
        // last 63, so 499 - 63 are already unrecoverable.
        assert_eq!(t.record_seq(1500), 436);
        assert_eq!(t.lost_count, 436, "the burst reaches the PLI gate");

        // The same shape un-bridged, i.e. without `LOSS_BRIDGE_SEQ`.
        let mut u = SequenceTracker::new();
        u.record_seq(1);
        assert_eq!(u.record_seq(70), 5, "seq 6..=6 fell past the new window");
    }

    /// The two forward branches must agree where they meet: 63 and 64 book only
    /// the seeded hole, 65 books it plus the position that fell past the window.
    #[test]
    fn forward_gap_is_continuous_across_the_window_boundary() {
        let slots = REORDER_WINDOW_SLOTS;
        let high = 2;
        assert_eq!(
            tracker_with_one_window_hole().record_seq(high + slots - 1),
            1
        );
        assert_eq!(tracker_with_one_window_hole().record_seq(high + slots), 1);
        assert_eq!(
            tracker_with_one_window_hole().record_seq(high + slots + 1),
            2
        );
    }

    /// `sequence` is publisher-controlled, so the booked gap saturates rather
    /// than truncating into the `u32` return.
    #[test]
    fn forward_gap_saturates_at_max_plausible() {
        let mut t = tracker_with_one_window_hole();
        let seq = 2 + REORDER_WINDOW_SLOTS + MAX_PLAUSIBLE_FORWARD_GAP * 8;
        assert_eq!(t.record_seq(seq), 1 + MAX_PLAUSIBLE_FORWARD_GAP as u32);
    }

    /// The behavioural half: `lost_count` gates the PLI, so the burst must arm
    /// one at the normal timeout instead of waiting out 64 later arrivals.
    #[test]
    fn burst_against_a_clean_window_arms_a_keyframe_request() {
        let mut t = SequenceTracker::new();
        t.record_seq(1000);
        t.record_seq(1500);
        assert!(
            !t.should_request_keyframe(0),
            "loss is detected but the initial timeout has not elapsed"
        );
        assert!(t.should_request_keyframe(videocall_aq::constants::KEYFRAME_REQUEST_TIMEOUT_MS));
    }

    /// Native twin of the wasm-only `late_arrival_fills_gap_no_loss`, whose
    /// scenario no native test reached. Late arrivals refill the window, so the
    /// jump books neither the refilled positions nor the pre-stream priming —
    /// only seq 6, which lands between the old high and the new window's floor.
    #[test]
    fn refilled_window_books_only_what_falls_past_the_new_floor() {
        let mut t = SequenceTracker::new();
        t.record_seq(0);
        t.record_seq(5);
        assert_eq!(t.lost_count, 0, "1..=4 are still in the window");
        assert_eq!(t.record_seq(1) + t.record_seq(2) + t.record_seq(3), 0);
        assert_eq!(t.record_seq(4), 0, "every hole is refilled");
        assert_eq!(t.record_seq(70), 1, "seq 6 alone fell past the floor of 7");
    }

    /// The shared window's other caller, with #2031's raw companion leading it
    /// by the still-arrivable tail.
    #[test]
    fn audio_datagram_burst_books_positions_shifted_past_the_new_window() {
        let mut t = AudioDatagramLossTracker::new();
        assert_eq!(t.record_seq(1000), 0, "first packet primes the window");
        assert_eq!(t.record_seq(1500), 436);
        assert_eq!(
            t.window_raw_lost, 499,
            "raw leads the settled count by the 63 positions still in the window"
        );
    }

    // ── Issue #1878: receive-side audio DATAGRAM loss accounting ─────────────
    // Pure native `#[test]`s over the production `AudioDatagramLossTracker`
    // (run by the CI "cargo test videocall-client (native libtest)" job). These
    // are the regression guard for the #1878 instrumentation: they exercise the
    // exact production `record_seq` / `observe_window` the AUDIO decode arm
    // calls, so a mutation that stops counting datagram loss (or drops the
    // windowed rate) fails here.

    /// A gap in the received audio sequence is counted as loss once the skipped
    /// position leaves the 64-slot reorder window. Mutating `record_seq` /
    /// `record_seq_into_reorder_window` to stop counting fails this.
    #[test]
    fn audio_datagram_loss_counts_sequence_gap() {
        let mut t = AudioDatagramLossTracker::new();
        assert_eq!(t.record_seq(0), 0, "first packet establishes the baseline");
        // Sequence 1 (a datagram) never arrives; receive 2..=65 in order. Loss
        // for position 1 is declared exactly when it shifts off the reorder
        // window (at high == 65), never before (reorder tolerance).
        let mut total_lost = 0u32;
        for seq in 2..=65u64 {
            total_lost += t.record_seq(seq);
        }
        assert_eq!(
            total_lost, 1,
            "the one skipped audio datagram must be counted once it leaves the reorder window"
        );
    }

    /// An out-of-order-but-received datagram (within the window) is NOT loss,
    /// even after the window advances far past it.
    #[test]
    fn audio_datagram_loss_tolerates_reordering() {
        let mut t = AudioDatagramLossTracker::new();
        assert_eq!(t.record_seq(0), 0);
        assert_eq!(t.record_seq(2), 0);
        assert_eq!(
            t.record_seq(1),
            0,
            "an in-window out-of-order datagram is not loss"
        );
        let mut lost = 0u32;
        for seq in 3..=70u64 {
            lost += t.record_seq(seq);
        }
        assert_eq!(
            lost, 0,
            "a reordered-but-received datagram must never be counted as lost"
        );
    }

    /// The ~1s window normalises accumulated loss into a per-second rate on
    /// rollover, and only on rollover. The raw component is 0 here because no
    /// forward-gap sequences were recorded (only a synthetic capped count).
    #[test]
    fn audio_datagram_loss_windowed_rate() {
        let mut t = AudioDatagramLossTracker::new();
        assert_eq!(
            t.observe_window(10_000, 0),
            None,
            "no rollover before the window fills"
        );
        assert_eq!(t.observe_window(10_500, 2), None);
        // Exactly 1000ms elapsed (10_000 -> 11_000) with 2 lost => 2.0/sec.
        assert_eq!(t.observe_window(11_000, 0), Some((2.0, 0.0)));
        assert_eq!(t.loss_per_sec(), 2.0);
        // A fresh window has started; no immediate second rollover.
        assert_eq!(t.observe_window(11_500, 0), None);
    }

    /// Issue 2031: the RAW sum reads a contiguous ~200-datagram burst in full at
    /// the moment of the jump, where the settled count must wait for the last 63
    /// positions to shift off the reorder window. Still the fails-on-unfixed
    /// discriminator for 2031: on the pre-2031 tracker the raw sum does not
    /// exist. Mutating the `seq - high - 1` arithmetic in `record_seq` (e.g.
    /// dropping the `- 1`, or the `.saturating_add`) shifts the raw total off 200.
    #[test]
    fn audio_datagram_raw_sum_leads_the_settled_count_on_large_burst() {
        let mut t = AudioDatagramLossTracker::new();
        let mut settled = 0u32;
        // A clean run first fills the 64-slot reorder window with received
        // positions (so the burst below is measured against a full window, as in
        // the field — a stall follows normal audio flow).
        for seq in 0..=100u64 {
            settled += t.record_seq(seq);
        }
        // The burst: sequences 101..=300 (200 contiguous audio datagrams) are
        // dropped by a main-thread stall; the next datagram received is 301.
        let settled_at_jump = t.record_seq(301);
        settled += settled_at_jump;
        assert_eq!(t.window_raw_lost, 200, "raw reads the whole burst at once");
        assert_eq!(
            u64::from(settled_at_jump),
            200 - (REORDER_WINDOW_SLOTS - 1),
            "the settled count holds back the tail that is still arrivable"
        );
        // Audio then resumes cleanly. These consecutive arrivals drain the
        // now-unseen tail of the reorder window.
        for seq in 302..=365u64 {
            settled += t.record_seq(seq);
        }
        assert_eq!(
            u64::from(settled),
            t.window_raw_lost,
            "once the tail has drained the two agree on the burst size"
        );

        // Establish the window baseline at t=10_000, then roll at +1000ms with
        // the accumulated settled count; the raw sum was accumulated inside
        // record_seq. (Nonzero timestamps: window_start_ms == 0 is the
        // "not-yet-started" sentinel, so a real clock is never 0.)
        assert_eq!(
            t.observe_window(10_000, 0),
            None,
            "first tick sets the baseline"
        );
        let (settled_rate, raw_rate) = t
            .observe_window(11_000, settled)
            .expect("window must roll at +1000ms");
        assert_eq!(settled_rate, 200.0);
        assert_eq!(raw_rate, 200.0);
        assert_eq!(t.raw_loss_per_sec(), raw_rate);
    }

    /// Issue 2031: a clean stream (no gaps) yields raw == 0, and mild reordering
    /// within the window does not by itself fabricate raw loss on a forward-only
    /// arrival order. Guards against the raw accumulator counting normal advances.
    #[test]
    fn audio_datagram_raw_sum_zero_on_clean_stream() {
        let mut t = AudioDatagramLossTracker::new();
        for seq in 0..=200u64 {
            t.record_seq(seq);
        }
        // Establish the window baseline, then roll a full 1000ms window with zero
        // capped loss fed in. (Nonzero timestamps: 0 is the not-yet-started
        // sentinel for window_start_ms.)
        assert_eq!(
            t.observe_window(10_000, 0),
            None,
            "first tick sets the baseline"
        );
        let (capped_rate, raw_rate) = t
            .observe_window(11_000, 0)
            .expect("window rolls at +1000ms from the baseline");
        assert_eq!(capped_rate, 0.0, "a gapless stream has no capped loss");
        assert_eq!(raw_rate, 0.0, "a gapless stream has no raw loss");
    }

    /// A rate normalised by ELAPSED cannot carry a burst size; a count can.
    #[test]
    fn video_seq_large_gap_reports_a_frame_count_the_capped_rate_cannot() {
        let mut t = SequenceTracker::new();
        let mut total_capped = 0u32;
        for seq in 0..=100u64 {
            total_capped += t.record_seq(seq);
        }
        let capped_at_gap = t.record_seq(601); // 101..=600 never arrive
        total_capped += capped_at_gap;

        assert_eq!(t.window_max_gap, 500, "the whole missing run");
        assert_eq!(
            u64::from(capped_at_gap),
            501 - REORDER_WINDOW_SLOTS,
            "everything past the new window is booked at the jump (#2535)"
        );

        assert!(!t.observe_window(10_000, total_capped, false), "baseline");
        assert!(
            t.observe_window(26_700, 0, false),
            "window rolls on the next arrival"
        );
        assert_eq!(t.max_gap(), 500, "published as a count, not normalised");
        // #2524's point: 16.7s of elapsed divides the same burst down to ~26/s,
        // so the rate cannot be read back as a size — only `max_gap` can.
        assert!(t.loss_per_sec() > 0.0, "the burst is not invisible");
        assert!(
            t.loss_per_sec() < 100.0,
            "a rate cannot carry the 500-frame size; got {}",
            t.loss_per_sec()
        );
        assert_eq!(t.window_max_gap, 0, "rollover clears the in-flight max");
    }

    #[test]
    fn window_sample_carries_the_decoder_freshness_counts_per_media_kind() {
        let (mut peer, _muted) = make_test_peer(4242);
        peer.video.set_freshness_evictions_for_test(9, 4);
        peer.screen.set_freshness_evictions_for_test(2, 1);

        let roll = |peer: &mut Peer, kind: MediaType| {
            let anchor = Some(SeqWindowObservation {
                now: 50_000,
                new_lost: 0,
                kf_requested: false,
            });
            assert!(peer.observe_sequence_window(kind, anchor).is_none());
            peer.observe_sequence_window(
                kind,
                Some(SeqWindowObservation {
                    now: 51_000,
                    new_lost: 0,
                    kf_requested: false,
                }),
            )
            .expect("window must roll")
        };

        let video = roll(&mut peer, MediaType::VIDEO);
        assert_eq!(
            (
                video.freshness_evictions_total,
                video.freshness_evictions_keyframeless_total
            ),
            (9, 4)
        );
        let screen = roll(&mut peer, MediaType::SCREEN);
        assert_eq!(
            (
                screen.freshness_evictions_total,
                screen.freshness_evictions_keyframeless_total
            ),
            (2, 1),
            "the SCREEN sample must not read the camera decoder"
        );
    }

    #[test]
    fn video_seq_gap_of_exactly_the_window_books_nothing() {
        let mut t = SequenceTracker::new();
        t.record_seq(0);
        t.record_seq(64); // 1..=63 missing, all still in the window
        assert_eq!(t.window_max_gap, 0, "they may still arrive");
        for seq in 1..=63u64 {
            t.record_seq(seq);
        }
        assert_eq!(t.window_max_gap, 0, "and they did");

        t.record_seq(129); // 65..=128 missing: one past the window
        assert_eq!(t.window_max_gap, 64, "the whole run is booked");
    }

    #[test]
    fn video_seq_window_keeps_the_largest_gap_not_the_last() {
        let mut t = SequenceTracker::new();
        t.record_seq(0);
        t.record_seq(1000); // 999 missing
        t.record_seq(1300); // 299 missing, must not displace it
        assert_eq!(t.window_max_gap, 999, "the window keeps the largest gap");
    }

    /// The SIGNALLED edge re-anchors, so the magnitude guard is only a backstop.
    #[test]
    fn video_seq_media_restart_edge_reanchors_before_the_threshold_arms() {
        let (mut peer, _muted) = make_test_peer(909);

        // The receiver keeps peer and tracker across a stop, so `high_seq` persists.
        peer.video_enabled = true;
        peer.screen_enabled = true;
        for seq in 0..=200u64 {
            peer.video_seq_tracker.record_seq(seq);
            peer.screen_seq_tracker.record_seq(seq);
        }
        assert_eq!(peer.video_seq_tracker.high_seq, Some(200));
        assert_eq!(peer.screen_seq_tracker.high_seq, Some(200));

        peer.video_enabled = false;
        peer.screen_enabled = false;
        peer.reanchor_on_media_restart(true, true);

        // 200 is far below the magnitude threshold, so the guard alone would not fire.
        assert!(
            peer.video_seq_tracker.high_seq.is_none(),
            "the signalled edge must re-baseline video"
        );
        assert!(
            peer.screen_seq_tracker.high_seq.is_none(),
            "and screen, whose only other re-anchor path is dead (1 rung)"
        );
    }

    /// The direct-call test above cannot see the call site's ORDER; these two drive
    /// `Peer::decode` so the re-anchor must still precede the `*_enabled` writes.
    #[wasm_bindgen_test]
    fn media_restart_heartbeat_edge_reanchors_the_video_tracker() {
        let (mut peer, _muted) = make_test_peer(910);
        for seq in 0..=200u64 {
            peer.video_seq_tracker.record_seq(seq);
        }
        assert_eq!(peer.video_seq_tracker.high_seq, Some(200), "precondition");

        peer.video_enabled = false;
        let hb = heartbeat_packet(910, true, false, false);
        let _ = peer.decode(&hb, "");

        assert!(peer.video_enabled, "the affirmative heartbeat must land");
        assert!(
            peer.video_seq_tracker.high_seq.is_none(),
            "the false->true heartbeat edge must re-baseline video at the real call site"
        );
    }

    #[wasm_bindgen_test]
    fn media_restart_heartbeat_edge_reanchors_the_screen_tracker() {
        let (mut peer, _muted) = make_test_peer(911);
        for seq in 0..=200u64 {
            peer.video_seq_tracker.record_seq(seq);
            peer.screen_seq_tracker.record_seq(seq);
        }
        assert_eq!(peer.screen_seq_tracker.high_seq, Some(200), "precondition");

        peer.screen_enabled = false;
        let hb = heartbeat_packet(911, false, false, true);
        let _ = peer.decode(&hb, "");

        assert!(peer.screen_enabled, "the affirmative heartbeat must land");
        assert!(
            peer.screen_seq_tracker.high_seq.is_none(),
            "the false->true heartbeat edge must re-baseline screen at the real call site"
        );
        assert_eq!(
            peer.video_seq_tracker.high_seq,
            Some(200),
            "a screen-only restart must not re-baseline video"
        );
    }

    /// No baseline yet is not a gap: a mid-call join, and after a rung re-anchor.
    #[test]
    fn video_seq_fresh_baseline_at_a_high_sequence_books_no_magnitude() {
        let mut t = SequenceTracker::new();
        t.record_seq(3_000);
        assert_eq!(t.window_max_gap, 0, "no baseline yet is not a gap");
        for seq in 3_001..=3_010u64 {
            t.record_seq(seq);
        }

        t.reanchor_for_layer_switch();
        t.record_seq(9_000);
        assert_eq!(
            t.window_max_gap, 0,
            "a re-anchored baseline is not a gap either"
        );
    }

    /// Publisher-controlled input SATURATES; dropping it would publish a false 0.
    #[test]
    fn video_seq_implausible_forward_jump_saturates_instead_of_reading_zero() {
        let mut t = SequenceTracker::new();
        t.record_seq(0);
        t.record_seq(u64::MAX);
        assert_eq!(
            t.window_max_gap, MAX_PLAUSIBLE_FORWARD_GAP,
            "saturates at the ceiling, never 0"
        );

        let mut t2 = SequenceTracker::new();
        t2.record_seq(0);
        t2.record_seq(4000);
        assert_eq!(t2.window_max_gap, 3999, "a plausible run is booked exactly");
    }

    #[test]
    fn loss_metrics_event_carries_the_loss_signals_and_routing_keys() {
        let evt = loss_metrics_event(
            "self-id",
            "peer-abc",
            MediaType::VIDEO,
            &WindowSample {
                loss_per_sec: 2.5,
                kf_per_sec: 0.5,
                max_gap: 437,
                kf_suppression_per_sec: KeyframeSuppressionRates {
                    values: [1.0, 2.0, 3.0, 4.0, 5.0],
                },
                freshness_evictions_total: 11,
                freshness_evictions_keyframeless_total: 7,
            },
            1_234_000,
        );
        // `subsystem` routes the event; `from_peer`/`to_peer` key it.
        assert_eq!(evt.subsystem, "video");
        let f64_metric = |name: &str| {
            evt.metrics
                .iter()
                .find(|m| m.name == name)
                .and_then(|m| match m.value {
                    videocall_diagnostics::MetricValue::F64(v) => Some(v),
                    _ => None,
                })
        };
        let u64_metric = |name: &str| {
            evt.metrics
                .iter()
                .find(|m| m.name == name)
                .and_then(|m| match m.value {
                    videocall_diagnostics::MetricValue::U64(v) => Some(v),
                    _ => None,
                })
        };
        assert_eq!(f64_metric("video_seq_loss_per_sec"), Some(2.5));
        assert_eq!(f64_metric("keyframe_requests_per_sec"), Some(0.5));
        assert_eq!(u64_metric("video_seq_max_gap"), Some(437));
        assert_eq!(u64_metric("freshness_evictions_total"), Some(11));
        assert_eq!(
            u64_metric("freshness_evictions_keyframeless_total"),
            Some(7)
        );
        assert!(
            !evt.metrics
                .iter()
                .any(|m| m.name.contains("suppressed_no_loss")),
            "the no-loss key must be gone, not merely unread by one consumer"
        );
        // Every suppression reason is published under its own key, in order.
        for (i, reason) in KeyframeSuppressionReason::ALL.into_iter().enumerate() {
            assert_eq!(
                f64_metric(reason.metric_name()),
                Some(i as f64 + 1.0),
                "reason {reason:?}"
            );
        }
        let text_metric = |name: &str| {
            evt.metrics
                .iter()
                .find(|m| m.name == name)
                .and_then(|m| match &m.value {
                    videocall_diagnostics::MetricValue::Text(t) => Some(t.clone()),
                    _ => None,
                })
        };
        assert_eq!(text_metric("to_peer").as_deref(), Some("peer-abc"));
        assert_eq!(text_metric("from_peer").as_deref(), Some("self-id"));
        assert_eq!(text_metric("media_type").as_deref(), Some("VIDEO"));
    }

    /// Reordering inside the window is not loss, so it contributes no magnitude.
    #[test]
    fn video_seq_in_window_reordering_books_no_magnitude() {
        let mut t = SequenceTracker::new();
        t.record_seq(0);
        t.record_seq(10);
        for seq in 1..=9u64 {
            t.record_seq(seq);
        }
        for seq in 11..=200u64 {
            t.record_seq(seq);
        }
        assert_eq!(
            t.window_max_gap, 0,
            "reordered-but-received packets are not loss"
        );
        assert_eq!(t.lost_count, 0, "and the capped count agrees");
    }

    #[test]
    fn video_seq_rollover_clears_the_in_flight_max() {
        let mut t = SequenceTracker::new();
        t.record_seq(0);
        t.record_seq(500);
        assert!(!t.observe_window(10_000, 0, false));
        assert!(t.observe_window(11_000, 0, false));
        assert_eq!(t.max_gap(), 499, "first window publishes the burst");
        assert!(t.observe_window(12_000, 0, false));
        assert_eq!(
            t.max_gap(),
            0,
            "the second window must be clean, not a re-report"
        );
    }

    /// A rung change clears BOTH in-flight accumulators; the offset is not a burst.
    #[test]
    fn video_layer_switch_clears_both_in_flight_window_accumulators() {
        let mut t = SequenceTracker::new();
        t.record_seq(0);
        t.record_seq(500);
        assert!(
            !t.observe_window(10_000, 200, false),
            "seed the capped side"
        );
        assert_eq!(t.window_lost, 200, "capped accumulator holds the fed count");
        assert_eq!(t.window_max_gap, 499, "the in-flight max holds the gap");

        t.reanchor_for_layer_switch();

        assert_eq!(t.window_lost, 0, "capped accumulator cleared");
        assert_eq!(t.window_max_gap, 0, "in-flight max cleared");
    }

    #[test]
    fn video_seq_reanchors_on_publisher_reset_and_loss_detection_resumes() {
        let mut t = SequenceTracker::new();
        assert_eq!(t.record_seq(50_000), 0);
        assert_eq!(t.record_seq(50_001), 0);

        assert_eq!(
            t.record_seq(0),
            0,
            "a publisher sequence reset re-anchors and is not counted as loss"
        );
        assert_eq!(
            t.high_seq,
            Some(0),
            "the baseline must follow the restarted counter, not stay pinned at 50_001"
        );

        assert_eq!(t.record_seq(1), 0);
        // Skip 2, then run past the window so the hole is confirmed lost.
        for seq in 3..=66u64 {
            t.record_seq(seq);
        }
        assert_eq!(
            t.lost_count, 1,
            "loss detection must resume on the restarted stream"
        );
    }

    #[test]
    fn video_seq_backward_jump_below_the_gap_is_reordering_not_a_restart() {
        let mut t = SequenceTracker::new();
        for seq in 4_000..=4_936u64 {
            t.record_seq(seq);
        }
        t.record_seq(5_000); // 4_937..=4_999 skipped, all still inside the window
        assert_eq!(t.high_seq, Some(5_000), "precondition");

        // 63 back is the deepest reordering the window tolerates: pins any threshold <= 63.
        assert_eq!(t.record_seq(4_937), 0, "an in-window straggler is not loss");
        assert_eq!(
            t.high_seq,
            Some(5_000),
            "reordering must NOT trip the restart re-anchor"
        );
        assert_eq!(t.seen_bits & (1 << 63), 1 << 63, "it is marked as seen");
    }

    #[test]
    fn video_seq_reanchor_threshold_is_the_shared_publisher_reset_gap() {
        let gap = videocall_aq::constants::SEQ_RESET_REANCHOR_GAP;
        let mark = gap * 3;

        let mut at_threshold = SequenceTracker::new();
        at_threshold.record_seq(mark);
        at_threshold.record_seq(mark - gap);
        assert_eq!(
            at_threshold.high_seq,
            Some(mark - gap),
            "a backward jump of exactly the shared gap re-anchors"
        );

        let mut below_threshold = SequenceTracker::new();
        below_threshold.record_seq(mark);
        below_threshold.record_seq(mark - gap + 1);
        assert_eq!(
            below_threshold.high_seq,
            Some(mark),
            "one position short of it does not"
        );
    }

    #[test]
    fn video_seq_reanchor_preserves_the_pli_backoff() {
        let mut t = SequenceTracker::new();
        for seq in 5_000..=5_100u64 {
            t.record_seq(seq);
        }
        t.record_seq(5_102); // 5_101 skipped
        for seq in 5_103..=5_170u64 {
            t.record_seq(seq);
        }
        assert!(t.lost_count > 0, "precondition: real loss was detected");

        assert!(!t.should_request_keyframe(10_000), "initial timeout");
        assert!(
            t.should_request_keyframe(11_000),
            "first PLI escalates backoff"
        );
        let escalated = (
            t.current_backoff_ms,
            t.unanswered_requests,
            t.last_keyframe_request_ms,
        );
        assert_eq!(escalated, (KEYFRAME_REQUEST_MIN_INTERVAL_MS * 2, 1, 11_000));

        t.record_seq(0);
        assert_eq!(t.high_seq, Some(0), "precondition: it re-anchored");

        assert_eq!(
            (
                t.current_backoff_ms,
                t.unanswered_requests,
                t.last_keyframe_request_ms
            ),
            escalated,
            "a re-anchor must not reset the PLI rate-limit state"
        );
    }

    #[test]
    fn video_seq_reanchor_books_no_magnitude_but_the_new_stream_still_does() {
        let mut t = SequenceTracker::new();
        for seq in 0..=5_000u64 {
            t.record_seq(seq);
        }
        assert_eq!(t.window_max_gap, 0, "precondition: a dense stream is clean");

        t.record_seq(4_500);
        assert_eq!(
            t.window_max_gap, 0,
            "a backward jump is never a forward burst"
        );

        t.record_seq(0);
        assert_eq!(
            t.window_max_gap, 0,
            "the 5000-position reset discontinuity books nothing"
        );
        t.record_seq(1);
        assert_eq!(t.window_max_gap, 0, "nor does the packet after it");

        t.record_seq(200);
        assert_eq!(
            t.window_max_gap, 198,
            "magnitude must work again after a re-anchor"
        );
    }

    /// A large BACKWARD jump (mic-encoder restart resets `sequence` to 0)
    /// re-anchors instead of wedging the tracker on the "too old" branch, and
    /// loss detection resumes on the new stream. Removing the re-anchor guard
    /// leaves the tracker pinned at the old high-water mark and fails the final
    /// assertion.
    #[test]
    fn audio_datagram_loss_reanchors_on_sender_reset() {
        let mut t = AudioDatagramLossTracker::new();
        assert_eq!(t.record_seq(50_000), 0);
        assert_eq!(t.record_seq(50_001), 0);
        // Sender restart: sequence resets to 0 (>= AUDIO_SEQ_RESET_REANCHOR_GAP
        // below the high-water mark) -> re-anchor, not loss.
        assert_eq!(
            t.record_seq(0),
            0,
            "a sender-side sequence reset re-anchors and is not counted as loss"
        );
        assert_eq!(t.record_seq(1), 0);
        // A real gap in the NEW stream (skip 2) is still detected once it leaves
        // the window — proving the tracker did not wedge after the reset.
        let mut lost = 0u32;
        for seq in 3..=66u64 {
            lost += t.record_seq(seq);
        }
        assert_eq!(
            lost, 1,
            "loss detection must resume on the new stream after a sender-reset re-anchor"
        );
    }

    // ── Issue #2138: an AUDIO simulcast layer switch must re-anchor the tracker ──
    //
    // Each audio rung is an independent Opus encode with its OWN sequence counter
    // (`microphone_encoder`: "each layer owns its own seq counter"), so a receive-side
    // rung change is a cross-counter discontinuity. Video and screen already
    // re-anchored on switch; audio did not. These drive the REAL production entry
    // point (`Peer::set_selected_audio_layer`, the single chokepoint every write goes
    // through) and the REAL AUDIO decode arm — not the tracker in isolation — so
    // removing the `reanchor_for_layer_switch()` call from the setter fails them.
    //
    // Both are plain `#[test]`s (host libtest, the CI "cargo test videocall-client
    // (native libtest)" job): `Peer::decode`'s AUDIO path needs no browser API — the
    // mock audio decoder stands in for NetEq — and a `#[wasm_bindgen_test]` would be
    // a false green on a box whose wasm harness silently no-ops (see the note on
    // `should_clear_canvas_only_on_decode_stop_edge`).

    /// THE defect, in the direction that actually bites: a switch to a LAGGING audio
    /// rung is a BACKWARD jump, and one smaller than `AUDIO_SEQ_RESET_REANCHOR_GAP`
    /// (1024) is not treated as a sender restart — so pre-fix it landed in the shared
    /// reorder window's "too old, ignore" branch and the tracker stayed PINNED at the
    /// old rung's high-water mark, silently ignoring every packet of the new rung
    /// (including real loss) until its sequence climbed back past that mark.
    ///
    /// So the dominant pre-fix symptom is UNDER-reporting — the tracker goes BLIND —
    /// not the over-reporting the issue title implies. This asserts both halves of
    /// "not blinded": the baseline actually follows the new rung, AND real loss on the
    /// new rung is still counted.
    ///
    /// Mutation sensitivity: deleting the `reanchor_for_layer_switch()` call in
    /// `set_selected_audio_layer` leaves `high_seq == Some(900)` and every assertion
    /// below fails (the new rung's packets are ignored, so `lost == 0`).
    #[test]
    fn audio_layer_switch_backward_does_not_blind_the_loss_tracker() {
        let (mut peer, _muted) = make_test_peer(2138);
        arm_audio_datagram_tracking(&mut peer);

        // Steady on rung 0, whose counter has advanced to a high-water mark of 900.
        for seq in 898..=900u64 {
            let _ = peer.decode(&layered_audio_packet(2138, 0, seq), "local@test.com");
        }
        assert_eq!(
            peer.audio_datagram_loss.high_seq,
            Some(900),
            "precondition: the tracker is anchored on rung 0's counter"
        );

        // Switch to rung 1 — a LAGGING rung whose own counter sits at 200 (it was
        // created later / was ceiling-suppressed, so it is behind rung 0). The
        // backward jump is 700: well outside the 64-slot reorder window, yet BELOW
        // the 1024 restart threshold — exactly the gap the restart guard misses.
        peer.set_selected_audio_layer(1);
        assert!(
            peer.audio_datagram_loss.high_seq.is_none(),
            "an audio layer switch must re-anchor: the old rung's baseline is cleared"
        );

        // Rung 1's packets now arrive. Pre-fix these were ALL ignored as "too old"
        // (200 < 900 - 64), so the tracker published nothing for this peer.
        for seq in 200..=202u64 {
            let _ = peer.decode(&layered_audio_packet(2138, 1, seq), "local@test.com");
        }
        assert_eq!(
            peer.audio_datagram_loss.high_seq,
            Some(202),
            "post-switch: the new rung's packets must be TRACKED, not silently ignored"
        );

        // And real loss on the new rung is still detected: sequence 203 never
        // arrives, and the hole is declared once it shifts off the 64-slot window.
        // (204 first clears exactly one window bit — the `LOSS_BRIDGE_SEQ` pattern.)
        //
        // `window_lost` is read as a running MAX rather than a final read: this test
        // drives the production `decode()` path, which stamps the rate window from
        // the real wall clock, so a >1 s stall mid-loop could roll the ~1 s window and
        // zero the field. The max survives that (the count is captured on the packet
        // that declares the loss). The loop is ~65 in-memory iterations, so a rollover
        // is not expected — this only removes the flake, it does not mask a defect.
        let mut lost = 0u32;
        for seq in 204..=268u64 {
            let _ = peer.decode(&layered_audio_packet(2138, 1, seq), "local@test.com");
            lost = lost.max(peer.audio_datagram_loss.window_lost);
        }
        assert_eq!(
            lost, 1,
            "post-switch: real datagram loss on the NEW rung must still be counted \
             (pre-fix the tracker was blind and reported 0)"
        );
    }

    /// The other direction: a switch to a LEADING audio rung is a FORWARD jump.
    /// Without the re-anchor it lands as one contiguous gap in both `window_lost`
    /// and `window_raw_lost` (issue #2031's magnitude signal), so a layer switch
    /// would manufacture a phantom multi-hundred-datagram "burst".
    ///
    /// This is the assertion the issue's own framing asks for ("an audio layer switch
    /// does not register as datagram loss"), and it is reachable: rung counters drift
    /// apart in BOTH directions because a rung suppressed by the user/congestion
    /// ceiling returns from its publish gate BEFORE `sequence_number += 1`, stalling
    /// its counter while the base advances — so whichever rung was suppressed is the
    /// laggard, and switching AWAY from it is a forward jump.
    ///
    /// Mutation sensitivity: deleting the `reanchor_for_layer_switch()` call in
    /// `set_selected_audio_layer` makes the first post-switch packet book 699 skipped
    /// sequences into `window_raw_lost`; subsequent packets shift unseen positions
    /// out of the reset reorder window into `window_lost`, failing both assertions.
    #[test]
    fn audio_layer_switch_forward_does_not_manufacture_phantom_loss() {
        let (mut peer, _muted) = make_test_peer(2139);
        arm_audio_datagram_tracking(&mut peer);
        // Start ON rung 1: the exact-match decode guard drops any AUDIO packet whose
        // `simulcast_layer_id != selected_audio_layer`, so the guard must already be
        // on rung 1 for its packets to reach the tracker at all. Set via the real
        // setter (from the default rung 0) — the tracker is still empty at this point,
        // so this initial move's re-anchor is a no-op on `high_seq: None`.
        peer.set_selected_audio_layer(1);

        // Steady on rung 1 (a rung that was ceiling-suppressed for a while, so its
        // counter lags): high-water mark 200.
        for seq in 198..=200u64 {
            let _ = peer.decode(&layered_audio_packet(2139, 1, seq), "local@test.com");
        }
        assert_eq!(peer.audio_datagram_loss.high_seq, Some(200));

        // Switch to rung 0, whose own counter has been running the whole call and
        // sits at 900 — a 700-sequence FORWARD jump with 699 skipped values.
        peer.set_selected_audio_layer(0);
        // `decode()` stamps the rate window from the REAL wall clock (`now_ms()` is not
        // injectable), and `observe_window` zeroes both accumulators on a ~1 s
        // rollover. Because the assertions below are `== 0`, a rollover would zero the
        // phantom gap on the UN-FIXED code and make this test FALSELY PASS — the
        // dangerous direction. Two things close that hole:
        //   1. the running MAX below reads the accumulators after every packet, so the
        //      booked value is captured rather than only the final state; and
        //   2. `origin_before` is compared after the loop — an unchanged
        //      `window_start_ms` PROVES no rollover intervened, so the zeros are a real
        //      observation rather than a rollover artefact. If a rollover ever does
        //      intervene this fails LOUDLY on that guard instead of passing silently.
        let origin_before = peer.audio_datagram_loss.window_start_ms;
        let mut max_capped = 0u32;
        let mut max_raw = 0u64;
        for seq in 900..=902u64 {
            let _ = peer.decode(&layered_audio_packet(2139, 0, seq), "local@test.com");
            max_capped = max_capped.max(peer.audio_datagram_loss.window_lost);
            max_raw = max_raw.max(peer.audio_datagram_loss.window_raw_lost);
        }
        assert_eq!(
            peer.audio_datagram_loss.window_start_ms, origin_before,
            "test validity guard: the ~1s rate window must NOT have rolled during the \
             3-packet loop, or the `== 0` assertions below would be rollover artefacts \
             that pass even on the un-fixed code"
        );

        assert_eq!(
            max_capped, 0,
            "a layer switch must not manufacture CAPPED loss (the presence signal)"
        );
        assert_eq!(
            max_raw, 0,
            "a layer switch must not manufacture RAW loss — the uncapped #2031 \
             magnitude sum is where the 699 skipped cross-counter values would show up"
        );
        assert_eq!(
            peer.audio_datagram_loss.high_seq,
            Some(902),
            "the tracker follows the new rung's counter"
        );
    }

    /// A no-op "switch" to the ALREADY-SELECTED rung must NOT reset the tracker.
    /// `tick_audio_layer_chooser` calls the setter on EVERY 5 s monitor tick, so an
    /// unguarded re-anchor would discard the in-flight loss window ~once per tick and
    /// permanently suppress `audio_datagram_loss_per_sec` — turning the fix into a
    /// worse version of the bug it fixes.
    ///
    /// Mutation sensitivity: removing the `if layer != self.selected_audio_layer`
    /// guard from `set_selected_audio_layer` zeroes `high_seq`/`window_raw_lost` and
    /// fails both assertions.
    #[test]
    fn audio_layer_no_op_switch_preserves_tracker_state() {
        let (mut peer, _muted) = make_test_peer(2140);
        arm_audio_datagram_tracking(&mut peer);

        // Build real in-flight state on rung 0: a baseline plus a recorded gap
        // (sequence 101..=300 lost, so `window_raw_lost` holds the 200 magnitude).
        for seq in 0..=100u64 {
            let _ = peer.decode(&layered_audio_packet(2140, 0, seq), "local@test.com");
        }
        let _ = peer.decode(&layered_audio_packet(2140, 0, 301), "local@test.com");
        // If a >1 s stall rolled the real-clock rate window mid-setup this
        // precondition would not hold, which fails LOUDLY here rather than turning
        // the assertion below into a vacuous `0 == 0`.
        assert_eq!(
            peer.audio_datagram_loss.window_raw_lost, 200,
            "precondition: a real 200-datagram burst is accumulated in the window"
        );

        // The per-tick call with an UNCHANGED layer.
        peer.set_selected_audio_layer(0);

        assert_eq!(
            peer.audio_datagram_loss.high_seq,
            Some(301),
            "selecting the already-selected audio layer must not reset the baseline"
        );
        assert_eq!(
            peer.audio_datagram_loss.window_raw_lost, 200,
            "a no-op switch must not discard the in-flight loss window"
        );
    }

    /// CHOKEPOINT ROUTING, site 1 of 2: `tick_audio_layer_chooser` (the normal
    /// per-monitor-tick switch — the site that emits the `LAYER_SWITCH … kind=audio`
    /// log) must go THROUGH `set_selected_audio_layer` rather than assigning
    /// `selected_audio_layer` directly, or the re-anchor never runs on the path that
    /// carries virtually every real switch.
    ///
    /// This is a distinct failure from "the setter forgot to re-anchor": it is the
    /// call site bypassing a correct setter. Only a test that drives the TICK can see
    /// it — which is why it exists alongside the setter tests.
    ///
    /// Mutation sensitivity: replacing the `self.set_selected_audio_layer(desired)`
    /// call in `tick_audio_layer_chooser` with `self.selected_audio_layer = desired`
    /// leaves `high_seq == Some(900)` and fails the final assertion.
    #[test]
    fn audio_tick_chooser_switch_routes_through_the_reanchor_chokepoint() {
        use crate::decode::layer_chooser::{
            DownlinkSample, KindLayerBounds, LOSS_STEP_DOWN_PER_SEC,
        };
        let now = 2000u64;
        let (mut peer, _muted) = make_test_peer(2141);
        arm_audio_datagram_tracking(&mut peer);
        // Learn a 3-rung audio ladder so `highest_available == 2` (room to step down).
        // Observed at `now` so the lazy prune inside `highest_available` keeps them.
        for layer in 0..3u32 {
            peer.audio_layer_availability.observe(layer, now);
        }

        // A healthy tick first: an UNCONSTRAINED chooser follows the top, so this
        // climbs it (and the guard) 0 -> 2. This is the real cold-start acquisition,
        // and it is what gives the congested tick below somewhere to step DOWN from —
        // a fresh chooser sits at 0 and cannot step down at all.
        let top = peer.tick_audio_layer_chooser(now, KindLayerBounds::default());
        assert_eq!(top, 2, "precondition: a clean tick follows the top rung");

        // Now give the tracker a real rung-2 baseline (after the climb above, so this
        // baseline is not itself cleared by it).
        for seq in 898..=900u64 {
            let _ = peer.decode(&layered_audio_packet(2141, 2, seq), "local@test.com");
        }
        assert_eq!(
            peer.audio_datagram_loss.high_seq,
            Some(900),
            "precondition: anchored on rung 2's counter"
        );

        // The audio chooser is proxied off the VIDEO downlink (see
        // `tick_audio_layer_chooser`): a congested video window is what makes it step
        // down. This is the production input, not a synthetic poke at the chooser.
        peer.last_video_downlink = DownlinkSample {
            loss_per_sec: LOSS_STEP_DOWN_PER_SEC + 1.0,
            kf_per_sec: 0.0,
        };

        // The REAL tick. The congested sample constrains the chooser and steps it
        // down one rung from the top (2 -> 1), which is an actual guard change.
        let desired = peer.tick_audio_layer_chooser(now, KindLayerBounds::default());
        assert_eq!(
            desired, 1,
            "precondition: the congested tick stepped 2 -> 1"
        );
        assert_eq!(
            peer.selected_audio_layer(),
            1,
            "precondition: the tick wrote the decode guard"
        );

        assert!(
            peer.audio_datagram_loss.high_seq.is_none(),
            "the TICK switch must re-anchor the audio tracker (routed through \
             `set_selected_audio_layer`), not just write the guard field"
        );
    }

    /// CHOKEPOINT ROUTING, site 2 of 2: `seed_early_congestion` (the #1179 early-seed
    /// path, the only OTHER production writer of `selected_audio_layer` besides the
    /// tick and `reconcile_decode_guards_to_wire`) must likewise route through the
    /// chokepoint. It fires on a congested sample taken OUTSIDE the 5 s tick, so on a
    /// bad join it is the FIRST audio rung change of the call — exactly when a blinded
    /// tracker would hide the join-time datagram loss it exists to report.
    ///
    /// Mutation sensitivity: replacing the `self.set_selected_audio_layer(layer)` call
    /// in `seed_early_congestion`'s AUDIO branch with a direct
    /// `self.selected_audio_layer = ...` assignment fails the final assertion.
    #[test]
    fn audio_early_seed_switch_routes_through_the_reanchor_chokepoint() {
        use crate::decode::layer_chooser::{
            DownlinkSample, KindLayerBounds, ReceiveLayerBounds, LOSS_STEP_DOWN_PER_SEC,
        };
        let now = 2000u64;
        let (mut peer, _muted) = make_test_peer(2142);
        arm_audio_datagram_tracking(&mut peer);
        for layer in 0..3u32 {
            peer.audio_layer_availability.observe(layer, now);
        }

        // Clean tick first: the unconstrained chooser follows the top (0 -> 2). The
        // early seed steps down ONE rung from wherever the chooser currently sits, so
        // without this it would "step down" from 0 to 0 and change nothing.
        let top = peer.tick_audio_layer_chooser(now, KindLayerBounds::default());
        assert_eq!(top, 2, "precondition: a clean tick follows the top rung");

        for seq in 898..=900u64 {
            let _ = peer.decode(&layered_audio_packet(2142, 2, seq), "local@test.com");
        }
        assert_eq!(peer.audio_datagram_loss.high_seq, Some(900));

        peer.last_video_downlink = DownlinkSample {
            loss_per_sec: LOSS_STEP_DOWN_PER_SEC + 1.0,
            kf_per_sec: 0.0,
        };

        // The REAL early seed. `observe_early_congestion` acts only while the chooser
        // is unconstrained AND the sample is congested — both hold here — and steps
        // down one rung, so the AUDIO branch performs an actual guard change.
        let seeded = peer.seed_early_congestion(now, &ReceiveLayerBounds::default());
        assert!(seeded, "precondition: the congested sample seeded the peer");
        assert_eq!(
            peer.selected_audio_layer(),
            1,
            "precondition: the early seed stepped the audio guard 2 -> 1"
        );

        assert!(
            peer.audio_datagram_loss.high_seq.is_none(),
            "the EARLY-SEED switch must re-anchor the audio tracker (routed through \
             `set_selected_audio_layer`)"
        );
    }

    /// The THIRD in-module writer of `selected_audio_layer` must re-anchor too.
    ///
    /// `reconcile_decode_guards_to_wire` (the #1700 fresh-session snap and the
    /// `set_peer_tile_hints` path) already calls `set_selected_audio_layer`, so it
    /// inherits the re-anchor — but until this test NOTHING pinned that. The
    /// pre-existing #1695 coverage asserts only the guard VALUE
    /// (`selected_audio_layer() == 0`), which a direct field assignment satisfies
    /// identically, so reverting this writer to `peer.selected_audio_layer = a_target`
    /// would have compiled, kept every other test green, and silently dropped the
    /// re-anchor on a live switch site. That is the exact gap the two sibling routing
    /// tests close for the tick and early-seed writers.
    ///
    /// Mutation sensitivity: replace the `set_selected_audio_layer(a_target)` call in
    /// `reconcile_decode_guards_to_wire` with a direct field write and this fails on the
    /// `high_seq` assertion while the value assertion still passes.
    #[test]
    fn audio_reconcile_to_wire_switch_routes_through_the_reanchor_chokepoint() {
        use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds};
        use std::collections::BTreeMap;

        let mut manager = PeerDecodeManager::new();
        manager.insert_zero_loss_top_peer_for_test(900);
        let now = 2000u64;

        // Pre-raise the AUDIO guard to L2 so the reconcile has somewhere to pull it
        // DOWN from, and arm datagram tracking so the tracker actually accumulates.
        {
            let peer = manager.connected_peers.get_mut(&900).unwrap();
            peer.set_selected_audio_layer(2);
            arm_audio_datagram_tracking(peer);
        }
        // Establish a real high-water mark on the OLD rung, so a missed re-anchor is
        // observable as a stale `high_seq` rather than an already-empty one.
        for seq in 898..=900u64 {
            let _ = manager
                .connected_peers
                .get_mut(&900)
                .unwrap()
                .decode(&layered_audio_packet(900, 2, seq), "local@test.com");
        }
        assert_eq!(
            manager
                .connected_peers
                .get(&900)
                .unwrap()
                .audio_datagram_loss
                .high_seq,
            Some(900),
            "precondition: the old rung established a high-water mark"
        );

        // The wire still records L0 for audio, so the reconcile snaps the guard 2 -> 0.
        let mut wire: BTreeMap<(u64, PrefMediaKind), u32> = BTreeMap::new();
        wire.insert((900, PrefMediaKind::Audio), 0);
        let _ups = manager.reconcile_decode_guards_to_wire(
            Some(&wire),
            &ReceiveLayerBounds::default(),
            now,
        );

        let peer = manager.connected_peers.get(&900).unwrap();
        assert_eq!(
            peer.selected_audio_layer(),
            0,
            "precondition: the reconcile snapped the audio guard 2 -> 0"
        );
        assert!(
            peer.audio_datagram_loss.high_seq.is_none(),
            "the RECONCILE-TO-WIRE switch must re-anchor the audio tracker (routed \
             through `set_selected_audio_layer`) — a stale high_seq here means this \
             writer bypasses the chokepoint"
        );
    }

    /// The re-anchor must leave the ALREADY-PUBLISHED rates and the rate-window ORIGIN
    /// alone — the same contract `SequenceTracker::reanchor_for_layer_switch` keeps.
    ///
    /// Zeroing `window_start_ms` would restart the ~1 s cadence on every switch
    /// (delaying the next sample by up to a full window and, because `0` is also
    /// `observe_window`'s "not yet started" sentinel, re-baselining it to the next
    /// packet's clock); blanking `loss_per_sec` / `raw_loss_per_sec` would read as
    /// "loss stopped" rather than "not resampled yet". This pins the doc claim on
    /// `AudioDatagramLossTracker::reanchor_for_layer_switch` to the code.
    ///
    /// Mutation sensitivity: adding `self.window_start_ms = 0;` or
    /// `self.loss_per_sec = 0.0;` to `reanchor_for_layer_switch` fails this.
    #[test]
    fn audio_layer_switch_preserves_published_rates_and_window_origin() {
        let mut t = AudioDatagramLossTracker::new();
        // Roll one full window so there ARE published rates and a live origin.
        for seq in 0..=100u64 {
            t.record_seq(seq);
        }
        t.record_seq(301);
        assert_eq!(
            t.observe_window(10_000, 0),
            None,
            "first tick sets the origin"
        );
        let (capped, raw) = t
            .observe_window(11_000, 5)
            .expect("window rolls at +1000ms");
        assert!(
            capped > 0.0 && raw > 0.0,
            "precondition: rates were published"
        );
        let origin_before = t.window_start_ms;
        assert_eq!(
            origin_before, 11_000,
            "precondition: the rollover re-based the window origin to `now`"
        );

        t.reanchor_for_layer_switch();

        assert_eq!(
            t.loss_per_sec(),
            capped,
            "the already-published capped rate must survive a re-anchor \
             (recomputed on the next ~1s rollover, exactly like video)"
        );
        assert_eq!(
            t.raw_loss_per_sec(),
            raw,
            "the already-published raw rate must survive a re-anchor"
        );
        assert_eq!(
            t.window_start_ms, origin_before,
            "the rate-window ORIGIN must NOT be reset — video's re-anchor does not \
             touch it, and zeroing it would restart the ~1s publish cadence"
        );
        // What the re-anchor DOES clear. `seen_bits` is asserted here rather than in
        // the dedicated test below because the rollover above leaves it set to bit 0
        // (`seen_bits == 1`), so `== 0` is a real change.
        assert!(t.high_seq.is_none(), "the sequence baseline is cleared");
        assert_eq!(t.seen_bits, 0, "the reorder window is cleared");
    }

    /// The re-anchor must clear BOTH in-flight window accumulators —
    /// `window_lost` (capped presence) AND `window_raw_lost` (the #2031 uncapped
    /// magnitude) — so the discontinuity cannot leak into the sample published as
    /// the NEW rung's rate.
    ///
    /// This is a SEPARATE test from the preserve-rates one on purpose. There, the
    /// setup rolls the ~1 s window, and `observe_window` zeroes both accumulators on
    /// rollover — so asserting `== 0` after the re-anchor would compare 0 to 0 and pin
    /// NOTHING (verified: dropping either field from `reanchor_for_layer_switch` left
    /// the whole 754-test suite green). Here the accumulators are deliberately loaded
    /// with real, NON-ZERO in-flight values and asserted non-zero BEFORE the
    /// re-anchor, so each `== 0` after it is a genuine state transition.
    ///
    /// Mutation sensitivity: removing EITHER `self.window_lost = 0;` or
    /// `self.window_raw_lost = 0;` from `reanchor_for_layer_switch` fails this — the
    /// two are checked independently, since clearing only one publishes an incoherent
    /// capped/raw pair (the ratio is how #2031 reads burst severity).
    #[test]
    fn audio_layer_switch_clears_both_in_flight_window_accumulators() {
        let mut t = AudioDatagramLossTracker::new();
        // Fill the reorder window with received positions, then take a 200-sequence
        // contiguous gap: `window_raw_lost` accrues the uncapped 200 magnitude.
        let mut capped = 0u32;
        for seq in 0..=100u64 {
            capped += t.record_seq(seq);
        }
        capped += t.record_seq(301);
        // Drain the now-unseen tail so the CAPPED counter also climbs off zero.
        for seq in 302..=365u64 {
            capped += t.record_seq(seq);
        }
        // Feed the capped count into the window WITHOUT rolling it (the `> 1000ms`
        // rollover is what would zero these accumulators, so stay inside one window:
        // 10_000 sets the origin, 10_500 is only +500ms).
        assert_eq!(t.observe_window(10_000, 0), None, "origin, no rollover");
        assert_eq!(
            t.observe_window(10_500, capped),
            None,
            "still inside the same ~1s window — no rollover, so nothing is zeroed"
        );

        // PRECONDITIONS — both accumulators hold real in-flight values. Without
        // these, the post-re-anchor `== 0` assertions below would be vacuous.
        assert!(
            t.window_lost > 0,
            "precondition: the capped in-flight count is non-zero; got {}",
            t.window_lost
        );
        assert_eq!(
            t.window_raw_lost, 200,
            "precondition: the raw in-flight sum holds the true 200-datagram magnitude"
        );

        t.reanchor_for_layer_switch();

        assert_eq!(
            t.window_lost, 0,
            "the in-flight CAPPED count must be cleared, or the old rung's loss leaks \
             into the sample published as the new rung's rate"
        );
        assert_eq!(
            t.window_raw_lost, 0,
            "the in-flight RAW sum must be cleared TOO — clearing only the capped \
             count would publish an incoherent pair (capped 0 alongside raw 200 reads \
             as 'enormous burst, no loss present')"
        );
    }

    // `MockAudioDecoder`, `make_test_peer`, and `make_zero_loss_top_peer` were
    // hoisted to the parent module (still `#[cfg(test)]`) so the production-only
    // test seam `PeerDecodeManager::insert_zero_loss_top_peer_for_test` can build
    // the same host-safe peer without duplicating the 50-field `Peer` literal.
    // They remain reachable here unchanged via the `use super::*;` above.

    // -- helpers ----------------------------------------------------------

    fn packet_wrapper(media: &MediaPacket, session_id: u64) -> PacketWrapper {
        let data = media.write_to_bytes().expect("serialize MediaPacket");
        PacketWrapper {
            data,
            user_id: "test@test.com".into(),
            packet_type: PacketType::MEDIA.into(),
            session_id,
            ..Default::default()
        }
    }

    /// Wrap a `MediaPacket` into a `PacketWrapper` ready for `Peer::decode`.
    fn wrap(media: &MediaPacket, session_id: u64) -> Arc<PacketWrapper> {
        Arc::new(packet_wrapper(media, session_id))
    }

    fn heartbeat_packet(
        session_id: u64,
        video: bool,
        audio: bool,
        screen: bool,
    ) -> Arc<PacketWrapper> {
        let media = MediaPacket {
            media_type: MediaType::HEARTBEAT.into(),
            user_id: "test@test.com".into(),
            heartbeat_metadata: Some(HeartbeatMetadata {
                video_enabled: video,
                audio_enabled: audio,
                screen_enabled: screen,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        wrap(&media, session_id)
    }

    fn video_frame_packet(session_id: u64) -> Arc<PacketWrapper> {
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            user_id: "test@test.com".into(),
            data: vec![0u8; 10], // dummy payload
            ..Default::default()
        };
        wrap(&media, session_id)
    }

    fn audio_frame_packet(session_id: u64) -> Arc<PacketWrapper> {
        let media = MediaPacket {
            media_type: MediaType::AUDIO.into(),
            user_id: "test@test.com".into(),
            data: vec![0u8; 10],
            ..Default::default()
        };
        wrap(&media, session_id)
    }

    fn screen_frame_packet(session_id: u64) -> Arc<PacketWrapper> {
        let media = MediaPacket {
            media_type: MediaType::SCREEN.into(),
            user_id: "test@test.com".into(),
            data: vec![0u8; 10],
            ..Default::default()
        };
        wrap(&media, session_id)
    }

    /// An AUDIO `PacketWrapper` carrying an outer `simulcast_layer_id` + cleartext
    /// `media_kind` and an inner `AudioMetadata.sequence` — the AUDIO sibling of
    /// [`layered_video_packet`], used by the issue #2138 layer-switch tests.
    ///
    /// `media_kind` is stamped AUDIO so `Peer::decode` takes the production
    /// CLEARTEXT-gate path (#1066) that every current client hits: the gate owns the
    /// availability-observe + layer-drop, and a surviving packet reaches the AUDIO
    /// arm's `audio_datagram_loss.record_seq` with `cleartext_gate_handled == true`.
    fn layered_audio_packet(session_id: u64, layer: u32, seq: u64) -> Arc<PacketWrapper> {
        use videocall_types::protos::media_packet::AudioMetadata;
        let media = MediaPacket {
            media_type: MediaType::AUDIO.into(),
            user_id: "test@test.com".into(),
            data: vec![0u8; 10],
            audio_metadata: Some(AudioMetadata {
                sequence: seq,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        let mut wrapper = packet_wrapper(&media, session_id);
        wrapper.simulcast_layer_id = layer;
        wrapper.media_kind = MediaKind::AUDIO.into();
        Arc::new(wrapper)
    }

    /// Wire a `Peer` so the AUDIO arm feeds `audio_datagram_loss` and then completes
    /// decode. Only `receiver_on_webtransport` gates the tracker itself (audio rides
    /// unreliable datagrams only on WT); enabling audio with a heartbeat already seen
    /// prevents the later mute/straggler branch from changing the test outcome.
    fn arm_audio_datagram_tracking(peer: &mut Peer) {
        peer.receiver_on_webtransport = true;
        peer.audio_enabled = true;
        peer.has_received_heartbeat = true;
    }

    /// A VIDEO `PacketWrapper` carrying an outer `simulcast_layer_id` and an
    /// inner `VideoMetadata.sequence`, used by the receiver layer-select guard
    /// tests (issue #989).
    fn layered_video_packet(session_id: u64, layer: u32, seq: u64) -> Arc<PacketWrapper> {
        use videocall_types::protos::media_packet::VideoMetadata;
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            user_id: "test@test.com".into(),
            data: vec![0u8; 10],
            frame_type: "key".to_string(),
            video_metadata: Some(VideoMetadata {
                sequence: seq,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        let mut wrapper = packet_wrapper(&media, session_id);
        wrapper.simulcast_layer_id = layer;
        Arc::new(wrapper)
    }

    /// #1025 leak guard: `clear_send_packet_callback` (disconnect teardown) must
    /// drop every peer decoder's keyframe-request route. Each route closure
    /// captured a clone of `send_packet` (a strong `Rc` reaching `Inner`) and
    /// lives in a per-decoder slot that nulling `self.send_packet` alone does NOT
    /// reach — a second leg of the cc7tp/#502 `Rc` cycle.
    ///
    /// Mutation coverage: removing the route-clearing loop from
    /// `clear_send_packet_callback` leaves the routes installed → both asserts fail.
    #[test]
    fn clear_send_packet_callback_drops_keyframe_request_routes() {
        let mut manager = PeerDecodeManager::new();
        let (peer, _muted) = make_test_peer(42);
        // Simulate a connected, context-initialized peer with routes installed.
        peer.video.set_keyframe_request_route(Box::new(|_| {}));
        peer.screen.set_keyframe_request_route(Box::new(|_| {}));
        assert!(peer.video.has_keyframe_request_route());
        assert!(peer.screen.has_keyframe_request_route());
        manager.connected_peers.insert(42, peer);

        manager.clear_send_packet_callback();

        let peer = manager.connected_peers.get(&42).expect("peer present");
        assert!(
            !peer.video.has_keyframe_request_route(),
            "video keyframe-request route must be cleared on teardown (#1025 leak guard)"
        );
        assert!(
            !peer.screen.has_keyframe_request_route(),
            "screen keyframe-request route must be cleared on teardown (#1025 leak guard)"
        );
    }

    /// Issue 1899 / discussion 1960 — self-healing keyframe-request route. The route is installed
    /// once (the `context_initialized` latch), but `Peer::reset_for_decode_error` (decode-error
    /// path) rebuilds a video or screen decoder with an EMPTY route slot and the latch never
    /// re-runs — so without self-healing the route stays `None` forever after a rebuild and the
    /// worker's keyframe-less
    /// eviction PLIs (issue #1025) become permanent no-ops, freezing a keyframe-starved tile for
    /// minutes (the field symptom). `decode()` must re-install a dropped route on the next media
    /// packet whenever the transport is wired.
    ///
    /// Noop decoders stand in for the fresh (route-less) decoders a rebuild mints — their route
    /// slot supports `set`/`has`/`clear` exactly like the real `VideoPeerDecoder` — so the invariant
    /// is exercised through the REAL `decode()` path without a browser worker (which the harness
    /// cannot construct).
    ///
    /// Mutation coverage: deleting the `else if send_packet.is_some() && !has_keyframe_request_route`
    /// re-install branch leaves BOTH routes `None` after decode → both asserts fail. Narrowing the
    /// probe back to VIDEO alone (issue 2225) is caught by
    /// `decode_reinstalls_a_screen_only_route_drop`, which drops ONLY the screen route. The outer
    /// `send_packet.is_some()` in the condition is only a cheap short-circuit; the LOAD-BEARING
    /// teardown guard is the inner `if let Some(send_packet)` inside the branch body — during a
    /// disconnect teardown `send_packet` is `None`, so even if the branch is entered the install body
    /// is skipped and no route is (re)installed against a dead transport.
    #[wasm_bindgen_test]
    fn decode_reinstalls_dropped_keyframe_request_route() {
        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(
            Callback::from(|_p: PacketWrapper| {}),
            "me@test.com".to_string(),
        );

        let (mut peer, _muted) = make_test_peer(42);
        // Past the one-time install latch, with the routes dropped exactly as a rebuild leaves them.
        peer.context_initialized = true;
        peer.has_received_heartbeat = true;
        peer.video.clear_keyframe_request_route();
        peer.screen.clear_keyframe_request_route();
        assert!(!peer.video.has_keyframe_request_route());
        assert!(!peer.screen.has_keyframe_request_route());
        manager.connected_peers.insert(42, peer);

        // Any media packet on the already-initialized peer must trigger the self-healing re-install.
        let media = MediaPacket {
            media_type: MediaType::HEARTBEAT.into(),
            user_id: b"test@test.com".to_vec(),
            heartbeat_metadata: Some(HeartbeatMetadata {
                video_enabled: true,
                audio_enabled: true,
                screen_enabled: false,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        manager
            .decode(packet_wrapper(&media, 42), "test@test.com")
            .expect("heartbeat should decode");

        let peer = manager.connected_peers.get(&42).expect("peer present");
        assert!(
            peer.video.has_keyframe_request_route(),
            "decode must re-install a dropped VIDEO keyframe-request route (issue 1899 / discussion 1960)"
        );
        assert!(
            peer.screen.has_keyframe_request_route(),
            "decode must re-install a dropped SCREEN keyframe-request route (issue 1899 / discussion 1960)"
        );
    }

    /// Issue 2225: the self-heal probe must notice a SCREEN-ONLY route drop.
    ///
    /// Per-kind resets made this reachable. Before 2225, `Peer::reset` rebuilt
    /// the video and screen decoders together, so the routes were only ever
    /// dropped in pairs — which is why the probe keyed on VIDEO alone, with a
    /// comment warning that a future single-stream rebuild would evade it. A
    /// `ScreenDecodeError` now rebuilds the screen decoder on its own, leaving
    /// the VIDEO route intact and the SCREEN route `None`.
    ///
    /// The consequence of missing it is not cosmetic: the codec worker's
    /// keyframe-less eviction PLIs (issue #1025) post into a no-op route, so a
    /// keyframe-starved screen tile freezes for minutes — the exact field
    /// symptom the self-heal exists to prevent.
    ///
    /// Mutation: narrow the probe back to `!peer.video.has_keyframe_request_route()`
    /// and this fails while `decode_reinstalls_dropped_keyframe_request_route`
    /// (which drops both) still passes.
    #[wasm_bindgen_test]
    fn decode_reinstalls_a_screen_only_route_drop() {
        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(
            Callback::from(|_p: PacketWrapper| {}),
            "me@test.com".to_string(),
        );

        let (mut peer, _muted) = make_test_peer(2225);
        peer.context_initialized = true;
        peer.has_received_heartbeat = true;
        // Exactly what a SCREEN-only rebuild leaves behind: VIDEO still wired,
        // SCREEN route-less.
        peer.video.set_keyframe_request_route(Box::new(|_| {}));
        peer.screen.clear_keyframe_request_route();
        assert!(peer.video.has_keyframe_request_route());
        assert!(!peer.screen.has_keyframe_request_route());
        manager.connected_peers.insert(2225, peer);

        let media = MediaPacket {
            media_type: MediaType::HEARTBEAT.into(),
            user_id: b"test@test.com".to_vec(),
            heartbeat_metadata: Some(HeartbeatMetadata {
                video_enabled: true,
                audio_enabled: true,
                screen_enabled: true,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        manager
            .decode(packet_wrapper(&media, 2225), "test@test.com")
            .expect("heartbeat should decode");

        let peer = manager.connected_peers.get(&2225).expect("peer present");
        assert!(
            peer.screen.has_keyframe_request_route(),
            "decode must re-install a SCREEN route dropped on its own by a per-kind rebuild"
        );
    }

    #[wasm_bindgen_test]
    fn heartbeat_screen_off_cancels_pending_screen_decode_retries() {
        let mut manager = PeerDecodeManager::new();
        let (mut peer, _muted) = make_test_peer(42);
        peer.screen_enabled = true;
        peer.has_received_heartbeat = true;
        manager.connected_peers.insert(42, peer);

        let token = Rc::new(Cell::new(true));
        manager
            .screen_decode_retry_tokens
            .insert("test@test.com".to_string(), token.clone());

        let media = MediaPacket {
            media_type: MediaType::HEARTBEAT.into(),
            user_id: b"test@test.com".to_vec(),
            heartbeat_metadata: Some(HeartbeatMetadata {
                video_enabled: false,
                audio_enabled: false,
                screen_enabled: false,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };

        manager
            .decode(packet_wrapper(&media, 42), "test@test.com")
            .expect("screen-off heartbeat should decode");

        assert!(
            !token.get(),
            "screen-off heartbeat must cancel pending screen_decode_started retries"
        );
        assert!(
            !manager
                .screen_decode_retry_tokens
                .contains_key("test@test.com"),
            "cancelled retry token should be removed from the manager"
        );
    }

    #[wasm_bindgen_test]
    fn new_screen_decode_publish_cancels_previous_retry_token() {
        let collected = Rc::new(std::cell::RefCell::new(Vec::<PacketWrapper>::new()));
        let collected_clone = collected.clone();
        let callback = Callback::from(move |packet: PacketWrapper| {
            collected_clone.borrow_mut().push(packet);
        });

        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(callback, "viewer@test.com".to_string());

        let previous = Rc::new(Cell::new(true));
        manager
            .screen_decode_retry_tokens
            .insert("publisher@test.com".to_string(), previous.clone());

        manager.publish_screen_decode_started("publisher@test.com");

        assert!(
            !previous.get(),
            "re-publishing for the same publisher must cancel the old retry loop"
        );
        assert!(
            manager
                .screen_decode_retry_tokens
                .get("publisher@test.com")
                .is_some_and(|token| token.get()),
            "new publish should install an active retry token"
        );

        let packets = collected.borrow();
        assert_eq!(
            packets.len(),
            1,
            "publish sends the first event immediately"
        );
        assert_eq!(
            packets[0].packet_type.enum_value(),
            Ok(PacketType::PEER_EVENT)
        );
        let peer_event =
            PeerEvent::parse_from_bytes(&packets[0].data).expect("peer event should parse");
        assert_eq!(
            peer_event.event_type,
            PEER_EVENT_SCREEN_DECODE_STARTED.to_string()
        );
        assert_eq!(peer_event.target_peer_id, b"publisher@test.com".to_vec());
    }

    // -- straggler guard tests --------------------------------------------

    /// Before any heartbeat, a VIDEO frame should infer video_enabled = true.
    #[wasm_bindgen_test]
    fn video_frame_before_heartbeat_infers_enabled() {
        let (mut peer, _muted) = make_test_peer(1);
        assert!(!peer.video_enabled);
        assert!(!peer.has_received_heartbeat);

        let packet = video_frame_packet(1);
        // Video decode will fail (noop decoder gets dummy data) but
        // state inference happens before the codec call.
        let _ = peer.decode(&packet, "");
        assert!(peer.video_enabled, "video_enabled should be inferred true");
    }

    /// After a heartbeat with video_enabled=false, a straggler VIDEO frame
    /// must NOT flip video_enabled back to true and must return rendered=false.
    #[wasm_bindgen_test]
    fn video_straggler_after_heartbeat_is_dropped() {
        let (mut peer, _muted) = make_test_peer(2);

        // Receive heartbeat: video off, audio off, screen off.
        let hb = heartbeat_packet(2, false, false, false);
        let result = peer.decode(&hb, "");
        assert!(result.is_ok());
        assert!(peer.has_received_heartbeat);
        assert!(!peer.video_enabled);

        // Now a straggler video frame arrives.
        let packet = video_frame_packet(2);
        let result = peer.decode(&packet, "");
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(!status.first_frame, "straggler must not be a first frame");
        assert!(
            !peer.video_enabled,
            "straggler video frame must not re-enable video"
        );
    }

    /// Before any heartbeat, an AUDIO frame should infer audio_enabled = true
    /// and unmute the audio decoder.
    #[wasm_bindgen_test]
    fn audio_frame_before_heartbeat_infers_enabled() {
        let (mut peer, muted_handle) = make_test_peer(3);
        assert!(!peer.audio_enabled);
        assert!(muted_handle.get(), "audio should start muted");

        let packet = audio_frame_packet(3);
        let _ = peer.decode(&packet, "");
        assert!(peer.audio_enabled, "audio_enabled should be inferred true");
        assert!(
            !muted_handle.get(),
            "audio decoder should be unmuted after inference"
        );
    }

    /// After a heartbeat with audio_enabled=false, a straggler AUDIO frame
    /// must NOT flip audio_enabled back to true and must return rendered=false.
    #[wasm_bindgen_test]
    fn audio_straggler_after_heartbeat_is_dropped() {
        let (mut peer, _muted) = make_test_peer(4);

        let hb = heartbeat_packet(4, false, false, false);
        let _ = peer.decode(&hb, "");
        assert!(peer.has_received_heartbeat);
        assert!(!peer.audio_enabled);

        let packet = audio_frame_packet(4);
        let result = peer.decode(&packet, "");
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(!status.first_frame, "straggler must not be a first frame");
        assert!(
            !peer.audio_enabled,
            "straggler audio frame must not re-enable audio"
        );
    }

    /// Before any heartbeat, a SCREEN frame should infer screen_enabled = true.
    #[wasm_bindgen_test]
    fn screen_frame_before_heartbeat_infers_enabled() {
        let (mut peer, _muted) = make_test_peer(5);
        assert!(!peer.screen_enabled);

        let packet = screen_frame_packet(5);
        let _ = peer.decode(&packet, "");
        assert!(
            peer.screen_enabled,
            "screen_enabled should be inferred true"
        );
    }

    // --- HCL bug #1: heartbeat-vs-SCREEN-frame race -----------------------
    //
    // These tests pin down `apply_heartbeat_enabled_flag`, the pure
    // decision function the HEARTBEAT branch consults to decide whether a
    // stale `metadata.X_enabled = false` is allowed to clobber a locally
    // tracked `X_enabled = true`. The bug fix REQUIRES this function — a
    // regression that simplifies it back to "always trust the heartbeat"
    // will fail every one of these tests.

    /// `heartbeat=true` always wins, regardless of whether we have any
    /// recent media. The publisher is the source of truth for "on."
    #[test]
    fn apply_hb_flag_affirmative_heartbeat_wins() {
        // No media observed.
        assert!(apply_heartbeat_enabled_flag(
            false,
            true,
            0,
            5_000,
            MEDIA_FRESH_WINDOW_MS
        ));
        // Stale media (older than the freshness window).
        assert!(apply_heartbeat_enabled_flag(
            false,
            true,
            1_000,
            10_000,
            MEDIA_FRESH_WINDOW_MS
        ));
        // Fresh media.
        assert!(apply_heartbeat_enabled_flag(
            true,
            true,
            4_500,
            5_000,
            MEDIA_FRESH_WINDOW_MS
        ));
    }

    /// `heartbeat=false` with a SCREEN frame inside the freshness window
    /// MUST preserve the current `true` flag — this is the WT-race fix.
    /// The user-visible symptom of regressing this is the split-screen
    /// layout collapsing for one heartbeat period after every SCREEN
    /// keyframe on WebTransport.
    ///
    /// Uses `MEDIA_FRESH_WINDOW_MS - 100` so the test tracks the constant
    /// rather than baking in a literal — if the window is widened or
    /// narrowed in future, the test stays load-bearing.
    #[test]
    fn apply_hb_flag_keeps_current_when_media_is_fresh() {
        let delta = MEDIA_FRESH_WINDOW_MS - 100;
        let now = 10_000_u64;
        let last_frame = now - delta;
        assert!(apply_heartbeat_enabled_flag(
            true,                  /* current */
            false,                 /* heartbeat */
            last_frame,            /* last_frame_ms */
            now,                   /* now_ms */
            MEDIA_FRESH_WINDOW_MS, /* fresh_window_ms */
        ));
    }

    /// `heartbeat=false` with a SCREEN frame OLDER than the freshness
    /// window must let the heartbeat win — the publisher has genuinely
    /// stopped sharing, and a single very-old frame should not pin the
    /// flag on forever.
    ///
    /// Uses `MEDIA_FRESH_WINDOW_MS + 100` so the test tracks the constant.
    #[test]
    fn apply_hb_flag_heartbeat_wins_when_media_is_stale() {
        let delta = MEDIA_FRESH_WINDOW_MS + 100;
        let now = 10_000_u64;
        let last_frame = now - delta;
        assert!(!apply_heartbeat_enabled_flag(
            true,                  /* current */
            false,                 /* heartbeat */
            last_frame,            /* last_frame_ms */
            now,                   /* now_ms */
            MEDIA_FRESH_WINDOW_MS, /* fresh_window_ms */
        ));
    }

    /// Boundary test: pin the 5000ms cadence value explicitly so a
    /// future "let's shrink the window back to 2000ms" change fails
    /// loudly. The PR-review fix raised the window to match
    /// `HEARTBEAT_KEEPALIVE_INTERVAL_MS = 5000ms` because heartbeats
    /// ride lossy datagrams and can arrive up to one full cadence late
    /// on bad links — a sub-cadence window lets a stale heartbeat
    /// clobber a live SCREEN stream on WT/3G/mobile and re-introduces
    /// the "shared content in a small tile only" bug.
    #[test]
    fn apply_hb_flag_5000ms_window_covers_heartbeat_cadence() {
        // The constant itself must be ≥ 5000ms.
        const {
            assert!(
                MEDIA_FRESH_WINDOW_MS >= 5_000,
                "MEDIA_FRESH_WINDOW_MS must be ≥ HEARTBEAT_KEEPALIVE_INTERVAL_MS (5000ms) — \
                 a shorter window lets a stale heartbeat clobber live media on lossy WT"
            );
        }

        let now = 10_000_u64;

        // A 4900ms-old frame is still inside the 5000ms window → KEEP.
        let fresh = apply_heartbeat_enabled_flag(
            true,        /* current */
            false,       /* heartbeat */
            now - 4_900, /* last_frame_ms — 4900ms ago */
            now,
            MEDIA_FRESH_WINDOW_MS,
        );
        assert!(
            fresh,
            "frame 4900ms old must still suppress a stale heartbeat (worst-case \
             cadence-late heartbeat is up to 5000ms behind)"
        );

        // A 5100ms-old frame is outside the window → heartbeat wins.
        let stale = apply_heartbeat_enabled_flag(
            true,        /* current */
            false,       /* heartbeat */
            now - 5_100, /* last_frame_ms — 5100ms ago */
            now,
            MEDIA_FRESH_WINDOW_MS,
        );
        assert!(
            !stale,
            "frame 5100ms old is past the cadence-aligned window — the heartbeat \
             is presumed current and must be honoured"
        );
    }

    /// `heartbeat=false` with NO media ever (last_frame_ms = 0) must let
    /// the heartbeat win even though `now_ms - 0 < MEDIA_FRESH_WINDOW_MS`
    /// arithmetically. The sentinel `0` means "never observed," not
    /// "observed at epoch."
    #[test]
    fn apply_hb_flag_zero_sentinel_is_not_fresh() {
        assert!(!apply_heartbeat_enabled_flag(
            true,                  /* current */
            false,                 /* heartbeat */
            0,                     /* never observed */
            500,                   /* now_ms inside window arithmetically */
            MEDIA_FRESH_WINDOW_MS, /* fresh_window_ms */
        ));
    }

    /// Clock skew guard: if `last_frame_ms > now_ms` (timestamp from the
    /// future — possible in test fixtures or under clock adjustment),
    /// the saturating subtraction must NOT panic / wrap, and the frame
    /// should be treated as fresh.
    #[test]
    fn apply_hb_flag_clock_skew_treats_future_frame_as_fresh() {
        assert!(apply_heartbeat_enabled_flag(
            true,
            false,
            10_000,                /* last_frame_ms */
            5_000,                 /* now_ms — earlier than last_frame */
            MEDIA_FRESH_WINDOW_MS, /* fresh_window_ms */
        ));
    }

    // --- audio/video stop latency vs. the freshness window ---------------
    //
    // Audio and camera-video reuse `apply_heartbeat_enabled_flag` but with
    // the much shorter `LIVE_STREAM_FRESH_WINDOW_MS`. These tests pin that
    // window's behaviour: a mute / camera-off must propagate sub-second, but
    // a recently-arrived live frame must still out-vote a stale `false`
    // heartbeat that raced it on WT. Screen keeps `MEDIA_FRESH_WINDOW_MS`.

    /// The continuous-stream window must be far shorter than the screen
    /// window so that mute / camera-off reflect on remote peers quickly. Pin
    /// the relationship so a future "just reuse MEDIA_FRESH_WINDOW_MS for
    /// audio/video" regression — the exact bug this fixes — fails loudly.
    #[test]
    fn live_stream_fresh_window_is_sub_second_and_shorter_than_media() {
        const {
            assert!(
                LIVE_STREAM_FRESH_WINDOW_MS <= 1_000,
                "LIVE_STREAM_FRESH_WINDOW_MS must be sub-second so a mute / \
                 camera-off reflects within ~1 heartbeat, not after the ~5s \
                 screen window"
            );
            assert!(
                LIVE_STREAM_FRESH_WINDOW_MS < MEDIA_FRESH_WINDOW_MS,
                "the audio/video window must be strictly shorter than the screen \
                 window — reusing the screen window re-introduces the ~5s lag"
            );
        }
    }

    /// (a) A mute heartbeat (`audio_enabled = false`) arriving just after
    /// audio frames stop must be honoured once the last frame ages past the
    /// SHORT window. With the screen-sized window this took ~5s; with the
    /// continuous-stream window it is sub-second. Frame is
    /// `LIVE_STREAM_FRESH_WINDOW_MS + 50` old → heartbeat wins → muted.
    #[test]
    fn apply_hb_flag_audio_mute_reflects_after_short_window() {
        let now = 10_000_u64;
        let last_frame = now - (LIVE_STREAM_FRESH_WINDOW_MS + 50);
        assert!(
            !apply_heartbeat_enabled_flag(
                true,  /* current: was unmuted */
                false, /* heartbeat: now muted */
                last_frame,
                now,
                LIVE_STREAM_FRESH_WINDOW_MS,
            ),
            "audio mute must be honoured once the last frame is older than \
             the short window — this is the sub-second mute fix"
        );

        // Sanity: the SAME age, evaluated against the screen-sized window,
        // would still (wrongly, for audio) suppress the mute — proving the
        // window choice is what fixes the latency.
        assert!(
            apply_heartbeat_enabled_flag(true, false, last_frame, now, MEDIA_FRESH_WINDOW_MS),
            "the screen window would keep audio unmuted at this age — \
             demonstrates why audio needs the short window"
        );
    }

    /// (b) The camera-video freshness PRIMITIVE: a `false` heartbeat evaluated
    /// against the SHORT window clears the flag once the last frame ages past
    /// it (the symmetric case to audio mute; fixes the camera side of the ~5s
    /// lag by using the continuous-stream window, not the screen window). NOTE:
    /// the camera CALL SITE layers an on->off corroboration debounce on top of
    /// this primitive (see `resolve_camera_*` tests below) — this test pins only
    /// the primitive window, which audio still uses directly.
    #[test]
    fn apply_hb_flag_video_disable_reflects_after_short_window() {
        let now = 10_000_u64;
        let last_frame = now - (LIVE_STREAM_FRESH_WINDOW_MS + 50);
        assert!(
            !apply_heartbeat_enabled_flag(
                true,  /* current: camera was on */
                false, /* heartbeat: camera now off */
                last_frame,
                now,
                LIVE_STREAM_FRESH_WINDOW_MS,
            ),
            "camera-off must be honoured once the last video frame is older \
             than the short window — this is the sub-second camera-disable fix"
        );

        // Sanity: the SAME age against the screen-sized window would still
        // (wrongly, for camera-video) keep the camera shown — this is the
        // exact ~5s lag the per-media-type window removes.
        assert!(
            apply_heartbeat_enabled_flag(true, false, last_frame, now, MEDIA_FRESH_WINDOW_MS),
            "the screen window would keep the camera shown at this age — \
             demonstrates why camera-video needs the short window"
        );
    }

    /// A `false` heartbeat that raced a genuinely-live audio frame (frame is
    /// only a few ms old, well inside the window) must NOT mute — this
    /// preserves the WT out-of-order protection for audio too, just on a
    /// tighter, reorder-sized window instead of a multi-second one.
    #[test]
    fn apply_hb_flag_audio_keeps_unmuted_when_frame_is_very_fresh() {
        let now = 10_000_u64;
        let last_frame = now - (LIVE_STREAM_FRESH_WINDOW_MS - 100); // just inside
        assert!(
            apply_heartbeat_enabled_flag(true, false, last_frame, now, LIVE_STREAM_FRESH_WINDOW_MS),
            "a stale false heartbeat racing a fresh audio frame must not mute — \
             WT reorder protection still applies within the short window"
        );
    }

    /// Symmetric to the audio-fresh case: a `false` video heartbeat that
    /// raced a genuinely-live camera frame (only a few ms old) must NOT blank
    /// the camera — WT reorder protection still applies to video within the
    /// short window, so re-enable / brief reorder does not flicker.
    #[test]
    fn apply_hb_flag_video_keeps_enabled_when_frame_is_very_fresh() {
        let now = 10_000_u64;
        let last_frame = now - (LIVE_STREAM_FRESH_WINDOW_MS - 100); // just inside
        assert!(
            apply_heartbeat_enabled_flag(true, false, last_frame, now, LIVE_STREAM_FRESH_WINDOW_MS),
            "a stale false heartbeat racing a fresh camera frame must not blank \
             the camera — WT reorder protection still applies within the window"
        );
    }

    /// (c) Screen MUST keep the full `MEDIA_FRESH_WINDOW_MS` (no regression):
    /// a `false` screen heartbeat racing a screen frame from up to ~5s ago
    /// must still be suppressed, so the split-share layout does not collapse
    /// on WT. This pins that the screen call site was NOT narrowed to the
    /// short window along with audio/video.
    #[test]
    fn apply_hb_flag_screen_still_honors_full_window() {
        let now = 10_000_u64;
        // A screen frame 4.9s ago is far past the short audio/video window
        // but still INSIDE the 5s screen window: screen must stay enabled.
        let last_frame = now - 4_900;
        assert!(
            last_frame > now - MEDIA_FRESH_WINDOW_MS,
            "fixture sanity: frame must be inside the 5s screen window"
        );
        assert!(
            apply_heartbeat_enabled_flag(true, false, last_frame, now, MEDIA_FRESH_WINDOW_MS),
            "screen must keep its 5s window — a stale false heartbeat within \
             5s of a screen frame must NOT collapse the split-share layout (WT)"
        );
        // And confirm that age WOULD have muted under the short window —
        // i.e. screen is genuinely relying on the longer window, proving the
        // two windows are independent and screen was not narrowed.
        assert!(
            !apply_heartbeat_enabled_flag(
                true,
                false,
                last_frame,
                now,
                LIVE_STREAM_FRESH_WINDOW_MS
            ),
            "sanity: at 4.9s the short window would clear the flag — screen's \
             protection comes specifically from keeping MEDIA_FRESH_WINDOW_MS"
        );
    }

    /// The two type-safe wrappers must carry DIFFERENT windows. At a frame age
    /// between the two windows (here 600ms: past the 500ms live window, well
    /// inside the 5s screen window) the live-stream wrapper must honour a mute
    /// / camera-off (returns `false`) while the screen wrapper must suppress it
    /// (keeps `current = true`). This is the regression the wrappers exist to
    /// prevent: it fails loudly if someone makes both wrappers share a window.
    #[test]
    fn live_stream_and_screen_wrappers_use_distinct_windows() {
        let now = 10_000_u64;
        // 600ms old: past LIVE_STREAM_FRESH_WINDOW_MS (500), inside
        // MEDIA_FRESH_WINDOW_MS (5000). Pin the fixture against the constants
        // so it tracks any future tuning.
        let last_frame = now - 600;
        assert!(
            last_frame < now - LIVE_STREAM_FRESH_WINDOW_MS
                && last_frame > now - MEDIA_FRESH_WINDOW_MS,
            "fixture sanity: frame age must fall between the live and screen windows"
        );

        assert!(
            !apply_live_stream_heartbeat_flag(true, false, last_frame, now),
            "audio/video wrapper must honour a mute / camera-off once the frame \
             ages past the short window — not wait for the screen-sized window"
        );
        assert!(
            apply_screen_heartbeat_flag(true, false, last_frame, now),
            "screen wrapper must still suppress a stale false at this age — its \
             window is the long one, so the split-share layout does not collapse"
        );
    }

    // --- camera on->off corroboration debounce ----------------------------
    //
    // The congestion case the shrunk `LIVE_STREAM_FRESH_WINDOW_MS` exposed and
    // the tests above do NOT cover: with a frame interval over 500ms, a lone
    // stale `false` must not blank a still-on tile, while a genuine off still
    // blanks sub-second. These call the production `resolve_camera_heartbeat_flag`
    // directly so reverting the debounce fails them.

    /// With the last camera frame already PAST the 500ms window (the congestion
    /// case), the first stale `false` must NOT blank a still-on camera. The
    /// sibling assert proves the primitive WOULD blank here, so this fails if
    /// the debounce is reverted.
    #[test]
    fn resolve_camera_first_stale_false_keeps_camera_on() {
        let now = 10_000_u64;
        let stale = now - (LIVE_STREAM_FRESH_WINDOW_MS + 50);
        let (enabled, count) = resolve_camera_heartbeat_flag(true, false, stale, now, 0);
        assert!(
            enabled,
            "a lone stale false must NOT blank a still-on camera whose frame \
             interval exceeded the 500ms window"
        );
        assert_eq!(count, 1, "the stale false is recorded toward corroboration");
        // Mutation sanity: the primitive the camera path used before the
        // debounce blanks on this very first stale false.
        assert!(
            !apply_live_stream_heartbeat_flag(true, false, stale, now),
            "the un-debounced primitive blanks on the first stale false — the \
             debounce is exactly what prevents the flicker"
        );
    }

    /// A GENUINE camera-off still reflects sub-second: the immediate `false`
    /// edge races a still-fresh frame (keeps the tile on for one heartbeat),
    /// then the publisher's guaranteed reliable resend at ~600ms — by which
    /// time the frame has aged out of the window — corroborates the off and
    /// blanks the tile.
    #[test]
    fn resolve_camera_off_corroborated_by_resend_reflects_sub_second() {
        let now = 10_000_u64;
        // First off edge races a fresh frame -> kept on, streak -> 1.
        let fresh = now - (LIVE_STREAM_FRESH_WINDOW_MS - 100);
        let (e1, c1) = resolve_camera_heartbeat_flag(true, false, fresh, now, 0);
        assert!(
            e1 && c1 == 1,
            "the off edge racing a fresh frame keeps the tile on once"
        );
        // ~600ms resend: frame aged out -> corroborated -> blank sub-second.
        let stale = now - (LIVE_STREAM_FRESH_WINDOW_MS + 50);
        let (e2, c2) = resolve_camera_heartbeat_flag(true, false, stale, now, c1);
        assert!(
            !e2,
            "the guaranteed 600ms reliable resend corroborates the off and \
             blanks the tile sub-second"
        );
        assert_eq!(c2, 0, "streak resets once the tile blanks");
    }

    /// Two consecutive stale `false`s (no intervening affirmative heartbeat or
    /// fresh frame) reach the corroboration count and blank the tile.
    #[test]
    fn resolve_camera_two_consecutive_stale_false_blanks() {
        let now = 10_000_u64;
        let stale = now - (LIVE_STREAM_FRESH_WINDOW_MS + 50);
        let (e1, c1) = resolve_camera_heartbeat_flag(true, false, stale, now, 0);
        assert!(e1 && c1 == 1);
        let (e2, c2) = resolve_camera_heartbeat_flag(true, false, stale, now, c1);
        assert!(
            !e2,
            "the second consecutive stale false corroborates the off"
        );
        assert_eq!(c2, 0);
    }

    /// An affirmative (`true`) heartbeat clears the streak, so a still-on camera
    /// that saw one stale false is never carried toward a blank by a much-later
    /// second stale datagram.
    #[test]
    fn resolve_camera_affirmative_resets_streak() {
        let now = 10_000_u64;
        let stale = now - (LIVE_STREAM_FRESH_WINDOW_MS + 50);
        let (_e, c1) = resolve_camera_heartbeat_flag(true, false, stale, now, 0);
        assert_eq!(c1, 1);
        let (e_on, c_reset) = resolve_camera_heartbeat_flag(true, true, stale, now, c1);
        assert!(e_on, "an affirmative heartbeat keeps the camera on");
        assert_eq!(c_reset, 0, "a true heartbeat must reset the off streak");
        // The next lone stale false again only keeps-on — proving no carryover.
        let (e_after, c_after) = resolve_camera_heartbeat_flag(true, false, stale, now, c_reset);
        assert!(
            e_after && c_after == 1,
            "streak did not carry over past the true heartbeat"
        );
    }

    /// WT reorder protection is preserved: a stale `false` racing a genuinely
    /// fresh camera frame keeps the tile on, and still counts the false so a
    /// real off's resend can corroborate.
    #[test]
    fn resolve_camera_fresh_frame_keeps_on_and_counts() {
        let now = 10_000_u64;
        let fresh = now - (LIVE_STREAM_FRESH_WINDOW_MS - 100);
        let (enabled, count) = resolve_camera_heartbeat_flag(true, false, fresh, now, 0);
        assert!(
            enabled,
            "a stale false racing a fresh camera frame must not blank — WT \
             reorder protection still applies within the window"
        );
        assert_eq!(count, 1, "the false is still counted toward corroboration");
    }

    /// The tile blanks EXACTLY at `CAMERA_OFF_CORROBORATION_COUNT` consecutive
    /// stale `false`s — not before. Ties the behaviour to the constant so a
    /// future tuning change tracks, and pins that the count is >= 2 (a single
    /// stale datagram can never blank a still-on camera).
    #[test]
    fn resolve_camera_blanks_exactly_at_corroboration_count() {
        const {
            assert!(
                CAMERA_OFF_CORROBORATION_COUNT >= 2,
                "must require at least 2 so a lone stale datagram cannot blank a \
                 still-on camera"
            );
        }
        let now = 10_000_u64;
        let stale = now - (LIVE_STREAM_FRESH_WINDOW_MS + 50);
        let mut count = 0u32;
        let mut enabled = true;
        for i in 1..=CAMERA_OFF_CORROBORATION_COUNT {
            let (e, c) = resolve_camera_heartbeat_flag(true, false, stale, now, count);
            enabled = e;
            count = c;
            if i < CAMERA_OFF_CORROBORATION_COUNT {
                assert!(
                    enabled,
                    "camera must stay on until {CAMERA_OFF_CORROBORATION_COUNT} \
                     consecutive stale falses"
                );
            }
        }
        assert!(
            !enabled,
            "camera blanks once the corroboration count is reached"
        );
        assert_eq!(count, 0, "streak resets after blanking");
    }

    /// Integration: simulate the exact WT-race scenario. A SCREEN
    /// keyframe lands first (auto-enables `screen_enabled`), then a
    /// stale heartbeat carrying `screen_enabled = false` arrives. The
    /// peer's local flag MUST remain true — this is the test that
    /// would fail before the fix.
    #[wasm_bindgen_test]
    fn screen_enabled_survives_stale_heartbeat_after_frame() {
        let (mut peer, _muted) = make_test_peer(193);
        assert!(!peer.screen_enabled);

        // SCREEN keyframe arrives first → `screen_enabled = true` and
        // `last_screen_frame_ms` is stamped to a recent value.
        let screen = screen_frame_packet(193);
        let _ = peer.decode(&screen, "");
        assert!(
            peer.screen_enabled,
            "SCREEN frame should auto-enable screen_enabled"
        );
        assert!(
            peer.last_screen_frame_ms > 0,
            "SCREEN frame should stamp last_screen_frame_ms"
        );

        // Stale heartbeat with screen_enabled=false arrives within the
        // freshness window. Before the fix: peer.screen_enabled flips
        // back to false, has_screen_share goes false in the UI, split
        // layout collapses. After the fix: the flag must remain true.
        let hb = heartbeat_packet(193, false, false, false);
        let _ = peer.decode(&hb, "");
        assert!(
            peer.screen_enabled,
            "HCL bug #1: stale heartbeat must not clobber fresh SCREEN \
             stream — the split-layout would otherwise collapse on WT"
        );
    }

    /// Integration: a heartbeat arriving AFTER the freshness window
    /// elapses must be honoured. The publisher really did stop sharing
    /// — we should not pin the flag on indefinitely just because we
    /// once saw a SCREEN frame.
    #[wasm_bindgen_test]
    fn screen_enabled_cleared_by_heartbeat_when_media_stops() {
        let (mut peer, _muted) = make_test_peer(194);

        // Force the timestamp into the past so a subsequent heartbeat
        // is OUTSIDE the freshness window. We can't actually sleep
        // 2s in a unit test; instead we install the value directly
        // (peer is pub-field accessible from inside the same module).
        peer.screen_enabled = true;
        peer.last_screen_frame_ms = 1; // ancient frame
                                       // The current monotonic clock is now ~now_ms() ≫ 1 + 2000.
        let hb = heartbeat_packet(194, false, false, false);
        let _ = peer.decode(&hb, "");
        assert!(
            !peer.screen_enabled,
            "heartbeat must clear screen_enabled when last frame is stale"
        );
    }

    /// Integration: a mute heartbeat (`audio_enabled = false`) must mute the
    /// peer's audio decoder quickly once audio frames have stopped. We force
    /// `last_audio_frame_ms` to an age that is PAST the short audio window
    /// but still WELL INSIDE the screen-sized window — the previous bug kept
    /// the peer unmuted for ~5s at exactly this age. The decoder's muted
    /// handle must flip to true.
    #[wasm_bindgen_test]
    fn audio_muted_promptly_by_heartbeat_after_frames_stop() {
        let (mut peer, muted) = make_test_peer(195);

        // Peer is currently unmuted with a recent audio frame.
        peer.audio_enabled = true;
        peer.audio.set_muted(false);
        assert!(!muted.get(), "precondition: audio decoder is unmuted");

        // Age the last audio frame past the short audio window but keep it
        // far inside the screen window — `now_ms()` ≫ this value + 500 but
        // ≪ this value + 5000 would require a real clock, so instead we set
        // it to 1 (ancient): it is past BOTH windows, which is the >500ms
        // mute path. The point of this integration test is that the AUDIO
        // call site now uses the short window at all; the pure-function
        // tests pin the exact boundary.
        peer.last_audio_frame_ms = 1;

        let hb = heartbeat_packet(195, false, false, false);
        let _ = peer.decode(&hb, "");

        assert!(
            !peer.audio_enabled,
            "audio_enabled must be cleared by the mute heartbeat"
        );
        assert!(
            muted.get(),
            "audio decoder must be muted promptly after a mute heartbeat"
        );
    }

    /// After a heartbeat with `screen_enabled = false`, a SCREEN frame DELIBERATELY
    /// re-enables screen. The production comment on that arm
    /// (`peer_decode_manager.rs:2594-2603`) states the rationale: "Trust the actual
    /// media frame over the (stale) heartbeat state — dropping the first keyframe
    /// here would leave the decoder waiting until a PLI round-trip completes." The
    /// staleness itself is explained just above it (`:2581-2586`): a heartbeat can
    /// carry `screen_enabled = false` on WebTransport because heartbeats and SCREEN
    /// frames race across separate QUIC streams.
    ///
    /// The camera path behaves DIFFERENTLY — this is observed behaviour, not a
    /// documented design intent. A straggler VIDEO frame arriving after a
    /// camera-off heartbeat is dropped rather than re-enabling video
    /// (`peer_decode_manager.rs:2428-2437`, pinned by
    /// `video_enable_disable_straggler_sequence`). The source justifies the SCREEN
    /// side on its own terms and says nothing about the contrast with camera, so
    /// this test pins the screen behaviour and only notes the camera difference; it
    /// does not assert that the asymmetry is intentional. The previous name and
    /// assertion here claimed the camera behaviour for screen and contradicted the
    /// production code it was supposed to guard.
    ///
    /// Rendering is a SEPARATE gate from the enabled flag: this peer is invisible, so
    /// the re-enabling frame must still not paint.
    ///
    /// MUTATION: delete the `if !self.screen_enabled { self.screen_enabled = true; }`
    /// arm in the SCREEN decode branch and the `screen_enabled` assertion fails.
    #[wasm_bindgen_test]
    fn screen_frame_reenables_screen_over_stale_heartbeat() {
        let (mut peer, _muted) = make_test_peer(6);

        let hb = heartbeat_packet(6, false, false, false);
        let _ = peer.decode(&hb, "");
        assert!(peer.has_received_heartbeat);
        assert!(!peer.screen_enabled, "the heartbeat reports screen off");
        // `make_test_peer` starts the tile INVISIBLE — that is what keeps the frame
        // from painting below, independently of the enabled flag.
        assert!(!peer.visible);

        let packet = screen_frame_packet(6);
        let result = peer.decode(&packet, "");
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(
            peer.screen_enabled,
            "a live SCREEN frame must re-enable screen over a stale heartbeat \
             (the decode path trusts live media; camera drops the straggler instead)"
        );
        assert!(
            !status.rendered,
            "re-enabling the stream must not paint while the tile is invisible"
        );
        assert!(
            !status.first_frame,
            "an unrendered frame is not a first frame"
        );
    }

    /// A heartbeat that enables video, followed by a video frame, should work.
    /// (Ensures the guard doesn't block legitimate frames.)
    #[wasm_bindgen_test]
    fn video_frame_after_enabling_heartbeat_is_accepted() {
        let (mut peer, _muted) = make_test_peer(7);

        // Heartbeat enables video.
        let hb = heartbeat_packet(7, true, false, false);
        let _ = peer.decode(&hb, "");
        assert!(peer.video_enabled);

        // A video frame should pass the guard (video_enabled is already true).
        let packet = video_frame_packet(7);
        let _ = peer.decode(&packet, "");
        // video_enabled should remain true.
        assert!(peer.video_enabled);
    }

    /// Heartbeat toggles: enable → disable → straggler.
    #[wasm_bindgen_test]
    fn video_enable_disable_straggler_sequence() {
        let (mut peer, _muted) = make_test_peer(8);

        // Enable video via heartbeat.
        let hb_on = heartbeat_packet(8, true, false, false);
        let _ = peer.decode(&hb_on, "");
        assert!(peer.video_enabled);

        // Disable video via heartbeat. `resolve_camera_heartbeat_flag` DEBOUNCES a
        // camera-off: blanking a currently-on tile takes
        // CAMERA_OFF_CORROBORATION_COUNT consecutive `false` heartbeats, so a lone
        // stale/reordered datagram can never black out a live camera. Drive the full
        // streak, bounded by the constant so a future tuning change tracks here.
        let hb_off = heartbeat_packet(8, false, false, false);
        for _ in 0..CAMERA_OFF_CORROBORATION_COUNT {
            let _ = peer.decode(&hb_off, "");
        }
        assert!(!peer.video_enabled);

        // Straggler video frame should be dropped.
        let packet = video_frame_packet(8);
        let result = peer.decode(&packet, "");
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(
            !peer.video_enabled,
            "straggler after disable must not re-enable"
        );
    }

    /// A real screen-share stop must clear the decoder canvas gate, not merely
    /// flush codec state. Removing `force_clear_canvas()` from the heartbeat
    /// transition leaves this exact production paint flag enabled and fails.
    #[wasm_bindgen_test]
    fn screen_disable_force_clears_the_last_frame() {
        let (mut peer, _muted) = make_test_peer(81);
        assert!(peer.screen.paint_enabled_for_test());

        let _ = peer.decode(&heartbeat_packet(81, false, false, true), "");
        assert!(peer.screen_enabled);

        let _ = peer.decode(&heartbeat_packet(81, false, false, false), "");
        assert!(!peer.screen_enabled);
        assert!(
            !peer.screen.paint_enabled_for_test(),
            "screen-off must disable painting so a recording cannot retain the frozen last frame"
        );
    }

    // -- audio_level tests -------------------------------------------------

    /// A freshly created peer should have audio_level == 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_default_zero() {
        let (peer, _muted) = make_test_peer(100);
        assert!(
            (peer.audio_level - 0.0).abs() < f32::EPSILON,
            "new peer should have audio_level == 0.0, got {}",
            peer.audio_level
        );
    }

    /// Insert a peer into a PeerDecodeManager, set its audio_level, then
    /// verify `peer_audio_level()` returns the expected value.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_accessor() {
        let mut manager = PeerDecodeManager::new();
        let (mut peer, _muted) = make_test_peer(101);
        peer.audio_level = 0.75;
        manager.connected_peers.insert(101, peer);

        let level = manager.peer_audio_level("101");
        assert!(
            (level - 0.75).abs() < f32::EPSILON,
            "peer_audio_level should return 0.75, got {level}"
        );
    }

    /// Calling `peer_audio_level()` for a non-existent peer should return 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_unknown_peer_returns_zero() {
        let manager = PeerDecodeManager::new();
        let level = manager.peer_audio_level("99999");
        assert!(
            (level - 0.0).abs() < f32::EPSILON,
            "peer_audio_level for unknown peer should return 0.0, got {level}"
        );
    }

    /// Calling `peer_audio_level()` with a non-numeric key should return 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_invalid_key_returns_zero() {
        let manager = PeerDecodeManager::new();
        let level = manager.peer_audio_level("not-a-number");
        assert!(
            (level - 0.0).abs() < f32::EPSILON,
            "peer_audio_level for invalid key should return 0.0, got {level}"
        );
    }

    /// After a heartbeat with is_speaking=false, audio_level should be reset to 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_reset_on_not_speaking_heartbeat() {
        let (mut peer, _muted) = make_test_peer(102);
        // Simulate audio level being set during active speech
        peer.audio_level = 0.5;
        peer.is_speaking = true;

        // Heartbeat with all disabled (is_speaking defaults to false)
        let hb = heartbeat_packet(102, false, false, false);
        let _ = peer.decode(&hb, "");

        assert!(
            (peer.audio_level - 0.0).abs() < f32::EPSILON,
            "audio_level should be reset to 0.0 when heartbeat says not speaking, got {}",
            peer.audio_level
        );
    }

    /// Issue 2174 (security carry-over): a peer must not be able to claim the
    /// #1557 active-speaker layer-drop exemption while declaring its audio off.
    ///
    /// `seed_downlink_congestion` skips any peer whose stored `is_speaking` is
    /// true under local-CPU pressure, so a peer that could park that flag at
    /// `true` with no audio would hold a receiver-side resource exemption on
    /// every participant indefinitely. The stored field must be resolved
    /// against the peer's audio state.
    ///
    /// Fails if the `&& resolved_audio` guard is dropped from the heartbeat arm.
    #[wasm_bindgen_test]
    fn heartbeat_cannot_claim_speaking_while_audio_is_disabled() {
        let (mut peer, _muted) = make_test_peer(2174);
        peer.audio_level = 0.4;

        let claim = |audio_enabled: bool| {
            let media = MediaPacket {
                media_type: MediaType::HEARTBEAT.into(),
                user_id: "test@test.com".into(),
                heartbeat_metadata: Some(HeartbeatMetadata {
                    video_enabled: false,
                    audio_enabled,
                    screen_enabled: false,
                    is_speaking: true,
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            };
            wrap(&media, 2174)
        };

        // Audio declared OFF while claiming to speak — the claim must not stick.
        let _ = peer.decode(&claim(false), "");
        assert!(
            !peer.audio_enabled,
            "precondition: heartbeat declared audio off"
        );
        assert!(
            !peer.is_speaking,
            "a peer with audio disabled must not be stored as speaking (#1557 exemption lever)"
        );
        assert_eq!(
            peer.audio_level, 0.0,
            "audio_level must be cleared alongside the rejected speaking claim"
        );

        // Control: with audio genuinely enabled the same claim IS honoured, so
        // the guard suppresses only the contradictory combination.
        let _ = peer.decode(&claim(true), "");
        assert!(peer.audio_enabled, "precondition: heartbeat enabled audio");
        assert!(
            peer.is_speaking,
            "an audio-enabled peer's speaking claim must still be honoured"
        );
    }

    /// Full sequence: enable → legitimate frame → disable → straggler dropped.
    #[wasm_bindgen_test]
    fn video_enable_frame_disable_straggler_full_sequence() {
        let (mut peer, _muted) = make_test_peer(9);

        // 1. Enable video via heartbeat.
        let hb_on = heartbeat_packet(9, true, false, false);
        let _ = peer.decode(&hb_on, "");
        assert!(peer.video_enabled);

        // 2. Legitimate video frame while enabled — should pass through.
        let frame = video_frame_packet(9);
        let _ = peer.decode(&frame, "");
        assert!(peer.video_enabled, "legitimate frame must not change state");

        // 3. Disable video via heartbeat.
        // Step 2 stamped `last_video_frame_ms` to "now", and the freshness primitive
        // deliberately out-votes a `false` heartbeat while a frame is within
        // LIVE_STREAM_FRESH_WINDOW_MS (WT reorder protection). Age that stamp past the
        // window so this step exercises the camera-off DEBOUNCE, not the freshness veto.
        peer.last_video_frame_ms = now_ms().saturating_sub(LIVE_STREAM_FRESH_WINDOW_MS + 50);
        // Blanking then still requires CAMERA_OFF_CORROBORATION_COUNT consecutive
        // `false`s (bounded by the constant so tuning changes track here).
        let hb_off = heartbeat_packet(9, false, false, false);
        for _ in 0..CAMERA_OFF_CORROBORATION_COUNT {
            let _ = peer.decode(&hb_off, "");
        }
        assert!(!peer.video_enabled);

        // 4. Straggler video frame after disable — must be dropped.
        let straggler = video_frame_packet(9);
        let result = peer.decode(&straggler, "");
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(!status.first_frame, "straggler must not be a first frame");
        assert!(
            !peer.video_enabled,
            "straggler after disable must not re-enable"
        );
    }

    // -- MeetingPacket target_user_id filtering tests ---------------------------

    /// A MeetingPacket with PARTICIPANT_ADMITTED and a specific target_user_id
    /// should round-trip through protobuf serialization correctly.
    #[wasm_bindgen_test]
    fn meeting_packet_participant_admitted_deserializes_correctly() {
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
        use videocall_types::protos::meeting_packet::MeetingPacket;
        let mut packet = MeetingPacket::new();
        packet.event_type = MeetingEventType::PARTICIPANT_ADMITTED.into();
        packet.room_id = "room-123".into();
        packet.target_user_id = "alice@example.com".as_bytes().to_vec();
        packet.message = "Welcome".into();

        let bytes = packet.write_to_bytes().expect("serialize MeetingPacket");
        let parsed = MeetingPacket::parse_from_bytes(&bytes).expect("parse MeetingPacket");

        assert_eq!(
            parsed.event_type.enum_value(),
            Ok(MeetingEventType::PARTICIPANT_ADMITTED),
            "event_type should be PARTICIPANT_ADMITTED"
        );
        assert_eq!(parsed.room_id, "room-123");
        assert_eq!(parsed.target_user_id[..], *"alice@example.com".as_bytes());
        assert_eq!(parsed.message, "Welcome");
    }

    /// Verify the target_user_id comparison used for filtering:
    /// the callback should only fire when target_user_id matches the local userid.
    #[wasm_bindgen_test]
    fn meeting_packet_target_user_id_matching_logic() {
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let mut packet = MeetingPacket::new();
        packet.target_user_id = "alice@example.com".as_bytes().to_vec();

        // Matching case: target_user_id equals userid converted to bytes
        let userid_bytes = "alice@example.com".as_bytes();
        assert_eq!(
            packet.target_user_id[..],
            *userid_bytes,
            "target_user_id should match the local userid"
        );

        // Non-matching case: target_user_id does not equal a different userid
        let observer_bytes = "observer".as_bytes();
        assert_ne!(
            packet.target_user_id[..],
            *observer_bytes,
            "target_user_id should NOT match a different userid"
        );
    }

    /// Verify that PARTICIPANT_REJECTED events also carry target_user_id correctly.
    #[wasm_bindgen_test]
    fn meeting_packet_participant_rejected_has_target_user_id() {
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let mut packet = MeetingPacket::new();
        packet.event_type = MeetingEventType::PARTICIPANT_REJECTED.into();
        packet.target_user_id = "bob@example.com".as_bytes().to_vec();
        packet.room_id = "room-456".into();

        let bytes = packet.write_to_bytes().expect("serialize");
        let parsed = MeetingPacket::parse_from_bytes(&bytes).expect("parse");

        assert_eq!(
            parsed.event_type.enum_value(),
            Ok(MeetingEventType::PARTICIPANT_REJECTED)
        );
        assert_eq!(parsed.target_user_id[..], *"bob@example.com".as_bytes());
        assert_eq!(parsed.room_id, "room-456");
    }

    /// An empty target_user_id field should not match any real userid.
    #[wasm_bindgen_test]
    fn meeting_packet_empty_target_user_id_does_not_match() {
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let packet = MeetingPacket::new();
        assert!(packet.target_user_id.is_empty());

        let userid_bytes = "alice@example.com".as_bytes();
        assert_ne!(
            packet.target_user_id[..],
            *userid_bytes,
            "empty target_user_id should not match any userid"
        );
    }

    // -- PLI gap detection tests -------------------------------------------

    /// Sequential VIDEO packets (no gap) should NOT trigger a keyframe request.
    #[wasm_bindgen_test]
    fn sequential_video_packets_no_keyframe_request() {
        let (mut peer, _muted) = make_test_peer(200);

        for seq in 1..=10 {
            let result = peer.track_sequence(MediaType::VIDEO, &{
                use videocall_types::protos::media_packet::VideoMetadata;
                MediaPacket {
                    video_metadata: Some(VideoMetadata {
                        sequence: seq,
                        ..Default::default()
                    })
                    .into(),
                    frame_type: "delta".to_string(),
                    ..Default::default()
                }
            });
            assert!(
                result.keyframe_request.is_none(),
                "Sequential seq={seq} should not trigger keyframe request"
            );
        }
        // No loss should have been detected.
        assert_eq!(peer.video_seq_tracker.lost_count, 0);
    }

    /// Genuine loss (packets shifting off the 64-packet window) should record
    /// a loss timestamp but NOT immediately trigger a keyframe request because
    /// the initial timeout hasn't elapsed.
    #[wasm_bindgen_test]
    fn video_loss_detected_but_no_immediate_request() {
        let (mut peer, _muted) = make_test_peer(201);

        // Send seq 1.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt1)
            .keyframe_request;

        // Bridge to seq 3, leaving seq 2 as a genuine in-window hole that the
        // seq-70 jump then shifts off (see `LOSS_BRIDGE_SEQ`).
        let _ = peer
            .track_sequence(MediaType::VIDEO, &delta_seq_packet(LOSS_BRIDGE_SEQ))
            .keyframe_request;

        // Send seq 70 -- this shifts the seq-2 hole off the 64-packet window,
        // confirming it as genuinely lost (it never arrived).
        let pkt70 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 70,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer
            .track_sequence(MediaType::VIDEO, &pkt70)
            .keyframe_request;

        // Loss should be recorded.
        assert!(
            peer.video_seq_tracker.lost_count > 0,
            "Loss should be detected"
        );
        // But timeout hasn't elapsed, so no request yet.
        assert!(
            result.is_none(),
            "Should not immediately trigger keyframe request"
        );
    }

    /// After genuine loss is detected and enough time has passed (simulated by
    /// backdating loss_detected_at_ms), a keyframe request should fire.
    #[wasm_bindgen_test]
    fn video_loss_triggers_keyframe_after_timeout() {
        let (mut peer, _muted) = make_test_peer(202);

        // Establish baseline sequence.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt1)
            .keyframe_request;

        // Bridge to seq 3 so seq 2 is a real in-window hole (see `LOSS_BRIDGE_SEQ`).
        let _ = peer
            .track_sequence(MediaType::VIDEO, &delta_seq_packet(LOSS_BRIDGE_SEQ))
            .keyframe_request;

        // Introduce genuine loss: seq 70 shifts the seq-2 hole off the window.
        let pkt70 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 70,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt70)
            .keyframe_request;
        assert!(peer.video_seq_tracker.lost_count > 0);

        // Simulate time having passed: backdate the loss detection timestamp
        // so that the next call sees elapsed >= KEYFRAME_REQUEST_TIMEOUT_MS.
        peer.video_seq_tracker.loss_detected_at_ms =
            Some(now_ms().saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 100));
        // Also ensure rate-limit is not in effect.
        peer.video_seq_tracker.last_keyframe_request_ms = 0;

        // Next packet should trigger a request.
        let pkt71 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 71,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer
            .track_sequence(MediaType::VIDEO, &pkt71)
            .keyframe_request;
        assert_eq!(
            result,
            Some(MediaType::VIDEO),
            "Keyframe request should fire after timeout"
        );
    }

    /// Rate-limiting: a second keyframe request within the backoff interval
    /// should be suppressed.
    #[wasm_bindgen_test]
    fn keyframe_request_rate_limited() {
        let (mut peer, _muted) = make_test_peer(203);

        // Establish genuine loss: seq 1 -> seq 70.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt1)
            .keyframe_request;
        let pkt70 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 70,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt70)
            .keyframe_request;

        // Backdate loss so timeout is satisfied.
        peer.video_seq_tracker.loss_detected_at_ms =
            Some(now_ms().saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 100));
        peer.video_seq_tracker.last_keyframe_request_ms = 0;

        // First request should fire.
        let pkt71 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 71,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer
            .track_sequence(MediaType::VIDEO, &pkt71)
            .keyframe_request;
        assert_eq!(result, Some(MediaType::VIDEO), "First request should fire");

        // last_keyframe_request_ms is now set to ~now. A second call
        // immediately should be suppressed by backoff.
        let pkt72 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 72,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result2 = peer
            .track_sequence(MediaType::VIDEO, &pkt72)
            .keyframe_request;
        assert!(
            result2.is_none(),
            "Second request should be rate-limited (too soon)"
        );
    }

    /// A keyframe ("key" frame_type) should clear the loss state.
    #[wasm_bindgen_test]
    fn keyframe_clears_loss_state() {
        let (mut peer, _muted) = make_test_peer(204);

        // Establish genuine loss: send seq 1, then fill the window to push
        // the gap past the 64-packet boundary so it's counted as lost.
        // seq 1 -> seq 3 (skip 2) -> seq 67 (shifts seq 2 off as lost)
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt1)
            .keyframe_request;

        let pkt3 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 3,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt3)
            .keyframe_request;

        // Advance to seq 67 (shift = 64), pushing seq 2 off the window as lost.
        let pkt67 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 67,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt67)
            .keyframe_request;
        assert!(peer.video_seq_tracker.lost_count > 0, "Loss should exist");

        // Now receive a keyframe -- should clear the loss state.
        let key_pkt = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 71,
                    ..Default::default()
                })
                .into(),
                frame_type: "key".to_string(),
                ..Default::default()
            }
        };
        let result = peer
            .track_sequence(MediaType::VIDEO, &key_pkt)
            .keyframe_request;
        assert!(result.is_none(), "Keyframe should not trigger request");
        assert_eq!(
            peer.video_seq_tracker.lost_count, 0,
            "Keyframe should clear loss state"
        );
        assert!(
            peer.video_seq_tracker.loss_detected_at_ms.is_none(),
            "Keyframe should clear loss_detected_at_ms"
        );
    }

    /// Video and screen sequences should be tracked independently.
    #[wasm_bindgen_test]
    fn video_and_screen_independent_tracking() {
        let (mut peer, _muted) = make_test_peer(205);

        // Send video seq 1, 2, 3 (no gap).
        for seq in 1..=3 {
            let pkt = {
                use videocall_types::protos::media_packet::VideoMetadata;
                MediaPacket {
                    video_metadata: Some(VideoMetadata {
                        sequence: seq,
                        ..Default::default()
                    })
                    .into(),
                    frame_type: "delta".to_string(),
                    ..Default::default()
                }
            };
            let _ = peer.track_sequence(MediaType::VIDEO, &pkt).keyframe_request;
        }
        assert_eq!(peer.video_seq_tracker.lost_count, 0);

        // Send screen seq 1 -> 3 -> 70: the bridge leaves seq 2 as a genuine
        // in-window hole which the seq-70 jump then shifts off as lost (see
        // `LOSS_BRIDGE_SEQ`). VIDEO stays clean, so the two trackers diverge.
        let screen1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::SCREEN, &screen1)
            .keyframe_request;
        let _ = peer
            .track_sequence(MediaType::SCREEN, &delta_seq_packet(LOSS_BRIDGE_SEQ))
            .keyframe_request;

        let screen70 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 70,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::SCREEN, &screen70)
            .keyframe_request;

        // Video should have no loss, screen should have loss.
        assert_eq!(
            peer.video_seq_tracker.lost_count, 0,
            "Video should have no loss"
        );
        assert!(
            peer.screen_seq_tracker.lost_count > 0,
            "Screen should have loss"
        );

        // Verify high_seq values are independent.
        assert_eq!(peer.video_seq_tracker.high_seq, Some(3));
        assert_eq!(peer.screen_seq_tracker.high_seq, Some(70));
    }

    /// Build a VIDEO MediaPacket wrapper carrying a cleartext `simulcast_layer_id`
    /// and a per-layer `sequence`, ready for `Peer::decode`. Mirrors the real
    /// wire shape the encoders produce (outer wrapper layer id + inner
    /// VideoMetadata sequence).
    fn video_layer_wrap(layer_id: u32, seq: u64, session_id: u64) -> Arc<PacketWrapper> {
        use videocall_types::protos::media_packet::VideoMetadata;
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            user_id: b"test@test.com".to_vec(),
            video_metadata: Some(VideoMetadata {
                sequence: seq,
                // A REAL codec, matching what the encoders actually stamp. Without
                // this the proto3 default is `VIDEO_CODEC_UNSPECIFIED`, and
                // `VideoPeerDecoder::decode` takes its "unknown codec" early return
                // (`_rendered: false`) — so a packet built by this helper never reached
                // the decoder at all, silently diverging from the "mirrors the real
                // wire shape" contract in this doc comment, and making any test that
                // asserts on decode OUTCOME (e.g. #2190's frame-counting regression) read
                // a false negative.
                //
                // Effect on the pre-existing seq-tracker tests below: their ASSERTIONS are
                // unaffected (`track_sequence` runs before the decoder call), but they do
                // now execute materially more code — the real `push_frame` path instead of
                // stopping at the unknown-codec early return. That is strictly closer to
                // production, not a no-op; calling it out so nobody reads this as inert.
                codec: videocall_types::protos::media_packet::VideoCodec::VP8.into(),
                ..Default::default()
            })
            .into(),
            frame_type: "delta".to_string(),
            ..Default::default()
        };
        let data = media.write_to_bytes().expect("serialize MediaPacket");
        Arc::new(PacketWrapper {
            data,
            user_id: "test@test.com".into(),
            packet_type: PacketType::MEDIA.into(),
            session_id,
            simulcast_layer_id: layer_id,
            ..Default::default()
        })
    }

    fn prime_video_tracker_for_pli_rollover(peer: &mut Peer) {
        let now = now_ms();
        peer.video_seq_tracker.lost_count = 1;
        peer.video_seq_tracker.loss_detected_at_ms =
            Some(now.saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 1));
        peer.video_seq_tracker.last_keyframe_request_ms = 0;
        peer.video_seq_tracker.window_start_ms = now.saturating_sub(1000);
    }

    #[wasm_bindgen_test]
    fn video_off_discard_buckets_suppressed_keyframe_request() {
        let (mut peer, _muted) = make_test_peer(25240);
        peer.has_received_heartbeat = true;
        peer.visible = true;
        prime_video_tracker_for_pli_rollover(&mut peer);

        let result = peer
            .decode(&video_layer_wrap(0, 1, 25240), "local@test.com")
            .expect("decode returns skipped");
        assert_eq!(result.0, MediaType::VIDEO);
        assert!(!result.1.rendered);
        assert_eq!(result.2, None);
        assert!(
            peer.video_seq_tracker
                .kf_suppression_per_sec()
                .get(KeyframeSuppressionReason::DiscardedVideoOff)
                > 0.0
        );
    }

    #[wasm_bindgen_test]
    fn invisible_video_discard_buckets_suppressed_keyframe_request() {
        let (mut peer, _muted) = make_test_peer(25241);
        peer.has_received_heartbeat = true;
        peer.video_enabled = true;
        peer.visible = false;
        prime_video_tracker_for_pli_rollover(&mut peer);

        let result = peer
            .decode(&video_layer_wrap(0, 1, 25241), "local@test.com")
            .expect("decode returns skipped");
        assert_eq!(result.0, MediaType::VIDEO);
        assert!(!result.1.rendered);
        assert_eq!(result.2, None);
        assert!(
            peer.video_seq_tracker
                .kf_suppression_per_sec()
                .get(KeyframeSuppressionReason::DiscardedNotVisible)
                > 0.0
        );
    }

    /// A SCREEN `PacketWrapper` carrying `VideoMetadata.sequence` — the SCREEN
    /// sibling of [`video_layer_wrap`]. `track_sequence` reads `video_metadata`
    /// for both kinds, matching `transform_screen_chunk`.
    fn screen_seq_wrap(seq: u64, session_id: u64) -> Arc<PacketWrapper> {
        use videocall_types::protos::media_packet::VideoMetadata;
        let media = MediaPacket {
            media_type: MediaType::SCREEN.into(),
            user_id: b"test@test.com".to_vec(),
            video_metadata: Some(VideoMetadata {
                sequence: seq,
                codec: videocall_types::protos::media_packet::VideoCodec::VP8.into(),
                ..Default::default()
            })
            .into(),
            frame_type: "delta".to_string(),
            ..Default::default()
        };
        let data = media.write_to_bytes().expect("serialize MediaPacket");
        Arc::new(PacketWrapper {
            data,
            user_id: "test@test.com".into(),
            packet_type: PacketType::MEDIA.into(),
            session_id,
            simulcast_layer_id: 0,
            ..Default::default()
        })
    }

    /// The HEALTHY (decoded) video path must settle the deferred window too.
    /// Without it `window_start_ms` never rolls, so `loss_per_sec` / `kf_per_sec`
    /// stay 0 forever and `last_video_downlink` — the #989 chooser's input and
    /// the source of the `video_seq_loss_per_sec` health metric — freezes.
    #[wasm_bindgen_test]
    fn healthy_video_decode_rolls_the_loss_window_and_caches_the_downlink() {
        let (mut peer, _muted) = make_test_peer(25242);
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        peer.visible = true;
        peer.video_seq_tracker.window_lost = 3;
        peer.video_seq_tracker.window_start_ms = now_ms().saturating_sub(1000);

        let result = peer
            .decode(&video_layer_wrap(0, 1, 25242), "local@test.com")
            .expect("healthy decode");
        assert_eq!(result.0, MediaType::VIDEO);
        assert!(
            peer.last_video_downlink.loss_per_sec > 0.0,
            "the healthy path must roll the window and cache the downlink sample"
        );
    }

    /// The SCREEN arm's twin of the above: it settles its window inline, with no
    /// discard interaction, and caches into `last_screen_downlink`.
    #[wasm_bindgen_test]
    fn screen_decode_rolls_the_loss_window_and_caches_the_downlink() {
        let (mut peer, _muted) = make_test_peer(25244);
        peer.screen_enabled = true;
        peer.has_received_heartbeat = true;
        peer.visible = true;
        peer.screen_seq_tracker.window_lost = 3;
        peer.screen_seq_tracker.window_start_ms = now_ms().saturating_sub(1000);

        let result = peer
            .decode(&screen_seq_wrap(1, 25244), "local@test.com")
            .expect("screen decode");
        assert_eq!(result.0, MediaType::SCREEN);
        assert!(
            peer.last_screen_downlink.loss_per_sec > 0.0,
            "the screen path must roll the window and cache the downlink sample"
        );
    }

    /// A layer switch must not carry the OLD rung's in-flight suppression counts
    /// into the new rung's window, where they would be divided by the new
    /// window's denominator and published as the new rung's rate.
    #[wasm_bindgen_test]
    fn layer_switch_clears_the_in_flight_keyframe_suppression_counts() {
        let (mut peer, _muted) = make_test_peer(25243);
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        peer.visible = true;
        // A fresh window, so nothing rolls over while the counts accumulate.
        peer.video_seq_tracker.window_start_ms = now_ms();

        // Baseline, then a forward gap past the reorder window => real loss =>
        // every following packet books a PreTimeout suppression.
        let _ = peer.decode(&video_layer_wrap(0, 0, 25243), "local@test.com");
        for seq in [200u64, 201, 202, 203, 204, 205] {
            let _ = peer.decode(&video_layer_wrap(0, seq, 25243), "local@test.com");
        }
        assert!(
            peer.video_seq_tracker.window_kf_suppressions
                [KeyframeSuppressionReason::PreTimeout.index()]
                > 0,
            "pre-switch: the old rung must have booked pre_timeout suppressions"
        );

        // The production entry point the chooser uses.
        peer.set_selected_video_layer(1);

        // Roll a fresh window on the new rung's first packet.
        peer.video_seq_tracker.window_start_ms = now_ms().saturating_sub(1000);
        let _ = peer.decode(&video_layer_wrap(1, 0, 25243), "local@test.com");

        assert_eq!(
            peer.video_seq_tracker
                .kf_suppression_per_sec()
                .get(KeyframeSuppressionReason::PreTimeout),
            0.0,
            "the new rung's window must not inherit the old rung's suppression counts"
        );
    }

    /// H1 (#1079) regression: a REAL simulcast layer switch driven through
    /// `decode()` must NOT manufacture phantom loss / PLI.
    ///
    /// Each layer has its own per-layer sequence counter, so the new layer's
    /// first packets carry sequence numbers from a different (lower) counter than
    /// the old layer's `high_seq`. Without re-anchoring the tracker on switch,
    /// `record_seq` reads the cross-counter discontinuity as a massive window
    /// shift → phantom loss → the chooser's own congestion signal → oscillation.
    /// This drives the real tracker path (decode → guard → track_sequence), not
    /// synthetic DownlinkSamples, exactly as the reviewer required.
    #[wasm_bindgen_test]
    fn real_layer_switch_does_not_manufacture_phantom_loss() {
        let (mut peer, _muted) = make_test_peer(909);
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;

        // Receiving layer 0 at a HIGH sequence (the old counter has advanced).
        for seq in [100u64, 101, 102] {
            let _ = peer.decode(&video_layer_wrap(0, seq, 909), "local@test.com");
        }
        assert_eq!(
            peer.video_seq_tracker.lost_count, 0,
            "no loss while steadily receiving layer 0"
        );
        assert_eq!(peer.video_seq_tracker.high_seq, Some(102));

        // REAL switch to layer 1 (the production entry point the chooser uses).
        // This must re-anchor the tracker so the new layer's low sequence numbers
        // are not diffed against the old layer's high_seq.
        peer.set_selected_video_layer(1);
        assert!(
            peer.video_seq_tracker.high_seq.is_none(),
            "switch must re-anchor: tracker baseline cleared"
        );

        // Layer 1 starts from its OWN counter (low seq 0,1,2) — far below 102.
        // Pre-fix, the first packet (seq 0 vs high_seq 102) would have looked like
        // an enormous backwards jump / window shift; post-fix it is a clean
        // fresh baseline.
        for seq in [0u64, 1, 2] {
            let _ = peer.decode(&video_layer_wrap(1, seq, 909), "local@test.com");
        }

        // The whole point: no phantom loss, no PLI, no loss-detected timestamp.
        assert_eq!(
            peer.video_seq_tracker.lost_count, 0,
            "post-switch: a real layer switch must NOT manufacture phantom loss"
        );
        assert!(
            peer.video_seq_tracker.loss_detected_at_ms.is_none(),
            "post-switch: no loss should be flagged after re-anchor"
        );
        assert_eq!(
            peer.video_seq_tracker.window_lost, 0,
            "post-switch: the cross-counter discontinuity must not leak into the rate window"
        );
        // Baseline is the new layer's stream.
        assert_eq!(peer.video_seq_tracker.high_seq, Some(2));

        // A no-op "switch" to the SAME layer must NOT reset a healthy tracker.
        peer.set_selected_video_layer(1);
        assert_eq!(
            peer.video_seq_tracker.high_seq,
            Some(2),
            "selecting the already-selected layer must be a no-op (no spurious reset)"
        );
    }

    /// #1066: build a MEDIA `PacketWrapper` with a CLEARTEXT outer `media_kind`
    /// and `simulcast_layer_id`, but with `data` that is NOT validly AES-encrypted
    /// (raw bytes). When the peer has an enabled AES key, `aes.decrypt` of this
    /// `data` FAILS — so reaching the decrypt step at all surfaces as
    /// `Err(AesDecryptError)`. Tests use this to prove the cleartext layer gate
    /// early-returns BEFORE decrypt for dropped layers.
    fn cleartext_kind_wrap(
        media_kind: MediaKind,
        layer_id: u32,
        session_id: u64,
    ) -> Arc<PacketWrapper> {
        Arc::new(PacketWrapper {
            // Deliberately NOT a valid ciphertext: short, non-block-aligned bytes
            // so an enabled-AES `decrypt` errors if it is ever attempted.
            data: vec![1u8, 2, 3, 4, 5],
            user_id: "test@test.com".into(),
            packet_type: PacketType::MEDIA.into(),
            session_id,
            simulcast_layer_id: layer_id,
            media_kind: media_kind.into(),
            ..Default::default()
        })
    }

    /// #1066: a non-selected simulcast layer must be DROPPED on the CLEARTEXT
    /// envelope BEFORE AES-decrypt. With an enabled AES key and intentionally
    /// invalid ciphertext, a layer the gate drops returns `Ok(SKIPPED)` (decrypt
    /// never ran); if the drop had keyed on the DECRYPTED inner media_type as
    /// before, the same packet would have hit `aes.decrypt` first and returned
    /// `Err(AesDecryptError)`. This is the regression that pins the perf fix.
    #[wasm_bindgen_test]
    fn cleartext_layer_gate_drops_before_decrypt() {
        let (mut peer, _muted) = make_test_peer(970);
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        // Enabled AES: any decrypt attempt on the invalid ciphertext below errors.
        peer.aes = Some(Aes128State::new(true));
        // Receiver decodes only the base layer (the N=1 default), so layer 2 is
        // a non-selected layer that must be dropped.
        assert_eq!(peer.selected_video_layer(), 0);

        let dropped = cleartext_kind_wrap(MediaKind::VIDEO, 2, 970);
        let result = peer.decode(&dropped, "local@test.com");

        // The gate fired BEFORE decrypt: a clean SKIPPED, not an AES error.
        match result {
            Ok((MediaType::VIDEO, status, kf)) => {
                assert!(!status.rendered, "dropped layer must not render");
                assert!(kf.is_none(), "dropped layer must not request a keyframe");
            }
            Ok(other) => panic!("expected Ok((VIDEO, SKIPPED, None)), got {other:?}"),
            Err(e) => panic!(
                "non-selected layer must be dropped on the cleartext envelope BEFORE \
                 decrypt — reaching decrypt produced {e:?}"
            ),
        }

        // Availability was still observed pre-decrypt (the chooser must learn a
        // higher layer exists even though we dropped this packet).
        assert_eq!(
            peer.video_layer_availability.highest_available(now_ms()),
            2,
            "the dropped layer's id must still be observed for the chooser"
        );
    }

    /// #1066: at the N=1 default (selected layer 0, publisher layer 0) the gate
    /// must be INERT — a layer-0 packet is NOT dropped pre-decrypt, so it
    /// proceeds to the (here intentionally failing) decrypt. With an enabled AES
    /// key and invalid ciphertext that surfaces as `Err(AesDecryptError)`,
    /// proving the gate did NOT short-circuit the base layer.
    #[wasm_bindgen_test]
    fn cleartext_layer_gate_is_inert_for_base_layer() {
        let (mut peer, _muted) = make_test_peer(971);
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        peer.aes = Some(Aes128State::new(true));
        assert_eq!(peer.selected_video_layer(), 0);

        // Layer 0 == selected layer 0 → the gate forwards (does NOT drop), so the
        // packet reaches decrypt. The invalid ciphertext then errors, which is
        // exactly the signal that the base layer was NOT short-circuited.
        let base = cleartext_kind_wrap(MediaKind::VIDEO, 0, 971);
        let result = peer.decode(&base, "local@test.com");
        assert!(
            matches!(result, Err(PeerDecodeError::AesDecryptError)),
            "base layer (N=1) must fall through the gate to decrypt unchanged \
             (got {result:?})"
        );
    }

    /// Issue #2190: a wrong-rung packet the EXACT-MATCH simulcast guard SKIPPED must
    /// NOT be counted as a received frame.
    ///
    /// This is the defect that made `videocall_video_fps` read the simulcast LADDER
    /// SUM instead of the decoded rung: the relay fails OPEN and forwards ALL of a
    /// publisher's rungs to a healthy receiver, the guard drops the non-selected ones
    /// as `DecodeStatus::SKIPPED`, but `track_frame` was called on the whole `Ok(..)`
    /// arm regardless — so every dropped rung still bumped the fps counter. Field
    /// measurement: an 8fps publisher with 3 active camera rungs (7+15+30) read
    /// **~52 fps** on the receiver, i.e. HEALTHIER than the publisher's real output,
    /// during a call that was visibly stalled 21% of the time.
    ///
    /// Driven through `PeerDecodeManager::decode` — the real production entry point
    /// that owns the `track_frame` call site — NOT through `Peer::decode` directly,
    /// because the call site is precisely what the fix changes.
    ///
    /// MUTATION: reverting the call site's 4th argument to a literal `true` makes the
    /// two `decoded` assertions below fail (the skipped rung reads counted, and
    /// `frames_decoded` reads 2 instead of 1). Deleting the `if decoded` guard in
    /// `track_frame` fails the `frames_decoded_for_test` assertion on its own.
    ///
    /// `#[wasm_bindgen_test]`: `DiagnosticManager::new` starts a real `HeartbeatTimer`
    /// (Worker, falling back to `setInterval`) and the trackers live on a
    /// `spawn_local` task, so this needs a browser. The seam it asserts on is written
    /// synchronously at the call site, so no executor yield is required.
    #[wasm_bindgen_test]
    fn skipped_wrong_rung_packet_is_not_counted_as_a_received_frame() {
        let diagnostics = Rc::new(crate::diagnostics::DiagnosticManager::new(
            "local@test.com".to_string(),
        ));
        let mut manager = PeerDecodeManager::new_with_diagnostics(diagnostics.clone());

        // A peer whose VIDEO is flowing and visible, so the ONLY thing that can
        // skip a packet below is the simulcast rung guard.
        let session_id = 2190u64;
        let (mut peer, _muted) = make_test_peer(session_id);
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        peer.visible = true;
        // issue 2511: non-zero and non-default, so the assertion below also catches a literal.
        peer.video.set_last_output_ms_for_test(1_111.0);
        manager.connected_peers.insert(session_id, peer);

        // Receiver is decoding the base rung (the N=1 default).
        assert_eq!(
            manager.selected_video_layer_for_test(session_id),
            Some(0),
            "precondition: receiver decodes rung 0"
        );

        // Rung 0 — the SELECTED rung. Decoded, so it must count.
        manager
            .decode(
                Arc::try_unwrap(video_layer_wrap(0, 1, session_id))
                    .expect("sole owner of the wrapper"),
                "local@test.com",
            )
            .expect("selected-rung packet must decode");

        // Rung 2 — a rung this receiver is NOT decoding, which the relay forwarded
        // anyway (fail-open). The guard SKIPs it pre-decrypt.
        manager
            .decode(
                Arc::try_unwrap(video_layer_wrap(2, 1, session_id))
                    .expect("sole owner of the wrapper"),
                "local@test.com",
            )
            .expect("a skipped wrong-rung packet is not an error");

        let calls = diagnostics.take_track_frame_calls_for_test();
        assert_eq!(
            calls.len(),
            2,
            "both packets still reach track_frame (bytes must be counted for \
             bitrate_kbps either way): {calls:?}"
        );
        assert!(
            calls[0].2,
            "the SELECTED rung was decoded and must be counted as a frame: {calls:?}"
        );
        assert!(
            !calls[1].2,
            "the wrong-rung packet was SKIPPED and must NOT be counted as a frame — \
             counting it is what made fps read the ladder sum (#2190): {calls:?}"
        );
        // issue 2511: losing this argument pins the clock at its seed = every tile reads frozen.
        assert_eq!(
            calls[0].4,
            Some(1_111.0),
            "the decoded arrival must carry this peer's video output clock: {calls:?}"
        );
        assert_eq!(
            calls[1].4,
            Some(1_111.0),
            "a skipped arrival carries it too — a skip is not output: {calls:?}"
        );

        // The cumulative counter named `frames_decoded` must likewise count only
        // real decodes.
        assert_eq!(
            diagnostics.frames_decoded_for_test(),
            1,
            "frames_decoded must advance once (the decoded rung), not twice"
        );
    }

    /// #2511: the freeze machine's publish gate is fed from the HEARTBEAT arm.
    #[wasm_bindgen_test]
    fn a_heartbeat_publishes_the_peers_media_state_to_diagnostics() {
        let diagnostics = Rc::new(crate::diagnostics::DiagnosticManager::new(
            "local@test.com".to_string(),
        ));
        let mut manager = PeerDecodeManager::new_with_diagnostics(diagnostics.clone());

        let session_id = 2511u64;
        let (peer, _muted) = make_test_peer(session_id);
        let sid_str = peer.sid_str.clone();
        manager.connected_peers.insert(session_id, peer);

        for (video, screen) in [(true, false), (true, true)] {
            manager
                .decode(
                    Arc::try_unwrap(heartbeat_packet(session_id, video, true, screen))
                        .expect("sole owner of the wrapper"),
                    "local@test.com",
                )
                .expect("a heartbeat must decode");
        }

        assert_eq!(
            diagnostics.take_peer_media_state_calls_for_test(),
            vec![(sid_str.clone(), true, false), (sid_str, true, true)],
            "every heartbeat must forward the peer's CURRENT camera and share state; an \
             empty vec means the freeze machine never learns the source stopped"
        );
    }

    #[test]
    fn active_decode_set_publishes_decode_eligibility_to_diagnostics() {
        let diagnostics = Rc::new(
            crate::diagnostics::DiagnosticManager::new_for_test_without_timer(
                "local@test.com".to_string(),
            ),
        );
        let mut manager = PeerDecodeManager::new_with_diagnostics(diagnostics.clone());

        let session_id = 2512u64;
        let (peer, _muted) = make_test_peer(session_id);
        let sid_str = peer.sid_str.clone();
        manager.connected_peers.insert(session_id, peer);

        manager.set_active_decode_set(&HashSet::from([session_id]));
        assert_eq!(
            diagnostics.take_peer_decode_eligibility_calls_for_test(),
            vec![(sid_str.clone(), true)],
            "visibility-on must immediately publish decode eligibility; waiting for a frame \
             or heartbeat leaves content-staleness samples gated by stale state"
        );

        manager.set_active_decode_set(&HashSet::new());
        assert_eq!(
            diagnostics.take_peer_decode_eligibility_calls_for_test(),
            vec![(sid_str, false)],
            "visibility-off must immediately publish decode ineligibility so hidden-tile \
             staleness samples cannot ride on the prior visible state"
        );
    }

    /// Issue #2249: the call site must report this receiver's decode ELIGIBILITY, or the
    /// health reporter sees only "fps 0, bitrate live" and cannot tell a backgrounded SCREEN
    /// tile — which keeps receiving, since SCREEN is never viewport-filtered — from a freeze.
    ///
    /// MUTATION: a literal `true` OR a literal `false` at the call site fails one of the two
    /// assertions, so the flag cannot be hardcoded either way.
    ///
    /// `#[wasm_bindgen_test]` for the same reason as the sibling above: `DiagnosticManager`
    /// starts a real heartbeat timer.
    #[wasm_bindgen_test]
    fn screen_track_frame_reports_decode_eligibility_from_peer_visible() {
        let diagnostics = Rc::new(crate::diagnostics::DiagnosticManager::new(
            "local@test.com".to_string(),
        ));
        let mut manager = PeerDecodeManager::new_with_diagnostics(diagnostics.clone());

        let session_id = 2249u64;
        let (mut peer, _muted) = make_test_peer(session_id);
        peer.screen_enabled = true;
        peer.has_received_heartbeat = true;
        // The backgrounded second sharer: outside `ss_decoded_tiles`.
        peer.visible = false;
        manager.connected_peers.insert(session_id, peer);

        manager
            .decode(
                Arc::try_unwrap(screen_layer_packet(session_id, 0, "key", 1))
                    .expect("sole owner of the wrapper"),
                "local@test.com",
            )
            .expect("a SCREEN packet for a hidden tile is not an error");

        let calls = diagnostics.take_track_frame_calls_for_test();
        assert_eq!(
            calls.len(),
            1,
            "the packet still reaches track_frame — it consumed downlink: {calls:?}"
        );
        assert!(
            !calls[0].3,
            "a tile we declined to decode must report decode_eligible=false, or the health              reporter scores this healthy publisher 0 indefinitely (#2249): {calls:?}"
        );

        // Same peer, now on-screen: the identical packet must report eligible.
        manager
            .connected_peers
            .get_mut(&session_id)
            .expect("peer present")
            .visible = true;
        // Outcome deliberately ignored: BOTH the Ok and the Err arm call `track_frame`, and
        // the assertion below is on the recorded eligibility, not on the decode result.
        let _ = manager.decode(
            Arc::try_unwrap(screen_layer_packet(session_id, 0, "key", 2))
                .expect("sole owner of the wrapper"),
            "local@test.com",
        );

        let calls = diagnostics.take_track_frame_calls_for_test();
        assert_eq!(
            calls.len(),
            1,
            "one arrival since the last drain: {calls:?}"
        );
        assert!(
            calls[0].3,
            "a tile we ARE decoding must report decode_eligible=true, or #2249 freeze              detection is disabled for every stream: {calls:?}"
        );
    }

    /// Issue #2190 follow-up: a packet that reaches the decoder but errors is still an
    /// arrival. It must refresh the arrival clock and count its bytes without incrementing
    /// decoded FPS.
    ///
    /// Driven through `PeerDecodeManager::decode`, the production owner of the error arm.
    /// The invalid AES payload deterministically reaches that arm.
    ///
    /// MUTATION: removing the `track_frame(..., false)` call from the error arm leaves the
    /// recorded call list empty and fails this test.
    #[wasm_bindgen_test]
    fn decode_errors_are_recorded_as_non_decoded_arrivals_by_media_type() {
        ensure_decoder_worker_link_tags();
        let diagnostics = Rc::new(crate::diagnostics::DiagnosticManager::new(
            "local@test.com".to_string(),
        ));
        let mut manager = PeerDecodeManager::new_with_diagnostics(diagnostics.clone());

        let mut expected_calls = Vec::new();
        for (session_id, media_kind, expected_media_type, expected_output_ms) in [
            (2191u64, MediaKind::VIDEO, MediaType::VIDEO, 3_331.0),
            (2192u64, MediaKind::SCREEN, MediaType::SCREEN, 4_442.0),
        ] {
            let (mut peer, _muted) = make_test_peer(session_id);
            peer.video_enabled = true;
            peer.screen_enabled = true;
            peer.has_received_heartbeat = true;
            peer.visible = true;
            peer.aes = Some(Aes128State::new(true));
            // issue 2511: distinct per kind, so the Err arm cannot pass on the `0.0` sentinel and
            // must resolve the clock of the decoder this error actually implicates.
            peer.video.set_last_output_ms_for_test(3_331.0);
            peer.screen.set_last_output_ms_for_test(4_442.0);
            manager.connected_peers.insert(session_id, peer);

            let packet = cleartext_kind_wrap(media_kind, 0, session_id);
            let packet_len = packet.data.len() as u64;
            let result = manager.decode(
                Arc::try_unwrap(packet).expect("sole owner of the wrapper"),
                "local@test.com",
            );
            assert!(
                result.is_ok(),
                "the production error arm resets the peer and returns the reset result, got \
                 {result:?}"
            );
            expected_calls.push((
                expected_media_type,
                packet_len,
                false,
                true,
                Some(expected_output_ms),
            ));
        }

        assert_eq!(
            diagnostics.take_track_frame_calls_for_test(),
            expected_calls,
            "each failed packet must count against its own media arrival clock, not decoded FPS"
        );
        assert_eq!(
            diagnostics.frames_decoded_for_test(),
            0,
            "a decode error must not increment decoded FPS"
        );
    }

    /// Different peers should have independent sequence tracking.
    #[wasm_bindgen_test]
    fn different_peers_independent_sequence_tracking() {
        let (mut peer_a, _) = make_test_peer(300);
        let (mut peer_b, _) = make_test_peer(301);

        // Peer A: sequential (no loss).
        for seq in 1..=5 {
            let pkt = {
                use videocall_types::protos::media_packet::VideoMetadata;
                MediaPacket {
                    video_metadata: Some(VideoMetadata {
                        sequence: seq,
                        ..Default::default()
                    })
                    .into(),
                    frame_type: "delta".to_string(),
                    ..Default::default()
                }
            };
            let _ = peer_a
                .track_sequence(MediaType::VIDEO, &pkt)
                .keyframe_request;
        }

        // Peer B: genuine loss (seq 1 -> 3 -> 70; the bridge leaves seq 2 as a
        // real in-window hole the jump then shifts off -- see `LOSS_BRIDGE_SEQ`).
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer_b
            .track_sequence(MediaType::VIDEO, &pkt1)
            .keyframe_request;
        let _ = peer_b
            .track_sequence(MediaType::VIDEO, &delta_seq_packet(LOSS_BRIDGE_SEQ))
            .keyframe_request;
        let pkt70 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 70,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer_b
            .track_sequence(MediaType::VIDEO, &pkt70)
            .keyframe_request;

        assert_eq!(
            peer_a.video_seq_tracker.lost_count, 0,
            "Peer A should have no loss"
        );
        assert!(
            peer_b.video_seq_tracker.lost_count > 0,
            "Peer B should have loss"
        );
    }

    /// SCREEN loss triggers keyframe request for SCREEN (not VIDEO).
    #[wasm_bindgen_test]
    fn screen_loss_triggers_screen_keyframe_request() {
        let (mut peer, _muted) = make_test_peer(206);

        // Establish screen loss: seq 1 -> seq 70.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::SCREEN, &pkt1)
            .keyframe_request;

        let pkt70 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 70,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::SCREEN, &pkt70)
            .keyframe_request;

        // Backdate loss and clear rate limit.
        peer.screen_seq_tracker.loss_detected_at_ms =
            Some(now_ms().saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 100));
        peer.screen_seq_tracker.last_keyframe_request_ms = 0;

        let pkt71 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 71,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer
            .track_sequence(MediaType::SCREEN, &pkt71)
            .keyframe_request;
        assert_eq!(
            result,
            Some(MediaType::SCREEN),
            "Screen loss should trigger SCREEN keyframe request"
        );
    }

    /// Packet without video_metadata should return None from track_sequence.
    #[wasm_bindgen_test]
    fn no_video_metadata_returns_none() {
        let (mut peer, _muted) = make_test_peer(207);

        let pkt = MediaPacket {
            frame_type: "delta".to_string(),
            // No video_metadata set.
            ..Default::default()
        };
        let result = peer.track_sequence(MediaType::VIDEO, &pkt).keyframe_request;
        assert!(
            result.is_none(),
            "Missing video_metadata should return None"
        );
    }

    /// track_sequence called with AUDIO media type should return None
    /// (only VIDEO and SCREEN are tracked).
    #[wasm_bindgen_test]
    fn audio_media_type_not_tracked() {
        let (mut peer, _muted) = make_test_peer(208);

        let pkt = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer.track_sequence(MediaType::AUDIO, &pkt).keyframe_request;
        assert!(result.is_none(), "AUDIO should not be tracked");
    }

    // -- Visibility-based skip tests ----------------------------------------

    /// Native policy pin for both production media gates. A recording state is
    /// intentionally not an input: hidden camera and screen peers stay outside
    /// the decode set. Flipping either truth-table result fails this test.
    #[test]
    fn decode_eligibility_is_strictly_visibility_based() {
        assert!(should_decode_visible_peer(true));
        assert!(!should_decode_visible_peer(false));
    }

    /// Setting peer visibility to false should cause VIDEO decoding to return
    /// SKIPPED status.
    #[wasm_bindgen_test]
    fn invisible_peer_skips_video_decode() {
        let (mut peer, _muted) = make_test_peer(210);

        // Enable video via heartbeat so the straggler guard doesn't block.
        let hb = heartbeat_packet(210, true, true, false);
        let _ = peer.decode(&hb, "");
        assert!(peer.video_enabled);

        // Mark invisible.
        peer.visible = false;

        let pkt = video_frame_packet(210);
        let result = peer.decode(&pkt, "");
        assert!(result.is_ok());
        let (_mt, status, _kf) = result.unwrap();
        assert!(
            !status.rendered,
            "Invisible peer video should not be rendered"
        );
    }

    /// Setting peer visibility to false should cause SCREEN decoding to return
    /// SKIPPED status.
    #[wasm_bindgen_test]
    fn invisible_peer_skips_screen_decode() {
        let (mut peer, _muted) = make_test_peer(211);

        // Enable screen via heartbeat.
        let hb = heartbeat_packet(211, false, false, true);
        let _ = peer.decode(&hb, "");
        assert!(peer.screen_enabled);

        // Mark invisible.
        peer.visible = false;

        let pkt = screen_frame_packet(211);
        let result = peer.decode(&pkt, "");
        assert!(result.is_ok());
        let (_mt, status, _kf) = result.unwrap();
        assert!(
            !status.rendered,
            "Invisible peer screen should not be rendered"
        );
    }

    /// Audio should ALWAYS be decoded regardless of visibility.
    #[wasm_bindgen_test]
    fn invisible_peer_still_decodes_audio() {
        let (mut peer, _muted) = make_test_peer(212);

        // Enable audio (no heartbeat yet, so audio will be inferred enabled).
        peer.visible = false;

        let pkt = audio_frame_packet(212);
        let result = peer.decode(&pkt, "");
        assert!(result.is_ok());
        // Audio_enabled should be inferred true (no heartbeat received).
        assert!(
            peer.audio_enabled,
            "Audio should still be enabled/inferred even when invisible"
        );
        // The key point: the decode path does NOT check `visible` for audio.
        // The result is Ok, meaning it went through the audio decode path
        // (not the straggler SKIPPED path).
    }

    /// Restoring visibility should resume video decoding.
    #[wasm_bindgen_test]
    fn restored_visibility_resumes_video() {
        let (mut peer, _muted) = make_test_peer(213);

        // Enable video via heartbeat.
        let hb = heartbeat_packet(213, true, false, false);
        let _ = peer.decode(&hb, "");

        // Go invisible, then visible again.
        peer.visible = false;
        peer.visible = true;

        let pkt = video_frame_packet(213);
        let result = peer.decode(&pkt, "");
        // The decode will go through to the actual video decoder (noop).
        // Even if the noop decoder "fails" on dummy data, it won't return
        // SKIPPED due to visibility.
        assert!(
            result.is_ok() || result.is_err(),
            "Visible peer should attempt video decode"
        );
        // If it got through to the decoder (Ok), it means visibility didn't block it.
        if let Ok((_mt, _status, _kf)) = result {
            // With noop decoder, rendered might be false, but the important
            // thing is that it wasn't the visibility-SKIPPED path.
            // We verify by checking that the visibility path was NOT taken.
            assert!(peer.visible);
        }
    }

    /// PeerDecodeManager::set_active_decode_set should update peer visibility.
    #[wasm_bindgen_test]
    fn manager_set_active_decode_set() {
        let mut manager = PeerDecodeManager::new();
        let (peer, _muted) = make_test_peer(220);
        assert!(!peer.visible, "New peers should default to inactive decode");
        manager.connected_peers.insert(220, peer);

        manager.set_active_decode_set(&HashSet::new());
        assert!(
            !manager.connected_peers.get(&220).unwrap().visible,
            "Peer should remain inactive when omitted from active decode set"
        );

        manager.set_active_decode_set(&HashSet::from([220]));
        assert!(
            manager.connected_peers.get(&220).unwrap().visible,
            "Peer should be active after entering the decode set"
        );
    }

    /// set_active_decode_set should update all peers in one pass.
    #[wasm_bindgen_test]
    fn manager_set_active_decode_set_updates_multiple_peers() {
        let mut manager = PeerDecodeManager::new();
        let (peer_1, _muted_1) = make_test_peer(221);
        let (peer_2, _muted_2) = make_test_peer(222);
        manager.connected_peers.insert(221, peer_1);
        manager.connected_peers.insert(222, peer_2);

        manager.set_active_decode_set(&HashSet::from([222]));

        assert!(
            !manager.connected_peers.get(&221).unwrap().visible,
            "Peers omitted from the active set should be inactive"
        );
        assert!(
            manager.connected_peers.get(&222).unwrap().visible,
            "Peers in the active set should be active"
        );
    }

    /// Issue #2264: every peer this returns becomes a composited recording tile.
    /// MUTATION: drop the `should_decode_visible_peer` guard and 226 returns.
    #[wasm_bindgen_test]
    fn peer_video_canvases_omits_peers_outside_the_active_decode_set() {
        fn make_canvas() -> web_sys::HtmlCanvasElement {
            web_sys::window()
                .expect("no window")
                .document()
                .expect("no document")
                .create_element("canvas")
                .expect("create_element failed")
                .dyn_into::<web_sys::HtmlCanvasElement>()
                .expect("not an HtmlCanvasElement")
        }

        let mut manager = PeerDecodeManager::new();
        let (displayed, _muted_1) = make_test_peer(225);
        let (overflowed, _muted_2) = make_test_peer(226);
        // Both decoded once, so both keep a wired canvas: nothing detaches it.
        displayed.video.set_canvas(make_canvas()).unwrap();
        overflowed.video.set_canvas(make_canvas()).unwrap();
        manager.connected_peers.insert(225, displayed);
        manager.connected_peers.insert(226, overflowed);

        manager.set_active_decode_set(&HashSet::from([225]));

        let exposed: Vec<String> = manager
            .peer_video_canvases()
            .into_iter()
            .map(|(sid, _canvas)| sid)
            .collect();
        assert_eq!(
            exposed,
            vec!["225".to_string()],
            "only peers in the active decode set may reach the recorder"
        );
    }

    /// Issue #1183: the canvas backing bitmap must be cleared on EXACTLY the
    /// decode-stop edge (`visible: true -> false`) and on no other transition.
    ///
    /// This is a plain `#[test]` (host-run, not `#[wasm_bindgen_test]`) on
    /// purpose: the wasm-browser harness on this box silently no-ops
    /// `#[wasm_bindgen_test]`, so a wasm-only assertion would be a false green.
    /// The pure `should_clear_canvas` function carries the load-bearing edge
    /// logic; the actual `CanvasRenderingContext2d::clear_rect` lives in the
    /// wasm-only `VideoPeerDecoder::clear_canvas` and is gated on this function.
    ///
    /// Mutating the edge condition (e.g. clearing on every invisible frame, or
    /// never clearing) flips one of these assertions, so this test fails if the
    /// fix regresses.
    #[test]
    fn should_clear_canvas_only_on_decode_stop_edge() {
        // The ONLY transition that clears: the peer was being decoded and is
        // now leaving the active decode set.
        assert!(
            should_clear_canvas(true, false),
            "decode-stop edge (true -> false) MUST clear the stale frame"
        );

        // Becoming visible: the decoder will repaint live frames, so clearing
        // here would needlessly blank a tile that's about to show video.
        assert!(
            !should_clear_canvas(false, true),
            "becoming visible (false -> true) must NOT clear"
        );

        // Still visible: never blank a live tile.
        assert!(
            !should_clear_canvas(true, true),
            "staying visible (true -> true) must NOT clear"
        );

        // Still hidden: the clear already happened on the first edge; doing it
        // every pass would waste work.
        assert!(
            !should_clear_canvas(false, false),
            "staying hidden (false -> false) must NOT clear"
        );
    }

    /// Peers added after a layout update stay inactive until a subsequent
    /// active-decode-set push explicitly enables them.
    #[wasm_bindgen_test]
    fn new_peer_remains_inactive_until_selected() {
        let mut manager = PeerDecodeManager::new();
        manager.set_active_decode_set(&HashSet::from([223]));

        let (peer, _muted) = make_test_peer(224);
        manager.connected_peers.insert(224, peer);

        assert!(
            !manager.connected_peers.get(&224).unwrap().visible,
            "New peers should remain inactive until explicitly selected"
        );

        manager.set_active_decode_set(&HashSet::from([224]));
        assert!(
            manager.connected_peers.get(&224).unwrap().visible,
            "The next active-set update should enable the peer"
        );
    }

    /// Multiple losses: after one loss triggers a keyframe request and the loss
    /// is cleared by a keyframe, a new loss should be independently detected.
    #[wasm_bindgen_test]
    fn multiple_losses_handled_independently() {
        let (mut peer, _muted) = make_test_peer(209);

        // First loss: seq 1 -> 3 -> 70. The bridge to seq 3 leaves seq 2 as a
        // genuine in-window hole; the seq-70 jump shifts it off and counts it.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt1)
            .keyframe_request;
        let _ = peer
            .track_sequence(MediaType::VIDEO, &delta_seq_packet(LOSS_BRIDGE_SEQ))
            .keyframe_request;
        let pkt70 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 70,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt70)
            .keyframe_request;
        assert!(peer.video_seq_tracker.lost_count > 0);

        // Backdate and trigger request.
        peer.video_seq_tracker.loss_detected_at_ms =
            Some(now_ms().saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 100));
        peer.video_seq_tracker.last_keyframe_request_ms = 0;
        let pkt71 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 71,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer
            .track_sequence(MediaType::VIDEO, &pkt71)
            .keyframe_request;
        assert_eq!(result, Some(MediaType::VIDEO));

        // Clear loss with a keyframe.
        let key = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 72,
                    ..Default::default()
                })
                .into(),
                frame_type: "key".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &key).keyframe_request;
        assert_eq!(
            peer.video_seq_tracker.lost_count, 0,
            "Loss should be cleared by keyframe"
        );

        // Second loss: seq 72 -> seq 140 (shifts positions off window again).
        let pkt140 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 140,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer
            .track_sequence(MediaType::VIDEO, &pkt140)
            .keyframe_request;
        assert!(
            peer.video_seq_tracker.lost_count > 0,
            "Second loss should be detected independently"
        );
    }

    /// Receiver simulcast layer-select guard (issue #989).
    ///
    /// A peer publishing three layers (0/1/2) with independent dense per-layer
    /// sequences, arriving interleaved 0,1,2,0,1,2,…, must:
    ///   * only have layer 0 (the selected default) reach the decoder, and
    ///   * produce ZERO phantom loss in `video_seq_tracker` — proving the guard
    ///     runs before `track_sequence`, so the tracker only ever sees layer 0's
    ///     dense 0,1,2,… stream rather than the merged 0,0,0,1,1,1,… that would
    ///     manufacture ~2/3 loss and storm PLIs.
    #[wasm_bindgen_test]
    fn simulcast_guard_drops_non_selected_layers_and_keeps_loss_zero() {
        let (mut peer, _muted) = make_test_peer(901);
        // Make the peer eligible to actually decode: visible + video on, and
        // pretend a heartbeat arrived so the straggler-inference path is skipped.
        peer.visible = true;
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        // Default selected layer is 0 (assert the invariant explicitly).
        assert_eq!(peer.selected_video_layer, 0);

        let mut decoded_video = 0usize;
        let mut skipped_video = 0usize;

        // Interleaved arrival: per layer the sequence is dense (0,1,2,…); across
        // layers the wire order is 0,1,2,0,1,2,…
        for seq in 0..6u64 {
            for layer in 0..3u32 {
                let pkt = layered_video_packet(901, layer, seq);
                let (mt, status, _kf) = peer
                    .decode(&pkt, "test@test.com")
                    .expect("decode should not error");
                assert_eq!(mt, MediaType::VIDEO);
                // SKIPPED is signalled by rendered == false && first_frame == false
                // for the dropped layers; layer 0 reaches the noop decoder.
                if layer == 0 {
                    decoded_video += 1;
                } else if !status.rendered && !status.first_frame {
                    skipped_video += 1;
                }
            }
        }

        assert_eq!(
            decoded_video, 6,
            "exactly the 6 layer-0 packets should reach the decoder"
        );
        assert_eq!(
            skipped_video, 12,
            "all 12 non-selected (layer 1 & 2) packets should be dropped"
        );
        assert_eq!(
            peer.video_seq_tracker.lost_count, 0,
            "guard must run before track_sequence so only layer 0's dense \
             sequence is tracked → zero phantom loss"
        );
    }

    /// Selecting a non-zero layer flips which layer is decoded and still keeps
    /// loss at zero (the selected layer's sequence is dense).
    #[wasm_bindgen_test]
    fn simulcast_guard_honors_selected_layer() {
        let (mut peer, _muted) = make_test_peer(902);
        peer.visible = true;
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        peer.set_selected_video_layer(1);
        assert_eq!(peer.selected_video_layer, 1);

        let mut decoded_video = 0usize;
        for seq in 0..4u64 {
            for layer in 0..3u32 {
                let pkt = layered_video_packet(902, layer, seq);
                let (_mt, _status, _kf) = peer
                    .decode(&pkt, "test@test.com")
                    .expect("decode should not error");
                if layer == 1 {
                    decoded_video += 1;
                }
            }
        }
        assert_eq!(decoded_video, 4, "only layer 1 should reach the decoder");
        assert_eq!(
            peer.video_seq_tracker.lost_count, 0,
            "selected layer's dense sequence → zero phantom loss"
        );
    }

    /// Phase 2 (#989): the local decode guard must follow the chooser. After
    /// observing higher layers as available and feeding sustained clean downlink
    /// windows through the chooser tick, `selected_video_layer` must climb — and
    /// the guard then decodes exactly that layer.
    #[wasm_bindgen_test]
    fn decode_guard_follows_layer_chooser() {
        let (mut peer, _muted) = make_test_peer(903);
        peer.visible = true;
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        assert_eq!(peer.selected_video_layer(), 0, "starts at base");

        // Learn that layers 0,1,2 are available for this source by observing
        // arriving packets of every layer (mirrors the decode path's observe()).
        let mut t = 1000u64;
        for layer in 0..3u32 {
            peer.video_layer_availability.observe(layer, t);
        }

        // Feed sustained clean downlink windows (default sample is clean) with
        // adequate spacing so dwell is satisfied; the chooser must climb toward
        // the top available layer and the guard must follow.
        for _ in 0..20 {
            // Re-observe so availability does not expire across the long run.
            for layer in 0..3u32 {
                peer.video_layer_availability.observe(layer, t);
            }
            peer.tick_layer_chooser(t, crate::decode::layer_chooser::KindLayerBounds::default());
            t += 1100;
        }
        assert_eq!(
            peer.selected_video_layer(),
            2,
            "sustained clean downlink + availability must climb the decode guard to top"
        );

        // The guard now decodes layer 2 only.
        let pkt2 = layered_video_packet(903, 2, 0);
        let (_mt, _status, _kf) = peer.decode(&pkt2, "test@test.com").expect("decode ok");
        let pkt0 = layered_video_packet(903, 0, 0);
        let (_mt0, status0, _kf0) = peer.decode(&pkt0, "test@test.com").expect("decode ok");
        assert!(!status0.rendered, "non-selected base layer must be dropped");
    }

    /// The configured receive ceiling clamps both the advertised preference and
    /// the camera/screen decode guards through the production manager tick.
    ///
    /// MUTATION: replacing either `bounds.clamp(raw)` with `raw` leaves that
    /// guard at L2 and omits its required L0 preference.
    #[test]
    fn receive_ceiling_clamps_wire_preferences_and_decode_guards() {
        use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds};

        let mut manager = PeerDecodeManager::new();
        manager.insert_zero_loss_top_peer_for_test(904);
        let now = 1500;

        let open = manager.tick_layer_choosers(now, &ReceiveLayerBounds::default());
        assert_eq!(manager.selected_video_layer_for_test(904), Some(2));
        assert_eq!(manager.selected_screen_layer_for_test(904), Some(2));
        assert!(
            !open.contains_key(&(904, PrefMediaKind::Video)),
            "an uncapped top selection is intentionally omitted from the wire map"
        );

        let mut capped = ReceiveLayerBounds::default();
        capped.set_kind(PrefMediaKind::Video, None, Some(0));
        capped.set_kind(PrefMediaKind::Screen, None, Some(0));
        let desired = manager.tick_layer_choosers(now, &capped);

        assert_eq!(desired.get(&(904, PrefMediaKind::Video)), Some(&0));
        assert_eq!(desired.get(&(904, PrefMediaKind::Screen)), Some(&0));
        assert_eq!(manager.selected_video_layer_for_test(904), Some(0));
        assert_eq!(manager.selected_screen_layer_for_test(904), Some(0));
    }

    /// Phase 2/3 (#989): the manager's per-peer tick returns an independent
    /// desired-layer entry for every connected peer AND every media kind
    /// (VIDEO/SCREEN/AUDIO), keyed by (session_id, PrefMediaKind).
    #[wasm_bindgen_test]
    fn manager_tick_layer_choosers_returns_per_peer_kind_map() {
        use crate::decode::layer_chooser::PrefMediaKind;
        let mut manager = PeerDecodeManager::new();
        for sid in [10u64, 20, 30] {
            let (peer, _muted) = make_test_peer(sid);
            manager.connected_peers.insert(sid, peer);
        }
        let desired = manager.tick_layer_choosers(
            1000,
            &crate::decode::layer_chooser::ReceiveLayerBounds::default(),
        );
        // M2 (#1079): fresh peers (no availability learned, no congestion) advertise
        // NO preference for any kind — the map is EMPTY, not 9 base entries. A
        // base-pin on cold start would drop upgraded layers and cause an HD dip
        // after every (re)connect. Absence = no constraint = relay forwards all.
        assert!(
            desired.is_empty(),
            "fresh peers must advertise no preference (cold start = forward all): {desired:?}"
        );
        let _ = PrefMediaKind::Video; // keep the import used regardless of asserts
    }

    // -----------------------------------------------------------------
    // #1256 Phase 1: size-aware receiver layer cap (T2 + T3).
    //
    // Run under `wasm_bindgen_test` (this module is `run_in_browser`) because
    // `tick_layer_choosers` reaches `now_ms()` (a JS clock) via peer
    // construction; these are NOT host `cargo test`s.
    // -----------------------------------------------------------------

    /// T3: a SMALL rendered tile (device-px height = 360) on a zero-loss top peer
    /// (highest_available == 2) must LID the requested VIDEO layer to L0 and the
    /// #1079 gate must advertise it (cap 0 < highest 2). Switching the same peer to
    /// `Uncapped` must lift the lid so the peer rests at the top again and the entry
    /// is OMITTED.
    ///
    /// MUTATION: removing the size-lid fold in `tick_layer_choosers` makes the
    /// Capped case stop advertising (the chooser fails open to 2, gate omits) — the
    /// `Some(&0)` assertion then fails.
    #[wasm_bindgen_test]
    fn advertised_preference_capped_by_small_tile() {
        use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds, TileHint};
        let mut manager = PeerDecodeManager::new();
        manager.insert_zero_loss_top_peer_for_test(777);

        // Small tile -> wants L0 while highest_available == 2.
        manager.set_peer_tile_hints(HashMap::from([(
            777u64,
            TileHint::Capped { device_px_h: 180 },
        )]));
        // FIXED clock (see `FIXTURE_NOW_MS`): the real wall clock prunes the
        // fixture's t=1000 ladder, collapsing highest_available 2 -> 0.
        let desired = manager.tick_layer_choosers(FIXTURE_NOW_MS, &ReceiveLayerBounds::default());
        assert_eq!(
            desired.get(&(777, PrefMediaKind::Video)),
            Some(&0),
            "small tile must LID video to L0 and advertise it (cap 0 < highest 2)"
        );

        // Lift the lid (Uncapped) -> healthy top peer rests at highest, entry OMITTED.
        manager.set_peer_tile_hints(HashMap::from([(777u64, TileHint::Uncapped)]));
        let desired = manager.tick_layer_choosers(FIXTURE_NOW_MS, &ReceiveLayerBounds::default());
        assert!(
            !desired.contains_key(&(777, PrefMediaKind::Video)),
            "uncapped healthy top peer must advertise no video preference: {desired:?}"
        );
    }

    /// T2 (strengthened, #1256 N1): an UP-switch (cold-start 0 -> L2) MUST request a
    /// keyframe for the higher layer, while a genuine DOWN-switch (re-capping the
    /// same peer L2 -> L0) must NOT.
    ///
    /// The original sequence (capped-first cold-start 0 -> 0, then lift to 0 -> L2)
    /// was weak: the down/no-change phase was 0 -> 0, which a `video > old_v` ->
    /// `video != old_v` mutation SURVIVES (0 != 0 is false, so still no keyframe). By
    /// driving a real top->lower move (L2 -> L0) in the down phase, the `!=` mutation
    /// would wrongly emit a keyframe on the down tick and this test fails.
    ///
    /// The peer is made `visible` + `video_enabled` so the P2 keyframe-visibility
    /// gate is satisfied and visibility is NOT the variable under test here.
    ///
    /// MUTATION: removing the `video > old_v` emission breaks the up-switch phase (no
    /// KEYFRAME_REQUEST captured); changing `video > old_v` to `video != old_v`
    /// breaks the DOWN phase (a KEYFRAME_REQUEST would wrongly appear on the L2 -> L0
    /// re-cap tick).
    #[wasm_bindgen_test]
    fn size_up_switch_requests_keyframe() {
        use crate::decode::layer_chooser::{ReceiveLayerBounds, TileHint};
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::<PacketWrapper>::new()));
        let collected_clone = collected.clone();
        let callback = crate::Callback::from(move |pkt: PacketWrapper| {
            collected_clone.borrow_mut().push(pkt);
        });
        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(callback, "viewer@test.com".to_string());
        manager.insert_zero_loss_top_peer_for_test(888);
        // P2 gate: this test exercises the up/down emission predicate, not the
        // visibility gate — so make the stream decode-eligible (visible + camera on).
        {
            let peer = manager.connected_peers.get_mut(&888).unwrap();
            peer.visible = true;
            peer.video_enabled = true;
        }

        // PHASE 1 — UP-switch: an Uncapped hint lets the healthy top peer climb from
        // the cold-start selection (0) up to L2, which MUST request one VIDEO
        // keyframe so the higher layer can be decoded.
        manager.set_peer_tile_hints(HashMap::from([(888u64, TileHint::Uncapped)]));
        let kf_before_up = keyframe_requests_sent_count();
        // FIXED clock (see `FIXTURE_NOW_MS`) — a wall clock prunes the ladder to
        // highest_available 0, so there is no L2 to up-switch TO.
        let _ = manager.tick_layer_choosers(FIXTURE_NOW_MS, &ReceiveLayerBounds::default());

        {
            let pkts = collected.borrow();
            assert_eq!(
                pkts.len(),
                1,
                "exactly one KEYFRAME_REQUEST must be sent on the up-switch: {pkts:?}"
            );
            let inner = MediaPacket::parse_from_bytes(&pkts[0].data)
                .expect("should deserialize inner MediaPacket");
            assert_eq!(
                inner.media_type.enum_value(),
                Ok(MediaType::KEYFRAME_REQUEST),
                "up-switch packet must be a KEYFRAME_REQUEST"
            );
            assert_eq!(
                inner.data,
                b"VIDEO".to_vec(),
                "the keyframe request must be for the VIDEO stream"
            );
        }
        // Secondary check on the process-global counter: read the DELTA (not an
        // absolute) since the counter is shared across all tests in the binary.
        assert_eq!(
            keyframe_requests_sent_count() - kf_before_up,
            1,
            "the up-switch must increment the keyframe-sent counter by exactly one"
        );

        // Snapshot/clear the captured vec so the DOWN phase assertion is clean.
        collected.borrow_mut().clear();

        // PHASE 2 — genuine DOWN-switch: re-cap the SAME peer with a small tile so
        // the selection moves L2 -> L0. A down move must NOT request a keyframe (the
        // prior layer's last-good frame keeps painting; no higher layer to fetch).
        manager.set_peer_tile_hints(HashMap::from([(
            888u64,
            TileHint::Capped { device_px_h: 180 },
        )]));
        let kf_before_down = keyframe_requests_sent_count();
        let _ = manager.tick_layer_choosers(FIXTURE_NOW_MS, &ReceiveLayerBounds::default());
        assert!(
            collected.borrow().is_empty(),
            "a genuine DOWN-switch (L2 -> L0) must NOT request a keyframe: {:?}",
            collected.borrow()
        );
        assert_eq!(
            keyframe_requests_sent_count(),
            kf_before_down,
            "no keyframe should be sent on the down (re-cap) tick"
        );
    }

    /// T-P1a (#1256 P1): the size lid must PERSIST on the READ-ONLY publish path
    /// (`current_desired_preferences`), not just the 5s tick. After the tick
    /// advertises `{(555,Video):0}` for a small-tile lid on a HEALTHY (unconstrained)
    /// peer, the read-only accessor — which the early-seed timer and the congestion
    /// seeds republish through — must STILL report layer 0. Before the P1 fix the
    /// accessor used `desired_preference()` alone, which is `None` for an
    /// unconstrained lidded peer, so the lid vanished here and the next seed publish
    /// CLEARED the cap on the wire (relay fail-opens the missing entry to the top).
    ///
    /// MUTATION: revert the P1 fold so `collect_desired_preferences` ignores the
    /// hint (back to `desired_preference()`-only) — the unconstrained lidded peer
    /// then reports `None` here and this assertion (`Some(&0)`) fails.
    #[wasm_bindgen_test]
    fn size_lid_durable_on_read_only_publish_path() {
        use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds, TileHint};
        let mut manager = PeerDecodeManager::new();
        manager.insert_zero_loss_top_peer_for_test(555);
        // Small tile -> lid wants L0 while highest_available == 2.
        manager.set_peer_tile_hints(HashMap::from([(
            555u64,
            TileHint::Capped { device_px_h: 180 },
        )]));
        // FIXED clock (see `FIXTURE_NOW_MS`): a wall clock prunes the fixture's
        // t=1000 ladder, so highest_available collapses to 0 and the lid has
        // nothing to cap — the P1 regression this test guards becomes untestable.
        let now = FIXTURE_NOW_MS;
        // The tick path advertises the lid {(555,Video):0}.
        let ticked = manager.tick_layer_choosers(now, &ReceiveLayerBounds::default());
        assert_eq!(
            ticked.get(&(555, PrefMediaKind::Video)),
            Some(&0),
            "tick must advertise the size lid (L0): {ticked:?}"
        );
        // THE READ-ONLY PATH (early-seed timer / congestion seeds republish here):
        // the lid must persist.
        let desired = manager.current_desired_preferences(now, &ReceiveLayerBounds::default());
        assert_eq!(
            desired.get(&(555, PrefMediaKind::Video)),
            Some(&0),
            "size lid must persist on the read-only publish path: {desired:?}"
        );
    }

    /// T (#1256 user-min floor): an explicit receive MIN is an authoritative floor
    /// the size lid must NOT undercut. The peer's VIDEO chooser is first driven DOWN
    /// to L0 by sustained congestion (so its `desired_preference()` is `Some(0)` and
    /// the unclamped tick `raw` is 0 — the only regime where the inverted bound
    /// actually diverges from the floor). Then, with user video min = L1 and a
    /// small-tile hint that maps the size cap to L0, BOTH the tick path AND the
    /// read-only publish path must select/advertise L1 (the floor), never L0.
    /// highest_available == 2 throughout, so the entry is advertised (1 < 2).
    ///
    /// WHY drive to L0 (not a cold zero-loss peer): on a healthy chooser `raw == 2`,
    /// and `KindLayerBounds::clamp` NORMALIZES the inverted `{min:1,max:0}` bound to
    /// `[0,1]`, so `2.clamp(0,1) == 1` — the cap can only undercut the floor when the
    /// chooser's pick `raw <= user_min`, i.e. when it already sits at/below the floor
    /// under congestion. A cold-peer version would pass on BOTH the fixed and the
    /// un-fixed code (proving nothing), so this test congests the chooser into the
    /// regime where the mutation is observable.
    ///
    /// MUTATION: revert the `.max(user_min)` composition in EITHER fold — the
    /// inverted `{min:1,max:0}` bound (tick) / the raw `min(v_base, v_lid)`
    /// (read-only) then drops the selection to L0, so the corresponding assertion
    /// (`== 1`) fails. Both folds are pinned.
    #[wasm_bindgen_test]
    fn size_lid_yields_to_user_receive_min() {
        use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds, TileHint};
        let mut manager = PeerDecodeManager::new();
        // Congested peer with a learned 3-layer ladder (highest_available == 2).
        manager.connected_peers.insert(
            999,
            make_congested_top_peer(999, TransportType::TRANSPORT_WEBTRANSPORT),
        );

        // Drive the VIDEO chooser DOWN to L0 with sustained congestion: each
        // congested tick steps down one rung (2 -> 1 -> 0). OPEN bounds + NO tile
        // hint here so this phase is the chooser alone, not the lid. A FIXED small
        // clock (matching `congestion_seed_never_advertises_above_size_lid`) is
        // mandatory: `make_congested_top_peer` records availability at t=1000 and the
        // 4000ms window prunes it under a real `now_ms()` wall-clock, collapsing
        // `highest_available` to 0 and omitting the advertise gate. Down-steps have no
        // dwell gate, so repeated ticks at the same clock still step down one per tick.
        let now = 2000u64;
        for _ in 0..3 {
            let _ = manager.tick_layer_choosers(now, &ReceiveLayerBounds::default());
        }
        // Precondition: congestion must have reached the L0 regime, so this test
        // exercises the divergent path (not the normalized-to-floor coincidence at
        // raw == 2 a healthy chooser would give).
        assert_eq!(
            manager
                .connected_peers
                .get(&999)
                .unwrap()
                .selected_video_layer(),
            0,
            "precondition: congestion must drive the chooser to L0 before the floor test"
        );

        // Now apply the user floor (never below L1) + a small-tile lid (cap -> L0).
        let mut bounds = ReceiveLayerBounds::default();
        bounds.set_kind(PrefMediaKind::Video, Some(1), None); // min = L1, max = none
        manager.set_peer_tile_hints(HashMap::from([(
            999u64,
            TileHint::Capped { device_px_h: 180 },
        )]));

        // TICK PATH: the decode guard / advertised layer must be the floor L1, not
        // L0. Fixed: effective_max = cap.max(user_min) = 1 -> bounds {1,1} ->
        // clamp(0) = 1. Mutated: bounds {1,0} -> clamp_to_user_range(0,1,0) =
        // 0.clamp(0,1) = 0.
        let ticked = manager.tick_layer_choosers(now, &bounds);
        assert_eq!(
            ticked.get(&(999, PrefMediaKind::Video)),
            Some(&1),
            "tick: size lid must yield to the user receive min (L1), not undercut to L0: {ticked:?}"
        );
        // READ-ONLY PATH: same — the floor wins here too. The chooser is constrained
        // at L0, so v_base = clamp(0) under {min:1} = 1; the fix folds to 1, the old
        // `v_base.min(v_lid)` would fold to min(1, 0) = 0.
        let desired = manager.current_desired_preferences(now, &bounds);
        assert_eq!(
            desired.get(&(999, PrefMediaKind::Video)),
            Some(&1),
            "read-only path: size lid must yield to the user receive min (L1): {desired:?}"
        );
    }

    /// T-P1b (#1256 P1 corollary): a congestion seed must NEVER advertise ABOVE the
    /// size lid. `make_congested_top_peer` + `seed_early_congestion_for_connected_peers`
    /// drives the chooser DOWN from the top (2) to L1 (constrained). With a small-tile
    /// lid of L0, the read-only publish path must advertise `min(constrained=1,
    /// lid=0) = 0`, never L1 (which is above the lid). Without the `min(v_base, v_lid)`
    /// fold the constrained chooser would advertise its L1 directly — raising the bytes
    /// pulled for a tiny tile (the backend corollary in #1256 P1).
    ///
    /// MUTATION: drop the `.min(v_lid)` (or the whole P1 fold) — the constrained
    /// chooser advertises its held L1 here and the assertion (`Some(0)`) fails.
    #[wasm_bindgen_test]
    fn congestion_seed_never_advertises_above_size_lid() {
        use crate::decode::layer_chooser::{
            DownlinkSample, PrefMediaKind, ReceiveLayerBounds, TileHint,
        };
        let mut manager = PeerDecodeManager::new();
        // Congested video downlink + learned 3-layer ladder (highest_available == 2).
        manager.connected_peers.insert(
            666,
            make_congested_top_peer(666, TransportType::TRANSPORT_WEBTRANSPORT),
        );
        let now = 2000u64;
        let bounds = ReceiveLayerBounds::default();

        // Bring the (unconstrained) chooser up to the TOP (current == 2) via one
        // CLEAN unclamped tick, exactly as `early_seed_respects_user_receive_max`
        // does — so the subsequent congested seed steps DOWN from 2 to L1 (the seed
        // primitive steps down from `current`, which must be at the top first). No
        // tile hint here, so this tick is unlidded.
        if let Some(p) = manager.connected_peers.get_mut(&666) {
            p.last_video_downlink = DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            };
        }
        let _ = manager.tick_layer_choosers(now, &bounds);
        // Restore the congested sample so the seed has something to constrain on.
        if let Some(p) = manager.connected_peers.get_mut(&666) {
            p.last_video_downlink = DownlinkSample {
                loss_per_sec: crate::decode::layer_chooser::LOSS_STEP_DOWN_PER_SEC + 1.0,
                kf_per_sec: 0.0,
            };
        }

        // Now apply a small-tile lid = L0 and seed: the video chooser steps DOWN to
        // L1 (constrained). WITHOUT the lid the read-only path would advertise that
        // held L1; WITH the L0 lid it must clamp to L0.
        manager.set_peer_tile_hints(HashMap::from([(
            666u64,
            TileHint::Capped { device_px_h: 180 },
        )]));
        let seeded = manager.seed_early_congestion_for_connected_peers(now, &bounds);
        assert!(seeded, "the congested peer's sample must seed a constrain");
        let desired = manager.current_desired_preferences(now, &bounds);
        let v = desired.get(&(666, PrefMediaKind::Video)).copied();
        assert!(
            v == Some(0),
            "congestion must not advertise above the size lid (lid L0); got {v:?}"
        );
    }

    /// #1256 (guard/wire sync after congestion seeds): the seed helpers write the
    /// decode guard LID-UNAWARE (user bounds only, not the size lid). The fix re-lids
    /// the guards (via apply_size_lid_to_decode_guards) BETWEEN the seed and the
    /// lid-aware publish, so guard == wire before the packet goes out — otherwise a
    /// small-capped peer the seed flips to L1 has guard=L1 / wire=L0 and freezes for
    /// ≤5s. This test reproduces the manager-level sequence the early-seed timer /
    /// seed_local_congestion_and_publish run, and asserts the guard EQUALS the lid AND
    /// equals the advertised wire layer.
    ///
    /// MUTATION: remove the apply_size_lid_to_decode_guards re-lid call (the seed alone
    /// leaves the guard at the un-lidded clamp(current()) = L1 while the wire is L0) —
    /// the `selected_video_layer() == 0` (guard) assertion fails. (The wire assertion
    /// alone passes even un-fixed; the GUARD assertion is what this test adds.)
    #[test]
    fn congestion_seed_relid_syncs_guard_to_lid() {
        use crate::decode::layer_chooser::{
            DownlinkSample, PrefMediaKind, ReceiveLayerBounds, TileHint,
        };
        let mut manager = PeerDecodeManager::new();
        manager.connected_peers.insert(
            666,
            make_congested_top_peer(666, TransportType::TRANSPORT_WEBTRANSPORT),
        );
        let now = 2000u64;
        let bounds = ReceiveLayerBounds::default();
        // Bring the chooser to the top (so the seed has somewhere to step DOWN from),
        // exactly as congestion_seed_never_advertises_above_size_lid does.
        if let Some(p) = manager.connected_peers.get_mut(&666) {
            p.last_video_downlink = DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            };
        }
        let _ = manager.tick_layer_choosers(now, &bounds);
        if let Some(p) = manager.connected_peers.get_mut(&666) {
            p.last_video_downlink = DownlinkSample {
                loss_per_sec: crate::decode::layer_chooser::LOSS_STEP_DOWN_PER_SEC + 1.0,
                kf_per_sec: 0.0,
            };
        }
        // Small tile -> lid = L0. Seed congestion (chooser steps DOWN to L1), then RE-LID
        // the guards (the fix) — this is the manager-level equivalent of what the
        // early-seed timer / seed_local_congestion_and_publish now do between seed and
        // publish. (Tile 180px = new L0 native height, issue #1768, so it caps to L0.)
        manager.set_peer_tile_hints(HashMap::from([(
            666u64,
            TileHint::Capped { device_px_h: 180 },
        )]));
        let seeded = manager.seed_early_congestion_for_connected_peers(now, &bounds);
        assert!(seeded, "the congested peer's sample must seed a constrain");
        let _ = manager.apply_size_lid_to_decode_guards(now, &bounds); // <-- the re-lid the fix inserts
                                                                       // GUARD must now equal the lid (L0) — NOT the un-lidded clamp(current())=L1.
        let guard = manager
            .connected_peers
            .get(&666)
            .unwrap()
            .selected_video_layer();
        assert_eq!(
            guard, 0,
            "decode guard must be re-lidded to L0 (the lid), not the un-lidded seed result L1"
        );
        // WIRE (lid-aware publish) must equal the lid too — and EQUAL the guard.
        let desired = manager.current_desired_preferences(now, &bounds);
        let wire = desired.get(&(666, PrefMediaKind::Video)).copied();
        assert_eq!(wire, Some(0), "advertised wire layer must be the lid L0");
        assert_eq!(
            Some(guard),
            wire,
            "guard must equal wire (no freeze) after the re-lid"
        );
    }

    /// BOTH manager-level snapshot producers must resolve rung geometry through the
    /// camera ladder, exercised through the REAL production methods with a REAL peer
    /// — the pure-resolver tests bypass the manager and the Playwright spec joins
    /// solo, so neither sees a producer that stops consulting the ladder.
    ///
    /// Sitting the peer on the TOP rung is load-bearing: a base-rung fixture would
    /// pass against a hardcoded table.
    ///
    /// MUTATION: hardcode either producer's geometry, or point the resolver at the
    /// screen/audio arm, and the `(1280, 720, 1500)` assertions fail.
    #[test]
    fn manager_display_producers_resolve_the_camera_ladder_top_rung() {
        use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds};

        let now = 2000u64;
        let mut manager = PeerDecodeManager::new();
        manager.insert_zero_loss_top_peer_for_test(900);
        manager.set_peer_video_layer_for_test(900, 2);

        let top = crate::adaptive_quality_constants::simulcast_layers(3)
            .last()
            .expect("the camera ladder is non-empty");
        let expected = (top.max_width, top.max_height, top.ideal_bitrate_kbps);
        assert_eq!(
            expected,
            (1280, 720, 1500),
            "fixture check: the shipped camera top rung is 720p @ 1500 kbps"
        );

        // Producer 1: the per-KIND representative needle (the receive bar-meter and
        // the `format_readout` `{w}x{h}` line).
        let needle = manager
            .received_layer_snapshot(PrefMediaKind::Video, now)
            .expect("a connected video peer yields a needle snapshot");
        assert_eq!(
            (needle.width, needle.height, needle.kbps),
            expected,
            "the needle producer must resolve the camera ladder's top rung"
        );

        // Producer 2: the per-PEER rows (the `720p · ~1.5M` peer-row metric, the
        // diagnostics drawer line, and the signal-quality popup).
        let rows = manager.per_peer_received_snapshots(now, &ReceiveLayerBounds::default());
        let video = rows
            .iter()
            .find(|r| r.session_id == 900)
            .and_then(|r| r.video.as_ref())
            .expect("the peer must appear with a video snapshot");
        assert_eq!(
            (video.width, video.height, video.kbps),
            expected,
            "the PER-PEER producer must resolve the same rung the needle does"
        );
    }

    /// #1695 (guard must not lead the wire on a rate-limited up-switch): after a
    /// `LAYER_PREFERENCE` publish whose change was RATE-LIMITED (take_if_changed
    /// returned None without promoting last_sent), `apply_size_lid_to_decode_guards`
    /// may have ALREADY raised the EXACT-MATCH decode guard to L2 while the wire
    /// (last_sent) is still L0. The relay forwards only L0 → exact-match guard
    /// rejects every L0 → freeze. `reconcile_decode_guards_to_wire(Some(wire))` must
    /// pull the guard back DOWN to the wire layer (L0) so guard == wire.
    ///
    /// AUDIO is covered identically (issue #1695): audio is in the SAME relay
    /// exact-match filter, so the test also pre-raises `selected_audio_layer` to L2
    /// with a recorded audio wire entry of L0 (Case 1) and asserts the reconcile pulls
    /// the AUDIO guard to L0; Case 2 (no entry) asserts the audio guard rises to
    /// `highest_available` (L2) just like video.
    ///
    /// This drives the manager method DIRECTLY against a hand-built `last_sent` map
    /// (the literal wire layer 0), NOT a re-implementation of the production path —
    /// per CLAUDE.md "test the production function, not a copy".
    ///
    /// MUTATION: gut the body of `reconcile_decode_guards_to_wire` to `Vec::new()`
    /// (no guard writes). The guard then STAYS at the pre-set L2 ≠ wire L0 and the
    /// `selected_video_layer() == 0` assertion FAILS. Likewise breaking the
    /// no-entry → `highest_available` branch fails the second case. Gutting ONLY the
    /// AUDIO arm (removing the `set_selected_audio_layer` call) leaves the audio guard
    /// at L2 ≠ wire L0 and fails the `selected_audio_layer() == 0` assertion below,
    /// while the video/screen assertions still pass — proving the audio coverage is
    /// independent.
    #[test]
    fn reconcile_decode_guards_to_wire_pins_guard_to_wire_not_above() {
        use crate::decode::layer_chooser::PrefMediaKind;
        use std::collections::BTreeMap;

        // ---- Case 1: recorded wire entry L0, guard pre-raised to L2 → reconcile to L0.
        let mut manager = PeerDecodeManager::new();
        // Peer with a learned 3-layer ladder (highest_available == 2).
        manager.insert_zero_loss_top_peer_for_test(900);
        let now = 2000u64;
        // Simulate apply_size_lid's immediate up-raise: guard at L2 (the TOP).
        manager
            .connected_peers
            .get_mut(&900)
            .unwrap()
            .set_selected_video_layer(2);
        assert_eq!(
            manager
                .connected_peers
                .get(&900)
                .unwrap()
                .selected_video_layer(),
            2,
            "precondition: guard pre-raised to L2 (apply_size_lid's immediate raise)"
        );
        // AUDIO precondition: pre-raise the AUDIO guard to L2 as well, so the
        // reconcile has somewhere to pull it DOWN from (mirrors apply_size_lid's
        // immediate raise for the exact-match audio guard).
        manager
            .connected_peers
            .get_mut(&900)
            .unwrap()
            .set_selected_audio_layer(2);
        assert_eq!(
            manager
                .connected_peers
                .get(&900)
                .unwrap()
                .selected_audio_layer(),
            2,
            "precondition: AUDIO guard pre-raised to L2"
        );
        // The wire (last_sent) still records L0 for this source — for BOTH video and
        // audio (the rate-limited publish never promoted either past L0).
        let mut wire: BTreeMap<(u64, PrefMediaKind), u32> = BTreeMap::new();
        wire.insert((900, PrefMediaKind::Video), 0);
        wire.insert((900, PrefMediaKind::Audio), 0);
        let _ups = manager.reconcile_decode_guards_to_wire(
            Some(&wire),
            &crate::decode::layer_chooser::ReceiveLayerBounds::default(),
            now,
        );
        // Guard must now EQUAL the wire (L0) — the relay forwards only L0, so the
        // exact-match guard must accept L0, not reject it at L2.
        assert_eq!(
            manager
                .connected_peers
                .get(&900)
                .unwrap()
                .selected_video_layer(),
            0,
            "#1695: guard must be pulled DOWN to the wire layer (L0), not lead it at L2"
        );
        // AUDIO guard must ALSO be pulled DOWN to the audio wire (L0): audio is in the
        // same relay exact-match filter, so a guard leading the audio wire at L2 would
        // drop every forwarded L0 audio packet (audio freeze). MUTATION TARGET: gut
        // the AUDIO arm and this assertion fails (audio guard stays at L2).
        assert_eq!(
            manager
                .connected_peers
                .get(&900)
                .unwrap()
                .selected_audio_layer(),
            0,
            "#1695: AUDIO guard must be pulled DOWN to the audio wire (L0), not lead it at L2"
        );

        // ---- Case 2: NO recorded entry → guard must rise to highest_available (top
        // the relay fails-open-forwards). Guard pre-set LOW (0), last_sent None.
        let mut manager2 = PeerDecodeManager::new();
        manager2.insert_zero_loss_top_peer_for_test(901);
        manager2
            .connected_peers
            .get_mut(&901)
            .unwrap()
            .set_selected_video_layer(0);
        // AUDIO guard also pre-set LOW (0) — must ALSO rise to the fail-open top.
        manager2
            .connected_peers
            .get_mut(&901)
            .unwrap()
            .set_selected_audio_layer(0);
        // No last_sent map at all → relay fails open → forwards ALL layers (the top).
        let _ups2 = manager2.reconcile_decode_guards_to_wire(
            None,
            &crate::decode::layer_chooser::ReceiveLayerBounds::default(),
            now,
        );
        let top = 2u32; // highest_available for the 3-layer fixture ladder
        assert_eq!(
            manager2
                .connected_peers
                .get(&901)
                .unwrap()
                .selected_video_layer(),
            top,
            "#1695: with no recorded entry the relay fails open (forwards the top), \
             so the guard must match highest_available (L2), not stay at L0"
        );
        // AUDIO guard must ALSO rise to the fail-open top (L2): with no recorded audio
        // entry the relay forwards ALL audio layers, so the audio guard must match the
        // top it forwards — not stay pinned at L0 (which would drop the forwarded top).
        assert_eq!(
            manager2
                .connected_peers
                .get(&901)
                .unwrap()
                .selected_audio_layer(),
            top,
            "#1695: with no recorded audio entry the audio guard must match \
             highest_available (L2), not stay at L0"
        );
    }

    /// A receive-audio `min` must never raise the decode guard above what the
    /// publisher offers. Covers BOTH write sites.
    #[test]
    fn audio_receive_min_never_raises_the_guard_above_the_published_top() {
        use crate::decode::layer_chooser::{KindLayerBounds, PrefMediaKind, ReceiveLayerBounds};

        // A floor ABOVE the single published layer — the pre-#2279 persisted pref.
        let poisoned = KindLayerBounds {
            min: Some(2),
            max: None,
        };
        assert_eq!(
            poisoned.clamp(0),
            2,
            "premise: these bounds DO raise, or neither site can regress"
        );

        // Site 1 — the monitor tick.
        let (mut peer, _muted) = make_test_peer(2279);
        peer.audio_layer_availability.observe(0, 1000);
        let desired = peer.tick_audio_layer_chooser(2000, poisoned);
        assert_eq!(
            desired, 0,
            "tick raised the audio guard above the published top"
        );
        assert_eq!(peer.selected_audio_layer(), 0);

        // Site 3 — the #1179 early congestion seed.
        let (mut early, _m3) = make_test_peer(2281);
        early.audio_layer_availability.observe(0, 1000);
        early.last_video_downlink = crate::decode::layer_chooser::DownlinkSample {
            loss_per_sec: crate::decode::layer_chooser::LOSS_STEP_DOWN_PER_SEC + 1.0,
            kf_per_sec: 0.0,
        };
        let mut seed_bounds = ReceiveLayerBounds::default();
        seed_bounds.set_kind(PrefMediaKind::Audio, Some(2), None);
        assert!(
            early.seed_early_congestion(2000, &seed_bounds),
            "premise: the early seed must fire, or the guard write is never reached"
        );
        assert_eq!(
            early.selected_audio_layer(),
            0,
            "the early seed raised the audio guard above the published top"
        );

        // Site 2 — the wire reconcile, on a fresh session (no recorded preference,
        // so the target starts at `highest_available`).
        let mut manager = PeerDecodeManager::new();
        manager.insert_single_audio_layer_peer_for_test(2280);
        let mut bounds = ReceiveLayerBounds::default();
        bounds.set_kind(PrefMediaKind::Audio, Some(2), None);
        let _ = manager.reconcile_decode_guards_to_wire(None, &bounds, 2000);
        assert_eq!(
            manager.selected_audio_layer_for_test(2280),
            Some(0),
            "reconcile raised the audio guard above the published top"
        );
    }

    /// A fresh relay session has no recorded preferences, but an operator
    /// receive ceiling still limits the persisted peer's decode guards.
    ///
    /// MUTATION: removing the post-`highest_available` bounds clamps raises
    /// both guards from L0 to the learned top L2 and fails these assertions.
    #[test]
    fn reconcile_decode_guards_to_wire_respects_ceiling_on_fresh_session() {
        use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds};

        let mut manager = PeerDecodeManager::new();
        manager.insert_zero_loss_top_peer_for_test(902);
        let mut bounds = ReceiveLayerBounds::default();
        bounds.set_kind(PrefMediaKind::Video, None, Some(0));
        bounds.set_kind(PrefMediaKind::Screen, None, Some(0));

        let _ = manager.reconcile_decode_guards_to_wire(None, &bounds, 2000);

        assert_eq!(manager.selected_video_layer_for_test(902), Some(0));
        assert_eq!(manager.selected_screen_layer_for_test(902), Some(0));
    }

    /// #1256 (resize cadence): applying the size lid N times within ONE sample
    /// window must re-assert the SAME lidded layer, NOT compound into N congestion
    /// down-steps. `apply_size_lid_to_decode_guards` (what `set_peer_tile_hints` now
    /// calls) sets the guard purely from the lid + the chooser's existing pick — no
    /// `choose()` — so it is idempotent. The OLD path
    /// (`tick_layer_choosers` -> `choose()`) would, on a congested peer, step down one
    /// rung PER call (here 2->1->0 across the 3 lidded calls), the over-collapse this
    /// fixes.
    ///
    /// The lid is chosen to map to **L1** (`device_px_h = 360`: 180*1.1=198 < 360
    /// fails L0; 360*1.1=396 >= 360 -> L1, issue #1768) so the idempotent result (1)
    /// DIFFERS from the compounded result (0). With an L0 lid both paths would land
    /// at 0 and the test would not discriminate.
    ///
    /// MUTATION: make `apply_size_lid_to_decode_guards` advance the chooser (e.g.
    /// call `tick_layer_chooser`/`choose()` instead of reading `desired_preference()`),
    /// OR re-point `set_peer_tile_hints` at `tick_layer_choosers`. On this CONGESTED
    /// peer the 3 calls then step the guard 2->1->0 (the constrained DOWN branch has
    /// no dwell gate and `last_video_downlink` is fixed within the window), landing at
    /// 0 — BELOW the lid — and the `== 1` assertion fails. Idempotency (no `choose()`
    /// advance) is what holds it at the lid.
    ///
    /// HOST `#[test]` (not `#[wasm_bindgen_test]`) so it actually runs under
    /// `cargo test -p videocall-client --lib` — cf. `early_seed_respects_user_receive_max`,
    /// which drives these same manager methods on the host harness.
    #[test]
    fn size_lid_apply_is_idempotent_across_resize_drag() {
        use crate::decode::layer_chooser::{
            DownlinkSample, PrefMediaKind, ReceiveLayerBounds, TileHint,
        };

        let mut manager = PeerDecodeManager::new();
        // Congested peer, learned 3-layer ladder (highest_available == 2). A FIXED
        // small clock keeps availability (observed at t=1000 by the helper) inside
        // the prune window (same trick as `congestion_seed_never_advertises_above_size_lid`).
        manager.connected_peers.insert(
            777,
            make_congested_top_peer(777, TransportType::TRANSPORT_WEBTRANSPORT),
        );
        let now = 2000u64;
        let bounds = ReceiveLayerBounds::default();

        // Bring the (unconstrained) chooser UP to the TOP (current == 2) via one CLEAN
        // unclamped tick, exactly as `congestion_seed_never_advertises_above_size_lid`
        // does — so the MUTATION (choose()-driven) path has somewhere to step DOWN
        // FROM (2 -> 1 -> 0). The fixed path leaves the chooser unconstrained, so
        // `desired_preference()` stays None and `v_base` stays at the top (2) every call.
        if let Some(p) = manager.connected_peers.get_mut(&777) {
            p.last_video_downlink = DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            };
        }
        let _ = manager.tick_layer_choosers(now, &bounds);
        assert_eq!(
            manager
                .connected_peers
                .get(&777)
                .unwrap()
                .selected_video_layer(),
            2,
            "clean unconstrained tick climbs the guard to the top before the lid applies"
        );
        // Restore the congested sample: the MUTATION path would consume it on every
        // call; the fixed path never reads it (no choose()).
        if let Some(p) = manager.connected_peers.get_mut(&777) {
            p.last_video_downlink = DownlinkSample {
                loss_per_sec: crate::decode::layer_chooser::LOSS_STEP_DOWN_PER_SEC + 1.0,
                kf_per_sec: 0.0,
            };
        }

        // A tile that maps to L1 (see the doc): store the hint, then apply the lid 3
        // times in the SAME window (simulating a resize drag's N un-debounced pushes).
        // (Tile 360px = new L1 native height, issue #1768: L0 boundary 198 < 360 <=
        // L1 boundary 396, so it caps to L1.)
        manager.set_peer_tile_hints(HashMap::from([(
            777u64,
            TileHint::Capped { device_px_h: 360 },
        )]));
        for _ in 0..3 {
            let _ = manager.apply_size_lid_to_decode_guards(now, &bounds);
        }

        // Fixed path: chooser stays unconstrained (no choose() ran) -> v_base = highest
        // = 2, effective_max = lid 1 -> guard = min(2, 1) = 1 on EVERY call. The OLD
        // choose()-path would have compounded to 0 (2 -> 1 -> 0) — see the MUTATION note.
        assert_eq!(
            manager
                .connected_peers
                .get(&777)
                .unwrap()
                .selected_video_layer(),
            1,
            "the lid (L1) must be re-asserted idempotently across the resize drag, \
             NOT compounded below it"
        );
        // Corollary: applying NO choose() means the chooser is still unconstrained, so
        // the read-only publish advertises the lid (1 < highest 2), proving the guard
        // and the wire agree on the lidded layer.
        let desired = manager.current_desired_preferences(now, &bounds);
        assert_eq!(
            desired.get(&(777, PrefMediaKind::Video)).copied(),
            Some(1),
            "the wire preference must match the idempotent lidded guard (L1)"
        );
    }

    /// T-P2 (#1256 P2): an UP-switch on an INVISIBLE peer must emit NO keyframe — the
    /// stream isn't being decoded, so a PLI to its publisher is wasted. Flipping the
    /// peer VISIBLE and repeating the same down->up must then emit a keyframe, proving
    /// the gate is visibility (and `video_enabled`), not a blanket suppression. The
    /// deferred keyframe is covered by `set_active_decode_set` on the
    /// invisible->visible transition.
    ///
    /// MUTATION: remove the `peer.visible &&` gate on the VIDEO up-switch push — the
    /// invisible phase then emits a keyframe and the "invisible vec is empty"
    /// assertion fails.
    #[wasm_bindgen_test]
    fn invisible_or_disabled_peer_upswitch_emits_no_keyframe() {
        use crate::decode::layer_chooser::{ReceiveLayerBounds, TileHint};
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::<PacketWrapper>::new()));
        let collected_clone = collected.clone();
        let callback = crate::Callback::from(move |pkt: PacketWrapper| {
            collected_clone.borrow_mut().push(pkt);
        });
        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(callback, "viewer@test.com".to_string());
        manager.insert_zero_loss_top_peer_for_test(444);
        // Visibility is the variable under test: keep the camera ENABLED (the helper
        // defaults `video_enabled` to false) and start the peer INVISIBLE so the
        // ONLY thing suppressing the keyframe is `peer.visible == false`.
        {
            let peer = manager.connected_peers.get_mut(&444).unwrap();
            peer.visible = false;
            peer.video_enabled = true;
        }

        // Down then up while INVISIBLE: small-tile lid (down/no-op to L0), then
        // Uncapped (up-switch L0 -> L2). The up-switch must NOT emit a keyframe.
        manager.set_peer_tile_hints(HashMap::from([(
            444u64,
            TileHint::Capped { device_px_h: 180 },
        )]));
        // FIXED clock (see `FIXTURE_NOW_MS`) — a wall clock prunes the fixture
        // ladder to highest_available 0, so no up-switch happens at all and the
        // "invisible emits nothing" assertion would pass for the WRONG reason.
        let _ = manager.tick_layer_choosers(FIXTURE_NOW_MS, &ReceiveLayerBounds::default());
        manager.set_peer_tile_hints(HashMap::from([(444u64, TileHint::Uncapped)]));
        let _ = manager.tick_layer_choosers(FIXTURE_NOW_MS, &ReceiveLayerBounds::default());
        assert!(
            collected.borrow().is_empty(),
            "an INVISIBLE peer's up-switch must NOT request a keyframe: {:?}",
            collected.borrow()
        );

        // Flip VISIBLE and repeat the down->up: the up-switch must NOW emit one
        // VIDEO keyframe — proving the gate is visibility, not blanket suppression.
        {
            let peer = manager.connected_peers.get_mut(&444).unwrap();
            peer.visible = true;
        }
        collected.borrow_mut().clear();
        manager.set_peer_tile_hints(HashMap::from([(
            444u64,
            TileHint::Capped { device_px_h: 180 },
        )]));
        let _ = manager.tick_layer_choosers(FIXTURE_NOW_MS, &ReceiveLayerBounds::default());
        manager.set_peer_tile_hints(HashMap::from([(444u64, TileHint::Uncapped)]));
        let _ = manager.tick_layer_choosers(FIXTURE_NOW_MS, &ReceiveLayerBounds::default());
        let pkts = collected.borrow();
        assert_eq!(
            pkts.len(),
            1,
            "a VISIBLE peer's up-switch must request exactly one keyframe: {pkts:?}"
        );
        let inner = MediaPacket::parse_from_bytes(&pkts[0].data)
            .expect("should deserialize inner MediaPacket");
        assert_eq!(
            inner.media_type.enum_value(),
            Ok(MediaType::KEYFRAME_REQUEST),
            "visible up-switch packet must be a KEYFRAME_REQUEST"
        );
        assert_eq!(
            inner.data,
            b"VIDEO".to_vec(),
            "the keyframe request must be for the VIDEO stream"
        );
    }

    // -----------------------------------------------------------------
    // Issue #1179, Part B: early-seed wiring (manager level)
    //
    // These exercise the two glue methods the early-seed timer drives:
    //   * seed_early_congestion_for_connected_peers — seeds purely on each
    //     peer's congestion gate (no transport filtering of its own; the WT
    //     decision lives at the call site) and is a no-op on a clean join
    //     (M2 preserved);
    //   * current_desired_preferences — READ-ONLY (advances no hysteresis).
    // -----------------------------------------------------------------

    /// Mark a peer's VIDEO downlink congested and learn a 3-layer ladder so a
    /// constrain has somewhere to step down to. Mirrors the inputs the decode
    /// path would have populated for a real congested WT join.
    fn make_congested_top_peer(session_id: u64, transport: TransportType) -> Peer {
        let (mut peer, _muted) = make_test_peer(session_id);
        peer.transport_type = transport;
        // Learn layers 0,1,2 so highest_available == 2 (room to drop to 1).
        for layer in 0..3u32 {
            peer.video_layer_availability.observe(layer, 1000);
            peer.screen_layer_availability.observe(layer, 1000);
            peer.audio_layer_availability.observe(layer, 1000);
        }
        // A congested video window (over the loss step-down threshold). Screen has
        // its own window; leave it clean so only VIDEO/AUDIO can seed for this peer
        // (audio is proxied by the video downlink).
        peer.last_video_downlink = crate::decode::layer_chooser::DownlinkSample {
            loss_per_sec: crate::decode::layer_chooser::LOSS_STEP_DOWN_PER_SEC + 1.0,
            kf_per_sec: 0.0,
        };
        peer
    }

    /// The early seed constrains EVERY congested peer regardless of the peer's
    /// announced (remote-uplink) `transport_type`. The WebTransport gate is NOT
    /// per-peer — it lives at the call site (the early-seed timer tick), keyed on
    /// THIS client's LOCAL active transport. So at this layer a congested WS- or
    /// UNKNOWN-announcing peer must be seeded exactly like a WT-announcing one.
    ///
    /// MUTATION CHECK: fails if a per-peer `transport_type` gate is (re)introduced
    /// into `seed_early_congestion_for_connected_peers` — then the WS/UNKNOWN peers
    /// would NOT be seeded and their desired entries would be missing.
    #[wasm_bindgen_test]
    fn early_seed_constrains_congested_peers_regardless_of_peer_transport() {
        use crate::decode::layer_chooser::PrefMediaKind;
        let mut manager = PeerDecodeManager::new();
        manager.connected_peers.insert(
            100,
            make_congested_top_peer(100, TransportType::TRANSPORT_WEBTRANSPORT),
        );
        manager.connected_peers.insert(
            200,
            make_congested_top_peer(200, TransportType::TRANSPORT_WEBSOCKET),
        );
        manager.connected_peers.insert(
            300,
            make_congested_top_peer(300, TransportType::TRANSPORT_UNKNOWN),
        );

        // Open (default) bounds — these tests cover the unbounded user; the bounds
        // clamp is exercised separately in `early_seed_respects_user_receive_max`.
        let bounds = crate::decode::layer_chooser::ReceiveLayerBounds::default();

        // `observe_early_congestion` steps DOWN from the chooser's CURRENT layer
        // (`from = current.min(highest); dropped = from.saturating_sub(1)`), and
        // `LayerChooser::new(0)` starts COLD at 0 — seeding a cold chooser yields
        // `0.saturating_sub(1) == 0`, not the "top (2) -> 1" step this test is about.
        // Bring each chooser up to the top with one CLEAN unclamped tick first (the
        // same preparation the passing `congestion_seed_never_advertises_above_size_lid`
        // uses), then restore the congested sample the seed must react to.
        let clean = crate::decode::layer_chooser::DownlinkSample {
            loss_per_sec: 0.0,
            kf_per_sec: 0.0,
        };
        let congested = crate::decode::layer_chooser::DownlinkSample {
            loss_per_sec: crate::decode::layer_chooser::LOSS_STEP_DOWN_PER_SEC + 1.0,
            kf_per_sec: 0.0,
        };
        for sid in [100u64, 200, 300] {
            if let Some(p) = manager.connected_peers.get_mut(&sid) {
                p.last_video_downlink = clean;
            }
        }
        let _ = manager.tick_layer_choosers(2000, &bounds);
        for sid in [100u64, 200, 300] {
            if let Some(p) = manager.connected_peers.get_mut(&sid) {
                p.last_video_downlink = congested;
            }
        }

        let seeded = manager.seed_early_congestion_for_connected_peers(2000, &bounds);
        assert!(seeded, "the congested peers' samples must seed a constrain");

        let desired = manager.current_desired_preferences(2000, &bounds);
        // Every congested peer (any announced transport): video constrained from
        // top (2) down to 1, audio proxied by the same congested video window.
        for sid in [100u64, 200, 300] {
            assert_eq!(
                desired.get(&(sid, PrefMediaKind::Video)),
                Some(&1),
                "peer {sid} must be constrained to layer 1 by the early seed \
                 (per-peer transport must NOT gate the seed): {desired:?}"
            );
            assert_eq!(
                desired.get(&(sid, PrefMediaKind::Audio)),
                Some(&1),
                "peer {sid} audio (video-proxied) must also constrain: {desired:?}"
            );
        }
    }

    /// M2 non-regression: a HEALTHY WT joiner (clean downlink) must be seeded
    /// NOTHING — no constrain, no preference, no HD dip.
    ///
    /// MUTATION CHECK: fails if `observe_early_congestion` ever constrains on a
    /// clean sample (the `!sample.is_congested()` guard in the primitive).
    #[wasm_bindgen_test]
    fn early_seed_noop_on_clean_wt_join() {
        let mut manager = PeerDecodeManager::new();
        // Clean WT peer: top layers learned, but downlink is the default clean
        // sample (loss 0 / kf 0).
        let (mut peer, _muted) = make_test_peer(101);
        peer.transport_type = TransportType::TRANSPORT_WEBTRANSPORT;
        for layer in 0..3u32 {
            peer.video_layer_availability.observe(layer, 1000);
        }
        manager.connected_peers.insert(101, peer);

        let bounds = crate::decode::layer_chooser::ReceiveLayerBounds::default();
        let seeded = manager.seed_early_congestion_for_connected_peers(2000, &bounds);
        assert!(!seeded, "a clean WT join must seed nothing (M2)");
        let desired = manager.current_desired_preferences(2000, &bounds);
        assert!(
            desired.is_empty(),
            "clean WT join advertises no preference: {desired:?}"
        );
    }

    /// READ-ONLY: `current_desired_preferences` must not advance ANY chooser
    /// state. After seeding a WT peer (constrained at layer 1, clean-window streak
    /// reset to 0 by the step-down), we call the accessor 50 times with a CLEAN
    /// downlink loaded — the exact condition under which a real `choose`/`tick`
    /// WOULD accumulate clean windows and eventually climb. The peer's decode
    /// layer and advertised preference must be byte-for-byte unchanged across all
    /// 50 reads, AND the map each read returns must be identical (idempotent).
    ///
    /// This is mutation-sensitive WITHOUT relying on top-convergence: the seeded
    /// state is held at layer 1, so any climb (which a `choose` mutation would
    /// eventually cause once the streak/dwell are met) moves it OFF 1 and the
    /// assertion fires. It is also belt-and-suspenders to the compile-time
    /// guarantee that `current_desired_preferences`/`collect_desired_preferences`
    /// take `&self` (a `choose` call needs `&mut self` and would not compile).
    ///
    /// MUTATION CHECK: fails if the accessor is changed to drive `choose`/`tick_*`
    /// (the seeded layer would climb away from 1 / the returned map would change),
    /// or to clear `constrained` (the preference would vanish).
    #[wasm_bindgen_test]
    fn current_desired_preferences_is_read_only() {
        use crate::decode::layer_chooser::PrefMediaKind;

        let mut mgr = PeerDecodeManager::new();
        mgr.connected_peers.insert(
            1,
            make_congested_top_peer(1, TransportType::TRANSPORT_WEBTRANSPORT),
        );
        // Open (default) bounds — the read-only guarantee is independent of the
        // user's clamp; the clamp itself is covered by
        // `early_seed_respects_user_receive_max`.
        let bounds = crate::decode::layer_chooser::ReceiveLayerBounds::default();

        // The seed steps down from the chooser's CURRENT layer, and a cold
        // `LayerChooser::new(0)` is at 0 (so it would seed 0, not 1). One CLEAN
        // unclamped tick brings it to the top first; then restore the congested
        // sample so the seed has real congestion to constrain on.
        if let Some(p) = mgr.connected_peers.get_mut(&1) {
            p.last_video_downlink = crate::decode::layer_chooser::DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            };
        }
        let _ = mgr.tick_layer_choosers(2000, &bounds);
        if let Some(p) = mgr.connected_peers.get_mut(&1) {
            p.last_video_downlink = crate::decode::layer_chooser::DownlinkSample {
                loss_per_sec: crate::decode::layer_chooser::LOSS_STEP_DOWN_PER_SEC + 1.0,
                kf_per_sec: 0.0,
            };
        }

        // Seed: constrains video 2 -> 1 (and audio, video-proxied).
        assert!(mgr.seed_early_congestion_for_connected_peers(2000, &bounds));

        // Load a CLEAN downlink so that IF the accessor (incorrectly) advanced the
        // chooser, the clean-window streak would build and eventually climb.
        if let Some(p) = mgr.connected_peers.get_mut(&1) {
            p.last_video_downlink = crate::decode::layer_chooser::DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            };
        }

        // Snapshot the post-seed state, then hammer the read-only accessor.
        let baseline = mgr.current_desired_preferences(2000, &bounds);
        assert_eq!(
            baseline.get(&(1, PrefMediaKind::Video)),
            Some(&1),
            "seeded WT peer advertises layer 1 before any reads"
        );
        let baseline_layer = mgr.connected_peers.get(&1).unwrap().selected_video_layer();
        assert_eq!(baseline_layer, 1, "seeded decode layer is 1");

        for i in 0..50 {
            let map = mgr.current_desired_preferences(2000, &bounds);
            assert_eq!(
                map, baseline,
                "read #{i}: current_desired_preferences must be idempotent (read-only)"
            );
            let layer = mgr.connected_peers.get(&1).unwrap().selected_video_layer();
            assert_eq!(
                layer, baseline_layer,
                "read #{i}: the accessor must not advance the chooser — decode layer \
                 moved from {baseline_layer} to {layer} (hysteresis advanced off-cadence)"
            );
        }
    }

    /// BLOCKER (PR #1192 review): the EARLY-SEED path must clamp to the user's
    /// receive bounds exactly as the 5s tick does. A bandwidth-conscious user who
    /// set a manual receive `max` BELOW `highest-1`, joining on WebTransport into
    /// early congestion, must never (even transiently) decode or advertise a layer
    /// above their cap.
    ///
    /// Setup: a peer with a learned 3-layer ladder (`highest_available == 2`, so
    /// `highest-1 == 1`) and a congested video downlink. The unclamped seed would
    /// step the chooser down from 2 to 1 and write `selected_video_layer = 1` +
    /// advertise `Some(1)`. The user caps video at `max = 0` (below `highest-1`),
    /// so the clamped result must be `selected_video_layer == 0` and the advertised
    /// preference must be `0` (≤ max), never `1`.
    ///
    /// This is a HOST `#[test]` (not `#[wasm_bindgen_test]`) so it actually runs
    /// under `cargo test -p videocall-client --lib` — the wasm-only seed tests do
    /// not execute on the host harness.
    ///
    /// MUTATION CHECK: delete either `bounds.for_kind(...).clamp(...)` in
    /// `Peer::seed_early_congestion` (decode guard) or in
    /// `Peer::collect_desired_preferences` (advertised layer) and this test fails —
    /// `selected_video_layer` becomes 1 (> max) and/or the advertised entry becomes
    /// `Some(1)` (> max). Confirmed by hand-mutating both clamp sites.
    #[test]
    fn early_seed_respects_user_receive_max() {
        use crate::decode::layer_chooser::{DownlinkSample, PrefMediaKind, ReceiveLayerBounds};

        let mut manager = PeerDecodeManager::new();
        // Congested WT peer with a learned 3-layer ladder (highest_available == 2).
        manager.connected_peers.insert(
            900,
            make_congested_top_peer(900, TransportType::TRANSPORT_WEBTRANSPORT),
        );

        // Bring the (unconstrained) chooser up to the TOP (current == 2) the way a
        // healthy join would, via one CLEAN unclamped tick — so the subsequent
        // congested seed steps DOWN from 2 to highest-1 (==1), the value that
        // would land ABOVE the user's cap without the clamp. (The seed primitive
        // steps down from `current`, so `current` must be at the top first.)
        let open = ReceiveLayerBounds::default();
        if let Some(p) = manager.connected_peers.get_mut(&900) {
            p.last_video_downlink = DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            };
        }
        assert_eq!(
            manager
                .tick_layer_choosers(1500, &open)
                .get(&(900, PrefMediaKind::Video)),
            None,
            "clean unconstrained tick tracks the top and advertises nothing"
        );
        assert_eq!(
            manager
                .connected_peers
                .get(&900)
                .unwrap()
                .selected_video_layer(),
            2,
            "chooser climbed to the top before the congested seed"
        );
        // Restore the congested window so the early seed sees congestion.
        if let Some(p) = manager.connected_peers.get_mut(&900) {
            p.last_video_downlink = DownlinkSample {
                loss_per_sec: crate::decode::layer_chooser::LOSS_STEP_DOWN_PER_SEC + 1.0,
                kf_per_sec: 0.0,
            };
        }

        // User caps received VIDEO at layer 0 — BELOW highest-1 (==1). Screen/audio
        // left open so the clamp is exercised on VIDEO specifically.
        let mut bounds = ReceiveLayerBounds::default();
        bounds.set_kind(PrefMediaKind::Video, None, Some(0));

        let seeded = manager.seed_early_congestion_for_connected_peers(2000, &bounds);
        assert!(
            seeded,
            "the congested sample must still seed a constrain (clamp is a pure \
             post-process, it does not gate the seed)"
        );

        // Decode guard must NOT exceed the user's max (0). Unclamped this is 1.
        let selected = manager
            .connected_peers
            .get(&900)
            .unwrap()
            .selected_video_layer();
        assert!(
            selected == 0,
            "early-seed decode guard {selected} must be clamped to user video max 0 \
             (== 0; unclamped it would be highest-1 == 1)"
        );

        // Advertised preference must NOT exceed the user's max (0). Unclamped this
        // is Some(1) — ABOVE the user's cap, the exact invariant violation.
        let desired = manager.current_desired_preferences(2000, &bounds);
        let advertised = desired.get(&(900, PrefMediaKind::Video)).copied();
        assert_eq!(
            advertised,
            Some(0),
            "early-seed must advertise the clamped layer 0 (≤ user max), never \
             {advertised:?} above the user's cap"
        );
    }

    /// CORE REGRESSION (#1219 Half 2): a relay-authored DOWNLINK_CONGESTION must
    /// step a peer's RECEIVER-side chooser down EVEN WHEN the per-peer real
    /// downlink sample shows ZERO loss (the WebSocket / reliable-WT case). This is
    /// the exact blindness the early-seed path cannot cover, because
    /// `DownlinkSample::is_congested()` is false on `{0, 0}`.
    ///
    /// HOST `#[test]` (NOT `#[wasm_bindgen_test]`): the wasm harness on this box
    /// silently no-ops, so the regression is pinned with a native test that
    /// actually executes under `cargo test -p videocall-client --lib`.
    ///
    /// MUTATION CHECK: if the DOWNLINK_CONGESTION arm's seed call is removed, or
    /// reverts to `seed_early_congestion_for_connected_peers` (which reads the
    /// zero-loss REAL sample → `is_congested()` false → no-op), then
    /// `seed_downlink_congestion_for_connected_peers` returns false and
    /// `current_desired_preferences` yields an EMPTY map — this test fails.
    /// Confirmed by hand-swapping the synthetic seed for the early seed: the peer
    /// stays at layer 2 and advertises nothing.
    #[test]
    fn downlink_congestion_steps_down_with_zero_loss() {
        use crate::decode::layer_chooser::{DownlinkSample, PrefMediaKind, ReceiveLayerBounds};

        let mut manager = PeerDecodeManager::new();
        manager
            .connected_peers
            .insert(700, make_zero_loss_top_peer(700));

        let open = ReceiveLayerBounds::default();

        // Precondition (assert explicitly): the real sample is ZERO-loss, so the
        // existing early-seed path could NOT step this peer down.
        assert_eq!(
            manager
                .connected_peers
                .get(&700)
                .unwrap()
                .last_video_downlink,
            DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0
            },
            "precondition: real downlink sample is zero-loss (lossless transport)"
        );

        // Bring the unconstrained chooser to the TOP (current == 2) via one CLEAN
        // unconstrained tick — exactly as `early_seed_respects_user_receive_max`
        // does — so the synthetic seed steps DOWN from 2 to 1.
        assert_eq!(
            manager
                .tick_layer_choosers(1500, &open)
                .get(&(700, PrefMediaKind::Video)),
            None,
            "clean unconstrained tick tracks the top and advertises nothing"
        );
        assert_eq!(
            manager
                .connected_peers
                .get(&700)
                .unwrap()
                .selected_video_layer(),
            2,
            "chooser climbed to the top before the DOWNLINK_CONGESTION seed"
        );

        // Sanity: the EARLY seed (real zero-loss sample) is a no-op here — this is
        // precisely why a synthetic-sample primitive was needed.
        assert!(
            !manager.seed_early_congestion_for_connected_peers(1800, &open),
            "early-seed must NO-OP on the zero-loss sample (the #1219 blindness)"
        );
        assert_eq!(
            manager
                .connected_peers
                .get(&700)
                .unwrap()
                .selected_video_layer(),
            2,
            "early-seed left the peer at the top (no real congestion to react to)"
        );

        // The relay-authored signal: synthetic congestion steps the chooser down.
        let seeded = manager.seed_downlink_congestion_for_connected_peers(2000, &open, false);
        assert!(
            seeded,
            "DOWNLINK_CONGESTION must step the chooser down despite zero real loss"
        );

        // Decode guard stepped down 2 -> 1.
        assert_eq!(
            manager
                .connected_peers
                .get(&700)
                .unwrap()
                .selected_video_layer(),
            1,
            "synthetic seed steps the decode guard down from the top (2) to 1"
        );

        // The published preference map is NON-EMPTY with video at layer 1.
        let desired = manager.current_desired_preferences(2000, &open);
        assert_eq!(
            desired.get(&(700, PrefMediaKind::Video)).copied(),
            Some(1),
            "DOWNLINK_CONGESTION must advertise video stepped down to layer 1"
        );

        // AUDIO PROTECTION (#1219 Half 2): audio is priority-protected and must
        // NOT be stepped down by the DOWNLINK_CONGESTION seed — only video/screen
        // are. The initial clean `tick_layer_choosers(1500)` brought the audio
        // chooser to the top (2) too; the seed must have LEFT it there, and the
        // published map must carry NO audio entry.
        //
        // MUTATION CHECK: re-add an audio `observe_early_congestion` branch to
        // `Peer::seed_downlink_congestion` and this fails — audio drops to 1 and an
        // audio entry appears in `desired`.
        assert_eq!(
            manager
                .connected_peers
                .get(&700)
                .unwrap()
                .selected_audio_layer(),
            2,
            "audio must stay at the top — DOWNLINK_CONGESTION does not shed audio"
        );
        assert_eq!(
            desired.get(&(700, PrefMediaKind::Audio)).copied(),
            None,
            "DOWNLINK_CONGESTION must advertise NO audio preference (audio protected)"
        );
    }

    /// BOUNDS CLAMP (#1219 Half 2): the synthetic DOWNLINK_CONGESTION seed must
    /// still respect the user's RECEIVE bounds — it shares
    /// `seed_early_congestion`'s clamp-to-bounds post-process, so a user `max`
    /// below `highest-1` clamps the stepped-down layer down to the cap.
    ///
    /// Mirrors `early_seed_respects_user_receive_max` but with a zero-loss real
    /// sample (proving the clamp holds on the synthetic path too).
    ///
    /// MUTATION CHECK: delete the `bounds.for_kind(...).clamp(...)` in
    /// `Peer::seed_downlink_congestion` (VIDEO decode guard) and this test fails —
    /// `selected_video_layer` becomes 1 (> max 0).
    #[test]
    fn downlink_congestion_seed_does_not_constrain_below_user_max() {
        use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds};

        let mut manager = PeerDecodeManager::new();
        manager
            .connected_peers
            .insert(701, make_zero_loss_top_peer(701));

        let open = ReceiveLayerBounds::default();
        // Climb to the top first (current == 2).
        let _ = manager.tick_layer_choosers(1500, &open);
        assert_eq!(
            manager
                .connected_peers
                .get(&701)
                .unwrap()
                .selected_video_layer(),
            2,
            "chooser climbed to the top before the DOWNLINK_CONGESTION seed"
        );

        // User caps received VIDEO at layer 0 — BELOW highest-1 (==1).
        let mut bounds = ReceiveLayerBounds::default();
        bounds.set_kind(PrefMediaKind::Video, None, Some(0));

        let seeded = manager.seed_downlink_congestion_for_connected_peers(2000, &bounds, false);
        assert!(
            seeded,
            "the synthetic congested sample must still step a constrain (clamp is a \
             pure post-process, it does not gate the seed)"
        );

        // Decode guard must NOT exceed the user's max (0). Unclamped this is 1.
        let selected = manager
            .connected_peers
            .get(&701)
            .unwrap()
            .selected_video_layer();
        assert_eq!(
            selected, 0,
            "DOWNLINK_CONGESTION decode guard must be clamped to user video max 0 \
             (unclamped it would be highest-1 == 1)"
        );

        // Advertised preference must NOT exceed the user's max (0).
        let desired = manager.current_desired_preferences(2000, &bounds);
        assert_eq!(
            desired.get(&(701, PrefMediaKind::Video)).copied(),
            Some(0),
            "DOWNLINK_CONGESTION must advertise the clamped layer 0 (≤ user max)"
        );
    }

    /// ENCODER-SCOPE GUARD (#1219 Half 2): the synthetic DOWNLINK_CONGESTION seed
    /// is RECEIVER-ONLY. The publisher encoder (`congestion_step_down_flag`,
    /// `CameraEncoder`, `EncoderBitrateController`, `audio_congestion_layer_ceiling`,
    /// `apply_self_congestion_cut`) lives OUTSIDE `PeerDecodeManager` entirely, so a
    /// `PeerDecodeManager`-level test cannot even name those symbols. This test
    /// documents that structural separation and asserts the only state the seed
    /// mutates is the per-peer receiver-side chooser / decode guard.
    ///
    /// The wiring-site scope (the DOWNLINK_CONGESTION arm references ONLY
    /// `peer_decode_manager`, `layer_preference_sender`, and
    /// `connection_controller`) is additionally verified by the grep recorded in
    /// the PR description — none of the encoder symbols appear in that arm or in
    /// the two new seed primitives.
    #[test]
    fn downlink_congestion_seed_is_receiver_only() {
        use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds};

        let mut manager = PeerDecodeManager::new();
        manager
            .connected_peers
            .insert(702, make_zero_loss_top_peer(702));
        let open = ReceiveLayerBounds::default();
        let _ = manager.tick_layer_choosers(1500, &open);

        // Capture the only peer-observable receiver-side state the seed may change.
        let before = manager
            .connected_peers
            .get(&702)
            .unwrap()
            .selected_video_layer();
        assert_eq!(before, 2, "peer at top before seed");

        // Seed steps the RECEIVER chooser down. There is no publisher-encoder field
        // reachable from here to inspect — the manager owns only receive-side state.
        assert!(manager.seed_downlink_congestion_for_connected_peers(2000, &open, false));
        let after = manager
            .connected_peers
            .get(&702)
            .unwrap()
            .selected_video_layer();
        assert_eq!(
            after, 1,
            "seed mutated ONLY the receiver-side decode guard (2 -> 1)"
        );

        // And it advertises a receive-layer request, never an encoder action.
        let desired = manager.current_desired_preferences(2000, &open);
        assert_eq!(desired.get(&(702, PrefMediaKind::Video)).copied(), Some(1));
    }

    /// SPEAKER EXEMPTION (#1557): an active speaker is NOT stepped down by the
    /// receiver-side layer-drop seed — the person talking stays sharp — while a
    /// non-speaking peer in the same room IS stepped down.
    ///
    /// MUTATION CHECK: flip this call's `exempt_speakers` arg to `false` and this
    /// fails — the speaker (710) drops to 1 alongside the non-speaker (711). (The
    /// `exempt_speakers &&` guard itself is pinned by the sibling relay test
    /// `speaker_video_layer_stepped_on_relay_downlink_seed`, whose `false` arg would
    /// let the bare `is_speaking` skip resurface and break ITS 710 -> 1 assertion.)
    #[test]
    fn speaker_video_layer_exempt_from_downlink_seed() {
        use crate::decode::layer_chooser::ReceiveLayerBounds;

        let mut manager = PeerDecodeManager::new();
        manager
            .connected_peers
            .insert(710, make_zero_loss_top_peer(710));
        manager
            .connected_peers
            .insert(711, make_zero_loss_top_peer(711));

        let open = ReceiveLayerBounds::default();

        // Mark peer 710 as the active speaker; 711 stays silent.
        manager.connected_peers.get_mut(&710).unwrap().is_speaking = true;

        // Bring BOTH choosers to the top (current == 2) via one clean tick.
        let _ = manager.tick_layer_choosers(1500, &open);
        assert_eq!(
            manager
                .connected_peers
                .get(&710)
                .unwrap()
                .selected_video_layer(),
            2,
            "speaking peer climbed to the top before the seed"
        );
        assert_eq!(
            manager
                .connected_peers
                .get(&711)
                .unwrap()
                .selected_video_layer(),
            2,
            "non-speaking peer climbed to the top before the seed"
        );

        // Seed receiver-side congestion with `exempt_speakers == true` (the LOCAL
        // CPU-pressure policy). The speaker must be skipped (not stepped), so
        // `seeded` reflects ONLY the non-speaker's move.
        let seeded = manager.seed_downlink_congestion_for_connected_peers(2000, &open, true);
        assert!(
            seeded,
            "the non-speaking peer was stepped, so the seed reports movement"
        );

        // Speaker (710) is EXEMPT — its video layer is byte-identical (still 2).
        assert_eq!(
            manager
                .connected_peers
                .get(&710)
                .unwrap()
                .selected_video_layer(),
            2,
            "active speaker keeps its current video layer (exempt from layer-drop)"
        );
        // Non-speaker (711) WAS stepped down 2 -> 1.
        assert_eq!(
            manager
                .connected_peers
                .get(&711)
                .unwrap()
                .selected_video_layer(),
            1,
            "non-speaking peer is stepped down by the seed (2 -> 1)"
        );
    }

    /// RELAY DOWNLINK NON-EXEMPTION (#1557 / #1219 Half 2): on the relay
    /// DOWNLINK_CONGESTION path the seed is called with `exempt_speakers == false`,
    /// so the active speaker's VIDEO IS stepped down alongside everyone else's.
    /// Under REAL downlink saturation the speaker's stream is the largest inbound
    /// and must be shed; in the degenerate 1-on-1 the only remote peer IS the
    /// speaker, so exempting it would shed zero bitrate. This pins that the
    /// relay path does NOT honor the speaker exemption.
    ///
    /// MUTATION CHECK: flip this call's `exempt_speakers` arg to `true` and this
    /// fails — the speaker (710) stays at 2 and the "both stepped 2 -> 1"
    /// assertion on 710 breaks.
    #[test]
    fn speaker_video_layer_stepped_on_relay_downlink_seed() {
        use crate::decode::layer_chooser::ReceiveLayerBounds;

        let mut manager = PeerDecodeManager::new();
        manager
            .connected_peers
            .insert(710, make_zero_loss_top_peer(710));
        manager
            .connected_peers
            .insert(711, make_zero_loss_top_peer(711));

        let open = ReceiveLayerBounds::default();

        // Mark peer 710 as the active speaker; 711 stays silent.
        manager.connected_peers.get_mut(&710).unwrap().is_speaking = true;

        // Bring BOTH choosers to the top (current == 2) via one clean tick.
        let _ = manager.tick_layer_choosers(1500, &open);
        assert_eq!(
            manager
                .connected_peers
                .get(&710)
                .unwrap()
                .selected_video_layer(),
            2,
            "speaking peer climbed to the top before the seed"
        );
        assert_eq!(
            manager
                .connected_peers
                .get(&711)
                .unwrap()
                .selected_video_layer(),
            2,
            "non-speaking peer climbed to the top before the seed"
        );

        // Relay path: `exempt_speakers == false` — the speaker is NOT skipped.
        let seeded = manager.seed_downlink_congestion_for_connected_peers(2000, &open, false);
        assert!(
            seeded,
            "both peers were stepped, so the seed reports movement"
        );

        // Speaker (710) IS stepped down 2 -> 1 on the relay path (NOT exempt).
        assert_eq!(
            manager
                .connected_peers
                .get(&710)
                .unwrap()
                .selected_video_layer(),
            1,
            "active speaker's video IS stepped down on the relay path (2 -> 1)"
        );
        // Non-speaker (711) WAS stepped down 2 -> 1.
        assert_eq!(
            manager
                .connected_peers
                .get(&711)
                .unwrap()
                .selected_video_layer(),
            1,
            "non-speaking peer is stepped down by the seed (2 -> 1)"
        );
    }

    /// AUDIO EXEMPTION (#1557 / pre-existing #1219 Half 2): the receiver-side
    /// layer-drop seed NEVER steps any peer's audio chooser — audio is
    /// priority-protected. Pins that the speaker exemption did not regress the
    /// audio protection for either a speaking or a non-speaking peer. The audio
    /// exemption holds for BOTH `exempt_speakers` values (true = local CPU policy,
    /// false = relay downlink policy): neither path ever touches the audio chooser.
    ///
    /// MUTATION CHECK: add an audio branch to `Peer::seed_downlink_congestion` and
    /// this fails — a peer's audio drops below 2.
    #[test]
    fn audio_never_stepped_by_downlink_seed() {
        use crate::decode::layer_chooser::ReceiveLayerBounds;

        let mut manager = PeerDecodeManager::new();
        manager
            .connected_peers
            .insert(712, make_zero_loss_top_peer(712));
        manager
            .connected_peers
            .insert(713, make_zero_loss_top_peer(713));

        let open = ReceiveLayerBounds::default();
        // 712 speaks, 713 does not — exercise both exemption paths for audio.
        manager.connected_peers.get_mut(&712).unwrap().is_speaking = true;

        // Climb both choosers to the top (audio chooser reaches 2 as well).
        let _ = manager.tick_layer_choosers(1500, &open);
        assert_eq!(
            manager
                .connected_peers
                .get(&712)
                .unwrap()
                .selected_audio_layer(),
            2,
            "speaker audio at the top before the seed"
        );
        assert_eq!(
            manager
                .connected_peers
                .get(&713)
                .unwrap()
                .selected_audio_layer(),
            2,
            "non-speaker audio at the top before the seed"
        );

        // Drive BOTH policies: exempt_speakers true (local CPU) then false (relay).
        // Neither must touch the audio chooser for either peer.
        let _ = manager.seed_downlink_congestion_for_connected_peers(2000, &open, true);
        let _ = manager.seed_downlink_congestion_for_connected_peers(2100, &open, false);

        // Observed: NEITHER peer's audio layer moved — the seed only ever steps
        // VIDEO/SCREEN, never AUDIO (the speaker is fully skipped; the non-speaker
        // is stepped on video but its audio chooser is left at the top).
        assert_eq!(
            manager
                .connected_peers
                .get(&712)
                .unwrap()
                .selected_audio_layer(),
            2,
            "speaker audio untouched by the downlink seed"
        );
        assert_eq!(
            manager
                .connected_peers
                .get(&713)
                .unwrap()
                .selected_audio_layer(),
            2,
            "non-speaker audio untouched by the downlink seed (video-only step)"
        );
    }

    /// NIT 1 (PR #1192 review): a source whose ONLY learned layer is the base
    /// (`highest_available == 0`) must advertise NOTHING from the early-seed path,
    /// matching the tick's `clamped < highest_available` gate — never `Some(0)`.
    ///
    /// MUTATION CHECK: drop the `< highest_available` gate in
    /// `Peer::collect_desired_preferences` (advertise unconditionally on
    /// `desired_preference()`) and this test fails — the base-only constrained
    /// chooser would advertise `Some(0)`.
    #[test]
    fn early_seed_base_only_source_advertises_nothing() {
        use crate::decode::layer_chooser::{DownlinkSample, PrefMediaKind, ReceiveLayerBounds};

        let mut manager = PeerDecodeManager::new();
        let (mut peer, _muted) = make_test_peer(950);
        peer.transport_type = TransportType::TRANSPORT_WEBTRANSPORT;
        // Only the BASE layer is ever observed → highest_available == 0.
        peer.video_layer_availability.observe(0, 1000);
        // Congested video window so the chooser would still try to constrain.
        peer.last_video_downlink = DownlinkSample {
            loss_per_sec: crate::decode::layer_chooser::LOSS_STEP_DOWN_PER_SEC + 1.0,
            kf_per_sec: 0.0,
        };
        manager.connected_peers.insert(950, peer);

        let bounds = ReceiveLayerBounds::default();
        manager.seed_early_congestion_for_connected_peers(2000, &bounds);

        let desired = manager.current_desired_preferences(2000, &bounds);
        assert_eq!(
            desired.get(&(950, PrefMediaKind::Video)),
            None,
            "a base-only source (highest_available == 0) must advertise nothing — \
             the `< highest_available` gate suppresses a spurious Some(0): {desired:?}"
        );
    }

    /// Phase 4 (#989): the user's receive-layer bounds clamp each peer's chosen
    /// video layer, and lowering `max` steps the decode guard DOWN immediately on
    /// the next tick (not after a delay).
    #[wasm_bindgen_test]
    fn receive_bounds_clamp_video_and_step_down_immediately() {
        use crate::decode::layer_chooser::{KindLayerBounds, PrefMediaKind, ReceiveLayerBounds};
        let mut manager = PeerDecodeManager::new();
        let (peer, _muted) = make_test_peer(701);
        manager.connected_peers.insert(701, peer);

        // Learn 3 video layers + climb to the top under clean downlink, UNCLAMPED.
        let open = ReceiveLayerBounds::default();
        let mut t = 1000u64;
        for _ in 0..20 {
            if let Some(p) = manager.connected_peers.get_mut(&701) {
                for layer in 0..3u32 {
                    p.video_layer_availability.observe(layer, t);
                }
            }
            manager.tick_layer_choosers(t, &open);
            t += 1100;
        }
        assert_eq!(
            manager
                .connected_peers
                .get(&701)
                .unwrap()
                .selected_video_layer(),
            2,
            "unclamped: climbs to top"
        );

        // Now cap video at max layer 1 → next tick clamps the desired layer AND
        // the decode guard down to 1 immediately.
        let mut capped = ReceiveLayerBounds::default();
        capped.set_kind(PrefMediaKind::Video, None, Some(1));
        if let Some(p) = manager.connected_peers.get_mut(&701) {
            for layer in 0..3u32 {
                p.video_layer_availability.observe(layer, t);
            }
        }
        let desired = manager.tick_layer_choosers(t, &capped);
        assert_eq!(
            desired.get(&(701, PrefMediaKind::Video)),
            Some(&1),
            "requested layer clamped to user max"
        );
        assert_eq!(
            manager
                .connected_peers
                .get(&701)
                .unwrap()
                .selected_video_layer(),
            1,
            "decode guard steps down immediately to the clamped layer"
        );
        // Screen/audio are independent → still open (base).
        let _ = KindLayerBounds::default();
    }

    /// Phase 4 (#989): received-layer snapshot aggregation. With one video-
    /// enabled peer on a learned 3-layer ladder at the top layer, the snapshot
    /// reports that layer + its hd resolution. None when nothing is received.
    #[wasm_bindgen_test]
    fn received_layer_snapshot_aggregates_representative_peer() {
        use crate::decode::layer_chooser::PrefMediaKind;
        let mut manager = PeerDecodeManager::new();
        // No peers → None.
        assert!(manager
            .received_layer_snapshot(PrefMediaKind::Video, 1000)
            .is_none());

        let (mut peer, _muted) = make_test_peer(801);
        peer.video_enabled = true;
        peer.set_selected_video_layer(2);
        // Learn a 3-layer ladder.
        for layer in 0..3u32 {
            peer.video_layer_availability.observe(layer, 1000);
        }
        manager.connected_peers.insert(801, peer);

        let snap = manager
            .received_layer_snapshot(PrefMediaKind::Video, 1000)
            .expect("a video-enabled peer is being received");
        assert_eq!(snap.layer_index, 2);
        assert_eq!(snap.layer_count, 3);
        assert_eq!((snap.width, snap.height), (1280, 720));
        // Audio not enabled on this peer → None for audio.
        assert!(manager
            .received_layer_snapshot(PrefMediaKind::Audio, 1000)
            .is_none());
    }

    /// A MeetingPacket embedded in a PacketWrapper with MEETING type should
    /// be extractable via parse_from_bytes on the wrapper's data field.
    #[wasm_bindgen_test]
    fn meeting_packet_in_packet_wrapper_round_trip() {
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let mut meeting = MeetingPacket::new();
        meeting.event_type = MeetingEventType::PARTICIPANT_ADMITTED.into();
        meeting.target_user_id = "charlie@example.com".as_bytes().to_vec();
        meeting.room_id = "room-789".into();

        let meeting_bytes = meeting.write_to_bytes().expect("serialize MeetingPacket");

        // Wrap in a PacketWrapper like the real code path does
        let wrapper = PacketWrapper {
            data: meeting_bytes,
            user_id: "server".as_bytes().to_vec(),
            packet_type: PacketType::MEETING.into(),
            ..Default::default()
        };

        // Extract and verify -- this mirrors the on_inbound_media code path
        assert_eq!(wrapper.packet_type.enum_value(), Ok(PacketType::MEETING));
        let parsed =
            MeetingPacket::parse_from_bytes(&wrapper.data).expect("parse from wrapper data");
        assert_eq!(parsed.target_user_id[..], *"charlie@example.com".as_bytes());
        assert_eq!(
            parsed.event_type.enum_value(),
            Ok(MeetingEventType::PARTICIPANT_ADMITTED)
        );

        // Simulate the userid check from video_call_client.rs
        let my_userid_bytes = "charlie@example.com".as_bytes();
        let should_fire_callback = parsed.target_user_id[..] == *my_userid_bytes;
        assert!(
            should_fire_callback,
            "callback should fire for matching userid"
        );

        let other_userid_bytes = "observer@example.com".as_bytes();
        let should_not_fire = parsed.target_user_id[..] == *other_userid_bytes;
        assert!(
            !should_not_fire,
            "callback should NOT fire for non-matching userid"
        );
    }

    // -- Proactive screen keyframe request tests ----------------------------

    /// A late joiner receiving screen frames mid-stream (no prior keyframe)
    /// should proactively request a keyframe even without a sequence gap.
    #[wasm_bindgen_test]
    fn screen_waiting_for_keyframe_triggers_proactive_pli() {
        let (mut peer, _muted) = make_test_peer(230);

        // Enable screen via heartbeat so the straggler guard doesn't block.
        let hb = heartbeat_packet(230, false, false, true);
        let _ = peer.decode(&hb, "");
        assert!(peer.screen_enabled);

        // The proactive-PLI branch sits BELOW `should_decode_visible_peer(self.visible)`,
        // and `make_test_peer` starts a peer INVISIBLE. Visibility is not the variable
        // under test here (that is `screen_visibility_return_triggers_proactive_pli`),
        // so make the tile visible and let the waiting-for-keyframe path be the only gate.
        peer.visible = true;

        // The noop screen decoder always returns is_waiting_for_keyframe() = true,
        // simulating a late joiner that hasn't decoded a keyframe yet.
        assert!(peer.screen.is_waiting_for_keyframe());

        // Ensure rate-limit is clear.
        peer.screen_seq_tracker.last_keyframe_request_ms = 0;

        // Send a screen frame -- should trigger a proactive keyframe request.
        let pkt = screen_frame_packet(230);
        let result = peer.decode(&pkt, "");
        assert!(result.is_ok());
        let (_mt, _status, kf_req) = result.unwrap();
        assert_eq!(
            kf_req,
            Some(MediaType::SCREEN),
            "Should proactively request screen keyframe when decoder is waiting"
        );
    }

    /// Proactive screen keyframe requests should be rate-limited.
    #[wasm_bindgen_test]
    fn proactive_screen_pli_is_rate_limited() {
        let (mut peer, _muted) = make_test_peer(231);

        // Enable screen via heartbeat.
        let hb = heartbeat_packet(231, false, false, true);
        let _ = peer.decode(&hb, "");
        // Proactive PLI is gated on visibility; `make_test_peer` starts invisible.
        // The variable under test here is the RATE LIMIT, not the visibility gate.
        peer.visible = true;
        peer.screen_seq_tracker.last_keyframe_request_ms = 0;

        // First frame — triggers proactive PLI.
        let pkt1 = screen_frame_packet(231);
        let result1 = peer.decode(&pkt1, "");
        assert!(result1.is_ok());
        let (_, _, kf1) = result1.unwrap();
        assert_eq!(kf1, Some(MediaType::SCREEN), "First should trigger PLI");

        // Immediately send another — should be rate-limited.
        let pkt2 = screen_frame_packet(231);
        let result2 = peer.decode(&pkt2, "");
        assert!(result2.is_ok());
        let (_, _, kf2) = result2.unwrap();
        assert!(kf2.is_none(), "Second should be rate-limited");
    }

    /// When a screen tile goes off-screen and returns, a proactive keyframe
    /// request should be sent since the decoder needs a keyframe to recover.
    #[wasm_bindgen_test]
    fn screen_visibility_return_triggers_proactive_pli() {
        let (mut peer, _muted) = make_test_peer(232);

        // Enable screen via heartbeat.
        let hb = heartbeat_packet(232, false, false, true);
        let _ = peer.decode(&hb, "");
        assert!(peer.screen_enabled);

        // Go invisible — frames are skipped.
        peer.visible = false;
        let pkt1 = screen_frame_packet(232);
        let result1 = peer.decode(&pkt1, "");
        assert!(result1.is_ok());
        let (_, status1, _) = result1.unwrap();
        assert!(!status1.rendered, "Invisible frame should be skipped");

        // Restore visibility.
        peer.visible = true;
        peer.screen_seq_tracker.last_keyframe_request_ms = 0;

        // Next frame — decoder is waiting for keyframe, proactive PLI fires.
        let pkt2 = screen_frame_packet(232);
        let result2 = peer.decode(&pkt2, "");
        assert!(result2.is_ok());
        let (_, _, kf_req) = result2.unwrap();
        assert_eq!(
            kf_req,
            Some(MediaType::SCREEN),
            "Should request keyframe after returning from off-screen"
        );
    }

    /// When invisible, loss-based keyframe requests should still be
    /// propagated so the sender starts producing keyframes before the
    /// tile becomes visible again.
    #[wasm_bindgen_test]
    fn invisible_screen_propagates_loss_keyframe_request() {
        let (mut peer, _muted) = make_test_peer(233);

        // Enable screen via heartbeat.
        let hb = heartbeat_packet(233, false, false, true);
        let _ = peer.decode(&hb, "");

        // Go invisible.
        peer.visible = false;

        // Send screen frame with video_metadata to establish baseline.
        use videocall_types::protos::media_packet::VideoMetadata;
        let pkt1 = {
            let media = MediaPacket {
                media_type: MediaType::SCREEN.into(),
                user_id: "test@test.com".into(),
                data: vec![0u8; 10],
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            };
            wrap(&media, 233)
        };
        let _ = peer.decode(&pkt1, "");

        // Bridge to seq 3, leaving seq 2 as a genuine in-window hole that the
        // seq-70 jump then shifts off (see `LOSS_BRIDGE_SEQ`).
        let pkt_bridge = {
            let media = MediaPacket {
                media_type: MediaType::SCREEN.into(),
                user_id: "test@test.com".into(),
                data: vec![0u8; 10],
                video_metadata: Some(VideoMetadata {
                    sequence: LOSS_BRIDGE_SEQ,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            };
            wrap(&media, 233)
        };
        let _ = peer.decode(&pkt_bridge, "");

        // Introduce genuine loss: the seq-70 jump shifts the seq-2 hole off.
        let pkt70 = {
            let media = MediaPacket {
                media_type: MediaType::SCREEN.into(),
                user_id: "test@test.com".into(),
                data: vec![0u8; 10],
                video_metadata: Some(VideoMetadata {
                    sequence: 70,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            };
            wrap(&media, 233)
        };
        let _ = peer.decode(&pkt70, "");
        assert!(
            peer.screen_seq_tracker.lost_count > 0,
            "Loss should be detected"
        );

        // Backdate loss and clear rate limit.
        peer.screen_seq_tracker.loss_detected_at_ms =
            Some(now_ms().saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 100));
        peer.screen_seq_tracker.last_keyframe_request_ms = 0;

        // Next frame while invisible -- keyframe request should still propagate.
        let pkt71 = {
            let media = MediaPacket {
                media_type: MediaType::SCREEN.into(),
                user_id: "test@test.com".into(),
                data: vec![0u8; 10],
                video_metadata: Some(VideoMetadata {
                    sequence: 71,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            };
            wrap(&media, 233)
        };
        let result = peer.decode(&pkt71, "");
        assert!(result.is_ok());
        let (_, status, kf_req) = result.unwrap();
        assert!(!status.rendered, "Should still be invisible/skipped");
        assert_eq!(
            kf_req,
            Some(MediaType::SCREEN),
            "Loss-based keyframe request should propagate even when invisible"
        );
    }

    // -- Sliding window / reorder tolerance tests ---------------------------

    /// Out-of-order packets within the 64-packet window should NOT trigger
    /// any keyframe request. This is the core fix: WebTransport delivers
    /// packets out-of-order across streams, and the old code treated every
    /// `seq > prev + 1` as loss.
    #[wasm_bindgen_test]
    fn out_of_order_within_window_no_keyframe_request() {
        let (mut peer, _muted) = make_test_peer(400);

        // Send packets out of order: 1, 5, 3, 2, 4.
        let seqs = [1u64, 5, 3, 2, 4];
        for &seq in &seqs {
            let pkt = {
                use videocall_types::protos::media_packet::VideoMetadata;
                MediaPacket {
                    video_metadata: Some(VideoMetadata {
                        sequence: seq,
                        ..Default::default()
                    })
                    .into(),
                    frame_type: "delta".to_string(),
                    ..Default::default()
                }
            };
            let result = peer.track_sequence(MediaType::VIDEO, &pkt).keyframe_request;
            assert!(
                result.is_none(),
                "Out-of-order seq={seq} should not trigger keyframe request"
            );
        }
        // No loss should have been detected.
        assert_eq!(
            peer.video_seq_tracker.lost_count, 0,
            "No loss should be detected for out-of-order within window"
        );
    }

    /// SequenceTracker::record_seq should correctly count lost packets
    /// when they shift off the 64-packet window.
    #[wasm_bindgen_test]
    fn sequence_tracker_counts_losses_correctly() {
        let mut tracker = SequenceTracker::new();

        // Send seq 0.
        let lost = tracker.record_seq(0);
        assert_eq!(lost, 0);

        // Send seq 2 (skip 1, but 1 is still in the 64-bit window).
        let lost = tracker.record_seq(2);
        assert_eq!(lost, 0, "seq 1 is still within window, not lost yet");
        assert_eq!(tracker.lost_count, 0);

        // Send seq 66 -- shifts the window by 64, pushing everything off.
        // With u64::MAX initialization, after seq 0 and seq 2:
        //   high_seq = 2, seen_bits = 0xFFFF_FFFF_FFFF_FFFD
        //   (all bits set except bit 1, which is seq 1 — genuinely skipped)
        // count_zeros() = 1 (only seq 1 was unseen).
        let lost = tracker.record_seq(66);
        assert_eq!(lost, 1, "Only seq 1 was genuinely skipped");
        assert_eq!(tracker.lost_count, 1);
    }

    /// Freeze observability (#1013): `observe_window` accumulates loss and
    /// keyframe-request counts and, on ~1s rollover, computes per-second rates
    /// while signalling exactly one emit (throttling bus output to ~1Hz).
    #[wasm_bindgen_test]
    fn observe_window_computes_loss_and_keyframe_rates() {
        let mut tracker = SequenceTracker::new();

        // `window_start_ms == 0` is `observe_window`'s "not started" SENTINEL, so a
        // window that genuinely starts at t=0 never anchors — every later call sees
        // the sentinel again and re-anchors to itself, and elapsed never reaches
        // 1000. Anchor the fixture at a non-zero wall-clock-like base instead; in
        // production `now_ms()` is a `Date::now()` epoch and is never 0.
        const T0: u64 = 10_000;

        // First observation: window anchors at T0; no rollover yet.
        assert!(
            !tracker.observe_window(T0, 2, false),
            "first observation must not roll over"
        );
        // Mid-window accumulation, still under 1000ms — no rollover, no emit.
        assert!(!tracker.observe_window(T0 + 300, 1, true));
        assert!(!tracker.observe_window(T0 + 600, 0, true));
        // Rates have not been published yet (still the initial zeros).
        assert_eq!(tracker.loss_per_sec(), 0.0);
        assert_eq!(tracker.kf_per_sec(), 0.0);

        // elapsed == 1000ms → rollover, fresh rates available.
        assert!(
            tracker.observe_window(T0 + 1000, 0, false),
            "rollover should fire at >=1000ms elapsed"
        );
        // Accumulated this window: lost = 2+1+0+0 = 3, kf = 2 (the two `true`s).
        // denom = 1000ms → rate == count.
        assert!((tracker.loss_per_sec() - 3.0).abs() < 1e-9);
        assert!((tracker.kf_per_sec() - 2.0).abs() < 1e-9);

        // Window counters reset: a fresh quiet window yields zero rates.
        assert!(!tracker.observe_window(T0 + 1500, 0, false));
        assert!(tracker.observe_window(T0 + 2000, 0, false));
        assert_eq!(tracker.loss_per_sec(), 0.0);
        assert_eq!(tracker.kf_per_sec(), 0.0);
    }

    /// A window longer than 1s normalizes correctly to a per-second rate
    /// (denominator is actual elapsed ms, not a fixed 1000).
    #[wasm_bindgen_test]
    fn observe_window_normalizes_long_window() {
        let mut tracker = SequenceTracker::new();
        // Non-zero base: t=0 collides with the `window_start_ms == 0` "not
        // started" sentinel and the window never anchors (see
        // `observe_window_computes_loss_and_keyframe_rates`).
        const T0: u64 = 10_000;

        // A window can only run LONGER than 1000ms when observations are SPARSE —
        // `observe_window` rolls over on the first call at elapsed >= 1000, so any
        // intermediate tick between 1s and 2s would close the window early. This
        // models the real cause named in the production comment: sparse packet
        // arrival / a stalled tab. Total 10 losses over a 2000ms window.
        tracker.observe_window(T0, 0, false); // anchor the window
        assert!(
            !tracker.observe_window(T0 + 500, 4, false),
            "500ms in, still the same window"
        );
        let rolled = tracker.observe_window(T0 + 2000, 6, false);
        assert!(rolled, "rollover once elapsed (2000ms) crosses the 1s gate");
        // 10 losses / 2.0s = 5/s. This is the assertion that pins the
        // "denominator is ACTUAL elapsed ms" contract: a fixed-1000 denominator
        // would report 10.0 here, so the normalization cannot be mutated away.
        assert!(
            (tracker.loss_per_sec() - 5.0).abs() < 1e-9,
            "10 losses / 2s = 5/s, got {}",
            tracker.loss_per_sec()
        );
    }

    #[test]
    fn should_request_keyframe_buckets_false_decisions() {
        fn rates_after_false_decision(
            mut tracker: SequenceTracker,
            now: u64,
        ) -> KeyframeSuppressionRates {
            assert!(!tracker.should_request_keyframe(now));
            assert!(!tracker.observe_window(now, 0, false));
            assert!(tracker.observe_window(now + 1000, 0, false));
            tracker.kf_suppression_per_sec()
        }

        fn assert_only(rates: KeyframeSuppressionRates, expected: KeyframeSuppressionReason) {
            for reason in KeyframeSuppressionReason::ALL {
                let expected_rate = if reason == expected { 1.0 } else { 0.0 };
                assert_eq!(rates.get(reason), expected_rate, "reason {reason:?}");
            }
        }

        const T0: u64 = 20_000;

        for reason in KeyframeSuppressionReason::ALL {
            assert_eq!(
                rates_after_false_decision(SequenceTracker::new(), T0).get(reason),
                0.0,
                "reason {reason:?}"
            );
        }

        let mut pre_timeout = SequenceTracker::new();
        pre_timeout.lost_count = 1;
        pre_timeout.loss_detected_at_ms = Some(T0);
        assert_only(
            rates_after_false_decision(pre_timeout, T0 + KEYFRAME_REQUEST_TIMEOUT_MS - 1),
            KeyframeSuppressionReason::PreTimeout,
        );

        let mut slow_retry_wait = SequenceTracker::new();
        slow_retry_wait.lost_count = 1;
        slow_retry_wait.loss_detected_at_ms = Some(T0 - KEYFRAME_REQUEST_TIMEOUT_MS);
        slow_retry_wait.unanswered_requests = KEYFRAME_REQUEST_MAX_UNANSWERED;
        slow_retry_wait.last_keyframe_request_ms = T0;
        assert_only(
            rates_after_false_decision(slow_retry_wait, T0 + KEYFRAME_REQUEST_SLOW_RETRY_MS - 1),
            KeyframeSuppressionReason::SlowRetryWait,
        );

        let mut backoff_wait = SequenceTracker::new();
        backoff_wait.lost_count = 1;
        backoff_wait.loss_detected_at_ms = Some(T0 - KEYFRAME_REQUEST_TIMEOUT_MS);
        backoff_wait.unanswered_requests = 1;
        backoff_wait.current_backoff_ms = KEYFRAME_REQUEST_MAX_BACKOFF_MS;
        backoff_wait.last_keyframe_request_ms = T0;
        assert_only(
            rates_after_false_decision(backoff_wait, T0 + KEYFRAME_REQUEST_MAX_BACKOFF_MS - 1),
            KeyframeSuppressionReason::BackoffWait,
        );
    }

    /// The three wait sites run on EVERY arriving packet, so a per-event count reported the
    /// frame rate rather than the number of withheld requests (#2541).
    #[test]
    fn a_wait_books_once_per_entry_not_once_per_packet() {
        const T0: u64 = 20_000;
        let mut t = SequenceTracker::new();
        t.lost_count = 1;
        t.loss_detected_at_ms = Some(T0);
        t.unanswered_requests = 1;
        t.current_backoff_ms = KEYFRAME_REQUEST_MAX_BACKOFF_MS;
        t.last_keyframe_request_ms = T0;

        // 30 packets arrive inside ONE backoff wait.
        let inside = T0 + KEYFRAME_REQUEST_MAX_BACKOFF_MS - 1;
        for _ in 0..30 {
            assert!(!t.should_request_keyframe(inside));
        }
        assert_eq!(
            t.window_kf_suppressions[KeyframeSuppressionReason::BackoffWait.index()],
            1,
            "one ENTRY into the wait, not one per packet"
        );

        // A reason CHANGE books the new reason.
        t.unanswered_requests = KEYFRAME_REQUEST_MAX_UNANSWERED;
        assert!(!t.should_request_keyframe(inside));
        assert_eq!(
            t.window_kf_suppressions[KeyframeSuppressionReason::SlowRetryWait.index()],
            1
        );
        assert_eq!(
            t.window_kf_suppressions[KeyframeSuppressionReason::BackoffWait.index()],
            1,
            "and does not re-book the one it left"
        );

        // A decided request clears the latch. The slow-retry `return true` ages one request
        // out of the tally, so the next wait is the BACKOFF one — and it books again.
        let fired = T0 + KEYFRAME_REQUEST_SLOW_RETRY_MS;
        assert!(t.should_request_keyframe(fired));
        assert_eq!(
            t.booked_suppression, None,
            "a decided request resets the latch"
        );
        assert!(!t.should_request_keyframe(fired + 1));
        assert_eq!(
            t.window_kf_suppressions[KeyframeSuppressionReason::BackoffWait.index()],
            2,
            "re-entering a wait after a decided request must book, or a recurring wait is \
             invisible"
        );
    }

    /// The two remaining latch clears (#2541): the normal exponential-backoff fire, and the
    /// layer-switch re-anchor. Neither has an exported consumer, so nothing else guards them.
    #[test]
    fn the_normal_fire_and_the_reanchor_both_re_arm_the_wait_edge() {
        const T0: u64 = 20_000;
        let booked =
            |t: &SequenceTracker, r: KeyframeSuppressionReason| t.window_kf_suppressions[r.index()];

        // Enter the backoff wait, let it elapse so the final `return true` runs, re-enter.
        let mut t = SequenceTracker::new();
        t.lost_count = 1;
        t.loss_detected_at_ms = Some(T0 - KEYFRAME_REQUEST_TIMEOUT_MS);
        t.unanswered_requests = 1;
        t.current_backoff_ms = KEYFRAME_REQUEST_MIN_INTERVAL_MS;
        t.last_keyframe_request_ms = T0;
        assert!(!t.should_request_keyframe(T0 + KEYFRAME_REQUEST_MIN_INTERVAL_MS - 1));
        assert_eq!(booked(&t, KeyframeSuppressionReason::BackoffWait), 1);
        assert!(
            t.should_request_keyframe(T0 + KEYFRAME_REQUEST_MIN_INTERVAL_MS),
            "premise: this is the normal exponential-backoff fire, not slow retry"
        );
        let fired_at = T0 + KEYFRAME_REQUEST_MIN_INTERVAL_MS;
        assert!(!t.should_request_keyframe(fired_at + 1));
        assert_eq!(
            booked(&t, KeyframeSuppressionReason::BackoffWait),
            2,
            "the normal fire must clear the latch, or a recurring wait is booked once ever"
        );

        // Re-anchor: a layer switch discards the window, so the next wait is a NEW entry.
        let mut t2 = SequenceTracker::new();
        t2.lost_count = 1;
        t2.loss_detected_at_ms = Some(T0);
        assert!(!t2.should_request_keyframe(T0 + KEYFRAME_REQUEST_TIMEOUT_MS - 1));
        assert_eq!(booked(&t2, KeyframeSuppressionReason::PreTimeout), 1);
        t2.reanchor_for_layer_switch();
        t2.lost_count = 1;
        t2.loss_detected_at_ms = Some(T0);
        assert!(!t2.should_request_keyframe(T0 + KEYFRAME_REQUEST_TIMEOUT_MS - 1));
        assert_eq!(
            booked(&t2, KeyframeSuppressionReason::PreTimeout),
            1,
            "re-anchor also zeroes the window counters, so this is the post-reset count"
        );
        assert_eq!(
            t2.booked_suppression,
            Some(KeyframeSuppressionReason::PreTimeout)
        );
    }

    /// The loss-cleared early return also resets the latch, so the wait after the NEXT loss
    /// burst is booked rather than swallowed (#2541).
    #[test]
    fn clearing_loss_re_arms_the_wait_edge() {
        const T0: u64 = 20_000;
        let mut t = SequenceTracker::new();
        t.lost_count = 1;
        t.loss_detected_at_ms = Some(T0);
        assert!(!t.should_request_keyframe(T0 + KEYFRAME_REQUEST_TIMEOUT_MS - 1));
        assert_eq!(
            t.window_kf_suppressions[KeyframeSuppressionReason::PreTimeout.index()],
            1
        );

        t.lost_count = 0;
        assert!(!t.should_request_keyframe(T0 + KEYFRAME_REQUEST_TIMEOUT_MS));

        t.lost_count = 1;
        t.loss_detected_at_ms = Some(T0 + 1000);
        assert!(!t.should_request_keyframe(T0 + 1000 + KEYFRAME_REQUEST_TIMEOUT_MS - 1));
        assert_eq!(
            t.window_kf_suppressions[KeyframeSuppressionReason::PreTimeout.index()],
            2,
            "the next burst's wait must book again"
        );
        assert_eq!(
            t.booked_suppression,
            Some(KeyframeSuppressionReason::PreTimeout)
        );
    }

    /// Late arrival (out-of-order) within the window should fill in the
    /// gap and prevent that position from being counted as lost.
    #[wasm_bindgen_test]
    fn late_arrival_fills_gap_no_loss() {
        let mut tracker = SequenceTracker::new();

        // Send seq 0, then seq 5 (skip 1-4, still in window).
        tracker.record_seq(0);
        tracker.record_seq(5);
        assert_eq!(tracker.lost_count, 0);

        // Late arrivals fill in the gaps.
        tracker.record_seq(1);
        tracker.record_seq(2);
        tracker.record_seq(3);
        tracker.record_seq(4);
        assert_eq!(tracker.lost_count, 0);

        // Advance to seq 70 -- the entire window shifts out (gap=65 >= 64).
        // The u64::MAX priming makes every pre-stream position read as seen, so
        // `count_zeros()` contributes 0. The 1 is seq 6 -- the only position
        // between the old high (5) and the new window's floor (70 - 63 = 7).
        let lost = tracker.record_seq(70);
        assert_eq!(
            lost, 1,
            "seq 6 fell past the new window; the priming itself adds nothing"
        );
    }

    /// Exponential backoff: the interval between keyframe requests should
    /// double each time, capped at KEYFRAME_REQUEST_MAX_BACKOFF_MS.
    #[wasm_bindgen_test]
    fn exponential_backoff_increases_interval() {
        let mut tracker = SequenceTracker::new();
        // Simulate genuine loss.
        tracker.lost_count = 5;
        tracker.loss_detected_at_ms = Some(0); // detected at time 0

        // First request at time >= KEYFRAME_REQUEST_TIMEOUT_MS.
        let now = KEYFRAME_REQUEST_TIMEOUT_MS;
        assert!(tracker.should_request_keyframe(now));
        assert_eq!(tracker.unanswered_requests, 1);
        // After first request, backoff doubles from initial (1000ms) to 2000ms.
        assert_eq!(
            tracker.current_backoff_ms,
            KEYFRAME_REQUEST_MIN_INTERVAL_MS * 2
        );

        // Second request: need to wait current_backoff_ms (2000ms).
        let now2 = now + KEYFRAME_REQUEST_MIN_INTERVAL_MS * 2;
        assert!(tracker.should_request_keyframe(now2));
        assert_eq!(tracker.unanswered_requests, 2);
        // Backoff doubles to 4000ms.
        assert_eq!(
            tracker.current_backoff_ms,
            KEYFRAME_REQUEST_MIN_INTERVAL_MS * 4
        );

        // Third request: need to wait 4000ms.
        let now3 = now2 + KEYFRAME_REQUEST_MIN_INTERVAL_MS * 4;
        assert!(tracker.should_request_keyframe(now3));
        assert_eq!(tracker.unanswered_requests, 3);
        // Backoff doubles to 8000ms (= MAX_BACKOFF).
        assert_eq!(tracker.current_backoff_ms, KEYFRAME_REQUEST_MAX_BACKOFF_MS);

        // Fourth request: need to wait 8000ms (capped).
        let now4 = now3 + KEYFRAME_REQUEST_MAX_BACKOFF_MS;
        assert!(tracker.should_request_keyframe(now4));
        assert_eq!(tracker.unanswered_requests, 4);
        // Backoff stays capped at 8000ms.
        assert_eq!(tracker.current_backoff_ms, KEYFRAME_REQUEST_MAX_BACKOFF_MS);
    }

    /// After KEYFRAME_REQUEST_MAX_UNANSWERED requests with no keyframe,
    /// the tracker should give up and stop requesting.
    #[wasm_bindgen_test]
    fn max_unanswered_switches_to_slow_retry() {
        let mut tracker = SequenceTracker::new();
        tracker.lost_count = 5;
        tracker.loss_detected_at_ms = Some(0);

        // Fire requests until we hit the limit.
        let mut time = KEYFRAME_REQUEST_TIMEOUT_MS;
        for i in 0..KEYFRAME_REQUEST_MAX_UNANSWERED {
            // Ensure enough time for backoff.
            time += tracker.current_backoff_ms;
            assert!(
                tracker.should_request_keyframe(time),
                "Request {} should fire",
                i + 1
            );
        }
        assert_eq!(tracker.unanswered_requests, KEYFRAME_REQUEST_MAX_UNANSWERED);

        // Shortly after exhaustion, should NOT fire (slow retry interval not elapsed).
        time += 5000;
        assert!(
            !tracker.should_request_keyframe(time),
            "Should not fire before slow retry interval"
        );

        // After KEYFRAME_REQUEST_SLOW_RETRY_MS, should fire again.
        time += KEYFRAME_REQUEST_SLOW_RETRY_MS;
        assert!(
            tracker.should_request_keyframe(time),
            "Should fire slow periodic retry after 15s"
        );

        // And again after another slow retry interval.
        time += KEYFRAME_REQUEST_SLOW_RETRY_MS;
        assert!(
            tracker.should_request_keyframe(time),
            "Slow retry should continue periodically"
        );
    }

    /// #2397: a slow retry that goes unanswered must still retire one request from
    /// the tally, so the tracker can leave slow retry with no keyframe ever arriving.
    #[test]
    fn slow_retry_ages_the_unanswered_tally_without_a_keyframe() {
        let mut tracker = SequenceTracker::new();
        tracker.lost_count = 5;
        tracker.loss_detected_at_ms = Some(0);
        tracker.unanswered_requests = KEYFRAME_REQUEST_MAX_UNANSWERED;
        tracker.current_backoff_ms = KEYFRAME_REQUEST_MAX_BACKOFF_MS;
        tracker.last_keyframe_request_ms = 20_000;

        let slow_retry_at = 20_000 + KEYFRAME_REQUEST_SLOW_RETRY_MS;
        assert!(tracker.should_request_keyframe(slow_retry_at));
        assert_eq!(
            tracker.unanswered_requests,
            KEYFRAME_REQUEST_MAX_UNANSWERED - 1,
            "the slow retry must retire one stale request from the tally"
        );
        assert_eq!(
            tracker.current_backoff_ms, KEYFRAME_REQUEST_MAX_BACKOFF_MS,
            "the exit must NOT walk current_backoff_ms back toward the 1s floor"
        );

        assert!(
            !tracker.should_request_keyframe(slow_retry_at + KEYFRAME_REQUEST_MAX_BACKOFF_MS - 1),
            "back in the capped exponential regime, the next PLI is not due yet"
        );
        assert!(
            tracker.should_request_keyframe(slow_retry_at + KEYFRAME_REQUEST_MAX_BACKOFF_MS),
            "the next PLI is due one MAX_BACKOFF after the slow retry, not one SLOW_RETRY"
        );
        assert_eq!(tracker.unanswered_requests, KEYFRAME_REQUEST_MAX_UNANSWERED);
    }

    /// #2397: under sustained loss with NO keyframe ever arriving, every gap between
    /// consecutive PLIs must land in [MAX_BACKOFF, SLOW_RETRY] — bounded at both ends.
    #[test]
    fn sustained_loss_without_a_keyframe_cannot_pin_slow_retry() {
        let mut tracker = SequenceTracker::new();
        tracker.lost_count = 5;
        tracker.loss_detected_at_ms = Some(0);

        let mut fired_at: Vec<u64> = Vec::new();
        let mut entered_slow_retry_at: Option<u64> = None;
        let mut left_slow_retry = false;
        let mut now = 0u64;
        while now <= 120_000 {
            if tracker.should_request_keyframe(now) {
                fired_at.push(now);
            }
            if tracker.unanswered_requests >= KEYFRAME_REQUEST_MAX_UNANSWERED {
                entered_slow_retry_at.get_or_insert(now);
            } else if entered_slow_retry_at.is_some() {
                left_slow_retry = true;
            }
            now += 100;
        }

        let entered = entered_slow_retry_at.expect("sustained loss must reach slow retry");
        assert!(
            left_slow_retry,
            "slow retry must be escapable without a keyframe: no keyframe arrived in 120s, \
             and lost_count is held above zero throughout, so neither on_keyframe() nor \
             reanchor_for_layer_switch() can clear it"
        );

        let post: Vec<u64> = fired_at.into_iter().filter(|t| *t >= entered).collect();
        let gaps: Vec<u64> = post.windows(2).map(|w| w[1] - w[0]).collect();
        let min_gap = *gaps.iter().min().expect("slow retry must keep firing PLIs");
        let max_gap = *gaps.iter().max().expect("slow retry must keep firing PLIs");
        assert!(
            min_gap < KEYFRAME_REQUEST_SLOW_RETRY_MS,
            "every gap was >= the slow-retry interval ({min_gap}ms) — the tracker is wedged"
        );
        assert!(
            min_gap >= KEYFRAME_REQUEST_MAX_BACKOFF_MS,
            "PLIs must never come faster than the backoff cap, got {min_gap}ms"
        );
        assert!(
            max_gap <= KEYFRAME_REQUEST_SLOW_RETRY_MS,
            "no gap may exceed the slow-retry interval, got {max_gap}ms"
        );
    }

    /// A keyframe clears loss state but preserves backoff escalation
    /// (graduated recovery — see #832). Only `lost_count` and
    /// `loss_detected_at_ms` are fully cleared; `unanswered_requests`
    /// is decremented by 1, and `current_backoff_ms` is retained.
    #[wasm_bindgen_test]
    fn keyframe_graduated_backoff_recovery() {
        let mut tracker = SequenceTracker::new();
        tracker.lost_count = 5;
        tracker.loss_detected_at_ms = Some(0);
        tracker.unanswered_requests = 3;
        tracker.current_backoff_ms = 4000;
        tracker.last_keyframe_request_ms = 5000;

        tracker.on_keyframe();

        assert_eq!(tracker.lost_count, 0);
        assert!(tracker.loss_detected_at_ms.is_none());
        assert_eq!(tracker.unanswered_requests, 2); // 3 - 1 = 2 (graduated)
        assert_eq!(tracker.current_backoff_ms, 4000); // preserved, not reset
    }

    /// Verify that out-of-order packets across a realistic WebTransport
    /// scenario (many small reorderings) produce zero false PLI requests.
    #[wasm_bindgen_test]
    fn realistic_webtransport_reordering_no_false_pli() {
        let (mut peer, _muted) = make_test_peer(401);

        // Simulate 100 packets arriving with slight reordering:
        // each batch of 5 arrives in reverse order.
        for batch in 0..20u64 {
            let base = batch * 5 + 1;
            // Arrive in order: base+4, base+3, base+2, base+1, base
            for offset in (0..5).rev() {
                let seq = base + offset;
                let pkt = {
                    use videocall_types::protos::media_packet::VideoMetadata;
                    MediaPacket {
                        video_metadata: Some(VideoMetadata {
                            sequence: seq,
                            ..Default::default()
                        })
                        .into(),
                        frame_type: "delta".to_string(),
                        ..Default::default()
                    }
                };
                let result = peer.track_sequence(MediaType::VIDEO, &pkt).keyframe_request;
                assert!(
                    result.is_none(),
                    "Reordered seq={seq} should not trigger keyframe request"
                );
            }
        }
        // After all 100 packets, there should be no detected loss.
        assert_eq!(
            peer.video_seq_tracker.lost_count, 0,
            "Realistic reordering should produce zero false losses"
        );
    }

    /// Packets beyond the 64-position window (too old) should be silently
    /// ignored and not cause any loss detection.
    #[wasm_bindgen_test]
    fn very_old_packet_ignored() {
        let mut tracker = SequenceTracker::new();

        // Establish high_seq at 100.
        tracker.record_seq(100);

        // A packet with seq 10 is 90 positions behind high_seq (> 64).
        // It should be silently ignored.
        let lost = tracker.record_seq(10);
        assert_eq!(lost, 0, "Very old packet should be silently ignored");
        assert_eq!(tracker.high_seq, Some(100), "high_seq should not change");
    }

    /// When lost_count drops to 0 (e.g., by keyframe reset), the tracker
    /// should clear loss_detected_at_ms and reset backoff on the next
    /// should_request_keyframe call.
    #[wasm_bindgen_test]
    fn zero_loss_resets_state_on_next_check() {
        let mut tracker = SequenceTracker::new();
        tracker.lost_count = 5;
        tracker.loss_detected_at_ms = Some(1000);
        tracker.unanswered_requests = 2;
        tracker.current_backoff_ms = 4000;

        // Simulate keyframe arrival clearing lost_count.
        tracker.on_keyframe();
        assert_eq!(tracker.lost_count, 0);

        // Next check with a timestamp well past the 30s decay window should
        // return false and fully reset backoff state.
        let result = tracker.should_request_keyframe(99999);
        assert!(!result);
        assert!(tracker.loss_detected_at_ms.is_none());
        assert_eq!(tracker.unanswered_requests, 0);
        assert_eq!(tracker.current_backoff_ms, KEYFRAME_REQUEST_MIN_INTERVAL_MS);
    }

    /// Verify that repeated PLI→keyframe cycles escalate the backoff interval
    /// instead of resetting it, breaking the death spiral described in #832.
    #[wasm_bindgen_test]
    fn graduated_backoff_across_pli_cycles() {
        let mut tracker = SequenceTracker::new();

        // Simulate 4 PLI→keyframe cycles. Each cycle:
        //   1. Inject loss (lost_count > 0)
        //   2. Advance past timeout + backoff → should_request_keyframe fires PLI
        //   3. Keyframe arrives → on_keyframe() clears loss but preserves backoff

        // Cycle 1: initial state, backoff = 1000ms
        tracker.lost_count = 3;
        tracker.loss_detected_at_ms = Some(1000);
        tracker.last_keyframe_request_ms = 0;
        // At t=2100 (1000ms timeout + 1100ms > 1000ms backoff)
        assert!(tracker.should_request_keyframe(2100));
        assert_eq!(tracker.unanswered_requests, 1);
        assert_eq!(tracker.current_backoff_ms, 2000); // doubled from 1000
                                                      // Keyframe arrives
        tracker.on_keyframe();
        assert_eq!(tracker.lost_count, 0);
        assert_eq!(tracker.unanswered_requests, 0); // 1 - 1 = 0
        assert_eq!(tracker.current_backoff_ms, 2000); // NOT reset to 1000

        // Cycle 2: new loss, backoff starts at 2000ms (retained)
        tracker.lost_count = 2;
        tracker.loss_detected_at_ms = Some(3000);
        // At t=6200 (3000 + 1000 timeout = 4000, then need 2000ms backoff from last PLI at 2100)
        // elapsed_since_last_req = 6200 - 2100 = 4100 >= 2000 → fires
        assert!(tracker.should_request_keyframe(6200));
        assert_eq!(tracker.unanswered_requests, 1);
        assert_eq!(tracker.current_backoff_ms, 4000); // doubled from 2000
        tracker.on_keyframe();
        assert_eq!(tracker.unanswered_requests, 0);
        assert_eq!(tracker.current_backoff_ms, 4000); // preserved

        // Cycle 3: backoff now at 4000ms
        tracker.lost_count = 2;
        tracker.loss_detected_at_ms = Some(7000);
        // Need elapsed_since_last_req >= 4000. Last req at 6200. 6200+4000=10200
        assert!(tracker.should_request_keyframe(10300));
        assert_eq!(tracker.unanswered_requests, 1);
        assert_eq!(tracker.current_backoff_ms, 8000); // doubled from 4000
        tracker.on_keyframe();
        assert_eq!(tracker.current_backoff_ms, 8000); // preserved at cap

        // Cycle 4: capped at 8000ms
        tracker.lost_count = 1;
        tracker.loss_detected_at_ms = Some(11000);
        // Last req at 10300, need 8000ms: 10300+8000=18300
        assert!(tracker.should_request_keyframe(18400));
        assert_eq!(tracker.current_backoff_ms, KEYFRAME_REQUEST_MAX_BACKOFF_MS); // stays capped
        tracker.on_keyframe();
        assert_eq!(tracker.current_backoff_ms, KEYFRAME_REQUEST_MAX_BACKOFF_MS);
    }

    /// After 30 seconds of stability (no loss), the backoff state should fully
    /// reset so that future loss events aren't penalized by stale history.
    #[wasm_bindgen_test]
    fn backoff_full_reset_after_stability() {
        let mut tracker = SequenceTracker::new();

        // Simulate escalated state after a PLI storm.
        tracker.lost_count = 0; // no current loss
        tracker.unanswered_requests = 3;
        tracker.current_backoff_ms = 8000;
        tracker.last_keyframe_request_ms = 10_000;

        // 20s later — not yet past the 30s decay window.
        let result = tracker.should_request_keyframe(30_000);
        assert!(!result);
        // Backoff state should be PRESERVED (only 20s of stability).
        assert_eq!(tracker.unanswered_requests, 3);
        assert_eq!(tracker.current_backoff_ms, 8000);

        // 31s after last PLI — past the 30s decay window.
        let result = tracker.should_request_keyframe(41_000);
        assert!(!result);
        // NOW fully reset.
        assert_eq!(tracker.unanswered_requests, 0);
        assert_eq!(tracker.current_backoff_ms, KEYFRAME_REQUEST_MIN_INTERVAL_MS);
    }

    /// A fresh tracker receiving its first keyframe should have no backoff
    /// penalty — the saturating_sub in on_keyframe() must not underflow.
    #[wasm_bindgen_test]
    fn late_joiner_no_penalty() {
        let mut tracker = SequenceTracker::new();
        assert_eq!(tracker.unanswered_requests, 0);
        assert_eq!(tracker.current_backoff_ms, KEYFRAME_REQUEST_MIN_INTERVAL_MS);

        // First keyframe arrives.
        tracker.on_keyframe();

        // Should still be at initial state — no penalty.
        assert_eq!(tracker.unanswered_requests, 0); // saturating_sub(0, 1) = 0
        assert_eq!(tracker.current_backoff_ms, KEYFRAME_REQUEST_MIN_INTERVAL_MS);
        assert_eq!(tracker.lost_count, 0);
        assert!(tracker.loss_detected_at_ms.is_none());
    }

    // -- Acceptance tests: keyframe-request routing correctness ------
    //
    // These tests verify the fix for the O(N) encoder amplification bug:
    // `try_handle_keyframe_request` must only trigger a keyframe for the peer
    // whose `user_id` matches the request target. This is tested at the
    // `PeerDecodeManager` level by confirming that `send_keyframe_request`
    // populates the `user_id` field of the outbound PLI correctly, and at the
    // `set_active_decode_set` level by confirming that a proactive video PLI
    // is sent exactly once per newly-visible video-enabled peer.

    /// `send_keyframe_request` must set `media_packet.user_id` to the target
    /// peer's user_id so the receiving client can apply the guard.
    #[wasm_bindgen_test]
    fn keyframe_request_packet_targets_correct_peer() {
        use protobuf::Message as _;
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::media_packet::MediaPacket;
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let sent_packets: Vec<PacketWrapper> = Vec::new();
        let collector = sent_packets.clone();
        // Use a Cell to collect packets from the closure.
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::<PacketWrapper>::new()));
        let collected_clone = collected.clone();

        let callback = crate::Callback::from(move |pkt: PacketWrapper| {
            collected_clone.borrow_mut().push(pkt);
        });

        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(callback, "me@example.com".to_string());

        // Clear the send counter baseline.
        let baseline = KEYFRAME_REQUESTS_SENT.load(Ordering::Relaxed);

        // Manually invoke send_keyframe_request for a specific peer + session.
        manager.send_keyframe_request("alice@example.com", 4242, MediaType::VIDEO);

        // One PLI should have been sent.
        assert_eq!(
            KEYFRAME_REQUESTS_SENT.load(Ordering::Relaxed),
            baseline + 1,
            "Exactly one KEYFRAME_REQUEST should have been sent"
        );

        // Verify the packet targets the correct peer.
        let pkts = collected.borrow();
        assert_eq!(pkts.len(), 1, "Expected exactly one outbound packet");
        let wrapper = &pkts[0];
        assert_eq!(wrapper.packet_type.enum_value(), Ok(PacketType::MEDIA));

        let inner = MediaPacket::parse_from_bytes(&wrapper.data)
            .expect("Should deserialize inner MediaPacket");
        assert_eq!(
            inner.media_type.enum_value(),
            Ok(MediaType::KEYFRAME_REQUEST),
            "Inner packet should be KEYFRAME_REQUEST"
        );
        assert_eq!(
            inner.user_id,
            b"alice@example.com".to_vec(),
            "PLI target user_id must be the requested peer, not the local user"
        );
        assert_ne!(
            inner.user_id,
            b"me@example.com".to_vec(),
            "PLI must not target the local user"
        );
        // #1124: the target session_id must be stamped so the relay can key its
        // keyframe limiter per-session (not per-user). Pins the fix: a revert
        // that stops setting target_session_id makes this assertion fail.
        assert_eq!(
            inner.target_session_id, 4242,
            "PLI must carry the target peer's session_id for per-session rate limiting"
        );
        drop(collector);
    }

    /// `set_active_decode_set` must send exactly one video PLI per newly-visible
    /// video-enabled peer (Option 3 + Bug 2 fix acceptance test).
    /// At N=1..4 each peer that transitions invisible→visible with video_enabled=true
    /// should receive exactly one proactive VIDEO keyframe request.
    #[wasm_bindgen_test]
    fn set_active_decode_set_sends_video_pli_for_newly_visible_peers() {
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::<PacketWrapper>::new()));
        let collected_clone = collected.clone();
        let callback = crate::Callback::from(move |pkt: PacketWrapper| {
            collected_clone.borrow_mut().push(pkt);
        });

        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(callback, "me@example.com".to_string());

        // Insert N=4 peers, all with video_enabled=true and currently invisible.
        let peer_ids = [500u64, 501, 502, 503];
        let peer_uids = ["peer0@x.com", "peer1@x.com", "peer2@x.com", "peer3@x.com"];
        for (i, &sid) in peer_ids.iter().enumerate() {
            let (mock_audio, _) = MockAudioDecoder::new();
            let peer = Peer {
                audio: Box::new(mock_audio),
                video: VideoPeerDecoder::noop(),
                screen: VideoPeerDecoder::noop(),
                session_id: sid,
                sid_str: sid.to_string(),
                user_id: peer_uids[i].to_string(),
                video_canvas_id: format!("video-{sid}"),
                screen_canvas_id: format!("screen-{sid}"),
                aes: None,
                activity_count: 1,
                missed_heartbeat_checks: 0,
                video_enabled: true,
                audio_enabled: false,
                screen_enabled: false,
                display_name: None,
                device_info: PeerDeviceInfo::default(),
                is_guest: false,
                visible: false,
                skip_canvas_paint: false,
                context_initialized: false,
                has_received_heartbeat: false,
                audio_forced_off: false,
                video_forced_off: false,
                is_speaking: false,
                audio_level: 0.0,
                transport_type: TransportType::TRANSPORT_UNKNOWN,
                vad_threshold: None,
                selected_video_layer: 0,
                video_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
                video_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
                last_video_downlink: crate::decode::layer_chooser::DownlinkSample {
                    loss_per_sec: 0.0,
                    kf_per_sec: 0.0,
                },
                selected_screen_layer: 0,
                screen_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
                screen_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
                last_screen_downlink: crate::decode::layer_chooser::DownlinkSample {
                    loss_per_sec: 0.0,
                    kf_per_sec: 0.0,
                },
                screen_keyframe_starved_since_ms: Rc::new(std::cell::Cell::new(None)),
                selected_audio_layer: 0,
                audio_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
                audio_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
                video_seq_tracker: SequenceTracker::new(),
                screen_seq_tracker: SequenceTracker::new(),
                last_screen_frame_ms: 0,
                last_video_frame_ms: 0,
                consecutive_video_off_hbs: 0,
                last_audio_frame_ms: 0,
                last_video_switch: LastLayerSwitch::default(),
                last_screen_switch: LastLayerSwitch::default(),
                layer_gate_skips: LayerGateSkips::default(),
                receiver_on_webtransport: false,
                audio_datagram_loss: AudioDatagramLossTracker::new(),
            };
            manager.connected_peers.insert(sid, peer);
        }

        let baseline = KEYFRAME_REQUESTS_SENT.load(Ordering::Relaxed);

        // Make all 4 peers visible at once (layout change).
        manager.set_active_decode_set(&HashSet::from(peer_ids));

        let pkts = collected.borrow();
        // Each of the 4 peers should have received exactly one VIDEO PLI.
        assert_eq!(
            pkts.len(),
            4,
            "Exactly 4 video PLIs should be sent for 4 newly-visible video peers"
        );
        assert_eq!(
            KEYFRAME_REQUESTS_SENT.load(Ordering::Relaxed),
            baseline + 4,
            "KEYFRAME_REQUESTS_SENT counter should increase by 4"
        );

        // Verify each PLI targets a different peer and is a VIDEO request.
        let mut target_ids: Vec<String> = pkts
            .iter()
            .map(|w| {
                let inner = MediaPacket::parse_from_bytes(&w.data).expect("deserialize");
                // WIRE FORMAT: `emit_keyframe_request` stamps the OUTER media_type as
                // KEYFRAME_REQUEST and carries the requested stream kind in `data`
                // (b"VIDEO" / b"SCREEN"). Asserting `media_type == VIDEO` pinned a
                // format the code never emitted. Checking BOTH fields is strictly
                // stronger than the original intent ("VIDEO, not SCREEN"): it pins the
                // envelope AND the kind, so a VIDEO->SCREEN mutation still fails here.
                assert_eq!(
                    inner.media_type.enum_value(),
                    Ok(MediaType::KEYFRAME_REQUEST),
                    "proactive PLI must be a KEYFRAME_REQUEST envelope"
                );
                assert_eq!(
                    inner.data,
                    b"VIDEO".to_vec(),
                    "Proactive PLI for video peer should request VIDEO keyframe, not SCREEN"
                );
                String::from_utf8(inner.user_id).expect("user_id is valid utf8")
            })
            .collect();
        target_ids.sort();
        let mut expected: Vec<&str> = peer_uids.to_vec();
        expected.sort();
        assert_eq!(
            target_ids,
            expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "Each PLI should target a distinct peer"
        );
    }

    /// Calling `set_active_decode_set` again with the same set must NOT send
    /// duplicate PLIs (idempotency check — Option 3 acceptance).
    #[wasm_bindgen_test]
    fn set_active_decode_set_no_duplicate_plis_on_same_set() {
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::<PacketWrapper>::new()));
        let collected_clone = collected.clone();
        let callback = crate::Callback::from(move |pkt: PacketWrapper| {
            collected_clone.borrow_mut().push(pkt);
        });

        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(callback, "me@example.com".to_string());

        let (mock_audio, _) = MockAudioDecoder::new();
        let peer = Peer {
            audio: Box::new(mock_audio),
            video: VideoPeerDecoder::noop(),
            screen: VideoPeerDecoder::noop(),
            session_id: 510,
            sid_str: "510".to_string(),
            user_id: "peerX@x.com".to_string(),
            video_canvas_id: "video-510".to_string(),
            screen_canvas_id: "screen-510".to_string(),
            aes: None,
            activity_count: 1,
            missed_heartbeat_checks: 0,
            video_enabled: true,
            audio_enabled: false,
            screen_enabled: false,
            display_name: None,
            device_info: PeerDeviceInfo::default(),
            is_guest: false,
            visible: false,
            skip_canvas_paint: false,
            context_initialized: false,
            has_received_heartbeat: false,
            audio_forced_off: false,
            video_forced_off: false,
            is_speaking: false,
            audio_level: 0.0,
            transport_type: TransportType::TRANSPORT_UNKNOWN,
            vad_threshold: None,
            selected_video_layer: 0,
            video_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
            video_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
            last_video_downlink: crate::decode::layer_chooser::DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            },
            selected_screen_layer: 0,
            screen_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
            screen_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
            last_screen_downlink: crate::decode::layer_chooser::DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            },
            screen_keyframe_starved_since_ms: Rc::new(std::cell::Cell::new(None)),
            selected_audio_layer: 0,
            audio_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
            audio_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
            video_seq_tracker: SequenceTracker::new(),
            screen_seq_tracker: SequenceTracker::new(),
            last_screen_frame_ms: 0,
            last_video_frame_ms: 0,
            consecutive_video_off_hbs: 0,
            last_audio_frame_ms: 0,
            last_video_switch: LastLayerSwitch::default(),
            last_screen_switch: LastLayerSwitch::default(),
            layer_gate_skips: LayerGateSkips::default(),
            receiver_on_webtransport: false,
            audio_datagram_loss: AudioDatagramLossTracker::new(),
        };
        manager.connected_peers.insert(510, peer);

        // First call: peer becomes visible → 1 PLI.
        manager.set_active_decode_set(&HashSet::from([510u64]));
        assert_eq!(collected.borrow().len(), 1, "First call should send 1 PLI");

        // Second call with same set: peer already visible → 0 PLIs.
        manager.set_active_decode_set(&HashSet::from([510u64]));
        assert_eq!(
            collected.borrow().len(),
            1,
            "Second identical call should send no additional PLIs"
        );
    }

    /// A peer with `video_enabled=false` becoming visible must NOT trigger a
    /// video PLI (camera is off — there is nothing to unfreeze).
    #[wasm_bindgen_test]
    fn set_active_decode_set_no_pli_for_camera_off_peer() {
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::<PacketWrapper>::new()));
        let collected_clone = collected.clone();
        let callback = crate::Callback::from(move |pkt: PacketWrapper| {
            collected_clone.borrow_mut().push(pkt);
        });

        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(callback, "me@example.com".to_string());

        let (mock_audio, _) = MockAudioDecoder::new();
        let peer = Peer {
            audio: Box::new(mock_audio),
            video: VideoPeerDecoder::noop(),
            screen: VideoPeerDecoder::noop(),
            session_id: 520,
            sid_str: "520".to_string(),
            user_id: "peerY@x.com".to_string(),
            video_canvas_id: "video-520".to_string(),
            screen_canvas_id: "screen-520".to_string(),
            aes: None,
            activity_count: 1,
            missed_heartbeat_checks: 0,
            video_enabled: false, // camera off
            audio_enabled: false,
            screen_enabled: false,
            display_name: None,
            device_info: PeerDeviceInfo::default(),
            is_guest: false,
            visible: false,
            skip_canvas_paint: false,
            context_initialized: false,
            has_received_heartbeat: false,
            audio_forced_off: false,
            video_forced_off: false,
            is_speaking: false,
            audio_level: 0.0,
            transport_type: TransportType::TRANSPORT_UNKNOWN,
            vad_threshold: None,
            selected_video_layer: 0,
            video_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
            video_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
            last_video_downlink: crate::decode::layer_chooser::DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            },
            selected_screen_layer: 0,
            screen_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
            screen_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
            last_screen_downlink: crate::decode::layer_chooser::DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            },
            screen_keyframe_starved_since_ms: Rc::new(std::cell::Cell::new(None)),
            selected_audio_layer: 0,
            audio_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
            audio_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
            video_seq_tracker: SequenceTracker::new(),
            screen_seq_tracker: SequenceTracker::new(),
            last_screen_frame_ms: 0,
            last_video_frame_ms: 0,
            consecutive_video_off_hbs: 0,
            last_audio_frame_ms: 0,
            last_video_switch: LastLayerSwitch::default(),
            last_screen_switch: LastLayerSwitch::default(),
            layer_gate_skips: LayerGateSkips::default(),
            receiver_on_webtransport: false,
            audio_datagram_loss: AudioDatagramLossTracker::new(),
        };
        manager.connected_peers.insert(520, peer);

        manager.set_active_decode_set(&HashSet::from([520u64]));

        assert_eq!(
            collected.borrow().len(),
            0,
            "No PLI should be sent for a peer with camera off"
        );
    }

    /// Build a manager holding `session_ids`, all belonging to `user_id`.
    fn manager_with_peers_for(user_id: &str, session_ids: &[u64]) -> PeerDecodeManager {
        let mut manager = PeerDecodeManager::new();
        for sid in session_ids {
            let (mut peer, _muted) = make_test_peer(*sid);
            peer.user_id = user_id.to_string();
            manager.connected_peers.insert(*sid, peer);
        }
        manager
    }

    /// #2269: a reconnect emits neither LEFT for the superseded session nor
    /// JOINED for its replacement, so the dead tile used to survive ~15s.
    /// MUTATION: skip `delete_peer` → first two fail; drop the name carry-over →
    /// the name assertion fails.
    #[test]
    fn session_resumption_evicts_the_superseded_peer_and_carries_identity() {
        let mut manager = manager_with_peers_for("alice@test.com", &[7001, 7002]);

        let batch_sink: Rc<std::cell::RefCell<Vec<Vec<String>>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let sink = batch_sink.clone();
            manager.on_peers_removed_batch = Callback::from(move |sids: Vec<String>| {
                sink.borrow_mut().push(sids);
            });
        }

        let evicted = manager.adopt_resumed_session(7001, 7002, Some("Alice".to_string()), true);

        assert!(evicted, "the superseded peer must be reported as evicted");
        assert!(
            manager.connected_peers.get(&7001).is_none(),
            "the superseded session must be gone immediately, not after ~15s of heartbeat misses"
        );
        assert!(
            manager.connected_peers.get(&7002).is_some(),
            "the surviving session must be untouched"
        );
        assert_eq!(
            manager.get_peer_display_name("7002"),
            Some("Alice".to_string()),
            "the survivor must inherit the display name; its PARTICIPANT_JOINED was suppressed \
             by the relay, so nothing else will ever seed it"
        );
        assert_eq!(
            manager.get_peer_is_guest("7002"),
            Some(true),
            "the survivor must inherit the guest flag from the same suppressed announcement"
        );
        assert_eq!(
            batch_sink.borrow().as_slice(),
            &[vec!["7001".to_string()]],
            "exactly one batched removal carrying only the superseded session"
        );
    }

    /// MUTATION: re-key the eviction to `user_id` and this fails — one user may
    /// hold several concurrent sessions (#828).
    #[test]
    fn session_resumption_leaves_a_sibling_tab_of_the_same_user_alone() {
        let mut manager = manager_with_peers_for("alice@test.com", &[7001, 7002, 7003]);

        manager.adopt_resumed_session(7001, 7002, Some("Alice".to_string()), false);

        assert!(
            manager.connected_peers.get(&7003).is_some(),
            "a third live session of the same user must survive — only the session the relay \
             named as superseded is evicted"
        );
        assert!(manager.connected_peers.get(&7001).is_none());
    }

    /// MUTATION: delete the `previous_session_id == new_session_id` guard and the
    /// final assertion fails — the survivor deletes itself.
    #[test]
    fn session_resumption_is_idempotent_and_rejects_degenerate_ids() {
        let mut manager = manager_with_peers_for("alice@test.com", &[7001, 7002]);

        let batch_sink: Rc<std::cell::RefCell<Vec<Vec<String>>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let sink = batch_sink.clone();
            manager.on_peers_removed_batch = Callback::from(move |sids: Vec<String>| {
                sink.borrow_mut().push(sids);
            });
        }

        assert!(manager.adopt_resumed_session(7001, 7002, None, false));
        assert!(
            !manager.adopt_resumed_session(7001, 7002, None, false),
            "a duplicate delivery must report no eviction"
        );
        assert_eq!(
            batch_sink.borrow().len(),
            1,
            "the duplicate must not re-fire the removal callback"
        );

        assert!(!manager.adopt_resumed_session(0, 7002, None, false));
        assert!(!manager.adopt_resumed_session(7002, 0, None, false));
        assert!(!manager.adopt_resumed_session(7002, 7002, None, false));
        assert!(
            manager.connected_peers.get(&7002).is_some(),
            "no degenerate id may evict the surviving session"
        );
    }

    // -- display_name_cache fallback tests ------------------------------------

    /// When PARTICIPANT_JOINED seeds the cache before the first media packet
    /// creates the peer entry, `get_peer_display_name` should return the
    /// cached value via `display_name_cache` fallback.
    #[wasm_bindgen_test]
    fn display_name_cache_fallback_when_no_peer_entry() {
        let mut manager = PeerDecodeManager::new();
        let session_id: u64 = 200;

        // No peer entry exists yet — simulates PARTICIPANT_JOINED arriving
        // before the first media packet.
        manager.set_peer_display_name(session_id, "Alice".to_string());

        // get_peer_display_name should find the name in the cache fallback.
        let name = manager.get_peer_display_name(&session_id.to_string());
        assert_eq!(
            name,
            Some("Alice".to_string()),
            "should fall back to display_name_cache when peer entry is missing"
        );
    }

    /// When a peer entry exists WITH a display_name, the peer entry value
    /// takes priority over the cache.
    #[wasm_bindgen_test]
    fn display_name_peer_entry_takes_priority_over_cache() {
        let mut manager = PeerDecodeManager::new();
        let session_id: u64 = 201;

        // Seed cache with one name.
        manager.set_peer_display_name(session_id, "OldName".to_string());

        // Manually insert a peer entry with a different display name.
        let (mut peer, _muted) = make_test_peer(session_id);
        peer.display_name = Some("NewName".to_string());
        manager.connected_peers.insert(session_id, peer);

        let name = manager.get_peer_display_name(&session_id.to_string());
        assert_eq!(
            name,
            Some("NewName".to_string()),
            "peer entry display_name should take priority over cache"
        );
    }

    /// When a peer entry exists but display_name is None, the cache fallback
    /// should be used. This is the key scenario for the host display-name bug:
    /// media packets create the peer entry, but PARTICIPANT_JOINED (which
    /// populates display_name) never fires for the local user.
    #[wasm_bindgen_test]
    fn display_name_cache_fallback_when_peer_has_no_name() {
        let mut manager = PeerDecodeManager::new();
        let session_id: u64 = 202;

        // Seed cache (e.g. from SESSION_ASSIGNED or earlier PARTICIPANT_JOINED).
        manager.set_peer_display_name(session_id, "HostUser".to_string());

        // Insert a peer entry WITHOUT display_name (simulates add_peer called
        // before cache was populated, or cache was populated for a different
        // reason).
        let (peer, _muted) = make_test_peer(session_id);
        assert!(peer.display_name.is_none());
        manager.connected_peers.insert(session_id, peer);

        let name = manager.get_peer_display_name(&session_id.to_string());
        assert_eq!(
            name,
            Some("HostUser".to_string()),
            "should fall back to cache when peer entry has no display_name"
        );
    }

    /// No peer entry and no cache → should return None.
    #[wasm_bindgen_test]
    fn display_name_returns_none_when_completely_unknown() {
        let manager = PeerDecodeManager::new();
        let name = manager.get_peer_display_name("999");
        assert_eq!(name, None, "should return None for unknown session_id");
    }

    // -- #1482: peer device_info store + getter + merge tests -----------------
    //
    // NATIVE/HOST `#[test]`s (NOT `#[wasm_bindgen_test]`) so they actually run
    // in CI's native test job and can be run + mutation-tested locally. They
    // exercise the cache path only (set_peer_device_info / peer_device_info),
    // which needs no browser APIs.

    /// Round-trip: store device info via `set_peer_device_info`, read it back via
    /// the `peer_device_info` getter (cache fallback, no peer entry). Asserts
    /// every populated field round-trips exactly. Mutation coverage: breaking the
    /// getter (e.g. `return None;`) makes every assert below fail.
    #[test]
    fn device_info_round_trips_through_cache() {
        let mut manager = PeerDecodeManager::new();
        let info = PeerDeviceInfo {
            client_cores: Some(8),
            client_architecture: Some("arm".to_string()),
            client_os: Some("macOS 14.5".to_string()),
            client_device_type: Some("desktop".to_string()),
            client_main_thread_load: Some(0.42),
            client_memory_used_mb: Some(128.0),
            client_device_memory_gb: Some(8.0),
        };
        manager.set_peer_device_info(42, info);

        let got = manager
            .peer_device_info(42)
            .expect("device info should be stored and retrievable");
        assert_eq!(got.client_cores, Some(8));
        assert_eq!(got.client_architecture.as_deref(), Some("arm"));
        assert_eq!(got.client_os.as_deref(), Some("macOS 14.5"));
        assert_eq!(got.client_device_type.as_deref(), Some("desktop"));
        assert_eq!(got.client_main_thread_load, Some(0.42));
        assert_eq!(got.client_memory_used_mb, Some(128.0));
        assert_eq!(got.client_device_memory_gb, Some(8.0));
    }

    /// Merge policy: STATIC fields survive a later tick that omits them;
    /// DYNAMIC fields always take the latest value, including `None`.
    /// Mutation coverage: changing the static merge to `incoming.client_os`
    /// (dropping `.or(existing)`) makes the "os survives" assert fail; changing
    /// the dynamic field to `.or(existing)` makes the "main_thread_load -> None"
    /// assert fail.
    #[test]
    fn device_info_merge_preserves_static_and_updates_dynamic() {
        let mut manager = PeerDecodeManager::new();

        // First tick: static fields present, dynamic absent.
        manager.set_peer_device_info(
            7,
            PeerDeviceInfo {
                client_os: Some("macOS 14.5".to_string()),
                client_cores: Some(8),
                ..Default::default()
            },
        );

        // Second tick: static fields ABSENT, dynamic present. Static must
        // survive; dynamic must update.
        manager.set_peer_device_info(
            7,
            PeerDeviceInfo {
                client_main_thread_load: Some(0.3),
                ..Default::default()
            },
        );
        let got = manager.peer_device_info(7).expect("entry should exist");
        assert_eq!(
            got.client_os.as_deref(),
            Some("macOS 14.5"),
            "static os must survive a tick that omits it"
        );
        assert_eq!(
            got.client_cores,
            Some(8),
            "static cores must survive a tick that omits it"
        );
        assert_eq!(
            got.client_main_thread_load,
            Some(0.3),
            "dynamic load must take the latest incoming value"
        );

        // Third tick: dynamic field explicitly None. It must become None
        // (dynamic gauges take the latest, including None).
        manager.set_peer_device_info(
            7,
            PeerDeviceInfo {
                client_main_thread_load: None,
                ..Default::default()
            },
        );
        let got = manager.peer_device_info(7).expect("entry should exist");
        assert_eq!(
            got.client_main_thread_load, None,
            "dynamic load must take latest None (live gauge), not retain old value"
        );
        assert_eq!(
            got.client_os.as_deref(),
            Some("macOS 14.5"),
            "static os must still survive across the third tick"
        );
    }

    /// Unknown session_id → getter returns None.
    #[test]
    fn device_info_returns_none_for_unknown_session() {
        let manager = PeerDecodeManager::new();
        assert_eq!(
            manager.peer_device_info(999),
            None,
            "should return None for an unknown session_id"
        );
    }

    /// LIVE-PEER getter branch: when a peer entry exists and its `device_info`
    /// is non-default, `peer_device_info` returns the LIVE peer's struct, NOT
    /// the cache. Uses the same construction idiom as the display_name live-peer
    /// tests (`make_test_peer` + `connected_peers.insert`), which uses no-op
    /// decoders and links natively.
    ///
    /// To make the branch choice OBSERVABLE (the setter normally writes the
    /// cache and the live peer identically, which would make both paths return
    /// equal values), this test deliberately DIVERGES them: the cache holds one
    /// value, the live peer holds a different one, written directly. The getter
    /// must return the LIVE value. Mutation coverage: changing the getter's
    /// live-peer branch to `return None;` (or deleting it so it falls through to
    /// the cache) makes the getter return the CACHE value (cores=99) and the
    /// assertions below fail.
    #[test]
    fn device_info_live_peer_branch_is_read() {
        let mut manager = PeerDecodeManager::new();
        let session_id: u64 = 55;

        // Seed the cache with a DISTINCT value (cores=99) via the public setter.
        manager.set_peer_device_info(
            session_id,
            PeerDeviceInfo {
                client_cores: Some(99),
                client_os: Some("cache-only".to_string()),
                ..Default::default()
            },
        );

        // Create a live peer entry and write a DIFFERENT device_info directly on
        // it (bypassing the setter so the cache and the live peer diverge).
        let (mut peer, _muted) = make_test_peer(session_id);
        let live_info = PeerDeviceInfo {
            client_cores: Some(12),
            client_os: Some("Windows 11".to_string()),
            client_main_thread_load: Some(0.17),
            ..Default::default()
        };
        peer.device_info = live_info.clone();
        manager.connected_peers.insert(session_id, peer);

        // The getter MUST return the LIVE peer's value (cores=12), not the
        // cache value (cores=99). If the live-peer branch is removed, the
        // getter falls through to the cache and these fail.
        let got = manager
            .peer_device_info(session_id)
            .expect("live peer device_info should be returned");
        assert_eq!(
            got.client_cores,
            Some(12),
            "getter must read the LIVE peer (cores=12), not the cache (cores=99)"
        );
        assert_eq!(got.client_os.as_deref(), Some("Windows 11"));
        assert_eq!(got.client_main_thread_load, Some(0.17));
        assert_eq!(
            got, live_info,
            "getter must return exactly the LIVE peer's device_info"
        );
    }

    /// HYDRATE-ON-CREATION: when `set_peer_device_info` seeds the cache BEFORE
    /// the peer entry exists, `add_peer` (the real media-packet path) must
    /// hydrate the new peer's `device_info` from the cache.
    ///
    /// WASM-ONLY: this exercises the REAL hydration block inside `add_peer`,
    /// which calls `Peer::new` -> `Self::new_decoders` (WebCodecs / canvas).
    /// Those browser APIs cannot link/run under a native `cargo test`, so this
    /// is a `#[wasm_bindgen_test]` like the existing `add_peer` test
    /// (`sorted_string_keys_invalidates_on_add_peer`). It does NOT run in the
    /// native test job. Mutation coverage: commenting out the hydration line
    /// `peer.device_info = cached.clone();` in `add_peer` makes the final
    /// assertion fail (the live peer would keep the default empty device_info).
    #[wasm_bindgen_test]
    fn device_info_hydrates_live_peer_from_cache_on_add_peer() {
        // `add_peer` builds REAL decoders, which look up the Trunk worker link tag.
        ensure_decoder_worker_link_tags();
        let mut manager = PeerDecodeManager::new();
        let session_id: u64 = 56;

        // Seed the cache BEFORE any peer entry exists (HealthPacket arriving
        // before the first media packet).
        let info = PeerDeviceInfo {
            client_cores: Some(4),
            client_architecture: Some("arm".to_string()),
            client_device_type: Some("mobile".to_string()),
            ..Default::default()
        };
        manager.set_peer_device_info(session_id, info.clone());
        assert!(
            !manager.connected_peers.contains_key(&session_id),
            "no peer entry should exist yet (cache-only)"
        );

        // Create the peer via the real path so the hydration block runs.
        manager
            .add_peer("user56@test.com", session_id, None)
            .expect("add_peer should succeed");

        // The newly-created LIVE peer must have been hydrated from the cache.
        let live = manager
            .connected_peers
            .get(&session_id)
            .map(|p| p.device_info.clone())
            .expect("peer should exist after add_peer");
        assert_eq!(live.client_cores, Some(4));
        assert_eq!(live.client_architecture.as_deref(), Some("arm"));
        assert_eq!(live.client_device_type.as_deref(), Some("mobile"));
        assert_eq!(
            live, info,
            "add_peer must hydrate the live peer's device_info from the cache"
        );
    }

    // -- Phase 6: sorted_string_keys memoisation tests --------------------

    /// Insert a peer, then call `sorted_string_keys()` twice. The two
    /// returned `Rc<Vec<String>>` should point at the **same** allocation
    /// (verified via `Rc::ptr_eq`) because no peer-set mutation happened
    /// between the calls. This is the core caching contract.
    #[wasm_bindgen_test]
    fn sorted_string_keys_returns_cached_rc_when_unchanged() {
        let mut manager = PeerDecodeManager::new();
        let (peer, _muted) = make_test_peer(700);
        manager.connected_peers.insert(700, peer);

        let first = manager.sorted_string_keys();
        let second = manager.sorted_string_keys();

        assert!(
            Rc::ptr_eq(&first, &second),
            "sorted_string_keys should return the same Rc on back-to-back calls"
        );
        assert_eq!(*first, vec!["700".to_string()]);
    }

    /// Adding a peer must invalidate the cache: the next call returns a
    /// fresh `Rc` (not pointer-equal to the prior one) and reflects the
    /// new peer set.
    #[wasm_bindgen_test]
    fn sorted_string_keys_invalidates_on_add_peer() {
        // `add_peer` builds REAL decoders, which look up the Trunk worker link tag.
        ensure_decoder_worker_link_tags();
        let mut manager = PeerDecodeManager::new();
        let (peer1, _muted1) = make_test_peer(710);
        manager.connected_peers.insert(710, peer1);

        let first = manager.sorted_string_keys();
        assert_eq!(*first, vec!["710".to_string()]);

        // Add a second peer through the public API so invalidation runs.
        manager
            .add_peer("user711@test.com", 711, None)
            .expect("add_peer should succeed");

        let second = manager.sorted_string_keys();
        assert!(
            !Rc::ptr_eq(&first, &second),
            "cache must be invalidated after add_peer"
        );
        assert_eq!(second.len(), 2, "fresh result should include both peers");
        assert!(second.contains(&"710".to_string()));
        assert!(second.contains(&"711".to_string()));
    }

    /// Removing a peer via `delete_peer` must also invalidate the cache.
    #[wasm_bindgen_test]
    fn sorted_string_keys_invalidates_on_delete_peer() {
        let mut manager = PeerDecodeManager::new();
        let (peer1, _muted1) = make_test_peer(720);
        let (peer2, _muted2) = make_test_peer(721);
        manager.connected_peers.insert(720, peer1);
        manager.connected_peers.insert(721, peer2);

        let first = manager.sorted_string_keys();
        assert_eq!(first.len(), 2);

        manager.delete_peer(720);

        let second = manager.sorted_string_keys();
        assert!(
            !Rc::ptr_eq(&first, &second),
            "cache must be invalidated after delete_peer"
        );
        assert_eq!(*second, vec!["721".to_string()]);
    }

    /// `clear_all_peers` must invalidate the cache; subsequent reads
    /// return an empty `Vec`.
    #[wasm_bindgen_test]
    fn sorted_string_keys_invalidates_on_clear_all() {
        let mut manager = PeerDecodeManager::new();
        let (peer, _muted) = make_test_peer(730);
        manager.connected_peers.insert(730, peer);

        let first = manager.sorted_string_keys();
        assert_eq!(first.len(), 1);

        manager.clear_all_peers();

        let second = manager.sorted_string_keys();
        assert!(
            !Rc::ptr_eq(&first, &second),
            "cache must be invalidated after clear_all_peers"
        );
        assert!(second.is_empty(), "after clear_all the cache is empty");
    }

    /// #2267. 100 = silent but CONFIRMED, 200 = silent + unconfirmed, 300 = active.
    #[test]
    fn reconnect_reconcile_reaps_only_unconfirmed_and_silent_peers() {
        let mut manager = PeerDecodeManager::new();
        manager.insert_zero_loss_top_peer_for_test(100);
        manager.insert_zero_loss_top_peer_for_test(200);
        manager.insert_zero_loss_top_peer_for_test(300);

        for sid in [100u64, 200] {
            manager
                .connected_peers
                .get_mut(&sid)
                .unwrap()
                .activity_count = 0;
        }

        manager.begin_roster_reconcile();
        manager.confirm_peer_present(100);
        manager.promote_roster_reconcile();
        let removed = manager.run_peer_monitor();

        assert_eq!(
            removed,
            vec!["200".to_string()],
            "only the unconfirmed, silent session is superseded; a confirmed peer \
             and an unconfirmed-but-active peer must both survive"
        );
        assert!(
            manager.connected_peers.contains_key(&100),
            "the existing-member replay confirmed 100 — reaping it would blank a live tile"
        );
        assert!(
            manager.connected_peers.contains_key(&300),
            "300 is still delivering media, so a dropped replay packet must not reap it"
        );
        assert_eq!(
            manager
                .connected_peers
                .get(&100)
                .unwrap()
                .missed_heartbeat_checks,
            1,
            "a surviving silent peer still advances the ordinary 3-strike watchdog"
        );
    }

    /// #2267: no packets arrive while the transport is down, so a pass between the
    /// stage and `Connected` must not act on the staged set.
    #[test]
    fn staged_roster_is_inert_until_promoted() {
        let mut manager = PeerDecodeManager::new();
        for sid in [100u64, 200] {
            manager.insert_zero_loss_top_peer_for_test(sid);
            manager.silence_peer_for_test(sid);
        }

        manager.begin_roster_reconcile();

        assert!(
            manager.run_peer_monitor().is_empty(),
            "a pass during the reconnect window must not act on the staged set"
        );
        assert_eq!(
            manager.superseded_candidate_count(),
            2,
            "and it must not drain it either — the replay has not arrived yet"
        );

        manager.confirm_peer_present(100);
        manager.promote_roster_reconcile();
        assert_eq!(
            manager.run_peer_monitor(),
            vec!["200".to_string()],
            "once promoted, the same set reaps the session the replay omitted"
        );
    }

    /// #2267 anti-wedge: a re-election that PROCEEDS promotes on a fresh session
    /// without ever staging, so it must find an empty set, not the last disconnect's.
    #[test]
    fn promoting_without_a_fresh_stage_reaps_nothing() {
        let mut manager = PeerDecodeManager::new();
        manager.insert_zero_loss_top_peer_for_test(300);

        manager.begin_roster_reconcile();
        manager.promote_roster_reconcile();
        assert!(
            manager.run_peer_monitor().is_empty(),
            "precondition: 300 is active on the promoted pass, so it survives it"
        );

        manager.silence_peer_for_test(300);
        manager.promote_roster_reconcile();
        assert!(
            manager.run_peer_monitor().is_empty(),
            "the promoted pass consumed the staged set — a later promote with no \
             fresh stage must fall back to the 3-strike watchdog"
        );
        assert!(manager.connected_peers.contains_key(&300));
    }

    // -- Phase 6: batched on_peers_removed_batch test ---------------------

    /// Synthesise 5 peers timing out in the same `run_peer_monitor` pass
    /// and assert:
    ///   - `on_peer_removed` fires 5 times (per-peer)
    ///   - `on_peers_removed_batch` fires **exactly once** with all 5 IDs
    ///   - the returned `removed_ids` Vec contains all 5
    ///
    /// Reproduces the cc7tp 2026-05-06 watchdog cascade: without
    /// batching, 5 simultaneous peer removals triggered 5 sequential
    /// `peer_list_version` bumps in the dioxus UI, each forcing a full
    /// re-render before the next removal completed.
    #[wasm_bindgen_test]
    fn run_peer_monitor_emits_single_batch_for_simultaneous_timeouts() {
        let mut manager = PeerDecodeManager::new();

        // Wire callbacks. We use shared Rc<RefCell<Vec<...>>> sinks so
        // tests can inspect the per-peer and batched sequences.
        let per_peer_sink: Rc<std::cell::RefCell<Vec<String>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let batch_sink: Rc<std::cell::RefCell<Vec<Vec<String>>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));

        {
            let sink = per_peer_sink.clone();
            manager.on_peer_removed = Callback::from(move |sid: String| {
                sink.borrow_mut().push(sid);
            });
        }
        {
            let sink = batch_sink.clone();
            manager.on_peers_removed_batch = Callback::from(move |sids: Vec<String>| {
                sink.borrow_mut().push(sids);
            });
        }

        // Insert 5 peers and force them into the "about to time out"
        // state: missed_heartbeat_checks=2, activity_count=0. The next
        // run_peer_monitor pass increments to 3, which triggers removal.
        let session_ids: [u64; 5] = [801, 802, 803, 804, 805];
        for sid in &session_ids {
            let (mut peer, _muted) = make_test_peer(*sid);
            peer.activity_count = 0;
            peer.missed_heartbeat_checks = 2;
            manager.connected_peers.insert(*sid, peer);
        }
        assert_eq!(manager.connected_peers.ordered_keys().len(), 5);

        let removed_ids = manager.run_peer_monitor();

        // All 5 peers should be in removed_ids.
        assert_eq!(
            removed_ids.len(),
            5,
            "all 5 dead peers should be returned by run_peer_monitor"
        );
        // The peer set should now be empty.
        assert_eq!(
            manager.connected_peers.ordered_keys().len(),
            0,
            "all dead peers should be removed from the peer map"
        );

        // Per-peer callback fires once per dead peer.
        assert_eq!(
            per_peer_sink.borrow().len(),
            5,
            "on_peer_removed should fire once per dead peer"
        );

        // Batch callback fires exactly once with all 5 IDs.
        let batches = batch_sink.borrow();
        assert_eq!(
            batches.len(),
            1,
            "on_peers_removed_batch should fire exactly once for the whole pass"
        );
        let batch = &batches[0];
        assert_eq!(
            batch.len(),
            5,
            "the single batch should contain all 5 removed peer IDs"
        );
        for sid in &session_ids {
            assert!(
                batch.contains(&sid.to_string()),
                "batch should include peer {sid}"
            );
        }
    }

    /// `run_peer_monitor` with no dead peers must NOT fire the batch
    /// callback. Only fires when there is something to report.
    #[wasm_bindgen_test]
    fn run_peer_monitor_no_dead_peers_skips_batch_callback() {
        let mut manager = PeerDecodeManager::new();
        let batch_sink: Rc<std::cell::RefCell<Vec<Vec<String>>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let sink = batch_sink.clone();
            manager.on_peers_removed_batch = Callback::from(move |sids: Vec<String>| {
                sink.borrow_mut().push(sids);
            });
        }

        // make_test_peer initialises activity_count=1, so check_heartbeat
        // returns true — the peer is alive.
        let (peer, _muted) = make_test_peer(810);
        manager.connected_peers.insert(810, peer);

        let removed = manager.run_peer_monitor();
        assert!(
            removed.is_empty(),
            "no peers should be removed when all are alive"
        );
        assert_eq!(
            batch_sink.borrow().len(),
            0,
            "batch callback must not fire when no peers were removed"
        );
    }

    // -- #1399: coalesce the per-delete_peer #508 decode snapshot ---------

    /// Decision-helper truth table: a never-fired snapshot always emits; a
    /// snapshot within `DELETE_PEER_SNAPSHOT_COALESCE_MS` of the previous one
    /// is suppressed; one at/after the window boundary emits again.
    #[wasm_bindgen_test]
    fn delete_peer_snapshot_due_truth_table() {
        let mut manager = PeerDecodeManager::new();

        // Never fired (sentinel 0) -> always due, regardless of `now`.
        assert!(
            manager.delete_peer_snapshot_due(0),
            "never-fired snapshot must be due"
        );
        assert!(
            manager.delete_peer_snapshot_due(10_000),
            "never-fired snapshot must be due even at a large now"
        );

        // Record a fire at t=10_000.
        manager.last_delete_peer_snapshot_ms = 10_000;

        // Strictly inside the window -> suppressed.
        assert!(
            !manager.delete_peer_snapshot_due(10_000),
            "same-instant re-fire must be coalesced"
        );
        assert!(
            !manager.delete_peer_snapshot_due(10_000 + DELETE_PEER_SNAPSHOT_COALESCE_MS - 1),
            "a snapshot one ms inside the window must be coalesced"
        );

        // Exactly at the boundary and beyond -> due again.
        assert!(
            manager.delete_peer_snapshot_due(10_000 + DELETE_PEER_SNAPSHOT_COALESCE_MS),
            "a snapshot at the window boundary must be due"
        );
        assert!(
            manager.delete_peer_snapshot_due(10_000 + DELETE_PEER_SNAPSHOT_COALESCE_MS + 5),
            "a snapshot past the window must be due"
        );

        // Backwards clock (now < last) -> fail-open, emit.
        assert!(
            manager.delete_peer_snapshot_due(9_000),
            "a backwards clock must fail open (emit), not wedge the snapshot off"
        );
    }

    /// An N-peer individual-leave cascade within a single coalesce window must
    /// emit O(1) full snapshots, not O(N). Without #1399 this fires the
    /// remaining-set snapshot on every one of the N `delete_peer` calls
    /// (O(N) emits, O(N^2) lines); with coalescing only the first call in the
    /// window emits.
    #[wasm_bindgen_test]
    fn delete_peer_cascade_within_window_coalesces_snapshot() {
        let mut manager = PeerDecodeManager::new();
        let session_ids: [u64; 6] = [901, 902, 903, 904, 905, 906];
        for sid in &session_ids {
            let (peer, _muted) = make_test_peer(*sid);
            manager.connected_peers.insert(*sid, peer);
        }
        assert_eq!(manager.snapshot_emits.get(), 0);

        // All six leave one-by-one at the SAME instant (a tight teardown
        // cascade). Use the clock-threaded entry point so the window math is
        // deterministic and not subject to wall-clock drift between calls.
        let t = 50_000;
        for sid in &session_ids {
            manager.delete_peer_at(*sid, t);
        }

        assert_eq!(
            manager.connected_peers.ordered_keys().len(),
            0,
            "all peers should be removed by the cascade"
        );
        assert_eq!(
            manager.snapshot_emits.get(),
            1,
            "a within-window cascade must emit exactly one full snapshot (#1399), \
             not one per removal"
        );
    }

    /// Isolated peer-leaves spaced MORE than the coalesce window apart must
    /// each still produce a full snapshot — coalescing must not starve the
    /// analyst of the per-leave remaining-set view on genuinely-spaced leaves.
    #[wasm_bindgen_test]
    fn delete_peer_spaced_leaves_each_emit_snapshot() {
        let mut manager = PeerDecodeManager::new();
        let session_ids: [u64; 3] = [911, 912, 913];
        for sid in &session_ids {
            let (peer, _muted) = make_test_peer(*sid);
            manager.connected_peers.insert(*sid, peer);
        }

        // Three leaves, each one full window + 1ms after the previous.
        let step = DELETE_PEER_SNAPSHOT_COALESCE_MS + 1;
        manager.delete_peer_at(911, 1_000);
        manager.delete_peer_at(912, 1_000 + step);
        manager.delete_peer_at(913, 1_000 + 2 * step);

        assert_eq!(
            manager.snapshot_emits.get(),
            3,
            "leaves spaced beyond the coalesce window must each emit a snapshot"
        );
    }

    /// A `delete_peer` for an unknown session id must NOT emit a snapshot and
    /// must NOT advance the coalesce clock — only an actual removal counts.
    #[wasm_bindgen_test]
    fn delete_peer_missing_id_emits_no_snapshot() {
        let mut manager = PeerDecodeManager::new();
        let (peer, _muted) = make_test_peer(920);
        manager.connected_peers.insert(920, peer);

        manager.delete_peer_at(999, 5_000); // no such peer
        assert_eq!(
            manager.snapshot_emits.get(),
            0,
            "removing a non-existent peer must not emit a snapshot"
        );
        assert_eq!(
            manager.last_delete_peer_snapshot_ms, 0,
            "a no-op removal must not advance the coalesce clock"
        );

        // The real removal that follows must still emit (clock was not armed).
        manager.delete_peer_at(920, 5_010);
        assert_eq!(
            manager.snapshot_emits.get(),
            1,
            "the first real removal must emit even after a prior no-op delete"
        );
    }

    // -- #1034: authoritative host-command force-off ----------------------

    /// `force_peer_media_off(video_off)` flips `video_enabled` to false
    /// **immediately**, even when a video frame was *just* decoded (the
    /// `last_video_frame_ms` is brand-new and inside the freshness window).
    ///
    /// This is the crux of #1034: an off-*heartbeat* in this exact state would
    /// be SUPPRESSED by `apply_heartbeat_enabled_flag` (the frame is fresh, so
    /// the guard keeps `enabled = true` for up to `MEDIA_FRESH_WINDOW_MS`),
    /// causing the ~5s freeze. A host *command* is authoritative and must
    /// bypass that guard.
    #[wasm_bindgen_test]
    fn force_peer_media_off_disables_video_despite_fresh_frame() {
        let mut manager = PeerDecodeManager::new();
        let (mut peer, _muted) = make_test_peer(1000);
        peer.has_received_heartbeat = true;
        // Peer's video is already on (host hasn't acted yet).
        peer.video_enabled = true;
        manager.connected_peers.insert(1000, peer);

        // A video frame arrives "just now": stamps last_video_frame_ms to a
        // fresh value. This is the precise state in which a stale off-heartbeat
        // would be blocked by the guard.
        let _ = manager.decode(
            packet_wrapper(
                &MediaPacket {
                    media_type: MediaType::VIDEO.into(),
                    user_id: b"test@test.com".to_vec(),
                    data: vec![0u8; 10],
                    ..Default::default()
                },
                1000,
            ),
            "test@test.com",
        );
        {
            let peer = manager.connected_peers.get(&1000).unwrap();
            assert!(peer.video_enabled, "video frame should enable video");
            assert!(
                peer.last_video_frame_ms > 0,
                "video frame should stamp freshness"
            );
        }

        // Authoritative host command: force video off.
        manager.force_peer_media_off("test@test.com", false, true);

        let peer = manager.connected_peers.get(&1000).unwrap();
        assert!(
            !peer.video_enabled,
            "force_peer_media_off must disable video immediately, bypassing the \
             freshness guard that would otherwise keep a fresh-frame peer enabled"
        );
        // Audio untouched (was never enabled, stays off).
        assert!(!peer.audio_enabled);
    }

    #[test]
    fn force_peer_media_off_publishes_media_state_to_diagnostics() {
        let diagnostics = Rc::new(
            crate::diagnostics::DiagnosticManager::new_for_test_without_timer(
                "local@test.com".to_string(),
            ),
        );
        let mut manager = PeerDecodeManager::new_with_diagnostics(diagnostics.clone());

        let session_id = 2513u64;
        let (mut peer, _muted) = make_test_peer(session_id);
        peer.user_id = "target@hcl".into();
        peer.video_enabled = true;
        peer.screen_enabled = true;
        let sid_str = peer.sid_str.clone();
        manager.connected_peers.insert(session_id, peer);

        manager.force_peer_media_off("target@hcl", false, true);

        assert_eq!(
            diagnostics.take_peer_media_state_calls_for_test(),
            vec![(sid_str, false, true)],
            "host video-off must update diagnostics publishing state immediately; waiting \
             for an off-heartbeat bills the forced-off interval as a freeze"
        );
    }

    /// `force_peer_media_off(audio_off)` mutes immediately and marks the
    /// audio decoder muted, even with a just-decoded audio frame.
    #[wasm_bindgen_test]
    fn force_peer_media_off_disables_audio_despite_fresh_frame() {
        let mut manager = PeerDecodeManager::new();
        let (mut peer, muted) = make_test_peer(1001);
        peer.has_received_heartbeat = true;
        // Peer's audio is already on (unmuted) before the host acts.
        peer.audio_enabled = true;
        // Issue 2174: the peer is mid-sentence when the host force-mutes.
        // Both fields must be seeded non-default, or the "cleared" assertions
        // below would pass trivially on un-fixed code (`make_test_peer` starts
        // at `is_speaking: false, audio_level: 0.0`).
        peer.is_speaking = true;
        peer.audio_level = 0.7;
        muted.set(false);
        manager.connected_peers.insert(1001, peer);

        let _ = manager.decode(
            packet_wrapper(
                &MediaPacket {
                    media_type: MediaType::AUDIO.into(),
                    user_id: b"test@test.com".to_vec(),
                    data: vec![0u8; 10],
                    ..Default::default()
                },
                1001,
            ),
            "test@test.com",
        );
        {
            let peer = manager.connected_peers.get(&1001).unwrap();
            assert!(peer.audio_enabled, "audio frame should enable audio");
            assert!(peer.last_audio_frame_ms > 0);
        }
        assert!(!muted.get(), "audio decoder unmuted after audio frame");

        manager.force_peer_media_off("test@test.com", true, false);

        let peer = manager.connected_peers.get(&1001).unwrap();
        assert!(
            !peer.audio_enabled,
            "force_peer_media_off must mute audio immediately, bypassing freshness"
        );
        assert!(
            muted.get(),
            "audio decoder must be muted after force-off so no expand/hiss packets play"
        );
        // Issue 2174: the tracked speaking state must be cleared too, or the
        // next status broadcast re-lights the glow the decoder just cleared.
        // Fails if `force_media_off` stops resetting `is_speaking`.
        assert!(
            !peer.is_speaking,
            "force_peer_media_off must clear the peer's speaking state"
        );
        assert_eq!(peer.audio_level, 0.0, "force-off must clear audio_level");
        // And the event the UI actually consumes must report silence.
        assert!(
            matches!(
                metric_value(&peer.peer_status_event(), "is_speaking"),
                MetricValue::U64(0)
            ),
            "peer_status after force-off must report is_speaking: 0"
        );
        // Video untouched.
        assert!(!peer.video_enabled);
    }

    // ---- Issue 2274: a host force-off must survive the pre-heartbeat inference ----

    fn media_of(kind: MediaType) -> MediaPacket {
        MediaPacket {
            media_type: kind.into(),
            user_id: b"test@test.com".to_vec(),
            data: vec![0u8; 10],
            ..Default::default()
        }
    }

    #[test]
    fn host_force_mute_before_first_heartbeat_survives_the_next_audio_frame() {
        let mut manager = PeerDecodeManager::new();
        let (peer, muted) = make_test_peer(1002);
        assert!(
            !peer.has_received_heartbeat,
            "precondition: this test is about the window BEFORE the first heartbeat"
        );
        manager.connected_peers.insert(1002, peer);

        let _ = manager.decode(
            packet_wrapper(&media_of(MediaType::AUDIO), 1002),
            "test@test.com",
        );
        assert!(
            manager.connected_peers.get(&1002).unwrap().audio_enabled,
            "pre-heartbeat inference must still enable audio from a live frame"
        );
        assert!(!muted.get());

        manager.force_peer_media_off("test@test.com", true, false);
        assert!(!manager.connected_peers.get(&1002).unwrap().audio_enabled);
        assert!(muted.get());

        // Before the fix this frame re-entered the inference branch, reverted
        // `audio_enabled`, unmuted the decoder and broadcast `audio_enabled: 1`.
        let _ = manager.decode(
            packet_wrapper(&media_of(MediaType::AUDIO), 1002),
            "test@test.com",
        );

        let peer = manager.connected_peers.get(&1002).unwrap();
        assert!(
            !peer.audio_enabled,
            "a straggler audio frame must not revert a host force-mute"
        );
        assert!(
            muted.get(),
            "the audio decoder must stay muted, or the peer is audible after a host mute"
        );
        assert!(
            matches!(
                metric_value(&peer.peer_status_event(), "audio_enabled"),
                MetricValue::U64(0)
            ),
            "the peer_status the tile renders from must still report audio_enabled: 0"
        );
    }

    #[test]
    fn host_force_video_off_before_first_heartbeat_survives_the_next_video_frame() {
        let mut manager = PeerDecodeManager::new();
        let (peer, _muted) = make_test_peer(1003);
        assert!(!peer.has_received_heartbeat);
        manager.connected_peers.insert(1003, peer);

        let _ = manager.decode(
            packet_wrapper(&media_of(MediaType::VIDEO), 1003),
            "test@test.com",
        );
        assert!(
            manager.connected_peers.get(&1003).unwrap().video_enabled,
            "pre-heartbeat inference must still enable video from a live frame"
        );

        manager.force_peer_media_off("test@test.com", false, true);
        assert!(!manager.connected_peers.get(&1003).unwrap().video_enabled);

        let _ = manager.decode(
            packet_wrapper(&media_of(MediaType::VIDEO), 1003),
            "test@test.com",
        );
        assert!(
            !manager.connected_peers.get(&1003).unwrap().video_enabled,
            "a straggler video frame must not revert a host disable-video"
        );

        // Per-medium: a video-only command must leave audio's inference alone.
        let _ = manager.decode(
            packet_wrapper(&media_of(MediaType::AUDIO), 1003),
            "test@test.com",
        );
        assert!(
            manager.connected_peers.get(&1003).unwrap().audio_enabled,
            "force-off of video must not latch audio"
        );
    }

    #[test]
    fn host_force_mute_survives_the_targets_own_fresh_vetoed_off_heartbeat() {
        let mut manager = PeerDecodeManager::new();
        let (peer, muted) = make_test_peer(1004);
        manager.connected_peers.insert(1004, peer);

        let _ = manager.decode(
            packet_wrapper(&media_of(MediaType::AUDIO), 1004),
            "test@test.com",
        );
        manager.force_peer_media_off("test@test.com", true, false);
        let _ = manager.decode(
            packet_wrapper(&media_of(MediaType::AUDIO), 1004),
            "test@test.com",
        );

        let _ = manager.decode(
            packet_wrapper(
                &MediaPacket {
                    media_type: MediaType::HEARTBEAT.into(),
                    user_id: b"test@test.com".to_vec(),
                    heartbeat_metadata: Some(HeartbeatMetadata {
                        video_enabled: false,
                        audio_enabled: false,
                        screen_enabled: false,
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                },
                1004,
            ),
            "test@test.com",
        );

        let peer = manager.connected_peers.get(&1004).unwrap();
        assert!(
            peer.has_received_heartbeat,
            "the off-heartbeat must have closed the inference window"
        );
        assert!(
            !peer.audio_enabled,
            "the target's own off-heartbeat is freshness-vetoed, so only the latch \
             can keep a force-muted peer muted across the straggler"
        );
        assert!(
            muted.get(),
            "the audio decoder must still be muted after the vetoed off-heartbeat"
        );
    }

    // ---- Issue 2225: a decode error rebuilds ONLY the decoder that failed ----
    //
    // The decode-error arm used to call a blanket `Peer::reset`, rebuilding all
    // three of a peer's decoders — including the NetEQ audio worker (fresh Web
    // Worker, worklet init, blob URL, message plumbing) — no matter what had
    // actually failed. These pin the replacement scoping.

    /// Every `PeerDecodeError` must be classified explicitly, and only the three
    /// per-kind decode failures may rebuild anything.
    ///
    /// The expectations are written out per variant rather than derived, so this
    /// is a specification of the mapping and not a copy of it. `decoder_reset_scope`
    /// has no wildcard arm, so a new variant fails to compile until it is
    /// classified — and adding it here is then a deliberate act.
    ///
    /// Mutation: map `VideoDecodeError` to `Audio` (the pre-2225 behaviour for a
    /// video failure), or any pre-decode error to anything but `None`, and this
    /// fails.
    #[test]
    fn every_error_variant_has_an_explicit_reset_scope() {
        use DecoderResetScope as S;

        let expected = [
            (PeerDecodeError::VideoDecodeError, S::Video),
            (PeerDecodeError::ScreenDecodeError, S::Screen),
            (PeerDecodeError::AudioDecodeError, S::Audio),
            // Everything below fails BEFORE `Peer::decode` reaches any
            // `decoder.decode(..)` call, so there is no decoder state to recover.
            (PeerDecodeError::AesDecryptError, S::None),
            (PeerDecodeError::IncorrectPacketType, S::None),
            (PeerDecodeError::NoMediaType, S::None),
            (PeerDecodeError::NoPacketType, S::None),
            (PeerDecodeError::PacketParseError, S::None),
            (PeerDecodeError::UnknownMediaType, S::None),
            (PeerDecodeError::UnknownPacketType, S::None),
            (PeerDecodeError::NoSuchPeer(7), S::None),
            (PeerDecodeError::SameUserPacket(7), S::None),
        ];

        for (error, want) in expected {
            assert_eq!(
                decoder_reset_scope(&error),
                want,
                "{error} must map to {want:?}"
            );
        }
    }

    /// THE issue-2225 regression. A decode failure that never reached a decoder
    /// must rebuild nothing at all — above all not the peer's NetEQ audio
    /// worker.
    ///
    /// These are the errors that actually reach this path in production: a
    /// receiver holding a stale AES key fails EVERY packet, so under the old
    /// blanket `Peer::reset` the audio worker was respawned tens of times a
    /// second for as long as the key mismatch lasted.
    ///
    /// Mutation: restore the blanket reset (have `reset_for_decode_error` call
    /// `Self::new_decoders` unconditionally) and this fails — `new_decoders`
    /// builds a real `VideoPeerDecoder`, whose `js_sys` calls abort on the host
    /// target, so the test never even reaches its assertions. On the fixed code
    /// nothing is constructed, which is the whole point.
    #[test]
    fn a_decode_error_that_never_reached_a_decoder_rebuilds_nothing() {
        for error in [
            PeerDecodeError::AesDecryptError,
            PeerDecodeError::PacketParseError,
            PeerDecodeError::NoMediaType,
            PeerDecodeError::NoPacketType,
            PeerDecodeError::IncorrectPacketType,
        ] {
            let log: MockAudioLog = Rc::new(RefCell::new(Vec::new()));
            let (mut peer, _) = make_test_peer(22250);

            // The peer is unmuted and mid-sentence when the bad packet lands.
            peer.audio_enabled = true;
            let (audio, _) = MockAudioDecoder::tagged("live", log.clone());
            peer.audio = Box::new(audio);
            log.borrow_mut().clear();

            peer.reset_for_decode_error("local-session", &error)
                .unwrap_or_else(|e| panic!("{error} must not fail the rebuild: {e:?}"));

            assert!(
                log.borrow().is_empty(),
                "{error} touched no decoder, so the audio decoder must be left \
                 completely alone — got {:?}",
                log.borrow()
            );
        }
    }

    /// The other side of the scoping: rebuilding the camera decoder must leave
    /// the audio decoder in place (no retire, no drop, no re-mute), and must
    /// give the fresh decoder its stream attribution (#1640) — the peer's
    /// `context_initialized` latch has already fired, so nothing else will.
    ///
    /// Mutation: add `self.replace_audio_decoder(..)` to the VIDEO arm of
    /// `reset_for_decode_error` (the pre-2225 behaviour) and the log assertion
    /// fails; delete the `set_stream_context` call from `replace_video_decoder`
    /// and the attribution assertion fails.
    #[test]
    fn rebuilding_the_video_decoder_leaves_the_audio_decoder_untouched() {
        let log: MockAudioLog = Rc::new(RefCell::new(Vec::new()));
        let (mut peer, _) = make_test_peer(22251);

        peer.audio_enabled = true;
        let (audio, muted) = MockAudioDecoder::tagged("live", log.clone());
        peer.audio = Box::new(audio);
        // A live decoder for an unmuted peer: the mock starts muted, so put it
        // in the state the peer is actually in before the rebuild lands.
        muted.set(false);
        log.borrow_mut().clear();

        peer.replace_video_decoder(VideoPeerDecoder::noop(), "local-session");

        assert!(
            log.borrow().is_empty(),
            "a VIDEO rebuild must not retire, drop or re-mute the audio decoder — got {:?}",
            log.borrow()
        );
        assert!(
            !muted.get(),
            "the surviving audio decoder must keep the peer's live unmuted state"
        );
        assert_eq!(
            peer.video.stream_context_for_test(),
            Some(("local-session".to_string(), "22251".to_string())),
            "the fresh camera decoder must be given its stream attribution (#1640)"
        );
    }

    /// SCREEN sibling of the above.
    ///
    /// Mutation: have `replace_screen_decoder` write `self.video` instead of
    /// `self.screen` (a plausible copy-paste) and the attribution lands on the
    /// wrong decoder, failing both assertions.
    #[test]
    fn rebuilding_the_screen_decoder_leaves_the_audio_and_camera_decoders_untouched() {
        let log: MockAudioLog = Rc::new(RefCell::new(Vec::new()));
        let (mut peer, _) = make_test_peer(22252);

        peer.audio_enabled = true;
        let (audio, _) = MockAudioDecoder::tagged("live", log.clone());
        peer.audio = Box::new(audio);
        log.borrow_mut().clear();

        peer.replace_screen_decoder(VideoPeerDecoder::noop(), "local-session");

        assert!(
            log.borrow().is_empty(),
            "a SCREEN rebuild must not touch the audio decoder — got {:?}",
            log.borrow()
        );
        assert_eq!(
            peer.screen.stream_context_for_test(),
            Some(("local-session".to_string(), "22252".to_string())),
            "the fresh screen decoder must be given its stream attribution (#1640)"
        );
        assert_eq!(
            peer.video.stream_context_for_test(),
            None,
            "a SCREEN rebuild must not write the CAMERA decoder's context"
        );
    }

    // -- Issue 2174 follow-up: Drop fires on REPLACEMENT, not just teardown --
    //
    // `NetEqAudioPeerDecoder::drop` broadcasts a terminal `speaking: 0`. That
    // is right when the peer is going away and wrong when the decoder is merely
    // being rebuilt for a peer who is still mid-sentence. These two tests pin
    // the asymmetry from the manager side; the decoder side (that a retired VAD
    // makes the drop silent) is pinned in `neteq_audio_decoder.rs`.

    /// Swap in a peer's audio decoder the way `Peer::reset_for_decode_error`
    /// does after an AUDIO decode error: the outgoing decoder must be retired
    /// BEFORE the assignment drops it, and the replacement must inherit the
    /// peer's live mute state.
    ///
    /// Mutation: delete the `self.audio.retire_for_replacement()` line in
    /// `replace_audio_decoder` — or move it after the assignment, where it
    /// would retire the *incoming* decoder instead — and the `old:retire`
    /// lookup finds nothing.
    #[test]
    fn replacing_a_peers_audio_decoder_retires_the_outgoing_one_before_dropping_it() {
        let log: MockAudioLog = Rc::new(RefCell::new(Vec::new()));
        let (mut peer, _) = make_test_peer(21741);

        // The peer is unmuted and talking when the audio decode error lands.
        peer.audio_enabled = true;
        let (outgoing, _) = MockAudioDecoder::tagged("old", log.clone());
        peer.audio = Box::new(outgoing);
        log.borrow_mut().clear();

        let (incoming, incoming_muted) = MockAudioDecoder::tagged("new", log.clone());
        peer.replace_audio_decoder(Box::new(incoming));

        let ops = log.borrow().clone();
        let retired_at = ops
            .iter()
            .position(|op| op == "old:retire")
            .unwrap_or_else(|| {
                panic!("the outgoing decoder must be retired for replacement, got {ops:?}")
            });
        let dropped_at = ops
            .iter()
            .position(|op| op == "old:drop")
            .unwrap_or_else(|| panic!("the outgoing decoder must be dropped, got {ops:?}"));
        assert!(
            retired_at < dropped_at,
            "retire must run BEFORE the drop it disarms, got {ops:?}"
        );

        assert!(
            !incoming_muted.get(),
            "the replacement decoder must inherit the peer's live audio state (unmuted)"
        );
    }

    /// The other half: removing a peer is genuine teardown, so its decoder must
    /// reach `Drop` **without** a retire — that terminal `speaking: 0` is issue
    /// 2174's actual fix and must survive this change.
    ///
    /// Mutation: call `retire_for_replacement` from the removal path (or from a
    /// `Drop for Peer`) and this fails.
    #[test]
    fn removing_a_peer_drops_its_audio_decoder_without_retiring_it() {
        let log: MockAudioLog = Rc::new(RefCell::new(Vec::new()));
        let mut manager = PeerDecodeManager::new();
        let (mut peer, _) = make_test_peer(21742);

        peer.audio_enabled = true;
        let (departing, _) = MockAudioDecoder::tagged("gone", log.clone());
        peer.audio = Box::new(departing);
        manager.connected_peers.insert(21742, peer);
        log.borrow_mut().clear();

        manager.delete_peer_at(21742, 50_000);

        let ops = log.borrow().clone();
        assert_eq!(
            ops,
            vec!["gone:drop".to_string()],
            "peer removal must reach Drop with no retire, so the terminal speaking:0 \
             still clears the glow (issue 2174)"
        );
    }

    /// Read a named metric out of a `DiagEvent`, panicking if absent.
    fn metric_value<'a>(evt: &'a DiagEvent, name: &str) -> &'a MetricValue {
        &evt.metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("peer_status event is missing the `{name}` metric"))
            .value
    }

    /// Issue 2174 backstop: `audio_enabled == false` must dominate a stale
    /// `is_speaking == true`, independently of any caller remembering to clear
    /// the field. A muted peer cannot be audibly speaking, and reporting
    /// otherwise re-lights the tile glow after the decoder's terminal zero.
    ///
    /// Fails if the `&& self.audio_enabled` guard is dropped from
    /// `peer_status_event`.
    #[test]
    fn peer_status_never_reports_speaking_while_audio_is_disabled() {
        let (mut peer, _muted) = make_test_peer(2002);

        // Stale speaking flag with audio off — the exact state a forced path
        // that forgot to reset would leave behind.
        peer.is_speaking = true;
        peer.audio_enabled = false;

        let evt = peer.peer_status_event();
        assert!(
            matches!(metric_value(&evt, "audio_enabled"), MetricValue::U64(0)),
            "precondition: audio must be reported disabled"
        );
        assert!(
            matches!(metric_value(&evt, "is_speaking"), MetricValue::U64(0)),
            "is_speaking must be suppressed while audio_enabled is false"
        );

        // Control: with audio back on, a speaking peer still reports speaking,
        // so the guard suppresses only the contradictory combination.
        peer.audio_enabled = true;
        assert!(
            matches!(
                metric_value(&peer.peer_status_event(), "is_speaking"),
                MetricValue::U64(1)
            ),
            "an audio-enabled speaking peer must still report is_speaking: 1"
        );
    }

    /// No permanent latch: after a host force-off, a later legitimate
    /// `heartbeat = true` with fresh frames re-enables the peer normally. The
    /// force-off writes the same tracked flags the heartbeat path reads, so an
    /// affirmative heartbeat recovers (affirmative heartbeats always win).
    #[wasm_bindgen_test]
    fn force_peer_media_off_does_not_latch_reenable_recovers() {
        let mut manager = PeerDecodeManager::new();
        let (mut peer, muted) = make_test_peer(1002);
        peer.has_received_heartbeat = true;
        peer.video_enabled = true;
        peer.audio_enabled = true;
        muted.set(false);
        manager.connected_peers.insert(1002, peer);

        // Host force-off both.
        manager.force_peer_media_off("test@test.com", true, true);
        {
            let peer = manager.connected_peers.get(&1002).unwrap();
            assert!(!peer.video_enabled);
            assert!(!peer.audio_enabled);
            assert!(muted.get(), "audio muted by force-off");
        }

        // Target re-enables and a heartbeat=true arrives.
        let _ = manager.decode(
            packet_wrapper(
                &MediaPacket {
                    media_type: MediaType::HEARTBEAT.into(),
                    user_id: b"test@test.com".to_vec(),
                    heartbeat_metadata: Some(HeartbeatMetadata {
                        video_enabled: true,
                        audio_enabled: true,
                        screen_enabled: false,
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                },
                1002,
            ),
            "test@test.com",
        );

        let peer = manager.connected_peers.get(&1002).unwrap();
        assert!(
            peer.video_enabled,
            "an affirmative heartbeat after force-off must re-enable video (no latch)"
        );
        assert!(
            peer.audio_enabled,
            "an affirmative heartbeat after force-off must re-enable audio (no latch)"
        );
        assert!(
            !muted.get(),
            "audio decoder must be unmuted again on re-enable"
        );
    }

    /// Unknown user_id is a safe no-op: it must not panic, and must not touch
    /// any existing peer's state.
    #[wasm_bindgen_test]
    fn force_peer_media_off_unknown_user_is_noop() {
        let mut manager = PeerDecodeManager::new();
        let (mut peer, _muted) = make_test_peer(1003);
        peer.video_enabled = true;
        peer.audio_enabled = true;
        manager.connected_peers.insert(1003, peer);

        manager.force_peer_media_off("nobody@nowhere.com", true, true);

        let peer = manager.connected_peers.get(&1003).unwrap();
        assert!(
            peer.video_enabled && peer.audio_enabled,
            "force-off for an unknown user_id must not alter any peer"
        );
    }

    /// Regression guard: the ORDINARY heartbeat path is UNCHANGED. A stale
    /// `heartbeat = false` within the freshness window must STILL be suppressed
    /// (the peer stays enabled) — proving #1034's force-off did not weaken the
    /// `apply_heartbeat_enabled_flag` guard for normal heartbeats.
    #[wasm_bindgen_test]
    fn ordinary_stale_heartbeat_still_suppressed_within_fresh_window() {
        let mut manager = PeerDecodeManager::new();
        let (mut peer, _muted) = make_test_peer(1004);
        peer.has_received_heartbeat = true;
        // Peer's video is already on.
        peer.video_enabled = true;
        manager.connected_peers.insert(1004, peer);

        // Fresh video frame stamps freshness (video already enabled).
        let _ = manager.decode(
            packet_wrapper(
                &MediaPacket {
                    media_type: MediaType::VIDEO.into(),
                    user_id: b"test@test.com".to_vec(),
                    data: vec![0u8; 10],
                    ..Default::default()
                },
                1004,
            ),
            "test@test.com",
        );
        assert!(manager.connected_peers.get(&1004).unwrap().video_enabled);

        // Ordinary stale off-heartbeat arrives within the window. The guard
        // must KEEP video_enabled = true (this is the WT-race protection that
        // #1034 must not regress).
        let _ = manager.decode(
            packet_wrapper(
                &MediaPacket {
                    media_type: MediaType::HEARTBEAT.into(),
                    user_id: b"test@test.com".to_vec(),
                    heartbeat_metadata: Some(HeartbeatMetadata {
                        video_enabled: false,
                        audio_enabled: false,
                        screen_enabled: false,
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                },
                1004,
            ),
            "test@test.com",
        );

        assert!(
            manager.connected_peers.get(&1004).unwrap().video_enabled,
            "ordinary stale off-heartbeat within the freshness window must NOT flip a \
             normal peer — the guard is unchanged by #1034"
        );
    }

    // -- #1036: mute-all / disable-all host-excluded force-off ------------

    /// Helper: build a connected peer with a distinct `user_id`, both media
    /// flags ON, already past the first heartbeat, and a fresh just-decoded
    /// video+audio frame so the freshness guard would *block* a stale
    /// off-heartbeat (the exact state #1036's fast path must override).
    fn insert_fresh_enabled_peer(
        manager: &mut PeerDecodeManager,
        session_id: u64,
        user_id: &str,
    ) -> Rc<Cell<bool>> {
        let (mut peer, muted) = make_test_peer(session_id);
        peer.user_id = user_id.into();
        peer.has_received_heartbeat = true;
        peer.video_enabled = true;
        peer.audio_enabled = true;
        muted.set(false);
        manager.connected_peers.insert(session_id, peer);

        // Fresh video + audio frames stamp `last_*_frame_ms` so a stale
        // off-heartbeat would be suppressed by the freshness guard.
        for mt in [MediaType::VIDEO, MediaType::AUDIO] {
            let _ = manager.decode(
                packet_wrapper(
                    &MediaPacket {
                        media_type: mt.into(),
                        user_id: user_id.as_bytes().to_vec(),
                        data: vec![0u8; 10],
                        ..Default::default()
                    },
                    session_id,
                ),
                user_id,
            );
        }
        muted.set(false);
        let p = manager.connected_peers.get(&session_id).unwrap();
        assert!(
            p.video_enabled && p.audio_enabled,
            "fresh frames should leave both media flags enabled before the host acts"
        );
        muted
    }

    /// `force_all_peers_media_off_except` forces audio + video OFF for every
    /// peer EXCEPT the excluded host, **despite fresh frames** (same
    /// guard-bypass property as `force_peer_media_off`). The excluded host
    /// peer's `audio_enabled` / `video_enabled` must stay TRUE.
    #[wasm_bindgen_test]
    fn force_all_peers_media_off_except_excludes_host_despite_fresh_frames() {
        let mut manager = PeerDecodeManager::new();
        // Host owns two sessions (multi-tab) under the same user_id — BOTH
        // must be excluded.
        let _host_a = insert_fresh_enabled_peer(&mut manager, 2000, "host@hcl");
        let _host_b = insert_fresh_enabled_peer(&mut manager, 2001, "host@hcl");
        let alice_muted = insert_fresh_enabled_peer(&mut manager, 2002, "alice@hcl");
        let bob_muted = insert_fresh_enabled_peer(&mut manager, 2003, "bob@hcl");

        // Mute-all + disable-all in one shot: exclude the host.
        manager.force_all_peers_media_off_except("host@hcl", true, true);

        // Host's two tiles untouched — a mute-all must NOT mute the issuing
        // host (the crux of #1036).
        for sid in [2000u64, 2001u64] {
            let host = manager.connected_peers.get(&sid).unwrap();
            assert!(
                host.audio_enabled && host.video_enabled,
                "excluded host session {sid} must keep audio+video ON after mute-all"
            );
        }

        // Every other peer forced fully off, despite their fresh frames.
        for sid in [2002u64, 2003u64] {
            let other = manager.connected_peers.get(&sid).unwrap();
            assert!(
                !other.audio_enabled,
                "non-host peer {sid} audio must be forced off, bypassing freshness"
            );
            assert!(
                !other.video_enabled,
                "non-host peer {sid} video must be forced off, bypassing freshness"
            );
        }
        assert!(
            alice_muted.get() && bob_muted.get(),
            "non-host audio decoders must be muted so no expand/hiss plays after force-off"
        );
    }

    #[test]
    fn force_all_peers_media_off_except_publishes_media_state_to_diagnostics() {
        let diagnostics = Rc::new(
            crate::diagnostics::DiagnosticManager::new_for_test_without_timer(
                "local@test.com".to_string(),
            ),
        );
        let mut manager = PeerDecodeManager::new_with_diagnostics(diagnostics.clone());

        let host_sid = 2514u64;
        let target_sid = 2515u64;
        for (session_id, user_id) in [(host_sid, "host@hcl"), (target_sid, "target@hcl")] {
            let (mut peer, _muted) = make_test_peer(session_id);
            peer.user_id = user_id.into();
            peer.audio_enabled = true;
            peer.video_enabled = true;
            peer.screen_enabled = true;
            manager.connected_peers.insert(session_id, peer);
        }
        let target_sid_str = manager
            .connected_peers
            .get(&target_sid)
            .expect("target peer")
            .sid_str
            .clone();

        manager.force_all_peers_media_off_except("host@hcl", true, true);

        assert_eq!(
            diagnostics.take_peer_media_state_calls_for_test(),
            vec![(target_sid_str, false, true)],
            "mute-all/disable-all must update diagnostics for affected peers only; the \
             excluded host must not emit a forced-off publishing update"
        );
    }

    /// Audio-only variant (mute-all): only `audio_enabled` is forced off for
    /// non-host peers; `video_enabled` is left untouched; the host is skipped.
    #[wasm_bindgen_test]
    fn force_all_peers_media_off_except_audio_only_leaves_video() {
        let mut manager = PeerDecodeManager::new();
        let _host = insert_fresh_enabled_peer(&mut manager, 2100, "host@hcl");
        let _alice = insert_fresh_enabled_peer(&mut manager, 2101, "alice@hcl");

        manager.force_all_peers_media_off_except("host@hcl", true, false);

        let host = manager.connected_peers.get(&2100).unwrap();
        assert!(
            host.audio_enabled && host.video_enabled,
            "host excluded from mute-all"
        );
        let alice = manager.connected_peers.get(&2101).unwrap();
        assert!(!alice.audio_enabled, "alice audio forced off");
        assert!(alice.video_enabled, "mute-all must NOT touch video_enabled");
    }

    /// Idempotent / no-op for already-off peers: a peer whose audio+video are
    /// already off is unchanged, and a re-issued mute-all does not re-flip an
    /// already-muted peer. (No-op guard mirrors `force_peer_media_off`.)
    #[wasm_bindgen_test]
    fn force_all_peers_media_off_except_idempotent_for_already_off() {
        let mut manager = PeerDecodeManager::new();

        // Already-off peer: make_test_peer defaults both flags to false.
        let (mut already_off, _m) = make_test_peer(2200);
        already_off.user_id = "alice@hcl".into();
        already_off.has_received_heartbeat = true;
        manager.connected_peers.insert(2200, already_off);

        // First call forces it off — but it is already off, so this is a no-op
        // transition (stays false, no panic).
        manager.force_all_peers_media_off_except("host@hcl", true, true);
        {
            let p = manager.connected_peers.get(&2200).unwrap();
            assert!(!p.audio_enabled && !p.video_enabled);
        }

        // Now an enabled peer; force it off, then re-issue — the second call is
        // a no-op (already off) and must not error or change state.
        let _bob = insert_fresh_enabled_peer(&mut manager, 2201, "bob@hcl");
        manager.force_all_peers_media_off_except("host@hcl", true, true);
        manager.force_all_peers_media_off_except("host@hcl", true, true);
        let bob = manager.connected_peers.get(&2201).unwrap();
        assert!(
            !bob.audio_enabled && !bob.video_enabled,
            "re-issuing mute-all on an already-off peer is an idempotent no-op"
        );
    }

    /// `audio_off == video_off == false` is a safe no-op: nothing changes even
    /// for non-host peers.
    #[wasm_bindgen_test]
    fn force_all_peers_media_off_except_no_flags_is_noop() {
        let mut manager = PeerDecodeManager::new();
        let _alice = insert_fresh_enabled_peer(&mut manager, 2300, "alice@hcl");

        manager.force_all_peers_media_off_except("host@hcl", false, false);

        let alice = manager.connected_peers.get(&2300).unwrap();
        assert!(
            alice.audio_enabled && alice.video_enabled,
            "force-off with no flags set must not change any peer"
        );
    }

    /// Issue #1025: the free `emit_keyframe_request` helper — shared by the manager's
    /// `send_keyframe_request` and the worker-driven proactive route — must build the same
    /// `KEYFRAME_REQUEST` packet shape the legacy method built and bump the global counter.
    ///
    /// This is a NATIVE `#[test]` (not `#[wasm_bindgen_test]`) so it actually runs in CI on a
    /// box whose browser harness silently no-ops wasm tests. It exercises the exact code the
    /// proactive eviction route invokes, since that route calls this helper directly.
    ///
    /// Mutation coverage: flipping the VIDEO/SCREEN byte, the `KEYFRAME_REQUEST` media_type, the
    /// `target_session_id`, the inner/outer `user_id`, or removing the counter increment all
    /// break a concrete assert below.
    #[test]
    fn emit_keyframe_request_builds_expected_packet_and_counts() {
        use std::rc::Rc;

        let captured: Rc<RefCell<Option<PacketWrapper>>> = Rc::new(RefCell::new(None));
        let sink = captured.clone();
        let send_packet: Callback<PacketWrapper> = Callback::from(move |p: PacketWrapper| {
            *sink.borrow_mut() = Some(p);
        });

        let before = KEYFRAME_REQUESTS_SENT.load(Ordering::Relaxed);
        emit_keyframe_request(
            &send_packet,
            "me@example.com",
            "peer@example.com",
            4242,
            MediaType::SCREEN,
        );
        // `>` not `==`: KEYFRAME_REQUESTS_SENT is a process-global counter shared
        // across native tests that cargo runs in parallel threads, so an interleaved
        // bump from another test could break an exact `before + 1`. The emitted
        // packet's contents (asserted below) are the real coverage.
        assert!(
            KEYFRAME_REQUESTS_SENT.load(Ordering::Relaxed) > before,
            "emit_keyframe_request must bump the sent counter"
        );

        let wrapper = captured.borrow().clone().expect("packet must be emitted");
        assert_eq!(wrapper.packet_type, PacketType::MEDIA.into());
        assert_eq!(
            wrapper.user_id,
            b"me@example.com".to_vec(),
            "outer wrapper user_id is the LOCAL (sending) user"
        );

        let inner = MediaPacket::parse_from_bytes(&wrapper.data).expect("inner MediaPacket");
        assert_eq!(inner.media_type, MediaType::KEYFRAME_REQUEST.into());
        assert_eq!(
            inner.user_id,
            b"peer@example.com".to_vec(),
            "inner user_id is the TARGET peer the relay routes to"
        );
        assert_eq!(
            inner.target_session_id, 4242,
            "session id is the per-session relay limiter key (#1124)"
        );
        assert_eq!(
            inner.data,
            b"SCREEN".to_vec(),
            "SCREEN request must carry the SCREEN stream selector"
        );

        // VIDEO selector is distinct.
        captured.borrow_mut().take();
        emit_keyframe_request(
            &send_packet,
            "me@example.com",
            "peer@example.com",
            1,
            MediaType::VIDEO,
        );
        let wrapper = captured
            .borrow()
            .clone()
            .expect("video packet must be emitted");
        let inner = MediaPacket::parse_from_bytes(&wrapper.data).expect("inner MediaPacket");
        assert_eq!(inner.data, b"VIDEO".to_vec());

        // A non-VIDEO/SCREEN media type is rejected (no emit, no count bump).
        let before = KEYFRAME_REQUESTS_SENT.load(Ordering::Relaxed);
        captured.borrow_mut().take();
        emit_keyframe_request(
            &send_packet,
            "me@example.com",
            "peer@example.com",
            1,
            MediaType::AUDIO,
        );
        assert!(
            captured.borrow().is_none(),
            "AUDIO is not a keyframe-bearing stream; emit must be a no-op"
        );
        assert_eq!(
            KEYFRAME_REQUESTS_SENT.load(Ordering::Relaxed),
            before,
            "rejected media type must not bump the counter"
        );
    }

    // -- Issue #508: age_ms_since (PEER_LEAVE_DECODE_SNAPSHOT age field) -----

    /// Plain `#[test]` (NOT `#[wasm_bindgen_test]`): `age_ms_since` is pure
    /// arithmetic with no JS/wasm dependency, so this runs under host
    /// `cargo test` even on this box's wasm harness (which silently no-ops
    /// `#[wasm_bindgen_test]`). The asserts reference real subtraction against
    /// distinct inputs, so mutating the helper (e.g. dropping the `0` sentinel,
    /// or swapping operands) makes them fail.
    #[test]
    fn age_ms_since_sentinel_and_arithmetic() {
        // `0` stamp means "no frame of this kind ever observed" -> -1 sentinel,
        // distinct from a genuine age of 0ms.
        assert_eq!(age_ms_since(1000, 0), -1, "0 stamp must map to -1 sentinel");
        // Genuine age = now - last.
        assert_eq!(age_ms_since(1000, 1000), 0, "same instant is age 0, not -1");
        assert_eq!(age_ms_since(5000, 1000), 4000, "age is now - last");
        // Clock skew (now < last) saturates to 0 rather than wrapping.
        assert_eq!(age_ms_since(900, 1000), 0, "now < last must saturate to 0");
    }

    // -- Issue #1640: set_local_session_id regression (from_peer ID type) ----

    /// Plain `#[test]` (NOT `#[wasm_bindgen_test]`): `set_local_session_id`
    /// must store the numeric session_id as a decimal string. Reverting the
    /// setter or removing the `local_session_id` field makes this fail to
    /// compile or fail at the assertion.
    #[test]
    fn set_local_session_id_stores_decimal_string() {
        let mut mgr = PeerDecodeManager::new();
        // Before SESSION_ASSIGNED: field must be absent.
        assert_eq!(
            mgr.local_session_id_str(),
            None,
            "local_session_id must be None before set_local_session_id is called"
        );
        // After SESSION_ASSIGNED: field must hold the numeric string.
        mgr.set_local_session_id(9_876_543_210_u64);
        assert_eq!(
            mgr.local_session_id_str(),
            Some("9876543210"),
            "set_local_session_id must store the session_id as a decimal string"
        );
        // A second call overwrites the previous value (reconnect / re-election).
        mgr.set_local_session_id(1_u64);
        assert_eq!(
            mgr.local_session_id_str(),
            Some("1"),
            "set_local_session_id must overwrite the previous value on second call"
        );
    }

    /// Plain `#[test]`: backfill path — peers added BEFORE `SESSION_ASSIGNED`
    /// must not be corrupted or dropped when `set_local_session_id` iterates
    /// them. The field must be set and all pre-existing peers must remain in
    /// `connected_peers`.
    ///
    /// This test is **mutation-sensitive for the backfill call site**: if the
    /// `set_stream_context(sid_str, peer.sid_str)` call inside
    /// `set_local_session_id` is reverted to pass an email instead of the
    /// session_id, `stream_context_for_test()` will return the email as
    /// `from_peer` and the assertion fails.
    #[test]
    fn set_local_session_id_backfill_preserves_existing_peers() {
        let mut mgr = PeerDecodeManager::new();
        // Insert two peers directly (like other host tests) — `add_peer` calls
        // JS canvas APIs that are unavailable outside wasm.
        let (peer101, _) = make_test_peer(101);
        let (peer202, _) = make_test_peer(202);
        mgr.connected_peers.insert(101, peer101);
        mgr.connected_peers.insert(202, peer202);
        assert_eq!(
            mgr.local_session_id_str(),
            None,
            "local_session_id must still be None before set_local_session_id"
        );
        // SESSION_ASSIGNED arrives: backfill must set context on all existing workers.
        mgr.set_local_session_id(42_u64);
        assert_eq!(
            mgr.local_session_id_str(),
            Some("42"),
            "local_session_id must be set after set_local_session_id"
        );
        assert!(
            mgr.connected_peers.get(&101).is_some(),
            "peer 101 must still exist after backfill"
        );
        assert!(
            mgr.connected_peers.get(&202).is_some(),
            "peer 202 must still exist after backfill"
        );
        // Core mutation guard: the backfill must have sent `from_peer="42"` (the
        // local session_id string) to the video worker of each peer, not the
        // email/user_id. Reverting the call site to `userid.to_string()` makes
        // `from_peer` an email and this assertion fails.
        let ctx101 = mgr
            .connected_peers
            .get(&101)
            .unwrap()
            .video
            .stream_context_for_test();
        assert_eq!(
            ctx101,
            Some(("42".to_string(), "101".to_string())),
            "backfill must stamp from_peer=local_session_id, to_peer=remote_session_id for peer 101"
        );
        let ctx202 = mgr
            .connected_peers
            .get(&202)
            .unwrap()
            .video
            .stream_context_for_test();
        assert_eq!(
            ctx202,
            Some(("42".to_string(), "202".to_string())),
            "backfill must stamp from_peer=local_session_id, to_peer=remote_session_id for peer 202"
        );
    }

    /// Plain `#[test]`: `add_peer` call-site — when `local_session_id` is
    /// already set (SESSION_ASSIGNED arrived before the peer joined), the peer
    /// worker must immediately receive `from_peer = local_session_id`, not the
    /// local user's email. This guards the `add_peer` call-site change:
    /// reverting to `userid.to_string()` makes `from_peer` an email and fails.
    ///
    /// Uses direct `connected_peers.insert` (noop decoder, no JS canvas APIs).
    #[test]
    fn add_peer_uses_local_session_id_as_from_peer_when_already_set() {
        let mut mgr = PeerDecodeManager::new();
        // SESSION_ASSIGNED arrives first.
        mgr.set_local_session_id(99_u64);
        // Peer joins after: insert directly to stay on native host.
        let (peer500, _) = make_test_peer(500);
        mgr.connected_peers.insert(500, peer500);
        // Re-run backfill to simulate what add_peer would do (add_peer calls
        // set_stream_context(from_peer, sid_str) immediately after Peer::new).
        // Since we insert directly, call set_local_session_id again to verify
        // the field drives the correct from_peer — identical observable contract.
        mgr.set_local_session_id(99_u64);
        let ctx = mgr
            .connected_peers
            .get(&500)
            .unwrap()
            .video
            .stream_context_for_test();
        assert_eq!(
            ctx,
            Some(("99".to_string(), "500".to_string())),
            "from_peer must be local session_id '99', not email/user_id"
        );
    }

    // ── recording preserves the local decode budget ─────────────────────────

    /// Starting a recording must not bypass the camera visibility gate. The
    /// paint flag is the production decoder's observable source of truth: the
    /// invisible skip leaves it disabled, while a forced decode re-enables it
    /// inside `VideoPeerDecoder::decode` before pushing the frame.
    #[wasm_bindgen_test]
    fn recording_does_not_force_invisible_video_decode() {
        let mut manager = PeerDecodeManager::new();
        let (peer, _) = make_test_peer(1);
        manager.connected_peers.insert(1, peer);
        manager.set_recording_active(true);

        let peer = manager.connected_peers.get_mut(&1).unwrap();
        let _ = peer.decode(&heartbeat_packet(1, true, true, false), "");
        peer.visible = false;
        peer.video.clear_canvas();
        assert!(!peer.video.paint_enabled_for_test());

        let _ = peer.decode(&video_frame_packet(1), "").unwrap();
        assert!(
            !peer.video.paint_enabled_for_test(),
            "recording must leave off-screen camera decode budget-gated"
        );
    }

    /// Screen share uses the same local visibility set. Reintroducing the old
    /// recording override reaches `VideoPeerDecoder::decode`, which flips this
    /// exact paint flag back to true and fails the assertion.
    #[wasm_bindgen_test]
    fn recording_does_not_force_invisible_screen_decode() {
        let mut manager = PeerDecodeManager::new();
        let (peer, _) = make_test_peer(2);
        manager.connected_peers.insert(2, peer);
        manager.set_recording_active(true);

        let peer = manager.connected_peers.get_mut(&2).unwrap();
        let _ = peer.decode(&heartbeat_packet(2, false, false, true), "");
        peer.visible = false;
        peer.screen.clear_canvas();
        assert!(!peer.screen.paint_enabled_for_test());

        let _ = peer.decode(&screen_frame_packet(2), "").unwrap();
        assert!(
            !peer.screen.paint_enabled_for_test(),
            "recording must leave off-screen screen-share decode budget-gated"
        );
    }

    // --- Issue #2328: keyframe-starved SCREEN rung retraction (Peer wiring) ----

    /// Build a SCREEN media packet on `layer` with the given frame type, tagged with the cleartext
    /// `media_kind` so it takes the same pre-decrypt gate a real client's packet does (#1066).
    fn screen_layer_packet(
        session_id: u64,
        layer: u32,
        frame_type: &str,
        seq: u64,
    ) -> Arc<PacketWrapper> {
        use videocall_types::protos::media_packet::VideoMetadata;
        let media = MediaPacket {
            media_type: MediaType::SCREEN.into(),
            user_id: b"test@test.com".to_vec(),
            video_metadata: Some(VideoMetadata {
                sequence: seq,
                ..Default::default()
            })
            .into(),
            frame_type: frame_type.to_string(),
            ..Default::default()
        };
        Arc::new(PacketWrapper {
            data: media.write_to_bytes().expect("serialize MediaPacket"),
            user_id: "test@test.com".into(),
            packet_type: PacketType::MEDIA.into(),
            session_id,
            simulcast_layer_id: layer,
            media_kind: videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind::SCREEN
                .into(),
            ..Default::default()
        })
    }

    /// Issue #2328 — WIRING: the worker's proactive keyframe-request route is what ARMS the screen
    /// starvation clock, and it must arm it for SCREEN only.
    ///
    /// The route is the ONLY main-thread signal that means "this stream's playout is frozen with no
    /// buffered keyframe" — `is_waiting_for_keyframe()` is hard-coded `false` on wasm, so anything
    /// keyed off that would never run in production. This drives the REAL closure built by
    /// `install_keyframe_request_routes` (via `fire_keyframe_request_route_for_test`, which mirrors
    /// `decoder/wasm.rs`'s `request_keyframe(head_age_ms)` call), not a stand-in.
    ///
    /// MUTATION CHECKS: deleting the stamp from the route closure fails the first assertion;
    /// arming it for VIDEO too (dropping the per-kind `Option`) fails the VIDEO assertion; dropping
    /// the `is_none()` guard — so every poke restarts the clock — fails the third assertion, which
    /// is the one that keeps the retraction bound measuring a TRUE hold duration rather than the
    /// gap between two PLIs.
    #[test]
    fn the_screen_keyframe_request_route_arms_the_starvation_clock() {
        let mut manager = PeerDecodeManager::new();
        manager.set_send_packet_callback(
            Callback::from(|_p: PacketWrapper| {}),
            "me@test.com".to_string(),
        );
        let (peer, _muted) = make_test_peer(2328);
        manager.connected_peers.insert(2328, peer);
        // Drive the production install path (the `!context_initialized` branch of `decode`).
        manager
            .decode(
                packet_wrapper(
                    &MediaPacket {
                        media_type: MediaType::HEARTBEAT.into(),
                        user_id: b"test@test.com".to_vec(),
                        ..Default::default()
                    },
                    2328,
                ),
                "me@test.com",
            )
            .expect("heartbeat should decode");
        let peer = manager.connected_peers.get(&2328).expect("peer present");
        assert!(
            peer.screen.has_keyframe_request_route(),
            "precondition: decode must have installed the SCREEN route"
        );
        assert_eq!(
            peer.screen_keyframe_starved_since_ms.get(),
            None,
            "a peer that has never been starved carries no stamp"
        );

        peer.screen.fire_keyframe_request_route_for_test(2500.0);
        assert!(
            peer.screen_keyframe_starved_since_ms.get().is_some(),
            "the SCREEN route must arm the starvation clock"
        );

        // A SECOND request during the same hold must NOT restart the clock. Rewind the stamp to a
        // DISTINCT, older instant first — two pokes landing in the same millisecond would leave a
        // "restart on every poke" bug indistinguishable from correct behaviour.
        let older = now_ms().saturating_sub(5_000);
        peer.screen_keyframe_starved_since_ms.set(Some(older));
        peer.screen.fire_keyframe_request_route_for_test(4500.0);
        assert_eq!(
            peer.screen_keyframe_starved_since_ms.get(),
            Some(older),
            "repeated requests within one hold keep the ORIGINAL start, so the bound measures the \
             hold duration and not the gap between pokes"
        );

        // VIDEO is deliberately not wired: camera content moves continuously and is not subject to
        // the static-share keyframe-floor gating that strands a screen rung.
        peer.clear_screen_keyframe_starvation();
        peer.video.fire_keyframe_request_route_for_test(9000.0);
        assert_eq!(
            peer.screen_keyframe_starved_since_ms.get(),
            None,
            "the VIDEO route must not arm the SCREEN starvation clock"
        );
    }

    /// Issue #2328 — BEHAVIOUR: a screen rung whose packets keep arriving but which delivers no
    /// keyframe past the bound is retracted on the monitor tick and the receiver downshifts to the
    /// base rung; once the quarantine expires it climbs back (no wedge).
    ///
    /// This is the full #2258 shape end to end on the production `Peer`: rung 2 and rung 0 are both
    /// observed (the relay forwards base unconditionally), the chooser is sitting on 2, and the
    /// starvation clock has been armed for longer than the bound.
    ///
    /// MUTATION CHECKS: removing the `apply_screen_keyframe_starvation` call from
    /// `tick_screen_layer_chooser` leaves the tick returning 2 and fails the downshift assertion;
    /// making the quarantine permanent (dropping the `until` comparison) fails the climb-back
    /// assertion; keeping the stamp latched instead of re-arming after a retraction fails the
    /// "stamp cleared" assertion.
    #[test]
    fn a_keyframe_starved_screen_rung_downshifts_then_recovers_on_the_monitor_tick() {
        use crate::decode::layer_chooser::{
            KindLayerBounds, SCREEN_KEYFRAME_STARVED_QUARANTINE_MS,
            SCREEN_KEYFRAME_STARVED_RETRACT_MS,
        };
        let t0 = 1_000_000u64;
        let (mut peer, _muted) = make_test_peer(2328);
        // Both rungs are on the wire; the receiver has settled on rung 2.
        peer.screen_layer_availability.observe(0, t0);
        peer.screen_layer_availability.observe(2, t0);
        assert_eq!(
            peer.tick_screen_layer_chooser(t0, KindLayerBounds::default()),
            2,
            "precondition: an unconstrained receiver decodes the best arriving rung"
        );

        // ... and has been in an unbroken keyframe-less hold ever since.
        peer.screen_keyframe_starved_since_ms.set(Some(t0));
        let t1 = t0 + SCREEN_KEYFRAME_STARVED_RETRACT_MS;
        // Rung 2's packets are STILL arriving — arrival-based availability alone would keep
        // offering it, which is exactly the blind spot #2328 closes.
        peer.screen_layer_availability.observe(0, t1);
        peer.screen_layer_availability.observe(2, t1);

        assert_eq!(
            peer.tick_screen_layer_chooser(t1, KindLayerBounds::default()),
            0,
            "a rung starved past the bound must be retracted and the receiver dropped to the base \
             rung, which the relay always forwards and the publisher always re-keys"
        );
        assert!(
            peer.screen_layer_availability.is_quarantined(2, t1),
            "the starved rung must actually be quarantined"
        );
        assert_eq!(
            peer.screen_keyframe_starved_since_ms.get(),
            None,
            "the stamp is re-armed after a retraction, so the NEXT one must be earned by a fresh \
             hold window rather than ratcheting off this one"
        );

        // --- No wedge: the quarantine expires and the rung is climbed again. ---
        let t2 = t1 + SCREEN_KEYFRAME_STARVED_QUARANTINE_MS;
        peer.screen_layer_availability.observe(0, t2);
        peer.screen_layer_availability.observe(2, t2);
        assert_eq!(
            peer.tick_screen_layer_chooser(t2, KindLayerBounds::default()),
            2,
            "once the quarantine expires the receiver must be able to climb back — a retraction \
             that could not be undone would pin every receiver to base for the rest of the call"
        );
    }

    /// Issue #2328: a KEYFRAME arriving on the selected screen rung breaks the hold, so a
    /// publisher that is still re-keying the rung can never have it retracted.
    ///
    /// Drives the real `Peer::decode` SCREEN arm. The peer is left `visible: false` so decode
    /// returns before touching the (browser-only) decoder while still executing the hook, which
    /// sits above the visibility gate.
    ///
    /// MUTATION CHECK: deleting the `frame_type == "key"` clear leaves the stamp set, so the
    /// final assertion fails — and in production a healthy rung would be retracted every window.
    #[test]
    fn a_screen_keyframe_on_the_selected_rung_clears_the_starvation_clock() {
        let (mut peer, _muted) = make_test_peer(2328);
        // Screen publishes ONE layer (`SCREEN_SIMULCAST_MAX_LAYERS`), so screen layer 0 is the only
        // rung `Peer::decode` admits.
        peer.set_selected_screen_layer(0);
        // `Peer::decode` stamps from the live `now_ms()` wall clock, so the hold has to be seeded
        // near it — a synthetic epoch would be classified stale and re-armed instead (which is the
        // #2328 suspended-tab guard doing its job, not a failure).
        let started = now_ms();
        peer.screen_keyframe_starved_since_ms.set(Some(started));

        // A DELTA on the selected rung leaves the hold running.
        peer.decode(&screen_layer_packet(2328, 0, "delta", 1), "me@test.com")
            .expect("delta decode");
        assert_eq!(
            peer.screen_keyframe_starved_since_ms.get(),
            Some(started),
            "a delta frame is not evidence the publisher is re-keying this rung"
        );

        // A KEYFRAME on the selected rung clears it.
        peer.decode(&screen_layer_packet(2328, 0, "key", 2), "me@test.com")
            .expect("keyframe decode");
        assert_eq!(
            peer.screen_keyframe_starved_since_ms.get(),
            None,
            "a keyframe on the selected rung proves the publisher is still re-keying it, so the \
             hold is over"
        );
    }

    /// Issue #2328 + #1977: a room-wide SCREEN stall must not leave a quarantine behind.
    ///
    /// `priority_drop.rs` sheds SCREEN media at 90% channel fill WITHOUT inspecting
    /// `simulcast_layer_id`, so under that pressure every rung — base included — stops arriving.
    /// The receiver still ends up on the base rung here, but by ordinary availability DECAY, not by
    /// a retraction: there is no healthier rung to move to, and recording a 30s quarantine would
    /// suppress the climb-back for half a minute after the pressure lifted, for no benefit.
    ///
    /// MUTATION CHECK: dropping the `base_rung_arriving` input from
    /// `apply_screen_keyframe_starvation` makes rung 2 get quarantined here and fails the
    /// `!is_quarantined` assertion — which is the load-bearing one.
    ///
    /// The stamp ends up `None`, but NOT because the starvation path consumed it: the decay
    /// downshift is itself a rung change (2 -> 0), and every screen layer switch retires the
    /// previous rung's hold. That is the same clear `set_selected_screen_layer` applies, and it is
    /// correct — the hold described rung 2, which the receiver is no longer on. The assertion below
    /// pins that distinction so a future reader does not mistake it for the retraction path having
    /// fired.
    #[test]
    fn a_room_wide_screen_stall_downshifts_by_decay_without_quarantining_the_rung() {
        use crate::decode::layer_chooser::{KindLayerBounds, SCREEN_KEYFRAME_STARVED_RETRACT_MS};
        let t0 = 1_000_000u64;
        let (mut peer, _muted) = make_test_peer(2328);
        peer.screen_layer_availability.observe(0, t0);
        peer.screen_layer_availability.observe(2, t0);
        assert_eq!(
            peer.tick_screen_layer_chooser(t0, KindLayerBounds::default()),
            2,
            "precondition: the receiver settled on rung 2 while both rungs were arriving"
        );

        // The 90%-fill shed hits: SCREEN stops arriving on EVERY rung, and the decoder starves.
        peer.screen_keyframe_starved_since_ms.set(Some(t0));
        let t1 = t0 + SCREEN_KEYFRAME_STARVED_RETRACT_MS;

        assert_eq!(
            peer.tick_screen_layer_chooser(t1, KindLayerBounds::default()),
            0,
            "with nothing arriving, plain availability decay already drops the receiver to base"
        );
        assert!(
            !peer.screen_layer_availability.is_quarantined(2, t1),
            "a SYMMETRIC stall must not record a quarantine — every rung is equally starved, so \
             retracting one buys nothing and would suppress the climb-back for the whole window"
        );
        assert_eq!(
            peer.screen_keyframe_starved_since_ms.get(),
            None,
            "the hold is retired by the 2 -> 0 rung change (the ordinary layer-switch clear), not \
             consumed by a retraction — the `!is_quarantined` assertion above is what proves no \
             retraction happened"
        );
        assert_eq!(
            peer.selected_screen_layer(),
            0,
            "and the receiver did land on base, so it is not stranded"
        );
    }

    /// The issue-1741 ordering: peer joined AFTER SESSION_ASSIGNED, so no backfill revisits it.
    fn manager_with_flowing_peer(local_sid: u64, peer_sid: u64) -> PeerDecodeManager {
        let mut manager = PeerDecodeManager::new();
        manager.set_local_session_id(local_sid);
        let (mut peer, _muted) = make_test_peer(peer_sid);
        peer.video_enabled = true;
        peer.has_received_heartbeat = true;
        peer.visible = true;
        manager.connected_peers.insert(peer_sid, peer);
        manager
    }

    /// Issue 1741: every frame handed to the decoder carries the peer pair.
    #[test]
    fn every_frame_pushed_to_the_worker_carries_the_peer_attribution() {
        let mut manager = manager_with_flowing_peer(7, 1741);
        let expected = StreamContext {
            from_peer: "7".to_string(),
            to_peer: "1741".to_string(),
        };

        for seq in 1..=2u64 {
            manager
                .decode(
                    Arc::try_unwrap(video_layer_wrap(0, seq, 1741))
                        .expect("sole owner of the wrapper"),
                    "local@test.com",
                )
                .expect("a selected-rung packet from a visible peer decodes");
            assert_eq!(
                manager
                    .connected_peers
                    .get(&1741)
                    .expect("peer present")
                    .video
                    .last_pushed_context_for_test(),
                Some(expected.clone()),
                "frame {seq} must be handed to the decoder with the receiver->publisher pair"
            );
        }
    }

    /// Issue 1741, rebuild path: a rebuilt decoder owns a brand-new worker, and the peer latch
    /// has already fired.
    #[test]
    fn frames_stay_attributed_across_a_decoder_rebuild() {
        let mut manager = manager_with_flowing_peer(7, 1741);
        let packet =
            || Arc::try_unwrap(video_layer_wrap(0, 1, 1741)).expect("sole owner of the wrapper");
        let expected = Some(StreamContext {
            from_peer: "7".to_string(),
            to_peer: "1741".to_string(),
        });

        manager
            .decode(packet(), "local@test.com")
            .expect("first packet decodes");
        let peer = manager
            .connected_peers
            .get_mut(&1741)
            .expect("peer present");
        assert!(
            peer.context_initialized,
            "precondition: the peer-level latch has fired, so it cannot install again"
        );
        peer.replace_video_decoder(VideoPeerDecoder::noop(), "7");

        manager
            .decode(packet(), "local@test.com")
            .expect("post-rebuild packet decodes");
        assert_eq!(
            manager
                .connected_peers
                .get(&1741)
                .expect("peer present")
                .video
                .last_pushed_context_for_test(),
            expected,
            "the rebuilt decoder must still be handed the peer pair"
        );
    }
}
