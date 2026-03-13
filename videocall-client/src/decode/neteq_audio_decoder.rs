use crate::adaptive_quality_constants::{
    AUDIO_RED_FORMAT, AUDIO_RED_SEQ_HISTORY_SIZE, OPUS_FRAME_DURATION_MS,
};
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
use videocall_types::protos::media_packet::MediaPacket;
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

    /// Ring buffer of recently received audio sequence numbers.
    /// Used to detect whether a redundant frame carried in a RED packet
    /// was already received, avoiding duplicate injection.
    received_sequences: VecDeque<u64>,
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

            // RED redundancy: track recently received sequence numbers
            received_sequences: VecDeque::with_capacity(AUDIO_RED_SEQ_HISTORY_SIZE),
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

    /// Record a sequence number as received for RED deduplication.
    fn record_sequence(&mut self, seq: u64) {
        if self.received_sequences.len() >= AUDIO_RED_SEQ_HISTORY_SIZE {
            self.received_sequences.pop_front();
        }
        self.received_sequences.push_back(seq);
    }

    /// Check whether a sequence number was already received.
    fn has_sequence(&self, seq: u64) -> bool {
        self.received_sequences.contains(&seq)
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

                // Track this sequence number so we can detect duplicates from
                // redundancy payloads later.
                self.record_sequence(seq);

                // Check whether the packet carries RED-style redundancy.
                let is_red = audio_meta.audio_format == AUDIO_RED_FORMAT;

                if is_red {
                    // Unpack the RED payload:
                    // [4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]
                    if let Some((primary, redundant_seq, redundant_data)) =
                        Self::unpack_red_audio(&packet.data)
                    {
                        // First, check if the redundant frame was lost (not yet received).
                        if !self.has_sequence(redundant_seq as u64) {
                            log::debug!(
                                "RED recovery: injecting lost audio seq {} for peer {}",
                                redundant_seq,
                                self.peer_id
                            );
                            self.record_sequence(redundant_seq as u64);
                            // Inject the recovered frame with its original sequence and
                            // an earlier timestamp (one Opus frame before the primary).
                            let recovered_insert = WorkerMsg::Insert {
                                seq: redundant_seq as u16,
                                timestamp: (packet.timestamp as u32)
                                    .saturating_sub(OPUS_FRAME_DURATION_MS),
                                payload: redundant_data,
                            };
                            self.send_worker_message(recovered_insert);
                        }

                        // Now send the primary frame.
                        let insert = WorkerMsg::Insert {
                            seq: seq as u16,
                            timestamp: packet.timestamp as u32,
                            payload: primary,
                        };
                        self.send_worker_message(insert);
                    } else {
                        // RED unpack failed -- fall back to treating the whole
                        // data blob as a single frame.
                        log::warn!(
                            "RED unpack failed for peer {} seq {}, falling back to raw",
                            self.peer_id,
                            seq
                        );
                        let insert = WorkerMsg::Insert {
                            seq: seq as u16,
                            timestamp: packet.timestamp as u32,
                            payload: packet.data.clone(),
                        };
                        self.send_worker_message(insert);
                    }
                } else {
                    // Standard (non-RED) audio packet.
                    let insert = WorkerMsg::Insert {
                        seq: seq as u16,
                        timestamp: packet.timestamp as u32,
                        payload: packet.data.clone(),
                    };
                    self.send_worker_message(insert);
                }

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
