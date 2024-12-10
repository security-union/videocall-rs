use super::super::wrappers::EncodedVideoChunkTypeWrapper;
use js_sys::Uint8Array;
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::{
    CodecState, EncodedVideoChunk, EncodedVideoChunkInit, VideoDecoder, VideoDecoderConfig,
    VideoDecoderInit,
};

// Define the trait
pub trait VideoDecoderTrait {
    fn new(init: &VideoDecoderInit) -> Result<Self, JsValue>
    where
        Self: Sized;
    fn configure(&self, config: &VideoDecoderConfig);
    fn decode(&self, image: Arc<MediaPacket>);
    fn state(&self) -> CodecState;
}

// Create a wrapper struct for the foreign struct
#[derive(Debug)]
pub struct VideoDecoderWrapper(web_sys::VideoDecoder);

// Implement the trait for the wrapper struct
impl VideoDecoderTrait for VideoDecoderWrapper {
    fn configure(&self, config: &VideoDecoderConfig) {
        self.0.configure(config);
    }

    fn decode(&self, image: Arc<MediaPacket>) {
        let chunk_type = EncodedVideoChunkTypeWrapper::from(image.frame_type.as_str()).0;
        let video_data = Uint8Array::new_with_length(image.data.len().try_into().unwrap());
        video_data.copy_from(&image.data);
        let mut video_chunk = EncodedVideoChunkInit::new(&video_data, image.timestamp, chunk_type);
        video_chunk.duration(image.duration);
        let encoded_video_chunk = EncodedVideoChunk::new(&video_chunk).unwrap();
        self.0.decode(&encoded_video_chunk);
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
