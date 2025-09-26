use js_sys::Promise;
use std::cell::RefCell;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextOptions, AudioWorkletNode, GainNode};

thread_local! {
    static SHARED: RefCell<Option<Shared>> = const { RefCell::new(None) };
}

struct Shared {
    context: AudioContext,
    master_gain: GainNode,
    worklet_registered: bool,
    current_device_id: Option<String>,
    register_promise: Option<Promise>,
}

pub struct SharedAudioContext;

impl SharedAudioContext {
    pub fn get_or_init(device_id: Option<String>) -> Result<AudioContext, JsValue> {
        let mut need_create = false;
        let mut current: Option<AudioContext> = None;

        SHARED.with(|cell| {
            if let Some(shared) = cell.borrow().as_ref() {
                current = Some(shared.context.clone());
            } else {
                need_create = true;
            }
        });

        if need_create {
            let options = AudioContextOptions::new();
            options.set_sample_rate(48000.0);

            let ctx = AudioContext::new_with_context_options(&options)?;

            let master_gain = ctx.create_gain()?;
            master_gain.gain().set_value(1.0);
            master_gain.connect_with_audio_node(&ctx.destination())?;

            // Apply sink id on the AudioContext if supported
            if let Some(id) = device_id.as_ref() {
                if js_sys::Reflect::has(&ctx, &JsValue::from_str("setSinkId")).unwrap_or(false) {
                    let p = ctx.set_sink_id_with_str(id);
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = JsFuture::from(p).await;
                    });
                }
            }

            SHARED.with(|cell| {
                *cell.borrow_mut() = Some(Shared {
                    context: ctx.clone(),
                    master_gain,
                    worklet_registered: false,
                    current_device_id: device_id.clone(),
                    register_promise: None,
                });
            });

            return Ok(ctx);
        }

        // Existing context: if a new device id is provided and differs, update sink on AudioContext
        if let Some(new_id) = device_id.as_ref() {
            SHARED.with(|cell| {
                if let Some(shared) = cell.borrow_mut().as_mut() {
                    if shared.current_device_id.as_ref() != Some(new_id) {
                        if js_sys::Reflect::has(&shared.context, &JsValue::from_str("setSinkId"))
                            .unwrap_or(false)
                        {
                            let p = shared.context.set_sink_id_with_str(new_id);
                            wasm_bindgen_futures::spawn_local(async move {
                                let _ = JsFuture::from(p).await;
                            });
                        }
                        shared.current_device_id = Some(new_id.clone());
                    }
                }
            });
        }

        Ok(current.expect("shared audio context should be initialized"))
    }

    pub fn update_speaker_device(device_id: Option<String>) -> Result<(), JsValue> {
        SHARED.with(|cell| {
            if let Some(shared) = cell.borrow_mut().as_mut() {
                if shared.current_device_id != device_id {
                    if let Some(id) = device_id.as_ref() {
                        if js_sys::Reflect::has(&shared.context, &JsValue::from_str("setSinkId"))
                            .unwrap_or(false)
                        {
                            let p = shared.context.set_sink_id_with_str(id);
                            wasm_bindgen_futures::spawn_local(async move {
                                let _ = JsFuture::from(p).await;
                            });
                        }
                    }
                    shared.current_device_id = device_id.clone();
                }
            }
        });
        Ok(())
    }

    pub fn ensure_pcm_worklet(worklet_js: &str) {
        wasm_bindgen_futures::spawn_local({
            let js = worklet_js.to_string();
            async move {
                let _ = Self::ensure_pcm_worklet_ready(&js).await;
            }
        });
    }

    pub async fn ensure_pcm_worklet_ready(worklet_js: &str) -> Result<(), JsValue> {
        // Fast path: already registered
        let already_registered = SHARED.with(|cell| {
            cell.borrow()
                .as_ref()
                .map(|s| s.worklet_registered)
                .unwrap_or(false)
        });
        if already_registered {
            return Ok(());
        }

        // If a registration is already in-flight, await it
        if let Some(existing_promise) = SHARED.with(|cell| {
            cell.borrow()
                .as_ref()
                .and_then(|s| s.register_promise.as_ref().cloned())
        }) {
            JsFuture::from(existing_promise).await?;
            return Ok(());
        }

        // Start a new registration
        let ctx = Self::require_context()?;

        let blob_parts = js_sys::Array::new();
        blob_parts.push(&JsValue::from_str(worklet_js));
        let blob_opts = web_sys::BlobPropertyBag::new();
        blob_opts.set_type("application/javascript");
        let blob = web_sys::Blob::new_with_str_sequence_and_options(&blob_parts, &blob_opts)?;

        let url = web_sys::Url::create_object_url_with_blob(&blob)?;
        let worklet = ctx.audio_worklet()?;
        let promise = worklet.add_module(&url)?;

        // Record the in-flight promise so concurrent callers can await it
        SHARED.with(|cell| {
            if let Some(shared) = cell.borrow_mut().as_mut() {
                shared.register_promise = Some(promise.clone());
            }
        });

        // Await registration, then clean up and mark as registered
        let result = JsFuture::from(promise).await;
        // Always try to revoke the URL
        let _ = web_sys::Url::revoke_object_url(&url);
        result?;

        SHARED.with(|cell| {
            if let Some(shared) = cell.borrow_mut().as_mut() {
                shared.worklet_registered = true;
                shared.register_promise = None;
            }
        });

        Ok(())
    }

    pub fn create_peer_playback_nodes(
        peer_id: &str,
    ) -> Result<(AudioWorkletNode, GainNode), JsValue> {
        let ctx = Self::require_context()?;
        let peer_gain = ctx.create_gain()?;
        peer_gain.gain().set_value(1.0);

        let worklet = AudioWorkletNode::new(&ctx, "pcm-player")?;

        SHARED.with(|cell| {
            if let Some(shared) = cell.borrow().as_ref() {
                let _ = worklet.connect_with_audio_node(&peer_gain);
                let _ = peer_gain.connect_with_audio_node(&shared.master_gain);
            }
        });

        // Configure the worklet
        let config = js_sys::Object::new();
        js_sys::Reflect::set(&config, &"command".into(), &"configure".into())?;
        js_sys::Reflect::set(&config, &"sampleRate".into(), &JsValue::from_f64(48000.0))?;
        js_sys::Reflect::set(
            &config,
            &"channels".into(),
            &JsValue::from_f64(crate::constants::AUDIO_CHANNELS as f64),
        )?;
        worklet.port().unwrap().post_message(&config)?;

        log::info!("Created peer playback nodes for {peer_id}");
        Ok((worklet, peer_gain))
    }

    fn require_context() -> Result<AudioContext, JsValue> {
        let mut ctx: Option<AudioContext> = None;
        SHARED.with(|cell| {
            if let Some(shared) = cell.borrow().as_ref() {
                ctx = Some(shared.context.clone());
            }
        });
        ctx.ok_or_else(|| JsValue::from_str("Shared AudioContext not initialized"))
    }
}
