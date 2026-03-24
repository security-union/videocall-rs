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

mod bridge;

use crate::actors::chat_server::ChatServer;
use crate::actors::transports::wt_chat_session::{WtChatSession, WtOutbound};
use crate::constants::VALID_ID_PATTERN;
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use crate::token_validator;
use actix::prelude::*;
use anyhow::{anyhow, Context, Result};
use bridge::WebTransportBridge;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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
    // NOTE: We use actix_rt::spawn instead of tokio::spawn because the WtChatSession
    // actor requires the actix LocalSet context for spawn_local.
    while let Some(request) = server.accept().await {
        trace_span!("New connection being attempted");
        let chat_server = chat_server.clone();
        let nats_client = nats_client.clone();
        let tracker_sender = tracker_sender.clone();
        let session_manager = session_manager.clone();
        actix_rt::spawn(async move {
            if let Err(err) = run_webtransport_connection_from_request(
                request,
                chat_server,
                nats_client,
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
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
) -> anyhow::Result<()> {
    warn!("received WebTransport request: {}", request.url());
    let uri = request.url();
    let path = urlencoding::decode(uri.path()).unwrap().into_owned();

    let parts = path.split('/').collect::<Vec<&str>>();
    // filter out the empty strings
    let parts: Vec<_> = parts.iter().filter(|s| !s.is_empty()).collect();
    info!("Parts {:?}", parts);

    // First part must be "lobby"
    if parts.is_empty() || parts[0] != &"lobby" {
        return Err(anyhow!("Invalid path: must start with /lobby"));
    }

    // Extract ?token= from query string
    let token = uri
        .query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, val)| val.into_owned());

    // Determine username, room, and observer flag from either the JWT or URL path params.
    let (username, lobby_id, observer, display_name) = if let Some(ref tok) = token {
        // Token-based flow: identity and room come from the JWT claims.
        let jwt_secret = std::env::var("JWT_SECRET").unwrap_or_default();
        if jwt_secret.is_empty() {
            return Err(anyhow!("JWT_SECRET not set"));
        }
        let claims = token_validator::decode_room_token(&jwt_secret, tok).map_err(|e| {
            e.log("WT");
            anyhow!("token validation failed: {}", e.client_message())
        })?;
        info!(
            "WT token-based connection: user_id={}, room={}, display_name={}, observer={}",
            claims.sub, claims.room, claims.display_name, claims.observer
        );
        (
            claims.sub,
            claims.room,
            claims.observer,
            claims.display_name,
        )
    } else if !videocall_types::FeatureFlags::meeting_management_enabled() {
        // Deprecated path-based flow (FF=off only): /lobby/{username}/{room}
        if parts.len() != 3 {
            return Err(anyhow!(
                "Invalid path: expected /lobby/{{user_id}}/{{room}} (deprecated) or /lobby?token=<JWT>"
            ));
        }
        let username = parts[1].replace(' ', "_");
        let lobby_id = parts[2].replace(' ', "_");
        let re = regex::Regex::new(VALID_ID_PATTERN).unwrap();
        if !re.is_match(&username) || !re.is_match(&lobby_id) {
            return Err(anyhow!("Invalid path input chars"));
        }
        info!(
            "WT deprecated path-based connection: user_id={}, room={}",
            username, lobby_id
        );
        // display_name fallback: use user_id for deprecated path
        let display = username.clone();
        (username, lobby_id, false, display) // deprecated path-based endpoint: never observer
    } else {
        // FF=on but no token provided
        info!("WT connection rejected: no token provided and meeting management is enabled");
        return Err(anyhow!(
            "room access token is required. Use /lobby?token=<JWT>"
        ));
    };

    // Accept the session.
    let session = request.ok().await.context("failed to accept session")?;
    debug!("accepted session");

    // Run the session with actor
    if let Err(err) = handle_webtransport_session(
        session,
        &username,
        &lobby_id,
        &display_name,
        chat_server,
        nats_client,
        tracker_sender,
        session_manager,
        observer,
    )
    .await
    {
        info!("closing session: {}", err);
    }
    Ok(())
}

/// Handle a WebTransport session using the WtChatSession actor
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    level = "trace",
    skip(session, chat_server, nats_client, tracker_sender, session_manager)
)]
async fn handle_webtransport_session(
    session: Session,
    username: &str,
    lobby_id: &str,
    display_name: &str,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
    observer: bool,
) -> anyhow::Result<()> {
    // Create channel for actor → WebTransport I/O
    let (outbound_tx, outbound_rx) = mpsc::channel::<WtOutbound>(256);

    // Start the WtChatSession actor
    let actor = WtChatSession::new(
        chat_server,
        lobby_id.to_string(),
        username.to_string(),
        display_name.to_string(),
        outbound_tx,
        nats_client,
        tracker_sender,
        session_manager,
        observer,
    );
    let actor_addr = actor.start();

    // Create bridge (with test callback if in test mode)
    #[cfg(any(test, feature = "testing"))]
    let on_packet_sent = {
        let username_for_callback = username.to_string();
        Some(Box::new(move || {
            test_helpers::increment_test_packet_counter_for_user(&username_for_callback)
        }) as bridge::PacketSentCallback)
    };
    #[cfg(not(any(test, feature = "testing")))]
    let on_packet_sent: Option<bridge::PacketSentCallback> = None;

    let mut bridge = WebTransportBridge::new_with_callback(
        session,
        actor_addr.clone(),
        outbound_rx,
        on_packet_sent,
    );
    bridge.wait_for_disconnect().await;
    bridge.shutdown().await;

    // Signal actor to stop
    actor_addr.do_send(crate::actors::transports::wt_chat_session::StopSession);

    warn!("Finished handling WebTransport session for {username} in {lobby_id}");
    Ok(())
}

// Note: handle_send_connection_started and handle_send_connection_ended are now
// handled by the WtChatSession actor internally via ChatServer coordination.

/// Test helpers for packet counting in integration tests.
///
/// These counters are used by the `#[cfg(any(test, feature = "testing"))]` callback in `handle_webtransport_session`
/// to track packets sent to each user. The external integration tests in
/// `tests/webtransport_tests.rs` also read/reset these counters.
#[cfg(any(test, feature = "testing"))]
pub mod test_helpers {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    lazy_static::lazy_static! {
        static ref TEST_PACKET_COUNTERS: Arc<Mutex<HashMap<String, AtomicU64>>> =
            Arc::new(Mutex::new(HashMap::new()));
    }

    /// Increment the packet counter for a given username (called from bridge callback).
    pub fn increment_test_packet_counter_for_user(username: &str) {
        let counters = TEST_PACKET_COUNTERS.clone();
        let mut counters_map = counters.lock().unwrap();
        let counter = counters_map
            .entry(username.to_string())
            .or_insert_with(|| AtomicU64::new(0));
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the current packet counter for a given username.
    pub fn get_test_packet_counter_for_user(username: &str) -> u64 {
        let counters = TEST_PACKET_COUNTERS.clone();
        let counters_map = counters.lock().unwrap();
        counters_map
            .get(username)
            .map(|counter| counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Clear all packet counters.
    pub fn reset_test_packet_counters() {
        let counters = TEST_PACKET_COUNTERS.clone();
        let mut counters_map = counters.lock().unwrap();
        counters_map.clear();
    }
}
