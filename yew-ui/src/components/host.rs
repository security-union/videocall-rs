use gloo_console::log;
use gloo_utils::window;
use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;

use std::fmt::Debug;
use std::future::join;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use types::protos::rust::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlVideoElement;
use web_sys::*;
use yew::prelude::*;

use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::constants::VIDEO_CODEC;
use crate::constants::VIDEO_HEIGHT;
use crate::constants::VIDEO_WIDTH;
use crate::model::transform_video_chunk;

pub enum Msg {
    Start,
}

enum State {
    Created,
    Initialized,
}

pub struct Host {
    state: State,
    pub destroy: Arc<AtomicBool>,
}

#[derive(Properties, Debug, PartialEq)]
pub struct MeetingProps {
    #[prop_or_default]
    pub id: String,

    #[prop_or_default]
    pub on_frame: Callback<MediaPacket>,

    #[prop_or_default]
    pub on_audio: Callback<AudioData>,

    #[prop_or_default]
    pub email: String,
}

impl Host {
    async fn start_video(
        email: Box<String>,
        device: MediaStream,
        on_frame: Box<Callback<MediaPacket>>,
        destroy: Arc<AtomicBool>,
    ) {
        let mut buffer: [u8; 300000] = [0; 300000];
        let video_output_handler = Box::new(move |chunk: JsValue| {
            let chunk = web_sys::EncodedVideoChunk::from(chunk);
            let media_packet: MediaPacket =
                transform_video_chunk(chunk, &mut buffer, email.clone());
            on_frame.emit(media_packet);
        });
        let video_track = Box::new(
            device
                .get_video_tracks()
                .find(&mut |_: JsValue, _: u32, _: Array| true)
                .unchecked_into::<VideoTrack>(),
        );
        let video_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
            log!("error_handler error", e);
        }) as Box<dyn FnMut(JsValue)>);

        let video_output_handler = Closure::wrap(video_output_handler as Box<dyn FnMut(JsValue)>);

        let video_encoder_init = VideoEncoderInit::new(
            video_error_handler.as_ref().unchecked_ref(),
            video_output_handler.as_ref().unchecked_ref(),
        );

        let video_encoder = Box::new(VideoEncoder::new(&video_encoder_init).unwrap());
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

        let mut video_encoder_config =
            VideoEncoderConfig::new(&VIDEO_CODEC, VIDEO_HEIGHT as u32, VIDEO_WIDTH as u32);

        video_encoder_config.bitrate(100000f64);
        video_encoder_config.latency_mode(LatencyMode::Realtime);
        video_encoder.configure(&video_encoder_config);

        let processor = MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
            &video_track.unchecked_into::<MediaStreamTrack>(),
        ))
        .unwrap();
        let video_reader = processor
            .readable()
            .get_reader()
            .unchecked_into::<ReadableStreamDefaultReader>();

        let video_element = window()
            .document()
            .unwrap()
            .get_element_by_id("webcam")
            .unwrap()
            .unchecked_into::<HtmlVideoElement>();
        video_element.set_src_object(Some(&device));
        video_element.set_muted(true);

        let mut counter = 0;
        loop {
            if destroy.load(Ordering::Acquire) {
                return;
            }
            match JsFuture::from(video_reader.read()).await {
                Ok(js_frame) => {
                    let video_frame = Reflect::get(&js_frame, &JsString::from("value"))
                        .unwrap()
                        .unchecked_into::<VideoFrame>();
                    let mut opts = VideoEncoderEncodeOptions::new();
                    counter = (counter + 1) % 50;
                    opts.key_frame(counter == 0);
                    video_encoder.encode(&video_frame);
                    video_frame.close();
                }
                Err(e) => {
                    log!("error", e);
                }
            }
        }
    }

    async fn start_audio(
        device: MediaStream,
        on_audio: Box<Callback<AudioData>>,
        destroy: Arc<AtomicBool>,
    ) {
        let audio_track = Box::new(
            device
                .get_audio_tracks()
                .find(&mut |_: JsValue, _: u32, _: Array| true)
                .unchecked_into::<AudioTrack>(),
        );
        let audio_processor = MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
            &audio_track.unchecked_into::<MediaStreamTrack>(),
        ))
        .unwrap();
        let audio_reader = audio_processor
            .readable()
            .get_reader()
            .unchecked_into::<ReadableStreamDefaultReader>();

        loop {
            if destroy.load(Ordering::Acquire) {
                return;
            }
            match JsFuture::from(audio_reader.read()).await {
                Ok(js_frame) => {
                    let audio_frame = Reflect::get(&js_frame, &JsString::from("value"))
                        .unwrap()
                        .unchecked_into::<AudioData>();
                    on_audio.emit(audio_frame);
                }
                Err(e) => {
                    log!("error", e);
                }
            }
        }
    }

    async fn get_device() -> MediaStream {
        let navigator = window().navigator();
        let media_devices = navigator.media_devices().unwrap();
        // TODO: Add dropdown so that user can select the device that they want to use.
        let mut constraints = MediaStreamConstraints::new();
        constraints.video(&Boolean::from(true));
        constraints.audio(&Boolean::from(true));
        let devices_query = media_devices
            .get_user_media_with_constraints(&constraints)
            .unwrap();
        JsFuture::from(devices_query)
            .await
            .unwrap()
            .unchecked_into::<MediaStream>()
    }

    async fn start_streaming(
        email: Box<String>,
        on_frame: Box<Callback<MediaPacket>>,
        on_audio: Box<Callback<AudioData>>,
        destroy: Arc<AtomicBool>,
    ) {
        let device = Self::get_device().await;
        join!(
            Self::start_video(email, device.clone(), on_frame, destroy.clone()),
            Self::start_audio(device, on_audio, destroy)
        )
        .await;
    }

    fn initialize(&mut self, ctx: &Context<Self>) {
        self.state = State::Initialized;
        let on_frame = Box::new(ctx.props().on_frame.clone());
        let on_audio = Box::new(ctx.props().on_audio.clone());
        let email = Box::new(ctx.props().email.clone());
        let destroy = self.destroy.clone();

        wasm_bindgen_futures::spawn_local(async move {
            Self::start_streaming(email, on_frame, on_audio, destroy).await;
        });
    }
}

impl Component for Host {
    type Message = Msg;
    type Properties = MeetingProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            state: State::Created,
            destroy: Arc::new(AtomicBool::new(false)),
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match (&self.state, msg) {
            (&State::Initialized, Msg::Start) => false,
            (&State::Created, Msg::Start) => {
                self.initialize(ctx);
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        // Initialize component on first render.
        if let State::Created = self.state {
            ctx.link().send_message(Msg::Start);
        }
        html! {
            <video class="self-camera" autoplay=true id="webcam"></video>
        }
    }

    fn destroy(&mut self, _ctx: &Context<Self>) {
        self.destroy.store(true, Ordering::Release);
    }
}
