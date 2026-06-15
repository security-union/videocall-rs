use js_sys::Promise;
use std::cell::RefCell;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextOptions, AudioWorkletNode, GainNode};

thread_local! {
    static SHARED: RefCell<Option<Shared>> = const { RefCell::new(None) };

    /// The desired output sink id, independent of whether the shared
    /// `AudioContext` exists yet (issue #1295). A pre-join speaker selection is
    /// applied via `update_speaker_device` BEFORE the first remote audio decoder
    /// lazily creates the context (the decoder is built with sink `None`, see
    /// `peer_decode_manager::new_decoders`), so the selection would otherwise be
    /// dropped. We stash it here so `get_or_init` can re-apply it the moment the
    /// context is first created. This `thread_local` lives for the page lifetime
    /// (the context is never torn down), so the desired sink also survives
    /// reconnection, which reuses the same context.
    static DESIRED_SINK_ID: RefCell<Option<String>> = const { RefCell::new(None) };
}

struct Shared {
    context: AudioContext,
    master_gain: GainNode,
    worklet_registered: bool,
    current_device_id: Option<String>,
    register_promise: Option<Promise>,
}

pub struct SharedAudioContext;

/// Apply an output sink id to the shared `AudioContext`, logging the outcome
/// instead of swallowing it (issue #1295).
///
/// Graceful degradation: `AudioContext.setSinkId` is not universally supported
/// (historically Firefox and Safari lack it). When it is absent we log at debug
/// and return without touching the context, so audio still plays on the default
/// device — sink selection being unavailable must never break playback.
///
/// When supported, the switch is asynchronous (returns a `Promise`); we spawn a
/// task that awaits it and `warn!`s on rejection so a failed switch is
/// diagnosable rather than silent. This mirrors the established pattern in
/// `decode::config::configure_audio_context`. We do not retry: a single re-apply
/// already happens on the next `update_speaker_device`/`get_or_init`, and an
/// unbounded loop could fight an in-flight in-call switch.
fn apply_sink_id(ctx: &AudioContext, device_id: &str) {
    if !js_sys::Reflect::has(ctx, &JsValue::from_str("setSinkId")).unwrap_or(false) {
        log::debug!(
            "AudioContext.setSinkId unsupported; keeping default output device (requested {device_id})"
        );
        return;
    }
    let p = ctx.set_sink_id_with_str(device_id);
    let id = device_id.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        match JsFuture::from(p).await {
            Ok(_) => log::debug!("Applied audio output sink id: {id}"),
            Err(e) => log::warn!("Failed to set audio output sink id {id}: {e:?}"),
        }
    });
}

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

            // Re-apply the desired sink at context creation (issue #1295). The
            // caller's `device_id` is almost always `None` here (the lazy decoder
            // path passes no sink), so fall back to the sink stashed by an earlier
            // `update_speaker_device` — that is the pre-join speaker selection that
            // ran before this context existed. Without this, that selection is
            // silently lost and audio comes out of the default device.
            let effective_id = device_id
                .clone()
                .or_else(|| DESIRED_SINK_ID.with(|c| c.borrow().clone()));
            if let Some(id) = effective_id.as_ref() {
                apply_sink_id(&ctx, id);
            }

            SHARED.with(|cell| {
                *cell.borrow_mut() = Some(Shared {
                    context: ctx.clone(),
                    master_gain,
                    worklet_registered: false,
                    // Record the sink we actually applied so a later
                    // `update_speaker_device` with the same id is a no-op and an
                    // in-call switch to a different id is detected as a change.
                    current_device_id: effective_id,
                    register_promise: None,
                });
            });

            return Ok(ctx);
        }

        // Existing context: if a new device id is provided and differs, update
        // the sink on the AudioContext. (The desired-sink store is left to
        // `update_speaker_device` — the explicit speaker-change entry point — so
        // the lazy decoder path passing `None` here never clears it.)
        if let Some(new_id) = device_id.as_ref() {
            SHARED.with(|cell| {
                if let Some(shared) = cell.borrow_mut().as_mut() {
                    if shared.current_device_id.as_ref() != Some(new_id) {
                        apply_sink_id(&shared.context, new_id);
                        shared.current_device_id = Some(new_id.clone());
                    }
                }
            });
        }

        Ok(current.expect("shared audio context should be initialized"))
    }

    pub fn update_speaker_device(device_id: Option<String>) -> Result<(), JsValue> {
        // Always record the desired sink, even when the shared `AudioContext`
        // does not exist yet (issue #1295). This is the pre-join case: the
        // speaker chosen on the lobby screen is selected before any remote audio
        // decoder has lazily created the context, so there is nothing to apply
        // to here. Stashing it lets `get_or_init` re-apply it the instant the
        // context is created — the previous silent no-op is what dropped the
        // pre-join selection.
        DESIRED_SINK_ID.with(|c| *c.borrow_mut() = device_id.clone());

        SHARED.with(|cell| {
            if let Some(shared) = cell.borrow_mut().as_mut() {
                if shared.current_device_id != device_id {
                    if let Some(id) = device_id.as_ref() {
                        apply_sink_id(&shared.context, id);
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

        log::debug!("Created peer playback nodes for {peer_id}");
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
