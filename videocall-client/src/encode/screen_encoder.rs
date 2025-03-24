use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gloo_utils::window;
use js_sys::Array;
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
use web_sys::LatencyMode;
use web_sys::MediaStream;
use web_sys::MediaStreamTrack;
use web_sys::MediaStreamTrackProcessor;
use web_sys::MediaStreamTrackProcessorInit;
use web_sys::ReadableStreamDefaultReader;
use web_sys::VideoEncoder;
use web_sys::VideoEncoderConfig;
use web_sys::VideoEncoderEncodeOptions;
use web_sys::VideoEncoderInit;
use web_sys::VideoFrame;
use web_sys::VideoTrack;
use yew::Callback;

use super::super::client::VideoCallClient;
use super::encoder_state::EncoderState;
use super::transform::transform_screen_chunk;

use crate::constants::SCREEN_HEIGHT;
use crate::constants::SCREEN_WIDTH;
use crate::constants::VIDEO_CODEC;
use crate::diagnostics::EncoderControlSender;

/// [ScreenEncoder] encodes the user's screen and sends it through a [`VideoCallClient`](crate::VideoCallClient) connection.
///
/// See also:
/// * [CameraEncoder](crate::CameraEncoder)
/// * [MicrophoneEncoder](crate::MicrophoneEncoder)
///
pub struct ScreenEncoder {
    client: VideoCallClient,
    state: EncoderState,
    current_bitrate: Rc<AtomicU32>,
    current_fps: Rc<AtomicU32>,
    on_encoder_settings_update: Option<Callback<String>>,
}

impl ScreenEncoder {
    /// Construct a screen encoder:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    pub fn new(client: VideoCallClient, bitrate_kbps: u32) -> Self {
        Self {
            client,
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(bitrate_kbps)),
            current_fps: Rc::new(AtomicU32::new(0)),
            on_encoder_settings_update: None,
        }
    }

    pub fn set_encoder_control(
        &mut self,
        mut diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let on_encoder_settings_update = self.on_encoder_settings_update.clone();
        let enabled = self.state.enabled.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control = EncoderControlSender::new(200, current_fps.clone());
            while let Some(event) = diagnostics_receiver.next().await {
                let output_wasted = encoder_control.process_diagnostics_packet(event);
                if let Some(bitrate) = output_wasted {
                    if enabled.load(Ordering::Acquire) {
                        if let Some(callback) = &on_encoder_settings_update {
                            callback.emit(format!("Bitrate: {:.2} kbps", bitrate));
                        }
                        let bitrate = bitrate * 1000.0;
                        current_bitrate.store(bitrate as u32, Ordering::Relaxed);
                    } else if let Some(callback) = &on_encoder_settings_update {
                        callback.emit("Disabled".to_string());
                    }
                }
            }
        });
    }

    /// Gets the current encoder output frame rate
    pub fn get_current_fps(&self) -> u32 {
        self.current_fps.load(Ordering::Relaxed)
    }

    /// Allows setting a callback to receive encoder settings updates
    pub fn set_encoder_settings_callback(&mut self, callback: Callback<String>) {
        self.on_encoder_settings_update = Some(callback);
    }

    // The next two methods delegate to self.state

    /// Enables/disables the encoder.   Returns true if the new value is different from the old value.
    ///
    /// The encoder starts disabled, [`encoder.set_enabled(true)`](Self::set_enabled) must be
    /// called prior to starting encoding.
    ///
    /// Disabling encoding after it has started will cause it to stop.
    pub fn set_enabled(&mut self, value: bool) -> bool {
        self.state.set_enabled(value)
    }

    /// Stops encoding after it has been started.
    pub fn stop(&mut self) {
        self.state.stop()
    }

    /// Start encoding and sending the data to the client connection (if it's currently connected).
    /// The user is prompted by the browser to select which window or screen to encode.
    ///
    /// This will not do anything if [`encoder.set_enabled(true)`](Self::set_enabled) has not been
    /// called.
    pub fn start(&mut self) {
        let EncoderState {
            enabled,
            destroy,
            switching,
            ..
        } = self.state.clone();
        let client = self.client.clone();
        let userid = client.userid().clone();
        let aes = client.aes();
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();

        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();
            let media_devices = navigator.media_devices().unwrap();
            let screen_to_share: MediaStream =
                JsFuture::from(media_devices.get_display_media().unwrap())
                    .await
                    .unwrap()
                    .unchecked_into::<MediaStream>();

            let screen_track = screen_to_share
                .get_video_tracks()
                .find(&mut |_: JsValue, _: u32, _: Array| true)
                .unchecked_into::<VideoTrack>();

            let media_track = screen_track.unchecked_into::<MediaStreamTrack>();

            // Setup FPS tracking and screen output handler
            let screen_output_handler = {
                let mut buffer: [u8; 150000] = [0; 150000];
                let mut sequence_number = 0;
                let mut last_chunk_time = window().performance().unwrap().now();
                let mut chunks_in_last_second = 0;

                Box::new(move |chunk: JsValue| {
                    let now = window().performance().unwrap().now();
                    let chunk = web_sys::EncodedVideoChunk::from(chunk);

                    // Update FPS calculation
                    chunks_in_last_second += 1;
                    if now - last_chunk_time >= 1000.0 {
                        let fps = chunks_in_last_second;
                        current_fps.store(fps, Ordering::Relaxed);
                        info!("Screen encoder output FPS: {}", fps);
                        chunks_in_last_second = 0;
                        last_chunk_time = now;
                    }

                    let packet: PacketWrapper = transform_screen_chunk(
                        chunk,
                        sequence_number,
                        &mut buffer,
                        &userid,
                        aes.clone(),
                    );
                    client.send_packet(packet);
                    sequence_number += 1;
                })
            };

            let screen_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                error!("error_handler error {:?}", e);
            }) as Box<dyn FnMut(JsValue)>);

            let screen_output_handler =
                Closure::wrap(screen_output_handler as Box<dyn FnMut(JsValue)>);

            let screen_encoder_init = VideoEncoderInit::new(
                screen_error_handler.as_ref().unchecked_ref(),
                screen_output_handler.as_ref().unchecked_ref(),
            );

            let screen_encoder = Box::new(VideoEncoder::new(&screen_encoder_init).unwrap());

            // Cache the initial bitrate
            let mut local_bitrate: u32 = current_bitrate.load(Ordering::Relaxed);
            let mut screen_encoder_config =
                VideoEncoderConfig::new(VIDEO_CODEC, SCREEN_HEIGHT, SCREEN_WIDTH);
            screen_encoder_config.bitrate(local_bitrate as f64);
            screen_encoder_config.latency_mode(LatencyMode::Realtime);
            screen_encoder.configure(&screen_encoder_config);

            let screen_processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(&media_track))
                    .unwrap();

            let screen_reader = screen_processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();

            let mut screen_frame_counter = 0;

            loop {
                // Check if we should stop encoding
                if destroy.load(Ordering::Acquire)
                    || !enabled.load(Ordering::Acquire)
                    || switching.load(Ordering::Acquire)
                {
                    switching.store(false, Ordering::Release);
                    media_track.stop();
                    screen_encoder.close();
                    break;
                }

                // Update the bitrate if it has changed from diagnostics system
                let new_bitrate = current_bitrate.load(Ordering::Relaxed);
                if new_bitrate != local_bitrate
                    && (new_bitrate as f64) / (local_bitrate as f64) > 0.9
                    && (new_bitrate as f64) / (local_bitrate as f64) < 1.1
                {
                    info!("📊 Updating screen bitrate to {}", new_bitrate);
                    local_bitrate = new_bitrate;
                    let mut new_config =
                        VideoEncoderConfig::new(VIDEO_CODEC, SCREEN_HEIGHT, SCREEN_WIDTH);
                    new_config.bitrate(local_bitrate as f64);
                    new_config.latency_mode(LatencyMode::Realtime);
                    screen_encoder.configure(&new_config);
                }

                match JsFuture::from(screen_reader.read()).await {
                    Ok(js_frame) => match Reflect::get(&js_frame, &JsString::from("value")) {
                        Ok(value) => {
                            let video_frame = value.unchecked_into::<VideoFrame>();
                            let mut opts = VideoEncoderEncodeOptions::new();
                            screen_frame_counter = (screen_frame_counter + 1) % 50;
                            opts.key_frame(screen_frame_counter == 0);
                            screen_encoder.encode_with_options(&video_frame, &opts);
                            video_frame.close();
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
