/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Shared test helpers for WebSocket and integration tests.
//! Only compiled when the `testing` feature is enabled.

use std::time::Duration;

use protobuf::Message as ProtobufMessage;

/// Wait for the MEETING_STARTED protobuf packet from the server.
pub async fn wait_for_meeting_started(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    timeout: Duration,
) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::Message;
    use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
    use videocall_types::protos::meeting_packet::MeetingPacket;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            msg = futures_util::StreamExt::next(ws) => {
                if let Some(Ok(Message::Binary(data))) = msg {
                    if let Ok(wrapper) = PacketWrapper::parse_from_bytes(&data) {
                        if wrapper.packet_type == PacketType::MEETING.into() {
                            if let Ok(meeting) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                                if meeting.event_type == MeetingEventType::MEETING_STARTED.into() {
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    anyhow::bail!("Timeout waiting for MEETING_STARTED")
}
