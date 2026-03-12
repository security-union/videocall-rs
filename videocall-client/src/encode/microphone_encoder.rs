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

use crate::adaptive_quality_constants::{AUDIO_QUALITY_TIERS, VAD_POLL_INTERVAL_MS};
use crate::audio_constants::{
    rms_to_intensity, AUDIO_LEVEL_DELTA_THRESHOLD, DEFAULT_VAD_THRESHOLD, VAD_FFT_SIZE,
    VAD_SMOOTHING_TIME_CONSTANT,
};
use crate::audio_worklet_codec::EncoderInitOptions;
use crate::audio_worklet_codec::{AudioWorkletCodec, CodecMessages};
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::crypto::aes::Aes128State;
use crate::diagnostics::EncoderBitrateController;
use crate::encode::encoder_state::EncoderState;
use crate::wrappers::EncodedAudioChunkTypeWrapper;
use crate::VideoCallClient;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gloo::timers::callback::Interval;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::Uint8Array;
use protobuf::Message;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
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
use web_time::SystemTime;

pub fn transform_audio_chunk(
    chunk: &Uint8Array,
    user_id: &str,
    sequence: u64,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    // chunk length in bytes

    let media_packet: MediaPacket = MediaPacket {
        user_id: Vec::new(),
        media_type: MediaType::AUDIO.into(),
        frame_type: EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Key).to_string(),
        data: chunk.to_vec(),
        timestamp: now_ms,
        audio_metadata: Some(AudioMetadata {
            sequence,
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
    /// Updated by the diagnostics loop when the audio tier changes.
    tier_audio_bitrate: Rc<AtomicU32>,
}

impl MicrophoneEncoder {
    pub fn new(
        client: VideoCallClient,
        _bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
        on_error: Callback<String>,
        vad_threshold: Option<f32>,
    ) -> Self {
        let default_audio_bitrate_bps = AUDIO_QUALITY_TIERS[0].bitrate_kbps * 1000;
        Self {
            client,
            state: EncoderState::new(),
            _on_encoder_settings_update: Some(on_encoder_settings_update),
            codec: AudioWorkletCodec::default(),
            on_error: Some(on_error),
            is_speaking: Rc::new(AtomicBool::new(false)),
            vad_interval: Rc::new(RefCell::new(None)),
            vad_threshold: vad_threshold.unwrap_or(DEFAULT_VAD_THRESHOLD),
            tier_audio_bitrate: Rc::new(AtomicU32::new(default_audio_bitrate_bps)),
        }
    }

    pub fn set_error_callback(&mut self, on_error: Callback<String>) {
        self.on_error = Some(on_error);
    }

    pub fn set_encoder_control(
        &mut self,
        mut diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        let tier_audio_bitrate = self.tier_audio_bitrate.clone();
        // We create a bitrate controller here to feed the adaptive quality manager.
        // The audio encoder does not do PID-based bitrate control itself, but we need
        // the quality manager to track conditions and select audio tiers.
        wasm_bindgen_futures::spawn_local(async move {
            // Use a dummy FPS target for the PID -- the audio encoder doesn't use PID output,
            // but the quality manager inside EncoderBitrateController needs to process packets.
            let dummy_fps = Rc::new(AtomicU32::new(50)); // ~50 audio frames/sec at 20ms
            let initial_bitrate = AUDIO_QUALITY_TIERS[0].bitrate_kbps;
            let mut encoder_control = EncoderBitrateController::new(initial_bitrate, dummy_fps);

            while let Some(packet) = diagnostics_receiver.next().await {
                // Feed the packet to get the quality manager to process it.
                let _ = encoder_control.process_diagnostics_packet(packet);

                // Check if audio tier changed and update the shared bitrate atomic.
                if encoder_control.take_tier_changed() {
                    let audio_tier = encoder_control.current_audio_tier();
                    let new_bitrate_bps = audio_tier.bitrate_kbps * 1000;
                    tier_audio_bitrate.store(new_bitrate_bps, Ordering::Relaxed);
                    log::info!(
                        "MicrophoneEncoder: audio tier changed to '{}' ({}kbps)",
                        audio_tier.label,
                        audio_tier.bitrate_kbps,
                    );
                    // TODO: Apply enable_dtx and enable_fec when WebCodecs AudioEncoder
                    // supports dynamic reconfiguration of these parameters. Currently
                    // the Opus encoder worklet only accepts bitrate changes via Init.
                }
            }
        });
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

        let audio_output_handler = {
            log::info!("Starting Microphone audio encoder with AnalyserNode VAD");
            let mut sequence_number = 0;
            let client_for_send = client.clone();

            Box::new(move |chunk: MessageEvent| {
                // Check if encoder should stop
                if !enabled_for_handler.load(Ordering::Acquire) {
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
                    let packet: PacketWrapper =
                        transform_audio_chunk(&data, &user_id, sequence_number, aes.clone());
                    client_for_send.send_media_packet(packet);
                    sequence_number += 1;
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

            let _ = codec.send_message(&CodecMessages::Init {
                options: Some(EncoderInitOptions {
                    encoder_frame_size: Some(20), // 20ms frames for 50Hz rate
                    original_sample_rate: Some(input_rate),
                    encoder_bit_rate: Some(50_000_u32),
                    encoder_sample_rate: Some(AUDIO_SAMPLE_RATE),
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
