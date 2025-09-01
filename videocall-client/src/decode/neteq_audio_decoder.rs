use crate::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::decode::peer_audio_worklet_manager::PeerAudioWorkletManager;
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
use web_sys::{AudioContext, AudioContextOptions, MessageEvent, Worker};

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
pub struct NetEqAudioPeerDecoder {
    worker: Worker,
    audio_context: AudioContext,
    decoded: bool,
    peer_id: String,
    worklet_manager: Rc<PeerAudioWorkletManager>,

    // Message queueing system
    pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
    worker_ready: Rc<RefCell<bool>>,
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
            log::info!(
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

    /// Handle PCM audio data from NetEq worker using the new worklet manager
    fn handle_pcm_data(
        pcm: Float32Array,
        worklet_manager: Rc<PeerAudioWorkletManager>,
        audio_context: &AudioContext,
    ) {
        // Ensure AudioContext is running
        if let Err(e) = audio_context.resume() {
            web_sys::console::warn_1(
                &format!("[neteq-audio-decoder] AudioContext resume error: {e:?}").into(),
            );
        }

        // Use the worklet manager to process PCM data safely
        if let Err(e) = worklet_manager.process_pcm_data(pcm) {
            // Only log errors that are not expected during shutdown
            match e {
                crate::decode::peer_audio_worklet_manager::PCMError::PeerDisconnected => {
                    // Expected during cleanup - no logging needed
                }
                _ => {
                    log::debug!("PCM processing failed: {e:?}");
                }
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
        worklet_manager: Rc<PeerAudioWorkletManager>,
        audio_context: AudioContext,
        peer_id: String,
        worker_ready: Rc<RefCell<bool>>,
        pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
        worker: Worker,
    ) -> Closure<dyn FnMut(MessageEvent)> {
        Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();

            if data.is_instance_of::<Float32Array>() {
                // High-performance PCM path using the new worklet manager
                let pcm = Float32Array::from(data);
                Self::handle_pcm_data(pcm, worklet_manager.clone(), &audio_context);
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
                                        log::info!("📤 Sent queued message: {msg:?}");
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
    ) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
        Self::new_with_mute_state(speaker_device_id, peer_id, true) // Default to muted
    }

    /// Create audio decoder with explicit initial mute state
    pub fn new_with_mute_state(
        speaker_device_id: Option<String>,
        peer_id: String,
        initial_muted: bool,
    ) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
        // Create worker
        let worker = Self::create_neteq_worker()?;

        // Create AudioContext with enforced 48kHz for all browsers
        let options = AudioContextOptions::new();
        options.set_sample_rate(48000.0);
        let audio_context = AudioContext::new_with_context_options(&options)?;
        log::info!(
            "[audio] Created AudioContext for peer {} at {:.0} Hz",
            peer_id,
            audio_context.sample_rate()
        );

        // Set sink device if specified
        if let Some(device_id) = &speaker_device_id {
            log::info!(
                "[audio] Requesting sink change via setSinkId to device: {}",
                device_id
            );
            if js_sys::Reflect::has(&audio_context, &JsValue::from_str("setSinkId"))
                .unwrap_or(false)
            {
                let promise = audio_context.set_sink_id_with_str(device_id);
                let device_id_clone = device_id.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match wasm_bindgen_futures::JsFuture::from(promise).await {
                        Ok(_) => {
                            log::info!(
                                "[audio] AudioContext.setSinkId() applied: {device_id_clone}"
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "[audio] AudioContext.setSinkId() failed for {device_id_clone}: {e:?}",
                            );
                        }
                    }
                });
            } else {
                log::warn!(
                    "[audio] setSinkId not supported on AudioContext in this browser; using default output"
                );
            }
        } else {
            log::info!("[audio] Using default output device (no sinkId provided)");
        }

        // Create the dedicated worklet manager for this peer
        let worklet_manager = Rc::new(PeerAudioWorkletManager::new(
            peer_id.clone(),
            audio_context.clone(),
        ));

        // Create decoder with the new worklet manager
        let mut decoder = Self {
            worker: worker.clone(),
            audio_context: audio_context.clone(),
            decoded: false,
            peer_id: peer_id.clone(),
            worklet_manager: worklet_manager.clone(),

            // Message queueing system
            pending_messages: Rc::new(RefCell::new(VecDeque::new())),
            worker_ready: Rc::new(RefCell::new(false)),
        };

        // Set up worker message handling with decoder's queue references
        let on_message_closure = Self::create_message_handler(
            worklet_manager.clone(),
            audio_context.clone(),
            peer_id.clone(),
            decoder.worker_ready.clone(),
            decoder.pending_messages.clone(),
            worker.clone(),
        );

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));
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

        // Attempt to resume audio context immediately to satisfy autoplay policies
        if let Err(e) = decoder.audio_context.resume() {
            log::warn!("[audio] AudioContext.resume() returned error (may be fine until user gesture): {:?}", e);
        } else {
            log::info!("[audio] AudioContext resume requested");
        }

        // Enable diagnostics in the NetEQ worker
        decoder.send_worker_message(WorkerMsg::SetDiagnostics { enabled: true });
        log::info!(
            "🔧 Enabled diagnostics for NetEq worker for peer {}",
            decoder.peer_id
        );

        Ok(Box::new(decoder))
    }
}

impl Drop for NetEqAudioPeerDecoder {
    fn drop(&mut self) {
        log::info!(
            "Cleaning up NetEqAudioPeerDecoder for peer {}",
            self.peer_id
        );

        // Initiate graceful shutdown of the worklet manager
        let worklet_manager = Rc::clone(&self.worklet_manager);
        wasm_bindgen_futures::spawn_local(async move {
            worklet_manager.graceful_shutdown().await;
        });

        // Clean up worker
        self.worker.terminate();

        // Close audio context
        let _ = self.audio_context.close();
    }
}

impl crate::decode::AudioPeerDecoderTrait for NetEqAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        match packet.audio_metadata.as_ref() {
            Some(audio_meta) => {
                // Normal path – send the packet to the NetEq worker through queue
                let insert = WorkerMsg::Insert {
                    seq: audio_meta.sequence as u16,
                    timestamp: packet.timestamp as u32,
                    payload: packet.data.clone(),
                };

                // Send through queue (will be immediate if worker ready, queued otherwise)
                self.send_worker_message(insert);

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
        log::info!(
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
