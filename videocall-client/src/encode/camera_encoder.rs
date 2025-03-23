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
use web_sys::MediaStreamTrackProcessor;
use web_sys::MediaStreamTrackProcessorInit;
use web_sys::ReadableStreamDefaultReader;
use web_sys::VideoEncoder;
use web_sys::VideoEncoderConfig;
use web_sys::VideoEncoderEncodeOptions;
use web_sys::VideoEncoderInit;
use web_sys::VideoFrame;
use web_sys::VideoTrack;

use super::super::client::VideoCallClient;
use super::encoder_state::EncoderState;
use super::transform::transform_video_chunk;

use crate::constants::VIDEO_CODEC;
use crate::constants::VIDEO_HEIGHT;
use crate::constants::VIDEO_WIDTH;
use crate::diagnostics::EncoderControlSender;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;

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
    pub fn new(client: VideoCallClient, video_elem_id: &str, initial_bitrate: u32) -> Self {
        Self {
            client,
            video_elem_id: video_elem_id.to_string(),
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(initial_bitrate)),
            current_fps: Rc::new(AtomicU32::new(0)),
        }
    }

    pub fn set_encoder_control(
        &mut self,
        mut diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control = EncoderControlSender::new(500, current_fps.clone());
            while let Some(event) = diagnostics_receiver.next().await {
                let output_wasted = encoder_control.process_diagnostics_packet(event);
                if let Some(bitrate) = output_wasted {
                    // transform kbits to bits
                    let bitrate = bitrate * 1000.0;
                    current_bitrate.store(bitrate as u32, Ordering::Relaxed);
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
        let EncoderState {
            destroy,
            enabled,
            switching,
            ..
        } = self.state.clone();
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let video_output_handler = {
            let mut buffer: [u8; 100000] = [0; 100000];
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
                    log::info!("Encoder output FPS: {}", fps);
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
            let navigator = window().navigator();
            let video_element = window()
                .document()
                .unwrap()
                .get_element_by_id(&video_elem_id)
                .unwrap()
                .unchecked_into::<HtmlVideoElement>();

            let media_devices = navigator.media_devices().unwrap();
            let mut constraints = MediaStreamConstraints::new();
            let mut media_info = web_sys::MediaTrackConstraints::new();
            media_info.device_id(&device_id.into());

            constraints.video(&media_info.into());
            constraints.audio(&Boolean::from(false));

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

            let video_settings = &mut video_track
                .clone()
                .unchecked_into::<MediaStreamTrack>()
                .get_settings();
            video_settings.width(VIDEO_WIDTH);
            video_settings.height(VIDEO_HEIGHT);

            let mut video_encoder_config =
                VideoEncoderConfig::new(VIDEO_CODEC, VIDEO_HEIGHT as u32, VIDEO_WIDTH as u32);
            video_encoder_config.bitrate(current_bitrate.load(Ordering::Relaxed) as f64);
            video_encoder_config.latency_mode(LatencyMode::Realtime);

            video_encoder.configure(&video_encoder_config);

            let video_processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
                    &video_track.clone().unchecked_into::<MediaStreamTrack>(),
                ))
                .unwrap();
            let video_reader = video_processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();

            // Start encoding video and audio.
            let mut video_frame_counter = 0;

            // Cache the initial bitrate
            let mut local_bitrate: u32 = current_bitrate.load(Ordering::Relaxed);

            loop {
                if !enabled.load(Ordering::Acquire)
                    || destroy.load(Ordering::Acquire)
                    || switching.load(Ordering::Acquire)
                {
                    switching.store(false, Ordering::Release);
                    let video_track = video_track.clone().unchecked_into::<MediaStreamTrack>();
                    video_track.stop();
                    video_encoder.close();
                    return;
                }

                // Update the bitrate if it has changed more than 10%
                let new_current_bitrate = current_bitrate.load(Ordering::Relaxed);
                if new_current_bitrate != local_bitrate
                    && (new_current_bitrate as f64) / (local_bitrate as f64) > 0.9
                    && (new_current_bitrate as f64) / (local_bitrate as f64) < 1.1
                {
                    log::info!("Updating video bitrate to {}", new_current_bitrate);
                    local_bitrate = new_current_bitrate;
                    video_encoder_config.bitrate(local_bitrate as f64);
                    video_encoder.configure(&video_encoder_config);
                }

                match JsFuture::from(video_reader.read()).await {
                    Ok(js_frame) => {
                        let video_frame = Reflect::get(&js_frame, &JsString::from("value"))
                            .unwrap()
                            .unchecked_into::<VideoFrame>();
                        video_encoder.encode_with_options(
                            &video_frame,
                            VideoEncoderEncodeOptions::new()
                                .key_frame(video_frame_counter % 150 == 0),
                        );
                        video_frame.close();
                        video_frame_counter += 1;
                    }
                    Err(e) => {
                        error!("error {:?}", e);
                    }
                }
            }
        });
    }
}
