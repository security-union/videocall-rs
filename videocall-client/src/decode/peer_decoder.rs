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

use super::audio_decoder_wrapper::{AudioDecoderTrait, AudioDecoderWrapper};
use super::config::configure_audio_context;
use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_CODEC;
use crate::constants::AUDIO_SAMPLE_RATE;
use gloo_timers::callback::Interval;
use log::error;
use std::sync::Arc;
use std::sync::Mutex;
use videocall_codecs::decoder::{Decodable, DecodedFrame, Decoder, VideoCodec};
use videocall_codecs::frame::{FrameType, VideoFrame as CodecVideoFrame};
use videocall_codecs::jitter_buffer::JitterBuffer;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::window;
use web_sys::HtmlCanvasElement;
use web_sys::{AudioData, AudioDecoderConfig, AudioDecoderInit};
use web_sys::{CanvasRenderingContext2d, CodecState};
use web_sys::{MediaStreamTrackGenerator, MediaStreamTrackGeneratorInit};
use web_time;

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
pub struct VideoPeerDecoder {
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    _polling_interval: Interval,
}

impl VideoPeerDecoder {
    pub fn new(canvas_id: &str) -> Result<Self, JsValue> {
        let id = canvas_id.to_owned();

        let on_decoded_frame = move |frame: DecodedFrame| {
            // This closure is called from the WasmDecoder, which runs in a worker.
            // It receives the decoded frame and renders it to the canvas.
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

            render_canvas.set_width(frame.width);
            render_canvas.set_height(frame.height);

            // TODO: Render the frame.data (which is likely YUV or similar) to the canvas.
            // This will require a conversion from the raw format to something Canvas can draw,
            // like an ImageData object. For now, we'll just clear the canvas.
            ctx.clear_rect(0.0, 0.0, frame.width as f64, frame.height as f64);
            log::info!("Rendered frame {}", frame.sequence_number);
        };

        let decoder = Decoder::new(
            VideoCodec::VP9, // Or whichever codec you are using
            Box::new(on_decoded_frame),
        );

        let jitter_buffer = Arc::new(Mutex::new(JitterBuffer::new(Box::new(decoder))));

        let jb_clone = jitter_buffer.clone();
        // Poll the jitter buffer to check for frames that are ready for playout.
        let polling_interval = Interval::new(20, move || {
            let now = web_time::SystemTime::now()
                .duration_since(web_time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis();
            let mut jb = jb_clone.lock().unwrap();
            jb.find_and_move_continuous_frames(now);
        });

        Ok(Self {
            jitter_buffer,
            _polling_interval: polling_interval,
        })
    }

    fn get_frame_type(&self, packet: &Arc<MediaPacket>) -> FrameType {
        match packet.frame_type.as_str() {
            "key" => FrameType::KeyFrame,
            _ => FrameType::DeltaFrame,
        }
    }

    pub fn is_waiting_for_keyframe(&self) -> bool {
        self.jitter_buffer.lock().unwrap().is_waiting_for_keyframe()
    }
}

impl PeerDecode for VideoPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        let now = web_time::SystemTime::now()
            .duration_since(web_time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();

        if let Some(video_metadata) = packet.video_metadata.as_ref() {
            let video_frame = CodecVideoFrame {
                sequence_number: video_metadata.sequence,
                frame_type: self.get_frame_type(packet),
                data: packet.data.clone(),
            };

            self.jitter_buffer
                .lock()
                .unwrap()
                .insert_frame(video_frame, now);
        }

        Ok(DecodeStatus {
            _rendered: true,    // This is now optimistic; rendering happens asynchronously.
            first_frame: false, // We can't easily know this synchronously anymore.
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
pub struct StandardAudioPeerDecoder {
    pub decoder: AudioDecoderWrapper,
    decoded: bool,
    _error: Closure<dyn FnMut(JsValue)>, // member exists to keep the closure in scope for the life of the struct
    _output: Closure<dyn FnMut(AudioData)>, // member exists to keep the closure in scope for the life of the struct
    _audio_context: web_sys::AudioContext,  // Keep audio context alive
}

impl StandardAudioPeerDecoder {
    pub fn new(speaker_device_id: Option<String>) -> Result<Self, JsValue> {
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            error!("{:?}", e);
        }) as Box<dyn FnMut(JsValue)>);
        let audio_stream_generator =
            MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new("audio")).unwrap();
        // The audio context is used to reproduce audio.
        let audio_context =
            configure_audio_context(&audio_stream_generator, speaker_device_id).unwrap();

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

impl Drop for StandardAudioPeerDecoder {
    fn drop(&mut self) {
        if let Err(e) = self._audio_context.close() {
            log::error!("Error closing audio context: {:?}", e);
        }
    }
}

impl PeerDecode for StandardAudioPeerDecoder {
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
