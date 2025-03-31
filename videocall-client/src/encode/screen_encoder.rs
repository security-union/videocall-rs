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

use crate::constants::VIDEO_CODEC;
use crate::diagnostics::EncoderBitrateController;

// Threshold for bitrate changes, represents 20% (0.2)
const BITRATE_CHANGE_THRESHOLD: f64 = 0.2;

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
    pub fn new(
        client: VideoCallClient,
        bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
    ) -> Self {
        Self {
            client,
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(bitrate_kbps)),
            current_fps: Rc::new(AtomicU32::new(0)),
            on_encoder_settings_update: Some(on_encoder_settings_update),
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
            let mut encoder_control = EncoderBitrateController::new(
                current_bitrate.load(Ordering::Relaxed),
                current_fps.clone(),
            );
            while let Some(event) = diagnostics_receiver.next().await {
                let output_wasted = encoder_control.process_diagnostics_packet(event);
                if let Some(bitrate) = output_wasted {
                    if enabled.load(Ordering::Acquire) {
                        // Only update if change is greater than threshold
                        let current = current_bitrate.load(Ordering::Relaxed) as f64;
                        let new = bitrate;
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
    /// This will toggle the enabled state of the encoder.
    pub fn start(&mut self) {
        let EncoderState {
            enabled,
            destroy,
            switching,
            ..
        } = self.state.clone();
        // enable the encoder
        // patch the destroy flag to false
        enabled.store(true, Ordering::Release);
        destroy.store(false, Ordering::Release);

        let client = self.client.clone();
        let userid = client.userid().clone();
        let aes = client.aes();
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();

        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();
            let media_devices = navigator.media_devices().unwrap_or_else(|_| {
                error!("Failed to get media devices - browser may not support screen sharing");
                panic!("MediaDevices not available");
            });

            let screen_to_share: MediaStream = match media_devices.get_display_media() {
                Ok(promise) => match JsFuture::from(promise).await {
                    Ok(stream) => stream.unchecked_into::<MediaStream>(),
                    Err(e) => {
                        error!(
                            "User denied screen sharing permission or error occurred: {:?}",
                            e
                        );
                        return;
                    }
                },
                Err(e) => {
                    error!("Failed to get display media: {:?}", e);
                    return;
                }
            };

            log::info!("Screen to share: {:?}", screen_to_share);

            let screen_track = Box::new(
                screen_to_share
                    .get_video_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<VideoTrack>(),
            );

            // Setup FPS tracking and screen output handler
            let screen_output_handler = {
                let mut buffer: [u8; 150000] = [0; 150000];
                let mut sequence_number = 0;
                let performance = window()
                    .performance()
                    .expect("Performance API not available");
                let mut last_chunk_time = performance.now();
                let mut chunks_in_last_second = 0;

                Box::new(move |chunk: JsValue| {
                    let now = window()
                        .performance()
                        .expect("Performance API not available")
                        .now();
                    let chunk = web_sys::EncodedVideoChunk::from(chunk);

                    // Update FPS calculation
                    chunks_in_last_second += 1;
                    if now - last_chunk_time >= 1000.0 {
                        let fps = chunks_in_last_second;
                        current_fps.store(fps, Ordering::Relaxed);
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
                error!("Screen encoder error: {:?}", e);
            }) as Box<dyn FnMut(JsValue)>);

            let screen_output_handler =
                Closure::wrap(screen_output_handler as Box<dyn FnMut(JsValue)>);

            let screen_encoder_init = VideoEncoderInit::new(
                screen_error_handler.as_ref().unchecked_ref(),
                screen_output_handler.as_ref().unchecked_ref(),
            );

            let screen_encoder = match VideoEncoder::new(&screen_encoder_init) {
                Ok(encoder) => Box::new(encoder),
                Err(e) => {
                    error!("Failed to create video encoder: {:?}", e);
                    return;
                }
            };

            let media_track = screen_track
                .as_ref()
                .clone()
                .unchecked_into::<MediaStreamTrack>();
            let track_settings = media_track.get_settings();

            let width = track_settings.get_width().expect("width is None");
            let height = track_settings.get_height().expect("height is None");
            // Cache the initial bitrate
            let mut local_bitrate: u32 = current_bitrate.load(Ordering::Relaxed) * 1000;
            let screen_encoder_config =
                VideoEncoderConfig::new(VIDEO_CODEC, height as u32, width as u32);
            screen_encoder_config.set_bitrate(local_bitrate as f64);
            screen_encoder_config.set_latency_mode(LatencyMode::Realtime);
            if let Err(e) = screen_encoder.configure(&screen_encoder_config) {
                error!("Error configuring screen encoder: {:?}", e);
                return;
            }

            let screen_processor = match MediaStreamTrackProcessor::new(
                &MediaStreamTrackProcessorInit::new(&media_track),
            ) {
                Ok(processor) => processor,
                Err(e) => {
                    error!("Failed to create media stream track processor: {:?}", e);
                    return;
                }
            };

            let screen_reader = screen_processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();

            let mut screen_frame_counter = 0;
            let mut current_encoder_width = width as u32;
            let mut current_encoder_height = height as u32;

            loop {
                // Check if we should stop encoding
                if destroy.load(Ordering::Acquire)
                    || !enabled.load(Ordering::Acquire)
                    || switching.load(Ordering::Acquire)
                {
                    switching.store(false, Ordering::Release);
                    media_track.stop();
                    if let Err(e) = screen_encoder.close() {
                        error!("Error closing screen encoder: {:?}", e);
                    }
                    break;
                }

                // Update the bitrate if it has changed from diagnostics system
                let new_bitrate = current_bitrate.load(Ordering::Relaxed) * 1000;
                if new_bitrate != local_bitrate {
                    info!("📊 Updating screen bitrate to {}", new_bitrate);
                    local_bitrate = new_bitrate;
                    let new_config = VideoEncoderConfig::new(
                        VIDEO_CODEC,
                        current_encoder_height,
                        current_encoder_width,
                    );
                    new_config.set_bitrate(local_bitrate as f64);
                    new_config.set_latency_mode(LatencyMode::Realtime);
                    if let Err(e) = screen_encoder.configure(&new_config) {
                        error!("Error configuring screen encoder: {:?}", e);
                    }
                }

                match JsFuture::from(screen_reader.read()).await {
                    Ok(js_frame) => {
                        let value = match Reflect::get(&js_frame, &JsString::from("value")) {
                            Ok(v) => v,
                            Err(e) => {
                                error!("Failed to get frame value: {:?}", e);
                                continue;
                            }
                        };

                        if value.is_undefined() {
                            error!("Screen share stream ended");
                            break;
                        }

                        let video_frame = value.unchecked_into::<VideoFrame>();
                        let frame_width = video_frame.display_width();
                        let frame_height = video_frame.display_height();
                        let frame_width = if frame_width > 0 {
                            frame_width as u32
                        } else {
                            0
                        };
                        let frame_height = if frame_height > 0 {
                            frame_height as u32
                        } else {
                            0
                        };

                        if frame_width > 0
                            && frame_height > 0
                            && (frame_width != current_encoder_width
                                || frame_height != current_encoder_height)
                        {
                            info!("Frame dimensions changed from {}x{} to {}x{}, reconfiguring encoder", 
                                current_encoder_width, current_encoder_height, frame_width, frame_height);

                            current_encoder_width = frame_width;
                            current_encoder_height = frame_height;

                            let new_config = VideoEncoderConfig::new(
                                VIDEO_CODEC,
                                current_encoder_height,
                                current_encoder_width,
                            );
                            new_config.set_bitrate(local_bitrate as f64);
                            new_config.set_latency_mode(LatencyMode::Realtime);
                            if let Err(e) = screen_encoder.configure(&new_config) {
                                error!(
                                    "Error reconfiguring screen encoder with new dimensions: {:?}",
                                    e
                                );
                            }
                        }

                        let opts = VideoEncoderEncodeOptions::new();
                        screen_frame_counter = (screen_frame_counter + 1) % 50;
                        opts.set_key_frame(screen_frame_counter == 0);

                        if let Err(e) = screen_encoder.encode_with_options(&video_frame, &opts) {
                            error!("Error encoding screen frame: {:?}", e);
                        }
                        video_frame.close();
                    }
                    Err(e) => {
                        error!("Error reading screen frame: {:?}", e);
                        break;
                    }
                }
            }

            // At the end of the loop, ensure proper cleanup
            media_track.stop();
            if let Some(tracks) = screen_to_share.get_tracks().dyn_ref::<Array>() {
                for i in 0..tracks.length() {
                    if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
                        track.stop();
                    }
                }
            }
        });
    }
}
