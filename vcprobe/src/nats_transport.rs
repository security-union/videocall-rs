use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use protobuf::Message;
use tokio::sync::mpsc;
use videocall_types::protos::health_packet::HealthPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::user_id_bytes_to_string;

/// Connect to NATS server and subscribe to health diagnostics and room packets
/// for the specified meeting ID.
///
/// Returns a channel receiver that delivers PacketWrapper bytes, compatible
/// with the existing WebTransport/WebSocket transport interface.
pub async fn connect_nats(nats_url: String, meeting_id: String) -> Result<mpsc::Receiver<Vec<u8>>> {
    // Connect to NATS
    let client = async_nats::connect(&nats_url).await?;
    log::info!("Connected to NATS at {}", nats_url);

    // Create channel for merged packet stream
    let (tx, rx) = mpsc::channel::<Vec<u8>>(256);

    // Spawn health diagnostics subscriber
    {
        let client = client.clone();
        let meeting_id = meeting_id.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = health_subscriber(client, meeting_id, tx).await {
                log::error!("Health subscriber error: {}", e);
            }
        });
    }

    // Spawn room subscriber
    {
        let client = client.clone();
        let meeting_id = meeting_id.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = room_subscriber(client, meeting_id, tx).await {
                log::error!("Room subscriber error: {}", e);
            }
        });
    }

    Ok(rx)
}

/// Subscribe to health diagnostics from all regions/servers, filter by meeting_id
async fn health_subscriber(
    client: async_nats::Client,
    meeting_id: String,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    use futures_util::StreamExt;

    let subject = "health.diagnostics.>";
    let mut subscription = client.subscribe(subject).await?;
    log::info!("Subscribed to NATS subject: {}", subject);

    while let Some(message) = subscription.next().await {
        log::debug!("Received message on subject: {}", message.subject);

        // Parse HEALTH packet (published as raw HealthPacket bytes, NOT wrapped)
        let health_packet = match HealthPacket::parse_from_bytes(&message.payload) {
            Ok(h) => h,
            Err(e) => {
                log::debug!(
                    "Failed to parse health packet from {}: {}",
                    message.subject,
                    e
                );
                continue;
            }
        };

        log::debug!(
            "Parsed health packet: meeting_id='{}', reporting_user='{}', target_meeting='{}'",
            health_packet.meeting_id,
            user_id_bytes_to_string(&health_packet.reporting_user_id),
            meeting_id
        );

        // Filter by meeting_id
        if health_packet.meeting_id != meeting_id {
            log::debug!(
                "Filtering out health packet: meeting_id '{}' != target '{}'",
                health_packet.meeting_id,
                meeting_id
            );
            continue;
        }

        // Stale check: discard packets older than 30 seconds
        if let Some(age_ms) = get_packet_age_ms(health_packet.timestamp_ms) {
            if age_ms > 30_000 {
                log::debug!(
                    "Discarding stale health packet (age: {}ms) for meeting {}",
                    age_ms,
                    meeting_id
                );
                continue;
            }
        }

        log::info!(
            "Forwarding HEALTH packet from {} for meeting {}",
            user_id_bytes_to_string(&health_packet.reporting_user_id),
            meeting_id
        );

        // Wrap in PacketWrapper for consistency with state.rs expectations
        let wrapper = PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            user_id: health_packet.reporting_user_id.clone(),
            data: message.payload.to_vec(), // Original bytes
            ..Default::default()
        };

        let wrapper_bytes = match wrapper.write_to_bytes() {
            Ok(b) => b,
            Err(e) => {
                log::error!("Failed to serialize PacketWrapper: {}", e);
                continue;
            }
        };

        // Send to channel; break if receiver dropped
        if tx.send(wrapper_bytes).await.is_err() {
            log::debug!("Health subscriber: receiver dropped, exiting");
            break;
        }
    }

    Ok(())
}

/// Subscribe to room packets (already wrapped in PacketWrapper)
async fn room_subscriber(
    client: async_nats::Client,
    meeting_id: String,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    use futures_util::StreamExt;

    let subject = format!("room.{}.*", meeting_id);
    let mut subscription = client.subscribe(subject.clone()).await?;
    log::info!("Subscribed to NATS subject: {}", subject);

    while let Some(message) = subscription.next().await {
        // Room packets are already PacketWrapper bytes, forward as-is
        if tx.send(message.payload.to_vec()).await.is_err() {
            log::debug!("Room subscriber: receiver dropped, exiting");
            break;
        }
    }

    Ok(())
}

/// Calculate packet age in milliseconds, return None if timestamp is invalid
fn get_packet_age_ms(timestamp_ms: u64) -> Option<u64> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;

    Some(now_ms.saturating_sub(timestamp_ms))
}
