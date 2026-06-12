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

//! Bot-side wrapper around `videocall_aq::EncoderBitrateController`.
//!
//! The upstream controller is designed for single-thread browser use (Rc
//! inside, serialized access from the encoder task). Bots are native, multi
//! threaded, and need lock-free reads from the hot audio/video producer loops.
//!
//! [`BotAq`] solves this by:
//!
//! 1. Storing the `EncoderBitrateController` behind a [`Mutex`] — only the
//!    single "ingest" task writing diagnostics packets ever takes the lock.
//! 2. Exposing the current-tier snapshot via atomic fields that can be read
//!    with a relaxed load from the producers every frame/packet.
//! 3. Bumping a [`tier_epoch`](Self::tier_epoch) counter whenever the tier
//!    changes so producers can cheaply detect "my encoder config is stale"
//!    without paying for a lock on every iteration.
//!
//! The producers read `tier_epoch()` each iteration (one `Acquire` load),
//! compare it to a local `last_epoch`, and only re-snapshot when the value
//! changed. The writer performs `Release` stores on every snapshot field and
//! then a `Release` fetch-add on `tier_epoch`, so the Acquire-load on the
//! reader synchronizes-with the Release-store on the writer and the reader
//! is guaranteed to observe every snapshot field written before the epoch
//! bump. Steady-state cost on the reader is one acquire load per frame.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use videocall_aq::constants::{
    AUDIO_QUALITY_TIERS, DEFAULT_VIDEO_TIER_INDEX, ENCODER_QUEUE_BACKPRESSURE_HIGH,
    SIMULCAST_MAX_LAYERS, VIDEO_QUALITY_TIERS,
};
use videocall_aq::{default_clock, Clock, EncoderBitrateController, TierTransitionRecord};

#[cfg(feature = "metrics")]
use crate::metrics_server::BotMetrics;

/// Snapshot of video encoder settings derived from the current AQ tier.
#[derive(Clone, Copy, Debug)]
pub struct VideoEncodeSettings {
    pub bitrate_kbps: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub target_fps: u32,
    pub keyframe_interval: u32,
}

/// Snapshot of audio encoder settings derived from the current AQ tier.
#[derive(Clone, Copy, Debug)]
pub struct AudioEncodeSettings {
    pub bitrate_kbps: u32,
    pub fec: bool,
    pub dtx: bool,
}

/// Cheap, lock-free snapshot of the current simulcast state (issue #1083 V21).
///
/// Producers poll [`BotAq::simulcast_epoch`] each loop iteration and only
/// re-read this when the epoch changed. In single-stream mode `is_simulcast`
/// is `false`, `active` and `layer_count` are `1`, and the producers ignore
/// the per-layer bitrates entirely (legacy path).
#[derive(Clone, Debug)]
pub struct SimulcastSnapshot {
    /// `true` once an N>=2 ladder is configured.
    pub is_simulcast: bool,
    /// Configured ladder ceiling (full layer count). `1` in single-stream mode.
    pub layer_count: usize,
    /// Currently-active layer count. The producer skips any layer whose
    /// `layer_id >= active` (top-down shed; the base layer at id 0 always flows).
    pub active: usize,
    /// Per-layer target bitrates (kbps), lowest layer first. `layer_count`
    /// entries; only the first `active` are encoded/sent.
    pub layer_bitrates_kbps: Vec<u32>,
}

/// Bot-side handle around an [`EncoderBitrateController`].
///
/// Thread-safety model:
///
/// - The inner controller is protected by a single `Mutex`. Only one writer
///   task (the one feeding it diagnostics packets) ever takes the lock.
/// - All snapshot fields (`video_bitrate_kbps`, `audio_bitrate_kbps`, …) are
///   written under the mutex with `Ordering::Release` whenever a tier change
///   fires. Producers then see them via an `Acquire` load of `tier_epoch`
///   followed by `Relaxed` loads of the snapshot fields — the Acquire on
///   the epoch synchronizes-with the `Release` fetch-add on the writer and
///   establishes happens-before over all prior writes. This keeps the
///   producer hot paths lock-free while remaining sound on architectures
///   with weaker memory models (aarch64, Power, RISC-V).
/// - [`tier_epoch`](Self::tier_epoch) is incremented on every tier change and
///   lets producers detect a tier change with a single Acquire load.
pub struct BotAq {
    inner: Mutex<EncoderBitrateController>,
    /// Target FPS shared with the inner controller. The bot keeps this fixed
    /// after construction.
    target_fps: Arc<AtomicU32>,
    /// Clock used to timestamp `tick()` calls (issue #1108). The same clock
    /// instance also lives inside the controller; the bot holds a clone so its
    /// `tick()` wrapper can read `now_ms()` without locking the controller.
    clock: Arc<dyn Clock>,

    // --- Video tier snapshot (mirrors `controller.current_video_tier()`) ---
    video_bitrate_kbps: AtomicU32,
    video_max_width: AtomicU32,
    video_max_height: AtomicU32,
    video_target_fps: AtomicU32,
    video_keyframe_interval: AtomicU32,

    // --- Audio tier snapshot (mirrors `controller.current_audio_tier()`) ---
    audio_bitrate_kbps: AtomicU32,
    audio_fec: AtomicBool,
    audio_dtx: AtomicBool,

    // --- Indices + epoch ---
    video_tier_index: AtomicU32,
    audio_tier_index: AtomicU32,
    /// Bumped on every tier transition. Producers poll this each iteration
    /// (one relaxed load) and only re-snapshot when it changes.
    tier_epoch: AtomicU64,

    // --- Simulcast snapshot (issue #1083 V21) ---
    /// `true` once [`Self::set_simulcast_layers`] enabled an N>=2 ladder. When
    /// false, [`Self::simulcast_snapshot`] reports a single-layer ladder and the
    /// producers run their legacy single-stream path unchanged.
    is_simulcast: AtomicBool,
    /// Configured ladder ceiling (number of layers in the full ladder). `1` in
    /// single-stream mode.
    simulcast_layer_count: AtomicUsize,
    /// Currently-active simulcast layer count (encoded + sent). Producers skip
    /// any layer whose `layer_id >= active_layer_count` (top-down shed). `1` in
    /// single-stream mode. Refreshed every [`Self::tick`] (not just on tier
    /// change) so a backpressure-driven shed/restore — which may not move the
    /// tier index — is still observed by the producers without a tier-change
    /// epoch bump.
    active_layer_count: AtomicUsize,
    /// Per-layer target bitrates (kbps), lowest layer first, one slot per ladder
    /// rung up to [`SIMULCAST_MAX_LAYERS`]. Only the first `active_layer_count`
    /// are meaningful to the encoder. Refreshed every tick. The slot count is a
    /// fixed array (no allocation on the read path) sized to the compile-time
    /// ladder max.
    simulcast_layer_bitrates_kbps: [AtomicU32; SIMULCAST_MAX_LAYERS],
    /// Bumped whenever the active layer count OR any per-layer target bitrate
    /// changes. Producers poll this each iteration (one Acquire load) and only
    /// re-read the simulcast snapshot when it changed — the same cheap-poll idiom
    /// as [`tier_epoch`](Self::tier_epoch), but for the per-layer simulcast state
    /// (which the legacy `tier_epoch` deliberately does NOT cover, since a shed
    /// need not change the tier).
    simulcast_epoch: AtomicU64,

    // --- Uplink saturation -> synthetic encoder queue depth (issue #1083 V21) ---
    /// The netsim uplink shim's cumulative `bandwidth_wait_us` observed at the
    /// last [`Self::observe_uplink_saturation`] sample. The bot has no WebCodecs
    /// encoder, so the gradual backpressure axis is fed from the bot's OWN uplink
    /// saturation instead: the netsim uplink shaper records, per packet, the
    /// microseconds of delay it imposed *solely* because the token bucket was in
    /// deficit (the offered byte rate exceeded the configured `uplink_kbps`). A
    /// positive delta between samples means the uplink was bandwidth-saturated
    /// this interval — the bot's honest analog of a browser encoder queue backing
    /// up. Crucially, a pure latency/jitter/loss profile (no rate cap) leaves
    /// this flat, so it does NOT trip on path delay. See
    /// [`Self::observe_uplink_saturation`].
    last_uplink_wait_us: AtomicU64,

    // --- PID-derived telemetry for health reporting ---
    /// PID-adjusted target bitrate in kbps, f32 bits packed into u32.
    last_target_bitrate_kbps_bits: AtomicU32,
    /// Last p75 received FPS as observed by the PID, f32 bits.
    last_p75_peer_fps_bits: AtomicU32,

    /// Optional Prometheus metrics handle + pre-built label pair.
    ///
    /// When set, every [`Self::tick`] call refreshes the `bot_aq_*` gauges.
    /// Kept behind the `metrics` feature so non-metrics builds do not pay the
    /// extra field or any storage for the labels.
    #[cfg(feature = "metrics")]
    metrics: Mutex<Option<MetricsBinding>>,
}

/// Glue struct tying a [`BotMetrics`] handle to the pre-computed label
/// values (`bot`, `meeting`) used by every AQ metric on this bot. Avoids
/// re-materializing the label slices on the hot path.
#[cfg(feature = "metrics")]
struct MetricsBinding {
    metrics: Arc<BotMetrics>,
    bot: String,
    meeting: String,
}

impl BotAq {
    /// Construct a new `BotAq` using the supplied [`Clock`].
    ///
    /// The initial tier is [`DEFAULT_VIDEO_TIER_INDEX`] (medium / 480p /
    /// 25 fps). Target FPS is seeded from that tier so the PID controller
    /// does not fight an artificial setpoint on the very first packet.
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        let initial_video = &VIDEO_QUALITY_TIERS[DEFAULT_VIDEO_TIER_INDEX];
        let target_fps = Arc::new(AtomicU32::new(initial_video.target_fps));

        // Use the initial tier's ideal bitrate as the controller's anchor.
        let controller = EncoderBitrateController::with_clock(
            initial_video.ideal_bitrate_kbps,
            Arc::clone(&target_fps),
            Arc::clone(&clock),
        );

        // Seed audio snapshot from the controller's current audio tier.
        let initial_audio = controller.current_audio_tier();

        let bot_aq = Self {
            inner: Mutex::new(controller),
            target_fps,
            clock,
            video_bitrate_kbps: AtomicU32::new(initial_video.ideal_bitrate_kbps),
            video_max_width: AtomicU32::new(initial_video.max_width),
            video_max_height: AtomicU32::new(initial_video.max_height),
            video_target_fps: AtomicU32::new(initial_video.target_fps),
            video_keyframe_interval: AtomicU32::new(initial_video.keyframe_interval_frames),
            audio_bitrate_kbps: AtomicU32::new(initial_audio.bitrate_kbps),
            audio_fec: AtomicBool::new(initial_audio.enable_fec),
            audio_dtx: AtomicBool::new(initial_audio.enable_dtx),
            video_tier_index: AtomicU32::new(DEFAULT_VIDEO_TIER_INDEX as u32),
            audio_tier_index: AtomicU32::new(
                AUDIO_QUALITY_TIERS
                    .iter()
                    .position(|t| t.label == initial_audio.label)
                    .unwrap_or(0) as u32,
            ),
            tier_epoch: AtomicU64::new(0),
            // Simulcast snapshot (issue #1083 V21). Starts in single-stream mode
            // (1 layer, full bitrate) until set_simulcast_layers() enables N>=2.
            is_simulcast: AtomicBool::new(false),
            simulcast_layer_count: AtomicUsize::new(1),
            active_layer_count: AtomicUsize::new(1),
            simulcast_layer_bitrates_kbps: std::array::from_fn(|_| AtomicU32::new(0)),
            simulcast_epoch: AtomicU64::new(0),
            last_uplink_wait_us: AtomicU64::new(0),
            last_target_bitrate_kbps_bits: AtomicU32::new(
                (initial_video.ideal_bitrate_kbps as f32).to_bits(),
            ),
            last_p75_peer_fps_bits: AtomicU32::new(0),
            #[cfg(feature = "metrics")]
            metrics: Mutex::new(None),
        };

        Arc::new(bot_aq)
    }

    /// Attach (or clear) the Prometheus metrics handle for this bot.
    ///
    /// Called once at construction time from [`main::run_client`] when the
    /// `metrics` feature is enabled. Updates published to `bot_aq_*` use the
    /// supplied `bot` (user_id) and `meeting` labels so dashboards can
    /// filter per-bot / per-meeting.
    #[cfg(feature = "metrics")]
    pub fn set_metrics(&self, metrics: Arc<BotMetrics>, bot: String, meeting: String) {
        // We take the mutex once here and never again — this is an
        // initialization-time call, not a hot-path operation.
        if let Ok(mut guard) = self.metrics.lock() {
            *guard = Some(MetricsBinding {
                metrics,
                bot,
                meeting,
            });
        }
    }

    /// Convenience constructor that uses the native default clock.
    pub fn with_default_clock() -> Arc<Self> {
        Self::new(default_clock())
    }

    /// Inject the (simulated) encoder-queue backpressure depth (issue #1108).
    ///
    /// The native bot has no WebCodecs encoder, so in normal operation it feeds
    /// `0` (no backpressure) and therefore never degrades on the gradual axis —
    /// it only reacts to the explicit `force_*` signals. Tests use this to
    /// synthesize backpressure and exercise the shed path.
    pub fn inject_encoder_queue_depth(&self, depth: u32) {
        let mut ctrl = self.lock_ctrl();
        ctrl.observe_encoder_queue_depth(depth);
    }

    /// Enable an `n`-layer simulcast ladder on the inner controller (issue #1083
    /// V21). No-op (single-stream) for `n < 2`.
    ///
    /// **DELIBERATE DIVERGENCE FROM THE BROWSER.** The browser CAMERA encoder
    /// calls `EncoderBitrateController::set_simulcast_ceiling_start_at_base`,
    /// which starts ACTIVE at the base layer (1) and earns rungs up at runtime
    /// via the headroom probe (issue #1140 / #1141). The bot instead calls
    /// `set_simulcast_layers`, which starts ALL `n` layers active immediately
    /// ("legacy full-ladder"). Rationale: the bot is a synthetic load generator,
    /// not a real device protecting a weak CPU — validation row V20 relies on a
    /// deterministic full-ladder publish from the first frame, and the cold-start
    /// ramp would make V20 flaky and delay V21's congestion phase. The bot is
    /// therefore **shed-only from N**: it begins at the full ladder and the AQ
    /// only ever sheds the top layer down under the bot's own uplink saturation
    /// (see [`observe_uplink_saturation`](Self::observe_uplink_saturation)); it
    /// never probes layers UP. After enabling, the snapshot is republished so the
    /// producers pick up the active count + per-layer targets on their next poll.
    pub fn set_simulcast_layers(&self, n: usize) {
        if n < 2 {
            return; // single-stream: leave the controller (and snapshot) untouched.
        }
        {
            let mut ctrl = self.lock_ctrl();
            // Full-ladder (NOT start-at-base): see the divergence note above.
            ctrl.set_simulcast_layers(n);
        }
        // Publish the freshly-configured ladder so producers see the active count
        // and per-layer targets immediately (a tick has not necessarily run yet).
        self.publish_simulcast_snapshot();
        info!(
            "BotAq: simulcast enabled (full ladder, shed-only) n={} (browser start-at-base ramp intentionally NOT used for the load-test bot)",
            n
        );
    }

    /// Feed the bot's own UPLINK SATURATION into the AQ (issue #1083 V21),
    /// translated into the controller's `encode_queue_size()` axis.
    ///
    /// The native bot has no WebCodecs encoder, so it cannot report a real
    /// encoder queue depth — it always fed `0`, leaving the gradual shed path
    /// inert. The bot DOES produce an honest, equivalent backpressure signal:
    /// the netsim uplink shim ([`videocall_netsim::NetSimShim`]) records, per
    /// packet, the microseconds of delay it imposed *solely* because the token
    /// bucket was in deficit — i.e. because the offered byte rate exceeded the
    /// configured `uplink_kbps`. That cumulative counter
    /// (`NetSimShim::bandwidth_wait_us`) advances iff the link was actually
    /// bandwidth-saturated. It deliberately excludes the base-latency, jitter,
    /// and reorder delay components, so a pure latency/jitter/loss profile (no
    /// rate cap, e.g. a 150ms-latency mobile preset) leaves it **flat** and
    /// never trips a shed — a latency link is not a bandwidth-limited link.
    ///
    /// We map "the uplink imposed new bandwidth-deficit delay since the last
    /// sample" to a synthetic depth at the controller's HIGH threshold (so the
    /// sustain timer can fire a shed) and "no new bandwidth wait" to `0` (so the
    /// recover timer can climb back). The controller's own hysteresis (sustain /
    /// stabilization windows) then debounces it — a single transient deficit
    /// does not shed. This is a real signal the bot genuinely produces; it is
    /// NOT a synthetic trigger fabricated to force a shed.
    ///
    /// NOTE on why this replaced the earlier `transport_drops_counter` signal:
    /// the outbound shim spawns a detached delay task per `Admission::Delay`, so
    /// `packet_tx` never actually backs up under bandwidth shaping and the
    /// producers' `try_send` never fail — the drop counter stayed flat on a real
    /// run and the shed never armed. The shim's `bandwidth_wait_us` measures the
    /// saturation directly at the source and is immune to that drain behavior.
    ///
    /// `cumulative_uplink_wait_us` is the monotonic
    /// `NetSimShim::bandwidth_wait_us()` value. Call once per tick interval (the
    /// bot's AQ tick task does this). In passthrough (no netsim) the shim never
    /// runs and this is always `0`, so the legacy zero-backpressure behavior is
    /// preserved exactly.
    pub fn observe_uplink_saturation(&self, cumulative_uplink_wait_us: u64) {
        let prev = self
            .last_uplink_wait_us
            .swap(cumulative_uplink_wait_us, Ordering::Relaxed);
        // Guard against a counter reset (defensive — the shim never resets it):
        // treat a decrease as "no new wait" rather than a giant negative delta.
        let new_wait_us = cumulative_uplink_wait_us.saturating_sub(prev);
        let depth = if new_wait_us > 0 {
            // At/above HIGH so the controller's sustain timer arms; the magnitude
            // beyond HIGH is irrelevant (the decision is threshold-based).
            ENCODER_QUEUE_BACKPRESSURE_HIGH
        } else {
            0
        };
        let mut ctrl = self.lock_ctrl();
        ctrl.observe_encoder_queue_depth(depth);
    }

    /// Aggressively cut quality on a self-targeted server CONGESTION signal
    /// (issue #1108 thin wrapper over the controller's kept `force_congestion_cut`).
    /// Republishes the tier snapshot if the tier changed.
    pub fn force_congestion_cut(&self) {
        let changed = {
            let mut ctrl = self.lock_ctrl();
            ctrl.force_congestion_cut()
        };
        if changed {
            self.tick();
        }
    }

    /// Step video quality down on WS/relay backpressure (issue #1108 thin
    /// wrapper over the controller's kept `force_video_step_down`).
    pub fn force_video_step_down(&self) {
        let changed = {
            let mut ctrl = self.lock_ctrl();
            ctrl.force_video_step_down()
        };
        if changed {
            self.tick();
        }
    }

    /// Lock the inner controller, recovering from a poisoned mutex (AQ is
    /// non-critical: log and continue rather than propagating the panic).
    fn lock_ctrl(&self) -> std::sync::MutexGuard<'_, EncoderBitrateController> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!("BotAq: controller mutex poisoned, recovering: {e}");
                e.into_inner()
            }
        }
    }

    /// Advance the AQ one tick (issue #1108): map the (bot-injected, normally 0)
    /// encoder backpressure into a degrade/recover decision and apply it, then
    /// publish telemetry and re-snapshot the tier if it changed. Replaces the
    /// former `process_diagnostics` — the bot no longer feeds receiver FPS into
    /// the AQ. Called on a periodic timer from `main`.
    pub fn tick(&self) {
        let now = self.clock.now_ms();
        let mut ctrl = self.lock_ctrl();

        ctrl.tick(now);

        // Refresh the simulcast snapshot EVERY tick (not just on tier change):
        // a backpressure-driven layer shed/restore (issue #1083 V21) may not move
        // the tier index, so the legacy `take_tier_changed()`/`tier_epoch` path
        // below would miss it. `publish_simulcast_snapshot_locked` only bumps
        // `simulcast_epoch` when the active count or a per-layer target actually
        // changed, so the producer's poll stays cheap and re-reads only on real
        // change. No-op in single-stream mode.
        self.publish_simulcast_snapshot_locked(&ctrl);

        // Always publish the latest telemetry — useful for health reporting even
        // when the tier has not changed. NOTE(#1184, see #1228): the
        // receiver-FPS -> sender-AQ ratio signals (fps_ratio / bitrate_ratio)
        // were removed — receiver FPS no longer feeds the sender AQ.
        // `encoder_queue_depth` carries the sender backpressure signal formerly
        // exposed via the misnamed `last_p75_peer_fps`.
        let target_bitrate = ctrl.last_target_bitrate_kbps() as f32;
        let p75_peer_fps = ctrl.encoder_queue_depth() as f32;
        self.last_target_bitrate_kbps_bits
            .store(target_bitrate.to_bits(), Ordering::Relaxed);
        self.last_p75_peer_fps_bits
            .store(p75_peer_fps.to_bits(), Ordering::Relaxed);

        // Refresh per-scrape Prometheus gauges every tick.
        #[cfg(feature = "metrics")]
        self.publish_live_metrics(
            target_bitrate,
            p75_peer_fps,
            ctrl.video_tier_index() as i64,
            ctrl.audio_tier_index() as i64,
        );

        // Tier changes are rare — only republish the full tier snapshot when
        // `take_tier_changed()` returns true.
        if ctrl.take_tier_changed() {
            let video = ctrl.current_video_tier();
            let audio = ctrl.current_audio_tier();

            // Release-store every snapshot field, then bump `tier_epoch` with
            // a Release fetch-add. Readers on the producer threads do an
            // Acquire load on `tier_epoch` (see `tier_epoch()`), so the
            // Release/Acquire pair establishes happens-before over all the
            // stores below. Relaxed loads of the snapshot fields on the
            // reader are safe because they are sequenced-after the Acquire
            // load of the epoch.
            self.video_bitrate_kbps
                .store(video.ideal_bitrate_kbps, Ordering::Release);
            self.video_max_width
                .store(video.max_width, Ordering::Release);
            self.video_max_height
                .store(video.max_height, Ordering::Release);
            self.video_target_fps
                .store(video.target_fps, Ordering::Release);
            self.video_keyframe_interval
                .store(video.keyframe_interval_frames, Ordering::Release);

            self.audio_bitrate_kbps
                .store(audio.bitrate_kbps, Ordering::Release);
            self.audio_fec.store(audio.enable_fec, Ordering::Release);
            self.audio_dtx.store(audio.enable_dtx, Ordering::Release);

            self.video_tier_index
                .store(ctrl.video_tier_index() as u32, Ordering::Release);
            self.audio_tier_index
                .store(ctrl.audio_tier_index() as u32, Ordering::Release);

            // Release fetch-add to publish every prior Release store to any
            // reader that subsequently does an Acquire load on `tier_epoch`.
            self.tier_epoch.fetch_add(1, Ordering::Release);

            info!(
                "BotAq: tier change -> video='{}' ({}kbps, {}x{}@{}fps, kf={}), audio='{}' ({}kbps, fec={}, dtx={})",
                video.label,
                video.ideal_bitrate_kbps,
                video.max_width,
                video.max_height,
                video.target_fps,
                video.keyframe_interval_frames,
                audio.label,
                audio.bitrate_kbps,
                audio.enable_fec,
                audio.enable_dtx,
            );
        }
    }

    /// Drain buffered tier-transition events from the inner controller.
    ///
    /// Mirrors the browser's per-heartbeat drain in
    /// `videocall-client/src/health_reporter.rs` so bot HealthPackets populate
    /// `HealthPacket.tier_transitions` with the same shape as real browsers,
    /// letting Prometheus counter `videocall_tier_transition_total` increment
    /// for bot meetings. Returns an empty `Vec` if the mutex is poisoned
    /// (non-critical path — log and continue, matching `process_diagnostics`).
    pub fn drain_tier_transitions(&self) -> Vec<TierTransitionRecord> {
        let mut ctrl = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!(
                    "BotAq: controller mutex poisoned on drain_tier_transitions, recovering: {e}"
                );
                e.into_inner()
            }
        };
        ctrl.drain_tier_transitions()
    }

    /// Push the latest AQ telemetry into the Prometheus gauges. No-op when
    /// no metrics handle has been installed.
    #[cfg(feature = "metrics")]
    fn publish_live_metrics(
        &self,
        target_bitrate_kbps: f32,
        p75_peer_fps: f32,
        video_tier_index: i64,
        audio_tier_index: i64,
    ) {
        let Ok(guard) = self.metrics.lock() else {
            return;
        };
        let Some(binding) = guard.as_ref() else {
            return;
        };
        let labels: [&str; 2] = [binding.bot.as_str(), binding.meeting.as_str()];
        binding
            .metrics
            .aq_target_bitrate_kbps
            .with_label_values(&labels)
            .set(target_bitrate_kbps as f64);
        binding
            .metrics
            .aq_p75_peer_fps
            .with_label_values(&labels)
            .set(p75_peer_fps as f64);
        binding
            .metrics
            .aq_video_tier_index
            .with_label_values(&labels)
            .set(video_tier_index);
        binding
            .metrics
            .aq_audio_tier_index
            .with_label_values(&labels)
            .set(audio_tier_index);
    }

    /// Cheap lock-free read of the current video tier settings.
    pub fn snapshot_video(&self) -> VideoEncodeSettings {
        VideoEncodeSettings {
            bitrate_kbps: self.video_bitrate_kbps.load(Ordering::Relaxed),
            max_width: self.video_max_width.load(Ordering::Relaxed),
            max_height: self.video_max_height.load(Ordering::Relaxed),
            target_fps: self.video_target_fps.load(Ordering::Relaxed),
            keyframe_interval: self.video_keyframe_interval.load(Ordering::Relaxed),
        }
    }

    /// Cheap lock-free read of the current audio tier settings.
    pub fn snapshot_audio(&self) -> AudioEncodeSettings {
        AudioEncodeSettings {
            bitrate_kbps: self.audio_bitrate_kbps.load(Ordering::Relaxed),
            fec: self.audio_fec.load(Ordering::Relaxed),
            dtx: self.audio_dtx.load(Ordering::Relaxed),
        }
    }

    /// Cheap, lock-free read of the current simulcast state (issue #1083 V21).
    ///
    /// Producers compare [`Self::simulcast_epoch`] against a local value each
    /// loop iteration and only call this when it changed. The `Acquire` load of
    /// the epoch (in `simulcast_epoch()`) synchronizes-with the writer's
    /// `Release` fetch-add, so the `Relaxed` loads of the count/bitrate fields
    /// here observe every field written before that epoch bump.
    pub fn simulcast_snapshot(&self) -> SimulcastSnapshot {
        let layer_count = self.simulcast_layer_count.load(Ordering::Relaxed);
        let active = self.active_layer_count.load(Ordering::Relaxed);
        let layer_bitrates_kbps = self.simulcast_layer_bitrates_kbps
            [..layer_count.min(SIMULCAST_MAX_LAYERS)]
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .collect();
        SimulcastSnapshot {
            is_simulcast: self.is_simulcast.load(Ordering::Relaxed),
            layer_count,
            active,
            layer_bitrates_kbps,
        }
    }

    /// Monotonic counter bumped whenever the active layer count OR any per-layer
    /// target bitrate changes (issue #1083 V21). Producers poll this each
    /// iteration and re-read [`Self::simulcast_snapshot`] only when it changed.
    /// `Acquire` so the snapshot fields written before the writer's `Release`
    /// fetch-add are visible.
    pub fn simulcast_epoch(&self) -> u64 {
        self.simulcast_epoch.load(Ordering::Acquire)
    }

    /// Publish the inner controller's current simulcast state into the snapshot
    /// atomics, bumping [`simulcast_epoch`](Self::simulcast_epoch) iff something
    /// actually changed. Acquires the controller lock; used by the
    /// initialization-time [`set_simulcast_layers`](Self::set_simulcast_layers).
    fn publish_simulcast_snapshot(&self) {
        let ctrl = self.lock_ctrl();
        self.publish_simulcast_snapshot_locked(&ctrl);
    }

    /// Publish the controller's simulcast state given an already-held lock guard
    /// (the `tick` hot path holds it). Bumps `simulcast_epoch` only on a real
    /// change so the producer poll re-reads at most once per genuine shed/restore
    /// or per-layer-bitrate change. INFO-logs a shed/restore and a budget-cap
    /// rescale (the cluster validation run reads these) — issue #1083 V21.
    fn publish_simulcast_snapshot_locked(&self, ctrl: &EncoderBitrateController) {
        let is_simulcast = ctrl.is_simulcast();
        self.is_simulcast.store(is_simulcast, Ordering::Relaxed);
        if !is_simulcast {
            // Single-stream: keep the snapshot at the inert 1-layer default and
            // never bump the epoch, so producers on the legacy path never see a
            // simulcast change.
            return;
        }

        let layer_count = ctrl.simulcast_layer_count();
        let active = ctrl.active_layer_count();
        let targets = ctrl.layer_target_bitrates_kbps();

        let prev_active = self.active_layer_count.swap(active, Ordering::Relaxed);
        self.simulcast_layer_count
            .store(layer_count, Ordering::Relaxed);

        let mut bitrate_changed = false;
        for (i, slot) in self
            .simulcast_layer_bitrates_kbps
            .iter()
            .enumerate()
            .take(layer_count.min(SIMULCAST_MAX_LAYERS))
        {
            // Round the f64 target to whole kbps for the atomic + the encoder
            // (`update_bitrate_kbps` takes a u32).
            let kbps = targets.get(i).map(|&b| b.round() as u32).unwrap_or(0);
            let prev = slot.swap(kbps, Ordering::Relaxed);
            if prev != kbps {
                bitrate_changed = true;
            }
        }

        let active_changed = prev_active != active;
        if active_changed {
            // INFO so a cluster validation run can grep the shed/restore moment
            // and the per-layer kbps it settled to (issue #1083 V21).
            let per_layer: Vec<u32> = self.simulcast_layer_bitrates_kbps
                [..layer_count.min(SIMULCAST_MAX_LAYERS)]
                .iter()
                .map(|a| a.load(Ordering::Relaxed))
                .collect();
            let direction = if active < prev_active {
                "SHED"
            } else {
                "RESTORE"
            };
            let sum_active: u32 = per_layer.iter().take(active).sum();
            info!(
                "BotAq: simulcast layer {} {} -> {} active of {} (per-layer kbps={:?}, active-sum={}kbps)",
                direction, prev_active, active, layer_count, per_layer, sum_active,
            );
        } else if bitrate_changed {
            // Budget cap rescaled the per-layer targets without changing the
            // active count (e.g. cap_layers_to_budget engaged). Log at INFO so
            // the V21 cap-tracking assertion has a line to read.
            let per_layer: Vec<u32> = self.simulcast_layer_bitrates_kbps
                [..layer_count.min(SIMULCAST_MAX_LAYERS)]
                .iter()
                .map(|a| a.load(Ordering::Relaxed))
                .collect();
            let sum_active: u32 = per_layer.iter().take(active).sum();
            info!(
                "BotAq: simulcast per-layer target rescale (active={} of {}, per-layer kbps={:?}, active-sum={}kbps)",
                active, layer_count, per_layer, sum_active,
            );
        }

        if active_changed || bitrate_changed {
            // Release fetch-add so a producer that later does an Acquire load of
            // `simulcast_epoch` observes every field stored above.
            self.simulcast_epoch.fetch_add(1, Ordering::Release);
        }
    }

    /// Current video-tier index (0 = highest, VIDEO_QUALITY_TIERS.len()-1 = lowest).
    pub fn video_tier_index(&self) -> u32 {
        self.video_tier_index.load(Ordering::Relaxed)
    }

    /// Current audio-tier index.
    pub fn audio_tier_index(&self) -> u32 {
        self.audio_tier_index.load(Ordering::Relaxed)
    }

    /// Monotonically-increasing counter. Producers compare this against a
    /// local value to decide whether to re-snapshot encoder settings.
    ///
    /// Uses `Acquire` ordering so that once a reader observes an epoch value
    /// greater than N, every snapshot field written before the writer's
    /// `Release` fetch-add that produced that epoch is guaranteed to be
    /// visible via subsequent `Relaxed` loads on `video_*` / `audio_*`
    /// atomics.
    pub fn tier_epoch(&self) -> u64 {
        self.tier_epoch.load(Ordering::Acquire)
    }

    /// PID-adjusted target bitrate in kbps (for health reporting).
    pub fn last_target_bitrate_kbps(&self) -> f32 {
        f32::from_bits(self.last_target_bitrate_kbps_bits.load(Ordering::Relaxed))
    }

    /// Last observed p75 received FPS as seen by the PID (for health reporting).
    pub fn last_p75_peer_fps(&self) -> f32 {
        f32::from_bits(self.last_p75_peer_fps_bits.load(Ordering::Relaxed))
    }

    /// Snapshot the climb-rate limiter state for health reporting.
    ///
    /// Returns `(crash_ceiling_active, crash_ceiling_tier_index, crash_ceiling_decay_ms,
    /// step_up_blocked_ceiling, step_up_blocked_slowdown, step_up_blocked_screen_share)`.
    pub fn snapshot_climb_limiter(&self) -> (bool, Option<u32>, Option<f64>, u64, u64, u64) {
        let ctrl = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!(
                    "BotAq: controller mutex poisoned on snapshot_climb_limiter, recovering: {e}"
                );
                e.into_inner()
            }
        };
        let (blocked_ceiling, blocked_slowdown, blocked_screen) = ctrl.step_up_blocked_counts();
        match ctrl.crash_ceiling_info() {
            Some((idx, _label, decay_ms)) => (
                true,
                Some(idx as u32),
                Some(decay_ms),
                blocked_ceiling,
                blocked_slowdown,
                blocked_screen,
            ),
            None => (
                false,
                None,
                None,
                blocked_ceiling,
                blocked_slowdown,
                blocked_screen,
            ),
        }
    }

    /// Drain accumulated tier dwell-time samples for health reporting.
    ///
    /// Returns `Vec<(tier_label, dwell_ms)>`. The inner controller accumulates
    /// dwell samples on every tier transition; draining once per health tick
    /// matches the browser's pattern.
    pub fn drain_dwell_samples(&self) -> Vec<(&'static str, f64)> {
        let mut ctrl = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!("BotAq: controller mutex poisoned on drain_dwell_samples, recovering: {e}");
                e.into_inner()
            }
        };
        ctrl.drain_dwell_samples()
    }

    /// Access the target-FPS atomic — the PID uses this as its setpoint.
    /// Bots currently never mutate it after construction, but exposing it
    /// keeps the door open for a future "bot follows tier target_fps" mode.
    #[allow(dead_code)]
    pub fn target_fps_shared(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.target_fps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_aq::TestClock;

    #[test]
    fn initial_snapshot_matches_default_tier() {
        let aq = BotAq::new(Arc::new(TestClock::new(0)));
        let v = aq.snapshot_video();
        let expected = &VIDEO_QUALITY_TIERS[DEFAULT_VIDEO_TIER_INDEX];
        assert_eq!(v.bitrate_kbps, expected.ideal_bitrate_kbps);
        assert_eq!(v.max_width, expected.max_width);
        assert_eq!(v.max_height, expected.max_height);
        assert_eq!(v.target_fps, expected.target_fps);
        assert_eq!(v.keyframe_interval, expected.keyframe_interval_frames);
        assert_eq!(aq.video_tier_index(), DEFAULT_VIDEO_TIER_INDEX as u32);
        assert_eq!(aq.tier_epoch(), 0);
    }

    /// Issue #1108: with no injected encoder backpressure (the bot's normal
    /// state — no WebCodecs encoder), ticking must never change the tier.
    #[test]
    fn ticking_with_no_backpressure_keeps_tier_stable() {
        let clock = Arc::new(TestClock::new(0));
        let aq = BotAq::new(clock.clone() as Arc<dyn Clock>);
        // Walk well past warmup and tick many times at the default (0) depth.
        for i in 0..30 {
            clock.set_ms(10_000 + i * 1_000);
            aq.tick();
        }
        assert_eq!(
            aq.tier_epoch(),
            0,
            "no tier change expected when the bot reports no backpressure"
        );
        assert_eq!(aq.video_tier_index(), DEFAULT_VIDEO_TIER_INDEX as u32);
    }

    #[test]
    fn snapshot_audio_returns_initial_audio_tier() {
        let aq = BotAq::new(Arc::new(TestClock::new(0)));
        let a = aq.snapshot_audio();
        // Audio default is the highest tier (index 0, "high").
        assert_eq!(a.bitrate_kbps, AUDIO_QUALITY_TIERS[0].bitrate_kbps);
        assert_eq!(a.fec, AUDIO_QUALITY_TIERS[0].enable_fec);
        assert_eq!(a.dtx, AUDIO_QUALITY_TIERS[0].enable_dtx);
    }

    // ---------------------------------------------------------------------
    // Simulcast AQ wiring (issue #1083 V21)
    // ---------------------------------------------------------------------

    /// `set_simulcast_layers(1)` must leave the bot in SINGLE-STREAM mode —
    /// the N==1 legacy path must be byte-identical (no simulcast snapshot, no
    /// epoch bump). Fails if a future edit accidentally enables simulcast for
    /// n<2.
    #[test]
    fn set_simulcast_layers_n1_stays_single_stream() {
        let aq = BotAq::new(Arc::new(TestClock::new(0)));
        let before_epoch = aq.simulcast_epoch();
        aq.set_simulcast_layers(1);
        let snap = aq.simulcast_snapshot();
        assert!(!snap.is_simulcast, "n=1 must NOT enable simulcast");
        assert_eq!(snap.active, 1, "single-stream active count is 1");
        assert_eq!(snap.layer_count, 1, "single-stream layer count is 1");
        assert_eq!(
            aq.simulcast_epoch(),
            before_epoch,
            "n=1 must not bump the simulcast epoch"
        );
    }

    /// `set_simulcast_layers(3)` must enable simulcast with ALL 3 layers active
    /// immediately (full-ladder / shed-only — the bot does NOT use the browser
    /// start-at-base ramp). The per-layer targets must be populated from the
    /// ladder (lowest-first [low, standard, hd] ideals, possibly budget-capped).
    /// Fails if the wiring forgot to call the controller or the snapshot is not
    /// published.
    #[test]
    fn set_simulcast_layers_n3_full_ladder_active() {
        let aq = BotAq::new(Arc::new(TestClock::new(0)));
        aq.set_simulcast_layers(3);
        let snap = aq.simulcast_snapshot();
        assert!(snap.is_simulcast, "n=3 must enable simulcast");
        assert_eq!(snap.layer_count, 3, "full ladder ceiling is 3");
        assert_eq!(
            snap.active, 3,
            "bot starts at the FULL ladder (shed-only), not start-at-base"
        );
        assert_eq!(
            snap.layer_bitrates_kbps.len(),
            3,
            "three per-layer targets must be published"
        );
        // Lowest layer first; each target is positive and the base is the
        // smallest (low < hd). Budget cap may scale them but never below floor.
        assert!(
            snap.layer_bitrates_kbps.iter().all(|&b| b > 0),
            "every active layer must have a positive target: {:?}",
            snap.layer_bitrates_kbps
        );
        assert!(
            snap.layer_bitrates_kbps[0] <= snap.layer_bitrates_kbps[2],
            "base layer target must not exceed the top layer target: {:?}",
            snap.layer_bitrates_kbps
        );
    }

    /// The SUM of the active-layer targets must track the uplink budget for the
    /// active count (cap engaged) — V21 item 1. The 3-layer budget is
    /// Σ ideal = 400+900+1500 = 2800 kbps; `cap_layers_to_budget` is a no-op
    /// when the per-layer ideals already sum to the budget, so the published
    /// sum must equal that budget. Fails if the budget cap path is unreachable
    /// (e.g. `set_simulcast_layers` never called on the controller, so
    /// `layer_tiers` stayed empty).
    #[test]
    fn active_layer_sum_tracks_uplink_budget() {
        use videocall_aq::constants::{simulcast_layers, uplink_budget_kbps};
        let clock = Arc::new(TestClock::new(0));
        let aq = BotAq::new(clock.clone() as Arc<dyn Clock>);
        aq.set_simulcast_layers(3);
        // Tick once so the controller computes per-layer targets.
        clock.set_ms(1_000);
        aq.tick();
        let snap = aq.simulcast_snapshot();
        let sum: u32 = snap
            .layer_bitrates_kbps
            .iter()
            .take(snap.active)
            .copied()
            .sum();
        let budget = uplink_budget_kbps(simulcast_layers(3), snap.active);
        // Allow ±3 kbps for the per-layer f64->u32 rounding across 3 layers.
        assert!(
            (sum as f64 - budget).abs() <= 3.0,
            "active-layer target sum {sum} must track the uplink budget {budget} for {} active layers",
            snap.active
        );
    }

    /// Sustained uplink saturation (the bot's honest uplink-squeeze signal) fed
    /// via `observe_uplink_saturation` must drive the AQ to SHED the top layer:
    /// active count must drop below the full ladder while the base layer (id 0)
    /// keeps flowing — V21 item 2. This is the core behavior the validation row
    /// exists to prove. Fails if the saturation signal does not reach the
    /// controller's shed path (e.g. depth stays 0, or simulcast not enabled).
    #[test]
    fn sustained_uplink_saturation_sheds_top_layer() {
        let clock = Arc::new(TestClock::new(0));
        let aq = BotAq::new(clock.clone() as Arc<dyn Clock>);
        aq.set_simulcast_layers(3);

        // Walk past warmup with NO saturation (cumulative counter flat at 0).
        let mut t: u64 = 0;
        for _ in 0..8 {
            t += 1_000;
            clock.set_ms(t);
            aq.observe_uplink_saturation(0);
            aq.tick();
        }
        let active_before = aq.simulcast_snapshot().active;
        assert_eq!(active_before, 3, "precondition: full ladder before squeeze");

        // Now simulate a sustained uplink squeeze: the cumulative bandwidth-wait
        // counter climbs every interval (each tick sees a positive delta), which
        // maps to depth >= HIGH and arms the controller's sustained-shed timer.
        let mut cumulative_wait_us: u64 = 0;
        let mut shed = false;
        for _ in 0..30 {
            t += 1_000;
            clock.set_ms(t);
            cumulative_wait_us += 50_000; // monotonic climb => positive delta each tick
            aq.observe_uplink_saturation(cumulative_wait_us);
            aq.tick();
            if aq.simulcast_snapshot().active < active_before {
                shed = true;
                break;
            }
        }
        let snap = aq.simulcast_snapshot();
        assert!(
            shed && snap.active < active_before,
            "sustained uplink saturation must shed the top layer (active {} -> {})",
            active_before,
            snap.active
        );
        assert!(
            snap.active >= 1,
            "the base layer must always keep flowing (active never below 1)"
        );
    }

    /// Negative control for the shed test: with the cumulative bandwidth-wait
    /// counter FLAT (no new saturation), the AQ must NOT shed — the full ladder
    /// stays active. This proves the shed in the test above is caused by the
    /// saturation signal, not by the mere passage of time / ticking. Fails if
    /// `observe_uplink_saturation` fabricated backpressure even with zero delta.
    /// This is the in-AQ analog of the netsim crate's latency-only negative
    /// control (`bandwidth_wait_us_flat_under_latency_only`): a flat counter is
    /// exactly what a pure latency/jitter profile produces.
    #[test]
    fn no_new_uplink_saturation_keeps_full_ladder() {
        let clock = Arc::new(TestClock::new(0));
        let aq = BotAq::new(clock.clone() as Arc<dyn Clock>);
        aq.set_simulcast_layers(3);
        let mut t: u64 = 0;
        // A non-zero but CONSTANT cumulative counter: delta is 0 every tick.
        // (This is what a pure latency/jitter profile yields — bandwidth-wait
        // never advances because the token bucket is never in deficit.)
        for _ in 0..40 {
            t += 1_000;
            clock.set_ms(t);
            aq.observe_uplink_saturation(500_000); // constant => no new saturation
            aq.tick();
        }
        assert_eq!(
            aq.simulcast_snapshot().active,
            3,
            "a flat bandwidth-wait counter (no new saturation) must NOT shed any layer"
        );
    }
}
