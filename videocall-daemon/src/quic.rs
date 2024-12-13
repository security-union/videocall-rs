use std::sync::Arc;

use anyhow::Error;
use clap::Parser;
use protobuf::Message;
use quinn::Connection;
use tokio::{
    sync::mpsc::{self, Sender},
    time::{self, Duration},
};
use tracing::{debug, info};
use url::Url;
use videocall_types::protos::{
    connection_packet::ConnectionPacket,
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};

#[derive(Parser, Debug)]
#[clap(name = "client")]
pub struct Opt {
    /// Perform NSS-compatible TLS key logging to the file specified in `SSLKEYLOGFILE`.
    #[clap(long = "keylog")]
    keylog: bool,

    /// URL to connect to.
    #[clap(long = "url", default_value = "https://transport.rustlemania.com")]
    url: Url,

    #[clap(long = "user-id")]
    pub user_id: String,

    #[clap(long = "meeting-id")]
    meeting_id: String,

    #[clap(long = "video-device-index")]
    pub video_device_index: usize,

    #[clap(long = "audio-device")]
    pub audio_device: Option<String>,

    /// Resolution in WIDTHxHEIGHT format (e.g., 1920x1080)
    #[clap(long = "resolution")]
    pub resolution: String,

    /// Frames per second (e.g. 10, 30, 60)
    #[clap(long = "fps")]
    pub fps: u32,
}

pub struct Client {
    options: Opt,
    connection: Option<Connection>,
    sender: Option<Sender<Vec<u8>>>,
    heartbeat_interval: Duration,
}

impl Client {
    pub fn new(options: Opt) -> Self {
        Self {
            options,
            connection: None,
            sender: None,
            heartbeat_interval: Duration::from_secs(5), // Heartbeat every 5 seconds
        }
    }

    pub async fn connect(&mut self) -> anyhow::Result<()> {
        let conn = connect_to_server(&self.options).await?;
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(100);
        self.sender = Some(tx);
        self.connection = Some(conn.clone());

        // Spawn a task to handle sending messages via the connection
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let Err(e) = Self::send(conn.clone(), message).await {
                    tracing::error!("Failed to send message: {}", e);
                }
            }
        });

        self.send_connection_packet().await?;

        // move the heartbeat to a separate task
        
        self.start_heartbeat().await;
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

    async fn send(conn: Connection, data: Vec<u8>) -> anyhow::Result<()> {
        debug!("Sending {} bytes", data.len());
        let mut stream = conn.open_uni().await?;
        stream.write_all(&data).await?;
        stream.finish().await?;
        debug!("Sent {} bytes", data.len());
        Ok(())
    }

    // Add public method to queue a message
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

    async fn start_heartbeat(&self) {
        let mut interval = time::interval(self.heartbeat_interval);
        loop {
            interval.tick().await;
            let heartbeat_packet = PacketWrapper {
                packet_type: PacketType::MEDIA.into(),
                ..Default::default()
            };
            if let Err(e) = self
                .queue_message(heartbeat_packet.write_to_bytes().unwrap())
                .await
            {
                tracing::error!("Failed to queue heartbeat: {}", e);
            }
        }
    }
}

async fn connect_to_server(options: &Opt) -> anyhow::Result<Connection> {
    loop {
        // Perform actual connection logic here
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
