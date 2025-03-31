use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    MediaStreamTrack, MediaStreamTrackProcessor, MediaStreamTrackProcessorInit,
    ReadableStreamDefaultReader,
};

use crate::media_processor::MediaFrameReader;

/// Implementation of MediaFrameReader that uses the native MediaStreamTrackProcessor
pub struct NativeMediaFrameReader {
    reader: ReadableStreamDefaultReader,
    track_kind: String,
}

impl NativeMediaFrameReader {
    pub fn new(track: &MediaStreamTrack) -> Result<Self, JsValue> {
        let track_kind = track.kind();
        let processor = MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(track))?;
        let reader = processor
            .readable()
            .get_reader()
            .unchecked_into::<ReadableStreamDefaultReader>();

        Ok(Self { reader, track_kind })
    }
}

impl MediaFrameReader for NativeMediaFrameReader {
    fn read_frame(&self) -> JsValue {
        self.reader.read().into()
    }

    fn close(&self) -> Result<(), JsValue> {
        let _ = self.reader.cancel();
        Ok(())
    }

    fn track_kind(&self) -> &str {
        &self.track_kind
    }
}
