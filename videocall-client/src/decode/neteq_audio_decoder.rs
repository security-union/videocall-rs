use crate::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::decode::{AudioPeerDecoderTrait, DecodeStatus};
use js_sys::Float32Array;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_wasm_bindgen;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextOptions, AudioWorkletNode, MessageEvent, Worker};

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
    audio_context: AudioContext,
    decoded: bool,
    peer_id: String, // Track which peer this decoder belongs to
    _pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>, // AudioWorklet PCM player

    // Message queueing system
    pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
    worker_ready: Rc<RefCell<bool>>,

    // AudioWorklet initialization guards (atomics only)
    worklet_ready: Arc<AtomicBool>,
    worklet_inflight: Arc<AtomicBool>,
    retry_scheduled: Arc<AtomicBool>,
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

    /// Create Safari-optimized AudioContext with PCM player worklet (original path)
    async fn create_safari_audio_context(
        speaker_device_id: Option<String>,
    ) -> Result<(AudioContext, AudioWorkletNode), JsValue> {
        // Create AudioContext with ENFORCED 48kHz
        let options = AudioContextOptions::new();
        options.set_sample_rate(48000.0);
        let audio_context = AudioContext::new_with_context_options(&options)?;

        let actual_sample_rate = audio_context.sample_rate();
        log::info!("Safari AudioContext sample rate: {actual_sample_rate}");
        if (actual_sample_rate - 48000.0).abs() > 1.0 {
            log::warn!("⚠️ Safari AudioContext sample rate mismatch! Expected 48000, got: {actual_sample_rate}");
        }

        // Register PCM player worklet
        let worklet_code = include_str!("../../../neteq/src/scripts/pcmPlayerWorker.js");
        let blob_parts = js_sys::Array::new();
        blob_parts.push(&JsValue::from_str(worklet_code));
        let blob_property_bag = web_sys::BlobPropertyBag::new();
        blob_property_bag.set_type("application/javascript");
        let blob =
            web_sys::Blob::new_with_str_sequence_and_options(&blob_parts, &blob_property_bag)?;
        let worklet_url = web_sys::Url::create_object_url_with_blob(&blob)?;
        let audio_worklet = audio_context.audio_worklet()?;
        let module_promise = audio_worklet.add_module(&worklet_url)?;
        JsFuture::from(module_promise).await?;
        web_sys::Url::revoke_object_url(&worklet_url)?;

        // Create node and connect
        let pcm_player = AudioWorkletNode::new(&audio_context, "pcm-player")?;
        pcm_player.connect_with_audio_node(&audio_context.destination())?;

        // Configure 48kHz + channels
        let config_message = js_sys::Object::new();
        js_sys::Reflect::set(&config_message, &"command".into(), &"configure".into())?;
        js_sys::Reflect::set(
            &config_message,
            &"sampleRate".into(),
            &JsValue::from(48000.0),
        )?;
        js_sys::Reflect::set(
            &config_message,
            &"channels".into(),
            &JsValue::from(AUDIO_CHANNELS as f32),
        )?;
        pcm_player.port()?.post_message(&config_message)?;

        // Optional sink device
        if let Some(device_id) = speaker_device_id {
            if js_sys::Reflect::has(&audio_context, &JsValue::from_str("setSinkId"))
                .unwrap_or(false)
            {
                let promise = audio_context.set_sink_id_with_str(&device_id);
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = JsFuture::from(promise).await;
                });
            }
        }

        Ok((audio_context, pcm_player))
    }

    // request_worklet_init removed in favor of minimal guard inside ensure_worklet_initialized

    /// Handle PCM audio data from NetEq worker
    fn handle_pcm_data(
        pcm: Float32Array,
        pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>,
        audio_context: &AudioContext,
        speaker_device_id: Option<String>,
        worklet_ready: Arc<AtomicBool>,
        worklet_inflight: Arc<AtomicBool>,
        retry_scheduled: Arc<AtomicBool>,
    ) {
        // Ensure AudioContext is running
        if let Err(e) = audio_context.resume() {
            web_sys::console::warn_1(
                &format!("[neteq-audio-decoder] AudioContext resume error: {e:?}").into(),
            );
        }

        let pcm_player_clone = pcm_player.clone();
        let worklet_ready_c = worklet_ready.clone();
        let worklet_inflight_c = worklet_inflight.clone();
        let retry_scheduled_c = retry_scheduled.clone();
        wasm_bindgen_futures::spawn_local(async move {
            NetEqAudioPeerDecoder::ensure_worklet_initialized(
                &pcm_player_clone,
                speaker_device_id,
                worklet_ready_c,
                worklet_inflight_c,
                retry_scheduled_c,
            )
            .await;
            if let Some(ref worklet) = *pcm_player_clone.borrow() {
                NetEqAudioPeerDecoder::send_pcm_to_safari_worklet(worklet, &pcm);
            }
        });
    }

    /// Ensure AudioWorklet is initialized (lazy) with single-flight guard and fixed retry
    async fn ensure_worklet_initialized(
        pcm_player: &Rc<RefCell<Option<AudioWorkletNode>>>,
        speaker_device_id: Option<String>,
        worklet_ready: Arc<AtomicBool>,
        worklet_inflight: Arc<AtomicBool>,
        retry_scheduled: Arc<AtomicBool>,
    ) {
        // Already present
        if pcm_player.borrow().is_some() || worklet_ready.load(Ordering::Relaxed) {
            return;
        }

        // Single-flight guard
        if worklet_inflight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        log::info!("Initializing AudioWorklet for PCM playback");
        match Self::create_safari_audio_context(speaker_device_id.clone()).await {
            Ok((_, worklet)) => {
                *pcm_player.borrow_mut() = Some(worklet);
                worklet_ready.store(true, Ordering::SeqCst);
                worklet_inflight.store(false, Ordering::SeqCst);
                log::info!("AudioWorklet initialized successfully");
            }
            Err(e) => {
                web_sys::console::error_2(&"Failed to initialize worklet:".into(), &e);
                worklet_inflight.store(false, Ordering::SeqCst);

                // Schedule one fixed-delay retry if not already scheduled
                if !retry_scheduled.load(Ordering::SeqCst) {
                    retry_scheduled.store(true, Ordering::SeqCst);
                    let pcm_player_r = pcm_player.clone();
                    let retry_scheduled_c = retry_scheduled.clone();
                    let worklet_ready_c = worklet_ready.clone();
                    let worklet_inflight_c = worklet_inflight.clone();
                    let speaker_retry = speaker_device_id.clone();
                    let cb = Closure::wrap(Box::new(move || {
                        retry_scheduled_c.store(false, Ordering::SeqCst);
                        let pcm_player_rr = pcm_player_r.clone();
                        let speaker_retry2 = speaker_retry.clone();
                        let worklet_ready_c = worklet_ready_c.clone();
                        let worklet_inflight_c = worklet_inflight_c.clone();
                        let retry_scheduled_c = retry_scheduled_c.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            NetEqAudioPeerDecoder::ensure_worklet_initialized(
                                &pcm_player_rr,
                                speaker_retry2,
                                worklet_ready_c,
                                worklet_inflight_c,
                                retry_scheduled_c,
                            )
                            .await;
                        });
                    }) as Box<dyn FnMut()>);
                    let _ = web_sys::window()
                        .unwrap()
                        .set_timeout_with_callback_and_timeout_and_arguments_0(
                            cb.as_ref().unchecked_ref(),
                            1500,
                        );
                    cb.forget();
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
    #[allow(clippy::too_many_arguments)]
    fn create_message_handler(
        pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>,
        audio_context: AudioContext,
        peer_id: String,
        speaker_device_id: Option<String>,
        worker_ready: Rc<RefCell<bool>>,
        pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
        worker: Worker,
        worklet_ready: Arc<AtomicBool>,
        worklet_inflight: Arc<AtomicBool>,
        retry_scheduled: Arc<AtomicBool>,
    ) -> Closure<dyn FnMut(MessageEvent)> {
        Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();

            if data.is_instance_of::<Float32Array>() {
                // High-performance PCM path (unchanged)
                let pcm = Float32Array::from(data);
                Self::handle_pcm_data(
                    pcm,
                    pcm_player.clone(),
                    &audio_context,
                    speaker_device_id.clone(),
                    worklet_ready.clone(),
                    worklet_inflight.clone(),
                    retry_scheduled.clone(),
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
                                        log::info!("📤 Sent queued message: {msg:?}");
                                    }
                                }
                            }

                            // Keep original behavior: worklet will lazily init on first PCM
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

        // Set sink device if specified
        if let Some(device_id) = &speaker_device_id {
            if js_sys::Reflect::has(&audio_context, &JsValue::from_str("setSinkId"))
                .unwrap_or(false)
            {
                let promise = audio_context.set_sink_id_with_str(device_id);
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = JsFuture::from(promise).await;
                });
            }
        }

        let pcm_player_ref = Rc::new(RefCell::new(None::<AudioWorkletNode>));

        // Create decoder with explicit mute state first
        let mut decoder = Self {
            worker: worker.clone(),
            audio_context: audio_context.clone(),
            decoded: false,
            peer_id: peer_id.clone(),
            _pcm_player: pcm_player_ref.clone(),

            // Message queueing system
            pending_messages: Rc::new(RefCell::new(VecDeque::new())),
            worker_ready: Rc::new(RefCell::new(false)),

            // AudioWorklet guards (minimized)
            worklet_ready: Arc::new(AtomicBool::new(false)),
            worklet_inflight: Arc::new(AtomicBool::new(false)),
            retry_scheduled: Arc::new(AtomicBool::new(false)),
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
            decoder.worklet_ready.clone(),
            decoder.worklet_inflight.clone(),
            decoder.retry_scheduled.clone(),
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

        // Enable diagnostics in the NetEQ worker
        decoder.send_worker_message(WorkerMsg::SetDiagnostics { enabled: true });
        log::info!(
            "🔧 Enabled diagnostics for NetEq worker for peer {}",
            decoder.peer_id
        );

        Ok(Box::new(decoder))
    }

    /// Create audio decoder with shared audio context
    #[cfg(feature = "neteq_ff")]
    pub fn new_with_shared_audio(
        peer_id: String,
        shared_audio_manager: Rc<RefCell<crate::audio::SharedNetEqAudioManager>>,
    ) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
        // Create worker
        let worker = Self::create_neteq_worker()?;

        // Use placeholder for PCM player - will be set up asynchronously via shared manager
        let pcm_player_ref = Rc::new(RefCell::new(None::<AudioWorkletNode>));

        // Create decoder structure
        let mut decoder = Self {
            worker: worker.clone(),
            audio_context: shared_audio_manager.borrow().audio_context().clone(),
            decoded: false,
            peer_id: peer_id.clone(),
            _pcm_player: pcm_player_ref.clone(),

            // Message queueing system
            pending_messages: Rc::new(RefCell::new(VecDeque::new())),
            worker_ready: Rc::new(RefCell::new(false)),

            // AudioWorklet guards
            worklet_ready: Arc::new(AtomicBool::new(false)),
            worklet_inflight: Arc::new(AtomicBool::new(false)),
            retry_scheduled: Arc::new(AtomicBool::new(false)),
        };

        // Set up worker message handling for shared audio
        let on_message_closure = Self::create_shared_audio_message_handler(
            peer_id.clone(),
            shared_audio_manager.clone(),
            pcm_player_ref.clone(),
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

        log::info!("NetEq audio decoder initialized for peer {peer_id} with shared audio context");

        // Set the initial mute state explicitly (peers start muted)
        decoder.set_muted(true);

        // Enable diagnostics in the NetEQ worker
        decoder.send_worker_message(WorkerMsg::SetDiagnostics { enabled: true });

        // Set up peer in shared audio manager asynchronously
        let peer_id_clone = peer_id.clone();
        let shared_audio_clone = shared_audio_manager.clone();

        #[allow(clippy::await_holding_refcell_ref)]
        wasm_bindgen_futures::spawn_local(async move {
            match shared_audio_clone
                .borrow_mut()
                .add_peer(peer_id_clone.clone())
                .await
            {
                Ok(_peer_sink) => {
                    log::info!("Successfully added peer to shared audio manager: {peer_id_clone}");
                }
                Err(e) => {
                    log::error!("Failed to add peer to shared audio manager: {e:?}");
                }
            }
        });

        Ok(Box::new(decoder))
    }

    /// Create message handler for shared audio architecture
    #[cfg(feature = "neteq_ff")]
    fn create_shared_audio_message_handler(
        peer_id: String,
        shared_audio_manager: Rc<RefCell<crate::audio::SharedNetEqAudioManager>>,
        _pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>,
        worker_ready: Rc<RefCell<bool>>,
        pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
        worker: Worker,
    ) -> Closure<dyn FnMut(MessageEvent)> {
        Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();

            if data.is_instance_of::<Float32Array>() {
                // PCM data from NetEq worker - send to shared audio manager
                let pcm = Float32Array::from(data);

                if let Err(e) = shared_audio_manager.borrow().send_peer_pcm(&peer_id, pcm) {
                    log::warn!(
                        "Failed to send PCM to shared audio manager for peer {peer_id}: {e:?}"
                    );
                }
            } else if data.is_object() {
                // Try to parse as WorkerResponse first
                if let Ok(response) = serde_wasm_bindgen::from_value::<WorkerResponse>(data.clone())
                {
                    match response {
                        WorkerResponse::WorkerReady { mute_state } => {
                            log::info!(
                                "🎵 NetEq worker ready for peer {peer_id} (muted: {mute_state})"
                            );
                            *worker_ready.borrow_mut() = true;

                            // Send all pending messages
                            let mut pending = pending_messages.borrow_mut();
                            while let Some(msg) = pending.pop_front() {
                                if let Err(e) = serde_wasm_bindgen::to_value(&msg)
                                    .map(|js_msg| worker.post_message(&js_msg))
                                {
                                    log::error!("Failed to send queued worker message: {e:?}");
                                }
                            }
                        }
                        WorkerResponse::Stats { .. } => {
                            // Handle stats if needed
                        }
                    }
                } else {
                    log::debug!(
                        "Unknown message type received from NetEq worker for peer {peer_id}"
                    );
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>)
    }
}

impl Drop for NetEqAudioPeerDecoder {
    fn drop(&mut self) {
        let _ = self.audio_context.close();
        self.worker.terminate();
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

        // If unmuting, worklet will be lazily initialized on first PCM path
    }
}
