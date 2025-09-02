/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use crate::audio::NetEqPeerSink;
use log::{info, warn};
use std::collections::HashMap;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextOptions, AudioWorkletNode, GainNode};

/// Manages a shared AudioContext with individual PCM worklets per peer
/// for NetEq audio decoding when the neteq_ff feature is enabled
pub struct SharedNetEqAudioManager {
    audio_context: AudioContext,
    current_speaker_device: Option<String>,
    active_peers: HashMap<String, PeerAudioSink>,
    worklet_module_loaded: bool,
}

impl std::fmt::Debug for SharedNetEqAudioManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedNetEqAudioManager")
            .field("current_speaker_device", &self.current_speaker_device)
            .field("peer_count", &self.active_peers.len())
            .field("worklet_module_loaded", &self.worklet_module_loaded)
            .finish()
    }
}

struct PeerAudioSink {
    pcm_worklet: AudioWorkletNode,
    gain_node: Option<GainNode>,
    is_muted: bool,
}

impl SharedNetEqAudioManager {
    /// Create a new shared NetEq audio manager with optional speaker device
    pub fn new(speaker_device_id: Option<String>) -> Result<Self, JsValue> {
        info!("Creating SharedNetEqAudioManager with device: {speaker_device_id:?}");

        // Create single AudioContext with 48kHz
        let options = AudioContextOptions::new();
        options.set_sample_rate(48000.0);
        let audio_context = AudioContext::new_with_context_options(&options)?;

        // Set initial speaker device
        if let Some(device_id) = &speaker_device_id {
            Self::set_audio_context_device(&audio_context, device_id.clone())?;
        }

        Ok(Self {
            audio_context,
            current_speaker_device: speaker_device_id,
            active_peers: HashMap::new(),
            worklet_module_loaded: false,
        })
    }

    /// Add a new peer - creates dedicated PCM worklet for this peer
    pub async fn add_peer(&mut self, peer_id: String) -> Result<NetEqPeerSink, JsValue> {
        info!("Adding peer to shared audio manager: {peer_id}");

        // Ensure worklet module is loaded
        self.ensure_worklet_module_loaded().await?;

        // Create individual PCM worklet for this peer
        let pcm_worklet = AudioWorkletNode::new(&self.audio_context, "pcm-player")?;

        // Create gain node for per-peer volume control
        let gain_node = self.audio_context.create_gain()?;
        gain_node.gain().set_value(1.0);

        // Connect: PCM Worklet → Gain Node → Destination
        pcm_worklet.connect_with_audio_node(&gain_node)?;
        gain_node.connect_with_audio_node(&self.audio_context.destination())?;

        // Configure worklet for 48kHz mono
        let config_message = js_sys::Object::new();
        js_sys::Reflect::set(&config_message, &"command".into(), &"configure".into())?;
        js_sys::Reflect::set(&config_message, &"sampleRate".into(), &48000.0.into())?;
        js_sys::Reflect::set(&config_message, &"channels".into(), &1.0.into())?;
        pcm_worklet.port()?.post_message(&config_message)?;

        // Create peer sink
        let peer_sink = PeerAudioSink {
            pcm_worklet: pcm_worklet.clone(),
            gain_node: Some(gain_node),
            is_muted: true, // Start muted by default
        };

        // Store peer
        self.active_peers.insert(peer_id.clone(), peer_sink);

        // Return sink interface for NetEq decoder to use
        Ok(NetEqPeerSink {
            peer_id,
            pcm_worklet: Some(pcm_worklet),
        })
    }

    /// Remove peer - destroys their PCM worklet
    pub fn remove_peer(&mut self, peer_id: &str) -> Result<(), JsValue> {
        info!("Removing peer from shared audio manager: {peer_id}");

        if let Some(peer_sink) = self.active_peers.remove(peer_id) {
            // Disconnect and cleanup
            peer_sink.pcm_worklet.disconnect()?;
            if let Some(gain_node) = peer_sink.gain_node {
                gain_node.disconnect()?;
            }
            info!("Successfully removed peer: {peer_id}");
        } else {
            warn!("Attempted to remove non-existent peer: {peer_id}");
        }

        Ok(())
    }

    /// Update speaker device - affects ALL peers simultaneously
    pub fn update_speaker_device(&mut self, device_id: Option<String>) -> Result<(), JsValue> {
        info!("Updating shared audio context speaker device: {device_id:?}");

        if self.current_speaker_device == device_id {
            info!("Speaker device unchanged, skipping update");
            return Ok(());
        }

        // Update the shared AudioContext device
        if let Some(device_id) = &device_id {
            Self::set_audio_context_device(&self.audio_context, device_id.clone())?;
        }

        self.current_speaker_device = device_id;
        info!(
            "Successfully updated speaker device for {} peers",
            self.active_peers.len()
        );

        Ok(())
    }

    /// Set individual peer mute state
    pub fn set_peer_muted(&mut self, peer_id: &str, muted: bool) -> Result<(), JsValue> {
        if let Some(peer_sink) = self.active_peers.get_mut(peer_id) {
            peer_sink.is_muted = muted;
            if let Some(gain_node) = &peer_sink.gain_node {
                gain_node.gain().set_value(if muted { 0.0 } else { 1.0 });
            }
            log::debug!("Set peer {peer_id} muted: {muted}");
        }
        Ok(())
    }

    /// Get number of active peers
    pub fn peer_count(&self) -> usize {
        self.active_peers.len()
    }

    /// Get reference to the shared AudioContext
    pub fn audio_context(&self) -> &AudioContext {
        &self.audio_context
    }

    /// Send PCM data for a specific peer to their worklet
    pub fn send_peer_pcm(&self, peer_id: &str, pcm: js_sys::Float32Array) -> Result<(), JsValue> {
        if let Some(peer_sink) = self.active_peers.get(peer_id) {
            let message = js_sys::Object::new();
            js_sys::Reflect::set(&message, &"command".into(), &"play".into())?;
            js_sys::Reflect::set(&message, &"pcm".into(), &pcm)?;

            peer_sink.pcm_worklet.port()?.post_message(&message)?;
        } else {
            log::warn!("Attempted to send PCM to non-existent peer: {peer_id}");
        }
        Ok(())
    }

    async fn ensure_worklet_module_loaded(&mut self) -> Result<(), JsValue> {
        if !self.worklet_module_loaded {
            info!("Loading PCM player worklet module");

            // Load the same worklet code used by individual decoders
            let worklet_code = include_str!("../../../neteq/src/scripts/pcmPlayerWorker.js");
            let blob_parts = js_sys::Array::new();
            blob_parts.push(&JsValue::from_str(worklet_code));
            let blob_property_bag = web_sys::BlobPropertyBag::new();
            blob_property_bag.set_type("application/javascript");
            let blob =
                web_sys::Blob::new_with_str_sequence_and_options(&blob_parts, &blob_property_bag)?;
            let worklet_url = web_sys::Url::create_object_url_with_blob(&blob)?;

            let module_promise = self
                .audio_context
                .audio_worklet()?
                .add_module(&worklet_url)?;
            JsFuture::from(module_promise).await?;
            web_sys::Url::revoke_object_url(&worklet_url)?;

            self.worklet_module_loaded = true;
            info!("PCM player worklet module loaded successfully");
        }
        Ok(())
    }

    fn set_audio_context_device(context: &AudioContext, device_id: String) -> Result<(), JsValue> {
        if js_sys::Reflect::has(context, &JsValue::from_str("setSinkId")).unwrap_or(false) {
            let promise = context.set_sink_id_with_str(&device_id);
            wasm_bindgen_futures::spawn_local(async move {
                match JsFuture::from(promise).await {
                    Ok(_) => info!("Successfully set shared AudioContext device: {device_id}"),
                    Err(e) => warn!("Failed to set shared AudioContext device: {e:?}"),
                }
            });
        }
        Ok(())
    }
}
