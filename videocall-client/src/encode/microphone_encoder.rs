use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use log::error;
use log::info;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
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
use yew::Callback;

use super::super::client::VideoCallClient;
use super::encoder_state::EncoderState;
use super::transform::transform_audio_chunk;

use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::diagnostics::EncoderControlSender;

// Threshold for bitrate changes, represents 20% (0.2)
const BITRATE_CHANGE_THRESHOLD: f64 = 0.2;

/// [MicrophoneEncoder] encodes the audio from a microphone and sends it through a [`VideoCallClient`](crate::VideoCallClient) connection.
///
/// See also:
/// * [CameraEncoder](crate::CameraEncoder)
/// * [ScreenEncoder](crate::ScreenEncoder)
///
pub struct MicrophoneEncoder {
    client: VideoCallClient,
    state: EncoderState,
    current_bitrate: Rc<AtomicU32>,
    on_encoder_settings_update: Option<Callback<String>>,
}

impl MicrophoneEncoder {
    /// Construct a microphone encoder, with arguments:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    /// The encoder is created without a microphone selected, [`encoder.select(device_id)`](Self::select) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
    ) -> Self {
        Self {
            client,
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(bitrate_kbps)),
            on_encoder_settings_update: Some(on_encoder_settings_update),
        }
    }

    pub fn set_encoder_control(
        &mut self,
        mut diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        let current_bitrate = self.current_bitrate.clone();
        // For audio we'll use a dummy FPS counter - audio doesn't have FPS but the API requires it
        let dummy_fps = Rc::new(AtomicU32::new(50));
        let on_encoder_settings_update = self.on_encoder_settings_update.clone();
        let enabled = self.state.enabled.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control = EncoderControlSender::new(
                current_bitrate.load(Ordering::Relaxed),
                dummy_fps.clone(),
            );
            while let Some(event) = diagnostics_receiver.next().await {
                let output_wasted = encoder_control.process_diagnostics_packet(event);
                if let Some(bitrate) = output_wasted {
                    if enabled.load(Ordering::Acquire) {
                        // Only update if change is greater than threshold
                        let current = current_bitrate.load(Ordering::Relaxed) as f64;
                        let new = bitrate as f64;
                        let percent_change = (new - current).abs() / current;

                        if percent_change > BITRATE_CHANGE_THRESHOLD {
                            if let Some(callback) = &on_encoder_settings_update {
                                callback.emit(format!("Bitrate: {:.2} kbps", bitrate));
                            }
                            current_bitrate.store(bitrate as u32, Ordering::Relaxed);
                        }
                    } else if let Some(callback) = &on_encoder_settings_update {
                        callback.emit("Disabled".to_string());
                    }
                }
            }
        });
    }

    /// Allows setting a callback to receive encoder settings updates
    pub fn set_encoder_settings_callback(&mut self, callback: Callback<String>) {
        self.on_encoder_settings_update = Some(callback);
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

        let current_bitrate = self.current_bitrate.clone();
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

            let audio_track = device
                .get_audio_tracks()
                .find(&mut |_: JsValue, _: u32, _: Array| true)
                .unchecked_into::<AudioTrack>();

            let media_track = audio_track.unchecked_into::<MediaStreamTrack>();

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

            // Cache the initial bitrate
            let mut local_bitrate: u32 = current_bitrate.load(Ordering::Relaxed) * 1000;
            let audio_encoder_config = AudioEncoderConfig::new(AUDIO_CODEC);
            audio_encoder_config.set_bitrate(local_bitrate as f64);
            audio_encoder_config.set_sample_rate(AUDIO_SAMPLE_RATE);
            audio_encoder_config.set_number_of_channels(AUDIO_CHANNELS);
            if let Err(e) = audio_encoder.configure(&audio_encoder_config) {
                error!("Error configuring microphone encoder: {:?}", e);
            }

            let audio_processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(&media_track))
                    .unwrap();
            let audio_reader = audio_processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();

            // Start encoding audio with dynamic bitrate control
            loop {
                // Check if we should stop encoding
                if destroy.load(Ordering::Acquire)
                    || !enabled.load(Ordering::Acquire)
                    || switching.load(Ordering::Acquire)
                {
                    switching.store(false, Ordering::Release);
                    media_track.stop();
                    if let Err(e) = audio_encoder.close() {
                        error!("Error closing microphone encoder: {:?}", e);
                    }
                    break;
                }

                // Update the bitrate if it has changed from diagnostics system
                let new_bitrate = current_bitrate.load(Ordering::Relaxed) * 1000;
                if new_bitrate != local_bitrate {
                    info!("📊 Updating microphone bitrate to {}", new_bitrate);
                    local_bitrate = new_bitrate;
                    let new_config = AudioEncoderConfig::new(AUDIO_CODEC);
                    new_config.set_bitrate(local_bitrate as f64);
                    new_config.set_sample_rate(AUDIO_SAMPLE_RATE);
                    new_config.set_number_of_channels(AUDIO_CHANNELS);
                    if let Err(e) = audio_encoder.configure(&new_config) {
                        error!("Error configuring microphone encoder: {:?}", e);
                    }
                }

                match JsFuture::from(audio_reader.read()).await {
                    Ok(js_frame) => match Reflect::get(&js_frame, &JsString::from("value")) {
                        Ok(value) => {
                            let audio_frame = value.unchecked_into::<web_sys::AudioData>();
                            if let Err(e) = audio_encoder.encode(&audio_frame) {
                                error!("Error encoding microphone frame: {:?}", e);
                            }
                            audio_frame.close();
                        }
                        Err(e) => {
                            error!("Error getting frame value: {:?}", e);
                        }
                    },
                    Err(e) => {
                        error!("Error reading frame: {:?}", e);
                    }
                }
            }
        });
    }
}
