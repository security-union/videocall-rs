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
use videocall_codecs::frame::{FrameBuffer, FrameCodec, FrameType, VideoFrame as CodecVideoFrame};
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::media_packet::VideoCodec;
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

/// Cached canvas rendering context to avoid expensive DOM queries
struct CanvasRenderer {
    canvas: HtmlCanvasElement,
    context: CanvasRenderingContext2d,
    last_width: u32,
    last_height: u32,
    /// Peer context for diagnostics. Set via [`VideoPeerDecoder::set_stream_context`].
    from_peer: Option<String>,
    to_peer: Option<String>,
}

///
/// VideoPeerDecoder
///
/// Caches canvas and rendering context to avoid expensive DOM queries on every frame.
/// The canvas can be set after creation using `set_canvas()`, enabling flexible initialization.
///
pub struct VideoPeerDecoder {
    decoder: Box<dyn VideoFrameDecoder>,
    canvas_renderer: Rc<RefCell<Option<CanvasRenderer>>>,
    /// Discriminator tag emitted on diagnostics events so consumers can tell
    /// camera-video resolution events apart from screen-share ones. Mirrors
    /// the `media_type` metric already carried by the FPS/bitrate events
    /// (`"VIDEO"` or `"SCREEN"`).
    media_type: &'static str,
    /// Last `(source_width, source_height)` we saw on a `MediaPacket`'s
    /// `VideoMetadata`. Used to dedupe `video_source_resolution` diag events
    /// — those would otherwise fire on every decoded frame. `(0, 0)` means
    /// either we've never seen the field or the publisher is older /
    /// doesn't report it; in both cases we suppress the broadcast.
    last_source_dims: RefCell<(u32, u32)>,
    /// Peer-id pair used to tag the source-resolution diag event. We can't
    /// borrow it from the `CanvasRenderer` because that storage may be
    /// `None` when the canvas hasn't been wired yet, but
    /// `set_stream_context` *does* run before any decoded frames. Set there.
    stream_context: RefCell<Option<(String, String)>>,
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

/// Media-type discriminator passed to [`VideoPeerDecoder::new`]. Distinguishes
/// camera video streams from screen-share streams in diagnostics events so the
/// UI can chart them separately. The values match the existing `media_type`
/// metric carried on FPS/bitrate events.
pub const MEDIA_TYPE_CAMERA: &str = "VIDEO";
pub const MEDIA_TYPE_SCREEN: &str = "SCREEN";

/// Decide what `(from_peer, to_peer)` to stamp on a freshly-constructed
/// [`CanvasRenderer`] inside [`VideoPeerDecoder::set_canvas`].
///
/// Two real-world orderings have to converge here:
///
/// 1. Canvas attached *before* `set_stream_context` (camera path: the
///    `<canvas>` element exists at peer-tile mount, before the first packet
///    arrives). The renderer was created with `(None, None)`, then
///    `set_stream_context` populated it directly. Subsequent re-attachments
///    must preserve that pair.
/// 2. Canvas attached *after* `set_stream_context` (screen-share path: the
///    `ScreenCanvas` tile only mounts once the peer's screen-share is
///    advertised, which is after the first media packet — and the first
///    packet is what triggers `set_stream_context`). The prior renderer is
///    either absent or carries `(None, None)` and we must seed the new
///    renderer from the decoder-level `stream_context` instead, otherwise
///    `render_to_canvas_cached` cannot emit `video_resolution` diag events
///    (it gates on `renderer.to_peer.is_some()`) and the screen-share
///    resolution stays hidden in the Signal Quality tooltip for the whole
///    session. This was the #883 regression.
fn resolve_renderer_context(
    prior_renderer_ctx: Option<(Option<String>, Option<String>)>,
    decoder_stream_ctx: Option<&(String, String)>,
) -> (Option<String>, Option<String>) {
    if let Some((fp, tp)) = prior_renderer_ctx {
        if fp.is_some() || tp.is_some() {
            return (fp, tp);
        }
    }
    match decoder_stream_ctx {
        Some((fp, tp)) => (Some(fp.clone()), Some(tp.clone())),
        None => (None, None),
    }
}

impl VideoPeerDecoder {
    /// Create a new video decoder with optional canvas element.
    /// Use `set_canvas()` to provide the canvas if not available at construction time.
    ///
    /// `media_type` tags the resolution diagnostics event so the UI can route
    /// camera-video and screen-share resolution updates to the right place.
    /// Use [`MEDIA_TYPE_CAMERA`] for the peer's camera decoder and
    /// [`MEDIA_TYPE_SCREEN`] for the peer's screen-share decoder.
    pub fn new(
        canvas: Option<HtmlCanvasElement>,
        media_type: &'static str,
    ) -> Result<Self, JsValue> {
        let canvas_renderer = Rc::new(RefCell::new(None));

        // Initialize canvas if provided
        if let Some(canvas) = canvas {
            let context = canvas
                .get_context("2d")?
                .ok_or_else(|| JsValue::from_str("Failed to get 2d context"))?
                .dyn_into::<CanvasRenderingContext2d>()?;

            *canvas_renderer.borrow_mut() = Some(CanvasRenderer {
                canvas,
                context,
                last_width: 0,
                last_height: 0,
                from_peer: None,
                to_peer: None,
            });
        }

        let canvas_ref = canvas_renderer.clone();
        let on_video_frame = move |video_frame: web_sys::VideoFrame| {
            Self::render_to_canvas_cached(&canvas_ref, video_frame, media_type);
        };

        let wasm_decoder = videocall_codecs::decoder::WasmDecoder::new_with_video_frame_callback(
            videocall_codecs::decoder::VideoCodec::Vp9Profile0Level10Bit8,
            Box::new(on_video_frame),
        );

        let decoder = Box::new(WasmVideoFrameDecoder {
            decoder: wasm_decoder,
        });
        Ok(Self {
            decoder,
            canvas_renderer,
            media_type,
            last_source_dims: RefCell::new((0, 0)),
            stream_context: RefCell::new(None),
        })
    }

    /// Set or update the canvas element for rendering. Can be called multiple times.
    /// Preserves existing peer context (from_peer / to_peer) if already set.
    pub fn set_canvas(&self, canvas: HtmlCanvasElement) -> Result<(), JsValue> {
        let context = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("Failed to get 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;

        let mut guard = self.canvas_renderer.borrow_mut();
        let prior_ctx = guard
            .as_ref()
            .map(|r| (r.from_peer.clone(), r.to_peer.clone()));
        let (from_peer, to_peer) =
            resolve_renderer_context(prior_ctx, self.stream_context.borrow().as_ref());
        *guard = Some(CanvasRenderer {
            canvas,
            context,
            last_width: 0,
            last_height: 0,
            from_peer,
            to_peer,
        });
        Ok(())
    }

    /// Provide original peer IDs to the underlying decoder so worker can tag diagnostics.
    /// Also stores the peer context in the canvas renderer so resolution changes can
    /// be broadcast with the correct peer_id.
    pub fn set_stream_context(&self, from_peer: String, to_peer: String) {
        // Mirror the peer-id pair on `self` so `decode()` can tag the
        // source-resolution diag event regardless of whether the canvas
        // renderer is set yet.
        *self.stream_context.borrow_mut() = Some((from_peer.clone(), to_peer.clone()));

        // Store peer context in the canvas renderer for resolution broadcasts.
        if let Some(renderer) = self.canvas_renderer.borrow_mut().as_mut() {
            renderer.from_peer = Some(from_peer.clone());
            renderer.to_peer = Some(to_peer.clone());
            // If the canvas already has dimensions (frames arrived before
            // set_stream_context was called), broadcast the resolution now.
            if renderer.last_width > 0 && renderer.last_height > 0 {
                let evt = DiagEvent {
                    subsystem: "video_resolution",
                    stream_id: None,
                    ts_ms: now_ms(),
                    metrics: vec![
                        metric!("resolution_width", renderer.last_width as u64),
                        metric!("resolution_height", renderer.last_height as u64),
                        metric!("from_peer", from_peer.clone()),
                        metric!("to_peer", to_peer.clone()),
                        metric!("media_type", self.media_type.to_string()),
                    ],
                };
                let _ = global_sender().try_broadcast(evt);
            }
        }
        self.decoder.set_stream_context(from_peer, to_peer);
    }

    /// Render video frame using cached canvas and context. Only resizes when dimensions change.
    fn render_to_canvas_cached(
        canvas_renderer: &Rc<RefCell<Option<CanvasRenderer>>>,
        video_frame: web_sys::VideoFrame,
        media_type: &'static str,
    ) {
        let mut renderer_guard = canvas_renderer.borrow_mut();

        if let Some(renderer) = renderer_guard.as_mut() {
            let width = video_frame.display_width();
            let height = video_frame.display_height();

            // Only resize canvas if dimensions changed (expensive operation)
            if renderer.last_width != width || renderer.last_height != height {
                renderer.canvas.set_width(width);
                renderer.canvas.set_height(height);
                renderer.last_width = width;
                renderer.last_height = height;
                log::debug!("Resized canvas to {width}x{height}");

                // Broadcast resolution change so the UI can display it in tooltips.
                if let Some(to_peer) = &renderer.to_peer {
                    let evt = DiagEvent {
                        subsystem: "video_resolution",
                        stream_id: None,
                        ts_ms: now_ms(),
                        metrics: vec![
                            metric!("resolution_width", width as u64),
                            metric!("resolution_height", height as u64),
                            metric!("from_peer", renderer.from_peer.clone().unwrap_or_default()),
                            metric!("to_peer", to_peer.clone()),
                            metric!("media_type", media_type.to_string()),
                        ],
                    };
                    let _ = global_sender().try_broadcast(evt);
                }
            }

            // Clear and draw frame
            renderer
                .context
                .clear_rect(0.0, 0.0, width as f64, height as f64);
            if let Err(e) = renderer
                .context
                .draw_image_with_video_frame(&video_frame, 0.0, 0.0)
            {
                log::error!("Error drawing video frame: {e:?}");
            }
        } else {
            log::debug!("Canvas not yet set, skipping frame render");
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

    /// No-op decoder for unit tests — avoids requiring WebCodecs / worker link tags.
    #[cfg(test)]
    pub(crate) fn noop() -> Self {
        struct NoopDecoder;
        impl VideoFrameDecoder for NoopDecoder {
            fn push_frame(&self, _: FrameBuffer) {}
            fn is_waiting_for_keyframe(&self) -> bool {
                true
            }
            fn flush(&self) {}
        }
        Self {
            decoder: Box::new(NoopDecoder),
            canvas_renderer: Rc::new(RefCell::new(None)),
            media_type: MEDIA_TYPE_CAMERA,
            last_source_dims: RefCell::new((0, 0)),
            stream_context: RefCell::new(None),
        }
    }
}

impl PeerDecode for VideoPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        if let Some(video_metadata) = packet.video_metadata.as_ref() {
            // Surface publisher-side source dimensions (from
            // `MediaStreamTrack.getSettings()` on the encoder side) so the
            // UI can show Source vs Received and detect in-transit
            // downscaling. Dedupe by tracking the last-seen pair — without
            // this we'd flood the diag bus with one event per decoded frame.
            // Proto3 default-zero acts as "unknown": older publishers that
            // don't stamp the fields are skipped here.
            let src_w = video_metadata.source_width;
            let src_h = video_metadata.source_height;
            if src_w != 0 && src_h != 0 {
                let mut last = self.last_source_dims.borrow_mut();
                if *last != (src_w, src_h) {
                    *last = (src_w, src_h);
                    drop(last);
                    if let Some((from_peer, to_peer)) = self.stream_context.borrow().clone() {
                        let evt = DiagEvent {
                            subsystem: "video_source_resolution",
                            stream_id: None,
                            ts_ms: now_ms(),
                            metrics: vec![
                                metric!("source_width", src_w as u64),
                                metric!("source_height", src_h as u64),
                                metric!("from_peer", from_peer),
                                metric!("to_peer", to_peer),
                                metric!("media_type", self.media_type.to_string()),
                            ],
                        };
                        let _ = global_sender().try_broadcast(evt);
                    }
                }
            }

            // Convert protobuf VideoCodec to internal FrameCodec
            let frame_codec = match video_metadata.codec.enum_value() {
                Ok(VideoCodec::VP8) => FrameCodec::Vp8,
                Ok(VideoCodec::VP9_PROFILE0_LEVEL10_8BIT) => FrameCodec::Vp9Profile0Level10Bit8,
                Ok(VideoCodec::VIDEO_CODEC_UNSPECIFIED) | Err(_) => {
                    // Skip decoding for unknown codec (e.g., older clients)
                    log::warn!("Skipping video frame with unknown codec");
                    return Ok(DecodeStatus {
                        _rendered: false,
                        first_frame: false,
                    });
                }
            };

            let video_frame = CodecVideoFrame {
                sequence_number: video_metadata.sequence,
                timestamp: packet.timestamp,
                frame_type: self.get_frame_type(packet),
                codec: frame_codec,
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
                    // Phase 1: This error will be caught and counted as a frame drop in peer_decode_manager
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Camera path: canvas was attached first, so the renderer already carries
    /// a valid `(from_peer, to_peer)`. Even if `stream_context` is populated,
    /// we must keep the prior pair — overwriting it would erase a peer-id swap
    /// that landed via `set_stream_context` after the renderer was constructed.
    #[test]
    fn resolve_renderer_context_keeps_prior_pair_when_present() {
        let prior = Some((Some("alice".to_string()), Some("session-1".to_string())));
        let stream_ctx = ("bob".to_string(), "session-2".to_string());
        let (fp, tp) = resolve_renderer_context(prior, Some(&stream_ctx));
        assert_eq!(fp.as_deref(), Some("alice"));
        assert_eq!(tp.as_deref(), Some("session-1"));
    }

    /// Screen-share path: the first packet arrives before the dioxus
    /// `ScreenCanvas` tile mounts, so `set_stream_context` populates the
    /// decoder-level `stream_context` while the renderer is still absent. When
    /// the tile finally calls `set_canvas`, we have to seed the new renderer
    /// from `stream_context` — otherwise `render_to_canvas_cached`'s
    /// `video_resolution` broadcast stays gated on `to_peer.is_some()` and
    /// never fires. This is the #883 regression.
    #[test]
    fn resolve_renderer_context_seeds_from_stream_ctx_when_renderer_absent() {
        let stream_ctx = ("alice".to_string(), "session-1".to_string());
        let (fp, tp) = resolve_renderer_context(None, Some(&stream_ctx));
        assert_eq!(fp.as_deref(), Some("alice"));
        assert_eq!(tp.as_deref(), Some("session-1"));
    }

    /// Renderer existed but was created before `set_stream_context` ran (canvas
    /// passed at construction time, peer-id pair plumbed in later). Both
    /// fields are `None`, so we must fall back to `stream_context`.
    #[test]
    fn resolve_renderer_context_seeds_from_stream_ctx_when_prior_pair_empty() {
        let prior = Some((None, None));
        let stream_ctx = ("alice".to_string(), "session-1".to_string());
        let (fp, tp) = resolve_renderer_context(prior, Some(&stream_ctx));
        assert_eq!(fp.as_deref(), Some("alice"));
        assert_eq!(tp.as_deref(), Some("session-1"));
    }

    /// Neither source has data — return `(None, None)` so the renderer
    /// remains in an un-tagged state until `set_stream_context` runs.
    #[test]
    fn resolve_renderer_context_returns_none_when_both_empty() {
        let (fp, tp) = resolve_renderer_context(None, None);
        assert!(fp.is_none());
        assert!(tp.is_none());
    }

    /// Partial prior context (only `from_peer` or only `to_peer` known) is
    /// still preserved — never overwritten by `stream_context`. This avoids
    /// accidentally clobbering a half-set state during a canvas swap, which
    /// can happen if `set_canvas` is called twice in a row by Dioxus
    /// `use_effect` re-runs.
    #[test]
    fn resolve_renderer_context_preserves_partial_prior() {
        let prior = Some((Some("alice".to_string()), None));
        let stream_ctx = ("bob".to_string(), "session-2".to_string());
        let (fp, tp) = resolve_renderer_context(prior, Some(&stream_ctx));
        assert_eq!(fp.as_deref(), Some("alice"));
        assert!(tp.is_none());
    }
}
