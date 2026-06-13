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

#![no_main]
/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Web worker decoder that handles both frame data and control messages using a JitterBuffer.

use std::cell::{Cell, RefCell};
use videocall_codecs::decoder::{Decodable, DecodedFrame, VideoCodec};
use videocall_codecs::frame::{FrameBuffer, FrameCodec, VideoFrame};
use videocall_codecs::jitter_buffer::{paint_lag_ms, JitterBuffer};
use videocall_codecs::messages::{RequestKeyframeMessage, VideoStatsMessage, WorkerMessage};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    console, CodecState, DedicatedWorkerGlobalScope, EncodedVideoChunk, EncodedVideoChunkInit,
    EncodedVideoChunkType, VideoDecoder, VideoDecoderConfig, VideoDecoderInit,
    VideoFrame as WebVideoFrame,
};

/// WebDecoder implementation that wraps WebCodecs VideoDecoder
struct WebDecoder {
    decoder: RefCell<Option<VideoDecoder>>,
    current_codec: RefCell<Option<FrameCodec>>,
    self_scope: DedicatedWorkerGlobalScope,
    /// Set to `true` immediately after a fresh `VideoDecoder` is created and configured.
    /// A freshly-configured WebCodecs `VideoDecoder` requires its first chunk to be a
    /// keyframe — feeding a delta will throw `DataError: A key frame is required after
    /// configure() or flush()`. This flag lets `decode()` drop stray delta frames that
    /// race in after a `reset_pipeline()` (which destroys the decoder synchronously but
    /// schedules the jitter-buffer reset asynchronously via `setTimeout(0)`), preventing
    /// a reconfigure loop until the next real keyframe arrives. Cleared only after a
    /// keyframe has been successfully handed off to the decoder.
    just_reinitialized: Cell<bool>,
}

// Safety: These are safe because we're in a single-threaded web worker context
unsafe impl Send for WebDecoder {}
unsafe impl Sync for WebDecoder {}

impl WebDecoder {
    fn new(self_scope: DedicatedWorkerGlobalScope) -> Self {
        Self {
            decoder: RefCell::new(None),
            current_codec: RefCell::new(None),
            self_scope,
            just_reinitialized: Cell::new(false),
        }
    }

    fn initialize_decoder(&self, codec: FrameCodec) -> Result<(), String> {
        // Skip unknown codecs - cannot decode
        let codec_str = match codec.as_webcodecs_str() {
            Some(s) => s,
            None => return Err("Unknown codec - cannot decode".to_string()),
        };

        let mut decoder_ref = self.decoder.borrow_mut();
        let mut codec_ref = self.current_codec.borrow_mut();

        // Check if we already have a decoder with the same codec AND it is still usable.
        // A decoder whose state is Closed (e.g. after an async WebCodecs error) must be
        // torn down and recreated — returning early here would leave the pipeline permanently
        // stuck.
        if let Some(existing) = decoder_ref.as_ref() {
            if *codec_ref == Some(codec) && existing.state() == CodecState::Configured {
                return Ok(());
            }
        }

        // Tear down the old decoder if it exists — either the codec changed or the decoder
        // entered a non-Configured state (Closed after an error, Unconfigured, etc.).
        if let Some(decoder) = decoder_ref.take() {
            let old_state = decoder.state();
            console::log_1(
                &format!(
                    "[WORKER] Replacing decoder (state={old_state:?}, old_codec={codec_ref:?}, new_codec={codec:?})"
                )
                .into(),
            );
            if old_state != CodecState::Closed {
                let _ = decoder.close();
            }
        }

        let self_scope = self.self_scope.clone();
        let on_output = {
            let global_scope = self_scope.clone();
            Closure::wrap(Box::new(move |video_frame: JsValue| {
                let video_frame = video_frame.dyn_into::<WebVideoFrame>().unwrap();
                // Post the VideoFrame back to the main thread
                if let Err(e) = global_scope.post_message(&video_frame) {
                    console::error_1(
                        &format!("[WORKER] Error posting decoded frame: {e:?}").into(),
                    );
                }
                // Stage-3 paint lag (issue #1252): count every decoded frame handed off to the
                // worker->main postMessage queue. The main thread ACKs how many it has drained
                // (WorkerMessage::PaintProgress); the 1Hz tick computes emitted - painted.
                FRAMES_EMITTED.with(|c| c.set(c.get().wrapping_add(1)));
                video_frame.close();
            }) as Box<dyn FnMut(_)>)
        };

        let on_error = Closure::wrap(Box::new(move |e: JsValue| {
            console::error_1(&"[WORKER] WebCodecs decoder error:".into());
            console::error_1(&e);
        }) as Box<dyn FnMut(_)>);

        let init = VideoDecoderInit::new(
            on_error.as_ref().unchecked_ref(),
            on_output.as_ref().unchecked_ref(),
        );

        let decoder =
            VideoDecoder::new(&init).map_err(|e| format!("Failed to create decoder: {e:?}"))?;

        // Configure with the codec from the incoming frame
        console::log_1(&format!("[WORKER] Configuring decoder with codec: {codec_str}").into());
        let config = VideoDecoderConfig::new(codec_str);
        decoder
            .configure(&config)
            .map_err(|e| format!("Failed to configure decoder: {e:?}"))?;

        on_output.forget();
        on_error.forget();

        *decoder_ref = Some(decoder);
        *codec_ref = Some(codec);
        console::log_1(
            &format!("[WORKER] WebCodecs decoder initialized with codec: {codec:?}").into(),
        );

        // Mark the decoder as freshly (re)initialized. `decode()` will refuse to feed
        // delta frames in this state and will clear the flag once the first keyframe
        // has been successfully handed off to the decoder. This is the race guard for
        // the `reset_pipeline()` path where the jitter buffer reset is deferred via
        // `setTimeout(0)`.
        self.just_reinitialized.set(true);

        Ok(())
    }

    /// Tear down the current decoder instance entirely, releasing all resources so that the next
    /// decode call will create a fresh `VideoDecoder`. This is required when the decoder enters an
    /// `InvalidStateError` that cannot be recovered from with `reset()`.
    fn destroy_decoder(&self) {
        // Acquire a mutable reference so we can replace the Option with `None`.
        let mut decoder_ref = self.decoder.borrow_mut();

        if let Some(decoder) = decoder_ref.take() {
            if decoder.state() != CodecState::Closed {
                if let Err(e) = decoder.close() {
                    console::error_1(
                        &format!("[WORKER] Failed to close decoder cleanly: {e:?}").into(),
                    );
                } else {
                    console::log_1(&"[WORKER] Video decoder closed".into());
                }
            }

            console::log_1(&"[WORKER] Video decoder destroyed".into());
        }
    }

    /// High-level helper that tears down the decoder and schedules a jitter-buffer reset on the
    /// next event-loop tick. This avoids nested borrows and ensures we always start clean, waiting
    /// for a keyframe.
    fn reset_pipeline(&self) {
        // First, drop the current decoder instance (if any)
        self.destroy_decoder();

        // Schedule jitter-buffer reset asynchronously to avoid borrow conflicts with whatever
        // call stack triggered this reset.
        let self_scope = self.self_scope.clone();

        let cb = Closure::once_into_js(move || {
            reset_jitter_buffer();
        });

        // Ignore errors from setTimeout – if scheduling fails we'll try again on next frame.
        let _ = self_scope
            .set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 0);
        // `cb` moved into JS runtime, no need to forget.
    }
}

impl Decodable for WebDecoder {
    type Frame = DecodedFrame;

    fn new(_codec: VideoCodec, _on_decoded_frame: Box<dyn Fn(Self::Frame) + Send + Sync>) -> Self {
        // This is not used in the worker context, decoder is created manually
        panic!("Use WebDecoder::new(self_scope) in worker context");
    }

    fn decode(&self, frame: FrameBuffer) {
        let frame_codec = frame.frame.codec;

        // Initialize or reconfigure decoder based on frame's codec
        if let Err(e) = self.initialize_decoder(frame_codec) {
            console::error_1(&format!("[WORKER] Failed to initialize decoder: {e:?}").into());
            return;
        }

        // Race guard: if `initialize_decoder()` just created a brand-new `VideoDecoder`
        // (e.g. after `reset_pipeline()` tore down the previous one, or at startup),
        // the WebCodecs spec requires the first chunk we feed it to be a keyframe. A
        // delta arriving here would trigger `DataError: A key frame is required after
        // configure() or flush()` and loop us into another reset. Drop deltas silently
        // until we successfully hand off a keyframe below. Note that `initialize_decoder()`
        // above may have just set this flag on this very call path — that is intentional;
        // the current frame is the first candidate to satisfy the keyframe requirement.
        // The flag is NOT cleared here — only after `decoder.decode(&chunk)` returns `Ok`,
        // so a failed chunk construction or a failed enqueue doesn't leave the decoder
        // in a "needs keyframe" state with the guard disarmed.
        if self.just_reinitialized.get()
            && matches!(
                frame.frame.frame_type,
                videocall_codecs::frame::FrameType::DeltaFrame
            )
        {
            log::debug!(
                "[WORKER] Dropping delta frame {} (codec {:?}) after decoder reinit; waiting for keyframe",
                frame.sequence_number(),
                frame.frame.codec
            );
            return;
        }

        let decoder_ref = self.decoder.borrow();
        if let Some(decoder) = decoder_ref.as_ref() {
            // Only decode when the VideoDecoder is in the Configured state.
            // After a successful initialize_decoder() call this should always be true,
            // but guard defensively against unexpected browser-side state transitions.
            if decoder.state() != CodecState::Configured {
                console::warn_1(
                    &format!(
                        "[WORKER] Decoder in unexpected state {:?} after initialization, skipping frame",
                        decoder.state()
                    )
                    .into(),
                );
                return;
            }

            // Second buffer stage observability (issue #1020). Frames handed to
            // `VideoDecoder.decode()` sit in WebCodecs' own internal queue, which is unpaced: even
            // with the jitter-buffer freshness deadline in place, a burst of frames could pile up
            // here and be painted back-to-back, partially defeating the buffer-side fix.
            //
            // Release-side backpressure (issue #1024) now caps this: the jitter buffer reads the
            // live depth via `Decodable::decode_queue_depth()` (implemented below) *before*
            // releasing a frame and holds new frames while the queue is at/above its high-water
            // mark, so under healthy pacing this depth stays around that mark and the debug log
            // below should rarely fire. If it still fires, the decoder genuinely can't keep up and
            // the freshness deadline will skip to live. This log is kept purely for observability.
            let decode_queue_size = decoder.decode_queue_size();
            if decode_queue_size > WEBCODECS_QUEUE_WARN_DEPTH {
                log::debug!(
                    "[WORKER] WebCodecs decode queue backing up: {decode_queue_size} chunks pending (seq {})",
                    frame.sequence_number()
                );
            }

            let chunk_type = match frame.frame.frame_type {
                videocall_codecs::frame::FrameType::KeyFrame => EncodedVideoChunkType::Key,
                videocall_codecs::frame::FrameType::DeltaFrame => EncodedVideoChunkType::Delta,
            };

            let data = js_sys::Uint8Array::from(frame.frame.data.as_slice());
            let init = EncodedVideoChunkInit::new(&data.into(), frame.frame.timestamp, chunk_type);

            match EncodedVideoChunk::new(&init) {
                Ok(chunk) => {
                    match decoder.decode(&chunk) {
                        Ok(()) => {
                            // Successful hand-off. If this was the keyframe we were waiting
                            // for after a fresh (re)init, disarm the race guard now that the
                            // WebCodecs decoder has accepted its required first keyframe.
                            if matches!(
                                frame.frame.frame_type,
                                videocall_codecs::frame::FrameType::KeyFrame
                            ) && self.just_reinitialized.get()
                            {
                                self.just_reinitialized.set(false);
                            }
                        }
                        Err(e) => {
                            console::error_1(&format!("[WORKER] Decoder error: {e:?}").into());

                            // Release the immutable borrow so we can safely mutate within
                            // `reset_pipeline()`.
                            drop(decoder_ref);

                            // Completely reset decoder + jitter buffer in a single abstraction.
                            self.reset_pipeline();
                        }
                    }
                }
                Err(e) => {
                    console::error_1(&format!("[WORKER] Failed to create chunk: {e:?}").into());
                    // The decoder was left untouched, but if we were waiting for a keyframe
                    // and this was it, we must NOT leave the guard disarmed — reset the whole
                    // pipeline so the next frame goes through a fresh init + keyframe sequence.
                    if self.just_reinitialized.get() {
                        drop(decoder_ref);
                        self.reset_pipeline();
                    }
                }
            }
        }
    }

    /// Live depth of the WebCodecs `VideoDecoder` internal queue, used by the jitter buffer for
    /// release-side backpressure (issue #1024). Returns `0` when no decoder is configured yet, so
    /// the buffer is free to release the first frame(s) and let the decoder come up.
    fn decode_queue_depth(&self) -> u32 {
        self.decoder
            .borrow()
            .as_ref()
            .map(|d| d.decode_queue_size())
            .unwrap_or(0)
    }

    /// Wedged-decoder recovery escalation (issue #1324). Tears down the current `VideoDecoder` and
    /// schedules the jitter-buffer reset on the next event-loop tick. Called by the jitter buffer's
    /// escape hatch when backpressure has held release past `MAX_BACKPRESSURE_HOLD_MS` and a
    /// force-release did not break the wedge. `reset_pipeline()` defers the buffer reset via
    /// `setTimeout(0)`, so calling it from inside the release loop is safe — the deferred
    /// `reset_jitter_buffer()` runs after the current call stack unwinds.
    fn reset(&self) {
        console::warn_1(
            &"[WORKER] Backpressure escape hatch: resetting decoder pipeline (wedged decoder, issue #1324)".into(),
        );
        self.reset_pipeline();
    }
}

// Thread-local storage for the jitter buffer and related state
thread_local! {
    static JITTER_BUFFER: RefCell<Option<JitterBuffer<DecodedFrame>>> = const { RefCell::new(None) };
    static INTERVAL_ID: RefCell<Option<i32>> = const { RefCell::new(None) };
    static CONTEXT_FROM: RefCell<Option<String>> = const { RefCell::new(None) };
    static CONTEXT_TO: RefCell<Option<String>> = const { RefCell::new(None) };
    static LAST_DIAGNOSTIC_EMIT_MS: RefCell<f64> = const { RefCell::new(0.0) };
    /// Cumulative count of decoded frames this worker has `post_message`'d to the main thread
    /// (issue #1252, stage-3 paint lag). Incremented in the WebCodecs `on_output` closure.
    static FRAMES_EMITTED: Cell<u64> = const { Cell::new(0) };
    /// Latest cumulative count of frames the main thread reports it has drained from the
    /// worker->main `postMessage` queue, ACK'd back via `WorkerMessage::PaintProgress`
    /// (issue #1252, stage-3 paint lag). The difference `emitted - painted` is the
    /// decoded-but-unpainted backlog `decode_queue_size()` cannot see.
    static FRAMES_PAINTED: Cell<u64> = const { Cell::new(0) };
}

const JITTER_BUFFER_CHECK_INTERVAL_MS: i32 = 10; // Check every 10ms for frames ready to decode
/// Depth of the WebCodecs `VideoDecoder` internal queue above which we log a backlog (debug) line.
/// Derived from the release-side gate's high-water mark (the single source of truth) so the two
/// can't silently desync. The gate (`jitter_buffer.rs`) HOLDS release at
/// `>= DECODE_QUEUE_HIGH_WATER_MARK`, so under healthy pacing the depth sits right at the mark;
/// this observability log intentionally uses a strict `>` (below) so it fires only when the depth
/// climbs ABOVE the mark — i.e. the decoder accepted more than the gate would normally let
/// accumulate, the genuine "can't keep up" signal (issue #1020, second buffer stage). At ~30fps a
/// healthy queue stays at 0-1. The `>` vs gate `>=` operator difference is intentional, not an
/// off-by-one.
const WEBCODECS_QUEUE_WARN_DEPTH: u32 =
    videocall_codecs::jitter_buffer::DECODE_QUEUE_HIGH_WATER_MARK;
const DIAGNOSTIC_EMIT_INTERVAL_MS: f64 = 1000.0; // Emit diagnostics at 1 Hz (once per second)

#[wasm_bindgen(start)]
pub fn main() {
    // Set up panic hook to get Rust panics in the console
    console_error_panic_hook::set_once();
    // Initialize Rust log to console logging
    log::set_max_level(log::LevelFilter::Debug);
    log::info!("Starting worker decoder with jitter buffer and message handling");

    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let on_message = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
        match serde_wasm_bindgen::from_value::<WorkerMessage>(event.data()) {
            Ok(message) => handle_worker_message(message),
            Err(e) => {
                console::error_1(&format!("[WORKER] Failed to deserialize message: {e:?}").into());
            }
        }
    }) as Box<dyn FnMut(_)>);

    self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    // Start the jitter buffer check interval
    start_jitter_buffer_interval();
}

fn handle_worker_message(message: WorkerMessage) {
    match message {
        WorkerMessage::DecodeFrame(frame) => {
            insert_frame_to_jitter_buffer(frame);
        }
        WorkerMessage::Flush => {
            console::log_1(&"[WORKER] Flushing jitter buffer and decoder".into());
            flush_jitter_buffer();
        }
        WorkerMessage::Reset => {
            console::log_1(&"[WORKER] Resetting jitter buffer and decoder state".into());
            reset_jitter_buffer();
        }
        WorkerMessage::SetContext { from_peer, to_peer } => {
            CONTEXT_FROM.with(|f| *f.borrow_mut() = Some(from_peer));
            CONTEXT_TO.with(|t| *t.borrow_mut() = Some(to_peer));
            console::log_1(&"[WORKER] Set diagnostics context (from_peer,to_peer)".into());
        }
        WorkerMessage::PaintProgress { painted } => {
            // Stage-3 paint lag (issue #1252): cumulative frames the main thread has drained from
            // the worker->main postMessage queue. Store the latest; the 1Hz tick computes
            // emitted - painted. This ACK is itself delayed by the same FIFO backlog, which only
            // makes the computed lag read slightly LARGER (conservative) — fine for a gross signal.
            FRAMES_PAINTED.with(|c| c.set(painted));
        }
    }
}

fn insert_frame_to_jitter_buffer(frame: FrameBuffer) {
    JITTER_BUFFER.with(|jb_cell| {
        let mut jb_opt = jb_cell.borrow_mut();

        if jb_opt.is_none() {
            match initialize_jitter_buffer() {
                Ok(jb) => *jb_opt = Some(jb),
                Err(e) => {
                    console::error_1(
                        &format!("[WORKER] Failed to initialize jitter buffer: {e:?}").into(),
                    );
                    return;
                }
            }
        }

        if let Some(jb) = jb_opt.as_mut() {
            // Convert FrameBuffer to VideoFrame, preserving the codec
            let video_frame = VideoFrame {
                sequence_number: frame.sequence_number(),
                frame_type: frame.frame.frame_type,
                codec: frame.frame.codec,
                data: frame.frame.data.clone(),
                timestamp: frame.frame.timestamp,
            };

            // Get current time in milliseconds
            let current_time_ms = js_sys::Date::now() as u128;
            jb.insert_frame(video_frame, current_time_ms);
        }
    });
}

fn start_jitter_buffer_interval() {
    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let interval_callback = Closure::wrap(Box::new(move || {
        check_jitter_buffer_for_ready_frames();
    }) as Box<dyn FnMut()>);

    let interval_id = self_scope
        .set_interval_with_callback_and_timeout_and_arguments_0(
            interval_callback.as_ref().unchecked_ref(),
            JITTER_BUFFER_CHECK_INTERVAL_MS,
        )
        .expect("Failed to set interval");

    interval_callback.forget();

    INTERVAL_ID.with(|id_cell| {
        *id_cell.borrow_mut() = Some(interval_id);
    });

    console::log_1(
        &format!("[WORKER] Started jitter buffer check interval with {JITTER_BUFFER_CHECK_INTERVAL_MS}ms interval")
        .into(),
    );
}

fn check_jitter_buffer_for_ready_frames() {
    JITTER_BUFFER.with(|jb_cell| {
        let mut jb_opt = jb_cell.borrow_mut();
        if let Some(jb) = jb_opt.as_mut() {
            let current_time_ms = js_sys::Date::now() as u128;
            jb.find_and_move_continuous_frames(current_time_ms);

            // Publish buffered frames metric periodically under subsystem "video" with stream_id unset.
            // Rate limited to 1 Hz to avoid flooding diagnostics.
            // The client layer will attach original ids later in the pipeline.
            let buffered = jb.buffered_frames_len() as u64;
            #[cfg(feature = "wasm")]
            {
                use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
                // Only emit when we have context so the server can attribute correctly
                CONTEXT_FROM.with(|from_cell| {
                    CONTEXT_TO.with(|to_cell| {
                        LAST_DIAGNOSTIC_EMIT_MS.with(|last_emit_cell| {
                            if let (Some(from_peer), Some(to_peer)) =
                                (from_cell.borrow().clone(), to_cell.borrow().clone())
                            {
                                let now = js_sys::Date::now();
                                let last_emit = *last_emit_cell.borrow();

                                // Only emit if at least DIAGNOSTIC_EMIT_INTERVAL_MS has passed
                                if now - last_emit >= DIAGNOSTIC_EMIT_INTERVAL_MS {
                                    *last_emit_cell.borrow_mut() = now;

                                    // Buffered video playout latency (issue #1252): how far behind
                                    // live this peer's video is. Compute only on the 1 Hz emit path.
                                    // Total spans both receive stages (jitter-buffer backlog +
                                    // decoder queue); stage-1 is emitted separately for attribution.
                                    let (playout_latency_ms, playout_stage1_span_ms) =
                                        jb.playout_latency_parts_ms(current_time_ms);
                                    // Stage-3 paint lag (issue #1252): decoded-but-unpainted frames
                                    // still sitting in the worker->main postMessage queue +
                                    // main-thread paint task queue — a region decode_queue_size()
                                    // (stage 2) cannot observe. Compute on the same 1 Hz emit path
                                    // so the metric reflects the same sampling cadence as the rest
                                    // of the video diagnostic packet.
                                    let playout_paint_lag_ms = paint_lag_ms(
                                        FRAMES_EMITTED.with(|c| c.get()),
                                        FRAMES_PAINTED.with(|c| c.get()),
                                        jb.source_frame_interval_ms(),
                                    );

                                    let evt = DiagEvent {
                                        subsystem: "video",
                                        stream_id: None,
                                        ts_ms: now_ms(),
                                        metrics: vec![
                                            metric!("from_peer", from_peer.clone()),
                                            metric!("to_peer", to_peer.clone()),
                                            metric!("frames_buffered", buffered),
                                            metric!("playout_latency_ms", playout_latency_ms),
                                            metric!(
                                                "playout_stage1_span_ms",
                                                playout_stage1_span_ms
                                            ),
                                            metric!("playout_paint_lag_ms", playout_paint_lag_ms),
                                        ],
                                    };
                                    let _ = global_sender().try_broadcast(evt);

                                    // Also post a lightweight message to the main thread so it can forward to its bus
                                    if let Ok(scope) =
                                        js_sys::global().dyn_into::<DedicatedWorkerGlobalScope>()
                                    {
                                        let msg = VideoStatsMessage::new(
                                            from_peer,
                                            to_peer,
                                            buffered,
                                            playout_latency_ms,
                                            playout_stage1_span_ms,
                                            playout_paint_lag_ms,
                                        );
                                        if let Ok(val) = serde_wasm_bindgen::to_value(&msg) {
                                            let _ = scope.post_message(&val);
                                        }
                                    }
                                }
                            }
                        })
                    })
                });
            }
        }
    });
}

fn initialize_jitter_buffer() -> Result<JitterBuffer<DecodedFrame>, String> {
    let self_scope = js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .unwrap();

    let web_decoder = WebDecoder::new(self_scope);
    let boxed_decoder = Box::new(web_decoder);

    console::log_1(&"[WORKER] Initializing jitter buffer with WebCodecs decoder".into());
    // Issue #1025: inject the proactive keyframe-request hook. The jitter buffer fires this
    // (via `with_keyframe_request`) the instant the freshness deadline evicts a stale
    // keyframe-less backlog — at which point playout is frozen on the last-good frame with
    // no buffered keyframe to resume from. We post a `RequestKeyframeMessage` to the main
    // thread, which owns the transport (WebTransport OR WebSocket) and the PeerDecodeManager,
    // so it can send a real KEYFRAME_REQUEST for this decoder's peer/stream. The diagnostics
    // context (CONTEXT_FROM/CONTEXT_TO) is read at FIRE time (not captured at init) because
    // `SetContext` can arrive after the buffer is constructed; it is carried for log symmetry
    // only — the main-side callback is per-decoder and needs no identity from the wire.
    Ok(JitterBuffer::with_keyframe_request(
        boxed_decoder,
        Box::new(post_request_keyframe_to_main),
    ))
}

/// Worker-side keyframe-request hook (issue #1025): post a [`RequestKeyframeMessage`] to the
/// main thread. Invoked by the jitter buffer on a keyframe-less stale-backlog eviction. Reading
/// the diagnostics context here (rather than capturing it at init) keeps the message tagged
/// correctly even when `SetContext` lands after `initialize_jitter_buffer`.
fn post_request_keyframe_to_main() {
    let Ok(scope) = js_sys::global().dyn_into::<DedicatedWorkerGlobalScope>() else {
        console::warn_1(&"[WORKER] request_keyframe: no worker scope; dropping".into());
        return;
    };
    let from_peer = CONTEXT_FROM.with(|f| f.borrow().clone());
    let to_peer = CONTEXT_TO.with(|t| t.borrow().clone());
    let msg = RequestKeyframeMessage::new(from_peer, to_peer);
    match serde_wasm_bindgen::to_value(&msg) {
        Ok(val) => {
            let _ = scope.post_message(&val);
        }
        Err(e) => {
            console::warn_1(&format!("[WORKER] request_keyframe: serialize failed: {e:?}").into());
        }
    }
}

fn flush_jitter_buffer() {
    JITTER_BUFFER.with(|jb_cell| {
        let mut jb_opt = jb_cell.borrow_mut();
        if let Some(jb) = jb_opt.as_mut() {
            jb.flush();
            console::log_1(&"[WORKER] Jitter buffer flushed".into());
        } else {
            console::log_1(&"[WORKER] No jitter buffer to flush".into());
        }
    });
}

fn reset_jitter_buffer() {
    JITTER_BUFFER.with(|jb_cell| {
        *jb_cell.borrow_mut() = None;
    });
    // Stage-3 paint lag (issue #1252): the FRAMES_EMITTED / FRAMES_PAINTED counters are
    // deliberately NOT reset here. They are lifetime-cumulative per worker, and the main thread's
    // painted counter (in the wasm.rs onmessage closure) is likewise monotonic and survives an
    // in-place reset — that closure is not recreated when the worker tears down its decoder. They
    // stay coherent across a reset on their own: frames dropped by `destroy_decoder()` were never
    // emitted (`on_output` never fired, so they were never counted), while frames already
    // `post_message`'d before the reset still drain on the main thread and are still counted. So
    // `emitted - painted` remains the true in-flight backlog. Zeroing only the worker side here
    // would desync it from the still-monotonic main-thread ACK (the next ACK would set
    // FRAMES_PAINTED far above a freshly-zeroed FRAMES_EMITTED), flooring paint lag to a false 0
    // for minutes — hiding exactly the lag this metric exists to surface.
    console::log_1(&"[WORKER] Jitter buffer reset to initial state".into());
}
