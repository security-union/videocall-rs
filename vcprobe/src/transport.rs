use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use protobuf::Message;
use tokio::sync::mpsc;
use tokio::time;
use url::Url;
use videocall_types::protos::connection_packet::ConnectionPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::to_user_id_bytes;
use web_transport_quinn::ClientBuilder;

pub use crate::nats_transport::connect_nats;

// ── WebTransport ──────────────────────────────────────────────────────────────

pub async fn connect_webtransport(
    url: Url,
    insecure: bool,
    username: String,
    meeting_id: String,
) -> Result<mpsc::Receiver<Vec<u8>>> {
    let client = if insecure {
        log::warn!("certificate verification disabled (--insecure)");
        ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()?
    } else {
        ClientBuilder::new().with_system_roots()?
    };

    let session = client.connect(url).await?;

    // Send CONNECTION packet
    send_via_session(&session, build_connection_packet(&username, &meeting_id)?).await?;

    // Heartbeat
    let quit = Arc::new(AtomicBool::new(false));
    {
        let session = session.clone();
        let username = username.clone();
        let quit = quit.clone();
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(1));
            loop {
                if quit.load(Ordering::Relaxed) {
                    break;
                }
                interval.tick().await;
                match build_heartbeat_packet(&username) {
                    Ok(data) => {
                        if let Err(e) = send_via_session(&session, data).await {
                            log::debug!("heartbeat send failed: {}", e);
                            break;
                        }
                    }
                    Err(e) => log::debug!("heartbeat build failed: {}", e),
                }
            }
        });
    }

    // Inbound reader — each uni stream is one PacketWrapper
    let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
    {
        let session = session.clone();
        tokio::spawn(async move {
            loop {
                match session.accept_uni().await {
                    Ok(mut stream) => {
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            match stream.read_to_end(4 * 1024 * 1024).await {
                                Ok(data) if !data.is_empty() => {
                                    let _ = tx.send(data).await;
                                }
                                Ok(_) => {}
                                Err(e) => log::debug!("stream read error: {}", e),
                            }
                        });
                    }
                    Err(e) => {
                        log::debug!("accept_uni ended: {}", e);
                        // Signal quit so heartbeat stops
                        quit.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
    }

    Ok(rx)
}

async fn send_via_session(session: &web_transport_quinn::Session, data: Vec<u8>) -> Result<()> {
    let mut stream = session.open_uni().await?;
    stream.write_all(&data).await?;
    stream.finish()?;
    Ok(())
}

// ── WebSocket ─────────────────────────────────────────────────────────────────

pub async fn connect_websocket(
    url: Url,
    insecure: bool,
    username: String,
    meeting_id: String,
) -> Result<mpsc::Receiver<Vec<u8>>> {
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = if insecure && url.scheme() == "wss" {
        log::warn!("certificate verification disabled (--insecure)");
        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()?;
        Some(tokio_tungstenite::Connector::NativeTls(tls))
    } else {
        None
    };

    let (ws_stream, _) =
        tokio_tungstenite::connect_async_tls_with_config(url.as_str(), None, false, connector)
            .await?;

    let (mut write, mut read) = ws_stream.split();

    // Send CONNECTION packet
    let conn_bytes = build_connection_packet(&username, &meeting_id)?;
    write.send(WsMessage::Binary(conn_bytes)).await?;

    // Outbound channel (heartbeat → writer task)
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(32);

    // Writer task
    tokio::spawn(async move {
        while let Some(data) = out_rx.recv().await {
            if write.send(WsMessage::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // Heartbeat
    {
        let username = username.clone();
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                match build_heartbeat_packet(&username) {
                    Ok(data) => {
                        if out_tx.send(data).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => log::debug!("heartbeat build failed: {}", e),
                }
            }
        });
    }

    // Inbound reader
    let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(WsMessage::Binary(data)) => {
                    if tx.send(data).await.is_err() {
                        break;
                    }
                }
                Ok(WsMessage::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    Ok(rx)
}

// ── Shared packet builders ────────────────────────────────────────────────────

fn build_connection_packet(username: &str, meeting_id: &str) -> Result<Vec<u8>> {
    let inner = ConnectionPacket {
        meeting_id: meeting_id.to_string(),
        ..Default::default()
    };
    let outer = PacketWrapper {
        packet_type: PacketType::CONNECTION.into(),
        user_id: to_user_id_bytes(username),
        data: inner.write_to_bytes()?,
        ..Default::default()
    };
    Ok(outer.write_to_bytes()?)
}

fn build_heartbeat_packet(username: &str) -> Result<Vec<u8>> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as f64;

    let heartbeat = MediaPacket {
        media_type: MediaType::HEARTBEAT.into(),
        user_id: to_user_id_bytes(username),
        timestamp: now_ms,
        heartbeat_metadata: Some(HeartbeatMetadata {
            video_enabled: false,
            audio_enabled: false,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };

    let outer = PacketWrapper {
        user_id: to_user_id_bytes(username),
        packet_type: PacketType::MEDIA.into(),
        data: heartbeat.write_to_bytes()?,
        ..Default::default()
    };
    Ok(outer.write_to_bytes()?)
}
