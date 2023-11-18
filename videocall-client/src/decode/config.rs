use crate::constants::AUDIO_SAMPLE_RATE;
use js_sys::Array;
use web_sys::{AudioContext, AudioContextOptions};
use web_sys::{MediaStream, MediaStreamTrackGenerator};

pub fn configure_audio_context(
    audio_stream_generator: &MediaStreamTrackGenerator,
) -> anyhow::Result<AudioContext> {
    let js_tracks = Array::new();
    js_tracks.push(audio_stream_generator);
    let media_stream = MediaStream::new_with_tracks(&js_tracks).unwrap();
    let mut audio_context_options = AudioContextOptions::new();
    audio_context_options.sample_rate(AUDIO_SAMPLE_RATE as f32);
    let audio_context = AudioContext::new_with_context_options(&audio_context_options).unwrap();
    let gain_node = audio_context.create_gain().unwrap();
    gain_node.set_channel_count(1);
    let source = audio_context
        .create_media_stream_source(&media_stream)
        .unwrap();
    let _ = source.connect_with_audio_node(&gain_node).unwrap();
    let _ = gain_node
        .connect_with_audio_node(&audio_context.destination())
        .unwrap();
    Ok(audio_context)
}
