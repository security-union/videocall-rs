use capnp::message::{Builder, ReaderOptions};
use capnp::serialize;

use crate::health_packet_capnp;
use crate::media_packet_capnp;
use crate::packet_wrapper_capnp;
use crate::payloads::{HealthPacketPayload, MediaPacketPayload, PacketWrapperPayload};

// --- MediaPacket ---

pub fn encode_media_packet(p: &MediaPacketPayload) -> Vec<u8> {
    let mut builder = Builder::new_default();
    {
        let mut msg = builder.init_root::<media_packet_capnp::media_packet::Builder<'_>>();
        msg.set_media_type(match p.media_type {
            1 => media_packet_capnp::MediaType::Video,
            2 => media_packet_capnp::MediaType::Audio,
            3 => media_packet_capnp::MediaType::Screen,
            _ => media_packet_capnp::MediaType::Unknown,
        });
        msg.set_email(&p.email);
        msg.set_data(&p.data);
        msg.set_frame_type(&p.frame_type);
        msg.set_timestamp(p.timestamp);
        msg.set_duration(p.duration);

        if let Some(ref am) = p.audio_metadata {
            let mut meta = msg.reborrow().init_audio_metadata();
            meta.set_audio_format(&am.audio_format);
            meta.set_audio_number_of_channels(am.channels);
            meta.set_audio_number_of_frames(am.frames);
            meta.set_audio_sample_rate(am.sample_rate);
            meta.set_sequence(am.sequence);
        }
        if let Some(ref vm) = p.video_metadata {
            let mut meta = msg.reborrow().init_video_metadata();
            meta.set_sequence(vm.sequence);
            meta.set_codec(match vm.codec {
                1 => media_packet_capnp::VideoCodec::Vp8,
                2 => media_packet_capnp::VideoCodec::Vp9Profile0Level108bit,
                _ => media_packet_capnp::VideoCodec::Unspecified,
            });
        }
    }
    let mut output = Vec::new();
    serialize::write_message(&mut output, &builder).unwrap();
    output
}

/// Decode Cap'n Proto bytes, touching all fields for fair zero-copy comparison.
pub fn decode_media_packet(bytes: &[u8]) {
    let reader = serialize::read_message(&mut &bytes[..], ReaderOptions::default()).unwrap();
    let msg = reader
        .get_root::<media_packet_capnp::media_packet::Reader<'_>>()
        .unwrap();

    let _ = msg.get_media_type();
    let _ = msg.get_email().unwrap();
    let _ = msg.get_data().unwrap();
    let _ = msg.get_frame_type().unwrap();
    let _ = msg.get_timestamp();
    let _ = msg.get_duration();
    if msg.has_audio_metadata() {
        let am = msg.get_audio_metadata().unwrap();
        let _ = am.get_audio_format().unwrap();
        let _ = am.get_audio_number_of_channels();
        let _ = am.get_audio_number_of_frames();
        let _ = am.get_audio_sample_rate();
        let _ = am.get_sequence();
    }
    if msg.has_video_metadata() {
        let vm = msg.get_video_metadata().unwrap();
        let _ = vm.get_sequence();
        let _ = vm.get_codec();
    }
    if msg.has_heartbeat_metadata() {
        let hm = msg.get_heartbeat_metadata().unwrap();
        let _ = hm.get_video_enabled();
        let _ = hm.get_audio_enabled();
        let _ = hm.get_screen_enabled();
    }
}

// --- PacketWrapper ---

pub fn encode_packet_wrapper(p: &PacketWrapperPayload) -> Vec<u8> {
    let mut builder = Builder::new_default();
    {
        let mut msg = builder.init_root::<packet_wrapper_capnp::packet_wrapper::Builder<'_>>();
        msg.set_packet_type(match p.packet_type {
            1 => packet_wrapper_capnp::PacketType::RsaPubKey,
            2 => packet_wrapper_capnp::PacketType::AesKey,
            3 => packet_wrapper_capnp::PacketType::Media,
            4 => packet_wrapper_capnp::PacketType::Connection,
            _ => packet_wrapper_capnp::PacketType::Unknown,
        });
        msg.set_email(&p.email);
        msg.set_data(&p.data);
        msg.set_session_id(p.session_id);
    }
    let mut output = Vec::new();
    serialize::write_message(&mut output, &builder).unwrap();
    output
}

pub fn decode_packet_wrapper(bytes: &[u8]) {
    let reader = serialize::read_message(&mut &bytes[..], ReaderOptions::default()).unwrap();
    let msg = reader
        .get_root::<packet_wrapper_capnp::packet_wrapper::Reader<'_>>()
        .unwrap();
    let _ = msg.get_packet_type();
    let _ = msg.get_email().unwrap();
    let _ = msg.get_data().unwrap();
    let _ = msg.get_session_id();
}

// --- HealthPacket ---

pub fn encode_health_packet(p: &HealthPacketPayload) -> Vec<u8> {
    let mut builder = Builder::new_default();
    {
        let mut msg = builder.init_root::<health_packet_capnp::health_packet::Builder<'_>>();
        msg.set_session_id(&p.session_id);
        msg.set_meeting_id(&p.meeting_id);
        msg.set_reporting_peer(&p.reporting_peer);
        msg.set_timestamp_ms(p.timestamp_ms);
        msg.set_reporting_audio_enabled(p.reporting_audio_enabled);
        msg.set_reporting_video_enabled(p.reporting_video_enabled);
        msg.set_active_server_url(&p.active_server_url);
        msg.set_active_server_type(&p.active_server_type);
        msg.set_active_server_rtt_ms(p.active_server_rtt_ms);

        let mut peers = msg.reborrow().init_peer_stats(p.peer_stats.len() as u32);
        for (i, (key, ps)) in p.peer_stats.iter().enumerate() {
            let mut entry = peers.reborrow().get(i as u32);
            entry.set_key(key);
            let mut value = entry.init_value();
            value.set_can_listen(ps.can_listen);
            value.set_can_see(ps.can_see);
            value.set_audio_enabled(ps.audio_enabled);
            value.set_video_enabled(ps.video_enabled);

            let mut neteq = value.reborrow().init_neteq_stats();
            neteq.set_current_buffer_size_ms(ps.neteq_stats.current_buffer_size_ms);
            neteq.set_packets_awaiting_decode(ps.neteq_stats.packets_awaiting_decode);
            neteq.set_packets_per_sec(ps.neteq_stats.packets_per_sec);

            let network = neteq.reborrow().init_network();
            let mut counters = network.init_operation_counters();
            counters.set_normal_per_sec(ps.neteq_stats.counters.normal_per_sec);
            counters.set_expand_per_sec(ps.neteq_stats.counters.expand_per_sec);
            counters.set_accelerate_per_sec(ps.neteq_stats.counters.accelerate_per_sec);
            counters.set_fast_accelerate_per_sec(ps.neteq_stats.counters.fast_accelerate_per_sec);
            counters
                .set_preemptive_expand_per_sec(ps.neteq_stats.counters.preemptive_expand_per_sec);
            counters.set_merge_per_sec(ps.neteq_stats.counters.merge_per_sec);
            counters.set_comfort_noise_per_sec(ps.neteq_stats.counters.comfort_noise_per_sec);
            counters.set_dtmf_per_sec(ps.neteq_stats.counters.dtmf_per_sec);
            counters.set_undefined_per_sec(ps.neteq_stats.counters.undefined_per_sec);

            let mut vs = value.init_video_stats();
            vs.set_fps_received(ps.video_stats.fps_received);
            vs.set_frames_buffered(ps.video_stats.frames_buffered);
            vs.set_frames_decoded(ps.video_stats.frames_decoded);
            vs.set_bitrate_kbps(ps.video_stats.bitrate_kbps);
        }
    }
    let mut output = Vec::new();
    serialize::write_message(&mut output, &builder).unwrap();
    output
}

pub fn decode_health_packet(bytes: &[u8]) {
    let reader = serialize::read_message(&mut &bytes[..], ReaderOptions::default()).unwrap();
    let msg = reader
        .get_root::<health_packet_capnp::health_packet::Reader<'_>>()
        .unwrap();
    let _ = msg.get_session_id().unwrap();
    let _ = msg.get_meeting_id().unwrap();
    let _ = msg.get_reporting_peer().unwrap();
    let _ = msg.get_timestamp_ms();
    let _ = msg.get_reporting_audio_enabled();
    let _ = msg.get_reporting_video_enabled();
    let _ = msg.get_active_server_url().unwrap();
    let _ = msg.get_active_server_type().unwrap();
    let _ = msg.get_active_server_rtt_ms();

    let peers = msg.get_peer_stats().unwrap();
    for entry in peers.iter() {
        let _ = entry.get_key().unwrap();
        let value = entry.get_value().unwrap();
        let _ = value.get_can_listen();
        let _ = value.get_can_see();
        let _ = value.get_audio_enabled();
        let _ = value.get_video_enabled();

        let neteq = value.get_neteq_stats().unwrap();
        let _ = neteq.get_current_buffer_size_ms();
        let _ = neteq.get_packets_awaiting_decode();
        let _ = neteq.get_packets_per_sec();
        let network = neteq.get_network().unwrap();
        let counters = network.get_operation_counters().unwrap();
        let _ = counters.get_normal_per_sec();
        let _ = counters.get_expand_per_sec();
        let _ = counters.get_accelerate_per_sec();
        let _ = counters.get_fast_accelerate_per_sec();
        let _ = counters.get_preemptive_expand_per_sec();
        let _ = counters.get_merge_per_sec();
        let _ = counters.get_comfort_noise_per_sec();
        let _ = counters.get_dtmf_per_sec();
        let _ = counters.get_undefined_per_sec();

        let vs = value.get_video_stats().unwrap();
        let _ = vs.get_fps_received();
        let _ = vs.get_frames_buffered();
        let _ = vs.get_frames_decoded();
        let _ = vs.get_bitrate_kbps();
    }
}
