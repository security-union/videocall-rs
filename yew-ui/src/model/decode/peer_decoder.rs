// This submodule defines two pub types:
//
//      AudioPeerDecoder
//      VideoPeerDecoder
//
// Both implement a method decoder.decode(packet) that decodes and sends the result to the
// appropriate output, as configured in the new() constructor.
//
// Both are specializations of a generic type PeerDecoder<...> for the decoding logic,
// and each one's new() contains the type-specific creation/configuration code.
//

use super::config::configure_audio_context;
use super::video_decoder_with_buffer::VideoDecoderWithBuffer;
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::constants::VIDEO_CODEC;
use crate::model::EncodedVideoChunkTypeWrapper;
use gloo_console::log;
use std::sync::Arc;
use types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::window;
use web_sys::{AudioData, AudioDecoder, AudioDecoderConfig, AudioDecoderInit};
use web_sys::{CanvasRenderingContext2d, CodecState};
use web_sys::{
    EncodedAudioChunk, EncodedAudioChunkInit, EncodedAudioChunkType, EncodedVideoChunkType,
};
use web_sys::{HtmlCanvasElement, HtmlImageElement};
use web_sys::{MediaStreamTrackGenerator, MediaStreamTrackGeneratorInit};
use web_sys::{VideoDecoderConfig, VideoDecoderInit, VideoFrame};

//
// Generic type for decoders captures common functionality.
//
pub struct PeerDecoder<WebDecoder, Chunk> {
    decoder: WebDecoder,
    waiting_for_keyframe: bool,
    _error: Closure<dyn FnMut(JsValue)>, // member exists to keep the closure in scope for the life of the struct
    _output: Closure<dyn FnMut(Chunk)>, // member exists to keep the closure in scope for the life of the struct
}

impl<WebDecoder, ChunkType> PeerDecoder<WebDecoder, ChunkType> {
    pub fn is_waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }
}

///
/// Core decoding logic.
///
/// (Defined as a macro rather than a trait because traits can't refer to members.)
///
macro_rules! impl_decode {
    ($self: expr, $packet: expr, $ChunkType: ty, $ref: tt) => {{
        let chunk_type = $self.get_chunk_type(&$packet);
        if !$self.waiting_for_keyframe || chunk_type == <$ChunkType>::Key {
            match $self.decoder.state() {
                CodecState::Configured => {
                    $self
                        .decoder
                        .decode(opt_ref!($self.get_chunk($packet, chunk_type), $ref));
                    $self.waiting_for_keyframe = false;
                }
                CodecState::Closed => {
                    return Err(());
                }
                _ => {}
            }
        }
        Ok(())
    }};
}

macro_rules! opt_ref {
    ($val: expr, "ref") => {
        &$val
    };
    ($val: expr, "") => {
        $val
    };
}

///
/// VideoPeerDecoder
///
/// Constructor must be given the DOM id of an HtmlCanvasElement into which the video should be
/// rendered. The size of the canvas is set at decode time to match the image size from the media
/// data.
///
pub type VideoPeerDecoder = PeerDecoder<VideoDecoderWithBuffer, JsValue>;

impl PeerDecoder<VideoDecoderWithBuffer, JsValue> {
    pub fn new(canvas_id: &String) -> PeerDecoder<VideoDecoderWithBuffer, JsValue> {
        let id = canvas_id.clone();
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            log!(&e);
        }) as Box<dyn FnMut(JsValue)>);
        let output = Closure::wrap(Box::new(move |original_chunk: JsValue| {
            let chunk = Box::new(original_chunk);
            let video_chunk = chunk.unchecked_into::<VideoFrame>();
            let width = video_chunk.coded_width();
            let height = video_chunk.coded_height();
            let video_chunk = video_chunk.unchecked_into::<HtmlImageElement>();
            let render_canvas = window()
                .unwrap()
                .document()
                .unwrap()
                .get_element_by_id(&id)
                .unwrap()
                .unchecked_into::<HtmlCanvasElement>();
            let ctx = render_canvas
                .get_context("2d")
                .unwrap()
                .unwrap()
                .unchecked_into::<CanvasRenderingContext2d>();
            render_canvas.set_width(width);
            render_canvas.set_height(height);
            if let Err(e) = ctx.draw_image_with_html_image_element(&video_chunk, 0.0, 0.0) {
                log!("error ", e);
            }
            video_chunk.unchecked_into::<VideoFrame>().close();
        }) as Box<dyn FnMut(JsValue)>);
        let decoder = VideoDecoderWithBuffer::new(&VideoDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        ))
        .unwrap();
        decoder.configure(&VideoDecoderConfig::new(VIDEO_CODEC));
        PeerDecoder {
            decoder,
            waiting_for_keyframe: true,
            _error: error,
            _output: output,
        }
    }

    pub fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<(), ()> {
        impl_decode!(self, packet, EncodedVideoChunkType, "")
    }

    fn get_chunk_type(&self, packet: &Arc<MediaPacket>) -> EncodedVideoChunkType {
        EncodedVideoChunkTypeWrapper::from(packet.frame_type.as_str()).0
    }

    fn get_chunk(&self, packet: &Arc<MediaPacket>, _: EncodedVideoChunkType) -> Arc<MediaPacket> {
        packet.clone()
    }
}

///
/// AudioPeerDecoder
///
/// Plays audio to the standard audio stream.
///
pub type AudioPeerDecoder = PeerDecoder<AudioDecoder, AudioData>;

impl PeerDecoder<AudioDecoder, AudioData> {
    pub fn new() -> PeerDecoder<AudioDecoder, AudioData> {
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            log!(&e);
        }) as Box<dyn FnMut(JsValue)>);
        let audio_stream_generator =
            MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new("audio")).unwrap();
        // The audio context is used to reproduce audio.
        let _audio_context = configure_audio_context(&audio_stream_generator).unwrap();

        let output = Closure::wrap(Box::new(move |audio_data: AudioData| {
            let writable = audio_stream_generator.writable();
            if writable.locked() {
                return;
            }
            if let Err(e) = writable.get_writer().map(|writer| {
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(e) = JsFuture::from(writer.ready()).await {
                        log!("write chunk error ", e);
                    }
                    if let Err(e) = JsFuture::from(writer.write_with_chunk(&audio_data)).await {
                        log!("write chunk error ", e);
                    };
                    writer.release_lock();
                });
            }) {
                log!("error", e);
            }
        }) as Box<dyn FnMut(AudioData)>);
        let decoder = AudioDecoder::new(&AudioDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        ))
        .unwrap();
        decoder.configure(&AudioDecoderConfig::new(
            AUDIO_CODEC,
            AUDIO_CHANNELS,
            AUDIO_SAMPLE_RATE,
        ));
        PeerDecoder {
            decoder,
            waiting_for_keyframe: true,
            _error: error,
            _output: output,
        }
    }

    pub fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<(), ()> {
        impl_decode!(self, packet, EncodedAudioChunkType, "ref")
    }

    fn get_chunk_type(&self, packet: &Arc<MediaPacket>) -> EncodedAudioChunkType {
        EncodedAudioChunkType::from_js_value(&JsValue::from(packet.frame_type.clone())).unwrap()
    }

    fn get_chunk(
        &self,
        packet: &Arc<MediaPacket>,
        chunk_type: EncodedAudioChunkType,
    ) -> EncodedAudioChunk {
        let audio_data = &packet.data;
        let audio_data_js: js_sys::Uint8Array =
            js_sys::Uint8Array::new_with_length(audio_data.len() as u32);
        audio_data_js.copy_from(audio_data.as_slice());
        let mut audio_chunk =
            EncodedAudioChunkInit::new(&audio_data_js.into(), packet.timestamp, chunk_type);
        audio_chunk.duration(packet.duration);
        EncodedAudioChunk::new(&audio_chunk).unwrap()
    }
}
