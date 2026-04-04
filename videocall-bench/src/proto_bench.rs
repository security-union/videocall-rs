use protobuf::{EnumOrUnknown, Message, MessageField};
use videocall_types::protos::health_packet::{
    HealthPacket, NetEqNetwork, NetEqOperationCounters, NetEqStats, PeerStats, VideoStats,
};
use videocall_types::protos::media_packet::{
    media_packet::MediaType, AudioMetadata, MediaPacket, VideoMetadata,
};
use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

use crate::payloads::{HealthPacketPayload, MediaPacketPayload, PacketWrapperPayload};

// --- MediaPacket ---

pub fn build_media_packet(p: &MediaPacketPayload) -> MediaPacket {
    let mut msg = MediaPacket::new();
    msg.media_type = EnumOrUnknown::new(match p.media_type {
        1 => MediaType::VIDEO,
        2 => MediaType::AUDIO,
        3 => MediaType::SCREEN,
        _ => MediaType::MEDIA_TYPE_UNKNOWN,
    });
    msg.email = p.email.clone();
    msg.data = p.data.clone();
    msg.frame_type = p.frame_type.clone();
    msg.timestamp = p.timestamp;
    msg.duration = p.duration;

    if let Some(ref am) = p.audio_metadata {
        let mut meta = AudioMetadata::new();
        meta.audio_format = am.audio_format.clone();
        meta.audio_number_of_channels = am.channels;
        meta.audio_number_of_frames = am.frames;
        meta.audio_sample_rate = am.sample_rate;
        meta.sequence = am.sequence;
        msg.audio_metadata = MessageField::some(meta);
    }
    if let Some(ref vm) = p.video_metadata {
        let mut meta = VideoMetadata::new();
        meta.sequence = vm.sequence;
        meta.codec = EnumOrUnknown::from_i32(vm.codec);
        msg.video_metadata = MessageField::some(meta);
    }
    msg
}

pub fn encode_media_packet(msg: &MediaPacket) -> Vec<u8> {
    msg.write_to_bytes().unwrap()
}

pub fn decode_media_packet(bytes: &[u8]) -> MediaPacket {
    MediaPacket::parse_from_bytes(bytes).unwrap()
}

// --- PacketWrapper ---

pub fn build_packet_wrapper(p: &PacketWrapperPayload) -> PacketWrapper {
    let mut msg = PacketWrapper::new();
    msg.packet_type = EnumOrUnknown::new(match p.packet_type {
        1 => PacketType::RSA_PUB_KEY,
        2 => PacketType::AES_KEY,
        3 => PacketType::MEDIA,
        4 => PacketType::CONNECTION,
        _ => PacketType::PACKET_TYPE_UNKNOWN,
    });
    msg.email = p.email.clone();
    msg.data = p.data.clone();
    msg.session_id = p.session_id;
    msg
}

pub fn encode_packet_wrapper(msg: &PacketWrapper) -> Vec<u8> {
    msg.write_to_bytes().unwrap()
}

pub fn decode_packet_wrapper(bytes: &[u8]) -> PacketWrapper {
    PacketWrapper::parse_from_bytes(bytes).unwrap()
}

// --- HealthPacket ---

pub fn build_health_packet(p: &HealthPacketPayload) -> HealthPacket {
    let mut msg = HealthPacket::new();
    msg.session_id = p.session_id.clone();
    msg.meeting_id = p.meeting_id.clone();
    msg.reporting_peer = p.reporting_peer.clone();
    msg.timestamp_ms = p.timestamp_ms;
    msg.reporting_audio_enabled = p.reporting_audio_enabled;
    msg.reporting_video_enabled = p.reporting_video_enabled;
    msg.active_server_url = p.active_server_url.clone();
    msg.active_server_type = p.active_server_type.clone();
    msg.active_server_rtt_ms = p.active_server_rtt_ms;

    for (key, ps) in &p.peer_stats {
        let mut peer = PeerStats::new();
        peer.can_listen = ps.can_listen;
        peer.can_see = ps.can_see;
        peer.audio_enabled = ps.audio_enabled;
        peer.video_enabled = ps.video_enabled;

        let mut neteq = NetEqStats::new();
        neteq.current_buffer_size_ms = ps.neteq_stats.current_buffer_size_ms;
        neteq.packets_awaiting_decode = ps.neteq_stats.packets_awaiting_decode;
        neteq.packets_per_sec = ps.neteq_stats.packets_per_sec;

        let mut network = NetEqNetwork::new();
        let mut counters = NetEqOperationCounters::new();
        counters.normal_per_sec = ps.neteq_stats.counters.normal_per_sec;
        counters.expand_per_sec = ps.neteq_stats.counters.expand_per_sec;
        counters.accelerate_per_sec = ps.neteq_stats.counters.accelerate_per_sec;
        counters.fast_accelerate_per_sec = ps.neteq_stats.counters.fast_accelerate_per_sec;
        counters.preemptive_expand_per_sec = ps.neteq_stats.counters.preemptive_expand_per_sec;
        counters.merge_per_sec = ps.neteq_stats.counters.merge_per_sec;
        counters.comfort_noise_per_sec = ps.neteq_stats.counters.comfort_noise_per_sec;
        counters.dtmf_per_sec = ps.neteq_stats.counters.dtmf_per_sec;
        counters.undefined_per_sec = ps.neteq_stats.counters.undefined_per_sec;
        network.operation_counters = MessageField::some(counters);
        neteq.network = MessageField::some(network);
        peer.neteq_stats = MessageField::some(neteq);

        let mut vs = VideoStats::new();
        vs.fps_received = ps.video_stats.fps_received;
        vs.frames_buffered = ps.video_stats.frames_buffered;
        vs.frames_decoded = ps.video_stats.frames_decoded;
        vs.bitrate_kbps = ps.video_stats.bitrate_kbps;
        peer.video_stats = MessageField::some(vs);

        msg.peer_stats.insert(key.clone(), peer);
    }
    msg
}

pub fn encode_health_packet(msg: &HealthPacket) -> Vec<u8> {
    msg.write_to_bytes().unwrap()
}

pub fn decode_health_packet(bytes: &[u8]) -> HealthPacket {
    HealthPacket::parse_from_bytes(bytes).unwrap()
}
