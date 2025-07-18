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

use super::media_decoder_trait::MediaDecoderTrait;
use js_sys::Uint8Array;
use log::{error, info};
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::{
    AudioDecoder, AudioDecoderConfig, AudioDecoderInit, CodecState, EncodedAudioChunk,
    EncodedAudioChunkInit, EncodedAudioChunkType,
};

// Define the trait for audio decoders
pub trait AudioDecoderTrait {
    fn new(init: &AudioDecoderInit) -> Result<Self, JsValue>
    where
        Self: Sized;
    fn configure(&self, config: &AudioDecoderConfig) -> Result<(), JsValue>;
    fn decode(&self, audio: Arc<MediaPacket>) -> Result<(), JsValue>;
    fn state(&self) -> CodecState;
}

// Create a wrapper struct for the web_sys AudioDecoder
#[derive(Debug)]
pub struct AudioDecoderWrapper(web_sys::AudioDecoder);

// Implement the trait for the wrapper struct
impl AudioDecoderTrait for AudioDecoderWrapper {
    fn configure(&self, config: &AudioDecoderConfig) -> Result<(), JsValue> {
        info!("Configuring audio decoder with config: {config:?}");
        self.0.configure(config)
    }

    fn decode(&self, audio: Arc<MediaPacket>) -> Result<(), JsValue> {
        let chunk_type =
            EncodedAudioChunkType::from_js_value(&JsValue::from(audio.frame_type.clone())).unwrap();
        let audio_data = Uint8Array::new_with_length(audio.data.len().try_into().unwrap());
        audio_data.copy_from(&audio.data);
        let audio_chunk =
            EncodedAudioChunkInit::new(&audio_data.into(), audio.timestamp, chunk_type);
        audio_chunk.set_duration(audio.duration);
        let encoded_audio_chunk = EncodedAudioChunk::new(&audio_chunk).unwrap();

        match self.0.decode(&encoded_audio_chunk) {
            Ok(_) => {
                log::debug!("Successfully decoded audio chunk");
                Ok(())
            }
            Err(e) => {
                error!("Error decoding audio chunk: {e:?}");
                Err(e)
            }
        }
    }

    fn state(&self) -> CodecState {
        let state = self.0.state();
        log::debug!("Audio decoder state: {state:?}");
        state
    }

    fn new(init: &AudioDecoderInit) -> Result<Self, JsValue>
    where
        Self: Sized,
    {
        info!("Creating new audio decoder");
        AudioDecoder::new(init).map(AudioDecoderWrapper)
    }
}

impl AudioDecoderWrapper {
    pub fn flush(&self) -> Result<(), JsValue> {
        // AudioDecoder.flush() returns a Promise, we'll call it and return immediately
        let _ = self.0.flush();
        Ok(())
    }
}

// Implement the general media decoder trait
impl MediaDecoderTrait for AudioDecoderWrapper {
    type InitType = AudioDecoderInit;
    type ConfigType = AudioDecoderConfig;

    fn new(init: &Self::InitType) -> Result<Self, JsValue>
    where
        Self: Sized,
    {
        info!("Creating new audio decoder (MediaDecoderTrait)");
        AudioDecoder::new(init).map(AudioDecoderWrapper)
    }

    fn configure(&self, config: &Self::ConfigType) -> Result<(), JsValue> {
        info!("Configuring audio decoder (MediaDecoderTrait) with config: {config:?}");
        self.0.configure(config)
    }

    fn decode(&self, packet: Arc<MediaPacket>) -> Result<(), JsValue> {
        log::debug!(
            "Decoding audio packet: sequence={}, frame_type={}",
            packet.audio_metadata.sequence,
            packet.frame_type
        );
        AudioDecoderTrait::decode(self, packet)
    }

    fn state(&self) -> CodecState {
        let state = self.0.state();
        info!("Audio decoder state (MediaDecoderTrait): {state:?}");
        state
    }

    fn get_sequence_number(&self, packet: &MediaPacket) -> u64 {
        packet.audio_metadata.sequence
    }

    fn is_keyframe(&self, packet: &MediaPacket) -> bool {
        let chunk_type =
            EncodedAudioChunkType::from_js_value(&JsValue::from(packet.frame_type.clone()))
                .unwrap();
        // For audio, "key" frame concept is different, but we'll determine based on the type
        chunk_type == EncodedAudioChunkType::Key
    }
}

impl Drop for AudioDecoderWrapper {
    fn drop(&mut self) {
        log::info!("Dropping AudioDecoderWrapper");
        if let Err(e) = self.0.close() {
            log::error!("Error closing AudioDecoderWrapper: {e:?}");
        }
    }
}
