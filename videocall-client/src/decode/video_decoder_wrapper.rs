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

use super::super::wrappers::EncodedVideoChunkTypeWrapper;
use super::media_decoder_trait::MediaDecoderTrait;
use js_sys::Uint8Array;
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::{
    CodecState, EncodedVideoChunk, EncodedVideoChunkInit, EncodedVideoChunkType, VideoDecoder,
    VideoDecoderConfig, VideoDecoderInit,
};

// Legacy trait kept for backward compatibility
pub trait VideoDecoderTrait {
    fn new(init: &VideoDecoderInit) -> Result<Self, JsValue>
    where
        Self: Sized;
    fn configure(&self, config: &VideoDecoderConfig) -> Result<(), JsValue>;
    fn decode(&self, image: Arc<MediaPacket>) -> Result<(), JsValue>;
    fn state(&self) -> CodecState;
}

// Create a wrapper struct for the foreign struct
#[derive(Debug)]
pub struct VideoDecoderWrapper(web_sys::VideoDecoder);

// Implement the original trait for backward compatibility
impl VideoDecoderTrait for VideoDecoderWrapper {
    fn configure(&self, config: &VideoDecoderConfig) -> Result<(), JsValue> {
        self.0.configure(config)
    }

    fn decode(&self, image: Arc<MediaPacket>) -> Result<(), JsValue> {
        let chunk_type = EncodedVideoChunkTypeWrapper::from(image.frame_type.as_str()).0;
        let video_data = Uint8Array::new_with_length(image.data.len().try_into().unwrap());
        video_data.copy_from(&image.data);
        let video_chunk = EncodedVideoChunkInit::new(&video_data, image.timestamp, chunk_type);
        video_chunk.set_duration(image.duration);
        let encoded_video_chunk = EncodedVideoChunk::new(&video_chunk).unwrap();
        self.0.decode(&encoded_video_chunk)
    }

    fn state(&self) -> CodecState {
        self.0.state()
    }

    fn new(init: &VideoDecoderInit) -> Result<Self, JsValue>
    where
        Self: Sized,
    {
        VideoDecoder::new(init).map(VideoDecoderWrapper)
    }
}

impl Drop for VideoDecoderWrapper {
    fn drop(&mut self) {
        log::info!("Dropping VideoDecoderWrapper");
        // only close if it's not already closed
        let encoder: &web_sys::VideoDecoder = &self.0;
        if encoder.state() != CodecState::Closed {
            if let Err(e) = encoder.close() {
                log::error!("Error closing VideoDecoderWrapper: {:?}", e);
            }
        }
    }
}

// Implement the general media decoder trait
impl MediaDecoderTrait for VideoDecoderWrapper {
    type InitType = VideoDecoderInit;
    type ConfigType = VideoDecoderConfig;

    fn new(init: &Self::InitType) -> Result<Self, JsValue>
    where
        Self: Sized,
    {
        VideoDecoder::new(init).map(VideoDecoderWrapper)
    }

    fn configure(&self, config: &Self::ConfigType) -> Result<(), JsValue> {
        self.0.configure(config)
    }

    fn decode(&self, packet: Arc<MediaPacket>) -> Result<(), JsValue> {
        VideoDecoderTrait::decode(self, packet)
    }

    fn state(&self) -> CodecState {
        self.0.state()
    }

    fn get_sequence_number(&self, packet: &MediaPacket) -> u64 {
        packet.video_metadata.sequence
    }

    fn is_keyframe(&self, packet: &MediaPacket) -> bool {
        let chunk_type = EncodedVideoChunkTypeWrapper::from(packet.frame_type.as_str()).0;
        chunk_type == EncodedVideoChunkType::Key
    }
}
