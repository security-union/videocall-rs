use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use log::debug;
use log::error;
use std::sync::{atomic::Ordering, Arc};
use types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::CanvasRenderingContext2d;
use web_sys::HtmlVideoElement;
use web_sys::LatencyMode;
use web_sys::MediaStream;
use web_sys::MediaStreamConstraints;
use web_sys::MediaStreamTrack;
use web_sys::OffscreenCanvas;
use web_sys::VideoEncoder;
use web_sys::VideoEncoderConfig;
use web_sys::VideoEncoderEncodeOptions;
use web_sys::VideoEncoderInit;
use web_sys::VideoFrame;
use web_sys::VideoFrameInit;
use web_sys::VideoTrack;

use super::encoder_state::EncoderState;
use super::transform::transform_video_chunk;

use crate::constants::VIDEO_CODEC;
use crate::constants::VIDEO_FPS;
use crate::constants::VIDEO_HEIGHT;
use crate::constants::VIDEO_WIDTH;
use crate::crypto::aes::Aes128State;

pub struct CameraEncoder {
    aes: Arc<Aes128State>,
    state: EncoderState,
}

impl CameraEncoder {
    pub fn new(aes: Arc<Aes128State>) -> Self {
        Self {
            aes,
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

    pub fn start(
        &mut self,
        userid: String,
        on_frame: impl Fn(PacketWrapper) + 'static,
        video_elem_id: &str,
    ) {
        // 1. Query the first device with a camera and a mic attached.
        // 2. setup WebCodecs, in particular
        // 3. send encoded video frames and raw audio to the server.
        let on_frame = Box::new(on_frame);
        let userid = Box::new(userid);
        let video_elem_id = video_elem_id.to_string();
        let EncoderState {
            destroy,
            enabled,
            switching,
            ..
        } = self.state.clone();
        let aes = self.aes.clone();
        let video_output_handler = {
            let userid = userid;
            let on_frame = on_frame;
            let mut buffer: [u8; 100000] = [0; 100000];
            let mut sequence_number = 0;
            Box::new(move |chunk: JsValue| {
                let chunk = web_sys::EncodedVideoChunk::from(chunk);
                let packet: PacketWrapper = transform_video_chunk(
                    chunk,
                    sequence_number,
                    &mut buffer,
                    &userid,
                    aes.clone(),
                );
                on_frame(packet);
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

            let canvas_element = window()
                .document()
                .unwrap()
                .create_element(&"canvas")
                .unwrap()
                .unchecked_into::<OffscreenCanvas>();
            let ctx = canvas_element
                .get_context("2d")
                .unwrap()
                .unwrap()
                .unchecked_into::<CanvasRenderingContext2d>();

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
            let _ = video_element.play();

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

            video_output_handler.forget();
            video_error_handler.forget();
            let video_encoder = Box::new(VideoEncoder::new(&video_encoder_init).unwrap());

            let video_settings = &mut video_track
                .clone()
                .unchecked_into::<MediaStreamTrack>()
                .get_settings();
            video_settings.width(VIDEO_WIDTH);
            video_settings.height(VIDEO_HEIGHT);

            let mut video_encoder_config =
                VideoEncoderConfig::new(VIDEO_CODEC, VIDEO_HEIGHT as u32, VIDEO_WIDTH as u32);

            video_encoder_config.bitrate(100_000f64);
            video_encoder_config.latency_mode(LatencyMode::Realtime);
            video_encoder.configure(&video_encoder_config);

            // Start encoding video and audio.
            let mut video_frame_counter = 0;
            let mut timestamp = 0.0;
            let process_frame = Closure::wrap(Box::new(move || {
                if !enabled.load(Ordering::Acquire)
                    || destroy.load(Ordering::Acquire)
                    || switching.load(Ordering::Acquire)
                {
                    video_track
                        .clone()
                        .unchecked_into::<MediaStreamTrack>()
                        .stop();
                    switching.store(false, Ordering::Release);
                    return;
                }
                if let Err(e) = ctx.draw_image_with_html_video_element_and_dw_and_dh(
                    &video_element,
                    0.0,
                    0.0,
                    300.0,
                    150.,
                ) {
                    debug!("Error {:?}", e);
                } else {
                    let mut video_frame_init = VideoFrameInit::new();
                    video_frame_init.timestamp(timestamp);
                    video_frame_init.duration(1.0 / VIDEO_FPS);

                    let video_frame = VideoFrame::new_with_offscreen_canvas_and_video_frame_init(
                        &canvas_element,
                        &video_frame_init,
                    )
                    .unwrap();

                    let mut opts: VideoEncoderEncodeOptions = VideoEncoderEncodeOptions::new();
                    video_frame_counter = (video_frame_counter + 1) % 50;
                    opts.key_frame(video_frame_counter == 0);
                    video_encoder.encode_with_options(&video_frame, &opts);
                    video_frame.close();
                    timestamp += 1.0 / VIDEO_FPS;
                }
            }) as Box<dyn FnMut()>);
            let _ = window().set_interval_with_callback_and_timeout_and_arguments_0(
                process_frame.as_ref().unchecked_ref(),
                1000 / VIDEO_FPS as i32,
            );
            process_frame.forget();
            debug!("Killing video streamer");
        });
    }
}
