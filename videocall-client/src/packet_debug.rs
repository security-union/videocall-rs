//! Emit packet debug events for the diagnostics UI.

use protobuf::Message;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;

/// Build inner summary for display (best-effort, no decryption).
fn inner_summary(packet_type: PacketType, data: &[u8]) -> String {
    match packet_type {
        PacketType::MEDIA => {
            if let Ok(mp) = MediaPacket::parse_from_bytes(data) {
                format!("{:?}", mp.media_type)
            } else {
                format!("encrypted {}B", data.len())
            }
        }
        PacketType::MEETING => {
            if let Ok(mp) = MeetingPacket::parse_from_bytes(data) {
                let evt = mp
                    .event_type
                    .enum_value()
                    .unwrap_or(MeetingEventType::MEETING_EVENT_TYPE_UNKNOWN);
                format!("{:?} room={}", evt, mp.room_id)
            } else {
                format!("{}B", data.len())
            }
        }
        _ => format!("{}B", data.len()),
    }
}

/// Emit a packet debug event for the diagnostics sidebar.
pub fn emit_packet_debug(direction: &str, p: &PacketWrapper) {
    let packet_type = p
        .packet_type
        .enum_value()
        .unwrap_or(PacketType::PACKET_TYPE_UNKNOWN);
    let type_str = format!("{:?}", packet_type);
    let inner = inner_summary(packet_type, &p.data);

    let event = DiagEvent {
        subsystem: "packet_debug",
        stream_id: Some(direction.to_string()),
        ts_ms: now_ms(),
        metrics: vec![
            metric!("direction", direction),
            metric!("packet_type", type_str),
            metric!("email", p.email.clone()),
            metric!("session_id", p.session_id),
            metric!("size_bytes", p.data.len() as u64),
            metric!("inner", inner),
        ],
    };
    let _ = global_sender().try_broadcast(event);
}

/// Wrap an inbound packet callback to emit packet debug at receive time before forwarding.
/// Use when passing the callback to transports so all inbound packets are logged in one place.
pub fn wrap_inbound_with_packet_debug(cb: Callback<PacketWrapper>) -> Callback<PacketWrapper> {
    Callback::from(move |p: PacketWrapper| {
        emit_packet_debug("in", &p);
        cb.emit(p);
    })
}
