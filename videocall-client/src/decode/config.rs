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

use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use js_sys::Array;
use log::{info, warn};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextOptions};
use web_sys::{MediaStream, MediaStreamTrackGenerator};

pub fn configure_audio_context(
    audio_stream_generator: &MediaStreamTrackGenerator,
    sink_id: Option<String>,
) -> anyhow::Result<AudioContext> {
    info!("Configuring audio context with sample rate: {AUDIO_SAMPLE_RATE} Hz");

    let js_tracks = Array::new();
    js_tracks.push(audio_stream_generator);
    let media_stream = MediaStream::new_with_tracks(&js_tracks)
        .map_err(|e| anyhow::anyhow!("Failed to create media stream: {:?}", e))?;
    info!("Created media stream with audio track");

    let audio_context_options = AudioContextOptions::new();
    audio_context_options.set_sample_rate(AUDIO_SAMPLE_RATE as f32);
    let audio_context = AudioContext::new_with_context_options(&audio_context_options).unwrap();
    info!("Created audio context");

    // Set the audio output device if specified and supported
    if let Some(device_id) = sink_id {
        // Check if setSinkId is supported
        if js_sys::Reflect::has(&audio_context, &JsValue::from_str("setSinkId")).unwrap_or(false) {
            let audio_context_clone = audio_context.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match JsFuture::from(audio_context_clone.set_sink_id_with_str(&device_id)).await {
                    Ok(_) => {
                        info!("Successfully set audio output device to: {device_id}");
                    }
                    Err(e) => {
                        warn!("Failed to set audio output device: {e:?}");
                    }
                }
            });
        } else {
            warn!("AudioContext.setSinkId() is not supported in this browser");
        }
    }

    // Create gain node for volume control
    let gain_node = audio_context
        .create_gain()
        .map_err(|e| anyhow::anyhow!("Failed to create gain node: {:?}", e))?;
    gain_node.gain().set_value(1.0);
    gain_node.set_channel_count(AUDIO_CHANNELS);
    info!("Created gain node with {AUDIO_CHANNELS} channels");

    // Create media stream source
    let source = audio_context
        .create_media_stream_source(&media_stream)
        .map_err(|e| anyhow::anyhow!("Failed to create media stream source: {:?}", e))?;
    info!("Created media stream source");

    // Connect nodes: source -> gain -> destination
    source
        .connect_with_audio_node(&gain_node)
        .map_err(|e| anyhow::anyhow!("Failed to connect source to gain node: {:?}", e))?;
    gain_node
        .connect_with_audio_node(&audio_context.destination())
        .map_err(|e| anyhow::anyhow!("Failed to connect gain node to destination: {:?}", e))?;
    info!("Connected audio nodes: source -> gain -> destination");

    Ok(audio_context)
}
