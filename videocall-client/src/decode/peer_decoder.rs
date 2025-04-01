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

use super::super::wrappers::EncodedVideoChunkTypeWrapper;
use super::audio_decoder_wrapper::{AudioDecoderTrait, AudioDecoderWrapper};
use super::config::configure_audio_context;
use super::media_decoder_with_buffer::VideoDecoderWithBuffer;
use super::video_decoder_wrapper::VideoDecoderWrapper;
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::constants::VIDEO_CODEC;
use log::error;
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::window;
use web_sys::EncodedVideoChunkType;
use web_sys::HtmlCanvasElement;
use web_sys::{AudioData, AudioDecoderConfig, AudioDecoderInit};
use web_sys::{CanvasRenderingContext2d, CodecState};
use web_sys::{MediaStreamTrackGenerator, MediaStreamTrackGeneratorInit};
use web_sys::{VideoDecoderConfig, VideoDecoderInit, VideoFrame};

pub struct DecodeStatus {
    pub _rendered: bool,
    pub first_frame: bool,
}

//
// Generic type for decoders captures common functionality.
//
#[derive(Debug)]
pub struct PeerDecoder<WebDecoder, Chunk> {
    pub decoder: WebDecoder,
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
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus>;
}

///
/// VideoPeerDecoder
///
/// Constructor must be given the DOM id of an HtmlCanvasElement into which the video should be
/// rendered. The size of the canvas is set at decode time to match the image size from the media
/// data.
///
pub type VideoPeerDecoder = PeerDecoder<VideoDecoderWithBuffer<VideoDecoderWrapper>, JsValue>;

impl VideoPeerDecoder {
    pub fn new(canvas_id: &str) -> Result<Self, JsValue> {
        let id = canvas_id.to_owned();
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            error!("{:?}", e);
        }) as Box<dyn FnMut(JsValue)>);
        
        // Direct canvas rendering implementation
        let output = Closure::wrap(Box::new(move |original_chunk: JsValue| {
            let chunk = Box::new(original_chunk);
            let video_chunk = chunk.unchecked_into::<VideoFrame>();
            let render_canvas = match window()
                .unwrap()
                .document()
                .unwrap()
                .get_element_by_id(&id) {
                    Some(canvas) => canvas.unchecked_into::<HtmlCanvasElement>(),
                    None => {
                        error!("Canvas element not found: {}", id);
                        video_chunk.close();
                        return;
                    }
                };
                
            let ctx = match render_canvas.get_context("2d") {
                Ok(Some(ctx)) => ctx.unchecked_into::<CanvasRenderingContext2d>(),
                _ => {
                    error!("Failed to get 2d context for canvas");
                    video_chunk.close();
                    return;
                }
            };

            // Get the video frame's dimensions from its settings
            let width = video_chunk.display_width();
            let height = video_chunk.display_height();

            // Set canvas dimensions to match video frame
            render_canvas.set_width(width);
            render_canvas.set_height(height);

            // Clear the canvas and draw the frame
            ctx.clear_rect(0.0, 0.0, width as f64, height as f64);
            if let Err(e) = ctx.draw_image_with_video_frame(&video_chunk, 0.0, 0.0) {
                error!("Error drawing video frame: {:?}", e);
            }

            video_chunk.close();
        }) as Box<dyn FnMut(JsValue)>);
        
        // Create and configure the video decoder
        let decoder = VideoDecoderWithBuffer::new(&VideoDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        ))
        .unwrap();
        decoder.configure(&VideoDecoderConfig::new(VIDEO_CODEC))?;
        
        Ok(Self {
            decoder,
            waiting_for_keyframe: true,
            decoded: false,
            _error: error,
            _output: output,
        })
    }

    fn get_chunk_type(&self, packet: &Arc<MediaPacket>) -> EncodedVideoChunkType {
        EncodedVideoChunkTypeWrapper::from(packet.frame_type.as_str()).0
    }
}

impl PeerDecode for VideoPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        let first_frame = !self.decoded;
        let chunk_type = self.get_chunk_type(packet);

        if !self.waiting_for_keyframe || chunk_type == EncodedVideoChunkType::Key {
            match self.decoder.state() {
                CodecState::Configured => {
                    self.decoder.decode(packet.clone());
                    self.waiting_for_keyframe = false;
                    self.decoded = true;
                }
                CodecState::Closed => {
                    return Err(anyhow::anyhow!("decoder closed"));
                }
                _ => {}
            }
        }

        Ok(DecodeStatus {
            _rendered: true,
            first_frame,
        })
    }
}

///
/// AudioPeerDecoder
///
/// Plays audio to the standard audio stream.
///
/// This is important https://plnkr.co/edit/1yQd8ozGXlV9bwK6?preview
/// https://github.com/WebAudio/web-audio-api-v2/issues/133
pub struct AudioPeerDecoder {
    pub decoder: AudioDecoderWrapper,
    decoded: bool,
    _error: Closure<dyn FnMut(JsValue)>, // member exists to keep the closure in scope for the life of the struct
    _output: Closure<dyn FnMut(AudioData)>, // member exists to keep the closure in scope for the life of the struct
    _audio_context: web_sys::AudioContext,  // Keep audio context alive
}

impl AudioPeerDecoder {
    pub fn new() -> Result<Self, JsValue> {
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            error!("{:?}", e);
        }) as Box<dyn FnMut(JsValue)>);
        let audio_stream_generator =
            MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new("audio")).unwrap();
        // The audio context is used to reproduce audio.
        let audio_context = configure_audio_context(&audio_stream_generator).unwrap();

        let output = Closure::wrap(Box::new(move |audio_data: AudioData| {
            let writable = audio_stream_generator.writable();
            if writable.locked() {
                return;
            }
            if let Err(e) = writable.get_writer().map(|writer| {
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(e) = JsFuture::from(writer.ready()).await {
                        error!("write chunk error {:?}", e);
                    }
                    if let Err(e) = JsFuture::from(writer.write_with_chunk(&audio_data)).await {
                        error!("write chunk error {:?}", e);
                    };
                    writer.release_lock();
                });
            }) {
                error!("error {:?}", e);
            }
        }) as Box<dyn FnMut(AudioData)>);
        let decoder = AudioDecoderWrapper::new(&AudioDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        ))?;
        decoder.configure(&AudioDecoderConfig::new(
            AUDIO_CODEC,
            AUDIO_CHANNELS,
            AUDIO_SAMPLE_RATE,
        ))?;
        Ok(Self {
            decoder,
            decoded: false,
            _error: error,
            _output: output,
            _audio_context: audio_context,
        })
    }
}

impl Drop for AudioPeerDecoder {
    fn drop(&mut self) {
        if let Err(e) = self._audio_context.close() {
            log::error!("Error closing audio context: {:?}", e);
        }
    }
}

impl PeerDecode for AudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        let first_frame = !self.decoded;
        let current_state = self.decoder.state();
        log::debug!("Audio decoder state before decode: {:?}", current_state);

        match current_state {
            CodecState::Configured => {
                log::debug!(
                    "Decoding audio packet with sequence: {}",
                    packet.audio_metadata.sequence
                );
                if let Err(e) = self.decoder.decode(packet.clone()) {
                    log::error!("Error decoding audio packet: {:?}", e);
                    return Err(anyhow::anyhow!("Failed to decode audio packet"));
                }
                self.decoded = true;
                log::debug!(
                    "Audio packet decoded, new state: {:?}",
                    self.decoder.state()
                );
            }
            CodecState::Closed => {
                log::error!("Audio decoder closed unexpectedly");
                return Err(anyhow::anyhow!("decoder closed"));
            }
            CodecState::Unconfigured => {
                log::warn!("Audio decoder unconfigured, attempting to reconfigure");
                if let Err(e) = self.decoder.configure(&AudioDecoderConfig::new(
                    AUDIO_CODEC,
                    AUDIO_CHANNELS,
                    AUDIO_SAMPLE_RATE,
                )) {
                    log::error!("Failed to reconfigure audio decoder: {:?}", e);
                    return Err(anyhow::anyhow!("Failed to reconfigure audio decoder"));
                }
            }
            _ => {
                log::warn!("Unexpected audio decoder state: {:?}", current_state);
            }
        }

        Ok(DecodeStatus {
            _rendered: true,
            first_frame,
        })
    }
}
