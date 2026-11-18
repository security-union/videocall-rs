// Subset of WebRTC api/audio/audio_frame.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd: the parts NetEq writes.
// Dropped: VAD activity, channel layout, absolute capture time.
// Copyright (c) 2018 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! 10 ms of interleaved `i16` output audio.

use crate::api::rtp::RtpPacketInfo;

/// `AudioFrame::SpeechType`. Values match C++.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum OutputSpeechType {
    NormalSpeech = 0,
    Plc = 1,
    Cng = 2,
    PlcCng = 3,
    #[default]
    Undefined = 4,
    CodecPlc = 5,
}

/// Interleaved 16-bit audio, up to [`AudioFrame::MAX_DATA_SIZE_SAMPLES`]
/// samples. A muted frame reads as zeros without touching the buffer.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// RTP timestamp of the first sample in the frame.
    pub timestamp: u32,
    /// Time since the first frame in milliseconds. -1 represents an
    /// uninitialized value.
    pub elapsed_time_ms: i64,
    /// NTP time of the estimated capture time in local timebase in
    /// milliseconds. -1 represents an uninitialized value.
    pub ntp_time_ms: i64,
    pub samples_per_channel: usize,
    pub sample_rate_hz: i32,
    pub num_channels: usize,
    pub speech_type: OutputSpeechType,
    /// Info about the RTP packets this frame was decoded from.
    pub packet_infos: Vec<RtpPacketInfo>,
    data: Box<[i16]>,
    muted: bool,
}

impl Default for AudioFrame {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioFrame {
    /// Stereo, 32 kHz, 120 ms (2 * 32 * 120), like C++ `kMaxDataSizeSamples`.
    pub const MAX_DATA_SIZE_SAMPLES: usize = 7680;

    /// An empty, muted frame.
    pub fn new() -> Self {
        Self {
            timestamp: 0,
            elapsed_time_ms: -1,
            ntp_time_ms: -1,
            samples_per_channel: 0,
            sample_rate_hz: 0,
            num_channels: 0,
            speech_type: OutputSpeechType::Undefined,
            packet_infos: Vec::new(),
            data: vec![0i16; Self::MAX_DATA_SIZE_SAMPLES].into_boxed_slice(),
            muted: true,
        }
    }

    /// Resets all metadata and mutes the frame.
    pub fn reset(&mut self) {
        self.reset_without_muting();
        self.muted = true;
    }

    /// Resets all metadata, leaving the mute flag and samples untouched.
    pub fn reset_without_muting(&mut self) {
        self.timestamp = 0;
        self.elapsed_time_ms = -1;
        self.ntp_time_ms = -1;
        self.samples_per_channel = 0;
        self.sample_rate_hz = 0;
        self.num_channels = 0;
        self.speech_type = OutputSpeechType::Undefined;
        self.packet_infos.clear();
    }

    /// The `samples_per_channel * num_channels` interleaved samples; all zeros
    /// if muted.
    pub fn data(&self) -> &[i16] {
        let len = self.samples_per_channel * self.num_channels;
        if self.muted {
            &ZEROS[..len]
        } else {
            &self.data[..len]
        }
    }

    /// C++ `mutable_data(samples_per_channel, num_channels)`: sets the shape,
    /// unmutes (zero-filling the used region if the frame was muted) and
    /// returns the writable interleaved region.
    pub fn mutable_data(&mut self, samples_per_channel: usize, num_channels: usize) -> &mut [i16] {
        let total_samples = samples_per_channel * num_channels;
        assert!(total_samples <= self.data.len());
        debug_assert!(
            (samples_per_channel == 0 && num_channels == 0) || num_channels <= samples_per_channel
        );
        if self.muted {
            self.data[..total_samples].fill(0);
            self.muted = false;
        }
        self.samples_per_channel = samples_per_channel;
        self.num_channels = num_channels;
        &mut self.data[..total_samples]
    }

    /// Marks the frame as muted. Samples read as zero until the next
    /// `mutable_data` call.
    pub fn mute(&mut self) {
        self.muted = true;
    }

    /// True if the frame is muted.
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Capacity of the sample buffer.
    pub fn max_16bit_samples(&self) -> usize {
        self.data.len()
    }
}

static ZEROS: [i16; AudioFrame::MAX_DATA_SIZE_SAMPLES] = [0; AudioFrame::MAX_DATA_SIZE_SAMPLES];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_frame_is_muted_and_empty() {
        let frame = AudioFrame::new();
        assert!(frame.muted());
        assert!(frame.data().is_empty());
        assert_eq!(OutputSpeechType::Undefined, frame.speech_type);
    }

    #[test]
    fn mutable_data_unmutes_and_zero_fills() {
        let mut frame = AudioFrame::new();
        frame.mutable_data(480, 1)[0] = 7;
        assert!(!frame.muted());
        assert_eq!(7, frame.data()[0]);
        frame.mute();
        assert_eq!(0, frame.data()[0]);
        let data = frame.mutable_data(480, 1);
        assert!(data.iter().all(|&s| s == 0));
    }

    #[test]
    fn reset_mutes() {
        let mut frame = AudioFrame::new();
        frame.mutable_data(160, 2);
        frame.timestamp = 17;
        frame.reset();
        assert!(frame.muted());
        assert_eq!(0, frame.timestamp);
        assert_eq!(0, frame.num_channels);
    }
}
