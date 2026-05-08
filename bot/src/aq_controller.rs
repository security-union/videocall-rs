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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use videocall_aq::constants::{AUDIO_QUALITY_TIERS, DEFAULT_VIDEO_TIER_INDEX, VIDEO_QUALITY_TIERS};
use videocall_aq::{default_clock, Clock, EncoderBitrateController, TierTransitionRecord};
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;

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
    /// Target FPS shared with the inner PID controller. The bot currently
    /// keeps this fixed after construction — changing the `target_fps` atomic
    /// would also change the controller's setpoint.
    target_fps: Arc<AtomicU32>,

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

    // --- PID-derived telemetry for health reporting ---
    /// PID-adjusted target bitrate in kbps, f32 bits packed into u32.
    last_target_bitrate_kbps_bits: AtomicU32,
    /// Last worst-peer (p75) FPS as observed by the PID, f32 bits.
    last_worst_peer_fps_bits: AtomicU32,
    /// fps_ratio = received / target, f32 bits.
    last_fps_ratio_bits: AtomicU32,
    /// bitrate_ratio = pid_clamped / tier_ideal, f32 bits.
    last_bitrate_ratio_bits: AtomicU32,

    /// Optional Prometheus metrics handle + pre-built label pair.
    ///
    /// When set, every [`Self::process_diagnostics`] call refreshes the
    /// `bot_aq_*` gauges. Kept behind the `metrics` feature so non-metrics
    /// builds do not pay the extra field or any storage for the labels.
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

        // Use the initial tier's ideal bitrate as the PID's anchor so the
        // first correction does not over- or under-shoot.
        let controller = EncoderBitrateController::with_clock(
            initial_video.ideal_bitrate_kbps,
            Arc::clone(&target_fps),
            clock,
        );

        // Seed audio snapshot from the controller's current audio tier.
        let initial_audio = controller.current_audio_tier();

        let bot_aq = Self {
            inner: Mutex::new(controller),
            target_fps,
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
            last_target_bitrate_kbps_bits: AtomicU32::new(
                (initial_video.ideal_bitrate_kbps as f32).to_bits(),
            ),
            last_worst_peer_fps_bits: AtomicU32::new(0),
            last_fps_ratio_bits: AtomicU32::new(0),
            last_bitrate_ratio_bits: AtomicU32::new(0),
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

    /// Feed a diagnostics packet into the controller. If the tier changes as
    /// a result, re-publish the new tier snapshot and bump the epoch.
    pub fn process_diagnostics(&self, pkt: DiagnosticsPacket) {
        let mut ctrl = match self.inner.lock() {
            Ok(g) => g,
            // Mutex is only poisoned on panic; AQ is non-critical, so log and
            // continue rather than propagating the panic further.
            Err(e) => {
                warn!("BotAq: controller mutex poisoned, recovering: {e}");
                e.into_inner()
            }
        };

        let _ = ctrl.process_diagnostics_packet(pkt);

        // Always publish the latest PID-derived telemetry — these are useful
        // for health reporting even when the tier has not changed.
        let target_bitrate = ctrl.last_target_bitrate_kbps() as f32;
        let worst_fps = ctrl.last_worst_peer_fps() as f32;
        let fps_ratio = ctrl.last_fps_ratio() as f32;
        let bitrate_ratio = ctrl.last_bitrate_ratio() as f32;
        self.last_target_bitrate_kbps_bits
            .store(target_bitrate.to_bits(), Ordering::Relaxed);
        self.last_worst_peer_fps_bits
            .store(worst_fps.to_bits(), Ordering::Relaxed);
        self.last_fps_ratio_bits
            .store(fps_ratio.to_bits(), Ordering::Relaxed);
        self.last_bitrate_ratio_bits
            .store(bitrate_ratio.to_bits(), Ordering::Relaxed);

        // Refresh per-scrape Prometheus gauges. Done on every packet so that
        // even without a tier change, the dashboard sees live fps_ratio /
        // bitrate_ratio / target_bitrate values.
        #[cfg(feature = "metrics")]
        self.publish_live_metrics(
            target_bitrate,
            worst_fps,
            fps_ratio,
            bitrate_ratio,
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
        worst_peer_fps: f32,
        fps_ratio: f32,
        bitrate_ratio: f32,
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
            .aq_worst_peer_fps
            .with_label_values(&labels)
            .set(worst_peer_fps as f64);
        binding
            .metrics
            .aq_fps_ratio
            .with_label_values(&labels)
            .set(fps_ratio as f64);
        binding
            .metrics
            .aq_bitrate_ratio
            .with_label_values(&labels)
            .set(bitrate_ratio as f64);
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

    /// Last observed worst-peer (p75) FPS as seen by the PID (for health reporting).
    pub fn last_worst_peer_fps(&self) -> f32 {
        f32::from_bits(self.last_worst_peer_fps_bits.load(Ordering::Relaxed))
    }

    /// Last fps_ratio (received / target) for health reporting.
    pub fn last_fps_ratio(&self) -> f32 {
        f32::from_bits(self.last_fps_ratio_bits.load(Ordering::Relaxed))
    }

    /// Last bitrate_ratio (pid_clamped / tier_ideal) for health reporting.
    pub fn last_bitrate_ratio(&self) -> f32 {
        f32::from_bits(self.last_bitrate_ratio_bits.load(Ordering::Relaxed))
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
    use videocall_types::protos::diagnostics_packet::{DiagnosticsPacket, VideoMetrics};
    use videocall_types::protos::media_packet::media_packet::MediaType;

    fn make_packet(target_id: &str, fps: f32) -> DiagnosticsPacket {
        let mut packet = DiagnosticsPacket::new();
        packet.sender_id = "peer-sender".to_string();
        packet.target_id = target_id.to_string();
        packet.media_type = MediaType::VIDEO.into();
        let mut video_metrics = VideoMetrics::new();
        video_metrics.fps_received = fps;
        video_metrics.bitrate_kbps = 500;
        packet.video_metrics = ::protobuf::MessageField::some(video_metrics);
        packet
    }

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

    #[test]
    fn good_fps_keeps_tier_stable() {
        let aq = BotAq::new(Arc::new(TestClock::new(0)));
        // A handful of good FPS packets should not trigger a tier change.
        for i in 0..5 {
            let mut p = make_packet("peer1", 25.0);
            p.timestamp_ms = i as u64;
            aq.process_diagnostics(p);
        }
        assert_eq!(
            aq.tier_epoch(),
            0,
            "no tier change expected under good conditions"
        );
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
}
