use gloo_console::log;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use std::sync::atomic::Ordering;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::AudioData;
use web_sys::AudioEncoder;
use web_sys::AudioEncoderConfig;
use web_sys::AudioEncoderInit;
use web_sys::AudioTrack;
use web_sys::MediaStream;
use web_sys::MediaStreamConstraints;
use web_sys::MediaStreamTrack;
use web_sys::MediaStreamTrackProcessor;
use web_sys::MediaStreamTrackProcessorInit;
use web_sys::ReadableStreamDefaultReader;

use super::encoder_state::EncoderState;
use super::transform::transform_audio_chunk;
use types::protos::media_packet::MediaPacket;

use crate::constants::AUDIO_BITRATE;
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;

pub struct MicrophoneEncoder {
    state: EncoderState,
}

impl MicrophoneEncoder {
    pub fn new() -> Self {
        Self {
            state: EncoderState::new(),
        }
    }

    // delegates to self.state
    pub fn set_enabled(&mut self, value: bool) -> bool {
        self.state.set_enabled(value)
    }
    pub fn select(&mut self, device: String) -> bool {
        self.state.select(device)
    }
    pub fn stop(&mut self) {
        self.state.stop()
    }

    pub fn start(&mut self, userid: String, on_audio: impl Fn(MediaPacket) + 'static) {
        let device_id = if let Some(mic) = &self.state.selected {
            mic.to_string()
        } else {
            return;
        };

        let audio_output_handler = {
            let email = userid;
            let on_audio = on_audio;
            let mut buffer: [u8; 100000] = [0; 100000];
            let mut sequence = 0;
            Box::new(move |chunk: JsValue| {
                let chunk = web_sys::EncodedAudioChunk::from(chunk);
                let media_packet: MediaPacket =
                    transform_audio_chunk(&chunk, &mut buffer, &email, sequence);
                on_audio(media_packet);
                sequence += 1;
            })
        };
        let EncoderState {
            destroy,
            enabled,
            switching,
            ..
        } = self.state.clone();

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

            // Setup audio encoder.

            let audio_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                log!("error_handler error", e);
            }) as Box<dyn FnMut(JsValue)>);

            let audio_output_handler =
                Closure::wrap(audio_output_handler as Box<dyn FnMut(JsValue)>);

            let audio_encoder_init = AudioEncoderInit::new(
                audio_error_handler.as_ref().unchecked_ref(),
                audio_output_handler.as_ref().unchecked_ref(),
            );
            let audio_encoder = Box::new(AudioEncoder::new(&audio_encoder_init).unwrap());
            let audio_track = Box::new(
                device
                    .get_audio_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<AudioTrack>(),
            );
            let mut audio_encoder_config = AudioEncoderConfig::new(AUDIO_CODEC);
            audio_encoder_config.bitrate(AUDIO_BITRATE);
            audio_encoder_config.sample_rate(AUDIO_SAMPLE_RATE);
            audio_encoder_config.number_of_channels(AUDIO_CHANNELS);
            audio_encoder.configure(&audio_encoder_config);

            let audio_processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
                    &audio_track.clone().unchecked_into::<MediaStreamTrack>(),
                ))
                .unwrap();
            let audio_reader = audio_processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();

            let poll_audio = async {
                loop {
                    if !enabled.load(Ordering::Acquire)
                        || destroy.load(Ordering::Acquire)
                        || switching.load(Ordering::Acquire)
                    {
                        switching.store(false, Ordering::Release);
                        let audio_track = audio_track.clone().unchecked_into::<MediaStreamTrack>();
                        audio_track.stop();
                        audio_encoder.close();
                        return;
                    }
                    match JsFuture::from(audio_reader.read()).await {
                        Ok(js_frame) => {
                            let audio_frame = Reflect::get(&js_frame, &JsString::from("value"))
                                .unwrap()
                                .unchecked_into::<AudioData>();
                            audio_encoder.encode(&audio_frame);
                            audio_frame.close();
                        }
                        Err(e) => {
                            log!("error", e);
                        }
                    }
                }
            };
            poll_audio.await;
        });
    }
}
