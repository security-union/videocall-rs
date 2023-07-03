use web_sys::{CodecState, EncodedVideoChunk, VideoDecoderConfig};

// Define the trait
pub trait VideoDecoderTrait {
    fn configure(&self, config: &VideoDecoderConfig);
    fn decode(&self, chunk: &EncodedVideoChunk);
    fn state(&self) -> CodecState;
}

// Create a wrapper struct for the foreign struct
pub struct VideoDecoderWrapper(web_sys::VideoDecoder);

// Implement the trait for the wrapper struct
impl VideoDecoderTrait for VideoDecoderWrapper {
    fn configure(&self, config: &VideoDecoderConfig) {
        self.0.configure(config);
    }
    
    fn decode(&self, chunk: &EncodedVideoChunk) {
        self.0.decode(chunk);
    }

    fn state(&self) -> CodecState {
        self.0.state()
    }
}
