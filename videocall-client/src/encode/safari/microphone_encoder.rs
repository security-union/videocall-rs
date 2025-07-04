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

use crate::audio_worklet_codec::EncoderInitOptions;
use crate::audio_worklet_codec::{AudioWorkletCodec, CodecMessages};
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::crypto::aes::Aes128State;
use crate::encode::encoder_state::EncoderState;
use crate::wrappers::EncodedAudioChunkTypeWrapper;
use crate::VideoCallClient;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::Uint8Array;
use protobuf::Message;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::{
    media_packet::{media_packet::MediaType, AudioMetadata, MediaPacket},
    packet_wrapper::packet_wrapper::PacketType,
};
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
use yew::Callback;

pub const AUDIO_BITRATE_KBPS: u32 = 65u32;

pub fn transform_audio_chunk(
    chunk: &Uint8Array,
    email: &str,
    sequence: u64,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;

    let media_packet: MediaPacket = MediaPacket {
        email: email.to_owned(),
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
        email: media_packet.email,
        packet_type: PacketType::MEDIA.into(),
        ..Default::default()
    }
}

pub struct MicrophoneEncoder {
    client: VideoCallClient,
    state: EncoderState,
    _on_encoder_settings_update: Option<Callback<String>>,
    codec: AudioWorkletCodec,
}

impl MicrophoneEncoder {
    pub fn new(
        client: VideoCallClient,
        _bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
    ) -> Self {
        Self {
            client,
            state: EncoderState::new(),
            _on_encoder_settings_update: Some(on_encoder_settings_update),
            codec: AudioWorkletCodec::default(),
        }
    }

    pub fn set_encoder_control(
        &mut self,
        mut diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        // Consume the diagnostics receiver in an async task to prevent "receiver is gone" errors
        // For Safari encoder, we'll just consume the packets without processing them for now
        // since Safari encoder doesn't support dynamic bitrate control yet
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(_packet) = diagnostics_receiver.next().await {
                // TODO: Implement bitrate control for Safari encoder if needed
                // For now, just consume the packets to prevent the "receiver is gone" error
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
    }

    pub fn start(&mut self) {
        let user_id = self.client.userid().clone();
        let client = self.client.clone();
        let device_id = if let Some(mic) = &self.state.selected {
            mic.to_string()
        } else {
            return;
        };

        // Don't start if not enabled - this is the key fix
        if !self.state.is_enabled() {
            log::debug!("Safari encoder start() called but encoder is not enabled");
            return;
        }

        if self.state.switching.load(Ordering::Acquire) && self.codec.is_instantiated() {
            self.stop();
        }
        if self.state.is_enabled() && self.codec.is_instantiated() {
            return;
        }
        let aes = client.aes();
        let EncoderState {
            destroy,
            enabled,
            switching,
            ..
        } = self.state.clone();

        // Clone atomic values for use in different closures
        let destroy_for_handler = destroy.clone();
        let enabled_for_handler = enabled.clone();

        let audio_output_handler = {
            log::info!("Starting Safari audio encoder");
            let mut sequence_number = 0;

            Box::new(move |chunk: MessageEvent| {
                // Check if encoder should stop
                if destroy_for_handler.load(Ordering::Acquire)
                    || !enabled_for_handler.load(Ordering::Acquire)
                {
                    log::debug!(
                        "Audio handler stopping: destroy={}, enabled={}",
                        destroy_for_handler.load(Ordering::Acquire),
                        enabled_for_handler.load(Ordering::Acquire)
                    );
                    return;
                }

                // Check if this is an actual audio frame message (not control messages)
                if let Ok(message_type) = js_sys::Reflect::get(&chunk.data(), &"message".into()) {
                    if let Some(msg_str) = message_type.as_string() {
                        if msg_str != "page" {
                            // This is a control message (ready, done, flushed), not an audio frame
                            log::debug!("Received control message: {}", msg_str);
                            return;
                        }
                    }
                }

                let data = js_sys::Reflect::get(&chunk.data(), &"page".into()).unwrap();
                if let Ok(data) = data.dyn_into::<Uint8Array>() {
                    let packet: PacketWrapper =
                        transform_audio_chunk(&data, &user_id, sequence_number, aes.clone());
                    client.send_packet(packet);
                    sequence_number += 1;
                    log::debug!("Sent audio frame with sequence: {}", sequence_number - 1);
                } else {
                    log::error!("Received non-MessageEvent: {:?}", chunk);
                }
            })
        };

        let codec = self.codec.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();
            let media_devices = navigator.media_devices().unwrap();
            let constraints = MediaStreamConstraints::new();
            let media_info = web_sys::MediaTrackConstraints::new();
            media_info.set_device_id(&device_id.into());

            constraints.set_audio(&media_info.into());
            constraints.set_video(&Boolean::from(false));
            let devices_query = media_devices
                .get_user_media_with_constraints(&constraints)
                .unwrap();
            let device = JsFuture::from(devices_query)
                .await
                .unwrap()
                .unchecked_into::<MediaStream>();

            let audio_track = Box::new(
                device
                    .get_audio_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<MediaStreamTrack>(),
            );

            let track_settings = audio_track.get_settings();

            // Sample Rate hasn't been added to the web_sys crate
            let input_rate: u32 =
                js_sys::Reflect::get(&track_settings, &JsValue::from_str("sampleRate"))
                    .unwrap()
                    .as_f64()
                    .unwrap() as u32;

            log::info!("Microphone input sample rate: {} Hz", input_rate);

            // Use the microphone's sample rate for the AudioContext to avoid Firefox sample rate mismatch
            let options = AudioContextOptions::new();
            options.set_sample_rate(input_rate as f32);

            let context = AudioContext::new_with_context_options(&options).unwrap();
            log::info!("Created AudioContext with sample rate: {} Hz", input_rate);

            let worklet = codec
                .create_node(
                    &context,
                    "/encoderWorker.min.js",
                    "encoder-worklet",
                    AUDIO_CHANNELS,
                )
                .await
                .unwrap();

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

            let source_node = context.create_media_stream_source(&device).unwrap();
            let gain_node = context.create_gain().unwrap();
            let _ = source_node
                .connect_with_audio_node(&gain_node)
                .unwrap()
                .connect_with_audio_node(&worklet);

            // Monitor for stop conditions and clean up when needed
            let check_interval = 100; // Check every 100ms
            let destroy_check = destroy.clone();
            let enabled_check = enabled.clone();
            let switching_check = switching.clone();
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
                if destroy_check.load(Ordering::Acquire)
                    || !enabled_check.load(Ordering::Acquire)
                    || switching_check.load(Ordering::Acquire)
                {
                    log::info!("Stopping Safari audio encoder");
                    switching_check.store(false, Ordering::Release);

                    // Stop the media track
                    audio_track.stop();

                    // Close the AudioContext
                    if let Err(e) = context.close() {
                        log::error!("Error closing AudioContext: {:?}", e);
                    }

                    // Destroy the codec
                    codec.destroy();

                    log::info!("Safari audio encoder stopped and cleaned up");
                    break;
                }
            }
        });
    }
}
