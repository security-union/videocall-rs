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

use crate::actors::chat_server::ChatServer;
use crate::actors::wt_chat_session::{WtChatSession, WtInbound, WtInboundSource, WtOutbound};
use crate::constants::VALID_ID_PATTERN;
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use actix::prelude::*;
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sqlx::PgPool;
use std::io::Read;
use std::{fs, io};
use std::{net::SocketAddr, path::PathBuf};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace_span, warn};

lazy_static::lazy_static! {
    static ref QUIC_MAX_IDLE_TIMEOUT_SECS: u64 = std::env::var("QUIC_MAX_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    static ref QUIC_KEEP_ALIVE_INTERVAL_SECS: u64 = std::env::var("QUIC_KEEP_ALIVE_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
}

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

#[cfg(test)]
lazy_static::lazy_static! {
    static ref TEST_PACKET_COUNTERS: Arc<Mutex<HashMap<String, AtomicU64>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

#[cfg(test)]
fn increment_test_packet_counter_for_user(username: &str) {
    let counters = TEST_PACKET_COUNTERS.clone();
    let mut counters_map = counters.lock().unwrap();
    let counter = counters_map
        .entry(username.to_string())
        .or_insert_with(|| AtomicU64::new(0));
    counter.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
fn get_test_packet_counter_for_user(username: &str) -> u64 {
    let counters = TEST_PACKET_COUNTERS.clone();
    let counters_map = counters.lock().unwrap();
    counters_map
        .get(username)
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

#[cfg(test)]
fn reset_test_packet_counters() {
    let counters = TEST_PACKET_COUNTERS.clone();
    let mut counters_map = counters.lock().unwrap();
    counters_map.clear();
}

use web_transport_quinn::{quinn, Session};

/// Videocall WebTransport API
///
/// This module contains the implementation of the Videocall WebTransport API.
/// It is responsible for accepting incoming WebTransport connections and handling them.
/// It also contains the logic for handling the WebTransport handshake and the WebTransport session.
///
///
pub const WEB_TRANSPORT_ALPN: &[&[u8]] = &[b"h3", b"h3-32", b"h3-31", b"h3-30", b"h3-29"];

pub const QUIC_ALPN: &[u8] = b"hq-29";

// Note: is_rtt_packet and KEEP_ALIVE_PING are now handled by WtChatSession actor

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

/// Start the WebTransport server
///
/// # Arguments
/// * `opt` - WebTransport server options (listen address, certs)
/// * `chat_server` - Address of the ChatServer actor
/// * `nats_client` - NATS client for health packet processing
/// * `pool` - Optional database pool
/// * `tracker_sender` - Server diagnostics tracker
/// * `session_manager` - Session lifecycle manager
pub async fn start(
    opt: WebTransportOpt,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    pool: Option<PgPool>,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("WebTransportOpt: {opt:#?}");

    let (key, certs) = get_key_and_cert_chain(opt.certs)?;

    // Manually configure Quinn with custom transport settings for fast disconnect detection
    let provider = rustls::crypto::ring::default_provider();

    let mut server_crypto_config = rustls::ServerConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    server_crypto_config.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];

    let mut server_config = quinn::ServerConfig::with_crypto(std::sync::Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto_config)?,
    ));

    // Configure transport with aggressive timeouts for fast disconnect detection
    let mut transport_config = quinn::TransportConfig::default();

    // Detect disconnection after inactivity (configurable via QUIC_MAX_IDLE_TIMEOUT_SECS)
    transport_config.max_idle_timeout(Some(
        std::time::Duration::from_secs(*QUIC_MAX_IDLE_TIMEOUT_SECS).try_into()?,
    ));

    // Send keep-alive pings to maintain connection (configurable via QUIC_KEEP_ALIVE_INTERVAL_SECS)
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(
        *QUIC_KEEP_ALIVE_INTERVAL_SECS,
    )));

    server_config.transport_config(std::sync::Arc::new(transport_config));

    // Create Quinn endpoint with our custom config
    let endpoint = quinn::Endpoint::server(server_config, opt.listen)?;

    let mut server = web_transport_quinn::Server::new(endpoint);

    info!(
        "listening on {} with {}s idle timeout and {}s keep-alive",
        opt.listen, *QUIC_MAX_IDLE_TIMEOUT_SECS, *QUIC_KEEP_ALIVE_INTERVAL_SECS
    );

    // Accept new WebTransport connections
    while let Some(request) = server.accept().await {
        trace_span!("New connection being attempted");
        let chat_server = chat_server.clone();
        let nats_client = nats_client.clone();
        let pool = pool.clone();
        let tracker_sender = tracker_sender.clone();
        let session_manager = session_manager.clone();
        tokio::spawn(async move {
            if let Err(err) = run_webtransport_connection_from_request(
                request,
                chat_server,
                nats_client,
                pool,
                tracker_sender,
                session_manager,
            )
            .await
            {
                error!("Failed to handle WebTransport connection: {err:?}");
            }
        });
    }

    Ok(())
}

async fn run_webtransport_connection_from_request(
    request: web_transport_quinn::Request,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    _pool: Option<PgPool>,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
) -> anyhow::Result<()> {
    warn!("received WebTransport request: {}", request.url());
    let url = request.url();

    let uri = url;
    let path = urlencoding::decode(uri.path()).unwrap().into_owned();

    let parts = path.split('/').collect::<Vec<&str>>();
    // filter out the empty strings
    let parts = parts.iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
    info!("Parts {:?}", parts);
    if parts.len() != 3 {
        return Err(anyhow!("Invalid path wrong length"));
    } else if parts[0] != &"lobby" {
        return Err(anyhow!("Invalid path wrong prefix"));
    }

    let username = parts[1].replace(' ', "_");
    let lobby_id = parts[2].replace(' ', "_");
    let re = regex::Regex::new(VALID_ID_PATTERN).unwrap();
    if !re.is_match(&username) && !re.is_match(&lobby_id) {
        return Err(anyhow!("Invalid path input chars"));
    }

    // Accept the session.
    let session = request.ok().await.context("failed to accept session")?;
    debug!("accepted session");

    // Run the session with actor
    if let Err(err) = handle_webtransport_session(
        session,
        &username,
        &lobby_id,
        chat_server,
        nats_client,
        tracker_sender,
        session_manager,
    )
    .await
    {
        info!("closing session: {}", err);
    }
    Ok(())
}

/// Handle a WebTransport session using the WtChatSession actor
#[tracing::instrument(
    level = "trace",
    skip(session, chat_server, nats_client, tracker_sender, session_manager)
)]
async fn handle_webtransport_session(
    session: Session,
    username: &str,
    lobby_id: &str,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
) -> anyhow::Result<()> {
    // Create channel for actor → WebTransport I/O
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<WtOutbound>(256);

    // Start the WtChatSession actor
    let actor = WtChatSession::new(
        chat_server,
        lobby_id.to_string(),
        username.to_string(),
        outbound_tx,
        nats_client,
        tracker_sender,
        session_manager,
    );
    let actor_addr = actor.start();

    let mut join_set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    // UniStream reader task: reads from quinn Session, sends WtInbound to actor
    {
        let session = session.clone();
        let actor_addr = actor_addr.clone();
        #[cfg_attr(not(test), allow(unused_variables))]
        let username_clone = username.to_string();
        join_set.spawn(async move {
            while let Ok(mut uni_stream) = session.accept_uni().await {
                let actor_addr = actor_addr.clone();
                #[cfg(test)]
                let username_for_test = username_clone.clone();
                tokio::spawn(async move {
                    match uni_stream.read_to_end(usize::MAX).await {
                        Ok(buf) => {
                            #[cfg(test)]
                            increment_test_packet_counter_for_user(&username_for_test);
                            let _ = actor_addr.try_send(WtInbound {
                                data: Bytes::from(buf),
                                source: WtInboundSource::UniStream,
                            });
                        }
                        Err(e) => {
                            error!("Error reading from unidirectional stream: {}", e);
                        }
                    }
                });
            }
            info!("WebTransport UniStream reader task ended");
        });
    }

    // Datagram reader task: reads datagrams from quinn Session, sends WtInbound to actor
    {
        let session = session.clone();
        let actor_addr = actor_addr.clone();
        join_set.spawn(async move {
            while let Ok(buf) = session.read_datagram().await {
                let _ = actor_addr.try_send(WtInbound {
                    data: buf,
                    source: WtInboundSource::Datagram,
                });
            }
            info!("WebTransport Datagram reader task ended");
        });
    }

    // Writer task: receives from outbound channel, writes to quinn Session
    {
        let session = session.clone();
        join_set.spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                match msg {
                    WtOutbound::UniStream(data) => match session.open_uni().await {
                        Ok(mut stream) => {
                            if let Err(e) = stream.write_all(&data).await {
                                error!("Error writing to UniStream: {}", e);
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Error opening UniStream: {}", e);
                            break;
                        }
                    },
                    WtOutbound::Datagram(data) => {
                        if let Err(e) = session.send_datagram(data) {
                            error!("Error sending datagram: {}", e);
                            // Don't break on datagram errors - they're unreliable
                        }
                    }
                }
            }
            info!("WebTransport Writer task ended");
        });
    }

    // Wait for any task to finish (indicates session end)
    join_set.join_next().await;
    join_set.shutdown().await;

    // Actor will handle cleanup via its stopping() method
    warn!("Finished handling WebTransport session for {username} in {lobby_id}");
    Ok(())
}

// Note: handle_send_connection_started and handle_send_connection_ended are now
// handled by the WtChatSession actor internally via ChatServer coordination.

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::crypto::CryptoProvider;
    use std::time::Duration;
    use videocall_types::protos::media_packet::media_packet::MediaType as VcMediaType;
    use videocall_types::protos::media_packet::MediaPacket as VcMediaPacket;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType as VcPacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper as VcPacketWrapper;

    const KEEP_ALIVE_PING: &[u8] = b"ping";

    async fn start_webtransport_server() -> tokio::task::JoinHandle<()> {
        if let Err(e) = CryptoProvider::install_default(rustls::crypto::ring::default_provider()) {
            error!("Error installing crypto provider: {e:?}");
        }
        use crate::actors::chat_server::ChatServer;
        use crate::db_pool;
        use crate::server_diagnostics::ServerDiagnostics;
        use crate::session_manager::SessionManager;
        use crate::webtransport::{self, Certs};
        use actix::Actor;
        use std::net::ToSocketAddrs;
        use videocall_types::feature_flags::FeatureFlags;

        // Connect to NATS
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        // Create database pool only if enabled
        let pool = if FeatureFlags::database_enabled() {
            Some(
                db_pool::create_pool()
                    .await
                    .expect("Failed to create database pool"),
            )
        } else {
            None
        };

        // Start ChatServer actor
        let chat_server = ChatServer::new(nats_client.clone(), pool.clone())
            .await
            .start();

        // Create SessionManager
        let session_manager = SessionManager::new(pool.clone());

        // Create connection tracker
        let (_, tracker_sender, tracker_receiver) =
            ServerDiagnostics::new_with_channel(nats_client.clone());

        // Start tracker message loop
        tokio::spawn(async move {
            let connection_tracker =
                std::sync::Arc::new(ServerDiagnostics::new(nats_client.clone()));
            connection_tracker.run_message_loop(tracker_receiver).await;
        });

        // Start WebTransport server
        let opt = webtransport::WebTransportOpt {
            listen: std::env::var("LISTEN_URL")
                .unwrap_or_else(|_| "0.0.0.0:4433".to_string())
                .to_socket_addrs()
                .expect("expected LISTEN_URL to be a valid socket address")
                .next()
                .expect("expected LISTEN_URL to be a valid socket address"),
            certs: Certs {
                key: std::env::var("KEY_PATH")
                    .unwrap_or_else(|_| "certs/localhost.key".to_string())
                    .into(),
                cert: std::env::var("CERT_PATH")
                    .unwrap_or_else(|_| "certs/localhost.pem".to_string())
                    .into(),
            },
        };

        tokio::spawn(async move {
            if let Err(e) = webtransport::start(
                opt,
                chat_server,
                nats_client,
                pool,
                tracker_sender,
                session_manager,
            )
            .await
            {
                eprintln!("WebTransport server error: {e}");
            }
        })
    }

    async fn wait_for_condition<F, Fut, T>(
        mut condition: F,
        timeout_duration: Duration,
        check_interval: Duration,
    ) -> Result<T, &'static str>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout_duration {
            if let Some(result) = condition().await {
                return Ok(result);
            }
            tokio::time::sleep(check_interval).await;
        }
        Err("Condition not met within timeout")
    }

    async fn wait_for_condition_bool<F, Fut>(
        condition: F,
        timeout_duration: Duration,
        check_interval: Duration,
    ) -> Result<(), &'static str>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout_duration {
            if condition().await {
                return Ok(());
            }
            tokio::time::sleep(check_interval).await;
        }
        Err("Condition not met within timeout")
    }

    async fn wait_for_server_ready() {
        let condition = || async {
            match connect_client("test", "test").await {
                Ok(_) => {
                    info!("Server is ready!");
                    true
                }
                Err(e) => {
                    error!("Error connecting to server: {e:?}");
                    info!("Retrying connection to server...");
                    false
                }
            }
        };

        wait_for_condition_bool(
            condition,
            Duration::from_secs(10),
            Duration::from_millis(200),
        )
        .await
        .expect("WebTransport server not ready after 10 seconds");
    }

    async fn connect_client(
        user: &str,
        meeting: &str,
    ) -> Result<web_transport_quinn::Session, Box<dyn std::error::Error>> {
        let base = std::env::var("WEBTRANSPORT_URL")
            .unwrap_or_else(|_| "https://127.0.0.1:4433".to_string());
        let url_str = format!("{}/lobby/{}/{}", base.trim_end_matches('/'), user, meeting);
        let url = url::Url::parse(&url_str)?;

        // Create WebTransport client using 0.7.3 API (same as bot)
        let client = unsafe {
            web_transport_quinn::ClientBuilder::new().with_no_certificate_verification()?
        };

        // Connect using modern API
        Ok(client.connect(url).await?)
    }

    async fn send_packet(session: &web_transport_quinn::Session, bytes: Vec<u8>) {
        let mut s = session.open_uni().await.expect("open uni");
        s.write_all(&bytes).await.expect("write packet");
        // Don't call finish() to avoid closing the session prematurely
    }

    async fn keep_alive(session: &web_transport_quinn::Session) {
        // Send a small datagram to keep connection alive
        let ping_data = KEEP_ALIVE_PING;
        if let Err(e) = session.send_datagram(ping_data.to_vec().into()) {
            debug!("Keep-alive ping failed: {}", e);
        }
    }

    /// Wait for a session to receive its MEETING_STARTED packet.
    /// The server subscribes to NATS BEFORE sending MEETING_STARTED, so receiving
    /// this packet confirms the session is fully initialized and ready to relay packets.
    async fn wait_for_session_ready(
        session: &web_transport_quinn::Session,
        name: &str,
    ) -> Result<(), &'static str> {
        println!("Waiting for {name} to receive MEETING_STARTED...");
        let session_clone = session.clone();
        wait_for_condition(
            || {
                let session = session_clone.clone();
                async move {
                    if let Ok(mut stream) = session.accept_uni().await {
                        if let Ok(buf) = stream.read_to_end(usize::MAX).await {
                            if !buf.is_empty() {
                                if let Ok(wrapper) = VcPacketWrapper::parse_from_bytes(&buf) {
                                    if wrapper.packet_type
                                        == videocall_types::protos::packet_wrapper::packet_wrapper::PacketType::MEETING.into()
                                    {
                                        println!("✓ {name} received MEETING_STARTED - session ready");
                                        return Some(());
                                    }
                                }
                            }
                        }
                    }
                    None
                }
            },
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .map(|_| ())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_relay_packet_webtransport_between_two_clients() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(std::io::stderr)
            .try_init();

        // Wrap entire test with 15 second timeout
        let test_result = tokio::time::timeout(Duration::from_secs(15), async {
            test_relay_packet_impl().await
        })
        .await;

        match test_result {
            Ok(Ok(())) => println!("Test completed successfully"),
            Ok(Err(e)) => panic!("Test failed: {e}"),
            Err(_) => panic!("Test timed out after 15 seconds"),
        }
    }

    async fn test_relay_packet_impl() -> anyhow::Result<()> {
        println!("=== STARTING INTEGRATION TEST ===");

        println!("Starting WebTransport server...");
        let _wt_handle = start_webtransport_server().await;

        println!("Waiting for server to be ready...");
        wait_for_server_ready().await;

        let meeting = "it-meeting-1";
        let user_a = "user-a";
        let user_b = "user-b";

        println!("Connecting client A: {user_a}");
        let session_a = connect_client(user_a, meeting)
            .await
            .expect("connect client A");
        println!("Client A connected!");

        println!("Connecting client B: {user_b}");
        let session_b = connect_client(user_b, meeting)
            .await
            .expect("connect client B");
        println!("Client B connected!");

        // Wait for both sessions to be fully ready (receive MEETING_STARTED)
        // This confirms NATS subscriptions are established
        wait_for_session_ready(&session_a, "client A")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        wait_for_session_ready(&session_b, "client B")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        // Start keep-alive tasks that will be cancelled when test ends
        let session_a_keep = session_a.clone();
        let session_b_keep = session_b.clone();

        let _keep_alive_a = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                keep_alive(&session_a_keep).await;
            }
        });

        let _keep_alive_b = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                keep_alive(&session_b_keep).await;
            }
        });

        // Craft a MEDIA packet that is not RTT and not health
        let media = VcMediaPacket {
            media_type: VcMediaType::AUDIO.into(),
            email: user_a.to_string(),
            ..Default::default()
        };
        let packet = VcPacketWrapper {
            packet_type: VcPacketType::MEDIA.into(),
            email: user_a.to_string(),
            data: media.write_to_bytes().expect("serialize media"),
            ..Default::default()
        };
        let bytes = packet.write_to_bytes().expect("serialize wrapper");

        println!("Sending packet from A to B (size: {} bytes)", bytes.len());
        send_packet(&session_a, bytes.clone()).await;
        println!("Packet sent from A!");

        // Wait for B to receive the packet
        println!("Waiting for packet on B...");
        let session_b_recv = session_b.clone();
        let received = wait_for_condition(
            || {
                let session = session_b_recv.clone();
                async move {
                    if let Ok(mut stream) = session.accept_uni().await {
                        if let Ok(buf) = stream.read_to_end(usize::MAX).await {
                            if !buf.is_empty() {
                                println!("Received packet on B (size: {} bytes)", buf.len());
                                return Some(buf);
                            }
                        }
                    }
                    None
                }
            },
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .expect("Should receive packet from A on B");

        println!("Packet successfully relayed!");
        assert_eq!(bytes, received, "B must receive the exact bytes sent by A");

        println!("=== INTEGRATION TEST PASSED ===");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_lobby_isolation() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(std::io::stderr)
            .try_init();

        // Wrap entire test with 15 second timeout
        let test_result = tokio::time::timeout(Duration::from_secs(15), async {
            test_lobby_isolation_impl().await
        })
        .await;

        match test_result {
            Ok(Ok(())) => println!("Test completed successfully"),
            Ok(Err(e)) => panic!("Test failed: {e}"),
            Err(_) => panic!("Test timed out after 15 seconds"),
        }
    }

    async fn test_lobby_isolation_impl() -> anyhow::Result<()> {
        println!("=== STARTING COMPREHENSIVE LOBBY ISOLATION TEST ===");

        // ========== SETUP ==========
        reset_test_packet_counters();
        println!("✓ Reset packet counters");

        println!("Starting WebTransport server...");
        let _wt_handle = start_webtransport_server().await;
        wait_for_server_ready().await;
        println!("✓ Server ready");

        // ========== CLIENT CONNECTIONS ==========
        let lobby_1 = "lobby-secure";
        let lobby_2 = "lobby-public";
        let user_a = "alice";
        let user_b = "bob";
        let user_c = "charlie";
        assert_eq!(get_test_packet_counter_for_user(user_a), 0);
        assert_eq!(get_test_packet_counter_for_user(user_b), 0);
        assert_eq!(get_test_packet_counter_for_user(user_c), 0);

        println!("\n--- Establishing Connections ---");

        let session_a = connect_client(user_a, lobby_1)
            .await
            .expect("connect alice");
        println!("✓ Alice connected to lobby-secure");

        let session_b = connect_client(user_b, lobby_2).await.expect("connect bob");
        println!("✓ Bob connected to lobby-public");

        let session_c = connect_client(user_c, lobby_1)
            .await
            .expect("connect charlie");
        println!("✓ Charlie connected to lobby-secure");

        // Wait for all sessions to be fully ready (receive MEETING_STARTED)
        // This confirms NATS subscriptions are established before sending test packets
        wait_for_session_ready(&session_a, "Alice")
            .await
            .expect("Alice session ready");
        wait_for_session_ready(&session_b, "Bob")
            .await
            .expect("Bob session ready");
        wait_for_session_ready(&session_c, "Charlie")
            .await
            .expect("Charlie session ready");

        // Keep connections alive
        start_keep_alive_tasks(&session_a, &session_b, &session_c).await;

        // ========== PHASE 1: CROSS-LOBBY ISOLATION TEST ==========
        println!("\n--- Phase 1: Testing Cross-Lobby Isolation ---");

        let [count_a, count_b, count_c] = get_all_user_counts(&[user_a, user_b, user_c])[..] else {
            panic!("Expected exactly 3 user counts");
        };
        println!("Initial counts: A={count_a}, B={count_b}, C={count_c}");

        // Alice sends 3 packets to her lobby (should only reach Charlie, not Bob)
        for i in 1..=3 {
            let packet = create_test_packet(user_a, VcMediaType::AUDIO, format!("alice-msg-{i}"));
            send_packet(&session_a, packet).await;
            println!("✓ Alice sent packet #{i}");
        }

        // Wait for Charlie to receive Alice's 3 packets
        let expected_charlie_count = count_c + 3;
        wait_for_condition_bool(
            || async move { get_test_packet_counter_for_user(user_c) >= expected_charlie_count },
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .expect("Charlie should receive Alice's packets");

        let [alice_count_after, bob_count_after, charlie_count_after] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "After Alice's packets: A={alice_count_after}, B={bob_count_after}, C={charlie_count_after}"
        );

        // Bob should have received ZERO packets (different lobby)
        assert_eq!(
            bob_count_after, count_b,
            "❌ ISOLATION BREACH: Bob in lobby-public received packets from Alice in lobby-secure!"
        );
        println!("✅ Confirmed: Bob (lobby-public) isolated from Alice's packets");

        // Alice should NOT receive her own packets back (no self-echo)
        assert_eq!(
            alice_count_after, count_a,
            "❌ Alice received her own packets back! Self-echo should be prevented."
        );
        println!("✅ Confirmed: Alice does not receive her own packets back (no self-echo)");

        // Charlie should have received Alice's packets (same lobby)
        assert!(
            charlie_count_after >= count_c + 3,
            "❌ Charlie should have received Alice's 3 packets, but only got {} new packets",
            charlie_count_after - count_c
        );
        println!("✅ Confirmed: Charlie (lobby-secure) received Alice's packets");

        // ========== PHASE 2: BIDIRECTIONAL SAME-LOBBY TEST ==========
        println!("\n--- Phase 2: Testing Bidirectional Same-Lobby Communication ---");

        let [alice_before_bidi, bob_before_bidi, charlie_before_bidi] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };

        // Charlie responds to Alice with 2 packets
        for i in 1..=2 {
            let packet =
                create_test_packet(user_c, VcMediaType::VIDEO, format!("charlie-reply-{i}"));
            send_packet(&session_c, packet).await;
            println!("✓ Charlie sent reply #{i}");
        }

        // Wait for Alice to receive Charlie's 2 packets
        let expected_alice_count = alice_before_bidi + 2;
        wait_for_condition_bool(
            || async move { get_test_packet_counter_for_user(user_a) >= expected_alice_count },
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .expect("Alice should receive Charlie's replies");

        let [alice_after_bidi, bob_after_bidi, charlie_after_bidi] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "After Charlie's replies: A={alice_after_bidi}, B={bob_after_bidi}, C={charlie_after_bidi}"
        );

        // Alice should receive Charlie's replies
        assert!(
            alice_after_bidi >= alice_before_bidi + 2,
            "❌ Alice should have received Charlie's 2 replies"
        );
        println!("✅ Confirmed: Bidirectional communication within lobby-secure works");

        // Charlie should NOT receive his own packets back (no self-echo)
        assert_eq!(
            charlie_after_bidi, charlie_before_bidi,
            "❌ Charlie received his own packets back! Self-echo should be prevented."
        );
        println!("✅ Confirmed: Charlie does not receive his own packets back (no self-echo)");

        // Bob should still have zero new packets
        assert_eq!(
            bob_after_bidi, bob_before_bidi,
            "❌ ISOLATION BREACH: Bob received packets from Charlie!"
        );
        println!("✅ Confirmed: Bob remains isolated from lobby-secure traffic");

        // ========== PHASE 3: ISOLATED LOBBY COMMUNICATION ==========
        println!("\n--- Phase 3: Testing Bob's Isolated Communication ---");

        let [alice_before_bob, bob_before_bob, charlie_before_bob] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };

        // Bob sends packet in his own lobby (should not reach Alice, Charlie, or echo back to himself)
        let packet = create_test_packet(user_b, VcMediaType::AUDIO, "bob-isolated-msg".to_string());
        send_packet(&session_b, packet).await;
        println!("✓ Bob sent packet in lobby-public");

        // Wait to verify isolation - expect this to timeout since no one should receive Bob's packet
        // We use a short timeout since we're verifying nothing happens
        let _ = wait_for_condition_bool(
            || async move {
                // This should never become true - Bob is alone in his lobby
                get_test_packet_counter_for_user(user_a) > alice_before_bob
                    || get_test_packet_counter_for_user(user_b) > bob_before_bob
                    || get_test_packet_counter_for_user(user_c) > charlie_before_bob
            },
            Duration::from_millis(300), // Short timeout - we expect this to fail
            Duration::from_millis(50),
        )
        .await;
        // We don't care if this times out - that's expected behavior

        let [alice_after_bob, bob_after_bob, charlie_after_bob] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "After Bob's packet: A={alice_after_bob}, B={bob_after_bob}, C={charlie_after_bob}"
        );

        // Alice and Charlie should not receive Bob's packet
        assert_eq!(
            alice_after_bob, alice_before_bob,
            "❌ Alice received Bob's packet across lobbies!"
        );
        assert_eq!(
            charlie_after_bob, charlie_before_bob,
            "❌ Charlie received Bob's packet across lobbies!"
        );

        // Bob should NOT receive his own packet back
        assert_eq!(
            bob_after_bob, bob_before_bob,
            "❌ Bob received his own packet back! Self-echo should be prevented."
        );

        println!("✅ Confirmed: Bob's packets isolated to lobby-public");
        println!("✅ Confirmed: Bob does not receive his own packets back (no self-echo)");

        // ========== SUMMARY ==========
        println!("\n=== COMPREHENSIVE LOBBY ISOLATION TEST PASSED ===");
        let [alice_final, bob_final, charlie_final] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!("Final packet counts:");
        println!("  • Alice (lobby-secure): {alice_final}");
        println!("  • Bob   (lobby-public):  {bob_final}");
        println!("  • Charlie (lobby-secure): {charlie_final}");
        println!("✅ All lobby isolation requirements verified!");
        println!("✅ Self-echo prevention verified for all users!");

        Ok(())
    }

    // ========== HELPER FUNCTIONS ==========

    async fn start_keep_alive_tasks(
        session_a: &web_transport_quinn::Session,
        session_b: &web_transport_quinn::Session,
        session_c: &web_transport_quinn::Session,
    ) {
        let session_a_keep = session_a.clone();
        let session_b_keep = session_b.clone();
        let session_c_keep = session_c.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(8));
            loop {
                interval.tick().await;
                keep_alive(&session_a_keep).await;
            }
        });

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(8));
            loop {
                interval.tick().await;
                keep_alive(&session_b_keep).await;
            }
        });

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(8));
            loop {
                interval.tick().await;
                keep_alive(&session_c_keep).await;
            }
        });
    }

    fn create_test_packet(sender: &str, media_type: VcMediaType, _message: String) -> Vec<u8> {
        let media = VcMediaPacket {
            media_type: media_type.into(),
            email: sender.to_string(),
            ..Default::default()
        };
        let packet = VcPacketWrapper {
            packet_type: VcPacketType::MEDIA.into(),
            email: sender.to_string(),
            data: media.write_to_bytes().expect("serialize media"),
            ..Default::default()
        };
        packet.write_to_bytes().expect("serialize wrapper")
    }

    fn get_all_user_counts(users: &[&str]) -> Vec<u64> {
        users
            .iter()
            .map(|user| get_test_packet_counter_for_user(user))
            .collect()
    }

    // ==========================================================================
    // Meeting Lifecycle Integration Test (WebTransport)
    // Tests: meeting creation, participant join/leave, meeting end
    // ==========================================================================

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn test_meeting_lifecycle_webtransport() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(std::io::stderr)
            .try_init();

        // Enable meeting management for this test
        videocall_types::FeatureFlags::set_meeting_management_override(true);

        let test_result = tokio::time::timeout(Duration::from_secs(30), async {
            test_meeting_lifecycle_impl().await
        })
        .await;

        // Clean up feature flag
        videocall_types::FeatureFlags::clear_meeting_management_override();

        match test_result {
            Ok(Ok(())) => println!("Test completed successfully"),
            Ok(Err(e)) => panic!("Test failed: {e}"),
            Err(_) => panic!("Test timed out after 30 seconds"),
        }
    }

    async fn test_meeting_lifecycle_impl() -> anyhow::Result<()> {
        use crate::models::meeting::Meeting;
        use crate::models::session_participant::SessionParticipant;

        println!("=== STARTING MEETING LIFECYCLE TEST (WebTransport) ===");

        // Get database pool for verification queries
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this test");
        let pool = sqlx::PgPool::connect(&database_url).await?;

        // Clean up any stale test data
        let room_id = "wt-meeting-lifecycle-test";
        cleanup_room(&pool, room_id).await;

        println!("Starting WebTransport server...");
        let _wt_handle = start_webtransport_server().await;
        wait_for_server_ready().await;
        println!("✓ Server ready");

        // ========== STEP 1: First user connects - meeting should be created ==========
        println!("\n--- Step 1: Alice connects (first participant) ---");

        let session_alice = connect_client("alice", room_id)
            .await
            .expect("connect alice");
        wait_for_session_ready(&session_alice, "Alice")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Alice connected");

        // Verify: 1 participant, meeting exists
        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(
            count, 1,
            "Should have 1 active participant after Alice joins"
        );
        println!("✓ Participant count: {count}");

        let meeting = Meeting::get_by_room_id_async(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert!(
            meeting.is_some(),
            "Meeting should exist after first participant joins"
        );
        let meeting = meeting.unwrap();
        assert_eq!(
            meeting.creator_id,
            Some("alice".to_string()),
            "Alice should be the meeting creator"
        );
        assert!(meeting.ended_at.is_none(), "Meeting should not be ended");
        let start_time = meeting.start_time_unix_ms();
        println!("✓ Meeting created with creator=alice, start_time={start_time}");

        // ========== STEP 2: Second user connects - participant count increases ==========
        println!("\n--- Step 2: Bob connects (second participant) ---");

        let session_bob = connect_client("bob", room_id).await.expect("connect bob");
        wait_for_session_ready(&session_bob, "Bob")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Bob connected");

        // Verify: 2 participants
        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(
            count, 2,
            "Should have 2 active participants after Bob joins"
        );
        println!("✓ Participant count: {count}");

        // Meeting should have same start time
        let meeting = Meeting::get_by_room_id_async(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .unwrap();
        assert_eq!(
            meeting.start_time_unix_ms(),
            start_time,
            "Meeting start time should not change when others join"
        );
        println!("✓ Meeting start time unchanged");

        // ========== STEP 3: Third user connects ==========
        println!("\n--- Step 3: Charlie connects (third participant) ---");

        let session_charlie = connect_client("charlie", room_id)
            .await
            .expect("connect charlie");
        wait_for_session_ready(&session_charlie, "Charlie")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Charlie connected");

        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(count, 3, "Should have 3 active participants");
        println!("✓ Participant count: {count}");

        // ========== STEP 4: Charlie disconnects - count drops ==========
        println!("\n--- Step 4: Charlie disconnects ---");

        // Explicitly close the session to trigger immediate disconnect
        session_charlie.close(0u32, b"test disconnect");
        drop(session_charlie);
        // Wait for disconnect to be processed
        wait_for_participant_count(&pool, room_id, 2, Duration::from_secs(5)).await?;
        println!("✓ Charlie disconnected");

        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(
            count, 2,
            "Should have 2 active participants after Charlie leaves"
        );
        println!("✓ Participant count: {count}");

        // Meeting should still be active
        let meeting = Meeting::get_by_room_id_async(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .unwrap();
        assert!(meeting.ended_at.is_none(), "Meeting should still be active");
        println!("✓ Meeting still active");

        // ========== STEP 5: Bob disconnects - count drops ==========
        println!("\n--- Step 5: Bob disconnects ---");

        session_bob.close(0u32, b"test disconnect");
        drop(session_bob);
        wait_for_participant_count(&pool, room_id, 1, Duration::from_secs(5)).await?;
        println!("✓ Bob disconnected");

        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(
            count, 1,
            "Should have 1 active participant after Bob leaves"
        );
        println!("✓ Participant count: {count}");

        // Meeting should still be active (Alice is still there)
        let meeting = Meeting::get_by_room_id_async(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .unwrap();
        assert!(
            meeting.ended_at.is_none(),
            "Meeting should still be active with Alice"
        );
        println!("✓ Meeting still active");

        // ========== STEP 6: Alice (host/last) disconnects - meeting ends ==========
        println!("\n--- Step 6: Alice (host) disconnects - meeting should end ---");

        session_alice.close(0u32, b"test disconnect");
        drop(session_alice);
        wait_for_participant_count(&pool, room_id, 0, Duration::from_secs(5)).await?;
        println!("✓ Alice disconnected");

        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(count, 0, "Should have 0 active participants");
        println!("✓ Participant count: {count}");

        // Meeting should be ended
        let meeting = Meeting::get_by_room_id_async(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .unwrap();
        assert!(
            meeting.ended_at.is_some(),
            "Meeting should be ended when last participant leaves"
        );
        println!("✓ Meeting ended at {:?}", meeting.ended_at);

        // ========== CLEANUP ==========
        cleanup_room(&pool, room_id).await;

        println!("\n=== MEETING LIFECYCLE TEST PASSED (WebTransport) ===");
        Ok(())
    }

    async fn cleanup_room(pool: &sqlx::PgPool, room_id: &str) {
        let _ = sqlx::query("DELETE FROM session_participants WHERE room_id = $1")
            .bind(room_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM meetings WHERE room_id = $1")
            .bind(room_id)
            .execute(pool)
            .await;
    }

    async fn wait_for_participant_count(
        pool: &sqlx::PgPool,
        room_id: &str,
        expected: i64,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        use crate::models::session_participant::SessionParticipant;
        let room = room_id.to_string();
        let pool = pool.clone();
        wait_for_condition_bool(
            || {
                let pool = pool.clone();
                let room = room.clone();
                async move {
                    SessionParticipant::count_active(&pool, &room)
                        .await
                        .unwrap_or(-1)
                        == expected
                }
            },
            timeout,
            Duration::from_millis(100),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
    }
}
