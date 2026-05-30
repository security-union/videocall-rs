// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-peer signal quality tracking and popup chart.
//!
//! [`PeerSignalHistory`] collects periodic quality samples and derives a
//! [`SignalLevel`] that drives the [`SignalBarsIcon`] overlay on each tile.
//! [`SignalQualityPopup`] renders a scrollable SVG line chart of the history
//! with separate lines for audio, video, screen share, and latency.

use std::collections::VecDeque;

use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use wasm_bindgen::JsCast;

use crate::theme::color as theme_color;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Scope filter for [`SignalQualityPopup`].
///
/// Drives which metric series (audio, video, screen) the popup renders in the
/// chart, legend, and tooltip. Introduced for HCL bug #2 so the LEFT panel of
/// the screen-share split layout can show ONLY the screen-share series, and
/// peer tiles can suppress the screen-share series (the shared-content tile
/// has its own popup for that).
///
/// Two distinct popups can therefore be open simultaneously for the same
/// publisher: one anchored to the screen-share tile (`ScreenOnly`) and one
/// anchored to that peer's video tile (`NoScreen`). The popup-state map keys
/// on `(peer_id, mode)` so they coexist without colliding.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub enum SignalMeterMode {
    /// Show every metric series the sample has data for. Used by legacy
    /// callers (e.g. the diagnostics overview) where the popup isn't paired
    /// with a specific tile mode.
    #[default]
    Full,
    /// Show ONLY the screen-share series. Used by the shared-content tile in
    /// the split layout — clicking its signal-meter icon must surface only the
    /// screen-share metric, not camera or audio.
    ScreenOnly,
    /// Show audio + video, hide the screen-share series. Originally used
    /// by peer tiles to suppress double-rendering of the screen-share
    /// metric (the LEFT-panel `ScreenOnly` popup was the dedicated
    /// source). The peer-tile default has since been moved back to
    /// `Full` so the peer-tile popup surfaces screen-share metrics when
    /// the peer starts sharing — `has_screen_data` already gates the
    /// Screen legend / tooltip line on samples actually carrying
    /// `screen_enabled == true`, so the suppression is a no-op for
    /// non-sharing peers and was hiding live data for sharing peers
    /// (caught by `peer-screen-diagnostics` / `peer-screen-static-fps`
    /// E2Es). The variant is retained so external callers / future
    /// surface areas (e.g. a settings preference) can opt back in
    /// without breaking the popup-state-map key shape.
    #[allow(dead_code)]
    NoScreen,
}

impl SignalMeterMode {
    /// Whether this mode renders the audio series.
    pub fn shows_audio(self) -> bool {
        matches!(self, Self::Full | Self::NoScreen)
    }

    /// Whether this mode renders the camera-video series.
    pub fn shows_video(self) -> bool {
        matches!(self, Self::Full | Self::NoScreen)
    }

    /// Whether this mode renders the screen-share series.
    pub fn shows_screen(self) -> bool {
        matches!(self, Self::Full | Self::ScreenOnly)
    }

    /// Short stable string for DOM ids. The popup-state map keys on
    /// `(peer_id, mode)` and we serialise both halves into the DOM id so
    /// CSS / Playwright selectors can target each popup independently.
    pub fn id_suffix(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ScreenOnly => "screen",
            Self::NoScreen => "peer",
        }
    }
}

/// Per-popup persistent state, lifted out of the per-tile `PeerTile` so
/// mid-meeting peer leaves / layout switches don't unmount the popup
/// containers and close every other popup (HCL bug #8). Stored in a
/// context-provided `Signal<HashMap<(peer_id, mode), SignalPopupState>>`
/// so multiple popups can coexist independently.
///
/// `position` honours the drag-and-drop rule from HCL bug #9: when the user
/// drags a popup, we transition from `Anchored` (auto-follow the tile) to
/// `Free` (fixed viewport coordinates). The popup's header carries a 📌
/// reanchor button that resets the state back to `Anchored`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SignalPopupState {
    pub position: SignalPopupPosition,
}

impl Default for SignalPopupState {
    fn default() -> Self {
        Self {
            position: SignalPopupPosition::Anchored,
        }
    }
}

/// Viewport-space position for a signal-quality popup. `Anchored` reads the
/// tile's `getBoundingClientRect()` and tracks the tile through layout
/// reflows; `Free { left, top }` pins the popup to fixed viewport
/// coordinates regardless of where the tile moves (drag-and-drop result).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SignalPopupPosition {
    Anchored,
    Free { left: f64, top: f64 },
}

impl SignalPopupPosition {
    /// Convenience predicate. Currently unused by the popup itself (the
    /// `data-anchor-mode` attribute on the popup DOM element is the source of
    /// truth for `reposition_popup`), but exposed for tests and downstream
    /// callers that want to inspect the state.
    #[allow(dead_code)]
    pub fn is_free(self) -> bool {
        matches!(self, Self::Free { .. })
    }
}

/// Discrete signal quality level shown as 0-5 filled bars.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SignalLevel {
    /// 5 bars -- excellent quality
    #[default]
    Excellent,
    /// 4 bars -- good quality
    Good,
    /// 3 bars -- fair quality
    Fair,
    /// 2 bars -- poor quality
    Poor,
    /// 1 bar -- bad quality
    Bad,
    /// 0 bars with red slash -- connection lost
    Lost,
}

impl SignalLevel {
    /// Number of filled bars for this level (0..=5).
    pub fn bars(self) -> u8 {
        match self {
            Self::Excellent => 5,
            Self::Good => 4,
            Self::Fair => 3,
            Self::Poor => 2,
            Self::Bad => 1,
            Self::Lost => 0,
        }
    }

    /// Whether the signal is completely lost.
    pub fn is_lost(self) -> bool {
        self == Self::Lost
    }

    /// Derive a signal level from a combined quality score (0.0 -- 1.0).
    pub fn from_quality(q: f64) -> Self {
        if q >= 0.9 {
            Self::Excellent
        } else if q >= 0.75 {
            Self::Good
        } else if q >= 0.5 {
            Self::Fair
        } else if q >= 0.25 {
            Self::Poor
        } else if q > 0.0 {
            Self::Bad
        } else {
            Self::Lost
        }
    }
}

/// A single quality measurement at a point in time.
#[derive(Clone, Debug)]
pub struct SignalSample {
    /// Milliseconds since epoch (`js_sys::Date::now()`).
    pub timestamp_ms: f64,
    // Normalized quality scores (0.0-1.0)
    pub audio_quality: f64,
    pub video_quality: f64,
    pub screen_quality: f64,
    // Raw video metrics
    pub video_fps: f64,
    pub video_bitrate_kbps: f64,
    /// Video resolution as "WxH" (e.g. "1280x720"), empty when unknown.
    pub video_resolution: String,
    // Raw audio metrics
    pub audio_bitrate_kbps: f64,
    pub audio_expand_rate: f64, // per-mille
    pub audio_buffer_ms: f64,
    // Raw screen share metrics
    pub screen_enabled: bool,
    pub screen_fps: f64,
    pub screen_bitrate_kbps: f64,
    /// Screen-share **received** resolution as "WxH" (e.g. "1920x1080"), i.e.
    /// the dimensions of the decoded canvas. Empty when unknown.
    pub screen_resolution: String,
    /// Screen-share **source** resolution as "WxH" — the publisher's native
    /// `MediaStreamTrack.getSettings()` capture dimensions. Empty when the
    /// publisher doesn't report it (older clients) or hasn't started sharing
    /// yet. When this differs from `screen_resolution` the publisher's
    /// encoder downscaled in transit.
    pub screen_source_resolution: String,
    /// Issue #903: publisher's encoder *target* bitrate for the screen-share // @token-exempt: issue ref, not a color
    /// track (kbps). What the encoder is currently trying to produce, not
    /// the realised on-the-wire bitrate (which is `screen_bitrate_kbps`).
    /// `0` means the publisher hasn't stamped the field — older client or
    /// AQ tier 0 (unconstrained). The Cause tooltip line is omitted in
    /// either case.
    pub screen_encoder_target_bitrate_kbps: u32,
    /// Issue #903: name of the adaptive-quality tier currently constraining // @token-exempt: issue ref, not a color
    /// the publisher's screen-share encoder (e.g. `"high"`, `"medium"`,
    /// `"low"`). Empty when AQ isn't engaged or the publisher is older.
    pub screen_adaptive_tier: String,
    /// Issue #903: short publisher-classified cause of the downscale, // @token-exempt: issue ref, not a color
    /// one of `"bitrate-limited"`, `"cpu-pressure"`, `"network-rtt"`,
    /// `"network-loss"`, `"manual-cap"`, or empty. Empty means the encoder
    /// is unconstrained or the publisher doesn't supply the field.
    pub screen_cause_hint: String,
    /// Issue #906: held-last value to render when the current screen FPS // @token-exempt: issue ref, not a color
    /// sample reads zero but a recent non-zero value exists. `None` means
    /// either the current sample is itself live (use `screen_fps`) or the
    /// held window has expired / no prior live value exists (render the
    /// raw zero with a `(no frames)` annotation).
    ///
    /// Populated by [`PeerSignalHistory::push_sample_at`] at sample-record
    /// time so the tooltip and chart renderers don't have to rescan the
    /// VecDeque on every paint.
    pub screen_fps_held: Option<f64>,
    /// Issue #906: held-last value for screen bitrate, paired with // @token-exempt: issue ref, not a color
    /// `screen_fps_held`. Tracked independently because the two metrics
    /// could in principle drop to zero on different cadences, though in
    /// practice they correlate (encoder emits or doesn't).
    pub screen_bitrate_kbps_held: Option<f64>,
    /// Issue #906: milliseconds since the last `peer_status` heartbeat // @token-exempt: issue ref, not a color
    /// from the peer at the time this sample was recorded. `None` means
    /// no heartbeat has been observed yet (very early in the connection).
    /// Used by the screen-state classifier to distinguish a `(static)`
    /// publisher (fresh heartbeat, zero metrics) from a `(no frames)`
    /// publisher (stale heartbeat, the connection is the problem).
    pub peer_status_age_ms: Option<f64>,
    // Latency
    pub latency_ms: f64,
}

// Manual PartialEq for SignalSample so the Props derive works.
impl PartialEq for SignalSample {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp_ms == other.timestamp_ms
            && self.audio_quality == other.audio_quality
            && self.video_quality == other.video_quality
            && self.screen_quality == other.screen_quality
            && self.video_fps == other.video_fps
            && self.video_bitrate_kbps == other.video_bitrate_kbps
            && self.video_resolution == other.video_resolution
            && self.audio_bitrate_kbps == other.audio_bitrate_kbps
            && self.audio_expand_rate == other.audio_expand_rate
            && self.audio_buffer_ms == other.audio_buffer_ms
            && self.screen_enabled == other.screen_enabled
            && self.screen_fps == other.screen_fps
            && self.screen_bitrate_kbps == other.screen_bitrate_kbps
            && self.screen_resolution == other.screen_resolution
            && self.screen_source_resolution == other.screen_source_resolution
            && self.screen_encoder_target_bitrate_kbps == other.screen_encoder_target_bitrate_kbps
            && self.screen_adaptive_tier == other.screen_adaptive_tier
            && self.screen_cause_hint == other.screen_cause_hint
            && self.screen_fps_held == other.screen_fps_held
            && self.screen_bitrate_kbps_held == other.screen_bitrate_kbps_held
            && self.peer_status_age_ms == other.peer_status_age_ms
            && self.latency_ms == other.latency_ms
    }
}

/// Builder struct for passing raw metrics to [`PeerSignalHistory::push_sample`].
/// Keeps the argument list manageable when many metrics are tracked.
#[derive(Clone, Debug, Default)]
pub struct SampleData {
    pub video_fps: f64,
    pub video_bitrate_kbps: f64,
    /// Video resolution as "WxH", empty when unknown.
    pub video_resolution: String,
    pub audio_bitrate_kbps: f64,
    pub audio_expand_rate: f64,
    pub audio_buffer_ms: f64,
    pub screen_enabled: bool,
    pub screen_fps: f64,
    pub screen_bitrate_kbps: f64,
    /// Screen-share **received** resolution as "WxH", empty when unknown.
    pub screen_resolution: String,
    /// Screen-share **source** resolution as "WxH" — publisher's native
    /// capture size as reported on the wire. Empty when the publisher
    /// doesn't report it or hasn't been seen yet.
    pub screen_source_resolution: String,
    /// Issue #903: publisher's encoder target bitrate for the screen-share // @token-exempt: issue ref, not a color
    /// track (kbps); `0` when the publisher doesn't supply the field.
    pub screen_encoder_target_bitrate_kbps: u32,
    /// Issue #903: name of the AQ tier currently constraining the // @token-exempt: issue ref, not a color
    /// publisher's screen-share encoder. Empty when AQ isn't engaged.
    pub screen_adaptive_tier: String,
    /// Issue #903: short publisher-classified cause of the downscale. // @token-exempt: issue ref, not a color
    /// Empty when the encoder is unconstrained or the publisher doesn't
    /// supply the field.
    pub screen_cause_hint: String,
    /// Issue #906: milliseconds since the most recent `peer_status` // @token-exempt: issue ref, not a color
    /// heartbeat from the peer at sample-record time. `None` when no
    /// heartbeat has been observed yet. Passed straight through onto
    /// `SignalSample.peer_status_age_ms` so the screen-state classifier
    /// can distinguish static publishers from broken connections.
    pub peer_status_age_ms: Option<f64>,
    pub latency_ms: f64,
    pub audio_enabled: bool,
    pub video_enabled: bool,
}

/// Maximum number of signal samples retained per peer.
/// At 1 sample/second this covers 30 minutes of history.
const MAX_SIGNAL_SAMPLES: usize = 1800;

// ---------------------------------------------------------------------------
// Issue #906: screen-share static-vs-no-frames classification.            // @token-exempt: issue ref, not a color
//
// During genuine static screen-share (no mouse motion, no UI changes) modern
// video codecs emit zero encoded frames — the publisher is healthy but the
// metrics read `0.0fps | 0kbps`, which is visually indistinguishable from a
// broken connection. The state machine below classifies each (peer, sample)
// into one of three states the tooltip and chart use to render correctly.
// ---------------------------------------------------------------------------

/// Maximum age (in milliseconds) of a prior non-zero screen FPS / bitrate
/// reading before we stop holding it. After this window elapses the sample
/// falls back to `NoFrames` regardless of heartbeat freshness — at that point
/// the encoder has been silent long enough that the held value is no longer
/// representative of what the publisher is doing now.
///
/// 30 seconds is the issue-spec value: long enough to bridge realistic
/// static periods (reading code, looking at a document) without latching the
/// held value indefinitely on stalled publishers.
pub(crate) const SCREEN_STATIC_HOLD_WINDOW_MS: f64 = 30_000.0;

/// Maximum `peer_status` age (in milliseconds) for the heartbeat to be
/// considered fresh enough to justify holding the screen metrics. The
/// `peer_status` event fires roughly every 1 second per peer, so 5s gives
/// 5 missed beats of tolerance for network jitter / packet reordering before
/// we conclude the publisher is unreachable.
///
/// Kept local to `signal_quality.rs` because the threshold is UI-policy, not
/// shared with the AQ controller's reaction windows in `videocall-aq`.
pub(crate) const SCREEN_STATIC_HEARTBEAT_FRESH_MS: f64 = 5_000.0;

/// Per-sample screen-state classification used by the tooltip and chart
/// renderers. Computed at render time from the sample's own held / heartbeat
/// fields so we don't have to re-scan history.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScreenSampleState {
    /// The encoder is actively emitting frames. Render `screen_fps` /
    /// `screen_bitrate_kbps` directly with no annotation.
    Live,
    /// The current sample reads zero, but a prior non-zero reading exists
    /// within the hold window AND the peer's heartbeat is fresh. Render
    /// the held values with a `(static)` annotation.
    Static {
        held_fps: f64,
        held_bitrate_kbps: f64,
    },
    /// Either the publisher has been silent for longer than the hold
    /// window, OR the peer's heartbeat is stale. Render `0fps` / `0kbps`
    /// with a `(no frames)` annotation — the connection or encoder is the
    /// problem, not a quiet desktop.
    NoFrames,
}

impl SignalSample {
    /// Classify the screen-share metric state for this sample.
    ///
    /// `Live` when either `screen_fps` or `screen_bitrate_kbps` reports a
    /// non-zero reading (we treat the two metrics as a single screen-level
    /// state — in practice they correlate, the encoder emits or doesn't).
    /// Otherwise we consult the held values + heartbeat staleness to
    /// distinguish static publishers from broken connections.
    pub(crate) fn screen_state(&self) -> ScreenSampleState {
        // Live: either metric reports a real reading.
        let live_fps = self.screen_fps > 0.0;
        let live_bitrate = self.screen_bitrate_kbps > 0.0;
        if live_fps || live_bitrate {
            return ScreenSampleState::Live;
        }

        // Both zero. Was the heartbeat fresh when we recorded the sample?
        // `None` means we haven't seen any peer_status yet — treat the same
        // as a stale heartbeat (we cannot prove the publisher is alive, so
        // we will not paper over the zero with held values).
        let heartbeat_fresh = match self.peer_status_age_ms {
            Some(age) => age <= SCREEN_STATIC_HEARTBEAT_FRESH_MS,
            None => false,
        };

        match (self.screen_fps_held, self.screen_bitrate_kbps_held) {
            // Recent non-zero values within the hold window AND heartbeat
            // fresh -> Static. We hold both metrics together; if only one
            // is set the other defaults to zero so the tooltip still
            // renders the available number.
            (Some(fps), bitrate) if heartbeat_fresh => ScreenSampleState::Static {
                held_fps: fps,
                held_bitrate_kbps: bitrate.unwrap_or(0.0),
            },
            (None, Some(bitrate)) if heartbeat_fresh => ScreenSampleState::Static {
                held_fps: 0.0,
                held_bitrate_kbps: bitrate,
            },
            // Anything else: no recent non-zero, heartbeat stale, or
            // neither held value is available. Render the raw zero with
            // the `(no frames)` annotation.
            _ => ScreenSampleState::NoFrames,
        }
    }
}

/// Accumulates [`SignalSample`]s for a single peer.  Uses a bounded
/// [`VecDeque`] so memory stays capped even for very long meetings or
/// tabs left open indefinitely.
#[derive(Clone, Debug, Default)]
pub struct PeerSignalHistory {
    samples: VecDeque<SignalSample>,
}

impl PeerSignalHistory {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
        }
    }

    /// Append a new sample, computing quality scores internally from raw
    /// metrics. Evicts the oldest sample when at capacity.
    pub fn push_sample(&mut self, data: &SampleData) {
        self.push_sample_at(data, js_sys::Date::now());
    }

    /// Append a sample with an explicit timestamp. Lets host unit tests
    /// exercise the quality-derivation logic without depending on `js_sys`.
    pub fn push_sample_at(&mut self, data: &SampleData, timestamp_ms: f64) {
        // Issue #906: before evicting / appending, scan recent history     // @token-exempt: issue ref, not a color
        // for the most recent non-zero screen FPS / bitrate values. We
        // only consult held values when the *current* reading is zero; for
        // live readings the held fields are `None`. Walking backwards is
        // O(window) but bounded by the 30s hold window divided by the
        // ~1s sample cadence, so in practice ~30 iterations max.
        let (screen_fps_held, screen_bitrate_kbps_held) =
            if data.screen_enabled && (data.screen_fps == 0.0 || data.screen_bitrate_kbps == 0.0) {
                find_recent_non_zero_screen_metrics(
                    &self.samples,
                    timestamp_ms,
                    SCREEN_STATIC_HOLD_WINDOW_MS,
                    data.screen_fps == 0.0,
                    data.screen_bitrate_kbps == 0.0,
                )
            } else {
                (None, None)
            };

        if self.samples.len() >= MAX_SIGNAL_SAMPLES {
            self.samples.pop_front();
        }

        // Video quality: fps as ratio of a 30fps target, clamped to 0-1.
        let video_quality = if data.video_enabled {
            (data.video_fps / 30.0).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Audio quality: composite of expand_rate and jitter buffer health.
        //
        // - expand_rate: 0 = perfect, 1000 = fully concealed -- weight 60%
        // - buffer_health: how close the buffer is to the optimal range --
        //   weight 40%. Buffer too large (>150ms) signals congestion/jitter;
        //   too small (<20ms) risks underrun.
        //
        // This composite ensures the audio line shows meaningful variation
        // based on buffer depth changes even when there is no packet loss.
        let audio_quality = if data.audio_enabled {
            let expand_score = (1.0 - (data.audio_expand_rate / 1000.0)).clamp(0.0, 1.0);
            let buffer_score = if data.audio_buffer_ms <= 0.0 {
                // No buffer data yet -- assume moderate health.
                0.5
            } else if data.audio_buffer_ms < 20.0 {
                // Buffer dangerously low -- risk of underrun.
                (data.audio_buffer_ms / 20.0).clamp(0.0, 1.0) * 0.8
            } else if data.audio_buffer_ms <= 150.0 {
                // Healthy range.
                1.0
            } else {
                // Buffer too large -- indicates network problems.
                (1.0 - ((data.audio_buffer_ms - 150.0) / 350.0)).clamp(0.0, 1.0)
            };
            (expand_score * 0.6 + buffer_score * 0.4).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Screen quality: fps as ratio of a 30fps target when screen share is active.
        let screen_quality = if data.screen_enabled {
            (data.screen_fps / 30.0).clamp(0.0, 1.0)
        } else {
            0.0
        };

        self.samples.push_back(SignalSample {
            timestamp_ms,
            audio_quality,
            video_quality,
            screen_quality,
            video_fps: data.video_fps,
            video_bitrate_kbps: data.video_bitrate_kbps,
            video_resolution: data.video_resolution.clone(),
            audio_bitrate_kbps: data.audio_bitrate_kbps,
            audio_expand_rate: data.audio_expand_rate,
            audio_buffer_ms: data.audio_buffer_ms,
            screen_enabled: data.screen_enabled,
            screen_fps: data.screen_fps,
            screen_bitrate_kbps: data.screen_bitrate_kbps,
            screen_resolution: data.screen_resolution.clone(),
            screen_source_resolution: data.screen_source_resolution.clone(),
            screen_encoder_target_bitrate_kbps: data.screen_encoder_target_bitrate_kbps,
            screen_adaptive_tier: data.screen_adaptive_tier.clone(),
            screen_cause_hint: data.screen_cause_hint.clone(),
            screen_fps_held,
            screen_bitrate_kbps_held,
            peer_status_age_ms: data.peer_status_age_ms,
            latency_ms: data.latency_ms,
        });
    }

    /// Return a `Vec` copy of the samples (for passing to the popup component).
    /// Callers should avoid calling this on every render -- only when the popup
    /// is actually visible.
    pub fn samples_vec(&self) -> Vec<SignalSample> {
        self.samples.iter().cloned().collect()
    }

    /// Derive the current signal level from the most recent sample.
    ///
    /// If audio or video is disabled for the peer the caller should set the
    /// corresponding quality to `None` when computing the combined score.
    pub fn current_level(
        &self,
        audio_enabled: bool,
        video_enabled: bool,
        screen_enabled: bool,
    ) -> SignalLevel {
        match self.samples.back() {
            Some(s) => {
                let combined = combined_quality(
                    s.audio_quality,
                    s.video_quality,
                    s.screen_quality,
                    audio_enabled,
                    video_enabled,
                    screen_enabled,
                );
                SignalLevel::from_quality(combined)
            }
            None => SignalLevel::Excellent, // no data yet -- assume good
        }
    }
}

/// Issue #906: walk the recorded samples backwards looking for the most // @token-exempt: issue ref, not a color
/// recent non-zero screen FPS and / or bitrate readings. Returns held values
/// only when the prior reading is within the supplied `hold_window_ms`. The
/// `want_fps` / `want_bitrate` flags let the caller skip the scan for whichever
/// metric is currently live — typically only one of the two drops to zero on
/// any given sample, though we handle both independently.
///
/// Iterating right-to-left is O(window) but bounded by the 30s hold window
/// divided by the ~1s sample cadence, so worst case ~30 iterations. We also
/// bail out early once both metrics have been resolved.
fn find_recent_non_zero_screen_metrics(
    samples: &VecDeque<SignalSample>,
    now_ms: f64,
    hold_window_ms: f64,
    want_fps: bool,
    want_bitrate: bool,
) -> (Option<f64>, Option<f64>) {
    let mut held_fps: Option<f64> = None;
    let mut held_bitrate: Option<f64> = None;
    let deadline = now_ms - hold_window_ms;
    for sample in samples.iter().rev() {
        // The sample's own timestamp must be inside the hold window. The
        // VecDeque is ordered oldest-first, so once we cross the deadline
        // all earlier samples are also too old.
        if sample.timestamp_ms < deadline {
            break;
        }
        // Recover the live value if the sample was itself live, OR walk
        // through samples that recorded a *held* value from earlier in the
        // chain — this keeps the held value latched across consecutive
        // zero samples without rescanning back to the original live one.
        if want_fps && held_fps.is_none() {
            if sample.screen_fps > 0.0 {
                held_fps = Some(sample.screen_fps);
            } else if let Some(h) = sample.screen_fps_held {
                held_fps = Some(h);
            }
        }
        if want_bitrate && held_bitrate.is_none() {
            if sample.screen_bitrate_kbps > 0.0 {
                held_bitrate = Some(sample.screen_bitrate_kbps);
            } else if let Some(h) = sample.screen_bitrate_kbps_held {
                held_bitrate = Some(h);
            }
        }
        let fps_done = !want_fps || held_fps.is_some();
        let bitrate_done = !want_bitrate || held_bitrate.is_some();
        if fps_done && bitrate_done {
            break;
        }
    }
    (held_fps, held_bitrate)
}

/// Compute a single combined quality score.
///
/// When multiple streams are enabled the score is the mean of the active ones.
/// When none are active we return 1.0 (peer has everything intentionally off --
/// not a quality problem).
fn combined_quality(
    audio: f64,
    video: f64,
    screen: f64,
    audio_en: bool,
    video_en: bool,
    screen_en: bool,
) -> f64 {
    let mut sum = 0.0;
    let mut count = 0;
    if audio_en {
        sum += audio;
        count += 1;
    }
    if video_en {
        sum += video;
        count += 1;
    }
    if screen_en {
        sum += screen;
        count += 1;
    }
    if count == 0 {
        1.0
    } else {
        sum / count as f64
    }
}

/// Convenience bundle passed through the rendering pipeline so we don't keep
/// adding individual arguments.
pub struct SignalInfo {
    pub level: SignalLevel,
    pub history: Vec<SignalSample>,
    /// Meeting start time (Unix ms) for the chart X-axis reference.
    pub meeting_start_ms: f64,
    /// Current transport string for this peer (`"webtransport"` |
    /// `"websocket"` | `"unknown"`), as reported via the `peer_status`
    /// diagnostics metric. `None` when no `peer_status` event has been
    /// observed yet. Renders as a header badge in [`SignalQualityPopup`];
    /// not part of the time-series chart.
    pub transport: Option<String>,
    /// HCL bug #2: scope filter for the popup. `Full` (legacy) shows every
    /// series; `ScreenOnly` restricts the chart / legend / tooltip to the
    /// screen-share series (used by the shared-content tile in the split
    /// layout); `NoScreen` hides the screen-share series (used by peer
    /// tiles so the screen metric only renders in the dedicated
    /// shared-content popup).
    pub meter_mode: SignalMeterMode,
}

// ---------------------------------------------------------------------------
// Popup component
// ---------------------------------------------------------------------------

/// Props for [`SignalQualityPopup`].
#[derive(Props, Clone, PartialEq)]
pub struct SignalQualityPopupProps {
    /// The peer's session ID -- used to generate unique DOM element IDs so
    /// multiple popups can coexist without duplicate-ID collisions.
    peer_id: String,
    /// Human-readable peer name shown in the popup header.
    peer_name: String,
    /// Full history of samples to chart.
    history: Vec<SignalSample>,
    /// Meeting start time (Unix ms). The X-axis is relative to this so all
    /// peers share the same time reference for easy comparison.
    meeting_start_ms: f64,
    /// Current transport string (`"webtransport"` | `"websocket"` |
    /// `"unknown"`) sourced from the `peer_status` diagnostics metric.
    /// Rendered as a small WT/WS/em-dash badge in the popup header.
    /// `None` is treated like `"unknown"` (em-dash).
    #[props(default)]
    transport: Option<String>,
    /// DOM id of the source tile element the popup should anchor to.  The
    /// popup is rendered with `position: fixed` and follows this element
    /// through grid reflows / window resizes / scrolls via a
    /// `ResizeObserver` + window listeners, so a grid reflow on peer
    /// join/leave keeps the popup glued to the right tile instead of
    /// stranding it where the tile used to be.
    anchor_id: String,
    /// HCL bug #2: scope filter for which metric series the popup shows.
    /// Defaults to `Full` so legacy call sites unaffected by bug #2 keep
    /// rendering every series.
    #[props(default)]
    meter_mode: SignalMeterMode,
    /// HCL bug #9: when `Some`, position the popup at fixed viewport
    /// coordinates instead of anchoring to the tile. `None` re-engages
    /// the anchored-follow behaviour. Owned by the popup-state context
    /// map (`SignalPopupStateMap`).
    #[props(default)]
    free_position: Option<(f64, f64)>,
    /// HCL bug #9: invoked when the user drags the popup to a new viewport
    /// position. The host installs this callback to commit the new free
    /// position into the popup-state map.
    #[props(default)]
    on_drag_commit: EventHandler<(f64, f64)>,
    /// HCL bug #9: invoked when the user clicks the reanchor button in the
    /// popup header. The host installs this callback to reset
    /// `free_position` to `None` so the popup snaps back to the tile.
    #[props(default)]
    on_reanchor: EventHandler<()>,
    /// Called when the user dismisses the popup.
    on_close: EventHandler<()>,
}

// ---------------------------------------------------------------------------
// Popup positioning math (portal-mode)
// ---------------------------------------------------------------------------

/// Minimum spacing between the popup and the viewport edges.  The popup is
/// clamped inside `[VIEWPORT_MARGIN_PX .. viewport - VIEWPORT_MARGIN_PX]`
/// on both axes so it never sits flush against a screen edge.
const VIEWPORT_MARGIN_PX: f64 = 8.0;
/// HCL follow-up (@token-exempt): fraction of the signal-quality button's
/// width at which the popup's RIGHT edge lands. `0.25` means the popup's
/// right edge sits 25% across the button from its left, so the popup
/// horizontally overlays only the LEFT QUARTER of the button — the body
/// of the popup extends to the LEFT of the button.
const POPUP_BUTTON_OVERLAY_X_FRACTION: f64 = 0.25;
/// HCL follow-up (@token-exempt): fraction of the signal-quality button's
/// height at which the popup's TOP edge lands. `0.5` puts the popup's
/// top edge at the button's vertical midpoint, so the popup hangs BELOW
/// the upper half of the button. Combined with the X fraction above, the
/// popup's upper-right corner touches the button at (25% from button
/// left, vertical midpoint).
const POPUP_BUTTON_OVERLAY_Y_FRACTION: f64 = 0.5;

/// HCL iter7: CSS-defined content-box width of `.signal-quality-popup`,
/// matching the `width: min(420px, calc(100vw - 16px))` rule in
/// `dioxus-ui/static/style.css`. The popup is rendered with the default
/// `box-sizing: content-box`, so the rendered border-box width is this
/// value PLUS the popup's symmetric padding and border.
const POPUP_CSS_CONTENT_WIDTH_PX: f64 = 420.0;
/// HCL iter7: popup's `calc(100vw - 16px)` upper bound — `16px` is the
/// shrink margin applied when the viewport is narrower than the natural
/// content width.
const POPUP_VIEWPORT_SHRINK_MARGIN_PX: f64 = 16.0;
/// HCL iter7: total horizontal padding the popup adds around its content
/// (`.signal-quality-popup { padding: 16px; }` → `2 * 16`).
const POPUP_HORIZONTAL_PADDING_PX: f64 = 32.0;
/// HCL iter7: total horizontal border the popup adds around its content
/// (`.signal-quality-popup { border: 1px solid ...; }` → `2 * 1`).
const POPUP_HORIZONTAL_BORDER_PX: f64 = 2.0;

/// HCL iter7: CSS-known border-box width of `.signal-quality-popup`.
///
/// `compute_popup_position` and the test assertions both compare the
/// popup's border-box right edge against the anchor's formula target
/// — `getBoundingClientRect()` returns border-box dimensions, so the
/// `popup_w` term passed to `compute_popup_position` must be the
/// border-box width too if we want `popup.right == anchor.left +
/// anchor.width * X_FRAC` to hold.
///
/// Live-measuring the popup via `get_bounding_client_rect()` is a
/// proxy for this CSS-known value. That measurement races with the
/// empty→populated rsx-branch transition on the LEFT-panel `ScreenOnly`
/// popup: snap-back's measurement can briefly capture an intermediate
/// width (HCL e2e iter7's deterministic 36.17px X delta), then the
/// popup re-renders to its full size and the test sees the math being
/// off by exactly the empty/populated width gap. The CSS rule fixes the
/// width at `min(420px, vw - 16px)` regardless of body state, so we
/// short-circuit the measurement and use the CSS-defined value directly
/// — the math then agrees with the steady-state DOM in every render
/// state.
///
/// Returns the border-box width clamped to non-negative values so callers
/// can pass it straight to `compute_popup_position` (which expects
/// non-negative `popup_w`).
fn css_popup_border_box_width(viewport_w: f64) -> f64 {
    let content_w = POPUP_CSS_CONTENT_WIDTH_PX.min(viewport_w - POPUP_VIEWPORT_SHRINK_MARGIN_PX);
    let content_w = content_w.max(0.0);
    content_w + POPUP_HORIZONTAL_PADDING_PX + POPUP_HORIZONTAL_BORDER_PX
}

/// Axis-aligned bounding box in viewport (CSS pixel) coordinates.  Mirrors
/// the fields of `DOMRect` we care about so the position-math helpers can
/// be unit-tested without a browser.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl Rect {
    pub(crate) fn width(&self) -> f64 {
        (self.right - self.left).max(0.0)
    }
    pub(crate) fn height(&self) -> f64 {
        (self.bottom - self.top).max(0.0)
    }
}

/// Compute the viewport-coordinate `(left, top)` for the signal-quality
/// popup given the source anchor (the tile's signal-quality button) rect,
/// the popup's own size, and the viewport size.
///
/// HCL follow-up (@token-exempt): the popup's UPPER-RIGHT corner lands at
/// `(button.left + button.width * POPUP_BUTTON_OVERLAY_X_FRACTION,
///   button.top  + button.height * POPUP_BUTTON_OVERLAY_Y_FRACTION)`.
/// With the defaults (`0.25`, `0.5`) that means the popup's right edge
/// sits 25% across the button from its left, and the popup's top edge
/// sits at the button's vertical midpoint. The popup body therefore
/// hangs mostly to the LEFT of, and BELOW the upper half of, the button
/// — overlaying only the button's upper-left quadrant slightly.
///
/// Anchoring rules (in order of preference):
///   1. Place the popup's upper-right corner at the (X, Y) point above —
///      i.e. `target_left = btn.left + btn.width * X_FRAC - popup_w`
///      and  `target_top  = btn.top  + btn.height * Y_FRAC`.
///   2. Clamp the result into `[VIEWPORT_MARGIN_PX, viewport - popup - margin]`
///      on both axes so the popup never extends past a screen edge.
///      Buttons near the viewport left can otherwise push `target_left`
///      negative; buttons near the bottom can otherwise push the popup
///      off the bottom edge.
///
/// The function operates on pure data, so unit tests can drive every
/// edge-case path without a browser.
pub(crate) fn compute_popup_position(
    anchor: Rect,
    popup_w: f64,
    popup_h: f64,
    viewport_w: f64,
    viewport_h: f64,
) -> (f64, f64) {
    // Horizontal: the popup's RIGHT edge lands at
    // `btn.left + btn.width * X_FRAC`, so `target_left = right - popup_w`.
    // Clamp into the viewport so a button near the left edge can't push
    // the popup off-screen on the left, and a narrow viewport can't let
    // it spill off on the right.
    let max_left = (viewport_w - popup_w - VIEWPORT_MARGIN_PX).max(VIEWPORT_MARGIN_PX);
    let min_left = VIEWPORT_MARGIN_PX;
    let target_right = anchor.left + anchor.width() * POPUP_BUTTON_OVERLAY_X_FRACTION;
    let target_left = target_right - popup_w;
    let left = target_left.clamp(min_left, max_left.max(min_left));

    // Vertical: the popup's TOP edge lands at the button's vertical
    // midpoint (`btn.top + btn.height * Y_FRAC`). Clamp into the viewport
    // so a button near the bottom edge can't push the popup off-screen,
    // and a button scrolled above the viewport can't yield a negative top.
    let max_top = (viewport_h - popup_h - VIEWPORT_MARGIN_PX).max(VIEWPORT_MARGIN_PX);
    let min_top = VIEWPORT_MARGIN_PX;
    let target_top = anchor.top + anchor.height() * POPUP_BUTTON_OVERLAY_Y_FRACTION;
    let top = target_top.clamp(min_top, max_top.max(min_top));

    (left, top)
}

/// Read an `Element`'s viewport-coordinate rect into our pure-data [`Rect`].
fn element_rect(el: &web_sys::Element) -> Rect {
    let r = el.get_bounding_client_rect();
    Rect {
        left: r.left(),
        top: r.top(),
        right: r.right(),
        bottom: r.bottom(),
    }
}

/// Reposition the popup element to anchor to its source tile.
///
/// No-ops silently if either element is missing from the DOM — that's the
/// normal state when the source tile has just unmounted from a peer leave
/// but the popup has not yet finished its own unmount cycle.
///
/// HCL bug #9: the popup carries a `data-anchor-mode` attribute reflecting
/// its current anchor state. `"free"` / `"dragging"` means the user has
/// dragged it; the JS-driven reposition must NOT overwrite the user's
/// coordinates in that case, so we early-return when the attribute is
/// `"free"`/`"dragging"`. This is the lever that lets the resize / scroll
/// / ResizeObserver callbacks coexist with drag-and-drop without re-
/// snapping the popup back to the tile mid-drag.
fn reposition_popup(anchor_id: &str, popup_id: &str) {
    let doc = gloo_utils::document();
    let win = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let popup = match doc.get_element_by_id(popup_id) {
        Some(el) => el,
        None => return,
    };

    // Bug #9: respect the user's manual drag position.
    if popup
        .get_attribute("data-anchor-mode")
        .as_deref()
        .map(|m| m == "free" || m == "dragging")
        .unwrap_or(false)
    {
        // Free popups are positioned by the user; only clamp them so they
        // remain within the current viewport. This handles window-resize
        // edge cases where a free popup would otherwise spill off-screen.
        clamp_free_popup_to_viewport(&popup, &win);
        return;
    }

    let anchor = match doc.get_element_by_id(anchor_id) {
        Some(el) => el,
        None => return,
    };

    let anchor_rect = element_rect(&anchor);
    // HCL iter7: use the CSS-known border-box width instead of a live
    // `getBoundingClientRect()` read. `.signal-quality-popup` is set to
    // `width: min(420px, calc(100vw - 16px))` so the natural border-box
    // width is constant for a given viewport — measuring it live races
    // with the empty→populated rsx-body swap (see
    // `css_popup_border_box_width` for the full rationale). Height is
    // still measured live because the popup's height is content-driven
    // (the chart + legend grow when more samples arrive); height drift
    // only affects vertical viewport-edge clamping, not the X anchor
    // math at the heart of the snap-back assertion.
    let popup_rect = element_rect(&popup);
    let viewport_w = win
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let viewport_h = win
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let (left, top) = compute_popup_position(
        anchor_rect,
        css_popup_border_box_width(viewport_w),
        popup_rect.height(),
        viewport_w,
        viewport_h,
    );

    let html_popup: web_sys::HtmlElement = popup.unchecked_into();
    let style = html_popup.style();
    let _ = style.set_property("left", &format!("{left:.0}px"));
    let _ = style.set_property("top", &format!("{top:.0}px"));
}

/// HCL follow-up 952 (@token-exempt): snap a popup back to its anchor
/// immediately, in response to the reanchor button click.
///
/// Without this helper, the click only commits `Anchored` to the
/// popup-state map. `reposition_popup` then runs ONLY when the next
/// resize / scroll / ResizeObserver fires — until then the popup keeps
/// the stale inline `left`/`top` written by the drag handler and the
/// user perceives nothing happening on click.
///
/// We flip `data-anchor-mode` to `anchored` so `reposition_popup` no
/// longer early-returns into the free-clamp branch, clear the inline
/// position styles the drag wrote, and run one immediate reposition
/// pass. The Dioxus re-render that follows the state-map write
/// confirms the same attribute / style declaratively, so the two paths
/// agree.
///
/// HCL follow-up 957: split-layout regression — clicking pin while a
/// peer was sharing left the popup parked at its dragged coordinates.
/// To make this robust across layout transitions (grid ↔ split panels):
///
///   1. The anchor id is read from the popup's live
///      `data-popup-anchor-id` attribute every call — not from a Rust
///      string captured at popup-open time, which could go stale if
///      the layout switched mid-session.
///   2. If the captured id no longer matches a live DOM element, we
///      fall back to the closest `[data-tile-root]` ancestor of the
///      popup and search inside it for a `.floating-name` element.
///      This gives the snap-back a "land somewhere sane" path even
///      when an anchor id mismatch would otherwise leave the popup
///      stranded.
///   3. The popup element itself is re-queried every call, so a stale
///      handle from a prior render can never cause a no-op.
fn snap_popup_back_to_anchor(popup_id: &str) {
    let doc = gloo_utils::document();
    let popup = match doc.get_element_by_id(popup_id) {
        Some(el) => el,
        None => return,
    };
    // Flip the mode FIRST so any subsequent reposition pass does not
    // early-return into the free-clamp branch.
    let _ = popup.set_attribute("data-anchor-mode", "anchored");

    // Clear drag-written inline coordinates so the reposition writes
    // below are the source of truth for the post-snap position.
    let html_popup: web_sys::HtmlElement = popup.clone().unchecked_into();
    let style = html_popup.style();
    let _ = style.remove_property("left");
    let _ = style.remove_property("top");

    // Resolve the anchor element. We prefer the popup's own live
    // `data-popup-anchor-id` attribute (set declaratively by Dioxus on
    // every render so it is always current for the active tile-mode);
    // when that lookup fails we fall back to a tile-relative
    // `.floating-name` search before giving up.
    let anchor_id = popup
        .get_attribute("data-popup-anchor-id")
        .unwrap_or_default();
    let anchor_el = if anchor_id.is_empty() {
        None
    } else {
        doc.get_element_by_id(&anchor_id)
    };
    let anchor_el = anchor_el.or_else(|| find_fallback_anchor(&popup));
    let Some(anchor_el) = anchor_el else {
        return;
    };

    let win = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let viewport_w = win
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let viewport_h = win
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let anchor_rect = element_rect(&anchor_el);
    // HCL iter7: use the CSS-known border-box width here too — this is
    // the snap-back path that the LEFT-panel `ScreenOnly` snap test
    // exercises. The live `popup_rect.width()` measurement that used
    // to feed this call could race with the empty→populated rsx-body
    // transition, producing a deterministic ~36px X delta between the
    // computed `target_left` (based on a transient narrow measurement)
    // and the post-snap popup that the test reads (settled at the wider
    // populated width). Pinning `popup_w` to the CSS-defined constant
    // sidesteps the race entirely. Height stays live because vertical
    // anchoring is content-driven and not asserted by the snap test.
    let popup_rect = element_rect(&popup);
    let (left, top) = compute_popup_position(
        anchor_rect,
        css_popup_border_box_width(viewport_w),
        popup_rect.height(),
        viewport_w,
        viewport_h,
    );
    let _ = style.set_property("left", &format!("{left:.0}px"));
    let _ = style.set_property("top", &format!("{top:.0}px"));
}

/// HCL follow-up 957: tile-relative fallback used when the popup's
/// stored anchor id does not match a live DOM element (popup mounts
/// before the new tile DOM commits; grid ↔ split transition tears down
/// the button mid-snap). Walks up from the popup to its owning tile
/// root (`[data-tile-root]`) and returns the first `.signal-indicator`
/// button it finds inside — the new anchor target. Returns `None` when
/// no tile root is found or the tile has no signal-indicator button —
/// in that case `snap_popup_back_to_anchor` gives up rather than
/// guessing.
fn find_fallback_anchor(popup: &web_sys::Element) -> Option<web_sys::Element> {
    let tile_root = popup.closest("[data-tile-root]").ok().flatten()?;
    tile_root.query_selector(".signal-indicator").ok().flatten()
}

/// HCL bug #9: clamp a `Free` popup so it stays within the viewport when
/// the window resizes. We don't touch the popup if it already fits — the
/// user dragged it there deliberately and we should not drift it.
fn clamp_free_popup_to_viewport(popup: &web_sys::Element, win: &web_sys::Window) {
    let html_popup: web_sys::HtmlElement = popup.clone().unchecked_into();
    let rect = element_rect(&html_popup);
    let viewport_w = win
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let viewport_h = win
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let (left, top) = clamp_free_position(
        rect.left,
        rect.top,
        rect.width(),
        rect.height(),
        viewport_w,
        viewport_h,
    );
    if (left - rect.left).abs() > 0.5 || (top - rect.top).abs() > 0.5 {
        let style = html_popup.style();
        let _ = style.set_property("left", &format!("{left:.0}px"));
        let _ = style.set_property("top", &format!("{top:.0}px"));
    }
}

/// Clamp a free-mode popup position to keep it inside the viewport with
/// the same `VIEWPORT_MARGIN_PX` breathing room used by
/// [`compute_popup_position`]. Extracted so unit tests can drive every
/// clamp branch without a browser.
pub(crate) fn clamp_free_position(
    left: f64,
    top: f64,
    popup_w: f64,
    popup_h: f64,
    viewport_w: f64,
    viewport_h: f64,
) -> (f64, f64) {
    let max_left = (viewport_w - popup_w - VIEWPORT_MARGIN_PX).max(VIEWPORT_MARGIN_PX);
    let min_left = VIEWPORT_MARGIN_PX;
    let max_top = (viewport_h - popup_h - VIEWPORT_MARGIN_PX).max(VIEWPORT_MARGIN_PX);
    let min_top = VIEWPORT_MARGIN_PX;
    let l = left.clamp(min_left, max_left.max(min_left));
    let t = top.clamp(min_top, max_top.max(min_top));
    (l, t)
}

/// Install everything the `SignalQualityPopup` needs to behave like a
/// portal-rendered overlay anchored to a source tile: a `ResizeObserver`
/// on the source tile + `resize`/`scroll` window listeners.  All closures
/// are stored in `use_hook` so they live for the popup's lifetime, and
/// `use_drop` tears them down on unmount.
///
/// Dismissal is intentionally limited to the explicit close button (the
/// "X" in the popup header) so the user can keep multiple per-peer popups
/// open simultaneously and inspect them at leisure.  Earlier revisions
/// also installed an `Escape` keydown listener and a click-outside
/// transparent backdrop; both were removed because, with one backdrop
/// per popup at the same z-index, the topmost backdrop swallowed clicks
/// on every other tile's signal-meter button and made it impossible to
/// open a second popup without first closing the first.
fn install_popup_anchor(anchor_id: String, popup_id: String, _on_close: EventHandler<()>) {
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::closure::Closure;

    // Holder kept alive for the popup's lifetime.  Listed fields are
    // dropped explicitly in `use_drop` so closures stop firing the moment
    // the popup unmounts (otherwise `forget()`-style closures would leak
    // across remounts).
    struct AnchorState {
        win: web_sys::Window,
        resize_cb: Option<Closure<dyn FnMut()>>,
        scroll_cb: Option<Closure<dyn FnMut()>>,
        observer: Option<web_sys::ResizeObserver>,
        _observer_cb: Option<Closure<dyn FnMut(js_sys::Array)>>,
    }

    let state: Rc<RefCell<Option<AnchorState>>> = use_hook(|| Rc::new(RefCell::new(None)));

    {
        let state = state.clone();
        let anchor_id_for_init = anchor_id.clone();
        let popup_id_for_init = popup_id.clone();
        // One-shot installation: use_hook runs once per popup mount.
        use_hook(move || {
            let win = match web_sys::window() {
                Some(w) => w,
                None => return,
            };

            // ── reposition trampoline ────────────────────────────────
            let reposition = {
                let aid = anchor_id_for_init.clone();
                let pid = popup_id_for_init.clone();
                move || {
                    reposition_popup(&aid, &pid);
                }
            };

            // Initial paint can race the popup actually being attached to
            // the DOM (Dioxus has not flushed yet), so schedule a single
            // microtask after the current render to lay it out correctly
            // on first appearance.  rAF gives the layout engine a chance
            // to measure the popup's natural size before we read it back.
            {
                let rep = reposition.clone();
                // rAF callback signature is `FnOnce(f64)` (the timestamp);
                // discard it and call `rep()` which expects no args.
                let cb = Closure::once_into_js(move |_ts: f64| rep());
                let _ = win.request_animation_frame(cb.as_ref().unchecked_ref());
            }

            // Window resize.
            let resize_cb: Closure<dyn FnMut()> = Closure::new({
                let rep = reposition.clone();
                move || rep()
            });
            let _ =
                win.add_event_listener_with_callback("resize", resize_cb.as_ref().unchecked_ref());

            // Window scroll (capture phase so we observe scroll on any
            // ancestor scroll container, not just window).  Scroll fires
            // a lot; `getBoundingClientRect` is cheap and the DOM writes
            // we issue are minimal, so we don't throttle here.  We use
            // the `_with_bool` overload which sets `useCapture=true`
            // without requiring the `AddEventListenerOptions` web-sys
            // feature.
            let scroll_cb: Closure<dyn FnMut()> = Closure::new({
                let rep = reposition.clone();
                move || rep()
            });
            let _ = win.add_event_listener_with_callback_and_bool(
                "scroll",
                scroll_cb.as_ref().unchecked_ref(),
                true,
            );

            // ResizeObserver on the anchor tile catches grid reflows on
            // peer join/leave (the tile's own size changes when CSS Grid
            // re-distributes available space).  `window` resize covers
            // viewport changes, but the grid can reflow without any
            // viewport change — that's the case this observer handles.
            //
            // HCL iter2 follow-up: we also observe the POPUP element itself.
            // The popup body switches between two rsx branches as the peer's
            // signal history populates ("No data yet" vs. the chart UI). The
            // popup's height (and on first paint, briefly its measured
            // width) changes across that transition. Without observing the
            // popup, the initial rAF reposition runs against the empty-body
            // dimensions and the position is never re-evaluated against the
            // populated-body dimensions — observed in HCL e2e iter2 as a
            // ~36px X delta on the LEFT-panel split-screen-tile popup snap-
            // back assertion (the snap-back recomputed against the wider
            // populated popup; the test's `initial` snapshot still reflected
            // the narrower empty-body measurement). Observing the popup
            // makes reposition fire when the body grows, so the
            // `compute_popup_position` math is always evaluated against the
            // popup's final dimensions and snap-back stays within tolerance.
            let observer_cb: Closure<dyn FnMut(js_sys::Array)> = Closure::new({
                let rep = reposition.clone();
                move |_entries: js_sys::Array| rep()
            });
            let observer = web_sys::ResizeObserver::new(observer_cb.as_ref().unchecked_ref()).ok();
            if let Some(obs) = observer.as_ref() {
                let doc = gloo_utils::document();
                if let Some(anchor_el) = doc.get_element_by_id(&anchor_id_for_init) {
                    obs.observe(&anchor_el);
                }
                if let Some(popup_el) = doc.get_element_by_id(&popup_id_for_init) {
                    obs.observe(&popup_el);
                }
            }

            *state.borrow_mut() = Some(AnchorState {
                win: win.clone(),
                resize_cb: Some(resize_cb),
                scroll_cb: Some(scroll_cb),
                observer,
                _observer_cb: Some(observer_cb),
            });
        });
    }

    // Tear down listeners + observer on unmount so popup remounts on a
    // different tile install a fresh anchor (and so old anchors do not
    // keep repositioning a popup that no longer exists).
    use_drop({
        let state = state.clone();
        move || {
            if let Some(s) = state.borrow_mut().take() {
                if let Some(cb) = s.resize_cb.as_ref() {
                    let _ = s
                        .win
                        .remove_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
                }
                if let Some(cb) = s.scroll_cb.as_ref() {
                    // Removal capture-flag must mirror the addition's
                    // `useCapture=true` so the matching listener is found.
                    let _ = s.win.remove_event_listener_with_callback_and_bool(
                        "scroll",
                        cb.as_ref().unchecked_ref(),
                        true,
                    );
                }
                if let Some(obs) = s.observer.as_ref() {
                    obs.disconnect();
                }
            }
        }
    });
}

/// HCL bug #9: install drag-and-drop on the popup header so users can
/// pull the popup off its tile anchor and place it wherever they like.
///
/// The header carries a `data-drag-handle` attribute. We attach
/// `mousedown` to the popup root (and filter to events that originated
/// inside the header). On mousedown we capture the pointer's offset from
/// the popup's current top-left, set `data-anchor-mode="dragging"` (which
/// `reposition_popup` honours by skipping its auto-layout math), and
/// install temporary `mousemove` / `mouseup` listeners on the window.
///
/// `mousemove` writes the new `left`/`top` directly to the popup's inline
/// style — using inline styles instead of a Dioxus signal keeps the drag
/// 60fps even when the popup tree is otherwise expensive to re-render.
/// On `mouseup` we commit the final position to the popup-state map via
/// the `on_drag_commit` callback, which clears the `dragging` data-attr
/// and flips it to `"free"` (the durable state). Touch support is
/// intentionally out of scope for the first cut — desktop drag is the
/// primary UX.
fn install_popup_drag(popup_id: String, on_drag_commit: EventHandler<(f64, f64)>) {
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::closure::Closure;

    struct DragState {
        win: web_sys::Window,
        mousedown_cb: Option<Closure<dyn FnMut(web_sys::MouseEvent)>>,
        mousemove_cb: Option<Closure<dyn FnMut(web_sys::MouseEvent)>>,
        mouseup_cb: Option<Closure<dyn FnMut(web_sys::MouseEvent)>>,
        popup_el: Option<web_sys::Element>,
    }

    let state: Rc<RefCell<Option<DragState>>> = use_hook(|| Rc::new(RefCell::new(None)));

    {
        let state = state.clone();
        let popup_id_for_init = popup_id.clone();
        let on_drag_commit_for_init = on_drag_commit;
        use_hook(move || {
            let win = match web_sys::window() {
                Some(w) => w,
                None => return,
            };

            // HCL follow-up 946 (@token-exempt): defer the `popup_el` lookup until after
            // Dioxus has committed the DOM. Without this rAF guard,
            // `doc.get_element_by_id(popup_id)` can race the same render
            // cycle that mounted the popup, returning `None` — the
            // listeners then never bind and drag silently fails. Mirrors
            // the rAF pattern `install_popup_anchor` uses for the same
            // mount-race reason ("Initial paint can race the popup
            // actually being attached to the DOM").
            //
            // If the popup unmounts before this rAF fires, the lookup
            // returns `None` and we silently bail. `state` stays at
            // `None`, so `use_drop` (below) is a no-op. No listener leak.
            let win_for_raf = win.clone();
            let popup_id_for_raf = popup_id_for_init;
            let state_for_raf = state;
            let on_drag_commit_for_raf = on_drag_commit_for_init;
            let raf_cb = Closure::once_into_js(move |_ts: f64| {
                let win = win_for_raf;
                let state = state_for_raf;
                let popup_id_for_init = popup_id_for_raf;
                let on_drag_commit_for_init = on_drag_commit_for_raf;

                let doc = gloo_utils::document();
                let popup_el = match doc.get_element_by_id(&popup_id_for_init) {
                    Some(el) => el,
                    None => return, // popup unmounted before rAF fired
                };

                // Per-drag-session offset of the cursor inside the popup so the
                // drag preserves the click point. Reset on every mousedown.
                let offset: Rc<RefCell<(f64, f64)>> = Rc::new(RefCell::new((0.0, 0.0)));
                // Whether a drag is currently active. Drives `mousemove` early-out
                // when the user isn't dragging.
                let active: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));

                // mousedown handler attached to the popup root. We bubble-listen
                // and filter by `closest('[data-drag-handle]')` so only clicks
                // inside the header start a drag. Buttons inside the header
                // (close, reanchor) carry `data-no-drag` so they're excluded.
                let mousedown_cb: Closure<dyn FnMut(web_sys::MouseEvent)> = Closure::new({
                    let popup_el = popup_el.clone();
                    let offset = offset.clone();
                    let active = active.clone();
                    move |evt: web_sys::MouseEvent| {
                        let target = match evt.target() {
                            Some(t) => t,
                            None => return,
                        };
                        let target_el: web_sys::Element = match target.dyn_into() {
                            Ok(el) => el,
                            Err(_) => return,
                        };
                        // Bail on no-drag controls (close button, reanchor button).
                        if let Ok(Some(_)) = target_el.closest("[data-no-drag]") {
                            return;
                        }
                        // Only fire when the click landed in the drag handle.
                        let in_handle =
                            matches!(target_el.closest("[data-drag-handle]"), Ok(Some(_)));
                        if !in_handle {
                            return;
                        }
                        if evt.button() != 0 {
                            return;
                        }
                        evt.prevent_default();
                        let rect = popup_el.get_bounding_client_rect();
                        let dx = evt.client_x() as f64 - rect.left();
                        let dy = evt.client_y() as f64 - rect.top();
                        *offset.borrow_mut() = (dx, dy);
                        *active.borrow_mut() = true;
                        let html_popup: web_sys::HtmlElement = popup_el.clone().unchecked_into();
                        let _ = html_popup.set_attribute("data-anchor-mode", "dragging");
                    }
                });
                let _ = popup_el.add_event_listener_with_callback(
                    "mousedown",
                    mousedown_cb.as_ref().unchecked_ref(),
                );

                // mousemove handler on window — runs only while a drag is
                // active, so the early-out keeps the cost negligible when
                // the user is just moving the cursor.
                let mousemove_cb: Closure<dyn FnMut(web_sys::MouseEvent)> = Closure::new({
                    let popup_el = popup_el.clone();
                    let offset = offset.clone();
                    let active = active.clone();
                    let win_inner = win.clone();
                    move |evt: web_sys::MouseEvent| {
                        if !*active.borrow() {
                            return;
                        }
                        let (dx, dy) = *offset.borrow();
                        let target_left = evt.client_x() as f64 - dx;
                        let target_top = evt.client_y() as f64 - dy;
                        let rect = popup_el.get_bounding_client_rect();
                        let viewport_w = win_inner
                            .inner_width()
                            .ok()
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        let viewport_h = win_inner
                            .inner_height()
                            .ok()
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        let (l, t) = clamp_free_position(
                            target_left,
                            target_top,
                            rect.width(),
                            rect.height(),
                            viewport_w,
                            viewport_h,
                        );
                        let html_popup: web_sys::HtmlElement = popup_el.clone().unchecked_into();
                        let style = html_popup.style();
                        let _ = style.set_property("left", &format!("{l:.0}px"));
                        let _ = style.set_property("top", &format!("{t:.0}px"));
                    }
                });
                let _ = win.add_event_listener_with_callback(
                    "mousemove",
                    mousemove_cb.as_ref().unchecked_ref(),
                );

                // mouseup commits the final position.
                let mouseup_cb: Closure<dyn FnMut(web_sys::MouseEvent)> = Closure::new({
                    let popup_el = popup_el.clone();
                    let active = active.clone();
                    move |_evt: web_sys::MouseEvent| {
                        if !*active.borrow() {
                            return;
                        }
                        *active.borrow_mut() = false;
                        let html_popup: web_sys::HtmlElement = popup_el.clone().unchecked_into();
                        let rect = html_popup.get_bounding_client_rect();
                        let _ = html_popup.set_attribute("data-anchor-mode", "free");
                        on_drag_commit_for_init.call((rect.left(), rect.top()));
                    }
                });
                let _ = win.add_event_listener_with_callback(
                    "mouseup",
                    mouseup_cb.as_ref().unchecked_ref(),
                );

                *state.borrow_mut() = Some(DragState {
                    win: win.clone(),
                    mousedown_cb: Some(mousedown_cb),
                    mousemove_cb: Some(mousemove_cb),
                    mouseup_cb: Some(mouseup_cb),
                    popup_el: Some(popup_el),
                });
            });
            let _ = win.request_animation_frame(raf_cb.as_ref().unchecked_ref());
        });
    }

    use_drop({
        let state = state.clone();
        move || {
            if let Some(s) = state.borrow_mut().take() {
                if let (Some(el), Some(cb)) = (s.popup_el.as_ref(), s.mousedown_cb.as_ref()) {
                    let _ = el.remove_event_listener_with_callback(
                        "mousedown",
                        cb.as_ref().unchecked_ref(),
                    );
                }
                if let Some(cb) = s.mousemove_cb.as_ref() {
                    let _ = s.win.remove_event_listener_with_callback(
                        "mousemove",
                        cb.as_ref().unchecked_ref(),
                    );
                }
                if let Some(cb) = s.mouseup_cb.as_ref() {
                    let _ = s.win.remove_event_listener_with_callback(
                        "mouseup",
                        cb.as_ref().unchecked_ref(),
                    );
                }
            }
        }
    });
}

/// Get or create the global tooltip element on `<body>`.
/// Rendering the tooltip outside all grid-items guarantees it is never
/// clipped or hidden by adjacent peer tile stacking contexts.
fn get_or_create_tooltip_el() -> web_sys::HtmlElement {
    let doc = gloo_utils::document();
    if let Some(el) = doc.get_element_by_id("signal-chart-tooltip-global") {
        el.unchecked_into()
    } else {
        let el = doc.create_element("div").unwrap();
        el.set_id("signal-chart-tooltip-global");
        el.set_class_name("signal-chart-tooltip");
        let html_el: web_sys::HtmlElement = el.unchecked_into();
        html_el.style().set_property("display", "none").unwrap();
        doc.body().unwrap().append_child(&html_el).unwrap();
        html_el
    }
}

/// Show the tooltip at viewport coordinates with metric content.
#[allow(clippy::too_many_arguments)]
fn show_body_tooltip(
    x: f64,
    y: f64,
    time_str: &str,
    sample: &SignalSample,
    show_video: bool,
    show_audio: bool,
    show_screen: bool,
    show_latency: bool,
) {
    let el = get_or_create_tooltip_el();
    let style = el.style();
    style.set_property("left", &format!("{x:.0}px")).unwrap();
    style.set_property("top", &format!("{y:.0}px")).unwrap();
    style.set_property("display", "block").unwrap();

    let video_tier = infer_video_tier(&sample.video_resolution);
    let video_line = if show_video {
        if sample.video_resolution.is_empty() {
            format!(
                "<span style='color:{}'>Video: {:.1} fps | {:.0} kbps</span>",
                theme_color::SIGNAL_VIDEO,
                sample.video_fps,
                sample.video_bitrate_kbps
            )
        } else if video_tier.is_empty() {
            format!(
                "<span style='color:{}'>Video: {} | {:.1} fps | {:.0} kbps</span>",
                theme_color::SIGNAL_VIDEO,
                sample.video_resolution,
                sample.video_fps,
                sample.video_bitrate_kbps
            )
        } else {
            format!(
                "<span style='color:{}'>Video: {} ({}) | {:.1} fps | {:.0} kbps</span>",
                theme_color::SIGNAL_VIDEO,
                sample.video_resolution,
                video_tier,
                sample.video_fps,
                sample.video_bitrate_kbps
            )
        }
    } else {
        String::new()
    };
    let audio_line = if show_audio {
        format!(
            "<span style='color:{}'>Audio: buf {:.0}ms | expand {:.0}\u{2030}</span>",
            theme_color::SIGNAL_AUDIO,
            sample.audio_buffer_ms,
            sample.audio_expand_rate
        )
    } else {
        String::new()
    };
    let screen_line = build_screen_tooltip_line(sample, show_screen);
    let screen_cause_line = build_screen_cause_line(sample, show_screen);
    let latency_line = if show_latency {
        format!(
            "<span style='color:{}'>Server RTT: {:.0} ms</span>",
            theme_color::SIGNAL_LATENCY,
            sample.latency_ms
        )
    } else {
        String::new()
    };

    let lines: Vec<String> = [
        Some(format!(
            "<div>Time: {time_str}</div><div style='border-bottom:1px solid {};margin:2px 0'></div>",
            theme_color::TOOLTIP_DIVIDER
        )),
        if video_line.is_empty() {
            None
        } else {
            Some(format!("<div>{video_line}</div>"))
        },
        if audio_line.is_empty() {
            None
        } else {
            Some(format!("<div>{audio_line}</div>"))
        },
        if screen_line.is_empty() {
            None
        } else {
            Some(format!("<div>{screen_line}</div>"))
        },
        if screen_cause_line.is_empty() {
            None
        } else {
            // Slight left-indent (font-size:11px) so the cause line reads as
            // a continuation of the Screen row above it.
            Some(format!(
                "<div style='font-size:11px;padding-left:8px'>{screen_cause_line}</div>"
            ))
        },
        if latency_line.is_empty() {
            None
        } else {
            Some(format!("<div>{latency_line}</div>"))
        },
    ]
    .into_iter()
    .flatten()
    .collect();
    el.set_inner_html(&lines.join(""));
}

fn infer_video_tier(resolution: &str) -> &'static str {
    let mut parts = resolution.split('x');
    let _width = parts.next().and_then(|w| w.parse::<u32>().ok());
    let height = parts.next().and_then(|h| h.parse::<u32>().ok());

    match height.unwrap_or_default() {
        h if h >= 1080 => "Full HD",
        h if h >= 900 => "HD+",
        h if h >= 720 => "HD",
        h if h >= 540 => "Standard",
        h if h >= 480 => "Medium",
        h if h >= 360 => "Low",
        h if h >= 270 => "Very Low",
        h if h > 0 => "Minimal",
        _ => "",
    }
}

/// Compact tier label used by the Screen tooltip line. The Screen tooltip
/// has been tightened to fit on one row alongside the resolution numbers,
/// so we substitute the most common labels for their standard
/// abbreviations (`Full HD` → `FHD`, `Quad HD` → `QHD`, `4K UHD` → `UHD`).
/// `HD` and the lower-density labels (`Medium`, `Low`, `Very Low`, etc.)
/// stay as-is because they're already short or have no widely-understood
/// abbreviation.
///
/// The camera-video tooltip line continues to use [`infer_video_tier`]
/// because that row has more horizontal real estate and the full label is
/// easier to scan when only a single value is shown.
fn infer_video_tier_short(resolution: &str) -> &'static str {
    match infer_video_tier(resolution) {
        "Full HD" => "FHD",
        "Quad HD" => "QHD",
        "4K UHD" => "UHD",
        other => other,
    }
}

/// Parse a `"WxH"` resolution string into `(width, height)`. Returns `None`
/// when either side is missing, non-numeric, or zero. Used by the
/// degradation-ratio helper below.
fn parse_resolution(resolution: &str) -> Option<(u32, u32)> {
    let mut parts = resolution.split('x');
    let w = parts.next()?.parse::<u32>().ok()?;
    let h = parts.next()?.parse::<u32>().ok()?;
    if w == 0 || h == 0 {
        None
    } else {
        Some((w, h))
    }
}

/// Compute the screen-share downscale ratio as a rounded percentage of the
/// publisher's source pixel area lost in transit:
///
/// `1 - (received_w × received_h) / (source_w × source_h)`
///
/// Returns `None` when either resolution is missing / unparseable OR when the
/// received area is not strictly smaller than the source (no downscale to
/// report). The percentage is rounded to the nearest integer in the
/// `1..=100` range — values that round to zero return `None` rather than
/// surfacing a misleading `↓0%` badge.
///
/// Using pixel area (not linear dimensions) is intentional: a 2× linear
/// downscale loses 75% of pixels, which is the user-visible quantity.
fn screen_downscale_percent(source: &str, received: &str) -> Option<u32> {
    let (sw, sh) = parse_resolution(source)?;
    let (rw, rh) = parse_resolution(received)?;
    let src_area = sw as u64 * sh as u64;
    let rcv_area = rw as u64 * rh as u64;
    if rcv_area >= src_area {
        return None;
    }
    // 0.0 < ratio < 1.0; multiply, round-half-away-from-zero.
    let pct = 100.0 * (1.0 - (rcv_area as f64 / src_area as f64));
    let pct_rounded = pct.round() as u32;
    if pct_rounded == 0 {
        None
    } else {
        Some(pct_rounded.min(100))
    }
}

/// Pick the tooltip color for a given downscale percentage.
/// Severity buckets follow the user-facing copy in the legend help text:
///   - ≥50% pixel-area loss is severe (danger).
///   - 25-49% is moderate (warning).
///   - <25% is mild (muted).
fn screen_downscale_color(pct: u32) -> &'static str {
    if pct >= 50 {
        theme_color::ERROR_TEXT
    } else if pct >= 25 {
        theme_color::WARNING_TEXT
    } else {
        theme_color::TEXT_MUTED
    }
}

/// Render the Screen-share tooltip line. Pulled out so the per-arm formatting
/// stays readable and so unit tests can exercise it without going through the
/// DOM. Returns an empty string when the screen share isn't active or the
/// caller has disabled the screen series.
///
/// Shape rules (post-#903 tightening — drop the colon after `Screen`, use // @token-exempt: issue ref, not a color
/// middle-dot separators, join units to numbers, abbreviate tier names):
///   - Received unknown → `Screen · Nfps · Mkbps` (legacy fallback).
///   - Source unknown or Source == Received → `Screen WxH (tier) · Nfps · Mkbps`.
///   - Source != Received → `Screen AxB → CxD ↓P%` plus `· Nfps · Mkbps`.
///     The `Source` / `Received` labels are dropped (the arrow already conveys
///     direction); tier names are dropped from the expanded form because the
///     resolution numbers are what matters when comparing the two; the
///     `pixel area` suffix is dropped because the `%` already implies it.
fn build_screen_tooltip_line(sample: &SignalSample, show_screen: bool) -> String {
    if !show_screen || !sample.screen_enabled {
        return String::new();
    }

    // Issue #906: classify the screen-state and pick which fps / kbps      // @token-exempt: issue ref, not a color
    // values the metrics tail should render. `Live` uses the sample's own
    // numbers; `Static` substitutes the held values + appends a `(static)`
    // marker; `NoFrames` keeps the literal zeros + appends `(no frames)`
    // so the user can tell apart "publisher's screen is quiet" from
    // "publisher's connection is broken / encoder crashed".
    let (display_fps, display_bitrate, fps_annotation, bitrate_annotation) =
        match sample.screen_state() {
            ScreenSampleState::Live => (sample.screen_fps, sample.screen_bitrate_kbps, "", ""),
            ScreenSampleState::Static {
                held_fps,
                held_bitrate_kbps,
            } => (held_fps, held_bitrate_kbps, " (static)", " (static)"),
            ScreenSampleState::NoFrames => (
                sample.screen_fps,
                sample.screen_bitrate_kbps,
                " (no frames)",
                " (no frames)",
            ),
        };

    // Compact metrics tail used by every branch. `·` (U+00B7 MIDDLE DOT)
    // replaces the previous `|` pipe so the row reads less like a CSV.
    // No space between number and unit (`850kbps`, `12.5fps`) — the user's
    // tightening spec called this out explicitly. Issue #906 appends an   // @token-exempt: issue ref, not a color
    // optional `(static)` / `(no frames)` annotation per metric.
    let metrics_suffix = format!(
        " \u{00B7} {display_fps:.1}fps{fps_annotation} \u{00B7} {display_bitrate:.0}kbps{bitrate_annotation}",
    );

    let received_known = !sample.screen_resolution.is_empty();
    let source_known = !sample.screen_source_resolution.is_empty();
    let source_equals_received = sample.screen_source_resolution == sample.screen_resolution;

    if !received_known {
        // Nothing to attribute. Same single-line shape used by older clients
        // before #883 introduced received-resolution tracking. // @token-exempt: issue ref, not a color
        return format!(
            "<span style='color:{}'>Screen{}</span>",
            theme_color::SIGNAL_SCREEN,
            metrics_suffix
        );
    }

    if !source_known || source_equals_received {
        // Either the publisher is older / doesn't report a source dimension,
        // or the encoder hit no tier constraint and shipped the native size.
        // In both cases collapse to a single value — there is nothing to
        // compare against.
        let tier = infer_video_tier_short(&sample.screen_resolution);
        return if tier.is_empty() {
            format!(
                "<span style='color:{}'>Screen {}{}</span>",
                theme_color::SIGNAL_SCREEN,
                sample.screen_resolution,
                metrics_suffix
            )
        } else {
            format!(
                "<span style='color:{}'>Screen {} ({}){}</span>",
                theme_color::SIGNAL_SCREEN,
                sample.screen_resolution,
                tier,
                metrics_suffix
            )
        };
    }

    // Source != Received. Show both with the arrow separator so it's
    // immediately legible that downscaling happened. Tier names are dropped
    // from the expanded form per the #903 tightening — the resolution // @token-exempt: issue ref, not a color
    // numbers are the comparison data, the tier names add noise.

    // Optional degradation badge when the encoder downscaled in transit.
    // U+2193 DOWNWARDS ARROW is the icon, the pct is bucketed for severity.
    // We drop the previous " pixel area" suffix — the % already implies
    // it and the row is tight enough as-is.
    let badge = if let Some(pct) =
        screen_downscale_percent(&sample.screen_source_resolution, &sample.screen_resolution)
    {
        format!(
            " <span style='color:{}'>\u{2193}{}%</span>",
            screen_downscale_color(pct),
            pct
        )
    } else {
        String::new()
    };

    format!(
        "<span style='color:{}'>Screen {} \u{2192} {}</span>{}{}",
        theme_color::SIGNAL_SCREEN,
        sample.screen_source_resolution,
        sample.screen_resolution,
        badge,
        format_args!(
            "<span style='color:{}'>{}</span>",
            theme_color::SIGNAL_SCREEN,
            metrics_suffix
        )
    )
}

/// Render the optional **Cause** sub-line that explains *why* the publisher's
/// encoder downscaled in transit (issue #903). The line is sourced from the // @token-exempt: issue ref, not a color
/// publisher-stamped `VideoMetadata` fields
/// `encoder_target_bitrate_kbps` / `adaptive_tier` / `cause_hint` and renders
/// in one of these compact shapes (post-#903 tightening — drop "encoder // @token-exempt: issue ref, not a color
/// target", "limited by", "adaptive-quality"; join units to numbers; use
/// middle-dot separators):
///
///   1. **Combined** — all three present:
///      `Cause: <cause_hint> · <N>kbps · tier '<tier>'`
///   2. **Primary** — bitrate + tier present:
///      `Cause: <N>kbps · tier '<tier>'`
///   3. **Hint-only fallback** — only `cause_hint`:
///      `Cause: <cause_hint>`
///
/// `tier` is preserved as a literal word because users may not recognise a
/// bare `'low'` / `'medium'` / `'high'` label without that cue.
///
/// Returns an empty string when:
/// * the screen series is hidden or `screen_enabled` is false, OR
/// * all three publisher-stamped fields are zero / empty (older publishers
///   that don't supply cause data, or the unconstrained-tier path).
///
/// The empty-string return is load-bearing: the tooltip render loop omits
/// the line entirely when this helper returns empty, so older publishers
/// never see a placeholder shipped in their UI. See the unit tests below.
fn build_screen_cause_line(sample: &SignalSample, show_screen: bool) -> String {
    if !show_screen || !sample.screen_enabled {
        return String::new();
    }

    let bitrate = sample.screen_encoder_target_bitrate_kbps;
    let tier = sample.screen_adaptive_tier.trim();
    let hint = sample.screen_cause_hint.trim();
    let has_bitrate = bitrate > 0;
    let has_tier = !tier.is_empty();
    let has_hint = !hint.is_empty();

    // No data → no line. Older publishers, AQ at top tier, and zero-initialised
    // newer publishers all land here.
    if !has_bitrate && !has_tier && !has_hint {
        return String::new();
    }

    // Use U+00B7 MIDDLE DOT as the inline separator, matching the Screen
    // tooltip line's tightened style. Each branch builds the trailing
    // evidence list once with the same dot-joining rule.
    let body = match (has_bitrate, has_tier, has_hint) {
        // Combined: hint + bitrate + tier. Most informative — lead with
        // the hint summary, then dot-join the concrete signals.
        (true, true, true) => {
            format!("Cause: {hint} \u{00B7} {bitrate}kbps \u{00B7} tier '{tier}'")
        }
        // Primary: bitrate + tier without a hint.
        (true, true, false) => {
            format!("Cause: {bitrate}kbps \u{00B7} tier '{tier}'")
        }
        // Bitrate + hint (no tier).
        (true, false, true) => format!("Cause: {hint} \u{00B7} {bitrate}kbps"),
        // Tier + hint (no bitrate).
        (false, true, true) => format!("Cause: {hint} \u{00B7} tier '{tier}'"),
        // Single signal fallbacks.
        (true, false, false) => format!("Cause: {bitrate}kbps"),
        (false, true, false) => format!("Cause: tier '{tier}'"),
        (false, false, true) => format!("Cause: {hint}"),
        (false, false, false) => return String::new(),
    };

    format!(
        "<span style='color:{}'>{}</span>",
        theme_color::TEXT_MUTED,
        body
    )
}

/// Hide the global tooltip.
fn hide_body_tooltip() {
    if let Some(el) = gloo_utils::document().get_element_by_id("signal-chart-tooltip-global") {
        let html_el: web_sys::HtmlElement = el.unchecked_into();
        html_el.style().set_property("display", "none").unwrap();
    }
}

/// Popup overlay showing a scrollable SVG line chart of audio, video,
/// screen share quality, and latency.
#[component]
pub fn SignalQualityPopup(props: SignalQualityPopupProps) -> Element {
    let history = &props.history;
    let on_close = props.on_close;
    let on_drag_commit = props.on_drag_commit;
    let on_reanchor = props.on_reanchor;
    let meter_mode = props.meter_mode;
    let free_position = props.free_position;
    let popup_title = match meter_mode {
        SignalMeterMode::ScreenOnly => format!("Screen Share Quality - {}", props.peer_name),
        _ => format!("Signal Quality - {}", props.peer_name),
    };

    // Derive the transport badge tuple once, before rsx, so we don't pay
    // for repeated `String::as_str` / match work inside the rsx! macro
    // during re-renders. Mirrors the diagnostics popup pattern.
    let (transport_label, transport_class, transport_title) = match props.transport.as_deref() {
        Some("webtransport") => ("WT", "connection-type type-webtransport", "WebTransport"),
        Some("websocket") => ("WS", "connection-type type-websocket", "WebSocket"),
        _ => ("\u{2014}", "connection-type", "Transport unknown"),
    };

    // No Dioxus signal for tooltip — we manipulate a <body>-level DOM element
    // directly to escape all stacking contexts from grid-items.
    // Hide tooltip when this popup component unmounts.
    use_drop(hide_body_tooltip);

    // ── Portal positioning ─────────────────────────────────────────────────
    // The popup is rendered with `position: fixed` so it escapes the source
    // tile's `overflow: hidden` clip (added in PR #923 for rounded-corner // @token-exempt: PR ref, not a color
    // canvases).  To keep it visually anchored to the tile we install a
    // [`PopupAnchor`] hook that:
    //
    //   - reads `getBoundingClientRect()` on the anchor tile,
    //   - clamps / flips the popup position so it stays in the viewport,
    //   - re-runs the math on window `resize`, `scroll` (capture phase), and
    //     a `ResizeObserver` on the anchor tile (grid reflows when peers
    //     join / leave bubble up via ResizeObserver).
    //
    // Dismissal is restricted to the explicit "X" close button so multiple
    // popups can be open at once without an Esc keystroke or a stray click
    // tearing them all down.
    //
    // All closures + the ResizeObserver are torn down via `use_drop` when
    // the popup unmounts, so reopening it on a different tile attaches
    // a fresh anchor cleanly.
    //
    // HCL bug #2: DOM id includes the meter-mode suffix so a peer's
    // screen-only popup and their no-screen peer popup can both exist
    // simultaneously without colliding on the same id. Legacy callers
    // (`meter_mode == Full`) get the original `signal-quality-popup-<peer_id>`
    // shape, which keeps the existing portal Playwright spec passing
    // unchanged.
    let popup_id = match meter_mode {
        SignalMeterMode::Full => format!("signal-quality-popup-{}", props.peer_id),
        other => format!(
            "signal-quality-popup-{}-{}",
            props.peer_id,
            other.id_suffix()
        ),
    };
    {
        let anchor_id = props.anchor_id.clone();
        let popup_id_for_hook = popup_id.clone();
        install_popup_anchor(anchor_id, popup_id_for_hook, on_close);
    }
    {
        // HCL bug #9: install drag-and-drop handlers on the header. The
        // drag installer is a no-op until the user mousedowns on
        // `.popup-header` (which carries `data-drag-handle`).
        let popup_id_for_drag = popup_id.clone();
        install_popup_drag(popup_id_for_drag, on_drag_commit);
    }

    // Which legend help text is currently expanded (if any).
    let mut help_visible = use_signal(|| None::<&'static str>);

    // Per-metric visibility toggles. Defaults follow the meter mode so the
    // popup opens with the right scope already applied (HCL bug #2). The
    // user can still toggle these checkboxes inside the popup if they
    // want to override the default for a single session.
    let mut show_audio = use_signal(|| meter_mode.shows_audio());
    let mut show_video = use_signal(|| meter_mode.shows_video());
    let mut show_screen = use_signal(|| meter_mode.shows_screen());
    let mut show_latency = use_signal(|| true);

    // HCL bug #9: compute the initial `data-anchor-mode` and inline
    // position style from the `free_position` prop. `data-anchor-mode`
    // is the lever `reposition_popup` reads to decide whether to skip
    // the auto-layout math; the inline `left`/`top` style covers the
    // first paint before any reposition tick runs.
    let (anchor_mode_attr, position_style) = match free_position {
        Some((l, t)) => ("free", format!("left: {l:.0}px; top: {t:.0}px;")),
        None => ("anchored", String::new()),
    };
    let show_reanchor = free_position.is_some();

    // Unique scroll container ID so multiple popups don't collide.
    let scroll_id = format!("signal-chart-scroll-{}", props.peer_id);

    // Chart dimensions
    let chart_height: f64 = 180.0;
    let px_per_sec: f64 = 10.0;
    let visible_seconds: f64 = 30.0;
    let visible_width: f64 = visible_seconds * px_per_sec;
    let padding_top: f64 = 10.0;
    let padding_bottom: f64 = 20.0;
    let draw_height = chart_height - padding_top - padding_bottom;

    if history.is_empty() {
        // HCL follow-up 952 (@token-exempt): capture the popup + anchor ids
        // so the reanchor button onclick can snap the popup back to its
        // tile-name anchor immediately rather than waiting for the next
        // resize/scroll/ResizeObserver tick. HCL follow-up 957: the
        // anchor id is published on the popup itself via
        // `data-popup-anchor-id` so `snap_popup_back_to_anchor` reads
        // it back live (rather than from a captured closure that could
        // go stale across a grid ↔ split layout transition).
        let popup_id_for_reanchor_empty = popup_id.clone();
        let popup_anchor_id_attr_empty = props.anchor_id.clone();
        return rsx! {
            div {
                id: "{popup_id}",
                class: "signal-quality-popup signal-quality-popup-portal",
                "data-anchor-mode": "{anchor_mode_attr}",
                "data-meter-mode": "{meter_mode.id_suffix()}",
                "data-popup-anchor-id": "{popup_anchor_id_attr_empty}",
                style: "{position_style}",
                onclick: move |e| e.stop_propagation(),
                div {
                    class: "popup-header",
                    "data-drag-handle": "true",
                    // HCL follow-up 952 (@token-exempt): visual drag-handle
                    // affordance so users discover the popup is draggable.
                    // Carries `data-drag-handle` so the existing mousedown
                    // closest('[data-drag-handle]') filter recognises clicks
                    // on the grip itself as drag starts. Six dots arranged
                    // in a 3x2 grid (the canonical "grip" iconography).
                    span {
                        class: "signal-popup-drag-handle",
                        "data-drag-handle": "true",
                        "aria-hidden": "true",
                        svg {
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "12",
                            height: "22",
                            view_box: "0 0 12 22",
                            fill: "currentColor",
                            circle { cx: "3", cy: "3", r: "1.2" }
                            circle { cx: "3", cy: "7", r: "1.2" }
                            circle { cx: "3", cy: "11", r: "1.2" }
                            circle { cx: "3", cy: "15", r: "1.2" }
                            circle { cx: "3", cy: "19", r: "1.2" }
                            circle { cx: "8", cy: "3", r: "1.2" }
                            circle { cx: "8", cy: "7", r: "1.2" }
                            circle { cx: "8", cy: "11", r: "1.2" }
                            circle { cx: "8", cy: "15", r: "1.2" }
                            circle { cx: "8", cy: "19", r: "1.2" }
                        }
                    }
                    span { class: "popup-title", "{popup_title}" }
                    div { class: "popup-header-actions",
                        span {
                            class: "{transport_class}",
                            title: "{transport_title}",
                            "{transport_label}"
                        }
                        if show_reanchor {
                            button {
                                class: "popup-reanchor",
                                title: "Reanchor to tile",
                                "aria-label": "Reanchor to tile",
                                "data-no-drag": "true",
                                onclick: move |_| {
                                    snap_popup_back_to_anchor(&popup_id_for_reanchor_empty);
                                    on_reanchor.call(());
                                },
                                "\u{1F4CC}"
                            }
                        }
                        button {
                            class: "popup-close",
                            "data-no-drag": "true",
                            onclick: move |_| on_close.call(()),
                            "X"
                        }
                    }
                }
                p { style: "color: {theme_color::TEXT_SUBTLE}; font-size: 12px;", "No data yet." }
            }
        };
    }

    // X-axis origin is meeting start, not when the first sample was recorded.
    // This means all peers share the same time reference so charts are directly
    // comparable side-by-side.
    let first_ts = props.meeting_start_ms;
    let last_ts = history.last().map(|s| s.timestamp_ms).unwrap_or(first_ts);
    let total_seconds = ((last_ts - first_ts) / 1000.0).max(1.0);
    let chart_width = (total_seconds * px_per_sec).max(visible_width) + 10.0;

    // Determine if any sample has screen share enabled
    let has_screen_data = history.iter().any(|s| s.screen_enabled);

    // Determine max latency for the right y-axis scale
    let max_latency = history
        .iter()
        .map(|s| s.latency_ms)
        .fold(0.0_f64, f64::max)
        .max(10.0); // At least 10ms scale so labels are meaningful
                    // Round up to a nice number for the axis
    let max_latency_axis = nice_ceil(max_latency);

    // Build polyline points for quality lines (left y-axis, 0-100%)
    let audio_points: String = build_quality_polyline(
        history,
        first_ts,
        px_per_sec,
        padding_top,
        draw_height,
        |s| s.audio_quality,
    );
    let video_points: String = build_quality_polyline(
        history,
        first_ts,
        px_per_sec,
        padding_top,
        draw_height,
        |s| s.video_quality,
    );
    let screen_points: String = if has_screen_data {
        // Issue #906: the screen polyline is state-aware so static periods // @token-exempt: issue ref, not a color
        // render at the held Y instead of dropping to zero. `NoFrames` and
        // `Live` use the raw `screen_quality`; `Static` plots at the held
        // value's normalized quality (held_fps / 30).
        build_screen_quality_polyline(history, first_ts, px_per_sec, padding_top, draw_height)
    } else {
        String::new()
    };

    // Build polyline points for latency (right y-axis, 0-max_latency ms)
    let latency_points: String = history
        .iter()
        .map(|s| {
            let x = ((s.timestamp_ms - first_ts) / 1000.0) * px_per_sec;
            let normalized = (s.latency_ms / max_latency_axis).clamp(0.0, 1.0);
            let y = padding_top + draw_height * (1.0 - normalized);
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ");

    // Y-axis labels (left side -- quality)
    let y_labels: Vec<(&str, f64)> = vec![
        ("100%", padding_top),
        ("50%", padding_top + draw_height * 0.5),
        ("0%", padding_top + draw_height),
    ];

    // Y-axis labels (right side -- latency)
    let max_latency_str = format!("{}ms", max_latency_axis as u32);
    let mid_latency_str = format!("{}ms", (max_latency_axis / 2.0) as u32);
    let mid_latency_y = padding_top + draw_height * 0.5;
    let bottom_latency_y = padding_top + draw_height;

    // X-axis tick labels (every 10 seconds)
    let tick_interval = 10.0_f64;
    let num_ticks = (total_seconds / tick_interval).ceil() as usize + 1;

    let chart_width_px = format!("{chart_width:.0}px");
    let chart_height_str = format!("{chart_height:.0}");
    let chart_width_str = format!("{chart_width:.0}");
    let visible_width_style = format!("max-width: {visible_width:.0}px;");

    // Grid lines span the full chart width
    let grid_lines: Vec<f64> = y_labels.iter().map(|(_, y)| *y).collect();

    // Auto-scroll only when the user is already viewing the latest data.
    // If the scrollbar is pulled back to inspect older data, don't jump
    // to the end — that would be disruptive. Only auto-scroll when the
    // scroll position is at (or within 20px of) the right edge.
    let scroll_id_for_scroll = scroll_id.clone();
    spawn(async move {
        TimeoutFuture::new(0).await;
        if let Some(el) = gloo_utils::document().get_element_by_id(&scroll_id_for_scroll) {
            let at_end = el.scroll_left() + el.client_width() >= el.scroll_width() - 20;
            if at_end {
                el.set_scroll_left(el.scroll_width());
            }
        }
    });

    // HCL follow-up 952 (@token-exempt) / 957: see the empty-history branch
    // above. The anchor id is also published as `data-popup-anchor-id` on
    // the popup div so `snap_popup_back_to_anchor` reads it live (defending
    // against any stale captured closure across a grid ↔ split layout
    // switch).
    let popup_id_for_reanchor = popup_id.clone();
    let popup_anchor_id_attr = props.anchor_id.clone();
    rsx! {
        div {
            id: "{popup_id}",
            class: "signal-quality-popup signal-quality-popup-portal",
            "data-anchor-mode": "{anchor_mode_attr}",
            "data-meter-mode": "{meter_mode.id_suffix()}",
            "data-popup-anchor-id": "{popup_anchor_id_attr}",
            style: "{position_style}",
            // Stop clicks inside the popup from bubbling to tile handlers
            // (e.g. the mobile-pin onclick on `.canvas-container`).
            onclick: move |e| e.stop_propagation(),
            div {
                class: "popup-header",
                "data-drag-handle": "true",
                // HCL follow-up 952 (@token-exempt): visual drag-handle
                // affordance — see the empty-history branch above for the
                // full rationale. Carries `data-drag-handle` so the
                // existing mousedown handler picks up clicks on the grip
                // itself as a drag start.
                span {
                    class: "signal-popup-drag-handle",
                    "data-drag-handle": "true",
                    "aria-hidden": "true",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "12",
                        height: "22",
                        view_box: "0 0 12 22",
                        fill: "currentColor",
                        circle { cx: "3", cy: "3", r: "1.2" }
                        circle { cx: "3", cy: "7", r: "1.2" }
                        circle { cx: "3", cy: "11", r: "1.2" }
                        circle { cx: "3", cy: "15", r: "1.2" }
                        circle { cx: "3", cy: "19", r: "1.2" }
                        circle { cx: "8", cy: "3", r: "1.2" }
                        circle { cx: "8", cy: "7", r: "1.2" }
                        circle { cx: "8", cy: "11", r: "1.2" }
                        circle { cx: "8", cy: "15", r: "1.2" }
                        circle { cx: "8", cy: "19", r: "1.2" }
                    }
                }
                span { class: "popup-title", "{popup_title}" }
                div { class: "popup-header-actions",
                    span {
                        class: "{transport_class}",
                        title: "{transport_title}",
                        "{transport_label}"
                    }
                    if show_reanchor {
                        button {
                            class: "popup-reanchor",
                            title: "Reanchor to tile",
                            "aria-label": "Reanchor to tile",
                            "data-no-drag": "true",
                            onclick: move |_| {
                                snap_popup_back_to_anchor(&popup_id_for_reanchor);
                                on_reanchor.call(());
                            },
                            "\u{1F4CC}"
                        }
                    }
                    button {
                        class: "popup-close",
                        "data-no-drag": "true",
                        onclick: move |_| on_close.call(()),
                        "X"
                    }
                }
            }
            div { class: "signal-chart-wrapper",
                // Fixed Y-axis overlay (left side -- quality %)
                svg {
                    class: "signal-chart-y-axis",
                    width: "30",
                    height: "{chart_height_str}",
                    view_box: "0 0 30 {chart_height_str}",
                    for (label, y) in y_labels.iter() {
                        text {
                            x: "28",
                            y: "{y}",
                            fill: "{theme_color::TEXT_SUBTLE}",
                            font_size: "9",
                            text_anchor: "end",
                            dominant_baseline: "middle",
                            "{label}"
                        }
                    }
                }
                // Scrollable chart area
                div {
                    class: "signal-chart-scroll",
                    id: "{scroll_id}",
                    style: "{visible_width_style}",
                    onscroll: {
                        let scroll_id = scroll_id.clone();
                        move |_| {
                            let doc = gloo_utils::document();
                            if let Some(src) = doc.get_element_by_id(&scroll_id) {
                                let scroll_left = src.scroll_left();
                                let els = doc.get_elements_by_class_name("signal-chart-scroll");
                                for i in 0..els.length() {
                                    if let Some(el) = els.item(i) {
                                        if el.id() != scroll_id {
                                            el.set_scroll_left(scroll_left);
                                        }
                                    }
                                }
                            }
                        }
                    },
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "{chart_width_px}",
                        height: "{chart_height_str}",
                        view_box: "0 0 {chart_width_str} {chart_height_str}",
                        // Grid lines
                        for grid_y in grid_lines.iter() {
                            line {
                                x1: "0",
                                y1: "{grid_y}",
                                x2: "{chart_width_str}",
                                y2: "{grid_y}",
                                stroke: "{theme_color::SIGNAL_GRID_MAJOR}",
                                stroke_width: "0.5",
                            }
                        }
                        // X-axis ticks
                        for tick_i in 0..num_ticks {
                            {
                                let t = tick_i as f64 * tick_interval;
                                let x = t * px_per_sec;
                                let mins = (t / 60.0).floor() as u32;
                                let secs = (t % 60.0).floor() as u32;
                                let label = if mins > 0 {
                                    format!("{mins}m{secs:02}s")
                                } else {
                                    format!("{secs}s")
                                };
                                let y_bottom = padding_top + draw_height;
                                rsx! {
                                    line {
                                        x1: "{x}",
                                        y1: "{padding_top}",
                                        x2: "{x}",
                                        y2: "{y_bottom}",
                                        stroke: "{theme_color::SIGNAL_GRID_MINOR}",
                                        stroke_width: "0.5",
                                    }
                                    text {
                                        x: "{x}",
                                        y: "{chart_height_str}",
                                        fill: "{theme_color::TEXT_SUBTLE}",
                                        font_size: "8",
                                        text_anchor: "middle",
                                        "{label}"
                                    }
                                }
                            }
                        }
                        // Audio polyline
                        if show_audio() {
                            polyline {
                                points: "{audio_points}",
                                fill: "none",
                                stroke: "{theme_color::SIGNAL_AUDIO}",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                            }
                        }
                        // Video polyline
                        if show_video() {
                            polyline {
                                points: "{video_points}",
                                fill: "none",
                                stroke: "{theme_color::SIGNAL_VIDEO}",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                            }
                        }
                        // Screen share polyline (only when data exists and enabled)
                        if has_screen_data && show_screen() {
                            polyline {
                                points: "{screen_points}",
                                fill: "none",
                                stroke: "{theme_color::SIGNAL_SCREEN}",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                            }
                        }
                        // Latency polyline
                        if show_latency() {
                            polyline {
                                points: "{latency_points}",
                                fill: "none",
                                stroke: "{theme_color::SIGNAL_LATENCY_DIM}",
                                stroke_width: "1",
                                stroke_linejoin: "round",
                                stroke_dasharray: "3 6",
                            }
                        }
                    }
                    // HTML overlay for tooltip interaction (more reliable than SVG rect in WASM).
                    // Positioned over the SVG chart area, same dimensions.
                    {
                        let first_ts_for_move = first_ts;
                        let history_for_move = history.clone();
                        let overlay_style = format!(
                            "position: absolute; top: {padding_top}px; left: 0; \
                             width: {chart_width:.0}px; height: {draw_height:.0}px; \
                             cursor: crosshair;"
                        );
                        rsx! {
                            div {
                                style: "{overlay_style}",
                                onmousemove: move |evt: MouseEvent| {
                                    let v_audio = show_audio();
                                    let v_video = show_video();
                                    let v_screen = show_screen();
                                    let v_latency = show_latency();
                                    let client = evt.client_coordinates();
                                    let elem = evt.element_coordinates();
                                    let time_offset_sec = elem.x / px_per_sec;
                                    let target_ts = first_ts_for_move + time_offset_sec * 1000.0;
                                    let idx = history_for_move
                                        .binary_search_by(|s| {
                                            s.timestamp_ms.partial_cmp(&target_ts)
                                                .unwrap_or(std::cmp::Ordering::Equal)
                                        })
                                        .unwrap_or_else(|i| i);
                                    let sample = [idx.saturating_sub(1), idx.min(history_for_move.len().saturating_sub(1))]
                                        .iter()
                                        .filter_map(|&i| history_for_move.get(i))
                                        .min_by(|a, b| {
                                            let da = (a.timestamp_ms - target_ts).abs();
                                            let db = (b.timestamp_ms - target_ts).abs();
                                            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                                        });
                                    if let Some(sample) = sample {
                                        let elapsed_secs = (sample.timestamp_ms - first_ts_for_move) / 1000.0;
                                        let mins = (elapsed_secs / 60.0).floor() as u32;
                                        let secs = (elapsed_secs % 60.0).floor() as u32;
                                        let time_str = format!("{mins}m {secs:02}s");
                                        show_body_tooltip(
                                            client.x + 12.0,
                                            client.y - 10.0,
                                            &time_str,
                                            sample,
                                            v_video,
                                            v_audio,
                                            v_screen,
                                            v_latency,
                                        );
                                    }
                                },
                                onmouseleave: move |_| {
                                    hide_body_tooltip();
                                },
                            }
                        }
                    }
                }
                // Fixed Y-axis overlay (right side -- latency)
                svg {
                    class: "signal-chart-y-axis-right",
                    width: "35",
                    height: "{chart_height_str}",
                    view_box: "0 0 35 {chart_height_str}",
                    text {
                        x: "2",
                        y: "{padding_top}",
                        fill: "{theme_color::TEXT_SUBTLE}",
                        font_size: "9",
                        dominant_baseline: "middle",
                        "{max_latency_str}"
                    }
                    text {
                        x: "2",
                        y: "{mid_latency_y}",
                        fill: "{theme_color::TEXT_SUBTLE}",
                        font_size: "9",
                        dominant_baseline: "middle",
                        "{mid_latency_str}"
                    }
                    text {
                        x: "2",
                        y: "{bottom_latency_y}",
                        fill: "{theme_color::TEXT_SUBTLE}",
                        font_size: "9",
                        dominant_baseline: "middle",
                        "0ms"
                    }
                }
            }
            // Tooltip is rendered directly on <body> via show_body_tooltip/hide_body_tooltip
            // to escape all grid-item stacking contexts.
            // Legend with visibility checkboxes. HCL bug #2: each row is
            // gated on `meter_mode` so a `ScreenOnly` popup only exposes the
            // Screen + RTT toggles, and a `NoScreen` popup hides the Screen
            // toggle (the dedicated shared-content tile owns it).
            div { class: "signal-chart-legend",
                if meter_mode.shows_audio() {
                    label { class: "legend-item",
                        input {
                            r#type: "checkbox",
                            checked: show_audio(),
                            onchange: move |_| show_audio.set(!show_audio()),
                        }
                        span { class: "dot", style: "background: {theme_color::SIGNAL_AUDIO};" }
                        "Audio"
                        button {
                            class: "legend-help-btn",
                            onclick: move |_| {
                                let current = help_visible();
                                if current == Some("audio") {
                                    help_visible.set(None);
                                } else {
                                    help_visible.set(Some("audio"));
                                }
                            },
                            "?"
                        }
                    }
                }
                if meter_mode.shows_video() {
                    label { class: "legend-item",
                        input {
                            r#type: "checkbox",
                            checked: show_video(),
                            onchange: move |_| show_video.set(!show_video()),
                        }
                        span { class: "dot", style: "background: {theme_color::SIGNAL_VIDEO};" }
                        "Video"
                        button {
                            class: "legend-help-btn",
                            onclick: move |_| {
                                let current = help_visible();
                                if current == Some("video") {
                                    help_visible.set(None);
                                } else {
                                    help_visible.set(Some("video"));
                                }
                            },
                            "?"
                        }
                    }
                }
                if has_screen_data && meter_mode.shows_screen() {
                    label { class: "legend-item",
                        input {
                            r#type: "checkbox",
                            checked: show_screen(),
                            onchange: move |_| show_screen.set(!show_screen()),
                        }
                        span { class: "dot", style: "background: {theme_color::SIGNAL_SCREEN};" }
                        "Screen"
                        button {
                            class: "legend-help-btn",
                            onclick: move |_| {
                                let current = help_visible();
                                if current == Some("screen") {
                                    help_visible.set(None);
                                } else {
                                    help_visible.set(Some("screen"));
                                }
                            },
                            "?"
                        }
                    }
                }
                label { class: "legend-item",
                    input {
                        r#type: "checkbox",
                        checked: show_latency(),
                        onchange: move |_| show_latency.set(!show_latency()),
                    }
                    span { class: "dot", style: "background: {theme_color::SIGNAL_LATENCY};" }
                    "Server RTT"
                    button {
                        class: "legend-help-btn",
                        onclick: move |_| {
                            let current = help_visible();
                            if current == Some("latency") {
                                help_visible.set(None);
                            } else {
                                help_visible.set(Some("latency"));
                            }
                        },
                        "?"
                    }
                }
            }
            // Legend help text (shown when a "?" button is clicked)
            if let Some(topic) = help_visible() {
                div { class: "legend-help-text",
                    match topic {
                        "audio" => rsx! {
                            strong { "Audio Quality" }
                            p { "Composite score from two metrics shown in the tooltip:" }
                            p {
                                strong { "Buffer (buf ms): " }
                                "How much audio is queued for playback. "
                                "20\u{2013}80ms is ideal. Below 20ms risks glitches; above 150ms means network congestion."
                            }
                            p {
                                strong { "Expand Rate (expand \u{2030}): " }
                                "How much audio the system had to synthesize due to missing packets. "
                                "0\u{2030} is perfect. Above 50\u{2030} you may hear artifacts; above 200\u{2030} is noticeable dropout."
                            }
                        },
                        "video" => rsx! {
                            strong { "Video Quality" }
                            p { "Based on received frames per second (FPS) relative to a 30fps target." }
                            p {
                                strong { "Resolution: " }
                                "The dimensions of the video being received (e.g., 1280x720)."
                            }
                            p {
                                strong { "FPS: " }
                                "Frames per second being received. Higher is smoother. "
                                "Below 15fps the video looks choppy."
                            }
                            p {
                                strong { "Bitrate (kbps): " }
                                "Data rate of the video stream. Higher bitrate generally means better picture quality."
                            }
                        },
                        "screen" => rsx! {
                            strong { "Screen Share Quality" }
                            p { "Based on received FPS for the shared screen content." }
                            p {
                                strong { "Source vs Received resolution: " }
                                "Source is the publisher's native capture resolution (their monitor / window). "
                                "Received is what your client decoded. A gap between the two means the publisher's "
                                "encoder downscaled the content in transit — usually because the network or CPU "
                                "couldn't sustain a full-resolution stream."
                            }
                            p {
                                strong { "↓ Pixel-area badge: " }
                                "Quantifies how much detail was lost in transit. A 2× linear downscale (e.g., 1080p \u{2192} 540p) "
                                "drops 75% of the pixels, which is what the badge reports. \u{2265}50% is shown in red, 25\u{2013}49% in amber, "
                                "<25% in muted text."
                            }
                            p {
                                strong { "Cause: " }
                                "Sub-line shown below the Screen row when the publisher's encoder "
                                "reports it is constrained. Sourced from the publisher's adaptive-"
                                "quality system: the encoder's current target bitrate, the tier "
                                "actively limiting it (e.g. 'low' / 'medium'), and a short cause "
                                "classifier (bitrate-limited, cpu-pressure, network-rtt, "
                                "network-loss, manual-cap). The line is omitted when the "
                                "publisher is unconstrained or is an older client that doesn't "
                                "report this data."
                            }
                            p {
                                strong { "FPS: " }
                                "Frames per second of the shared screen. Screen shares typically run at 5\u{2013}15fps."
                            }
                            p {
                                strong { "Bitrate (kbps): " }
                                "Data rate of the screen share stream. Combined with resolution this is the main driver of how sharp the shared content looks."
                            }
                        },
                        "latency" => rsx! {
                            strong { "Server RTT" }
                            p { "Round-trip time from your device to the relay server and back." }
                            p { "This is the same value for all peers in your session — it measures your connection to the relay, not end-to-end latency to each peer." }
                            p {
                                "Below 50ms is excellent. "
                                "50\u{2013}150ms is acceptable. "
                                "Above 200ms causes noticeable delay."
                            }
                        },
                        _ => rsx! {},
                    }
                }
            }
        }
    }
}

/// Build a polyline `points` string from history, mapping a quality accessor
/// to y-coordinates on the chart (0.0 at top = 100%, 1.0 at bottom = 0%).
fn build_quality_polyline(
    history: &[SignalSample],
    first_ts: f64,
    px_per_sec: f64,
    padding_top: f64,
    draw_height: f64,
    quality_fn: impl Fn(&SignalSample) -> f64,
) -> String {
    history
        .iter()
        .map(|s| {
            let x = ((s.timestamp_ms - first_ts) / 1000.0) * px_per_sec;
            let y = padding_top + draw_height * (1.0 - quality_fn(s));
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the screen-share polyline. Unlike the camera-video / audio lines
/// the screen series classifies each sample (issue #906) so static periods // @token-exempt: issue ref, not a color
/// flatline at the held value's Y position instead of dropping to zero,
/// which would otherwise be visually indistinguishable from a broken
/// encoder. `NoFrames` and `Live` use the raw `screen_quality`; `Static`
/// substitutes the held FPS's normalized quality (held_fps clamped to the
/// same 30fps target the live path uses).
fn build_screen_quality_polyline(
    history: &[SignalSample],
    first_ts: f64,
    px_per_sec: f64,
    padding_top: f64,
    draw_height: f64,
) -> String {
    history
        .iter()
        .map(|s| {
            let x = ((s.timestamp_ms - first_ts) / 1000.0) * px_per_sec;
            let quality = screen_chart_quality(s);
            let y = padding_top + draw_height * (1.0 - quality);
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Issue #906: pick the quality value the screen chart should plot for a   // @token-exempt: issue ref, not a color
/// given sample, honoring the screen-state classification:
///
///   * `Live` -> raw `screen_quality` (fps/30 clamped).
///   * `Static { held_fps, .. }` -> held value normalized the same way the
///     live path does (held_fps / 30, clamped).
///   * `NoFrames` -> raw zero (or whatever `screen_quality` resolved to,
///     which is also zero in practice).
///
/// Pulled out so unit tests can drive the Y-coordinate decision without
/// constructing a full polyline string.
pub(crate) fn screen_chart_quality(sample: &SignalSample) -> f64 {
    match sample.screen_state() {
        ScreenSampleState::Static { held_fps, .. } => (held_fps / 30.0).clamp(0.0, 1.0),
        // NoFrames and Live both use the recorded quality. NoFrames drops
        // to zero (visually distinct from the flat held line above), and
        // Live preserves the existing rendering behavior.
        ScreenSampleState::Live | ScreenSampleState::NoFrames => sample.screen_quality,
    }
}

/// Round a value up to a "nice" number for axis labels.
fn nice_ceil(val: f64) -> f64 {
    if val <= 0.0 {
        return 10.0;
    }
    let magnitude = 10.0_f64.powf(val.log10().floor());
    let normalized = val / magnitude;
    let nice = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_level_from_quality_boundaries() {
        assert_eq!(SignalLevel::from_quality(1.0), SignalLevel::Excellent);
        assert_eq!(SignalLevel::from_quality(0.9), SignalLevel::Excellent);
        assert_eq!(SignalLevel::from_quality(0.89), SignalLevel::Good);
        assert_eq!(SignalLevel::from_quality(0.75), SignalLevel::Good);
        assert_eq!(SignalLevel::from_quality(0.74), SignalLevel::Fair);
        assert_eq!(SignalLevel::from_quality(0.5), SignalLevel::Fair);
        assert_eq!(SignalLevel::from_quality(0.49), SignalLevel::Poor);
        assert_eq!(SignalLevel::from_quality(0.25), SignalLevel::Poor);
        assert_eq!(SignalLevel::from_quality(0.24), SignalLevel::Bad);
        assert_eq!(SignalLevel::from_quality(0.01), SignalLevel::Bad);
        assert_eq!(SignalLevel::from_quality(0.0), SignalLevel::Lost);
    }

    #[test]
    fn signal_level_bars() {
        assert_eq!(SignalLevel::Excellent.bars(), 5);
        assert_eq!(SignalLevel::Lost.bars(), 0);
    }

    #[test]
    fn combined_quality_both() {
        let q = combined_quality(0.8, 0.6, 0.0, true, true, false);
        assert!((q - 0.7).abs() < 1e-9);
    }

    #[test]
    fn combined_quality_audio_only() {
        let q = combined_quality(0.8, 0.0, 0.0, true, false, false);
        assert!((q - 0.8).abs() < 1e-9);
    }

    #[test]
    fn combined_quality_neither() {
        let q = combined_quality(0.0, 0.0, 0.0, false, false, false);
        assert!((q - 1.0).abs() < 1e-9);
    }

    #[test]
    fn combined_quality_all_three() {
        let q = combined_quality(0.9, 0.6, 0.3, true, true, true);
        assert!((q - 0.6).abs() < 1e-9);
    }

    #[test]
    fn combined_quality_screen_only() {
        let q = combined_quality(0.0, 0.0, 0.7, false, false, true);
        assert!((q - 0.7).abs() < 1e-9);
    }

    #[test]
    fn push_sample_records_screen_resolution() {
        let mut history = PeerSignalHistory::new();
        let data = SampleData {
            video_fps: 30.0,
            video_bitrate_kbps: 800.0,
            video_resolution: "1280x720".to_string(),
            audio_bitrate_kbps: 64.0,
            audio_expand_rate: 0.0,
            audio_buffer_ms: 60.0,
            screen_enabled: true,
            screen_fps: 15.0,
            screen_bitrate_kbps: 1200.0,
            screen_resolution: "1920x1080".to_string(),
            screen_source_resolution: "1920x1080".to_string(),
            screen_encoder_target_bitrate_kbps: 0,
            screen_adaptive_tier: String::new(),
            screen_cause_hint: String::new(),
            peer_status_age_ms: None,
            latency_ms: 40.0,
            audio_enabled: true,
            video_enabled: true,
        };
        history.push_sample_at(&data, 1_000.0);

        let samples = history.samples_vec();
        assert_eq!(samples.len(), 1);
        let s = &samples[0];
        assert_eq!(s.screen_resolution, "1920x1080");
        assert_eq!(s.screen_source_resolution, "1920x1080");
        assert!(s.screen_enabled);
        assert!((s.screen_fps - 15.0).abs() < 1e-9);
        // Screen quality is fps / 30 when enabled.
        assert!((s.screen_quality - 0.5).abs() < 1e-9);
    }

    #[test]
    fn push_sample_screen_quality_zero_when_disabled() {
        let mut history = PeerSignalHistory::new();
        let data = SampleData {
            screen_enabled: false,
            screen_fps: 15.0,
            screen_bitrate_kbps: 1200.0,
            screen_resolution: String::new(),
            screen_source_resolution: String::new(),
            video_enabled: true,
            audio_enabled: true,
            ..Default::default()
        };
        history.push_sample_at(&data, 2_000.0);
        let samples = history.samples_vec();
        assert_eq!(samples[0].screen_quality, 0.0);
        assert_eq!(samples[0].screen_resolution, "");
        assert_eq!(samples[0].screen_source_resolution, "");
    }

    #[test]
    fn infer_video_tier_classifies_screen_resolutions() {
        // Long-form tier name is used by the camera-video line and as the
        // input to the short-form lookup below.
        assert_eq!(infer_video_tier("1920x1080"), "Full HD");
        assert_eq!(infer_video_tier("1280x720"), "HD");
        assert_eq!(infer_video_tier("640x480"), "Medium");
        assert_eq!(infer_video_tier(""), "");
        assert_eq!(infer_video_tier("garbage"), "");
    }

    #[test]
    fn infer_video_tier_short_abbreviates_common_labels() {
        // Screen tooltip uses the abbreviated form so the row stays narrow.
        // FHD / QHD / UHD are the only abbreviations applied; HD and the
        // lower tiers are already compact and keep their long form.
        assert_eq!(infer_video_tier_short("1920x1080"), "FHD");
        assert_eq!(infer_video_tier_short("1280x720"), "HD");
        assert_eq!(infer_video_tier_short("960x540"), "Standard");
        assert_eq!(infer_video_tier_short("640x480"), "Medium");
        // Unknown / empty inputs pass through.
        assert_eq!(infer_video_tier_short(""), "");
        assert_eq!(infer_video_tier_short("garbage"), "");
    }

    // -----------------------------------------------------------------
    // Source vs received tooltip behavior. These tests exercise the pure
    // formatter, so we can drive them through host `cargo test` without
    // any browser / DOM dependency.
    // -----------------------------------------------------------------

    fn screen_sample(received: &str, source: &str) -> SignalSample {
        SignalSample {
            timestamp_ms: 0.0,
            audio_quality: 0.0,
            video_quality: 0.0,
            screen_quality: 0.5,
            video_fps: 0.0,
            video_bitrate_kbps: 0.0,
            video_resolution: String::new(),
            audio_bitrate_kbps: 0.0,
            audio_expand_rate: 0.0,
            audio_buffer_ms: 0.0,
            screen_enabled: true,
            screen_fps: 8.0,
            screen_bitrate_kbps: 720.0,
            screen_resolution: received.to_string(),
            screen_source_resolution: source.to_string(),
            screen_encoder_target_bitrate_kbps: 0,
            screen_adaptive_tier: String::new(),
            screen_cause_hint: String::new(),
            screen_fps_held: None,
            screen_bitrate_kbps_held: None,
            peer_status_age_ms: None,
            latency_ms: 0.0,
        }
    }

    #[test]
    fn screen_downscale_percent_pixel_area_math() {
        // 2× linear downscale -> 75% pixel-area loss.
        assert_eq!(screen_downscale_percent("2560x1440", "1280x720"), Some(75));
        // 1.5× linear-ish (1920x1080 -> 1280x720) -> ~55.6% rounded to 56.
        assert_eq!(screen_downscale_percent("1920x1080", "1280x720"), Some(56));
        // Source == Received -> no badge.
        assert_eq!(screen_downscale_percent("1920x1080", "1920x1080"), None);
        // Received >= Source area (no downscale) -> no badge.
        assert_eq!(screen_downscale_percent("1280x720", "1920x1080"), None);
        // Source unknown -> no badge.
        assert_eq!(screen_downscale_percent("", "1920x1080"), None);
        // Received unknown -> no badge.
        assert_eq!(screen_downscale_percent("1920x1080", ""), None);
        // Sub-1% loss rounds to 0 and returns None to avoid "↓0%" noise.
        // 1920x1080 -> 1920x1079 = 0.09% area loss -> None.
        assert_eq!(screen_downscale_percent("1920x1080", "1920x1079"), None);
        // Unparseable input -> no badge.
        assert_eq!(screen_downscale_percent("garbage", "1920x1080"), None);
    }

    #[test]
    fn screen_downscale_color_severity_buckets() {
        // <25% -> muted text.
        assert_eq!(screen_downscale_color(0), theme_color::TEXT_MUTED);
        assert_eq!(screen_downscale_color(24), theme_color::TEXT_MUTED);
        // 25-49% -> amber warning.
        assert_eq!(screen_downscale_color(25), theme_color::WARNING_TEXT);
        assert_eq!(screen_downscale_color(49), theme_color::WARNING_TEXT);
        // >=50% -> danger.
        assert_eq!(screen_downscale_color(50), theme_color::ERROR_TEXT);
        assert_eq!(screen_downscale_color(75), theme_color::ERROR_TEXT);
        assert_eq!(screen_downscale_color(100), theme_color::ERROR_TEXT);
    }

    #[test]
    fn tooltip_collapses_when_source_equals_received() {
        // Post-#903 tightening: `Screen ` (no colon), abbreviated tier name, // @token-exempt: issue ref, not a color
        // middle-dot separators, joined units. // @token-exempt: issue ref, not a color
        let s = screen_sample("1920x1080", "1920x1080");
        let line = build_screen_tooltip_line(&s, true);
        assert!(line.contains("Screen 1920x1080"));
        assert!(line.contains("(FHD)"));
        // No legacy noise.
        assert!(!line.contains("Screen:"));
        assert!(!line.contains("(Full HD)"));
        assert!(!line.contains(" | "));
        // Single value — no arrow, no badge.
        assert!(!line.contains("Source"));
        assert!(!line.contains("\u{2192}"));
        assert!(!line.contains("\u{2193}"));
    }

    #[test]
    fn tooltip_shows_arrow_when_source_differs_from_received() {
        let s = screen_sample("1280x720", "2560x1440");
        let line = build_screen_tooltip_line(&s, true);
        // Post-tightening: bare numbers + arrow, no "Source" / "Received"
        // labels, no tier names in the expanded form.
        assert!(line.contains("Screen 2560x1440 \u{2192} 1280x720"));
        assert!(!line.contains("Source 2560x1440"));
        assert!(!line.contains("Received 1280x720"));
        assert!(!line.contains("(HD)"));
        assert!(!line.contains("(Full HD)"));
        // 2560x1440 -> 1280x720 = 75% pixel-area loss, danger color.
        // Compact badge: no "pixel area" suffix.
        assert!(line.contains("\u{2193}75%"));
        assert!(!line.contains("pixel area"));
        assert!(line.contains(theme_color::ERROR_TEXT));
    }

    #[test]
    fn tooltip_uses_warning_color_for_moderate_downscale() {
        // 1920x1080 -> 1600x900 = 30.6% loss -> warning. Compact badge.
        let s = screen_sample("1600x900", "1920x1080");
        let line = build_screen_tooltip_line(&s, true);
        assert!(line.contains(theme_color::WARNING_TEXT));
        assert!(line.contains("\u{2193}31%"));
        assert!(!line.contains("pixel area"));
    }

    #[test]
    fn tooltip_uses_muted_color_for_minor_downscale() {
        // 1920x1080 -> 1820x1024 = ~10.1% loss -> muted.
        let s = screen_sample("1820x1024", "1920x1080");
        let line = build_screen_tooltip_line(&s, true);
        assert!(line.contains(theme_color::TEXT_MUTED));
        assert!(line.contains("\u{2193}"));
        assert!(!line.contains("pixel area"));
    }

    #[test]
    fn tooltip_received_only_when_source_unknown() {
        let s = screen_sample("1280x720", "");
        let line = build_screen_tooltip_line(&s, true);
        // Source unknown collapses to the single-value shape; HD stays
        // long-form because it has no widely-known shorter abbreviation.
        assert!(line.contains("Screen 1280x720"));
        assert!(line.contains("(HD)"));
        assert!(!line.contains("Screen:"));
        // Older publisher -> no arrow, no badge.
        assert!(!line.contains("Source"));
        assert!(!line.contains("\u{2192}"));
        assert!(!line.contains("\u{2193}"));
    }

    #[test]
    fn tooltip_legacy_shape_when_received_unknown() {
        // Both unknown -> fall back to no-resolution shape (pre-#891 baseline). // @token-exempt: issue ref, not a color
        // Post-tightening: `Screen` with no colon, dot-joined metrics tail.
        let s = screen_sample("", "");
        let line = build_screen_tooltip_line(&s, true);
        assert!(line.starts_with("<span"));
        assert!(line.contains("Screen"));
        assert!(!line.contains("Screen:"));
        // Compact units: `8.0fps`, `720kbps` (no space).
        assert!(line.contains("8.0fps"));
        assert!(line.contains("720kbps"));
        assert!(!line.contains("Source"));
        assert!(!line.contains("Received"));
    }

    #[test]
    fn tooltip_empty_when_screen_disabled() {
        let mut s = screen_sample("1280x720", "1920x1080");
        s.screen_enabled = false;
        assert_eq!(build_screen_tooltip_line(&s, true), "");
    }

    #[test]
    fn tooltip_metrics_suffix_always_present() {
        // All three branches use the dot-joined compact metrics tail.
        for (recv, src) in [
            ("1280x720", "1280x720"),
            ("1280x720", "1920x1080"),
            ("1280x720", ""),
        ] {
            let s = screen_sample(recv, src);
            let line = build_screen_tooltip_line(&s, true);
            assert!(line.contains("8.0fps"), "missing 8.0fps in {line}");
            assert!(line.contains("720kbps"), "missing 720kbps in {line}");
            assert!(
                line.contains("\u{00B7}"),
                "missing middle-dot separator in {line}"
            );
        }
    }

    #[test]
    fn tooltip_drops_badge_at_or_below_zero_rounded() {
        // <0.5% downscale rounds to 0 — we must NOT render "↓0%".
        let s = screen_sample("1919x1080", "1920x1080");
        let line = build_screen_tooltip_line(&s, true);
        // The two resolutions differ as strings so the expanded shape
        // still fires (we get the arrow), but no badge because the area
        // delta rounds to 0.
        assert!(line.contains("\u{2192}"));
        assert!(!line.contains("\u{2193}"));
        assert!(!line.contains("pixel area"));
    }

    // -----------------------------------------------------------------
    // Issue #903: Cause line rendering. Sourced from publisher-stamped // @token-exempt: issue ref, not a color
    // `VideoMetadata` fields. Post-tightening copy is compact:
    //   * No data → empty (older publisher or unconstrained tier).
    //   * Bitrate + tier → `Cause: <N>kbps · tier '<tier>'`.
    //   * Cause hint only → `Cause: <hint>`.
    //   * All three → `Cause: <hint> · <N>kbps · tier '<tier>'`.
    // The Screen line wrapper drops the row entirely when the helper
    // returns an empty string, so it is load-bearing that `""` and
    // not a placeholder is returned for the no-data cases.
    // -----------------------------------------------------------------

    #[test]
    fn cause_line_empty_when_no_publisher_data() {
        // Older publisher / unconstrained AQ tier: all three fields
        // arrive as proto3 defaults. We MUST omit the line — shipping
        // "not yet instrumented" or any placeholder regressed in #891. // @token-exempt: issue ref, not a color
        let s = screen_sample("1280x720", "2560x1440");
        assert_eq!(s.screen_encoder_target_bitrate_kbps, 0);
        assert!(s.screen_adaptive_tier.is_empty());
        assert!(s.screen_cause_hint.is_empty());
        assert_eq!(build_screen_cause_line(&s, true), "");
    }

    #[test]
    fn cause_line_primary_shape_with_bitrate_and_tier() {
        // Bitrate + tier with no hint → compact "Cause: 800kbps · tier 'low'".
        let mut s = screen_sample("1280x720", "1920x1080");
        s.screen_encoder_target_bitrate_kbps = 800;
        s.screen_adaptive_tier = "low".to_string();
        let line = build_screen_cause_line(&s, true);
        assert!(line.contains("Cause: 800kbps \u{00B7} tier 'low'"));
        // Wordy phrasing must be gone.
        assert!(!line.contains("encoder target"));
        assert!(!line.contains("limited by"));
        assert!(!line.contains("adaptive-quality"));
        assert!(line.contains(theme_color::TEXT_MUTED));
    }

    #[test]
    fn cause_line_hint_only_fallback() {
        // Older / partial publisher only stamps cause_hint. Compact
        // fallback: bare "Cause: <hint>" — no bitrate, no tier word.
        let mut s = screen_sample("1280x720", "1920x1080");
        s.screen_cause_hint = "cpu-pressure".to_string();
        let line = build_screen_cause_line(&s, true);
        assert!(line.contains("Cause: cpu-pressure"));
        assert!(!line.contains("kbps"));
        assert!(!line.contains("tier"));
        assert!(line.contains(theme_color::TEXT_MUTED));
    }

    #[test]
    fn cause_line_combined_shape_with_all_three() {
        // All three present → `Cause: <hint> · <N>kbps · tier '<tier>'`.
        // Hint leads as the human summary; the dot-joined evidence
        // follows for users who want the concrete numbers.
        let mut s = screen_sample("1280x720", "2560x1440");
        s.screen_encoder_target_bitrate_kbps = 500;
        s.screen_adaptive_tier = "low".to_string();
        s.screen_cause_hint = "network-rtt".to_string();
        let line = build_screen_cause_line(&s, true);
        assert!(
            line.contains("Cause: network-rtt \u{00B7} 500kbps \u{00B7} tier 'low'"),
            "unexpected combined cause line: {line}"
        );
        // No legacy wordy phrasing.
        assert!(!line.contains("encoder target"));
        assert!(!line.contains("\u{2014}")); // em-dash gone, dot is the joiner
    }

    #[test]
    fn cause_line_empty_when_screen_disabled() {
        let mut s = screen_sample("1280x720", "2560x1440");
        s.screen_enabled = false;
        // Even with publisher data, hide when screen series is off.
        s.screen_encoder_target_bitrate_kbps = 800;
        s.screen_adaptive_tier = "low".to_string();
        s.screen_cause_hint = "bitrate-limited".to_string();
        assert_eq!(build_screen_cause_line(&s, true), "");
    }

    #[test]
    fn cause_line_empty_when_screen_series_hidden() {
        let mut s = screen_sample("1280x720", "2560x1440");
        s.screen_encoder_target_bitrate_kbps = 800;
        s.screen_adaptive_tier = "low".to_string();
        s.screen_cause_hint = "bitrate-limited".to_string();
        assert_eq!(build_screen_cause_line(&s, false), "");
    }

    #[test]
    fn cause_line_no_placeholder_text() {
        // Regression test for the #891 lesson — earlier code shipped a // @token-exempt: issue ref, not a color
        // "not yet instrumented" placeholder when no publisher data was
        // available. The omit-line behaviour is the contract; verify the
        // helper never emits that placeholder for any of the no-data
        // configurations (resolution match, resolution mismatch, source
        // unknown).
        let cases = [
            ("1920x1080", "1920x1080"),
            ("1280x720", "2560x1440"),
            ("1280x720", ""),
            ("", ""),
        ];
        for (recv, src) in cases {
            let s = screen_sample(recv, src);
            let line = build_screen_cause_line(&s, true);
            assert!(!line.contains("not yet instrumented"));
            assert!(!line.contains("#903")); // @token-exempt: issue ref, not a color
        }
    }

    #[test]
    fn push_sample_carries_source_resolution_through_to_signal_sample() {
        let mut history = PeerSignalHistory::new();
        let data = SampleData {
            screen_enabled: true,
            screen_fps: 10.0,
            screen_bitrate_kbps: 800.0,
            screen_resolution: "1280x720".to_string(),
            screen_source_resolution: "2560x1440".to_string(),
            video_enabled: true,
            audio_enabled: true,
            ..Default::default()
        };
        history.push_sample_at(&data, 5_000.0);
        let s = &history.samples_vec()[0];
        assert_eq!(s.screen_resolution, "1280x720");
        assert_eq!(s.screen_source_resolution, "2560x1440");
    }

    #[test]
    fn push_sample_carries_encoder_state_through_to_signal_sample() {
        // Issue #903: publisher-stamped encoder state must round-trip // @token-exempt: issue ref, not a color
        // through `SampleData` to `SignalSample` so the Cause line in
        // the tooltip has data to render. Regression guard against
        // forgetting to wire one of the three fields.
        let mut history = PeerSignalHistory::new();
        let data = SampleData {
            screen_enabled: true,
            screen_fps: 10.0,
            screen_bitrate_kbps: 800.0,
            screen_resolution: "1280x720".to_string(),
            screen_source_resolution: "2560x1440".to_string(),
            screen_encoder_target_bitrate_kbps: 500,
            screen_adaptive_tier: "low".to_string(),
            screen_cause_hint: "bitrate-limited".to_string(),
            video_enabled: true,
            audio_enabled: true,
            ..Default::default()
        };
        history.push_sample_at(&data, 6_000.0);
        let s = &history.samples_vec()[0];
        assert_eq!(s.screen_encoder_target_bitrate_kbps, 500);
        assert_eq!(s.screen_adaptive_tier, "low");
        assert_eq!(s.screen_cause_hint, "bitrate-limited");
    }

    #[test]
    fn nice_ceil_values() {
        assert!((nice_ceil(45.0) - 50.0).abs() < 1e-9);
        assert!((nice_ceil(150.0) - 200.0).abs() < 1e-9);
        assert!((nice_ceil(8.0) - 10.0).abs() < 1e-9);
        assert!((nice_ceil(0.0) - 10.0).abs() < 1e-9);
    }

    // -----------------------------------------------------------------
    // Issue #906: static-vs-no-frames classification.                  // @token-exempt: issue ref, not a color
    //
    // Modern video codecs emit zero encoded frames during truly static
    // screen-share content. The metrics then read `0fps | 0kbps`, which
    // is visually indistinguishable from a broken encoder. The state
    // machine holds the last-known value for up to 30s and renders
    // a `(static)` annotation while the publisher's heartbeat is fresh
    // (<5s old). Stale heartbeat or expired hold window falls back to
    // `(no frames)` so the user can distinguish quiet desktop from
    // broken connection.
    // -----------------------------------------------------------------

    /// Build a sample data builder pre-set with screen-enabled defaults so
    /// each test only spells out the fields it cares about.
    fn screen_sample_data() -> SampleData {
        SampleData {
            screen_enabled: true,
            screen_fps: 12.5,
            screen_bitrate_kbps: 850.0,
            screen_resolution: "1280x720".to_string(),
            screen_source_resolution: "1280x720".to_string(),
            video_enabled: false,
            audio_enabled: false,
            peer_status_age_ms: Some(1_000.0),
            ..Default::default()
        }
    }

    #[test]
    fn screen_state_live_when_metrics_non_zero() {
        let mut history = PeerSignalHistory::new();
        history.push_sample_at(&screen_sample_data(), 1_000.0);
        let s = &history.samples_vec()[0];
        assert_eq!(s.screen_state(), ScreenSampleState::Live);
        // Held fields are None on a live sample so the tooltip / chart
        // render the raw values directly.
        assert!(s.screen_fps_held.is_none());
        assert!(s.screen_bitrate_kbps_held.is_none());
    }

    #[test]
    fn test_zero_with_recent_non_zero_renders_as_static() {
        // Record a live sample, then a zero sample 2s later with the
        // heartbeat fresh. The second sample should classify as Static
        // and the tooltip should hold the prior FPS / kbps values.
        let mut history = PeerSignalHistory::new();
        history.push_sample_at(&screen_sample_data(), 0.0);

        let mut zero = screen_sample_data();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        zero.peer_status_age_ms = Some(1_000.0); // 1s old -> fresh
        history.push_sample_at(&zero, 2_000.0);

        let s = &history.samples_vec()[1];
        match s.screen_state() {
            ScreenSampleState::Static {
                held_fps,
                held_bitrate_kbps,
            } => {
                assert!(
                    (held_fps - 12.5).abs() < 1e-9,
                    "expected held_fps 12.5, got {held_fps}"
                );
                assert!(
                    (held_bitrate_kbps - 850.0).abs() < 1e-9,
                    "expected held_bitrate 850, got {held_bitrate_kbps}"
                );
            }
            other => panic!("expected Static, got {other:?}"),
        }
        // Tooltip wording: held values with `(static)` annotation.
        let line = build_screen_tooltip_line(s, true);
        assert!(
            line.contains("12.5fps (static)"),
            "missing 12.5fps (static) in {line}"
        );
        assert!(
            line.contains("850kbps (static)"),
            "missing 850kbps (static) in {line}"
        );
        // No no-frames marker.
        assert!(!line.contains("(no frames)"));
    }

    #[test]
    fn test_zero_with_stale_heartbeat_renders_as_no_frames() {
        let mut history = PeerSignalHistory::new();
        history.push_sample_at(&screen_sample_data(), 0.0);

        let mut zero = screen_sample_data();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        // Heartbeat is 7s stale -> NoFrames even though prior live value
        // is in the hold window.
        zero.peer_status_age_ms = Some(7_000.0);
        history.push_sample_at(&zero, 2_000.0);

        let s = &history.samples_vec()[1];
        assert_eq!(s.screen_state(), ScreenSampleState::NoFrames);
        let line = build_screen_tooltip_line(s, true);
        assert!(
            line.contains("0.0fps (no frames)"),
            "missing 0.0fps (no frames) in {line}"
        );
        assert!(
            line.contains("0kbps (no frames)"),
            "missing 0kbps (no frames) in {line}"
        );
        assert!(!line.contains("(static)"));
    }

    #[test]
    fn test_static_lasts_until_30s_then_falls_to_no_frames() {
        // Record a live sample at t=0, then a zero sample at t=31s with
        // the heartbeat still fresh. The hold window is 30s so the new
        // sample should NOT hold the value any longer — `NoFrames` wins.
        let mut history = PeerSignalHistory::new();
        history.push_sample_at(&screen_sample_data(), 0.0);

        let mut zero = screen_sample_data();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        zero.peer_status_age_ms = Some(1_000.0); // heartbeat fresh
        history.push_sample_at(&zero, 31_000.0);

        let s = &history.samples_vec()[1];
        assert_eq!(s.screen_state(), ScreenSampleState::NoFrames);
        let line = build_screen_tooltip_line(s, true);
        assert!(
            line.contains("(no frames)"),
            "expected (no frames) in {line}"
        );
        assert!(!line.contains("(static)"));
    }

    #[test]
    fn test_static_latches_across_consecutive_zero_samples() {
        // Sequence: live @ 0s, zero @ 1s (Static, held=12.5), zero @ 2s.
        // The third sample's own `screen_fps_held` should still be 12.5
        // — the held value latches across consecutive zeros so the line
        // stays flat without rescanning all the way back to the live one
        // every sample.
        let mut history = PeerSignalHistory::new();
        history.push_sample_at(&screen_sample_data(), 0.0);

        let mut zero = screen_sample_data();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        zero.peer_status_age_ms = Some(500.0);
        history.push_sample_at(&zero, 1_000.0);
        history.push_sample_at(&zero, 2_000.0);

        let samples = history.samples_vec();
        for (idx, expected_state) in [1, 2].iter().map(|&i| {
            (
                i,
                ScreenSampleState::Static {
                    held_fps: 12.5,
                    held_bitrate_kbps: 850.0,
                },
            )
        }) {
            assert_eq!(
                samples[idx].screen_state(),
                expected_state,
                "sample idx {idx} state mismatch"
            );
        }
    }

    #[test]
    fn test_transition_back_to_non_zero_drops_annotation() {
        // Sequence: live @ 0, zero @ 1 (Static), live @ 2. The third
        // sample must classify as Live and the tooltip must not carry
        // the `(static)` annotation.
        let mut history = PeerSignalHistory::new();
        history.push_sample_at(&screen_sample_data(), 0.0);

        let mut zero = screen_sample_data();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        zero.peer_status_age_ms = Some(500.0);
        history.push_sample_at(&zero, 1_000.0);

        // New live sample picks up at full fps.
        let mut live2 = screen_sample_data();
        live2.screen_fps = 14.0;
        live2.screen_bitrate_kbps = 900.0;
        live2.peer_status_age_ms = Some(500.0);
        history.push_sample_at(&live2, 2_000.0);

        let s = &history.samples_vec()[2];
        assert_eq!(s.screen_state(), ScreenSampleState::Live);
        assert!(s.screen_fps_held.is_none());
        assert!(s.screen_bitrate_kbps_held.is_none());

        let line = build_screen_tooltip_line(s, true);
        assert!(line.contains("14.0fps"));
        assert!(line.contains("900kbps"));
        assert!(!line.contains("(static)"));
        assert!(!line.contains("(no frames)"));
    }

    #[test]
    fn test_no_held_value_when_no_prior_live() {
        // First sample is itself zero — there is no prior live value to
        // hold. The state must be NoFrames regardless of heartbeat
        // freshness.
        let mut history = PeerSignalHistory::new();
        let mut zero = screen_sample_data();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        zero.peer_status_age_ms = Some(500.0); // fresh
        history.push_sample_at(&zero, 1_000.0);

        let s = &history.samples_vec()[0];
        assert_eq!(s.screen_state(), ScreenSampleState::NoFrames);
        let line = build_screen_tooltip_line(s, true);
        assert!(line.contains("0.0fps (no frames)"));
        assert!(line.contains("0kbps (no frames)"));
    }

    #[test]
    fn test_no_held_value_when_no_heartbeat_yet() {
        // `peer_status_age_ms = None` means we haven't observed any
        // heartbeat — be conservative and treat the zero as NoFrames.
        // Otherwise we would paper over an unproven publisher with held
        // values that may not be accurate.
        let mut history = PeerSignalHistory::new();
        history.push_sample_at(&screen_sample_data(), 0.0);

        let mut zero = screen_sample_data();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        zero.peer_status_age_ms = None;
        history.push_sample_at(&zero, 1_000.0);

        let s = &history.samples_vec()[1];
        assert_eq!(s.screen_state(), ScreenSampleState::NoFrames);
    }

    #[test]
    fn test_static_holds_bitrate_when_fps_is_live() {
        // Edge: one metric is zero while the other is live. We classify
        // by treating any non-zero metric as Live (so this is Live, not
        // Static-half). Held fields are populated for the zero metric so
        // a *future* sample where both go to zero can still pick up the
        // bitrate from this sample's held field.
        let mut history = PeerSignalHistory::new();
        history.push_sample_at(&screen_sample_data(), 0.0);

        let mut mixed = screen_sample_data();
        mixed.screen_fps = 10.0;
        mixed.screen_bitrate_kbps = 0.0;
        mixed.peer_status_age_ms = Some(500.0);
        history.push_sample_at(&mixed, 1_000.0);

        let s = &history.samples_vec()[1];
        // Live wins because screen_fps is non-zero.
        assert_eq!(s.screen_state(), ScreenSampleState::Live);
        // But the bitrate held value is populated so it's available for
        // a future fully-zero sample.
        assert_eq!(s.screen_fps_held, None);
        assert_eq!(s.screen_bitrate_kbps_held, Some(850.0));
    }

    #[test]
    fn test_chart_y_during_static_uses_held_value() {
        // The screen chart polyline must plot Static samples at the held
        // value's Y position instead of dropping to zero. We compare
        // `screen_chart_quality` against the live sample's quality.
        let mut history = PeerSignalHistory::new();
        let live = screen_sample_data(); // screen_fps 12.5 -> quality 12.5/30
        history.push_sample_at(&live, 0.0);

        let mut zero = screen_sample_data();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        zero.peer_status_age_ms = Some(500.0);
        history.push_sample_at(&zero, 1_000.0);

        let samples = history.samples_vec();
        let live_q = screen_chart_quality(&samples[0]);
        let static_q = screen_chart_quality(&samples[1]);
        assert!(
            (live_q - 12.5 / 30.0).abs() < 1e-9,
            "live sample quality unexpected: {live_q}"
        );
        // Static must equal the live value's quality (flat line) and
        // must NOT be zero (which would be the bad pre-#906 behavior).  // @token-exempt: issue ref, not a color
        assert!(
            (static_q - 12.5 / 30.0).abs() < 1e-9,
            "static sample chart Y should match held value, got {static_q}"
        );
        assert!(static_q > 0.0, "static chart Y must not flatline at zero");
    }

    #[test]
    fn test_chart_y_during_no_frames_drops_to_zero() {
        // NoFrames samples should plot at zero so the chart visually
        // distinguishes them from static (held) periods.
        let mut history = PeerSignalHistory::new();
        let mut zero = screen_sample_data();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        // Stale heartbeat -> NoFrames.
        zero.peer_status_age_ms = Some(10_000.0);
        history.push_sample_at(&zero, 1_000.0);

        let s = &history.samples_vec()[0];
        assert_eq!(s.screen_state(), ScreenSampleState::NoFrames);
        let q = screen_chart_quality(s);
        assert!(q.abs() < 1e-9, "NoFrames chart Y should be 0, got {q}");
    }

    #[test]
    fn test_static_tooltip_preserves_resolution_prefix() {
        // The `(static)` annotation only modifies the FPS / kbps tail.
        // The resolution prefix (with tier label or arrow / badge) must
        // still appear — we are not collapsing the line.
        let mut history = PeerSignalHistory::new();
        let mut live = screen_sample_data();
        live.screen_resolution = "1920x1080".to_string();
        live.screen_source_resolution = "1920x1080".to_string();
        history.push_sample_at(&live, 0.0);

        let mut zero = screen_sample_data();
        zero.screen_resolution = "1920x1080".to_string();
        zero.screen_source_resolution = "1920x1080".to_string();
        zero.screen_fps = 0.0;
        zero.screen_bitrate_kbps = 0.0;
        zero.peer_status_age_ms = Some(500.0);
        history.push_sample_at(&zero, 1_000.0);

        let s = &history.samples_vec()[1];
        let line = build_screen_tooltip_line(s, true);
        assert!(
            line.contains("Screen 1920x1080"),
            "missing prefix in {line}"
        );
        assert!(line.contains("(FHD)"), "missing tier in {line}");
        assert!(
            line.contains("12.5fps (static)"),
            "missing held fps in {line}"
        );
    }

    #[test]
    fn test_peer_status_age_ms_round_trips_through_sample_data() {
        // Regression guard: forgetting to wire `peer_status_age_ms`
        // through SampleData -> SignalSample would silently revert the
        // classifier to NoFrames in every case.
        let mut history = PeerSignalHistory::new();
        let mut data = screen_sample_data();
        data.peer_status_age_ms = Some(2_500.0);
        history.push_sample_at(&data, 1_000.0);
        let s = &history.samples_vec()[0];
        assert_eq!(s.peer_status_age_ms, Some(2_500.0));
    }

    // ── Portal positioning math ──────────────────────────────────────────
    // `compute_popup_position` is the pure-data heart of the portal anchor;
    // every browser-side branch (initial render, resize, scroll,
    // ResizeObserver fire) ultimately funnels into this function.  Cover
    // each branch — basic placement (popup upper-right corner at the
    // button's (25% across, vertical midpoint) point), left-edge clamp,
    // right-edge clamp, top-edge clamp, bottom-edge clamp, and the
    // dense-grid sweep — so a future refactor cannot silently strand the
    // popup off-screen.

    /// Build a `Rect` from `(left, top, w, h)`.
    fn rect_from(left: f64, top: f64, w: f64, h: f64) -> super::Rect {
        super::Rect {
            left,
            top,
            right: left + w,
            bottom: top + h,
        }
    }

    #[test]
    fn popup_overlays_button_upper_left_quadrant_when_space_available() {
        // 1920x1080 viewport, 40x20 anchor (signal-quality button) at
        // (300,200), 200x100 popup. Plenty of room — the popup's
        // upper-right corner should land at
        //   (btn.left + btn.width  * X_FRAC, btn.top + btn.height * Y_FRAC)
        // = (300 + 40*0.25, 200 + 20*0.5) = (310, 210).
        // So popup_left = 310 - 200 = 110, popup_top = 210.
        let anchor = rect_from(300.0, 200.0, 40.0, 20.0);
        let popup_w = 200.0;
        let popup_h = 100.0;
        let (left, top) = super::compute_popup_position(anchor, popup_w, popup_h, 1920.0, 1080.0);
        let expected_right = 300.0 + 40.0 * super::POPUP_BUTTON_OVERLAY_X_FRACTION;
        let expected_left = expected_right - popup_w;
        let expected_top = 200.0 + 20.0 * super::POPUP_BUTTON_OVERLAY_Y_FRACTION;
        assert!(
            (left - expected_left).abs() < 0.01,
            "expected left == {expected_left}, got {left}"
        );
        assert!(
            (top - expected_top).abs() < 0.01,
            "expected top == {expected_top}, got {top}"
        );
        // Sanity: the popup's body sits mostly to the LEFT of the button
        // (its right edge is at 25% across the button, so its bulk is
        // to the left).
        let popup_right = left + popup_w;
        assert!(
            (popup_right - expected_right).abs() < 0.01,
            "popup right edge should sit at {expected_right} (25% across button), got {popup_right}"
        );
        assert!(
            popup_right < anchor.right,
            "popup right edge should sit inside the button, not past its right edge"
        );
        // And the popup's top edge sits at the button's vertical midpoint.
        let btn_vmid = anchor.top + anchor.height() * 0.5;
        assert!(
            (top - btn_vmid).abs() < 0.01,
            "popup top should sit at button vertical midpoint ({btn_vmid}), got {top}"
        );
    }

    #[test]
    fn popup_clamps_left_when_button_near_left_edge() {
        // Anchor near the left edge: `target_left = btn.left + btn.width*X_FRAC
        // - popup_w` goes deeply negative, so the clamp pulls the popup
        // back to `VIEWPORT_MARGIN_PX`.
        let anchor = rect_from(4.0, 200.0, 32.0, 32.0);
        let (left, _top) = super::compute_popup_position(anchor, 420.0, 300.0, 1920.0, 1080.0);
        assert!(
            (left - super::VIEWPORT_MARGIN_PX).abs() < 0.01,
            "expected clamp to left margin {}, got {left}",
            super::VIEWPORT_MARGIN_PX
        );
    }

    #[test]
    fn popup_clamps_when_button_near_right_edge() {
        // Narrow viewport with the button hugging the right edge. The
        // unclamped target_left would push the popup's right edge past
        // the viewport margin, so the clamp should pull it back to
        // `viewport_w - popup_w - margin`.
        let anchor = rect_from(495.0, 50.0, 40.0, 32.0);
        let viewport_w = 500.0;
        let popup_w = 420.0;
        let (left, _top) = super::compute_popup_position(anchor, popup_w, 200.0, viewport_w, 800.0);
        let expected_max_left = viewport_w - popup_w - super::VIEWPORT_MARGIN_PX;
        // Sanity: the unclamped target really did overflow the right margin.
        let target_right = anchor.left + anchor.width() * super::POPUP_BUTTON_OVERLAY_X_FRACTION;
        let target_left_unclamped = target_right - popup_w;
        assert!(
            target_left_unclamped > expected_max_left,
            "test sanity: unclamped target_left ({target_left_unclamped}) should exceed max_left ({expected_max_left})"
        );
        assert!(
            (left - expected_max_left).abs() < 0.01,
            "expected clamp to right margin {expected_max_left}, got {left}"
        );
        // Popup is fully on-screen on the right side.
        assert!(left + popup_w <= viewport_w);
    }

    #[test]
    fn popup_clamps_vertically_when_button_is_near_bottom() {
        // Anchor at the bottom of the viewport — the downward Y_FRAC
        // shift would push the popup off-screen, so the clamp pulls
        // it back to `viewport_h - popup_h - margin`.
        let anchor = rect_from(100.0, 950.0, 32.0, 32.0);
        let popup_h = 500.0;
        let viewport_h = 1000.0;
        let (_left, top) =
            super::compute_popup_position(anchor, 420.0, popup_h, 1920.0, viewport_h);
        let expected_max_top = viewport_h - popup_h - super::VIEWPORT_MARGIN_PX;
        assert!(
            (top - expected_max_top).abs() < 0.01,
            "expected clamp to {expected_max_top}, got {top}"
        );
        assert!(top + popup_h <= viewport_h);
    }

    #[test]
    fn popup_clamps_vertically_when_button_is_above_viewport() {
        // Negative `btn.top` (scrolled above viewport) deep enough that
        // even after the downward Y_FRAC shift the target is still
        // negative. Popup must still sit inside the visible region with
        // at least VIEWPORT_MARGIN_PX breathing room from the top edge.
        let anchor = rect_from(100.0, -200.0, 32.0, 32.0);
        let (_left, top) = super::compute_popup_position(anchor, 420.0, 400.0, 1920.0, 1080.0);
        assert!(
            top >= super::VIEWPORT_MARGIN_PX,
            "expected clamp >= {}, got {top}",
            super::VIEWPORT_MARGIN_PX
        );
    }

    #[test]
    fn popup_never_overflows_viewport_in_dense_grid() {
        // Sweep a few dense-grid scenarios to catch any clamp/flip
        // regression that would land the popup off-screen.
        let popup_w = 420.0;
        let popup_h = 400.0;
        let viewport_w = 1280.0;
        let viewport_h = 720.0;
        for (left, top) in [
            (10.0, 10.0),
            (1000.0, 10.0),
            (10.0, 600.0),
            (1000.0, 600.0),
            (640.0, 350.0),
        ] {
            let anchor = rect_from(left, top, 200.0, 150.0);
            let (l, t) =
                super::compute_popup_position(anchor, popup_w, popup_h, viewport_w, viewport_h);
            assert!(
                l >= 0.0 && (l + popup_w) <= viewport_w && t >= 0.0 && (t + popup_h) <= viewport_h,
                "popup off-screen at anchor=({left},{top}): pos=({l},{t})"
            );
        }
    }

    // -----------------------------------------------------------------
    // HCL bug #2: SignalMeterMode scope filter.
    //
    // The mode-driven helpers gate which series the popup renders. These
    // are pure-data predicates so they're trivially unit-testable; the
    // contract is contractual for the popup body that reads them and the
    // call sites in `attendants.rs` that pick the mode.
    // -----------------------------------------------------------------

    #[test]
    fn meter_mode_full_shows_every_series() {
        let m = super::SignalMeterMode::Full;
        assert!(m.shows_audio());
        assert!(m.shows_video());
        assert!(m.shows_screen());
    }

    #[test]
    fn meter_mode_screen_only_shows_only_screen() {
        let m = super::SignalMeterMode::ScreenOnly;
        assert!(!m.shows_audio());
        assert!(!m.shows_video());
        assert!(m.shows_screen());
    }

    #[test]
    fn meter_mode_no_screen_hides_screen() {
        let m = super::SignalMeterMode::NoScreen;
        assert!(m.shows_audio());
        assert!(m.shows_video());
        assert!(!m.shows_screen());
    }

    #[test]
    fn meter_mode_id_suffix_is_stable() {
        // The suffix is load-bearing for the popup-state map key and the
        // DOM id, so a refactor that renames a variant would break
        // every open popup's DOM identity. Lock it down.
        assert_eq!(super::SignalMeterMode::Full.id_suffix(), "full");
        assert_eq!(super::SignalMeterMode::ScreenOnly.id_suffix(), "screen");
        assert_eq!(super::SignalMeterMode::NoScreen.id_suffix(), "peer");
    }

    #[test]
    fn meter_mode_default_is_full() {
        // Default is `Full` so legacy callers (e.g. the diagnostics
        // popup) that don't supply a mode keep showing every series.
        assert_eq!(
            super::SignalMeterMode::default(),
            super::SignalMeterMode::Full
        );
    }

    // -----------------------------------------------------------------
    // HCL bug #9: clamp_free_position keeps a dragged popup inside the
    // viewport. Sweep every clamp branch (no clamp, both axes clamped,
    // tiny viewport that flips min/max math).
    // -----------------------------------------------------------------

    #[test]
    fn clamp_free_inside_viewport_is_noop() {
        // Popup that already fits inside the viewport with breathing room
        // should pass through unchanged.
        let (l, t) = super::clamp_free_position(100.0, 100.0, 420.0, 400.0, 1920.0, 1080.0);
        assert!((l - 100.0).abs() < 0.01);
        assert!((t - 100.0).abs() < 0.01);
    }

    #[test]
    fn clamp_free_left_overflow_clamps_to_margin() {
        // Negative target left -> clamp to VIEWPORT_MARGIN_PX.
        let (l, _) = super::clamp_free_position(-50.0, 100.0, 420.0, 400.0, 1920.0, 1080.0);
        assert!((l - super::VIEWPORT_MARGIN_PX).abs() < 0.01, "got {l}");
    }

    #[test]
    fn clamp_free_right_overflow_clamps_to_max_left() {
        // Target left that would push the popup off the right edge -> clamp
        // to (viewport_w - popup_w - margin).
        let viewport_w = 1920.0;
        let popup_w = 420.0;
        let (l, _) = super::clamp_free_position(2000.0, 100.0, popup_w, 400.0, viewport_w, 1080.0);
        let expected = viewport_w - popup_w - super::VIEWPORT_MARGIN_PX;
        assert!((l - expected).abs() < 0.01, "got {l}, expected {expected}");
    }

    #[test]
    fn clamp_free_top_overflow_clamps_to_margin() {
        // Negative target top -> clamp to VIEWPORT_MARGIN_PX.
        let (_, t) = super::clamp_free_position(100.0, -50.0, 420.0, 400.0, 1920.0, 1080.0);
        assert!((t - super::VIEWPORT_MARGIN_PX).abs() < 0.01, "got {t}");
    }

    #[test]
    fn clamp_free_bottom_overflow_clamps_to_max_top() {
        // Target top that would push the popup off the bottom edge ->
        // clamp to (viewport_h - popup_h - margin).
        let viewport_h = 1080.0;
        let popup_h = 400.0;
        let (_, t) = super::clamp_free_position(100.0, 2000.0, 420.0, popup_h, 1920.0, viewport_h);
        let expected = viewport_h - popup_h - super::VIEWPORT_MARGIN_PX;
        assert!((t - expected).abs() < 0.01, "got {t}, expected {expected}");
    }

    #[test]
    fn clamp_free_handles_oversized_popup() {
        // Popup wider than the viewport: max_left math goes negative and
        // the `.max()` keeps the result at VIEWPORT_MARGIN_PX. The popup
        // should sit at the left margin, not at a negative coordinate.
        let (l, _) = super::clamp_free_position(500.0, 100.0, 1000.0, 400.0, 600.0, 1080.0);
        assert!((l - super::VIEWPORT_MARGIN_PX).abs() < 0.01, "got {l}");
    }

    // -----------------------------------------------------------------
    // HCL bug #9: SignalPopupState defaults to Anchored. The popup body
    // reads this to decide whether to render the 📌 reanchor button —
    // legacy callers that don't bother to insert a state value get the
    // expected anchored behaviour.
    // -----------------------------------------------------------------

    #[test]
    fn signal_popup_state_default_is_anchored() {
        let s = super::SignalPopupState::default();
        assert_eq!(s.position, super::SignalPopupPosition::Anchored);
        assert!(!s.position.is_free());
    }

    #[test]
    fn signal_popup_position_free_is_free() {
        let p = super::SignalPopupPosition::Free {
            left: 100.0,
            top: 200.0,
        };
        assert!(p.is_free());
    }

    // -----------------------------------------------------------------
    // HCL iter7: `css_popup_border_box_width` mirrors the popup's
    // `.signal-quality-popup` CSS rules. The math callers in
    // `reposition_popup` and `snap_popup_back_to_anchor` pass its return
    // value as `popup_w` to `compute_popup_position`. These tests pin
    // the CSS-equivalent so any future style.css refactor that changes
    // the popup's natural width, padding, or border without updating
    // these constants gets caught at unit-test time rather than as a
    // flaky e2e snap-back delta.
    // -----------------------------------------------------------------

    #[test]
    fn css_popup_border_box_width_at_typical_viewport() {
        // Standard Playwright Desktop Chrome viewport is 1280x720. The
        // popup's natural content width is 420px; padding adds 32px and
        // border adds 2px, so the border-box width is 454px.
        let w = super::css_popup_border_box_width(1280.0);
        assert!((w - 454.0).abs() < 0.01, "got {w}, expected 454.0");
    }

    #[test]
    fn css_popup_border_box_width_shrinks_below_min_viewport() {
        // Viewport narrower than `420 + 16 = 436px` hits the `100vw - 16`
        // arm of the `min()` and shrinks the popup. At vw=300, content =
        // 300 - 16 = 284; border-box = 284 + 32 + 2 = 318.
        let w = super::css_popup_border_box_width(300.0);
        assert!((w - 318.0).abs() < 0.01, "got {w}, expected 318.0");
    }

    #[test]
    fn css_popup_border_box_width_clamps_negative_content_to_zero() {
        // Pathologically narrow viewport (< 16px) would produce a
        // negative content width via `vw - 16`. The helper clamps the
        // content width to zero before adding padding + border so the
        // result is at least `padding + border = 34` and never negative.
        let w = super::css_popup_border_box_width(8.0);
        assert!((w - 34.0).abs() < 0.01, "got {w}, expected 34.0");
        let w_zero = super::css_popup_border_box_width(0.0);
        assert!((w_zero - 34.0).abs() < 0.01, "got {w_zero}, expected 34.0");
    }

    #[test]
    fn css_popup_border_box_width_uses_min_arm_above_threshold() {
        // At viewport exactly 436px, both arms of the `min()` evaluate
        // to 420 (since `436 - 16 = 420`). Above 436px we stay locked
        // at 420 content -> 454 border-box regardless of how wide the
        // viewport gets, matching `.signal-quality-popup { width: min(
        // 420px, calc(100vw - 16px)); }`.
        let w_436 = super::css_popup_border_box_width(436.0);
        assert!((w_436 - 454.0).abs() < 0.01, "got {w_436}");
        let w_2000 = super::css_popup_border_box_width(2000.0);
        assert!((w_2000 - 454.0).abs() < 0.01, "got {w_2000}");
        let w_4000 = super::css_popup_border_box_width(4000.0);
        assert!((w_4000 - 454.0).abs() < 0.01, "got {w_4000}");
    }
}
