// This submodule defines two pub types:
//
//      AudioPeerDecoder
//      VideoPeerDecoder
//
// Both implement a method decoder.decode(packet) that decodes and sends the result to the
// appropriate output, as configured in the new() constructor.
//
// Both are specializations of a generic type PeerDecoder<...> for the decoding logic,
// and each one's new() contains the type-specific creation/configuration code.
//

use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use crate::decode::safari::audio_worklet_codec::AudioWorkletCodec;
use crate::decode::safari::audio_worklet_codec::DecoderInitOptions;
use crate::decode::safari::audio_worklet_codec::DecoderMessages;
use js_sys::Uint8Array;
use log::{error, info, warn};
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::AudioContext;
use web_sys::AudioContextOptions;
use web_sys::AudioData;

pub struct DecodeStatus {
    pub rendered: bool,
    pub first_frame: bool,
}

//
// Generic type for decoders captures common functionality.
//
pub struct PeerDecoder<WebDecoder, Chunk> {
    decoder: WebDecoder,
    waiting_for_keyframe: bool,
    decoded: bool,
    _error: Closure<dyn FnMut(JsValue)>, // member exists to keep the closure in scope for the life of the struct
    _output: Closure<dyn FnMut(Chunk)>, // member exists to keep the closure in scope for the life of the struct
}

impl<WebDecoder, ChunkType> PeerDecoder<WebDecoder, ChunkType> {
    pub fn is_waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }
}

pub trait PeerDecode {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<DecodeStatus, ()>;
}

///
/// Implementation of decode(packet) -> Result<DecodeStatus, ()>
///
/// (Defined as a macro rather than a trait because traits can't refer to members.)
///
macro_rules! impl_decode {
    ($self: expr, $packet: expr, $ChunkType: ty, $ref: tt) => {{
        let first_frame = !$self.decoded;
        let chunk_type = $self.get_chunk_type(&$packet);
        if !$self.waiting_for_keyframe || chunk_type == <$ChunkType>::Key {
            match $self.decoder.state() {
                CodecState::Configured => {
                    $self
                        .decoder
                        .decode(opt_ref!($self.get_chunk($packet, chunk_type), $ref));
                    $self.waiting_for_keyframe = false;
                    $self.decoded = true;
                }
                CodecState::Closed => {
                    return Err(());
                }
                _ => {}
            }
        }
        Ok(DecodeStatus {
            rendered: true,
            first_frame,
        })
    }};
}

macro_rules! opt_ref {
    ($val: expr, "ref") => {
        &$val
    };
    ($val: expr, "") => {
        $val
    };
}

///
/// AudioPeerDecoder
///
/// Plays audio to the standard audio stream.
///
/// This is important https://plnkr.co/edit/1yQd8ozGXlV9bwK6?preview
/// https://github.com/WebAudio/web-audio-api-v2/issues/133

pub type AudioPeerDecoder = PeerDecoder<AudioWorkletCodec, AudioData>;

impl Default for AudioPeerDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPeerDecoder {
    pub fn new() -> Self {
        Self::new_with_speaker(None)
    }

    pub fn new_with_speaker(speaker_device_id: Option<String>) -> Self {
        let codec = AudioWorkletCodec::default();
        {
            let codec = codec.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let mut options = AudioContextOptions::new();
                options.sample_rate(AUDIO_SAMPLE_RATE as f32);
                let context = AudioContext::new_with_context_options(&options).unwrap();

                // Set the audio output device if specified and supported
                if let Some(device_id) = speaker_device_id {
                    // Check if setSinkId is supported
                    if js_sys::Reflect::has(&context, &JsValue::from_str("setSinkId"))
                        .unwrap_or(false)
                    {
                        match JsFuture::from(context.set_sink_id_with_str(&device_id)).await {
                            Ok(_) => {
                                info!(
                                    "Successfully set Safari audio output device to: {}",
                                    device_id
                                );
                            }
                            Err(e) => {
                                warn!("Failed to set Safari audio output device: {:?}", e);
                            }
                        }
                    } else {
                        warn!("AudioContext.setSinkId() is not supported in this Safari version");
                    }
                }

                let gain_node = context.create_gain().unwrap();

                // Add the decoder audio worklet to the context
                let worklet_node = codec
                    .create_node(
                        &context,
                        "/decoderWorker.min.js",
                        "decoder-worklet",
                        AUDIO_CHANNELS,
                    )
                    .await
                    .unwrap();

                let _ = worklet_node
                    .connect_with_audio_node(&gain_node)
                    .unwrap()
                    .connect_with_audio_node(&context.destination());

                let _ = codec.send_message(DecoderMessages::Init {
                    options: Some(DecoderInitOptions {
                        output_buffer_sample_rate: Some(AUDIO_SAMPLE_RATE),
                        ..Default::default()
                    }),
                });
            })
        }

        // These aren't really used, but kept to ensure parity with video decoding
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            error!("{:?}", e);
        }) as Box<dyn FnMut(JsValue)>);

        let output = Closure::wrap(Box::new(move |_| {}) as Box<dyn FnMut(AudioData)>);

        Self {
            decoder: codec,
            // There is no keyframe
            waiting_for_keyframe: false,
            decoded: false,
            _error: error,
            _output: output,
        }
    }

    fn get_chunk(&self, packet: &Arc<MediaPacket>) -> Uint8Array {
        let audio_data = &packet.data;
        let audio_data_js: js_sys::Uint8Array =
            js_sys::Uint8Array::new_with_length(audio_data.len() as u32);
        audio_data_js.copy_from(audio_data.as_slice());
        audio_data_js
    }
}

impl PeerDecode for AudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<DecodeStatus, ()> {
        let buffer = self.get_chunk(packet);
        let first_frame = !self.decoded;

        let rendered = if self.decoder.is_instantiated() {
            let _ = self.decoder.send_message(&DecoderMessages::Decode {
                pages: buffer.to_vec(),
            });
            self.decoded = true;
            true
        } else {
            false
        };

        Ok(DecodeStatus {
            rendered,
            first_frame,
        })
    }
}
