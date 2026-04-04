use flatbuffers::FlatBufferBuilder;

use crate::fb_health::videocall::bench as fb_health;
use crate::fb_media::videocall::bench as fb_media;
use crate::fb_packet_wrapper::videocall::bench as fb_pw;
use crate::payloads::{HealthPacketPayload, MediaPacketPayload, PacketWrapperPayload};

// --- MediaPacket ---

pub fn encode_media_packet(p: &MediaPacketPayload) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::with_capacity(p.data.len() + 512);

    let email = builder.create_string(&p.email);
    let data = builder.create_vector(&p.data);
    let frame_type = builder.create_string(&p.frame_type);

    let audio_metadata = p.audio_metadata.as_ref().map(|am| {
        let fmt = builder.create_string(&am.audio_format);
        fb_media::AudioMetadata::create(
            &mut builder,
            &fb_media::AudioMetadataArgs {
                audio_format: Some(fmt),
                audio_number_of_channels: am.channels,
                audio_number_of_frames: am.frames,
                audio_sample_rate: am.sample_rate,
                sequence: am.sequence,
            },
        )
    });

    let video_metadata = p.video_metadata.as_ref().map(|vm| {
        fb_media::VideoMetadata::create(
            &mut builder,
            &fb_media::VideoMetadataArgs {
                sequence: vm.sequence,
                codec: match vm.codec {
                    1 => fb_media::VideoCodec::Vp8,
                    2 => fb_media::VideoCodec::Vp9Profile0Level10_8bit,
                    _ => fb_media::VideoCodec::Unspecified,
                },
            },
        )
    });

    let media_type = match p.media_type {
        1 => fb_media::MediaType::Video,
        2 => fb_media::MediaType::Audio,
        3 => fb_media::MediaType::Screen,
        _ => fb_media::MediaType::Unknown,
    };

    let packet = fb_media::MediaPacket::create(
        &mut builder,
        &fb_media::MediaPacketArgs {
            media_type,
            email: Some(email),
            data: Some(data),
            frame_type: Some(frame_type),
            timestamp: p.timestamp,
            duration: p.duration,
            audio_metadata,
            video_metadata,
            heartbeat_metadata: None,
        },
    );
    builder.finish(packet, None);
    builder.finished_data().to_vec()
}

/// Decode FlatBuffers bytes with verification, touching all fields.
pub fn decode_media_packet(bytes: &[u8]) {
    let msg = flatbuffers::root::<fb_media::MediaPacket>(bytes).unwrap();
    let _ = msg.media_type();
    let _ = msg.email();
    let _ = msg.data();
    let _ = msg.frame_type();
    let _ = msg.timestamp();
    let _ = msg.duration();
    if let Some(am) = msg.audio_metadata() {
        let _ = am.audio_format();
        let _ = am.audio_number_of_channels();
        let _ = am.audio_number_of_frames();
        let _ = am.audio_sample_rate();
        let _ = am.sequence();
    }
    if let Some(vm) = msg.video_metadata() {
        let _ = vm.sequence();
        let _ = vm.codec();
    }
    if let Some(hm) = msg.heartbeat_metadata() {
        let _ = hm.video_enabled();
        let _ = hm.audio_enabled();
        let _ = hm.screen_enabled();
    }
}

/// Decode FlatBuffers bytes WITHOUT verification (unsafe, shows raw access speed).
pub fn decode_media_packet_unchecked(bytes: &[u8]) {
    let msg = unsafe { flatbuffers::root_unchecked::<fb_media::MediaPacket>(bytes) };
    let _ = msg.media_type();
    let _ = msg.email();
    let _ = msg.data();
    let _ = msg.frame_type();
    let _ = msg.timestamp();
    let _ = msg.duration();
    if let Some(am) = msg.audio_metadata() {
        let _ = am.audio_format();
        let _ = am.audio_number_of_channels();
        let _ = am.audio_number_of_frames();
        let _ = am.audio_sample_rate();
        let _ = am.sequence();
    }
    if let Some(vm) = msg.video_metadata() {
        let _ = vm.sequence();
        let _ = vm.codec();
    }
}

// --- PacketWrapper ---

pub fn encode_packet_wrapper(p: &PacketWrapperPayload) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::with_capacity(p.data.len() + 256);
    let email = builder.create_string(&p.email);
    let data = builder.create_vector(&p.data);

    let packet_type = match p.packet_type {
        1 => fb_pw::PacketType::RsaPubKey,
        2 => fb_pw::PacketType::AesKey,
        3 => fb_pw::PacketType::Media,
        4 => fb_pw::PacketType::Connection,
        _ => fb_pw::PacketType::Unknown,
    };

    let packet = fb_pw::PacketWrapper::create(
        &mut builder,
        &fb_pw::PacketWrapperArgs {
            packet_type,
            email: Some(email),
            data: Some(data),
            session_id: p.session_id,
        },
    );
    builder.finish(packet, None);
    builder.finished_data().to_vec()
}

pub fn decode_packet_wrapper(bytes: &[u8]) {
    let msg = flatbuffers::root::<fb_pw::PacketWrapper>(bytes).unwrap();
    let _ = msg.packet_type();
    let _ = msg.email();
    let _ = msg.data();
    let _ = msg.session_id();
}

// --- HealthPacket ---

pub fn encode_health_packet(p: &HealthPacketPayload) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::with_capacity(4096);

    // Build peer stats entries (must create inner-out for FlatBuffers)
    let peer_entries: Vec<_> = p
        .peer_stats
        .iter()
        .map(|(key, ps)| {
            let key = builder.create_string(key);

            let counters = fb_health::NetEqOperationCounters::create(
                &mut builder,
                &fb_health::NetEqOperationCountersArgs {
                    normal_per_sec: ps.neteq_stats.counters.normal_per_sec,
                    expand_per_sec: ps.neteq_stats.counters.expand_per_sec,
                    accelerate_per_sec: ps.neteq_stats.counters.accelerate_per_sec,
                    fast_accelerate_per_sec: ps.neteq_stats.counters.fast_accelerate_per_sec,
                    preemptive_expand_per_sec: ps.neteq_stats.counters.preemptive_expand_per_sec,
                    merge_per_sec: ps.neteq_stats.counters.merge_per_sec,
                    comfort_noise_per_sec: ps.neteq_stats.counters.comfort_noise_per_sec,
                    dtmf_per_sec: ps.neteq_stats.counters.dtmf_per_sec,
                    undefined_per_sec: ps.neteq_stats.counters.undefined_per_sec,
                },
            );

            let network = fb_health::NetEqNetwork::create(
                &mut builder,
                &fb_health::NetEqNetworkArgs {
                    operation_counters: Some(counters),
                },
            );

            let neteq = fb_health::NetEqStats::create(
                &mut builder,
                &fb_health::NetEqStatsArgs {
                    current_buffer_size_ms: ps.neteq_stats.current_buffer_size_ms,
                    packets_awaiting_decode: ps.neteq_stats.packets_awaiting_decode,
                    network: Some(network),
                    packets_per_sec: ps.neteq_stats.packets_per_sec,
                },
            );

            let video_stats = fb_health::VideoStats::create(
                &mut builder,
                &fb_health::VideoStatsArgs {
                    fps_received: ps.video_stats.fps_received,
                    frames_buffered: ps.video_stats.frames_buffered,
                    frames_decoded: ps.video_stats.frames_decoded,
                    bitrate_kbps: ps.video_stats.bitrate_kbps,
                },
            );

            let value = fb_health::PeerStats::create(
                &mut builder,
                &fb_health::PeerStatsArgs {
                    can_listen: ps.can_listen,
                    can_see: ps.can_see,
                    audio_enabled: ps.audio_enabled,
                    video_enabled: ps.video_enabled,
                    neteq_stats: Some(neteq),
                    video_stats: Some(video_stats),
                },
            );

            fb_health::PeerStatsEntry::create(
                &mut builder,
                &fb_health::PeerStatsEntryArgs {
                    key: Some(key),
                    value: Some(value),
                },
            )
        })
        .collect();

    let peers_vec = builder.create_vector(&peer_entries);
    let session_id = builder.create_string(&p.session_id);
    let meeting_id = builder.create_string(&p.meeting_id);
    let reporting_peer = builder.create_string(&p.reporting_peer);
    let active_server_url = builder.create_string(&p.active_server_url);
    let active_server_type = builder.create_string(&p.active_server_type);

    let packet = fb_health::HealthPacket::create(
        &mut builder,
        &fb_health::HealthPacketArgs {
            session_id: Some(session_id),
            meeting_id: Some(meeting_id),
            reporting_peer: Some(reporting_peer),
            timestamp_ms: p.timestamp_ms,
            reporting_audio_enabled: p.reporting_audio_enabled,
            reporting_video_enabled: p.reporting_video_enabled,
            peer_stats: Some(peers_vec),
            active_server_url: Some(active_server_url),
            active_server_type: Some(active_server_type),
            active_server_rtt_ms: p.active_server_rtt_ms,
        },
    );
    builder.finish(packet, None);
    builder.finished_data().to_vec()
}

pub fn decode_health_packet(bytes: &[u8]) {
    let msg = flatbuffers::root::<fb_health::HealthPacket>(bytes).unwrap();
    let _ = msg.session_id();
    let _ = msg.meeting_id();
    let _ = msg.reporting_peer();
    let _ = msg.timestamp_ms();
    let _ = msg.reporting_audio_enabled();
    let _ = msg.reporting_video_enabled();
    let _ = msg.active_server_url();
    let _ = msg.active_server_type();
    let _ = msg.active_server_rtt_ms();

    if let Some(peers) = msg.peer_stats() {
        for entry in peers.iter() {
            let _ = entry.key();
            if let Some(value) = entry.value() {
                let _ = value.can_listen();
                let _ = value.can_see();
                let _ = value.audio_enabled();
                let _ = value.video_enabled();

                if let Some(neteq) = value.neteq_stats() {
                    let _ = neteq.current_buffer_size_ms();
                    let _ = neteq.packets_awaiting_decode();
                    let _ = neteq.packets_per_sec();
                    if let Some(network) = neteq.network() {
                        if let Some(counters) = network.operation_counters() {
                            let _ = counters.normal_per_sec();
                            let _ = counters.expand_per_sec();
                            let _ = counters.accelerate_per_sec();
                            let _ = counters.fast_accelerate_per_sec();
                            let _ = counters.preemptive_expand_per_sec();
                            let _ = counters.merge_per_sec();
                            let _ = counters.comfort_noise_per_sec();
                            let _ = counters.dtmf_per_sec();
                            let _ = counters.undefined_per_sec();
                        }
                    }
                }
                if let Some(vs) = value.video_stats() {
                    let _ = vs.fps_received();
                    let _ = vs.frames_buffered();
                    let _ = vs.frames_decoded();
                    let _ = vs.bitrate_kbps();
                }
            }
        }
    }
}
