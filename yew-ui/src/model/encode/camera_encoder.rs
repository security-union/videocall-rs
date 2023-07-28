use gloo_console::log;
use gloo_timers::future::sleep;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use web_sys::HtmlImageElement;
use web_sys::VideoFrameInit;
use std::sync::atomic::Ordering;
use std::time::Duration;
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

use super::encoder_state::EncoderState;
use super::transform::transform_video_chunk;
use types::protos::media_packet::MediaPacket;

use crate::constants::VIDEO_CODEC;
use crate::constants::VIDEO_HEIGHT;
use crate::constants::VIDEO_WIDTH;

pub struct CameraEncoder {
    state: EncoderState,
}

impl CameraEncoder {
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

    pub fn start(
        &mut self,
        userid: String,
        on_frame: impl Fn(MediaPacket) + 'static,
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
        let video_output_handler = {
            let userid = userid;
            let on_frame = on_frame;
            let mut buffer: [u8; 100000] = [0; 100000];
            let mut sequence_number = 0;
            Box::new(move |chunk: JsValue| {
                let chunk = web_sys::EncodedVideoChunk::from(chunk);
                let media_packet: MediaPacket =
                    transform_video_chunk(chunk, sequence_number, &mut buffer, userid.clone());
                on_frame(media_packet);
                sequence_number += 1;
            })
        };
        let device_id = if let Some(vid) = &self.state.selected {
            vid.to_string()
        } else {
            return;
        };
        wasm_bindgen_futures::spawn_local(async move {
            let window = web_sys::window().expect("No global `window` exists");
            let document = window.document().expect("Should have a document on window");
            let video_element = document.get_element_by_id(&video_elem_id).unwrap()
                .dyn_into::<web_sys::HtmlVideoElement>()
                .unwrap();
    
            let offscreen_canvas = web_sys::OffscreenCanvas::new(VIDEO_WIDTH as u32, VIDEO_HEIGHT as u32).unwrap();
            
            let context = offscreen_canvas.get_context("2d").unwrap().unwrap()
                .dyn_into::<web_sys::OffscreenCanvasRenderingContext2d>()
                .unwrap();
            let html_image_element = offscreen_canvas.unchecked_into::<HtmlImageElement>();
    
            let media_devices = window.navigator().media_devices().unwrap();
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
                log!("error_handler error", e);
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

            video_encoder_config.bitrate(100_000f64);
            video_encoder_config.latency_mode(LatencyMode::Realtime);
            video_encoder.configure(&video_encoder_config);

    
            // Start encoding video and audio.
            let mut video_frame_counter = 0;
            let poll_video = async {
                loop {
                    log!("polling video");
                    if !enabled.load(Ordering::Acquire)
                        || destroy.load(Ordering::Acquire)
                        || switching.load(Ordering::Acquire)
                    {
                        video_track
                            .clone()
                            .unchecked_into::<MediaStreamTrack>()
                            .stop();
                        video_encoder.close();
                        switching.store(false, Ordering::Release);
                        return;
                    }
    
                    if let Err(e) = context.draw_image_with_html_video_element_and_dw_and_dh(&video_element, 0.0, 0.0, VIDEO_WIDTH.into(), VIDEO_HEIGHT.into()) {
                        log!("error", e);
                    }

                    // create a JsDict with a timestamp property
                    let mut video_frame_init = VideoFrameInit::new();
                    video_frame_init.timestamp(0.0);
                    video_frame_init.duration(1.0/30.0);
                    

                    let video_frame = VideoFrame::new_with_html_image_element_and_video_frame_init(
                        &html_image_element, 
                        &video_frame_init);
                    match video_frame {
                        Ok(video_frame) => {
                            let mut opts = VideoEncoderEncodeOptions::new();
                            video_frame_counter = (video_frame_counter + 1) % 50;
                            opts.key_frame(video_frame_counter == 0);
                            video_encoder.encode_with_options(&video_frame, &opts);
                            video_frame.close();
                        },
                        Err(e) => {
                            log!("error", e);
                        }
                    };
                    sleep(Duration::from_millis(50)).await;
                }
            };
            poll_video.await;
            log!("Killing video streamer");
        });
    }

    /* 
    pub fn start(
        &mut self,
        userid: String,
        on_frame: impl Fn(MediaPacket) + 'static,
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
        let video_output_handler = {
            let userid = userid;
            let on_frame = on_frame;
            let mut buffer: [u8; 100000] = [0; 100000];
            let mut sequence_number = 0;
            Box::new(move |chunk: JsValue| {
                let chunk = web_sys::EncodedVideoChunk::from(chunk);
                let media_packet: MediaPacket =
                    transform_video_chunk(chunk, sequence_number, &mut buffer, userid.clone());
                on_frame(media_packet);
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
                log!("error_handler error", e);
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

            video_encoder_config.bitrate(100_000f64);
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
            let poll_video = async {
                loop {
                    if !enabled.load(Ordering::Acquire)
                        || destroy.load(Ordering::Acquire)
                        || switching.load(Ordering::Acquire)
                    {
                        video_track
                            .clone()
                            .unchecked_into::<MediaStreamTrack>()
                            .stop();
                        video_encoder.close();
                        switching.store(false, Ordering::Release);
                        return;
                    }
                    match JsFuture::from(video_reader.read()).await {
                        Ok(js_frame) => {
                            let video_frame = Reflect::get(&js_frame, &JsString::from("value"))
                                .unwrap()
                                .unchecked_into::<VideoFrame>();
                            let mut opts = VideoEncoderEncodeOptions::new();
                            video_frame_counter = (video_frame_counter + 1) % 50;
                            opts.key_frame(video_frame_counter == 0);
                            video_encoder.encode_with_options(&video_frame, &opts);
                            video_frame.close();
                        }
                        Err(e) => {
                            log!("error", e);
                        }
                    }
                }
            };
            poll_video.await;
            log!("Killing video streamer");
        });
    } */
}
