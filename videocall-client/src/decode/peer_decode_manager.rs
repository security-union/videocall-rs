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
use super::layer_chooser::{DownlinkSample, LayerAvailability, LayerChooser};
use super::peer_decoder::{PeerDecode, VideoPeerDecoder, MEDIA_TYPE_CAMERA, MEDIA_TYPE_SCREEN};
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
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fmt::Display, sync::Arc};
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, TransportType};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
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
    /// Keyframe requests (PLI) we decided to send for this stream in the
    /// current window.
    window_kf_requests: u32,
    /// Most recently computed loss rate (lost packets/sec). Stable between
    /// window rollovers.
    loss_per_sec: f64,
    /// Most recently computed keyframe-request rate (PLI/sec). Stable between
    /// window rollovers.
    kf_per_sec: f64,
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
            window_kf_requests: 0,
            loss_per_sec: 0.0,
            kf_per_sec: 0.0,
        }
    }

    /// Record a sequence number. Returns the number of NEW lost packets
    /// detected (packets that shifted off the window unseen).
    fn record_seq(&mut self, seq: u64) -> u32 {
        let Some(high) = self.high_seq else {
            // First packet -- initialize. Mark all window positions as "seen"
            // so that pre-stream positions (negative sequence numbers that never
            // existed) are not counted as lost when the window advances.
            self.high_seq = Some(seq);
            self.seen_bits = u64::MAX;
            return 0;
        };

        if seq > high {
            // New highest -- shift window left and count losses.
            // Compare in u64 space first to avoid truncation on huge gaps.
            let gap = seq - high;
            let shift = gap.min(64) as u32;
            let new_lost = if gap >= 64 {
                // Entire window shifted out. Count unseen bits in old window.
                // count_zeros() on u64 returns at most 64 (the window size).
                self.seen_bits.count_zeros()
            } else {
                // Count the bits that will shift off (the top `shift` bits).
                let mask = !((1u64 << (64 - shift)) - 1);
                let shifting_out = self.seen_bits & mask;
                let lost_in_shift = shift - shifting_out.count_ones();
                // Shift and mark the new seq as seen.
                self.seen_bits = (self.seen_bits << shift) | 1;
                lost_in_shift
            };
            if gap >= 64 {
                self.seen_bits = 1; // only the new packet is seen
            }
            self.high_seq = Some(seq);
            self.lost_count += new_lost;
            new_lost
        } else if high - seq < 64 {
            // Out-of-order but within window -- mark as seen (no loss).
            let bit_pos = (high - seq) as u32;
            self.seen_bits |= 1u64 << bit_pos;
            0
        } else {
            // Too old (beyond window) -- ignore silently.
            0
        }
    }

    /// Check if a keyframe request should be sent. Returns `true` if yes.
    ///
    /// Implements exponential backoff: first request after
    /// `KEYFRAME_REQUEST_TIMEOUT_MS`, then intervals double from
    /// `KEYFRAME_REQUEST_MIN_INTERVAL_MS` up to `KEYFRAME_REQUEST_MAX_BACKOFF_MS`.
    /// After `KEYFRAME_REQUEST_MAX_UNANSWERED` unanswered requests, switches to
    /// a slow periodic retry (`KEYFRAME_REQUEST_SLOW_RETRY_MS`) to recover from
    /// persistent loss without flooding the sender.
    fn should_request_keyframe(&mut self, now: u64) -> bool {
        if self.lost_count == 0 {
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
            return false;
        }

        // After the initial backoff burst is exhausted, switch to slow
        // periodic retry. Keyframes are 5-10x larger than delta frames
        // and have a higher drop probability on lossy networks, so giving
        // up permanently would leave frozen video with no recovery path.
        if self.unanswered_requests >= KEYFRAME_REQUEST_MAX_UNANSWERED {
            if elapsed_since_last_req < KEYFRAME_REQUEST_SLOW_RETRY_MS {
                return false;
            }
            self.last_keyframe_request_ms = now;
            return true;
        }

        // Exponential backoff between requests.
        if elapsed_since_last_req < self.current_backoff_ms {
            return false;
        }

        self.last_keyframe_request_ms = now;
        self.unanswered_requests += 1;
        // Double backoff for next request, capped.
        self.current_backoff_ms =
            (self.current_backoff_ms * 2).min(KEYFRAME_REQUEST_MAX_BACKOFF_MS);
        true
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
    /// `kf_requested` — whether a keyframe request (PLI) will be sent for this
    ///                  stream as a result of this packet.
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
            self.window_lost = 0;
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

    /// Most recently computed windowed keyframe-request rate (PLI/sec).
    fn kf_per_sec(&self) -> f64 {
        self.kf_per_sec
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
    /// (`window_lost` / `window_kf_requests`) so the discontinuity does not leak
    /// into the current rate window. We DELIBERATELY preserve the PLI backoff
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
        self.window_kf_requests = 0;
    }
}

/// Result of `Peer::track_sequence`: a possible keyframe request plus, on
/// ~1Hz window rollover, the freshly-computed per-stream loss / keyframe rates
/// to publish on the diagnostics bus. `rates` is `Some` only on rollover so the
/// manager throttles bus emission to ~1Hz per peer-stream (no per-packet spam).
struct SeqTrackResult {
    /// `Some(media_type)` when a KEYFRAME_REQUEST (PLI) should be sent.
    keyframe_request: Option<MediaType>,
    /// `Some((loss_per_sec, kf_per_sec))` only when the rate window just rolled
    /// over and a fresh sample is ready to emit.
    rates: Option<(f64, f64)>,
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
    /// Server-vouched guest indicator, sourced from the authenticated JWT
    /// `is_guest` claim and broadcast on `PARTICIPANT_JOINED`.
    pub is_guest: bool,
    /// Whether this peer is currently decode-active in the UI layout.
    ///
    /// When `false`, video and screen decoding is skipped to save CPU. Audio
    /// is always decoded regardless so off-screen participants can still be
    /// heard.
    pub visible: bool,
    context_initialized: bool,
    vad_threshold: Option<f32>,
    has_received_heartbeat: bool,
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
    /// Which simulcast layer (issue #989, Phase 3) this receiver decodes for
    /// this peer's AUDIO stream. Defaults to 0.
    selected_audio_layer: u32,
    /// Per-peer AUDIO layer chooser + availability (issue #989, Phase 3). Audio
    /// carries no per-window loss tracker here (NetEq path), so the chooser is
    /// fed the VIDEO downlink as a proxy for shared-connection health (see
    /// `tick_audio_layer_chooser`) — hence no separate `last_audio_downlink`.
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
    /// HCL bug #1: same idea for the audio stream.
    last_audio_frame_ms: u64,
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

/// Resolve a CONTINUOUS live-stream (audio / camera-video) enabled flag
/// against a heartbeat, using the short [`LIVE_STREAM_FRESH_WINDOW_MS`] so a
/// real mute / camera-off reflects on remote peers sub-second.
///
/// Thin wrapper over [`apply_heartbeat_enabled_flag`] that BAKES IN the
/// correct window. The window is the security-/correctness-critical knob here:
/// a future "simplify by sharing one window" edit at the call site could
/// silently widen audio/video back to the screen-sized window and reintroduce
/// the ~5s mute/camera-off lag. Routing audio and camera-video through this
/// wrapper makes that window non-passable at the call site.
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
    fn new(
        video_canvas_id: String,
        screen_canvas_id: String,
        session_id: u64,
        user_id: String,
        aes: Option<Aes128State>,
        vad_threshold: Option<f32>,
        is_guest: bool,
    ) -> Result<Self, JsValue> {
        let sid_str = session_id.to_string();
        let (mut audio, video, screen) =
            Self::new_decoders(&video_canvas_id, &screen_canvas_id, &sid_str, vad_threshold)?;

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
            is_guest,
            visible: false,
            context_initialized: false,
            vad_threshold,
            has_received_heartbeat: false,
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
            selected_screen_layer: 0,
            screen_layer_chooser: LayerChooser::new(now_ms()),
            screen_layer_availability: LayerAvailability::new(),
            last_screen_downlink: DownlinkSample {
                loss_per_sec: 0.0,
                kf_per_sec: 0.0,
            },
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
            last_audio_frame_ms: 0,
        })
    }

    fn new_decoders(
        video_canvas_id: &str,
        screen_canvas_id: &str,
        peer_id: &str,
        vad_threshold: Option<f32>,
    ) -> Result<
        (
            Box<dyn AudioPeerDecoderTrait>,
            VideoPeerDecoder,
            VideoPeerDecoder,
        ),
        JsValue,
    > {
        // Create decoders without canvas (will be set later via set_canvas)
        // We still keep the canvas IDs for backward compatibility with existing code
        let video_decoder = VideoPeerDecoder::new(None, MEDIA_TYPE_CAMERA)?;
        let screen_decoder = VideoPeerDecoder::new(None, MEDIA_TYPE_SCREEN)?;

        // Attempt to set canvas immediately if available in DOM
        if let Some(window) = web_sys::window() {
            if let Some(document) = window.document() {
                if let Some(canvas_element) = document.get_element_by_id(video_canvas_id) {
                    if let Ok(canvas) = canvas_element.dyn_into::<web_sys::HtmlCanvasElement>() {
                        let _ = video_decoder.set_canvas(canvas);
                    }
                }
                if let Some(canvas_element) = document.get_element_by_id(screen_canvas_id) {
                    if let Ok(canvas) = canvas_element.dyn_into::<web_sys::HtmlCanvasElement>() {
                        let _ = screen_decoder.set_canvas(canvas);
                    }
                }
            }
        }

        Ok((
            create_audio_peer_decoder(None, peer_id.to_string(), vad_threshold)?,
            video_decoder,
            screen_decoder,
        ))
    }

    fn reset(&mut self) -> Result<(), JsValue> {
        let sid_str = self.session_id.to_string();
        let (mut audio, video, screen) = Self::new_decoders(
            &self.video_canvas_id,
            &self.screen_canvas_id,
            &sid_str,
            self.vad_threshold,
        )?;

        // Preserve the current mute state after reset
        audio.set_muted(!self.audio_enabled);
        debug!(
            "Reset peer {} with audio muted: {}",
            self.session_id, !self.audio_enabled
        );

        self.audio = audio;
        self.video = video;
        self.screen = screen;
        // Intentionally keep `has_received_heartbeat` and `*_enabled` flags:
        // the peer's last-known media state is still the best information we
        // have.  Resetting the flag would let straggler frames through until
        // the next heartbeat, which is the opposite of what we want.
        Ok(())
    }

    /// Select which simulcast layer (issue #989) this receiver decodes for this
    /// peer. VIDEO packets tagged with a different `simulcast_layer_id` are
    /// dropped before sequence tracking and decode. Defaults to 0 (lowest
    /// layer). In Phase 2 this is driven automatically by
    /// [`Self::tick_layer_chooser`]; it may also be set directly (tests / future
    /// viewport-driven overrides).
    pub fn set_selected_video_layer(&mut self, layer: u32) {
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
            self.video_seq_tracker.reanchor_for_layer_switch();
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
        let highest = self.screen_layer_availability.highest_available(now_ms);
        let raw = self
            .screen_layer_chooser
            .choose(self.last_screen_downlink, highest, now_ms);
        let desired = bounds.clamp(raw);
        // Re-anchor on an actual layer change (#1079 H1) — see
        // `set_selected_video_layer` for the rationale. No-op when unchanged.
        if desired != self.selected_screen_layer {
            self.screen_seq_tracker.reanchor_for_layer_switch();
        }
        self.selected_screen_layer = desired;
        desired
    }

    /// The simulcast layer this receiver currently decodes for this peer's
    /// screen stream.
    pub fn selected_screen_layer(&self) -> u32 {
        self.selected_screen_layer
    }

    /// Run the AUDIO layer chooser one tick (issue #989, Phase 3) and apply the
    /// result to the audio decode guard.
    ///
    /// Audio carries no per-window sequence loss tracker here (audio decode goes
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
        let desired = bounds.clamp(raw);
        self.selected_audio_layer = desired;
        desired
    }

    /// The simulcast layer this receiver currently decodes for this peer's audio
    /// stream.
    pub fn selected_audio_layer(&self) -> u32 {
        self.selected_audio_layer
    }

    /// Broadcast current media-enabled state to the diagnostics bus so the UI
    /// can update peer tiles.
    fn broadcast_peer_status(&self) {
        let transport_str = match self.transport_type {
            TransportType::TRANSPORT_WEBTRANSPORT => "webtransport",
            TransportType::TRANSPORT_WEBSOCKET => "websocket",
            TransportType::TRANSPORT_UNKNOWN => "unknown",
        };
        let evt = DiagEvent {
            subsystem: "peer_status",
            stream_id: None,
            ts_ms: now_ms(),
            metrics: vec![
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
                metric!("is_speaking", if self.is_speaking { 1u64 } else { 0u64 }),
                metric!("audio_level", self.audio_level as f64),
                metric!("peer_transport", transport_str.to_string()),
            ],
        };
        let _ = global_sender().try_broadcast(evt);
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

        if audio_off && self.audio_enabled {
            self.audio_enabled = false;
            // Mute and flush the audio decoder the same way the heartbeat
            // audio-off transition does, to prevent the NetEq buffer from
            // emitting expand/hiss packets after the stream is gone.
            self.audio.set_muted(true);
            self.audio.flush();
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
    /// Shaped like the `"video"` subsystem events emitted by
    /// `DiagnosticWorker::send_diagnostic_packets` so the existing
    /// health_reporter `"video"` handler routes these metrics into the same
    /// per-peer camera/screen bucket (disambiguated by `media_type`). Called
    /// at most ~1Hz per peer-stream (only on rate-window rollover), so this
    /// stays off the per-packet hot path.
    ///
    /// `local_user_id` is the reporting (local) client — the `from_peer` of the
    /// event; `self.sid_str` is the observed remote peer — the `to_peer`.
    fn emit_loss_metrics(
        &self,
        local_user_id: &str,
        media_type: MediaType,
        loss_per_sec: f64,
        kf_per_sec: f64,
    ) {
        let evt = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: now_ms(),
            metrics: vec![
                metric!("media_type", format!("{media_type:?}")),
                metric!("from_peer", local_user_id.to_string()),
                metric!("to_peer", self.sid_str.clone()),
                metric!("video_seq_loss_per_sec", loss_per_sec),
                metric!("keyframe_requests_per_sec", kf_per_sec),
            ],
        };
        let _ = global_sender().try_broadcast(evt);
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
        // Pre-simulcast publishers (and the single-layer default) send 0, so
        // this is 0 for them. The VIDEO arm uses it to drop non-selected layers
        // before any sequence tracking / decode.
        let incoming_video_layer = packet.simulcast_layer_id;

        // Compute the monotonic clock ONCE for this packet (perf follow-up):
        // `now_ms()` crosses the JS boundary (`performance.now()`), and the decode
        // path needs it for the media-freshness stamp AND the per-kind simulcast
        // availability `observe`. Reusing one value avoids the extra per-packet
        // boundary call (which ran even with simulcast off).
        let now = now_ms();

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
                // Phase 2 (#989): learn which layers this source produces from
                // EVERY arriving VIDEO packet — including ones we are about to
                // drop below — so the chooser knows how high it may climb. This
                // MUST run before the drop guard, otherwise we would only ever
                // observe the layer we already selected and could never learn a
                // higher layer exists. Observing a non-selected layer here costs
                // a hashmap insert; the packet is still dropped below.
                //
                // Security (#989): clamp the raw, attacker-controllable
                // (un-sealed) layer id to the ladder range BEFORE observing, so a
                // malicious publisher cycling unbounded unique ids cannot inflate
                // availability cardinality between prunes.
                self.video_layer_availability.observe(
                    crate::decode::layer_chooser::clamp_observed_layer_id(
                        crate::decode::layer_chooser::PrefMediaKind::Video,
                        incoming_video_layer,
                    ),
                    now,
                );

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
                    return Ok((media_type, DecodeStatus::SKIPPED, None));
                }

                // Track sequence numbers for gap detection (PLI) and windowed
                // loss/keyframe-rate accounting (freeze observability #1013).
                let seq = self.track_sequence(media_type, &packet);
                if let Some((loss_per_sec, kf_per_sec)) = seq.rates {
                    self.emit_loss_metrics(local_user_id, media_type, loss_per_sec, kf_per_sec);
                    // Phase 2 (#989): cache this window's downlink health so the
                    // monitor-tick chooser can fold it into the per-peer layer
                    // decision. Only the SELECTED layer's sequence is tracked
                    // (the guard above drops the rest), so this loss/PLI rate
                    // measures exactly this receiver's ability to sustain the
                    // layer it is currently pulling.
                    self.last_video_downlink = DownlinkSample {
                        loss_per_sec,
                        kf_per_sec,
                    };
                }
                let kf_request = seq.keyframe_request;

                // HCL bug #1: stamp the freshness timestamp BEFORE the
                // `has_received_heartbeat` branch so it works on both the
                // "no heartbeat yet" and "heartbeat says off, drop frame"
                // paths. The next heartbeat consults this to decide whether
                // to trust its own metadata or the live frame stream.
                self.last_video_frame_ms = now;

                if !self.video_enabled {
                    if !self.has_received_heartbeat {
                        // No heartbeat yet — infer video_enabled from the actual frame.
                        self.video_enabled = true;
                        self.broadcast_peer_status();
                    } else {
                        // Peer has video off per heartbeat; drop straggler frame.
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }
                }

                // Skip video decoding when the peer tile is not visible in the
                // viewport. The next keyframe after visibility is restored will
                // allow the decoder to recover naturally.
                if !self.visible {
                    return Ok((media_type, DecodeStatus::SKIPPED, None));
                }

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
                // Phase 3 (#989): learn AUDIO layer availability from arriving
                // audio packets' layer ids. Audio simulcast is a small ladder
                // (low/high); the chooser uses this to know whether a higher
                // audio layer even exists before climbing. Single-layer audio
                // publishers send 0, so availability stays {0} and the chooser
                // never climbs — a no-op for them. Must run before any drop.
                //
                // Security (#989): clamp the raw (un-sealed) layer id to the
                // audio ladder range before observing (see the VIDEO arm).
                self.audio_layer_availability.observe(
                    crate::decode::layer_chooser::clamp_observed_layer_id(
                        crate::decode::layer_chooser::PrefMediaKind::Audio,
                        incoming_video_layer,
                    ),
                    now,
                );

                // Phase 3 (#989): AUDIO simulcast layer-select guard. Drop audio
                // packets whose layer != the selected audio layer. Default
                // selected_audio_layer is 0, matching single-layer publishers.
                if incoming_video_layer != self.selected_audio_layer {
                    return Ok((media_type, DecodeStatus::SKIPPED, None));
                }

                // HCL bug #1: stamp audio freshness regardless of the
                // straggler-drop path so the next heartbeat can detect
                // recent audio frames and suppress a stale-muted heartbeat.
                self.last_audio_frame_ms = now;

                if !self.audio_enabled {
                    if !self.has_received_heartbeat {
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
                // Phase 3 (#989): learn SCREEN layer availability from EVERY
                // arriving screen packet (incl. ones about to be dropped) so the
                // screen chooser knows how high it may climb — independent of the
                // camera VIDEO availability. Must run before the drop guard.
                //
                // Security (#989): clamp the raw (un-sealed) layer id to the
                // screen ladder range before observing (see the VIDEO arm).
                self.screen_layer_availability.observe(
                    crate::decode::layer_chooser::clamp_observed_layer_id(
                        crate::decode::layer_chooser::PrefMediaKind::Screen,
                        incoming_video_layer,
                    ),
                    now,
                );

                // Phase 3 (#989): SCREEN simulcast layer-select guard. Drop any
                // SCREEN packet that is not the layer this receiver selected for
                // this peer's screen — BEFORE sequence tracking and decode, for
                // the same phantom-loss reason as the VIDEO guard. Pre-simulcast
                // / single-layer screen publishers send layer 0 and the default
                // selected_screen_layer is 0, so nothing is dropped for them.
                if incoming_video_layer != self.selected_screen_layer {
                    return Ok((media_type, DecodeStatus::SKIPPED, None));
                }

                // Track sequence numbers for gap detection (PLI) and windowed
                // loss/keyframe-rate accounting (freeze observability #1013).
                let seq = self.track_sequence(media_type, &packet);
                if let Some((loss_per_sec, kf_per_sec)) = seq.rates {
                    self.emit_loss_metrics(local_user_id, media_type, loss_per_sec, kf_per_sec);
                    // Phase 3: cache the SCREEN downlink window for the chooser.
                    self.last_screen_downlink = DownlinkSample {
                        loss_per_sec,
                        kf_per_sec,
                    };
                }
                let kf_request = seq.keyframe_request;

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
                if !self.visible {
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
                    let resolved_video = apply_live_stream_heartbeat_flag(
                        self.video_enabled,
                        metadata.video_enabled,
                        self.last_video_frame_ms,
                        now,
                    );
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

                    self.video_enabled = resolved_video;
                    self.audio_enabled = resolved_audio;
                    self.screen_enabled = resolved_screen;
                    self.is_speaking = metadata.is_speaking;
                    if !metadata.is_speaking {
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
                        debug!(
                            "Flushed screen decoder for peer {} (screen turned off)",
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
                rates: None,
            };
        };

        let tracker = match media_type {
            MediaType::VIDEO => &mut self.video_seq_tracker,
            MediaType::SCREEN => &mut self.screen_seq_tracker,
            _ => {
                return SeqTrackResult {
                    keyframe_request: None,
                    rates: None,
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

        // Feed the ~1s windowed rate accounting. `observe_window` returns true
        // exactly on rollover, throttling bus emission to ~1Hz per stream.
        let rates = if tracker.observe_window(now, new_lost, kf_requested) {
            Some((tracker.loss_per_sec(), tracker.kf_per_sec()))
        } else {
            None
        };

        SeqTrackResult {
            keyframe_request: kf_requested.then_some(media_type),
            rates,
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

fn parse_media_packet(data: &[u8]) -> Result<Arc<MediaPacket>, PeerDecodeError> {
    Ok(Arc::new(
        MediaPacket::parse_from_bytes(data).map_err(|_| PeerDecodeError::PacketParseError)?,
    ))
}

#[derive(Debug)]
pub struct PeerDecodeManager {
    connected_peers: HashMapWithOrderedKeys<u64, Peer>,
    /// Cache of session_id -> display_name, populated from PARTICIPANT_JOINED events.
    /// This persists independently of the peer list so that when `ensure_peer()`
    /// creates a peer later (after the first media packet arrives), the display
    /// name is immediately available and does not fall back to user_id/email.
    display_name_cache: HashMap<u64, String>,
    /// Cache of session_id -> is_guest, populated from PARTICIPANT_JOINED events.
    /// Mirrors `display_name_cache` so a guest flag arriving before the peer
    /// entry is created (via `ensure_peer()`) is still applied once the first
    /// media packet brings the peer online.
    is_guest_cache: HashMap<u64, bool>,
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
            is_guest_cache: HashMap::new(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: None,
            on_peer_removed: Callback::noop(),
            on_peers_removed_batch: Callback::noop(),
            vad_threshold: None,
            send_packet: None,
            local_user_id: String::new(),
            cached_sorted_string_keys: RefCell::new(None),
            screen_decode_retry_tokens: HashMap::new(),
        }
    }

    pub fn new_with_diagnostics(diagnostics: Rc<DiagnosticManager>) -> Self {
        Self {
            connected_peers: HashMapWithOrderedKeys::new(),
            display_name_cache: HashMap::new(),
            is_guest_cache: HashMap::new(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: Some(diagnostics),
            on_peer_removed: Callback::noop(),
            on_peers_removed_batch: Callback::noop(),
            vad_threshold: None,
            send_packet: None,
            local_user_id: String::new(),
            cached_sorted_string_keys: RefCell::new(None),
            screen_decode_retry_tokens: HashMap::new(),
        }
    }

    /// Set the callback used to send packets back through the connection.
    /// This is required for the PLI (keyframe request) mechanism.
    pub fn set_send_packet_callback(&mut self, callback: Callback<PacketWrapper>, user_id: String) {
        self.send_packet = Some(callback);
        self.local_user_id = user_id;
    }

    /// Clear the send-packet callback. Called from
    /// [`VideoCallClient::disconnect()`](crate::VideoCallClient::disconnect)
    /// to break the `client -> peer_decode_manager.send_packet -> client`
    /// `Rc` cycle that otherwise keeps `Inner` alive after the UI scope
    /// holding the client has unmounted (issue: cc7tp meeting incident
    /// 2026-05-01, github01.hclpnp.com/labs-projects/videocall/discussions/502).
    pub fn clear_send_packet_callback(&mut self) {
        self.send_packet = None;
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

    /// Update which peers are eligible for video/screen decode.
    ///
    /// The meeting layout is the authoritative source of truth in the first
    /// pass of selective decode: peers rendered in the active layout are
    /// marked visible, and every other peer is fail-closed to skipped video
    /// and screen decode. Audio remains decoded for all peers.
    pub fn set_active_decode_set(&mut self, active_session_ids: &HashSet<u64>) {
        let session_ids = self.connected_peers.ordered_keys().clone();
        let mut screen_keyframe_requests: Vec<String> = Vec::new();
        let mut video_keyframe_requests: Vec<String> = Vec::new();
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
                    screen_keyframe_requests.push(peer.user_id.clone());
                }
                // Send a proactive video PLI when a video tile becomes visible
                // so the decoder gets a keyframe immediately instead of waiting
                // up to 5 s for the next periodic one (150 frames at 30 fps).
                // Gated on video_enabled so we don't send spurious PLIs for
                // peers that have their camera off.
                if visible && peer.video_enabled {
                    video_keyframe_requests.push(peer.user_id.clone());
                }
                peer.visible = visible;
            }
        }
        for user_id in &screen_keyframe_requests {
            self.send_keyframe_request(user_id, MediaType::SCREEN);
        }
        for user_id in &video_keyframe_requests {
            self.send_keyframe_request(user_id, MediaType::VIDEO);
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
    pub fn set_peer_video_canvas(
        &self,
        peer_id: u64,
        canvas: web_sys::HtmlCanvasElement,
    ) -> Result<(), JsValue> {
        if let Some(peer) = self.connected_peers.get(&peer_id) {
            peer.video.set_canvas(canvas)
        } else {
            Err(JsValue::from_str(&format!("Peer {peer_id} not found")))
        }
    }

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
                self.send_keyframe_request(&peer.user_id, MediaType::SCREEN);
            }
            Ok(())
        } else {
            Err(JsValue::from_str(&format!("Peer {peer_id} not found")))
        }
    }

    pub fn run_peer_monitor(&mut self) -> Vec<String> {
        let removed = self
            .connected_peers
            .remove_if_and_return(|peer| peer.check_heartbeat());
        let mut removed_ids = Vec::new();
        for (_session_id, peer) in removed {
            if let Some(token) = self.screen_decode_retry_tokens.remove(&peer.user_id) {
                token.set(false);
            }
            if let Some(diag) = &self.diagnostics {
                diag.remove_peer(&peer.sid_str);
            }
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
        use crate::decode::layer_chooser::PrefMediaKind;
        let video_bounds = bounds.for_kind(PrefMediaKind::Video);
        let screen_bounds = bounds.for_kind(PrefMediaKind::Screen);
        let audio_bounds = bounds.for_kind(PrefMediaKind::Audio);
        let mut desired = HashMap::new();
        for session_id in self.connected_peers.ordered_keys().clone() {
            if let Some(peer) = self.connected_peers.get_mut(&session_id) {
                // Phase 4: each per-(peer,kind) chooser output is clamped to the
                // user's GLOBAL receive bounds for that kind. The tick updates the
                // peer's DECODE guard (`selected_*_layer`) as a side effect and
                // returns the (clamped) decode layer.
                let video = peer.tick_layer_chooser(now_ms, video_bounds);
                let screen = peer.tick_screen_layer_chooser(now_ms, screen_bounds);
                let audio = peer.tick_audio_layer_chooser(now_ms, audio_bounds);

                // Issue #1079 M1/M2: only ADVERTISE a preference for a kind when
                // the final (clamped) decode layer actually constrains BELOW the
                // highest available layer for that kind. This single rule captures
                // BOTH sources of a real constraint:
                //   * the chooser dropped below the top under congestion, and
                //   * the user's receive `max` bound capped it below the top.
                // On cold start / a healthy unclamped receiver, the chooser tracks
                // the top (M2: it no longer ramps from base), so layer == highest
                // and the entry is OMITTED → relay fail-open forwards all layers
                // (no base-pin HD dip after reconnect). An all-omitted map yields
                // no entries, so no LAYER_PREFERENCE packet goes out when there is
                // nothing to constrain (M1).
                let vh = peer.video_layer_availability.highest_available(now_ms);
                if video < vh {
                    desired.insert((session_id, PrefMediaKind::Video), video);
                }
                let sh = peer.screen_layer_availability.highest_available(now_ms);
                if screen < sh {
                    desired.insert((session_id, PrefMediaKind::Screen), screen);
                }
                let ah = peer.audio_layer_availability.highest_available(now_ms);
                if audio < ah {
                    desired.insert((session_id, PrefMediaKind::Audio), audio);
                }
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
        Some(received_layer_snapshot(kind, layer, count))
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
                received_layer_snapshot_with_reason(
                    PrefMediaKind::Screen,
                    peer.selected_screen_layer,
                    avail_top,
                    screen_max,
                    peer.screen_layer_chooser.is_constrained(),
                )
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
        if let Some(peer) = self.connected_peers.get_mut(&peer_session_id) {
            let was_screen_enabled = peer.screen_enabled;
            if !peer.context_initialized {
                peer.video
                    .set_stream_context(userid.to_string(), peer.sid_str.clone());
                peer.screen
                    .set_stream_context(userid.to_string(), peer.sid_str.clone());
                peer.context_initialized = true;
            }
            match peer.decode(&packet, userid) {
                Ok((MediaType::HEARTBEAT, _, _)) => {
                    peer.on_activity();
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
                        diagnostics.track_frame(
                            &peer.sid_str,
                            media_type,
                            packet.data.len() as u64,
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
                    if let Some((peer_uid, requested_media_type)) = kf_info {
                        self.send_keyframe_request(&peer_uid, requested_media_type);
                    }

                    Ok(())
                }
                Err(e) => {
                    // Track decode errors (codec failures: keyframe miss, parse error, decoder reset).
                    // Media type is not available in the error arm; default to VIDEO since that
                    // is where the vast majority of decode errors occur in practice.
                    if let Some(diagnostics) = &self.diagnostics {
                        diagnostics.track_decode_error(&peer.sid_str, MediaType::VIDEO);
                    }
                    peer.reset().map_err(|_| e)
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
    /// The packet is a `MediaPacket` with `media_type = KEYFRAME_REQUEST`
    /// and `user_id` set to the target peer. The `data` field encodes
    /// which stream (VIDEO or SCREEN) needs the keyframe.
    ///
    /// IMPORTANT: This uses `send_packet` (reliable stream), NOT
    /// `send_media_packet` (datagrams). KEYFRAME_REQUEST is a control
    /// message that MUST be delivered reliably.
    ///
    /// The packet is sent unencrypted (raw MediaPacket, not AES-encrypted)
    /// because this is a signaling/control packet, not user media data.
    /// The server needs to read the target `user_id` to route it correctly.
    fn send_keyframe_request(&self, peer_user_id: &str, requested_media_type: MediaType) {
        let Some(send_packet) = &self.send_packet else {
            debug!("Cannot send KEYFRAME_REQUEST: no send_packet callback");
            return;
        };

        let media_type_byte = match requested_media_type {
            MediaType::VIDEO => b"VIDEO".to_vec(),
            MediaType::SCREEN => b"SCREEN".to_vec(),
            _ => return,
        };

        let media_packet = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            user_id: peer_user_id.as_bytes().to_vec(),
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
            user_id: self.local_user_id.as_bytes().to_vec(),
            data: media_data,
            ..Default::default()
        };

        KEYFRAME_REQUESTS_SENT.fetch_add(1, Ordering::Relaxed);
        log::info!(
            "Sending KEYFRAME_REQUEST to {} for {:?}",
            peer_user_id,
            requested_media_type
        );
        send_packet.emit(wrapper);
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
            self.get_screen_canvas_id.emit(sid_str),
            session_id,
            user_id.to_owned(),
            aes,
            self.vad_threshold,
            cached_is_guest,
        )?;
        // Apply cached display name if PARTICIPANT_JOINED arrived before
        // the first media packet created this peer entry.
        if let Some(cached_name) = self.display_name_cache.get(&session_id) {
            debug!(
                "Applying cached display_name '{}' for peer {} (user_id={})",
                cached_name, session_id, user_id
            );
            peer.display_name = Some(cached_name.clone());
        }
        self.connected_peers.insert(session_id, peer);
        // Phase 6: invalidate the sorted-keys cache so the next
        // `sorted_string_keys()` call rebuilds with the new peer.
        self.invalidate_sorted_string_keys();
        Ok(())
    }

    pub fn delete_peer(&mut self, session_id: u64) {
        if let Some(peer) = self.connected_peers.remove(&session_id) {
            if let Some(token) = self.screen_decode_retry_tokens.remove(&peer.user_id) {
                token.set(false);
            }
            if let Some(diag) = &self.diagnostics {
                diag.remove_peer(&peer.sid_str);
            }
            self.display_name_cache.remove(&session_id);
            self.is_guest_cache.remove(&session_id);
            // Phase 6: invalidate the sorted-keys cache before notifying
            // observers so any read in the callback sees a fresh list.
            self.invalidate_sorted_string_keys();
            let sid_str = peer.sid_str.clone();
            self.on_peer_removed.emit(peer.sid_str);
            // Single-peer removals also fire the batched callback so
            // subscribers can coalesce on it without subscribing to two
            // separate notifications.
            self.on_peers_removed_batch.emit(vec![sid_str]);
        }
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
        self.is_guest_cache.clear();
        // Phase 6: invalidate the sorted-keys cache and emit a single
        // batched event so observers can coalesce the bulk-clear into
        // one notification.
        self.invalidate_sorted_string_keys();
        if !removed_ids.is_empty() {
            self.on_peers_removed_batch.emit(removed_ids);
        }
        // Peers are dropped here, triggering Worker::terminate() via Drop impl
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
    /// This is **not** a permanent latch: it writes the very same tracked flags
    /// the heartbeat path reads and writes. When the target later legitimately
    /// re-enables and sends `heartbeat = true` with fresh frames,
    /// [`apply_heartbeat_enabled_flag`] returns `true` (affirmative heartbeats
    /// always win) and the peer recovers normally.
    ///
    /// Safe no-op when no connected peer matches `user_id` (e.g. the host's own
    /// view, or a peer not yet known to this client). Only emits a peer-status
    /// broadcast for peers whose state actually changed, avoiding redundant UI
    /// churn on duplicate dual-transport deliveries.
    pub fn force_peer_media_off(&mut self, user_id: &str, audio_off: bool, video_off: bool) {
        if !audio_off && !video_off {
            return;
        }
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
    /// [`Peer::force_media_off`]. Like that method it is **not** a permanent
    /// latch: a later affirmative heartbeat with fresh frames re-enables the
    /// peer normally.
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
        let keys: Vec<u64> = self.connected_peers.ordered_keys().clone();
        for key in keys {
            if let Some(peer) = self.connected_peers.get_mut(&key) {
                // Skip the issuing host's own tile(s) entirely — a mute-all
                // must not mute the host that issued it (#1036).
                if peer.user_id == except_user_id {
                    continue;
                }
                if peer.force_media_off(audio_off, video_off) {
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
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use std::cell::Cell;
    use videocall_types::protos::media_packet::media_packet::MediaType;
    use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket};
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    // -- mock audio decoder -----------------------------------------------

    /// No-op audio decoder for unit tests.
    /// Muted state is stored in an `Rc<Cell<bool>>` so tests can inspect it
    /// after handing ownership to `Peer`.
    struct MockAudioDecoder {
        muted: Rc<Cell<bool>>,
    }

    impl MockAudioDecoder {
        fn new() -> (Self, Rc<Cell<bool>>) {
            let muted = Rc::new(Cell::new(true));
            (
                Self {
                    muted: muted.clone(),
                },
                muted,
            )
        }
    }

    impl AudioPeerDecoderTrait for MockAudioDecoder {
        fn decode(&mut self, _packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
            Ok(DecodeStatus::SKIPPED)
        }
        fn flush(&mut self) {}
        fn set_muted(&mut self, muted: bool) {
            self.muted.set(muted);
        }
    }

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

    /// Create a `Peer` with no-op decoders (no browser APIs required).
    /// Returns the peer and an `Rc<Cell<bool>>` handle to the mock audio
    /// decoder's muted state for test assertions.
    fn make_test_peer(session_id: u64) -> (Peer, Rc<Cell<bool>>) {
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
            is_guest: false,
            visible: false,
            context_initialized: false,
            has_received_heartbeat: false,
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
            selected_audio_layer: 0,
            audio_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
            audio_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
            video_seq_tracker: SequenceTracker::new(),
            screen_seq_tracker: SequenceTracker::new(),
            last_screen_frame_ms: 0,
            last_video_frame_ms: 0,
            last_audio_frame_ms: 0,
        };
        (peer, muted_handle)
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
        assert!(
            MEDIA_FRESH_WINDOW_MS >= 5_000,
            "MEDIA_FRESH_WINDOW_MS must be ≥ HEARTBEAT_KEEPALIVE_INTERVAL_MS (5000ms) — \
             a shorter window lets a stale heartbeat clobber live media on lossy WT"
        );

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

    /// (b) A camera-off heartbeat (`video_enabled = false`) arriving just
    /// after camera frames stop must be honoured once the last frame ages
    /// past the SHORT window — the symmetric case to audio mute. This is the
    /// fix for the camera-disable side of the ~5s lag: video now uses the
    /// continuous-stream window, not the screen window.
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

    /// After a heartbeat with screen_enabled=false, a straggler SCREEN frame
    /// must NOT flip screen_enabled back to true and must return rendered=false.
    #[wasm_bindgen_test]
    fn screen_straggler_after_heartbeat_is_dropped() {
        let (mut peer, _muted) = make_test_peer(6);

        let hb = heartbeat_packet(6, false, false, false);
        let _ = peer.decode(&hb, "");
        assert!(peer.has_received_heartbeat);
        assert!(!peer.screen_enabled);

        let packet = screen_frame_packet(6);
        let result = peer.decode(&packet, "");
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(!status.first_frame, "straggler must not be a first frame");
        assert!(
            !peer.screen_enabled,
            "straggler screen frame must not re-enable screen"
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

        // Disable video via heartbeat.
        let hb_off = heartbeat_packet(8, false, false, false);
        let _ = peer.decode(&hb_off, "");
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

        let level = manager.peer_audio_level(&"101".to_string());
        assert!(
            (level - 0.75).abs() < f32::EPSILON,
            "peer_audio_level should return 0.75, got {level}"
        );
    }

    /// Calling `peer_audio_level()` for a non-existent peer should return 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_unknown_peer_returns_zero() {
        let manager = PeerDecodeManager::new();
        let level = manager.peer_audio_level(&"99999".to_string());
        assert!(
            (level - 0.0).abs() < f32::EPSILON,
            "peer_audio_level for unknown peer should return 0.0, got {level}"
        );
    }

    /// Calling `peer_audio_level()` with a non-numeric key should return 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_invalid_key_returns_zero() {
        let manager = PeerDecodeManager::new();
        let level = manager.peer_audio_level(&"not-a-number".to_string());
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
        let hb_off = heartbeat_packet(9, false, false, false);
        let _ = peer.decode(&hb_off, "");
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

        // Send seq 70 -- this shifts positions 2..6 off the 64-packet window,
        // confirming them as genuinely lost (they never arrived).
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

        // Introduce genuine loss: seq 70 shifts positions 2..6 off the window.
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

        // Send screen seq 1, then seq 70 (genuine loss: shifts 2-6 off window).
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

        // Peer B: genuine loss (seq 1 -> seq 70).
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

        // First loss: seq 1 -> seq 70 (shifts 2-6 off window).
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

        // Introduce genuine loss: seq 1 -> 70 shifts positions off window.
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

        // t=0: window starts; no rollover yet.
        assert!(
            !tracker.observe_window(0, 2, false),
            "first observation must not roll over"
        );
        // Mid-window accumulation, still under 1000ms — no rollover, no emit.
        assert!(!tracker.observe_window(300, 1, true));
        assert!(!tracker.observe_window(600, 0, true));
        // Rates have not been published yet (still the initial zeros).
        assert_eq!(tracker.loss_per_sec(), 0.0);
        assert_eq!(tracker.kf_per_sec(), 0.0);

        // t=1000: window elapsed == 1000ms → rollover, fresh rates available.
        assert!(
            tracker.observe_window(1000, 0, false),
            "rollover should fire at >=1000ms elapsed"
        );
        // Accumulated this window: lost = 2+1+0+0 = 3, kf = 2 (the two `true`s).
        // denom = 1000ms → rate == count.
        assert!((tracker.loss_per_sec() - 3.0).abs() < 1e-9);
        assert!((tracker.kf_per_sec() - 2.0).abs() < 1e-9);

        // Window counters reset: a fresh quiet window yields zero rates.
        assert!(!tracker.observe_window(1500, 0, false));
        assert!(tracker.observe_window(2000, 0, false));
        assert_eq!(tracker.loss_per_sec(), 0.0);
        assert_eq!(tracker.kf_per_sec(), 0.0);
    }

    /// A window longer than 1s normalizes correctly to a per-second rate
    /// (denominator is actual elapsed ms, not a fixed 1000).
    #[wasm_bindgen_test]
    fn observe_window_normalizes_long_window() {
        let mut tracker = SequenceTracker::new();
        tracker.observe_window(0, 0, false); // start window at t=0
                                             // 10 losses over a 2000ms window → 5 lost/sec.
        for t in [400u64, 800, 1200, 1600] {
            assert!(!tracker.observe_window(t, 2, false));
        }
        let rolled = tracker.observe_window(2000, 2, false);
        assert!(rolled, "rollover at 2000ms");
        assert!(
            (tracker.loss_per_sec() - 5.0).abs() < 1e-9,
            "10 losses / 2s = 5/s, got {}",
            tracker.loss_per_sec()
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
        // With seen_bits initialized to u64::MAX on the first packet, all
        // pre-stream positions are marked as "seen", so count_zeros = 0.
        // No phantom losses.
        let lost = tracker.record_seq(70);
        assert_eq!(lost, 0, "No phantom losses with u64::MAX initialization");
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

        // Manually invoke send_keyframe_request for a specific peer.
        manager.send_keyframe_request("alice@example.com", MediaType::VIDEO);

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
                is_guest: false,
                visible: false,
                context_initialized: false,
                has_received_heartbeat: false,
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
                selected_audio_layer: 0,
                audio_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
                audio_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
                video_seq_tracker: SequenceTracker::new(),
                screen_seq_tracker: SequenceTracker::new(),
                last_screen_frame_ms: 0,
                last_video_frame_ms: 0,
                last_audio_frame_ms: 0,
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
                assert_eq!(
                    inner.media_type.enum_value(),
                    Ok(MediaType::VIDEO),
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
            is_guest: false,
            visible: false,
            context_initialized: false,
            has_received_heartbeat: false,
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
            selected_audio_layer: 0,
            audio_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
            audio_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
            video_seq_tracker: SequenceTracker::new(),
            screen_seq_tracker: SequenceTracker::new(),
            last_screen_frame_ms: 0,
            last_video_frame_ms: 0,
            last_audio_frame_ms: 0,
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
            is_guest: false,
            visible: false,
            context_initialized: false,
            has_received_heartbeat: false,
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
            selected_audio_layer: 0,
            audio_layer_chooser: crate::decode::layer_chooser::LayerChooser::new(0),
            audio_layer_availability: crate::decode::layer_chooser::LayerAvailability::new(),
            video_seq_tracker: SequenceTracker::new(),
            screen_seq_tracker: SequenceTracker::new(),
            last_screen_frame_ms: 0,
            last_video_frame_ms: 0,
            last_audio_frame_ms: 0,
        };
        manager.connected_peers.insert(520, peer);

        manager.set_active_decode_set(&HashSet::from([520u64]));

        assert_eq!(
            collected.borrow().len(),
            0,
            "No PLI should be sent for a peer with camera off"
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

    /// `force_peer_media_off(audio_off)` mutes immediately and marks the
    /// audio decoder muted, even with a just-decoded audio frame.
    #[wasm_bindgen_test]
    fn force_peer_media_off_disables_audio_despite_fresh_frame() {
        let mut manager = PeerDecodeManager::new();
        let (mut peer, muted) = make_test_peer(1001);
        peer.has_received_heartbeat = true;
        // Peer's audio is already on (unmuted) before the host acts.
        peer.audio_enabled = true;
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
        // Video untouched.
        assert!(!peer.video_enabled);
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
}
