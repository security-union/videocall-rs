use crate::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::decode::config::configure_audio_context;
use crate::decode::DecodeStatus;
use js_sys::{Float32Array, Object};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_wasm_bindgen;
use std::sync::Arc;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioData, AudioDataInit, MediaStreamTrackGenerator,
    MediaStreamTrackGeneratorInit, MessageEvent, Worker,
};

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
    _generator: MediaStreamTrackGenerator,
    decoded: bool,
    _on_message_closure: Closure<dyn FnMut(MessageEvent)>, // Keep closure alive
    peer_id: String, // Track which peer this decoder belongs to
}

impl NetEqAudioPeerDecoder {
    pub fn new(speaker_device_id: Option<String>, peer_id: String) -> Result<Self, JsValue> {
        // Locate worker URL from <link id="neteq-worker" ...>
        let window = web_sys::window().expect("no window");
        let document = window.document().expect("no document");
        let worker_url = document
            .get_element_by_id("neteq-worker")
            .expect("neteq-worker link tag not found")
            .get_attribute("href")
            .expect("link tag has no href");

        let worker = Worker::new(&worker_url)?;

        // Use shared helper so routing/sink handling matches the standard decoder path
        let generator =
            MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new("audio"))?;
        let audio_context = configure_audio_context(&generator, speaker_device_id.clone())
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        // Try setSinkId if provided
        if let Some(device_id) = speaker_device_id {
            if js_sys::Reflect::has(&audio_context, &JsValue::from_str("setSinkId"))
                .unwrap_or(false)
            {
                // setSinkId returns a Promise.
                let promise = audio_context.set_sink_id_with_str(&device_id);
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = JsFuture::from(promise).await;
                });
            }
        }

        // Set up message handler BEFORE sending the Init command so the worker definitely
        // has its listener ready when the first message arrives.
        let audio_ctx_clone = audio_context.clone();
        let generator_for_cb = generator.clone();
        let peer_id_clone = peer_id.clone();
        let on_message_closure = Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();
            if data.is_instance_of::<Float32Array>() {
                // Ensure AudioContext is running (browser may start it suspended until user gesture).
                if let Err(e) = audio_ctx_clone.resume() {
                    web_sys::console::warn_1(
                        &format!("[neteq-audio-decoder] AudioContext resume error: {:?}", e).into(),
                    );
                }

                let pcm = Float32Array::from(data);
                let length = pcm.length() as usize;
                let frames = length as u32; // mono

                let adi = AudioDataInit::new(
                    &pcm.unchecked_into::<Object>(),
                    web_sys::AudioSampleFormat::F32,
                    AUDIO_CHANNELS,
                    frames,
                    AUDIO_SAMPLE_RATE as f32,
                    audio_ctx_clone.current_time() * 1e6,
                );

                if let Ok(audio_data) = AudioData::new(&adi) {
                    let writable = generator_for_cb.writable();
                    if !writable.locked() {
                        if let Ok(writer) = writable.get_writer() {
                            wasm_bindgen_futures::spawn_local(async move {
                                if JsFuture::from(writer.ready()).await.is_ok() {
                                    let _ =
                                        JsFuture::from(writer.write_with_chunk(&audio_data)).await;
                                }
                                writer.release_lock();
                            });
                        }
                    }
                } else {
                    web_sys::console::warn_1(
                        &"[neteq-audio-decoder] failed to create AudioData".into(),
                    );
                }
            } else if data.is_object() {
                let obj: js_sys::Object = data.clone().unchecked_into();
                let cmd = js_sys::Reflect::get(&obj, &JsValue::from_str("cmd"))
                    .unwrap_or(JsValue::UNDEFINED);
                if cmd.as_string().as_deref() == Some("stats") {
                    if let Ok(stats_js) = js_sys::Reflect::get(&obj, &JsValue::from_str("stats")) {
                        // Convert to JSON string for quick display
                        if let Ok(stats_json) = js_sys::JSON::stringify(&stats_js) {
                            if let Some(json_str) = stats_json.as_string() {
                                // Always emit the raw JSON block for debugging/UI display.

                                let _ = global_sender().send(DiagEvent {
                                    subsystem: "neteq",
                                    stream_id: Some(peer_id_clone.clone()),
                                    ts_ms: now_ms(),
                                    metrics: vec![metric!("stats_json", json_str.clone())],
                                });

                                if let Ok(parsed) = serde_json::from_str::<Value>(&json_str) {
                                    // Extract useful numbers
                                    if let Some(lifetime) = parsed.get("lifetime") {
                                        if let Some(jitter) = lifetime
                                            .get("jitter_buffer_delay_ms")
                                            .and_then(|v| v.as_u64())
                                        {
                                            let _ = global_sender().send(DiagEvent {
                                                subsystem: "neteq",
                                                stream_id: Some(peer_id_clone.clone()),
                                                ts_ms: now_ms(),
                                                metrics: vec![metric!(
                                                    "jitter_buffer_delay_ms",
                                                    jitter
                                                )],
                                            });
                                        }
                                        if let Some(target) = parsed
                                            .get("jitter_buffer_target_delay_ms")
                                            .and_then(|v| v.as_u64())
                                        {
                                            let _ = global_sender().send(DiagEvent {
                                                subsystem: "neteq",
                                                stream_id: Some(peer_id_clone.clone()),
                                                ts_ms: now_ms(),
                                                metrics: vec![metric!(
                                                    "jitter_buffer_target_delay_ms",
                                                    target
                                                )],
                                            });
                                        }
                                    }
                                    if let Some(network) = parsed.get("network") {
                                        if let Some(buf) = network
                                            .get("current_buffer_size_ms")
                                            .and_then(|v| v.as_u64())
                                        {
                                            let _ = global_sender().send(DiagEvent {
                                                subsystem: "neteq",
                                                stream_id: Some(peer_id_clone.clone()),
                                                ts_ms: now_ms(),
                                                metrics: vec![metric!(
                                                    "current_buffer_size_ms",
                                                    buf
                                                )],
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }) as Box<dyn FnMut(_)>);

        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        // Now that the message listener is wired up, dispatch the Init message with a short
        // `setTimeout` so we give the worker JS runtime a moment to finish evaluating its
        // top-level code (and thus have its own onmessage installed).
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
        // 10 ms is plenty; even 0 would usually work but this is extra safe.
        web_sys::window()
            .expect("no window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                send_cb.as_ref().unchecked_ref(),
                10,
            )?;
        // Leak the closure (single-shot) so it lives until the timeout fires.
        send_cb.forget();

        Ok(Self {
            worker,
            audio_context,
            _generator: generator,
            decoded: false,
            _on_message_closure: on_message_closure,
            peer_id,
        })
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
                    log::error!("Failed to dispatch NetEq insert message: {:?}", e);
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
            log::error!("Failed to dispatch NetEq flush message: {:?}", e);
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
            log::error!("Failed to dispatch NetEq mute message: {:?}", e);
            web_sys::console::error_1(&format!("Failed to send mute message: {:?}", e).into());
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
