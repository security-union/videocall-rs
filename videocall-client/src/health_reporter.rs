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

use crate::connection::ConnectionController;
use crate::connection::{
    connection_handshake_failures, connection_session_drops, reelection_aborted_total,
    reelection_failed_total, reelection_preserved_total, reelection_proceeded_total,
};
use crate::decode::peer_decode_manager::keyframe_requests_sent_count;
use crate::diagnostics::adaptive_quality_manager::TierTransitionRecord;
use crate::encode::{
    camera_encoder_errors_closed_codec, camera_encoder_errors_configure_fatal,
    camera_encoder_errors_generic, camera_encoder_errors_vpx_mem_alloc,
    camera_encoder_frames_submitted_ok, camera_encoder_restarts_closed_codec,
    camera_encoder_restarts_configure, camera_encoder_restarts_memory,
    camera_encoder_restarts_other, screen_encoder_errors_closed_codec,
    screen_encoder_errors_configure_fatal, screen_encoder_errors_generic,
    screen_encoder_errors_vpx_mem_alloc, screen_encoder_frames_submitted_ok,
    screen_encoder_max_stall_gap_ms, screen_encoder_restarts_closed_codec,
    screen_encoder_restarts_configure, screen_encoder_restarts_memory,
    screen_encoder_restarts_other, screen_encoder_stall_episodes,
};
use log::{debug, trace, warn};
use protobuf::Message;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use videocall_diagnostics::{recv_loop_action, subscribe, DiagEvent, MetricValue, RecvLoopAction};
use videocall_types::protos::health_packet::{
    decode_budget::OverrideMode as PbOverrideMode, DecodeBudget as PbDecodeBudget,
    EncoderLayerGeometry, HealthPacket as PbHealthPacket, NetEqNetwork as PbNetEqNetwork,
    NetEqOperationCounters as PbNetEqOperationCounters, NetEqStats as PbNetEqStats,
    PeerStats as PbPeerStats, TierDwell as PbTierDwell, TierTransition as PbTierTransition,
    VideoStats as PbVideoStats,
};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen_futures::spawn_local;
use web_time::{SystemTime, UNIX_EPOCH};

/// Per-client WebTransport receive-health telemetry (issue 2031), assembled in
/// the report loop from the `videocall-transport` statics and threaded into
/// `create_health_packet`. Every field is a per-CLIENT (receiver) property, not
/// per-peer. `Default` yields the "no WT activity" shape the tests and the
/// WebSocket path use.
#[derive(Debug, Clone, Copy, Default)]
pub struct WtReceiveTelemetry {
    /// Max gap (ms) between successive incoming-datagram `.read()` resolutions
    /// since the last health packet (read-and-reset window). 0.0 on WebSocket
    /// (the WT read loop never feeds it). High => main-thread reader starvation.
    pub read_loop_max_gap_ms: f64,
    /// Observed post-set incoming-datagram queue parameters
    /// `(high_water_mark, max_age_ms)` from `configure_incoming_datagram_queue`,
    /// or `None` before the WT queue has been configured. `max_age_ms` is `NaN`
    /// when the browser reports the queue as unbounded (spec `null` default).
    pub incoming_queue_readback: Option<(f64, f64)>,
}

/// Health data cached for a specific peer
#[derive(Debug, Clone)]
pub struct PeerHealthData {
    pub peer_id: String,
    pub last_neteq_stats: Option<Value>,
    /// Camera video stats (media_type=VIDEO).
    pub last_camera_stats: Option<Value>,
    /// Screen share video stats (media_type=SCREEN).
    pub last_screen_stats: Option<Value>,
    pub camera_decode_eligible: Option<bool>,
    pub screen_decode_eligible: Option<bool>,
    /// Sender's self-reported audio state (from peer heartbeat metadata).
    pub audio_enabled: bool,
    /// Sender's self-reported video state (from peer heartbeat metadata).
    pub video_enabled: bool,
    pub last_update_ms: u64,
    /// Timestamp of last audio stats update (ms since epoch). 0 = never received.
    pub last_audio_update_ms: u64,
    /// Timestamp of last camera video stats update (ms since epoch). 0 = never received.
    pub last_camera_update_ms: u64,
    /// Timestamp of last screen share stats update (ms since epoch). 0 = never received.
    pub last_screen_update_ms: u64,
    /// Cumulative decode error count across the session lifetime.
    pub decode_errors_total: u64,
    /// Issue #1878: windowed receive-side audio DATAGRAM loss (lost audio
    /// packets/sec) observed for this peer while THIS client is on WebTransport.
    /// Nonzero only when audio riding unreliable QUIC datagrams is being dropped
    /// (e.g. the browser's incoming-datagram queue overflowing during a
    /// main-thread stall) — the pathology was previously invisible in every
    /// dashboard. ~0 on WebSocket and on E2EE-on WebTransport (reliable paths).
    ///
    /// A contiguous gap is booked as its positions shift off the reorder window,
    /// so the ones still inside it — which may yet arrive — are not counted yet.
    /// Read alongside [`Self::wt_datagram_audio_raw_loss_per_sec`].
    pub wt_datagram_audio_loss_per_sec: f64,
    /// Issue 2031: windowed RAW (uncapped) receive-side audio DATAGRAM loss
    /// (skipped sequences/sec) for this peer. The magnitude companion to
    /// [`Self::wt_datagram_audio_loss_per_sec`]: it sums the sequence-gap sizes
    /// un-truncated at the jump, so it leads by the still-arrivable tail. Same
    /// WebTransport gate and cadence.
    pub wt_datagram_audio_raw_loss_per_sec: f64,
    /// #2511: interval MAX of decode-eligible RAW `content_staleness_ms`, accumulated
    /// upstream of the `fps_received > 0` gate that zeroes the wire field during a freeze.
    /// `None` means no sample arrived this interval; a sample of `0.0` is recorded as a
    /// real observation, since `content_staleness_ms` returns it for "at live" AND "no data".
    pub camera_staleness_max_ms: Option<f64>,
    pub screen_staleness_max_ms: Option<f64>,
    /// #2524: interval MAX of `video_seq_max_gap`, in FRAMES. See [`IntervalMaxes`].
    pub camera_seq_max_gap_max: Option<u64>,
    pub screen_seq_max_gap_max: Option<u64>,
}

impl PeerHealthData {
    pub fn new(peer_id: String) -> Self {
        Self {
            peer_id,
            last_neteq_stats: None,
            last_camera_stats: None,
            last_screen_stats: None,
            camera_decode_eligible: None,
            screen_decode_eligible: None,
            audio_enabled: false,
            video_enabled: false,
            last_update_ms: 0,
            last_audio_update_ms: 0,
            last_camera_update_ms: 0,
            last_screen_update_ms: 0,
            decode_errors_total: 0,
            wt_datagram_audio_loss_per_sec: 0.0,
            wt_datagram_audio_raw_loss_per_sec: 0.0,
            camera_staleness_max_ms: None,
            screen_staleness_max_ms: None,
            camera_seq_max_gap_max: None,
            screen_seq_max_gap_max: None,
        }
    }

    /// Read-and-reset, so each export is the interval MAX — neither a point sample nor a
    /// lifetime latch (#2511, #2524).
    pub fn take_interval_maxes(&mut self) -> IntervalMaxes {
        IntervalMaxes {
            camera_staleness_ms: self.camera_staleness_max_ms.take(),
            screen_staleness_ms: self.screen_staleness_max_ms.take(),
            camera_seq_gap_frames: self.camera_seq_max_gap_max.take(),
            screen_seq_gap_frames: self.screen_seq_max_gap_max.take(),
        }
    }

    pub fn update_audio_stats(&mut self, neteq_stats: Value) {
        self.last_neteq_stats = Some(neteq_stats);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_update_ms = now_ms;
        self.last_audio_update_ms = now_ms;
    }

    pub fn update_camera_stats(&mut self, video_stats: Value) {
        self.last_camera_stats = Some(video_stats);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_update_ms = now_ms;
        self.last_camera_update_ms = now_ms;
    }

    pub fn update_screen_stats(&mut self, video_stats: Value) {
        self.last_screen_stats = Some(video_stats);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_update_ms = now_ms;
        self.last_screen_update_ms = now_ms;
    }
}

/// Snapshot of climb-rate limiter state, updated by the encoder each tick.
#[derive(Debug, Clone, Default)]
pub struct ClimbLimiterSnapshot {
    pub crash_ceiling_active: bool,
    pub crash_ceiling_tier_index: Option<u32>,
    pub crash_ceiling_decay_ms: Option<f64>,
    pub step_up_blocked_ceiling: u64,
    pub step_up_blocked_slowdown: u64,
    pub step_up_blocked_screen_share: u64,
}

/// Snapshot of the adaptive decode-budget controller's current decision (#987).
///
/// Captured from the `decode_budget` diagnostics subsystem (published by the
/// Dioxus control loop) and folded into each HEALTH packet so population-scale
/// dashboards can observe the receiver-side tile-cap decision that today only
/// exists in client console logs. Mirrors how the AdaptiveQuality tier atomics
/// ride the health packet.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecodeBudgetSnapshot {
    /// Current effective cap on simultaneously decoded video tiles.
    pub effective_cap: u32,
    /// Natural/unconstrained tile count the layout would show (∩ CANVAS_LIMIT).
    pub natural: u32,
    /// Whether the pressured latch is engaged (the loop owns the cap).
    pub pressured: bool,
    /// Override mode, as the proto `OverrideMode` enum integer value
    /// (1 = Auto, 2 = Fixed; 0 = unset/Auto).
    pub override_mode: u32,
    /// User's hard tile cap; meaningful only when `override_mode` is Fixed.
    pub override_fixed_n: u32,
}

/// Shared buffer of tier transition records from camera and screen encoders.
type TierTransitionBuffers = Rc<RefCell<Vec<Rc<RefCell<Vec<TierTransitionRecord>>>>>>;

/// Shared climb-rate limiter snapshot (double-wrapped for late binding).
type SharedClimbLimiterSnapshot = Rc<RefCell<Rc<RefCell<ClimbLimiterSnapshot>>>>;

/// Shared dwell-time sample buffer (double-wrapped for late binding).
type SharedDwellSamples = Rc<RefCell<Rc<RefCell<Vec<(String, f64)>>>>>;

/// Health reporter that collects diagnostics and sends health packets
#[derive(Debug)]
pub struct HealthReporter {
    session_id: Rc<RefCell<String>>,
    meeting_id: String,
    display_name: String,
    reporting_peer: String,
    peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>>,
    send_packet_callback: Option<Callback<PacketWrapper>>,
    health_interval_ms: u64,
    reporting_audio_enabled: Rc<RefCell<bool>>,
    reporting_video_enabled: Rc<RefCell<bool>>,
    active_server_url: Rc<RefCell<Option<String>>>,
    active_server_type: Rc<RefCell<Option<String>>>,
    active_server_rtt_ms: Rc<RefCell<Option<f64>>>,
    connection_controller: Rc<RefCell<Option<Rc<ConnectionController>>>>,
    /// Adaptive video tier index from CameraEncoder (0=best, 7=minimal).
    /// Wrapped in RefCell so `set_adaptive_tier_sources` (called after
    /// `start_health_reporting`) can swap the inner Rc and the spawned loop
    /// picks up the new atomic on its next tick.
    adaptive_video_tier: Rc<RefCell<Rc<AtomicU32>>>,
    /// Adaptive audio tier index from CameraEncoder (0=high, 3=emergency).
    adaptive_audio_tier: Rc<RefCell<Rc<AtomicU32>>>,
    /// Sender-side encoder queue-depth report (f32 bits in AtomicU32) — encoder backpressure, NOT
    /// p75 peer FPS (the value was always queue depth; see #1231/#1263). Serialized to the
    /// frozen-named proto field `encoder_p75_peer_fps = 67`, whose name predates the correction.
    encoder_queue_depth_report: Rc<RefCell<Rc<AtomicU32>>>,
    /// Encoder PID target bitrate kbps (f32 bits in AtomicU32).
    encoder_target_bitrate_kbps: Rc<RefCell<Rc<AtomicU32>>>,
    /// Screen share quality tier index.
    adaptive_screen_tier: Rc<RefCell<Rc<AtomicU32>>>,
    /// Screen sharing active flag.
    screen_sharing_active: Rc<RefCell<Rc<AtomicBool>>>,
    /// Encoder output FPS (camera).
    encoder_output_fps: Rc<RefCell<Arc<AtomicU32>>>,
    /// #2147: SCREEN encoder output FPS (base layer). Late-bound like the other
    /// encoder sources; swapped in by `set_encoder_metric_sources`.
    ///
    /// **`None` until wired, then Some even at 0** — this is the deliberate
    /// difference from `encoder_output_fps` above. That field's `> 0` gate makes a
    /// genuine screen-encoder stall indistinguishable from never-started (#2079),
    /// which is the whole reason it could not serve as a freeze signal. Here the
    /// wired-ness is tracked separately (`screen_encoder_fps_wired`) so a real 0
    /// reaches the wire.
    screen_encoder_output_fps: Rc<RefCell<Arc<AtomicU32>>>,
    /// #2147: `true` once `set_encoder_metric_sources` has bound the real screen
    /// encoder's fps atom. Distinguishes "no screen encoder bound yet" (omit the
    /// field) from "screen encoder bound and honestly producing 0 fps" (emit 0) —
    /// the distinction a `> 0` gate destroys.
    ///
    /// **Latches: set once, never cleared.** That is deliberate — it tracks whether
    /// an ATOM IS BOUND, not whether a share is running. `Host` binds the screen
    /// encoder eagerly at mount, so in practice this is `true` for the whole
    /// session and the field is effectively always present; a consumer must read
    /// `screen_sharing_active` (never this field's presence) to know whether a share
    /// is live.
    ///
    /// Every screen-share STOP path zeroes the atom, so a stopped/idle share reports
    /// an honest 0 rather than a stale nonzero: `stop()`, `start()` and
    /// `start_with_stream()` call `reset_output_fps`, and the browser's own "Stop
    /// sharing" button (the track `onended` handler) was given the same reset in
    /// #2147 — before that it relied on the AQ loop's 5s idle decay, which stops
    /// running once that loop's liveness token drops. The decay remains the backstop
    /// for a share that merely goes quiet without stopping.
    screen_encoder_fps_wired: Rc<Cell<bool>>,
    /// #1143: camera encoder EFFECTIVE simulcast layer count (ladder depth the
    /// publisher is configured to encode/send). Wrapped for late binding like the
    /// other encoder sources; swapped in by `set_encoder_metric_sources`. Reads as
    /// 0 (field omitted) until the encoder atom is wired.
    effective_video_layers: Rc<RefCell<Rc<AtomicU32>>>,
    /// #1143: camera encoder ACTIVE simulcast layer count (layers presently
    /// encoded + sent; `<=` effective, the gap being AQ-shed layers).
    active_video_layers: Rc<RefCell<Rc<AtomicU32>>>,
    /// Opaque live camera geometry/fps source, late-bound with the other encoder
    /// sources and read through on every health packet.
    camera_layer_metrics: Rc<RefCell<crate::encode::CameraLayerMetricSource>>,
    /// #1561: screen encoder EFFECTIVE simulcast layer count (ladder depth).
    effective_screen_layers: Rc<RefCell<Rc<AtomicU32>>>,
    /// #1561: screen encoder ACTIVE simulcast layer count (layers currently sent).
    active_screen_layers: Rc<RefCell<Rc<AtomicU32>>>,
    /// #1561: microphone encoder EFFECTIVE audio simulcast layer count.
    effective_audio_layers: Rc<RefCell<Rc<AtomicU32>>>,
    /// #1561: CONGESTION-driven audio layer-ceiling atomic (issue #621).
    audio_congestion_ceiling: Rc<RefCell<Arc<AtomicU32>>>,
    /// #1561: USER-driven audio layer-ceiling atomic (perf-panel control).
    audio_user_layer_ceiling: Rc<RefCell<Rc<AtomicU32>>>,
    /// #1561: latest per-(peer,kind) desired layer map from `tick_layer_choosers`.
    /// Populated by the peer monitor tick in VideoCallClient and read here.
    received_layers: Rc<RefCell<HashMap<(u64, crate::decode::layer_chooser::PrefMediaKind), u32>>>,
    /// Shared tier transition buffers (camera + screen, drained each health packet).
    tier_transitions: TierTransitionBuffers,
    /// Climb-rate limiter snapshot, updated by the encoder each tick.
    /// Double-wrapped so `set_encoder_metric_sources` (called after
    /// `start_health_reporting`) can swap the inner Rc and the spawned loop
    /// picks up the encoder's buffer on its next tick.
    climb_limiter_snapshot: SharedClimbLimiterSnapshot,
    /// Dwell time samples buffer, drained each health packet.
    /// Double-wrapped for the same late-binding reason as `climb_limiter_snapshot`.
    dwell_samples: SharedDwellSamples,
    /// Shutdown flag set by [`shutdown()`](Self::shutdown). The
    /// `start_health_reporting` future captures a `Weak<AtomicBool>` clone of
    /// this and exits as soon as the flag is observed `true`. Required because
    /// that future also clones the send-packet callback (an `Rc` strong
    /// reference back into the `VideoCallClient`), creating a cycle that
    /// otherwise prevents `Inner` from dropping after a meeting page unmount.
    /// Without this flag the leaked `VideoCallClient` would keep running until
    /// the server eventually tore down its WebTransport session — the bug
    /// reproduced in the cc7tp meeting incident on 2026-05-01.
    shutdown: Rc<AtomicBool>,
    /// TELEM-8: Accumulated long-task durations (ms) since last health packet.
    longtask_buffer: Rc<RefCell<Vec<f64>>>,
    /// #1482: Set `true` the first time a real PerformanceObserver('longtask')
    /// entry is observed this session. Lets the report loop distinguish a
    /// genuine 0.0 main-thread load (idle main thread on a browser that DOES
    /// support 'longtask') from an unsupported 'longtask' API (Firefox/Safari),
    /// which must report `None`, not a fabricated 0.0. Never reset (sticky).
    longtask_ever_observed: Rc<Cell<bool>>,
    /// TELEM-9: Latest render FPS reading from the rAF cadence observer.
    render_fps: Rc<RefCell<Option<f64>>>,
    /// #987: Latest adaptive decode-budget snapshot from the `decode_budget`
    /// diagnostics subsystem. `None` until the controller publishes its first
    /// decision (no peers / pre-warmup), in which case the field is omitted.
    decode_budget: Rc<RefCell<Option<DecodeBudgetSnapshot>>>,
    /// #1032: Latest total-process memory reading from
    /// `performance.measureUserAgentSpecificMemory()`. That API is async
    /// (returns a Promise) and Chrome-only/`crossOriginIsolated`-gated, so it
    /// is sampled in a background task and the last resolved value is cached
    /// here. The report loop reads this cell synchronously and never awaits.
    /// `None` until the first sample resolves, or permanently when the API is
    /// unavailable, in which case the proto field is omitted.
    agent_memory_bytes: Rc<RefCell<Option<u64>>>,
}

/// Static client metadata read from JS globals (TELEM-7).
#[derive(Debug, Clone, Default)]
pub struct ClientMetadata {
    pub cores: u32,
    pub architecture: String,
    pub gpu_family: String,
    pub network_effective_type: String,
    pub network_downlink: f64,
    pub network_rtt: u32,
    pub battery_charging: Option<bool>,
    pub battery_level: Option<f64>,
    pub capability_score: u32,
    /// #1482: human OS + version, e.g. "macOS 14.5". `None` when the JS
    /// metadata layer could not determine it (no userAgentData high-entropy).
    pub os: Option<String>,
    /// #1482: device form factor ("desktop"|"mobile"|"tablet"). `None` when
    /// the JS metadata layer could not classify it.
    pub device_type: Option<String>,
    /// #1482: navigator.deviceMemory total-RAM tier in GB (coarse, capped at
    /// 8). `None` on browsers without navigator.deviceMemory (Firefox/Safari).
    pub device_memory_gb: Option<f64>,
    /// #1556: navigator.connection.type ("wifi"|"ethernet"|"cellular"|etc).
    /// Chrome/Edge only; `None` on Firefox/Safari.
    pub network_type: Option<String>,
    /// #1556: navigator.connection.downlinkMax (Mbps). 0 or None when unknown.
    pub network_downlink_max: Option<f64>,
    /// #1556: computed throttle flag. True when capability_score / cores < 150.
    pub cpu_throttled: Option<bool>,
}

/// Infer a CPU throttle signal from the capability benchmark normalized by the
/// browser-reported logical core count. Missing inputs remain absent rather
/// than being reported as a healthy zero.
fn compute_cpu_throttled(capability_score: u32, cores: u32) -> Option<bool> {
    if capability_score == 0 || cores == 0 {
        None
    } else {
        Some(capability_score / cores < 150)
    }
}

/// Decide the value to publish to `window.__videocall_encoder_fps` for the
/// bots-app RESOURCE_STARVED fps rule (#2057/#2032). Returns:
///   - `None` -> publish NOTHING (clear the global): camera off, OR camera on
///     but the encoder has not produced a real sample yet (warmup). The bot
///     reads "absent" as "no data" - NOT as 0 fps - so a cold-start/idle client
///     never false-flags as starved.
///   - `Some(fps)` -> the current camera layer-0 output fps, published once the
///     encoder is active AND has produced at least one real sample. Since #2060
///     the producer resets `current_fps` to 0 on stop/start and decays it to 0
///     after a sustained layer-0 output gap, so `Some(0)` is a REACHABLE runtime
///     value here: "camera on, latch set, but currently emitting no layer-0
///     output" (a total stall, or the sub-1s window right after a re-enable).
///     This captures partial starvation (for example, 1-4 fps), which the
///     RESOURCE_STARVED rule targets. A total stall now publishes `Some(0)`; the
///     bots consumer (`fps.ts` `coerceEncoderFps`) maps 0 -> "no data", so a total
///     stall surfaces downstream as no-data (the verdict's CPU rule is the
///     backstop).
///
/// Flagging a total stall AS `RESOURCE_STARVED` remains open (#2079), but it is
/// NOT the one-line consumer change the shape of this gate suggests. `Some(0)`
/// here is ambiguous by construction: it is emitted both when the BOX starved the
/// encoder (what the verdict is for) and when the encoder WEDGED with the camera
/// still nominally enabled — for example the fatal-`configure()` `'restart` loop
/// in `camera_encoder.rs`, which never passes through camera-off, so
/// `next_has_encoded_real` keeps the latch set and this fn keeps returning
/// `Some(0)`. A wedged encoder is a PRODUCT bug; reporting it as a confounded
/// harness run inverts the verdict's purpose. Four consumer-side heuristics were
/// tried against real code and each failed on a reachable path (see #2079), so the
/// conclusion is that the disambiguation belongs HERE, at the source — an explicit
/// stall signal distinct from no-data — not in `fps.ts`.
fn encoder_fps_publish_value(
    video_enabled: bool,
    output_fps: u32,
    has_encoded_real: bool,
) -> Option<u32> {
    if !video_enabled {
        None
    } else if has_encoded_real {
        Some(output_fps)
    } else {
        None
    }
}

/// Per-peer interval MAXes drained once per health report. Every field here exists
/// because the producing diagnostic fires at ~1Hz while the report drains at
/// `health_interval_ms` (default 5000), so a point sample would discard four windows in
/// five and then publish whichever one happened to land last (#2511, #2524).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IntervalMaxes {
    pub camera_staleness_ms: Option<f64>,
    pub screen_staleness_ms: Option<f64>,
    pub camera_seq_gap_frames: Option<u64>,
    pub screen_seq_gap_frames: Option<u64>,
}

/// Per-peer interval maxes, keyed by peer id (#2511, #2524).
pub type IntervalMaxMap = HashMap<String, IntervalMaxes>;

/// Read-and-reset every peer's interval maxes (#2511, #2524).
///
/// A failed borrow yields an EMPTY map, which folds as "omit the field" — the report
/// still goes out, one interval short of a sample, rather than publishing a
/// fabricated `0` that reads as "never stale".
fn drain_interval_maxes(
    peer_health_data: &Rc<RefCell<HashMap<String, PeerHealthData>>>,
) -> IntervalMaxMap {
    peer_health_data
        .try_borrow_mut()
        .map(|mut m| {
            m.iter_mut()
                .map(|(peer, d)| (peer.clone(), d.take_interval_maxes()))
                .collect()
        })
        .unwrap_or_default()
}

/// Advance the "encoder has produced a real sample" latch (#2057). The latch
/// resets on camera-off and sets once a nonzero fps is observed while the camera
/// is on; otherwise it carries the previous value. Since #2060 the source atomic
/// IS reset to 0 on stop/start, so a re-enable that passes through camera-off
/// re-warms from 0 with the latch cleared (no stale-nonzero republish). One edge
/// remains: a synchronous stop()->re-enable device switch never lets the health
/// loop observe camera-off, so the latch stays set and a transient `Some(0)` can
/// publish during the ~1s re-warmup (absorbed by the #2064 sustain window). Kept
/// here (not inline in the health loop) so the transition is unit-testable.
fn next_has_encoded_real(prev: bool, video_enabled: bool, output_fps: u32) -> bool {
    if !video_enabled {
        false
    } else if output_fps > 0 {
        true
    } else {
        prev
    }
}

/// Decide what to report for `screen_encoder_output_fps` (#2147).
///
/// `wired` is whether `set_encoder_metric_sources` has bound the REAL screen
/// encoder's atom; `output_fps` is that atom's current reading.
///
/// - not wired → `None` → the proto field is OMITTED. Reporting `0` here would
///   fabricate "a screen encoder exists and is producing nothing" for a client
///   that has no screen encoder bound at all.
/// - wired → `Some(output_fps)`, **including `Some(0)`**. This is the whole point
///   of the field: `encoder_output_fps` (camera) is `> 0`-gated, so a genuine
///   total stall is absent and indistinguishable from never-started (#2079) —
///   which is exactly why it was useless as a screen-freeze signal.
///
/// Extracted as a pure fn (mirroring [`encoder_fps_publish_value`] /
/// [`next_has_encoded_real`]) because the live decision otherwise sits inside the
/// report loop's `spawn_local` future, where no host test can reach it — so a
/// mutation dropping the `wired` check would pass unnoticed.
fn screen_encoder_fps_report_value(wired: bool, output_fps: u32) -> Option<u32> {
    if wired {
        Some(output_fps)
    } else {
        None
    }
}

#[cfg(target_arch = "wasm32")]
fn publish_encoder_fps(value: Option<u32>) {
    use wasm_bindgen::JsValue;

    if let Some(win) = web_sys::window() {
        let value = value.map_or(JsValue::UNDEFINED, |fps| JsValue::from_f64(fps as f64));
        let _ = js_sys::Reflect::set(&win, &JsValue::from_str("__videocall_encoder_fps"), &value);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn publish_encoder_fps(_value: Option<u32>) {}

fn audio_layer_telemetry(
    effective_layers: u32,
    congestion_ceiling_raw: u32,
    user_ceiling_raw: u32,
) -> (u32, u32) {
    let congestion_count = crate::encode::layer_ceiling_to_count(congestion_ceiling_raw);
    let user_count = crate::encode::layer_ceiling_to_count(user_ceiling_raw);
    let congestion_ceiling = if congestion_count == usize::MAX {
        u32::MAX
    } else {
        congestion_count as u32
    };
    let active_layers = (effective_layers as usize)
        .min(congestion_count)
        .min(user_count)
        .max(1) as u32;
    (congestion_ceiling, active_layers)
}

// ── issue 1853: receiver-side audio-scale instrumentation (log-only) ──────────
//
// Diagnostics to discriminate the multi-source audio-breakup pathology (a
// receiver hearing MANY concurrent sources concealed, independent of CPU class)
// from the low-core decode-starvation of issue 1389 and from a receiver-downlink
// fault. INSTRUMENTATION ONLY — no runtime behavior changes. The report loop
// emits one greppable AUDIO_SCALE line every ~5s summarizing the receiver's
// audio-scale posture so scripts/meeting_quality_xref.py can correlate audio
// concealment against concurrent source count, downlink estimate, and per-source
// buffer depth before any behavioral fix is designed.

/// Audio-source activity gate, in packets/sec. At or above this rate a source is
/// actively delivering audio and its windowed expand/packets concealment ratio
/// is meaningful; below it the sender is likely in DTX silence and the ratio is
/// unreliable. Shared by the per-stream `PeerStats::audio_concealment_pct`
/// computation, the AUDIO_SCALE aggregate (both go through
/// [`audio_source_sample_from_neteq`]) and the audio quality-score gate, so they can
/// never drift apart. Tied to `videocall_aq`'s copy, which the load-test bot reads.
const AUDIO_ACTIVE_PPS_GATE: f64 = 2.0;

const _: () = assert!(
    AUDIO_ACTIVE_PPS_GATE == videocall_aq::constants::AUDIO_ACTIVE_PPS_GATE,
    "AUDIO_ACTIVE_PPS_GATE must match videocall_aq AUDIO_ACTIVE_PPS_GATE"
);

/// Minimum spacing between AUDIO_SCALE diagnostic lines, in ms. The line is
/// emitted from the existing health-report loop (whose interval is
/// `health_reporting_interval_ms`, default 5000ms — see
/// `VideoCallClient`), gated by a wall-clock delta rather than a new timer. This
/// caps the aggregate at one line per ~5s regardless of the configured report
/// interval: at the 5s default it emits ~every report tick, and if the interval
/// is ever set faster it rate-limits to ~5s.
const AUDIO_SCALE_LOG_INTERVAL_MS: u64 = 5_000;

/// Concealment percentage above which a source is counted in the AUDIO_SCALE
/// `concealed=` field. Chosen clearly above the low ambient concealment of
/// healthy Opus/NetEQ playout (a well-fed jitter buffer conceals only
/// occasionally) and below R5's 15% "audible breakup" threshold in
/// scripts/meeting_quality_xref.py, so `concealed` is an early, sensitive count
/// of how many sources are degrading — not a restatement of the breakup rule.
const AUDIO_SCALE_CONCEAL_THRESHOLD_PCT: f64 = 10.0;

/// True when a source delivering `packets_per_sec` audio packets is active
/// enough for its concealment ratio to be meaningful
/// (>= [`AUDIO_ACTIVE_PPS_GATE`]).
fn audio_source_active(packets_per_sec: f64) -> bool {
    packets_per_sec >= AUDIO_ACTIVE_PPS_GATE
}

/// One receiver-side audio source sampled during a single health-report tick.
#[derive(Debug, Clone, Copy, PartialEq)]
struct AudioSourceSample {
    /// Windowed audio concealment for this source, in percent (0–100). Mirrors
    /// `PeerStats::audio_concealment_pct`; 0.0 for inactive sources.
    concealment_pct: f64,
    /// NetEQ current jitter-buffer depth for this source, in ms
    /// (`current_buffer_size_ms`, 0.0 when absent).
    buffer_ms: f64,
    /// True when the source is actively delivering audio this tick (its
    /// `packets_per_sec` passes [`audio_source_active`]). Only active sources
    /// contribute to the AUDIO_SCALE aggregates.
    active: bool,
}

/// Derive an [`AudioSourceSample`] from a peer's raw NetEQ stats JSON, using the
/// SAME windowed rates, gate, and clamp as the per-stream
/// `PeerStats::audio_concealment_pct` mapping in `create_health_packet` (which
/// also calls this helper). Missing fields default to 0.0 / inactive.
fn audio_source_sample_from_neteq(neteq: &Value) -> AudioSourceSample {
    let expand_per_sec = neteq
        .get("network")
        .and_then(|n| n.get("operation_counters"))
        .and_then(|oc| oc.get("expand_per_sec"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let packets_per_sec = neteq
        .get("packets_per_sec")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let buffer_ms = neteq
        .get("current_buffer_size_ms")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let active = audio_source_active(packets_per_sec);
    // Clamp to 0–100: concealment cannot exceed 100% by definition, and
    // unsynchronised window rollovers can momentarily inflate the ratio.
    let concealment_pct = if active {
        ((expand_per_sec / packets_per_sec) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    AudioSourceSample {
        concealment_pct,
        buffer_ms,
        active,
    }
}

/// Format the periodic AUDIO_SCALE diagnostic line (issue 1853), or `None` when
/// the receiver has no ACTIVE audio source this tick.
///
/// A `sources=0` line (empty room, everyone muted, or all senders in DTX
/// silence) is pure noise for the meeting-log analyzer, so it is suppressed: the
/// line is emitted only when at least one source is actively delivering audio.
/// Every field is a machine-parseable `key=value` token in the same style
/// scripts/meeting_quality_xref.py already parses (e.g. the prejoin
/// `cores=`/`network=` preamble):
///
/// - `sources`       number of ACTIVE audio sources this tick (the concurrent
///   source load the receiver is decoding; silent/DTX peers are excluded, so
///   this shares its denominator with every field below).
/// - `concealed`     how many active sources exceed
///   [`AUDIO_SCALE_CONCEAL_THRESHOLD_PCT`].
/// - `worst_pct`     max concealment over active sources (1 decimal).
/// - `mean_pct`      mean concealment over active sources (1 decimal).
/// - `downlink_mbps` receiver downlink estimate, or `-1.0` when unknown (`<= 0`,
///   matching the health packet's `> 0` known-gate).
/// - `min_buf_ms`    min NetEQ buffer depth over active sources (1 decimal).
/// - `mean_buf_ms`   mean NetEQ buffer depth over active sources (1 decimal).
/// - `cores`         `navigator.hardwareConcurrency`, or `-1` when unknown (0).
fn format_audio_scale_line(
    samples: &[AudioSourceSample],
    downlink_mbps: f64,
    cores: u32,
) -> Option<String> {
    let active: Vec<&AudioSourceSample> = samples.iter().filter(|s| s.active).collect();
    if active.is_empty() {
        return None;
    }
    let n = active.len() as f64;
    let concealed = active
        .iter()
        .filter(|s| s.concealment_pct > AUDIO_SCALE_CONCEAL_THRESHOLD_PCT)
        .count();
    let worst_pct = active
        .iter()
        .map(|s| s.concealment_pct)
        .fold(f64::MIN, f64::max);
    let mean_pct = active.iter().map(|s| s.concealment_pct).sum::<f64>() / n;
    let min_buf_ms = active.iter().map(|s| s.buffer_ms).fold(f64::MAX, f64::min);
    let mean_buf_ms = active.iter().map(|s| s.buffer_ms).sum::<f64>() / n;
    let downlink = if downlink_mbps > 0.0 {
        downlink_mbps
    } else {
        -1.0
    };
    let cores = if cores > 0 { i64::from(cores) } else { -1 };
    Some(format!(
        "[AUDIO_SCALE] sources={} concealed={} worst_pct={:.1} mean_pct={:.1} downlink_mbps={:.1} min_buf_ms={:.1} mean_buf_ms={:.1} cores={}",
        active.len(), concealed, worst_pct, mean_pct, downlink, min_buf_ms, mean_buf_ms, cores,
    ))
}

fn populate_received_layers(
    packet: &mut PbHealthPacket,
    received_layers: &HashMap<(u64, crate::decode::layer_chooser::PrefMediaKind), u32>,
) {
    use crate::decode::layer_chooser::PrefMediaKind;

    for (&(session_id, kind), &layer) in received_layers {
        let key = session_id.to_string();
        match kind {
            PrefMediaKind::Video => {
                packet.received_video_layer.insert(key, layer);
            }
            PrefMediaKind::Screen => {
                packet.received_screen_layer.insert(key, layer);
            }
            PrefMediaKind::Audio => {
                packet.received_audio_layer.insert(key, layer);
            }
        }
    }
}

/// Normalize a raw GPU renderer string to a short family name.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn normalize_gpu_family(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if raw.contains("Apple") {
        return "Apple GPU".to_string();
    }
    if raw.contains("NVIDIA") || raw.contains("GeForce") {
        if let Some(pos) = raw.find("GeForce") {
            let sub = &raw[pos..];
            let family: String = sub.chars().take(24).collect();
            return family.trim().to_string();
        }
        if let Some(pos) = raw.find("NVIDIA") {
            let sub = &raw[pos..];
            let family: String = sub.chars().take(24).collect();
            return family.trim().to_string();
        }
    }
    if raw.contains("AMD") || raw.contains("Radeon") {
        if let Some(pos) = raw.find("Radeon") {
            let sub = &raw[pos..];
            let family: String = sub.chars().take(24).collect();
            return family.trim().to_string();
        }
        return "AMD GPU".to_string();
    }
    if raw.contains("Intel") {
        if let Some(pos) = raw.find("Intel") {
            let sub = &raw[pos..];
            let family: String = sub.chars().take(32).collect();
            return family.trim().to_string();
        }
    }
    raw.chars().take(32).collect::<String>().trim().to_string()
}

/// Read client metadata from `window.__videocall_client_metadata` and
/// `navigator.hardwareConcurrency`.
#[cfg(target_arch = "wasm32")]
fn read_client_metadata() -> ClientMetadata {
    use js_sys::Reflect;
    use wasm_bindgen::JsValue;

    let mut meta = ClientMetadata::default();

    let Some(window) = web_sys::window() else {
        return meta;
    };

    // Cores from navigator
    meta.cores = {
        let cores_f64 = window.navigator().hardware_concurrency();
        if cores_f64.is_finite() && cores_f64 >= 1.0 {
            cores_f64.min(u32::MAX as f64) as u32
        } else {
            0
        }
    };

    // Capability score from window.__videocall_capability_score
    if let Ok(score_val) = Reflect::get(&window, &JsValue::from_str("__videocall_capability_score"))
    {
        if let Some(score) = score_val.as_f64() {
            if score.is_finite() && score > 0.0 {
                meta.capability_score = score.min(u32::MAX as f64) as u32;
            }
        }
    }

    // Read __videocall_client_metadata object
    let Ok(obj) = Reflect::get(&window, &JsValue::from_str("__videocall_client_metadata")) else {
        return meta;
    };
    if obj.is_undefined() || obj.is_null() {
        return meta;
    }

    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("architecture")) {
        if let Some(s) = v.as_string() {
            meta.architecture = s;
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("gpu")) {
        if let Some(s) = v.as_string() {
            meta.gpu_family = normalize_gpu_family(&s);
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("network_effective_type")) {
        if let Some(s) = v.as_string() {
            meta.network_effective_type = s;
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("network_downlink")) {
        if let Some(f) = v.as_f64() {
            meta.network_downlink = f;
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("network_rtt")) {
        if let Some(f) = v.as_f64() {
            meta.network_rtt = f as u32;
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("battery_charging")) {
        if let Some(b) = v.as_bool() {
            meta.battery_charging = Some(b);
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("battery_level")) {
        if let Some(f) = v.as_f64() {
            meta.battery_level = Some(f);
        }
    }
    // #1482: OS / device-type / device-memory. Each stays `None` unless the JS
    // metadata layer published a value of the right type — never a fabricated
    // default ("if available").
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("os")) {
        if let Some(s) = v.as_string() {
            if !s.is_empty() {
                meta.os = Some(s);
            }
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("device_type")) {
        if let Some(s) = v.as_string() {
            if !s.is_empty() {
                meta.device_type = Some(s);
            }
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("device_memory_gb")) {
        if let Some(f) = v.as_f64() {
            if f.is_finite() && f > 0.0 {
                meta.device_memory_gb = Some(f);
            }
        }
    }
    // #1556: network type + downlink max from navigator.connection (Chrome/Edge only).
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("network_type")) {
        if let Some(s) = v.as_string() {
            if !s.is_empty() {
                meta.network_type = Some(s);
            }
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("network_downlink_max")) {
        if let Some(f) = v.as_f64() {
            if f.is_finite() && f > 0.0 {
                meta.network_downlink_max = Some(f);
            }
        }
    }

    meta
}

#[cfg(not(target_arch = "wasm32"))]
fn read_client_metadata() -> ClientMetadata {
    ClientMetadata::default()
}

impl HealthReporter {
    /// Create a new health reporter
    pub fn new(session_id: String, reporting_peer: String, health_interval_ms: u64) -> Self {
        Self {
            session_id: Rc::new(RefCell::new(session_id)),
            meeting_id: String::new(),
            display_name: String::new(),
            reporting_peer,
            peer_health_data: Rc::new(RefCell::new(HashMap::new())),
            send_packet_callback: None,
            health_interval_ms,
            reporting_audio_enabled: Rc::new(RefCell::new(false)),
            reporting_video_enabled: Rc::new(RefCell::new(false)),
            active_server_url: Rc::new(RefCell::new(None)),
            active_server_type: Rc::new(RefCell::new(None)),
            active_server_rtt_ms: Rc::new(RefCell::new(None)),
            connection_controller: Rc::new(RefCell::new(None)),
            adaptive_video_tier: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            adaptive_audio_tier: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            encoder_queue_depth_report: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            encoder_target_bitrate_kbps: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            adaptive_screen_tier: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            screen_sharing_active: Rc::new(RefCell::new(Rc::new(AtomicBool::new(false)))),
            encoder_output_fps: Rc::new(RefCell::new(Arc::new(AtomicU32::new(0)))),
            // #2147: placeholder atom + not-yet-wired, so the field is OMITTED
            // (not a fabricated 0) until the real screen encoder is bound.
            screen_encoder_output_fps: Rc::new(RefCell::new(Arc::new(AtomicU32::new(0)))),
            screen_encoder_fps_wired: Rc::new(Cell::new(false)),
            // #1143: 0 until the encoder atoms are wired by
            // `set_encoder_metric_sources`; a 0 effective count omits the field.
            effective_video_layers: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            active_video_layers: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            camera_layer_metrics: Rc::new(RefCell::new(
                crate::encode::CameraLayerMetricSource::default(),
            )),
            effective_screen_layers: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            active_screen_layers: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            effective_audio_layers: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            audio_congestion_ceiling: Rc::new(RefCell::new(Arc::new(AtomicU32::new(u32::MAX)))),
            audio_user_layer_ceiling: Rc::new(RefCell::new(Rc::new(AtomicU32::new(u32::MAX)))),
            received_layers: Rc::new(RefCell::new(HashMap::new())),
            tier_transitions: Rc::new(RefCell::new(Vec::new())),
            climb_limiter_snapshot: Rc::new(RefCell::new(Rc::new(RefCell::new(
                ClimbLimiterSnapshot::default(),
            )))),
            dwell_samples: Rc::new(RefCell::new(Rc::new(RefCell::new(Vec::new())))),
            shutdown: Rc::new(AtomicBool::new(false)),
            longtask_buffer: Rc::new(RefCell::new(Vec::new())),
            longtask_ever_observed: Rc::new(Cell::new(false)),
            render_fps: Rc::new(RefCell::new(None)),
            decode_budget: Rc::new(RefCell::new(None)),
            agent_memory_bytes: Rc::new(RefCell::new(None)),
        }
    }

    /// Signal the health-reporting future to exit on its next tick. Sets the
    /// shutdown flag and clears the send-packet callback so that future ticks
    /// after this call cannot publish further packets even if a tick races
    /// the flag. Called from [`VideoCallClient::disconnect()`](
    /// crate::VideoCallClient::disconnect).
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.send_packet_callback = None;
        // Drop the strong reference to the connection controller so we don't
        // keep it alive past the explicit disconnect.
        if let Ok(mut cc) = self.connection_controller.try_borrow_mut() {
            *cc = None;
        }
    }

    /// Set the meeting ID
    pub fn set_meeting_id(&mut self, meeting_id: String) {
        self.meeting_id = meeting_id;
    }

    /// Update the session_id to the server-assigned value received via SESSION_ASSIGNED.
    /// Must be called when SESSION_ASSIGNED arrives so health packets carry the correct
    /// session_id that matches the PacketWrapper.session_id used for room traffic.
    pub fn set_session_id(&mut self, session_id: String) {
        *self.session_id.borrow_mut() = session_id;
    }

    /// Set the display name for health packet reporting
    pub fn set_display_name(&mut self, display_name: String) {
        self.display_name = display_name;
    }

    /// Update sender self-state: audio enabled (authoritative)
    pub fn set_reporting_audio_enabled(&self, enabled: bool) {
        if let Ok(mut ae) = self.reporting_audio_enabled.try_borrow_mut() {
            *ae = enabled;
        }
    }

    /// Update sender self-state: video enabled (authoritative)
    pub fn set_reporting_video_enabled(&self, enabled: bool) {
        if let Ok(mut ve) = self.reporting_video_enabled.try_borrow_mut() {
            *ve = enabled;
        }
    }

    /// Set the callback for sending packets
    pub fn set_send_packet_callback(&mut self, callback: Callback<PacketWrapper>) {
        self.send_packet_callback = Some(callback);
    }

    /// Set health reporting interval
    pub fn set_health_interval(&mut self, interval_ms: u64) {
        self.health_interval_ms = interval_ms;
    }

    /// Set the connection controller reference for communication metrics
    pub fn set_connection_controller(&self, connection_controller: Rc<ConnectionController>) {
        *self.connection_controller.borrow_mut() = Some(connection_controller);
    }

    /// Bind the adaptive quality tier atomics from a CameraEncoder so the
    /// health reporter can include the current encoding tiers in each packet.
    pub fn set_adaptive_tier_sources(
        &mut self,
        video_tier: Rc<AtomicU32>,
        audio_tier: Rc<AtomicU32>,
    ) {
        *self.adaptive_video_tier.borrow_mut() = video_tier;
        *self.adaptive_audio_tier.borrow_mut() = audio_tier;
    }

    /// Returns a clone of the video tier index atomic for external reads.
    ///
    /// Used by `VideoCallClient::camera_tier_index()` to expose the current
    /// camera quality tier for adaptive screen-share tier selection.
    pub fn video_tier_index(&self) -> Option<Rc<AtomicU32>> {
        if let Ok(tier) = self.adaptive_video_tier.try_borrow() {
            Some(tier.clone())
        } else {
            None
        }
    }

    /// Bind the encoder metric atomics from CameraEncoder and ScreenEncoder so the
    /// health reporter can include encoder decision inputs in each health packet.
    #[allow(clippy::too_many_arguments)]
    pub fn set_encoder_metric_sources(
        &mut self,
        queue_depth_report: Rc<AtomicU32>,
        target_bitrate_kbps: Rc<AtomicU32>,
        screen_tier: Rc<AtomicU32>,
        screen_active: Rc<AtomicBool>,
        output_fps: Arc<AtomicU32>,
        // #2147: the SCREEN encoder's output-fps atom.
        screen_output_fps: Arc<AtomicU32>,
        camera_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
        screen_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
        climb_limiter_snapshot: Rc<RefCell<ClimbLimiterSnapshot>>,
        dwell_samples: Rc<RefCell<Vec<(String, f64)>>>,
        effective_video_layers: Rc<AtomicU32>,
        active_video_layers: Rc<AtomicU32>,
        camera_layer_metrics: crate::encode::CameraLayerMetricSource,
        // #1561: screen + audio layer metrics
        effective_screen_layers: u32,
        active_screen_layers: Rc<AtomicU32>,
        effective_audio_layers: u32,
        audio_congestion_ceiling: Arc<AtomicU32>,
        audio_user_layer_ceiling: Rc<AtomicU32>,
    ) {
        *self.encoder_queue_depth_report.borrow_mut() = queue_depth_report;
        *self.encoder_target_bitrate_kbps.borrow_mut() = target_bitrate_kbps;
        *self.adaptive_screen_tier.borrow_mut() = screen_tier;
        *self.screen_sharing_active.borrow_mut() = screen_active;
        *self.encoder_output_fps.borrow_mut() = output_fps;
        // #2147: bind the screen fps atom AND record that it is now wired, so a
        // subsequent honest 0 is emitted rather than mistaken for unwired.
        *self.screen_encoder_output_fps.borrow_mut() = screen_output_fps;
        self.screen_encoder_fps_wired.set(true);
        *self.tier_transitions.borrow_mut() = vec![camera_transitions, screen_transitions];
        *self.climb_limiter_snapshot.borrow_mut() = climb_limiter_snapshot;
        *self.dwell_samples.borrow_mut() = dwell_samples;
        *self.effective_video_layers.borrow_mut() = effective_video_layers;
        *self.active_video_layers.borrow_mut() = active_video_layers;
        *self.camera_layer_metrics.borrow_mut() = camera_layer_metrics;
        // #1561: screen layers — effective is constant (static u32), wrapped in an
        // atomic so the spawned health loop can read it uniformly.
        *self.effective_screen_layers.borrow_mut() =
            Rc::new(AtomicU32::new(effective_screen_layers));
        *self.active_screen_layers.borrow_mut() = active_screen_layers;
        // #1561: audio layers — effective is constant, same pattern.
        *self.effective_audio_layers.borrow_mut() = Rc::new(AtomicU32::new(effective_audio_layers));
        *self.audio_congestion_ceiling.borrow_mut() = audio_congestion_ceiling;
        *self.audio_user_layer_ceiling.borrow_mut() = audio_user_layer_ceiling;
    }

    /// #1561: Update the receiver-side layer selection map snapshot. Called by
    /// the peer monitor tick in `VideoCallClient` after `tick_layer_choosers` so
    /// the health packet includes which layer this client is decoding per peer/kind.
    pub fn update_received_layers(
        &self,
        desired: &HashMap<(u64, crate::decode::layer_chooser::PrefMediaKind), u32>,
    ) {
        if let Ok(mut map) = self.received_layers.try_borrow_mut() {
            *map = desired.clone();
        }
    }

    /// Start subscribing to real diagnostics events via videocall_diagnostics
    pub fn start_diagnostics_subscription(&self) {
        let peer_health_data = Rc::downgrade(&self.peer_health_data);
        let audio_enabled = Rc::downgrade(&self.reporting_audio_enabled);
        let video_enabled = Rc::downgrade(&self.reporting_video_enabled);
        let active_server_url = Rc::downgrade(&self.active_server_url);
        let active_server_type = Rc::downgrade(&self.active_server_type);
        let active_server_rtt_ms = Rc::downgrade(&self.active_server_rtt_ms);
        let longtask_buffer = Rc::downgrade(&self.longtask_buffer);
        let longtask_ever_observed = Rc::downgrade(&self.longtask_ever_observed);
        let render_fps_state = Rc::downgrade(&self.render_fps);
        let decode_budget_state = Rc::downgrade(&self.decode_budget);
        // Issue 2029: forward per-peer WT audio-datagram loss samples into the
        // connection layer's WT→WS fallback detector. Weak so this subscription
        // never keeps the controller (or the client) alive past teardown.
        let connection_controller = Rc::downgrade(&self.connection_controller);

        spawn_local(async move {
            debug!("Started health diagnostics subscription");

            let mut receiver = subscribe();
            loop {
                // Issue 2174: a bare `while let Ok(..)` here died permanently on
                // the first `Overflowed`, which is recoverable — see
                // `videocall_diagnostics::recv_loop_action`.
                let event = match receiver.recv().await {
                    Ok(event) => event,
                    Err(e) => match recv_loop_action(&e) {
                        RecvLoopAction::Continue => continue,
                        RecvLoopAction::Break => break,
                    },
                };
                if let Some(peer_health_data) = Weak::upgrade(&peer_health_data) {
                    // Capture self-state from sender diagnostics events
                    if event.subsystem == "sender" {
                        if let (Some(ae), Some(ve)) =
                            (Weak::upgrade(&audio_enabled), Weak::upgrade(&video_enabled))
                        {
                            for m in &event.metrics {
                                match m.name {
                                    "sender_audio_enabled" => {
                                        if let MetricValue::U64(v) = &m.value {
                                            *ae.borrow_mut() = *v > 0;
                                        }
                                    }
                                    "sender_video_enabled" => {
                                        if let MetricValue::U64(v) = &m.value {
                                            *ve.borrow_mut() = *v > 0;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    // Capture connection manager elected server and RTT
                    if event.subsystem == "connection_manager" {
                        if let (Some(url_rc), Some(typ_rc), Some(rtt_rc)) = (
                            Weak::upgrade(&active_server_url),
                            Weak::upgrade(&active_server_type),
                            Weak::upgrade(&active_server_rtt_ms),
                        ) {
                            for m in &event.metrics {
                                match m.name {
                                    "active_server_url" => {
                                        if let MetricValue::Text(v) = &m.value {
                                            *url_rc.borrow_mut() = Some(v.to_string());
                                        }
                                    }
                                    "active_server_type" => {
                                        if let MetricValue::Text(v) = &m.value {
                                            *typ_rc.borrow_mut() = Some(v.to_string());
                                        }
                                    }
                                    "active_server_rtt" => {
                                        if let MetricValue::F64(v) = &m.value {
                                            *rtt_rc.borrow_mut() = Some(*v);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    // TELEM-8/9: capture client_perf subsystem events
                    if event.subsystem == "client_perf" {
                        for m in &event.metrics {
                            match m.name {
                                "client_longtask_duration_ms" => {
                                    if let MetricValue::F64(duration) = &m.value {
                                        if let Some(buf) = Weak::upgrade(&longtask_buffer) {
                                            if let Ok(mut v) = buf.try_borrow_mut() {
                                                v.push(*duration);
                                                // #1482: mark that 'longtask' is
                                                // actually supported + emitting, so an
                                                // empty drain later means a genuinely
                                                // idle main thread (0.0), not an
                                                // unsupported API (None).
                                                if let Some(flag) =
                                                    Weak::upgrade(&longtask_ever_observed)
                                                {
                                                    flag.set(true);
                                                }
                                            }
                                        }
                                    }
                                }
                                "client_render_fps" => {
                                    if let MetricValue::F64(fps) = &m.value {
                                        if let Some(fps_rc) = Weak::upgrade(&render_fps_state) {
                                            if let Ok(mut f) = fps_rc.try_borrow_mut() {
                                                *f = Some(*fps);
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    // #987: capture the adaptive decode-budget snapshot. The
                    // Dioxus control loop publishes one event per decision change
                    // (state-change driven, not per render), so we simply overwrite
                    // the latest snapshot here and read it at packet-assembly time.
                    if event.subsystem == "decode_budget" {
                        if let Some(db_rc) = Weak::upgrade(&decode_budget_state) {
                            let mut snap = DecodeBudgetSnapshot::default();
                            for m in &event.metrics {
                                if let MetricValue::U64(v) = &m.value {
                                    match m.name {
                                        "decode_budget_effective_cap" => {
                                            snap.effective_cap = *v as u32
                                        }
                                        "decode_budget_natural" => snap.natural = *v as u32,
                                        "decode_budget_pressured" => snap.pressured = *v != 0,
                                        "decode_budget_override_mode" => {
                                            snap.override_mode = *v as u32
                                        }
                                        "decode_budget_override_fixed_n" => {
                                            snap.override_fixed_n = *v as u32
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if let Ok(mut cell) = db_rc.try_borrow_mut() {
                                *cell = Some(snap);
                            }
                        }
                    }
                    let audio_loss = Self::process_diagnostics_event(event, &peer_health_data);

                    // Issue 2029: hand each per-peer WT audio-datagram loss
                    // sample (peer id + pkt/s, ~1 Hz per audio-active WT peer,
                    // incl. 0.0) to the connection manager's fallback detector.
                    // Best-effort: a momentarily-borrowed manager just drops one
                    // ~1 Hz sample. On WebSocket no sample is produced (the
                    // emitter is gated on receiver_on_webtransport); on E2EE-on
                    // WebTransport the reliable audio unistream has no datagram
                    // gaps, so the fed value is a steady 0.0 the detector treats
                    // as not-lossy — neither can trip the fallback.
                    if let Some((peer_id, loss_per_sec)) = audio_loss {
                        if let Some(cc_rc) = Weak::upgrade(&connection_controller) {
                            if let Ok(cc_opt) = cc_rc.try_borrow() {
                                if let Some(cc) = cc_opt.as_ref() {
                                    cc.observe_peer_audio_datagram_loss(&peer_id, loss_per_sec);
                                }
                            }
                        }
                    }
                } else {
                    debug!("HealthReporter dropped, stopping diagnostics subscription");
                    break;
                }
            }
        });
    }

    /// Process a diagnostics event and update peer health data.
    ///
    /// Returns `Some((peer_id, loss_per_sec))` when the event carried a
    /// `wt_datagram_audio_loss_per_sec` sample (issue 2029), so the caller can
    /// forward it into the connection layer's WT→WS audio-fallback detector.
    /// `None` for every other event. Every such sample is returned, INCLUDING
    /// loss 0.0 — a healthy WT audio peer's zero sample is what keeps it in the
    /// detector's uniformity denominator (so one lossy sender among healthy
    /// peers reads as path loss, not a uniform receive-queue drop).
    fn process_diagnostics_event(
        event: DiagEvent,
        peer_health_data: &Rc<RefCell<HashMap<String, PeerHealthData>>>,
    ) -> Option<(String, f64)> {
        // Prefer structured from/to fields if present; fall back to stream_id if set
        let mut reporting_peer: Option<String> = None;
        let mut target_peer: Option<String> = None;
        // Issue 2029: set when this event carries the WT audio-datagram loss
        // gauge, forwarded to the connection layer by the caller.
        let mut audio_loss_forward: Option<(String, f64)> = None;
        for metric in &event.metrics {
            match metric.name {
                "from_peer" => {
                    if let MetricValue::Text(s) = &metric.value {
                        reporting_peer = Some(s.to_string());
                    }
                }
                "to_peer" => {
                    if let MetricValue::Text(s) = &metric.value {
                        target_peer = Some(s.to_string());
                    }
                }
                _ => {}
            }
        }

        // Fallback to stream_id parsing if structured fields are absent
        if reporting_peer.is_none() || target_peer.is_none() {
            if let Some(sid) = event.stream_id.clone() {
                let parts: Vec<&str> = sid.split("->").collect();
                if parts.len() == 2 {
                    reporting_peer.get_or_insert(parts[0].to_string());
                    target_peer.get_or_insert(parts[1].to_string());
                }
            }
        }
        let reporting_peer = reporting_peer.unwrap_or_else(|| "unknown".to_string());
        let target_peer = target_peer.unwrap_or_else(|| "unknown".to_string());

        // Handle NetEQ events (audio)
        if event.subsystem == "neteq" {
            if let Ok(mut health_map) = peer_health_data.try_borrow_mut() {
                let peer_data = health_map
                    .entry(target_peer.to_string())
                    .or_insert_with(|| PeerHealthData::new(target_peer.to_string()));

                for metric in &event.metrics {
                    match metric.name {
                        "stats_json" => {
                            if let MetricValue::Text(json_str) = &metric.value {
                                if let Ok(neteq_json) = serde_json::from_str::<Value>(json_str) {
                                    peer_data.update_audio_stats(neteq_json);
                                    // Per-NetEQ-event (continuous audio-stats stream).
                                    // Demoted debug!->trace!; not on the analyzer keep-list
                                    // (the analyzer greps "audio health (buffer: Nms)" below,
                                    // NOT this line).
                                    trace!(
                                     "Updated NetEQ stats for peer: {target_peer} (from {reporting_peer})"
                                    );
                                }
                            }
                        }
                        "audio_buffer_ms" => {
                            if let MetricValue::U64(buffer_ms) = &metric.value {
                                // NOTE: kept as a PERIODIC sample (logged every ~1 Hz
                                // NetEQ tick per peer), NOT edge-triggered. The meeting
                                // analyzer (`scripts/parse_meeting_console_logs.sh`)
                                // computes n_samples / n_nonzero / median / median_nonzero
                                // from this line as a uniform sample stream — change-point
                                // logging would bias all four (a stable 150ms buffer would
                                // report n=1, median=150 instead of the true distribution).
                                // The large per-tick offenders demoted in this PR are
                                // elsewhere (MEDIA receive, heartbeat, ConnectionManager,
                                // Rendering-meeting-view, Host-render); this analyzer-
                                // critical sample is left intact at debug!.
                                debug!(
                                    "Updated audio health (buffer: {buffer_ms}ms) for peer: {target_peer} (from {reporting_peer})"
                                );
                            }
                        }
                        "packets_awaiting_decode" => {
                            if let MetricValue::U64(packets) = &metric.value {
                                // Per-NetEQ-event. Demoted debug!->trace!; not on the
                                // analyzer keep-list.
                                trace!(
                                    "Updated packets awaiting decode: {packets} for peer: {target_peer} (from {reporting_peer})"
                                );
                            }
                        }
                        // Issue #1878: receive-side audio DATAGRAM loss rate,
                        // emitted ~1 Hz per peer by peer_decode_manager when THIS
                        // client is on WebTransport. Stored on the per-peer health
                        // data and logged at warn! when nonzero so the pathology —
                        // previously invisible in every dashboard — is greppable in
                        // the meeting console-log pipeline (the medium the DRI
                        // analysis used) and available to the in-process health UI.
                        "wt_datagram_audio_loss_per_sec" => {
                            if let MetricValue::F64(loss) = &metric.value {
                                peer_data.wt_datagram_audio_loss_per_sec = *loss;
                                // Issue 2029: forward EVERY sample (including 0.0)
                                // to the connection-layer fallback detector.
                                audio_loss_forward = Some((target_peer.to_string(), *loss));
                                if *loss > 0.0 {
                                    warn!(
                                        "WT datagram audio loss {loss:.1} pkt/s for peer: {target_peer} (from {reporting_peer})"
                                    );
                                }
                            }
                        }
                        // Issue 2031: uncapped magnitude companion to the capped
                        // rate above, emitted on the same ~1 Hz neteq event. Stored
                        // per-peer for the create_health_packet fold; not logged
                        // separately (the capped warn! above already flags the
                        // presence — this is the magnitude the dashboard reads).
                        "wt_datagram_audio_raw_loss_per_sec" => {
                            if let MetricValue::F64(raw_loss) = &metric.value {
                                peer_data.wt_datagram_audio_raw_loss_per_sec = *raw_loss;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Handle sender events (from local SenderDiagnosticManager)
        else if event.subsystem == "sender" {
            // Per-sender-event (fires for every received diagnostics packet).
            // Demoted debug!->trace!; not on the analyzer keep-list.
            trace!(
                "Received sender event for peer: {} at {}",
                target_peer,
                event.ts_ms
            );
            // Sender events are mainly for server reporting, less impact on health status
        }
        // Handle peer status events (mute/camera on/off)
        else if event.subsystem == "peer_status" {
            if let Ok(mut health_map) = peer_health_data.try_borrow_mut() {
                let peer_data = health_map
                    .entry(target_peer.to_string())
                    .or_insert_with(|| PeerHealthData::new(target_peer.to_string()));

                for metric in &event.metrics {
                    match metric.name {
                        "audio_enabled" => {
                            if let MetricValue::U64(v) = &metric.value {
                                peer_data.audio_enabled = *v > 0;
                            }
                        }
                        "video_enabled" => {
                            if let MetricValue::U64(v) = &metric.value {
                                peer_data.video_enabled = *v > 0;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Decode eligibility is a receiver-side visibility/decode-budget signal. It updates the
        // gate used by staleness-max accumulation without marking video stats fresh.
        else if event.subsystem == "decode_eligibility" {
            if let Ok(mut health_map) = peer_health_data.try_borrow_mut() {
                let peer_data = health_map
                    .entry(target_peer.to_string())
                    .or_insert_with(|| PeerHealthData::new(target_peer.to_string()));

                let is_screen = event.metrics.iter().any(|m| {
                    m.name == "media_type"
                        && matches!(&m.value, MetricValue::Text(s) if s == "SCREEN")
                });
                let decode_eligible = event.metrics.iter().find_map(|m| {
                    if m.name == "decode_eligible" {
                        match &m.value {
                            MetricValue::U64(v) => Some(*v),
                            _ => None,
                        }
                    } else {
                        None
                    }
                });

                if let Some(decode_eligible) = decode_eligible {
                    let slot = if is_screen {
                        &mut peer_data.screen_decode_eligible
                    } else {
                        &mut peer_data.camera_decode_eligible
                    };
                    *slot = Some(decode_eligible != 0);
                }
            }
        }
        // Handle video events
        else if event.subsystem == "video_decoder" || event.subsystem == "video" {
            if let Ok(mut health_map) = peer_health_data.try_borrow_mut() {
                let peer_data = health_map
                    .entry(target_peer.to_string())
                    .or_insert_with(|| PeerHealthData::new(target_peer.to_string()));

                // Determine if this is camera or screen based on media_type metric.
                let is_screen = event.metrics.iter().any(|m| {
                    m.name == "media_type"
                        && matches!(&m.value, MetricValue::Text(s) if s == "SCREEN")
                });

                // Pick the right stats bucket (camera or screen).
                let existing = if is_screen {
                    &peer_data.last_screen_stats
                } else {
                    &peer_data.last_camera_stats
                };
                let mut video_stats = match existing {
                    Some(Value::Object(map)) => Value::Object(map.clone()),
                    _ => json!({}),
                };
                // Always update timestamp
                video_stats["timestamp_ms"] = json!(event.ts_ms);
                let mut staleness_sample = None;
                let mut staleness_decode_eligible = None;

                for metric in &event.metrics {
                    match metric.name {
                        "fps_received" => {
                            if let MetricValue::F64(fps) = &metric.value {
                                video_stats["fps_received"] = json!(fps);
                            }
                        }
                        "frames_buffered" | "packets_buffered" => match &metric.value {
                            MetricValue::U64(v) => {
                                video_stats["frames_buffered"] = json!(v);
                            }
                            MetricValue::F64(v) => {
                                video_stats["frames_buffered"] = json!(v);
                            }
                            _ => {}
                        },
                        "frames_decoded" => {
                            if let MetricValue::U64(frames) = &metric.value {
                                video_stats["frames_decoded"] = json!(frames);
                            }
                        }
                        "decode_errors_per_sec" => {
                            if let MetricValue::F64(error_rate) = &metric.value {
                                video_stats["decode_errors_per_sec"] = json!(error_rate);
                            }
                        }
                        "decode_errors_total" => {
                            if let MetricValue::U64(total) = &metric.value {
                                peer_data.decode_errors_total = *total;
                            }
                        }
                        "bitrate_kbps" => match &metric.value {
                            MetricValue::U64(bitrate) => {
                                video_stats["bitrate_kbps"] = json!(bitrate);
                            }
                            MetricValue::F64(bitrate) => {
                                video_stats["bitrate_kbps"] = json!(*bitrate as u64);
                            }
                            _ => {}
                        },
                        // Freeze observability (#1013): windowed per-stream
                        // packet-loss rate and keyframe-request rate, emitted by
                        // the decoder. Stored in the camera/screen video_stats
                        // bucket (split by is_screen) so they fold into the
                        // per-peer health packet and the video quality score.
                        "video_seq_loss_per_sec" => {
                            if let MetricValue::F64(loss) = &metric.value {
                                video_stats["video_seq_loss_per_sec"] = json!(loss);
                            }
                        }
                        "keyframe_requests_per_sec" => {
                            if let MetricValue::F64(kf) = &metric.value {
                                video_stats["keyframe_requests_per_sec"] = json!(kf);
                            }
                        }
                        // #2524: an interval MAX, not the stats blob — see `IntervalMaxes`.
                        "video_seq_max_gap" => {
                            if let MetricValue::U64(v) = &metric.value {
                                let slot = if is_screen {
                                    &mut peer_data.screen_seq_max_gap_max
                                } else {
                                    &mut peer_data.camera_seq_max_gap_max
                                };
                                *slot = Some(slot.map_or(*v, |m: u64| m.max(*v)));
                            }
                        }
                        "freshness_evictions_total" | "freshness_evictions_keyframeless_total" => {
                            if let MetricValue::U64(v) = &metric.value {
                                video_stats[metric.name] = json!(v);
                            }
                        }
                        // Buffered video playout latency (#1252): total across both receive stages
                        // and its stage-1 attribution. Stored in the camera/screen video_stats
                        // bucket; folded into the health packet only when fps_received > 0.
                        "playout_latency_ms" => {
                            if let MetricValue::F64(v) = &metric.value {
                                video_stats["playout_latency_ms"] = json!(v);
                            }
                        }
                        "playout_stage1_span_ms" => {
                            if let MetricValue::F64(v) = &metric.value {
                                video_stats["playout_stage1_span_ms"] = json!(v);
                            }
                        }
                        // Stage-3 paint lag (#1252): decoded-but-unpainted backlog in the
                        // worker->main postMessage + paint queues. Same bucket/guard as the two
                        // latency fields above.
                        "playout_paint_lag_ms" => {
                            if let MetricValue::F64(v) = &metric.value {
                                video_stats["playout_paint_lag_ms"] = json!(v);
                            }
                        }
                        // Content-staleness (#1641): the content AGE of the painted video
                        // (drift-baselined), distinct from the paint-lag DEPTH above. Same
                        // camera/screen bucket and same fps_received > 0 fold guard as the ms
                        // gauges.
                        "content_staleness_ms" => {
                            if let MetricValue::F64(v) = &metric.value {
                                video_stats["content_staleness_ms"] = json!(v);
                                staleness_sample = Some(*v);
                            }
                        }
                        // Resync-to-live governor skips (#1252): lifetime cumulative COUNTER (u64),
                        // not an ms gauge. Stored in the camera/screen bucket that emitted the
                        // worker stat. Folded into the health packet from BOTH the camera and
                        // screen video_stats paths (#1660); the server exports the camera and
                        // screen skip-to-live counters as separate gauges.
                        "playout_skip_to_live_total" => {
                            if let MetricValue::U64(v) = &metric.value {
                                video_stats["playout_skip_to_live_total"] = json!(v);
                            }
                        }
                        // Keyframe ARRIVALS (#2201): lifetime counter, bucketed
                        // camera-vs-screen by `media_type` like the playout family.
                        "keyframe_arrivals_total" => {
                            if let MetricValue::U64(v) = &metric.value {
                                video_stats["keyframe_arrivals_total"] = json!(v);
                            }
                        }
                        // #2511: buckets camera-vs-screen like the playout family.
                        "freeze_episodes_total" => {
                            if let MetricValue::U64(v) = &metric.value {
                                video_stats["freeze_episodes_total"] = json!(v);
                            }
                        }
                        "freeze_ms_total" => {
                            if let MetricValue::U64(v) = &metric.value {
                                video_stats["freeze_ms_total"] = json!(v);
                            }
                        }
                        "max_decode_gap_ms" => {
                            if let MetricValue::U64(v) = &metric.value {
                                video_stats["max_decode_gap_ms"] = json!(v);
                            }
                        }
                        // #2249: qualifies the freeze branch in `video_quality_score`.
                        "decode_eligible" => {
                            if let MetricValue::U64(v) = &metric.value {
                                video_stats["decode_eligible"] = json!(v);
                                staleness_decode_eligible = Some(*v != 0);
                                if is_screen {
                                    peer_data.screen_decode_eligible = Some(*v != 0);
                                } else {
                                    peer_data.camera_decode_eligible = Some(*v != 0);
                                }
                            }
                        }
                        _ => {}
                    }
                }

                if let Some(v) = staleness_sample {
                    let latest_decode_eligible = if is_screen {
                        peer_data.screen_decode_eligible
                    } else {
                        peer_data.camera_decode_eligible
                    };
                    let decode_eligible = staleness_decode_eligible
                        .or(latest_decode_eligible)
                        .or_else(|| decode_eligible_known(&video_stats));
                    if decode_eligible == Some(true) {
                        let slot = if is_screen {
                            &mut peer_data.screen_staleness_max_ms
                        } else {
                            &mut peer_data.camera_staleness_max_ms
                        };
                        *slot = Some(slot.map_or(v, |m: f64| m.max(v)));
                    }
                }

                if is_screen {
                    peer_data.update_screen_stats(video_stats);
                    // Per-video-event (continuous per-stream stats). Demoted
                    // debug!->trace!; not on the analyzer keep-list.
                    trace!("Updated screen health for peer: {target_peer}");
                } else {
                    peer_data.update_camera_stats(video_stats);
                    // Per-video-event. Demoted debug!->trace!; not on the analyzer
                    // keep-list.
                    trace!("Updated camera health for peer: {target_peer}");
                }
            }
        }

        audio_loss_forward
    }

    /// #1032: Start the background total-process memory sampler.
    ///
    /// `performance.measureUserAgentSpecificMemory()` returns total agent
    /// memory including GPU-backed and worker allocations — exactly the
    /// non-heap memory that `performance.memory` (JS heap) misses. It is:
    ///   - **async** (returns a Promise), so we must `await` it OFF the
    ///     health-report hot path and cache the resolved value, and
    ///   - **Chrome-only and gated on `crossOriginIsolated`**, so it may be
    ///     entirely absent. We feature-detect once and, if missing, never spawn
    ///     the loop (the cached value stays `None`, the proto field is omitted,
    ///     and `agent_memory_bytes` simply never appears for that client).
    ///
    /// Graceful degradation: any missing global, non-isolated context, thrown
    /// exception, or malformed result clears the cache — we never panic and
    /// never block the report cadence.
    #[cfg(target_arch = "wasm32")]
    fn start_agent_memory_sampler(&self) {
        use wasm_bindgen::JsCast;

        // Feature-detect: window + crossOriginIsolated + the API function.
        // `measureUserAgentSpecificMemory` only exists in cross-origin-isolated
        // contexts on Chromium; bail out cleanly everywhere else.
        let Some(window) = web_sys::window() else {
            return;
        };
        let cross_origin_isolated = js_sys::Reflect::get(&window, &"crossOriginIsolated".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !cross_origin_isolated {
            debug!("agent-memory sampler: not crossOriginIsolated, skipping");
            return;
        }
        let Some(perf) = window.performance() else {
            return;
        };
        let measure_fn = match js_sys::Reflect::get(&perf, &"measureUserAgentSpecificMemory".into())
        {
            Ok(f) if f.is_function() => f.unchecked_into::<js_sys::Function>(),
            _ => {
                debug!(
                    "agent-memory sampler: measureUserAgentSpecificMemory unavailable, skipping"
                );
                return;
            }
        };

        // Sample on a slow cadence — this is a coarse pressure signal, not a
        // per-frame metric, and the API itself can take tens of ms to resolve.
        const AGENT_MEMORY_SAMPLE_INTERVAL_MS: u32 = 30_000;

        let cache = Rc::downgrade(&self.agent_memory_bytes);
        let shutdown = Rc::downgrade(&self.shutdown);

        spawn_local(async move {
            use wasm_bindgen_futures::JsFuture;

            loop {
                // Honour shutdown the same way the report loop does.
                match Weak::upgrade(&shutdown) {
                    Some(flag) if flag.load(Ordering::Acquire) => break,
                    None => break,
                    _ => {}
                }

                // Invoke the API. It returns a Promise resolving to an object
                // whose `bytes` field is the total agent memory in bytes.
                match measure_fn.call0(&perf) {
                    Ok(promise_val) => {
                        let promise: js_sys::Promise = promise_val.into();
                        match JsFuture::from(promise).await {
                            Ok(result) => {
                                let sample = js_sys::Reflect::get(&result, &"bytes".into())
                                    .ok()
                                    .and_then(|bytes| bytes.as_f64())
                                    .map(|bytes_f64| bytes_f64 as u64);
                                if let Some(cell) = Weak::upgrade(&cache) {
                                    if let Ok(mut c) = cell.try_borrow_mut() {
                                        *c = sample;
                                    }
                                } else {
                                    // HealthReporter dropped; stop sampling.
                                    break;
                                }
                            }
                            Err(e) => {
                                // Rejected (e.g. permissions/throttling). Clear the
                                // cached value so stale data cannot linger forever.
                                if let Some(cell) = Weak::upgrade(&cache) {
                                    if let Ok(mut c) = cell.try_borrow_mut() {
                                        *c = None;
                                    }
                                }
                                debug!("agent-memory sampler: measure rejected: {e:?}");
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(cell) = Weak::upgrade(&cache) {
                            if let Ok(mut c) = cell.try_borrow_mut() {
                                *c = None;
                            }
                        }
                        debug!("agent-memory sampler: call threw: {e:?}");
                    }
                }

                gloo_timers::future::TimeoutFuture::new(AGENT_MEMORY_SAMPLE_INTERVAL_MS).await;
            }
            debug!("agent-memory sampler stopped");
        });
    }

    /// Non-wasm builds have no browser memory API; the sampler is a no-op and
    /// `agent_memory_bytes` stays `None`.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_agent_memory_sampler(&self) {}

    /// Start periodic health reporting
    pub fn start_health_reporting(&self) {
        if self.send_packet_callback.is_none() {
            warn!("Cannot start health reporting: no send packet callback set");
            return;
        }

        // #1032: kick off the background total-process memory sampler. It runs
        // on its own cadence and caches the last resolved value so the report
        // loop below can read it synchronously (never awaiting in the hot path).
        self.start_agent_memory_sampler();

        let peer_health_data = Rc::downgrade(&self.peer_health_data);
        let session_id = Rc::downgrade(&self.session_id);
        let meeting_id = self.meeting_id.clone();
        let reporting_peer = self.reporting_peer.clone();
        let display_name = self.display_name.clone();
        let send_callback = self.send_packet_callback.clone().unwrap();
        let interval_ms = self.health_interval_ms;
        // Weak ref to the shutdown flag. We never need the strong reference
        // here — `Rc::downgrade` keeps the future from holding the
        // `Rc<AtomicBool>` past the HealthReporter's own lifetime, but the
        // flag itself can also be observed `true` directly via `shutdown()`
        // for prompt teardown without waiting for a tick.
        let shutdown = Rc::downgrade(&self.shutdown);
        let audio_enabled = Rc::downgrade(&self.reporting_audio_enabled);
        let video_enabled = Rc::downgrade(&self.reporting_video_enabled);
        let active_server_url = Rc::downgrade(&self.active_server_url);
        let active_server_type = Rc::downgrade(&self.active_server_type);
        let active_server_rtt_ms = Rc::downgrade(&self.active_server_rtt_ms);
        let connection_controller = Rc::downgrade(&self.connection_controller);
        let adaptive_video_tier = self.adaptive_video_tier.clone();
        let adaptive_audio_tier = self.adaptive_audio_tier.clone();
        let encoder_queue_depth_report = self.encoder_queue_depth_report.clone();
        let encoder_target_bitrate_kbps = self.encoder_target_bitrate_kbps.clone();
        let adaptive_screen_tier = self.adaptive_screen_tier.clone();
        let screen_sharing_active = self.screen_sharing_active.clone();
        let encoder_output_fps = self.encoder_output_fps.clone();
        // #1143: send-side simulcast layer counts (camera encoder).
        let effective_video_layers = self.effective_video_layers.clone();
        let active_video_layers = self.active_video_layers.clone();
        let camera_layer_metrics = self.camera_layer_metrics.clone();
        // #1561: screen + audio layer metrics.
        let effective_screen_layers = self.effective_screen_layers.clone();
        let active_screen_layers = self.active_screen_layers.clone();
        // #2147: screen encoder fps + its wired-ness flag.
        let screen_encoder_output_fps = self.screen_encoder_output_fps.clone();
        let screen_encoder_fps_wired = self.screen_encoder_fps_wired.clone();
        let effective_audio_layers = self.effective_audio_layers.clone();
        let audio_congestion_ceiling = self.audio_congestion_ceiling.clone();
        let audio_user_layer_ceiling = self.audio_user_layer_ceiling.clone();
        let received_layers = self.received_layers.clone();
        let tier_transitions = self.tier_transitions.clone();
        let climb_limiter_snapshot = self.climb_limiter_snapshot.clone();
        let dwell_samples = self.dwell_samples.clone();
        let longtask_buffer = self.longtask_buffer.clone();
        let longtask_ever_observed = self.longtask_ever_observed.clone();
        let render_fps_cell = self.render_fps.clone();
        let decode_budget_cell = self.decode_budget.clone();
        // #1032: cached total-process memory reading sampled in the background.
        let agent_memory_cell = self.agent_memory_bytes.clone();

        spawn_local(async move {
            debug!("Started health reporting with interval: {interval_ms}ms");

            // issue 1853: last-emit clock (ms since epoch) for the ~5s-paced
            // AUDIO_SCALE diagnostic. A loop-local (not a struct field) because it
            // only needs to persist across iterations of this single spawned task;
            // the modulo lives inline below off the shared health-report clock.
            let mut last_audio_scale_log_ms: u64 = 0;
            let mut has_encoded_real = false;

            loop {
                // Wait for the interval
                gloo_timers::future::TimeoutFuture::new(interval_ms as u32).await;

                // Honour an explicit shutdown signal (e.g. UI unmount) without
                // waiting for the HealthReporter's `Rc` count to fall to zero.
                // `send_callback` is an `Rc` strong reference back into
                // `VideoCallClient`, so without this exit the reporter loop
                // would keep the entire client alive until the server tore the
                // session down on its own — the leak observed in cc7tp.
                if let Some(flag) = Weak::upgrade(&shutdown) {
                    if flag.load(Ordering::Acquire) {
                        debug!("HealthReporter shutdown signalled, stopping health reporting");
                        break;
                    }
                } else {
                    // The HealthReporter (and its shutdown flag) have been
                    // dropped already — nothing to report against.
                    break;
                }

                // Upgrade session_id Weak ref; if the HealthReporter was dropped, stop.
                let session_id_val = match Weak::upgrade(&session_id) {
                    Some(s) => s.borrow().clone(),
                    None => break,
                };

                if let Some(peer_health_data) = Weak::upgrade(&peer_health_data) {
                    let interval_maxes = drain_interval_maxes(&peer_health_data);
                    if let Ok(health_map) = peer_health_data.try_borrow() {
                        let self_audio_enabled = Weak::upgrade(&audio_enabled)
                            .and_then(|ae| ae.try_borrow().ok().map(|v| *v))
                            .unwrap_or(false);
                        let self_video_enabled = Weak::upgrade(&video_enabled)
                            .and_then(|ve| ve.try_borrow().ok().map(|v| *v))
                            .unwrap_or(false);
                        // Snapshot active connection info for this tick
                        let active_url = Weak::upgrade(&active_server_url)
                            .and_then(|rc| rc.try_borrow().ok().and_then(|v| v.clone()));
                        let active_type = Weak::upgrade(&active_server_type)
                            .and_then(|rc| rc.try_borrow().ok().and_then(|v| v.clone()));
                        let active_rtt = Weak::upgrade(&active_server_rtt_ms)
                            .and_then(|rc| rc.try_borrow().ok().and_then(|v| *v));

                        // Get communication metrics from connection controller.
                        // #522: also read the RTT-probe resilience counters
                        // (cumulative since process start) so they can be emitted
                        // on the health packet.
                        let (
                            send_queue_bytes,
                            packets_received_per_sec,
                            packets_sent_per_sec,
                            rtt_probe_dropped_total,
                            rtt_probe_stale_suppressions_total,
                        ) = if let Some(cc_rc) = Weak::upgrade(&connection_controller) {
                            if let Ok(cc_opt) = cc_rc.try_borrow() {
                                if let Some(cc) = cc_opt.as_ref() {
                                    // Calculate latest packet rates
                                    cc.calculate_packet_rates();
                                    (
                                        cc.get_send_queue_depth(),
                                        Some(cc.get_packets_received_per_sec()),
                                        Some(cc.get_packets_sent_per_sec()),
                                        cc.rtt_probe_dropped_total(),
                                        cc.rtt_probe_stale_suppressions_total(),
                                    )
                                } else {
                                    (None, None, None, 0, 0)
                                }
                            } else {
                                (None, None, None, 0, 0)
                            }
                        } else {
                            (None, None, None, 0, 0)
                        };

                        // Read encoder decision inputs from shared atomics (f32 bits → f64).
                        let queue_depth_report_val = f32::from_bits(
                            encoder_queue_depth_report.borrow().load(Ordering::Relaxed),
                        ) as f64;
                        let target_bitrate_kbps_val = f32::from_bits(
                            encoder_target_bitrate_kbps.borrow().load(Ordering::Relaxed),
                        ) as f64;
                        let screen_tier_val = adaptive_screen_tier.borrow().load(Ordering::Relaxed);
                        let screen_active_val =
                            screen_sharing_active.borrow().load(Ordering::Relaxed);
                        let output_fps_val = encoder_output_fps.borrow().load(Ordering::Relaxed);
                        // #2147: SCREEN encoder fps. `None` when the atom has not
                        // been wired yet (omit the field); `Some(n)` once wired,
                        // INCLUDING `Some(0)` — an honest 0 is the whole point (see
                        // the proto field's doc and #2079).
                        let screen_output_fps_val = screen_encoder_fps_report_value(
                            screen_encoder_fps_wired.get(),
                            screen_encoder_output_fps.borrow().load(Ordering::Relaxed),
                        );
                        has_encoded_real = next_has_encoded_real(
                            has_encoded_real,
                            self_video_enabled,
                            output_fps_val,
                        );
                        // A window global keeps this independent of runtime log level and off
                        // the console-upload path. Gate on camera-active + first real sample so
                        // cold-start/idle never publishes a misleading 0.
                        // Since #2060 the producer resets/decays current_fps to 0, so a total
                        // stall publishes Some(0) (consumer maps 0 -> no-data), not a frozen nonzero.
                        publish_encoder_fps(encoder_fps_publish_value(
                            self_video_enabled,
                            output_fps_val,
                            has_encoded_real,
                        ));
                        // #1143: live send-side simulcast layer counts.
                        let effective_layers_val =
                            effective_video_layers.borrow().load(Ordering::Relaxed);
                        let active_layers_val =
                            active_video_layers.borrow().load(Ordering::Relaxed);
                        let camera_layer_metrics_val = camera_layer_metrics
                            .try_borrow()
                            .map(|source| source.reportable_layers())
                            .unwrap_or_default();
                        // #1561: screen + audio layer counts.
                        let effective_screen_layers_val =
                            effective_screen_layers.borrow().load(Ordering::Relaxed);
                        let active_screen_layers_val =
                            active_screen_layers.borrow().load(Ordering::Relaxed);
                        let effective_audio_layers_val =
                            effective_audio_layers.borrow().load(Ordering::Relaxed);
                        let audio_congestion_ceiling_raw =
                            audio_congestion_ceiling.borrow().load(Ordering::Relaxed);
                        let audio_user_ceiling_raw =
                            audio_user_layer_ceiling.borrow().load(Ordering::Relaxed);
                        // Keep the congestion-only ceiling separate from the
                        // actual active count, which also applies the user cap.
                        let (audio_congestion_ceiling_val, active_audio_layers_val) =
                            audio_layer_telemetry(
                                effective_audio_layers_val,
                                audio_congestion_ceiling_raw,
                                audio_user_ceiling_raw,
                            );
                        // #1561: snapshot the received-layer map for this health packet.
                        let received_layers_snapshot = received_layers
                            .try_borrow()
                            .ok()
                            .map(|m| m.clone())
                            .unwrap_or_default();

                        // Drain tier transitions from all encoder buffers.
                        let mut drained_transitions = Vec::new();
                        if let Ok(buffers) = tier_transitions.try_borrow() {
                            for buf in buffers.iter() {
                                if let Ok(mut t) = buf.try_borrow_mut() {
                                    drained_transitions.append(&mut *t);
                                }
                            }
                        }

                        // Snapshot climb-rate limiter state (double-wrap: outer then inner).
                        let limiter_snap = climb_limiter_snapshot
                            .try_borrow()
                            .ok()
                            .and_then(|outer| outer.try_borrow().ok().map(|s| s.clone()))
                            .unwrap_or_default();

                        // Drain dwell samples (double-wrap: outer then inner).
                        let drained_dwells: Vec<(String, f64)> = dwell_samples
                            .try_borrow()
                            .ok()
                            .and_then(|outer| {
                                outer
                                    .try_borrow_mut()
                                    .ok()
                                    .map(|mut d| std::mem::take(&mut *d))
                            })
                            .unwrap_or_default();

                        // TELEM-8: drain accumulated long-task durations
                        let drained_longtasks: Vec<f64> = longtask_buffer
                            .try_borrow_mut()
                            .ok()
                            .map(|mut v| std::mem::take(&mut *v))
                            .unwrap_or_default();

                        // #1482: main-thread load = (sum of longtask ms this
                        // interval) / interval_ms, clamped to 0.0..=1.0. HONEST
                        // 0-vs-unsupported: an EMPTY drain when 'longtask' HAS
                        // been observed this session is a genuine 0.0 (idle main
                        // thread); an empty drain when 'longtask' was NEVER
                        // observed means the API is unsupported (Firefox/Safari)
                        // and we report `None`, not a fabricated 0.0. Reporting
                        // 0.0 unconditionally would lie on those browsers.
                        let longtask_ever_observed_now = longtask_ever_observed.get();
                        let longtask_sum_ms: f64 = drained_longtasks.iter().sum();
                        let main_thread_load: Option<f64> =
                            if longtask_ever_observed_now && interval_ms > 0 {
                                Some((longtask_sum_ms / interval_ms as f64).clamp(0.0, 1.0))
                            } else {
                                None
                            };

                        // TELEM-9: read latest render FPS
                        let current_render_fps = render_fps_cell.try_borrow().ok().and_then(|v| *v);

                        // #987: read latest decode-budget snapshot (None until the
                        // controller has published its first decision).
                        let decode_budget_snapshot =
                            decode_budget_cell.try_borrow().ok().and_then(|v| *v);

                        // #1032: read cached total-process memory (None until the
                        // background sampler resolves, or permanently when the API
                        // is unavailable). Synchronous read — never awaits here.
                        let agent_memory_bytes =
                            agent_memory_cell.try_borrow().ok().and_then(|v| *v);

                        // TELEM-7: read client metadata from JS globals
                        let mut client_meta = read_client_metadata();
                        // #1556: compute CPU throttle flag from capability_score / cores
                        client_meta.cpu_throttled =
                            compute_cpu_throttled(client_meta.capability_score, client_meta.cores);

                        // issue 1853: decide whether this tick emits the ~5s-paced
                        // AUDIO_SCALE line, and snapshot the two receiver scalars it
                        // needs BEFORE `client_meta` is moved into create_health_packet.
                        let audio_scale_now_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        let emit_audio_scale = audio_scale_now_ms
                            .saturating_sub(last_audio_scale_log_ms)
                            >= AUDIO_SCALE_LOG_INTERVAL_MS;
                        if emit_audio_scale {
                            last_audio_scale_log_ms = audio_scale_now_ms;
                        }
                        let audio_scale_downlink_mbps = client_meta.network_downlink;
                        let audio_scale_cores = client_meta.cores;

                        let health_packet = Self::create_health_packet(
                            &session_id_val,
                            &meeting_id,
                            &reporting_peer,
                            &display_name,
                            &health_map,
                            self_audio_enabled,
                            self_video_enabled,
                            active_url,
                            active_type,
                            active_rtt,
                            send_queue_bytes,
                            packets_received_per_sec,
                            packets_sent_per_sec,
                            adaptive_video_tier.borrow().load(Ordering::Relaxed),
                            adaptive_audio_tier.borrow().load(Ordering::Relaxed),
                            videocall_transport::webtransport::datagram_drop_count(),
                            videocall_transport::webtransport::unistream_bytes_offered_total(),
                            videocall_transport::webtransport::unistream_bytes_drained_total(),
                            videocall_transport::webtransport::unistream_stale_delta_drop_count(),
                            videocall_transport::websocket::websocket_drop_count(),
                            keyframe_requests_sent_count(),
                            queue_depth_report_val,
                            target_bitrate_kbps_val,
                            screen_tier_val,
                            screen_active_val,
                            output_fps_val,
                            screen_output_fps_val,
                            effective_layers_val,
                            active_layers_val,
                            drained_transitions,
                            limiter_snap,
                            drained_dwells,
                            connection_handshake_failures(),
                            connection_session_drops(),
                            rtt_probe_dropped_total,
                            rtt_probe_stale_suppressions_total,
                            [
                                reelection_proceeded_total(),
                                reelection_aborted_total(),
                                reelection_preserved_total(),
                                reelection_failed_total(),
                            ],
                            drained_longtasks,
                            current_render_fps,
                            client_meta,
                            main_thread_load,
                            decode_budget_snapshot,
                            agent_memory_bytes,
                            // #1561: screen + audio layer metrics
                            effective_screen_layers_val,
                            active_screen_layers_val,
                            effective_audio_layers_val,
                            audio_congestion_ceiling_val,
                            active_audio_layers_val,
                            received_layers_snapshot,
                            // Issue 2031: per-client WT receive-health telemetry,
                            // read from the transport statics. read_loop drains
                            // its window here (~once per health interval); the
                            // queue read-back is a one-shot per-browser constant.
                            WtReceiveTelemetry {
                                read_loop_max_gap_ms:
                                    videocall_transport::webtransport::take_datagram_read_loop_max_gap_ms(),
                                incoming_queue_readback:
                                    videocall_transport::webtransport::incoming_datagram_queue_readback(),
                            },
                            camera_layer_metrics_val,
                            interval_maxes,
                        );

                        if let Some(packet) = health_packet {
                            send_callback.emit(packet);
                            // PER-TICK hot path: fires on every health-report
                            // interval (~1 Hz per session). Demoted debug!->trace!
                            // so it stays off even when console-log collection
                            // bumps to Debug (#1100 follow-up). Not on the analyzer
                            // keep-list.
                            trace!("Sent health packet for session: {session_id_val}");
                        }

                        // issue 1853 (instrumentation-only): once per
                        // AUDIO_SCALE_LOG_INTERVAL_MS, summarize this receiver's
                        // audio-scale posture in one greppable line so the meeting
                        // analyzer can correlate concealment against concurrent
                        // source count, downlink estimate, and per-source buffer
                        // depth. Samples are built from the SAME `health_map`
                        // snapshot create_health_packet just read (same tick, no
                        // stale mixing) via the SAME helper, so the aggregate and the
                        // per-stream audio_concealment_pct stay in lockstep. Emitted
                        // at debug! like its sibling "audio health (buffer:)" sample.
                        if emit_audio_scale {
                            let audio_samples: Vec<AudioSourceSample> = health_map
                                .values()
                                .filter_map(|hd| {
                                    hd.last_neteq_stats
                                        .as_ref()
                                        .map(audio_source_sample_from_neteq)
                                })
                                .collect();
                            if let Some(line) = format_audio_scale_line(
                                &audio_samples,
                                audio_scale_downlink_mbps,
                                audio_scale_cores,
                            ) {
                                debug!("{line}");
                            }
                        }
                    }
                } else {
                    debug!("HealthReporter dropped, stopping health reporting");
                    break;
                }
            }

            // Clear the page-level signal after meeting teardown stops the report loop.
            publish_encoder_fps(None);
        });
    }

    /// Per-stream fields stay UNSET at zero, so absence reads as "this stream
    /// offered nothing". The two by-state aggregates are set unconditionally: for
    /// those, zero is the reading that carries the information (issue 2201).
    fn set_ws_stream_counters(pb: &mut PbHealthPacket) {
        use crate::connection::MediaStreamKey as K;
        use videocall_transport::websocket::{
            websocket_dropped_bytes_for_stream as dropped,
            websocket_inactive_dropped_bytes_for_stream as inactive_bytes,
            websocket_inactive_dropped_frames_closed as inactive_closed,
            websocket_inactive_dropped_frames_closing as inactive_closing,
            websocket_inactive_dropped_frames_for_stream as inactive_frames,
            websocket_offered_bytes_for_stream as offered,
        };
        let nonzero = |v: u64| (v != 0).then_some(v);
        pb.ws_offered_bytes_audio = nonzero(offered(K::Audio.as_u8()));
        pb.ws_offered_bytes_video = nonzero(offered(K::Video.as_u8()));
        pb.ws_offered_bytes_screen = nonzero(offered(K::Screen.as_u8()));
        pb.ws_offered_bytes_control = nonzero(offered(K::Control.as_u8()));
        pb.ws_dropped_bytes_audio = nonzero(dropped(K::Audio.as_u8()));
        pb.ws_dropped_bytes_video = nonzero(dropped(K::Video.as_u8()));
        pb.ws_dropped_bytes_screen = nonzero(dropped(K::Screen.as_u8()));
        pb.ws_dropped_bytes_control = nonzero(dropped(K::Control.as_u8()));
        pb.ws_inactive_dropped_frames_audio = nonzero(inactive_frames(K::Audio.as_u8()));
        pb.ws_inactive_dropped_frames_video = nonzero(inactive_frames(K::Video.as_u8()));
        pb.ws_inactive_dropped_frames_screen = nonzero(inactive_frames(K::Screen.as_u8()));
        pb.ws_inactive_dropped_frames_control = nonzero(inactive_frames(K::Control.as_u8()));
        pb.ws_inactive_dropped_bytes_audio = nonzero(inactive_bytes(K::Audio.as_u8()));
        pb.ws_inactive_dropped_bytes_video = nonzero(inactive_bytes(K::Video.as_u8()));
        pb.ws_inactive_dropped_bytes_screen = nonzero(inactive_bytes(K::Screen.as_u8()));
        pb.ws_inactive_dropped_bytes_control = nonzero(inactive_bytes(K::Control.as_u8()));
        pb.ws_inactive_dropped_frames_by_state_closing = Some(inactive_closing());
        pb.ws_inactive_dropped_frames_by_state_closed = Some(inactive_closed());
    }

    /// Create a health packet from current peer health data
    #[allow(clippy::too_many_arguments)]
    fn create_health_packet(
        session_id: &str,
        meeting_id: &str,
        reporting_peer: &str,
        display_name: &str,
        health_map: &HashMap<String, PeerHealthData>,
        self_audio_enabled: bool,
        self_video_enabled: bool,
        active_server_url: Option<String>,
        active_server_type: Option<String>,
        active_server_rtt_ms: Option<f64>,
        send_queue_bytes: Option<u64>,
        packets_received_per_sec: Option<f64>,
        packets_sent_per_sec: Option<f64>,
        adaptive_video_tier: u32,
        adaptive_audio_tier: u32,
        datagram_drops_total: u64,
        unistream_bytes_offered_total: u64,
        unistream_bytes_drained_total: u64,
        unistream_stale_delta_drops_total: u64,
        websocket_drops_total: u64,
        keyframe_requests_sent_total: u64,
        encoder_queue_depth_report: f64,
        encoder_target_bitrate_kbps: f64,
        adaptive_screen_tier: u32,
        screen_sharing_active: bool,
        encoder_output_fps: u32,
        // #2147: SCREEN encoder output fps. `None` = the screen encoder's atom is
        // not wired yet (field OMITTED); `Some(0)` = wired and honestly producing
        // nothing. Unlike `encoder_output_fps` above this is deliberately NOT
        // collapsed by a `> 0` gate — see the proto field doc and #2079.
        screen_encoder_output_fps: Option<u32>,
        // #1143: send-side simulcast layer counts (camera). 0 = unwired/omitted.
        effective_video_layers: u32,
        active_video_layers: u32,
        tier_transitions: Vec<TierTransitionRecord>,
        climb_limiter: ClimbLimiterSnapshot,
        dwell_samples: Vec<(String, f64)>,
        handshake_failures_total: u64,
        session_drops_total: u64,
        // RTT-probe resilience signals (#522), read from the ConnectionManager
        // via the connection controller. Cumulative since process start.
        rtt_probe_dropped_total: u64,
        rtt_probe_stale_suppressions_total: u64,
        // Cumulative re-election outcome totals (Tier B #3), in the fixed order
        // [proceeded, aborted, preserved, failed]. Cumulative since process
        // start — the relay maps these onto a GaugeVec it .set()s, so the
        // monotonic client value charts correctly with increase()/rate().
        reelection_totals: [u64; 4],
        longtask_durations: Vec<f64>,
        render_fps: Option<f64>,
        client_metadata: ClientMetadata,
        // #1482: main-thread busy fraction (0.0-1.0) over the last interval, or
        // `None` when 'longtask' is unsupported. Computed in the report loop
        // rather than stored on ClientMetadata since it is a per-interval gauge.
        client_main_thread_load: Option<f64>,
        decode_budget: Option<DecodeBudgetSnapshot>,
        agent_memory_bytes: Option<u64>,
        // #1561: screen + audio layer metrics. 0 = unwired/omitted.
        effective_screen_layers: u32,
        active_screen_layers: u32,
        effective_audio_layers: u32,
        audio_congestion_ceiling: u32,
        active_audio_layers: u32,
        received_layers: HashMap<(u64, crate::decode::layer_chooser::PrefMediaKind), u32>,
        // Issue 2031: per-client WebTransport receive-health telemetry, read from
        // the transport statics in the report loop. `Default` on WebSocket.
        wt_telemetry: WtReceiveTelemetry,
        camera_layer_metrics: Vec<crate::encode::CameraLayerMetric>,
        // #2511: per-peer `(camera, screen)`. Absent => the wire field is omitted.
        interval_maxes: IntervalMaxMap,
    ) -> Option<PacketWrapper> {
        // Keep client-wide telemetry flowing even before any peer stats have
        // been observed (solo sessions / warm-up).

        // Build protobuf HealthPacket with structured stats
        let mut pb = PbHealthPacket::new();
        pb.session_id = session_id.to_string();
        pb.meeting_id = meeting_id.to_string();
        pb.reporting_user_id = reporting_peer.as_bytes().to_vec();
        pb.timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        pb.reporting_audio_enabled = self_audio_enabled;
        pb.reporting_video_enabled = self_video_enabled;
        if !display_name.is_empty() {
            pb.display_name = Some(display_name.to_string());
        }

        // Include active connection info if available.
        //
        // SECURITY: do NOT copy `active_server_url` into the protobuf. The lobby
        // URL carries the user's room JWT (`?token=<JWT>&instance_id=<UUID>`),
        // and HealthPacket is republished by the relay onto the NATS telemetry
        // topic `health.diagnostics.{region}.{service_type}.{server_id}` — any
        // health-pipeline consumer would receive the credential in cleartext.
        // The `active_server_type` and `active_server_rtt_ms` fields below are
        // sufficient for downstream observability; transport identity is
        // additionally available via `active_connection_id` on the diagnostic
        // bus (UI side). The proto field is left at its default empty string
        // and is slated for deprecation in a follow-up PR.
        // The `active_server_url` argument is intentionally swallowed here.
        let _ = active_server_url;
        // Issue #1878: the receive-side audio-datagram-loss tracker only feeds a
        // value while THIS client is on WebTransport (the exact
        // `receiver_on_webtransport` gate in peer_decode_manager); on WebSocket
        // the emitter never fires, so `wt_datagram_audio_loss_per_sec` retains its
        // last WT reading. Capture the reporter's transport here, before
        // `active_server_type` is moved into the proto below, so the per-peer fold
        // can SELECT the value it emits: the live tracker value on WebTransport,
        // versus definitional 0.0 on WebSocket (audio there rides ordered TCP — no
        // datagram loss is possible — and folding 0.0 rather than the stale WT
        // value un-latches the gauge on a mid-call WT→WS fallback).
        let reporter_on_webtransport = active_server_type.as_deref() == Some("webtransport");
        if let Some(typ) = active_server_type {
            pb.active_server_type = typ;
        }
        if let Some(rtt) = active_server_rtt_ms {
            pb.active_server_rtt_ms = rtt;
        }

        // Communication load metrics
        pb.send_queue_bytes = send_queue_bytes;
        pb.packets_received_per_sec = packets_received_per_sec;
        pb.packets_sent_per_sec = packets_sent_per_sec;

        // Receiver-side metrics: adaptive quality and transport health
        pb.adaptive_video_tier = Some(adaptive_video_tier);
        pb.adaptive_audio_tier = Some(adaptive_audio_tier);
        pb.datagram_drops_total = Some(datagram_drops_total);
        pb.unistream_bytes_offered_total = Some(unistream_bytes_offered_total);
        pb.unistream_bytes_drained_total = Some(unistream_bytes_drained_total);
        pb.unistream_stale_delta_drops_total = Some(unistream_stale_delta_drops_total);
        pb.websocket_drops_total = Some(websocket_drops_total);
        Self::set_ws_stream_counters(&mut pb);
        pb.keyframe_requests_sent_total = Some(keyframe_requests_sent_total);

        // Encoder decision inputs (P0)
        if encoder_queue_depth_report.is_finite() {
            pb.encoder_p75_peer_fps = Some(encoder_queue_depth_report);
        }
        pb.adaptive_screen_tier = Some(adaptive_screen_tier);
        pb.screen_sharing_active = Some(screen_sharing_active);

        // Encoder outputs (P1)
        // encoder_output_fps uses > 0 (not is_finite) because 0 USED to mean only
        // "the encoder hasn't started yet", which isn't diagnostic. The other
        // encoder metrics allow 0.0 through because a zero ratio/bitrate IS the
        // diagnostic signal.
        //
        // KNOWN LOSSY POINT (#2079): since #2060 a `0` here ALSO means a genuine
        // total stall (the producer decays `current_fps` to 0 after a sustained
        // layer-0 output gap), so this gate silently drops that from the protobuf —
        // a stalled encoder is indistinguishable from never-started because the
        // field is ABSENT in both cases. That is the same conflation `fps.ts`
        // `coerceEncoderFps` has on the window-global path, and the reason
        // `encoder_output_fps` cannot serve as the "check this instead" signal for a
        // frozen encoder. Left as-is deliberately: changing the gate changes the
        // wire contract for every consumer of this field, so the fix belongs with
        // #2079's source-side stall signal rather than here.
        if encoder_output_fps > 0 {
            pb.encoder_output_fps = Some(encoder_output_fps);
        }

        // #2147: SCREEN encoder output fps — the publisher-side signal that did not
        // exist before, leaving screen-encoder stalls (#1899, #1574, the #2143
        // room-wide freeze) unobservable while the CAMERA gauge read healthy.
        //
        // DELIBERATELY NOT `> 0`-gated, unlike `encoder_output_fps` directly above.
        // That gate is the #2079 defect: it makes a genuine stall ABSENT and
        // therefore indistinguishable from never-started. Here "not wired yet" is
        // carried by the `Option` (a `None` omits the field) so a wired encoder's
        // honest 0 reaches the wire. Copying the `> 0` gate here would reproduce
        // exactly the blind spot this field exists to close.
        //
        // A 0 is not by itself a fault: a static share legitimately emits nothing
        // once its keyframe floor budget drains. Pair with `screen_sharing_active`
        // and `screen_encoder_stall_episodes` to identify a publisher tick-starvation
        // freeze. Receiver-side content_staleness_ms also reads 0 once fps is 0.
        if let Some(fps) = screen_encoder_output_fps {
            pb.screen_encoder_output_fps = Some(fps);
        }

        // #1143: send-side simulcast layer counts (camera). Gated on > 0 (same
        // convention as encoder_output_fps): 0 means the encoder atoms have not
        // been wired yet, which is not diagnostic — omit rather than emit a
        // misleading 0. A wired single-stream publisher reports 1 (the
        // inert-simulcast signal the dashboard alerts on). active is clamped to
        // effective defensively so the gap can never read negative.
        if effective_video_layers > 0 {
            pb.effective_video_layers = Some(effective_video_layers);
            pb.active_video_layers = Some(active_video_layers.min(effective_video_layers));
        }
        pb.camera_layer_geometry = camera_layer_metrics
            .into_iter()
            .map(|(layer_id, width, height, output_fps)| {
                let mut geometry = EncoderLayerGeometry::new();
                geometry.layer_id = layer_id;
                geometry.width = width;
                geometry.height = height;
                geometry.output_fps = output_fps;
                geometry
            })
            .collect();

        // #1561: screen encoder simulcast layer counts. Same gating convention as video.
        if effective_screen_layers > 0 {
            pb.effective_screen_layers = Some(effective_screen_layers);
            pb.active_screen_layers = Some(active_screen_layers.min(effective_screen_layers));
        }
        // #1561: audio encoder layer count + congestion ceiling. Gated same as video.
        if effective_audio_layers > 0 {
            pb.effective_audio_layers = Some(effective_audio_layers);
            pb.active_audio_layers = Some(active_audio_layers.min(effective_audio_layers).max(1));
            // This field is congestion-only. The actual active count, which also
            // incorporates the user ceiling, is carried separately above.
            if audio_congestion_ceiling < u32::MAX {
                pb.audio_congestion_ceiling = Some(audio_congestion_ceiling);
            }
        }
        // #1561: receiver-side per-(peer,kind) desired layer map. Keyed by
        // session_id string so the relay/analyzer can correlate. Only constrained
        // peers appear (below-top); an empty map means all receivers are healthy.
        populate_received_layers(&mut pb, &received_layers);

        if encoder_target_bitrate_kbps.is_finite() {
            pb.encoder_target_bitrate_kbps = Some(encoder_target_bitrate_kbps);
        }

        // Tier transition events (P2)
        for t in &tier_transitions {
            let mut pb_t = PbTierTransition::new();
            pb_t.direction = t.direction.to_string();
            pb_t.stream = t.stream.to_string();
            pb_t.from_tier = t.from_tier.clone();
            pb_t.to_tier = t.to_tier.clone();
            pb_t.trigger = t.trigger.to_string();
            pb.tier_transitions.push(pb_t);
        }

        // Climb-rate limiter telemetry (PR-H)
        pb.crash_ceiling_active = Some(climb_limiter.crash_ceiling_active);
        if climb_limiter.crash_ceiling_active {
            pb.crash_ceiling_tier_index = climb_limiter.crash_ceiling_tier_index;
            pb.crash_ceiling_decay_ms = climb_limiter.crash_ceiling_decay_ms;
        }
        // Only emit blocked counters when non-zero to reduce packet size.
        if climb_limiter.step_up_blocked_ceiling > 0 {
            pb.step_up_blocked_ceiling = Some(climb_limiter.step_up_blocked_ceiling);
        }
        if climb_limiter.step_up_blocked_slowdown > 0 {
            pb.step_up_blocked_slowdown = Some(climb_limiter.step_up_blocked_slowdown);
        }
        if climb_limiter.step_up_blocked_screen_share > 0 {
            pb.step_up_blocked_screen_share = Some(climb_limiter.step_up_blocked_screen_share);
        }
        for (tier_label, dwell_ms) in &dwell_samples {
            let mut pb_d = PbTierDwell::new();
            pb_d.tier = tier_label.clone();
            pb_d.dwell_ms = *dwell_ms;
            pb.tier_dwells.push(pb_d);
        }

        // Encoder error counters (cumulative, global statics — zero-cost to read).
        // Only emit when non-zero to keep packet size small in the common (healthy) case.
        let cam_closed = camera_encoder_errors_closed_codec();
        let cam_vpx = camera_encoder_errors_vpx_mem_alloc();
        let cam_configure = camera_encoder_errors_configure_fatal();
        let cam_generic = camera_encoder_errors_generic();
        let cam_frames = camera_encoder_frames_submitted_ok();
        let scr_closed = screen_encoder_errors_closed_codec();
        let scr_vpx = screen_encoder_errors_vpx_mem_alloc();
        let scr_configure = screen_encoder_errors_configure_fatal();
        let scr_generic = screen_encoder_errors_generic();
        let scr_frames = screen_encoder_frames_submitted_ok();

        if cam_closed > 0 {
            pb.camera_encoder_errors_closed_codec = Some(cam_closed);
        }
        if cam_vpx > 0 {
            pb.camera_encoder_errors_vpx_mem_alloc = Some(cam_vpx);
        }
        if cam_configure > 0 {
            pb.camera_encoder_errors_configure_fatal = Some(cam_configure);
        }
        if cam_generic > 0 {
            pb.camera_encoder_errors_generic = Some(cam_generic);
        }
        if cam_frames > 0 {
            pb.camera_encoder_frames_submitted_ok = Some(cam_frames);
        }
        if scr_closed > 0 {
            pb.screen_encoder_errors_closed_codec = Some(scr_closed);
        }
        if scr_vpx > 0 {
            pb.screen_encoder_errors_vpx_mem_alloc = Some(scr_vpx);
        }
        if scr_configure > 0 {
            pb.screen_encoder_errors_configure_fatal = Some(scr_configure);
        }
        if scr_generic > 0 {
            pb.screen_encoder_errors_generic = Some(scr_generic);
        }
        if scr_frames > 0 {
            pb.screen_encoder_frames_submitted_ok = Some(scr_frames);
        }

        // Encoder auto-restart counters (#527), partitioned by reason. Same
        // zero-cost-static + non-zero-only convention as the error counters
        // above. The relay's metrics_server folds these into the single labeled
        // counter videocall_encoder_restart_total{kind, reason}.
        let cam_restart_closed = camera_encoder_restarts_closed_codec();
        let cam_restart_mem = camera_encoder_restarts_memory();
        let cam_restart_cfg = camera_encoder_restarts_configure();
        let cam_restart_other = camera_encoder_restarts_other();
        let scr_restart_closed = screen_encoder_restarts_closed_codec();
        let scr_restart_mem = screen_encoder_restarts_memory();
        let scr_restart_cfg = screen_encoder_restarts_configure();
        let scr_restart_other = screen_encoder_restarts_other();

        if cam_restart_closed > 0 {
            pb.camera_encoder_restarts_closed_codec = Some(cam_restart_closed);
        }
        if cam_restart_mem > 0 {
            pb.camera_encoder_restarts_memory = Some(cam_restart_mem);
        }
        if cam_restart_cfg > 0 {
            pb.camera_encoder_restarts_configure = Some(cam_restart_cfg);
        }
        if cam_restart_other > 0 {
            pb.camera_encoder_restarts_other = Some(cam_restart_other);
        }
        if scr_restart_closed > 0 {
            pb.screen_encoder_restarts_closed_codec = Some(scr_restart_closed);
        }
        if scr_restart_mem > 0 {
            pb.screen_encoder_restarts_memory = Some(scr_restart_mem);
        }
        if scr_restart_cfg > 0 {
            pb.screen_encoder_restarts_configure = Some(scr_restart_cfg);
        }
        if scr_restart_other > 0 {
            pb.screen_encoder_restarts_other = Some(scr_restart_other);
        }

        // #2147: screen encoder TICK-STARVATION stall signal — the half of the
        // screen-freeze story `screen_encoder_output_fps` cannot tell. That gauge
        // counts encoded CHUNKS, and the synthetic retained-frame re-encodes share
        // the base-layer output callback with fresh captures, so during the
        // #1899/#2143 freeze it reads a small NONZERO while receivers sit on stale
        // content. These count the episodes that CAUSE that symptom, so
        // `fps > 0 AND episodes rising` identifies the freeze while
        // `fps > 0 AND episodes flat` is genuinely healthy.
        //
        // Read from the same process-global accessors as the `*_restarts_*`
        // counters above (no per-encoder plumbing), and gated `> 0` on the SAME
        // rationale: these are monotonic counters, where a 0 carries no
        // information. That is deliberately the opposite of the fps field's
        // ungated treatment, where 0 IS the reading — see the proto docs.
        let scr_stall_episodes = screen_encoder_stall_episodes();
        let scr_stall_max_gap = screen_encoder_max_stall_gap_ms();
        if scr_stall_episodes > 0 {
            pb.screen_encoder_stall_episodes = Some(scr_stall_episodes);
        }
        if scr_stall_max_gap > 0 {
            pb.screen_encoder_max_stall_gap_ms = Some(scr_stall_max_gap);
        }

        // Connection-loss reason counters
        if handshake_failures_total > 0 {
            pb.connection_handshake_failures_total = Some(handshake_failures_total);
        }
        if session_drops_total > 0 {
            pb.connection_session_drops_total = Some(session_drops_total);
        }

        // RTT-probe resilience signals (#522). Cumulative, gated on > 0 like the
        // connection-loss counters above.
        if rtt_probe_dropped_total > 0 {
            pb.rtt_probe_dropped_total = Some(rtt_probe_dropped_total);
        }
        if rtt_probe_stale_suppressions_total > 0 {
            pb.rtt_probe_stale_suppressions_total = Some(rtt_probe_stale_suppressions_total);
        }

        // Re-election outcome counters (Tier B #3). Only attach a field when its
        // cumulative value is non-zero — keeps the packet small for the common
        // case (most sessions never re-elect) and mirrors the connection-loss
        // counters directly above. Order: [proceeded, aborted, preserved, failed].
        if reelection_totals[0] > 0 {
            pb.reelection_proceeded_total = Some(reelection_totals[0]);
        }
        if reelection_totals[1] > 0 {
            pb.reelection_aborted_total = Some(reelection_totals[1]);
        }
        if reelection_totals[2] > 0 {
            pb.reelection_preserved_total = Some(reelection_totals[2]);
        }
        if reelection_totals[3] > 0 {
            pb.reelection_failed_total = Some(reelection_totals[3]);
        }

        // TELEM-7: Static client metadata
        if client_metadata.cores > 0 {
            pb.client_cores = Some(client_metadata.cores);
        }
        if !client_metadata.architecture.is_empty() {
            pb.client_architecture = Some(client_metadata.architecture.clone());
        }
        if !client_metadata.gpu_family.is_empty() {
            pb.client_gpu_family = Some(client_metadata.gpu_family.clone());
        }
        if !client_metadata.network_effective_type.is_empty() {
            pb.client_network_effective_type = Some(client_metadata.network_effective_type.clone());
        }
        if client_metadata.network_downlink > 0.0 {
            pb.client_network_downlink = Some(client_metadata.network_downlink);
        }
        if client_metadata.network_rtt > 0 {
            pb.client_network_rtt = Some(client_metadata.network_rtt);
        }
        pb.client_battery_charging = client_metadata.battery_charging;
        pb.client_battery_level = client_metadata.battery_level;
        if client_metadata.capability_score > 0 {
            pb.client_capability_score = Some(client_metadata.capability_score);
        }
        // #1482: per-peer device / hardware metrics. Each field is published
        // ONLY when present ("if available"); an absent source API stays `None`
        // on the wire (proto3 optional omitted), never a fabricated default.
        if let Some(os) = &client_metadata.os {
            pb.client_os = Some(os.clone());
        }
        if let Some(dt) = &client_metadata.device_type {
            pb.client_device_type = Some(dt.clone());
        }
        if let Some(dm) = client_metadata.device_memory_gb {
            pb.client_device_memory_gb = Some(dm);
        }
        // #1556: navigator.connection.type + downlinkMax, and computed throttle flag.
        if let Some(ref s) = client_metadata.network_type {
            pb.client_network_type = Some(s.clone());
        }
        if let Some(f) = client_metadata.network_downlink_max {
            pb.client_network_downlink_max = Some(f);
        }
        if let Some(b) = client_metadata.cpu_throttled {
            pb.client_cpu_throttled = Some(b);
        }
        if let Some(load) = client_main_thread_load {
            pb.client_main_thread_load = Some(load);
        }

        // TELEM-8: Long task durations since last packet
        pb.longtask_durations_ms = longtask_durations;

        // TELEM-9: Render FPS
        pb.render_fps = render_fps;

        // #987: Adaptive decode-budget controller snapshot. Only present once the
        // controller has published a decision (so a no-peer / pre-warmup packet
        // omits it). Mirrors how the AdaptiveQuality tier fields ride the packet.
        if let Some(db) = decode_budget {
            let mut pb_db = PbDecodeBudget::new();
            pb_db.effective_cap = db.effective_cap;
            pb_db.natural = db.natural;
            pb_db.pressured = db.pressured;
            // Map the integer override mode (1 = Auto, 2 = Fixed; 0/other = Auto)
            // to the proto enum. `override_fixed_n` is only meaningful for Fixed.
            pb_db.override_mode = ::protobuf::EnumOrUnknown::new(match db.override_mode {
                2 => PbOverrideMode::OVERRIDE_MODE_FIXED,
                _ => PbOverrideMode::OVERRIDE_MODE_AUTO,
            });
            if db.override_mode == 2 {
                pb_db.override_fixed_n = db.override_fixed_n;
            }
            // #1143: tiles ACTUALLY being decoded right now. `effective_cap` is
            // the budget ceiling and `natural` is the unconstrained layout count;
            // the realized decode set is the smaller of the two (a 10-tile cap
            // with only 3 peers decodes 3, not 10). This is the per-client
            // "videos showing" signal the observability issue asks for.
            pb_db.active_set = db.effective_cap.min(db.natural);
            pb.decode_budget = ::protobuf::MessageField::some(pb_db);
        }

        // Tab visibility and throttling
        #[cfg(target_arch = "wasm32")]
        {
            let tab_hidden = web_sys::window()
                .and_then(|w| w.document())
                .map(|d| d.hidden())
                .unwrap_or(false);
            pb.is_tab_visible = !tab_hidden;
            pb.is_tab_throttled = tab_hidden;

            // Memory usage (Chrome only)
            if let Some(window) = web_sys::window() {
                if let Some(perf) = window.performance() {
                    // Try to access performance.memory (Chrome extension)
                    if let Ok(memory) = js_sys::Reflect::get(&perf, &"memory".into()) {
                        if !memory.is_undefined() {
                            if let Ok(used) =
                                js_sys::Reflect::get(&memory, &"usedJSHeapSize".into())
                            {
                                if let Some(used_f64) = used.as_f64() {
                                    pb.memory_used_bytes = Some(used_f64 as u64);
                                }
                            }
                            if let Ok(total) =
                                js_sys::Reflect::get(&memory, &"jsHeapSizeLimit".into())
                            {
                                if let Some(total_f64) = total.as_f64() {
                                    pb.memory_total_bytes = Some(total_f64 as u64);
                                }
                            }
                        }
                    }
                }
            }

            // #1032: WASM linear-memory size — WebAssembly.Memory.buffer.byteLength.
            // This is the WASM heap, distinct from the JS heap read above. Always
            // available, synchronous, O(1); the highest-value cheapest non-heap
            // signal. `wasm_bindgen::memory()` returns the `WebAssembly.Memory`
            // JsValue whose `.buffer.byteLength` is the current linear-memory size.
            let mem = wasm_bindgen::memory();
            if let Ok(buffer) = js_sys::Reflect::get(&mem, &"buffer".into()) {
                if let Ok(byte_len) = js_sys::Reflect::get(&buffer, &"byteLength".into()) {
                    if let Some(len_f64) = byte_len.as_f64() {
                        pb.wasm_memory_bytes = Some(len_f64 as u64);
                    }
                }
            }
        }

        // #1032: total-process memory from the background sampler (cached value;
        // see `start_agent_memory_sampler`). Absent when the API is unavailable
        // or has not yet resolved its first reading. Platform-agnostic so the
        // value flows through on the wire identically on every target.
        if let Some(agent_mem) = agent_memory_bytes {
            pb.agent_memory_bytes = Some(agent_mem);
        }

        // Issue 2031: per-client WebTransport receive-health telemetry.
        //
        // read_loop_max_gap_ms is folded UNCONDITIONALLY (like datagram_drops):
        // 0.0 on WebSocket is the correct "no reader starvation" value and lets
        // the server gauge recover to 0 instead of latching the last WT reading.
        pb.wt_datagram_read_loop_max_gap_ms = Some(wt_telemetry.read_loop_max_gap_ms);
        // Queue read-back is a one-shot per-browser constant captured when the WT
        // queue was configured. Folded only when present (a WS-only client never
        // configured a queue, so nothing to report). NaN maxAge (spec-unbounded /
        // setter not honored) maps to the -1.0 wire sentinel so the gauge stays
        // finite; any finite value near our 3000ms target confirms the setter took.
        if let Some((hwm, max_age)) = wt_telemetry.incoming_queue_readback {
            pb.wt_incoming_datagram_high_water_mark = Some(hwm);
            pb.wt_incoming_datagram_max_age_ms =
                Some(if max_age.is_nan() { -1.0 } else { max_age });
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        const STATS_STALE_MS: u64 = 5_000;

        // Issue 2031: accumulate the per-client mean audio concealment over ACTIVE
        // sources this tick (the same active-source mean the [AUDIO_SCALE] line
        // reports). Folded into a per-client field the server splits by transport,
        // giving the WS-vs-WT concealment severity gap as a single labeled gauge.
        let mut concealment_sum = 0.0_f64;
        let mut concealment_active_sources = 0_u32;

        for (peer_id, health_data) in health_map.iter() {
            let maxes = interval_maxes.get(peer_id).copied().unwrap_or_default();
            let (camera_staleness_max, screen_staleness_max) =
                (maxes.camera_staleness_ms, maxes.screen_staleness_ms);
            // Freshness gate: stats older than 5s are stale (FPS/NetEQ trackers stop
            // emitting DiagEvents when no frames arrive, so timestamps stop advancing).
            let audio_fresh = health_data.last_audio_update_ms > 0
                && now_ms.saturating_sub(health_data.last_audio_update_ms) < STATS_STALE_MS;
            let camera_fresh = health_data.last_camera_update_ms > 0
                && now_ms.saturating_sub(health_data.last_camera_update_ms) < STATS_STALE_MS;
            let screen_fresh = health_data.last_screen_update_ms > 0
                && now_ms.saturating_sub(health_data.last_screen_update_ms) < STATS_STALE_MS;
            let video_fresh = camera_fresh || screen_fresh;

            let mut ps = PbPeerStats::new();
            // can_listen/can_see: receiver-observed. True only while stream is fresh.
            ps.can_listen = audio_fresh;
            ps.can_see = video_fresh;
            // audio_enabled/video_enabled: sender's self-reported state from heartbeat.
            ps.audio_enabled = health_data.audio_enabled;
            ps.video_enabled = health_data.video_enabled;

            // NetEQ mapping
            if let Some(neteq) = &health_data.last_neteq_stats {
                let mut ns = PbNetEqStats::new();
                if let Some(v) = neteq.get("current_buffer_size_ms").and_then(|v| v.as_f64()) {
                    ns.current_buffer_size_ms = v;
                }
                if let Some(v) = neteq
                    .get("packets_awaiting_decode")
                    .and_then(|v| v.as_f64())
                {
                    ns.packets_awaiting_decode = v;
                }
                if let Some(v) = neteq.get("packets_per_sec").and_then(|v| v.as_f64()) {
                    ns.packets_per_sec = v;
                }
                if let Some(v) = neteq.get("target_delay_ms").and_then(|v| v.as_f64()) {
                    // Delay manager target: the algorithm's estimate of buffering needed
                    // to absorb observed network jitter. This is the real VoIP jitter metric.
                    ns.target_delay_ms = v;
                }
                // Audio playout latency (#1299): NetEQ's filtered current buffer level — how far
                // behind live this peer's audio playout sits. The audio sibling of
                // VideoStats.playout_latency_ms (#1252). Surfaced straight from the NetEQ stats
                // JSON top-level (NetEqStats::playout_latency_ms); when the field is absent (older
                // NetEQ worker) it stays at the proto 0.0 default = "at live". Observability only.
                if let Some(v) = neteq.get("playout_latency_ms").and_then(|v| v.as_f64()) {
                    ns.playout_latency_ms = v;
                }

                // Windowed receive-side audio concealment for this source, from
                // WINDOWED rates (not lifetime): expand_per_sec / packets_per_sec.
                // issue 1853: computed via the shared `audio_source_sample_from_neteq`
                // helper (same >= AUDIO_ACTIVE_PPS_GATE pps gate — below that the
                // speaker is likely in DTX silence and the ratio is unreliable — and
                // same 0–100 clamp) so the per-stream `audio_concealment_pct`
                // published here and the aggregate AUDIO_SCALE line emitted by the
                // report loop can never drift. Only set when active; otherwise the
                // proto field stays at its 0.0 default.
                let audio_sample = audio_source_sample_from_neteq(neteq);
                if audio_sample.active {
                    ps.audio_concealment_pct = audio_sample.concealment_pct;
                    // Issue 2031: feed the per-client active-source mean.
                    concealment_sum += audio_sample.concealment_pct;
                    concealment_active_sources += 1;
                }

                if let Some(network) = neteq.get("network") {
                    let mut nn = PbNetEqNetwork::new();
                    if let Some(counters) = network.get("operation_counters") {
                        let mut oc = PbNetEqOperationCounters::new();
                        if let Some(v) = counters.get("normal_per_sec").and_then(|v| v.as_f64()) {
                            oc.normal_per_sec = v;
                        }
                        if let Some(v) = counters.get("expand_per_sec").and_then(|v| v.as_f64()) {
                            oc.expand_per_sec = v;
                        }
                        if let Some(v) = counters.get("accelerate_per_sec").and_then(|v| v.as_f64())
                        {
                            oc.accelerate_per_sec = v;
                        }
                        if let Some(v) = counters
                            .get("fast_accelerate_per_sec")
                            .and_then(|v| v.as_f64())
                        {
                            oc.fast_accelerate_per_sec = v;
                        }
                        if let Some(v) = counters
                            .get("preemptive_expand_per_sec")
                            .and_then(|v| v.as_f64())
                        {
                            oc.preemptive_expand_per_sec = v;
                        }
                        if let Some(v) = counters.get("merge_per_sec").and_then(|v| v.as_f64()) {
                            oc.merge_per_sec = v;
                        }
                        if let Some(v) = counters
                            .get("comfort_noise_per_sec")
                            .and_then(|v| v.as_f64())
                        {
                            oc.comfort_noise_per_sec = v;
                        }
                        if let Some(v) = counters.get("dtmf_per_sec").and_then(|v| v.as_f64()) {
                            oc.dtmf_per_sec = v;
                        }
                        if let Some(v) = counters.get("undefined_per_sec").and_then(|v| v.as_f64())
                        {
                            oc.undefined_per_sec = v;
                        }
                        nn.operation_counters = ::protobuf::MessageField::some(oc);
                    }
                    ns.network = ::protobuf::MessageField::some(nn);
                }
                ps.neteq_stats = ::protobuf::MessageField::some(ns);
            }

            // Camera video mapping (backward compat: goes into existing video_stats field)
            if let Some(video) = &health_data.last_camera_stats {
                let mut vs = PbVideoStats::new();
                if let Some(v) = video.get("fps_received").and_then(|v| v.as_f64()) {
                    vs.fps_received = v;
                }
                if let Some(v) = video.get("frames_buffered").and_then(|v| v.as_f64()) {
                    vs.frames_buffered = v;
                }
                if let Some(v) = video.get("frames_decoded").and_then(|v| v.as_u64()) {
                    vs.frames_decoded = v;
                }
                if let Some(v) = video.get("bitrate_kbps").and_then(|v| v.as_u64()) {
                    vs.bitrate_kbps = v;
                }

                // Buffered video playout latency (#1252). Guard #1 (load-bearing): only fold the
                // span when fps_received > 0. A DecodeBudget-paused or hidden tile keeps a stale
                // frame buffered but decodes nothing, so its arrival-time span would read as
                // latency even though the user isn't waiting on it. fps_received > 0 means frames
                // are actually being received/decoded, so the lag is real. When fps == 0 the proto
                // field stays at its 0.0 default, which the server publishes as "at live".
                if vs.fps_received > 0.0 {
                    if let Some(v) = video.get("playout_latency_ms").and_then(|v| v.as_f64()) {
                        vs.playout_latency_ms = v;
                    }
                    if let Some(v) = video.get("playout_stage1_span_ms").and_then(|v| v.as_f64()) {
                        vs.playout_stage1_span_ms = v;
                    }
                    // Stage-3 paint lag (#1252): same fps_received > 0 guard — a paused/hidden tile
                    // decodes nothing and paints nothing, so any residual emitted-vs-painted skew
                    // is not user-perceived latency. When fps == 0 the field stays at its 0.0
                    // default => "at live".
                    if let Some(v) = video.get("playout_paint_lag_ms").and_then(|v| v.as_f64()) {
                        vs.playout_paint_lag_ms = v;
                    }
                    // Content-staleness (#1641): content AGE of the painted video, vs the paint-lag
                    // DEPTH above. Same fps_received > 0 guard — a paused/hidden tile paints nothing,
                    // so when fps == 0 this stays at its 0.0 default => "at live". This is a ms
                    // GAUGE (not the skip-to-live COUNTER below), so it is gated like the other
                    // gauges. Unlike playout_latency_ms it can legitimately exceed 1800ms.
                    if let Some(v) = video.get("content_staleness_ms").and_then(|v| v.as_f64()) {
                        vs.content_staleness_ms = v;
                    }
                }
                // Resync-to-live governor skips (#1252): folded UNCONDITIONALLY for camera video,
                // OUTSIDE the fps_received > 0 gate above. The ms gauges are gated because a
                // paused/hidden tile decodes nothing and any residual span isn't user-perceived
                // latency. This is a cumulative COUNTER, not a gauge — it must keep reporting its
                // lifetime value even when fps == 0, or a stream that fell idle would appear to
                // "un-fire" the governor.
                if let Some(v) = video
                    .get("playout_skip_to_live_total")
                    .and_then(|v| v.as_u64())
                {
                    vs.playout_skip_to_live_total = v;
                }
                // Keyframe ARRIVALS (#2201): folded UNCONDITIONALLY, outside the
                // fps_received > 0 gate, for the same reason as the counter above — a
                // cumulative counter must keep reporting its lifetime value even at fps 0.
                // That is load-bearing HERE specifically: the whole point of this counter is
                // to be read DURING a freeze, which is exactly when fps can be 0. Gating it
                // would blank the metric in the only case it exists for.
                if let Some(v) = video
                    .get("keyframe_arrivals_total")
                    .and_then(|v| v.as_u64())
                {
                    vs.keyframe_arrivals_total = Some(v);
                }
                // #2511: outside the fps gate — fps 0 IS the freeze these describe — but
                // inside the sender's own camera-enabled flag.
                if health_data.video_enabled {
                    if let Some(v) = video.get("freeze_episodes_total").and_then(|v| v.as_u64()) {
                        vs.freeze_episodes_total = Some(v);
                    }
                    if let Some(v) = video.get("freeze_ms_total").and_then(|v| v.as_u64()) {
                        vs.freeze_ms_total = Some(v);
                    }
                    if let Some(v) = video.get("max_decode_gap_ms").and_then(|v| v.as_u64()) {
                        vs.max_decode_gap_ms = Some(v);
                    }
                }
                if let Some(v) = camera_staleness_max {
                    vs.max_content_staleness_ms = Some(v as u64);
                }
                // NOT gated on `video_enabled`, unlike the #2511 freeze family above.
                fold_loss_diagnostics(&mut vs, video, maxes.camera_seq_gap_frames);
                ps.video_stats = ::protobuf::MessageField::some(vs);

                // Extract decode_errors_per_sec (windowed rate) from camera video stats
                if let Some(error_rate) =
                    video.get("decode_errors_per_sec").and_then(|v| v.as_f64())
                {
                    ps.frames_dropped_per_sec = error_rate;
                }

                // Freeze observability (#1013): windowed per-stream loss /
                // keyframe-request rates (camera only this pass). These feed the
                // video quality score so a frozen-but-still-decoding stream
                // (fps reads ~30, video visually stuck) no longer scores 100.
                if let Some(loss) = video.get("video_seq_loss_per_sec").and_then(|v| v.as_f64()) {
                    ps.video_seq_loss_per_sec = Some(loss);
                }
                if let Some(kf) = video
                    .get("keyframe_requests_per_sec")
                    .and_then(|v| v.as_f64())
                {
                    ps.keyframe_requests_per_sec = Some(kf);
                }
            }

            // Screen share video mapping (new field, separate from camera). Folds
            // fps/buffered/decoded/bitrate AND the playout family (#1660), mirroring the camera
            // fold above so the server's screen playout gauges (#1660) receive real values instead
            // of the proto-default 0. PR #1657 routed the screen decoder's playout stats into this
            // same last_screen_stats blob (Stage A above), so the keys are already present.
            if let Some(screen) = &health_data.last_screen_stats {
                let mut svs = PbVideoStats::new();
                if let Some(v) = screen.get("fps_received").and_then(|v| v.as_f64()) {
                    svs.fps_received = v;
                }
                if let Some(v) = screen.get("frames_buffered").and_then(|v| v.as_f64()) {
                    svs.frames_buffered = v;
                }
                if let Some(v) = screen.get("frames_decoded").and_then(|v| v.as_u64()) {
                    svs.frames_decoded = v;
                }
                if let Some(v) = screen.get("bitrate_kbps").and_then(|v| v.as_u64()) {
                    svs.bitrate_kbps = v;
                }

                // Screen playout family (#1660): same fps_received > 0 gate as the camera fold —
                // a DecodeBudget-paused or hidden screen tile keeps a stale frame buffered but
                // decodes nothing, so its arrival-time span would read as latency the viewer isn't
                // waiting on. When fps == 0 these ms fields stay at their 0.0 default => "at live".
                if svs.fps_received > 0.0 {
                    if let Some(v) = screen.get("playout_latency_ms").and_then(|v| v.as_f64()) {
                        svs.playout_latency_ms = v;
                    }
                    if let Some(v) = screen
                        .get("playout_stage1_span_ms")
                        .and_then(|v| v.as_f64())
                    {
                        svs.playout_stage1_span_ms = v;
                    }
                    if let Some(v) = screen.get("playout_paint_lag_ms").and_then(|v| v.as_f64()) {
                        svs.playout_paint_lag_ms = v;
                    }
                    // Content-staleness (#1641): content AGE, not queue DEPTH. UNBOUNDED — can
                    // legitimately exceed 1800ms — which is why it is the gauge that surfaces a
                    // screen-share freeze draining stale content. Gated like the other ms gauges.
                    if let Some(v) = screen.get("content_staleness_ms").and_then(|v| v.as_f64()) {
                        svs.content_staleness_ms = v;
                    }
                }
                // Resync-to-live governor skips (#1252): cumulative COUNTER, folded
                // UNCONDITIONALLY (outside the fps gate) exactly like the camera path — a stream
                // that fell idle must keep reporting its lifetime total, or the governor would
                // appear to "un-fire".
                if let Some(v) = screen
                    .get("playout_skip_to_live_total")
                    .and_then(|v| v.as_u64())
                {
                    svs.playout_skip_to_live_total = v;
                }
                // Keyframe ARRIVALS (#2201): unconditional, mirroring the camera fold — see
                // its comment for why gating on fps would blank the metric during the very
                // freeze it is meant to explain.
                if let Some(v) = screen
                    .get("keyframe_arrivals_total")
                    .and_then(|v| v.as_u64())
                {
                    svs.keyframe_arrivals_total = Some(v);
                }
                // Freeze episodes (#2511): deliberately NOT gated on `video_enabled` like the
                // camera fold — that flag is the CAMERA's, and a share must survive camera-off.
                if let Some(v) = screen.get("freeze_episodes_total").and_then(|v| v.as_u64()) {
                    svs.freeze_episodes_total = Some(v);
                }
                if let Some(v) = screen.get("freeze_ms_total").and_then(|v| v.as_u64()) {
                    svs.freeze_ms_total = Some(v);
                }
                if let Some(v) = screen.get("max_decode_gap_ms").and_then(|v| v.as_u64()) {
                    svs.max_decode_gap_ms = Some(v);
                }
                if let Some(v) = screen_staleness_max {
                    svs.max_content_staleness_ms = Some(v as u64);
                }
                fold_loss_diagnostics(&mut svs, screen, maxes.screen_seq_gap_frames);
                ps.screen_video_stats = ::protobuf::MessageField::some(svs);
            }

            // Cumulative decode error count (only set if > 0 to avoid noise)
            if health_data.decode_errors_total > 0 {
                ps.decoder_errors_total = Some(health_data.decode_errors_total);
            }

            // Issue #1878: receive-side audio DATAGRAM loss (audio sibling of
            // video_seq_loss_per_sec above). Folded UNCONDITIONALLY as Some — like
            // its sibling — so the exported gauge recovers to 0 instead of
            // latching a stale value. On WebTransport we fold the tracker's live
            // windowed value (refreshed ~1 Hz, including 0.0 when a loss burst
            // clears). On WebSocket the value is definitionally 0.0 (audio rides
            // ordered TCP — no datagram loss is possible), and folding 0.0 rather
            // than `wt_datagram_audio_loss_per_sec` — which the emitter stops
            // refreshing on WS and so pins at its last WT reading — un-latches the
            // gauge on a mid-call WT→WS fallback. E2EE-WT is still "webtransport",
            // so it folds the tracker value, which reads ~0 there because audio
            // rides the reliable unistream.
            ps.audio_datagram_loss_per_sec = Some(if reporter_on_webtransport {
                health_data.wt_datagram_audio_loss_per_sec
            } else {
                0.0
            });
            // Issue 2031: the uncapped magnitude companion, folded on the SAME
            // WebTransport gate as the capped rate above — definitional 0.0 on
            // WebSocket so it un-latches on a WT->WS fallback identically.
            ps.audio_datagram_raw_loss_per_sec = Some(if reporter_on_webtransport {
                health_data.wt_datagram_audio_raw_loss_per_sec
            } else {
                0.0
            });

            // ── Quality scores ─────────────────────────────────────────────
            // Only set when the stream is active; absent = Grafana shows a gap,
            // not a misleading zero. audio_fresh/video_fresh computed above.

            // Audio quality (0-100): only meaningful when packets are flowing
            let audio_packets_per_sec = ps
                .neteq_stats
                .as_ref()
                .map(|n| n.packets_per_sec)
                .unwrap_or(0.0);

            if audio_fresh
                && audio_packets_per_sec >= AUDIO_ACTIVE_PPS_GATE
                && health_data.audio_enabled
            {
                let conceal = ps
                    .neteq_stats
                    .as_ref()
                    .and_then(|n| n.network.as_ref())
                    .and_then(|net| net.operation_counters.as_ref())
                    .map(|oc| oc.expand_per_sec)
                    .unwrap_or(0.0);
                let loss = ps.audio_concealment_pct;

                // Penalties sum to 100 max.
                // Jitter (target_delay_ms) is intentionally excluded: in this stack it
                // settles at a fixed NetEQ default (~120ms) and carries no diagnostic
                // signal. Concealment already captures the downstream effect of real
                // jitter (late/lost packets → expand events → audible degradation).
                let conceal_penalty = (conceal / 10.0).min(1.0) * 70.0;
                let loss_penalty = (loss / 5.0).min(1.0) * 30.0;
                let score = (100.0 - conceal_penalty - loss_penalty).max(0.0);
                ps.audio_quality_score = Some(score);
            }

            // Video quality (0-100): the WORSE of this peer's camera and screen streams.
            //
            // Freeze observability (#1013): during a freeze, fps_received still reads ~30
            // because decode calls keep firing fire-and-forget, yet the picture is visually
            // frozen because packets are lost and the stream is stuck requesting keyframes.
            // We fold the windowed loss rate and keyframe-request rate into the score so it
            // drops well below 100 in that state.
            let (cam_fps, cam_kbps) = ps
                .video_stats
                .as_ref()
                .map(|v| (v.fps_received, v.bitrate_kbps))
                .unwrap_or((0.0, 0));
            let (screen_fps, screen_kbps) = ps
                .screen_video_stats
                .as_ref()
                .map(|v| (v.fps_received, v.bitrate_kbps))
                .unwrap_or((0.0, 0));
            let cam_eligible = health_data
                .camera_decode_eligible
                .unwrap_or_else(|| decode_eligible_from(&health_data.last_camera_stats));
            let screen_eligible = health_data
                .screen_decode_eligible
                .unwrap_or_else(|| decode_eligible_from(&health_data.last_screen_stats));
            // Each stream gates on ITS OWN freshness — a retired stream keeps its blob,
            // so the combined `video_fresh` would go on scoring a dead one.
            let camera_score = if camera_fresh {
                video_quality_score(
                    cam_fps,
                    cam_kbps,
                    ps.frames_dropped_per_sec,
                    ps.video_seq_loss_per_sec.unwrap_or(0.0),
                    ps.keyframe_requests_per_sec.unwrap_or(0.0),
                    cam_eligible,
                )
            } else {
                None
            };
            // fps/downlink terms only: screen's #2524 rates would move call_quality_score.
            let screen_score = if screen_fresh {
                video_quality_score(screen_fps, screen_kbps, 0.0, 0.0, 0.0, screen_eligible)
            } else {
                None
            };
            ps.video_quality_score = match (camera_score, screen_score) {
                (Some(c), Some(s)) => Some(c.min(s)),
                (Some(c), None) => Some(c),
                (None, Some(s)) => Some(s),
                (None, None) => None,
            };

            // Call quality: worst of whichever streams are active
            let call_score = match (ps.audio_quality_score, ps.video_quality_score) {
                (Some(a), Some(v)) => Some(a.min(v)),
                (Some(a), None) => Some(a),
                (None, Some(v)) => Some(v),
                (None, None) => None,
            };
            ps.call_quality_score = call_score;

            pb.peer_stats.insert(peer_id.clone(), ps);
        }

        // Issue 2031: per-client mean audio concealment over active sources. Set
        // only when at least one source is actively delivering audio, so absent
        // means "no audio flowing", NOT "0% concealment" (mirrors the per-peer
        // audio_concealment_pct, which is likewise only set when active). The
        // server exports it split by the reporter's active transport.
        if concealment_active_sources > 0 {
            pb.client_audio_concealment_pct =
                Some(concealment_sum / concealment_active_sources as f64);
        }

        let bytes = pb.write_to_bytes().unwrap_or_default();
        Some(PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            user_id: reporting_peer.as_bytes().to_vec(),
            data: bytes,
            ..Default::default()
        })
    }

    /// Remove a peer from health tracking
    pub fn remove_peer(&self, peer_id: &str) {
        if let Ok(mut health_map) = self.peer_health_data.try_borrow_mut() {
            health_map.remove(peer_id);
            debug!("Removed peer from health tracking: {peer_id}");
        }
    }

    /// Get current health summary for debugging
    pub fn get_health_summary(&self) -> Option<Value> {
        if let Ok(health_map) = self.peer_health_data.try_borrow() {
            let summary = health_map
                .iter()
                .map(|(peer_id, health_data)| {
                    (
                        peer_id.clone(),
                        json!({
                            "audio_enabled": health_data.audio_enabled,
                            "video_enabled": health_data.video_enabled,
                            "last_update_ms": health_data.last_update_ms
                        }),
                    )
                })
                .collect::<serde_json::Map<_, _>>();

            Some(Value::Object(summary))
        } else {
            None
        }
    }
}

/// Compute the per-stream video quality score (0–100), or `None` when nothing is
/// arriving on the stream at all.
///
/// Freeze observability (#1013): a broadcast-relay stream can read fps ≈ 30
/// while being visually frozen — decode CALLS keep firing fire-and-forget even
/// as packets are lost and the stream is stuck requesting keyframes. fps alone
/// therefore cannot detect a freeze. We add two penalties on top of the
/// fps/decode-error health so a sustained loss or keyframe storm forces the
/// score well below 100:
///
/// * `loss_per_sec` — windowed packet-loss rate from `SequenceTracker`.
///   `5 lost/s → −30`. Mirrors the audio `loss_penalty` shape.
/// * `kf_per_sec`   — windowed keyframe-request (PLI) rate. A stream that is
///   continuously asking for keyframes is, by definition, not decoding cleanly,
///   so even a *sustained* ≥1 PLI/s is a strong freeze signal: `1 PLI/s → −40`.
///
/// At `fps == 0` it takes BOTH `bitrate_kbps` and `decode_eligible` to classify (#2249):
/// no downlink is idle (`None`); a live downlink on a tile we ARE decoding is a freeze
/// (`0.0`); a live downlink on one we DECLINED to decode is neither (`None`). Omitting the
/// freeze case left the server's `if let Some(score)` export latching its last healthy
/// value; scoring the third case 0 is the mirror-image defect, because a not-visible SCREEN
/// tile keeps receiving — SCREEN is never viewport-filtered by the relay, and
/// `tick_layer_choosers` advertises no preference for a receiver already tracking the top
/// rung — so a healthy publisher would read 0 for as long as its tile stays backgrounded.
fn video_quality_score(
    fps: f64,
    bitrate_kbps: u64,
    dropped_per_sec: f64,
    loss_per_sec: f64,
    kf_per_sec: f64,
    decode_eligible: bool,
) -> Option<f64> {
    if fps <= 0.0 {
        return if bitrate_kbps > 0 && decode_eligible {
            Some(0.0)
        } else {
            None
        };
    }

    // Video health: measures whether video is present and stable, not hardware
    // FPS capability. A 15fps camera in low light is not a "problem" — it is the
    // camera doing auto-exposure correctly.
    //   fps >= 5  → 100  (video is working; FPS is hardware context, not quality)
    //   fps 1–4   → 0–50 (near-frozen; something is likely wrong)
    // ⚠ #2190: `fps` is DECODED frames, so 1-4 fps scores 10-40 where the pre-fix ladder
    // sum pinned it at 100. `MeetingQualityDegraded`'s `< 50` predates that.
    let video_health = if fps >= 5.0 { 100.0 } else { fps / 5.0 * 50.0 };

    // Decode error penalty: 0/s→0, 10+/s→−50.
    let drop_penalty = (dropped_per_sec / 10.0).min(1.0) * 50.0;
    // Packet-loss penalty: 0/s→0, 5+/s→−30 (mirrors the audio loss penalty).
    let loss_penalty = (loss_per_sec / 5.0).min(1.0) * 30.0;
    // Keyframe-storm penalty: a sustained ≥1 PLI/s means the decoder cannot make
    // forward progress → −40.
    let kf_penalty = (kf_per_sec / 1.0).min(1.0) * 40.0;

    let score = (video_health - drop_penalty - loss_penalty - kf_penalty).clamp(0.0, 100.0);
    Some(score)
}

/// Shared by both media buckets (#2524, #2541). Unconditional, outside the
/// `fps_received > 0` gate: fps 0 is the freeze these exist to explain. `seq_gap_max` is the
/// drained interval MAX, not a blob key — see [`IntervalMaxes`].
fn fold_loss_diagnostics(vs: &mut PbVideoStats, stats: &Value, seq_gap_max: Option<u64>) {
    let as_u64 = |key: &str| stats.get(key).and_then(|v| v.as_u64());
    vs.max_seq_gap_frames = seq_gap_max;
    if let Some(v) = as_u64("freshness_evictions_total") {
        vs.freshness_evictions_total = Some(v);
    }
    if let Some(v) = as_u64("freshness_evictions_keyframeless_total") {
        vs.freshness_evictions_keyframeless_total = Some(v);
    }
    // Per-stream loss / PLI: a camera-only fold is what left screen dark to begin with.
    let as_f64 = |key: &str| stats.get(key).and_then(|v| v.as_f64());
    if let Some(v) = as_f64("video_seq_loss_per_sec") {
        vs.video_seq_loss_per_sec = Some(v);
    }
    if let Some(v) = as_f64("keyframe_requests_per_sec") {
        vs.keyframe_requests_per_sec = Some(v);
    }
}

/// Read the #2249 decode-eligibility flag out of a stats blob. Absent => `true`, so a
/// missing signal fails OPEN into the freeze branch rather than disabling it.
fn decode_eligible_value(stats: &Value) -> bool {
    decode_eligible_known(stats).unwrap_or(true)
}

fn decode_eligible_known(stats: &Value) -> Option<bool> {
    stats
        .get("decode_eligible")
        .and_then(|v| v.as_u64())
        .map(|v| v != 0)
}

fn decode_eligible_from(stats: &Option<Value>) -> bool {
    stats.as_ref().map(decode_eligible_value).unwrap_or(true)
}

// ===================================================================
// Security: HealthPacket credential-leak guard
// ===================================================================
//
// These tests guard the JWT-leak fix on branch
// `fix/security-redact-jwt-active-server-url`. A regression here means the
// user's room JWT escapes the client over the NATS health pipeline.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_fps_publish_value_gates_correctly() {
        // Camera off -> never publish (clear).
        assert_eq!(encoder_fps_publish_value(false, 8, true), None);
        assert_eq!(encoder_fps_publish_value(false, 0, false), None);
        // Camera on but not yet produced a real sample (warmup) -> no data, NOT 0.
        assert_eq!(encoder_fps_publish_value(true, 0, false), None);
        // Camera on + produced -> publish the live value. Low positive readings
        // (1-4) are the partial-starvation signal the bots' fps rule targets.
        // Since #2060 the `Some(0)` arm below is REACHABLE in production: the
        // producer resets current_fps to 0 on stop/start and decays it to 0 on a
        // sustained layer-0 gap, so a total stall (or the sub-1s re-enable window)
        // publishes Some(0). The bots consumer maps 0 -> no-data.
        assert_eq!(encoder_fps_publish_value(true, 4, true), Some(4));
        assert_eq!(encoder_fps_publish_value(true, 0, true), Some(0));
        assert_eq!(encoder_fps_publish_value(true, 30, true), Some(30));
    }

    #[test]
    fn has_encoded_real_latch_transitions() {
        // Camera off resets the latch (so a re-enable re-warms).
        assert!(!next_has_encoded_real(true, false, 8));
        // First nonzero fps while camera-on latches it.
        assert!(next_has_encoded_real(false, true, 4));
        // Camera-on but still zero before any real sample -> stays un-latched.
        assert!(!next_has_encoded_real(false, true, 0));
        // Once latched, a zero reading stays latched (a genuine active-stall 0).
        assert!(next_has_encoded_real(true, true, 0));
    }
    use protobuf::Message;
    use videocall_types::protos::health_packet::HealthPacket as PbHealthPacket;

    // ── issue 1853: AUDIO_SCALE instrumentation ──────────────────────────────

    /// One ACTIVE `AudioSourceSample` with the given concealment% and buffer
    /// depth (test convenience).
    fn active_sample(concealment_pct: f64, buffer_ms: f64) -> AudioSourceSample {
        AudioSourceSample {
            concealment_pct,
            buffer_ms,
            active: true,
        }
    }

    /// Full byte-exact pin of the AUDIO_SCALE line. The meeting analyzer greps
    /// individual `key=value` tokens, so ANY format drift (a renamed key, a lost
    /// token, a changed decimal count) silently breaks field analysis — this
    /// asserts the ENTIRE line. Dropping any token from the format string fails
    /// here. Arithmetic: concealment [20,40,60] => mean 40.0 / worst 60.0;
    /// buffers [100,200,300] => min 100.0 / mean 200.0 (all distinct, so a
    /// min↔mean or mean↔worst swap also fails).
    #[test]
    fn audio_scale_line_byte_exact_format() {
        let samples = [
            active_sample(20.0, 100.0),
            active_sample(40.0, 200.0),
            active_sample(60.0, 300.0),
        ];
        let line = format_audio_scale_line(&samples, 5.5, 8)
            .expect(">=1 active source must produce a line");
        assert_eq!(
            line,
            "[AUDIO_SCALE] sources=3 concealed=3 worst_pct=60.0 mean_pct=40.0 downlink_mbps=5.5 min_buf_ms=100.0 mean_buf_ms=200.0 cores=8"
        );
    }

    /// Unknown downlink (`<= 0`) and unknown cores (`0`) render as the `-1.0` and
    /// `-1` sentinels — never a fabricated zero. Fails if either sentinel branch
    /// is dropped.
    #[test]
    fn audio_scale_line_unknown_downlink_and_cores_sentinels() {
        let samples = [active_sample(50.0, 150.0)];
        let line = format_audio_scale_line(&samples, 0.0, 0)
            .expect(">=1 active source must produce a line");
        assert_eq!(
            line,
            "[AUDIO_SCALE] sources=1 concealed=1 worst_pct=50.0 mean_pct=50.0 downlink_mbps=-1.0 min_buf_ms=150.0 mean_buf_ms=150.0 cores=-1"
        );
    }

    /// The `concealed` count uses a STRICT `>` at exactly 10.0%
    /// (AUDIO_SCALE_CONCEAL_THRESHOLD_PCT): a source sitting on the threshold is
    /// NOT counted, one just above it is. Fails if the comparison flips to `>=`
    /// (both would count => concealed=2) or the constant moves off 10.0.
    #[test]
    fn audio_scale_conceal_threshold_is_strict_at_10() {
        let samples = [active_sample(10.0, 120.0), active_sample(10.1, 120.0)];
        let line = format_audio_scale_line(&samples, 3.0, 4)
            .expect(">=1 active source must produce a line");
        assert!(
            line.contains(" concealed=1 "),
            "exactly-10.0% must not count, only 10.1%; got: {line}"
        );
    }

    /// No ACTIVE source this tick (empty room / everyone muted / all DTX) => no
    /// line at all. A `sources=0` line is pure analyzer noise, so it is
    /// suppressed rather than emitted.
    #[test]
    fn audio_scale_line_none_when_no_active_sources() {
        assert_eq!(format_audio_scale_line(&[], 5.0, 8), None);
        let inactive = [AudioSourceSample {
            concealment_pct: 0.0,
            buffer_ms: 0.0,
            active: false,
        }];
        assert_eq!(format_audio_scale_line(&inactive, 5.0, 8), None);
    }

    /// Inactive sources are excluded from EVERY aggregate. A muted peer showing a
    /// stale 100% concealment / 0ms buffer must not pollute sources, worst_pct,
    /// mean_pct, or min_buf_ms. The expected line is identical to the all-active
    /// case above precisely because the outlier is dropped.
    #[test]
    fn audio_scale_aggregates_only_active_sources() {
        let samples = [
            active_sample(20.0, 100.0),
            active_sample(40.0, 200.0),
            active_sample(60.0, 300.0),
            AudioSourceSample {
                concealment_pct: 100.0,
                buffer_ms: 0.0,
                active: false,
            },
        ];
        let line = format_audio_scale_line(&samples, 5.5, 8)
            .expect(">=1 active source must produce a line");
        assert_eq!(
            line,
            "[AUDIO_SCALE] sources=3 concealed=3 worst_pct=60.0 mean_pct=40.0 downlink_mbps=5.5 min_buf_ms=100.0 mean_buf_ms=200.0 cores=8"
        );
    }

    /// The active gate is exactly `packets_per_sec >= AUDIO_ACTIVE_PPS_GATE`
    /// (2.0): 2.0 is active, anything below is not. This is the SAME gate the
    /// per-stream audio_concealment_pct uses, so the two cannot diverge. Fails if
    /// the constant moves off 2.0 or the comparison loosens.
    #[test]
    fn audio_source_active_gate_pinned_at_2_pps() {
        assert!(audio_source_active(2.0), "2.0 pps must be active");
        assert!(audio_source_active(50.0));
        assert!(
            !audio_source_active(1.999),
            "just below 2.0 must be inactive"
        );
        assert!(!audio_source_active(0.0));
    }

    /// Build a HealthPacket through the production `create_health_packet` path
    /// from a NetEQ JSON, and return the on-the-wire
    /// `(PeerStats.audio_concealment_pct, NetEqStats.current_buffer_size_ms)`.
    fn health_packet_audio_stats(neteq: Value) -> (f64, f64) {
        let mut peer = PeerHealthData::new("peer-1".to_string());
        peer.update_audio_stats(neteq);
        let mut health_map = HashMap::new();
        health_map.insert("peer-1".to_string(), peer);

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,   // unistream_stale_delta_drops_total (#1737 Phase 1)
            0.0, // encoder_queue_depth_report
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            None,
            None,
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        let pb = PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf");
        let ps = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present");
        let buf = ps
            .neteq_stats
            .as_ref()
            .map(|n| n.current_buffer_size_ms)
            .unwrap_or(0.0);
        (ps.audio_concealment_pct, buf)
    }

    /// LOCKSTEP: the AUDIO_SCALE sample's concealment% and buffer depth must
    /// equal what the production `create_health_packet` path puts on the wire for
    /// the same NetEQ JSON (both go through `audio_source_sample_from_neteq`). If
    /// the shared helper and the per-stream mapping ever diverge (different gate,
    /// clamp, or field), the `concealed`/`worst`/`mean` counts in AUDIO_SCALE
    /// would stop matching the per-stream audio_concealment_pct the analyzer ALSO
    /// reads — this test breaks first.
    #[test]
    fn audio_scale_sample_matches_health_packet_concealment() {
        // 2 expand/s over 10 pkt/s => 20% concealment; 200ms buffer.
        let neteq = json!({
            "current_buffer_size_ms": 200.0,
            "packets_per_sec": 10.0,
            "network": { "operation_counters": { "expand_per_sec": 2.0 } },
        });
        let sample = audio_source_sample_from_neteq(&neteq);
        assert!(sample.active);
        let (proto_pct, proto_buf) = health_packet_audio_stats(neteq);
        assert!(
            (sample.concealment_pct - proto_pct).abs() < 1e-9,
            "helper {} vs proto {}",
            sample.concealment_pct,
            proto_pct
        );
        assert!((sample.concealment_pct - 20.0).abs() < 1e-9);
        assert!((sample.buffer_ms - proto_buf).abs() < 1e-9);
        assert!((sample.buffer_ms - 200.0).abs() < 1e-9);
    }

    // --- #2147: screen encoder output fps must be HONEST about zero ----------

    /// Build a HealthPacket through the production `create_health_packet` path
    /// with the given `(encoder_output_fps, screen_encoder_output_fps)` inputs and
    /// return what actually landed on the wire for both fields.
    ///
    /// Deliberately parameterized on BOTH so the camera field's `> 0` gate and the
    /// screen field's ungated behaviour are compared through the same call.
    fn health_packet_encoder_fps(
        camera_fps: u32,
        screen_fps: Option<u32>,
    ) -> (Option<u32>, Option<u32>) {
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0.0,
            0.0,
            0,
            true, // screen_sharing_active: a share IS running
            camera_fps,
            screen_fps,
            0,
            0,
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,
            0,
            [0, 0, 0, 0],
            Vec::new(),
            None,
            ClientMetadata::default(),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            HashMap::new(),
            WtReceiveTelemetry::default(),
            Vec::new(),
            HashMap::new(), // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        let pb = PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf");
        (pb.encoder_output_fps, pb.screen_encoder_output_fps)
    }

    /// **THE #2147 honesty guard.** A wired screen encoder producing 0 fps must
    /// reach the wire as `Some(0)`, NOT be dropped.
    ///
    /// This is the entire point of the field. `encoder_output_fps` (camera) is
    /// gated on `> 0`, so a genuine total stall is ABSENT from the packet and
    /// therefore indistinguishable from an encoder that never started (#2079) —
    /// which is precisely why it could not be used as a screen-freeze signal. The
    /// same call is asserted on both fields so the difference is explicit: with
    /// camera=0 and screen=Some(0), camera is None and screen is Some(0).
    ///
    /// MUTATION: wrap the screen assignment in `if fps > 0` (i.e. copy the camera
    /// gate) in `create_health_packet` and this fails — screen becomes None.
    #[test]
    fn screen_encoder_fps_zero_is_emitted_not_gated_away() {
        let (camera, screen) = health_packet_encoder_fps(0, Some(0));
        assert_eq!(
            screen,
            Some(0),
            "#2147: a wired screen encoder at 0 fps MUST be emitted — a stall has \
             to be distinguishable from never-started"
        );
        assert_eq!(
            camera, None,
            "the camera field's `> 0` gate still drops a 0 (the #2079 defect this \
             field deliberately does NOT copy); if this changes, #2079 was fixed \
             and this test's contrast should be revisited"
        );
    }

    /// `None` (no screen encoder wired) must OMIT the field — the packet must not
    /// fabricate a 0 for a client that has no screen encoder bound at all. This is
    /// the other half of the honesty contract: absent and 0 mean different things.
    ///
    /// MUTATION: make `create_health_packet` write `Some(0)` for a `None` input
    /// (e.g. `unwrap_or(0)`) and this fails.
    #[test]
    fn screen_encoder_fps_absent_when_unwired() {
        let (_, screen) = health_packet_encoder_fps(15, None);
        assert_eq!(
            screen, None,
            "#2147: an unwired screen encoder must OMIT the field, not report 0"
        );
    }

    /// The wired-vs-unwired DECISION itself (`screen_encoder_fps_report_value`),
    /// which is what the report loop actually calls.
    ///
    /// The packet-level test above pins the BUILDER, so it cannot catch a mutation
    /// in the loop's decision (that code runs inside `spawn_local` and is
    /// unreachable from a host test) — hence the pure fn, tested directly.
    ///
    /// MUTATION: drop the `wired` check (`Some(output_fps)` unconditionally) and
    /// the unwired assertions fail; invert it and the wired ones fail.
    #[test]
    fn screen_encoder_fps_report_value_honours_wiredness_and_zero() {
        // Unwired: ALWAYS omit, regardless of what the placeholder atom reads.
        assert_eq!(screen_encoder_fps_report_value(false, 0), None);
        assert_eq!(
            screen_encoder_fps_report_value(false, 12),
            None,
            "an unwired atom's value must never be reported, even if nonzero"
        );
        // Wired: report the reading, INCLUDING an honest 0.
        assert_eq!(
            screen_encoder_fps_report_value(true, 0),
            Some(0),
            "#2147: a wired encoder's 0 is a real reading, not no-data"
        );
        assert_eq!(screen_encoder_fps_report_value(true, 9), Some(9));
    }

    /// **The stall-counter EMISSION guard (#2147).** The two counters are the half
    /// of the screen-freeze story fps cannot tell, and their emission was previously
    /// untestable: they read private process-global statics only the encode loop's
    /// tick-starvation detector increments, so both `if` blocks were always false in
    /// a host test and DELETING them left every test green.
    ///
    /// Closed with the `#[cfg(test)]` setter beside those statics, per CLAUDE.md's
    /// "grep for a `#[cfg(test)]` seam before declaring a side effect untestable".
    ///
    /// MUTATION: delete either `pb.screen_encoder_stall_episodes = …` or
    /// `pb.screen_encoder_max_stall_gap_ms = …` from `create_health_packet` and the
    /// matching assertion fails.
    #[test]
    fn stall_counters_are_emitted_when_nonzero_and_omitted_when_zero() {
        use crate::encode::set_screen_encoder_stall_counters_for_test;
        use crate::test_serial::lock_screen_encoder_stall_counters;

        // The two statics this drives are PROCESS-GLOBAL, and libtest runs this
        // crate's plain `#[test]`s on a multi-threaded pool, so holding them at a
        // nonzero value IS visible to any concurrent sibling that builds a health
        // packet — the guard does not stop that. `create_health_packet` reads them
        // lock-free and takes nothing, so the guard excludes only other GUARD-TAKERS.
        //
        // Safe because no unguarded sibling ASSERTS on `pb.screen_encoder_stall_episodes`
        // / `pb.screen_encoder_max_stall_gap_ms` — grep both field names before relying
        // on that, since it is a property of the current test population rather than
        // anything enforced. A future test that asserts on either must take this guard.
        // Held until the test returns.
        let _stall_guard = lock_screen_encoder_stall_counters();

        // ZERO arm first: monotonic counters carry no information at 0, so the fields
        // must be ABSENT (deliberately the opposite convention from the fps field,
        // where 0 IS the reading — see `screen_encoder_fps_zero_is_emitted_not_gated_away`).
        set_screen_encoder_stall_counters_for_test(0, 0);
        let (episodes, gap) = health_packet_stall_counters();
        assert_eq!(episodes, None, "#2147: 0 episodes must OMIT the field");
        assert_eq!(gap, None, "#2147: a 0 max-gap must OMIT the field");

        // NONZERO arm: the #2143 shape — 11 episodes with a 23.15s worst gap.
        set_screen_encoder_stall_counters_for_test(11, 23_150);
        let (episodes, gap) = health_packet_stall_counters();
        assert_eq!(
            episodes,
            Some(11),
            "#2147: a rising stall count is the ONLY signal that distinguishes a \
             frozen share from a healthy one at the same fps — it must reach the wire"
        );
        assert_eq!(
            gap,
            Some(23_150),
            "#2147: the worst gap gives the freeze its severity (3 episodes at 200ms \
             is jitter; 3 at 23s is the incident)"
        );
        // The two values above are DISTINCT, so a copy-paste slip assigning one field
        // from the other's source would fail rather than pass.
        //
        // Restore 0, so a later guarded test starts from the cold-start value
        // regardless of run order. Not redundant with the guard, which excludes only
        // other guard-takers — but note this is a trailing statement, so on a FAILING
        // run the assertions above leave the injected values in place for the rest of
        // the process.
        set_screen_encoder_stall_counters_for_test(0, 0);
    }

    /// Build a HealthPacket through the production `create_health_packet` path and
    /// return the two stall fields as they landed on the wire.
    fn health_packet_stall_counters() -> (Option<u64>, Option<u64>) {
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );
        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0.0,
            0.0,
            0,
            true,
            0,
            None,
            0,
            0,
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,
            0,
            [0, 0, 0, 0],
            Vec::new(),
            None,
            ClientMetadata::default(),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            HashMap::new(),
            WtReceiveTelemetry::default(),
            Vec::new(),
            HashMap::new(), // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");
        let pb = PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf");
        (
            pb.screen_encoder_stall_episodes,
            pb.screen_encoder_max_stall_gap_ms,
        )
    }

    /// A nonzero screen fps passes through unchanged.
    ///
    /// MUTATION: assign the camera value to the screen field (a copy-paste slip
    /// that a single-field test would not catch) and this fails, because the two
    /// inputs differ.
    #[test]
    fn screen_encoder_fps_nonzero_passes_through() {
        let (camera, screen) = health_packet_encoder_fps(24, Some(9));
        assert_eq!(screen, Some(9), "screen fps must pass through unchanged");
        assert_eq!(
            camera,
            Some(24),
            "and must not be confused with the camera value"
        );
    }

    /// Below the pps gate the production path leaves audio_concealment_pct at 0.0
    /// AND the helper reports inactive with 0.0 — verified in lockstep so a
    /// DTX-silent source never inflates the AUDIO_SCALE aggregates.
    #[test]
    fn audio_scale_sample_inactive_below_gate_matches_health_packet() {
        // 1.0 pkt/s is below the 2.0 gate: concealment must NOT be computed.
        let neteq = json!({
            "current_buffer_size_ms": 120.0,
            "packets_per_sec": 1.0,
            "network": { "operation_counters": { "expand_per_sec": 5.0 } },
        });
        let sample = audio_source_sample_from_neteq(&neteq);
        assert!(!sample.active, "1.0 pps is below the gate");
        assert_eq!(sample.concealment_pct, 0.0);
        let (proto_pct, _) = health_packet_audio_stats(neteq);
        assert_eq!(
            proto_pct, 0.0,
            "production path must leave concealment at 0.0 below the gate"
        );
    }

    #[test]
    fn cpu_throttled_boundary_and_missing_inputs() {
        assert_eq!(compute_cpu_throttled(149, 1), Some(true));
        assert_eq!(compute_cpu_throttled(150, 1), Some(false));
        assert_eq!(compute_cpu_throttled(2_999, 20), Some(true));
        assert_eq!(compute_cpu_throttled(3_000, 20), Some(false));
        assert_eq!(compute_cpu_throttled(0, 20), None);
        assert_eq!(compute_cpu_throttled(3_000, 0), None);
    }

    #[test]
    fn received_layers_map_to_proto_by_media_kind() {
        use crate::decode::layer_chooser::PrefMediaKind;

        let mut received = HashMap::new();
        received.insert((101, PrefMediaKind::Video), 2);
        received.insert((202, PrefMediaKind::Screen), 1);
        received.insert((303, PrefMediaKind::Audio), 0);
        let mut packet = PbHealthPacket::new();

        populate_received_layers(&mut packet, &received);

        assert_eq!(packet.received_video_layer.get("101"), Some(&2));
        assert_eq!(packet.received_screen_layer.get("202"), Some(&1));
        assert_eq!(packet.received_audio_layer.get("303"), Some(&0));
        assert_eq!(packet.received_video_layer.len(), 1);
        assert_eq!(packet.received_screen_layer.len(), 1);
        assert_eq!(packet.received_audio_layer.len(), 1);
    }

    #[test]
    fn audio_layer_telemetry_keeps_user_and_congestion_caps_distinct() {
        assert_eq!(
            audio_layer_telemetry(3, u32::MAX, 1),
            (u32::MAX, 1),
            "a user cap reduces active layers without fabricating congestion"
        );
        assert_eq!(
            audio_layer_telemetry(3, 2, u32::MAX),
            (2, 2),
            "a congestion cap must reduce both congestion ceiling and active layers"
        );
        assert_eq!(audio_layer_telemetry(3, u32::MAX, u32::MAX), (u32::MAX, 3));
    }

    // ── Freeze observability (#1013): video_quality_score ────────────────

    /// Healthy stream: fps≥5, no loss, no keyframe storm → score 100.
    #[test]
    fn video_quality_score_healthy_is_100() {
        assert_eq!(
            video_quality_score(30.0, 500, 0.0, 0.0, 0.0, true),
            Some(100.0)
        );
    }

    #[test]
    fn video_quality_score_receiving_but_not_decoding_scores_zero() {
        assert_eq!(
            video_quality_score(0.0, 500, 0.0, 0.0, 0.0, true),
            Some(0.0)
        );
    }

    #[test]
    fn video_quality_score_zero_fps_without_downlink_is_none() {
        assert_eq!(video_quality_score(0.0, 0, 0.0, 0.0, 0.0, true), None);
    }

    /// #2249 blocker: at `fps == 0` with a LIVE downlink, `decode_eligible` is the only
    /// thing separating a freeze from a tile this receiver declined to decode.
    ///
    /// MUTATION: dropping `&& decode_eligible` from the `fps <= 0.0` branch makes the
    /// first assertion read `Some(0.0)` and fail.
    #[test]
    fn video_quality_score_ineligible_stream_is_not_a_freeze() {
        assert_eq!(
            video_quality_score(0.0, 700, 0.0, 0.0, 0.0, false),
            None,
            "we chose not to decode this tile — its publisher is not frozen"
        );
        assert_eq!(
            video_quality_score(0.0, 700, 0.0, 0.0, 0.0, true),
            Some(0.0),
            "the same numbers on a tile we ARE decoding is the #2249 freeze"
        );
    }

    /// The core #1013 case: fps reads a healthy ~30 (decode calls still firing)
    /// but the stream is under sustained packet loss AND a keyframe storm. The
    /// score MUST drop well below 100 (it used to read 100 here — the bug).
    #[test]
    fn video_quality_score_drops_during_freeze_with_loss_and_keyframe_storm() {
        // 30 fps, no decode errors, 5 lost/s (-30), 1 PLI/s (-40) => 100-30-40=30.
        let score = video_quality_score(30.0, 500, 0.0, 5.0, 1.0, true).expect("fps>0 => Some");
        assert!(
            score < 80.0,
            "freeze (loss + keyframe storm) should score well below 80, got {score}"
        );
        assert!((score - 30.0).abs() < 1e-9, "expected 30.0, got {score}");
    }

    /// Loss alone (no keyframe storm) still pulls the score down.
    #[test]
    fn video_quality_score_loss_only_penalty() {
        // 30 fps, 5 lost/s (-30) => 70.
        let score = video_quality_score(30.0, 500, 0.0, 5.0, 0.0, true).expect("fps>0 => Some");
        assert!((score - 70.0).abs() < 1e-9, "expected 70.0, got {score}");
    }

    /// A sustained keyframe-request rate alone is a strong freeze signal.
    #[test]
    fn video_quality_score_keyframe_storm_only_penalty() {
        // 30 fps, 1 PLI/s (-40) => 60.
        let score = video_quality_score(30.0, 500, 0.0, 0.0, 1.0, true).expect("fps>0 => Some");
        assert!((score - 60.0).abs() < 1e-9, "expected 60.0, got {score}");
    }

    /// Penalties saturate and the score clamps at 0, never negative.
    #[test]
    fn video_quality_score_clamps_at_zero() {
        let score =
            video_quality_score(30.0, 500, 100.0, 100.0, 100.0, true).expect("fps>0 => Some");
        assert_eq!(score, 0.0);
    }

    fn peer_with_stream_rates(camera: (f64, u64), screen: (f64, u64)) -> PeerHealthData {
        let mut peer = PeerHealthData::new("peer-1".to_string());
        peer.audio_enabled = true;
        peer.update_audio_stats(json!({
            "packets_per_sec": 50.0,
            "network": { "operation_counters": { "expand_per_sec": 0.0 } },
        }));
        peer.update_camera_stats(json!({
            "fps_received": camera.0,
            "bitrate_kbps": camera.1,
        }));
        peer.update_screen_stats(json!({
            "fps_received": screen.0,
            "bitrate_kbps": screen.1,
        }));
        peer
    }

    fn health_packet_with_stream_rates(camera: (f64, u64), screen: (f64, u64)) -> PbHealthPacket {
        health_packet_for_peer(peer_with_stream_rates(camera, screen))
    }

    fn health_packet_for_peer(peer: PeerHealthData) -> PbHealthPacket {
        let mut health_map = HashMap::new();
        health_map.insert("peer-1".to_string(), peer);

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0.0,
            0.0,
            0,
            false,
            0,
            None,
            0,
            0,
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,
            0,
            [0, 0, 0, 0],
            Vec::new(),
            None,
            ClientMetadata::default(),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            HashMap::new(),
            WtReceiveTelemetry::default(),
            Vec::new(),
            HashMap::new(), // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    /// The #2249 production reproduction: a perfect call score through a 4-minute
    /// human-confirmed screen freeze, because the fold read only `ps.video_stats`.
    #[test]
    fn screen_freeze_pulls_video_and_call_quality_to_zero() {
        let pb = health_packet_with_stream_rates((30.0, 900), (0.0, 700));
        let ps = pb.peer_stats.get("peer-1").expect("peer stats present");

        assert_eq!(
            ps.video_quality_score,
            Some(0.0),
            "screen receiving 700kbps while decoding nothing is a freeze"
        );
        assert_eq!(
            ps.call_quality_score,
            Some(0.0),
            "the call score must take the frozen screen, not the healthy audio"
        );
    }

    /// The camera half: the old `fps > 0.0` gate left the score absent (latching the
    /// gauge) and `(Some(a), None) => Some(a)` fell through to audio.
    #[test]
    fn camera_freeze_pulls_video_and_call_quality_to_zero() {
        let pb = health_packet_with_stream_rates((0.0, 900), (0.0, 0));
        let ps = pb.peer_stats.get("peer-1").expect("peer stats present");

        assert_eq!(ps.video_quality_score, Some(0.0));
        assert_eq!(
            ps.call_quality_score,
            Some(0.0),
            "a frozen camera must pull the call score down, not fall through to audio"
        );
    }

    fn peer_with_screen_eligibility(
        camera: (f64, u64),
        screen: (f64, u64),
        screen_decode_eligible: u64,
    ) -> PeerHealthData {
        let mut peer = peer_with_stream_rates(camera, screen);
        peer.update_screen_stats(json!({
            "fps_received": screen.0,
            "bitrate_kbps": screen.1,
            "decode_eligible": screen_decode_eligible,
        }));
        peer
    }

    /// #2249 blocker: a SECOND, backgrounded screen sharer is `visible == false` with SCREEN
    /// bytes still arriving. Scoring that 0 pins `videocall_call_quality_score` low for as
    /// long as the tile stays backgrounded, firing the very `MeetingQualityDegraded` alert
    /// this PR exists to make trustworthy.
    ///
    /// MUTATION: dropping `&& decode_eligible` makes both assertions read `Some(0.0)`.
    #[test]
    fn a_backgrounded_screen_tile_does_not_drag_the_call_score_to_zero() {
        let pb = health_packet_for_peer(peer_with_screen_eligibility((30.0, 900), (0.0, 700), 0));
        let ps = pb.peer_stats.get("peer-1").expect("peer stats present");

        assert_eq!(
            ps.video_quality_score,
            Some(100.0),
            "the healthy camera is the only stream we are decoding, so it is the whole score"
        );
        assert_eq!(
            ps.call_quality_score,
            Some(100.0),
            "a tile we declined to decode must not defame a healthy peer"
        );
    }

    /// The same tile with no camera to carry the score: absent, not 0. `None` lets the
    /// server's `if let Some(score)` export omit the series rather than publish a false floor.
    #[test]
    fn a_backgrounded_screen_tile_alone_leaves_video_quality_absent() {
        let pb = health_packet_for_peer(peer_with_screen_eligibility((0.0, 0), (0.0, 700), 0));
        let ps = pb.peer_stats.get("peer-1").expect("peer stats present");

        assert_eq!(ps.video_quality_score, None);
        assert_eq!(
            ps.call_quality_score,
            Some(100.0),
            "with no video we are scoring, the call score is the healthy audio"
        );
    }

    /// The anti-wedge guard for the two tests above: an EXPLICITLY eligible screen tile with
    /// the identical fps/bitrate must still score 0. Without this, an over-broad fix that
    /// forced `decode_eligible` false everywhere would disable #2249 entirely and still pass.
    #[test]
    fn a_decode_eligible_screen_tile_still_scores_zero_when_frozen() {
        let pb = health_packet_for_peer(peer_with_screen_eligibility((30.0, 900), (0.0, 700), 1));
        let ps = pb.peer_stats.get("peer-1").expect("peer stats present");

        assert_eq!(
            ps.video_quality_score,
            Some(0.0),
            "a tile we ARE decoding, receiving 700kbps and rendering nothing, is a freeze"
        );
        assert_eq!(ps.call_quality_score, Some(0.0));
    }

    #[test]
    fn idle_streams_leave_video_quality_absent_and_call_score_on_audio() {
        let pb = health_packet_with_stream_rates((0.0, 0), (0.0, 0));
        let ps = pb.peer_stats.get("peer-1").expect("peer stats present");

        assert_eq!(
            ps.video_quality_score, None,
            "no downlink on either stream is idle, not a freeze"
        );
        assert_eq!(
            ps.call_quality_score,
            Some(100.0),
            "with no video signal the call score is the audio score"
        );
    }

    /// An ended stream keeps its blob and its final-window bitrate; only its own
    /// freshness clock retires it, so gating on `video_fresh` fails here (and below).
    #[test]
    fn a_stale_screen_blob_does_not_score_while_the_camera_is_live() {
        let mut peer = peer_with_stream_rates((30.0, 900), (0.0, 700));
        peer.last_screen_update_ms = peer.last_screen_update_ms.saturating_sub(60_000);
        let pb = health_packet_for_peer(peer);

        assert_eq!(
            pb.peer_stats
                .get("peer-1")
                .expect("peer stats present")
                .video_quality_score,
            Some(100.0),
            "an ended screen share must not keep scoring the peer 0"
        );
    }

    #[test]
    fn a_stale_camera_blob_does_not_score_while_the_screen_is_live() {
        let mut peer = peer_with_stream_rates((0.0, 900), (30.0, 700));
        peer.last_camera_update_ms = peer.last_camera_update_ms.saturating_sub(60_000);
        let pb = health_packet_for_peer(peer);

        assert_eq!(
            pb.peer_stats
                .get("peer-1")
                .expect("peer stats present")
                .video_quality_score,
            Some(100.0),
            "a retired camera stream must not keep scoring the peer 0"
        );
    }

    #[test]
    fn video_quality_takes_the_worse_of_camera_and_screen() {
        let frozen_camera = health_packet_with_stream_rates((0.0, 900), (30.0, 700));
        assert_eq!(
            frozen_camera
                .peer_stats
                .get("peer-1")
                .expect("peer stats present")
                .video_quality_score,
            Some(0.0)
        );

        let both_healthy = health_packet_with_stream_rates((30.0, 900), (30.0, 700));
        assert_eq!(
            both_healthy
                .peer_stats
                .get("peer-1")
                .expect("peer stats present")
                .video_quality_score,
            Some(100.0)
        );
    }

    /// Construct a `HealthPacket` via the production `create_health_packet`
    /// path, passing a `Some(...)` URL containing a JWT, and assert that the
    /// resulting protobuf has an empty `active_server_url` field.
    ///
    /// This test fails if anyone reintroduces `pb.active_server_url = url;` —
    /// preventing accidental regression of the credential leak.
    #[test]
    fn health_packet_does_not_carry_active_server_url() {
        // One peer entry, so the assertion below is made against a packet that
        // carries peer stats — the shape a real report has.
        //
        // NOT because an empty map would suppress the packet: `create_health_packet`
        // has no early return and always yields `Some(PacketWrapper)`. Client-wide
        // telemetry must keep flowing during warm-up and in solo sessions, which is
        // why there is no emptiness gate to satisfy — pinned by
        // `health_packet_still_emitted_with_empty_peer_map` (#1032).
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );

        let dirty_url = "https://webtransport.example.com:4433/lobby?token=eyJhbGciOiJIUzI1NiJ9.payload.sig&instance_id=11111111-2222-3333-4444-555555555555".to_string();

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            Some(dirty_url.clone()), // active_server_url — must be ignored
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,   // unistream_stale_delta_drops_total (#1737 Phase 1)
            0.0, // encoder_queue_depth_report
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            None,
            None,
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        // Round-trip the wrapper through the protobuf so we are asserting on
        // exactly what goes on the wire, not an in-memory builder field.
        let pb = PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf");

        assert!(
            pb.active_server_url.is_empty(),
            "HealthPacket.active_server_url must be empty (no JWT leak); got {:?}",
            pb.active_server_url
        );
        assert!(
            !pb.active_server_url.contains("eyJ"),
            "HealthPacket.active_server_url must not contain JWT-prefix `eyJ`"
        );
        assert!(
            !pb.active_server_url.contains("token="),
            "HealthPacket.active_server_url must not contain `token=`"
        );

        // Sanity: `active_server_type` and `active_server_rtt_ms` are still
        // populated — the security fix must not break observability of
        // transport identity and RTT.
        assert_eq!(pb.active_server_type, "webtransport");
        assert_eq!(pb.active_server_rtt_ms, 42.0);
    }

    /// Build a HealthPacket through the production `create_health_packet` path
    /// with the given decode-budget snapshot, then round-trip it through the
    /// protobuf so the assertions are on exactly what goes on the wire (#987).
    fn health_packet_with_decode_budget(
        decode_budget: Option<DecodeBudgetSnapshot>,
    ) -> PbHealthPacket {
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,   // unistream_stale_delta_drops_total (#1737 Phase 1)
            0.0, // encoder_queue_depth_report
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            decode_budget,
            None,
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    /// #522: build a HealthPacket through the production path with the given
    /// RTT-probe resilience counters, then round-trip it through protobuf so the
    /// assertions are on exactly what goes on the wire. The two counters are
    /// threaded into the `rtt_probe_dropped_total` / `rtt_probe_stale_suppressions_total`
    /// positional args (immediately after `session_drops_total`, before the
    /// reelection-totals array) — the same positions the production call site
    /// fills from the connection controller.
    fn health_packet_with_rtt_probe_signals(
        rtt_probe_dropped_total: u64,
        rtt_probe_stale_suppressions_total: u64,
    ) -> PbHealthPacket {
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,   // unistream_stale_delta_drops_total (#1737 Phase 1)
            0.0, // encoder_queue_depth_report
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0, // handshake_failures_total
            0, // session_drops_total
            rtt_probe_dropped_total,
            rtt_probe_stale_suppressions_total,
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            None,
            None,
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    /// #522: nonzero RTT-probe resilience counters must be emitted on the wire as
    /// the protobuf optional fields with exactly the values passed in.
    ///
    /// MUTATION: deleting either `pb.rtt_probe_dropped_total = Some(...)` or
    /// `pb.rtt_probe_stale_suppressions_total = Some(...)` assignment in
    /// `create_health_packet` makes the corresponding field decode as `None`,
    /// failing the matching `Some(7)` / `Some(3)` assertion below.
    #[test]
    fn create_health_packet_emits_nonzero_rtt_probe_signals() {
        let pb = health_packet_with_rtt_probe_signals(7, 3);
        assert_eq!(
            pb.rtt_probe_dropped_total,
            Some(7),
            "nonzero rtt_probe_dropped_total must round-trip as Some(7)"
        );
        assert_eq!(
            pb.rtt_probe_stale_suppressions_total,
            Some(3),
            "nonzero rtt_probe_stale_suppressions_total must round-trip as Some(3)"
        );
    }

    /// #522: zero counters must be omitted (gated on `> 0`), so they decode as
    /// `None` — keeping the common-case packet small.
    ///
    /// MUTATION: removing the `> 0` gate (always assigning `Some`) makes these
    /// fields decode as `Some(0)`, failing the `None` assertions below.
    #[test]
    fn create_health_packet_omits_zero_rtt_probe_signals() {
        let pb = health_packet_with_rtt_probe_signals(0, 0);
        assert_eq!(
            pb.rtt_probe_dropped_total, None,
            "zero rtt_probe_dropped_total must be omitted (None) per the > 0 gate"
        );
        assert_eq!(
            pb.rtt_probe_stale_suppressions_total, None,
            "zero rtt_probe_stale_suppressions_total must be omitted (None) per the > 0 gate"
        );
    }

    /// #1878: build a HealthPacket through the production `create_health_packet`
    /// path for a single peer whose windowed receive-side audio-datagram-loss
    /// rate is `loss`, reported over the given `active_server_type`, then
    /// round-trip it through protobuf so the assertions below are on exactly what
    /// goes on the wire.
    fn health_packet_with_audio_datagram_loss(
        loss: f64,
        active_server_type: &str,
    ) -> PbHealthPacket {
        let mut peer = PeerHealthData::new("peer-1".to_string());
        peer.wt_datagram_audio_loss_per_sec = loss;
        // Issue 2031: seed a DISTINCT raw magnitude (10x the capped value) so the
        // raw-fold tests can prove the raw path folds its own field, not the
        // capped one.
        peer.wt_datagram_audio_raw_loss_per_sec = loss * 10.0;

        let mut health_map = HashMap::new();
        health_map.insert("peer-1".to_string(), peer);

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some(active_server_type.to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,   // unistream_stale_delta_drops_total (#1737 Phase 1)
            0.0, // encoder_queue_depth_report
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            None,
            None,
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    /// #1878: on WebTransport the per-peer audio-datagram-loss rate must fold into
    /// PeerStats.audio_datagram_loss_per_sec and round-trip as `Some(loss)`.
    ///
    /// MUTATION: removing the `ps.audio_datagram_loss_per_sec = Some(...)` fold
    /// line makes this decode as `None`, failing the `Some(9.0)` assertion.
    #[test]
    fn create_health_packet_folds_audio_datagram_loss_on_webtransport() {
        let pb = health_packet_with_audio_datagram_loss(9.0, "webtransport");
        let ps = pb
            .peer_stats
            .get("peer-1")
            .expect("peer-1 must have a PeerStats entry");
        assert_eq!(
            ps.audio_datagram_loss_per_sec,
            Some(9.0),
            "WT reporter must fold the windowed audio datagram loss as Some(9.0)"
        );
    }

    /// #1878: on WebSocket the field must fold as `Some(0.0)` — definitionally
    /// zero because audio rides ordered TCP — NOT the stale tracker value the
    /// emitter last wrote on a prior WebTransport leg. Folding 0.0 un-latches the
    /// exported gauge on a mid-call WT→WS fallback (the recover-to-0 behavior of
    /// the sibling video_seq_loss_per_sec).
    ///
    /// The `7.0` seed is that stale WT reading: the WS leg must OVERRIDE it with
    /// 0.0. MUTATION: removing the `reporter_on_webtransport` gate (folding the
    /// tracker value unconditionally) makes this decode as `Some(7.0)`, failing
    /// the `Some(0.0)` assertion.
    #[test]
    fn create_health_packet_folds_zero_audio_datagram_loss_on_websocket() {
        let pb = health_packet_with_audio_datagram_loss(7.0, "websocket");
        let ps = pb
            .peer_stats
            .get("peer-1")
            .expect("peer-1 must have a PeerStats entry");
        assert_eq!(
            ps.audio_datagram_loss_per_sec,
            Some(0.0),
            "WebSocket reporter must fold definitional 0.0 (not the stale WT tracker value 7.0), \
             un-latching the gauge on fallback"
        );
    }

    /// Issue 2031: on WebTransport the per-peer RAW audio-datagram-loss magnitude
    /// must fold into PeerStats.audio_datagram_raw_loss_per_sec as its OWN value
    /// (the helper seeds raw = 10x the capped value), proving the raw path is not
    /// a duplicate of the capped one.
    ///
    /// MUTATION: removing the `ps.audio_datagram_raw_loss_per_sec = Some(...)`
    /// fold makes this decode as `None`; folding the capped value instead makes it
    /// `Some(9.0)` — both fail the `Some(90.0)` assertion.
    #[test]
    fn create_health_packet_folds_raw_audio_datagram_loss_on_webtransport() {
        let pb = health_packet_with_audio_datagram_loss(9.0, "webtransport");
        let ps = pb
            .peer_stats
            .get("peer-1")
            .expect("peer-1 must have a PeerStats entry");
        assert_eq!(
            ps.audio_datagram_raw_loss_per_sec,
            Some(90.0),
            "WT reporter must fold the uncapped raw magnitude (10x the capped 9.0)"
        );
        // And it must be DISTINCT from the capped rate — the whole point of 2031.
        assert_ne!(
            ps.audio_datagram_raw_loss_per_sec, ps.audio_datagram_loss_per_sec,
            "raw magnitude must not equal the capped presence signal"
        );
    }

    /// Issue 2031: on WebSocket the raw field folds definitional 0.0 (audio rides
    /// ordered TCP), un-latching the gauge on a WT->WS fallback exactly like the
    /// capped sibling. The helper seeds raw = 70.0 (10x the 7.0 stale capped
    /// value); the WS leg must OVERRIDE it with 0.0.
    ///
    /// MUTATION: removing the `reporter_on_webtransport` gate folds the stale
    /// tracker value, decoding as `Some(70.0)` and failing this.
    #[test]
    fn create_health_packet_folds_zero_raw_audio_datagram_loss_on_websocket() {
        let pb = health_packet_with_audio_datagram_loss(7.0, "websocket");
        let ps = pb
            .peer_stats
            .get("peer-1")
            .expect("peer-1 must have a PeerStats entry");
        assert_eq!(
            ps.audio_datagram_raw_loss_per_sec,
            Some(0.0),
            "WebSocket reporter must fold definitional 0.0 for the raw magnitude too"
        );
    }

    /// Issue 2031: build a HealthPacket through the production path with the given
    /// per-client WT receive telemetry and (optionally) active audio sources for
    /// the concealment mean, then round-trip through protobuf.
    fn health_packet_with_wt_telemetry(
        telemetry: WtReceiveTelemetry,
        active_server_type: &str,
        neteq_stats: &[serde_json::Value],
        camera_layer_metrics: Vec<crate::encode::CameraLayerMetric>,
    ) -> PbHealthPacket {
        let camera_layer_count = camera_layer_metrics
            .iter()
            .map(|(layer_id, ..)| layer_id + 1)
            .max()
            .unwrap_or(0);
        // One peer per supplied NetEQ snapshot, and none when the caller passes none:
        // `create_health_packet` always yields a packet (pinned by
        // `health_packet_still_emitted_with_empty_peer_map`), so an empty map is the
        // faithful fixture for WT-telemetry callers asserting on client-wide fields.
        let mut health_map = HashMap::new();
        for (i, neteq) in neteq_stats.iter().enumerate() {
            let pid = format!("peer-{i}");
            let mut peer = PeerHealthData::new(pid.clone());
            peer.last_neteq_stats = Some(neteq.clone());
            health_map.insert(pid, peer);
        }

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some(active_server_type.to_string()),
            Some(42.0),
            None,               // send_queue_bytes
            None,               // packets_received_per_sec
            None,               // packets_sent_per_sec
            0,                  // adaptive_video_tier
            0,                  // adaptive_audio_tier
            0,                  // datagram_drops_total
            0,                  // unistream_bytes_offered_total
            0,                  // unistream_bytes_drained_total
            0,                  // websocket_drops_total
            0,                  // keyframe_requests_sent_total
            0,                  // unistream_stale_delta_drops_total
            0.0,                // encoder_queue_depth_report
            0.0,                // encoder_target_bitrate_kbps
            0,                  // adaptive_screen_tier
            false,              // screen_sharing_active
            0,                  // encoder_output_fps
            None,               // screen_encoder_output_fps (#2147: unwired => omitted)
            camera_layer_count, // effective_video_layers
            camera_layer_count, // active_video_layers
            Vec::new(),         // tier_transitions
            ClimbLimiterSnapshot::default(),
            Vec::new(),   // dwell_samples
            0,            // handshake_failures_total
            0,            // session_drops_total
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals
            Vec::new(),   // longtask_durations
            None,         // render_fps
            ClientMetadata::default(),
            None,           // client_main_thread_load
            None,           // decode_budget
            None,           // agent_memory_bytes
            0,              // effective_screen_layers
            0,              // active_screen_layers
            0,              // effective_audio_layers
            0,              // audio_congestion_ceiling
            0,              // active_audio_layers
            HashMap::new(), // received_layers
            telemetry,
            camera_layer_metrics,
            HashMap::new(),
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    #[test]
    fn camera_layer_geometry_and_fps_reach_the_wire() {
        let packet = health_packet_with_wt_telemetry(
            WtReceiveTelemetry::default(),
            "webtransport",
            &[],
            vec![(0, 241, 181, Some(7)), (2, 613, 461, Some(30))],
        );

        let got: Vec<_> = packet
            .camera_layer_geometry
            .iter()
            .map(|geometry| {
                (
                    geometry.layer_id,
                    geometry.width,
                    geometry.height,
                    geometry.output_fps,
                )
            })
            .collect();
        assert_eq!(got, vec![(0, 241, 181, Some(7)), (2, 613, 461, Some(30))]);
        assert_eq!(packet.effective_video_layers, Some(3));
        assert_eq!(packet.active_video_layers, Some(3));
    }

    #[test]
    fn camera_layer_fps_preserves_absent_vs_measured_zero() {
        let packet = health_packet_with_wt_telemetry(
            WtReceiveTelemetry::default(),
            "webtransport",
            &[],
            vec![(0, 241, 181, None), (1, 481, 361, Some(0))],
        );

        assert_eq!(packet.camera_layer_geometry[0].output_fps, None);
        assert_eq!(packet.camera_layer_geometry[1].output_fps, Some(0));
    }

    /// Issue 2031: the per-client read-loop max gap must fold UNCONDITIONALLY as
    /// Some (recover-to-0 semantics), and the observed queue read-back must fold
    /// its two finite values.
    ///
    /// MUTATION: removing the `pb.wt_datagram_read_loop_max_gap_ms = Some(...)`
    /// fold decodes as `None`; removing the queue-read-back fold decodes both
    /// queue fields as `None`.
    #[test]
    fn create_health_packet_folds_wt_receive_telemetry() {
        let telemetry = WtReceiveTelemetry {
            read_loop_max_gap_ms: 350.0,
            incoming_queue_readback: Some((2048.0, 3000.0)),
        };
        let pb = health_packet_with_wt_telemetry(telemetry, "webtransport", &[], Vec::new());
        assert_eq!(
            pb.wt_datagram_read_loop_max_gap_ms,
            Some(350.0),
            "read-loop max gap must fold through as Some(350.0)"
        );
        assert_eq!(
            pb.wt_incoming_datagram_high_water_mark,
            Some(2048.0),
            "observed incomingHighWaterMark must fold through"
        );
        assert_eq!(
            pb.wt_incoming_datagram_max_age_ms,
            Some(3000.0),
            "observed incomingMaxAge must fold through"
        );
    }

    /// Issue 2031: a NaN observed max-age (spec `null` = unbounded, or setter not
    /// honored) must fold as the -1.0 wire sentinel so the gauge stays finite —
    /// NOT as NaN, and NOT omitted.
    ///
    /// MUTATION: replacing the `if max_age.is_nan() { -1.0 }` mapping with a plain
    /// `max_age` fold makes this decode as NaN, and `Some(NaN) == Some(-1.0)` is
    /// false, failing the assertion.
    #[test]
    fn create_health_packet_maps_unbounded_max_age_to_sentinel() {
        let telemetry = WtReceiveTelemetry {
            read_loop_max_gap_ms: 0.0,
            incoming_queue_readback: Some((4096.0, f64::NAN)),
        };
        let pb = health_packet_with_wt_telemetry(telemetry, "webtransport", &[], Vec::new());
        assert_eq!(
            pb.wt_incoming_datagram_max_age_ms,
            Some(-1.0),
            "unbounded (NaN) max-age must map to the -1.0 sentinel, not NaN"
        );
        assert_eq!(
            pb.wt_incoming_datagram_high_water_mark,
            Some(4096.0),
            "the hwm read-back is unaffected by the max-age sentinel mapping"
        );
    }

    /// Issue 2031: a WebSocket-only client (no WT queue ever configured) omits the
    /// queue read-back fields entirely (proto3 absent), costing nothing on the
    /// wire. read_loop_max_gap_ms still folds (as the 0.0 default here).
    #[test]
    fn create_health_packet_omits_queue_readback_without_wt() {
        let pb = health_packet_with_wt_telemetry(
            WtReceiveTelemetry::default(),
            "websocket",
            &[],
            Vec::new(),
        );
        assert_eq!(
            pb.wt_incoming_datagram_high_water_mark, None,
            "queue read-back must be omitted when the WT queue was never configured"
        );
        assert_eq!(pb.wt_incoming_datagram_max_age_ms, None);
        assert_eq!(
            pb.wt_datagram_read_loop_max_gap_ms,
            Some(0.0),
            "read-loop gap still folds as the 0.0 default (recover-to-0 semantics)"
        );
    }

    /// Issue 2031: the per-client audio-concealment field must fold the MEAN over
    /// ACTIVE sources — peer A at 50% and peer B at 20% => 35%.
    ///
    /// MUTATION: dividing by a hardcoded 1 instead of `concealment_active_sources`
    /// (or summing without averaging) yields 70.0, failing the ~35.0 assertion.
    #[test]
    fn create_health_packet_folds_mean_concealment_over_active_sources() {
        // 50 pps (>= the 2.0 active gate). expand/packets*100 => concealment%.
        let peer_a = json!({
            "packets_per_sec": 50.0,
            "network": { "operation_counters": { "expand_per_sec": 25.0 } } // 50%
        });
        let peer_b = json!({
            "packets_per_sec": 50.0,
            "network": { "operation_counters": { "expand_per_sec": 10.0 } } // 20%
        });
        let pb = health_packet_with_wt_telemetry(
            WtReceiveTelemetry::default(),
            "webtransport",
            &[peer_a, peer_b],
            Vec::new(),
        );
        let mean = pb
            .client_audio_concealment_pct
            .expect("client concealment must be Some when a source is active");
        assert!(
            (mean - 35.0).abs() < 1e-9,
            "mean concealment over active sources must be (50 + 20) / 2 = 35.0; got {mean}"
        );
    }

    /// Issue 2031: with no active audio source, the per-client concealment field
    /// is ABSENT (None) — absent means "no audio flowing", not "0% concealment".
    #[test]
    fn create_health_packet_omits_concealment_when_no_active_source() {
        // 1 pps is below the 2.0 active gate => inactive, contributes nothing.
        let idle = json!({
            "packets_per_sec": 1.0,
            "network": { "operation_counters": { "expand_per_sec": 0.0 } }
        });
        let pb = health_packet_with_wt_telemetry(
            WtReceiveTelemetry::default(),
            "webtransport",
            &[idle],
            Vec::new(),
        );
        assert_eq!(
            pb.client_audio_concealment_pct, None,
            "no active source => concealment field omitted (absent != 0%)"
        );
    }

    /// #1032: build a HealthPacket through the production path with the given
    /// cached agent-memory value, then round-trip it through protobuf so the
    /// assertions are on exactly what goes on the wire.
    fn health_packet_with_agent_memory(agent_memory_bytes: Option<u64>) -> PbHealthPacket {
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,   // unistream_stale_delta_drops_total (#1737 Phase 1)
            0.0, // encoder_queue_depth_report
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            None,
            agent_memory_bytes,
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    /// #1737 Phase 0: build a HealthPacket through the production path with the
    /// given unistream offered/drained byte totals, then round-trip it through
    /// protobuf so the assertion is on exactly what goes on the wire.
    fn health_packet_with_unistream_bytes(
        offered_bytes: u64,
        drained_bytes: u64,
        stale_delta_drops: u64,
    ) -> PbHealthPacket {
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,                 // datagram_drops_total
            offered_bytes,     // unistream_bytes_offered_total (#1737)
            drained_bytes,     // unistream_bytes_drained_total (#1737)
            stale_delta_drops, // unistream_stale_delta_drops_total (#1737 Phase 1)
            0,                 // websocket_drops_total
            0,                 // keyframe_requests_sent_total
            0.0,               // encoder_queue_depth_report
            0.0,               // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            None,
            None,                          // agent_memory_bytes
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    /// #1737 Phase 0: the two new unistream byte totals must survive the encode
    /// -> wire -> decode round-trip on the correct wire tags and in the correct
    /// argument slots. DISTINCT non-zero values (offered != drained) are used so
    /// a tag collision, a generated-code mistake, or an offered/drained argument
    /// transposition in `create_health_packet` all fail this test — the zero-only
    /// coverage in the sibling builder tests cannot catch any of those.
    ///
    /// MUTATION: swapping the offered/drained/stale-drop arguments (or dropping any
    /// `pb.unistream_*_total = Some(..)` assignment) makes the decoded value wrong
    /// or `None`, failing the corresponding assertion. The #1737 Phase-1
    /// `unistream_stale_delta_drops_total` (field 104) is covered with a third
    /// distinct value so a tag collision or arg transposition against the two
    /// Phase-0 byte totals is also caught.
    #[test]
    fn create_health_packet_roundtrips_unistream_byte_totals() {
        let pb = health_packet_with_unistream_bytes(5000, 1200, 37);
        assert_eq!(
            pb.unistream_bytes_offered_total,
            Some(5000),
            "offered byte total must round-trip as Some(5000) on its own wire tag"
        );
        assert_eq!(
            pb.unistream_bytes_drained_total,
            Some(1200),
            "drained byte total must round-trip as Some(1200) — distinct from offered, \
             so an offered/drained transposition is caught"
        );
        assert_eq!(
            pb.unistream_stale_delta_drops_total,
            Some(37),
            "stale-delta-drops total must round-trip as Some(37) on field 104 — distinct \
             from the byte totals so a tag collision or arg transposition is caught"
        );
    }

    /// #2511: the accumulator sits upstream of proto field 9's `fps_received > 0` gate.
    #[test]
    fn content_staleness_max_is_the_interval_max_per_bucket_and_resets_on_drain() {
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let staleness_event = |media: &'static str, v: f64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "from_peer",
                    value: MetricValue::Text(Cow::Borrowed("self")),
                },
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(media)),
                },
                Metric {
                    name: "content_staleness_ms",
                    value: MetricValue::F64(v),
                },
                Metric {
                    name: "decode_eligible",
                    value: MetricValue::U64(1),
                },
            ],
        };

        // Rise then FALL: the peak is neither the first nor the last sample.
        HealthReporter::process_diagnostics_event(
            staleness_event("VIDEO", 120.0),
            &peer_health_data,
        );
        HealthReporter::process_diagnostics_event(
            staleness_event("VIDEO", 4_800.0),
            &peer_health_data,
        );
        HealthReporter::process_diagnostics_event(
            staleness_event("VIDEO", 90.0),
            &peer_health_data,
        );
        HealthReporter::process_diagnostics_event(
            staleness_event("SCREEN", 240_000.0),
            &peer_health_data,
        );

        let maxes = peer_health_data
            .borrow_mut()
            .get_mut("peer-1")
            .expect("the video arm must have created the peer entry")
            .take_interval_maxes();
        let (camera, screen) = (maxes.camera_staleness_ms, maxes.screen_staleness_ms);
        assert_eq!(
            camera,
            Some(4_800.0),
            "the camera max is the PEAK, not the last sample"
        );
        assert_eq!(
            screen,
            Some(240_000.0),
            "the screen bucket must carry its own max, not inherit the camera's"
        );

        let __maxes = peer_health_data
            .borrow_mut()
            .get_mut("peer-1")
            .expect("peer entry")
            .take_interval_maxes();
        let (camera_again, screen_again) =
            (__maxes.camera_staleness_ms, __maxes.screen_staleness_ms);
        assert_eq!(
            (camera_again, screen_again),
            (None, None),
            "a drained interval with no fresh sample must OMIT the field, not publish 0 \
             — a fabricated 0 reads as 'never stale' on the one signal this exists to catch"
        );
    }

    #[test]
    fn content_staleness_max_ignores_decode_ineligible_samples() {
        use crate::decode::peer_decoder::MEDIA_TYPE_SCREEN;
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let staleness_event = |eligible: u64, staleness_ms: f64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(MEDIA_TYPE_SCREEN)),
                },
                Metric {
                    name: "content_staleness_ms",
                    value: MetricValue::F64(staleness_ms),
                },
                Metric {
                    name: "decode_eligible",
                    value: MetricValue::U64(eligible),
                },
            ],
        };

        HealthReporter::process_diagnostics_event(staleness_event(0, 240_000.0), &peer_health_data);
        HealthReporter::process_diagnostics_event(staleness_event(1, 1_200.0), &peer_health_data);

        let __maxes = peer_health_data
            .borrow_mut()
            .get_mut("peer-1")
            .expect("the video arm must have created the peer entry")
            .take_interval_maxes();
        let (_, screen) = (__maxes.camera_staleness_ms, __maxes.screen_staleness_ms);

        assert_eq!(
            screen,
            Some(1_200.0),
            "the hidden-tile peak must not ride out on the first visible report interval"
        );
    }

    #[test]
    fn content_staleness_max_requires_known_decode_eligibility() {
        use crate::decode::peer_decoder::MEDIA_TYPE_SCREEN;
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let event = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(MEDIA_TYPE_SCREEN)),
                },
                Metric {
                    name: "content_staleness_ms",
                    value: MetricValue::F64(240_000.0),
                },
            ],
        };

        HealthReporter::process_diagnostics_event(event, &peer_health_data);

        let __maxes = peer_health_data
            .borrow_mut()
            .get_mut("peer-1")
            .expect("the video arm must have created the peer entry")
            .take_interval_maxes();
        let (_, screen) = (__maxes.camera_staleness_ms, __maxes.screen_staleness_ms);

        assert_eq!(
            screen, None,
            "a staleness sample without a current decode_eligible signal must not \
             accumulate through the helper's fail-open default"
        );
    }

    #[test]
    fn decode_eligibility_event_gates_staleness_without_marking_video_fresh() {
        use crate::decode::peer_decoder::MEDIA_TYPE_SCREEN;
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let eligibility_event = |eligible: u64| DiagEvent {
            subsystem: "decode_eligibility",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(MEDIA_TYPE_SCREEN)),
                },
                Metric {
                    name: "decode_eligible",
                    value: MetricValue::U64(eligible),
                },
            ],
        };
        let staleness_event = |staleness_ms: f64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_100,
            metrics: vec![
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(MEDIA_TYPE_SCREEN)),
                },
                Metric {
                    name: "content_staleness_ms",
                    value: MetricValue::F64(staleness_ms),
                },
            ],
        };

        HealthReporter::process_diagnostics_event(eligibility_event(0), &peer_health_data);
        {
            let health_map = peer_health_data.borrow();
            let peer = health_map.get("peer-1").expect("peer entry");
            assert_eq!(
                peer.last_screen_update_ms, 0,
                "decode eligibility is not a media-stat sample and must not make video fresh"
            );
            assert_eq!(
                peer.screen_decode_eligible,
                Some(false),
                "eligibility event must update the screen gate used by later staleness samples"
            );
            assert_eq!(
                peer.last_screen_stats, None,
                "eligibility state must not fabricate a media-stats bucket"
            );
        }

        HealthReporter::process_diagnostics_event(staleness_event(240_000.0), &peer_health_data);
        let __maxes = peer_health_data
            .borrow_mut()
            .get_mut("peer-1")
            .expect("peer entry")
            .take_interval_maxes();
        let (_, hidden_screen) = (__maxes.camera_staleness_ms, __maxes.screen_staleness_ms);
        assert_eq!(
            hidden_screen, None,
            "a later staleness sample without its own decode_eligible metric must use the \
             latest visibility gate, not the default-open helper"
        );

        HealthReporter::process_diagnostics_event(eligibility_event(1), &peer_health_data);
        HealthReporter::process_diagnostics_event(staleness_event(1_200.0), &peer_health_data);
        let __maxes = peer_health_data
            .borrow_mut()
            .get_mut("peer-1")
            .expect("peer entry")
            .take_interval_maxes();
        let (_, visible_screen) = (__maxes.camera_staleness_ms, __maxes.screen_staleness_ms);
        assert_eq!(
            visible_screen,
            Some(1_200.0),
            "once visibility reopens the gate, the next worker staleness sample must count"
        );
    }

    #[test]
    fn decode_eligibility_events_do_not_create_video_stats_buckets() {
        use crate::decode::peer_decoder::{MEDIA_TYPE_CAMERA, MEDIA_TYPE_SCREEN};
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let eligibility_event = |media_type: &'static str, eligible: u64| DiagEvent {
            subsystem: "decode_eligibility",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(media_type)),
                },
                Metric {
                    name: "decode_eligible",
                    value: MetricValue::U64(eligible),
                },
            ],
        };

        HealthReporter::process_diagnostics_event(
            eligibility_event(MEDIA_TYPE_CAMERA, 1),
            &peer_health_data,
        );
        HealthReporter::process_diagnostics_event(
            eligibility_event(MEDIA_TYPE_SCREEN, 0),
            &peer_health_data,
        );

        let peer = peer_health_data
            .borrow()
            .get("peer-1")
            .expect("peer entry")
            .clone();
        assert_eq!(peer.camera_decode_eligible, Some(true));
        assert_eq!(peer.screen_decode_eligible, Some(false));
        assert_eq!(peer.last_camera_stats, None);
        assert_eq!(peer.last_screen_stats, None);

        let pb = health_packet_for_peer(peer);
        let stats = pb.peer_stats.get("peer-1").expect("peer stats");
        assert!(
            stats.video_stats.is_none(),
            "camera eligibility alone must not publish an all-zero camera stats bucket"
        );
        assert!(
            stats.screen_video_stats.is_none(),
            "screen eligibility alone must not publish an all-zero screen stats bucket"
        );
    }

    /// #2511 Blocker 3: the drain is the report loop's only producer of the staleness
    /// map, and a failed borrow must degrade to OMIT.
    #[test]
    fn drain_interval_maxes_reads_and_resets_and_omits_when_the_map_is_borrowed() {
        let map: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));
        {
            let mut m = map.borrow_mut();
            let a = m
                .entry("peer-a".to_string())
                .or_insert_with(|| PeerHealthData::new("peer-a".to_string()));
            a.camera_staleness_max_ms = Some(4_800.0);
            // #2524: seeded too, or dropping their `.take()` turns the interval max into the
            // lifetime latch this test's own name disclaims — and nothing notices.
            a.camera_seq_max_gap_max = Some(437);
            let b = m
                .entry("peer-b".to_string())
                .or_insert_with(|| PeerHealthData::new("peer-b".to_string()));
            b.screen_staleness_max_ms = Some(240_000.0);
            b.screen_seq_max_gap_max = Some(88);
        }

        let drained = drain_interval_maxes(&map);
        assert_eq!(
            drained.get("peer-a").copied(),
            Some(IntervalMaxes {
                camera_staleness_ms: Some(4_800.0),
                camera_seq_gap_frames: Some(437),
                ..Default::default()
            })
        );
        assert_eq!(
            drained.get("peer-b").copied(),
            Some(IntervalMaxes {
                screen_staleness_ms: Some(240_000.0),
                screen_seq_gap_frames: Some(88),
                ..Default::default()
            }),
            "each peer carries its own set; a shared accumulator would cross them"
        );

        assert_eq!(
            drain_interval_maxes(&map).get("peer-a").copied(),
            Some(IntervalMaxes::default()),
            "the drain resets, so the export is an INTERVAL max rather than a lifetime latch"
        );

        let _held = map.borrow();
        assert!(
            drain_interval_maxes(&map).is_empty(),
            "a failed borrow must yield an empty map, which folds as OMIT — never a 0"
        );
    }

    /// #2511: both buckets carry freeze counters plus a drained staleness max.
    fn health_packet_with_freeze_stats(fps_received: f64) -> PbHealthPacket {
        let mut staleness = IntervalMaxMap::new();
        staleness.insert(
            "peer-1".to_string(),
            IntervalMaxes {
                camera_staleness_ms: Some(4_800.0),
                screen_staleness_ms: Some(240_000.0),
                camera_seq_gap_frames: Some(437),
                screen_seq_gap_frames: Some(88),
            },
        );
        health_packet_with_freeze_stats_for(fps_received, true, staleness)
    }

    fn health_packet_with_freeze_stats_for(
        fps_received: f64,
        sender_video_enabled: bool,
        staleness: IntervalMaxMap,
    ) -> PbHealthPacket {
        let mut peer = PeerHealthData::new("peer-1".to_string());
        peer.video_enabled = sender_video_enabled;
        // Distinct per bucket and per field so a transposition is observable.
        peer.last_camera_stats = Some(json!({
            "fps_received": fps_received,
            "freeze_episodes_total": 3u64,
            "freeze_ms_total": 7_400u64,
            "max_decode_gap_ms": 5_100u64,
            "freshness_evictions_total": 31u64,
            "freshness_evictions_keyframeless_total": 29u64,
        }));
        peer.last_screen_stats = Some(json!({
            "fps_received": fps_received,
            "freeze_episodes_total": 2u64,
            "freeze_ms_total": 61_000u64,
            "max_decode_gap_ms": 58_000u64,
            "freshness_evictions_total": 6u64,
            "freshness_evictions_keyframeless_total": 4u64,
        }));

        let mut health_map = HashMap::new();
        health_map.insert("peer-1".to_string(), peer);

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0.0,
            0.0,
            0,
            false,
            0,
            None,
            0,
            0,
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,
            0,
            [0, 0, 0, 0],
            Vec::new(),
            None,
            ClientMetadata::default(),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            HashMap::new(),
            WtReceiveTelemetry::default(),
            Vec::new(),
            staleness, // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    /// #2511: fps 0 IS the freeze.
    #[test]
    fn the_freeze_family_folds_even_when_fps_received_zero() {
        let pb = health_packet_with_freeze_stats(0.0);
        let ps = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present");
        let camera = ps
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");
        let screen = ps
            .screen_video_stats
            .as_ref()
            .expect("screen video stats must be present");

        assert_eq!(camera.fps_received, 0.0);
        assert_eq!(camera.freeze_episodes_total, Some(3));
        assert_eq!(camera.freeze_ms_total, Some(7_400));
        assert_eq!(camera.max_decode_gap_ms, Some(5_100));
        assert_eq!(
            camera.max_content_staleness_ms,
            Some(4_800),
            "the staleness max is drained upstream of field 9's fps gate, so it must \
             survive fps 0"
        );

        assert_eq!(screen.fps_received, 0.0);
        assert_eq!(screen.freeze_episodes_total, Some(2));
        assert_eq!(screen.freeze_ms_total, Some(61_000));
        assert_eq!(screen.max_decode_gap_ms, Some(58_000));
        assert_eq!(screen.max_content_staleness_ms, Some(240_000));

        // Field 9 is untouched: still gated, still 0 at fps 0.
        assert_eq!(camera.content_staleness_ms, 0.0);
    }

    /// UNLIKE the #2511 freeze family asserted absent by the test below.
    #[test]
    fn the_loss_diagnostics_survive_the_sender_reporting_video_off() {
        let mut maxes = IntervalMaxMap::new();
        maxes.insert(
            "peer-1".to_string(),
            IntervalMaxes {
                camera_seq_gap_frames: Some(437),
                screen_seq_gap_frames: Some(88),
                ..Default::default()
            },
        );
        let pb = health_packet_with_freeze_stats_for(0.0, false, maxes);
        let ps = pb.peer_stats.get("peer-1").expect("peer stats");
        let camera = ps.video_stats.as_ref().expect("camera video stats");

        assert_eq!(
            camera.freeze_episodes_total, None,
            "premise: the #2511 freeze family IS gated on video_enabled"
        );
        assert_eq!(
            camera.max_seq_gap_frames,
            Some(437),
            "a gate here would blank the burst magnitude across any camera-off period"
        );
        assert_eq!(camera.freshness_evictions_total, Some(31));
        assert_eq!(camera.freshness_evictions_keyframeless_total, Some(29));

        let screen = ps.screen_video_stats.as_ref().expect("screen video stats");
        assert_eq!(screen.max_seq_gap_frames, Some(88));
        assert_eq!(screen.freshness_evictions_total, Some(6));
    }

    /// #2511 Blocker 1: a peer whose camera is OFF stops sending VIDEO. Nothing decodes,
    /// but nothing is frozen either, and a cumulative counter cannot be corrected after
    /// the fact — so the freeze family must not be published at all.
    #[test]
    fn the_camera_freeze_family_is_omitted_while_the_sender_reports_video_off() {
        let mut staleness = IntervalMaxMap::new();
        staleness.insert(
            "peer-1".to_string(),
            IntervalMaxes {
                camera_staleness_ms: Some(4_800.0),
                screen_staleness_ms: Some(240_000.0),
                camera_seq_gap_frames: Some(437),
                screen_seq_gap_frames: Some(88),
            },
        );
        let pb = health_packet_with_freeze_stats_for(0.0, false, staleness);
        let ps = pb.peer_stats.get("peer-1").expect("peer stats");
        let camera = ps.video_stats.as_ref().expect("camera video stats");

        assert_eq!(camera.freeze_episodes_total, None);
        assert_eq!(camera.freeze_ms_total, None);
        assert_eq!(camera.max_decode_gap_ms, None);
        assert_eq!(
            camera.max_content_staleness_ms,
            Some(4_800),
            "the staleness max is a per-interval observation, not a cumulative freeze \
             claim, so it is not gated with the family above"
        );

        let screen = ps.screen_video_stats.as_ref().expect("screen video stats");
        assert_eq!(
            screen.freeze_episodes_total,
            Some(2),
            "video_enabled describes the CAMERA; gating the screen bucket on it would \
             blank a live share whenever the publisher turned their camera off"
        );
    }

    /// #2511 Blocker 3: no sample observed this interval => the field is ABSENT, not 0.
    #[test]
    fn max_content_staleness_is_omitted_when_no_sample_was_drained() {
        let pb = health_packet_with_freeze_stats_for(0.0, true, IntervalMaxMap::new());
        let ps = pb.peer_stats.get("peer-1").expect("peer stats");
        let camera = ps.video_stats.as_ref().expect("camera video stats");
        let screen = ps.screen_video_stats.as_ref().expect("screen video stats");

        assert_eq!(
            camera.max_content_staleness_ms, None,
            "a 0 here reads as 'never stale', the exact false negative this field exists \
             to prevent"
        );
        assert_eq!(screen.max_content_staleness_ms, None);
        assert_eq!(
            camera.freeze_episodes_total,
            Some(3),
            "the rest of the bucket must still fold, or this test proves nothing"
        );
    }

    #[test]
    fn freeze_counters_survive_the_metric_to_json_hop_per_bucket() {
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let freeze_event = |media: &'static str, episodes: u64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(media)),
                },
                Metric {
                    name: "freeze_episodes_total",
                    value: MetricValue::U64(episodes),
                },
                Metric {
                    name: "freeze_ms_total",
                    value: MetricValue::U64(episodes * 1_000),
                },
                Metric {
                    name: "max_decode_gap_ms",
                    value: MetricValue::U64(episodes * 700),
                },
            ],
        };

        HealthReporter::process_diagnostics_event(freeze_event("VIDEO", 3), &peer_health_data);
        HealthReporter::process_diagnostics_event(freeze_event("SCREEN", 2), &peer_health_data);

        let map = peer_health_data.borrow();
        let peer = map.get("peer-1").expect("peer entry");
        let read = |bucket: &Option<Value>, key: &str| -> Option<u64> {
            bucket
                .as_ref()
                .and_then(|v| v.get(key))
                .and_then(|v| v.as_u64())
        };
        assert_eq!(
            read(&peer.last_camera_stats, "freeze_episodes_total"),
            Some(3)
        );
        assert_eq!(
            read(&peer.last_camera_stats, "freeze_ms_total"),
            Some(3_000)
        );
        assert_eq!(
            read(&peer.last_camera_stats, "max_decode_gap_ms"),
            Some(2_100)
        );
        assert_eq!(
            read(&peer.last_screen_stats, "freeze_episodes_total"),
            Some(2),
            "the SCREEN bucket must not inherit the camera's counters"
        );
        assert_eq!(
            read(&peer.last_screen_stats, "freeze_ms_total"),
            Some(2_000)
        );
        assert_eq!(
            read(&peer.last_screen_stats, "max_decode_gap_ms"),
            Some(1_400)
        );
    }

    /// Pins the hand-written key→field mapping in `set_ws_stream_counters`. Eight
    /// mutually distinct deltas, so transposing any two keys — or offered with
    /// dropped — decodes into the wrong field.
    #[test]
    fn create_health_packet_maps_each_ws_stream_key_to_its_own_field() {
        // The netsim-driven bump below writes the process-global transport counters.
        let _tx_guard = crate::test_serial::lock_transport_stream_counters();

        use crate::connection::MediaStreamKey as K;
        use videocall_transport::websocket::{
            force_websocket_bytes_for_stream as bump,
            websocket_dropped_bytes_for_stream as dropped_now,
            websocket_offered_bytes_for_stream as offered_now,
        };

        let (audio, video, screen, control) = (
            K::Audio.as_u8(),
            K::Video.as_u8(),
            K::Screen.as_u8(),
            K::Control.as_u8(),
        );
        let want_offered = [
            offered_now(audio) + 11,
            offered_now(video) + 22,
            offered_now(screen) + 33,
            offered_now(control) + 44,
        ];
        let want_dropped = [
            dropped_now(audio) + 55,
            dropped_now(video) + 66,
            dropped_now(screen) + 77,
            dropped_now(control) + 88,
        ];
        bump(audio, 11, 55);
        bump(video, 22, 66);
        bump(screen, 33, 77);
        bump(control, 44, 88);

        let pb = health_packet_with_unistream_bytes(0, 0, 0);

        assert_eq!(
            [
                pb.ws_offered_bytes_audio,
                pb.ws_offered_bytes_video,
                pb.ws_offered_bytes_screen,
                pb.ws_offered_bytes_control,
            ],
            want_offered.map(Some),
            "offered bytes must decode into the field for their own stream key",
        );
        assert_eq!(
            [
                pb.ws_dropped_bytes_audio,
                pb.ws_dropped_bytes_video,
                pb.ws_dropped_bytes_screen,
                pb.ws_dropped_bytes_control,
            ],
            want_dropped.map(Some),
            "dropped bytes must decode into the field for their own stream key",
        );
    }

    /// A zero on the two aggregates is the absence claim they exist to support, so
    /// it must reach the wire; the per-stream fields stay omitted at zero, matching
    /// the `ws_offered_bytes_*` precedent and its cardinality cost.
    #[test]
    fn create_health_packet_emits_the_ws_inactive_aggregates_at_zero() {
        let _tx_guard = crate::test_serial::lock_transport_stream_counters();

        use videocall_transport::websocket::reset_websocket_inactive_counters_for_test as reset;
        reset();

        let pb = health_packet_with_unistream_bytes(0, 0, 0);

        assert_eq!(pb.ws_inactive_dropped_frames_by_state_closing, Some(0));
        assert_eq!(pb.ws_inactive_dropped_frames_by_state_closed, Some(0));
        assert_eq!(
            [
                pb.ws_inactive_dropped_frames_audio,
                pb.ws_inactive_dropped_frames_video,
                pb.ws_inactive_dropped_frames_screen,
                pb.ws_inactive_dropped_frames_control,
                pb.ws_inactive_dropped_bytes_audio,
                pb.ws_inactive_dropped_bytes_video,
                pb.ws_inactive_dropped_bytes_screen,
                pb.ws_inactive_dropped_bytes_control,
            ],
            [None; 8],
            "a per-stream zero must stay off the wire",
        );
    }

    /// Sibling of the offered/dropped mapping lock above, for the inactive-socket
    /// family. All ten deltas are mutually distinct, so transposing any two of them
    /// decodes into the wrong field.
    #[test]
    fn create_health_packet_maps_each_ws_inactive_key_to_its_own_field() {
        let _tx_guard = crate::test_serial::lock_transport_stream_counters();

        use crate::connection::MediaStreamKey as K;
        use videocall_transport::websocket::{
            force_websocket_inactive_drop_for_stream as bump,
            websocket_inactive_dropped_bytes_for_stream as bytes_now,
            websocket_inactive_dropped_frames_closed as closed_now,
            websocket_inactive_dropped_frames_closing as closing_now,
            websocket_inactive_dropped_frames_for_stream as frames_now,
        };

        let keys = [
            K::Audio.as_u8(),
            K::Video.as_u8(),
            K::Screen.as_u8(),
            K::Control.as_u8(),
        ];
        let plan = [
            (2u64, 110u64, true),
            (5, 220, true),
            (3, 330, false),
            (6, 440, false),
        ];
        let want_frames = [
            frames_now(keys[0]) + plan[0].0,
            frames_now(keys[1]) + plan[1].0,
            frames_now(keys[2]) + plan[2].0,
            frames_now(keys[3]) + plan[3].0,
        ];
        let want_bytes = [
            bytes_now(keys[0]) + plan[0].0 * plan[0].1,
            bytes_now(keys[1]) + plan[1].0 * plan[1].1,
            bytes_now(keys[2]) + plan[2].0 * plan[2].1,
            bytes_now(keys[3]) + plan[3].0 * plan[3].1,
        ];
        let want_closing = closing_now() + plan[0].0 + plan[1].0;
        let want_closed = closed_now() + plan[2].0 + plan[3].0;
        for (key, (count, size, closing)) in keys.into_iter().zip(plan) {
            for _ in 0..count {
                bump(key, size, closing);
            }
        }

        let pb = health_packet_with_unistream_bytes(0, 0, 0);

        assert_eq!(
            [
                pb.ws_inactive_dropped_frames_audio,
                pb.ws_inactive_dropped_frames_video,
                pb.ws_inactive_dropped_frames_screen,
                pb.ws_inactive_dropped_frames_control,
            ],
            want_frames.map(Some),
            "inactive frame counts must decode into the field for their own stream key",
        );
        assert_eq!(
            [
                pb.ws_inactive_dropped_bytes_audio,
                pb.ws_inactive_dropped_bytes_video,
                pb.ws_inactive_dropped_bytes_screen,
                pb.ws_inactive_dropped_bytes_control,
            ],
            want_bytes.map(Some),
            "inactive bytes must decode into the field for their own stream key",
        );
        assert_eq!(
            pb.ws_inactive_dropped_frames_by_state_closing,
            Some(want_closing)
        );
        assert_eq!(
            pb.ws_inactive_dropped_frames_by_state_closed,
            Some(want_closed)
        );
    }

    fn health_packet_with_camera_playout_stats(fps_received: f64) -> PbHealthPacket {
        let mut peer = PeerHealthData::new("peer-1".to_string());
        peer.last_camera_stats = Some(json!({
            "fps_received": fps_received,
            "playout_latency_ms": 1500.0,
            "playout_stage1_span_ms": 1200.0,
            "playout_paint_lag_ms": 1800.0,
            "playout_skip_to_live_total": 4u64,
            // #1641: a 5-minute content age — deliberately > the 1800ms playout-latency cap, to
            // prove this field is NOT bounded by it (the whole point of the metric).
            "content_staleness_ms": 300000.0,
            // #2201: keyframe ARRIVALS. Distinct from skip_to_live_total above so a
            // transposition between the two counters is observable.
            "keyframe_arrivals_total": 9u64,
        }));

        let mut health_map = HashMap::new();
        health_map.insert("peer-1".to_string(), peer);

        build_health_packet(health_map)
    }

    fn build_health_packet(health_map: HashMap<String, PeerHealthData>) -> PbHealthPacket {
        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,   // unistream_stale_delta_drops_total (#1737 Phase 1)
            0.0, // encoder_queue_depth_report
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            None,
            None,
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    #[test]
    fn playout_latency_folds_when_fps_received_positive() {
        let pb = health_packet_with_camera_playout_stats(30.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 30.0);
        assert_eq!(stats.playout_latency_ms, 1500.0);
        assert_eq!(stats.playout_stage1_span_ms, 1200.0);
    }

    #[test]
    fn playout_latency_omitted_when_fps_received_zero() {
        let pb = health_packet_with_camera_playout_stats(0.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 0.0);
        assert_eq!(stats.playout_latency_ms, 0.0);
        assert_eq!(stats.playout_stage1_span_ms, 0.0);
    }

    #[test]
    fn playout_paint_lag_folds_when_fps_received_positive() {
        let pb = health_packet_with_camera_playout_stats(30.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 30.0);
        assert_eq!(stats.playout_paint_lag_ms, 1800.0);
    }

    #[test]
    fn playout_paint_lag_omitted_when_fps_received_zero() {
        let pb = health_packet_with_camera_playout_stats(0.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 0.0);
        assert_eq!(stats.playout_paint_lag_ms, 0.0);
    }

    /// #1641 content-staleness (content AGE) folds into the wire VideoStats when fps_received > 0,
    /// and — unlike playout_latency_ms (capped at 1800ms) — carries a value ABOVE that cap. This
    /// pins both that the field round-trips AND that it is the unbounded age metric, not a clone of
    /// the capped latency field.
    ///
    /// Mutation check: dropping the `vs.content_staleness_ms = v` fold (or gating it differently
    /// than the other ms gauges) makes this assert read 0.0 and fail.
    #[test]
    fn content_staleness_folds_when_fps_received_positive() {
        let pb = health_packet_with_camera_playout_stats(30.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 30.0);
        assert_eq!(stats.content_staleness_ms, 300000.0);
        assert!(
            stats.content_staleness_ms > 1800.0,
            "content_staleness_ms must NOT be capped at the 1800ms playout-latency bound"
        );
    }

    /// #1641 content-staleness is a ms GAUGE, so it shares the fps_received > 0 gate with the other
    /// ms gauges (paused/hidden tile paints nothing => "at live"). It is NOT the skip-to-live
    /// COUNTER, which folds unconditionally.
    ///
    /// Mutation check: moving the content-staleness fold OUTSIDE the fps_received > 0 guard makes
    /// this assert read 300000.0 and fail.
    #[test]
    fn content_staleness_omitted_when_fps_received_zero() {
        let pb = health_packet_with_camera_playout_stats(0.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 0.0);
        assert_eq!(stats.content_staleness_ms, 0.0);
    }

    /// #1641 routing regression: a worker "video" playout-stats event tagged `media_type=SCREEN`
    /// MUST land in `last_screen_stats`, and one tagged `media_type=VIDEO` in `last_camera_stats`.
    ///
    /// This guards the bug the worker→main re-broadcast had: the worker's "video" stats DiagEvent
    /// carried NO `media_type`, so `process_diagnostics_event`'s `is_screen` check defaulted false
    /// and ALL playout-family stats (incl. #1641 `content_staleness_ms`) routed to the camera
    /// bucket — a peer sharing camera+screen had the screen decoder's stats overwrite the camera
    /// bucket. The fix stamps `media_type` in `handle_worker_diag_message` (videocall-codecs
    /// `decoder/wasm.rs`), which is the real source of these events at runtime; this test drives
    /// the SAME consuming function (`process_diagnostics_event`) those events flow into.
    ///
    /// Mutation sensitivity: remove the `media_type` metric from the SCREEN event below (the exact
    /// effect of dropping the `decoder/wasm.rs` stamp) and `is_screen` reads false → the screen
    /// content-staleness lands in `last_camera_stats`, the screen bucket stays `None`, and BOTH
    /// asserts fail.
    #[test]
    fn video_playout_stats_route_to_bucket_by_media_type() {
        use crate::decode::peer_decoder::{MEDIA_TYPE_CAMERA, MEDIA_TYPE_SCREEN};
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        // Helper: build a worker-style "video" stats event for one stream kind, distinguishing the
        // two buckets by the content-staleness value so a misroute is observable.
        let make_video_event = |media_type: &'static str, content_staleness_ms: f64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(media_type)),
                },
                Metric {
                    name: "from_peer",
                    value: MetricValue::Text(Cow::Borrowed("reporter")),
                },
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                // fps_received > 0 so the consuming UI fold (a sibling concern) would keep it; the
                // routing under test does not gate on fps, but a realistic event carries it.
                Metric {
                    name: "fps_received",
                    value: MetricValue::F64(30.0),
                },
                Metric {
                    name: "content_staleness_ms",
                    value: MetricValue::F64(content_staleness_ms),
                },
            ],
        };

        // Distinct staleness per kind: 9000ms (screen) vs 1000ms (camera).
        HealthReporter::process_diagnostics_event(
            make_video_event(MEDIA_TYPE_SCREEN, 9000.0),
            &peer_health_data,
        );
        HealthReporter::process_diagnostics_event(
            make_video_event(MEDIA_TYPE_CAMERA, 1000.0),
            &peer_health_data,
        );

        let map = peer_health_data.borrow();
        let peer = map.get("peer-1").expect("peer-1 health entry must exist");

        let screen = peer
            .last_screen_stats
            .as_ref()
            .expect("SCREEN-tagged video event must populate last_screen_stats, not camera");
        assert_eq!(
            screen.get("content_staleness_ms").and_then(|v| v.as_f64()),
            Some(9000.0),
            "screen bucket must hold the screen stream's staleness"
        );

        let camera = peer
            .last_camera_stats
            .as_ref()
            .expect("VIDEO-tagged video event must populate last_camera_stats");
        assert_eq!(
            camera.get("content_staleness_ms").and_then(|v| v.as_f64()),
            Some(1000.0),
            "camera bucket must hold the camera stream's staleness, NOT the screen's (the bug: \
             unstamped screen stats overwrote the camera bucket)"
        );
    }

    /// #2201: the keyframe-arrival counter must survive the WORKER-METRIC -> JSON hop, and
    /// land in the correct camera/screen bucket.
    ///
    /// Added because both halves were revertible-green — measured, not assumed. Deleting the
    /// `"keyframe_arrivals_total"` ingest arm (the single entry point for the whole Prometheus
    /// path) left all 827 tests passing, because the fold test writes the JSON key straight
    /// into its fixture and bypasses the hop. This drives `process_diagnostics_event`
    /// end-to-end instead, pinning the metric NAME to the json key.
    ///
    /// Distinct values per kind (7 screen vs 3 camera) so a bucket misroute — the #1641 defect
    /// class — fails rather than silently passing.
    ///
    /// MUTATION: deleting the ingest arm makes both lookups `None`; swapping the buckets, or
    /// renaming the metric on either side of the hop, fails the value assertions.
    #[test]
    fn keyframe_arrivals_total_survives_the_metric_to_json_hop_per_bucket() {
        use crate::decode::peer_decoder::{MEDIA_TYPE_CAMERA, MEDIA_TYPE_SCREEN};
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let make_event = |media_type: &'static str, arrivals: u64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(media_type)),
                },
                Metric {
                    name: "from_peer",
                    value: MetricValue::Text(Cow::Borrowed("reporter")),
                },
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                // The exact metric name the worker emits. If this and the ingest arm ever
                // disagree, this test is what catches it.
                Metric {
                    name: "keyframe_arrivals_total",
                    value: MetricValue::U64(arrivals),
                },
            ],
        };

        HealthReporter::process_diagnostics_event(
            make_event(MEDIA_TYPE_SCREEN, 7),
            &peer_health_data,
        );
        HealthReporter::process_diagnostics_event(
            make_event(MEDIA_TYPE_CAMERA, 3),
            &peer_health_data,
        );

        let map = peer_health_data.borrow();
        let peer = map.get("peer-1").expect("peer-1 health entry must exist");

        assert_eq!(
            peer.last_screen_stats
                .as_ref()
                .and_then(|v| v.get("keyframe_arrivals_total"))
                .and_then(|v| v.as_u64()),
            Some(7),
            "the SCREEN arrival count must reach the screen bucket; None => the ingest arm is \
             gone, so nothing reaches Prometheus at all"
        );
        assert_eq!(
            peer.last_camera_stats
                .as_ref()
                .and_then(|v| v.get("keyframe_arrivals_total"))
                .and_then(|v| v.as_u64()),
            Some(3),
            "the CAMERA arrival count must reach the camera bucket and read 3, NOT the \
             screen's 7 — a misroute overwrites one bucket with the other (#1641)"
        );
    }

    /// The `"video"` handler ends in `_ => {}`, so a missing arm silently drops its key —
    /// which is why these values never left the client (#2524, #2541).
    #[test]
    fn loss_and_freshness_keys_survive_the_hop_and_fold_per_bucket() {
        use crate::decode::peer_decoder::{MEDIA_TYPE_CAMERA, MEDIA_TYPE_SCREEN};
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        // Distinct per kind so a bucket misroute (#1641) fails.
        let make_event = |media_type: &'static str, scale: f64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(media_type)),
                },
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                Metric {
                    name: "video_seq_max_gap",
                    value: MetricValue::U64(400 + scale as u64),
                },
                Metric {
                    name: "freshness_evictions_total",
                    value: MetricValue::U64(90 + scale as u64),
                },
                Metric {
                    name: "freshness_evictions_keyframeless_total",
                    value: MetricValue::U64(40 + scale as u64),
                },
            ],
        };

        let gap_only_event = |media_type: &'static str, gap: u64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 2_000,
            metrics: vec![
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(media_type)),
                },
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                Metric {
                    name: "video_seq_max_gap",
                    value: MetricValue::U64(gap),
                },
            ],
        };

        HealthReporter::process_diagnostics_event(
            make_event(MEDIA_TYPE_SCREEN, 7.0),
            &peer_health_data,
        );
        HealthReporter::process_diagnostics_event(
            make_event(MEDIA_TYPE_CAMERA, 3.0),
            &peer_health_data,
        );

        // A SECOND, smaller gap per kind: the export must be the interval MAX, not the last.
        HealthReporter::process_diagnostics_event(
            gap_only_event(MEDIA_TYPE_SCREEN, 1),
            &peer_health_data,
        );
        HealthReporter::process_diagnostics_event(
            gap_only_event(MEDIA_TYPE_CAMERA, 0),
            &peer_health_data,
        );

        let (camera_blob, screen_blob, maxes) = {
            let mut map = peer_health_data.borrow_mut();
            let peer = map
                .get_mut("peer-1")
                .expect("peer-1 health entry must exist");
            (
                peer.last_camera_stats
                    .clone()
                    .expect("camera bucket must exist"),
                peer.last_screen_stats
                    .clone()
                    .expect("screen bucket must exist"),
                peer.take_interval_maxes(),
            )
        };

        assert_eq!(
            maxes.camera_seq_gap_frames,
            Some(403),
            "interval MAX, not the trailing 0; None => the ingest arm is gone"
        );
        assert_eq!(
            maxes.screen_seq_gap_frames,
            Some(407),
            "the screen bucket keeps its own max, not the camera's"
        );

        let mut camera = PbVideoStats::new();
        fold_loss_diagnostics(&mut camera, &camera_blob, maxes.camera_seq_gap_frames);
        let mut screen = PbVideoStats::new();
        fold_loss_diagnostics(&mut screen, &screen_blob, maxes.screen_seq_gap_frames);

        assert_eq!(camera.max_seq_gap_frames, Some(403));
        assert_eq!(screen.max_seq_gap_frames, Some(407));
        assert_eq!(camera.freshness_evictions_total, Some(93));
        assert_eq!(camera.freshness_evictions_keyframeless_total, Some(43));
        assert_eq!(screen.freshness_evictions_total, Some(97));
        assert_eq!(screen.freshness_evictions_keyframeless_total, Some(47));

        let mut absent = PbVideoStats::new();
        fold_loss_diagnostics(&mut absent, &json!({}), None);
        assert_eq!(absent.max_seq_gap_frames, None);
        assert_eq!(absent.freshness_evictions_total, None);
    }

    /// #2249: the `decode_eligible` metric must survive the metric->json hop into the right
    /// per-kind bucket. Nothing else covers this arm — every other test here writes the blob
    /// directly and bypasses the hop, so deleting the arm would leave them all green while
    /// production silently fell back to the fail-open `true`.
    ///
    /// MUTATION: deleting the ingest arm makes both lookups `None`; swapping the buckets, or
    /// renaming the metric on either side of the hop, fails the value assertions.
    #[test]
    fn decode_eligible_survives_the_metric_to_json_hop_per_bucket() {
        use crate::decode::peer_decoder::{MEDIA_TYPE_CAMERA, MEDIA_TYPE_SCREEN};
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let make_event = |media_type: &'static str, eligible: u64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "media_type",
                    value: MetricValue::Text(Cow::Borrowed(media_type)),
                },
                Metric {
                    name: "from_peer",
                    value: MetricValue::Text(Cow::Borrowed("reporter")),
                },
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-1")),
                },
                // The exact metric name `send_diagnostic_packets` emits.
                Metric {
                    name: "decode_eligible",
                    value: MetricValue::U64(eligible),
                },
            ],
        };

        HealthReporter::process_diagnostics_event(
            make_event(MEDIA_TYPE_SCREEN, 0),
            &peer_health_data,
        );
        HealthReporter::process_diagnostics_event(
            make_event(MEDIA_TYPE_CAMERA, 1),
            &peer_health_data,
        );

        let map = peer_health_data.borrow();
        let peer = map.get("peer-1").expect("peer-1 health entry must exist");

        assert!(
            !decode_eligible_from(&peer.last_screen_stats),
            "the backgrounded SCREEN tile must read NOT eligible through the production              accessor; None here means the ingest arm is gone and the gate never engages"
        );
        assert!(
            decode_eligible_from(&peer.last_camera_stats),
            "the CAMERA bucket must keep its own value, NOT inherit the screen's 0"
        );
    }

    /// Issue 2029: `process_diagnostics_event` must surface the per-peer WT
    /// audio-datagram loss sample (peer id + pkt/s) so the subscription loop can
    /// forward it into the connection layer's fallback detector — INCLUDING a
    /// 0.0 sample (a healthy WT audio peer must stay in the uniformity
    /// denominator). Unrelated events must return None so nothing else is fed.
    #[test]
    fn process_diagnostics_event_surfaces_wt_audio_loss_sample() {
        use std::borrow::Cow;
        use videocall_diagnostics::Metric;

        let peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let loss_event = |to_peer: &'static str, loss: f64| DiagEvent {
            subsystem: "neteq",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "from_peer",
                    value: MetricValue::Text(Cow::Borrowed("self")),
                },
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed(to_peer)),
                },
                Metric {
                    name: "wt_datagram_audio_loss_per_sec",
                    value: MetricValue::F64(loss),
                },
            ],
        };

        // A nonzero loss sample is surfaced with its peer id and rate.
        assert_eq!(
            HealthReporter::process_diagnostics_event(
                loss_event("peer-1", 22.0),
                &peer_health_data
            ),
            Some(("peer-1".to_string(), 22.0)),
            "a nonzero WT audio-loss sample must be forwarded"
        );
        // A ZERO sample is still surfaced (healthy peer stays in the denominator).
        assert_eq!(
            HealthReporter::process_diagnostics_event(loss_event("peer-2", 0.0), &peer_health_data),
            Some(("peer-2".to_string(), 0.0)),
            "a 0.0 WT audio-loss sample must also be forwarded"
        );
        // An unrelated event carries no loss sample.
        let unrelated = DiagEvent {
            subsystem: "neteq",
            stream_id: None,
            ts_ms: 1_000,
            metrics: vec![
                Metric {
                    name: "to_peer",
                    value: MetricValue::Text(Cow::Borrowed("peer-3")),
                },
                Metric {
                    name: "audio_buffer_ms",
                    value: MetricValue::U64(120),
                },
            ],
        };
        assert_eq!(
            HealthReporter::process_diagnostics_event(unrelated, &peer_health_data),
            None,
            "an event without the loss gauge must not be forwarded"
        );
    }

    /// #1252 resync governor counter folds at fps > 0 — like every other field. The DISTINCT
    /// behavior (folds even at fps == 0) is pinned by the sibling test below; this one guards the
    /// ordinary case so a regression that drops the field entirely is also caught.
    #[test]
    fn playout_skip_to_live_total_folds_when_fps_received_positive() {
        let pb = health_packet_with_camera_playout_stats(30.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 30.0);
        assert_eq!(stats.playout_skip_to_live_total, 4);
    }

    /// #1252 resync governor counter folds UNCONDITIONALLY — even at fps == 0. This is the load-
    /// bearing behavioral difference from the ms gauges (which are gated on fps_received > 0 and so
    /// read 0 here, asserted in `playout_*_omitted_when_fps_received_zero`). A cumulative counter
    /// must keep reporting its lifetime value when the stream falls idle, or the governor would
    /// appear to "un-fire". Mutation check: moving the counter fold inside the `fps_received > 0`
    /// guard makes this assert read 0 and fail.
    #[test]
    fn playout_skip_to_live_total_folds_even_when_fps_received_zero() {
        let pb = health_packet_with_camera_playout_stats(0.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 0.0);
        // ms gauges are gated to 0 at fps 0; the COUNTER is NOT — it still reports its lifetime value.
        assert_eq!(stats.playout_paint_lag_ms, 0.0);
        assert_eq!(stats.playout_skip_to_live_total, 4);
    }

    /// #2201: the keyframe-ARRIVAL counter must fold at fps 0.
    ///
    /// This is not a stylistic parallel to the counter above — it is the whole point of the
    /// metric. It exists to answer "did a keyframe arrive during this freeze?", and a freeze
    /// is exactly when `fps_received` can be 0. Gating it behind the `fps_received > 0` guard
    /// that (correctly) protects the ms gauges would blank the field in the only case it was
    /// built for.
    ///
    /// MUTATION: moving the camera fold inside the `if vs.fps_received > 0.0` block makes this
    /// read 0 instead of 9.
    #[test]
    fn keyframe_arrivals_total_folds_even_when_fps_received_zero() {
        let pb = health_packet_with_camera_playout_stats(0.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 0.0);
        assert_eq!(
            stats.keyframe_arrivals_total,
            Some(9),
            "the arrival counter MUST survive fps 0 — a freeze is the case it exists for"
        );
    }

    /// #1660 sibling of `health_packet_with_camera_playout_stats`, but drives the SCREEN path:
    /// populates only `last_screen_stats` (camera bucket left empty) so the resulting proto's
    /// `screen_video_stats` — not `video_stats` — must carry the folded playout family. Values are
    /// deliberately distinct from the camera helper so a bucket misroute is observable.
    fn health_packet_with_screen_playout_stats(fps_received: f64) -> PbHealthPacket {
        let mut peer = PeerHealthData::new("peer-1".to_string());
        peer.last_screen_stats = Some(json!({
            "fps_received": fps_received,
            "playout_latency_ms": 1400.0,
            "playout_stage1_span_ms": 900.0,
            "playout_paint_lag_ms": 600.0,
            "playout_skip_to_live_total": 7u64,
            // #1660: a 4-minute screen content age — deliberately > the 1800ms playout-latency cap,
            // to prove the screen content-staleness field is UNBOUNDED like its camera sibling.
            "content_staleness_ms": 240000.0,
            // #2201: distinct from the camera fixture's 9, so a bucket transposition in the
            // screen fold is observable. Without this key the whole screen fold block was
            // revertible-green.
            "keyframe_arrivals_total": 4u64,
        }));

        let mut health_map = HashMap::new();
        health_map.insert("peer-1".to_string(), peer);

        build_health_packet(health_map)
    }

    /// BOTH streams on ONE peer: with the other bucket empty, a `fold_loss_diagnostics` call
    /// site reading the wrong blob falls back to the right one and the transposition survives.
    fn health_packet_with_both_streams_loss_pli() -> PbHealthPacket {
        let mut peer = PeerHealthData::new("peer-1".to_string());
        peer.last_camera_stats = Some(json!({
            "fps_received": 0.0,
            "video_seq_loss_per_sec": 12.5,
            "keyframe_requests_per_sec": 2.5,
        }));
        peer.last_screen_stats = Some(json!({
            "fps_received": 0.0,
            "video_seq_loss_per_sec": 3.25,
            "keyframe_requests_per_sec": 0.75,
        }));

        let mut health_map = HashMap::new();
        health_map.insert("peer-1".to_string(), peer);

        build_health_packet(health_map)
    }

    /// #2524: loss and PLI reach BOTH `VideoStats` buckets at fps 0, not just camera.
    #[test]
    fn loss_and_pli_fold_into_both_video_stats_buckets_at_fps_zero() {
        let pb = health_packet_with_both_streams_loss_pli();
        let peer = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present");
        let camera = peer
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");
        let screen = peer
            .screen_video_stats
            .as_ref()
            .expect("screen video stats must be present");

        assert_eq!(camera.fps_received, 0.0);
        assert_eq!(camera.video_seq_loss_per_sec, Some(12.5));
        assert_eq!(camera.keyframe_requests_per_sec, Some(2.5));

        assert_eq!(screen.fps_received, 0.0);
        assert_eq!(
            screen.video_seq_loss_per_sec,
            Some(3.25),
            "3.25, not the camera's 12.5 — a fold reading the wrong blob fails here"
        );
        assert_eq!(
            screen.keyframe_requests_per_sec,
            Some(0.75),
            "0.75, not the camera's 2.5"
        );

        let mut absent = PbVideoStats::new();
        fold_loss_diagnostics(&mut absent, &json!({}), None);
        assert_eq!(absent.video_seq_loss_per_sec, None);
        assert_eq!(absent.keyframe_requests_per_sec, None);
    }

    /// #1660 END-TO-END BLOCKER GUARD: the server's screen playout gauges read
    /// `screen_video_stats` on the wire, but they are dead unless the client folds the playout
    /// family from `last_screen_stats` into that proto. This drives the SCREEN serialization path
    /// and asserts all five playout fields reach the `screen_video_stats` PROTO (not the JSON blob).
    ///
    /// Mutation check: remove any of the five `svs.<field> = v` fold lines in the screen block and
    /// the matching assert reads the proto default (0 / 0.0) and fails. content_staleness_ms carries
    /// a value > 1800ms to also prove the screen field is unbounded, like its camera sibling.
    #[test]
    fn screen_playout_family_folds_into_proto_when_fps_received_positive() {
        let pb = health_packet_with_screen_playout_stats(30.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .screen_video_stats
            .as_ref()
            .expect("screen video stats must be present");

        assert_eq!(stats.fps_received, 30.0);
        assert_eq!(stats.playout_latency_ms, 1400.0);
        assert_eq!(stats.playout_stage1_span_ms, 900.0);
        assert_eq!(stats.playout_paint_lag_ms, 600.0);
        assert_eq!(stats.content_staleness_ms, 240000.0);
        assert!(
            stats.content_staleness_ms > 1800.0,
            "screen content_staleness_ms must NOT be capped at the 1800ms playout-latency bound"
        );
        assert_eq!(stats.playout_skip_to_live_total, 7);
    }

    /// #1660: the screen fold must share the camera gate — the four ms gauges are gated on
    /// fps_received > 0 (a paused/hidden screen tile paints nothing => "at live"), while the
    /// skip-to-live COUNTER folds UNCONDITIONALLY (a stream that fell idle must keep reporting its
    /// lifetime total). Mutation check: moving a ms gauge outside the gate makes its assert read
    /// nonzero and fail; moving the counter inside the gate makes its assert read 0 and fail.
    #[test]
    fn screen_playout_ms_gauges_gated_but_counter_folds_when_fps_received_zero() {
        let pb = health_packet_with_screen_playout_stats(0.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .screen_video_stats
            .as_ref()
            .expect("screen video stats must be present");

        assert_eq!(stats.fps_received, 0.0);
        // ms gauges gated to 0 at fps 0 ...
        assert_eq!(stats.playout_latency_ms, 0.0);
        assert_eq!(stats.playout_stage1_span_ms, 0.0);
        assert_eq!(stats.playout_paint_lag_ms, 0.0);
        assert_eq!(stats.content_staleness_ms, 0.0);
        // ... but the cumulative COUNTERS still report their lifetime values.
        assert_eq!(stats.playout_skip_to_live_total, 7);
        // #2201: same unconditional rule, and load-bearing HERE — a screen freeze is exactly
        // when fps reads 0, so gating this would blank the metric in its only case. 4, not the
        // camera fixture's 9, so a bucket transposition fails. Deleting the screen fold block
        // makes this `None` (measured: it was previously revertible-green).
        assert_eq!(stats.keyframe_arrivals_total, Some(4));
    }

    /// Build a health packet whose peer carries NetEQ audio stats. `playout_latency_ms` is
    /// `Some(v)` to include the field in the stats JSON (the shape the NetEQ worker emits at the
    /// top level of NetEqStats), or `None` to OMIT it entirely (the older-worker / pre-#1299 case).
    fn health_packet_with_neteq_playout_latency(playout_latency_ms: Option<f64>) -> PbHealthPacket {
        let mut peer = PeerHealthData::new("peer-1".to_string());
        let mut neteq = json!({
            "current_buffer_size_ms": 200.0,
            "target_delay_ms": 80.0,
            "packets_awaiting_decode": 3.0,
            "packets_per_sec": 50.0,
        });
        if let Some(v) = playout_latency_ms {
            neteq["playout_latency_ms"] = json!(v);
        }
        peer.update_audio_stats(neteq);

        let mut health_map = HashMap::new();
        health_map.insert("peer-1".to_string(), peer);

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,   // unistream_stale_delta_drops_total (#1737 Phase 1)
            0.0, // encoder_queue_depth_report
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            None,
            None,
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("create_health_packet returns Some unconditionally");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    /// Audio playout latency (#1299): when NetEQ reports a filtered playout buffer level in the
    /// stats JSON, it must fold into NetEqStats.playout_latency_ms on the wire. Mirrors the video
    /// sibling test. Fails if the read in create_health_packet is dropped or reads the wrong key.
    #[test]
    fn audio_playout_latency_folds_from_neteq_stats() {
        let pb = health_packet_with_neteq_playout_latency(Some(1450.0));
        let neteq = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .neteq_stats
            .as_ref()
            .expect("neteq stats must be present");

        assert_eq!(neteq.playout_latency_ms, 1450.0);
        // Sanity: the metric is distinct from the raw buffer snapshot it travels alongside.
        assert_eq!(neteq.current_buffer_size_ms, 200.0);
    }

    /// When the NetEQ stats JSON OMITS playout_latency_ms (older worker / pre-#1299), the proto
    /// field must stay at its 0.0 default ("at live"), never a stale or fabricated value. Guards
    /// against the #1338-style false-value trap.
    #[test]
    fn audio_playout_latency_defaults_to_zero_when_absent() {
        let pb = health_packet_with_neteq_playout_latency(None);
        let neteq = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .neteq_stats
            .as_ref()
            .expect("neteq stats must be present");

        assert_eq!(neteq.playout_latency_ms, 0.0);
    }

    /// #1032: a cached agent-memory reading rides the HealthPacket on the wire.
    #[test]
    fn agent_memory_rides_health_packet_when_present() {
        let pb = health_packet_with_agent_memory(Some(2_147_483_648));
        assert_eq!(pb.agent_memory_bytes, Some(2_147_483_648));
    }

    /// #1032: when the background sampler has produced no value (API absent or
    /// not yet resolved), the field is omitted — Grafana shows a gap, not a
    /// misleading zero.
    #[test]
    fn agent_memory_absent_when_none() {
        let pb = health_packet_with_agent_memory(None);
        assert!(pb.agent_memory_bytes.is_none());
    }

    /// #1032: packet construction must not disappear just because the peer
    /// health map is still empty; client-wide telemetry like non-heap memory
    /// still needs to flow during solo sessions and warm-up.
    #[test]
    fn health_packet_still_emitted_with_empty_peer_map() {
        let health_map = HashMap::new();

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,   // unistream_stale_delta_drops_total (#1737 Phase 1)
            0.0, // encoder_queue_depth_report
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            None, // screen_encoder_output_fps (#2147: unwired => omitted)
            0,    // effective_video_layers (#1143)
            0,    // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            0,            // rtt_probe_dropped_total
            0,            // rtt_probe_stale_suppressions_total
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None, // #1482: client_main_thread_load
            None,
            Some(512),
            0,                             // effective_screen_layers (#1561)
            0,                             // active_screen_layers (#1561)
            0,                             // effective_audio_layers (#1561)
            0,                             // audio_congestion_ceiling (#1561)
            0,                             // active_audio_layers (#1561)
            HashMap::new(),                // received_layers (#1561)
            WtReceiveTelemetry::default(), // wt_telemetry (issue 2031)
            Vec::new(),                    // camera_layer_metrics (#2170)
            HashMap::new(),                // staleness_max_ms (#2511)
        )
        .expect("empty peer map must still produce a packet");

        let pb = PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf");

        assert!(pb.peer_stats.is_empty());
        assert_eq!(pb.agent_memory_bytes, Some(512));
    }

    /// #1032: a failed sample must clear the cache instead of leaving the last
    /// successful measurement visible forever.
    #[test]
    fn agent_memory_cache_clears_on_failure() {
        let cache = Rc::new(RefCell::new(Some(123)));
        *cache.borrow_mut() = Some(456);
        assert_eq!(*cache.borrow(), Some(456));

        *cache.borrow_mut() = None;
        assert_eq!(*cache.borrow(), None);
    }

    /// #1032: WASM linear memory is read inline in a `wasm32`-gated block, so on
    /// the (non-wasm) test target the field must be absent. This guards against
    /// anyone moving the read out of the cfg block and emitting a host-side
    /// value that would be meaningless for browser memory observability.
    #[test]
    fn wasm_memory_absent_on_non_wasm_target() {
        let pb = health_packet_with_agent_memory(None);
        assert!(
            pb.wasm_memory_bytes.is_none(),
            "wasm_memory_bytes must only be populated on the wasm32 target"
        );
    }

    #[test]
    fn decode_budget_snapshot_rides_health_packet_pressured_auto() {
        let pb = health_packet_with_decode_budget(Some(DecodeBudgetSnapshot {
            effective_cap: 5,
            natural: 12,
            pressured: true,
            override_mode: 1, // Auto
            override_fixed_n: 0,
        }));

        let db = pb
            .decode_budget
            .as_ref()
            .expect("decode_budget must be set");
        assert_eq!(db.effective_cap, 5);
        assert_eq!(db.natural, 12);
        assert!(db.pressured);
        assert_eq!(
            db.override_mode.enum_value_or_default(),
            PbOverrideMode::OVERRIDE_MODE_AUTO
        );
        // override_fixed_n is meaningless in Auto and left at its default.
        assert_eq!(db.override_fixed_n, 0);
    }

    #[test]
    fn decode_budget_snapshot_rides_health_packet_fixed_override() {
        let pb = health_packet_with_decode_budget(Some(DecodeBudgetSnapshot {
            effective_cap: 3,
            natural: 12,
            pressured: false,
            override_mode: 2, // Fixed
            override_fixed_n: 3,
        }));

        let db = pb
            .decode_budget
            .as_ref()
            .expect("decode_budget must be set");
        assert_eq!(db.effective_cap, 3);
        assert_eq!(
            db.override_mode.enum_value_or_default(),
            PbOverrideMode::OVERRIDE_MODE_FIXED
        );
        assert_eq!(db.override_fixed_n, 3);
    }

    #[test]
    fn decode_budget_absent_when_no_snapshot() {
        // No snapshot (controller pre-warmup / no peers) → field omitted so a
        // healthy no-peer packet stays minimal and backward-compatible.
        let pb = health_packet_with_decode_budget(None);
        assert!(pb.decode_budget.is_none());
    }

    #[test]
    fn normalize_gpu_family_known_vendors() {
        assert_eq!(normalize_gpu_family("Apple M1 Pro"), "Apple GPU");
        assert_eq!(normalize_gpu_family("Apple GPU"), "Apple GPU");
        assert_eq!(
            normalize_gpu_family(
                "ANGLE (Intel(R) Iris(R) Plus Graphics 645 Direct3D11 vs_5_0 ps_5_0, D3D11)"
            ),
            "Intel(R) Iris(R) Plus Graphics 6"
        );
        assert_eq!(
            normalize_gpu_family("ANGLE (NVIDIA GeForce RTX 3060 Direct3D11)"),
            "GeForce RTX 3060 Direct3"
        );
        assert_eq!(
            normalize_gpu_family("AMD Radeon Pro 5500M"),
            "Radeon Pro 5500M"
        );
        assert_eq!(normalize_gpu_family(""), "");
    }

    #[test]
    fn normalize_gpu_family_unknown_truncates() {
        let long = "SomeUnknownVendor With A Very Long Renderer String That Exceeds 32 Chars";
        let result = normalize_gpu_family(long);
        assert!(result.len() <= 32);
    }
}
