use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use http::Method;
use protobuf::Message;
use quinn::crypto::rustls::HandshakeData;
use quinn::VarInt;
use rustls::{Certificate, PrivateKey};
use sec_http3::error::Code;
use sec_http3::sec_http3_quinn as h3_quinn;
use sec_http3::webtransport::{server::WebTransportSession, stream};
use sec_http3::{
    error::ErrorLevel,
    ext::Protocol,
    quic::{self, RecvDatagramExt, SendDatagramExt, SendStreamUnframed},
    server::Connection,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{watch, RwLock};
use tracing::{error, info, trace_span};
use types::protos::connection_packet::ConnectionPacket;
use types::protos::packet_wrapper::packet_wrapper::PacketType;
use types::protos::packet_wrapper::PacketWrapper;

pub const WEB_TRANSPORT_ALPN: &[&[u8]] = &[b"h3", b"h3-32", b"h3-31", b"h3-30", b"h3-29"];

pub const QUIC_ALPN: &[u8] = b"hq-29";

const MAX_UNIDIRECTIONAL_STREAM_SIZE: usize = 500_000;

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

fn get_key_and_cert_chain(certs: Certs) -> anyhow::Result<(PrivateKey, Vec<Certificate>)> {
    let key_path = certs.key;
    let cert_path = certs.cert;
    let key = std::fs::read(&key_path).context("failed to read private key")?;
    let key = if key_path.extension().map_or(false, |x| x == "der") {
        PrivateKey(key)
    } else {
        let pkcs8 = rustls_pemfile::pkcs8_private_keys(&mut &*key)
            .context("malformed PKCS #8 private key")?;
        match pkcs8.into_iter().next() {
            Some(x) => PrivateKey(x),
            None => {
                let rsa = rustls_pemfile::rsa_private_keys(&mut &*key)
                    .context("malformed PKCS #1 private key")?;
                match rsa.into_iter().next() {
                    Some(x) => PrivateKey(x),
                    None => {
                        anyhow::bail!("no private keys found");
                    }
                }
            }
        }
    };
    let certs = std::fs::read(&cert_path).context("failed to read certificate chain")?;
    let certs = if cert_path.extension().map_or(false, |x| x == "der") {
        vec![Certificate(certs)]
    } else {
        rustls_pemfile::certs(&mut &*certs)
            .context("invalid PEM-encoded certificate")?
            .into_iter()
            .map(Certificate)
            .collect()
    };
    Ok((key, certs))
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

    let mut tls_config = rustls::ServerConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    tls_config.max_early_data_size = u32::MAX;
    let mut alpn = vec![];
    for proto in WEB_TRANSPORT_ALPN {
        alpn.push(proto.to_vec());
    }
    alpn.push(QUIC_ALPN.to_vec());

    tls_config.alpn_protocols = alpn;

    // 1. create quinn server endpoint and bind UDP socket
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(Duration::from_secs(2)));
    transport_config.max_idle_timeout(Some(VarInt::from_u32(10_000).into()));
    transport_config.max_concurrent_uni_streams(1000u32.into());
    server_config.transport = Arc::new(transport_config);
    let endpoint = quinn::Endpoint::server(server_config, opt.listen)?;

    info!("listening on {}", opt.listen);

    let nc =
        async_nats::connect(std::env::var("NATS_URL").expect("NATS_URL env var must be defined"))
            .await
            .unwrap();

    // 2. Accept new quic connections and spawn a new task to handle them
    while let Some(new_conn) = endpoint.accept().await {
        trace_span!("New connection being attempted");
        let nc = nc.clone();

        tokio::spawn(async move {
            match new_conn.await {
                Ok(conn) => {
                    if is_http3(&conn) {
                        info!("new http3 established");
                        let h3_conn = sec_http3::server::builder()
                            .enable_webtransport(true)
                            .enable_connect(true)
                            .enable_datagram(true)
                            .max_webtransport_sessions(1)
                            .send_grease(true)
                            .build(h3_quinn::Connection::new(conn))
                            .await
                            .unwrap();
                        let nc = nc.clone();
                        if let Err(err) = handle_h3_connection(h3_conn, nc).await {
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
    endpoint.wait_idle().await;

    Ok(())
}

async fn handle_h3_connection(
    mut conn: Connection<h3_quinn::Connection, Bytes>,
    nc: async_nats::client::Client,
) -> Result<()> {
    // 3. TODO: Conditionally, if the client indicated that this is a webtransport session, we should accept it here, else use regular h3.
    // if this is a webtransport session, then h3 needs to stop handing the datagrams, bidirectional streams, and unidirectional streams and give them
    // to the webtransport session.

    loop {
        match conn.accept().await {
            Ok(Some((req, stream))) => {
                info!("new request: {:#?}", req);
                let ext = req.extensions();
                match req.method() {
                    &Method::CONNECT if ext.get::<Protocol>() == Some(&Protocol::WEB_TRANSPORT) => {
                        let uri = req.uri().clone();
                        let path = urlencoding::decode(uri.path()).unwrap().into_owned();

                        info!("Got path : {} ", path);

                        let parts = path.split('/').collect::<Vec<&str>>();
                        // filter out the empty strings
                        let parts = parts.iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
                        info!("Parts {:?}", parts);
                        if parts.len() != 3 {
                            conn.close(Code::H3_REQUEST_REJECTED, "Invalid path wrong length");
                            return Err(anyhow!("Invalid path wrong length"));
                        } else if parts[0] != &"lobby" {
                            conn.close(Code::H3_REQUEST_REJECTED, "Invalid path wrong prefix");
                            return Err(anyhow!("Invalid path wrong prefix"));
                        }

                        let username = parts[1].replace(' ', "_");
                        let lobby_id = parts[2].replace(' ', "_");
                        let re = regex::Regex::new("^[a-zA-Z0-9_]*$").unwrap();
                        if !re.is_match(&username) && !re.is_match(&lobby_id) {
                            conn.close(Code::H3_REQUEST_REJECTED, "Invalid path input chars");
                            return Err(anyhow!("Invalid path input chars"));
                        }

                        info!("Peer wants to initiate a webtransport session");

                        info!("Handing over connection to WebTransport");

                        let session = WebTransportSession::accept(req, stream, conn).await?;
                        info!("Established webtransport session");
                        // 4. Get datagrams, bidirectional streams, and unidirectional streams and wait for client requests here.
                        // h3_conn needs to handover the datagrams, bidirectional streams, and unidirectional streams to the webtransport session.
                        handle_session(session, &username, &lobby_id, nc.clone()).await?;
                        return Ok(());
                    }
                    _ => {
                        info!(?req, "Received request");
                    }
                }
            }

            // indicating no more streams to be received
            Ok(None) => {
                break;
            }

            Err(err) => {
                error!("Error on accept {}", err);
                match err.get_error_level() {
                    ErrorLevel::ConnectionError => break,
                    ErrorLevel::StreamError => continue,
                }
            }
        }
    }
    Ok(())
}

#[tracing::instrument(level = "trace", skip(session))]
async fn handle_session<C>(
    session: WebTransportSession<C, Bytes>,
    username: &str,
    lobby_id: &str,
    nc: async_nats::client::Client,
) -> anyhow::Result<()>
where
    // Use trait bounds to ensure we only happen to use implementation that are only for the quinn
    // backend.
    C: 'static
        + Send
        + sec_http3::quic::Connection<Bytes>
        + RecvDatagramExt<Buf = Bytes>
        + SendDatagramExt<Bytes>,
    <C::SendStream as sec_http3::quic::SendStream<Bytes>>::Error:
        'static + std::error::Error + Send + Sync + Into<std::io::Error>,
    <C::RecvStream as sec_http3::quic::RecvStream>::Error:
        'static + std::error::Error + Send + Sync + Into<std::io::Error>,
    stream::BidiStream<C::BidiStream, Bytes>:
        quic::BidiStream<Bytes> + Unpin + AsyncWrite + AsyncRead,
    <stream::BidiStream<C::BidiStream, Bytes> as quic::BidiStream<Bytes>>::SendStream:
        Unpin + AsyncWrite + Send + Sync,
    <stream::BidiStream<C::BidiStream, Bytes> as quic::BidiStream<Bytes>>::RecvStream:
        Unpin + AsyncRead + Send + Sync,
    C::SendStream: Send + Sync + Unpin,
    C::RecvStream: Send + Unpin,
    C::BidiStream: Send + Unpin,
    stream::SendStream<C::SendStream, Bytes>: AsyncWrite,
    C::BidiStream: SendStreamUnframed<Bytes>,
    C::SendStream: SendStreamUnframed<Bytes> + Send,
    <C as sec_http3::quic::Connection<bytes::Bytes>>::OpenStreams: Send,
    <C as sec_http3::quic::Connection<bytes::Bytes>>::BidiStream: Sync,
{
    let session_id = session.session_id();
    let session = Arc::new(RwLock::new(session));
    let should_run = Arc::new(AtomicBool::new(true));

    let subject = format!("room.{}.*", lobby_id).replace(' ', "_");
    let specific_subject = format!("room.{}.{}", lobby_id, username).replace(' ', "_");
    let mut sub = match nc
        .queue_subscribe(subject.clone(), specific_subject.clone())
        .await
    {
        Ok(sub) => {
            info!("Subscribed to subject {}", subject);
            sub
        }
        Err(e) => {
            let err = format!("error subscribing to subject {}: {}", subject, e);
            error!("{}", err);
            return Err(anyhow!(err));
        }
    };

    let specific_subject_clone = specific_subject.clone();

    let nats_task = {
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
                if msg.payload.len() > 400 {
                    let stream = session.open_uni(session_id).await;
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
                } else if let Err(e) = session.send_datagram(msg.payload) {
                    error!("Error sending datagram: {}", e);
                }
            }
        })
    };

    let quic_task = {
        let session = session.clone();
        let nc = nc.clone();
        let specific_subject = specific_subject.clone();
        tokio::spawn(async move {
            let session = session.read().await;
            while let Ok(uni_stream) = session.accept_uni().await {
                if let Some((_id, mut uni_stream)) = uni_stream {
                    let nc = nc.clone();
                    let specific_subject = specific_subject.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        if let Err(e) = uni_stream.read_to_end(&mut buf).await {
                            error!("Error reading from unidirectional stream: {}", e);
                        }
                        if let Err(e) = nc.publish(specific_subject.clone(), buf.into()).await {
                            error!("Error publishing to subject {}: {}", &specific_subject, e);
                        }
                    });
                }
            }
        })
    };

    let _datagrams_task = {
        tokio::spawn(async move {
            let session = session.read().await;
            while let Ok(datagram) = session.accept_datagram().await {
                if let Some((_id, buf)) = datagram {
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
    nats_task.abort();
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
    let (specific_subject_tx, mut specific_subject_rx) = watch::channel::<Option<String>>(None);

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
                .queue_subscribe(subject.clone(), specific_subject.clone())
                .await
            {
                Ok(sub) => {
                    info!("Subscribed to subject {}", subject);
                    sub
                }
                Err(e) => {
                    let err = format!("error subscribing to subject {}: {}", subject, e);
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
                if msg.payload.len() > 400 {
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
                } else if let Err(e) = session.send_datagram(msg.payload) {
                    error!("Error sending datagram: {}", e);
                }
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
                                        .send(Some(specific_subject.clone()))
                                        .unwrap();
                                }
                            }
                        } else {
                            let specific_subject = specific_subject_rx.borrow().clone().unwrap();
                            if let Err(e) = nc.publish(specific_subject.clone(), d.into()).await {
                                error!("Error publishing to subject {}: {}", &specific_subject, e);
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
        tokio::spawn(async move {
            let session = session.read().await;
            if specific_subject_rx.borrow().is_none() {
                specific_subject_rx.changed().await.unwrap();
            }
            let specific_subject = specific_subject_rx.borrow().clone().unwrap();
            while let Ok(datagram) = session.read_datagram().await {
                let nc = nc.clone();
                if let Err(e) = nc.publish(specific_subject.clone(), datagram).await {
                    error!("Error publishing to subject {}: {}", specific_subject, e);
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
