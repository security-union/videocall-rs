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

use crate::client_diagnostics::health_processor;
use crate::constants::VALID_ID_PATTERN;
use crate::server_diagnostics::{
    send_connection_ended, send_connection_started, DataTracker, ServerDiagnostics, TrackerSender,
};
use anyhow::{anyhow, Context, Result};
use async_nats::Subject;
use futures::StreamExt;
use protobuf::Message;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::Read;
use std::{fs, io};
use std::{net::SocketAddr, path::PathBuf};
use tracing::{debug, error, info, trace, trace_span};

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

use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use web_transport_quinn::{Session, SessionError};

/// Videocall WebTransport API
///
/// This module contains the implementation of the Videocall WebTransport API.
/// It is responsible for accepting incoming WebTransport connections and handling them.
/// It also contains the logic for handling the WebTransport handshake and the WebTransport session.
///
///
pub const WEB_TRANSPORT_ALPN: &[&[u8]] = &[b"h3", b"h3-32", b"h3-31", b"h3-30", b"h3-29"];

pub const QUIC_ALPN: &[u8] = b"hq-29";

const KEEP_ALIVE_PING: &[u8] = b"ping";

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

pub async fn start(opt: WebTransportOpt) -> Result<(), Box<dyn std::error::Error>> {
    info!("WebTransportOpt: {opt:#?}");

    let (key, certs) = get_key_and_cert_chain(opt.certs)?;

    // Use ServerBuilder for 0.7.3 API
    let mut server = web_transport_quinn::ServerBuilder::new()
        .with_addr(opt.listen)
        .with_certificate(certs, key)?;

    info!("listening on {}", opt.listen);

    let nc =
        async_nats::connect(std::env::var("NATS_URL").expect("NATS_URL env var must be defined"))
            .await
            .unwrap();

    // Create connection tracker with message channel
    let (connection_tracker, tracker_sender, tracker_receiver) =
        ServerDiagnostics::new_with_channel(nc.clone());

    // Start the connection tracker message processing task
    let connection_tracker = std::sync::Arc::new(connection_tracker);
    let tracker_task = connection_tracker.clone();
    tokio::spawn(async move {
        tracker_task.run_message_loop(tracker_receiver).await;
    });

    // 2. Accept new WebTransport connections using 0.7.3 API
    while let Some(request) = server.accept().await {
        trace_span!("New connection being attempted");
        let nc = nc.clone();
        let tracker_sender = tracker_sender.clone();
        tokio::spawn(async move {
            // Handle WebTransport request directly using 0.7.3 API
            if let Err(err) =
                run_webtransport_connection_from_request(request, nc, tracker_sender).await
            {
                error!("Failed to handle WebTransport connection: {err:?}");
            }
        });
    }

    Ok(())
}

async fn run_webtransport_connection_from_request(
    request: web_transport_quinn::Request,
    nc: async_nats::client::Client,
    tracker_sender: TrackerSender,
) -> anyhow::Result<()> {
    info!("received WebTransport request: {}", request.url());
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
    info!("accepted session");

    // Run the session
    if let Err(err) =
        handle_webtransport_session(session, &username, &lobby_id, nc, tracker_sender).await
    {
        info!("closing session: {}", err);
    }
    Ok(())
}

#[tracing::instrument(level = "trace", skip(session))]
async fn handle_webtransport_session(
    session: Session,
    username: &str,
    lobby_id: &str,
    nc: async_nats::client::Client,
    tracker_sender: TrackerSender,
) -> anyhow::Result<()> {
    // Generate unique session ID for this WebTransport connection
    let session_id = uuid::Uuid::new_v4().to_string();

    // Track connection start for metrics
    send_connection_started(
        &tracker_sender,
        session_id.clone(),
        username.to_string(),
        lobby_id.to_string(),
        "webtransport".to_string(),
    );

    let mut join_set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    let subject = format!("room.{lobby_id}.*").replace(' ', "_");
    let specific_subject: Subject = format!("room.{lobby_id}.{username}")
        .replace(' ', "_")
        .into();
    let mut sub = match nc
        .queue_subscribe(subject.clone(), specific_subject.to_string())
        .await
    {
        Ok(sub) => {
            info!("Subscribed to subject {subject}");
            sub
        }
        Err(e) => {
            let err = format!("error subscribing to subject {subject}: {e}");
            error!("{err}");
            return Err(anyhow!(err));
        }
    };

    let specific_subject_clone = specific_subject.clone();

    // NATS receive task
    {
        let session = session.clone();
        let session_id_clone = session_id.clone();
        let tracker_sender_nats = tracker_sender.clone();
        #[cfg_attr(not(test), allow(unused_variables))]
        let username_clone = username.to_string();
        join_set.spawn(async move {
            let _data_tracker = DataTracker::new(tracker_sender_nats.clone());
            while let Some(msg) = sub.next().await {
                if msg.subject == specific_subject_clone {
                    continue;
                }

                #[cfg(test)]
                increment_test_packet_counter_for_user(&username_clone);
                let session_id_clone = session_id_clone.clone();
                let payload_size = msg.payload.len() as u64;
                let tracker_sender_inner = tracker_sender_nats.clone();
                let session = session.clone();
                tokio::spawn(async move {
                    let stream = session.open_uni().await;
                    let data_tracker_inner = DataTracker::new(tracker_sender_inner);
                    match stream {
                        Ok(mut uni_stream) => {
                            if let Err(e) = uni_stream.write_all(&msg.payload).await {
                                error!("Error writing to unidirectional stream: {}", e);
                            } else {
                                // Track data sent
                                data_tracker_inner.track_sent(&session_id_clone, payload_size);
                            }
                        }
                        Err(SessionError::ConnectionError(e)) => {
                            error!("Connection error: {}", e);
                        }
                        Err(SessionError::WebTransportError(e)) => {
                            error!("WebTransport error: {}", e);
                        }
                        Err(e) => {
                            error!("Error opening unidirectional stream: {}", e);
                        }
                    }
                });
            }
        });
    }

    // WebTransport unidirectional receive task
    {
        let session = session.clone();
        let nc = nc.clone();
        let specific_subject = specific_subject.clone();
        let session_id_clone = session_id.clone();
        let tracker_sender_wt = tracker_sender.clone();
        join_set.spawn(async move {
            while let Ok(mut uni_stream) = session.accept_uni().await {
                let nc = nc.clone();
                let specific_subject = specific_subject.clone();
                let session_clone = session.clone();
                let session_id_clone = session_id_clone.clone();
                let tracker_sender_inner = tracker_sender_wt.clone();
                tokio::spawn(async move {
                    let data_tracker = DataTracker::new(tracker_sender_inner);
                    let result = uni_stream.read_to_end(usize::MAX).await;
                    match result {
                        Ok(buf) => {
                            let buf_size = buf.len() as u64;
                            // Track data received
                            data_tracker.track_received(&session_id_clone, buf_size);

                            // Check if this is an RTT packet that should be echoed back
                            if is_rtt_packet(&buf) {
                                trace!("Echoing RTT packet back via WebTransport");
                                match session_clone.open_uni().await {
                                    Ok(mut echo_stream) => {
                                        if let Err(e) = echo_stream.write_all(&buf).await {
                                            error!("Error echoing RTT packet: {}", e);
                                        } else {
                                            // Track data sent for echo
                                            data_tracker.track_sent(&session_id_clone, buf_size);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Error opening echo stream: {}", e);
                                    }
                                }
                            } else if health_processor::is_health_packet_bytes(&buf) {
                                // Process health packet for diagnostics (don't relay)
                                debug!("WT-SERVER: Received health packet via unidirectional stream (size: {} bytes) - processing locally", buf.len());
                                health_processor::process_health_packet_bytes(&buf, nc.clone());
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
        });
    }

    // WebTransport datagram receive task
    {
        let session_id_clone = session_id.clone();
        let tracker_sender_wt_datagram = tracker_sender.clone();
        join_set.spawn(async move {
            let data_tracker = DataTracker::new(tracker_sender_wt_datagram);
            while let Ok(buf) = session.read_datagram().await {
                let buf_size = buf.len() as u64;
                // Track data received
                data_tracker.track_received(&session_id_clone, buf_size);

                // Check if this is an RTT packet that should be echoed back
                if is_rtt_packet(&buf) {
                    debug!("Echoing RTT datagram back via WebTransport");
                    if let Err(e) = session.send_datagram(buf.clone()) {
                        error!("Error echoing RTT datagram: {}", e);
                    } else {
                        // Track data sent for echo
                        data_tracker.track_sent(&session_id_clone, buf_size);
                    }
                } else if health_processor::is_health_packet_bytes(&buf) {
                    // Process health packet for diagnostics (don't relay)
                    health_processor::process_health_packet_bytes(&buf, nc.clone());
                } else if buf.as_ref() == KEEP_ALIVE_PING {
                    // Keep-alive packet - don't relay, just ignore
                } else {
                    // Normal datagram processing - publish to NATS
                    let nc = nc.clone();
                    if let Err(e) = nc.publish(specific_subject.clone(), buf).await {
                        error!("Error publishing to subject {}: {}", specific_subject, e);
                    }
                }
            }
        });
    }

    join_set.join_next().await;
    join_set.shutdown().await;

    // Track connection end for metrics
    send_connection_ended(&tracker_sender, session_id.clone());

    info!("Finished handling session: {}", session_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::crypto::CryptoProvider;
    use std::time::Duration;
    use videocall_types::protos::media_packet::media_packet::MediaType as VcMediaType;
    use videocall_types::protos::media_packet::MediaPacket as VcMediaPacket;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType as VcPacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper as VcPacketWrapper;

    async fn start_webtransport_server() -> tokio::task::JoinHandle<()> {
        if let Err(e) = CryptoProvider::install_default(rustls::crypto::ring::default_provider()) {
            error!("Error installing crypto provider: {e:?}");
        }
        use crate::webtransport::{self, Certs};
        use std::net::ToSocketAddrs;

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
            if let Err(e) = webtransport::start(opt).await {
                eprintln!("WebTransport server error: {}", e);
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
            Ok(Err(e)) => panic!("Test failed: {}", e),
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

        println!("Connecting client A: {}", user_a);
        let session_a = connect_client(user_a, meeting)
            .await
            .expect("connect client A");
        println!("Client A connected!");

        println!("Connecting client B: {}", user_b);
        let session_b = connect_client(user_b, meeting)
            .await
            .expect("connect client B");
        println!("Client B connected!");

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
        // Send from A
        send_packet(&session_a, bytes.clone()).await;
        println!("Packet sent from A!");

        // Give NATS time to relay the message
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        println!("Waiting for packet on B...");
        // Receive on B
        let condition = || async {
            match session_b.accept_uni().await {
                Ok(mut stream) => {
                    if let Ok(buf) = stream.read_to_end(usize::MAX).await {
                        if !buf.is_empty() {
                            println!("Received packet on B (size: {} bytes)", buf.len());
                            return Some(buf);
                        }
                    }
                }
                Err(_) => {}
            }
            None
        };

        let received =
            wait_for_condition(condition, Duration::from_secs(5), Duration::from_millis(50))
                .await
                .expect("timely receive");

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
            Ok(Err(e)) => panic!("Test failed: {}", e),
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

        // Keep connections alive
        start_keep_alive_tasks(&session_a, &session_b, &session_c).await;

        // ========== PHASE 1: CROSS-LOBBY ISOLATION TEST ==========
        println!("\n--- Phase 1: Testing Cross-Lobby Isolation ---");

        let [count_a, count_b, count_c] = get_all_user_counts(&[user_a, user_b, user_c])[..] else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "Initial counts: A={}, B={}, C={}",
            count_a, count_b, count_c
        );

        // Alice sends 3 packets to her lobby (should only reach Charlie, not Bob)
        for i in 1..=3 {
            let packet = create_test_packet(user_a, VcMediaType::AUDIO, format!("alice-msg-{}", i));
            send_packet(&session_a, packet).await;
            println!("✓ Alice sent packet #{}", i);
        }

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let [alice_count_after, bob_count_after, charlie_count_after] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "After Alice's packets: A={}, B={}, C={}",
            alice_count_after, bob_count_after, charlie_count_after
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
                create_test_packet(user_c, VcMediaType::VIDEO, format!("charlie-reply-{}", i));
            send_packet(&session_c, packet).await;
            println!("✓ Charlie sent reply #{}", i);
        }

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let [alice_after_bidi, bob_after_bidi, charlie_after_bidi] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "After Charlie's replies: A={}, B={}, C={}",
            alice_after_bidi, bob_after_bidi, charlie_after_bidi
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

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let [alice_after_bob, bob_after_bob, charlie_after_bob] =
            get_all_user_counts(&[user_a, user_b, user_c])[..]
        else {
            panic!("Expected exactly 3 user counts");
        };
        println!(
            "After Bob's packet: A={}, B={}, C={}",
            alice_after_bob, bob_after_bob, charlie_after_bob
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
        println!("  • Alice (lobby-secure): {}", alice_final);
        println!("  • Bob   (lobby-public):  {}", bob_final);
        println!("  • Charlie (lobby-secure): {}", charlie_final);
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
}
