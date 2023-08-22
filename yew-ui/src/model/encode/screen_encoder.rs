use gloo_utils::window;
use js_sys::Array;
use js_sys::JsString;
use js_sys::Reflect;
use log::error;
use std::sync::{atomic::Ordering, Arc};
use types::protos::packet_wrapper::PacketWrapper;
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

use super::encoder_state::EncoderState;
use super::transform::transform_screen_chunk;

use crate::constants::VIDEO_CODEC;
use crate::constants::VIDEO_HEIGHT;
use crate::constants::VIDEO_WIDTH;
use crate::crypto::aes::Aes128State;

pub struct ScreenEncoder {
    aes: Arc<Aes128State>,
    state: EncoderState,
}

impl ScreenEncoder {
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
    pub fn stop(&mut self) {
        self.state.stop()
    }

    pub fn start(&mut self, userid: String, on_frame: impl Fn(PacketWrapper) + 'static) {
        let EncoderState {
            enabled, destroy, ..
        } = self.state.clone();
        let on_frame = Box::new(on_frame);
        let userid = Box::new(userid);
        let aes = self.aes.clone();
        let screen_output_handler = {
            let userid = userid;
            let on_frame = on_frame;
            let mut buffer: [u8; 100000] = [0; 100000];
            let mut sequence_number = 0;
            Box::new(move |chunk: JsValue| {
                let chunk = web_sys::EncodedVideoChunk::from(chunk);
                let packet: PacketWrapper = transform_screen_chunk(
                    chunk,
                    sequence_number,
                    &mut buffer,
                    userid.clone(),
                    aes.clone(),
                );
                on_frame(packet);
                sequence_number += 1;
            })
        };
        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();
            let media_devices = navigator.media_devices().unwrap();
            let screen_to_share: MediaStream =
                JsFuture::from(media_devices.get_display_media().unwrap())
                    .await
                    .unwrap()
                    .unchecked_into::<MediaStream>();

            let screen_track = Box::new(
                screen_to_share
                    .get_video_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<VideoTrack>(),
            );

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
            let mut screen_encoder_config =
                VideoEncoderConfig::new(VIDEO_CODEC, VIDEO_HEIGHT as u32, VIDEO_WIDTH as u32);
            screen_encoder_config.bitrate(60_000f64);
            screen_encoder_config.latency_mode(LatencyMode::Realtime);
            screen_encoder.configure(&screen_encoder_config);

            let screen_processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
                    &screen_track.unchecked_into::<MediaStreamTrack>(),
                ))
                .unwrap();

            let screen_reader = screen_processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();

            let mut screen_frame_counter = 0;

            let poll_screen = async {
                loop {
                    if destroy.load(Ordering::Acquire) {
                        return;
                    }
                    if !enabled.load(Ordering::Acquire) {
                        return;
                    }
                    match JsFuture::from(screen_reader.read()).await {
                        Ok(js_frame) => {
                            let video_frame = Reflect::get(&js_frame, &JsString::from("value"))
                                .unwrap()
                                .unchecked_into::<VideoFrame>();
                            let mut opts = VideoEncoderEncodeOptions::new();
                            screen_frame_counter = (screen_frame_counter + 1) % 50;
                            opts.key_frame(screen_frame_counter == 0);
                            screen_encoder.encode_with_options(&video_frame, &opts);
                            video_frame.close();
                        }
                        Err(e) => {
                            error!("error {:?}", e);
                        }
                    }
                }
            };
            poll_screen.await;
        });
    }
}
