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

use crate::adaptive_quality_constants::{
    AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS, AUDIO_CONGESTION_RECOVERY_TICK_MS,
    AUDIO_FEC_RECONFIG_TICK_MS, AUDIO_QUALITY_TIERS, AUDIO_REDUNDANCY_ENABLED, AUDIO_RED_FORMAT,
    VAD_POLL_INTERVAL_MS,
};
use crate::audio_constants::{
    rms_to_intensity, AUDIO_LEVEL_DELTA_THRESHOLD, DEFAULT_VAD_THRESHOLD, VAD_FFT_SIZE,
    VAD_SMOOTHING_TIME_CONSTANT,
};
use crate::audio_worklet_codec::EncoderInitOptions;
use crate::audio_worklet_codec::{AudioWorkletCodec, CodecMessages};
use crate::connection::MediaStreamKey;
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::crypto::aes::Aes128State;
use crate::encode::encoder_state::EncoderState;
use crate::wrappers::EncodedAudioChunkTypeWrapper;
use crate::VideoCallClient;
use gloo::timers::callback::Interval;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::Uint8Array;
use protobuf::Message;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::{
    media_packet::{media_packet::MediaType, AudioMetadata, MediaPacket},
    packet_wrapper::packet_wrapper::{MediaKind, PacketType},
};
use videocall_types::Callback;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::AudioContext;
use web_sys::EncodedAudioChunkType;
use web_sys::MediaStream;
use web_sys::MediaStreamConstraints;
use web_sys::MediaStreamTrack;
use web_sys::MessageEvent;
use web_time::SystemTime;

/// Per-layer AUDIO simulcast bitrates in kbps, **lowest layer first** (index ==
/// `simulcast_layer_id`). Audio simulcast is intentionally a shallow ladder
/// (issue #989, Phase 3c → 3 rungs in #1082) because audio is ~1-3% of call
/// bandwidth, so a deep ladder is not worth the per-layer Opus encode cost.
///
/// - layer 0 = LOW (12 kbps) — the base the relay always forwards and a
///   congested receiver pulls. Matches the AQ "low" tier (issue #1768).
/// - layer 1 = MID (24 kbps) — an intermediate rung for moderate downlinks.
/// - layer 2 = HIGH (48 kbps) — the upgrade layer a receiver with headroom
///   selects. Matches the AQ "high" tier.
///
/// This slice is the **single source of truth** for the publisher-side audio
/// ladder; its length defines the maximum supported audio layer count and is
/// kept in lockstep with the receiver-side `AUDIO_LAYER_KBPS` table by the
/// compile-time assert below (issue #1077). Retuned lighter in issue #1768.
const AUDIO_SIMULCAST_LAYER_KBPS: &[u32] = &[12, 24, 48];

/// Upper bound on AUDIO simulcast layers — the ladder length (issue #1082).
const AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS: u32 = AUDIO_SIMULCAST_LAYER_KBPS.len() as u32;

// Compile-time tie between the publisher ladder and the receiver-side
// `AUDIO_LAYER_KBPS` snapshot table so the two cannot silently diverge
// (issue #1077): if either table changes length, this assert fails to compile.
const _: () = assert!(
    AUDIO_SIMULCAST_LAYER_KBPS.len() == crate::decode::layer_chooser::audio_layer_kbps_len(),
    "publisher AUDIO_SIMULCAST_LAYER_KBPS and receiver AUDIO_LAYER_KBPS must have the same length"
);

/// Clamp a requested audio `max_layers` to `[1, AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS]`.
/// `0`/`1` → single layer (feature off, byte-identical mic path). Free function
/// so it is unit-testable without constructing a `MicrophoneEncoder`.
fn clamp_audio_layer_count(max_layers: u32) -> u32 {
    max_layers.clamp(1, AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS)
}

/// Decide whether a given audio `layer_id` should be PUBLISHED under BOTH the
/// user's SEND layer-ceiling (the perf-panel "layers published" control) AND the
/// congestion-driven layer ceiling (issue #621).
///
/// `user_ceiling_atomic` and `congestion_ceiling_atomic` are the raw
/// shared-atomic values (`u32::MAX` = Auto / no cap). Each is mapped to a layer
/// COUNT via the shared [`camera_encoder::layer_ceiling_to_count`]
/// (sentinel-safe), and the EFFECTIVE count is `min` of the two, FLOORED at 1, so
/// the base layer (`layer_id == 0`) is ALWAYS published regardless of either
/// ceiling — mirroring the video/screen base-present invariant. A layer is
/// published iff `layer_id < effective_count`. Pure free function so the gate is
/// host-testable without a `MicrophoneEncoder` / AudioWorklet.
///
/// The two ceilings are SEPARATE levers with different owners and lifecycles:
///   * the USER ceiling is the explicit perf-panel choice (persists across
///     reconnect; only the user changes it);
///   * the CONGESTION ceiling is driven DOWN to base-only on a self-targeted
///     server CONGESTION signal and climbs back automatically after a cooldown
///     (see [`audio_congestion_recover`] and [`AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS`](crate::adaptive_quality_constants::AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS)).
///
/// Composing them with `min` means whichever is tighter wins, and at the default
/// (both `u32::MAX` / Auto) the full ladder publishes — byte-identical to the
/// pre-#621 behaviour.
///
/// NOTE (#1201 — partially superseded by #621): the user SEND ceiling used to be
/// the ONLY runtime gate on audio rungs; #621 adds the congestion ceiling as a
/// SECOND runtime gate driven by the server CONGESTION signal. Audio still has no
/// shed from encoder backpressure or a relay `LAYER_HINT` (the client ignores
/// AUDIO hints, and the relay stopped computing the AUDIO union under #1118 N3 /
/// PR #1330). At the default (Auto) ceilings the full 12/24/48 kbps ladder
/// (~84 kbps, issue #1768) publishes unconditionally — a deliberate, documented
/// cost (audio is ~1-3% of call bandwidth).
fn audio_layer_is_published(
    layer_id: u32,
    user_ceiling_atomic: u32,
    congestion_ceiling_atomic: u32,
) -> bool {
    let user_count = crate::encode::camera_encoder::layer_ceiling_to_count(user_ceiling_atomic);
    let congestion_count =
        crate::encode::camera_encoder::layer_ceiling_to_count(congestion_ceiling_atomic);
    let effective_count = user_count.min(congestion_count).max(1);
    (layer_id as usize) < effective_count
}

/// Pure recovery state machine for the AUDIO congestion layer ceiling (issue
/// #621). Decides the NEXT congestion-ceiling layer COUNT given the current
/// count, the wall-clock time, and when the last congestion cut fired.
///
/// Inputs:
///   * `current_count`: the congestion ceiling expressed as a layer COUNT
///     (`u32::MAX` is the Auto / fail-open sentinel = "no congestion cap"; any
///     real value is the post-cut count, floored at 1 = base-only after a cut).
///   * `configured_layers`: the publisher's configured audio layer count (the
///     clamped `max_layers`); recovery never climbs past this — re-adding a layer
///     that was never built is meaningless.
///   * `now_ms` / `last_congestion_ms`: wall-clock now and the timestamp of the
///     last cut. `last_congestion_ms == None` means "no cut is active" — already
///     fully recovered — so this returns the fail-open sentinel unchanged.
///   * `cooldown_ms`: the per-rung cooldown
///     ([`AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS`](crate::adaptive_quality_constants::AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS)).
///
/// Behaviour: once `now_ms - last_congestion_ms >= cooldown_ms` AND the ceiling
/// is below `configured_layers`, climb by exactly ONE rung. Returns
/// `(next_count, fully_recovered)`; `fully_recovered == true` once the ceiling
/// has climbed back to (or already was at) `configured_layers`, signalling the
/// caller to drop back to the fail-open sentinel and stop ticking. ONE rung per
/// cooldown gives the hysteresis the issue requires — a flapping link cannot
/// thrash the ladder because every climb costs a full cooldown of stability.
///
/// Pure (no clock, no atomics) so it is host-testable without a browser, mirror
/// of how `evaluate_self_congestion` is host-tested in `videocall-aq`.
fn audio_congestion_recover(
    current_count: u32,
    configured_layers: u32,
    now_ms: f64,
    last_congestion_ms: Option<f64>,
    cooldown_ms: f64,
) -> (u32, bool) {
    let configured = configured_layers.max(1);
    // No active cut → fail-open, fully recovered (nothing to climb).
    let Some(cut_ms) = last_congestion_ms else {
        return (u32::MAX, true);
    };
    // Treat the sentinel as already-at-configured (defensive: a fail-open value
    // with a stale timestamp must not be read as "0 rungs").
    let current = if current_count == u32::MAX {
        configured
    } else {
        current_count.clamp(1, configured)
    };
    if current >= configured {
        // Already at the top — collapse to the fail-open sentinel and stop.
        return (u32::MAX, true);
    }
    if now_ms - cut_ms >= cooldown_ms {
        let next = current + 1;
        // Climbing the final rung returns to full → fail-open sentinel + done.
        if next >= configured {
            (u32::MAX, true)
        } else {
            (next, false)
        }
    } else {
        // Still in cooldown — hold.
        (current, false)
    }
}

/// One tick of the AUDIO congestion-recovery state machine (issue #621). Pure so
/// the FULL per-tick behaviour — not just the single-rung climb math in
/// [`audio_congestion_recover`] — is host-testable without a browser or
/// `Interval`. The recovery `Interval` in [`MicrophoneEncoder::start`] is a thin
/// wrapper that only reads the clock + atom and writes back what this returns.
///
/// State carried by the caller between ticks:
///   * `current`: the congestion ceiling atom's value right now (`u32::MAX` =
///     fail-open / no cap; a real value = post-cut layer COUNT).
///   * `last_seen`: the ceiling value THIS loop last left the atom at — used to
///     detect a NEW cut (the client storing a lower value out-of-band).
///   * `last_congestion_ms`: when the current cooldown window started (`None` =
///     no active cut).
///
/// Returns the new `(ceiling, last_seen, last_congestion_ms)` triple. Behaviour:
///   * **New-cut detection** — if `current < last_seen` the client just cut the
///     ceiling (e.g. `u32::MAX → 1`), so (re)start the cooldown from `now_ms`.
///   * **One rung per cooldown** — delegate the climb to
///     [`audio_congestion_recover`]; crucially, when it climbs an INTERMEDIATE
///     rung the cooldown anchor is reset to `now_ms` so the NEXT rung waits a
///     full cooldown too. Without this reset every rung after the first would
///     climb on consecutive ticks, collapsing the hysteresis the issue requires.
///   * **Full recovery** — once back at the fail-open sentinel, clear the cut
///     memory so the next decrease is seen as a fresh cut.
fn audio_congestion_tick(
    current: u32,
    last_seen: u32,
    configured_layers: u32,
    now_ms: f64,
    last_congestion_ms: Option<f64>,
    cooldown_ms: f64,
) -> (u32, u32, Option<f64>) {
    // New-cut detection: a decrease vs what we last left means the client cut
    // the ceiling out-of-band; restart the cooldown from now.
    let cut = if current < last_seen {
        Some(now_ms)
    } else {
        last_congestion_ms
    };
    let (next, fully_recovered) =
        audio_congestion_recover(current, configured_layers, now_ms, cut, cooldown_ms);
    let next_cut = if fully_recovered {
        // Back at the fail-open sentinel — forget the cut so the next decrease
        // is detected fresh.
        None
    } else if next > current {
        // Intermediate climb — restart the per-rung cooldown so rungs are spaced
        // a full cooldown apart (true one-rung-per-cooldown hysteresis).
        Some(now_ms)
    } else {
        cut
    };
    // We leave the atom at `next`, so that is what the next tick must compare
    // against for new-cut detection (a climb we performed is not a new cut).
    (next, next, next_cut)
}

/// Change-detection + debounce for the live Opus FEC ctl-reconfig (issue #1567).
///
/// Given the audio tier's CURRENT `(enable_fec, packet_loss_perc)` and the
/// `(fec, loss%)` we LAST sent to the worklet, returns `Some(current)` only when
/// it differs (a real transition that must be re-applied to the live encoder),
/// and `None` when unchanged (suppress — do not spam the worklet).
///
/// `last_sent == None` means "nothing applied beyond the encoder's INIT state".
/// We treat the init state as the healthy top tier (`AUDIO_QUALITY_TIERS[0]` =
/// FEC off, 0% loss) — the only tier the mic ever inits at (see
/// [`MicrophoneEncoder::start`]). So a first observation that already equals
/// `(false, 0)` is correctly suppressed (no redundant reconfig at startup while
/// healthy), and the FIRST drop to a FEC tier returns `Some`, engaging FEC.
///
/// Pure (no clock, no atomics, no `Interval`) so the "only send on change"
/// debounce — the mutation-meaningful core of the fix — is host-testable without
/// a browser, mirroring how [`audio_congestion_tick`] is tested.
///
/// The CADENCE/rate-limit is supplied by the caller: the reconfig `Interval`
/// ticks at [`AUDIO_FEC_RECONFIG_TICK_MS`] (1 Hz), so this helper can emit at
/// most one reconfig per second, and only on a genuine FEC/loss transition. A
/// tier that flaps but re-evaluates to the same `(fec, loss%)` between ticks is
/// coalesced to a single (or zero) reconfig.
///
/// As of #1398 the PRODUCTION reconfig timer uses [`audio_reconfig_change`] for
/// BOTH modes (multi-layer pins the bitrate component to `None`, which reduces
/// exactly to the `(fec, loss%)`-only behaviour). This `(fec, loss%)`-only
/// helper is retained as a `#[cfg(test)]` wrapper that delegates to
/// [`audio_reconfig_change`] (bitrate pinned to `None`), so the pre-#1398 FEC
/// change-detection tests continue to pin the canonical `(fec, loss%)` contract
/// AND prove the multi-layer path is the bitrate-free projection of the unified
/// detector — the two cannot diverge. It is test-only because production never
/// calls the `(fec, loss%)`-only form directly anymore.
#[cfg(test)]
fn audio_fec_reconfig_change(
    current: (bool, u32),
    last_sent: Option<(bool, u32)>,
) -> Option<(bool, u32)> {
    // Lift to the bitrate-aware key with the bitrate component fixed to None
    // (multi-layer / FEC-only), then project the (fec, loss%) result back out.
    let lifted = audio_reconfig_change(
        (current.0, current.1, None),
        last_sent.map(|(f, l)| (f, l, None)),
        None,
    );
    lifted.map(|(f, l, _)| (f, l))
}

/// The change-detection key for the live Opus reconfig (issue #1398):
/// `(enable_fec, packet_loss_perc, Option<bitrate_bps>)`. The bitrate component
/// is `None` in multi-layer mode (FEC-only, byte-identical to pre-#1398) and
/// `Some(effective_bps)` in single-layer mode. Aliased so the timer's debounce
/// `Cell` and the pure detector share one (non-`type_complexity`-tripping) name.
type AudioReconfigKey = (bool, u32, Option<u32>);

/// Change-detection + debounce for the live Opus reconfig INCLUDING the
/// single-layer bitrate (issue #1398). The bitrate-aware superset of
/// [`audio_fec_reconfig_change`]: the key is `(enable_fec, packet_loss_perc,
/// Option<bitrate_bps>)`, where the bitrate component is:
///   * `None` in MULTI-LAYER mode — the layer-ceiling lever (#621) handles audio
///     congestion there, so bitrate is never part of the reconfig (preserving
///     the exact pre-#1398 behaviour: the emitted `ReconfigOpus` carries
///     `bit_rate: None`). With the bitrate component pinned to `None`, this
///     function reduces EXACTLY to the `(fec, loss%)` change-detection.
///   * `Some(effective_bps)` in SINGLE-LAYER mode — the camera-state-aware
///     effective bitrate ([`effective_audio_bitrate`]); a change in EITHER the
///     tier `(fec, loss%)` OR the effective bitrate re-applies.
///
/// `last_sent == None` is the encoder's INIT state. The init `(fec, loss%)` is
/// the top tier `(false, 0)`; the init bitrate is whatever the caller passes as
/// `init_bitrate` (`None` multi-layer, or `Some(top-tier bps)` single-layer),
/// so a first observation that already matches init is suppressed (no startup
/// spam while healthy), and the first real transition emits.
///
/// Pure (no clock, no atomics) so the "only send on change" debounce — now over
/// the (fec, loss%, bitrate) tuple — is host-testable.
fn audio_reconfig_change(
    current: AudioReconfigKey,
    last_sent: Option<AudioReconfigKey>,
    init_bitrate: Option<u32>,
) -> Option<AudioReconfigKey> {
    // Init state: top tier FEC/loss, and the caller-supplied init bitrate.
    let baseline = last_sent.unwrap_or((false, 0, init_bitrate));
    if current == baseline {
        None
    } else {
        Some(current)
    }
}

/// Per-axis window snapshot for [`audio_uplink_step_down_decision`]. The three
/// transport-distress axes (WT slow-`ready()` saturation, WS send-buffer
/// backpressure, and WT unistream DROP) each carry an INDEPENDENT tumbling
/// window — different widths (`AUDIO_UPLINK_SATURATION_WINDOW_MS` vs
/// `AUDIO_UPLINK_WS_WINDOW_MS` vs `AUDIO_UPLINK_WT_DROP_WINDOW_MS`) and different
/// counters — so each gets its own snapshot/elapsed pair.
#[derive(Debug, Clone, Copy)]
struct AudioUplinkAxisInput {
    /// Cumulative counter reading right now (monotonic `AtomicU64`).
    current: u64,
    /// The counter value captured when this axis's current window opened.
    snapshot: u64,
    /// How long this axis's window has been open (ms).
    elapsed_ms: f64,
}

/// Outcome of one [`audio_uplink_step_down_decision`] tick. `step_down` fires if
/// ANY axis crossed its threshold within its (closed) window; `roll_*` /
/// `new_*_snapshot` are PER AXIS because the three windows are independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioUplinkDecision {
    /// Step the single-layer audio bitrate floor DOWN one tier this tick.
    step_down: bool,
    /// The WT-saturation axis window closed → caller adopts `new_sat_snapshot`
    /// and resets that axis's window start.
    roll_sat: bool,
    new_sat_snapshot: u64,
    /// The WS-backpressure axis window closed → caller adopts `new_ws_snapshot`
    /// and resets that axis's window start.
    roll_ws: bool,
    new_ws_snapshot: u64,
    /// The WT-DROP axis window closed → caller adopts `new_wtdrop_snapshot` and
    /// resets that axis's window start (issue #1398: third OR axis, the audio
    /// analogue of the camera AQ's `wt_drop_step_down_decision`).
    roll_wtdrop: bool,
    new_wtdrop_snapshot: u64,
}

/// Pure decision helper for the MIC-SIDE single-layer audio uplink-distress
/// detector (issue #1398) — the audio analogue of the camera's
/// `wt_saturation_step_down_decision` / `wt_drop_step_down_decision`, folding
/// THREE transport-distress axes into one decision so a single mic-side tick
/// covers whichever transport is live and whichever distress mode it is in:
///   1. the WebTransport slow-`ready()` SATURATION axis (a slow-but-alive uplink
///      bandwidth cliff — `unistream_ready_stall_count`);
///   2. the WebSocket send-buffer BACKPRESSURE axis (`websocket_drop_count`);
///   3. the WebTransport unistream DROP axis (hard stream-reset/write-failure —
///      `unistream_drop_count`). This third axis mirrors the camera AQ's
///      `wt_drop_step_down_decision`: the camera sheds VIDEO on this counter, so
///      a single-layer audio publisher must shed AUDIO on it too.
///
/// Each axis is evaluated by the SAME tumbling-window delta test the camera uses
/// ([`evaluate_self_congestion`]), but with the AUDIO constants
/// ([`AUDIO_UPLINK_SATURATION_STALL_THRESHOLD`](crate::adaptive_quality_constants::AUDIO_UPLINK_SATURATION_STALL_THRESHOLD) /
/// [`AUDIO_UPLINK_SATURATION_WINDOW_MS`](crate::adaptive_quality_constants::AUDIO_UPLINK_SATURATION_WINDOW_MS),
/// [`AUDIO_UPLINK_WS_DROP_THRESHOLD`](crate::adaptive_quality_constants::AUDIO_UPLINK_WS_DROP_THRESHOLD) /
/// [`AUDIO_UPLINK_WS_WINDOW_MS`](crate::adaptive_quality_constants::AUDIO_UPLINK_WS_WINDOW_MS), and
/// [`AUDIO_UPLINK_WT_DROP_THRESHOLD`](crate::adaptive_quality_constants::AUDIO_UPLINK_WT_DROP_THRESHOLD) /
/// [`AUDIO_UPLINK_WT_DROP_WINDOW_MS`](crate::adaptive_quality_constants::AUDIO_UPLINK_WT_DROP_WINDOW_MS)),
/// which are deliberately WIDER/HIGHER than the video constants so audio sheds
/// AFTER video (the wider window does the real work — see the constants module).
///
/// `step_down` is the OR of the three axes (any of sustained WT saturation, WS
/// backpressure, OR WT drop trips the audio downshift); `roll_*` /
/// `new_*_snapshot` are returned per axis because the windows are independent.
/// On a transport that is not in use the relevant counter stays flat, so that
/// axis is a true no-op (WS users hold both WT counters flat; WT users hold the
/// WS-drop counter flat).
///
/// Pure (no clock, no atomics) so the encoder's CHOICE OF SIGNAL — the three
/// transport counters wired through `evaluate_self_congestion` with the AUDIO
/// constants — is pinned by a NATIVE `#[test]` (the recovery `Interval` it lives
/// in depends on `js_sys::Date::now()` and cannot run on host). A mutation that
/// fed the VIDEO constants, swapped the axes' counters, or inverted the
/// comparison changes the returned decision and fails the test.
fn audio_uplink_step_down_decision(
    saturation: AudioUplinkAxisInput,
    ws: AudioUplinkAxisInput,
    wtdrop: AudioUplinkAxisInput,
) -> AudioUplinkDecision {
    use crate::adaptive_quality_constants::{
        evaluate_self_congestion, AUDIO_UPLINK_SATURATION_STALL_THRESHOLD,
        AUDIO_UPLINK_SATURATION_WINDOW_MS, AUDIO_UPLINK_WS_DROP_THRESHOLD,
        AUDIO_UPLINK_WS_WINDOW_MS, AUDIO_UPLINK_WT_DROP_THRESHOLD, AUDIO_UPLINK_WT_DROP_WINDOW_MS,
    };
    let sat = evaluate_self_congestion(
        saturation.current,
        saturation.snapshot,
        saturation.elapsed_ms,
        AUDIO_UPLINK_SATURATION_WINDOW_MS,
        AUDIO_UPLINK_SATURATION_STALL_THRESHOLD,
    );
    let wsd = evaluate_self_congestion(
        ws.current,
        ws.snapshot,
        ws.elapsed_ms,
        AUDIO_UPLINK_WS_WINDOW_MS,
        AUDIO_UPLINK_WS_DROP_THRESHOLD,
    );
    let wtd = evaluate_self_congestion(
        wtdrop.current,
        wtdrop.snapshot,
        wtdrop.elapsed_ms,
        AUDIO_UPLINK_WT_DROP_WINDOW_MS,
        AUDIO_UPLINK_WT_DROP_THRESHOLD,
    );
    AudioUplinkDecision {
        step_down: sat.step_down || wsd.step_down || wtd.step_down,
        roll_sat: sat.roll_window,
        new_sat_snapshot: sat.new_snapshot,
        roll_ws: wsd.roll_window,
        new_ws_snapshot: wsd.new_snapshot,
        roll_wtdrop: wtd.roll_window,
        new_wtdrop_snapshot: wtd.new_snapshot,
    }
}

/// Pure GATE for the mic uplink-distress detector (issue #1398 + #1611 backstop):
/// the detector evaluates a tick ONLY when audio is single-layer AND every active
/// video stream is either OFF or EXHAUSTED:
///
/// ```text
/// single_layer
///   && (!camera_active || camera_video_exhausted)
///   && (!screen_active || screen_video_exhausted)
/// ```
///
/// Why this 3-signal form (issue #1611). The original gate (`!camera_active`)
/// silenced the detector whenever the camera was on, because the camera AQ's
/// audio-tier is CPU/encoder-queue gated (controller.rs:603-624) — it steps
/// VIDEO on uplink distress, not audio. But once video reaches its floor (tier
/// at the user-capped cap AND active layers at 1), video CAN'T shed further and
/// audio is the only remaining axis. The backstop extends the gate to permit
/// audio downshift in that case.
///
/// The screen-active term uses the same pattern: screen writes no audio-tier
/// atom, so a screen-sharing publisher relies on this detector for its only
/// audio downshift, but the gate must not open while screen video can still
/// shed (screen has priority).
///
/// In multi-layer AUDIO mode the layer-ceiling lever (#621) handles congestion
/// and the FEC timer ignores the bitrate floor, so the detector never evaluates
/// regardless of video state. Pure so the gate is host-testable; the closure
/// passes the live atomic loads in.
///
/// NOTE on per-stream attribution (Tony's #1615 review comment): the 3 uplink
/// counters (unistream_ready_stall_count, unistream_drop_count,
/// websocket_drop_count) are PROCESS-GLOBAL with NO per-stream attribution.
/// When the screen is active and driving distress, this gate may fire an audio
/// downshift. This is BOUNDED (audio floors at 8 kbps and never stops) and
/// accepted as a known limitation. The root fix is per-stream counter
/// attribution at the transport level — out of scope for this issue.
fn audio_detector_gate_open(
    single_layer: bool,
    camera_active: bool,
    camera_video_exhausted: bool,
    screen_active: bool,
    screen_video_exhausted: bool,
) -> bool {
    single_layer
        && (!camera_active || camera_video_exhausted)
        && (!screen_active || screen_video_exhausted)
}

/// Pure RE-SEED decision for the mic uplink-distress detector window (issue #1398,
/// FIX 1 + reconnect-reseed P1). `should_evaluate` = the detector's gate
/// ([`audio_detector_gate_open`]) is OPEN this tick. `was_active` = the detector
/// evaluated on the PREVIOUS tick. `force_reseed` = a connection RECONNECT
/// occurred since the last evaluation (the client sets this in the `Connected`
/// handler; the closure consumes it). Returns true iff this tick must RE-SEED the
/// tumbling-window snapshots to `now` and SKIP the step-down decision.
///
/// TWO independent re-seed triggers, OR'd:
///   * `!was_active` — the detector is (RE)ACTIVATING after having been GATED
///     (camera on, multi-layer) or after an early-return (mic muted, connection
///     switching). While inactive the windows are NOT rolled.
///   * `force_reseed` — a network RECONNECT happened while the detector stayed
///     ACTIVE (camera OFF + single-layer throughout, so the gate never closed and
///     `was_active` stayed `true`). The transport teardown/rebuild BUMPS the
///     monotonic `unistream_*` / `websocket_drop` counters, so without this the
///     first closed window on the fresh session would compute `current -
///     stale_snapshot` across the reconnect and cash a SPURIOUS cut that has
///     nothing to do with the new session's uplink. The `!was_active` trigger
///     alone MISSES this because the mic is never restarted on a plain reconnect
///     (mic stays enabled, `switching` stays false) — the detector keeps running
///     with `was_active == true`. This is the core camera-off path.
///
/// In BOTH cases the global counters advanced while the detector was not measuring
/// from a fresh anchor, so the first post-event delta over a too-long/stale
/// `elapsed` would otherwise fire a SPURIOUS immediate cut. Re-seeding re-anchors
/// all windows to "now" so distress is measured from now forward, never across the
/// gap/reconnect. Pure so it is host-testable; the closure threads the gate result,
/// a `Cell<bool>` of the previous state, and the consumed reconnect flag in.
fn audio_detector_should_reseed(
    should_evaluate: bool,
    was_active: bool,
    force_reseed: bool,
) -> bool {
    should_evaluate && (!was_active || force_reseed)
}

// ===========================================================================
// Single-layer congestion BITRATE floor (issue #1398)
// ===========================================================================
//
// A SINGLE-LAYER audio publisher (device capability-gated to 1 audio layer, or
// audio simulcast disabled) has NO upper simulcast layer to shed, so the
// layer-ceiling lever (#621) is a no-op for it: layer 0 always publishes. The
// only way such a publisher can downshift audio under publisher-uplink distress
// is to lower the bitrate of the ONE running Opus stream LIVE (via the #1578
// `reconfigOpus` worklet command, now extended with ctl 4002 = OPUS_SET_BITRATE).
// The DOWN trigger is the mic-side uplink-distress detector
// (`audio_uplink_step_down_decision`, below) — NOT the b127ee80 self-targeted
// `PacketType::CONGESTION` server packet, which #1219 Half 1 removed server-side
// so it never fired in production. These pure functions are the state machine
// for that bitrate floor; they mirror the existing layer-ceiling congestion
// machine (`audio_congestion_recover` / `audio_congestion_tick`) one-for-one so
// the two levers behave identically (one rung per cooldown, hysteresis,
// fail-open).
//
// All tier bitrates are derived from `AUDIO_QUALITY_TIERS` (the SINGLE source of
// truth) — never hardcode 48/24/12/8 here. The tier table is ordered HIGHEST
// first (index 0 = 48 kbps top, ascending index = lower bitrate), so a "step
// down" is a step to a HIGHER index, and a recovery "climb" is a step to a
// LOWER index.

/// Map an `AUDIO_QUALITY_TIERS` index to its bitrate in BPS (kbps × 1000).
/// Clamps a stale/out-of-range index to the last (lowest/emergency) tier so it
/// can never panic. Pure; the tier table is the single source of truth.
fn audio_tier_bps_for_index(idx: usize) -> u32 {
    let clamped = idx.min(AUDIO_QUALITY_TIERS.len() - 1);
    AUDIO_QUALITY_TIERS[clamped].bitrate_kbps * 1000
}

/// The `AUDIO_QUALITY_TIERS` index whose bitrate (bps) equals `floor_bps`, if
/// any. Returns `None` when `floor_bps` matches no tier (e.g. the `u32::MAX`
/// fail-open sentinel, or an arbitrary off-ladder value) — callers treat that
/// as "not on the ladder / no cut". Pure; derived entirely from the table.
fn audio_tier_index_for_bps(floor_bps: u32) -> Option<usize> {
    AUDIO_QUALITY_TIERS
        .iter()
        .position(|t| t.bitrate_kbps * 1000 == floor_bps)
}

/// Pure DOWN-step for the single-layer congestion BITRATE floor (issue #1398).
/// Returns the NEXT floor (bps) given the CURRENT floor, stepping DOWN exactly
/// ONE tier per call — mirroring the multi-layer one-rung-per-cooldown ladder
/// (`audio_congestion_recover`), NOT a jump straight to emergency.
///
/// Ladder (from `AUDIO_QUALITY_TIERS`, ×1000): index0=48, 1=24, 2=12, 3=8 kbps
/// (issue #1768).
///
/// Behaviour:
///   * From the `u32::MAX` fail-open sentinel ("no congestion cut") the FIRST
///     cut lands on tier index 1 (24 kbps), NOT index 0 (48 kbps). RATIONALE:
///     the TOP tier (index 0, 48 kbps) IS the healthy / no-cut bitrate, so the
///     first DOWN step must be a REAL reduction — landing on index 0 would be a
///     no-op cut. This is the bitrate analogue of the layer ceiling's first cut
///     going from "no cap" straight to base-only.
///   * From a tier already on the ladder, step to the next-lower tier (24→12,
///     12→8).
///   * At the LOWEST/emergency tier (index 3 = 8 kbps) it CLAMPS — 8 stays at
///     8; a single transient blip cannot gut audio below the emergency floor,
///     and repeated signals simply hold there.
///   * A floor that is not on the ladder (an unusual off-ladder value) is
///     treated as "between tiers": step to the first tier strictly below it.
///     Defensive only; the detector only ever stores tier-aligned values here.
///
/// Pure (no clock, no atomics) so it is host-testable and is the SINGLE source
/// of truth for the step ladder, shared by the mic-side uplink-distress detector
/// (the DOWN trigger) and the recovery tick's inverse climb — see #1398.
fn audio_congestion_bitrate_step_down(current_floor_bps: u32) -> u32 {
    let top_idx = 0usize;
    let bottom_idx = AUDIO_QUALITY_TIERS.len() - 1;
    // No cut yet (fail-open sentinel): the first DOWN step must be a real
    // reduction, so go to index 1 (one tier below the healthy top), NOT index 0.
    if current_floor_bps == u32::MAX {
        return audio_tier_bps_for_index(top_idx + 1);
    }
    match audio_tier_index_for_bps(current_floor_bps) {
        Some(idx) => {
            // On the ladder: step to the next-lower tier, clamped at the bottom.
            let next = (idx + 1).min(bottom_idx);
            audio_tier_bps_for_index(next)
        }
        None => {
            // Off-ladder (defensive): step to the first tier strictly below the
            // current floor, clamped at the bottom.
            let next = AUDIO_QUALITY_TIERS
                .iter()
                .position(|t| t.bitrate_kbps * 1000 < current_floor_bps)
                .unwrap_or(bottom_idx);
            audio_tier_bps_for_index(next)
        }
    }
}

/// Pure recovery state machine for the single-layer congestion BITRATE floor
/// (issue #1398) — the bitrate analogue of [`audio_congestion_recover`].
/// Decides the NEXT floor (bps) given the current floor, the wall-clock time,
/// and when the last congestion cut fired. Climbs the floor UP exactly ONE tier
/// (toward 48 kbps) per `cooldown_ms`, and returns the `u32::MAX` fail-open
/// sentinel once it reaches (or already is at) the top tier (48 kbps).
///
/// Inputs mirror [`audio_congestion_recover`]:
///   * `current_floor_bps`: the floor right now (`u32::MAX` = fail-open / no cut;
///     any tier-aligned bps value is the post-cut floor).
///   * `now_ms` / `last_congestion_ms`: wall-clock now and the timestamp of the
///     last cut. `None` = no active cut → fail-open, fully recovered.
///   * `cooldown_ms`: the per-rung cooldown
///     ([`AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS`](crate::adaptive_quality_constants::AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS)).
///
/// Returns `(next_floor_bps, fully_recovered)`; `fully_recovered == true` once
/// the floor has climbed back to (or already was at) the top tier (48 kbps),
/// signalling the caller to drop to the fail-open sentinel and stop ticking.
/// ONE tier per cooldown gives the same hysteresis as the layer-ceiling ladder.
///
/// Pure (no clock, no atomics) so it is host-testable.
fn audio_bitrate_recover(
    current_floor_bps: u32,
    now_ms: f64,
    last_congestion_ms: Option<f64>,
    cooldown_ms: f64,
) -> (u32, bool) {
    let top_bps = audio_tier_bps_for_index(0);
    // No active cut → fail-open, fully recovered (nothing to climb).
    let Some(cut_ms) = last_congestion_ms else {
        return (u32::MAX, true);
    };
    // Sentinel or already at/above the top tier → already fully recovered.
    if current_floor_bps == u32::MAX || current_floor_bps >= top_bps {
        return (u32::MAX, true);
    }
    // Resolve the current rung; an off-ladder value is treated as the
    // next-lower tier (so a climb still makes progress). Defensive only.
    let current_idx = audio_tier_index_for_bps(current_floor_bps).unwrap_or_else(|| {
        AUDIO_QUALITY_TIERS
            .iter()
            .position(|t| t.bitrate_kbps * 1000 <= current_floor_bps)
            .unwrap_or(AUDIO_QUALITY_TIERS.len() - 1)
    });
    if now_ms - cut_ms >= cooldown_ms {
        // Climb ONE tier toward the top (lower index = higher bitrate).
        let next_idx = current_idx.saturating_sub(1);
        if next_idx == 0 {
            // Reached the top tier (48 kbps) → fail-open + done.
            (u32::MAX, true)
        } else {
            (audio_tier_bps_for_index(next_idx), false)
        }
    } else {
        // Still in cooldown — hold.
        (current_floor_bps, false)
    }
}

/// One tick of the single-layer congestion BITRATE-floor recovery state machine
/// (issue #1398) — the bitrate analogue of [`audio_congestion_tick`]. Pure so
/// the FULL per-tick behaviour (new-cut detection, one-tier climb, per-rung
/// cooldown reset, full-recovery clear) is host-testable without a browser.
///
/// State carried by the caller between ticks mirrors [`audio_congestion_tick`]:
///   * `current`: the floor atom's value now (`u32::MAX` = fail-open; a real
///     value = post-cut floor bps).
///   * `last_seen`: the floor value THIS loop last left the atom at — used to
///     detect a NEW cut (the client storing a LOWER bps value out-of-band). A
///     cut stores a SMALLER bitrate, so "new cut" is `current < last_seen`,
///     exactly like the ceiling tick (where a cut stores a smaller layer count).
///   * `last_congestion_ms`: when the current cooldown window started.
///   * `distress_active_now`: TRUE when the uplink-distress DETECTOR fired a
///     step-down DECISION on THIS tick (issue #1398 hold-at-floor fix). It
///     re-anchors the cooldown to `now_ms` REGARDLESS of whether the floor VALUE
///     changed. This is the load-bearing input at the EMERGENCY floor (8 kbps):
///     a step-down there is a clamped no-op, so `current == last_seen` and the
///     value-decrease path below would NOT re-anchor — leaving the cooldown to
///     elapse and recovery to climb 8k→12k while severe distress is still
///     ongoing, only to be re-cut a window later (a wasteful ~4 s excursion every
///     cooldown). With this flag, an ongoing decision HOLDS the floor: it leaves
///     `current` in place and resets the cut timestamp to `now_ms`, so the climb
///     can only begin after distress STOPS for a full cooldown. OFF the floor the
///     existing per-tier behaviour is unchanged — a real value decrease already
///     re-anchors via `current < last_seen`, and a same-value/no-decrease tick
///     with the flag set anchors to the SAME `now_ms` it would resolve to anyway.
///
/// Returns the new `(floor, last_seen, last_congestion_ms)` triple.
fn audio_bitrate_tick(
    current: u32,
    last_seen: u32,
    now_ms: f64,
    last_congestion_ms: Option<f64>,
    cooldown_ms: f64,
    distress_active_now: bool,
) -> (u32, u32, Option<f64>) {
    // HOLD-AT-FLOOR (issue #1398): a step-down DECISION fired this tick. Re-anchor
    // the cooldown to `now_ms` so recovery does NOT climb — distress is still
    // ongoing. At the emergency floor (8 kbps) the step-down was a clamped no-op
    // (`current == last_seen`), so the value-decrease path below would otherwise
    // NOT re-anchor; this flag is what holds the floor there. We HOLD the current
    // value (no climb), leave `last_seen` at `current` (a hold is not a new cut
    // next tick), and anchor the cut to `now_ms`. NOTE: at the fail-open sentinel
    // (`current == u32::MAX`) there is nothing to hold — the floor is already
    // fully recovered, so we fall through to the normal path, which short-circuits
    // MAX to fully-recovered and clears the cut (a stale distress flag must never
    // wedge a fail-open floor). Off the floor, a decision with a real value
    // decrease gives the SAME `(current, current, Some(now_ms))` the normal path
    // would, so the two paths agree there.
    if distress_active_now && current != u32::MAX {
        return (current, current, Some(now_ms));
    }
    // New-cut detection: a DECREASE in bitrate vs what we last left means the
    // client cut the floor out-of-band; restart the cooldown from now.
    let cut = if current < last_seen {
        Some(now_ms)
    } else {
        last_congestion_ms
    };
    let (next, fully_recovered) = audio_bitrate_recover(current, now_ms, cut, cooldown_ms);
    let next_cut = if fully_recovered {
        // Back at the fail-open sentinel — forget the cut so the next decrease
        // is detected fresh.
        None
    } else if next > current {
        // Intermediate climb (higher bitrate) — restart the per-rung cooldown so
        // tiers are spaced a full cooldown apart (true one-tier-per-cooldown
        // hysteresis). Without this every tier after the first would climb on
        // consecutive ticks, collapsing the hysteresis.
        Some(now_ms)
    } else {
        cut
    };
    // We leave the atom at `next`, so that is what the next tick must compare
    // against for new-cut detection (a climb we performed is not a new cut).
    (next, next, next_cut)
}

/// CAMERA-STATE-AWARE select of the effective single-layer audio bitrate (issue
/// #1398, FIX B/C). Pure. The two levers that govern single-layer audio bitrate
/// live independently and have leaky handoffs across camera transitions, so the
/// selection is keyed on the LIVE camera state rather than blindly MIN-composed:
///
///   * `tier_bps` — the bitrate the CAMERA AQ loop writes into the shared
///     tier-bitrate atom. It is written ONLY by the camera encoder's AQ loop
///     (needs camera frames) and never reset, so when the camera is OFF it holds
///     a STALE value (its last camera-on tier, or the 48000 top-tier default if
///     the camera was never on this session).
///   * `congestion_floor_bps` — the MIC-side single-layer congestion floor
///     (`u32::MAX` = fail-open / no cut), driven by the mic uplink-distress
///     detector while the camera is off.
///
/// Selection:
///   * CAMERA ON: return `tier_bps`. The camera AQ loop is the live authority on
///     the audio tier (its audio-tier degrade is gated on encoder-queue
///     backpressure; its uplink self-shed steps VIDEO, not audio — the camera-on
///     uplink→audio downshift is the deferred #1611 backstop). The mic detector is
///     GATED OFF (see [`audio_detector_gate_open`]) and the mic floor must NOT
///     apply here.
///   * CAMERA OFF, floor fail-open (`u32::MAX`): return the HEALTHY TOP-TIER
///     constant `AUDIO_QUALITY_TIERS[0].bitrate_kbps * 1000` (48000), IGNORING the
///     (possibly stale) `tier_bps`. The camera AQ loop never restored `tier_bps`
///     after a camera-on congestion episode, so reading it would pin audio-only
///     at a stale low tier forever on a healthy link. With no cut, the correct
///     audio-only bitrate is the healthy top tier.
///   * CAMERA OFF, floor cut: return the `floor`. The mic detector is the live
///     authority audio-only; the STALE `tier_bps` must be IGNORED (FIX B).
///
/// Only the BITRATE choice is camera-state-aware; the FEC/packet-loss ctls still
/// derive from the tier index, unchanged.
///
/// Issue #1611 backstop: when `camera_video_exhausted` is true AND the camera is
/// on, the detector gate opened because video can't shed further. In this case
/// the effective bitrate is `min(tier_bps, congestion_floor_bps)` — composing the
/// camera AQ tier with the mic floor, whichever is more restrictive. This is SAFE
/// because the camera audio-tier degrade is CPU/encoder-queue-gated
/// (controller.rs:603-624) and the mic floor is uplink-gated — independent
/// causes, so MIN composition cannot over-suppress (only the tighter of two
/// independent constraints applies). If the mic floor has no cut (u32::MAX),
/// `min(tier, MAX)` = tier (no change from pre-#1611).
///
/// Screen: `screen_video_exhausted` is NOT composed into the MIN because the
/// screen encoder writes NO audio-tier atom. The screen-exhausted signal only
/// GATES the detector open (via `audio_detector_gate_open`); it does not influence
/// the bitrate select. A screen-only publisher (camera off) follows the existing
/// camera-off path (floor governs).
fn effective_audio_bitrate(
    tier_bps: u32,
    congestion_floor_bps: u32,
    camera_active: bool,
    camera_video_exhausted: bool,
) -> u32 {
    if camera_active && camera_video_exhausted {
        // Camera on BUT video exhausted (#1611 backstop): compose the camera AQ
        // tier with the mic floor — the more restrictive wins.
        tier_bps.min(congestion_floor_bps)
    } else if camera_active {
        // Camera on, video NOT exhausted: the camera AQ tier governs and the mic
        // floor does not apply (unchanged from pre-#1611).
        tier_bps
    } else if congestion_floor_bps == u32::MAX {
        // Camera off, no cut: return the healthy TOP-TIER constant, IGNORING the
        // (possibly stale) tier_bps. The camera AQ loop never restores tier_bps
        // after a camera-on congestion episode, so reading it here would pin
        // audio-only at the stale low tier forever on a healthy link.
        AUDIO_QUALITY_TIERS[0].bitrate_kbps * 1000
    } else {
        // Camera off, active cut: the mic floor governs; stale tier_bps ignored
        // (the floor is the live audio-only authority).
        congestion_floor_bps
    }
}

/// Holds the previous audio frame for RED-style redundancy.
pub(crate) struct PreviousAudioFrame {
    data: Vec<u8>,
    sequence: u64,
}

/// Pack primary and redundant audio frames into a single data buffer.
///
/// Format: `[4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]`
///
/// The receiver uses `primary_len` to split the buffer and `redundant_seq`
/// to check whether the redundant frame was already received.
fn pack_redundant_audio(primary: &[u8], redundant: &PreviousAudioFrame) -> Vec<u8> {
    let primary_len = primary.len() as u32;
    let redundant_seq = redundant.sequence as u32;
    let total_len = 4 + primary.len() + 4 + redundant.data.len();
    let mut buf = Vec::with_capacity(total_len);
    buf.extend_from_slice(&primary_len.to_le_bytes());
    buf.extend_from_slice(primary);
    buf.extend_from_slice(&redundant_seq.to_le_bytes());
    buf.extend_from_slice(&redundant.data);
    buf
}

#[allow(clippy::too_many_arguments)]
pub fn transform_audio_chunk(
    chunk: &Uint8Array,
    user_id: &str,
    sequence: u64,
    aes: Rc<Aes128State>,
    previous_frame: Option<&PreviousAudioFrame>,
    simulcast_layer_id: u32,
) -> PacketWrapper {
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;

    let primary_data = chunk.to_vec();

    // Determine whether to include redundancy.
    let (data, audio_format) = match previous_frame {
        Some(prev) => {
            let packed = pack_redundant_audio(&primary_data, prev);
            (packed, AUDIO_RED_FORMAT.to_string())
        }
        None => (primary_data, String::new()),
    };

    let media_packet: MediaPacket = MediaPacket {
        user_id: Vec::new(),
        media_type: MediaType::AUDIO.into(),
        frame_type: EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Key).to_string(),
        data,
        timestamp: now_ms,
        audio_metadata: Some(AudioMetadata {
            sequence,
            audio_format,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };
    let data = media_packet.write_to_bytes().unwrap();
    let data = aes.encrypt(&data).unwrap();
    PacketWrapper {
        data,
        user_id: user_id.as_bytes().to_vec(),
        packet_type: PacketType::MEDIA.into(),
        // Cleartext discriminator so the relay can apply viewport-aware VIDEO
        // filtering while ALWAYS forwarding AUDIO (HCL issue #988). Phase 3
        // additionally lets the relay layer-filter AUDIO per receiver.
        media_kind: MediaKind::AUDIO.into(),
        // Cleartext simulcast layer id (issue #989, Phase 3c). Tag 5 serializes
        // only when non-zero, so layer 0 — the single-layer default and what
        // every pre-simulcast mic publisher emits — is wire-identical to today.
        // The relay's per-(source, AUDIO) layer filter and the receiver's audio
        // layer-select guard read this (mirrors transform_video/screen_chunk).
        simulcast_layer_id,
        ..Default::default()
    }
}

pub struct MicrophoneEncoder {
    client: VideoCallClient,
    state: EncoderState,
    _on_encoder_settings_update: Option<Callback<String>>,
    /// Per-layer Opus encoders, **lowest layer first** (index ==
    /// `simulcast_layer_id`). Always at least one element: index 0 is the BASE
    /// layer, which in single-layer mode (the default) is the only encoder and
    /// runs at the tier bitrate, byte-identical to the pre-simulcast mic path.
    /// In N-layer simulcast mode (issue #989 / #1082) indices `1..N` are
    /// additional `AudioWorkletNode`s on the SAME `AudioContext`, each fed the
    /// same captured audio (fanned out from the analyser node) and encoding at
    /// its rung's [`AUDIO_SIMULCAST_LAYER_KBPS`] bitrate, stamping
    /// `simulcast_layer_id == index`.
    ///
    /// `AudioWorkletCodec` is `Rc<RefCell<…>>`-backed, so cloning a codec out of
    /// this Vec into a worker closure shares the same underlying node (cheap).
    /// Sized to the effective layer count in [`MicrophoneEncoder::start`]; holds
    /// a single default (empty) codec until then so `set_enabled`/`stop` are
    /// safe before `start`.
    ///
    /// ROLLOUT NOTE (low-power devices): each higher layer (`1..N`) is a SECOND+
    /// full Opus encode of the same mic input, so audio encode CPU scales roughly
    /// linearly with the active layer count. Opus is cheap relative to video, so
    /// this is acceptable — and it is flag-gated: higher layers are only
    /// instantiated when the effective audio layer count is > 1 (driven by
    /// `experimentalSimulcastMaxLayers` × the device-capability ceiling), so a
    /// weak device that gates audio to a single layer pays nothing. If a future
    /// rollout sees audio-CPU pressure on low-power hardware, gate the higher
    /// audio layers behind a higher capability tier than video.
    codecs: Vec<AudioWorkletCodec>,
    /// Maximum audio simulcast layers (issue #989, Phase 3c → up to 3 in #1082).
    /// 1 = single layer (default, byte-identical). Clamped in `start` to
    /// `[1, AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS]`.
    max_layers: u32,
    on_error: Option<Callback<String>>,
    is_speaking: Rc<AtomicBool>,
    vad_interval: Rc<RefCell<Option<Interval>>>,
    /// CONGESTION-recovery timer (issue #621). Created in [`Self::start`] and torn
    /// down on stop / disable / reconnect exactly like [`Self::vad_interval`], so
    /// it cannot outlive the encoder. Climbs the congestion layer ceiling back up
    /// one rung per cooldown when no new congestion has fired. Owned on the MIC
    /// side (NOT the camera AQ loop) so recovery works even when the camera is
    /// off (audio-only).
    congestion_recovery_interval: Rc<RefCell<Option<Interval>>>,
    /// Live Opus FEC ctl-reconfig timer (issue #1567). Created in [`Self::start`]
    /// and torn down on stop / disable / reconnect exactly like
    /// [`Self::vad_interval`] and [`Self::congestion_recovery_interval`], so it
    /// cannot outlive the encoder. Ticks at
    /// [`AUDIO_FEC_RECONFIG_TICK_MS`] (1 Hz), reads the shared audio-tier index,
    /// and — only when the tier's `(enable_fec, packet_loss_perc)` changed —
    /// posts a `reconfigOpus` message to the live worklet encoder(s). Owned on
    /// the MIC side (NOT the camera AQ loop) so it works even when the camera is
    /// off (audio-only), and so the worklet ports are in scope.
    fec_reconfig_interval: Rc<RefCell<Option<Interval>>>,
    vad_threshold: f32,
    /// Tier-controlled audio bitrate in bps (e.g. 48000 for 48 kbps). Shared with
    /// the camera encoder's quality manager, which is its ONLY writer: it lowers
    /// this on each camera-driven audio-tier change (which needs camera frames) and
    /// never resets it. Defaults to the top tier (48000 bps). When the camera is
    /// off this atom holds a STALE value — its last camera-on tier, or the 48000
    /// default if the camera was never on this session — so [`effective_audio_bitrate`]
    /// IGNORES it in the camera-off case (FIX B).
    ///
    /// READ AT RUNTIME (issue #1398): in SINGLE-LAYER mode the live Opus reconfig
    /// timer feeds this (with the congestion bitrate floor and the live camera
    /// state) to the CAMERA-STATE-AWARE [`effective_audio_bitrate`] — camera-on
    /// uses THIS tier; camera-off uses the floor when cut, else the healthy
    /// top-tier default (and IGNORES this stale tier) — and re-applies the result
    /// to the running encoder via the `reconfigOpus` ctl 4002 (OPUS_SET_BITRATE). The AudioWorklet
    /// CAN now reconfigure bitrate live (the worklet's `reconfigOpus` case calls
    /// `setOpusControl(4002, bitRate)` — the same ctl libopus is initialized
    /// with), so the old "not read at runtime / worklet has no dynamic bitrate
    /// reconfig" caveat no longer holds. In MULTI-LAYER mode the per-layer
    /// encoders own their fixed ladder bitrates and the layer-ceiling lever
    /// (#621) handles congestion, so this atom is not used for the live reconfig
    /// there.
    tier_audio_bitrate: Rc<AtomicU32>,
    /// Whether the current audio tier has FEC enabled.
    /// When true AND `AUDIO_REDUNDANCY_ENABLED`, each packet carries the
    /// previous frame as redundant data for loss recovery.
    tier_enable_fec: Rc<AtomicBool>,
    /// Current audio quality tier INDEX (0 = healthy "high", up; written by the
    /// camera encoder's AQ loop on each audio-tier change). The live FEC
    /// ctl-reconfig timer (issue #1567) reads this once per second, maps it to
    /// `AUDIO_QUALITY_TIERS[idx]` to recover BOTH `enable_fec` and
    /// `packet_loss_perc` from a single source of truth, and re-applies the Opus
    /// ctl to the running worklet encoder when that pair changes. Sharing the
    /// INDEX (not a second loss-% atom) keeps FEC and loss-% from ever drifting
    /// apart vs. the existing `tier_enable_fec` bool. Defaults to 0 (top tier).
    shared_audio_tier_index: Rc<AtomicU32>,
    /// User SEND audio layer-ceiling (perf-panel "layers published" thumb). The
    /// performance panel lets the user bound how many audio simulcast layers this
    /// publisher emits; the UI writes the chosen layer COUNT here (via
    /// [`Self::set_user_layer_ceiling`]), and each per-layer publish handler reads
    /// it LIVE at publish time and skips layers whose `layer_id >= ceiling_count`.
    /// The base layer (`layer_id == 0`, 24 kbps) is ALWAYS published (the ceiling
    /// floors at 1) — mirroring the video/screen base-present invariant.
    ///
    /// **Initialized to [`u32::MAX`] = fail-open (Auto / no user cap):** until the
    /// user drags the thumb below full, every configured layer publishes. The
    /// value is mapped through `camera_encoder::layer_ceiling_to_count` (the
    /// `u32::MAX` sentinel → `usize::MAX` fail-open) at read time. NOT reset on
    /// reconnect — the user's explicit choice persists; `Host` re-applies it from
    /// the persisted preference on encoder (re)start regardless.
    shared_user_layer_ceiling: Rc<AtomicU32>,
    /// CONGESTION-driven SEND audio layer-ceiling (issue #621) — SEPARATE from the
    /// user perf-panel ceiling above. Composed with it via `min` in
    /// [`audio_layer_is_published`], so whichever is tighter wins and the base
    /// layer is always published.
    ///
    /// Owned by [`VideoCallClient`] (an `Arc`, wired in via
    /// [`Self::set_congestion_layer_ceiling`]) so the CONGESTION dispatch arm can
    /// drive it DOWN to base-only (count `1`) on a self-targeted CONGESTION signal
    /// — the audio analogue of the camera's `force_congestion_cut`, but via the
    /// layer-ceiling lever because the Opus AudioWorklet cannot reconfigure
    /// bitrate live (the issue's literal "step down one tier" path is blocked).
    /// Read LIVE at publish time by every layer handler, so a cut takes effect on
    /// the very next frame with NO audio interruption (the base layer keeps
    /// flowing; only the upper layers stop).
    ///
    /// **Initialized to [`u32::MAX`] = fail-open (no congestion cap).** A
    /// self-contained recovery `Interval` on the mic side (see
    /// [`Self::start`]) climbs it back ONE rung per
    /// [`AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS`](crate::adaptive_quality_constants::AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS),
    /// independent of whether the CAMERA is on — critical for the audio-only case
    /// where the camera AQ loop (which drives audio tier decisions) is NOT
    /// running. Reset to the fail-open sentinel on reconnect by the client so a
    /// stale cut from the old session does not suppress audio on a fresh one.
    ///
    /// `Arc` (not `Rc`) so it can cross into `VideoCallClient`, matching the
    /// camera's `Arc<AtomicBool>` congestion-flag wiring.
    ///
    /// SINGLE-LAYER NOTE (#621 → CLOSED by #1398): when the device is
    /// capability-gated to a SINGLE audio layer (or audio simulcast is disabled),
    /// THIS ceiling is a no-op — the base layer is always published and there is
    /// no upper layer to shed. For that case the single-layer congestion BITRATE
    /// floor below ([`Self::shared_congestion_bitrate_floor`], #1398) downshifts
    /// the one running Opus stream's bitrate live instead, so a single-encoder
    /// congested publisher CAN now downshift audio. In multi-layer mode this
    /// ceiling remains the lever (the bitrate floor is not applied there — see
    /// the FEC reconfig timer's single-layer gate) so the two levers never
    /// double-dip.
    shared_congestion_layer_ceiling: Arc<AtomicU32>,
    /// SINGLE-LAYER audio BITRATE floor in bps (issue #1398) — the bitrate
    /// analogue of [`Self::shared_congestion_layer_ceiling`], and the lever that
    /// closes the single-layer gap that ceiling could not.
    ///
    /// `u32::MAX` = fail-open ("no congestion cut"). DRIVEN DOWN by the mic-side
    /// uplink-distress detector (the congestion-recovery `Interval` in
    /// [`Self::start`], #1398), NOT by the client: each detector tick reads the
    /// live process-global transport counters (`unistream_ready_stall_count` /
    /// `websocket_drop_count`) and, on SUSTAINED distress while the camera is OFF
    /// (audio-only or screen-only), stores
    /// `audio_congestion_bitrate_step_down(floor)` here. (The original b127ee80
    /// trigger — a self-targeted `PacketType::CONGESTION` server packet — was
    /// retired: #1219 Half 1 removed that emission server-side, so it never fired
    /// in production. The retarget onto the live uplink signal is #1398.)
    ///
    /// The atom is still OWNED by [`VideoCallClient`] (an `Arc`, wired in via
    /// [`Self::set_congestion_bitrate_floor`]) for ONE reason only: the client
    /// RESETS it to the fail-open sentinel on reconnect so a stale cut does not
    /// pin audio bitrate low on a fresh session. Atom identity is unchanged from
    /// b127ee80; only its WRITER moved (from the client dispatch to the mic
    /// detector).
    ///
    /// Read LIVE by the FEC/bitrate reconfig timer ONLY in single-layer mode
    /// (`effective_audio_layers() == 1`): there the timer feeds it (with the tier
    /// bitrate and the live camera state) to [`effective_audio_bitrate`], which is
    /// CAMERA-STATE-AWARE — camera-on uses the tier, camera-off uses THIS floor —
    /// and re-applies the result via the worklet's `reconfigOpus` ctl 4002
    /// (OPUS_SET_BITRATE). In multi-layer mode the timer sends `bit_rate: None`
    /// (the layer-ceiling handles congestion there), so this floor has no effect.
    /// On the camera-ON edge the detector clears this floor to fail-open once
    /// (FIX C) so a stale cut cannot re-cap the stream if the camera later goes off.
    ///
    /// Recovery: the same mic-side `Interval` climbs it back ONE tier per
    /// [`AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS`](crate::adaptive_quality_constants::AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS),
    /// independent of whether the CAMERA is on — critical for the audio-only case.
    ///
    /// `Arc` (not `Rc`) so it can cross into `VideoCallClient`, matching the
    /// layer-ceiling atom's wiring.
    shared_congestion_bitrate_floor: Arc<AtomicU32>,
    /// Camera ENABLED flag (issue #1398): the GATE term for the mic-side uplink
    /// distress detector AND the camera-state selector for the effective audio
    /// bitrate. `true` = camera on, `false` = camera off.
    ///
    /// Owned by the [`CameraEncoder`] (its `EncoderState::enabled` atom, shared in
    /// via [`Self::set_camera_active_signal`]). The Host writes it directly
    /// (`camera.set_enabled(true/false)`), so `false` is an UNAMBIGUOUS,
    /// always-current "camera off" indication.
    ///
    /// Two uses (#1398):
    ///   * GATE: the mic uplink-distress detector fires ONLY when this reads
    ///     `false` (camera off). When the camera is ON, `effective_audio_bitrate`
    ///     returns the camera AQ tier and ignores the mic floor, so a mic-side
    ///     floor cut would never be read — the detector stays quiet to avoid
    ///     cutting a dead floor. (The camera AQ owns the audio tier via
    ///     encoder-queue backpressure; its uplink self-shed steps VIDEO, not audio
    ///     — the camera-on uplink→audio downshift is the deferred #1611 backstop.)
    ///     The screen encoder is NOT a gate term — it writes no audio-tier atom, so
    ///     a screen-sharing publisher relies on the mic detector for its only audio
    ///     downshift.
    ///   * BITRATE SELECT: the FEC reconfig timer reads this to choose the
    ///     effective single-layer bitrate (see [`effective_audio_bitrate`]) —
    ///     camera-on uses the camera AQ tier, camera-off uses the mic floor.
    ///
    /// Defaults to `false` (camera-off) when unwired — matching [`EncoderState`]'s
    /// own `enabled` default — so a mic constructed without the signal (tests)
    /// treats the publisher as audio-only, which is the safe assumption.
    ///
    /// `Arc` (not `Rc`) because it is the camera's `Arc<AtomicBool>` crossing
    /// encoders, matching the congestion-flag wiring.
    camera_active: Arc<AtomicBool>,
    /// Camera video-exhausted flag (issue #1611 backstop, lever 2): `true` when
    /// the camera AQ's video quality is fully exhausted (tier at user-capped
    /// step-down floor AND active simulcast layers at 1). Stored by the camera
    /// encoder's AQ control loop unconditionally each tick.
    ///
    /// Used in the detector gate: when this is `true` AND the camera is on, the
    /// gate opens anyway (the camera video can't shed further, so audio is the
    /// only remaining axis). Also used in `effective_audio_bitrate` to compose
    /// `min(tier, floor)` — see the function's doc for the safety justification.
    ///
    /// Defaults to `false` (camera video not exhausted) when unwired (tests),
    /// which is the SAFE assumption: the detector stays gated to camera-off and
    /// behaves identically to pre-#1611. Replaced by the camera's atom via
    /// [`Self::set_camera_video_exhausted_signal`].
    camera_video_exhausted: Arc<AtomicBool>,
    /// Screen video-exhausted flag (issue #1611 backstop, lever 3): `true` when
    /// the screen AQ's video quality is fully exhausted. Stored by the screen
    /// encoder's AQ control loop each tick while sharing, cleared synchronously
    /// on share-start.
    ///
    /// Used in the detector gate: when this is `true` AND screen is active, the
    /// gate opens (screen video can't shed further). NOT used in
    /// `effective_audio_bitrate` (screen writes no audio-tier atom).
    ///
    /// Defaults to `false` when unwired (tests) — safe: detector treats screen
    /// as "can still shed" and stays gated. Replaced via
    /// [`Self::set_screen_video_exhausted_signal`].
    screen_video_exhausted: Arc<AtomicBool>,
    /// Screen-sharing-active flag (issue #1611 backstop, lever 3): `true` when
    /// screen capture is running. This is the SAME `screen_sharing_active`
    /// `Rc<AtomicBool>` from the camera encoder, but we accept `Arc<AtomicBool>`
    /// for the wire. The screen encoder writes it on share start/stop.
    ///
    /// Used ONLY in the detector gate's screen term:
    /// `(!screen_active || screen_video_exhausted)`. When screen is NOT active,
    /// the term is vacuously true (screen can't block the gate). MUST use
    /// `now_sharing` semantics (the `screen_sharing_active` atom) rather than
    /// `state.enabled` (which leads the controller by ~1s and would gate audio
    /// off prematurely; see issue #1611 exactness item #3).
    ///
    /// Defaults to `false` (no screen share) when unwired — safe: gate's screen
    /// term is vacuously true. Replaced via
    /// [`Self::set_screen_sharing_active_signal`].
    screen_sharing_active: Arc<AtomicBool>,
    /// Connection RECONNECT-reseed flag (issue #1398 reconnect P1): the client
    /// sets this `true` in its `Connected` handler (next to the bitrate-floor
    /// reset) on every (re)connect; the mic-side uplink-distress detector tick
    /// CONSUMES it (swap-to-false) and, while consuming, FORCES a window re-seed
    /// even though the detector stayed continuously active across the reconnect.
    ///
    /// Why the existing `!was_active` reseed is insufficient: on a plain network
    /// reconnect the mic encoder is NOT restarted (the mic stays enabled and
    /// `EncoderState::switching` stays false), so the detector keeps running with
    /// its gate open (camera off, single-layer) and `det_was_active == true` —
    /// the `!was_active` path never fires. But the transport teardown/rebuild
    /// BUMPS the monotonic `unistream_*` / `websocket_drop` counters, so the first
    /// window that closes after the reconnect would compute a cross-reconnect
    /// delta and cash a spurious cut on the FRESH session. Consuming this flag
    /// re-anchors the windows to "now" so distress is measured from the new
    /// session forward. Shared in via [`Self::set_reconnect_reseed_signal`]; the
    /// client OWNS it (an `Arc`) for the same reason it owns the floor atom — to
    /// set it from the `Connected` handler even when `Inner` is contended.
    ///
    /// Defaults to `false` (no reconnect pending) when unwired (tests) — the safe
    /// value that leaves the FIX-1 `!was_active` reseed behaviour unchanged.
    reconnect_reseed: Arc<AtomicBool>,
}

impl MicrophoneEncoder {
    /// Construct a microphone encoder.
    ///
    /// `shared_audio_tier_bitrate`, `shared_audio_tier_fec`, and
    /// `shared_audio_tier_index` are shared atomics owned by the `CameraEncoder`.
    /// The camera encoder's quality manager writes to these when the audio tier
    /// changes, and the microphone encoder reads them to apply the current audio
    /// settings. This avoids creating a duplicate `EncoderBitrateController` that
    /// would redundantly process the same diagnostics packets.
    ///
    /// `shared_audio_tier_index` (issue #1567) drives the live Opus FEC
    /// ctl-reconfig timer: the mic maps the index to `AUDIO_QUALITY_TIERS[idx]`
    /// to derive `(enable_fec, packet_loss_perc)` and re-apply them to the live
    /// encoder worklet on a mid-call tier change.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: VideoCallClient,
        _bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
        on_error: Callback<String>,
        vad_threshold: Option<f32>,
        shared_audio_tier_bitrate: Option<Rc<AtomicU32>>,
        shared_audio_tier_fec: Option<Rc<AtomicBool>>,
        shared_audio_tier_index: Option<Rc<AtomicU32>>,
        max_layers: u32,
    ) -> Self {
        let default_audio_bitrate_bps = AUDIO_QUALITY_TIERS[0].bitrate_kbps * 1000;
        let default_enable_fec = AUDIO_QUALITY_TIERS[0].enable_fec;
        Self {
            client,
            state: EncoderState::new(),
            _on_encoder_settings_update: Some(on_encoder_settings_update),
            // Start with a single (empty) base codec; `start` resizes to the
            // effective layer count. Always non-empty so pre-`start`
            // enable/stop are safe.
            codecs: vec![AudioWorkletCodec::default()],
            max_layers,
            on_error: Some(on_error),
            is_speaking: Rc::new(AtomicBool::new(false)),
            vad_interval: Rc::new(RefCell::new(None)),
            congestion_recovery_interval: Rc::new(RefCell::new(None)),
            fec_reconfig_interval: Rc::new(RefCell::new(None)),
            vad_threshold: vad_threshold.unwrap_or(DEFAULT_VAD_THRESHOLD),
            tier_audio_bitrate: shared_audio_tier_bitrate
                .unwrap_or_else(|| Rc::new(AtomicU32::new(default_audio_bitrate_bps))),
            tier_enable_fec: shared_audio_tier_fec
                .unwrap_or_else(|| Rc::new(AtomicBool::new(default_enable_fec))),
            // Audio tier INDEX shared from the camera AQ loop (issue #1567).
            // Defaults to 0 (the healthy top tier the mic inits at) when no
            // shared atom is provided, so the FEC-reconfig timer observes the
            // init state and stays quiescent until a real tier change.
            shared_audio_tier_index: shared_audio_tier_index
                .unwrap_or_else(|| Rc::new(AtomicU32::new(0))),
            // User SEND audio layer-ceiling (perf-panel). Fail-open: u32::MAX =
            // Auto / no user cap until the panel writes a layer count.
            shared_user_layer_ceiling: Rc::new(AtomicU32::new(u32::MAX)),
            // CONGESTION-driven audio layer-ceiling (issue #621). Fail-open:
            // u32::MAX = no congestion cap until a self-targeted CONGESTION cut
            // drives it down. Replaced by the client-owned atom via
            // `set_congestion_layer_ceiling` so the CONGESTION dispatch arm can
            // cut it without the camera AQ loop in the path.
            shared_congestion_layer_ceiling: Arc::new(AtomicU32::new(u32::MAX)),
            // Single-layer audio BITRATE floor (issue #1398). Fail-open:
            // u32::MAX = no congestion cut until the mic-side uplink-distress
            // detector steps it down one tier. Replaced by the client-owned atom
            // via `set_congestion_bitrate_floor` (the client RESETS it on
            // reconnect); the mic detector is the writer.
            shared_congestion_bitrate_floor: Arc::new(AtomicU32::new(u32::MAX)),
            // Camera-active gate + bitrate selector (issue #1398). Default false
            // (camera-off / audio-only) — matching EncoderState::enabled's own
            // default — so an unwired mic treats the publisher as audio-only.
            // Replaced by the camera's `EncoderState::enabled` atom via
            // `set_camera_active_signal`.
            camera_active: Arc::new(AtomicBool::new(false)),
            // Issue #1611 backstop signals. Default false = "not exhausted" / "not
            // sharing" — the safe assumption that keeps the detector behaving
            // identically to pre-#1611 until wired.
            camera_video_exhausted: Arc::new(AtomicBool::new(false)),
            screen_video_exhausted: Arc::new(AtomicBool::new(false)),
            screen_sharing_active: Arc::new(AtomicBool::new(false)),
            // Reconnect-reseed flag (issue #1398 reconnect P1). Default false (no
            // reconnect pending) so an unwired mic keeps the FIX-1 `!was_active`
            // reseed behaviour unchanged. Replaced by the client-owned atom via
            // `set_reconnect_reseed_signal`; the client sets it true on every
            // (re)connect and the detector tick consumes it.
            reconnect_reseed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set the user's SEND audio layer-ceiling from the performance panel — the
    /// "layers published" control (mirror of
    /// [`CameraEncoder::set_user_layer_ceiling`](crate::CameraEncoder::set_user_layer_ceiling)).
    ///
    /// `ceiling` is the maximum number of audio simulcast layers the user wants
    /// this publisher to emit, as a layer COUNT (1 = base only / 24 kbps, up to
    /// the audio ladder depth). `None` = Auto / no user cap. Applied LIVE with NO
    /// mic-encoder restart: each per-layer publish handler reads this atomic at
    /// publish time and skips layers above the ceiling, so lowering it stops
    /// sending the top audio layer(s) on the very next frame and raising it
    /// resumes them — no audio interruption. The base layer (layer 0) is always
    /// published (the read-side floors the count at 1).
    ///
    /// Valid whether or not the encoder is running; the value persists in the
    /// shared atomic (cloned into every handler), so it survives across reconnect
    /// and `Host` re-applies it from the persisted preference on re-init.
    pub fn set_user_layer_ceiling(&self, ceiling: Option<u32>) {
        self.shared_user_layer_ceiling
            .store(ceiling.unwrap_or(u32::MAX), Ordering::Relaxed);
    }

    /// The current user SEND audio layer-ceiling (layer COUNT), or `None` for Auto
    /// / no user cap. For the UI to render its current selection.
    pub fn user_layer_ceiling(&self) -> Option<u32> {
        match self.shared_user_layer_ceiling.load(Ordering::Relaxed) {
            u32::MAX => None,
            n => Some(n),
        }
    }

    /// Returns a clone of the user layer ceiling atomic for the health reporter.
    pub fn shared_user_layer_ceiling(&self) -> Rc<AtomicU32> {
        self.shared_user_layer_ceiling.clone()
    }

    /// Replace the internal CONGESTION audio layer-ceiling atom with an
    /// externally-owned one (issue #621).
    ///
    /// Call this after construction to share the atom with [`VideoCallClient`],
    /// which drives it DOWN to base-only on a self-targeted server CONGESTION
    /// signal (see [`VideoCallClient::audio_congestion_layer_ceiling`]). Mirrors
    /// [`CameraEncoder::set_congestion_step_down_flag`](crate::CameraEncoder::set_congestion_step_down_flag)
    /// — the atom is owned by the client and shared into the encoder so the
    /// CONGESTION dispatch can cut audio without depending on the camera AQ loop
    /// (which is not running in the audio-only case).
    pub fn set_congestion_layer_ceiling(&mut self, ceiling: Arc<AtomicU32>) {
        self.shared_congestion_layer_ceiling = ceiling;
    }

    /// The shared CONGESTION audio layer-ceiling atom, for wiring into
    /// [`VideoCallClient`] or for a host test to drive/observe (issue #621).
    pub fn congestion_layer_ceiling(&self) -> Arc<AtomicU32> {
        self.shared_congestion_layer_ceiling.clone()
    }

    /// Replace the internal single-layer audio BITRATE floor atom with an
    /// externally-owned one (issue #1398).
    ///
    /// Call this after construction to share the atom with [`VideoCallClient`].
    /// The client OWNS it only to RESET it to the fail-open sentinel on reconnect
    /// (see [`VideoCallClient::audio_congestion_bitrate_floor`]); the WRITER is
    /// the mic-side uplink-distress detector in [`Self::start`], which steps it
    /// down on sustained uplink distress while the camera is off. Sharing the atom
    /// (rather than keeping the mic's own) lets the client's reconnect handler
    /// clear a stale cut.
    pub fn set_congestion_bitrate_floor(&mut self, floor: Arc<AtomicU32>) {
        self.shared_congestion_bitrate_floor = floor;
    }

    /// The shared single-layer audio BITRATE floor atom, for wiring into
    /// [`VideoCallClient`] or for a host test to drive/observe (issue #1398).
    pub fn congestion_bitrate_floor(&self) -> Arc<AtomicU32> {
        self.shared_congestion_bitrate_floor.clone()
    }

    /// Share the CAMERA's enabled flag (issue #1398). Two uses: it GATES the
    /// mic-side uplink-distress detector to the camera being off, AND it selects
    /// how the FEC reconfig timer chooses the effective single-layer audio bitrate
    /// (camera-on → camera AQ tier; camera-off → the mic congestion floor; see
    /// [`effective_audio_bitrate`]).
    ///
    /// Pass [`CameraEncoder::camera_enabled_flag`](crate::CameraEncoder::camera_enabled_flag).
    /// The mic detector fires ONLY when this reads `false` (camera off); when the
    /// camera is active, [`effective_audio_bitrate`] returns the camera AQ tier and
    /// ignores the mic floor anyway, so gating the detector to camera-off keeps the
    /// two levers mutually exclusive (no compounding) and avoids cutting a floor
    /// that would never be read. (The camera AQ's own uplink self-shed steps VIDEO,
    /// not audio — the camera-on uplink→audio downshift is the deferred #1611
    /// backstop.) The screen encoder is NOT a gate term — it writes no audio-tier
    /// atom, so a screen-sharing publisher relies on the mic detector.
    pub fn set_camera_active_signal(&mut self, camera_active: Arc<AtomicBool>) {
        self.camera_active = camera_active;
    }

    /// Share the connection RECONNECT-reseed flag (issue #1398 reconnect P1).
    ///
    /// Pass [`VideoCallClient::audio_detector_reconnect_reseed`]. The client sets
    /// it `true` in its `Connected` handler on every (re)connect; the mic-side
    /// uplink-distress detector tick CONSUMES it (swap-to-false) and forces a
    /// window re-seed, so the monotonic transport counters bumped by the
    /// transport teardown/rebuild are never read as a cross-reconnect distress
    /// delta on the fresh session. Needed because a plain reconnect does NOT
    /// restart the mic (the detector stays active with `det_was_active == true`),
    /// so the existing `!was_active` reseed path never fires across a reconnect.
    pub fn set_reconnect_reseed_signal(&mut self, reconnect_reseed: Arc<AtomicBool>) {
        self.reconnect_reseed = reconnect_reseed;
    }

    /// Share the camera's video-at-floor flag (issue #1611, lever 2). When this
    /// reads `true` AND the camera is on, the backstop gate opens — video can't
    /// shed further, so audio is the only remaining axis.
    ///
    /// Pass [`CameraEncoder::video_at_floor_flag`](crate::CameraEncoder::video_at_floor_flag).
    pub fn set_camera_video_exhausted_signal(&mut self, flag: Arc<AtomicBool>) {
        self.camera_video_exhausted = flag;
    }

    /// Share the screen's video-at-floor flag (issue #1611, lever 3). When this
    /// reads `true` AND screen is active, the backstop gate's screen term passes
    /// — screen video can't shed further.
    ///
    /// Pass [`ScreenEncoder::screen_at_floor_flag`](crate::ScreenEncoder::screen_at_floor_flag).
    pub fn set_screen_video_exhausted_signal(&mut self, flag: Arc<AtomicBool>) {
        self.screen_video_exhausted = flag;
    }

    /// Share the screen-sharing-active flag (issue #1611, lever 3). The gate's
    /// screen term is `(!screen_active || screen_video_exhausted)` — when screen
    /// is NOT active, the term is vacuously true (screen can't block the gate).
    /// Uses `now_sharing` (the `screen_sharing_active` atom) rather than
    /// `state.enabled` which leads the controller by ~1s.
    ///
    /// Pass a clone of [`CameraEncoder::screen_sharing_flag`](crate::CameraEncoder::screen_sharing_flag)
    /// cast to `Arc<AtomicBool>`. Because the camera holds it as `Rc<AtomicBool>`,
    /// the host must construct a separate `Arc<AtomicBool>` that the screen encoder
    /// also writes; alternatively, the host can pass a reference obtained from
    /// [`ScreenEncoder::screen_at_floor_flag`] pattern. See host.rs wiring.
    pub fn set_screen_sharing_active_signal(&mut self, flag: Arc<AtomicBool>) {
        self.screen_sharing_active = flag;
    }

    /// Returns the effective audio simulcast layer count (#1561): the clamped
    /// `max_layers` this encoder was constructed with. Constant for the
    /// session (the encoder is reconstructed on remount).
    pub fn effective_audio_layers(&self) -> u32 {
        clamp_audio_layer_count(self.max_layers)
    }

    pub fn set_error_callback(&mut self, on_error: Callback<String>) {
        self.on_error = Some(on_error);
    }

    // delegates to self.state
    pub fn set_enabled(&mut self, value: bool) -> bool {
        let is_changed = self.state.set_enabled(value);
        if is_changed {
            if value {
                // Start every layer (no-op for any not yet instantiated).
                for codec in &self.codecs {
                    let _ = codec.start();
                }
            } else {
                // First stop the codec(s) to prevent new audio frames
                for codec in &self.codecs {
                    let _ = codec.stop();
                }
                // The monitoring loop in start() will detect the enabled flag change
                // and stop the microphone capture within 100ms
                if let Some(interval) = self.vad_interval.borrow_mut().take() {
                    drop(interval);
                }
                // Tear down the congestion-recovery timer too (issue #621); it is
                // re-created on the next start(). The congestion ceiling atom is
                // left as-is (it persists with the same survive-restart contract as
                // the user ceiling and is reset to fail-open on reconnect by the
                // client).
                if let Some(interval) = self.congestion_recovery_interval.borrow_mut().take() {
                    drop(interval);
                }
                // Reset speaking state and audio level when mic is disabled
                self.is_speaking.store(false, Ordering::Relaxed);
                self.client.set_speaking(false);
                self.client.set_audio_level(0.0);
            };
        }
        is_changed
    }

    pub fn select(&mut self, device: String) -> bool {
        self.state.select(device)
    }
    pub fn stop(&mut self) {
        self.state.stop();
        for codec in &self.codecs {
            codec.destroy();
        }
        if let Some(interval) = self.vad_interval.borrow_mut().take() {
            drop(interval);
        }
        // Tear down the congestion-recovery timer (issue #621), mirroring the
        // vad_interval teardown above.
        if let Some(interval) = self.congestion_recovery_interval.borrow_mut().take() {
            drop(interval);
        }
        // Tear down the live FEC ctl-reconfig timer (issue #1567), same pattern.
        if let Some(interval) = self.fec_reconfig_interval.borrow_mut().take() {
            drop(interval);
        }
        // Reset speaking state and audio level when encoder stops
        self.is_speaking.store(false, Ordering::Relaxed);
        self.client.set_speaking(false);
        self.client.set_audio_level(0.0);
    }

    pub fn start(&mut self) {
        let user_id = self.client.user_id().clone();
        let client = self.client.clone();
        let device_id = if let Some(mic) = &self.state.selected {
            mic.to_string()
        } else {
            return;
        };

        // Don't start if not enabled - this is the key fix
        if !self.state.is_enabled() {
            log::debug!("Microphone encoder start() called but encoder is not enabled");
            return;
        }

        // The BASE codec (index 0) is the canary for "already running": it is
        // always the first instantiated and last destroyed.
        let base_instantiated = self.codecs[0].is_instantiated();
        if self.state.switching.load(Ordering::Acquire) && base_instantiated {
            self.stop();
        }
        if self.state.is_enabled() && base_instantiated {
            return;
        }
        // FIX 2 (#1398): a genuinely fresh encoder session is starting (past the
        // early-returns above). Clear any stale single-layer congestion bitrate
        // floor to the fail-open sentinel so we begin at the HEALTHY bitrate — a
        // prior audio-only distress cut whose recovery Interval was torn down on
        // mute must NOT pin the new session low (which would also restart the
        // cooldown from zero despite no current distress). `u32::MAX` is the
        // fully-recovered/no-cut state shared with the FIX-D reconnect reset and
        // the recovery state machine, so this does not fight either. The detector
        // re-seeds its windows on its first active tick and re-cuts within one
        // window if the LIVE uplink is actually distressed.
        self.shared_congestion_bitrate_floor
            .store(u32::MAX, Ordering::Relaxed);
        let aes = client.aes();
        let on_error = self.on_error.clone();
        let EncoderState {
            enabled, switching, ..
        } = self.state.clone();

        // Clone atomic values for use in different closures.
        // Audio simulcast layer count (issue #989, Phase 3c → up to 3 in #1082).
        // 1 = single layer (default, byte-identical). N>1 = LOW base (layer 0)
        // plus the higher rungs of AUDIO_SIMULCAST_LAYER_KBPS.
        let n_audio_layers = clamp_audio_layer_count(self.max_layers) as usize;
        log::info!(
            "MicrophoneEncoder: effective audio layers = {}",
            clamp_audio_layer_count(self.max_layers)
        );
        let audio_simulcast = n_audio_layers > 1;

        // Resize the per-layer codec Vec to the effective layer count. Index 0
        // (the base) is preserved (it is the canary `is_instantiated` checks
        // read); higher rungs get fresh empty codecs. Done before the async
        // block so the clones it captures are the right length.
        if self.codecs.len() != n_audio_layers {
            self.codecs
                .resize_with(n_audio_layers, AudioWorkletCodec::default);
        }

        // Per-layer audio output handler builder. `layer_id` is stamped on every
        // emitted packet; each layer owns its own seq counter + RED previous-
        // frame buffer so a receiver decoding ONE audio layer sees a dense
        // sequence. The captured values are cloned per handler so the handlers
        // can coexist. For N=1 only the base (layer 0) handler is built —
        // byte-identical to the legacy path.
        let make_audio_handler = |layer_id: u32| -> Box<dyn FnMut(MessageEvent)> {
            log::info!(
                "Starting Microphone audio encoder (layer {layer_id}) with AnalyserNode VAD"
            );
            let mut sequence_number: u64 = 0;
            let client_for_send = client.clone();
            let user_id = user_id.clone();
            let aes = aes.clone();
            let enabled_for_handler = enabled.clone();
            let enable_fec_for_handler = self.tier_enable_fec.clone();
            // User SEND audio layer-ceiling (perf-panel). Each handler reads it
            // LIVE at publish time and self-gates: a handler for a layer at or
            // above the ceiling count drops its packet (and resets its redundancy
            // buffer) instead of sending. The base (layer 0) is always published
            // because the read-side count floors at 1.
            let user_layer_ceiling = self.shared_user_layer_ceiling.clone();
            // CONGESTION-driven SEND audio layer-ceiling (issue #621). Composed
            // with the user ceiling via `min` in `audio_layer_is_published`. Also
            // read LIVE at publish time, so a self-targeted CONGESTION cut (which
            // stores `1` here) stops the upper layers on the very next frame.
            let congestion_layer_ceiling = self.shared_congestion_layer_ceiling.clone();
            // Buffer for RED-style redundancy: stores the previous frame's
            // encoded data and sequence number so it can be included in the
            // next packet for loss recovery.
            let mut previous_frame: Option<PreviousAudioFrame> = None;

            Box::new(move |chunk: MessageEvent| {
                // Check if encoder should stop
                if !enabled_for_handler.load(Ordering::Acquire) {
                    log::debug!(
                        "Audio handler (layer {layer_id}) stopping: enabled={}",
                        enabled_for_handler.load(Ordering::Acquire)
                    );
                    return;
                }

                // Check if this is an actual audio frame message (not control messages)
                if let Ok(message_type) = js_sys::Reflect::get(&chunk.data(), &"message".into()) {
                    if let Some(msg_str) = message_type.as_string() {
                        if msg_str != "page" {
                            // The worklet's `reconfigOpus` ACK (issue #1398, FIX 2):
                            // posted ONLY when the worklet actually applied ctl 4002
                            // (OPUS_SET_BITRATE) — see encoderWorker.min.js. Log the
                            // applied bitrate (read from the `bitRate` field; default
                            // 0 if absent) so a live single-layer bitrate reconfig is
                            // observable end-to-end on the MAIN thread. Only the base
                            // codec (layer 0) posts to THIS handler, which is enough:
                            // the single-layer case has only layer 0 anyway.
                            if msg_str == "opusReconfigured" {
                                let n = js_sys::Reflect::get(&chunk.data(), &"bitRate".into())
                                    .ok()
                                    .and_then(|v| v.as_f64())
                                    .map(|f| f as i64)
                                    .unwrap_or(0);
                                log::info!(
                                    "MicrophoneEncoder: worklet ACK opusReconfigured bitRate={n}"
                                );
                                return;
                            }
                            // Any other control message (ready, done, flushed), not
                            // an audio frame.
                            log::debug!("Received control message: {msg_str}");
                            return;
                        }
                    }
                }

                let data = js_sys::Reflect::get(&chunk.data(), &"page".into()).unwrap();
                if let Ok(data) = data.dyn_into::<Uint8Array>() {
                    // SEND audio layer-ceiling gate: composed from BOTH the user
                    // perf-panel ceiling and the CONGESTION-driven ceiling (issue
                    // #621), each mapped (u32::MAX = Auto) to a layer COUNT via the
                    // shared sentinel mapper and combined with `min`; the effective
                    // count is floored at 1 so the base layer (layer_id 0) is
                    // ALWAYS published. A layer at or above the effective count is
                    // NOT sent. We DROP this packet entirely (publish-gate) rather
                    // than encode-gate: the Opus encode already ran on the
                    // AudioWorklet thread (cheap, off the main thread — see the
                    // ROLLOUT NOTE on `codecs`), so the win here is the uplink
                    // saving; skipping the encode would require tearing down the
                    // worklet node, which is exactly the restart we are avoiding.
                    // Also reset this layer's redundancy buffer so that, if the
                    // ceiling is later raised (user thumb up, or congestion
                    // recovery), the resumed layer starts a fresh RED chain rather
                    // than carrying a stale previous frame across the gap.
                    if !audio_layer_is_published(
                        layer_id,
                        user_layer_ceiling.load(Ordering::Relaxed),
                        congestion_layer_ceiling.load(Ordering::Relaxed),
                    ) {
                        previous_frame = None;
                        return;
                    }

                    // Decide whether to include redundancy based on the
                    // AUDIO_REDUNDANCY_ENABLED constant and the current tier's
                    // enable_fec flag.
                    let use_redundancy = AUDIO_REDUNDANCY_ENABLED
                        && enable_fec_for_handler.load(Ordering::Relaxed)
                        && previous_frame.is_some();

                    let red_ref = if use_redundancy {
                        previous_frame.as_ref()
                    } else {
                        None
                    };

                    let packet: PacketWrapper = transform_audio_chunk(
                        &data,
                        &user_id,
                        sequence_number,
                        aes.clone(),
                        red_ref,
                        layer_id,
                    );
                    // Phase 2 of WT freeze fix: route audio on its dedicated
                    // persistent QUIC stream so it can never be HOL-blocked by
                    // a stalled video write.
                    client_for_send.send_media_packet(packet, MediaStreamKey::Audio);

                    // Store current frame as the previous frame for the next
                    // iteration's redundancy payload.
                    previous_frame = Some(PreviousAudioFrame {
                        data: data.to_vec(),
                        sequence: sequence_number,
                    });
                    sequence_number += 1;
                } else {
                    log::error!("Received non-MessageEvent: {chunk:?}");
                }
            })
        };
        // Base layer (0) handler — always built (the legacy path for N=1).
        let audio_output_handler = make_audio_handler(0);
        // Higher-layer handlers (indices 1..N) — only in simulcast mode. One
        // per extra rung, lowest first, so `higher_handlers[i]` drives layer
        // `i + 1`. Empty when not simulcasting.
        let higher_handlers: Vec<Box<dyn FnMut(MessageEvent)>> = if audio_simulcast {
            (1..n_audio_layers as u32).map(make_audio_handler).collect()
        } else {
            Vec::new()
        };

        // Clone the codec handles for the async block. `AudioWorkletCodec` is
        // Rc-backed so these share the underlying nodes. `base_codec` is index
        // 0; `higher_codecs` are indices 1..N (parallel to `higher_handlers`).
        // `all_codecs_for_teardown` is a parallel clone kept alive so the
        // monitor loop can destroy every layer on stop (the `for` loop below
        // consumes `higher_codecs` by value).
        let base_codec = self.codecs[0].clone();
        let higher_codecs: Vec<AudioWorkletCodec> = self.codecs[1..].to_vec();
        let all_codecs_for_teardown: Vec<AudioWorkletCodec> = self.codecs.clone();
        let is_speaking_for_vad = self.is_speaking.clone();
        let client_for_vad = client.clone();
        let vad_interval_holder = self.vad_interval.clone();
        let vad_threshold = self.vad_threshold;
        // CONGESTION-recovery state (issue #621), all cloned into the async block.
        // `n_audio_layers` is the configured ladder depth the recovery loop climbs
        // back toward; the congestion ceiling atom is the lever it drives; the
        // holder owns the timer so stop()/disable can tear it down.
        let congestion_ceiling_for_recovery = self.shared_congestion_layer_ceiling.clone();
        let congestion_recovery_holder = self.congestion_recovery_interval.clone();
        let configured_audio_layers = n_audio_layers as u32;
        // Live Opus FEC ctl-reconfig state (issue #1567), cloned into the async
        // block. `fec_reconfig_tier_index` is the shared audio-tier index the
        // camera AQ loop writes; `fec_reconfig_codecs` are the live worklet
        // codec handles (Rc-backed, share the nodes with `self.codecs`) the
        // timer posts `reconfigOpus` to; `fec_reconfig_holder` owns the timer so
        // stop()/disable can tear it down.
        let fec_reconfig_tier_index = self.shared_audio_tier_index.clone();
        let fec_reconfig_codecs: Vec<AudioWorkletCodec> = self.codecs.clone();
        let fec_reconfig_holder = self.fec_reconfig_interval.clone();
        // Single-layer congestion BITRATE-floor state (issue #1398). The bitrate
        // mechanism is gated to single-layer mode (`n_audio_layers == 1`): only
        // then does a single Opus stream with no upper layer to shed need a live
        // bitrate downshift. `fec_reconfig_tier_bitrate` is the camera AQ loop's
        // tier-bitrate atom (defaults to 48000 bps, stays there audio-only);
        // `fec_reconfig_bitrate_floor` is the congestion floor (u32::MAX =
        // fail-open); `fec_camera_active` is the live camera state. The FEC
        // reconfig timer feeds all three to the CAMERA-STATE-AWARE
        // [`effective_audio_bitrate`] (camera-on → tier, camera-off → floor) and
        // re-applies via ctl 4002. The SAME floor atom is driven DOWN by the
        // mic-side uplink-distress detector AND UP by the congestion-recovery climb
        // (both in the recovery timer below), so a step is observed by this timer's
        // change-detection on its next tick (they read the same atom).
        let fec_reconfig_single_layer = n_audio_layers == 1;
        let fec_reconfig_tier_bitrate = self.tier_audio_bitrate.clone();
        let fec_reconfig_bitrate_floor = self.shared_congestion_bitrate_floor.clone();
        let fec_camera_active = self.camera_active.clone();
        // Issue #1611: the FEC reconfig timer needs the camera-exhausted signal
        // for the updated `effective_audio_bitrate` which now takes 4 args.
        let fec_camera_video_exhausted = self.camera_video_exhausted.clone();
        // Bitrate-floor recovery lever for the congestion-recovery timer (#1398),
        // a clone of the SAME atom the FEC reconfig timer reads above.
        let bitrate_floor_for_recovery = self.shared_congestion_bitrate_floor.clone();
        // Mic-side uplink-distress DETECTOR state (issue #1398 + #1611 backstop).
        // The DOWN trigger for the single-layer bitrate floor. Gated to single-layer
        // mode AND the 3-signal backstop gate (issue #1611):
        //   single_layer && (!camera || camera_exhausted) && (!screen || screen_exhausted)
        // On the FIRST tick the gate reopens after being closed, the detector
        // RE-SEEDS its windows to `now` (FIX 1) so it never cashes a stale
        // cross-gap delta. A clone of the SAME floor atom the recovery climb reads,
        // so a detector step-down restarts recovery automatically.
        let detector_single_layer = n_audio_layers == 1;
        let detector_bitrate_floor = self.shared_congestion_bitrate_floor.clone();
        let detector_camera_active = self.camera_active.clone();
        // Issue #1611 backstop signals for the detector gate.
        let detector_camera_video_exhausted = self.camera_video_exhausted.clone();
        let detector_screen_video_exhausted = self.screen_video_exhausted.clone();
        let detector_screen_active = self.screen_sharing_active.clone();
        // Reconnect-reseed flag (issue #1398 reconnect P1): the client sets this
        // true on every (re)connect; the detector tick CONSUMES it (swap-to-false)
        // and forces a window re-seed even though the detector stayed active across
        // the reconnect, so the transport counters bumped by teardown/rebuild are
        // never read as a cross-reconnect distress delta on the fresh session.
        let detector_reconnect_reseed = self.reconnect_reseed.clone();

        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();
            let media_devices = match navigator.media_devices() {
                Ok(md) => md,
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Failed to access media devices: {e:?}"));
                    }
                    return;
                }
            };
            let constraints = MediaStreamConstraints::new();
            let media_info = web_sys::MediaTrackConstraints::new();

            // Always request browser audio processing as "ideal" hints. AEC
            // is what stops a peer's speakers feeding back into their mic;
            // without it, every peer becomes a self-feedback path for the
            // talker. Confirmed in the 2026-05-08 production logs: the mic
            // stream went through the explicit-deviceId branch with none of
            // these flags set, and the user heard themselves via peers'
            // failing AEC. Use plain `true` (ideal) rather than
            // `{ exact: true }` so the browser may downgrade silently on
            // virtual audio devices instead of failing the stream.
            media_info.set_echo_cancellation(&JsValue::TRUE);
            media_info.set_noise_suppression(&JsValue::TRUE);
            media_info.set_auto_gain_control(&JsValue::TRUE);

            // Force exact deviceId match (avoids falling back to the default mic).
            if device_id.is_empty() {
                log::warn!("Microphone device_id is empty, using default constraint");
            } else {
                let exact = js_sys::Object::new();
                js_sys::Reflect::set(
                    &exact,
                    &JsValue::from_str("exact"),
                    &JsValue::from_str(&device_id),
                )
                .unwrap();

                log::info!("MicrophoneEncoder: deviceId.exact = {}", device_id);
                media_info.set_device_id(&exact.into());
            }
            constraints.set_audio(&media_info.into());

            constraints.set_video(&Boolean::from(false));
            let devices_query = match media_devices.get_user_media_with_constraints(&constraints) {
                Ok(p) => p,
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Microphone access failed: {e:?}"));
                    }
                    return;
                }
            };
            let device = match JsFuture::from(devices_query).await {
                Ok(ok) => ok.unchecked_into::<MediaStream>(),
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Failed to get microphone stream: {e:?}"));
                    }
                    return;
                }
            };

            let audio_track = Box::new(
                device
                    .get_audio_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<MediaStreamTrack>(),
            );

            let track_settings = audio_track.get_settings();

            // Sample Rate hasn't been added to the web_sys crate
            // Firefox doesn't report sampleRate in MediaTrackSettings, so we need a fallback
            let input_rate: u32 = match js_sys::Reflect::get(
                &track_settings,
                &JsValue::from_str("sampleRate"),
            ) {
                Ok(v) => match v.as_f64() {
                    Some(f) => f as u32,
                    None => {
                        // Firefox fallback: create a temporary AudioContext to get system sample rate
                        log::info!("sampleRate not in track settings (Firefox), using AudioContext default");
                        match AudioContext::new() {
                            Ok(temp_ctx) => {
                                let rate = temp_ctx.sample_rate() as u32;
                                let _ = temp_ctx.close();
                                rate
                            }
                            Err(e) => {
                                if let Some(cb) = &on_error {
                                    cb.emit(format!(
                                        "Could not determine microphone sample rate: {e:?}"
                                    ));
                                }
                                return;
                            }
                        }
                    }
                },
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Failed reading microphone settings: {e:?}"));
                    }
                    return;
                }
            };

            log::info!("Microphone input sample rate: {input_rate} Hz");

            // Diagnostic: log what the browser actually applied for AEC/NS/AGC
            // (and the other fields we asked for). We request these as "ideal"
            // hints — the browser may silently downgrade depending on driver,
            // OS audio profile, or virtual device. If we hit another self-echo
            // report, we want to be able to confirm from logs whether the
            // browser honored the request, before chasing other suspects.
            {
                let read = |key: &str| -> String {
                    match js_sys::Reflect::get(&track_settings, &JsValue::from_str(key)) {
                        Ok(v) if v.is_undefined() || v.is_null() => "<unset>".to_string(),
                        Ok(v) => {
                            if let Some(b) = v.as_bool() {
                                b.to_string()
                            } else if let Some(f) = v.as_f64() {
                                f.to_string()
                            } else if let Some(s) = v.as_string() {
                                s
                            } else {
                                format!("{v:?}")
                            }
                        }
                        Err(_) => "<error>".to_string(),
                    }
                };
                log::info!(
                    "Microphone applied settings: echoCancellation={}, noiseSuppression={}, autoGainControl={}, sampleRate={}, channelCount={}, deviceId={}",
                    read("echoCancellation"),
                    read("noiseSuppression"),
                    read("autoGainControl"),
                    read("sampleRate"),
                    read("channelCount"),
                    read("deviceId"),
                );
            }

            // Let the browser choose the AudioContext sample rate rather than
            // forcing it to the mic's native rate. Forcing a specific rate can
            // cause the browser to reconfigure the audio device, interrupting
            // microphone streams in other tabs/apps (e.g. Google Meet).
            // The encoder handles resampling from the context rate to Opus's
            // 48 kHz internally via original_sample_rate.
            let context = match AudioContext::new() {
                Ok(ctx) => ctx,
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Failed to create audio context: {e:?}"));
                    }
                    return;
                }
            };
            let context_rate = context.sample_rate() as u32;
            log::info!(
                "Created AudioContext: context rate={context_rate} Hz, mic native rate={input_rate} Hz"
            );

            let analyser = match context.create_analyser() {
                Ok(a) => a,
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Failed to create analyser: {e:?}"));
                    }
                    let _ = context.close();
                    return;
                }
            };
            analyser.set_fft_size(VAD_FFT_SIZE);
            analyser.set_smoothing_time_constant(VAD_SMOOTHING_TIME_CONSTANT);

            let worklet = match base_codec
                .create_node(
                    &context,
                    "/encoderWorker.min.js",
                    "encoder-worklet",
                    AUDIO_CHANNELS,
                )
                .await
            {
                Ok(node) => node,
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Failed to initialize audio encoder: {e:?}"));
                    }
                    let _ = context.close();
                    return;
                }
            };

            let output_handler =
                Closure::wrap(audio_output_handler as Box<dyn FnMut(MessageEvent)>);
            base_codec.set_onmessage(output_handler.as_ref().unchecked_ref());
            output_handler.forget();

            // Use the tier-controlled bitrate (defaults to AUDIO_QUALITY_TIERS[0]),
            // and pass FEC/DTX/loss-% from the initial audio tier. The worklet
            // (encoderWorker.min.js) applies the matching Opus ctl calls, but
            // ONLY at this init send — there is no live reconfig path.
            //
            // What this means at runtime:
            //  - DTX engages today: every tier (including this top tier) sets
            //    enable_dtx=true, so DTX is active for the whole call.
            //  - Inband FEC does NOT engage on a mid-call AQ tier drop: we init
            //    at the healthy top tier (enable_fec=false, packet_loss_perc=0),
            //    and a later tier change only writes shared atomics — it never
            //    re-applies the ctl to the running encoder. Wiring the flag is
            //    the prerequisite; runtime FEC engagement (a live ctl-reconfig
            //    message) is tracked as a follow-up (see #1567). See audio_worklet_codec.rs.
            let initial_tier = &AUDIO_QUALITY_TIERS[0];
            // Base-layer bitrate: in single-layer mode use the tier default
            // (byte-identical to today). In simulcast mode the base layer IS the
            // LOW layer (the relay always forwards it; a congested receiver pulls
            // it), so it inits at the lowest rung AUDIO_SIMULCAST_LAYER_KBPS[0].
            let base_bitrate_bps = if audio_simulcast {
                AUDIO_SIMULCAST_LAYER_KBPS[0] * 1000
            } else {
                initial_tier.bitrate_kbps * 1000
            };
            let _ = base_codec.send_message(&CodecMessages::Init {
                options: Some(EncoderInitOptions {
                    encoder_frame_size: Some(20), // 20ms frames for 50Hz rate
                    original_sample_rate: Some(context_rate),
                    encoder_bit_rate: Some(base_bitrate_bps),
                    encoder_sample_rate: Some(AUDIO_SAMPLE_RATE),
                    encoder_fec: Some(initial_tier.enable_fec),
                    encoder_dtx: Some(initial_tier.enable_dtx),
                    encoder_packet_loss_perc: Some(initial_tier.packet_loss_perc),
                    ..Default::default()
                }),
            });

            let source_node = match context.create_media_stream_source(&device) {
                Ok(s) => s,
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Failed to create media source: {e:?}"));
                    }
                    let _ = context.close();
                    return;
                }
            };
            let gain_node = match context.create_gain() {
                Ok(g) => g,
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Failed to create gain node: {e:?}"));
                    }
                    let _ = context.close();
                    return;
                }
            };
            if let Err(e) = source_node
                .connect_with_audio_node(&gain_node)
                .and_then(|g| g.connect_with_audio_node(&analyser))
                .and_then(|a| a.connect_with_audio_node(&worklet))
            {
                if let Some(cb) = &on_error {
                    cb.emit(format!("Failed to connect audio graph: {e:?}"));
                }
                let _ = context.close();
                return;
            }

            // --- Audio simulcast HIGHER layers (issue #989, Phase 3c → #1082) ---
            // For each rung above the base, build an additional AudioWorkletNode
            // on the SAME context, fed the same captured audio (fanned out from
            // the analyser node), encoding at that rung's
            // AUDIO_SIMULCAST_LAYER_KBPS bitrate and stamping layer_id = index.
            // A per-layer Opus encode is the only way to get a distinct bitrate
            // (the worklet has no dynamic bitrate reconfig). On any per-layer
            // failure we log + skip that layer (the base + lower layers keep
            // working) rather than tearing down audio.
            //
            // `higher_codecs[i]` / `higher_handlers[i]` both correspond to
            // simulcast layer `i + 1` (lowest extra rung first).
            for (i, (codec_n, handler_n)) in
                higher_codecs.into_iter().zip(higher_handlers).enumerate()
            {
                let layer_id = (i + 1) as u32;
                // Per-layer bitrate from the ladder; guard the index defensively
                // (codecs were sized from the same n, so this always hits).
                let layer_kbps = AUDIO_SIMULCAST_LAYER_KBPS
                    .get(layer_id as usize)
                    .copied()
                    .unwrap_or(AUDIO_SIMULCAST_LAYER_KBPS[AUDIO_SIMULCAST_LAYER_KBPS.len() - 1]);
                match codec_n
                    .create_node(
                        &context,
                        "/encoderWorker.min.js",
                        "encoder-worklet",
                        AUDIO_CHANNELS,
                    )
                    .await
                {
                    Ok(worklet_n) => {
                        let output_n = Closure::wrap(handler_n as Box<dyn FnMut(MessageEvent)>);
                        codec_n.set_onmessage(output_n.as_ref().unchecked_ref());
                        output_n.forget();
                        let _ = codec_n.send_message(&CodecMessages::Init {
                            options: Some(EncoderInitOptions {
                                encoder_frame_size: Some(20),
                                original_sample_rate: Some(context_rate),
                                encoder_bit_rate: Some(layer_kbps * 1000),
                                encoder_sample_rate: Some(AUDIO_SAMPLE_RATE),
                                encoder_fec: Some(initial_tier.enable_fec),
                                encoder_dtx: Some(initial_tier.enable_dtx),
                                encoder_packet_loss_perc: Some(initial_tier.packet_loss_perc),
                                ..Default::default()
                            }),
                        });
                        // Fan out the captured audio to this encoder too.
                        if let Err(e) = analyser.connect_with_audio_node(&worklet_n) {
                            log::error!(
                                "Audio simulcast: failed to connect layer {layer_id}, skipping it: {e:?}"
                            );
                            codec_n.destroy();
                        } else {
                            // Match the base codec's started/stopped state.
                            if enabled.load(Ordering::Acquire) {
                                let _ = codec_n.start();
                            }
                            log::info!(
                                "Audio simulcast: layer {layer_id} ({layer_kbps}kbps) active"
                            );
                        }
                    }
                    Err(e) => {
                        log::error!(
                            "Audio simulcast: failed to create layer {layer_id} worklet, skipping it: {e:?}"
                        );
                    }
                }
            }

            let buffer_length = analyser.frequency_bin_count() as usize;
            let data_array = Rc::new(RefCell::new(vec![0.0f32; buffer_length]));

            let enabled_check = enabled.clone();
            let switching_check = switching.clone();
            let data_array_for_interval = data_array.clone();
            let is_speaking_clone = is_speaking_for_vad.clone();
            let client_clone = client_for_vad.clone();

            let prev_audio_level = Rc::new(Cell::new(0.0f32));
            let prev_level_clone = prev_audio_level.clone();

            // LOCAL user Voice Activity Detection (VAD) via AnalyserNode.
            //
            // This runs every 100ms and computes the RMS energy of the
            // microphone's time-domain signal.  The resulting `is_speaking`
            // flag is included in the 1Hz heartbeat so that *remote* peers
            // can show a speaking indicator for this user.
            let vad_interval = Interval::new(VAD_POLL_INTERVAL_MS, move || {
                if !enabled_check.load(Ordering::Acquire) || switching_check.load(Ordering::Acquire)
                {
                    // Reset audio level to zero when mic is disabled/switching
                    let prev_lvl = prev_level_clone.get();
                    if prev_lvl > 0.0 {
                        prev_level_clone.set(0.0);
                        client_clone.set_audio_level(0.0);
                    }
                    return;
                }

                let mut array = data_array_for_interval.borrow_mut();
                analyser.get_float_time_domain_data(&mut array);

                let mut sum = 0.0f32;
                for sample in array.iter() {
                    sum += sample * sample;
                }
                let rms = (sum / array.len() as f32).sqrt();

                let speaking = rms > vad_threshold;

                // Compute normalized intensity using the shared perceptual
                // curve so the host tile shows a smooth, intensity-driven glow.
                let intensity = rms_to_intensity(rms, vad_threshold);

                // Emit audio level when it changes meaningfully.
                let prev_lvl = prev_level_clone.get();
                if (intensity - prev_lvl).abs() > AUDIO_LEVEL_DELTA_THRESHOLD {
                    prev_level_clone.set(intensity);
                    client_clone.set_audio_level(intensity);
                }

                log::trace!("VAD: RMS={:.4}, speaking={}", rms, speaking);

                // Only propagate when the speaking state actually changes to
                // avoid unnecessary callback emissions every 100ms.
                let prev = is_speaking_clone.load(Ordering::Relaxed);
                if speaking != prev {
                    is_speaking_clone.store(speaking, Ordering::Relaxed);
                    client_clone.set_speaking(speaking);
                }
            });

            *vad_interval_holder.borrow_mut() = Some(vad_interval);

            // --- CONGESTION-recovery timer (issue #621) ---
            // Climbs the congestion layer ceiling back up ONE rung per
            // AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS once no new congestion has
            // fired for that window. Runs on the MIC side (not the camera AQ loop)
            // so recovery works even when the camera is OFF (audio-only) — the
            // lifecycle constraint the issue calls out.
            //
            // It learns the congestion timestamp by WATCHING the ceiling atom: the
            // client's CONGESTION dispatch stores `1` (base-only) into it; when this
            // loop observes the ceiling drop below what it last left, it treats that
            // as a fresh cut and (re)starts the cooldown from `now`. This keeps the
            // cut TIMESTAMP entirely mic-side (no extra cross-thread atom) and makes
            // a repeated cut naturally reset the cooldown. The recovery math itself
            // is the pure, host-tested `audio_congestion_recover`.
            let recovery_enabled_check = enabled.clone();
            let recovery_switching_check = switching.clone();
            let recovery_ceiling = congestion_ceiling_for_recovery.clone();
            let recovery_configured = configured_audio_layers;
            // `None` = no active cut. Set to `Some(now)` when a fresh cut is seen.
            let last_congestion_ms: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
            // The ceiling value this loop last observed, to detect a NEW cut
            // (a drop). Starts at the fail-open sentinel (no cut yet).
            let last_seen_ceiling: Rc<Cell<u32>> = Rc::new(Cell::new(u32::MAX));
            // --- Single-layer congestion BITRATE-floor recovery state (#1398) ---
            // Parallel to the ceiling state above but for the bitrate floor atom.
            // Same hysteresis machine (one tier per cooldown), driven by the pure
            // host-tested `audio_bitrate_tick`. Independent cut-timestamp/last-seen
            // cells because the ceiling (multi-layer) and the bitrate floor
            // (single-layer) are cut/recovered separately. This climbs the SAME
            // floor atom the FEC reconfig timer reads, so a recovery step is
            // observed by that timer's change-detection on its next tick.
            let recovery_bitrate_floor = bitrate_floor_for_recovery.clone();
            let bitrate_last_congestion_ms: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
            let bitrate_last_seen: Rc<Cell<u32>> = Rc::new(Cell::new(u32::MAX));
            // --- Mic-side uplink-distress DETECTOR state (issue #1398) ---
            // The DOWN trigger for the single-layer bitrate floor. Mic-OWNED
            // tumbling-window state (NOT consume-once): the process-global
            // transport counters are monotonic and read by multiple consumers, so
            // each consumer keeps its OWN snapshot/window — exactly how the camera
            // AQ loop's WT-saturation / WS-drop / WT-drop blocks each keep
            // independent windows. THREE axes (WT slow-`ready()` saturation, WS
            // send-buffer backpressure, WT unistream DROP), each with its own
            // snapshot + window-start, because their windows differ in width (see
            // AUDIO_UPLINK_*_WINDOW_MS). Seeded from the CURRENT counter values so
            // the first window measures a delta from "now", not from 0 (a
            // long-lived counter must not look like a fresh burst on the first
            // tick). The detector evaluates ONLY while its gate is open —
            // single-layer AND camera OFF (see `audio_detector_gate_open`); on
            // each reopen after being gated/early-returned it RE-SEEDS these windows
            // to `now` (FIX 1) so it never cashes a stale cross-gap delta
            // accumulated while inactive.
            let detector_floor = detector_bitrate_floor.clone();
            let detector_camera = detector_camera_active.clone();
            // Issue #1611: backstop signals, re-cloned for the move closure.
            let detector_cam_exhausted = detector_camera_video_exhausted.clone();
            let detector_scr_exhausted = detector_screen_video_exhausted.clone();
            let detector_scr_active = detector_screen_active.clone();
            // Reconnect-reseed flag (issue #1398 reconnect P1), re-cloned for the
            // move closure. CONSUMED (swap-to-false) each tick the detector
            // evaluates, forcing a window re-seed once per reconnect.
            let detector_reconnect_reseed = detector_reconnect_reseed.clone();
            let det_sat_snapshot: Rc<Cell<u64>> = Rc::new(Cell::new(
                videocall_transport::webtransport::unistream_ready_stall_count(),
            ));
            let det_sat_window_start: Rc<Cell<f64>> = Rc::new(Cell::new(js_sys::Date::now()));
            let det_ws_snapshot: Rc<Cell<u64>> = Rc::new(Cell::new(
                videocall_transport::websocket::websocket_drop_count(),
            ));
            let det_ws_window_start: Rc<Cell<f64>> = Rc::new(Cell::new(js_sys::Date::now()));
            // Third axis (#1398): WT unistream DROP. Seeded from the live drop
            // counter so the first window measures from "now" — exactly like the
            // two axes above and the camera AQ's own WT-drop window.
            let det_wtdrop_snapshot: Rc<Cell<u64>> = Rc::new(Cell::new(
                videocall_transport::webtransport::unistream_drop_count(),
            ));
            let det_wtdrop_window_start: Rc<Cell<f64>> = Rc::new(Cell::new(js_sys::Date::now()));
            // FIX 1: tracks whether the detector EVALUATED on the previous tick.
            // Starts `false` so the very first activation re-seeds (harmless — it
            // just re-anchors to ~now, matching the start() seed). Drives the pure
            // re-seed-on-reactivation decision ([`audio_detector_should_reseed`]).
            let det_was_active: Rc<Cell<bool>> = Rc::new(Cell::new(false));
            // Tick at the dedicated coarse recovery cadence
            // (AUDIO_CONGESTION_RECOVERY_TICK_MS = 1s), NOT the 20 Hz VAD cadence:
            // the cooldown is minutes and the cut takes effect on the next frame
            // via the live publish-gate read, so this timer only governs how
            // promptly recovery NOTICES a cut and how granularly it climbs back.
            // A 1 Hz tick is effectively exact for a minutes-long cooldown and
            // avoids a redundant 20 Hz wakeup on battery-constrained devices.
            let recovery_interval = Interval::new(AUDIO_CONGESTION_RECOVERY_TICK_MS, move || {
                if !recovery_enabled_check.load(Ordering::Acquire)
                    || recovery_switching_check.load(Ordering::Acquire)
                {
                    // Mic muted / connection switching: the detector is NOT
                    // evaluating this tick, so mark it inactive (issue #1398, FIX 1).
                    // On unmute/resume the next active tick sees `was_active == false`
                    // and RE-SEEDS, never cashing in a stale cross-gap delta. (The
                    // recovery ceiling/bitrate climb also early-returns here —
                    // existing behavior, unchanged.)
                    det_was_active.set(false);
                    return;
                }
                let now = js_sys::Date::now();
                // HOLD-AT-FLOOR flag (issue #1398): set TRUE below if the uplink
                // distress detector fires a step-down DECISION this tick. The
                // detector block runs BEFORE the bitrate-recovery block (same
                // closure scope), so the recovery tick observes a decision FIRED
                // this tick and re-anchors its cooldown — even at the emergency
                // floor where the step-down is a clamped no-op (no value change).
                // It stays FALSE on every tick the detector did NOT fire
                // step_down (gate closed, reseed tick, or no-distress windows), so
                // the normal climb resumes once distress stops — the anti-wedge
                // property.
                let mut bitrate_distress_this_tick = false;
                let current = recovery_ceiling.load(Ordering::Relaxed);
                // The whole per-tick transition (new-cut detection, one-rung
                // climb, per-rung cooldown reset, full-recovery clear) is the
                // pure, host-tested `audio_congestion_tick`; this closure only
                // bridges it to the clock and the atom.
                let (next, next_seen, next_cut) = audio_congestion_tick(
                    current,
                    last_seen_ceiling.get(),
                    recovery_configured,
                    now,
                    last_congestion_ms.get(),
                    AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
                );
                if next != current {
                    recovery_ceiling.store(next, Ordering::Relaxed);
                    log::info!(
                        "MicrophoneEncoder: congestion ceiling {} -> {} (recovery)",
                        current,
                        next
                    );
                }
                last_seen_ceiling.set(next_seen);
                last_congestion_ms.set(next_cut);

                // --- Mic-side uplink-distress DETECTOR (issue #1398) ---
                // The DOWN trigger for the single-layer bitrate floor. Runs BEFORE
                // the recovery climb below so a step-down DECISION it makes this
                // tick re-anchors the recovery's cooldown on the SAME tick. The
                // re-anchor is driven by the `bitrate_distress_this_tick` flag set
                // on the firing path (passed into `audio_bitrate_tick` as
                // `distress_active_now`), NOT by an observed value change — so it
                // holds the cooldown even at the EMERGENCY floor (8 kbps), where
                // the step-down is a clamped no-op and the floor value does not
                // change. This is the desired "ongoing distress holds the floor";
                // off the floor a real value decrease still re-anchors via the
                // recovery tick's `current < last_seen` path (same result).
                // Gated ([`audio_detector_gate_open`]) to:
                //   * single-layer mode — the floor only matters when there is one
                //     Opus stream with no upper layer to shed; multi-layer uses the
                //     layer-ceiling lever (#621, driven elsewhere).
                //   * the CAMERA being OFF (audio-only). While the camera is on its
                //     AQ loop self-sheds from the SAME transport counters and drives
                //     the audio tier, so a mic-side write here would COMPOUND — the
                //     gate stays CLOSED. The screen encoder is NOT a gate term: it
                //     writes no audio-tier atom, so a screen-sharing publisher
                //     relies on this detector for its only audio downshift.
                // RE-SEED ON (RE)ACTIVATION (FIX 1): on the FIRST tick the gate is
                // open after being closed (camera on, multi-layer) or after an
                // early-return (mic muted, switching) — all leave
                // `det_was_active == false` — re-anchor ALL THREE axes to `now` and
                // SKIP the step-down this tick. The windows were NOT rolled while
                // inactive but the global counters kept climbing, so the delta over
                // a too-long `elapsed` would otherwise be a spurious immediate cut.
                // Re-seeding measures distress from now forward. The windowed
                // decision (steady-state path) is the pure, host-tested
                // `audio_uplink_step_down_decision`. On a non-WT transport the WT
                // counters stay flat (and vice versa for WS), so the unused axes are
                // true no-ops.
                let detector_should_be_active = audio_detector_gate_open(
                    detector_single_layer,
                    detector_camera.load(Ordering::Acquire),
                    detector_cam_exhausted.load(Ordering::Acquire),
                    detector_scr_active.load(Ordering::Acquire),
                    detector_scr_exhausted.load(Ordering::Acquire),
                );
                if detector_should_be_active {
                    // Reconnect-reseed (issue #1398 reconnect P1): CONSUME the
                    // client's reconnect flag exactly once per reconnect via an
                    // atomic swap-to-false. When it was set, force a window re-seed
                    // this tick even though the detector stayed continuously active
                    // (camera off + single-layer across the reconnect → the gate
                    // never closed and `det_was_active` stayed `true`, so the
                    // `!was_active` path alone would MISS it). Swap (not a plain
                    // load) so a second active tick does not re-seed again.
                    let force_reseed = detector_reconnect_reseed.swap(false, Ordering::AcqRel);
                    let reseeding = audio_detector_should_reseed(
                        detector_should_be_active,
                        det_was_active.get(),
                        force_reseed,
                    );
                    if reseeding {
                        // First tick of a (re)activation (incl. process start):
                        // re-anchor ALL THREE axes to `now` and DO NOT evaluate the
                        // step-down this tick — measure distress from now forward.
                        // H2: the WT-DROP axis MUST reseed here too; if it did not,
                        // the first post-gap tick would compute `current -
                        // stale_snapshot` over a too-long `elapsed` and could cash a
                        // spurious cross-gap cut on the drop counter (which kept
                        // climbing while the detector was gated/early-returned).
                        det_sat_snapshot
                            .set(videocall_transport::webtransport::unistream_ready_stall_count());
                        det_sat_window_start.set(now);
                        det_ws_snapshot.set(videocall_transport::websocket::websocket_drop_count());
                        det_ws_window_start.set(now);
                        det_wtdrop_snapshot
                            .set(videocall_transport::webtransport::unistream_drop_count());
                        det_wtdrop_window_start.set(now);
                    } else {
                        let sat_now =
                            videocall_transport::webtransport::unistream_ready_stall_count();
                        let ws_now = videocall_transport::websocket::websocket_drop_count();
                        let wtdrop_now = videocall_transport::webtransport::unistream_drop_count();
                        let decision = audio_uplink_step_down_decision(
                            AudioUplinkAxisInput {
                                current: sat_now,
                                snapshot: det_sat_snapshot.get(),
                                elapsed_ms: now - det_sat_window_start.get(),
                            },
                            AudioUplinkAxisInput {
                                current: ws_now,
                                snapshot: det_ws_snapshot.get(),
                                elapsed_ms: now - det_ws_window_start.get(),
                            },
                            AudioUplinkAxisInput {
                                current: wtdrop_now,
                                snapshot: det_wtdrop_snapshot.get(),
                                elapsed_ms: now - det_wtdrop_window_start.get(),
                            },
                        );
                        if decision.step_down {
                            // A step-down DECISION fired: distress is ongoing this
                            // tick. Flag it so the bitrate-recovery block below
                            // re-anchors its cooldown REGARDLESS of whether the
                            // floor VALUE changes — the load-bearing case is the
                            // emergency floor (8 kbps), where the step is a
                            // clamped no-op and the value-decrease re-anchor would
                            // NOT fire, so recovery would otherwise climb under
                            // sustained distress (issue #1398 hold-at-floor fix).
                            bitrate_distress_this_tick = true;
                            // Step the floor DOWN one tier via the SINGLE source of
                            // truth. Log only on a real change; a step at the
                            // emergency floor is a clamped no-op.
                            let prev_floor = detector_floor.load(Ordering::Relaxed);
                            let next_floor = audio_congestion_bitrate_step_down(prev_floor);
                            if next_floor != prev_floor {
                                detector_floor.store(next_floor, Ordering::Relaxed);
                                log::warn!(
                                    "MicrophoneEncoder: audio uplink distress detected \
                                     (audio-only, single-layer); stepping Opus bitrate floor \
                                     {prev_floor} -> {next_floor} bps (#1398)"
                                );
                            }
                        }
                        // Roll each axis's window independently when its window closed.
                        if decision.roll_sat {
                            det_sat_snapshot.set(decision.new_sat_snapshot);
                            det_sat_window_start.set(now);
                        }
                        if decision.roll_ws {
                            det_ws_snapshot.set(decision.new_ws_snapshot);
                            det_ws_window_start.set(now);
                        }
                        if decision.roll_wtdrop {
                            det_wtdrop_snapshot.set(decision.new_wtdrop_snapshot);
                            det_wtdrop_window_start.set(now);
                        }
                    }
                    det_was_active.set(true);
                } else {
                    // Gate closed for ANY reason: the camera is on with video NOT
                    // exhausted (its AQ loop is the active audio authority), screen
                    // is on with video NOT exhausted, or multi-layer. The `else`
                    // branch handles ALL gate-closed reasons UNIFORMLY. On the
                    // CLOSING edge specifically (the detector evaluated last tick,
                    // `was_active == true`, and is now gated) CLEAR the bitrate floor
                    // back to the fail-open sentinel exactly once (FIX C, Codex P1).
                    // This covers the camera-recovers path: when camera video steps
                    // back up from the floor (exhausted → not-exhausted), the gate
                    // closes and clears any mic floor the detector cut earlier, so the
                    // effective bitrate hands cleanly back to the camera AQ tier with
                    // no stale mic-side floor lingering. Clearing to `u32::MAX` RAISES
                    // the floor; the recovery state machine reads `u32::MAX` as
                    // fully-recovered and clears the cut memory — no misfire.
                    if det_was_active.get() {
                        detector_floor.store(u32::MAX, Ordering::Relaxed);
                        log::info!(
                            "MicrophoneEncoder: distress-detector gate closed (video can shed \
                             again — video AQ now governs); clearing single-layer audio \
                             bitrate floor to fail-open (#1398 FIX C / #1611)"
                        );
                    }
                    // Mark inactive so the next reopen re-seeds (FIX 1).
                    det_was_active.set(false);
                }

                // --- Bitrate-floor recovery (issue #1398) ---
                // Climb the single-layer congestion bitrate floor back up ONE
                // tier per cooldown, mirroring the ceiling climb above. Reads/
                // writes the SAME floor atom the FEC reconfig timer reads, so the
                // climb is picked up by that timer's effective-bitrate select on its
                // next tick (it governs only while the camera is off). Runs
                // regardless of camera state (this timer is mic-side), which is what
                // makes audio-only recovery work. In multi-layer mode the
                // floor is never cut (the dispatch still steps it, but the FEC
                // timer ignores it), so this is a harmless no-op there.
                let bcurrent = recovery_bitrate_floor.load(Ordering::Relaxed);
                let (bnext, bnext_seen, bnext_cut) = audio_bitrate_tick(
                    bcurrent,
                    bitrate_last_seen.get(),
                    now,
                    bitrate_last_congestion_ms.get(),
                    AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
                    // TRUE iff the detector fired a step-down DECISION this tick
                    // (set in the detector block above, same closure scope). Holds
                    // the floor under sustained distress; FALSE on non-firing ticks
                    // (gate closed, reseed, or no-distress) so the climb resumes.
                    bitrate_distress_this_tick,
                );
                if bnext != bcurrent {
                    recovery_bitrate_floor.store(bnext, Ordering::Relaxed);
                    log::info!(
                        "MicrophoneEncoder: congestion bitrate floor {} -> {} bps (recovery)",
                        bcurrent,
                        bnext
                    );
                }
                bitrate_last_seen.set(bnext_seen);
                bitrate_last_congestion_ms.set(bnext_cut);
            });
            *congestion_recovery_holder.borrow_mut() = Some(recovery_interval);

            // --- Live Opus FEC ctl-reconfig timer (issue #1567) ---
            // Makes inband FEC actually ENGAGE on a mid-call AQ audio-tier drop
            // (and DISENGAGE on recovery) WITHOUT an encoder teardown. The mic
            // inits at the healthy top tier (FEC off); a later tier change only
            // wrote shared atomics, so the live encoder never re-applied the
            // Opus ctl. This 1 Hz timer reads the shared audio-tier index, maps
            // it to `AUDIO_QUALITY_TIERS[idx]` to recover `(enable_fec,
            // packet_loss_perc)`, and — ONLY when that pair changed since the
            // last reconfig (see `audio_fec_reconfig_change`) — posts a
            // `reconfigOpus` message to every live worklet encoder. The worklet
            // calls OPUS_SET_INBAND_FEC (4012) + OPUS_SET_PACKET_LOSS_PERC
            // (4014) on the RUNNING OggOpusEncoder. DTX is untouched (every tier
            // inits with DTX on). This is the INBAND-FEC ctl, separate from the
            // application-level RED path (`AUDIO_REDUNDANCY_ENABLED`).
            //
            // Cadence/debounce: a 1 Hz tick rate-limits reconfigs to at most one
            // per second so a flapping tier cannot flood the worklet; the change
            // check suppresses ticks where the tier is stable, so a steady call
            // sends ZERO reconfigs. `last_sent = None` means the encoder is at
            // its INIT state (top tier, FEC off), so the first observation while
            // still healthy is correctly coalesced to no message.
            let fec_enabled_check = enabled.clone();
            let fec_switching_check = switching.clone();
            let fec_tier_index = fec_reconfig_tier_index;
            let fec_codecs = fec_reconfig_codecs;
            // Single-layer congestion-bitrate state (issue #1398). In single-layer
            // mode the reconfig key includes the CAMERA-STATE-AWARE effective
            // bitrate (camera-on → tier, camera-off → floor), and the init baseline
            // bitrate is the top tier (the bitrate the base encoder inits at). In
            // multi-layer mode the bitrate component is pinned to `None` so the key
            // + emitted message reduce EXACTLY to the pre-#1398 (fec, loss%)-only
            // path (no bitrate, no behaviour change).
            let fec_single_layer = fec_reconfig_single_layer;
            let fec_tier_bitrate = fec_reconfig_tier_bitrate;
            let fec_bitrate_floor = fec_reconfig_bitrate_floor;
            let fec_camera = fec_camera_active;
            // Issue #1611: fec_camera_video_exhausted is already the Arc, captured directly
            let fec_init_bitrate: Option<u32> = if fec_single_layer {
                // Top-tier bitrate = the bitrate the base encoder inits at while
                // healthy (no floor cut), so the first healthy observation is
                // suppressed and the first real downshift emits.
                Some(AUDIO_QUALITY_TIERS[0].bitrate_kbps * 1000)
            } else {
                None
            };
            // `None` = nothing re-applied beyond the encoder's init state.
            let fec_last_sent: Rc<Cell<Option<AudioReconfigKey>>> = Rc::new(Cell::new(None));
            let fec_reconfig_interval = Interval::new(AUDIO_FEC_RECONFIG_TICK_MS, move || {
                if !fec_enabled_check.load(Ordering::Acquire)
                    || fec_switching_check.load(Ordering::Acquire)
                {
                    return;
                }
                // Map the live tier index to the static tier table; clamp
                // defensively so a stale/out-of-range index can never panic.
                let idx = (fec_tier_index.load(Ordering::Relaxed) as usize)
                    .min(AUDIO_QUALITY_TIERS.len() - 1);
                let tier = &AUDIO_QUALITY_TIERS[idx];
                // Bitrate component (issue #1398): ONLY in single-layer mode. The
                // CAMERA-STATE-AWARE select of the tier-driven bitrate (camera AQ
                // loop) and the congestion floor — camera-ON uses the tier;
                // camera-OFF uses the floor when cut, else the healthy 48000
                // top-tier default (ignoring the stale tier). `None` in multi-layer
                // mode → no bitrate in the reconfig.
                let bit_rate: Option<u32> = if fec_single_layer {
                    Some(effective_audio_bitrate(
                        fec_tier_bitrate.load(Ordering::Relaxed),
                        fec_bitrate_floor.load(Ordering::Relaxed),
                        fec_camera.load(Ordering::Acquire),
                        fec_camera_video_exhausted.load(Ordering::Acquire),
                    ))
                } else {
                    None
                };
                let current = (tier.enable_fec, tier.packet_loss_perc, bit_rate);
                // Pure, host-tested change-detection over the (fec, loss%,
                // bitrate) tuple: emit only on a real transition; coalesce an
                // unchanged key to no message.
                if let Some((fec, packet_loss_perc, bit_rate)) =
                    audio_reconfig_change(current, fec_last_sent.get(), fec_init_bitrate)
                {
                    // Re-apply to EVERY live layer's encoder (base + any
                    // simulcast rungs) so all stay in lockstep. A not-yet/no-
                    // longer-instantiated codec is a safe no-op: send_message
                    // returns Err (no port) and the worklet's `reconfigOpus`
                    // case is inside its `if(this.encoder)` guard.
                    for codec in &fec_codecs {
                        let _ = codec.send_message(
                            &CodecMessages::<EncoderInitOptions>::ReconfigOpus {
                                fec,
                                packet_loss_perc,
                                bit_rate,
                            },
                        );
                    }
                    fec_last_sent.set(Some((fec, packet_loss_perc, bit_rate)));
                    log::info!(
                        "MicrophoneEncoder: live Opus reconfig applied (tier {idx} '{}'): fec={fec}, packet_loss_perc={packet_loss_perc}, bit_rate={bit_rate:?}",
                        tier.label,
                    );
                }
            });
            *fec_reconfig_holder.borrow_mut() = Some(fec_reconfig_interval);

            // Monitor for stop conditions and clean up when needed
            let check_interval = VAD_POLL_INTERVAL_MS as i32; // Check every VAD_POLL_INTERVAL_MS
            let enabled_check_monitor = enabled.clone();
            let switching_check_monitor = switching.clone();
            loop {
                // Wait for the check interval
                let delay_promise = js_sys::Promise::new(&mut |resolve, _| {
                    web_sys::window()
                        .unwrap()
                        .set_timeout_with_callback_and_timeout_and_arguments_0(
                            &resolve,
                            check_interval,
                        )
                        .unwrap();
                });
                let _ = wasm_bindgen_futures::JsFuture::from(delay_promise).await;

                // Check if we should stop
                if !enabled_check_monitor.load(Ordering::Acquire)
                    || switching_check_monitor.load(Ordering::Acquire)
                {
                    log::info!("Stopping Microphone audio encoder");
                    switching_check_monitor.store(false, Ordering::Release);

                    is_speaking_for_vad.store(false, Ordering::Relaxed);
                    client_for_vad.set_speaking(false);
                    client_for_vad.set_audio_level(0.0);

                    if let Some(interval) = vad_interval_holder.borrow_mut().take() {
                        drop(interval);
                    }
                    // Tear down the congestion-recovery timer too (issue #621).
                    if let Some(interval) = congestion_recovery_holder.borrow_mut().take() {
                        drop(interval);
                    }
                    // Tear down the live FEC ctl-reconfig timer too (issue #1567).
                    if let Some(interval) = fec_reconfig_holder.borrow_mut().take() {
                        drop(interval);
                    }

                    // Stop the media track
                    audio_track.stop();

                    // Close the AudioContext
                    if let Err(e) = context.close() {
                        log::error!("Error closing AudioContext: {e:?}");
                    }

                    // Destroy every layer's codec (context.close() above already
                    // tears down the attached worklet nodes; this releases the
                    // codecs' own state for each simulcast layer).
                    for codec in &all_codecs_for_teardown {
                        codec.destroy();
                    }

                    log::info!("Microphone audio encoder stopped and cleaned up");
                    break;
                }
            }
        });
    }
}

/// Pure host tests for the audio simulcast layer-count clamp (issue #989,
/// Phase 3c). No browser needed.
#[cfg(test)]
mod layer_count_tests {
    use super::{
        audio_bitrate_recover, audio_bitrate_tick, audio_congestion_bitrate_step_down,
        audio_congestion_recover, audio_congestion_tick, audio_detector_gate_open,
        audio_detector_should_reseed, audio_fec_reconfig_change, audio_layer_is_published,
        audio_reconfig_change, audio_uplink_step_down_decision, clamp_audio_layer_count,
        effective_audio_bitrate, AudioUplinkAxisInput, AUDIO_SIMULCAST_LAYER_KBPS,
        AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS,
    };
    use crate::adaptive_quality_constants::{
        AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS, AUDIO_QUALITY_TIERS,
        AUDIO_UPLINK_SATURATION_STALL_THRESHOLD, AUDIO_UPLINK_SATURATION_WINDOW_MS,
        AUDIO_UPLINK_WS_DROP_THRESHOLD, AUDIO_UPLINK_WS_WINDOW_MS, AUDIO_UPLINK_WT_DROP_THRESHOLD,
        AUDIO_UPLINK_WT_DROP_WINDOW_MS, WS_SELF_CONGESTION_DROP_THRESHOLD,
        WS_SELF_CONGESTION_WINDOW_MS, WT_SATURATION_STALL_THRESHOLD, WT_SATURATION_WINDOW_MS,
        WT_SELF_CONGESTION_DROP_THRESHOLD, WT_SELF_CONGESTION_WINDOW_MS,
    };

    // Tier bitrates in bps, derived from the table (single source of truth) so
    // the tests reference the SAME values the production code does.
    fn tier_bps(idx: usize) -> u32 {
        AUDIO_QUALITY_TIERS[idx].bitrate_kbps * 1000
    }

    #[test]
    fn clamp_audio_layer_count_treats_zero_and_one_as_one() {
        // 0 and 1 → single layer (feature off, byte-identical mic path).
        assert_eq!(clamp_audio_layer_count(0), 1);
        assert_eq!(clamp_audio_layer_count(1), 1);
    }

    #[test]
    fn clamp_audio_layer_count_caps_at_three() {
        // Audio ladder is shallow but now 3 rungs (issue #1082).
        assert_eq!(clamp_audio_layer_count(2), 2);
        assert_eq!(clamp_audio_layer_count(3), 3);
        assert_eq!(
            clamp_audio_layer_count(4),
            AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS
        );
        assert_eq!(
            clamp_audio_layer_count(99),
            AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS
        );
        assert_eq!(AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS, 3);
    }

    #[test]
    fn audio_ladder_is_three_rungs_low_mid_high() {
        // The publisher ladder is the single source of truth for the cap and is
        // ordered lowest→highest (issue #1082; retuned lighter in #1768).
        assert_eq!(AUDIO_SIMULCAST_LAYER_KBPS, &[12, 24, 48]);
        assert_eq!(
            AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS as usize,
            AUDIO_SIMULCAST_LAYER_KBPS.len()
        );
        // Strictly ascending bitrate per rung.
        for w in AUDIO_SIMULCAST_LAYER_KBPS.windows(2) {
            assert!(
                w[1] > w[0],
                "audio layer bitrates must ascend: {AUDIO_SIMULCAST_LAYER_KBPS:?}"
            );
        }
    }

    #[test]
    fn audio_publish_gate_respects_user_ceiling() {
        // Ceiling count 2 (raw atomic value 2): layers 0 and 1 publish, layer 2 is
        // gated off. This is the runtime publish-gate the perf-panel drives. The
        // congestion ceiling is fail-open (u32::MAX) here so ONLY the user ceiling
        // is exercised.
        assert!(
            audio_layer_is_published(0, 2, u32::MAX),
            "base always under any ceiling"
        );
        assert!(
            audio_layer_is_published(1, 2, u32::MAX),
            "layer 1 within ceiling 2"
        );
        assert!(
            !audio_layer_is_published(2, 2, u32::MAX),
            "layer 2 gated by ceiling 2"
        );
        // Ceiling count 1 → only the base publishes.
        assert!(audio_layer_is_published(0, 1, u32::MAX));
        assert!(
            !audio_layer_is_published(1, 1, u32::MAX),
            "layer 1 gated by ceiling 1"
        );
    }

    #[test]
    fn audio_publish_gate_always_publishes_base_even_at_zero_ceiling() {
        // A degenerate ceiling of 0 must still publish the base layer (the count
        // floors at 1) — the base-present invariant, mirroring video/screen.
        assert!(
            audio_layer_is_published(0, 0, u32::MAX),
            "base layer must publish even at a 0 ceiling"
        );
        assert!(
            !audio_layer_is_published(1, 0, u32::MAX),
            "no higher layer at ceiling 0"
        );
    }

    #[test]
    fn audio_publish_gate_auto_sentinel_publishes_all() {
        // BOTH ceilings at u32::MAX (Auto / no cap) map to the usize::MAX fail-open
        // count, so EVERY layer publishes — the default, byte-identical to the
        // pre-control behaviour.
        for layer_id in 0u32..=2 {
            assert!(
                audio_layer_is_published(layer_id, u32::MAX, u32::MAX),
                "layer {layer_id} must publish under the Auto sentinels"
            );
        }
    }

    #[test]
    fn audio_publish_gate_congestion_ceiling_cuts_to_base() {
        // Issue #621: with the user ceiling fail-open, a congestion ceiling of 1
        // (the post-cut value) gates EVERY upper layer off while the base stays
        // published — the aggressive cut.
        assert!(
            audio_layer_is_published(0, u32::MAX, 1),
            "base always published under a congestion cut"
        );
        assert!(
            !audio_layer_is_published(1, u32::MAX, 1),
            "layer 1 gated by congestion ceiling 1"
        );
        assert!(
            !audio_layer_is_published(2, u32::MAX, 1),
            "layer 2 gated by congestion ceiling 1"
        );
    }

    #[test]
    fn audio_publish_gate_takes_min_of_both_ceilings() {
        // Issue #621: the EFFECTIVE ceiling is min(user, congestion). The tighter
        // one wins regardless of which it is.
        // user=2 tighter than congestion=u32::MAX → layer 2 gated.
        assert!(audio_layer_is_published(1, 2, u32::MAX));
        assert!(!audio_layer_is_published(2, 2, u32::MAX));
        // congestion=2 tighter than user=u32::MAX → layer 2 gated.
        assert!(audio_layer_is_published(1, u32::MAX, 2));
        assert!(!audio_layer_is_published(2, u32::MAX, 2));
        // Both set: min(3, 2) = 2 → layer 2 gated even though user allows it.
        assert!(audio_layer_is_published(1, 3, 2));
        assert!(!audio_layer_is_published(2, 3, 2));
    }

    #[test]
    fn audio_congestion_recover_holds_during_cooldown() {
        // Issue #621: right after a cut (ceiling=1), the recovery loop must HOLD
        // at base-only until a full cooldown has elapsed — no early climb.
        let cut_at = 1000.0;
        let (next, done) = audio_congestion_recover(
            1,                                                    // current = base-only (post-cut)
            3,                                                    // configured 3-rung ladder
            cut_at + AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS - 1.0, // 1ms before cooldown
            Some(cut_at),
            AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
        );
        assert_eq!(next, 1, "must hold at base during cooldown");
        assert!(!done, "not fully recovered while still cut");
    }

    #[test]
    fn audio_congestion_recover_climbs_one_rung_per_cooldown() {
        // After exactly one cooldown, climb base(1) → 2 (one rung), NOT straight to
        // full. This is the anti-thrash hysteresis the issue requires.
        let cut_at = 1000.0;
        let (next, done) = audio_congestion_recover(
            1,
            3,
            cut_at + AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
            Some(cut_at),
            AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
        );
        assert_eq!(next, 2, "climb exactly one rung after one cooldown");
        assert!(!done, "still one rung below full");
    }

    #[test]
    fn audio_congestion_recover_final_rung_returns_fail_open() {
        // Climbing the LAST rung (2 → 3 on a 3-rung ladder) collapses to the
        // fail-open sentinel and signals fully-recovered, so the loop stops and the
        // gate is byte-identical to no-cap.
        let cut_at = 1000.0;
        let (next, done) = audio_congestion_recover(
            2, // one below full
            3,
            cut_at + AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
            Some(cut_at),
            AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
        );
        assert_eq!(next, u32::MAX, "final climb returns the fail-open sentinel");
        assert!(done, "fully recovered after the last rung");
    }

    #[test]
    fn audio_congestion_recover_no_cut_is_fail_open() {
        // No active cut (last_congestion_ms == None) → fail-open, done. The loop
        // must not invent a cap out of nowhere.
        let (next, done) = audio_congestion_recover(
            u32::MAX,
            3,
            123_456.0,
            None,
            AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
        );
        assert_eq!(next, u32::MAX);
        assert!(done);
    }

    #[test]
    fn audio_congestion_recover_single_layer_ladder_is_noop() {
        // KNOWN GAP (#621): a 1-layer ladder has no upper rung to restore, so even
        // a cut + elapsed cooldown stays fully-recovered at the sentinel (the base
        // is always published). Documents the single-encoder no-op.
        let cut_at = 1000.0;
        let (next, done) = audio_congestion_recover(
            1,
            1, // single-layer ladder
            cut_at + AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS * 10.0,
            Some(cut_at),
            AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
        );
        assert_eq!(
            next,
            u32::MAX,
            "single-layer recovery is a no-op (fail-open)"
        );
        assert!(done);
    }

    #[test]
    fn audio_congestion_tick_spaces_rungs_one_cooldown_apart() {
        // Issue #621 hysteresis: after a cut to base, recovery must climb back
        // EXACTLY one rung per cooldown — NOT all remaining rungs on consecutive
        // ticks once the first cooldown elapses. This drives the FULL per-tick
        // state machine (`audio_congestion_tick`), the integration the single-call
        // `audio_congestion_recover_*` tests cannot cover, and is the regression
        // guard for the per-rung cooldown re-anchor.
        let cfg = 3;
        let cd = AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS;
        let tick = 100.0; // VAD cadence
        let t0 = 1_000.0;

        // First tick after the client cut the ceiling to base (1): the decrease
        // (1 < last_seen=u32::MAX) is detected and the cooldown is anchored.
        let (c, ls, cut) = audio_congestion_tick(1, u32::MAX, cfg, t0, None, cd);
        assert_eq!(c, 1, "held at base right after the cut");
        assert_eq!(cut, Some(t0), "cooldown anchored at the cut time");

        // Just before the first cooldown elapses: still base.
        let (c, ls, cut) = audio_congestion_tick(c, ls, cfg, t0 + cd - tick, cut, cd);
        assert_eq!(c, 1, "holds base until a full cooldown has passed");

        // First cooldown elapsed: climb exactly one rung (NOT straight to full).
        let (c, ls, cut) = audio_congestion_tick(c, ls, cfg, t0 + cd, cut, cd);
        assert_eq!(c, 2, "climbs exactly one rung after one cooldown");
        let climb2_at = t0 + cd;
        assert_eq!(
            cut,
            Some(climb2_at),
            "per-rung cooldown re-anchored at the climb"
        );

        // THE anti-regression assertion: the very next tick must HOLD at 2.
        // Without the per-rung re-anchor, `cut` would still be `Some(t0)` here and
        // this tick (now - t0 >= cd) would jump straight to full.
        let (c, ls, cut) = audio_congestion_tick(c, ls, cfg, climb2_at + tick, cut, cd);
        assert_eq!(
            c, 2,
            "must NOT climb a second rung on the immediate next tick"
        );

        // Just before the second cooldown elapses: still rung 2.
        let (c, ls, cut) = audio_congestion_tick(c, ls, cfg, climb2_at + cd - tick, cut, cd);
        assert_eq!(c, 2, "holds rung 2 for a full second cooldown");

        // Second cooldown elapsed: final rung → fully recovered (fail-open).
        let (c, _ls, cut) = audio_congestion_tick(c, ls, cfg, climb2_at + cd, cut, cd);
        assert_eq!(c, u32::MAX, "fully recovered after the second cooldown");
        assert_eq!(cut, None, "cut memory cleared on full recovery");
    }

    // --- Live Opus FEC ctl-reconfig change-detection (issue #1567) ---
    //
    // These pin the mutation-meaningful core of the runtime-engagement fix: the
    // "only post a reconfigOpus when the tier's (fec, loss%) actually changed"
    // debounce that the 1 Hz mic timer relies on. The actual Opus ctl
    // re-application is browser/worklet runtime (validated by the serde-shape
    // test in `audio_worklet_codec.rs` + manual/codec validation — see #619);
    // these cover the change/suppress decision that gates whether a message is
    // sent at all.

    #[test]
    fn fec_reconfig_suppresses_at_init_state_while_healthy() {
        // First observation with last_sent=None and the encoder at its init
        // (top-tier) state (FEC off, 0% loss) must NOT send a reconfig — the
        // live encoder already inited there, so a startup message would be
        // redundant spam.
        assert_eq!(audio_fec_reconfig_change((false, 0), None), None);
    }

    #[test]
    fn fec_reconfig_sends_on_drop_to_fec_tier() {
        // The production scenario: start healthy (last_sent=None ≡ top tier),
        // then the AQ audio tier drops to a FEC tier. This MUST emit so inband
        // FEC actually engages on the live encoder.
        assert_eq!(
            audio_fec_reconfig_change((true, 10), None),
            Some((true, 10)),
            "drop to a FEC tier must engage FEC at runtime"
        );
    }

    #[test]
    fn fec_reconfig_suppresses_unchanged_tier() {
        // A tier that re-evaluates to the SAME (fec, loss%) between ticks must be
        // coalesced to no message — this is the anti-spam debounce. Mutating the
        // helper to "always Some" would fail this; mutating it to "always None"
        // would fail `sends_on_drop`/`toggles_off` — so the pair pins both arms.
        assert_eq!(
            audio_fec_reconfig_change((true, 10), Some((true, 10))),
            None
        );
        assert_eq!(
            audio_fec_reconfig_change((false, 0), Some((false, 0))),
            None
        );
    }

    #[test]
    fn fec_reconfig_toggles_off_on_recovery() {
        // Recovery: tier climbs from a FEC tier back to the healthy top tier.
        // FEC must be turned back OFF on the live encoder (not left stuck on).
        assert_eq!(
            audio_fec_reconfig_change((false, 0), Some((true, 10))),
            Some((false, 0)),
            "recovery to the top tier must disengage FEC"
        );
    }

    #[test]
    fn fec_reconfig_sends_when_only_loss_perc_changes() {
        // Stepping between two FEC tiers keeps fec=true but changes the loss
        // hint (e.g. 10% → 15%); libopus scales FEC aggressiveness off this, so
        // a loss-only change must still re-apply.
        assert_eq!(
            audio_fec_reconfig_change((true, 15), Some((true, 10))),
            Some((true, 15)),
            "a loss-% change at the same FEC state must re-apply"
        );
    }

    #[test]
    fn fec_reconfig_full_lifecycle_drop_then_recover() {
        // End-to-end debounce trace over the real tier table, threading
        // last_sent exactly as the timer does: healthy → FEC tier (engage) →
        // same tier (suppress) → top tier (disengage) → top tier (suppress).
        let top = (
            AUDIO_QUALITY_TIERS[0].enable_fec,
            AUDIO_QUALITY_TIERS[0].packet_loss_perc,
        );
        let fec_tier = (
            AUDIO_QUALITY_TIERS[1].enable_fec,
            AUDIO_QUALITY_TIERS[1].packet_loss_perc,
        );
        assert_eq!(top, (false, 0), "table sanity: top tier is FEC-off");
        assert!(fec_tier.0, "table sanity: tier 1 enables FEC");

        let mut last_sent: Option<(bool, u32)> = None;

        // Healthy at init: suppress.
        assert_eq!(audio_fec_reconfig_change(top, last_sent), None);

        // Drop to the FEC tier: engage.
        let sent = audio_fec_reconfig_change(fec_tier, last_sent);
        assert_eq!(sent, Some(fec_tier));
        last_sent = sent;

        // Tier stable: suppress (no flap spam).
        assert_eq!(audio_fec_reconfig_change(fec_tier, last_sent), None);

        // Recover to the top tier: disengage.
        let sent = audio_fec_reconfig_change(top, last_sent);
        assert_eq!(sent, Some(top));
        last_sent = sent;

        // Healthy stable: suppress.
        assert_eq!(audio_fec_reconfig_change(top, last_sent), None);
    }

    // ======================================================================
    // Single-layer congestion BITRATE floor (issue #1398)
    // ======================================================================

    #[test]
    fn bitrate_step_down_first_cut_lands_on_index_one_not_top() {
        // From the fail-open sentinel (no cut), the FIRST down-step must be a REAL
        // reduction: tier index 1 (24 kbps), NOT index 0 (48 kbps, the healthy
        // no-cut bitrate). A step that returned index 0 would be a no-op cut.
        // Revert it catches: making `audio_congestion_bitrate_step_down` return
        // `audio_tier_bps_for_index(0)` (top) from the sentinel → this fails
        // (24000 != 48000).
        assert_eq!(audio_congestion_bitrate_step_down(u32::MAX), tier_bps(1));
        assert_eq!(tier_bps(0), 48_000, "table sanity: top tier is 48 kbps");
        assert_eq!(tier_bps(1), 24_000, "table sanity: second tier is 24 kbps");
    }

    #[test]
    fn bitrate_step_down_walks_the_ladder_one_tier_at_a_time() {
        // 48(top)→cut→24→12→8, one tier per call (NOT a jump to emergency).
        // Revert it catches: returning `current` unchanged → 24 stays 24, this
        // fails; jumping straight to the bottom from the sentinel → first cut
        // would be 8000, the index_one test fails.
        assert_eq!(audio_congestion_bitrate_step_down(tier_bps(0)), tier_bps(1)); // 48→24
        assert_eq!(audio_congestion_bitrate_step_down(tier_bps(1)), tier_bps(2)); // 24→12
        assert_eq!(audio_congestion_bitrate_step_down(tier_bps(2)), tier_bps(3)); // 12→8
        assert_eq!(tier_bps(2), 12_000);
        assert_eq!(tier_bps(3), 8_000);
    }

    #[test]
    fn bitrate_step_down_clamps_at_emergency_floor() {
        // At the lowest tier (8 kbps) a further cut HOLDS at 8 — a single
        // transient blip cannot gut audio below emergency, and repeated signals
        // simply hold. Revert it catches: an unclamped `idx + 1` index → panic /
        // out-of-bounds, or returning 0 → this fails.
        let bottom = tier_bps(AUDIO_QUALITY_TIERS.len() - 1);
        assert_eq!(bottom, 8_000);
        assert_eq!(audio_congestion_bitrate_step_down(bottom), bottom);
    }

    // effective_audio_bitrate args: (tier_bps, congestion_floor_bps, camera_active, camera_video_exhausted)
    #[test]
    fn effective_audio_bitrate_camera_off_no_cut_ignores_stale_tier_returns_top() {
        // CAMERA-OFF, FLOOR FAIL-OPEN (no cut): the tier atom may hold a STALE low
        // value — the camera AQ loop lowers it on camera-on congestion and never
        // restores it — so it MUST be IGNORED here. With no cut, the correct
        // audio-only bitrate is the healthy TOP tier (48000, #1768).
        // camera_video_exhausted is irrelevant when camera is off.
        assert_eq!(
            effective_audio_bitrate(8_000, u32::MAX, false, false),
            48_000
        );
        assert_eq!(
            effective_audio_bitrate(48_000, u32::MAX, false, false),
            48_000
        );
    }

    #[test]
    fn effective_audio_bitrate_camera_off_cut_ignores_stale_tier_and_uses_floor() {
        // FIX B: CAMERA-OFF with an ACTIVE mic floor cut. The tier atom may hold a
        // STALE camera-on value; it MUST be IGNORED — the mic floor is the live
        // authority audio-only. Returns the FLOOR regardless of the tier.
        assert_eq!(
            effective_audio_bitrate(48_000, 24_000, false, false),
            24_000
        );
        // THE FIX-B decisive case: a STALE tier BELOW the floor.
        assert_eq!(
            effective_audio_bitrate(12_000, 24_000, false, false),
            24_000
        );
    }

    #[test]
    fn effective_audio_bitrate_camera_on_not_exhausted_uses_tier_ignores_floor() {
        // CAMERA-ON, video NOT exhausted: the camera AQ loop is the live authority
        // and the mic detector is gated OFF, so the floor MUST NOT apply.
        assert_eq!(effective_audio_bitrate(48_000, 24_000, true, false), 48_000);
        // Tier already below the floor: still the tier (the AQ loop owns it).
        assert_eq!(
            effective_audio_bitrate(12_000, u32::MAX, true, false),
            12_000
        );
    }

    #[test]
    fn effective_audio_bitrate_camera_on_exhausted_composes_min() {
        // Issue #1611 backstop: camera ON + video EXHAUSTED → min(tier, floor).
        // This is the "camera can't shed video, audio is the only axis" path.
        // Floor tighter than tier: floor governs.
        assert_eq!(effective_audio_bitrate(48_000, 24_000, true, true), 24_000);
        // Tier tighter than floor: tier governs.
        assert_eq!(effective_audio_bitrate(12_000, 24_000, true, true), 12_000);
        // No cut (floor = u32::MAX): min(tier, MAX) = tier (no change from healthy).
        assert_eq!(
            effective_audio_bitrate(48_000, u32::MAX, true, true),
            48_000
        );
    }

    #[test]
    fn bitrate_recover_holds_during_cooldown() {
        // Right after a cut to 24 kbps, recovery HOLDS until a full cooldown has
        // elapsed — no early climb. Revert it catches: climbing before the
        // cooldown (dropping the `>=` guard) → this fails.
        let cut_at = 1000.0;
        let (next, done) = audio_bitrate_recover(
            tier_bps(1), // 24 kbps (post first-cut, #1768)
            cut_at + AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS - 1.0,
            Some(cut_at),
            AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
        );
        assert_eq!(next, tier_bps(1), "must hold at 24 kbps during cooldown");
        assert!(!done, "not fully recovered while still cut");
    }

    #[test]
    fn bitrate_recover_climbs_one_tier_per_cooldown() {
        // After one cooldown, 12 kbps climbs to 24 kbps (one tier UP, toward the
        // top), NOT straight to 48. Revert it catches: a two-tier climb
        // (`saturating_sub(2)`) → this fails (would land on 48000/sentinel);
        // climbing the wrong direction (toward lower bitrate) → this fails.
        let cut_at = 1000.0;
        let (next, done) = audio_bitrate_recover(
            tier_bps(2), // 12 kbps (#1768)
            cut_at + AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
            Some(cut_at),
            AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
        );
        assert_eq!(next, tier_bps(1), "climb exactly one tier (12→24)");
        assert!(!done, "still below the top tier");
    }

    #[test]
    fn bitrate_recover_final_tier_returns_fail_open() {
        // Climbing the LAST tier back to the top (24→48) collapses to the
        // fail-open sentinel and signals fully-recovered. Revert it catches:
        // returning `tier_bps(0)` (48000) instead of the sentinel → this fails
        // (48000 != u32::MAX), which would leave a permanent 48 kbps "cut" that
        // never clears.
        let cut_at = 1000.0;
        let (next, done) = audio_bitrate_recover(
            tier_bps(1), // 24 kbps (one below top, #1768)
            cut_at + AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
            Some(cut_at),
            AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
        );
        assert_eq!(next, u32::MAX, "final climb returns the fail-open sentinel");
        assert!(done, "fully recovered after the top tier");
    }

    #[test]
    fn bitrate_recover_no_cut_is_fail_open() {
        // No active cut (last_congestion_ms == None) → fail-open, done. The loop
        // must not invent a floor out of nowhere. Revert it catches: removing the
        // `None` early-return → this fails.
        let (next, done) = audio_bitrate_recover(
            u32::MAX,
            123_456.0,
            None,
            AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS,
        );
        assert_eq!(next, u32::MAX);
        assert!(done);
    }

    #[test]
    fn bitrate_tick_spaces_tiers_one_cooldown_apart() {
        // The FULL per-tick hysteresis (analogue of
        // `audio_congestion_tick_spaces_rungs_one_cooldown_apart`): after a cut to
        // 24 kbps, recovery climbs back EXACTLY one tier per cooldown — NOT all
        // tiers on consecutive ticks once the first cooldown elapses. This is the
        // regression guard for the per-tier cooldown re-anchor in
        // `audio_bitrate_tick`.
        let cd = AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS;
        let tick = 1000.0; // recovery cadence
        let t0 = 1_000.0;

        // First tick after the client cut the floor to 24 kbps (a decrease vs the
        // fail-open sentinel): detected, cooldown anchored.
        // (`distress_active_now == false` throughout: this test models distress
        // having STOPPED — the floor was cut out-of-band by the client and now the
        // recovery climbs back. The new hold-at-floor flag is exercised separately.)
        let (c, ls, cut) = audio_bitrate_tick(tier_bps(1), u32::MAX, t0, None, cd, false);
        assert_eq!(c, tier_bps(1), "held at 24 kbps right after the cut");
        assert_eq!(cut, Some(t0), "cooldown anchored at the cut time");

        // Suppose congestion deepens: a SECOND cut to 12 kbps (another decrease).
        let (c, ls, cut) = audio_bitrate_tick(tier_bps(2), ls, t0 + tick, cut, cd, false);
        assert_eq!(c, tier_bps(2), "held at 12 kbps after the second cut");
        assert_eq!(cut, Some(t0 + tick), "cooldown re-anchored at the new cut");

        // Just before the cooldown elapses: still 12 kbps.
        let (c, ls, cut) = audio_bitrate_tick(c, ls, t0 + tick + cd - tick, cut, cd, false);
        assert_eq!(c, tier_bps(2), "holds 12 kbps until a full cooldown");

        // Cooldown elapsed: climb exactly one tier (12→24), NOT straight to top.
        let climb_at = t0 + tick + cd;
        let (c, ls, cut) = audio_bitrate_tick(c, ls, climb_at, cut, cd, false);
        assert_eq!(c, tier_bps(1), "climbs exactly one tier after one cooldown");
        assert_eq!(
            cut,
            Some(climb_at),
            "per-tier cooldown re-anchored at climb"
        );

        // THE anti-regression assertion: the immediate next tick must HOLD at 24.
        // Without the per-tier re-anchor, `cut` would still be the old cut time
        // and this tick would jump straight to the top.
        let (c, ls, cut) = audio_bitrate_tick(c, ls, climb_at + tick, cut, cd, false);
        assert_eq!(c, tier_bps(1), "must NOT climb a second tier immediately");

        // Second cooldown elapsed: final climb (32→top) → fully recovered.
        let (c, _ls, cut) = audio_bitrate_tick(c, ls, climb_at + cd, cut, cd, false);
        assert_eq!(c, u32::MAX, "fully recovered after the second cooldown");
        assert_eq!(cut, None, "cut memory cleared on full recovery");
    }

    #[test]
    fn bitrate_tick_clear_to_max_is_read_as_fully_recovered_no_new_cut() {
        // FIX C RECOVERY-SAFETY: the camera-ON edge stores `u32::MAX` into the
        // floor atom out-of-band (clearing the mic cut to fail-open). This RAISES
        // the floor from a prior cut (e.g. 24 kbps) to `u32::MAX`. The recovery
        // state machine must read that raise correctly on the next tick:
        //   * `current == u32::MAX` → `audio_bitrate_recover` returns
        //     `(u32::MAX, true)` (fully recovered), so the tick clears `next_cut`
        //     to `None` (no lingering cooldown).
        //   * the raise (32000 → u32::MAX) is NOT a decrease, so `current <
        //     last_seen` is false → no SPURIOUS new-cut is detected.
        // `last_seen` is the prior, LOWER floor (24 kbps) the loop last left; a
        // pre-existing cooldown timestamp is present.
        let cd = AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS;
        let now = 10_000.0;
        let prior_cut_ts = 5_000.0;
        let (floor, last_seen, cut) =
            audio_bitrate_tick(u32::MAX, tier_bps(1), now, Some(prior_cut_ts), cd, false);
        assert_eq!(
            floor,
            u32::MAX,
            "a clear-to-MAX is read as fully recovered — the floor stays fail-open"
        );
        assert_eq!(
            cut, None,
            "clear-to-MAX clears the cut memory (no lingering cooldown after FIX C)"
        );
        assert_eq!(
            last_seen,
            u32::MAX,
            "the loop leaves the atom at MAX so the next tick's new-cut compare is fresh"
        );
        // Revert it catches: if `audio_bitrate_recover` did NOT short-circuit a
        // `u32::MAX` current to fully-recovered (the
        // `current_floor_bps == u32::MAX || current_floor_bps >= top_bps` guard at
        // the top), the off-ladder MAX would fall through to the climb path, be
        // resolved to the top rung (index 0), and — with the 2-min cooldown not
        // elapsed (now-prior_cut = 5s) — return `(MAX, false)` (NOT fully
        // recovered), leaving `cut == Some(prior_cut_ts)`. This `cut == None`
        // assertion then FAILS, proving the clear is mis-read as a live cut.
        // (NOTE: removing only the `== u32::MAX` sub-term is NOT caught — the
        // `>= top_bps` clause also matches MAX — so this guards the whole
        // short-circuit, the actual load-bearing branch.)
    }

    #[test]
    fn bitrate_tick_holds_floor_under_sustained_distress() {
        // ISSUE #1398 HOLD-AT-FLOOR: at the EMERGENCY floor (8 kbps), an ongoing
        // step-down DECISION is a clamped no-op (the value cannot drop further), so
        // the value-decrease re-anchor (`current < last_seen`) never fires. The new
        // `distress_active_now` flag is what re-anchors the cooldown there, so the
        // 2-min recovery climb only begins after distress STOPS for a full cooldown.
        // Without the fix the cooldown would elapse under sustained distress and
        // recovery would climb 8k→12k, only to be re-cut a window later — a
        // wasteful ~4 s excursion every cooldown. Audio must HOLD at 8k.
        let cd = AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS;
        let tick = 1000.0; // recovery cadence
        let floor_min = tier_bps(AUDIO_QUALITY_TIERS.len() - 1); // 8 kbps, bottom rung

        // The detector has cut the floor to 8k and distress is ONGOING. The loop
        // last left the atom at 8k (a hold is not a new cut), and a cooldown is
        // anchored from an earlier cut.
        let cut_ts = 1_000.0;
        let mut last_seen = floor_min;
        let mut floor = floor_min;
        let mut cut = Some(cut_ts);

        // Tick repeatedly PAST the cooldown with distress STILL firing each tick.
        // Each tick re-anchors the cooldown to `now`, so it never elapses → hold.
        let mut now = cut_ts + tick;
        for _ in 0..((cd / tick) as i64 + 5) {
            let (f, ls, c) = audio_bitrate_tick(floor, last_seen, now, cut, cd, true);
            assert_eq!(
                f, floor_min,
                "must HOLD at the 8 kbps emergency floor while distress is ongoing"
            );
            assert_eq!(
                c,
                Some(now),
                "an ongoing distress decision re-anchors the cooldown to NOW each tick"
            );
            assert_eq!(ls, floor_min, "a hold leaves last_seen at the held floor");
            floor = f;
            last_seen = ls;
            cut = c;
            now += tick;
        }
        // Sanity: total elapsed since the original cut FAR exceeds one cooldown,
        // proving the hold is NOT merely "still inside the first cooldown window".
        assert!(
            now - cut_ts > cd + 4.0 * tick,
            "the test really did tick past a full cooldown under sustained distress"
        );

        // Now distress STOPS: `distress_active_now == false`. The cooldown was last
        // anchored at the final distress tick; once a FULL cooldown passes with no
        // further decision, recovery climbs one tier (8k→12k).
        let stop_anchor = cut.expect("cooldown anchored at the last distress tick");
        // Just before a full cooldown after distress stopped: still 8k.
        let (f, ls, c) =
            audio_bitrate_tick(floor, last_seen, stop_anchor + cd - tick, cut, cd, false);
        assert_eq!(
            f, floor_min,
            "holds 8k until a full cooldown AFTER distress stops"
        );
        floor = f;
        last_seen = ls;
        cut = c;
        // Full cooldown after distress stopped: climb one tier (8k→12k).
        let (f, _ls, _c) = audio_bitrate_tick(floor, last_seen, stop_anchor + cd, cut, cd, false);
        assert_eq!(
            f,
            tier_bps(AUDIO_QUALITY_TIERS.len() - 2),
            "climbs ONE tier (8k→12k) only after distress stops for a full cooldown"
        );
        // Revert it catches: forcing the call site to pass `false` (or removing the
        // `distress_active_now && current != u32::MAX` re-anchor branch in
        // `audio_bitrate_tick`, or dropping the `Some(now_ms)` re-anchor in it) makes
        // the cooldown elapse DURING sustained distress, so the in-loop assertion
        // `f == floor_min` fails on the tick where recovery climbs 8k→12k.
    }

    #[test]
    fn bitrate_tick_still_climbs_after_distress_stops_no_wedge() {
        // ANTI-WEDGE (issue #1398): the hold-at-floor branch must NOT be sticky.
        // Once distress stops (`distress_active_now == false` on every tick), the
        // floor climbs normally one tier per cooldown all the way back to the
        // fail-open sentinel — the floor is never wedged at a low tier forever.
        let cd = AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS;
        let floor_min = tier_bps(AUDIO_QUALITY_TIERS.len() - 1); // 8 kbps

        // Start cut at the 8k floor with a cooldown anchored; distress has stopped.
        let cut_anchor = 1_000.0;
        let mut floor = floor_min;
        let mut last_seen = floor_min;
        let mut cut = Some(cut_anchor);
        let mut now = cut_anchor;

        // Drive forward in cooldown-sized hops with NO distress. The floor must
        // climb 8k→12k→24k→48k(=MAX) and finally clear the cut to None.
        let mut climbs = 0;
        for _ in 0..10 {
            now += cd;
            let (f, ls, c) = audio_bitrate_tick(floor, last_seen, now, cut, cd, false);
            if f != floor {
                climbs += 1;
            }
            floor = f;
            last_seen = ls;
            cut = c;
            if floor == u32::MAX {
                break;
            }
        }
        assert_eq!(
            floor,
            u32::MAX,
            "with distress stopped the floor climbs all the way back to fail-open — no wedge"
        );
        assert_eq!(cut, None, "full recovery clears the cut memory");
        // 8k→12k→24k→MAX = three climbs from the bottom rung.
        assert_eq!(
            climbs, 3,
            "climbed exactly one tier per cooldown back to the top"
        );
        // Revert it catches: making the hold branch sticky (e.g. returning
        // `Some(now_ms)` unconditionally, ignoring `distress_active_now`) wedges the
        // floor — it never reaches u32::MAX, so the `floor == u32::MAX` assertion fails.
    }

    #[test]
    fn bitrate_tick_at_max_never_wedges_even_if_distress_flag_set() {
        // FAIL-OPEN SAFETY (issue #1398): at the fail-open sentinel (`u32::MAX`)
        // there is nothing to HOLD — the floor is already fully recovered. Even if
        // a stale `distress_active_now == true` is passed (it should not happen at
        // MAX, since the detector clears to MAX only on the gate-CLOSED tick where
        // the flag is false, but defend it anyway), the tick must report fully
        // recovered and clear the cut, NOT wedge MAX with a live cooldown.
        let cd = AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS;
        let now = 10_000.0;

        // distress flag FALSE at MAX → fully recovered, cut cleared (the real path).
        let (floor, last_seen, cut) =
            audio_bitrate_tick(u32::MAX, u32::MAX, now, Some(5_000.0), cd, false);
        assert_eq!(
            floor,
            u32::MAX,
            "MAX with no distress stays fully recovered"
        );
        assert_eq!(cut, None, "fully recovered clears the cut");
        assert_eq!(last_seen, u32::MAX, "last_seen stays at MAX");

        // distress flag TRUE at MAX → STILL fully recovered, cut cleared. The
        // hold-at-floor branch is guarded by `current != u32::MAX`, so it does NOT
        // fire here; MAX is fail-open and there is nothing to hold.
        let (floor, last_seen, cut) =
            audio_bitrate_tick(u32::MAX, u32::MAX, now, Some(5_000.0), cd, true);
        assert_eq!(
            floor,
            u32::MAX,
            "a stale distress flag at MAX must NOT wedge the fail-open floor"
        );
        assert_eq!(
            cut, None,
            "a stale distress flag at MAX still clears the cut"
        );
        assert_eq!(last_seen, u32::MAX, "last_seen stays at MAX");
        // Revert it catches: dropping the `&& current != u32::MAX` guard from the
        // hold branch makes the distress-true call return `(u32::MAX, u32::MAX,
        // Some(now))` — the floor is technically still MAX, but `cut == Some(now)`
        // (a live cooldown wedged at fail-open), so the `cut == None` assertion
        // fails. This pins the guard that keeps MAX from wedging.
    }

    #[test]
    fn reconfig_change_bitrate_only_change_emits() {
        // Single-layer key: a change in ONLY the bitrate (fec/loss steady) must
        // emit so the live bitrate downshift is applied. Revert it catches:
        // dropping the bitrate component from the key (comparing only fec/loss) →
        // this fails (would suppress a pure bitrate change). init_bitrate=48000.
        let init = Some(48_000);
        // Healthy at init (top tier, full bitrate): suppress.
        assert_eq!(
            audio_reconfig_change((false, 0, Some(48_000)), None, init),
            None
        );
        // Floor cut drops the effective bitrate to 24 kbps (fec/loss unchanged):
        // must emit.
        assert_eq!(
            audio_reconfig_change(
                (false, 0, Some(24_000)),
                Some((false, 0, Some(48_000))),
                init
            ),
            Some((false, 0, Some(24_000))),
            "a pure bitrate downshift must re-apply in single-layer mode"
        );
        // Same key again: suppress (debounce).
        assert_eq!(
            audio_reconfig_change(
                (false, 0, Some(24_000)),
                Some((false, 0, Some(24_000))),
                init
            ),
            None
        );
    }

    #[test]
    fn reconfig_change_multilayer_is_bitrate_free_and_matches_fec_only() {
        // Multi-layer mode pins the bitrate component to None and init_bitrate to
        // None, so the bitrate-aware change-detector reduces EXACTLY to the
        // pre-#1398 (fec, loss%)-only path. Revert it catches: if the multilayer
        // path accidentally included a bitrate, the emitted message would carry a
        // bitRate key and this lockstep with `audio_fec_reconfig_change` breaks.
        // Healthy at init: suppress (matches fec-only).
        assert_eq!(audio_reconfig_change((false, 0, None), None, None), None);
        assert_eq!(audio_fec_reconfig_change((false, 0), None), None);
        // Drop to a FEC tier: emit with bitrate None (matches fec-only lifted).
        assert_eq!(
            audio_reconfig_change((true, 10, None), None, None),
            Some((true, 10, None))
        );
        assert_eq!(
            audio_fec_reconfig_change((true, 10), None),
            Some((true, 10))
        );
    }

    // ======================================================================
    // Mic-side uplink-distress DETECTOR (issue #1398)
    // ======================================================================

    // A "quiet" axis: window closed, but ZERO delta. Used to isolate one of the
    // three axes. `elapsed_ms` >= the widest audio window so the window is always
    // considered closed for the axis under test's threshold check (a closed
    // window with zero delta never fires, so the quiet axes contribute nothing to
    // the OR).
    fn quiet_axis() -> AudioUplinkAxisInput {
        AudioUplinkAxisInput {
            current: 0,
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_WS_WINDOW_MS, // >= all three audio windows
        }
    }

    #[test]
    fn uplink_detector_wt_saturation_fires_at_threshold_over_closed_window() {
        // A sustained WT-saturation cluster (delta == AUDIO threshold) over a
        // CLOSED audio-saturation window steps the floor down; the WS axis is
        // quiet. Revert it catches: feeding the VIDEO window/threshold (see the
        // after-video test) or inverting the comparison flips `step_down`.
        let sat = AudioUplinkAxisInput {
            current: AUDIO_UPLINK_SATURATION_STALL_THRESHOLD,
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_SATURATION_WINDOW_MS,
        };
        let d = audio_uplink_step_down_decision(sat, quiet_axis(), quiet_axis());
        assert!(d.step_down, "delta == audio saturation threshold must fire");
        assert!(d.roll_sat, "a closed saturation window must roll");
        assert_eq!(d.new_sat_snapshot, AUDIO_UPLINK_SATURATION_STALL_THRESHOLD);
    }

    #[test]
    fn uplink_detector_ws_backpressure_fires_independently() {
        // The WS axis alone (delta == AUDIO WS threshold over a closed WS window)
        // must trip the downshift even with the WT axis quiet — proving the OR
        // and that the WS axis reads the WS counter/constants. Revert it catches:
        // dropping the WS axis from the OR → this fails.
        let ws = AudioUplinkAxisInput {
            current: AUDIO_UPLINK_WS_DROP_THRESHOLD,
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_WS_WINDOW_MS,
        };
        let d = audio_uplink_step_down_decision(quiet_axis(), ws, quiet_axis());
        assert!(d.step_down, "WS backpressure alone must fire the downshift");
        assert!(d.roll_ws, "a closed WS window must roll");
        assert_eq!(d.new_ws_snapshot, AUDIO_UPLINK_WS_DROP_THRESHOLD);
    }

    #[test]
    fn uplink_detector_wt_drop_fires_independently() {
        // ISSUE #1398 P2: the WT-DROP axis alone (delta == AUDIO WT-drop threshold
        // over a closed WT-drop window) must trip the downshift even with the
        // saturation AND WS axes quiet — proving the THIRD OR term and that the
        // WT-drop axis reads the WT-drop counter/constants. This is the audio
        // analogue of the camera AQ's `wt_drop_step_down_decision`. Revert it
        // catches: DROPPING the wtdrop OR term (`|| wtd.step_down`) → this fails
        // (step_down would be false with the other two axes quiet).
        let wtdrop = AudioUplinkAxisInput {
            current: AUDIO_UPLINK_WT_DROP_THRESHOLD,
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_WT_DROP_WINDOW_MS,
        };
        let d = audio_uplink_step_down_decision(quiet_axis(), quiet_axis(), wtdrop);
        assert!(
            d.step_down,
            "WT unistream drop alone must fire the downshift"
        );
        assert!(d.roll_wtdrop, "a closed WT-drop window must roll");
        assert_eq!(d.new_wtdrop_snapshot, AUDIO_UPLINK_WT_DROP_THRESHOLD);
    }

    #[test]
    fn uplink_detector_wt_drop_below_audio_threshold_does_not_fire() {
        // One below the audio WT-drop threshold over a CLOSED WT-drop window must
        // NOT fire (a single transient unistream reset is not sustained distress),
        // but the closed window still rolls. Revert it catches: a `>` vs `>=`
        // boundary slip or a -1 threshold drift on the WT-drop axis.
        let wtdrop = AudioUplinkAxisInput {
            current: AUDIO_UPLINK_WT_DROP_THRESHOLD - 1,
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_WT_DROP_WINDOW_MS,
        };
        let d = audio_uplink_step_down_decision(quiet_axis(), quiet_axis(), wtdrop);
        assert!(
            !d.step_down,
            "delta below the audio WT-drop threshold must not fire"
        );
        assert!(
            d.roll_wtdrop,
            "a closed WT-drop window still rolls when it does not fire"
        );
    }

    #[test]
    fn uplink_detector_wt_drop_audio_sheds_after_video_not_with_video_constants() {
        // THE after-video / wrong-constants guard for the WT-DROP axis (P2),
        // mirroring the saturation/WS guard. A tick the VIDEO WT-drop detector
        // WOULD shed on but the AUDIO WT-drop axis must NOT:
        //   * delta == the VIDEO WT-drop threshold (3, below the audio 5), and
        //   * elapsed == the VIDEO WT-drop window (< the audio 2x window) → the
        //     AUDIO WT-drop window is still OPEN.
        // If the WT-drop axis were (mistakenly) wired to the VIDEO WT-drop
        // constants (WT_SELF_CONGESTION_*), both would let it fire and this fails —
        // pinning that the audio WT-drop axis uses the AUDIO constants and sheds
        // strictly after video. Compile-time const checks (clippy-clean).
        const _: () = assert!(
            AUDIO_UPLINK_WT_DROP_THRESHOLD > WT_SELF_CONGESTION_DROP_THRESHOLD,
            "audio WT-drop threshold must exceed the video one"
        );
        const _: () = assert!(
            AUDIO_UPLINK_WT_DROP_WINDOW_MS > WT_SELF_CONGESTION_WINDOW_MS,
            "audio WT-drop window must exceed the video one"
        );
        // OPEN-window case: a video-fire delta over a video-length window — the
        // audio WT-drop window is not yet closed, so audio cannot even evaluate.
        let wtdrop_open = AudioUplinkAxisInput {
            current: WT_SELF_CONGESTION_DROP_THRESHOLD, // a video-fire delta
            snapshot: 0,
            elapsed_ms: WT_SELF_CONGESTION_WINDOW_MS, // audio WT-drop window still OPEN
        };
        let d = audio_uplink_step_down_decision(quiet_axis(), quiet_axis(), wtdrop_open);
        assert!(
            !d.step_down,
            "a video-WT-drop delta over a video-length window must NOT shed audio"
        );
        // THRESHOLD-isolating case: a CLOSED audio WT-drop window, but a delta ==
        // the VIDEO WT-drop threshold (3), BELOW the audio threshold (5). Video
        // would fire; audio must not. Revert it catches: replacing
        // AUDIO_UPLINK_WT_DROP_THRESHOLD with WT_SELF_CONGESTION_DROP_THRESHOLD
        // makes delta 3 >= 3 fire → this fails (caught even though the open-window
        // case above passes).
        let wtdrop_closed = AudioUplinkAxisInput {
            current: WT_SELF_CONGESTION_DROP_THRESHOLD, // 3: a video-fire delta, < audio 5
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_WT_DROP_WINDOW_MS, // audio WT-drop window CLOSED
        };
        let d = audio_uplink_step_down_decision(quiet_axis(), quiet_axis(), wtdrop_closed);
        assert!(
            !d.step_down,
            "a video-WT-drop delta (3) over a CLOSED audio window must NOT shed \
             audio — audio needs its higher threshold (5)"
        );
    }

    #[test]
    fn uplink_detector_open_window_does_not_fire_or_roll() {
        // Before either window closes (elapsed < window), even a large delta must
        // NOT fire and must NOT roll — the evidence has not persisted long enough.
        // Revert it catches: dropping the `elapsed_ms < window_ms` guard in
        // `evaluate_self_congestion` → this fires on a half-open window.
        let sat = AudioUplinkAxisInput {
            current: 1_000, // huge delta
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_SATURATION_WINDOW_MS - 1.0,
        };
        let ws = AudioUplinkAxisInput {
            current: 1_000,
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_WS_WINDOW_MS - 1.0,
        };
        let wtdrop = AudioUplinkAxisInput {
            current: 1_000,
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_WT_DROP_WINDOW_MS - 1.0,
        };
        let d = audio_uplink_step_down_decision(sat, ws, wtdrop);
        assert!(!d.step_down, "an open window must not fire");
        assert!(
            !d.roll_sat && !d.roll_ws && !d.roll_wtdrop,
            "an open window must not roll"
        );
        // Snapshot is held (unchanged) while the window is open.
        assert_eq!(d.new_sat_snapshot, 0);
        assert_eq!(d.new_ws_snapshot, 0);
        assert_eq!(d.new_wtdrop_snapshot, 0);
    }

    #[test]
    fn uplink_detector_below_audio_threshold_does_not_fire() {
        // One below the audio threshold on a closed window must NOT fire (a single
        // transient blip is not sustained distress). Revert it catches: an `>`
        // instead of `>=`/`<` boundary slip, or a -1 threshold drift.
        let sat = AudioUplinkAxisInput {
            current: AUDIO_UPLINK_SATURATION_STALL_THRESHOLD - 1,
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_SATURATION_WINDOW_MS,
        };
        let d = audio_uplink_step_down_decision(sat, quiet_axis(), quiet_axis());
        assert!(
            !d.step_down,
            "delta below the audio threshold must not fire"
        );
        assert!(
            d.roll_sat,
            "a closed window still rolls even when it does not fire"
        );
    }

    #[test]
    fn uplink_detector_audio_sheds_after_video_not_with_video_constants() {
        // THE after-video / wrong-constants guard. Construct a tick that the
        // VIDEO detector WOULD shed on but the AUDIO detector must NOT:
        //   * delta == the VIDEO saturation threshold (one below the audio one,
        //     since audio = video + 2), so a video tick fires but audio must not
        //     on the COUNT axis;
        //   * elapsed == the VIDEO saturation window (which is < the audio window),
        //     so the AUDIO window is still OPEN — the audio detector cannot even
        //     evaluate yet.
        // If `audio_uplink_step_down_decision` were (mistakenly) wired to the
        // VIDEO constants (WT_SATURATION_*), BOTH of these would let it fire and
        // this test fails — pinning that audio uses the audio constants AND sheds
        // strictly after video. Sanity-assert the relationships the test leans on
        // at COMPILE time (`const {}` — these compare two consts, so a runtime
        // `assert!` is both wasteful and clippy-flagged as constant-valued).
        const _: () = assert!(
            AUDIO_UPLINK_SATURATION_STALL_THRESHOLD > WT_SATURATION_STALL_THRESHOLD,
            "audio saturation threshold must exceed video's"
        );
        const _: () = assert!(
            AUDIO_UPLINK_SATURATION_WINDOW_MS > WT_SATURATION_WINDOW_MS,
            "audio saturation window must exceed video's"
        );
        let sat = AudioUplinkAxisInput {
            current: WT_SATURATION_STALL_THRESHOLD, // a video-fire delta
            snapshot: 0,
            elapsed_ms: WT_SATURATION_WINDOW_MS, // audio window still OPEN here
        };
        let d = audio_uplink_step_down_decision(sat, quiet_axis(), quiet_axis());
        assert!(
            !d.step_down,
            "a video-threshold delta over a video-length window must NOT shed audio \
             (audio sheds after video; uses the audio constants)"
        );
        // THRESHOLD-isolating case: a CLOSED audio window (elapsed >= audio
        // window), but a delta == the VIDEO threshold (3), which is BELOW the
        // audio threshold (5). Video would fire; audio must not. This catches a
        // threshold-only mutation (audio threshold → video threshold) that the
        // open-window case above would mask. Revert it catches: replacing
        // AUDIO_UPLINK_SATURATION_STALL_THRESHOLD with WT_SATURATION_STALL_THRESHOLD
        // makes delta 3 >= 3 fire → this fails.
        let sat_closed = AudioUplinkAxisInput {
            current: WT_SATURATION_STALL_THRESHOLD, // 3: a video-fire delta, < audio 5
            snapshot: 0,
            elapsed_ms: AUDIO_UPLINK_SATURATION_WINDOW_MS, // audio window CLOSED
        };
        let d = audio_uplink_step_down_decision(sat_closed, quiet_axis(), quiet_axis());
        assert!(
            !d.step_down,
            "a video-threshold delta (3) over a CLOSED audio window must NOT shed \
             audio — audio needs its higher threshold (5)"
        );
        // Same idea on the WS axis: a video-WS-fire delta over a video-WS window
        // must not shed audio. Compile-time const checks (clippy-clean).
        const _: () = assert!(AUDIO_UPLINK_WS_DROP_THRESHOLD > WS_SELF_CONGESTION_DROP_THRESHOLD);
        const _: () = assert!(AUDIO_UPLINK_WS_WINDOW_MS > WS_SELF_CONGESTION_WINDOW_MS);
        let ws = AudioUplinkAxisInput {
            current: WS_SELF_CONGESTION_DROP_THRESHOLD,
            snapshot: 0,
            elapsed_ms: WS_SELF_CONGESTION_WINDOW_MS,
        };
        let d = audio_uplink_step_down_decision(quiet_axis(), ws, quiet_axis());
        assert!(
            !d.step_down,
            "a video-WS-threshold delta over a video-WS window must NOT shed audio"
        );
    }

    // FIX A: detector GATE (audio_detector_gate_open).
    // Args: (single_layer, camera_active).
    // Gate = single_layer && !camera_active.
    // Args: (single_layer, camera_active, camera_video_exhausted, screen_active, screen_video_exhausted).
    #[test]
    fn detector_gate_audio_only_opens() {
        // Single-layer + camera off + no screen → gate OPEN (the detector evaluates).
        // This is the audio-only single-layer publisher the lever exists for.
        assert!(audio_detector_gate_open(true, false, false, false, false));
    }
    #[test]
    fn detector_gate_camera_active_not_exhausted_closes() {
        // Single-layer + camera ON + video NOT exhausted → gate CLOSED (camera AQ
        // can still shed video). REVERT it catches: dropping the camera term opens.
        assert!(!audio_detector_gate_open(true, true, false, false, false));
    }
    #[test]
    fn detector_gate_camera_active_exhausted_opens() {
        // Issue #1611 lever 2: camera ON but video EXHAUSTED → gate OPEN (video
        // can't shed further, audio is the only remaining axis).
        assert!(audio_detector_gate_open(true, true, true, false, false));
    }
    #[test]
    fn detector_gate_screen_active_not_exhausted_closes() {
        // Issue #1611 lever 3: screen active + video NOT exhausted → gate CLOSED
        // (screen video can still shed).
        assert!(!audio_detector_gate_open(true, false, false, true, false));
    }
    #[test]
    fn detector_gate_screen_active_exhausted_opens() {
        // Issue #1611 lever 3: screen active + video EXHAUSTED → gate OPEN.
        assert!(audio_detector_gate_open(true, false, false, true, true));
    }
    #[test]
    fn detector_gate_both_active_both_exhausted_opens() {
        // Both camera and screen on, both exhausted → gate OPEN.
        assert!(audio_detector_gate_open(true, true, true, true, true));
    }
    #[test]
    fn detector_gate_both_active_camera_not_exhausted_closes() {
        // Camera not exhausted blocks even if screen is exhausted.
        assert!(!audio_detector_gate_open(true, true, false, true, true));
    }
    #[test]
    fn detector_gate_multilayer_closes_regardless() {
        // Multi-layer mode → gate CLOSED for any video state (the layer-ceiling
        // lever handles congestion there; the bitrate floor is inert).
        assert!(!audio_detector_gate_open(false, false, false, false, false));
        assert!(!audio_detector_gate_open(false, true, true, true, true));
    }
    // FIX 1: window RE-SEED decision (audio_detector_should_reseed).
    // Args: (should_evaluate, was_active, force_reseed).
    #[test]
    fn detector_reseed_on_reactivation() {
        // Inactive last tick, active this tick (gate reopened or process start),
        // no reconnect pending: RE-SEED so the first post-gap window measures
        // distress from now forward.
        assert!(audio_detector_should_reseed(true, false, false));
    }
    #[test]
    fn detector_no_reseed_in_steady_state() {
        // Active last AND this tick, no reconnect: evaluate windows normally, do
        // NOT re-seed. REVERT it catches: dropping `&& (!was_active || ...)` makes
        // the body `should_evaluate` = true, flipping this from false to TRUE and
        // FAILING.
        assert!(!audio_detector_should_reseed(true, true, false));
    }
    #[test]
    fn detector_no_reseed_when_gated() {
        // Gate closed → never re-seed, regardless of the other inputs (the closure
        // only re-seeds on a tick it actually evaluates).
        assert!(!audio_detector_should_reseed(false, true, false));
        assert!(!audio_detector_should_reseed(false, false, false));
        assert!(!audio_detector_should_reseed(false, true, true));
        assert!(!audio_detector_should_reseed(false, false, true));
    }
    #[test]
    fn detector_reseed_on_reconnect_while_active() {
        // RECONNECT-RESEED P1 (issue #1398): the detector stayed CONTINUOUSLY
        // ACTIVE across a reconnect (camera off + single-layer the whole time, so
        // the gate never closed and `was_active` is TRUE), but a reconnect flag is
        // pending. It MUST re-seed so the transport counters bumped by the
        // teardown/rebuild are not read as a cross-reconnect distress delta on the
        // fresh session.
        //
        // REVERT it catches: dropping the `|| force_reseed` term makes the body
        // `should_evaluate && !was_active` = `true && !true` = FALSE here, flipping
        // this from true to FALSE and FAILING — proving the reconnect path forces a
        // re-seed that the `!was_active` path alone (false here) would miss. This is
        // the load-bearing case for the whole fix: without it the first closed
        // window after a reconnect cashes a spurious cut.
        assert!(audio_detector_should_reseed(true, true, true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::aes::Aes128State;
    use crate::decode::neteq_audio_decoder::NetEqAudioPeerDecoder;
    use protobuf::Message;
    use videocall_types::protos::packet_wrapper::PacketWrapper;
    use wasm_bindgen_test::*;

    fn make_audio_data() -> Uint8Array {
        let d = Uint8Array::new_with_length(8);
        d.copy_from(&[1, 2, 3, 4, 5, 6, 7, 8]);
        d
    }

    /// Phase 3c: a layer-0 audio chunk is wire-identical to one that never set
    /// the field (so single-layer mic publishers are byte-identical), and a
    /// non-zero audio layer round-trips with media_kind AUDIO.
    #[wasm_bindgen_test]
    fn audio_chunk_layer_zero_is_wire_absent() {
        let aes = Rc::new(Aes128State::new(false));
        let with_zero = transform_audio_chunk(&make_audio_data(), "alice", 0, aes.clone(), None, 0);
        let parsed = PacketWrapper::parse_from_bytes(&with_zero.write_to_bytes().unwrap()).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 0);
        assert_eq!(
            parsed.media_kind.enum_value(),
            Ok(MediaKind::AUDIO),
            "audio media_kind preserved"
        );
        // Tag 5 is omitted at layer 0: re-serializing the parsed wrapper must
        // not gain a simulcast_layer_id field.
        assert_eq!(parsed.simulcast_layer_id, 0);
    }

    #[wasm_bindgen_test]
    fn audio_chunk_layer_one_round_trips() {
        let aes = Rc::new(Aes128State::new(false));
        let with_one = transform_audio_chunk(&make_audio_data(), "alice", 0, aes, None, 1);
        let parsed = PacketWrapper::parse_from_bytes(&with_one.write_to_bytes().unwrap()).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 1);
        assert_eq!(parsed.media_kind.enum_value(), Ok(MediaKind::AUDIO));
    }

    /// Issue #1082: the new top audio rung (layer 2) round-trips with media_kind
    /// AUDIO, confirming the 3-rung ladder is wire-representable.
    #[wasm_bindgen_test]
    fn audio_chunk_layer_two_round_trips() {
        let aes = Rc::new(Aes128State::new(false));
        let with_two = transform_audio_chunk(&make_audio_data(), "alice", 0, aes, None, 2);
        let parsed = PacketWrapper::parse_from_bytes(&with_two.write_to_bytes().unwrap()).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 2);
        assert_eq!(parsed.media_kind.enum_value(), Ok(MediaKind::AUDIO));
    }

    #[wasm_bindgen_test]
    fn pack_normal_primary_and_redundant() {
        let primary = b"hello_primary";
        let redundant = PreviousAudioFrame {
            data: b"prev_frame".to_vec(),
            sequence: 42,
        };

        let packed = pack_redundant_audio(primary, &redundant);

        // Verify total length: 4 + primary.len() + 4 + redundant.len()
        assert_eq!(packed.len(), 4 + 13 + 4 + 10);

        // Verify primary_len field (first 4 bytes, little-endian)
        let primary_len = u32::from_le_bytes([packed[0], packed[1], packed[2], packed[3]]);
        assert_eq!(primary_len, 13);

        // Verify primary data
        assert_eq!(&packed[4..4 + 13], b"hello_primary");

        // Verify redundant_seq field
        let redundant_seq = u32::from_le_bytes([packed[17], packed[18], packed[19], packed[20]]);
        assert_eq!(redundant_seq, 42);

        // Verify redundant data
        assert_eq!(&packed[21..], b"prev_frame");
    }

    #[wasm_bindgen_test]
    fn pack_empty_primary() {
        let primary = b"";
        let redundant = PreviousAudioFrame {
            data: b"redundant_data".to_vec(),
            sequence: 0,
        };

        let packed = pack_redundant_audio(primary, &redundant);

        // 4 (primary_len) + 0 (primary) + 4 (redundant_seq) + 14 (redundant)
        assert_eq!(packed.len(), 22);

        let primary_len = u32::from_le_bytes([packed[0], packed[1], packed[2], packed[3]]);
        assert_eq!(primary_len, 0);

        // Redundant seq starts immediately after primary_len + 0 bytes of data
        let redundant_seq = u32::from_le_bytes([packed[4], packed[5], packed[6], packed[7]]);
        assert_eq!(redundant_seq, 0);

        assert_eq!(&packed[8..], b"redundant_data");
    }

    #[wasm_bindgen_test]
    fn pack_empty_redundant_data() {
        let primary = b"some_audio";
        let redundant = PreviousAudioFrame {
            data: vec![],
            sequence: 100,
        };

        let packed = pack_redundant_audio(primary, &redundant);

        // 4 (primary_len) + 10 (primary) + 4 (redundant_seq) + 0 (redundant)
        assert_eq!(packed.len(), 18);

        let primary_len = u32::from_le_bytes([packed[0], packed[1], packed[2], packed[3]]);
        assert_eq!(primary_len, 10);

        assert_eq!(&packed[4..14], b"some_audio");

        let redundant_seq = u32::from_le_bytes([packed[14], packed[15], packed[16], packed[17]]);
        assert_eq!(redundant_seq, 100);

        // No redundant data after the seq
        assert_eq!(packed.len(), 18);
    }

    #[wasm_bindgen_test]
    fn pack_typical_opus_frame_size() {
        // Typical Opus frame at 48kbps, 20ms = ~120 bytes
        let primary: Vec<u8> = (0..120).collect();
        let redundant = PreviousAudioFrame {
            data: (0..100).collect(),
            sequence: 9999,
        };

        let packed = pack_redundant_audio(&primary, &redundant);

        assert_eq!(packed.len(), 4 + 120 + 4 + 100);

        let primary_len = u32::from_le_bytes([packed[0], packed[1], packed[2], packed[3]]);
        assert_eq!(primary_len, 120);

        assert_eq!(&packed[4..124], primary.as_slice());

        let redundant_seq =
            u32::from_le_bytes([packed[124], packed[125], packed[126], packed[127]]);
        assert_eq!(redundant_seq, 9999);

        assert_eq!(&packed[128..], redundant.data.as_slice());
    }

    #[wasm_bindgen_test]
    fn pack_large_sequence_number_truncation() {
        // Sequence number > u32::MAX should be truncated to lower 32 bits
        let primary = b"data";
        let redundant = PreviousAudioFrame {
            data: b"red".to_vec(),
            sequence: (u32::MAX as u64) + 5, // 0x1_0000_0004
        };

        let packed = pack_redundant_audio(primary, &redundant);

        let redundant_seq = u32::from_le_bytes([packed[8], packed[9], packed[10], packed[11]]);
        // u64 0x1_0000_0004 cast to u32 = 4
        assert_eq!(redundant_seq, 4);
    }

    #[wasm_bindgen_test]
    fn round_trip_pack_then_unpack() {
        let primary = b"primary_audio_frame_data";
        let redundant = PreviousAudioFrame {
            data: b"redundant_audio_frame".to_vec(),
            sequence: 77,
        };

        let packed = pack_redundant_audio(primary, &redundant);

        // Unpack using the decoder's function
        let result = NetEqAudioPeerDecoder::unpack_red_audio_public(&packed);
        assert!(
            result.is_some(),
            "unpack should succeed for valid packed data"
        );

        let (unpacked_primary, unpacked_seq, unpacked_redundant) = result.unwrap();
        assert_eq!(unpacked_primary, primary);
        assert_eq!(unpacked_seq, 77);
        assert_eq!(unpacked_redundant, redundant.data);
    }

    #[wasm_bindgen_test]
    fn round_trip_with_typical_opus_sizes() {
        let primary: Vec<u8> = (0..80).collect();
        let redundant = PreviousAudioFrame {
            data: (0..60).collect(),
            sequence: 12345,
        };

        let packed = pack_redundant_audio(&primary, &redundant);
        let result = NetEqAudioPeerDecoder::unpack_red_audio_public(&packed);
        assert!(result.is_some());

        let (unpacked_primary, unpacked_seq, unpacked_redundant) = result.unwrap();
        assert_eq!(unpacked_primary, primary);
        assert_eq!(unpacked_seq, 12345);
        assert_eq!(unpacked_redundant, redundant.data);
    }
}
