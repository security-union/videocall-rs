use chrono::{Local, Utc};
use protobuf::Message;
use videocall_types::protos::connection_packet::ConnectionPacket;
use videocall_types::protos::health_packet::HealthPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, VideoCodec};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::user_id_bytes_to_string;

pub fn handle_packet(raw: &[u8], verbose: bool, use_utc: bool) {
    let pkt = match PacketWrapper::parse_from_bytes(raw) {
        Ok(p) => p,
        Err(e) => {
            if verbose {
                println!("? parse error: {} ({} bytes)", e, raw.len());
            }
            return;
        }
    };

    let pt = pkt
        .packet_type
        .enum_value()
        .unwrap_or(PacketType::PACKET_TYPE_UNKNOWN);

    match pt {
        PacketType::MEDIA => {
            if let Ok(media) = MediaPacket::parse_from_bytes(&pkt.data) {
                print_media(&media, &pkt.user_id, verbose, use_utc);
            } else if verbose {
                println!(
                    "? MEDIA parse error from {} ({} bytes)",
                    user_id_bytes_to_string(&pkt.user_id),
                    raw.len()
                );
            }
        }
        PacketType::CONNECTION => {
            let meeting = ConnectionPacket::parse_from_bytes(&pkt.data)
                .map(|c| c.meeting_id)
                .unwrap_or_default();
            let uid = user_id_bytes_to_string(&pkt.user_id);
            if meeting.is_empty() {
                println!("← {} joined", uid);
            } else {
                println!("← {} joined (meeting={})", uid, meeting);
            }
        }
        PacketType::SESSION_ASSIGNED => {
            println!("* session established (id={})", pkt.session_id);
        }
        PacketType::DIAGNOSTICS => {
            if verbose {
                println!(
                    "[{}] DIAG          {:<16} {:>6} B",
                    ts(use_utc),
                    user_id_bytes_to_string(&pkt.user_id),
                    raw.len()
                );
            }
        }
        PacketType::HEALTH => {
            if let Ok(health) = HealthPacket::parse_from_bytes(&pkt.data) {
                print_health(&health, verbose, use_utc);
            } else if verbose {
                println!(
                    "? HEALTH parse error from {} ({} bytes)",
                    user_id_bytes_to_string(&pkt.user_id),
                    raw.len()
                );
            }
        }
        PacketType::MEETING => {
            if verbose {
                println!("[{}] MEETING       {:>6} B", ts(use_utc), raw.len());
            }
        }
        PacketType::RSA_PUB_KEY | PacketType::AES_KEY => {
            // Crypto handshake from browser clients — skip silently unless verbose
            if verbose {
                println!(
                    "[{}] CRYPTO        {:<16} {:>6} B  ({})",
                    ts(use_utc),
                    user_id_bytes_to_string(&pkt.user_id),
                    raw.len(),
                    pkt.packet_type.value()
                );
            }
        }
        _ => {
            if verbose {
                println!(
                    "? unknown packet type {} ({} bytes)",
                    pkt.packet_type.value(),
                    raw.len()
                );
            }
        }
    }
}

fn print_media(media: &MediaPacket, wrapper_user_id: &[u8], verbose: bool, use_utc: bool) {
    let now = ts(use_utc);
    let sender_str;
    let sender = if !media.user_id.is_empty() {
        sender_str = user_id_bytes_to_string(&media.user_id);
        &sender_str
    } else if !wrapper_user_id.is_empty() {
        sender_str = user_id_bytes_to_string(wrapper_user_id);
        &sender_str
    } else {
        "?"
    };
    let size = media.data.len();
    let mt = media
        .media_type
        .enum_value()
        .unwrap_or(MediaType::MEDIA_TYPE_UNKNOWN);

    match mt {
        MediaType::VIDEO | MediaType::SCREEN => {
            let kind = if mt == MediaType::VIDEO {
                "VIDEO "
            } else {
                "SCREEN"
            };
            let frame = if media.frame_type == "key" {
                "key  "
            } else {
                "delta"
            };
            let codec = video_codec_str(media);
            let seq = media
                .video_metadata
                .as_ref()
                .map(|m| m.sequence)
                .unwrap_or(0);
            if verbose {
                println!(
                    "[{}] {}  {}  {:<16} {:>6} B  {}  seq={}",
                    now, kind, frame, sender, size, codec, seq
                );
            } else {
                println!(
                    "[{}] {}  {}  {:<16} {:>6} B  {}",
                    now, kind, frame, sender, size, codec
                );
            }
        }
        MediaType::AUDIO => {
            let channels = media
                .audio_metadata
                .as_ref()
                .map(|m| m.audio_number_of_channels)
                .unwrap_or(0);
            let sample_rate = media
                .audio_metadata
                .as_ref()
                .map(|m| m.audio_sample_rate as u32)
                .unwrap_or(0);
            let seq = media
                .audio_metadata
                .as_ref()
                .map(|m| m.sequence)
                .unwrap_or(0);
            if verbose {
                println!(
                    "[{}] AUDIO         {:<16} {:>6} B  {}Hz/{}ch  seq={}",
                    now, sender, size, sample_rate, channels, seq
                );
            } else {
                println!(
                    "[{}] AUDIO         {:<16} {:>6} B  {}Hz/{}ch",
                    now, sender, size, sample_rate, channels
                );
            }
        }
        MediaType::HEARTBEAT => {
            if let Some(hb) = media.heartbeat_metadata.as_ref() {
                let video = if hb.video_enabled { "on " } else { "off" };
                let audio = if hb.audio_enabled { "on " } else { "off" };
                let screen = if hb.screen_enabled { "on" } else { "off" };
                println!(
                    "[{}] HEARTBEAT     {:<16} video={} audio={} screen={}",
                    now, sender, video, audio, screen
                );
            } else {
                println!("[{}] HEARTBEAT     {}", now, sender);
            }
        }
        MediaType::RTT => {
            println!("[{}] RTT           {:<16}", now, sender);
        }
        _ => {
            if verbose {
                println!("[{}] UNKNOWN_MEDIA {:<16} {:>6} B", now, sender, size);
            }
        }
    }
}

fn print_health(health: &HealthPacket, verbose: bool, use_utc: bool) {
    let now = ts(use_utc);
    let peer_count = health.peer_stats.len();
    let rtt = health.active_server_rtt_ms;

    if verbose {
        // Verbose mode: show full details including browser state
        let tab = if health.is_tab_visible {
            "visible"
        } else {
            "hidden "
        };
        let mem_info = match (health.memory_used_bytes, health.memory_total_bytes) {
            (Some(used), Some(total)) => {
                format!(" mem={}MB/{}MB", used / 1_000_000, total / 1_000_000)
            }
            (Some(used), None) => format!(" mem={}MB", used / 1_000_000),
            _ => String::new(),
        };

        let reporter = user_id_bytes_to_string(&health.reporting_user_id);
        println!(
            "[{}] HEALTH        {:<16} session={} peers={} rtt={:.1}ms {} {}{}",
            now,
            reporter,
            health.session_id,
            peer_count,
            rtt,
            tab,
            health.active_server_type,
            mem_info
        );

        if peer_count == 0 {
            println!("                      └─ (no peer data)");
        }

        for (peer_id, stats) in &health.peer_stats {
            let jitter = stats
                .neteq_stats
                .as_ref()
                .map(|n| n.target_delay_ms)
                .unwrap_or(0.0);
            let fps = stats
                .video_stats
                .as_ref()
                .map(|v| v.fps_received)
                .unwrap_or(0.0);
            let kbps = stats
                .video_stats
                .as_ref()
                .map(|v| v.bitrate_kbps)
                .unwrap_or(0);
            // Audio concealment and decode errors
            let audio_loss = if stats.audio_concealment_pct > 0.01 {
                format!(" conceal={:.1}%", stats.audio_concealment_pct)
            } else {
                String::new()
            };
            let dropped = if stats.frames_dropped_per_sec > 0.01 {
                format!(" decerr={:.1}/s", stats.frames_dropped_per_sec)
            } else {
                String::new()
            };

            println!(
                "                      ├─ {:<16} jitter={:.0}ms fps={:.0} {}kbps{}{}",
                peer_id, jitter, fps, kbps, audio_loss, dropped
            );
        }
    } else {
        // Normal mode: one-line summary
        println!(
            "[{}] HEALTH        {:<16} session={} peers={} rtt={:.1}ms",
            now,
            user_id_bytes_to_string(&health.reporting_user_id),
            health.session_id,
            peer_count,
            rtt
        );
    }
}

fn video_codec_str(media: &MediaPacket) -> &'static str {
    match media
        .video_metadata
        .as_ref()
        .and_then(|m| m.codec.enum_value().ok())
    {
        Some(VideoCodec::VP8) => "VP8",
        Some(VideoCodec::VP9_PROFILE0_LEVEL10_8BIT) => "VP9",
        _ => "?",
    }
}

fn ts(use_utc: bool) -> String {
    if use_utc {
        Utc::now().format("%H:%M:%S%.3f UTC").to_string()
    } else {
        Local::now().format("%H:%M:%S%.3f %Z").to_string()
    }
}
