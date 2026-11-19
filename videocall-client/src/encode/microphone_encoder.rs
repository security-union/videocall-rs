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
use crate::audio_worklet_codec::{
    AudioWorkletCodec, CodecStartError, EncodedPacket, EncoderInitOptions, EncoderWorker,
};
use crate::crypto::aes::Aes128State;
use crate::encode::encoder_state::EncoderState;
use crate::encode::mic_opus::{BitrateFollower, PacketClock};
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
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::{
    media_packet::{media_packet::MediaType, AudioMetadata, MediaPacket},
    packet_wrapper::packet_wrapper::PacketType,
};
use videocall_types::Callback;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::AudioContext;
use web_sys::AudioContextOptions;
use web_sys::EncodedAudioChunkType;
use web_sys::MediaStream;
use web_sys::MediaStreamConstraints;
use web_sys::MediaStreamTrack;
use web_time::SystemTime;

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

/// Build the wire packet for one Opus packet.
///
/// `timestamp_ms` goes into `MediaPacket.timestamp`. For audio it is the
/// frame-exact stamp from [`PacketClock`] (session start + frame index *
/// 20 ms), which receivers turn into the RTP timestamp.
pub fn transform_audio_chunk(
    chunk: &Uint8Array,
    user_id: &str,
    sequence: u64,
    timestamp_ms: f64,
    aes: Rc<Aes128State>,
    previous_frame: Option<&PreviousAudioFrame>,
) -> PacketWrapper {
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
        timestamp: timestamp_ms,
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
        ..Default::default()
    }
}

/// Destroys a codec handle when dropped.
struct DestroyOnDrop(AudioWorkletCodec);

impl Drop for DestroyOnDrop {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

pub struct MicrophoneEncoder {
    client: VideoCallClient,
    state: EncoderState,
    _on_encoder_settings_update: Option<Callback<String>>,
    /// The current capture session. Replaced by every `start()`.
    codec: AudioWorkletCodec,
    /// The Opus encoder worker, kept across sessions.
    encoder_worker: EncoderWorker,
    /// Stamps `MediaPacket.timestamp`; kept across sessions so a new
    /// session never stamps behind the previous one.
    packet_clock: Rc<RefCell<PacketClock>>,
    on_error: Option<Callback<String>>,
    is_speaking: Rc<AtomicBool>,
    /// The current session's VAD timer. Replaced by every `start()`, so an
    /// old session's cleanup only clears its own.
    vad_interval: Rc<RefCell<Option<Interval>>>,
    vad_threshold: f32,
    /// Tier-controlled audio bitrate in bps (e.g. 50000 for 50 kbps).
    /// Shared with the camera encoder's quality manager. Read when a session
    /// starts and polled while it runs; changes go to the worker as a
    /// `reconfigure`.
    tier_audio_bitrate: Rc<AtomicU32>,
    /// Whether the current audio tier has FEC enabled.
    /// When true AND `AUDIO_REDUNDANCY_ENABLED`, each packet carries the
    /// previous frame as redundant data for loss recovery.
    tier_enable_fec: Rc<AtomicBool>,
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
        // Start downloading the encoder worker now, so the first unmute
        // doesn't wait for it. (Browser only: web-sys calls panic natively.)
        let encoder_worker = EncoderWorker::default();
        #[cfg(target_arch = "wasm32")]
        encoder_worker.prefetch();
        Self {
            client,
            state: EncoderState::new(),
            _on_encoder_settings_update: Some(on_encoder_settings_update),
            codec: AudioWorkletCodec::default(),
            encoder_worker,
            packet_clock: Rc::new(RefCell::new(PacketClock::default())),
            on_error: Some(on_error),
            is_speaking: Rc::new(AtomicBool::new(false)),
            vad_interval: Rc::new(RefCell::new(None)),
            vad_threshold: vad_threshold.unwrap_or(DEFAULT_VAD_THRESHOLD),
            tier_audio_bitrate: shared_audio_tier_bitrate
                .unwrap_or_else(|| Rc::new(AtomicU32::new(default_audio_bitrate_bps))),
            tier_enable_fec: shared_audio_tier_fec
                .unwrap_or_else(|| Rc::new(AtomicBool::new(default_enable_fec))),
        }
    }

    pub fn set_error_callback(&mut self, on_error: Callback<String>) {
        self.on_error = Some(on_error);
    }

    /// Stop the session's VAD timer and clear the speaking indicator. Only
    /// for when the session ends; a mute keeps the timer (see set_enabled).
    fn reset_vad(&self) {
        if let Some(interval) = self.vad_interval.borrow_mut().take() {
            drop(interval);
        }
        self.clear_speaking();
    }

    fn clear_speaking(&self) {
        self.is_speaking.store(false, Ordering::Relaxed);
        self.client.set_speaking(false);
        self.client.set_audio_level(0.0);
    }

    // delegates to self.state
    pub fn set_enabled(&mut self, value: bool) -> bool {
        let is_changed = self.state.set_enabled(value);
        if is_changed && !value {
            // The packet handler drops frames once `enabled` is false, and
            // the monitoring loop in start() tears down the capture, the
            // session and its VAD timer within VAD_POLL_INTERVAL_MS. The
            // timer stays until then: an unmute inside that window keeps
            // the session (start() returns early), and needs its VAD.
            self.clear_speaking();
        }
        is_changed
    }

    pub fn select(&mut self, device: String) -> bool {
        self.state.select(device)
    }

    pub fn stop(&mut self) {
        self.state.stop();
        self.codec.destroy();
        self.reset_vad();
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

        if self.state.switching.swap(false, Ordering::AcqRel) {
            // Device switch: end the current session (starting or running)
            // and keep `enabled` as it is. The old session's monitor loop
            // sees its codec destroyed and releases its track and context.
            self.codec.destroy();
            self.reset_vad();
        }
        if self.codec.is_instantiated() {
            return;
        }
        // A fresh handle per session: the previous session's cleanup can
        // only destroy its own worklet and worker session, never this one's.
        let codec = AudioWorkletCodec::default();
        codec.mark_starting();
        self.codec = codec.clone();
        let vad_interval_holder = Rc::new(RefCell::new(None));
        self.vad_interval = vad_interval_holder.clone();
        self.packet_clock.borrow_mut().start_session();

        let aes = client.aes();
        let on_error = self.on_error.clone();
        let enabled = self.state.enabled.clone();
        let encoder_worker = self.encoder_worker.clone();
        let packet_clock = self.packet_clock.clone();
        let tier_bitrate = self.tier_audio_bitrate.clone();

        // Clone atomic values for use in different closures
        let enabled_for_handler = enabled.clone();
        let enable_fec_for_handler = self.tier_enable_fec.clone();

        let audio_output_handler = {
            log::info!("Starting Microphone audio encoder with AnalyserNode VAD");
            let mut sequence_number: u64 = 0;
            let client_for_send = client.clone();
            // Buffer for RED-style redundancy: stores the previous frame's
            // encoded data and sequence number so it can be included in the
            // next packet for loss recovery.
            let mut previous_frame: Option<PreviousAudioFrame> = None;
            let mut previous_frame_index: Option<u64> = None;

            Box::new(move |packet: EncodedPacket| {
                // Check if encoder should stop
                if !enabled_for_handler.load(Ordering::Acquire) {
                    return;
                }
                let EncodedPacket { frame_index, data } = packet;

                // One Opus packet from the encoder worker. During DTX silence
                // the worker sends none: the sequence stays contiguous, and
                // the frame-exact stamp carries the pause.
                let timestamp_ms = packet_clock.borrow_mut().stamp(now_ms(), frame_index);

                // Redundancy only when the previous packet is the previous
                // frame: after a DTX gap the receiver could not place it.
                let adjacent = previous_frame_index.is_some_and(|prev| prev + 1 == frame_index);
                let use_redundancy = AUDIO_REDUNDANCY_ENABLED
                    && enable_fec_for_handler.load(Ordering::Relaxed)
                    && adjacent;
                let red_ref = if use_redundancy {
                    previous_frame.as_ref()
                } else {
                    None
                };

                let wire: PacketWrapper = transform_audio_chunk(
                    &data,
                    &user_id,
                    sequence_number,
                    timestamp_ms,
                    aes.clone(),
                    red_ref,
                );
                client_for_send.send_media_packet(wire);

                // Store current frame as the previous frame for the next
                // iteration's redundancy payload.
                previous_frame = Some(PreviousAudioFrame {
                    data: data.to_vec(),
                    sequence: sequence_number,
                });
                previous_frame_index = Some(frame_index);
                sequence_number += 1;
            }) as Box<dyn FnMut(EncodedPacket)>
        };

        let is_speaking_for_vad = self.is_speaking.clone();
        let client_for_vad = client.clone();
        let vad_threshold = self.vad_threshold;

        wasm_bindgen_futures::spawn_local(async move {
            // Whatever way this task ends (an early return or the stop
            // below), the session's worklet and worker session go with it,
            // and is_instantiated() turns false so start() can run again.
            let _destroy_codec = DestroyOnDrop(codec.clone());
            // Errors of a session that was stopped meanwhile (mute, device
            // switch, the encoder dropped with its UI) are not reported.
            let report = |message: String| {
                if !codec.is_instantiated() {
                    log::info!("Microphone session stopped; not reporting: {message}");
                    return;
                }
                if let Some(cb) = &on_error {
                    cb.emit(message);
                }
            };
            let navigator = window().navigator();
            let media_devices = match navigator.media_devices() {
                Ok(md) => md,
                Err(e) => {
                    report(format!("Failed to access media devices: {e:?}"));
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
                if let Err(e) = js_sys::Reflect::set(
                    &exact,
                    &JsValue::from_str("exact"),
                    &JsValue::from_str(&device_id),
                ) {
                    report(format!("Failed to build microphone constraints: {e:?}"));
                    return;
                }

                log::info!("MicrophoneEncoder: deviceId.exact = {}", device_id);
                media_info.set_device_id(&exact.into());
                constraints.set_audio(&media_info.into());
            }

            constraints.set_video(&Boolean::from(false));
            let devices_query = match media_devices.get_user_media_with_constraints(&constraints) {
                Ok(p) => p,
                Err(e) => {
                    report(format!("Microphone access failed: {e:?}"));
                    return;
                }
            };
            let device = match JsFuture::from(devices_query).await {
                Ok(ok) => ok.unchecked_into::<MediaStream>(),
                Err(e) => {
                    report(format!("Failed to get microphone stream: {e:?}"));
                    return;
                }
            };

            let audio_track = device
                .get_audio_tracks()
                .find(&mut |_: JsValue, _: u32, _: Array| true)
                .unchecked_into::<MediaStreamTrack>();

            // Stopped (mute, device switch) while the permission prompt or
            // device open was pending: release the device quietly.
            if !codec.is_instantiated() {
                audio_track.stop();
                return;
            }
            // From here on, every early return stops the track.
            let fail = |message: String| {
                audio_track.stop();
                report(message);
            };

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
                                fail(format!("Could not determine microphone sample rate: {e:?}"));
                                return;
                            }
                        }
                    }
                },
                Err(e) => {
                    fail(format!("Failed reading microphone settings: {e:?}"));
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
                    fail(format!("Failed to create audio context: {e:?}"));
                    return;
                }
            };
            // The browser may not honor the requested rate; the worklet
            // captures at whatever the context actually runs at.
            let context_rate = context.sample_rate() as u32;
            log::info!("Created AudioContext with sample rate: {context_rate} Hz");
            let fail = |message: String| {
                let _ = context.close();
                fail(message);
            };

            let analyser = match context.create_analyser() {
                Ok(a) => a,
                Err(e) => {
                    fail(format!("Failed to create analyser: {e:?}"));
                    return;
                }
            };
            analyser.set_fft_size(VAD_FFT_SIZE);
            analyser.set_smoothing_time_constant(VAD_SMOOTHING_TIME_CONSTANT);

            // ropus in the encoder worker: 48 kHz mono VoIP, 20 ms frames,
            // the current audio tier's bitrate, DTX from the initial tier.
            // Opus in-band FEC stays off; the tier's enable_fec drives RED.
            let initial_bitrate = tier_bitrate.load(Ordering::Relaxed);
            let options = EncoderInitOptions {
                input_sample_rate: context_rate,
                bitrate_bps: initial_bitrate,
                dtx: AUDIO_QUALITY_TIERS[0].enable_dtx,
            };
            let on_worker_error = on_error.clone();
            let worklet = match codec
                .create(
                    &context,
                    &encoder_worker,
                    options,
                    audio_output_handler,
                    Box::new(move |message: String| {
                        log::error!("{message}");
                        if let Some(cb) = &on_worker_error {
                            cb.emit(message);
                        }
                    }),
                )
                .await
            {
                Ok(node) => node,
                Err(CodecStartError::Cancelled) => {
                    log::info!("Microphone encoder stopped while starting");
                    audio_track.stop();
                    let _ = context.close();
                    return;
                }
                Err(CodecStartError::Failed(e)) => {
                    fail(format!("Failed to initialize audio encoder: {e}"));
                    return;
                }
            };

            let source_node = match context.create_media_stream_source(&device) {
                Ok(s) => s,
                Err(e) => {
                    fail(format!("Failed to create media source: {e:?}"));
                    return;
                }
            };
            let gain_node = match context.create_gain() {
                Ok(g) => g,
                Err(e) => {
                    fail(format!("Failed to create gain node: {e:?}"));
                    return;
                }
            };
            if let Err(e) = source_node
                .connect_with_audio_node(&gain_node)
                .and_then(|g| g.connect_with_audio_node(&analyser))
                .and_then(|a| a.connect_with_audio_node(&worklet))
            {
                fail(format!("Failed to connect audio graph: {e:?}"));
                return;
            }

            let buffer_length = analyser.frequency_bin_count() as usize;
            let data_array = Rc::new(RefCell::new(vec![0.0f32; buffer_length]));

            let enabled_check = enabled.clone();
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
                if !enabled_check.load(Ordering::Acquire) {
                    // Reset audio level to zero when mic is disabled
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

            // Monitor for stop conditions and clean up when needed. Also
            // carries audio tier changes to the encoder.
            let mut bitrate = BitrateFollower::new(initial_bitrate);
            loop {
                gloo_timers::future::TimeoutFuture::new(VAD_POLL_INTERVAL_MS).await;

                let disabled = !enabled.load(Ordering::Acquire);
                // Destroyed by stop(), a device switch, or a worker failure.
                let superseded = !codec.is_instantiated();
                if disabled || superseded {
                    log::info!("Stopping Microphone audio encoder");

                    // Only this session's VAD timer; a newer session has its own.
                    if let Some(interval) = vad_interval_holder.borrow_mut().take() {
                        drop(interval);
                    }
                    // No UI calls here: set_enabled(false) and stop() already
                    // cleared the indicators, a newer session owns them after
                    // a switch, and after a drop the UI may be gone.
                    if disabled {
                        is_speaking_for_vad.store(false, Ordering::Relaxed);
                    }

                    audio_track.stop();
                    if let Err(e) = context.close() {
                        log::error!("Error closing AudioContext: {e:?}");
                    }
                    // End the worker session and detach the worklet.
                    codec.destroy();

                    log::info!("Microphone audio encoder stopped and cleaned up");
                    break;
                }

                if let Some(bps) = bitrate.poll(tier_bitrate.load(Ordering::Relaxed)) {
                    log::info!("Microphone encoder bitrate -> {bps} bps");
                    codec.set_bitrate(bps);
                }
            }
        });
    }
}

impl Drop for MicrophoneEncoder {
    /// Ends the session and the worker without touching the UI: the
    /// client's speaking and audio-level callbacks may write to UI state
    /// that is already gone (a Dioxus signal write then panics in wasm).
    fn drop(&mut self) {
        if let Some(interval) = self.vad_interval.borrow_mut().take() {
            drop(interval);
        }
        self.codec.destroy();
        self.encoder_worker.terminate();
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
