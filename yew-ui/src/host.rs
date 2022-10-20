use gloo_console::log;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Number;
use js_sys::Reflect;
use protobuf::Message;
use std::fmt;
use std::fmt::Debug;
use types::protos::media_packet::media_packet;
use types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlVideoElement;
use web_sys::*;
use yew::prelude::*;
use yew_websocket::websocket::{Binary, Text};

use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
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

pub struct MediaPacketWrapper(pub MediaPacket);

impl From<Text> for MediaPacketWrapper {
    fn from(_: Text) -> Self {
        MediaPacketWrapper(MediaPacket::default())
    }
}

impl From<Binary> for MediaPacketWrapper {
    fn from(bin: Binary) -> Self {
        let media_packet: MediaPacket = bin
            .map(|data| MediaPacket::parse_from_bytes(&data.into_boxed_slice()).unwrap())
            .unwrap_or(MediaPacket::default());
        MediaPacketWrapper(media_packet)
    }
}

pub struct EncodedVideoChunkTypeWrapper(pub EncodedVideoChunkType);

impl From<String> for EncodedVideoChunkTypeWrapper {
    fn from(s: String) -> Self {
        match s.as_str() {
            "key" => EncodedVideoChunkTypeWrapper(EncodedVideoChunkType::Key),
            _ => EncodedVideoChunkTypeWrapper(EncodedVideoChunkType::Delta),
        }
    }
}

impl fmt::Display for EncodedVideoChunkTypeWrapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            EncodedVideoChunkType::Delta => write!(f, "delta"),
            EncodedVideoChunkType::Key => write!(f, "key"),
            _ => todo!(),
        }
    }
}

pub struct EncodedAudioChunkTypeWrapper(pub EncodedAudioChunkType);

impl From<String> for EncodedAudioChunkTypeWrapper {
    fn from(s: String) -> Self {
        match s.as_str() {
            "key" => EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Key),
            _ => EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Delta),
        }
    }
}

impl fmt::Display for EncodedAudioChunkTypeWrapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            EncodedAudioChunkType::Delta => write!(f, "delta"),
            EncodedAudioChunkType::Key => write!(f, "key"),
            _ => todo!(),
        }
    }
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
                let output_handler = {
                    let email = email.clone();
                    let on_frame = on_frame.clone();
                    Box::new(move |chunk: JsValue| {
                        let chunk = web_sys::EncodedVideoChunk::from(chunk);
                        let mut media_packet: MediaPacket = MediaPacket::default();
                        media_packet.email = *email.clone();
                        let byte_length: Number =
                            Reflect::get(&chunk, &JsString::from("byteLength"))
                                .unwrap()
                                .into();
                        let byte_length: usize = byte_length.as_f64().unwrap() as usize;
                        let mut chunk_data: Vec<u8> = vec![0; byte_length];
                        let mut chunk_data = chunk_data.as_mut_slice();
                        chunk.copy_to_with_u8_array(&mut chunk_data);
                        media_packet.video = chunk_data.to_vec();
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

                let audio_output_handler = {
                    let email = email.clone();
                    let on_frame = on_frame.clone();
                    Box::new(move |chunk: JsValue| {
                        log!("encoded audio packet :) ");
                        let chunk = web_sys::EncodedAudioChunk::from(chunk);
                        let mut media_packet: MediaPacket = MediaPacket::default();
                        media_packet.email = *email.clone();
                        let byte_length: usize =  chunk.byte_length() as usize;
                        let mut chunk_data: Vec<u8> = vec![0; byte_length];
                        let mut chunk_data = chunk_data.as_mut_slice();
                        chunk.copy_to_with_u8_array(&mut chunk_data);
                        media_packet.audio = chunk_data.to_vec();
                        media_packet.timestamp = chunk.timestamp();
                        media_packet.media_type = media_packet::MediaType::AUDIO.into();
                        media_packet.video_type =
                            EncodedAudioChunkTypeWrapper(chunk.type_()).to_string();
                        if let Some(duration0) = chunk.duration() {
                            media_packet.duration = duration0;
                        }
                        on_frame.emit(media_packet);
                    })
                };

                wasm_bindgen_futures::spawn_local(async move {
                    let navigator = window().navigator();
                    let media_devices = navigator.media_devices().unwrap();
                    let video_element = window()
                        .document()
                        .unwrap()
                        .get_element_by_id("webcam")
                        .unwrap()
                        .unchecked_into::<HtmlVideoElement>();

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

                    let error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                        log!("error_handler error", e);
                    })
                        as Box<dyn FnMut(JsValue)>);

                    let audio_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                        log!("audio_error_handler error", e);
                    })
                        as Box<dyn FnMut(JsValue)>);

                    let output_handler = Closure::wrap(output_handler as Box<dyn FnMut(JsValue)>);
                    let audio_output_handler =
                        Closure::wrap(audio_output_handler as Box<dyn FnMut(JsValue)>);
                    let video_encoder_init = VideoEncoderInit::new(
                        error_handler.as_ref().unchecked_ref(),
                        output_handler.as_ref().unchecked_ref(),
                    );
                    let audio_encoder_init = AudioEncoderInit::new(
                        audio_error_handler.as_ref().unchecked_ref(),
                        audio_output_handler.as_ref().unchecked_ref(),
                    );

                    let audio_encoder = AudioEncoder::new(&audio_encoder_init).unwrap();
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

                    let mut audio_encoder_config = AudioEncoderConfig::new(&AUDIO_CODEC);
                    audio_encoder_config.number_of_channels(AUDIO_CHANNELS);
                    audio_encoder_config.sample_rate(AUDIO_SAMPLE_RATE);
                    audio_encoder.configure(&audio_encoder_config);

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

                    loop {
                        // let mut counter = 0u32;

                        match JsFuture::from(video_reader.read()).await {
                            Ok(js_frame) => {
                                let video_frame = Reflect::get(&js_frame, &JsString::from("value"))
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
                        match JsFuture::from(audio_reader.read()).await {
                            Ok(js_frame) => {
                                let audio_frame = Reflect::get(&js_frame, &JsString::from("value"))
                                    .unwrap()
                                    .unchecked_into::<AudioData>();
                                log!("before encode packet", &audio_frame);
                                audio_encoder.encode(&audio_frame);
                                audio_frame.close();
                            }
                            Err(e) => {
                                log!("error", e);
                            }
                        }
                    }
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
