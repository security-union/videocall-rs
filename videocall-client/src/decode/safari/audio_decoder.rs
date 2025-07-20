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
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::AudioData;
use web_sys::{AudioContext, AudioContextOptions};

#[derive(Debug, Clone, Copy)]
pub struct DecodeStatus {
    pub rendered: bool,
    pub first_frame: bool,
}

#[derive(Debug)]
pub struct DecodeError {
    pub message: String,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Decode error: {}", self.message)
    }
}

impl std::error::Error for DecodeError {}

//
// Generic type for decoders captures common functionality.
//
pub struct PeerDecoder<WebDecoder, Chunk> {
    decoder: Rc<RefCell<WebDecoder>>,
    waiting_for_keyframe: bool,
    decoded: bool,
    _error: Closure<dyn FnMut(JsValue)>, // member exists to keep the closure in scope for the life of the struct
    _output: Closure<dyn FnMut(Chunk)>, // member exists to keep the closure in scope for the life of the struct
    audio_context: Rc<RefCell<Option<AudioContext>>>, // Store audio context for speaker updates
    current_speaker_id: Rc<RefCell<Option<String>>>, // Store current speaker device ID
}

impl<WebDecoder, ChunkType> PeerDecoder<WebDecoder, ChunkType> {
    pub fn is_waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }
}

pub trait PeerDecode {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<DecodeStatus, DecodeError>;
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
        let audio_context_ref = Rc::new(RefCell::new(None));
        let current_speaker_id = Rc::new(RefCell::new(speaker_device_id.clone()));

        {
            let codec = codec.clone();
            let audio_context_ref = audio_context_ref.clone();
            let speaker_id = speaker_device_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match Self::create_audio_context_with_speaker(speaker_id).await {
                    Ok(context) => {
                        *audio_context_ref.borrow_mut() = Some(context.clone());

                        // Create the audio worklet node and connect it
                        if let Err(e) = Self::setup_audio_worklet(&codec, &context).await {
                            error!("Failed to setup audio worklet: {e:?}");
                        } else {
                            info!(
                                "Safari audio decoder initialized successfully with speaker device"
                            );
                        }
                    }
                    Err(e) => {
                        error!("Failed to create audio context: {e:?}");
                    }
                }
            });
        }

        let error = Closure::wrap(Box::new(move |e: JsValue| {
            error!("{e:?}");
        }) as Box<dyn FnMut(JsValue)>);

        let output = Closure::wrap(Box::new(move |_audio_data: AudioData| {
            // Audio data is handled by the worklet
        }) as Box<dyn FnMut(AudioData)>);

        Self {
            decoder: Rc::new(RefCell::new(codec)),
            waiting_for_keyframe: false, // Audio doesn't have keyframes like video
            decoded: false,
            _error: error,
            _output: output,
            audio_context: audio_context_ref,
            current_speaker_id,
        }
    }

    /// Creates a new AudioContext with the specified speaker device
    async fn create_audio_context_with_speaker(
        speaker_device_id: Option<String>,
    ) -> Result<AudioContext, JsValue> {
        let options = AudioContextOptions::new();
        options.set_sample_rate(AUDIO_SAMPLE_RATE as f32);

        // Set the speaker device if specified
        if let Some(device_id) = speaker_device_id.clone() {
            if !device_id.is_empty() {
                info!("Creating AudioContext with speaker device: {device_id}");
                options.set_sink_id(&JsValue::from_str(&device_id));
            }
        }

        let context = AudioContext::new_with_context_options(&options)?;

        // Try to resume the context
        if let Ok(promise) = context.resume() {
            match JsFuture::from(promise).await {
                Ok(_) => info!("AudioContext resumed successfully"),
                Err(e) => warn!("Failed to resume AudioContext: {e:?}"),
            }
        }

        Ok(context)
    }

    /// Sets up the audio worklet with the given context
    async fn setup_audio_worklet(
        codec: &AudioWorkletCodec,
        context: &AudioContext,
    ) -> Result<(), JsValue> {
        // Create the worklet node
        let worklet_node = codec
            .create_node(
                context,
                "/decoderWorker.min.js",
                "decoder-worklet",
                AUDIO_CHANNELS,
            )
            .await?;

        // Create a gain node and connect the worklet to the destination
        let gain_node = context.create_gain()?;
        worklet_node.connect_with_audio_node(&gain_node)?;
        gain_node.connect_with_audio_node(&context.destination())?;

        // Send initialization message
        let init_message = DecoderMessages::Init {
            options: Some(DecoderInitOptions {
                decoder_sample_rate: Some(AUDIO_SAMPLE_RATE),
                output_buffer_sample_rate: Some(AUDIO_SAMPLE_RATE),
                number_of_channels: Some(AUDIO_CHANNELS),
                resample_quality: Some(0),
            }),
        };

        codec.send_message(init_message)?;

        Ok(())
    }

    /// Updates the speaker device by recreating the AudioContext
    pub fn update_speaker_device(&self, speaker_device_id: Option<String>) {
        let current_id = self.current_speaker_id.borrow().clone();

        // Check if the speaker device is actually changing
        if current_id == speaker_device_id {
            info!("Speaker device unchanged, skipping update");
            return;
        }

        info!("Updating Safari audio decoder speaker device to: {speaker_device_id:?}");

        let audio_context_ref = self.audio_context.clone();
        let current_speaker_id = self.current_speaker_id.clone();
        let decoder_ref = self.decoder.clone();
        let old_codec = self.decoder.borrow().clone();
        let new_speaker_id = speaker_device_id.clone();

        wasm_bindgen_futures::spawn_local(async move {
            // Update the stored speaker ID
            *current_speaker_id.borrow_mut() = new_speaker_id.clone();

            // First, destroy the old worklet to stop audio processing
            info!("Destroying old audio worklet");
            old_codec.destroy();

            // Close the old context if it exists and wait for it to fully close
            let old_context_option = {
                let borrowed = audio_context_ref.borrow();
                borrowed.clone()
            };

            if let Some(old_context) = old_context_option {
                info!("Closing old AudioContext");

                if let Ok(promise) = old_context.close() {
                    match JsFuture::from(promise).await {
                        Ok(_) => {
                            info!("Old AudioContext closed successfully");
                        }
                        Err(e) => {
                            error!("Failed to close old AudioContext: {e:?}");
                        }
                    }
                } else {
                    warn!("Failed to get close promise from old AudioContext");
                }

                // Clear the old context reference
                *audio_context_ref.borrow_mut() = None;
                info!("Cleared old AudioContext reference");
            } else {
                info!("No old AudioContext to close");
            }

            // Add a small delay to ensure cleanup is complete
            let delay_promise = js_sys::Promise::new(&mut |resolve, _| {
                web_sys::window()
                    .unwrap()
                    .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 100)
                    .unwrap();
            });
            let _ = JsFuture::from(delay_promise).await;
            info!("Completed cleanup delay");

            // Create a new codec instance
            let new_codec = AudioWorkletCodec::default();
            info!("Created new AudioWorkletCodec");

            // Create the new AudioContext with the new speaker device
            match Self::create_audio_context_with_speaker(new_speaker_id.clone()).await {
                Ok(new_context) => {
                    info!("New AudioContext created successfully");

                    // Store the new context
                    *audio_context_ref.borrow_mut() = Some(new_context.clone());
                    info!("Stored new AudioContext reference");

                    // Try to resume the context if it's not already running
                    if let Ok(resume_promise) = new_context.resume() {
                        match JsFuture::from(resume_promise).await {
                            Ok(_) => {
                                info!("New AudioContext resumed successfully");
                            }
                            Err(e) => {
                                warn!("Failed to resume new AudioContext: {e:?}");
                            }
                        }
                    }

                    // Setup the audio worklet with the new context and new codec
                    info!("Setting up audio worklet with new context");
                    match Self::setup_audio_worklet(&new_codec, &new_context).await {
                        Ok(_) => {
                            // Update the decoder with the new codec
                            *decoder_ref.borrow_mut() = new_codec;
                            info!("Successfully updated Safari audio decoder speaker device");
                        }
                        Err(e) => {
                            error!("Failed to setup audio worklet with new AudioContext: {e:?}");
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to create new AudioContext for speaker update: {e:?}");
                }
            }
        });
    }

    fn get_chunk(&self, packet: &Arc<MediaPacket>) -> Uint8Array {
        let data = Uint8Array::new_with_length(packet.data.len() as u32);
        data.copy_from(&packet.data);
        data
    }
}

impl PeerDecode for AudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<DecodeStatus, DecodeError> {
        let buffer = self.get_chunk(packet);
        let first_frame = !self.decoded;

        let rendered = {
            let decoder = self.decoder.borrow();
            if decoder.is_instantiated() {
                // Drop the immutable borrow before getting a mutable one
                drop(decoder);
                let _ = self
                    .decoder
                    .borrow_mut()
                    .send_message(DecoderMessages::Decode {
                        pages: buffer.to_vec(),
                    });
                self.decoded = true;
                true
            } else {
                false
            }
        };

        Ok(DecodeStatus {
            rendered,
            first_frame,
        })
    }
}
