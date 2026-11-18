// Slim port of `SdpAudioFormat` from WebRTC api/audio_codecs/audio_format.{h,cc}
// at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2016 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! SDP audio format description (`SdpAudioFormat`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// SDP `fmtp` parameters. `std::map<std::string, std::string>` in C++.
pub type CodecParameterMap = BTreeMap<String, String>;

/// An SDP audio format, e.g. `L16/48000/1`, `opus/48000/2`, `CN/8000/1`,
/// `telephone-event/48000/1` or `red/48000/1`.
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct SdpAudioFormat {
    pub name: String,
    pub clockrate_hz: i32,
    pub num_channels: usize,
    pub parameters: CodecParameterMap,
}

impl SdpAudioFormat {
    /// `SdpAudioFormat(name, clockrate_hz, num_channels)`.
    pub fn new(name: impl Into<String>, clockrate_hz: i32, num_channels: usize) -> Self {
        Self {
            name: name.into(),
            clockrate_hz,
            num_channels,
            parameters: CodecParameterMap::new(),
        }
    }

    /// `SdpAudioFormat(name, clockrate_hz, num_channels, param)`.
    pub fn with_parameters(
        name: impl Into<String>,
        clockrate_hz: i32,
        num_channels: usize,
        parameters: CodecParameterMap,
    ) -> Self {
        Self {
            name: name.into(),
            clockrate_hz,
            num_channels,
            parameters,
        }
    }

    /// Returns true if this format is compatible with `o`: names equal
    /// ignoring ASCII case, same clock rate and channel count. Parameters are
    /// not compared.
    pub fn matches(&self, o: &SdpAudioFormat) -> bool {
        self.name.eq_ignore_ascii_case(&o.name)
            && self.clockrate_hz == o.clockrate_hz
            && self.num_channels == o.num_channels
    }
}

/// C++ `operator==`: like [`SdpAudioFormat::matches`] plus equal parameters.
impl PartialEq for SdpAudioFormat {
    fn eq(&self, o: &Self) -> bool {
        self.matches(o) && self.parameters == o.parameters
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_compare_ignores_case() {
        let a = SdpAudioFormat::new("opus", 48000, 2);
        let b = SdpAudioFormat::new("OPUS", 48000, 2);
        assert!(a.matches(&b));
        assert_eq!(a, b);
    }

    #[test]
    fn equality_includes_parameters_but_matches_does_not() {
        let a = SdpAudioFormat::new("opus", 48000, 2);
        let mut params = CodecParameterMap::new();
        params.insert("stereo".to_string(), "1".to_string());
        let b = SdpAudioFormat::with_parameters("opus", 48000, 2, params);
        assert!(a.matches(&b));
        assert_ne!(a, b);
        assert!(!a.matches(&SdpAudioFormat::new("opus", 48000, 1)));
    }
}
