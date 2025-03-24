use super::media_decoder_trait::MediaDecoderTrait;
use js_sys::Uint8Array;
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::{
    AudioDecoder, AudioDecoderConfig, AudioDecoderInit, CodecState,
    EncodedAudioChunk, EncodedAudioChunkInit, EncodedAudioChunkType,
};

// Define the trait for audio decoders
pub trait AudioDecoderTrait {
    fn new(init: &AudioDecoderInit) -> Result<Self, JsValue>
    where
        Self: Sized;
    fn configure(&self, config: &AudioDecoderConfig);
    fn decode(&self, audio: Arc<MediaPacket>);
    fn state(&self) -> CodecState;
}

// Create a wrapper struct for the web_sys AudioDecoder
#[derive(Debug)]
pub struct AudioDecoderWrapper(web_sys::AudioDecoder);

// Implement the trait for the wrapper struct
impl AudioDecoderTrait for AudioDecoderWrapper {
    fn configure(&self, config: &AudioDecoderConfig) {
        self.0.configure(config);
    }

    fn decode(&self, audio: Arc<MediaPacket>) {
        let chunk_type = EncodedAudioChunkType::from_js_value(&JsValue::from(audio.frame_type.clone())).unwrap();
        let audio_data = Uint8Array::new_with_length(audio.data.len().try_into().unwrap());
        audio_data.copy_from(&audio.data);
        let mut audio_chunk = EncodedAudioChunkInit::new(&audio_data.into(), audio.timestamp, chunk_type);
        audio_chunk.duration(audio.duration);
        let encoded_audio_chunk = EncodedAudioChunk::new(&audio_chunk).unwrap();
        self.0.decode(&encoded_audio_chunk);
    }

    fn state(&self) -> CodecState {
        self.0.state()
    }

    fn new(init: &AudioDecoderInit) -> Result<Self, JsValue>
    where
        Self: Sized,
    {
        AudioDecoder::new(init).map(AudioDecoderWrapper)
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
        AudioDecoder::new(init).map(AudioDecoderWrapper)
    }
    
    fn configure(&self, config: &Self::ConfigType) {
        self.0.configure(config);
    }
    
    fn decode(&self, packet: Arc<MediaPacket>) {
        AudioDecoderTrait::decode(self, packet);
    }
    
    fn state(&self) -> CodecState {
        self.0.state()
    }
    
    fn get_sequence_number(&self, packet: &MediaPacket) -> u64 {
        packet.audio_metadata.sequence
    }
    
    fn is_keyframe(&self, packet: &MediaPacket) -> bool {
        let chunk_type = EncodedAudioChunkType::from_js_value(&JsValue::from(packet.frame_type.clone())).unwrap();
        // For audio, "key" frame concept is different, but we'll determine based on the type
        chunk_type == EncodedAudioChunkType::Key
    }
} 