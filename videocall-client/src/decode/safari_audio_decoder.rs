use js_sys::Uint8Array;
use log::{error, info};
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::{CodecState, EncodedAudioChunk, EncodedAudioChunkInit, EncodedAudioChunkType};

use super::audio_decoder_wrapper::AudioDecoderTrait;
use super::media_decoder_trait::MediaDecoderTrait;

// Create a simplified SafariAudioDecoder that does no actual decoding
#[derive(Debug)]
pub struct SafariAudioDecoder {
    state: CodecState,
}

impl SafariAudioDecoder {
    // Check if we're in Safari
    pub fn is_safari() -> bool {
        let window = web_sys::window().expect("no global window exists");
        let navigator = window.navigator();
        let user_agent = navigator.user_agent().unwrap_or_default();

        // Check if we're in Safari
        user_agent.contains("Safari") && !user_agent.contains("Chrome")
    }
}

// Implement the AudioDecoderTrait for SafariAudioDecoder
impl AudioDecoderTrait for SafariAudioDecoder {
    fn new(_init: &web_sys::AudioDecoderInit) -> Result<Self, JsValue>
    where
        Self: Sized,
    {
        info!("Creating new simplified SafariAudioDecoder");

        Ok(Self {
            state: CodecState::Unconfigured,
        })
    }

    fn configure(&self, _config: &web_sys::AudioDecoderConfig) -> Result<(), JsValue> {
        info!("Configuring SafariAudioDecoder (no-op)");
        Ok(())
    }

    fn decode(&self, _audio: Arc<MediaPacket>) -> Result<(), JsValue> {
        // No-op decode function
        log::debug!("Safari audio decoder - skipping actual decode");
        Ok(())
    }

    fn state(&self) -> CodecState {
        self.state
    }
}

// Implement MediaDecoderTrait
impl MediaDecoderTrait for SafariAudioDecoder {
    type InitType = web_sys::AudioDecoderInit;
    type ConfigType = web_sys::AudioDecoderConfig;

    fn new(init: &Self::InitType) -> Result<Self, JsValue>
    where
        Self: Sized,
    {
        AudioDecoderTrait::new(init)
    }

    fn configure(&self, config: &Self::ConfigType) -> Result<(), JsValue> {
        AudioDecoderTrait::configure(self, config)
    }

    fn decode(&self, packet: Arc<MediaPacket>) -> Result<(), JsValue> {
        AudioDecoderTrait::decode(self, packet)
    }

    fn state(&self) -> CodecState {
        AudioDecoderTrait::state(self)
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

impl Drop for SafariAudioDecoder {
    fn drop(&mut self) {
        info!("Dropping SafariAudioDecoder (no-op)");
    }
}
