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
    AUDIO_QUALITY_TIERS, AUDIO_REDUNDANCY_ENABLED, AUDIO_RED_FORMAT, VAD_POLL_INTERVAL_MS,
};
use crate::audio_constants::{
    rms_to_intensity, AUDIO_LEVEL_DELTA_THRESHOLD, DEFAULT_VAD_THRESHOLD, VAD_FFT_SIZE,
    VAD_SMOOTHING_TIME_CONSTANT,
};
use crate::audio_worklet_codec::EncoderInitOptions;
use crate::audio_worklet_codec::{AudioWorkletCodec, CodecMessages};
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
use videocall_types::audio_rtp::AudioRtpClock;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::{
    media_packet::{media_packet::MediaType, AudioMetadata, MediaPacket},
    packet_wrapper::packet_wrapper::PacketType,
};
use videocall_types::Callback;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::AudioContext;
use web_sys::AudioContextOptions;
use web_sys::EncodedAudioChunkType;
use web_sys::MediaStream;
use web_sys::MediaStreamConstraints;
use web_sys::MediaStreamTrack;
use web_sys::MessageEvent;
use web_time::{Instant, SystemTime};

/// Holds the previous audio frame for RED-style redundancy.
#[derive(Debug)]
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

/// One encoded frame ready to be wrapped in a `MediaPacket`.
#[derive(Debug)]
pub(crate) struct OutgoingAudioFrame {
    pub sequence: u64,
    /// RTP timestamp in samples at 48 kHz (`AudioMetadata.rtp_timestamp`).
    pub rtp_timestamp: u32,
    pub primary: Vec<u8>,
    /// The previous frame, attached as RED redundancy. Never set across a
    /// send pause, because that frame is not contiguous with this one.
    pub redundant: Option<PreviousAudioFrame>,
}

/// Sequence, timestamp and redundancy state for one microphone sender
/// session. It outlives individual `start()`/`stop()` cycles (mute/unmute,
/// device switch) so neither the sequence nor the RTP timestamp ever goes
/// backward. Every call is O(1).
pub(crate) struct AudioSendState {
    clock: AudioRtpClock,
    next_sequence: u64,
    previous_frame: Option<PreviousAudioFrame>,
    /// Keep a copy of each frame for the next packet's RED payload. Off when
    /// RED is compiled out, which saves an allocation per frame.
    keep_previous_frame: bool,
}

impl AudioSendState {
    pub(crate) fn new() -> Self {
        Self::with_clock(AudioRtpClock::opus_20ms(), AUDIO_REDUNDANCY_ENABLED)
    }

    fn with_clock(clock: AudioRtpClock, keep_previous_frame: bool) -> Self {
        Self {
            clock,
            next_sequence: 0,
            previous_frame: None,
            keep_previous_frame,
        }
    }

    /// Sending stopped (mute, encoder torn down, frame dropped). The next
    /// frame's timestamp jumps by the elapsed time and carries no redundancy.
    pub(crate) fn pause(&mut self) {
        self.clock.pause();
        self.previous_frame = None;
    }

    /// Stamp one encoded 20 ms frame captured at `capture_ms` (monotonic ms).
    /// `redundancy_allowed` is the current RED/FEC setting.
    pub(crate) fn next_frame(
        &mut self,
        primary: Vec<u8>,
        capture_ms: f64,
        redundancy_allowed: bool,
    ) -> OutgoingAudioFrame {
        let stamped = self.clock.stamp(capture_ms);
        let previous = self.previous_frame.take();
        let redundant = if redundancy_allowed && !stamped.after_gap {
            previous
        } else {
            None
        };
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        if self.keep_previous_frame {
            self.previous_frame = Some(PreviousAudioFrame {
                data: primary.clone(),
                sequence,
            });
        }
        OutgoingAudioFrame {
            sequence,
            rtp_timestamp: stamped.rtp_timestamp,
            primary,
            redundant,
        }
    }
}

/// Build the (unencrypted) `MediaPacket` for one frame. `wall_clock_ms` goes
/// into `MediaPacket.timestamp` exactly as before; the media timestamp goes
/// into `AudioMetadata.rtp_timestamp`.
pub(crate) fn build_audio_media_packet(
    frame: OutgoingAudioFrame,
    wall_clock_ms: f64,
) -> MediaPacket {
    let (data, audio_format) = match &frame.redundant {
        Some(prev) => (
            pack_redundant_audio(&frame.primary, prev),
            AUDIO_RED_FORMAT.to_string(),
        ),
        None => (frame.primary, String::new()),
    };
    MediaPacket {
        user_id: Vec::new(),
        media_type: MediaType::AUDIO.into(),
        frame_type: EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Key).to_string(),
        data,
        timestamp: wall_clock_ms,
        audio_metadata: Some(AudioMetadata {
            sequence: frame.sequence,
            rtp_timestamp: Some(frame.rtp_timestamp),
            audio_format,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

pub(crate) fn transform_audio_chunk(
    frame: OutgoingAudioFrame,
    user_id: &str,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;

    let media_packet = build_audio_media_packet(frame, now_ms);
    let data = media_packet.write_to_bytes().unwrap();
    let data = aes.encrypt(&data).unwrap();
    PacketWrapper {
        data,
        user_id: user_id.as_bytes().to_vec(),
        packet_type: PacketType::MEDIA.into(),
        ..Default::default()
    }
}

pub struct MicrophoneEncoder {
    client: VideoCallClient,
    state: EncoderState,
    _on_encoder_settings_update: Option<Callback<String>>,
    codec: AudioWorkletCodec,
    on_error: Option<Callback<String>>,
    is_speaking: Rc<AtomicBool>,
    vad_interval: Rc<RefCell<Option<Interval>>>,
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
    /// Sequence / RTP timestamp / RED state for this sender session. Shared
    /// with every `start()`'s output handler so it survives mic restarts.
    send_state: Rc<RefCell<AudioSendState>>,
    /// Origin of the monotonic capture clock fed to the RTP timestamp
    /// (`performance.now()` in the browser via `web_time`).
    capture_epoch: Instant,
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
    pub fn new(
        client: VideoCallClient,
        _bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
        on_error: Callback<String>,
        vad_threshold: Option<f32>,
        shared_audio_tier_bitrate: Option<Rc<AtomicU32>>,
        shared_audio_tier_fec: Option<Rc<AtomicBool>>,
    ) -> Self {
        let default_audio_bitrate_bps = AUDIO_QUALITY_TIERS[0].bitrate_kbps * 1000;
        let default_enable_fec = AUDIO_QUALITY_TIERS[0].enable_fec;
        Self {
            client,
            state: EncoderState::new(),
            _on_encoder_settings_update: Some(on_encoder_settings_update),
            codec: AudioWorkletCodec::default(),
            on_error: Some(on_error),
            is_speaking: Rc::new(AtomicBool::new(false)),
            vad_interval: Rc::new(RefCell::new(None)),
            vad_threshold: vad_threshold.unwrap_or(DEFAULT_VAD_THRESHOLD),
            tier_audio_bitrate: shared_audio_tier_bitrate
                .unwrap_or_else(|| Rc::new(AtomicU32::new(default_audio_bitrate_bps))),
            tier_enable_fec: shared_audio_tier_fec
                .unwrap_or_else(|| Rc::new(AtomicBool::new(default_enable_fec))),
            send_state: Rc::new(RefCell::new(AudioSendState::new())),
            capture_epoch: Instant::now(),
        }
    }

    pub fn set_error_callback(&mut self, on_error: Callback<String>) {
        self.on_error = Some(on_error);
    }

    // delegates to self.state
    pub fn set_enabled(&mut self, value: bool) -> bool {
        let is_changed = self.state.set_enabled(value);
        if is_changed {
            if value {
                let _ = self.codec.start();
            } else {
                // First stop the codec to prevent new audio frames
                let _ = self.codec.stop();
                // The next frame (after unmute) jumps its RTP timestamp by the
                // muted time and carries no redundancy.
                self.send_state.borrow_mut().pause();
                // The monitoring loop in start() will detect the enabled flag change
                // and stop the microphone capture within 100ms
                if let Some(interval) = self.vad_interval.borrow_mut().take() {
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
        self.codec.destroy();
        self.send_state.borrow_mut().pause();
        if let Some(interval) = self.vad_interval.borrow_mut().take() {
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

        if self.state.switching.load(Ordering::Acquire) && self.codec.is_instantiated() {
            self.stop();
        }
        if self.state.is_enabled() && self.codec.is_instantiated() {
            return;
        }
        let aes = client.aes();
        let on_error = self.on_error.clone();
        let EncoderState {
            enabled, switching, ..
        } = self.state.clone();

        // Clone atomic values for use in different closures
        let enabled_for_handler = enabled.clone();
        let enable_fec_for_handler = self.tier_enable_fec.clone();

        // A fresh codec instance: whatever time passed since the last frame
        // sent (if any) is a gap in the media timeline.
        self.send_state.borrow_mut().pause();

        let audio_output_handler = {
            log::info!("Starting Microphone audio encoder with AnalyserNode VAD");
            let client_for_send = client.clone();
            // Sequence, RTP timestamp and RED buffer live on the encoder so
            // they continue across restarts instead of resetting to 0.
            let send_state = self.send_state.clone();
            let capture_epoch = self.capture_epoch;

            Box::new(move |chunk: MessageEvent| {
                // Check if encoder should stop
                if !enabled_for_handler.load(Ordering::Acquire) {
                    // Dropping a frame is a send pause.
                    send_state.borrow_mut().pause();
                    log::debug!(
                        "Audio handler stopping: enabled={}",
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
                    // Redundancy follows the AUDIO_REDUNDANCY_ENABLED constant
                    // and the current tier's enable_fec flag.
                    let use_redundancy =
                        AUDIO_REDUNDANCY_ENABLED && enable_fec_for_handler.load(Ordering::Relaxed);
                    // The worklet posts one 20 ms Opus packet per message and
                    // no capture timestamp, so arrival time on a monotonic
                    // clock is the best capture-time proxy. It only matters on
                    // the first frame after a pause; while running, each frame
                    // advances the RTP timestamp by exactly 960.
                    let capture_ms = capture_epoch.elapsed().as_secs_f64() * 1_000.0;
                    let frame = send_state.borrow_mut().next_frame(
                        data.to_vec(),
                        capture_ms,
                        use_redundancy,
                    );
                    let packet: PacketWrapper = transform_audio_chunk(frame, &user_id, aes.clone());
                    client_for_send.send_media_packet(packet);
                } else {
                    log::error!("Received non-MessageEvent: {chunk:?}");
                }
            })
        };

        let codec = self.codec.clone();
        let is_speaking_for_vad = self.is_speaking.clone();
        let client_for_vad = client.clone();
        let vad_interval_holder = self.vad_interval.clone();
        let vad_threshold = self.vad_threshold;

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

            // Force exact deviceId match (avoids falling back to the default mic).
            if device_id.is_empty() {
                log::warn!("Microphone device_id is empty, using default constraint");
                constraints.set_audio(&JsValue::TRUE);
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
                constraints.set_audio(&media_info.into());
            }

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

            // Use the microphone's sample rate for the AudioContext to avoid Firefox sample rate mismatch
            let options = AudioContextOptions::new();
            options.set_sample_rate(input_rate as f32);

            let context = match AudioContext::new_with_context_options(&options) {
                Ok(ctx) => ctx,
                Err(e) => {
                    if let Some(cb) = &on_error {
                        cb.emit(format!("Failed to create audio context: {e:?}"));
                    }
                    return;
                }
            };
            log::info!("Created AudioContext with sample rate: {input_rate} Hz");

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

            let worklet = match codec
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
            codec.set_onmessage(output_handler.as_ref().unchecked_ref());
            output_handler.forget();

            // Use the tier-controlled bitrate (defaults to AUDIO_QUALITY_TIERS[0]),
            // and pass FEC/DTX settings from the initial audio tier.
            // NOTE: FEC and DTX require AudioWorklet support; the fields are
            // serialized here for forward-compatibility but the worklet currently
            // ignores them.  See audio_worklet_codec.rs for details.
            let initial_tier = &AUDIO_QUALITY_TIERS[0];
            let _ = codec.send_message(&CodecMessages::Init {
                options: Some(EncoderInitOptions {
                    encoder_frame_size: Some(20), // 20ms frames for 50Hz rate
                    original_sample_rate: Some(input_rate),
                    encoder_bit_rate: Some(initial_tier.bitrate_kbps * 1000),
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

                    // Stop the media track
                    audio_track.stop();

                    // Close the AudioContext
                    if let Err(e) = context.close() {
                        log::error!("Error closing AudioContext: {e:?}");
                    }

                    // Destroy the codec
                    codec.destroy();

                    log::info!("Microphone audio encoder stopped and cleaned up");
                    break;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::neteq_audio_decoder::NetEqAudioPeerDecoder;
    use wasm_bindgen_test::*;

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

/// Native (`cargo test -p videocall-client --lib`) tests for the sender's
/// sequence / RTP timestamp / RED decisions. No browser APIs are touched.
#[cfg(test)]
mod rtp_send_tests {
    use super::*;
    use crate::decode::neteq_audio_decoder::plan_neteq_inserts;

    const FRAME: u32 = videocall_types::audio_rtp::OPUS_RTP_SAMPLES_PER_20MS_FRAME;

    fn red_state() -> AudioSendState {
        AudioSendState::with_clock(AudioRtpClock::opus_20ms(), true)
    }

    #[test]
    fn consecutive_frames_advance_rtp_timestamp_by_960() {
        let mut state = AudioSendState::new();
        let mut capture_ms = 0.0;
        for i in 0..200u32 {
            let frame = state.next_frame(vec![i as u8], capture_ms, false);
            assert_eq!(frame.sequence, u64::from(i));
            assert_eq!(frame.rtp_timestamp, i * FRAME);
            capture_ms += 20.0;
        }
    }

    #[test]
    fn packet_carries_rtp_timestamp_and_keeps_wall_clock_timestamp() {
        let mut state = AudioSendState::new();
        state.next_frame(vec![1], 0.0, false);
        let frame = state.next_frame(vec![2], 20.0, false);
        let packet = build_audio_media_packet(frame, 1_763_251_200_000.0);
        let meta = packet.audio_metadata.as_ref().unwrap();
        assert_eq!(meta.rtp_timestamp, Some(FRAME));
        assert_eq!(meta.sequence, 1);
        assert_eq!(packet.timestamp, 1_763_251_200_000.0);
        assert_eq!(packet.data, vec![2]);
        assert!(meta.audio_format.is_empty());
    }

    #[test]
    fn pause_advances_by_gap_in_whole_frames_and_sequence_continues() {
        let mut state = AudioSendState::new();
        let a = state.next_frame(vec![0], 1_000.0, false);
        let b = state.next_frame(vec![1], 1_020.0, false);
        assert_eq!(b.rtp_timestamp, a.rtp_timestamp + FRAME);
        // Muted for 3.01 s after frame b: 150 whole frames of wall time.
        state.pause();
        let c = state.next_frame(vec![2], 4_030.0, false);
        assert_eq!(c.rtp_timestamp, b.rtp_timestamp + 150 * FRAME);
        assert_eq!(c.sequence, b.sequence + 1, "sequence does not restart");
        let d = state.next_frame(vec![3], 4_050.0, false);
        assert_eq!(d.rtp_timestamp, c.rtp_timestamp + FRAME);
    }

    #[test]
    fn redundancy_attached_while_running_but_never_across_a_gap() {
        let mut state = red_state();
        let first = state.next_frame(vec![10], 0.0, true);
        assert!(first.redundant.is_none(), "nothing to repeat yet");
        let second = state.next_frame(vec![11], 20.0, true);
        let red = second.redundant.as_ref().expect("previous frame attached");
        assert_eq!(red.sequence, first.sequence);
        assert_eq!(red.data, vec![10]);

        state.pause();
        let resumed = state.next_frame(vec![12], 2_000.0, true);
        assert!(resumed.redundant.is_none(), "no RED across the gap");
        let after = state.next_frame(vec![13], 2_020.0, true);
        assert_eq!(after.redundant.as_ref().unwrap().sequence, resumed.sequence);
    }

    #[test]
    fn redundancy_off_attaches_nothing() {
        let mut state = red_state();
        state.next_frame(vec![1], 0.0, false);
        assert!(state.next_frame(vec![2], 20.0, false).redundant.is_none());
    }

    #[test]
    fn rtp_timestamp_wraps_modulo_2_pow_32() {
        let start = u32::MAX - FRAME + 1;
        let mut state = AudioSendState::with_clock(
            AudioRtpClock::opus_20ms().with_initial_timestamp(start),
            false,
        );
        assert_eq!(state.next_frame(vec![], 0.0, false).rtp_timestamp, start);
        assert_eq!(state.next_frame(vec![], 20.0, false).rtp_timestamp, 0);
        assert_eq!(state.next_frame(vec![], 40.0, false).rtp_timestamp, FRAME);
    }

    /// Sender and receiver agree end to end: the receiver passes the field
    /// through, and a RED-recovered frame lands exactly where the lost frame
    /// would have.
    #[test]
    fn receiver_sees_sender_timestamps_including_red_recovery() {
        let mut state = red_state();
        let mut sent = Vec::new();
        for i in 0..5u8 {
            let frame = state.next_frame(vec![i], f64::from(i) * 20.0, true);
            sent.push(build_audio_media_packet(frame, 0.0));
        }
        // Packet 3 was lost; packet 4 carries it as redundancy.
        let meta4 = sent[4].audio_metadata.as_ref().unwrap();
        let plan = plan_neteq_inserts(meta4, &sent[4].data, |s| s != 3);
        assert_eq!(plan.primary.timestamp, 4 * FRAME);
        let recovered = plan.recovered.unwrap();
        assert_eq!(recovered.seq, 3);
        assert_eq!(
            Some(recovered.timestamp),
            sent[3].audio_metadata.as_ref().unwrap().rtp_timestamp
        );
    }
}
