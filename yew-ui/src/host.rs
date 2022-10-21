use gloo_console::log;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;

use std::fmt::Debug;
use std::future::join;
use types::protos::rust::media_packet::media_packet;
use types::protos::rust::media_packet::media_packet::MediaType;
use types::protos::rust::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlVideoElement;
use web_sys::*;
use yew::prelude::*;

use crate::model::{AudioSampleFormatWrapper, EncodedVideoChunkTypeWrapper};

use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::constants::VIDEO_CODEC;
use crate::constants::VIDEO_HEIGHT;
use crate::constants::VIDEO_WIDTH;

pub enum Msg {
    Start,
}

pub struct Host {
    pub initialized: bool,
}

#[derive(Properties, Debug, PartialEq)]
pub struct MeetingProps {
    #[prop_or_default]
    pub id: String,

    #[prop_or_default]
    pub on_frame: Callback<MediaPacket>,

    #[prop_or_default]
    pub email: String,
}

impl Component for Host {
    type Message = Msg;
    type Properties = MeetingProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self { initialized: false }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::Start => {
                self.initialized = true;
                let on_frame = Box::new(ctx.props().on_frame.clone());
                let email = Box::new(ctx.props().email.clone());
                let video_output_handler = {
                    let email = email.clone();
                    let on_frame = on_frame.clone();
                    let mut buffer = [0; 500000];
                    Box::new(move |chunk: JsValue| {
                        let chunk = web_sys::EncodedVideoChunk::from(chunk);
                        let mut media_packet: MediaPacket = MediaPacket::default();
                        media_packet.email = *email.clone();
                        let byte_length = chunk.byte_length() as usize;
                        log!("byte length video", byte_length);

                        chunk.copy_to_with_u8_array(&mut buffer);
                        media_packet.video = buffer[0..byte_length].to_vec();
                        media_packet.video_type =
                            EncodedVideoChunkTypeWrapper(chunk.type_()).to_string();
                        media_packet.media_type = media_packet::MediaType::VIDEO.into();
                        media_packet.timestamp = chunk.timestamp();
                        if let Some(duration0) = chunk.duration() {
                            media_packet.duration = duration0;
                        }
                        on_frame.emit(media_packet);
                    })
                };

                let on_frame = on_frame.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let navigator = window().navigator();
                    let media_devices = navigator.media_devices().unwrap();
                    let video_element = window()
                        .document()
                        .unwrap()
                        .get_element_by_id("webcam")
                        .unwrap()
                        .unchecked_into::<HtmlVideoElement>();

                    // TODO: Add dropdown so that user can select the device that they want to use.
                    let mut constraints = MediaStreamConstraints::new();
                    constraints.video(&Boolean::from(true));
                    constraints.audio(&Boolean::from(true));
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

                    let audio_track = Box::new(
                        device
                            .get_audio_tracks()
                            .find(&mut |_: JsValue, _: u32, _: Array| true)
                            .unchecked_into::<AudioTrack>(),
                    );

                    let video_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                        log!("error_handler error", e);
                    })
                        as Box<dyn FnMut(JsValue)>);

                    let video_output_handler =
                        Closure::wrap(video_output_handler as Box<dyn FnMut(JsValue)>);

                    let video_encoder_init = VideoEncoderInit::new(
                        video_error_handler.as_ref().unchecked_ref(),
                        video_output_handler.as_ref().unchecked_ref(),
                    );

                    let video_encoder = VideoEncoder::new(&video_encoder_init).unwrap();
                    let settings = &mut video_track
                        .clone()
                        .unchecked_into::<MediaStreamTrack>()
                        .get_settings();
                    settings.width(VIDEO_WIDTH);
                    settings.height(VIDEO_HEIGHT);
                    if let Err(e) = js_sys::Reflect::set(
                        settings.as_ref(),
                        &JsValue::from("sampleRate"),
                        &JsValue::from(AUDIO_SAMPLE_RATE),
                    ) {
                        log!("error", e);
                    }
                    settings.channel_count(AUDIO_CHANNELS as i32);

                    let mut video_encoder_config = VideoEncoderConfig::new(
                        &VIDEO_CODEC,
                        VIDEO_HEIGHT as u32,
                        VIDEO_WIDTH as u32,
                    );

                    video_encoder_config.bitrate(100000f64);
                    video_encoder_config.latency_mode(LatencyMode::Realtime);
                    video_encoder.configure(&video_encoder_config);

                    let processor =
                        MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
                            &video_track.unchecked_into::<MediaStreamTrack>(),
                        ))
                        .unwrap();
                    let video_reader = processor
                        .readable()
                        .get_reader()
                        .unchecked_into::<ReadableStreamDefaultReader>();

                    let audio_processor =
                        MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
                            &audio_track.unchecked_into::<MediaStreamTrack>(),
                        ))
                        .unwrap();
                    let audio_reader = audio_processor
                        .readable()
                        .get_reader()
                        .unchecked_into::<ReadableStreamDefaultReader>();

                    let poll_video = async {
                        loop {
                            match JsFuture::from(video_reader.read()).await {
                                Ok(js_frame) => {
                                    let video_frame =
                                        Reflect::get(&js_frame, &JsString::from("value"))
                                            .unwrap()
                                            .unchecked_into::<VideoFrame>();
                                    let mut opts = VideoEncoderEncodeOptions::new();
                                    // counter = (counter + 1) % 50;
                                    opts.key_frame(true);
                                    video_encoder.encode_with_options(&video_frame, &opts);
                                    video_frame.close();
                                }
                                Err(e) => {
                                    log!("error", e);
                                }
                            }
                        }
                    };
                    let poll_audio = async {
                        let mut buffer = [0; 2000];
                        loop {
                            match JsFuture::from(audio_reader.read()).await {
                                Ok(js_frame) => {
                                    let audio_frame =
                                        Reflect::get(&js_frame, &JsString::from("value"))
                                            .unwrap()
                                            .unchecked_into::<AudioData>();

                                    let byte_length: usize = audio_frame
                                        .allocation_size(&AudioDataCopyToOptions::new(0))
                                        as usize;
                                    audio_frame.copy_to_with_u8_array(
                                        &mut buffer,
                                        &AudioDataCopyToOptions::new(0),
                                    );
                                    let mut packet: MediaPacket = MediaPacket::default();
                                    packet.email = *email.clone();
                                    packet.media_type = MediaType::AUDIO.into();
                                    packet.audio = buffer[0..byte_length].to_vec();
                                    packet.audio_format =
                                        AudioSampleFormatWrapper(audio_frame.format().unwrap())
                                            .to_string();
                                    packet.audio_number_of_channels =
                                        audio_frame.number_of_channels();
                                    packet.audio_number_of_frames = audio_frame.number_of_frames();
                                    packet.audio_sample_rate = audio_frame.sample_rate();
                                    on_frame.emit(packet);
                                    audio_frame.close();
                                }
                                Err(e) => {
                                    log!("error", e);
                                }
                            }
                        }
                    };
                    join!(poll_video, poll_audio).await;
                });
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        if !self.initialized {
            ctx.link().send_message(Msg::Start);
        }
        html! {
            <video class="self-camera" autoplay=true id="webcam"></video>
        }
    }
}
