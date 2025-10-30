/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

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
use log::error;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use videocall_codecs::decoder::WasmDecoder;
use videocall_codecs::frame::{FrameBuffer, FrameType, VideoFrame as CodecVideoFrame};
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlCanvasElement;
use web_sys::{AudioData, AudioDecoderConfig, AudioDecoderInit};
use web_sys::{CanvasRenderingContext2d, CodecState};
use web_sys::{MediaStreamTrackGenerator, MediaStreamTrackGeneratorInit};
use web_time;

pub struct DecodeStatus {
    pub _rendered: bool,
    pub first_frame: bool,
}

pub trait PeerDecode {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus>;
}

/// Cached canvas rendering context to avoid repeated DOM lookups
struct CanvasCache {
    canvas: HtmlCanvasElement,
    ctx: CanvasRenderingContext2d,
    /// Track current dimensions to avoid expensive resize operations
    current_width: u32,
    current_height: u32,
}

///
/// VideoPeerDecoder
///
/// Constructor must be given the DOM id of an HtmlCanvasElement into which the video should be
/// rendered. The size of the canvas is set at decode time to match the image size from the media
/// data.
///
pub struct VideoPeerDecoder {
    decoder: Box<dyn VideoFrameDecoder>,
}

// Trait to handle VideoFrame callbacks in WASM
trait VideoFrameDecoder {
    fn push_frame(&self, frame: FrameBuffer);
    fn is_waiting_for_keyframe(&self) -> bool;
    fn flush(&self);
    fn set_stream_context(&self, _from_peer: String, _to_peer: String) {}
}

struct WasmVideoFrameDecoder {
    decoder: WasmDecoder,
}

impl VideoFrameDecoder for WasmVideoFrameDecoder {
    fn push_frame(&self, frame: FrameBuffer) {
        self.decoder.push_frame(frame);
    }

    fn is_waiting_for_keyframe(&self) -> bool {
        self.decoder.is_waiting_for_keyframe()
    }

    fn flush(&self) {
        self.decoder.flush()
    }

    fn set_stream_context(&self, from_peer: String, to_peer: String) {
        self.decoder.set_context(from_peer, to_peer);
    }
}

impl VideoPeerDecoder {
    pub fn new(canvas_id: &str) -> Result<Self, JsValue> {
        let canvas_id = canvas_id.to_owned();
        // Use Option<CanvasCache> for lazy initialization on first frame
        let canvas_cache: Rc<RefCell<Option<CanvasCache>>> = Rc::new(RefCell::new(None));

        let on_video_frame = move |video_frame: web_sys::VideoFrame| {
            Self::render_to_canvas_cached(&canvas_id, &canvas_cache, video_frame);
        };

        let wasm_decoder = videocall_codecs::decoder::WasmDecoder::new_with_video_frame_callback(
            videocall_codecs::decoder::VideoCodec::VP9,
            Box::new(on_video_frame),
        );

        let decoder = Box::new(WasmVideoFrameDecoder {
            decoder: wasm_decoder,
        });
        Ok(Self { decoder })
    }

    /// Create and cache canvas and context references once at initialization
    fn create_canvas_cache(canvas_id: &str) -> Result<CanvasCache, JsValue> {
        let window = web_sys::window().ok_or("No window found")?;
        let document = window.document().ok_or("No document found")?;
        let canvas_element = document
            .get_element_by_id(canvas_id)
            .ok_or_else(|| format!("Canvas element with id '{canvas_id}' not found"))?;

        let canvas = canvas_element
            .dyn_into::<HtmlCanvasElement>()
            .map_err(|_| "Element is not a canvas")?;

        let ctx = canvas
            .get_context("2d")?
            .ok_or("Failed to get 2d context")?
            .dyn_into::<CanvasRenderingContext2d>()?;

        // Initialize with current canvas dimensions
        let current_width = canvas.width();
        let current_height = canvas.height();

        Ok(CanvasCache {
            canvas,
            ctx,
            current_width,
            current_height,
        })
    }

    /// Provide original peer IDs to the underlying decoder so worker can tag diagnostics
    pub fn set_stream_context(&self, from_peer: String, to_peer: String) {
        self.decoder.set_stream_context(from_peer, to_peer);
    }

    /// Check if cached canvas is still valid (connected to DOM)
    fn is_canvas_valid(cache: &CanvasCache) -> bool {
        // Check if canvas is still connected to the document
        cache.canvas.is_connected()
    }

    /// Render video frame using cached canvas/context (Steps 1 & 2: optimized DOM + conditional resize)
    /// Lazily initializes canvas cache on first frame to avoid race conditions
    /// Automatically invalidates and reinitializes cache when canvas is recreated (peer join/leave)
    fn render_to_canvas_cached(
        canvas_id: &str,
        canvas_cache: &Rc<RefCell<Option<CanvasCache>>>,
        video_frame: web_sys::VideoFrame,
    ) {
        let mut cache_option = canvas_cache.borrow_mut();

        // Check if we need to initialize or reinitialize the cache
        let needs_init = match cache_option.as_ref() {
            None => true,                                 // Never initialized
            Some(cache) => !Self::is_canvas_valid(cache), // Canvas was recreated (peer join/leave)
        };

        if needs_init {
            match Self::create_canvas_cache(canvas_id) {
                Ok(cache) => {
                    if cache_option.is_some() {
                        log::info!(
                            "Canvas cache reinitialized for '{canvas_id}' (canvas was recreated)"
                        );
                    } else {
                        log::info!("Canvas cache initialized for '{canvas_id}'");
                    }
                    *cache_option = Some(cache);
                }
                Err(e) => {
                    // Canvas not ready yet - this is normal on first few frames
                    log::debug!("Canvas '{canvas_id}' not ready yet: {e:?}");
                    video_frame.close();
                    return;
                }
            }
        }

        // At this point we have a valid cache
        let cache = cache_option.as_mut().unwrap();
        let width = video_frame.display_width();
        let height = video_frame.display_height();

        // Step 2: Only resize canvas when dimensions actually change
        // This is a HUGE performance win - set_width/set_height are very expensive!
        let needs_resize = cache.current_width != width || cache.current_height != height;

        if needs_resize {
            cache.canvas.set_width(width);
            cache.canvas.set_height(height);
            cache.current_width = width;
            cache.current_height = height;
            log::debug!("Canvas resized to {width}x{height}");
            // Note: set_width/set_height automatically clears the canvas, so no need for clear_rect
        }

        if let Err(e) = cache
            .ctx
            .draw_image_with_video_frame(&video_frame, 0.0, 0.0)
        {
            log::error!("Error drawing video frame: {e:?}");
        } else {
            log::debug!("Rendered video frame ({width}x{height})");
        }

        video_frame.close();
    }

    fn get_frame_type(&self, packet: &Arc<MediaPacket>) -> FrameType {
        match packet.frame_type.as_str() {
            "key" => FrameType::KeyFrame,
            _ => FrameType::DeltaFrame,
        }
    }

    pub fn is_waiting_for_keyframe(&self) -> bool {
        self.decoder.is_waiting_for_keyframe()
    }

    pub fn flush(&self) {
        self.decoder.flush()
    }
}

impl PeerDecode for VideoPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        if let Some(video_metadata) = packet.video_metadata.as_ref() {
            let video_frame = CodecVideoFrame {
                sequence_number: video_metadata.sequence,
                timestamp: packet.timestamp,
                frame_type: self.get_frame_type(packet),
                data: packet.data.clone(),
            };

            // Create a FrameBuffer and push it to the decoder
            let current_time_ms = web_time::SystemTime::now()
                .duration_since(web_time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis();

            let frame_buffer = FrameBuffer::new(video_frame, current_time_ms);

            // Use the new ergonomic API - decoder handles jitter buffer internally,
            // and calls our VideoFrame callback for rendering
            self.decoder.push_frame(frame_buffer);
        }

        Ok(DecodeStatus {
            _rendered: true,
            first_frame: false,
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
            error!("{e:?}");
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
                        error!("write chunk error {e:?}");
                    }
                    if let Err(e) = JsFuture::from(writer.write_with_chunk(&audio_data)).await {
                        error!("write chunk error {e:?}");
                    };
                    writer.release_lock();
                });
            }) {
                error!("error {e:?}");
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
            error!("Error closing audio context: {e:?}");
        }
    }
}

impl PeerDecode for StandardAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        let first_frame = !self.decoded;
        let current_state = self.decoder.state();
        log::debug!("Audio decoder state before decode: {current_state:?}");

        match current_state {
            CodecState::Configured => {
                log::debug!(
                    "Decoding audio packet with sequence: {}",
                    packet.audio_metadata.sequence
                );
                if let Err(e) = self.decoder.decode(packet.clone()) {
                    log::error!("Error decoding audio packet: {e:?}");
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
                    log::error!("Failed to reconfigure audio decoder: {e:?}");
                    return Err(anyhow::anyhow!("Failed to reconfigure audio decoder"));
                }
            }
            _ => {
                log::warn!("Unexpected audio decoder state: {current_state:?}");
            }
        }

        Ok(DecodeStatus {
            _rendered: true,
            first_frame,
        })
    }
}
