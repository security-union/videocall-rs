use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use js_sys::Array;
use log::info;
use web_sys::{AudioContext, AudioContextOptions};
use web_sys::{MediaStream, MediaStreamTrack};
use wasm_bindgen::JsValue;

pub fn configure_audio_context(
    audio_track: &JsValue,
) -> anyhow::Result<AudioContext> {
    info!(
        "Configuring audio context with sample rate: {}",
        AUDIO_SAMPLE_RATE
    );

    let js_tracks = Array::new();
    js_tracks.push(audio_track);
    let media_stream = MediaStream::new_with_tracks(&js_tracks)
        .map_err(|e| anyhow::anyhow!("Failed to create media stream: {:?}", e))?;
    info!("Created media stream with audio track");

    let audio_context_options = AudioContextOptions::new();
    audio_context_options.set_sample_rate(AUDIO_SAMPLE_RATE as f32);
    let audio_context = AudioContext::new_with_context_options(&audio_context_options).unwrap();
    info!("Created audio context");

    // Create gain node for volume control
    let gain_node = audio_context
        .create_gain()
        .map_err(|e| anyhow::anyhow!("Failed to create gain node: {:?}", e))?;
    gain_node.gain().set_value(1.0);
    gain_node.set_channel_count(AUDIO_CHANNELS);
    info!("Created gain node with {} channels", AUDIO_CHANNELS);

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
