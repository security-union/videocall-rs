use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::Uint8Array;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::AudioContext;
use web_sys::AudioContextOptions;
use web_sys::MediaStream;
use web_sys::MediaStreamConstraints;
use web_sys::MediaStreamTrack;
use web_sys::MessageEvent;

use super::encoder_state::EncoderState;
use super::transform::transform_audio_chunk;

use crate::constants::AUDIO_BITRATE;
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::crypto::aes::Aes128State;
use crate::model::audio_worklet_codec::EncoderInitOptions;
use crate::model::audio_worklet_codec::{AudioWorkletCodec, CodecMessages};

pub struct MicrophoneEncoder {
    aes: Arc<Aes128State>,
    state: EncoderState,
    codec: AudioWorkletCodec,
}

impl MicrophoneEncoder {
    pub fn new(aes: Arc<Aes128State>) -> Self {
        Self {
            aes,
            state: EncoderState::new(),
            codec: AudioWorkletCodec::default(),
        }
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

    pub fn start(&mut self, userid: String, on_audio: impl Fn(PacketWrapper) + 'static) {
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
        let aes = self.aes.clone();
        let audio_output_handler = {
            let email = userid;
            let on_audio = on_audio;
            let mut sequence = 0;
            Box::new(move |chunk: MessageEvent| {
                let data = js_sys::Reflect::get(&chunk.data(), &"page".into()).unwrap();
                if let Ok(data) = data.dyn_into::<Uint8Array>() {
                    let packet: PacketWrapper =
                        transform_audio_chunk(&data, &email, sequence, aes.clone());
                    on_audio(packet);
                    sequence += 1;
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
                js_sys::Reflect::get(&track_settings, &JsValue::from_str(&"sampleRate"))
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
                    original_sample_rate: Some(input_rate),
                    encoder_bit_rate: Some(AUDIO_BITRATE as u32),
                    ..Default::default()
                }),
            });

            let source_node = context.create_media_stream_source(&device).unwrap();
            let gain_node = context.create_gain().unwrap();
            let _ = source_node
                .connect_with_audio_node(&gain_node)
                .unwrap()
                .connect_with_audio_node(&worklet)
                .unwrap()
                .connect_with_audio_node(&context.destination());
        });
    }
}
