//! Implementation-neutral types shared by both NetEqs under test.
//!
//! These mirror the public WebRTC NetEq API (`api/neteq/neteq.h`) so the
//! Rust port can be driven and compared through exactly the same surface.

use serde::{Deserialize, Serialize};

/// Mirrors `webrtc::NetEq::Config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetEqConfig {
    /// Initial output sample rate; NetEq switches to the decoder's rate.
    pub sample_rate_hz: i32,
    /// Packet buffer capacity in packets.
    pub max_packets_in_buffer: u32,
    /// Upper bound on target delay (0 = none).
    pub max_delay_ms: i32,
    /// Lower bound on target delay (0 = none).
    pub min_delay_ms: i32,
    /// Accelerate based on arrival time when the buffer is far too full.
    pub enable_fast_accelerate: bool,
    /// Allow muted output after a long expand period.
    pub enable_muted_state: bool,
    /// Treat retransmitted packets specially in the delay estimator.
    pub enable_rtx_handling: bool,
    /// Test-only switch that disables accelerate / preemptive expand.
    pub for_test_no_time_stretching: bool,
}

impl Default for NetEqConfig {
    /// Same defaults as `webrtc::NetEq::Config`.
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000,
            max_packets_in_buffer: 200,
            max_delay_ms: 0,
            min_delay_ms: 0,
            enable_fast_accelerate: false,
            enable_muted_state: false,
            enable_rtx_handling: false,
            for_test_no_time_stretching: false,
        }
    }
}

/// Mirrors `webrtc::NetEq::Operation`: the decision taken for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// Decode and play normally.
    Normal,
    /// Merge decoded audio with the tail of an expansion.
    Merge,
    /// Packet loss concealment.
    Expand,
    /// Time-compress to drain the buffer.
    Accelerate,
    /// Arrival-time based aggressive accelerate.
    FastAccelerate,
    /// Time-stretch to grow the buffer.
    PreemptiveExpand,
    /// RFC 3389 comfort noise from a SID packet.
    Rfc3389Cng,
    /// RFC 3389 comfort noise continued without a new packet.
    Rfc3389CngNoPacket,
    /// Codec-internal CNG (e.g. Opus DTX).
    CodecInternalCng,
    /// DTMF tone generation.
    Dtmf,
    /// No operation recorded yet.
    Undefined,
}

impl Operation {
    /// Maps the numeric `webrtc::NetEq::Operation` value.
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            0 => Self::Normal,
            1 => Self::Merge,
            2 => Self::Expand,
            3 => Self::Accelerate,
            4 => Self::FastAccelerate,
            5 => Self::PreemptiveExpand,
            6 => Self::Rfc3389Cng,
            7 => Self::Rfc3389CngNoPacket,
            8 => Self::CodecInternalCng,
            9 => Self::Dtmf,
            _ => Self::Undefined,
        }
    }

    /// All variants, in WebRTC enum order.
    pub const ALL: [Operation; 11] = [
        Self::Normal,
        Self::Merge,
        Self::Expand,
        Self::Accelerate,
        Self::FastAccelerate,
        Self::PreemptiveExpand,
        Self::Rfc3389Cng,
        Self::Rfc3389CngNoPacket,
        Self::CodecInternalCng,
        Self::Dtmf,
        Self::Undefined,
    ];
}

/// Mirrors `webrtc::AudioFrame::SpeechType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechType {
    /// Decoded speech.
    NormalSpeech,
    /// Packet loss concealment.
    Plc,
    /// Comfort noise.
    Cng,
    /// Concealment that faded into comfort noise.
    PlcCng,
    /// Codec-internal PLC.
    CodecPlc,
    /// Not set.
    Undefined,
}

impl SpeechType {
    /// Maps the numeric `webrtc::AudioFrame::SpeechType` value.
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            0 => Self::NormalSpeech,
            1 => Self::Plc,
            2 => Self::Cng,
            3 => Self::PlcCng,
            5 => Self::CodecPlc,
            _ => Self::Undefined,
        }
    }
}

/// Mirrors `webrtc::AudioFrame::VADActivity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VadActivity {
    /// Voice active.
    Active,
    /// Voice passive.
    Passive,
    /// Unknown.
    Unknown,
}

impl VadActivity {
    /// Maps the numeric `webrtc::AudioFrame::VADActivity` value.
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            0 => Self::Active,
            1 => Self::Passive,
            _ => Self::Unknown,
        }
    }
}

/// RTP header fields NetEq consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RtpHeader {
    /// RTP payload type.
    pub payload_type: u8,
    /// Marker bit (start of talkspurt for audio).
    pub marker: bool,
    /// RTP sequence number.
    pub sequence_number: u16,
    /// RTP timestamp.
    pub timestamp: u32,
    /// Synchronization source.
    pub ssrc: u32,
}

/// One 10 ms output frame plus the decision that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioOutput {
    /// Interleaved samples (`samples_per_channel * num_channels`).
    pub samples: Vec<i16>,
    /// Samples per channel (10 ms worth at `sample_rate_hz`).
    pub samples_per_channel: usize,
    /// Output channel count.
    pub num_channels: usize,
    /// Output sample rate.
    pub sample_rate_hz: i32,
    /// Speech type of the frame.
    pub speech_type: SpeechType,
    /// VAD activity of the frame.
    pub vad_activity: VadActivity,
    /// True when NetEq returned a muted frame (samples are zero).
    pub muted: bool,
    /// Operation NetEq performed for this frame.
    pub operation: Operation,
    /// RTP timestamp of the last sample played, if known.
    pub playout_timestamp: Option<u32>,
    /// `NetEq::GetAudio` return code (0 = ok).
    pub return_code: i32,
}

/// Mirrors `webrtc::NetEqNetworkStatistics`. Rates are Q14 fractions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct NetworkStatistics {
    pub current_buffer_size_ms: u16,
    pub preferred_buffer_size_ms: u16,
    pub jitter_peaks_found: u16,
    pub expand_rate: u16,
    pub speech_expand_rate: u16,
    pub preemptive_rate: u16,
    pub accelerate_rate: u16,
    pub secondary_decoded_rate: u16,
    pub secondary_discarded_rate: u16,
    pub mean_waiting_time_ms: i32,
    pub median_waiting_time_ms: i32,
    pub min_waiting_time_ms: i32,
    pub max_waiting_time_ms: i32,
}

/// Mirrors `webrtc::NetEqLifetimeStatistics`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct LifetimeStatistics {
    pub total_samples_received: u64,
    pub concealed_samples: u64,
    pub concealment_events: u64,
    pub jitter_buffer_delay_ms: u64,
    pub jitter_buffer_emitted_count: u64,
    pub jitter_buffer_target_delay_ms: u64,
    pub jitter_buffer_minimum_delay_ms: u64,
    pub inserted_samples_for_deceleration: u64,
    pub removed_samples_for_acceleration: u64,
    pub silent_concealed_samples: u64,
    pub fec_packets_received: u64,
    pub fec_packets_discarded: u64,
    pub packets_discarded: u64,
    pub delayed_packet_outage_samples: u64,
    pub delayed_packet_outage_events: u64,
    pub relative_packet_arrival_delay_ms: u64,
    pub jitter_buffer_packets_received: u64,
    pub interruption_count: i32,
    pub total_interruption_duration_ms: i32,
    pub generated_noise_samples: u64,
    pub total_processing_delay_us: u64,
}

/// Mirrors `webrtc::NetEqOperationsAndState`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct OperationsAndState {
    pub preemptive_samples: u64,
    pub accelerate_samples: u64,
    pub packet_buffer_flushes: u64,
    pub last_waiting_time_ms: u64,
    pub current_buffer_size_ms: u64,
    pub current_frame_size_ms: u64,
    pub next_packet_available: bool,
}

/// Codec a trace is encoded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Codec {
    /// Linear 16-bit big-endian PCM (RTP "L16", WebRTC PCM16B).
    L16 {
        /// 8000, 16000, 32000 or 48000.
        sample_rate_hz: u32,
        /// 1 or 2.
        channels: u8,
    },
    /// Opus, always 48 kHz RTP clock.
    Opus {
        /// 1 or 2 (SDP `stereo`).
        channels: u8,
    },
}

impl Codec {
    /// RTP clock rate.
    pub fn rtp_clock_hz(&self) -> u32 {
        match self {
            Codec::L16 { sample_rate_hz, .. } => *sample_rate_hz,
            Codec::Opus { .. } => 48_000,
        }
    }

    /// Channel count.
    pub fn channels(&self) -> u8 {
        match self {
            Codec::L16 { channels, .. } | Codec::Opus { channels } => *channels,
        }
    }

    /// Payload type the harness uses for this codec.
    pub fn payload_type(&self) -> u8 {
        match self {
            Codec::L16 { .. } => 96,
            Codec::Opus { .. } => 111,
        }
    }

    /// Payload type the harness uses for RFC 3389 CN at the codec's rate.
    pub fn cn_payload_type(&self) -> u8 {
        if self.rtp_clock_hz() == 8000 {
            13
        } else {
            105
        }
    }

    /// Short stable name, e.g. `l16-16k-mono`, `opus-stereo`.
    pub fn label(&self) -> String {
        let ch = if self.channels() == 2 {
            "stereo"
        } else {
            "mono"
        };
        match self {
            Codec::L16 { sample_rate_hz, .. } => format!("l16-{}k-{ch}", sample_rate_hz / 1000),
            Codec::Opus { .. } => format!("opus-{ch}"),
        }
    }

    /// Parses [`Codec::label`] output.
    pub fn parse(label: &str) -> Option<Codec> {
        let parts: Vec<&str> = label.split('-').collect();
        let channels = |s: &str| match s {
            "mono" => Some(1),
            "stereo" => Some(2),
            _ => None,
        };
        match parts.as_slice() {
            ["l16", rate, ch] => {
                let sample_rate_hz = match *rate {
                    "8k" => 8000,
                    "16k" => 16000,
                    "32k" => 32000,
                    "48k" => 48000,
                    _ => return None,
                };
                Some(Codec::L16 {
                    sample_rate_hz,
                    channels: channels(ch)?,
                })
            }
            ["opus", ch] => Some(Codec::Opus {
                channels: channels(ch)?,
            }),
            _ => None,
        }
    }

    /// Every codec configuration the harness supports.
    pub fn all() -> Vec<Codec> {
        let mut v = Vec::new();
        for rate in [8000, 16000, 32000, 48000] {
            for ch in [1, 2] {
                v.push(Codec::L16 {
                    sample_rate_hz: rate,
                    channels: ch,
                });
            }
        }
        v.push(Codec::Opus { channels: 1 });
        v.push(Codec::Opus { channels: 2 });
        v
    }
}
