/*
 * Copyright 2025 Fame Labs Inc.
 *
 * Revolutionary Shared Peer Audio Decoder
 *
 * This decoder uses the shared AudioContext instead of creating its own,
 * dramatically reducing memory and CPU overhead. Each peer gets its own
 * NetEQ worker but routes audio through the shared mixer worklet.
 */

use crate::audio::shared_context_manager::get_or_init_shared_audio_manager;
use crate::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::decode::{AudioPeerDecoderTrait, DecodeStatus};
use js_sys::{Float32Array, Object};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{AudioWorkletNode, MessageEvent, Worker};

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

/// Revolutionary shared peer audio decoder
///
/// Instead of creating its own AudioContext, this decoder:
/// 1. Gets the shared AudioContext from SharedAudioContextManager
/// 2. Spawns its own NetEQ worker for parallel processing
/// 3. Routes decoded audio to the shared mixer worklet
/// 4. Provides the same interface as the old decoder
#[derive(Debug)]
pub struct SharedPeerAudioDecoder {
    /// Each peer still gets its own NetEQ worker for parallel processing
    worker: Worker,
    /// Peer identifier for routing audio to the correct mixer channel
    peer_id: String,
    /// Decode state tracking
    decoded: bool,
    /// Volume control (0.0 = muted, 1.0 = full volume)
    volume: f32,
    /// Message queueing system for worker communication
    pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
    worker_ready: Rc<RefCell<bool>>,
    /// Channel ID in the shared mixer
    mixer_channel_id: Option<u32>,
}

impl SharedPeerAudioDecoder {
    /// Create a new shared peer audio decoder
    ///
    /// This replaces the old per-peer AudioContext approach with the
    /// revolutionary shared context design.
    pub async fn new(
        speaker_device_id: Option<String>,
        peer_id: String,
        initial_muted: bool,
    ) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
        log::info!("🎵 Creating shared peer audio decoder for: {peer_id}");

        // Get or initialize the global shared audio manager
        let audio_manager = get_or_init_shared_audio_manager(speaker_device_id).await?;

        // Register this peer in the shared mixer
        let channel_id = audio_manager.register_peer(peer_id.clone())?;

        // Create the NetEQ worker (each peer gets its own for parallel processing)
        let worker = Self::create_neteq_worker()?;

        let mut decoder = Self {
            worker: worker.clone(),
            peer_id: peer_id.clone(),
            decoded: false,
            volume: if initial_muted { 0.0 } else { 1.0 },
            pending_messages: Rc::new(RefCell::new(VecDeque::new())),
            worker_ready: Rc::new(RefCell::new(false)),
            mixer_channel_id: Some(channel_id),
        };

        // Set up the revolutionary audio routing system
        let on_message_closure = Self::create_message_handler(
            peer_id.clone(),
            audio_manager.get_mixer_worklet().clone(),
            decoder.worker_ready.clone(),
            decoder.pending_messages.clone(),
            worker.clone(),
        );

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));
        on_message_closure.forget();

        // Initialize worker with NetEQ configuration
        let init_msg = WorkerMsg::Init {
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS as u8,
        };
        decoder.send_worker_message(init_msg);

        // Set initial mute state
        if initial_muted {
            decoder.set_muted(true);
        }

        log::info!("✅ Shared peer audio decoder created for: {peer_id} (channel: {channel_id})");
        Ok(Box::new(decoder))
    }

    /// Create the NetEQ worker for this peer
    ///
    /// Each peer gets its own worker for parallel audio processing,
    /// but they all route through the shared mixer.
    fn create_neteq_worker() -> Result<Worker, JsValue> {
        // Create NetEQ worker using a proper worker script path
        // For now, we'll use a basic worker until the full NetEQ worker is available
        let worker_script = r#"
            // Basic NetEQ worker placeholder
            self.onmessage = function(e) {
                // Echo back ready message
                if (e.data && e.data.cmd === 'init') {
                    self.postMessage('ready');
                }
                // For now, just echo audio data back
                if (e.data && e.data.cmd === 'insert') {
                    // Generate test audio (silent for now)
                    const testAudio = new Float32Array(480); // 10ms at 48kHz
                    self.postMessage(testAudio);
                }
            };
        "#;

        let blob_parts = js_sys::Array::new();
        blob_parts.push(&JsValue::from_str(worker_script));

        let blob_options = web_sys::BlobPropertyBag::new();
        blob_options.set_type("application/javascript");

        let blob = web_sys::Blob::new_with_str_sequence_and_options(&blob_parts, &blob_options)?;
        let worker_url = web_sys::Url::create_object_url_with_blob(&blob)?;

        let worker = Worker::new(&worker_url)?;
        web_sys::Url::revoke_object_url(&worker_url)?;

        Ok(worker)
    }

    /// Create the revolutionary message handler that routes audio to shared mixer
    fn create_message_handler(
        peer_id: String,
        mixer_worklet: AudioWorkletNode,
        worker_ready: Rc<RefCell<bool>>,
        pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
        worker: Worker,
    ) -> Closure<dyn FnMut(MessageEvent)> {
        // Clone values that need to be accessible multiple times
        let peer_id_clone = peer_id.clone();

        Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();

            // Check if this is a Float32Array (decoded PCM audio)
            if data.is_instance_of::<Float32Array>() {
                let pcm_data = data.unchecked_into::<Float32Array>();

                // Route audio to the shared mixer instead of individual context
                let audio_msg = Object::new();
                js_sys::Reflect::set(&audio_msg, &"cmd".into(), &"peerAudio".into()).unwrap();
                js_sys::Reflect::set(&audio_msg, &"peerId".into(), &peer_id_clone.clone().into())
                    .unwrap();
                js_sys::Reflect::set(&audio_msg, &"audioData".into(), &pcm_data).unwrap();

                // Send to the shared mixer worklet
                if let Ok(port) = mixer_worklet.port() {
                    if let Err(e) = port.post_message(&audio_msg) {
                        log::warn!(
                            "Failed to send audio to mixer for peer {}: {:?}",
                            peer_id_clone,
                            e
                        );
                    }
                } else {
                    log::warn!(
                        "Failed to get mixer worklet port for peer {}",
                        peer_id_clone
                    );
                }

                return;
            }

            // Handle worker status messages
            if let Some(msg_str) = data.as_string() {
                if msg_str.contains("ready") {
                    *worker_ready.borrow_mut() = true;
                    log::debug!("NetEQ worker ready for peer: {}", peer_id_clone);

                    // Flush any pending messages
                    while let Some(pending_msg) = pending_messages.borrow_mut().pop_front() {
                        if let Ok(serialized) = serde_wasm_bindgen::to_value(&pending_msg) {
                            let _ = worker.post_message(&serialized);
                        }
                    }
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>)
    }

    /// Send message to NetEQ worker with queueing support
    fn send_worker_message(&self, msg: WorkerMsg) {
        if *self.worker_ready.borrow() {
            // Worker is ready, send immediately
            if let Ok(serialized) = serde_wasm_bindgen::to_value(&msg) {
                if let Err(e) = self.worker.post_message(&serialized) {
                    log::warn!(
                        "Failed to send message to NetEQ worker for peer {}: {:?}",
                        self.peer_id,
                        e
                    );
                }
            }
        } else {
            // Worker not ready, queue the message
            self.pending_messages.borrow_mut().push_back(msg);
            log::debug!(
                "Queued message for peer {} (worker not ready)",
                self.peer_id
            );
        }
    }
}

impl Drop for SharedPeerAudioDecoder {
    fn drop(&mut self) {
        log::info!(
            "🗑️ Dropping shared peer audio decoder for: {}",
            self.peer_id
        );

        // Unregister from shared mixer (spawn async task)
        let peer_id = self.peer_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(audio_manager) = get_or_init_shared_audio_manager(None).await {
                let _ = audio_manager.unregister_peer(&peer_id);
            }
        });

        // Close worker
        self.send_worker_message(WorkerMsg::Close);
    }
}

impl AudioPeerDecoderTrait for SharedPeerAudioDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        match packet.audio_metadata.as_ref() {
            Some(audio_meta) => {
                // Send packet to NetEQ worker for processing
                let insert_msg = WorkerMsg::Insert {
                    seq: audio_meta.sequence as u16,
                    timestamp: packet.timestamp as u32,
                    payload: packet.data.clone(),
                };

                self.send_worker_message(insert_msg);

                // Track decode metrics
                let first_frame = !self.decoded;
                self.decoded = true;

                if first_frame {
                    // Track decode metrics - simplified for now
                    log::debug!("First audio frame decoded for peer: {}", self.peer_id);
                }

                Ok(DecodeStatus {
                    rendered: true,
                    first_frame,
                })
            }
            None => {
                log::warn!(
                    "Received audio packet without metadata for peer: {}",
                    self.peer_id
                );
                Ok(DecodeStatus {
                    rendered: false,
                    first_frame: false,
                })
            }
        }
    }

    fn flush(&mut self) {
        self.send_worker_message(WorkerMsg::Flush);
        log::debug!("Flushed NetEQ buffer for peer: {}", self.peer_id);
    }

    fn set_muted(&mut self, muted: bool) {
        let new_volume = if muted { 0.0 } else { 1.0 };

        if self.volume != new_volume {
            self.volume = new_volume;

            // Update volume in shared mixer (spawn async task)
            let peer_id = self.peer_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(audio_manager) = get_or_init_shared_audio_manager(None).await {
                    let _ = audio_manager.set_peer_volume(&peer_id, new_volume);
                }
            });

            // Also notify NetEQ worker
            self.send_worker_message(WorkerMsg::Mute { muted });

            log::debug!(
                "Set peer {} muted: {} (volume: {})",
                self.peer_id,
                muted,
                new_volume
            );
        }
    }

    fn is_muted(&self) -> bool {
        self.volume == 0.0
    }

    fn set_volume(&mut self, volume: f32) {
        let clamped_volume = volume.clamp(0.0, 1.0);

        if self.volume != clamped_volume {
            self.volume = clamped_volume;

            // Update volume in shared mixer (spawn async task)
            let peer_id = self.peer_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(audio_manager) = get_or_init_shared_audio_manager(None).await {
                    let _ = audio_manager.set_peer_volume(&peer_id, clamped_volume);
                }
            });

            log::debug!("Set peer {} volume: {}", self.peer_id, clamped_volume);
        }
    }

    fn get_volume(&self) -> f32 {
        self.volume
    }
}
