// Subset of WebRTC api/rtp_headers.h (`RTPHeader`) and api/rtp_packet_info.h
// (`RtpPacketInfo`) at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd: only the
// fields NetEq reads. Copyright (c) 2017 The WebRTC project authors.
// BSD-3-Clause, see LICENSE-WEBRTC.

//! RTP header and per-packet info, as NetEq consumes them.

use serde::{Deserialize, Serialize};

/// The RTP header fields NetEq uses (`RTPHeader`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RtpHeader {
    pub marker_bit: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    /// RTP timestamp in units of the codec clock (48 kHz for Opus).
    pub timestamp: u32,
    pub ssrc: u32,
}

/// Per-packet receive info (`RtpPacketInfo`), reduced to what NetEq reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RtpPacketInfo {
    pub ssrc: u32,
    pub rtp_timestamp: u32,
    /// Receive time in microseconds. `None` is C++ `Timestamp::MinusInfinity()`.
    pub receive_time_us: Option<i64>,
}

impl RtpPacketInfo {
    /// C++ `RtpPacketInfo(const RTPHeader&, Timestamp receive_time)`.
    pub fn new(header: &RtpHeader, receive_time_us: Option<i64>) -> Self {
        Self {
            ssrc: header.ssrc,
            rtp_timestamp: header.timestamp,
            receive_time_us,
        }
    }
}
