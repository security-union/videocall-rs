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
    AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS, AUDIO_CONGESTION_RECOVERY_TICK_MS, AUDIO_QUALITY_TIERS,
    AUDIO_REDUNDANCY_ENABLED, AUDIO_RED_FORMAT, VAD_POLL_INTERVAL_MS,
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
/// - layer 0 = LOW (24 kbps) — the base the relay always forwards and a
///   congested receiver pulls. Matches the AQ "low" tier.
/// - layer 1 = MID (32 kbps) — an intermediate rung for moderate downlinks.
/// - layer 2 = HIGH (50 kbps) — the upgrade layer a receiver with headroom
///   selects. Matches the AQ "high" tier.
///
/// This slice is the **single source of truth** for the publisher-side audio
/// ladder; its length defines the maximum supported audio layer count and is
/// kept in lockstep with the receiver-side `AUDIO_LAYER_KBPS` table by the
/// compile-time assert below (issue #1077).
const AUDIO_SIMULCAST_LAYER_KBPS: &[u32] = &[24, 32, 50];

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
/// PR #1330). At the default (Auto) ceilings the full 24/32/50 kbps ladder
/// (~106 kbps) publishes unconditionally — a deliberate, documented cost (audio
/// is ~1-3% of call bandwidth).
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
    vad_threshold: f32,
    /// Tier-controlled audio bitrate in bps (e.g. 50000 for 50 kbps).
    /// Shared with the camera encoder's quality manager.
    /// Currently not read at runtime because the AudioWorklet does not
    /// support dynamic bitrate reconfiguration. Kept for forward-
    /// compatibility when the worklet gains that capability.
    #[allow(dead_code)]
    tier_audio_bitrate: Rc<AtomicU32>,
    /// Whether the current audio tier has FEC enabled.
    /// When true AND `AUDIO_REDUNDANCY_ENABLED`, each packet carries the
    /// previous frame as redundant data for loss recovery.
    tier_enable_fec: Rc<AtomicBool>,
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
    /// KNOWN GAP (intentional, #621): when the device is capability-gated to a
    /// SINGLE audio layer (or audio simulcast is disabled), this ceiling is a
    /// no-op — the base layer is always published and there is no upper layer to
    /// shed, so a single-encoder congested publisher cannot downshift at all. True
    /// single-encoder audio downshift requires the deferred live worklet-bitrate
    /// reconfiguration work; until that lands, congestion downshift only helps a
    /// multi-layer audio publisher.
    shared_congestion_layer_ceiling: Arc<AtomicU32>,
}

impl MicrophoneEncoder {
    /// Construct a microphone encoder.
    ///
    /// `shared_audio_tier_bitrate` and `shared_audio_tier_fec` are shared
    /// atomics owned by the `CameraEncoder`. The camera encoder's quality
    /// manager writes to these when the audio tier changes, and the
    /// microphone encoder reads them to apply the current audio settings.
    /// This avoids creating a duplicate `EncoderBitrateController` that
    /// would redundantly process the same diagnostics packets.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: VideoCallClient,
        _bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
        on_error: Callback<String>,
        vad_threshold: Option<f32>,
        shared_audio_tier_bitrate: Option<Rc<AtomicU32>>,
        shared_audio_tier_fec: Option<Rc<AtomicBool>>,
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
            vad_threshold: vad_threshold.unwrap_or(DEFAULT_VAD_THRESHOLD),
            tier_audio_bitrate: shared_audio_tier_bitrate
                .unwrap_or_else(|| Rc::new(AtomicU32::new(default_audio_bitrate_bps))),
            tier_enable_fec: shared_audio_tier_fec
                .unwrap_or_else(|| Rc::new(AtomicBool::new(default_enable_fec))),
            // User SEND audio layer-ceiling (perf-panel). Fail-open: u32::MAX =
            // Auto / no user cap until the panel writes a layer count.
            shared_user_layer_ceiling: Rc::new(AtomicU32::new(u32::MAX)),
            // CONGESTION-driven audio layer-ceiling (issue #621). Fail-open:
            // u32::MAX = no congestion cap until a self-targeted CONGESTION cut
            // drives it down. Replaced by the client-owned atom via
            // `set_congestion_layer_ceiling` so the CONGESTION dispatch arm can
            // cut it without the camera AQ loop in the path.
            shared_congestion_layer_ceiling: Arc::new(AtomicU32::new(u32::MAX)),
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
                            // This is a control message (ready, done, flushed), not an audio frame
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
            // and pass FEC/DTX settings from the initial audio tier.
            // NOTE: FEC and DTX require AudioWorklet support; the fields are
            // serialized here for forward-compatibility but the worklet currently
            // ignores them.  See audio_worklet_codec.rs for details.
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
                    return;
                }
                let now = js_sys::Date::now();
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
                }
                last_seen_ceiling.set(next_seen);
                last_congestion_ms.set(next_cut);
            });
            *congestion_recovery_holder.borrow_mut() = Some(recovery_interval);

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
        audio_congestion_recover, audio_congestion_tick, audio_layer_is_published,
        clamp_audio_layer_count, AUDIO_SIMULCAST_LAYER_KBPS, AUDIO_SIMULCAST_MAX_SUPPORTED_LAYERS,
    };
    use crate::adaptive_quality_constants::AUDIO_CONGESTION_RECOVERY_COOLDOWN_MS;

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
        // ordered lowest→highest (issue #1082).
        assert_eq!(AUDIO_SIMULCAST_LAYER_KBPS, &[24, 32, 50]);
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
        // Typical Opus frame at 50kbps, 20ms = ~125 bytes
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
