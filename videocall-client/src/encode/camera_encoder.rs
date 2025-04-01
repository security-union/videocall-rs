use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use log::error;
use std::rc::Rc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlVideoElement;
use web_sys::LatencyMode;
use web_sys::MediaStream;
use web_sys::MediaStreamConstraints;
use web_sys::MediaStreamTrack;
use web_sys::ReadableStreamDefaultReader;
use web_sys::VideoEncoder;
use web_sys::VideoEncoderConfig;
use web_sys::VideoEncoderEncodeOptions;
use web_sys::VideoEncoderInit;
use web_sys::VideoFrame;
use web_sys::VideoTrack;
use web_sys::VideoFrameBufferInit;
use web_sys::VideoPixelFormat;
use yew::Callback;

use super::super::client::VideoCallClient;
use super::encoder_state::EncoderState;
use super::transform::transform_video_chunk;
use super::camera_bridge::VideoProcessorCompat;

use crate::constants::VIDEO_CODEC;
use crate::diagnostics::EncoderBitrateController;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;

// Threshold for bitrate changes, represents 20% (0.2)
const BITRATE_CHANGE_THRESHOLD: f64 = 0.20;
// Frame processing interval in milliseconds
const FRAME_PROCESSING_INTERVAL: f64 = 1000.0 / 30.0; // 30fps default

/// [CameraEncoder] encodes the video from a camera and sends it through a [`VideoCallClient`](crate::VideoCallClient) connection.
///
/// To use this struct, the caller must first create an `HtmlVideoElement` DOM node, to which the
/// camera will be connected.
///
/// See also:
/// * [MicrophoneEncoder](crate::MicrophoneEncoder)
/// * [ScreenEncoder](crate::ScreenEncoder)
///
pub struct CameraEncoder {
    client: VideoCallClient,
    video_elem_id: String,
    state: EncoderState,
    current_bitrate: Rc<AtomicU32>,
    current_fps: Rc<AtomicU32>,
    on_encoder_settings_update: Callback<String>,
}

impl CameraEncoder {
    /// Construct a camera encoder, with arguments:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    ///
    /// * `video_elem_id` - the the ID of an `HtmlVideoElement` to which the camera will be connected.  It does not need to currently exist.
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    /// The encoder is created without a camera selected, [`encoder.select(device_id)`](Self::select) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        video_elem_id: &str,
        initial_bitrate: u32,
        on_encoder_settings_update: Callback<String>,
    ) -> Self {
        Self {
            client,
            video_elem_id: video_elem_id.to_string(),
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(initial_bitrate)),
            current_fps: Rc::new(AtomicU32::new(0)),
            on_encoder_settings_update,
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
        let destroy = self.state.destroy.clone();
        let switching = self.state.switching.clone();
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
                            on_encoder_settings_update
                                .emit(format!("Bitrate: {:.2} kbps", bitrate));
                            current_bitrate.store(bitrate as u32, Ordering::Relaxed);
                        }
                    } else {
                        on_encoder_settings_update.emit("Disabled".to_string());
                    }
                }
            }
        });
    }

    /// Gets the current encoder output frame rate
    pub fn get_current_fps(&self) -> u32 {
        self.current_fps.load(Ordering::Relaxed)
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

    /// Selects a camera:
    ///
    /// * `device_id` - The value of `entry.device_id` for some entry in
    ///   [`media_device_list.video_inputs.devices()`](crate::MediaDeviceList::video_inputs)
    ///
    /// The encoder starts without a camera associated,
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
        // 1. Query the first device with a camera and a mic attached.
        // 2. setup WebCodecs, in particular
        // 3. send encoded video frames and raw audio to the server.
        let client = self.client.clone();
        let userid = client.userid().clone();
        let aes = client.aes();
        let video_elem_id = self.video_elem_id.clone();
        let state = self.state.clone();
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let video_output_handler = {
            let mut buffer: [u8; 100000] = [0; 100000];
            let mut sequence_number = 0;
            let mut last_chunk_time = window().performance().unwrap().now();
            let mut chunks_in_last_second = 0;
            let state = state.clone();

            Box::new(move |chunk: JsValue| {
                let now = window().performance().unwrap().now();
                let chunk = web_sys::EncodedVideoChunk::from(chunk);

                // Update FPS calculation
                chunks_in_last_second += 1;
                if now - last_chunk_time >= 1000.0 {
                    let fps = chunks_in_last_second;
                    current_fps.store(fps, Ordering::Relaxed);
                    log::debug!("Encoder output FPS: {}", fps);
                    chunks_in_last_second = 0;
                    last_chunk_time = now;
                }

                let packet: PacketWrapper = transform_video_chunk(
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
        let device_id = if let Some(vid) = &self.state.selected {
            vid.to_string()
        } else {
            return;
        };

        wasm_bindgen_futures::spawn_local(async move {
            let enabled = state.enabled.clone();
            let destroy = state.destroy.clone();
            let switching = state.switching.clone();
            
            let navigator = window().navigator();
            let video_element = window()
                .document()
                .unwrap()
                .get_element_by_id(&video_elem_id)
                .unwrap()
                .unchecked_into::<HtmlVideoElement>();

            let media_devices = navigator.media_devices().unwrap();
            let constraints = MediaStreamConstraints::new();
            let media_info = web_sys::MediaTrackConstraints::new();
            media_info.set_device_id(&device_id.into());

            constraints.set_video(&media_info.into());
            constraints.set_audio(&Boolean::from(false));

            let devices_query = media_devices
                .get_user_media_with_constraints(&constraints)
                .unwrap();
            let device = JsFuture::from(devices_query)
                .await
                .unwrap()
                .unchecked_into::<MediaStream>();
            video_element.set_src_object(Some(&device));
            video_element.set_muted(true);

            let video_track = Box::new(
                device
                    .get_video_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<VideoTrack>(),
            );

            // Setup video encoder
            let video_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                error!("error_handler error {:?}", e);
            }) as Box<dyn FnMut(JsValue)>);

            let video_output_handler =
                Closure::wrap(video_output_handler as Box<dyn FnMut(JsValue)>);

            let video_encoder_init = VideoEncoderInit::new(
                video_error_handler.as_ref().unchecked_ref(),
                video_output_handler.as_ref().unchecked_ref(),
            );

            let video_encoder = Box::new(VideoEncoder::new(&video_encoder_init).unwrap());

            // Get track settings to get actual width and height
            let media_track = video_track
                .as_ref()
                .clone()
                .unchecked_into::<MediaStreamTrack>();
            let track_settings = media_track.get_settings();

            let width = track_settings.get_width().expect("width is None");
            let height = track_settings.get_height().expect("height is None");
            let frame_rate = track_settings.get_frame_rate().unwrap_or(30.0);

            let video_encoder_config =
                VideoEncoderConfig::new(VIDEO_CODEC, height as u32, width as u32);
            video_encoder_config
                .set_bitrate(current_bitrate.load(Ordering::Relaxed) as f64 * 1000.0);
            video_encoder_config.set_latency_mode(LatencyMode::Realtime);

            if let Err(e) = video_encoder.configure(&video_encoder_config) {
                error!("Error configuring video encoder: {:?}", e);
            }

            // Use our compatibility layer instead of MediaStreamTrackProcessor
            let mut video_processor_compat = match VideoProcessorCompat::new(&media_track) {
                Ok(processor) => processor,
                Err(e) => {
                    error!("Failed to create video processor compatibility layer: {:?}", e);
                    return;
                }
            };

            // Counter for keyframe generation
            let video_frame_counter = Rc::new(std::cell::RefCell::new(0));
            
            // Cache the initial bitrate
            let local_bitrate = Rc::new(std::cell::RefCell::new(current_bitrate.load(Ordering::Relaxed) * 1000));

            // Start processing frames
            let encoder_clone = video_encoder.clone();
            let frame_counter = video_frame_counter.clone();
            let target_frame_rate = frame_rate as u32;
            let local_bitrate_clone = local_bitrate.clone();
            let enabled_for_frame = enabled.clone();
            let destroy_for_frame = destroy.clone();
            let switching_for_frame = switching.clone();
            
            // Create a closure that processes video frames
            let on_frame = Closure::wrap(Box::new(move |frame: JsValue| {
                // Extract frame data from the canvas buffer
                let width = js_sys::Reflect::get(&frame, &"width".into())
                    .unwrap()
                    .as_f64()
                    .unwrap() as u32;
                let height = js_sys::Reflect::get(&frame, &"height".into())
                    .unwrap()
                    .as_f64()
                    .unwrap() as u32;
                let buffer = js_sys::Reflect::get(&frame, &"data".into())
                    .unwrap()
                    .dyn_into::<js_sys::ArrayBuffer>()
                    .unwrap();
                
                // Create VideoFrameBufferInit with all required arguments
                let init = web_sys::VideoFrameBufferInit::new(
                    width,
                    height,
                    web_sys::VideoPixelFormat::Rgba,
                    0f64 // timestamp
                );
                
                // Create a VideoFrame from the buffer
                let video_frame = web_sys::VideoFrame::new_with_buffer_source_and_video_frame_buffer_init(
                    &buffer,
                    &init
                ).unwrap();
                
                // Create encoding options
                let encode_options = VideoEncoderEncodeOptions::new();
                
                // Check if we should quit
                if !enabled_for_frame.load(Ordering::Acquire)
                    || destroy_for_frame.load(Ordering::Acquire)
                    || switching_for_frame.load(Ordering::Acquire)
                {
                    video_frame.close();
                    return;
                }
                
                // Update the bitrate if needed
                let new_current_bitrate = current_bitrate.load(Ordering::Relaxed) * 1000;
                let mut current_local_bitrate = local_bitrate_clone.borrow_mut();
                if *current_local_bitrate != new_current_bitrate {
                    log::info!("Updating video bitrate to {}", new_current_bitrate);
                    *current_local_bitrate = new_current_bitrate;
                    video_encoder_config.set_bitrate(new_current_bitrate as f64);
                    if let Err(e) = encoder_clone.configure(&video_encoder_config) {
                        error!("Error configuring video encoder: {:?}", e);
                    }
                }
                
                // Generate keyframes regularly
                let mut counter = frame_counter.borrow_mut();
                if *counter % (target_frame_rate * 3) == 0 {
                    encode_options.set_key_frame(true);
                }
                *counter += 1;
                
                // Encode the frame
                if let Err(e) = encoder_clone.encode_with_options(&video_frame, &encode_options) {
                    error!("Error encoding video frame: {:?}", e);
                }
                
                // Close the frame
                video_frame.close();
            }) as Box<dyn FnMut(JsValue)>);

            // Start the processor
            if let Err(e) = video_processor_compat.start(on_frame) {
                error!("Failed to start video processor: {:?}", e);
                return;
            }

            // Keep things running until stop is called
            let quit_check_interval = Rc::new(std::cell::RefCell::new(None));
            let quit_interval = quit_check_interval.clone();
            
            let enabled_for_quit = enabled.clone();
            let destroy_for_quit = destroy.clone();
            let switching_for_quit = switching.clone();
            
            let check_quit = Closure::wrap(Box::new(move || {
                if !enabled_for_quit.load(Ordering::Acquire)
                    || destroy_for_quit.load(Ordering::Acquire)
                    || switching_for_quit.load(Ordering::Acquire)
                {
                    switching_for_quit.store(false, Ordering::Release);
                    let video_track = video_track.clone().unchecked_into::<MediaStreamTrack>();
                    video_track.stop();
                    if let Err(e) = video_encoder.close() {
                        error!("Error closing video encoder: {:?}", e);
                    }
                    
                    // Clear the interval
                    if let Some(handle_id) = *quit_check_interval.borrow() {
                        window().clear_interval_with_handle(handle_id);
                    }
                }
            }) as Box<dyn FnMut()>);
            
            // Set up an interval to check if we need to quit
            let handle_id = window().set_interval_with_callback_and_timeout_and_arguments(
                check_quit.as_ref().unchecked_ref(),
                500, // Check every 500ms
                &js_sys::Array::new(),
            ).unwrap();
            
            *quit_interval.borrow_mut() = Some(handle_id);
            
            // Keep closures alive
            check_quit.forget();
        });
    }
}

// Helper function to request animation frame
fn request_animation_frame(f: &Closure<dyn FnMut()>) {
    window()
        .request_animation_frame(f.as_ref().unchecked_ref())
        .expect("should register `requestAnimationFrame` OK");
}
