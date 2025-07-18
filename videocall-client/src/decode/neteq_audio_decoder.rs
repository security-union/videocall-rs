use crate::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::decode::{AudioPeerDecoderTrait, DecodeStatus};
use js_sys::Float32Array;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_wasm_bindgen;
use std::cell::RefCell;
use std::rc::Rc;
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
}

/// Audio decoder that sends packets to a NetEq worker and plays the returned PCM via WebAudio.
#[derive(Debug)]
pub struct NetEqAudioPeerDecoder {
    worker: Worker,
    audio_context: AudioContext,
    decoded: bool,
    _on_message_closure: Closure<dyn FnMut(MessageEvent)>, // Keep closure alive
    peer_id: String, // Track which peer this decoder belongs to
    _pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>, // AudioWorklet PCM player
}

impl NetEqAudioPeerDecoder {
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
        // Create AudioContext with ENFORCED 48kHz for Safari (critical!)
        let options = AudioContextOptions::new();
        options.set_sample_rate(48000.0); // Explicitly force 48kHz
        let audio_context = AudioContext::new_with_context_options(&options)?;

        // CRITICAL: Verify actual sample rate Safari is using
        let actual_sample_rate = audio_context.sample_rate();
        web_sys::console::log_2(
            &"Safari AudioContext sample rate:".into(),
            &JsValue::from_f64(actual_sample_rate as f64),
        );

        if (actual_sample_rate - 48000.0).abs() > 1.0 {
            web_sys::console::warn_2(
                &"⚠️ Safari AudioContext sample rate mismatch! Expected 48000, got:".into(),
                &JsValue::from_f64(actual_sample_rate as f64),
            );
        }

        // Load the PCM player worklet
        JsFuture::from(
            audio_context
                .audio_worklet()?
                .add_module("/pcmPlayerWorker.js")?,
        )
        .await?;

        // Create the PCM player worklet node
        let pcm_player = AudioWorkletNode::new(&audio_context, "pcm-player")?;

        // Connect worklet to destination
        pcm_player.connect_with_audio_node(&audio_context.destination())?;

        // Configure the worklet with explicit 48kHz
        let config_message = js_sys::Object::new();
        js_sys::Reflect::set(&config_message, &"command".into(), &"configure".into())?;
        js_sys::Reflect::set(
            &config_message,
            &"sampleRate".into(),
            &JsValue::from(48000.0),
        )?; // Force 48kHz
        js_sys::Reflect::set(
            &config_message,
            &"channels".into(),
            &JsValue::from(AUDIO_CHANNELS as f32),
        )?;
        pcm_player.port()?.post_message(&config_message)?;

        web_sys::console::log_1(&"Safari: Configured PCM worklet for 48kHz playback".into());

        // Set sink device if specified (Safari supports setSinkId)
        if let Some(device_id) = speaker_device_id {
            if js_sys::Reflect::has(&audio_context, &JsValue::from_str("setSinkId"))
                .unwrap_or(false)
            {
                let promise = audio_context.set_sink_id_with_str(&device_id);
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(e) = JsFuture::from(promise).await {
                        web_sys::console::warn_1(
                            &format!("Safari: Failed to set audio output device: {e:?}").into(),
                        );
                    } else {
                        web_sys::console::log_1(
                            &"Safari: Successfully set audio output device".into(),
                        );
                    }
                });
            }
        }

        Ok((audio_context, pcm_player))
    }

    /// Handle PCM audio data from NetEq worker
    fn handle_pcm_data(
        pcm: Float32Array,
        pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>,
        audio_context: &AudioContext,
    ) {
        // Ensure AudioContext is running
        if let Err(e) = audio_context.resume() {
            web_sys::console::warn_1(
                &format!("[neteq-audio-decoder] AudioContext resume error: {e:?}").into(),
            );
        }

        let pcm_player_clone = pcm_player.clone();
        wasm_bindgen_futures::spawn_local(async move {
            Self::ensure_worklet_initialized(&pcm_player_clone).await;

            if let Some(ref worklet) = *pcm_player_clone.borrow() {
                Self::send_pcm_to_safari_worklet(worklet, &pcm);
            }
        });
    }

    /// Ensure AudioWorklet is initialized (lazy initialization)
    async fn ensure_worklet_initialized(pcm_player: &Rc<RefCell<Option<AudioWorkletNode>>>) {
        if pcm_player.borrow().is_some() {
            return;
        }

        web_sys::console::log_1(&"Initializing AudioWorklet for PCM playback".into());

        match Self::create_safari_audio_context(None).await {
            Ok((_, worklet)) => {
                *pcm_player.borrow_mut() = Some(worklet);
                web_sys::console::log_1(&"AudioWorklet initialized successfully".into());
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
        let _ = global_sender().send(DiagEvent {
            subsystem: "neteq",
            stream_id: Some(peer_id.to_string()),
            ts_ms: now_ms(),
            metrics: vec![metric!("stats_json", json_str.to_string())],
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
            let _ = global_sender().send(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(peer_id.to_string()),
                ts_ms: now_ms(),
                metrics: vec![metric!("jitter_buffer_delay_ms", jitter)],
            });
        }

        if let Some(target) = lifetime
            .get("jitter_buffer_target_delay_ms")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().send(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(peer_id.to_string()),
                ts_ms: now_ms(),
                metrics: vec![metric!("jitter_buffer_target_delay_ms", target)],
            });
        }
    }

    /// Emit buffer size metrics
    fn emit_buffer_metrics(parsed: &Value, peer_id: &str) {
        let network = match parsed.get("network") {
            Some(n) => n,
            None => return,
        };

        if let Some(buf) = network
            .get("current_buffer_size_ms")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().send(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(peer_id.to_string()),
                ts_ms: now_ms(),
                metrics: vec![metric!("current_buffer_size_ms", buf)],
            });
        }
    }

    /// Create message handler for NetEq worker
    fn create_message_handler(
        pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>,
        audio_context: AudioContext,
        peer_id: String,
    ) -> Closure<dyn FnMut(MessageEvent)> {
        Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();

            if data.is_instance_of::<Float32Array>() {
                let pcm = Float32Array::from(data);
                Self::handle_pcm_data(pcm, pcm_player.clone(), &audio_context);
            } else if data.is_object() {
                Self::handle_stats_message(&data, &peer_id);
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

        let pcm_player_ref = Rc::new(RefCell::new(None::<AudioWorkletNode>));

        // Set up worker message handling
        let on_message_closure = Self::create_message_handler(
            pcm_player_ref.clone(),
            audio_context.clone(),
            peer_id.clone(),
        );

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

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

        web_sys::console::log_1(
            &format!("NetEq audio decoder initialized for peer {peer_id} (muted: {initial_muted})")
                .into(),
        );

        // Create decoder with explicit mute state
        let mut decoder = Self {
            worker,
            audio_context,
            decoded: false,
            _on_message_closure: on_message_closure,
            peer_id,
            _pcm_player: pcm_player_ref,
        };

        // Set the initial mute state explicitly
        decoder.set_muted(initial_muted);
        web_sys::console::log_1(
            &format!(
                "✅ NetEq decoder initialized for peer {} with muted: {}",
                decoder.peer_id, initial_muted
            )
            .into(),
        );

        Ok(Box::new(decoder))
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
                // Normal path – send the packet to the NetEq worker.
                let insert = WorkerMsg::Insert {
                    seq: audio_meta.sequence as u16,
                    timestamp: packet.timestamp as u32,
                    payload: packet.data.clone(),
                };

                // Any serialisation or postMessage error will simply be logged. We don't want it
                // to bubble up and force a complete decoder reset, which leads to the video
                // worker being recreated ("Terminating worker" loops observed in the console).
                if let Err(e) =
                    serde_wasm_bindgen::to_value(&insert).map(|msg| self.worker.post_message(&msg))
                {
                    log::error!("Failed to dispatch NetEq insert message: {e:?}");
                    // Still report success so the caller doesn't reset the whole peer.
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
        // Send flush message to NetEq worker to clear the buffer
        let flush_msg = WorkerMsg::Flush;
        if let Err(e) =
            serde_wasm_bindgen::to_value(&flush_msg).map(|msg| self.worker.post_message(&msg))
        {
            log::error!("Failed to dispatch NetEq flush message: {e:?}");
        } else {
            log::debug!(
                "Sent flush message to NetEq worker for peer {}",
                self.peer_id
            );
        }
    }

    fn set_muted(&mut self, muted: bool) {
        // Send mute message to NetEq worker to stop/start audio production
        let mute_msg = WorkerMsg::Mute { muted };

        // Use console.log for immediate visibility in browser console
        web_sys::console::log_2(
            &format!(
                "[MUTE DEBUG] Sending mute message for peer {}",
                self.peer_id
            )
            .into(),
            &JsValue::from_bool(muted),
        );

        if let Err(e) =
            serde_wasm_bindgen::to_value(&mute_msg).map(|msg| self.worker.post_message(&msg))
        {
            log::error!("Failed to dispatch NetEq mute message: {e:?}");
            web_sys::console::error_1(&format!("Failed to send mute message: {e:?}").into());
        } else {
            log::debug!(
                "Sent mute message to NetEq worker for peer {} (muted: {})",
                self.peer_id,
                muted
            );
            web_sys::console::log_1(
                &format!(
                    "✅ Mute message sent successfully for peer {} (muted: {})",
                    self.peer_id, muted
                )
                .into(),
            );
        }
    }
}
