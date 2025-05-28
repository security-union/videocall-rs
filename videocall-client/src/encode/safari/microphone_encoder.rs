use crate::encode::encoder_state::EncoderState;
use futures::channel::mpsc::UnboundedReceiver;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::Uint8Array;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::packet_wrapper::PacketWrapper;
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
use yew::Callback;

pub const AUDIO_BITRATE_KBPS: u32 = 65u32;
use crate::audio_worklet_codec::EncoderInitOptions;
use crate::audio_worklet_codec::{AudioWorkletCodec, CodecMessages};
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::crypto::aes::Aes128State;
use crate::wrappers::EncodedAudioChunkTypeWrapper;
use crate::VideoCallClient;
use protobuf::Message;
use videocall_types::protos::{
    media_packet::{media_packet::MediaType, MediaPacket, VideoMetadata},
    packet_wrapper::packet_wrapper::PacketType,
};

pub fn transform_audio_chunk(
    chunk: &Uint8Array,
    email: &str,
    sequence: u64,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    let media_packet: MediaPacket = MediaPacket {
        email: email.to_owned(),
        media_type: MediaType::AUDIO.into(),
        frame_type: EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Key).to_string(),
        data: chunk.to_vec(),
        video_metadata: Some(VideoMetadata {
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
    on_encoder_settings_update: Option<Callback<String>>,
    codec: AudioWorkletCodec,
}

impl MicrophoneEncoder {
    pub fn new(
        client: VideoCallClient,
        bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
    ) -> Self {
        Self {
            client,
            state: EncoderState::new(),
            on_encoder_settings_update: Some(on_encoder_settings_update),
            codec: AudioWorkletCodec::default(),
        }
    }

    pub fn set_encoder_control(
        &mut self,
        diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        // TODO: ignore this for now
        // self.client.subscribe_diagnostics(diagnostics_receiver, MediaType::AUDIO);
    }

    // delegates to self.state
    pub fn set_enabled(&mut self, value: bool) -> bool {
        let is_changed = self.state.set_enabled(value);
        if is_changed {
            if value {
                let _ = self.codec.start();
            } else {
                let _ = self.codec.stop();
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
        if self.state.switching.load(Ordering::Acquire) && self.codec.is_instantiated() {
            self.stop();
        }
        if self.state.is_enabled() && self.codec.is_instantiated() {
            return;
        }
        let aes = client.aes();
        let EncoderState { .. } = self.state.clone();
        let audio_output_handler = {
            let buffer: [u8; 100000] = [0; 100000];
            log::info!("Starting audio encoder");
            let mut sequence_number = 0;

            Box::new(move |chunk: MessageEvent| {
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
                } else {
                    log::error!("Received non-MessageEvent: {:?}", chunk);
                }
            })
        };

        let codec = self.codec.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();
            let media_devices = navigator.media_devices().unwrap();
            // TODO: Add dropdown so that user can select the device that they want to use.
            let mut constraints = MediaStreamConstraints::new();
            let mut media_info = web_sys::MediaTrackConstraints::new();
            media_info.device_id(&device_id.into());

            constraints.audio(&media_info.into());
            constraints.video(&Boolean::from(false));
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

            let mut options = AudioContextOptions::new();
            options.sample_rate(AUDIO_SAMPLE_RATE as f32);

            let context = AudioContext::new_with_context_options(&options).unwrap();

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
        });
    }
}
