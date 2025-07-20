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

use anyhow::{anyhow, Context, Result};
use async_nats::Subject;
use futures::StreamExt;
use protobuf::Message;
use quinn::crypto::rustls::HandshakeData;
use quinn::VarInt;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{fs, io};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::{watch, RwLock};
use tracing::{debug, error, info, trace_span};
use videocall_types::protos::connection_packet::ConnectionPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use web_transport_quinn::Session;

/// Videocall WebTransport API
///
/// This module contains the implementation of the Videocall WebTransport API.
/// It is responsible for accepting incoming WebTransport connections and handling them.
/// It also contains the logic for handling the WebTransport handshake and the WebTransport session.
///
///
pub const WEB_TRANSPORT_ALPN: &[&[u8]] = &[b"h3", b"h3-32", b"h3-31", b"h3-30", b"h3-29"];

pub const QUIC_ALPN: &[u8] = b"hq-29";

const MAX_UNIDIRECTIONAL_STREAM_SIZE: usize = 500_000;

/// Check if the binary data is an RTT packet that should be echoed back
fn is_rtt_packet(data: &[u8]) -> bool {
    if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
        if packet_wrapper.packet_type == PacketType::MEDIA.into() {
            if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
                return media_packet.media_type == MediaType::RTT.into();
            }
        }
    }
    false
}

#[derive(Debug)]
pub struct WebTransportOpt {
    pub listen: SocketAddr,
    pub certs: Certs,
}

#[derive(Debug, Clone)]
pub struct Certs {
    pub cert: PathBuf,
    pub key: PathBuf,
}

fn get_key_and_cert_chain<'a>(
    certs: Certs,
) -> anyhow::Result<(PrivateKeyDer<'a>, Vec<CertificateDer<'a>>)> {
    let key_path = certs.key;
    let cert_path = certs.cert;
    let mut keys = fs::File::open(key_path).context("failed to open key file")?;

    // Read the keys into a Vec so we can parse it twice.
    let mut buf = Vec::new();
    keys.read_to_end(&mut buf)?;

    // Try to parse a PKCS#8 key
    // -----BEGIN PRIVATE KEY-----
    let key = rustls_pemfile::private_key(&mut io::Cursor::new(&buf))
        .context("failed to load private key")?
        .context("missing private key")?;

    // Read the PEM certificate chain
    let chain = fs::File::open(cert_path).context("failed to open cert file")?;
    let mut chain = io::BufReader::new(chain);

    let chain: Vec<CertificateDer> = rustls_pemfile::certs(&mut chain)
        .collect::<Result<_, _>>()
        .context("failed to load certs")?;

    anyhow::ensure!(!chain.is_empty(), "could not find certificate");
    Ok((key, chain))
}

pub fn is_http3(conn: &quinn::Connection) -> bool {
    if let Some(data) = conn.handshake_data() {
        if let Some(d) = data.downcast_ref::<HandshakeData>() {
            if let Some(alpn) = &d.protocol {
                return WEB_TRANSPORT_ALPN.contains(&alpn.as_slice());
            }
        }
    };
    false
}

pub async fn start(opt: WebTransportOpt) -> Result<(), Box<dyn std::error::Error>> {
    info!("WebTransportOpt: {opt:#?}");

    let (key, certs) = get_key_and_cert_chain(opt.certs)?;

    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(certs, key)?;

    config.max_early_data_size = u32::MAX;
    let mut alpn = vec![];
    for proto in WEB_TRANSPORT_ALPN {
        alpn.push(proto.to_vec());
    }
    alpn.push(QUIC_ALPN.to_vec());
    config.alpn_protocols = alpn;

    // 1. create quinn server endpoint and bind UDP socket
    let config: quinn::crypto::rustls::QuicServerConfig = config.try_into()?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(config));
    // configure pings
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(Duration::from_secs(2)));
    transport_config.max_idle_timeout(Some(VarInt::from_u32(10_000).into()));
    config.transport = Arc::new(transport_config);
    let server = quinn::Endpoint::server(config, opt.listen)?;

    info!("listening on {}", opt.listen);

    let nc =
        async_nats::connect(std::env::var("NATS_URL").expect("NATS_URL env var must be defined"))
            .await
            .unwrap();

    // 2. Accept new quic connections and spawn a new task to handle them
    while let Some(new_conn) = server.accept().await {
        trace_span!("New connection being attempted");
        let nc = nc.clone();
        tokio::spawn(async move {
            match new_conn.await {
                Ok(conn) => {
                    if is_http3(&conn) {
                        info!("new http3 established");
                        if let Err(err) = run_webtransport_connection(conn.clone(), nc).await {
                            error!("Failed to handle connection: {err:?}");
                        }
                    } else {
                        info!("new quic established");
                        let nc = nc.clone();
                        if let Err(err) = handle_quic_connection(conn, nc).await {
                            error!("Failed to handle connection: {err:?}");
                        }
                    }
                }
                Err(err) => {
                    error!("accepting connection failed: {:?}", err);
                }
            }
        });
    }

    // shut down gracefully
    // wait for connections to be closed before exiting
    server.wait_idle().await;
    Ok(())
}

async fn run_webtransport_connection(
    conn: quinn::Connection,
    nc: async_nats::client::Client,
) -> anyhow::Result<()> {
    info!("received new QUIC connection");

    // Perform the WebTransport handshake.
    let request = web_transport_quinn::accept(conn.clone()).await?;
    info!("received WebTransport request: {}", request.url());
    let url = request.url();

    let uri = url;
    let path = urlencoding::decode(uri.path()).unwrap().into_owned();

    info!("Got path : {} ", path);

    let parts = path.split('/').collect::<Vec<&str>>();
    // filter out the empty strings
    let parts = parts.iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
    info!("Parts {:?}", parts);
    if parts.len() != 3 {
        conn.close(VarInt::from_u32(0x1), b"Invalid path wrong length");
        return Err(anyhow!("Invalid path wrong length"));
    } else if parts[0] != &"lobby" {
        conn.close(VarInt::from_u32(0x1), b"Invalid path wrong prefix");
        return Err(anyhow!("Invalid path wrong prefix"));
    }

    let username = parts[1].replace(' ', "_");
    let lobby_id = parts[2].replace(' ', "_");
    let re = regex::Regex::new("^[a-zA-Z0-9_]*$").unwrap();
    if !re.is_match(&username) && !re.is_match(&lobby_id) {
        conn.close(VarInt::from_u32(0x1), b"Invalid path input chars");
        return Err(anyhow!("Invalid path input chars"));
    }

    // Accept the session.
    let session = request.ok().await.context("failed to accept session")?;
    info!("accepted session");

    // Run the session
    if let Err(err) = handle_session(session, &username, &lobby_id, nc).await {
        info!("closing session: {}", err);
    }
    Ok(())
}

#[tracing::instrument(level = "trace", skip(session))]
async fn handle_session(
    session: Session,
    username: &str,
    lobby_id: &str,
    nc: async_nats::client::Client,
) -> anyhow::Result<()> {
    let session = Arc::new(RwLock::new(session));
    let should_run = Arc::new(AtomicBool::new(true));

    let subject = format!("room.{lobby_id}.*").replace(' ', "_");
    let specific_subject: Subject = format!("room.{lobby_id}.{username}")
        .replace(' ', "_")
        .into();
    let mut sub = match nc
        .queue_subscribe(subject.clone(), specific_subject.to_string())
        .await
    {
        Ok(sub) => {
            info!("Subscribed to subject {}", subject);
            sub
        }
        Err(e) => {
            let err = format!("error subscribing to subject {subject}: {e}");
            error!("{}", err);
            return Err(anyhow!(err));
        }
    };

    let specific_subject_clone = specific_subject.clone();

    let nats_receive_task = {
        let session = session.clone();
        let should_run = should_run.clone();
        tokio::spawn(async move {
            while let Some(msg) = sub.next().await {
                if !should_run.load(Ordering::SeqCst) {
                    break;
                }
                if msg.subject == specific_subject_clone {
                    continue;
                }
                let session = session.read().await;
                let stream = session.open_uni().await;
                tokio::spawn(async move {
                    match stream {
                        Ok(mut uni_stream) => {
                            if let Err(e) = uni_stream.write_all(&msg.payload).await {
                                error!("Error writing to unidirectional stream: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("Error opening unidirectional stream: {}", e);
                        }
                    }
                });
            }
        })
    };

    let quic_task = {
        let session = session.clone();
        let nc = nc.clone();
        let specific_subject = specific_subject.clone();
        tokio::spawn(async move {
            let session = session.read().await;
            while let Ok(mut uni_stream) = session.accept_uni().await {
                let nc = nc.clone();
                let specific_subject = specific_subject.clone();
                let session_clone = session.clone();
                tokio::spawn(async move {
                    let result = uni_stream.read_to_end(1_000_000).await;
                    match result {
                        Ok(buf) => {
                            // Check if this is an RTT packet that should be echoed back
                            if is_rtt_packet(&buf) {
                                debug!("Echoing RTT packet back via WebTransport");
                                match session_clone.open_uni().await {
                                    Ok(mut echo_stream) => {
                                        if let Err(e) = echo_stream.write_all(&buf).await {
                                            error!("Error echoing RTT packet: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Error opening echo stream: {}", e);
                                    }
                                }
                            } else {
                                // Normal packet processing - publish to NATS
                                tokio::spawn(async move {
                                    if let Err(e) =
                                        nc.publish(specific_subject.clone(), buf.into()).await
                                    {
                                        error!(
                                            "Error publishing to subject {}: {}",
                                            &specific_subject, e
                                        );
                                    }
                                });
                            }
                        }
                        Err(e) => {
                            error!("Error reading from unidirectional stream: {}", e);
                        }
                    }
                });
            }
        })
    };

    let _datagrams_task = {
        let session_clone = session.clone();
        tokio::spawn(async move {
            let session = session_clone.read().await;
            while let Ok(buf) = session.read_datagram().await {
                // Check if this is an RTT packet that should be echoed back
                if is_rtt_packet(&buf) {
                    debug!("Echoing RTT datagram back via WebTransport");
                    if let Err(e) = session.send_datagram(buf) {
                        error!("Error echoing RTT datagram: {}", e);
                    }
                } else {
                    // Normal datagram processing - publish to NATS
                    let nc = nc.clone();
                    if let Err(e) = nc.publish(specific_subject.clone(), buf).await {
                        error!("Error publishing to subject {}: {}", specific_subject, e);
                    }
                }
            }
        })
    };
    quic_task.await?;
    should_run.store(false, Ordering::SeqCst);
    nats_receive_task.abort();
    info!("Finished handling session");
    Ok(())
}

async fn handle_quic_connection(
    conn: quinn::Connection,
    nc: async_nats::client::Client,
) -> Result<()> {
    let _session_id = conn.stable_id();
    let session = Arc::new(RwLock::new(conn));
    let should_run = Arc::new(AtomicBool::new(true));
    let (specific_subject_tx, mut specific_subject_rx) = watch::channel::<Option<Subject>>(None);

    let nats_task = {
        let session = session.clone();
        let should_run = should_run.clone();
        let nc_clone = nc.clone();
        let specific_subject_rx_clone = specific_subject_rx.clone();
        tokio::spawn(async move {
            let mut specific_subject_rx = specific_subject_rx_clone;
            let nc = nc_clone;
            specific_subject_rx.changed().await.unwrap();
            let specific_subject = specific_subject_rx.borrow().clone().unwrap();
            let subject = session_subject_to_lobby_subject(&specific_subject);
            let mut sub = match nc
                .queue_subscribe(subject.clone(), specific_subject.to_string())
                .await
            {
                Ok(sub) => {
                    info!("Subscribed to subject {}", subject);
                    sub
                }
                Err(e) => {
                    let err = format!("error subscribing to subject {subject}: {e}");
                    error!("{}", err);
                    return;
                }
            };
            while let Some(msg) = sub.next().await {
                if !should_run.load(Ordering::SeqCst) {
                    break;
                }
                if Some(msg.subject) == specific_subject_rx.borrow().clone() {
                    continue;
                }
                let session = session.read().await;

                let stream = session.open_uni().await;
                tokio::spawn(async move {
                    match stream {
                        Ok(mut uni_stream) => {
                            if let Err(e) = uni_stream.write_all(&msg.payload).await {
                                error!("Error writing to unidirectional stream: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("Error opening unidirectional stream: {}", e);
                        }
                    }
                });
            }
        })
    };

    let quic_task = {
        let specific_subject_rx_clone = specific_subject_rx.clone();
        let session = session.clone();
        let nc = nc.clone();
        tokio::spawn(async move {
            let session = session.read().await;
            let specific_subject_tx = Arc::new(specific_subject_tx);
            while let Ok(mut uni_stream) = session.accept_uni().await {
                let nc = nc.clone();
                let specific_subject_tx_clone = specific_subject_tx.clone();
                let specific_subject_rx = specific_subject_rx_clone.clone();
                let session_for_echo = session.clone();
                tokio::spawn(async move {
                    if let Ok(d) = uni_stream.read_to_end(MAX_UNIDIRECTIONAL_STREAM_SIZE).await {
                        if specific_subject_rx.borrow().is_none() {
                            if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(&d) {
                                if packet_wrapper.packet_type == PacketType::CONNECTION.into() {
                                    info!("Got connection packet");
                                    let connection_packet =
                                        ConnectionPacket::parse_from_bytes(&packet_wrapper.data)
                                            .unwrap();
                                    let specific_subject = format!(
                                        "room.{}.{}",
                                        connection_packet.meeting_id, packet_wrapper.email
                                    )
                                    .replace(' ', "_");
                                    info!("Specific subject: {}", specific_subject);
                                    specific_subject_tx_clone
                                        .send(Some(specific_subject.into()))
                                        .unwrap();
                                }
                            }
                        } else {
                            // Check if this is an RTT packet that should be echoed back
                            if is_rtt_packet(&d) {
                                debug!("Echoing RTT packet back via QUIC");
                                let session_read = session_for_echo.clone();
                                match session_read.open_uni().await {
                                    Ok(mut echo_stream) => {
                                        if let Err(e) = echo_stream.write_all(&d).await {
                                            error!("Error echoing RTT packet via QUIC: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Error opening QUIC echo stream: {}", e);
                                    }
                                }
                            } else {
                                // Normal packet processing - publish to NATS
                                let specific_subject =
                                    specific_subject_rx.borrow().clone().unwrap();
                                if let Err(e) = nc.publish(specific_subject.clone(), d.into()).await
                                {
                                    error!(
                                        "Error publishing to subject {}: {}",
                                        &specific_subject, e
                                    );
                                }
                            }
                        }
                    } else {
                        error!("Error reading from unidirectional stream");
                    };
                });
            }
        })
    };

    let _datagrams_task = {
        let session_clone = session.clone();
        tokio::spawn(async move {
            let session = session_clone.read().await;
            if specific_subject_rx.borrow().is_none() {
                specific_subject_rx.changed().await.unwrap();
            }
            let specific_subject = specific_subject_rx.borrow().clone().unwrap();
            while let Ok(datagram) = session.read_datagram().await {
                // Check if this is an RTT packet that should be echoed back
                if is_rtt_packet(&datagram) {
                    debug!("Echoing RTT datagram back via QUIC");
                    if let Err(e) = session.send_datagram(datagram) {
                        error!("Error echoing RTT datagram via QUIC: {}", e);
                    }
                } else {
                    // Normal datagram processing - publish to NATS
                    let nc = nc.clone();
                    if let Err(e) = nc.publish(specific_subject.clone(), datagram).await {
                        error!("Error publishing to subject {}: {}", specific_subject, e);
                    }
                }
            }
        })
    };
    quic_task.await?;
    should_run.store(false, Ordering::SeqCst);
    nats_task.abort();
    info!("Finished handling session");
    Ok(())
}

fn session_subject_to_lobby_subject(subject: &str) -> String {
    let parts = subject.split('.').collect::<Vec<&str>>();
    let mut lobby_subject = String::from("room.");
    lobby_subject.push_str(parts[1]);
    lobby_subject.push_str(".*");
    lobby_subject
}
