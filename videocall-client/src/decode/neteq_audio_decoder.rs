use crate::adaptive_quality_constants::{AUDIO_RED_FORMAT, AUDIO_RED_SEQ_HISTORY_SIZE};
use crate::audio::shared_audio_context::SharedAudioContext;
use crate::audio_constants::{
    rms_to_intensity, AUDIO_LEVEL_DELTA_THRESHOLD, DEFAULT_VAD_THRESHOLD,
};
use crate::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::decode::{AudioPeerDecoderTrait, DecodeStatus};
use js_sys::Float32Array;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_wasm_bindgen;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::audio_rtp::{legacy_rtp_timestamp, OPUS_RTP_SAMPLES_PER_20MS_FRAME};
use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{AudioContext, AudioWorkletNode, MessageEvent, Worker};

const WORKLET_CODE: &str = include_str!("../scripts/pcmPlayerWorker.js");

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
enum WorkerMsg {
    Init {
        sample_rate: u32,
        channels: u8,
    },
    Insert {
        seq: u16,
        timestamp: u32,
        #[serde(with = "serde_bytes")]
        payload: Vec<u8>,
    },
    Flush,
    Clear,
    Close,
    Mute {
        muted: bool,
    },
    SetDiagnostics {
        enabled: bool,
    },
}

/// Messages received from worker (matches neteq_worker.rs)
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum WorkerResponse {
    WorkerReady {
        mute_state: bool,
    },
    Stats {
        #[serde(skip)]
        stats: JsValue, // Will be processed manually
    },
}

/// Audio decoder that sends packets to a NetEq worker and plays the returned PCM via WebAudio.
#[derive(Debug)]
pub struct NetEqAudioPeerDecoder {
    worker: Worker,
    _audio_context: AudioContext,
    decoded: bool,
    peer_id: String, // Track which peer this decoder belongs to
    _pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>, // AudioWorklet PCM player

    // Message queueing system
    pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
    worker_ready: Rc<RefCell<bool>>,

    // Voice activity detection state
    speaking: Rc<RefCell<bool>>,
    audio_level: Rc<RefCell<f32>>,

    /// Sequence/timestamp resolution for this peer: legacy-sender restart
    /// handling and the RED dedup ring.
    timeline: NetEqTimeline,
    /// Origin of the monotonic arrival clock used to size a legacy sender's
    /// restart gap (`performance.now()` in the browser via `web_time`).
    arrival_epoch: web_time::Instant,
}

impl NetEqAudioPeerDecoder {
    /// Send message through queue (immediate if worker ready, otherwise queued)
    fn send_worker_message(&self, msg: WorkerMsg) {
        let is_ready = *self.worker_ready.borrow();

        if is_ready {
            // Worker ready - send immediately
            self.send_message_immediate(msg);
        } else {
            // Worker not ready - queue the message
            log::debug!(
                "🔄 Queueing message for peer {} (worker not ready)",
                self.peer_id
            );
            self.pending_messages.borrow_mut().push_back(msg);
        }
    }

    /// Send message immediately to worker
    fn send_message_immediate(&self, msg: WorkerMsg) {
        if let Err(e) =
            serde_wasm_bindgen::to_value(&msg).map(|js_msg| self.worker.post_message(&js_msg))
        {
            log::error!("Failed to send worker message: {e:?}");
            web_sys::console::error_1(&format!("Failed to send worker message: {e:?}").into());
        }
    }

    /// Create a NetEq worker.
    fn create_neteq_worker() -> Result<Worker, JsValue> {
        let window = web_sys::window().expect("no window");
        let document = window.document().expect("no document");
        let worker_url = document
            .get_element_by_id("neteq-worker")
            .expect("neteq-worker link tag not found")
            .get_attribute("href")
            .expect("link tag has no href");
        Worker::new(&worker_url)
    }

    /// Send PCM data to Safari AudioWorklet (simple and efficient)
    fn send_pcm_to_safari_worklet(pcm_player: &AudioWorkletNode, pcm: &Float32Array) {
        // Create message object for the worklet
        let message = js_sys::Object::new();
        js_sys::Reflect::set(&message, &"command".into(), &"play".into()).unwrap();
        js_sys::Reflect::set(&message, &"pcm".into(), pcm).unwrap();

        // Send PCM data to the worklet - it handles all timing internally
        if let Err(e) = pcm_player.port().unwrap().post_message(&message) {
            web_sys::console::warn_1(
                &format!("Safari: Failed to send PCM to worklet: {e:?}").into(),
            );
        }
    }

    /// Create Safari-optimized AudioContext with PCM player worklet
    async fn create_safari_audio_context(
        speaker_device_id: Option<String>,
    ) -> Result<(AudioContext, AudioWorkletNode), JsValue> {
        // Use shared context and ensure worklet is registered before creating node
        let audio_context = SharedAudioContext::get_or_init(speaker_device_id.clone())?;
        SharedAudioContext::ensure_pcm_worklet_ready(WORKLET_CODE).await?;

        // Create per-peer nodes after registration completes
        let (pcm_player, _peer_gain) = SharedAudioContext::create_peer_playback_nodes("safari")?;

        Ok((audio_context, pcm_player))
    }

    /// Calculate RMS (Root Mean Square) of audio samples for voice activity detection.
    ///
    /// This is part of the **decoder-side (remote peer) VAD**.  We run a
    /// fast-path RMS check on every decoded PCM frame so the UI can show a
    /// speaking indicator for remote peers with sub-second latency, rather
    /// than waiting for the 1Hz heartbeat update that carries the remote
    /// user's own (encoder-side) `is_speaking` flag.
    fn calculate_rms(pcm: &Float32Array) -> f32 {
        let length = pcm.length() as usize;
        if length == 0 {
            return 0.0;
        }

        let mut sum_squares: f32 = 0.0;
        for i in 0..length {
            let sample = pcm.get_index(i as u32);
            sum_squares += sample * sample;
        }

        (sum_squares / length as f32).sqrt()
    }

    /// Handle PCM audio data from NetEq worker.
    ///
    /// Includes decoder-side VAD: computes RMS on the decoded PCM and emits
    /// a `peer_speaking` diagnostics event when the speaking state changes.
    /// This gives the UI a faster speaking indicator for remote peers than
    /// the 1Hz heartbeat, which only reflects the remote user's own
    /// encoder-side VAD result.
    #[allow(clippy::too_many_arguments)]
    fn handle_pcm_data(
        pcm: Float32Array,
        pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>,
        audio_context: &AudioContext,
        speaker_device_id: Option<String>,
        peer_id: String,
        speaking: Rc<RefCell<bool>>,
        audio_level: Rc<RefCell<f32>>,
        vad_threshold: f32,
    ) {
        // Calculate RMS for voice activity detection
        let rms = Self::calculate_rms(&pcm);
        let is_speaking = rms > vad_threshold;

        // Normalize RMS to a 0.0–1.0 intensity range using the shared
        // perceptual curve (sqrt for human hearing).
        let intensity = rms_to_intensity(rms, vad_threshold);

        // Emit a diagnostics event when the speaking boolean toggles OR
        // when the audio level changes by more than 0.02.  This keeps the
        // event rate reasonable while giving the UI smooth level updates.
        let prev_speaking = *speaking.borrow();
        let prev_level = *audio_level.borrow();
        let level_changed = (intensity - prev_level).abs() > AUDIO_LEVEL_DELTA_THRESHOLD;

        if is_speaking != prev_speaking || level_changed {
            *speaking.borrow_mut() = is_speaking;
            *audio_level.borrow_mut() = intensity;

            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "peer_speaking",
                stream_id: Some(format!("speaking->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("to_peer", peer_id.clone()),
                    metric!("speaking", if is_speaking { 1u64 } else { 0u64 }),
                    metric!("audio_level", intensity as f64),
                ],
            });
        }

        // Ensure AudioContext is running
        if let Err(e) = audio_context.resume() {
            web_sys::console::warn_1(
                &format!("[neteq-audio-decoder] AudioContext resume error: {e:?}").into(),
            );
        }

        let pcm_player_clone = pcm_player.clone();
        wasm_bindgen_futures::spawn_local(async move {
            Self::ensure_worklet_initialized(&pcm_player_clone, speaker_device_id).await;

            if let Some(ref worklet) = *pcm_player_clone.borrow() {
                Self::send_pcm_to_safari_worklet(worklet, &pcm);
            }
        });
    }

    /// Ensure AudioWorklet is initialized (lazy initialization)
    async fn ensure_worklet_initialized(
        pcm_player: &Rc<RefCell<Option<AudioWorkletNode>>>,
        speaker_device_id: Option<String>,
    ) {
        if pcm_player.borrow().is_some() {
            return;
        }

        log::info!("Initializing AudioWorklet for PCM playback");

        match Self::create_safari_audio_context(speaker_device_id).await {
            Ok((_, worklet)) => {
                *pcm_player.borrow_mut() = Some(worklet);
                log::info!("AudioWorklet initialized successfully");
            }
            Err(e) => {
                web_sys::console::error_2(&"Failed to initialize worklet:".into(), &e);
            }
        }
    }

    /// Handle statistics messages from NetEq worker
    fn handle_stats_message(data: &JsValue, peer_id: &str) {
        let obj = match data.dyn_ref::<js_sys::Object>() {
            Some(obj) => obj,
            None => return,
        };

        let cmd =
            js_sys::Reflect::get(obj, &JsValue::from_str("cmd")).unwrap_or(JsValue::UNDEFINED);

        if cmd.as_string().as_deref() != Some("stats") {
            return;
        }

        let stats_js = match js_sys::Reflect::get(obj, &JsValue::from_str("stats")) {
            Ok(stats) => stats,
            Err(_) => return,
        };

        let stats_json = match js_sys::JSON::stringify(&stats_js) {
            Ok(json) => json,
            Err(_) => return,
        };

        let json_str = match stats_json.as_string() {
            Some(s) => s,
            None => return,
        };

        Self::emit_stats_diagnostics(&json_str, peer_id);
        Self::emit_parsed_metrics(&json_str, peer_id);
    }

    /// Emit raw stats JSON for debugging
    fn emit_stats_diagnostics(json_str: &str, peer_id: &str) {
        // peer_id here is the target peer (whose audio we're decoding)
        // We need to get the current user's ID for the reporting peer
        // For now, we'll use a placeholder and enhance this later
        let current_user = "current_user"; // TODO: Get from VideoCallClient

        let _ = global_sender().try_broadcast(DiagEvent {
            subsystem: "neteq",
            stream_id: Some(format!("{current_user}->{peer_id}")), // reporting_peer->target_peer
            ts_ms: now_ms(),
            metrics: vec![
                metric!("stats_json", json_str.to_string()),
                metric!("reporting_peer", current_user),
                metric!("target_peer", peer_id.to_string()),
            ],
        });
    }

    /// Parse and emit specific metrics
    fn emit_parsed_metrics(json_str: &str, peer_id: &str) {
        let parsed: Value = match serde_json::from_str(json_str) {
            Ok(p) => p,
            Err(_) => return,
        };

        Self::emit_jitter_metrics(&parsed, peer_id);
        Self::emit_buffer_metrics(&parsed, peer_id);
    }

    /// Emit jitter buffer metrics
    fn emit_jitter_metrics(parsed: &Value, peer_id: &str) {
        let lifetime = match parsed.get("lifetime") {
            Some(l) => l,
            None => return,
        };

        if let Some(jitter) = lifetime
            .get("jitter_buffer_delay_ms")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(format!("current_user->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("jitter_buffer_delay_ms", jitter),
                    metric!("reporting_peer", "current_user"),
                    metric!("target_peer", peer_id.to_string()),
                ],
            });
        }

        if let Some(target) = lifetime
            .get("jitter_buffer_target_delay_ms")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(format!("current_user->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("jitter_buffer_target_delay_ms", target),
                    metric!("reporting_peer", "current_user"),
                    metric!("target_peer", peer_id.to_string()),
                ],
            });
        }
    }

    /// Emit buffer size metrics
    fn emit_buffer_metrics(parsed: &Value, peer_id: &str) {
        let network = match parsed.get("network") {
            Some(n) => n,
            None => return,
        };

        // Audio data buffered for playback
        if let Some(buf) = network
            .get("current_buffer_size_ms")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(format!("current_user->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("audio_buffer_ms", buf),
                    metric!("reporting_peer", "current_user"),
                    metric!("target_peer", peer_id.to_string()),
                ],
            });
        }

        // Encoded packets awaiting decode
        if let Some(packets) = network
            .get("packets_awaiting_decode")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(format!("current_user->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("packets_awaiting_decode", packets),
                    metric!("reporting_peer", "current_user"),
                    metric!("target_peer", peer_id.to_string()),
                ],
            });
        }
    }

    /// Create message handler for NetEq worker
    #[allow(clippy::too_many_arguments)]
    fn create_message_handler(
        pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>,
        audio_context: AudioContext,
        peer_id: String,
        speaker_device_id: Option<String>,
        worker_ready: Rc<RefCell<bool>>,
        pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
        worker: Worker,
        speaking: Rc<RefCell<bool>>,
        audio_level: Rc<RefCell<f32>>,
        vad_threshold: f32,
    ) -> Closure<dyn FnMut(MessageEvent)> {
        Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();

            if data.is_instance_of::<Float32Array>() {
                // High-performance PCM path with voice activity detection
                let pcm = Float32Array::from(data);
                Self::handle_pcm_data(
                    pcm,
                    pcm_player.clone(),
                    &audio_context,
                    speaker_device_id.clone(),
                    peer_id.clone(),
                    speaking.clone(),
                    audio_level.clone(),
                    vad_threshold,
                );
            } else if data.is_object() {
                // Try to parse as WorkerResponse first
                if let Ok(response) = serde_wasm_bindgen::from_value::<WorkerResponse>(data.clone())
                {
                    match response {
                        WorkerResponse::WorkerReady { mute_state } => {
                            // Handle worker ready - flush queue
                            log::info!(
                                "✅ Worker ready for peer {peer_id} (worker mute: {mute_state})"
                            );

                            *worker_ready.borrow_mut() = true;

                            // Flush queued messages in FIFO order
                            let mut queue = pending_messages.borrow_mut();
                            let queue_length = queue.len();

                            if queue_length > 0 {
                                log::info!(
                                    "📤 Flushing {queue_length} queued messages for peer {peer_id}"
                                );

                                // Send each queued message immediately
                                while let Some(msg) = queue.pop_front() {
                                    if let Err(e) = serde_wasm_bindgen::to_value(&msg)
                                        .map(|js_msg| worker.post_message(&js_msg))
                                    {
                                        log::error!("Failed to send queued message: {e:?}");
                                    } else {
                                        log::debug!("📤 Sent queued message: {msg:?}");
                                    }
                                }
                            }
                        }
                        WorkerResponse::Stats { .. } => {
                            // Handle stats message (fallback to old method for now)
                            Self::handle_stats_message(&data, &peer_id);
                        }
                    }
                } else {
                    // Fallback to old stats message handling
                    Self::handle_stats_message(&data, &peer_id);
                }
            }
        }) as Box<dyn FnMut(_)>)
    }

    /// Create audio decoder that uses NetEq worker for buffering and timing
    pub fn new_with_muted_state(
        speaker_device_id: Option<String>,
        peer_id: String,
        vad_threshold: Option<f32>,
    ) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
        Self::new_with_mute_state(speaker_device_id, peer_id, true, vad_threshold)
        // Default to muted
    }

    /// Create audio decoder with explicit initial mute state
    pub fn new_with_mute_state(
        speaker_device_id: Option<String>,
        peer_id: String,
        initial_muted: bool,
        vad_threshold: Option<f32>,
    ) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
        // Create worker
        let worker = Self::create_neteq_worker()?;

        // Use shared AudioContext and ensure worklet registered once
        let audio_context = SharedAudioContext::get_or_init(speaker_device_id.clone())?;
        SharedAudioContext::ensure_pcm_worklet(WORKLET_CODE);

        let pcm_player_ref = Rc::new(RefCell::new(None::<AudioWorkletNode>));

        let threshold = vad_threshold.unwrap_or(DEFAULT_VAD_THRESHOLD);

        // Create decoder with explicit mute state first
        let mut decoder = Self {
            worker: worker.clone(),
            _audio_context: audio_context.clone(),
            decoded: false,
            peer_id: peer_id.clone(),
            _pcm_player: pcm_player_ref.clone(),

            // Message queueing system
            pending_messages: Rc::new(RefCell::new(VecDeque::new())),
            worker_ready: Rc::new(RefCell::new(false)),

            // Voice activity detection state
            speaking: Rc::new(RefCell::new(false)),
            audio_level: Rc::new(RefCell::new(0.0)),

            timeline: NetEqTimeline::default(),
            arrival_epoch: web_time::Instant::now(),
        };

        // Set up worker message handling with decoder's queue references
        let on_message_closure = Self::create_message_handler(
            pcm_player_ref.clone(),
            audio_context.clone(),
            peer_id.clone(),
            speaker_device_id.clone(),
            decoder.worker_ready.clone(),
            decoder.pending_messages.clone(),
            worker.clone(),
            decoder.speaking.clone(),
            decoder.audio_level.clone(),
            threshold,
        );

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));
        // Intentionally leaked: the closure must live as long as the Worker,
        // which has no mechanism for preventing the closure from being GC'd
        // other than calling `.forget()`.  All captured `Rc`s (including
        // `speaking`, `pcm_player`, `worker_ready`, `pending_messages`) are
        // therefore permanently held until the Worker is terminated via Drop.
        on_message_closure.forget();

        // Initialize worker
        let init_msg = WorkerMsg::Init {
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS as u8,
        };

        let init_js = serde_wasm_bindgen::to_value(&init_msg)?;
        let worker_clone = worker.clone();
        let send_cb = Closure::wrap(Box::new(move || {
            if let Err(e) = worker_clone.post_message(&init_js) {
                web_sys::console::error_2(&"[neteq-audio-decoder] failed to post Init:".into(), &e);
            }
        }) as Box<dyn FnMut()>);

        web_sys::window()
            .expect("no window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                send_cb.as_ref().unchecked_ref(),
                10,
            )?;
        send_cb.forget();

        log::info!("NetEq audio decoder initialized for peer {peer_id} (muted: {initial_muted})");

        // Set the initial mute state explicitly
        decoder.set_muted(initial_muted);
        log::info!(
            "✅ NetEq decoder initialized for peer {} with muted: {}",
            decoder.peer_id,
            initial_muted
        );

        // Enable diagnostics in the NetEQ worker
        decoder.send_worker_message(WorkerMsg::SetDiagnostics { enabled: true });
        log::info!(
            "🔧 Enabled diagnostics for NetEq worker for peer {}",
            decoder.peer_id
        );

        Ok(Box::new(decoder))
    }

    /// Unpack a RED-encoded audio data buffer.
    ///
    /// Expected format:
    /// `[4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]`
    ///
    /// Returns `(primary_data, redundant_sequence, redundant_data)` or `None` if
    /// the buffer is too short or malformed.
    fn unpack_red_audio(data: &[u8]) -> Option<(Vec<u8>, u32, Vec<u8>)> {
        // Minimum: 4 (primary_len) + 0 (primary) + 4 (redundant_seq) + 0 (redundant)
        if data.len() < 8 {
            return None;
        }

        let primary_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;

        // Sanity check: an individual Opus audio frame should never exceed 10KB.
        // Reject clearly malformed or corrupt packets early.
        if primary_len > 10_000 {
            return None;
        }

        // Validate: primary_len + 4 (itself) + 4 (redundant_seq) must not exceed total
        let redundant_seq_offset = 4 + primary_len;
        if redundant_seq_offset + 4 > data.len() {
            return None;
        }

        let primary_data = data[4..4 + primary_len].to_vec();

        let redundant_seq = u32::from_le_bytes([
            data[redundant_seq_offset],
            data[redundant_seq_offset + 1],
            data[redundant_seq_offset + 2],
            data[redundant_seq_offset + 3],
        ]);

        let redundant_data = data[redundant_seq_offset + 4..].to_vec();

        Some((primary_data, redundant_seq, redundant_data))
    }

    /// Public wrapper around `unpack_red_audio` for cross-module tests.
    #[cfg(test)]
    pub fn unpack_red_audio_public(data: &[u8]) -> Option<(Vec<u8>, u32, Vec<u8>)> {
        Self::unpack_red_audio(data)
    }
}

/// One encoded frame to hand the NetEq worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetEqInsert {
    pub seq: u64,
    /// RTP timestamp in samples at 48 kHz (what NetEq orders and dedupes by).
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

/// What one received audio packet turns into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetEqInsertPlan {
    /// A RED-recovered earlier frame, inserted before `primary`.
    pub recovered: Option<NetEqInsert>,
    pub primary: NetEqInsert,
    /// The packet claimed RED but its payload did not parse; `primary`
    /// carries the whole blob.
    pub red_malformed: bool,
}

/// RTP timestamp of a packet's primary frame.
///
/// Uses `rtp_timestamp` when the sender set it. Senders that predate the
/// field emit one 20 ms frame per sequence number, so `sequence * 960` is
/// exact for them. `MediaPacket.timestamp` (wall-clock ms since 1970) is
/// never used: as a u32 it saturates to 4294967295 for every packet.
/// Stateless (first-epoch) form; the decoder uses [`NetEqTimeline`].
#[cfg(test)]
pub(crate) fn primary_rtp_timestamp(meta: &AudioMetadata) -> u32 {
    meta.rtp_timestamp
        .unwrap_or_else(|| legacy_rtp_timestamp(meta.sequence))
}

/// RTP timestamp of a RED redundant frame: the primary's minus the sequence
/// distance in 20 ms frames.
///
/// Returns `None` when the distance is not in `1..=AUDIO_RED_SEQ_HISTORY_SIZE`
/// (malformed, or too far back to dedupe), in which case the frame is not
/// recovered. Correct for both sender generations because a sender never
/// attaches redundancy across a timestamp gap.
#[cfg(test)]
pub(crate) fn redundant_rtp_timestamp(
    primary_timestamp: u32,
    primary_seq: u64,
    redundant_seq: u32,
) -> Option<u32> {
    red_distance(primary_seq, redundant_seq).map(|distance| {
        primary_timestamp.wrapping_sub(distance.wrapping_mul(OPUS_RTP_SAMPLES_PER_20MS_FRAME))
    })
}

/// Frames between a RED primary and its redundant frame, from the sender's
/// raw sequence numbers, if in `1..=AUDIO_RED_SEQ_HISTORY_SIZE`.
fn red_distance(primary_seq: u64, redundant_seq: u32) -> Option<u32> {
    // The RED header carries only the low 32 bits of the sequence number.
    let distance = (primary_seq as u32).wrapping_sub(redundant_seq);
    (distance != 0 && distance as usize <= AUDIO_RED_SEQ_HISTORY_SIZE).then_some(distance)
}

/// Stateless planning with the sender's raw sequence (no legacy restart
/// handling). Tests use it; the decoder goes through [`NetEqTimeline`].
#[cfg(test)]
pub(crate) fn plan_neteq_inserts(
    meta: &AudioMetadata,
    data: &[u8],
    already_received: impl Fn(u64) -> bool,
) -> NetEqInsertPlan {
    plan_neteq_inserts_at(
        meta,
        data,
        meta.sequence,
        primary_rtp_timestamp(meta),
        already_received,
    )
}

/// Decide which frames (and with which timestamps) one audio packet feeds to
/// NetEq, given the primary frame's resolved sequence `seq` and `timestamp`.
/// The RED-recovered frame is placed `distance` frames before the primary in
/// both. O(1) apart from copying the payload. `already_received(seq)`
/// reports whether a resolved sequence was inserted before (RED dedup).
fn plan_neteq_inserts_at(
    meta: &AudioMetadata,
    data: &[u8],
    seq: u64,
    timestamp: u32,
    already_received: impl Fn(u64) -> bool,
) -> NetEqInsertPlan {
    let whole_blob = |red_malformed| NetEqInsertPlan {
        recovered: None,
        primary: NetEqInsert {
            seq,
            timestamp,
            payload: data.to_vec(),
        },
        red_malformed,
    };

    if meta.audio_format != AUDIO_RED_FORMAT {
        return whole_blob(false);
    }
    // [4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]
    let Some((primary, redundant_seq, redundant_data)) =
        NetEqAudioPeerDecoder::unpack_red_audio(data)
    else {
        return whole_blob(true);
    };

    let recovered = red_distance(meta.sequence, redundant_seq).and_then(|distance| {
        let redundant = seq.wrapping_sub(u64::from(distance));
        if already_received(redundant) {
            return None;
        }
        Some(NetEqInsert {
            seq: redundant,
            timestamp: timestamp
                .wrapping_sub(distance.wrapping_mul(OPUS_RTP_SAMPLES_PER_20MS_FRAME)),
            payload: redundant_data,
        })
    });

    NetEqInsertPlan {
        recovered,
        primary: NetEqInsert {
            seq,
            timestamp,
            payload: primary,
        },
        red_malformed: false,
    }
}

/// Backward distance (in sequence numbers) a legacy packet may arrive late
/// and still count as reordering rather than a sender restart. Matches the
/// RED dedup window.
pub(crate) const LEGACY_REORDER_WINDOW: u64 = AUDIO_RED_SEQ_HISTORY_SIZE as u64;

/// Minimum receive idle time before a small legacy sequence (see
/// [`LEGACY_RESTART_MAX_FIRST_SEQ`]) counts as a restart even inside the
/// reorder window. A legacy browser sender restarts at 0 after mute or
/// device switch, which takes well over this (getUserMedia + AudioContext +
/// worklet load); a reordered packet arrives within a few frames of its
/// successors.
pub(crate) const LEGACY_RESTART_MIN_IDLE_MS: f64 = 100.0;

/// Largest first-seen sequence that still identifies a restart inside the
/// reorder window (after [`LEGACY_RESTART_MIN_IDLE_MS`] idle). 3 tolerates
/// the first three packets of a restarted stream (60 ms) being lost or
/// overtaken by later ones. Each packet travels on its own stream, so
/// three in a row going missing at once is already rare, while a larger
/// value widens the only false-positive case: a reordered packet near the
/// start of an epoch arriving after a stall.
pub(crate) const LEGACY_RESTART_MAX_FIRST_SEQ: u64 = 3;

/// Maps a legacy sender's raw `sequence` (which restarts at 0 on every mic
/// restart) to a monotonic extended frame count for one peer session.
///
/// Within an epoch, `count = raw + offset`, so `offset` is the count of the
/// epoch's raw 0. A restart (raw goes backward past [`LEGACY_REORDER_WINDOW`],
/// or back to at most [`LEGACY_RESTART_MAX_FIRST_SEQ`] after
/// [`LEGACY_RESTART_MIN_IDLE_MS`] of silence) opens a new epoch whose raw 0
/// sits after the highest count seen by the receive idle time in whole 20 ms
/// frames (less the frames before the first seen raw, and always at least
/// one skipped frame), so NetEq sees skipped time instead of obsolete
/// packets, and a raw 0 that arrives late still lands just before raw 1. O(1).
#[derive(Debug, Default, Clone)]
pub(crate) struct LegacySequenceExtender {
    offset: u64,
    epoch_highest_raw: Option<u64>,
    /// `(offset, highest_raw)` of the epoch before the current one, so a
    /// straggler from before the restart maps back to its old place.
    previous_epoch: Option<(u64, u64)>,
    highest_count: u64,
    last_arrival_ms: Option<f64>,
}

impl LegacySequenceExtender {
    /// Extended count for a legacy primary frame with raw sequence `raw`,
    /// received at `arrival_ms` on a monotonic millisecond clock.
    pub(crate) fn extend(&mut self, raw: u64, arrival_ms: f64) -> u64 {
        let idle_ms = self.last_arrival_ms.map(|last| arrival_ms - last);
        self.last_arrival_ms = Some(arrival_ms);

        let Some(epoch_highest_raw) = self.epoch_highest_raw else {
            // First packet: identity mapping, same as `sequence * 960`.
            self.epoch_highest_raw = Some(raw);
            self.highest_count = raw.wrapping_add(self.offset);
            return self.highest_count;
        };

        // A late packet from the previous epoch: far ahead of this epoch,
        // but within the reorder window of where the previous one ended.
        if let Some((previous_offset, previous_highest_raw)) = self.previous_epoch {
            if raw > epoch_highest_raw.saturating_add(LEGACY_REORDER_WINDOW)
                && raw <= previous_highest_raw
                && previous_highest_raw - raw <= LEGACY_REORDER_WINDOW
            {
                return raw.wrapping_add(previous_offset);
            }
        }

        let backward = epoch_highest_raw.saturating_sub(raw);
        let idle_ms = idle_ms.unwrap_or(0.0);
        let restarted = backward > LEGACY_REORDER_WINDOW
            || (raw <= LEGACY_RESTART_MAX_FIRST_SEQ
                && backward > 0
                && idle_ms >= LEGACY_RESTART_MIN_IDLE_MS);
        if restarted {
            // Same jump as libwebrtc on resume: idle time in whole frames
            // past the last frame. Raw 0 was captured `raw` frames before
            // this packet, so its slot is that much earlier, but always at
            // least one skipped frame past the old epoch so the restart
            // never looks like contiguous audio.
            let idle_frames = (idle_ms.max(0.0) / 20.0).floor() as u64;
            let base = self
                .highest_count
                .wrapping_add(idle_frames.saturating_sub(raw).max(2));
            let count = base.wrapping_add(raw);
            self.previous_epoch = Some((self.offset, epoch_highest_raw));
            self.offset = base;
            self.epoch_highest_raw = Some(raw);
            self.highest_count = count;
            return count;
        }

        let count = raw.wrapping_add(self.offset);
        if raw > epoch_highest_raw {
            self.epoch_highest_raw = Some(raw);
            self.highest_count = count;
        }
        count
    }
}

/// Per-peer state that turns received audio packets into NetEq inserts:
/// legacy sequence extension plus the RED dedup ring, both keyed by the
/// sequence actually handed to NetEq. Pure (no browser APIs), O(1) per
/// packet (the ring scan is bounded by `AUDIO_RED_SEQ_HISTORY_SIZE`).
#[derive(Debug, Default)]
pub(crate) struct NetEqTimeline {
    legacy: LegacySequenceExtender,
    received: VecDeque<u64>,
}

impl NetEqTimeline {
    fn record(&mut self, seq: u64) {
        if self.received.len() >= AUDIO_RED_SEQ_HISTORY_SIZE {
            self.received.pop_front();
        }
        self.received.push_back(seq);
    }

    /// Plan the NetEq inserts for one packet that arrived at `arrival_ms`.
    pub(crate) fn plan(
        &mut self,
        meta: &AudioMetadata,
        data: &[u8],
        arrival_ms: f64,
    ) -> NetEqInsertPlan {
        // New senders' sequence and timestamp never go backward: pass them
        // through. Legacy ones go through the restart-aware extender, and
        // both the NetEq sequence and timestamp derive from its count.
        let (seq, timestamp) = match meta.rtp_timestamp {
            Some(timestamp) => (meta.sequence, timestamp),
            None => {
                let count = self.legacy.extend(meta.sequence, arrival_ms);
                (count, legacy_rtp_timestamp(count))
            }
        };
        // The dedup ring is keyed by the resolved sequence, so raw sequence
        // numbers from before a legacy restart cannot shadow new frames.
        self.record(seq);
        let received = &self.received;
        let plan = plan_neteq_inserts_at(meta, data, seq, timestamp, |s| received.contains(&s));
        if let Some(recovered) = &plan.recovered {
            let seq = recovered.seq;
            self.record(seq);
        }
        plan
    }
}

impl Drop for NetEqAudioPeerDecoder {
    fn drop(&mut self) {
        self.worker.terminate();
    }
}

impl crate::decode::AudioPeerDecoderTrait for NetEqAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        match packet.audio_metadata.as_ref() {
            Some(audio_meta) => {
                let seq = audio_meta.sequence;

                // Timestamps come from `rtp_timestamp`, or for senders that
                // predate it from a restart-aware extension of `sequence`;
                // never from the wall-clock `packet.timestamp`, which
                // saturates as a u32. The timeline also does RED dedup.
                let arrival_ms = self.arrival_epoch.elapsed().as_secs_f64() * 1_000.0;
                let plan = self.timeline.plan(audio_meta, &packet.data, arrival_ms);

                if plan.red_malformed {
                    log::warn!(
                        "RED unpack failed for peer {} seq {}, falling back to raw",
                        self.peer_id,
                        seq
                    );
                }

                // A lost earlier frame recovered from the RED payload goes in
                // first, with its own sequence and timestamp.
                if let Some(recovered) = plan.recovered {
                    log::debug!(
                        "RED recovery: injecting lost audio seq {} (ts {}) for peer {}",
                        recovered.seq,
                        recovered.timestamp,
                        self.peer_id
                    );
                    self.send_worker_message(WorkerMsg::Insert {
                        seq: recovered.seq as u16,
                        timestamp: recovered.timestamp,
                        payload: recovered.payload,
                    });
                }

                self.send_worker_message(WorkerMsg::Insert {
                    seq: plan.primary.seq as u16,
                    timestamp: plan.primary.timestamp,
                    payload: plan.primary.payload,
                });

                let first_frame = !self.decoded;
                self.decoded = true;
                Ok(DecodeStatus {
                    rendered: true,
                    first_frame,
                })
            }
            None => {
                // Malformed/old packet that lacks metadata – skip with a warning instead of
                // propagating an error that would reset the entire peer.
                log::warn!(
                    "Received audio packet with length {} without metadata – skipping",
                    packet.data.len()
                );
                Ok(DecodeStatus {
                    rendered: false,
                    first_frame: false,
                })
            }
        }
    }

    fn flush(&mut self) {
        // Send flush message to NetEq worker through queue
        self.send_worker_message(WorkerMsg::Flush);
        log::debug!(
            "Sent flush message to NetEq worker for peer {}",
            self.peer_id
        );
    }

    fn set_muted(&mut self, muted: bool) {
        // Send mute message to NetEq worker through queue
        let mute_msg = WorkerMsg::Mute { muted };
        let now = js_sys::Date::now();
        let is_ready = *self.worker_ready.borrow();
        let queue_length = self.pending_messages.borrow().len();

        // Enhanced logging for mute state tracking
        log::info!(
            "🔇 [MUTE DEBUG] Peer {} set_muted({}) at {:.0}ms - worker_ready: {}, queue_length: {}",
            self.peer_id,
            muted,
            now,
            is_ready,
            queue_length
        );

        self.send_worker_message(mute_msg);

        log::debug!(
            "Sent mute message to NetEq worker for peer {} (muted: {})",
            self.peer_id,
            muted
        );
        log::debug!(
            "✅ Mute message {} for peer {} (muted: {}) at {:.0}ms",
            if is_ready {
                "sent immediately"
            } else {
                "queued"
            },
            self.peer_id,
            muted,
            now
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn unpack_valid_red_data() {
        // Manually build a RED buffer:
        // [4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]
        let primary = b"primary_frame";
        let redundant = b"redundant_frame";
        let primary_len = (primary.len() as u32).to_le_bytes();
        let redundant_seq = 42u32.to_le_bytes();

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(primary);
        data.extend_from_slice(&redundant_seq);
        data.extend_from_slice(redundant);

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (p, seq, r) = result.unwrap();
        assert_eq!(p, primary);
        assert_eq!(seq, 42);
        assert_eq!(r, redundant);
    }

    #[wasm_bindgen_test]
    fn unpack_empty_input() {
        let result = NetEqAudioPeerDecoder::unpack_red_audio(&[]);
        assert!(result.is_none(), "empty input should return None");
    }

    #[wasm_bindgen_test]
    fn unpack_too_short_input() {
        // Less than 8 bytes (minimum: 4 primary_len + 0 primary + 4 redundant_seq)
        let result = NetEqAudioPeerDecoder::unpack_red_audio(&[0, 0, 0]);
        assert!(result.is_none(), "3 bytes should return None");

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&[0, 0, 0, 0, 0, 0, 0]);
        assert!(result.is_none(), "7 bytes should return None");
    }

    #[wasm_bindgen_test]
    fn unpack_exactly_8_bytes_zero_length_frames() {
        // primary_len=0, redundant_seq=0, no primary data, no redundant data
        let data = [0u8; 8];
        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (p, seq, r) = result.unwrap();
        assert!(p.is_empty());
        assert_eq!(seq, 0);
        assert!(r.is_empty());
    }

    #[wasm_bindgen_test]
    fn unpack_primary_len_exceeds_sanity_limit() {
        // primary_len > 10,000 should return None
        let primary_len = 10_001u32.to_le_bytes();
        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&[0u8; 8]); // some filler

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_none(), "primary_len > 10000 should be rejected");
    }

    #[wasm_bindgen_test]
    fn unpack_primary_len_at_sanity_limit() {
        // primary_len == 10,000 should be accepted (boundary)
        let primary_len = 10_000u32.to_le_bytes();
        let primary_data = vec![0xAA; 10_000];
        let redundant_seq = 5u32.to_le_bytes();
        let redundant_data = b"red";

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&primary_data);
        data.extend_from_slice(&redundant_seq);
        data.extend_from_slice(redundant_data);

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some(), "primary_len == 10000 should be accepted");

        let (p, seq, r) = result.unwrap();
        assert_eq!(p.len(), 10_000);
        assert_eq!(seq, 5);
        assert_eq!(r, redundant_data);
    }

    #[wasm_bindgen_test]
    fn unpack_primary_len_exceeds_data_length() {
        // primary_len claims 100 bytes but total data is only 20 bytes
        let primary_len = 100u32.to_le_bytes();
        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&[0u8; 16]); // only 16 bytes after primary_len header

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(
            result.is_none(),
            "malformed packet with primary_len > remaining data should return None"
        );
    }

    #[wasm_bindgen_test]
    fn unpack_no_room_for_redundant_seq() {
        // primary_len = 10, but data only has 4 (header) + 10 (primary) + 3 (not enough for seq)
        let primary_len = 10u32.to_le_bytes();
        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&[0xBB; 10]); // primary data
        data.extend_from_slice(&[0, 0, 0]); // only 3 bytes, need 4 for seq

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(
            result.is_none(),
            "not enough room for redundant_seq should return None"
        );
    }

    #[wasm_bindgen_test]
    fn unpack_no_redundant_data_after_seq() {
        // Valid format but zero-length redundant data
        let primary_len = 5u32.to_le_bytes();
        let redundant_seq = 99u32.to_le_bytes();

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(b"AUDIO");
        data.extend_from_slice(&redundant_seq);
        // No redundant data after seq

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (p, seq, r) = result.unwrap();
        assert_eq!(p, b"AUDIO");
        assert_eq!(seq, 99);
        assert!(r.is_empty());
    }

    #[wasm_bindgen_test]
    fn unpack_preserves_binary_data() {
        // Ensure all byte values 0x00-0xFF are preserved correctly
        let primary: Vec<u8> = (0..=255).collect();
        let redundant: Vec<u8> = (0..=255).rev().collect();
        let primary_len = (primary.len() as u32).to_le_bytes();
        let redundant_seq = 1000u32.to_le_bytes();

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&primary);
        data.extend_from_slice(&redundant_seq);
        data.extend_from_slice(&redundant);

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (p, seq, r) = result.unwrap();
        assert_eq!(p, primary);
        assert_eq!(seq, 1000);
        assert_eq!(r, redundant);
    }

    #[wasm_bindgen_test]
    fn unpack_max_valid_sequence_number() {
        let primary_len = 1u32.to_le_bytes();
        let redundant_seq = u32::MAX.to_le_bytes();

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.push(0xFF); // 1 byte primary
        data.extend_from_slice(&redundant_seq);
        data.push(0xAA); // 1 byte redundant

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (_, seq, _) = result.unwrap();
        assert_eq!(seq, u32::MAX);
    }

    #[wasm_bindgen_test]
    fn record_and_has_sequence() {
        // Test the sequence tracking ring buffer used for RED deduplication.
        // We can't construct a full NetEqAudioPeerDecoder without browser APIs,
        // so we test the VecDeque logic directly.
        use std::collections::VecDeque;

        let capacity = crate::adaptive_quality_constants::AUDIO_RED_SEQ_HISTORY_SIZE;
        let mut received: VecDeque<u64> = VecDeque::with_capacity(capacity);

        // Helper: mirrors record_sequence logic
        let record = |buf: &mut VecDeque<u64>, seq: u64| {
            if buf.len() >= capacity {
                buf.pop_front();
            }
            buf.push_back(seq);
        };

        // Record some sequences
        record(&mut received, 10);
        record(&mut received, 11);
        record(&mut received, 12);

        assert!(received.contains(&10));
        assert!(received.contains(&11));
        assert!(received.contains(&12));
        assert!(!received.contains(&13));

        // Fill to capacity and verify eviction
        for i in 13..(13 + capacity as u64) {
            record(&mut received, i);
        }

        // Sequence 10 should have been evicted
        assert!(!received.contains(&10));
        assert_eq!(received.len(), capacity);
    }
}

/// Native (`cargo test -p videocall-client --lib`) tests for the timestamps
/// handed to NetEq. No browser APIs are touched.
#[cfg(test)]
mod rtp_timestamp_tests {
    use super::*;
    use crate::adaptive_quality_constants::AUDIO_RED_FORMAT;
    use protobuf::Message;
    use videocall_types::protos::media_packet::media_packet::MediaType;

    /// A wall-clock timestamp in ms since 1970, as legacy senders write it.
    const EPOCH_MS: f64 = 1_763_251_200_000.0;

    fn meta(sequence: u64, rtp_timestamp: Option<u32>, red: bool) -> AudioMetadata {
        AudioMetadata {
            sequence,
            rtp_timestamp,
            audio_format: if red {
                AUDIO_RED_FORMAT.to_string()
            } else {
                String::new()
            },
            ..Default::default()
        }
    }

    fn red_payload(primary: &[u8], redundant_seq: u32, redundant: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(primary.len() as u32).to_le_bytes());
        buf.extend_from_slice(primary);
        buf.extend_from_slice(&redundant_seq.to_le_bytes());
        buf.extend_from_slice(redundant);
        buf
    }

    /// Round-trip through the wire format so presence/absence is the real
    /// decoded state, not just the struct we built.
    fn over_the_wire(meta: AudioMetadata) -> AudioMetadata {
        let packet = MediaPacket {
            media_type: MediaType::AUDIO.into(),
            timestamp: EPOCH_MS,
            audio_metadata: Some(meta).into(),
            ..Default::default()
        };
        let bytes = packet.write_to_bytes().unwrap();
        MediaPacket::parse_from_bytes(&bytes)
            .unwrap()
            .audio_metadata
            .unwrap()
    }

    #[test]
    fn present_rtp_timestamp_passes_through_unchanged() {
        for ts in [0u32, 960, 123_456_789, u32::MAX - 959, u32::MAX] {
            let m = over_the_wire(meta(7, Some(ts), false));
            assert_eq!(m.rtp_timestamp, Some(ts), "presence survives the wire");
            assert_eq!(primary_rtp_timestamp(&m), ts);
            let plan = plan_neteq_inserts(&m, b"opus", |_| false);
            assert_eq!(plan.primary.timestamp, ts);
            assert_eq!(plan.primary.seq, 7);
            assert_eq!(plan.primary.payload, b"opus");
            assert!(plan.recovered.is_none());
        }
    }

    #[test]
    fn absent_rtp_timestamp_falls_back_to_sequence_times_960() {
        for seq in [0u64, 1, 392, 4_473_925, u64::MAX] {
            let m = over_the_wire(meta(seq, None, false));
            assert_eq!(m.rtp_timestamp, None);
            let expected = ((seq as u128 * 960) % (1u128 << 32)) as u32;
            assert_eq!(primary_rtp_timestamp(&m), expected);
            assert_eq!(
                plan_neteq_inserts(&m, b"x", |_| false).primary.timestamp,
                expected
            );
        }
    }

    /// The captured live call: 393 legacy packets with epoch-ms timestamps
    /// all reached NetEq as 4294967295. They must now be distinct and
    /// advance by exactly one frame.
    #[test]
    fn legacy_epoch_ms_packets_never_saturate() {
        let mut previous: Option<u32> = None;
        for seq in 0..393u64 {
            let m = over_the_wire(meta(seq, None, false));
            let ts = plan_neteq_inserts(&m, b"x", |_| false).primary.timestamp;
            assert_ne!(ts, u32::MAX, "seq {seq} saturated");
            if let Some(prev) = previous {
                assert_eq!(ts.wrapping_sub(prev), 960);
            }
            previous = Some(ts);
        }
    }

    #[test]
    fn red_recovered_frame_is_one_frame_before_primary_new_sender() {
        let m = over_the_wire(meta(11, Some(50_000), true));
        let data = red_payload(b"primary", 10, b"redundant");
        let plan = plan_neteq_inserts(&m, &data, |_| false);
        let recovered = plan.recovered.expect("seq 10 was lost, so it is recovered");
        assert_eq!(recovered.seq, 10);
        assert_eq!(recovered.timestamp, 50_000 - 960);
        assert_eq!(recovered.payload, b"redundant");
        assert_eq!(plan.primary.timestamp, 50_000);
        assert_eq!(plan.primary.payload, b"primary");
        assert!(!plan.red_malformed);
    }

    #[test]
    fn red_recovered_frame_is_one_frame_before_primary_legacy_sender() {
        let m = over_the_wire(meta(11, None, true));
        let data = red_payload(b"primary", 10, b"redundant");
        let plan = plan_neteq_inserts(&m, &data, |_| false);
        let recovered = plan.recovered.expect("seq 10 was lost, so it is recovered");
        assert_eq!(plan.primary.timestamp, 11 * 960);
        assert_eq!(recovered.timestamp, 10 * 960);
        // Same value the legacy fallback gives seq 10 had it arrived itself,
        // so NetEq sees a true duplicate, not two different frames.
        assert_eq!(
            recovered.timestamp,
            primary_rtp_timestamp(&meta(10, None, false))
        );
    }

    #[test]
    fn red_recovery_wraps_below_zero() {
        let m = meta(1, Some(100), true);
        let plan = plan_neteq_inserts(&m, &red_payload(b"p", 0, b"r"), |_| false);
        assert_eq!(plan.recovered.unwrap().timestamp, 100u32.wrapping_sub(960));
        // Legacy seq 0 -> ts 0; its redundant frame from seq u32::MAX wraps too.
        assert_eq!(
            redundant_rtp_timestamp(0, 1u64 << 32, u32::MAX),
            Some(0u32.wrapping_sub(960))
        );
    }

    #[test]
    fn red_frame_already_received_is_not_reinserted() {
        let m = meta(11, Some(50_000), true);
        let plan = plan_neteq_inserts(&m, &red_payload(b"p", 10, b"r"), |s| s == 10);
        assert!(plan.recovered.is_none());
        assert_eq!(plan.primary.payload, b"p");
    }

    #[test]
    fn red_with_nonsense_distance_is_not_recovered() {
        // Redundant seq equal to or after the primary, or far beyond the
        // dedupe window: drop the redundant frame, keep the primary.
        assert_eq!(redundant_rtp_timestamp(50_000, 11, 11), None);
        assert_eq!(redundant_rtp_timestamp(50_000, 11, 12), None);
        assert_eq!(redundant_rtp_timestamp(50_000, 1_000, 1), None);
        let plan = plan_neteq_inserts(
            &meta(11, Some(50_000), true),
            &red_payload(b"p", 12, b"r"),
            |_| false,
        );
        assert!(plan.recovered.is_none());
        assert_eq!(plan.primary.timestamp, 50_000);
    }

    #[test]
    fn malformed_red_falls_back_to_whole_blob() {
        let m = meta(3, None, true);
        let plan = plan_neteq_inserts(&m, &[1, 2, 3], |_| false);
        assert!(plan.red_malformed);
        assert!(plan.recovered.is_none());
        assert_eq!(plan.primary.payload, vec![1, 2, 3]);
        assert_eq!(plan.primary.timestamp, 3 * 960);
    }
}

/// Native tests for legacy-sender restarts (senders without `rtp_timestamp`
/// restart `sequence` at 0 on every mic restart or device switch).
#[cfg(test)]
mod legacy_restart_tests {
    use super::*;
    use crate::adaptive_quality_constants::AUDIO_RED_FORMAT;

    const FRAME: u32 = OPUS_RTP_SAMPLES_PER_20MS_FRAME;

    fn legacy(sequence: u64) -> AudioMetadata {
        AudioMetadata {
            sequence,
            ..Default::default()
        }
    }

    fn legacy_red(sequence: u64) -> AudioMetadata {
        AudioMetadata {
            sequence,
            audio_format: AUDIO_RED_FORMAT.to_string(),
            ..Default::default()
        }
    }

    fn red_payload(primary: &[u8], redundant_seq: u32, redundant: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(primary.len() as u32).to_le_bytes());
        buf.extend_from_slice(primary);
        buf.extend_from_slice(&redundant_seq.to_le_bytes());
        buf.extend_from_slice(redundant);
        buf
    }

    /// Feed raw legacy sequences `range` at 20 ms spacing starting at
    /// `start_ms`; returns the arrival time of the last one.
    fn feed(
        timeline: &mut NetEqTimeline,
        range: std::ops::RangeInclusive<u64>,
        start_ms: f64,
    ) -> f64 {
        let mut t = start_ms;
        let mut last = start_ms;
        for raw in range {
            timeline.plan(&legacy(raw), b"x", t);
            last = t;
            t += 20.0;
        }
        last
    }

    #[test]
    fn in_order_legacy_stream_is_sequence_times_960() {
        let mut timeline = NetEqTimeline::default();
        for raw in 0..500u64 {
            let plan = timeline.plan(&legacy(raw), b"x", raw as f64 * 20.0);
            assert_eq!(plan.primary.seq, raw);
            assert_eq!(plan.primary.timestamp, raw as u32 * FRAME);
        }
    }

    #[test]
    fn reordering_within_window_does_not_start_an_epoch() {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=100, 0.0);
        // A late packet from 30 frames back, arriving right away.
        let late = timeline.plan(&legacy(70), b"x", last + 5.0);
        assert_eq!(late.primary.seq, 70);
        assert_eq!(late.primary.timestamp, 70 * FRAME);
        let next = timeline.plan(&legacy(101), b"x", last + 20.0);
        assert_eq!(next.primary.seq, 101);
        assert_eq!(next.primary.timestamp, 101 * FRAME);
    }

    #[test]
    fn red_recovered_older_frame_does_not_start_an_epoch() {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=99, 0.0);
        // Seq 100 lost; 101 carries it as redundancy.
        let plan = timeline.plan(&legacy_red(101), &red_payload(b"p", 100, b"r"), last + 40.0);
        let recovered = plan.recovered.expect("100 recovered");
        assert_eq!(recovered.seq, 100);
        assert_eq!(recovered.timestamp, 100 * FRAME);
        assert_eq!(plan.primary.seq, 101);
        let next = timeline.plan(&legacy(102), b"x", last + 60.0);
        assert_eq!(next.primary.seq, 102);
        assert_eq!(next.primary.timestamp, 102 * FRAME);
    }

    fn assert_restart_continues_forward(idle_ms: f64) {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=499, 0.0);
        let restarted = timeline.plan(&legacy(0), b"x", last + idle_ms);
        // Continues after the highest count (499) by the idle time in whole
        // frames, like libwebrtc's resume jump.
        let expected = 499 + (idle_ms / 20.0).floor() as u64;
        assert_eq!(restarted.primary.seq, expected);
        assert_eq!(restarted.primary.timestamp, legacy_rtp_timestamp(expected));
        let next = timeline.plan(&legacy(1), b"x", last + idle_ms + 20.0);
        assert_eq!(next.primary.seq, expected + 1);
        assert_eq!(
            next.primary.timestamp,
            restarted.primary.timestamp.wrapping_add(FRAME)
        );
    }

    #[test]
    fn restart_at_zero_after_3_s_continues_forward_with_a_gap() {
        assert_restart_continues_forward(3_000.0);
    }

    #[test]
    fn restart_at_zero_after_60_s_continues_forward_with_a_gap() {
        assert_restart_continues_forward(60_000.0);
    }

    #[test]
    fn restart_with_no_idle_still_skips_at_least_one_frame() {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=499, 0.0);
        // Backward far beyond the window with no idle time at all. The new
        // epoch's raw 0 skips one frame past 499; raw 3 is three after it.
        let restarted = timeline.plan(&legacy(3), b"x", last + 1.0);
        assert_eq!(restarted.primary.seq, 499 + 2 + 3);
    }

    #[test]
    fn quick_restart_inside_window_is_detected_by_seq_zero_after_idle() {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=10, 0.0);
        // Muted after 11 frames, unmuted 500 ms later: back to 0.
        let restarted = timeline.plan(&legacy(0), b"x", last + 500.0);
        assert_eq!(restarted.primary.seq, 10 + 25);
    }

    /// Short first epoch (raw 0..=10), then a device switch 500 ms later
    /// whose first packets are lost: the first seen raw is `first`.
    fn restart_with_leading_loss(first: u64) {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=10, 0.0);
        let arrival = last + 500.0;
        let restarted = timeline.plan(&legacy(first), b"x", arrival);
        // raw 0 of the new epoch would have arrived `first` frames earlier:
        // it maps to 10 + (25 - first), and raw `first` is that plus `first`.
        let base = 10 + 25 - first;
        assert_eq!(
            restarted.primary.seq,
            base + first,
            "first seen raw {first}"
        );
        assert!(
            restarted.primary.seq > 10 + 1,
            "skips time past the old epoch"
        );
        let next = timeline.plan(&legacy(first + 1), b"x", arrival + 20.0);
        assert_eq!(next.primary.seq, base + first + 1);
    }

    #[test]
    fn restart_with_packet_zero_lost_first_seen_1() {
        restart_with_leading_loss(1);
    }

    #[test]
    fn restart_with_packet_zero_lost_first_seen_2() {
        restart_with_leading_loss(2);
    }

    #[test]
    fn restart_with_packet_zero_lost_first_seen_3() {
        restart_with_leading_loss(3);
    }

    #[test]
    fn restart_where_raw_1_arrives_before_raw_0() {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=10, 0.0);
        let one = timeline.plan(&legacy(1), b"x", last + 500.0);
        let zero = timeline.plan(&legacy(0), b"x", last + 505.0);
        // Raw 0 lands just before raw 1 in the new epoch, not in the old one.
        assert_eq!(zero.primary.seq + 1, one.primary.seq);
        assert!(zero.primary.seq > 10 + 1);
        assert_eq!(
            zero.primary.timestamp.wrapping_add(FRAME),
            one.primary.timestamp
        );
        let two = timeline.plan(&legacy(2), b"x", last + 520.0);
        assert_eq!(two.primary.seq, one.primary.seq + 1);
    }

    #[test]
    fn small_backward_jump_without_idle_is_still_reordering() {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=10, 0.0);
        let late = timeline.plan(&legacy(2), b"x", last + 5.0);
        assert_eq!(late.primary.seq, 2);
    }

    /// Known limitation, documented: a reordered raw 0 at stream start that
    /// arrives after a >= 100 ms stall is indistinguishable from a restart.
    /// It opens a new epoch, so the stream jumps forward once by the stall
    /// (here 180 ms) instead of NetEq dropping frames for up to 1.28 s.
    #[test]
    fn reordered_zero_after_stall_at_stream_start_is_read_as_restart() {
        let mut timeline = NetEqTimeline::default();
        timeline.plan(&legacy(1), b"x", 0.0);
        timeline.plan(&legacy(2), b"x", 20.0);
        let late_zero = timeline.plan(&legacy(0), b"x", 200.0);
        assert_eq!(late_zero.primary.seq, 2 + 9);
        let three = timeline.plan(&legacy(3), b"x", 205.0);
        assert_eq!(three.primary.seq, 2 + 9 + 3);
    }

    #[test]
    fn reordered_first_packet_is_not_a_restart() {
        let mut timeline = NetEqTimeline::default();
        timeline.plan(&legacy(1), b"x", 0.0);
        timeline.plan(&legacy(2), b"x", 20.0);
        let late_zero = timeline.plan(&legacy(0), b"x", 25.0);
        assert_eq!(late_zero.primary.seq, 0);
        assert_eq!(late_zero.primary.timestamp, 0);
    }

    #[test]
    fn projections_wrap_u16_and_u32() {
        let mut extender = LegacySequenceExtender::default();
        // Around the u16 sequence wrap.
        for raw in 65_530..65_545u64 {
            let count = extender.extend(raw, raw as f64 * 20.0);
            assert_eq!(count, raw);
            assert_eq!(count as u16, (raw % 65_536) as u16);
        }
        // Around the u32 timestamp wrap (2^32 / 960 = 4_473_924.27).
        let mut timeline = NetEqTimeline::default();
        let mut prev: Option<u32> = None;
        for raw in 4_473_920..4_473_930u64 {
            let ts = timeline
                .plan(&legacy(raw), b"x", raw as f64 * 20.0)
                .primary
                .timestamp;
            if let Some(p) = prev {
                assert_eq!(ts.wrapping_sub(p), FRAME);
            }
            prev = Some(ts);
        }
        // A restart epoch that crosses the u16 boundary keeps advancing.
        let restarted = timeline.plan(&legacy(0), b"x", 4_473_929.0 * 20.0 + 3_000.0);
        assert_eq!(restarted.primary.seq, 4_473_929 + 150);
        assert_eq!(
            restarted.primary.timestamp,
            prev.unwrap().wrapping_add(150 * FRAME)
        );
    }

    #[test]
    fn present_rtp_timestamp_passes_through_and_leaves_legacy_state_alone() {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=499, 0.0);
        let new = AudioMetadata {
            sequence: 3,
            rtp_timestamp: Some(77_000),
            ..Default::default()
        };
        let plan = timeline.plan(&new, b"x", last + 5_000.0);
        assert_eq!(plan.primary.seq, 3);
        assert_eq!(plan.primary.timestamp, 77_000);
        // The legacy mapping is unaffected by the new-format packet.
        let next = timeline.plan(&legacy(500), b"x", last + 20.0);
        assert_eq!(next.primary.seq, 500);
    }

    #[test]
    fn late_packet_from_before_the_restart_maps_into_the_old_epoch() {
        let mut timeline = NetEqTimeline::default();
        let last = feed(&mut timeline, 0..=499, 0.0);
        let t = feed(&mut timeline, 0..=3, last + 3_000.0);
        let new_epoch_3 = 499 + 150 + 3;
        // A straggler from the old stream (raw 495) shows up late. It keeps
        // its old place (NetEq drops it as obsolete) instead of being read
        // as a forward jump in the new epoch.
        let straggler = timeline.plan(&legacy(495), b"x", t + 5.0);
        assert_eq!(straggler.primary.seq, 495);
        let next = timeline.plan(&legacy(4), b"x", t + 20.0);
        assert_eq!(next.primary.seq, new_epoch_3 + 1);
    }

    #[test]
    fn red_dedup_survives_a_legacy_restart() {
        let mut timeline = NetEqTimeline::default();
        // A short first epoch, so raw seq 4 is still in the dedup ring.
        let last = feed(&mut timeline, 0..=20, 0.0);
        // Restart after 3 s; raw 0..=3 arrive, raw 4 is lost, raw 5 carries 4.
        let t = feed(&mut timeline, 0..=3, last + 3_000.0);
        let plan = timeline.plan(&legacy_red(5), &red_payload(b"p", 4, b"r"), t + 40.0);
        let recovered = plan
            .recovered
            .expect("old raw seq 4 from before the restart must not suppress recovery");
        assert_eq!(recovered.seq + 1, plan.primary.seq);
        assert_eq!(
            recovered.timestamp.wrapping_add(FRAME),
            plan.primary.timestamp
        );
        // And a genuine duplicate in the new epoch is still suppressed.
        let again = timeline.plan(&legacy_red(6), &red_payload(b"q", 5, b"p"), t + 60.0);
        assert!(again.recovered.is_none());
    }
}
