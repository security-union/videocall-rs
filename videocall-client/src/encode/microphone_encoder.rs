use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use log::error;
use log::info;
use std::sync::atomic::Ordering;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
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

use super::super::client::VideoCallClient;
use super::encoder_state::EncoderState;
use super::transform::transform_audio_chunk;

use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::diagnostics::{EncoderControl, EncoderControlSender};
use videocall_types::protos::media_packet::media_packet::MediaType;

use futures::channel::mpsc::UnboundedReceiver;
use futures::{select, FutureExt, StreamExt};

/// [MicrophoneEncoder] encodes the audio from a microphone and sends it through a [`VideoCallClient`](crate::VideoCallClient) connection.
///
/// See also:
/// * [CameraEncoder](crate::CameraEncoder)
/// * [ScreenEncoder](crate::ScreenEncoder)
///
pub struct MicrophoneEncoder {
    client: VideoCallClient,
    state: EncoderState,
    encoder_control: Option<UnboundedReceiver<EncoderControl>>,
    current_bitrate: u32,
}

impl MicrophoneEncoder {
    /// Construct a microphone encoder, with arguments:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    /// The encoder is created without a microphone selected, [`encoder.select(device_id)`](Self::select) must be called before it can start encoding.
    pub fn new(client: VideoCallClient, bitrate_kbps: u32) -> Self {
        Self {
            client,
            state: EncoderState::new(),
            encoder_control: None,
            current_bitrate: bitrate_kbps,
        }
    }

    pub fn set_encoder_control(&mut self, mut control: UnboundedReceiver<EncoderControl>) {
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(event) = control.next().await {
                info!("Microphone encoder control event: {:?}", event);
            }
        });
    }

    // The next three methods delegate to self.state

    /// Enables/disables the encoder.   Returns true if the new value is different from the old value.
    ///
    /// The encoder starts disabled, [`encoder.set_enabled(true)`](Self::set_enabled) must be
    /// called prior to starting encoding.
    ///
    /// Disabling encoding after it has started will cause it to stop.
    pub fn set_enabled(&mut self, value: bool) -> bool {
        self.state.set_enabled(value)
    }

    /// Selects a microphone:
    ///
    /// * `device_id` - The value of `entry.device_id` for some entry in
    ///   [`media_device_list.audio_inputs.devices()`](crate::MediaDeviceList::audio_inputs)
    ///
    /// The encoder starts without a microphone associated,
    /// [`encoder.selected(device_id)`](Self::select) must be called prior to starting encoding.
    pub fn select(&mut self, device_id: String) -> bool {
        self.state.select(device_id)
    }

    /// Stops encoding after it has been started.
    pub fn stop(&mut self) {
        self.state.stop()
    }

    /// Start encoding and sending the data to the client connection (if it's currently connected).
    ///
    /// This will not do anything if [`encoder.set_enabled(true)`](Self::set_enabled) has not been
    /// called, or if [`encoder.select(device_id)`](Self::select) has not been called.
    pub fn start(&mut self) {
        let client = self.client.clone();
        let userid = client.userid().clone();
        let aes = client.aes();
        let EncoderState {
            destroy,
            enabled,
            switching,
            ..
        } = self.state.clone();
        let audio_output_handler = {
            let mut buffer: [u8; 100000] = [0; 100000];
            let mut sequence_number = 0;
            Box::new(move |chunk: JsValue| {
                let chunk = web_sys::EncodedAudioChunk::from(chunk);
                let packet: PacketWrapper = transform_audio_chunk(
                    &chunk,
                    &mut buffer,
                    &userid,
                    sequence_number,
                    aes.clone(),
                );
                client.send_packet(packet);
                sequence_number += 1;
            })
        };
        let device_id = if let Some(vid) = &self.state.selected {
            vid.to_string()
        } else {
            return;
        };

        let current_bitrate = self.current_bitrate;
        let encoder_control = self.encoder_control.take();
        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();

            let media_devices = navigator.media_devices().unwrap();
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
                    .unchecked_into::<AudioTrack>(),
            );

            // Setup audio encoder

            let audio_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                error!("error_handler error {:?}", e);
            }) as Box<dyn FnMut(JsValue)>);

            let audio_output_handler =
                Closure::wrap(audio_output_handler as Box<dyn FnMut(JsValue)>);

            let audio_encoder_init = AudioEncoderInit::new(
                audio_error_handler.as_ref().unchecked_ref(),
                audio_output_handler.as_ref().unchecked_ref(),
            );

            let audio_encoder = Box::new(AudioEncoder::new(&audio_encoder_init).unwrap());

            let mut audio_encoder_config = AudioEncoderConfig::new(AUDIO_CODEC);
            audio_encoder_config.bitrate(current_bitrate as f64);
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

            // Start encoding audio.
            let mut current_bitrate = current_bitrate;

            if let Some(mut control_rx) = encoder_control {
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

                    select! {
                        control = control_rx.next() => {
                            if let Some(EncoderControl::UpdateBitrate { target_bitrate_kbps }) = control {
                                    info!("🎤 Microphone encoder applying bitrate update - Old: {} kbps, New: {} kbps",
                                        current_bitrate, target_bitrate_kbps);
                                    current_bitrate = target_bitrate_kbps;
                                    // let mut config = AudioEncoderConfig::new(AUDIO_CODEC);
                                    // config.bitrate(current_bitrate as f64);
                                    // config.number_of_channels(AUDIO_CHANNELS);
                                    // config.sample_rate(AUDIO_SAMPLE_RATE);
                                    // audio_encoder.configure(&config);
                                    info!("🎤 Microphone encoder bitrate update applied successfully");
                            }
                        }
                        frame = JsFuture::from(audio_reader.read()).fuse() => {
                            match frame {
                                Ok(js_frame) => {
                                    let audio_frame = Reflect::get(&js_frame, &JsString::from("value"))
                                        .unwrap()
                                        .unchecked_into::<web_sys::AudioData>();
                                    audio_encoder.encode(&audio_frame);
                                    audio_frame.close();
                                }
                                Err(e) => {
                                    error!("error {:?}", e);
                                }
                            }
                        }
                    }
                }
            } else {
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
                                .unchecked_into::<web_sys::AudioData>();
                            audio_encoder.encode(&audio_frame);
                            audio_frame.close();
                        }
                        Err(e) => {
                            error!("error {:?}", e);
                        }
                    }
                }
            }
        });
    }
}
