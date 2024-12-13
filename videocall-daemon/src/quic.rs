use clap::Parser;
use protobuf::Message;
use quinn::ConnectError;
use videocall_types::protos::{
    connection_packet::ConnectionPacket,
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};

use std::{path::PathBuf, sync::Arc, time::Instant};
use thiserror::Error;
use tracing::{debug, info};
use url::Url;

const DEFAULT_MAX_PACKET_SIZE: usize = 500_000;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("failed to connect: {}", .0)]
    FailedToConnect(String),
    #[error("not connected")]
    NotConnected,
    #[error("oversized packet of {} bytes", .0)]
    OversizedPacket(usize),
    #[error("could not resolve address")]
    UnresolvedAddress,
    #[error("no hostname specified")]
    UnspecifiedHostname,
}

impl From<ConnectError> for ClientError {
    fn from(e: ConnectError) -> Self {
        ClientError::FailedToConnect(e.to_string())
    }
}

type Result<T> = std::result::Result<T, ClientError>;
/// Connects to a QUIC server.
///
/// ## Args
///
/// - opt: command line options.
pub async fn connect(opt: &Opt) -> Result<quinn::Connection> {
    let remote = opt
        .url
        .socket_addrs(|| Some(443))
        .expect("couldn't resolve the address provided")
        .first()
        .ok_or(ClientError::UnresolvedAddress)?
        .to_owned();
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
    if opt.keylog {
        client_crypto.key_log = Arc::new(rustls::KeyLogFile::new());
    }
    let client_config = quinn::ClientConfig::new(Arc::new(client_crypto));

    let mut endpoint = quinn::Endpoint::client("[::]:0".parse().unwrap())
        .map_err(|e| ClientError::FailedToConnect(format!("failed to create endpoint: {}", e)))?;
    endpoint.set_default_client_config(client_config);
    let start = Instant::now();
    info!("connecting to {remote}");
    let host = opt
        .url
        .host_str()
        .ok_or(ClientError::UnspecifiedHostname)?
        .to_owned();
    let conn = endpoint
        .connect(remote, &host)?
        .await
        .map_err(|e| ClientError::FailedToConnect(e.to_string()))?;
    info!("connected at {:?}", start.elapsed());
    Ok(conn)
}

/// HTTP/0.9 over QUIC client
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
    pub fps: u32
}

pub struct Client {
    options: Opt,
    connection: Option<quinn::Connection>,
}

impl Client {
    /// Initialize a new QUIC client and load trusted certificates.
    ///
    /// ## Args
    ///
    /// - options: command line options.
    pub fn new(options: Opt) -> Result<Client> {
        Ok(Client {
            options,
            connection: None,
        })
    }

    pub async fn connect(&mut self) -> Result<()> {
        let conn = connect(&self.options).await?;
        self.connection = Some(conn);

        // Send connection message with meeting id
        let connection_packet = ConnectionPacket {
            meeting_id: self.options.meeting_id.clone(),
            ..Default::default()
        };
        let packet = PacketWrapper {
            packet_type: PacketType::CONNECTION.into(),
            email: self.options.user_id.clone(),
            data: connection_packet.write_to_bytes().unwrap(),
            ..Default::default()
        };
        let packet = packet.write_to_bytes().unwrap();
        self.send(packet).await?;

        debug!("connected to server {}", self.options.url);
        Ok(())
    }

    pub async fn send(&mut self, data: Vec<u8>) -> Result<()> {
        let packet_size = data.len();
        let conn = self
            .connection
            .as_mut()
            .ok_or(ClientError::NotConnected)?
            .clone();
        async fn send(conn: quinn::Connection, data: Vec<u8>, packet_size: usize) -> Result<()> {
            debug!("sending {} bytes", packet_size);
            let mut stream = conn.open_uni().await.unwrap();
            stream.write_all(&data).await.unwrap();
            stream.finish().await.unwrap();
            debug!("sent {} bytes", packet_size);
            Ok(())
        }
        tokio::spawn(async move {
            send(conn, data, packet_size).await.unwrap();
        });
        Ok(())
    }
}
