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

use super::video_decoder_with_buffer::VideoDecoderWithBuffer;
use super::video_decoder_wrapper::VideoDecoderWrapper;
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::constants::VIDEO_CODEC;
use crate::model::audio_worklet_codec::AudioWorkletCodec;
use crate::model::audio_worklet_codec::DecoderInitOptions;
use crate::model::audio_worklet_codec::DecoderMessages;
use crate::model::EncodedVideoChunkTypeWrapper;
use js_sys::Uint8Array;
use log::error;
use std::sync::Arc;
use types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::window;
use web_sys::AudioContext;
use web_sys::AudioContextOptions;
use web_sys::AudioData;
use web_sys::EncodedVideoChunkType;
use web_sys::HtmlCanvasElement;
use web_sys::{CanvasRenderingContext2d, CodecState};
use web_sys::{VideoDecoderConfig, VideoDecoderInit, VideoFrame};

pub struct DecodeStatus {
    pub rendered: bool,
    pub first_frame: bool,
}

//
// Generic type for decoders captures common functionality.
//
pub struct PeerDecoder<WebDecoder, Chunk> {
    decoder: WebDecoder,
    waiting_for_keyframe: bool,
    decoded: bool,
    _error: Closure<dyn FnMut(JsValue)>, // member exists to keep the closure in scope for the life of the struct
    _output: Closure<dyn FnMut(Chunk)>, // member exists to keep the closure in scope for the life of the struct
}

impl<WebDecoder, ChunkType> PeerDecoder<WebDecoder, ChunkType> {
    pub fn is_waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }
}

pub trait PeerDecode {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<DecodeStatus, ()>;
}

///
/// Implementation of decode(packet) -> Result<DecodeStatus, ()>
///
/// (Defined as a macro rather than a trait because traits can't refer to members.)
///
macro_rules! impl_decode {
    ($self: expr, $packet: expr, $ChunkType: ty, $ref: tt) => {{
        let first_frame = !$self.decoded;
        let chunk_type = $self.get_chunk_type(&$packet);
        if !$self.waiting_for_keyframe || chunk_type == <$ChunkType>::Key {
            match $self.decoder.state() {
                CodecState::Configured => {
                    $self
                        .decoder
                        .decode(opt_ref!($self.get_chunk($packet, chunk_type), $ref));
                    $self.waiting_for_keyframe = false;
                    $self.decoded = true;
                }
                CodecState::Closed => {
                    return Err(());
                }
                _ => {}
            }
        }
        Ok(DecodeStatus {
            rendered: true,
            first_frame,
        })
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
pub type VideoPeerDecoder = PeerDecoder<VideoDecoderWithBuffer<VideoDecoderWrapper>, VideoFrame>;

impl VideoPeerDecoder {
    pub fn new(canvas_id: &str) -> Self {
        let id = canvas_id.to_owned();
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            error!("{:?}", e);
        }) as Box<dyn FnMut(JsValue)>);
        let output = Closure::wrap(Box::new(move |video_chunk: VideoFrame| {
            let width = video_chunk.coded_width();
            let height = video_chunk.coded_height();
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
            ctx.clear_rect(0., 0., width as f64, height as f64);
            if let Err(e) = ctx.draw_image_with_video_frame_and_dw_and_dh(
                &video_chunk,
                0.0,
                0.0,
                render_canvas.width() as f64,
                render_canvas.height() as f64,
            ) {
                error!("error {:?}", e);
            }
            video_chunk.close();
        }) as Box<dyn FnMut(VideoFrame)>);
        let decoder = VideoDecoderWithBuffer::new(&VideoDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        ))
        .unwrap();
        decoder.configure(&VideoDecoderConfig::new(VIDEO_CODEC));
        Self {
            decoder,
            waiting_for_keyframe: true,
            decoded: false,
            _error: error,
            _output: output,
        }
    }

    fn get_chunk_type(&self, packet: &Arc<MediaPacket>) -> EncodedVideoChunkType {
        EncodedVideoChunkTypeWrapper::from(packet.frame_type.as_str()).0
    }

    fn get_chunk(&self, packet: &Arc<MediaPacket>, _: EncodedVideoChunkType) -> Arc<MediaPacket> {
        packet.clone()
    }
}

impl PeerDecode for VideoPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<DecodeStatus, ()> {
        impl_decode!(self, packet, EncodedVideoChunkType, "")
    }
}

///
/// AudioPeerDecoder
///
/// Plays audio to the standard audio stream.
///
/// This is important https://plnkr.co/edit/1yQd8ozGXlV9bwK6?preview
/// https://github.com/WebAudio/web-audio-api-v2/issues/133

pub type AudioPeerDecoder = PeerDecoder<AudioWorkletCodec, AudioData>;

impl AudioPeerDecoder {
    pub fn new() -> Self {
        let codec = AudioWorkletCodec::default();
        {
            let codec = codec.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let mut options = AudioContextOptions::new();
                options.sample_rate(AUDIO_SAMPLE_RATE as f32);
                let context = AudioContext::new_with_context_options(&options).unwrap();

                let gain_node = context.create_gain().unwrap();

                // Add the decoder audio worklet to the context
                let worklet_node = codec
                    .create_node(
                        &context,
                        "/decoderWorker.min.js",
                        "decoder-worklet",
                        AUDIO_CHANNELS,
                    )
                    .await
                    .unwrap();

                let _ = worklet_node
                    .connect_with_audio_node(&gain_node)
                    .unwrap()
                    .connect_with_audio_node(&context.destination());

                let _ = codec.send_message(DecoderMessages::Init {
                    options: Some(DecoderInitOptions {
                        output_buffer_sample_rate: Some(AUDIO_SAMPLE_RATE),
                        ..Default::default()
                    }),
                });
            })
        }

        // These aren't really used, but kept to ensure parity with video decoding
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            error!("{:?}", e);
        }) as Box<dyn FnMut(JsValue)>);

        let output = Closure::wrap(Box::new(move |_| {}) as Box<dyn FnMut(AudioData)>);

        Self {
            decoder: codec,
            // There is no keyframe
            waiting_for_keyframe: false,
            decoded: false,
            _error: error,
            _output: output,
        }
    }

    fn get_chunk(&self, packet: &Arc<MediaPacket>) -> Uint8Array {
        let audio_data = &packet.data;
        let audio_data_js: js_sys::Uint8Array =
            js_sys::Uint8Array::new_with_length(audio_data.len() as u32);
        audio_data_js.copy_from(audio_data.as_slice());
        audio_data_js
    }
}

impl PeerDecode for AudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<DecodeStatus, ()> {
        let buffer = self.get_chunk(packet);
        let first_frame = !self.decoded;

        let rendered = if self.decoder.is_instantiated() {
            let _ = self.decoder.send_message(&DecoderMessages::Decode {
                pages: buffer.to_vec(),
            });
            self.decoded = true;
            true
        } else {
            false
        };

        Ok(DecodeStatus {
            rendered,
            first_frame,
        })
    }
}
