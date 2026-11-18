// Port target: WebRTC
// modules/audio_coding/neteq/tools/audio_sink.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2014 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Where `NetEqTest` writes output audio.

use crate::api::audio_frame::AudioFrame;

/// C++ `AudioSink`.
pub trait AudioSink {
    /// Writes interleaved samples. Returns false on failure.
    fn write_array(&mut self, audio: &[i16]) -> bool;

    /// Writes the frame's samples.
    fn write_audio_frame(&mut self, audio_frame: &AudioFrame) -> bool {
        self.write_array(audio_frame.data())
    }
}

/// C++ `VoidAudioSink`: discards everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct VoidAudioSink;

impl AudioSink for VoidAudioSink {
    fn write_array(&mut self, _audio: &[i16]) -> bool {
        true
    }
}

/// Collects every written sample. Shares its storage so a caller can read it
/// after handing the sink to `NetEqTest`. Not in C++ (tests use
/// `OutputAudioFile` and checksums there).
#[derive(Debug, Default, Clone)]
pub struct VectorAudioSink {
    samples: std::sync::Arc<std::sync::Mutex<Vec<i16>>>,
}

impl VectorAudioSink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of everything written so far.
    pub fn samples(&self) -> Vec<i16> {
        self.samples
            .lock()
            .map(|samples| samples.clone())
            .unwrap_or_default()
    }
}

impl AudioSink for VectorAudioSink {
    fn write_array(&mut self, audio: &[i16]) -> bool {
        match self.samples.lock() {
            Ok(mut samples) => {
                samples.extend_from_slice(audio);
                true
            }
            Err(_) => false,
        }
    }
}
