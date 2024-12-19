use std::sync::Arc;

use anyhow::Error;
use protobuf::Message;
use quinn::Connection;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    sync::mpsc::{self, Sender},
    time::{self, Duration},
};
use tracing::{debug, info};
use videocall_types::protos::{
    connection_packet::ConnectionPacket,
    media_packet::{media_packet::MediaType, MediaPacket},
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};

use crate::cli_args::Streaming;

pub struct Client {
    options: Streaming,
    sender: Option<Sender<Vec<u8>>>,
}

impl Client {
    pub fn new(options: Streaming) -> Self {
        Self {
            options,
            sender: None,
        }
    }

    pub async fn connect(&mut self) -> anyhow::Result<()> {
        let conn = connect_to_server(&self.options).await?;
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(100);
        self.sender = Some(tx);

        // Spawn a task to handle sending messages via the connection
        let cloned_conn = conn.clone();
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let Err(e) = Self::send(cloned_conn.clone(), message).await {
                    tracing::error!("Failed to send message: {}", e);
                }
            }
        });

        // Spawn a separate task for heartbeat
        self.start_heartbeat(conn.clone(), &self.options).await;

        self.send_connection_packet().await?;
        Ok(())
    }

    async fn send_connection_packet(&self) -> anyhow::Result<()> {
        let connection_packet = ConnectionPacket {
            meeting_id: self.options.meeting_id.clone(),
            ..Default::default()
        };
        let packet = PacketWrapper {
            packet_type: PacketType::CONNECTION.into(),
            email: self.options.user_id.clone(),
            data: connection_packet.write_to_bytes()?,
            ..Default::default()
        };
        self.queue_message(packet.write_to_bytes()?).await?;
        Ok(())
    }

    pub async fn send(conn: Connection, data: Vec<u8>) -> anyhow::Result<()> {
        let mut stream = conn.open_uni().await?;
        stream.write_all(&data).await?;
        stream.finish().await?;
        Ok(())
    }

    pub async fn send_packet(&self, data: Vec<u8>) -> anyhow::Result<()> {
        self.queue_message(data).await
    }

    async fn queue_message(&self, message: Vec<u8>) -> anyhow::Result<()> {
        if let Some(sender) = &self.sender {
            sender
                .send(message)
                .await
                .map_err(|_| Error::msg("Failed to send message to queue"))
        } else {
            Err(Error::msg("No sender available"))
        }
    }

    async fn start_heartbeat(&self, conn: Connection, options: &Streaming) {
        let interval = time::interval(Duration::from_secs(1));
        let email = options.user_id.clone();
        tokio::spawn(async move {
            let mut interval = interval;
            loop {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_millis(); // Get milliseconds since Unix epoch
                interval.tick().await;
                let actual_heartbeat = MediaPacket {
                    media_type: MediaType::HEARTBEAT.into(),
                    email: email.clone(),
                    timestamp: now_ms as f64,
                    ..Default::default()
                };

                let packet = PacketWrapper {
                    email: email.clone(),
                    packet_type: PacketType::MEDIA.into(),
                    data: actual_heartbeat.write_to_bytes().unwrap(),
                    ..Default::default()
                };
                let data = packet.write_to_bytes().unwrap();
                if let Err(e) = Self::send(conn.clone(), data).await {
                    tracing::error!("Failed to send heartbeat: {}", e);
                }
            }
        });
    }
}

async fn connect_to_server(options: &Streaming) -> anyhow::Result<Connection> {
    loop {
        info!("Attempting to connect to {}", options.url);
        let addrs = options
            .url
            .socket_addrs(|| Some(443))
            .expect("couldn't resolve the address provided");
        let remote = addrs.first().to_owned();
        let remote = remote.unwrap();
        let mut root_store = rustls::RootCertStore::empty();
        root_store.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
            rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));
        let mut client_crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        let alpn = vec![b"hq-29".to_vec()];
        client_crypto.alpn_protocols = alpn;
        if options.keylog {
            client_crypto.key_log = Arc::new(rustls::KeyLogFile::new());
        }
        let client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
        let host = options.url.host_str();

        match quinn::Endpoint::client("[::]:0".parse().unwrap()) {
            Ok(mut endpoint) => {
                endpoint.set_default_client_config(client_config);
                match endpoint.connect(*remote, host.unwrap()) {
                    Ok(conn) => {
                        let conn = conn.await?;
                        info!("Connected successfully");
                        return Ok(conn);
                    }
                    Err(e) => {
                        tracing::error!("Connection failed: {}. Retrying in 5 seconds...", e);
                        time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
            Err(e) => {
                tracing::error!("Endpoint creation failed: {}. Retrying in 5 seconds...", e);
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}
