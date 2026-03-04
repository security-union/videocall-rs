use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// Format-agnostic payload for MediaPacket benchmarks.
pub struct MediaPacketPayload {
    pub media_type: i32,
    pub email: String,
    pub data: Vec<u8>,
    pub frame_type: String,
    pub timestamp: f64,
    pub duration: f64,
    pub audio_metadata: Option<AudioMetadataPayload>,
    pub video_metadata: Option<VideoMetadataPayload>,
}

pub struct AudioMetadataPayload {
    pub audio_format: String,
    pub channels: u32,
    pub frames: u32,
    pub sample_rate: f32,
    pub sequence: u64,
}

pub struct VideoMetadataPayload {
    pub sequence: u64,
    pub codec: i32,
}

/// Format-agnostic payload for PacketWrapper benchmarks.
pub struct PacketWrapperPayload {
    pub packet_type: i32,
    pub email: String,
    pub data: Vec<u8>,
    pub session_id: u64,
}

/// Format-agnostic payload for HealthPacket benchmarks.
pub struct HealthPacketPayload {
    pub session_id: String,
    pub meeting_id: String,
    pub reporting_peer: String,
    pub timestamp_ms: u64,
    pub reporting_audio_enabled: bool,
    pub reporting_video_enabled: bool,
    pub peer_stats: Vec<(String, PeerStatsPayload)>,
    pub active_server_url: String,
    pub active_server_type: String,
    pub active_server_rtt_ms: f64,
}

pub struct PeerStatsPayload {
    pub can_listen: bool,
    pub can_see: bool,
    pub audio_enabled: bool,
    pub video_enabled: bool,
    pub neteq_stats: NetEqStatsPayload,
    pub video_stats: VideoStatsPayload,
}

pub struct NetEqStatsPayload {
    pub current_buffer_size_ms: f64,
    pub packets_awaiting_decode: f64,
    pub packets_per_sec: f64,
    pub counters: NetEqCountersPayload,
}

pub struct NetEqCountersPayload {
    pub normal_per_sec: f64,
    pub expand_per_sec: f64,
    pub accelerate_per_sec: f64,
    pub fast_accelerate_per_sec: f64,
    pub preemptive_expand_per_sec: f64,
    pub merge_per_sec: f64,
    pub comfort_noise_per_sec: f64,
    pub dtmf_per_sec: f64,
    pub undefined_per_sec: f64,
}

pub struct VideoStatsPayload {
    pub fps_received: f64,
    pub frames_buffered: f64,
    pub frames_decoded: u64,
    pub bitrate_kbps: u64,
}

/// ~5KB VP8 keyframe with video metadata.
pub fn video_media_packet() -> MediaPacketPayload {
    let mut rng = StdRng::seed_from_u64(42);
    let mut data = vec![0u8; 5120];
    rng.fill(&mut data[..]);

    MediaPacketPayload {
        media_type: 1, // VIDEO
        email: "user@example.com".to_string(),
        data,
        frame_type: "key".to_string(),
        timestamp: 1700000000.123,
        duration: 33.333,
        audio_metadata: None,
        video_metadata: Some(VideoMetadataPayload {
            sequence: 12345,
            codec: 1, // VP8
        }),
    }
}

/// ~160 byte Opus audio frame with audio metadata.
pub fn audio_media_packet() -> MediaPacketPayload {
    let mut rng = StdRng::seed_from_u64(43);
    let mut data = vec![0u8; 160];
    rng.fill(&mut data[..]);

    MediaPacketPayload {
        media_type: 2, // AUDIO
        email: "user@example.com".to_string(),
        data,
        frame_type: "opus".to_string(),
        timestamp: 1700000000.456,
        duration: 20.0,
        audio_metadata: Some(AudioMetadataPayload {
            audio_format: "opus".to_string(),
            channels: 1,
            frames: 960,
            sample_rate: 48000.0,
            sequence: 67890,
        }),
        video_metadata: None,
    }
}

/// PacketWrapper wrapping a video MediaPacket (data = pre-serialized bytes).
pub fn video_packet_wrapper(inner_bytes: Vec<u8>) -> PacketWrapperPayload {
    PacketWrapperPayload {
        packet_type: 3, // MEDIA
        email: "user@example.com".to_string(),
        data: inner_bytes,
        session_id: 999888777,
    }
}

/// HealthPacket with 5 peers, each having full stats.
pub fn health_packet_5_peers() -> HealthPacketPayload {
    let peers: Vec<(String, PeerStatsPayload)> = (0..5)
        .map(|i| {
            (
                format!("peer-{}@example.com", i),
                PeerStatsPayload {
                    can_listen: true,
                    can_see: i % 2 == 0,
                    audio_enabled: true,
                    video_enabled: i % 2 == 0,
                    neteq_stats: NetEqStatsPayload {
                        current_buffer_size_ms: 60.0 + i as f64,
                        packets_awaiting_decode: 2.0,
                        packets_per_sec: 50.0,
                        counters: NetEqCountersPayload {
                            normal_per_sec: 48.0,
                            expand_per_sec: 0.5,
                            accelerate_per_sec: 0.1,
                            fast_accelerate_per_sec: 0.0,
                            preemptive_expand_per_sec: 0.2,
                            merge_per_sec: 0.1,
                            comfort_noise_per_sec: 0.0,
                            dtmf_per_sec: 0.0,
                            undefined_per_sec: 0.0,
                        },
                    },
                    video_stats: VideoStatsPayload {
                        fps_received: 30.0,
                        frames_buffered: 3.0,
                        frames_decoded: 9000 + i as u64,
                        bitrate_kbps: 2500,
                    },
                },
            )
        })
        .collect();

    HealthPacketPayload {
        session_id: "session-abc-123".to_string(),
        meeting_id: "meeting-xyz-789".to_string(),
        reporting_peer: "host@example.com".to_string(),
        timestamp_ms: 1700000000000,
        reporting_audio_enabled: true,
        reporting_video_enabled: true,
        peer_stats: peers,
        active_server_url: "wss://sfu.example.com:443".to_string(),
        active_server_type: "webtransport".to_string(),
        active_server_rtt_ms: 15.5,
    }
}
