use crate::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::decode::config::configure_audio_context;
use crate::decode::AudioPeerDecoderTrait;
use crate::decode::DecodeStatus;
use crate::utils::is_ios; // maybe unused
use js_sys::{Float32Array, Object};
use log::error;
use serde::{Deserialize, Serialize};
use serde_wasm_bindgen;
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AnalyserNode, AudioBufferSourceNode, AudioContext, AudioData, AudioDataInit,
    CanvasRenderingContext2d, HtmlCanvasElement, MediaStreamTrackGenerator,
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
}

/// Audio decoder that sends packets to a NetEq worker and plays the returned PCM via WebAudio.
#[derive(Debug)]
pub struct NetEqAudioPeerDecoder {
    worker: Worker,
    audio_context: AudioContext,
    _generator: MediaStreamTrackGenerator,
    analyser: AnalyserNode,
    decoded: bool,
    _on_message_closure: Closure<dyn FnMut(MessageEvent)>, // Keep closure alive
    _draw_closure: Closure<dyn FnMut()>,                   // Keep rAF closure alive
}

impl NetEqAudioPeerDecoder {
    pub fn new(speaker_device_id: Option<String>) -> Result<Self, JsValue> {
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

        // Create an AnalyserNode for debug visualisation.
        let analyser = audio_context.create_analyser()?;
        analyser.set_fft_size(2048);

        // Create (or reuse) a debug canvas on the page.
        let canvas: HtmlCanvasElement =
            if let Some(elem) = document.get_element_by_id("audio-debug") {
                elem.dyn_into::<HtmlCanvasElement>()?
            } else {
                let canvas: HtmlCanvasElement = document
                    .create_element("canvas")?
                    .dyn_into::<HtmlCanvasElement>()?;
                canvas.set_id("audio-debug");
                canvas.set_width(512);
                canvas.set_height(100);
                canvas.style().set_property("position", "fixed")?;
                canvas.style().set_property("bottom", "0")?;
                canvas.style().set_property("left", "0")?;
                canvas.style().set_property("z-index", "10000")?;
                document.body().unwrap().append_child(&canvas)?;
                canvas
            };
        let ctx: CanvasRenderingContext2d = canvas
            .get_context("2d")?
            .unwrap()
            .dyn_into::<CanvasRenderingContext2d>()?;

        // Animation frame loop to draw waveform.
        let analyser_clone = analyser.clone();
        let draw_closure = Closure::wrap(Box::new(move || {
            let buffer_len = analyser_clone.frequency_bin_count();
            let mut data = vec![0u8; buffer_len as usize];
            analyser_clone.get_byte_time_domain_data(&mut data);
            ctx.set_fill_style(&JsValue::from_str("black"));
            ctx.fill_rect(0.0, 0.0, 512.0, 100.0);
            ctx.set_stroke_style(&JsValue::from_str("lime"));
            ctx.begin_path();
            for (i, v) in data.iter().enumerate() {
                let x = i as f64 / buffer_len as f64 * 512.0;
                let y = (*v as f64 / 255.0) * 100.0;
                if i == 0 {
                    ctx.move_to(x, y);
                } else {
                    ctx.line_to(x, y);
                }
            }
            ctx.stroke();
        }) as Box<dyn FnMut()>);
        // Draw at ~30 fps
        window.set_interval_with_callback_and_timeout_and_arguments_0(
            draw_closure.as_ref().unchecked_ref(),
            33,
        )?;

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
            analyser,
            decoded: false,
            _on_message_closure: on_message_closure,
            _draw_closure: draw_closure,
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

                // Debug: log what we are sending to the worker so we can confirm the
                // metadata and payload length look correct and that the `Insert`
                // messages are actually being generated from the decoder.
                #[cfg(debug_assertions)]
                {
                    use wasm_bindgen::JsValue;
                    // web_sys::console::log_3(
                    //     &JsValue::from_str("[neteq-audio-decoder] Insert:"),
                    //     &JsValue::from_f64(audio_meta.sequence as f64),
                    //     &JsValue::from_f64(packet.data.len() as f64),
                    // );
                }

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
}
