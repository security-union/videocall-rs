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
mod cert_preflight;

use crate::actors::chat_server::ChatServer;
use crate::actors::priority_drop::SharedQueueByteMeter;
use crate::actors::shed_escalation::DownlinkShedEscalation;
use crate::actors::transports::wt_chat_session::{
    WtChatSession, WtOutboundFrame, WT_HEARTBEAT_INTERVAL,
};
use crate::constants::{
    wt_audio_downlink_lane, AudioDownlinkLane, WT_QUIC_DATAGRAM_SEND_BUFFER_BYTES,
    WT_QUIC_KEEP_ALIVE_DEFAULT_SECS, WT_QUIC_MAX_IDLE_TIMEOUT_DEFAULT_SECS,
    WT_QUIC_SEND_WINDOW_BYTES, WT_QUIC_UDP_BUFFER_DEFAULT_BYTES,
};
use crate::metrics::{
    duration_to_millis_f64, forget_connection_path_stats, publish_connection_path_stats,
    ConnectionPathSample, RELAY_UDP_SOCKET_RECV_BUFFER_BYTES, RELAY_UDP_SOCKET_SEND_BUFFER_BYTES,
    RELAY_WT_AUDIO_DOWNLINK_SESSIONS_TOTAL,
};
use crate::relay_shards::SessionShards;
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use crate::token_validator;
use actix::prelude::*;
use anyhow::{anyhow, Context, Result};
use bridge::{DownlinkStreamMode, WebTransportBridge};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::{fs, io};
use std::{net::SocketAddr, path::PathBuf};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace_span, warn};

/// Resolve one positive-integer `QUIC_*` override, loudly (#2742).
///
/// Every one of these is unusable at `0`, and an unparsable value used to take
/// the default in silence.
pub(crate) fn resolve_positive_quic_override(var: &str, raw: Option<&str>, default: u64) -> u64 {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return default;
    };
    match text.parse::<u64>() {
        Ok(0) => {
            warn!("{var}=0 is not a usable QUIC setting; using {default}");
            default
        }
        Ok(value) => value,
        Err(_) => {
            warn!("{var}={text:?} is not a positive integer; using {default}");
            default
        }
    }
}

fn quic_override_u64(var: &str, default: u64) -> u64 {
    resolve_positive_quic_override(var, std::env::var(var).ok().as_deref(), default)
}

fn quic_override_usize(var: &str, default: usize) -> usize {
    let resolved = quic_override_u64(var, default as u64);
    usize::try_from(resolved).unwrap_or(default)
}

lazy_static::lazy_static! {
    static ref QUIC_MAX_IDLE_TIMEOUT_SECS: u64 =
        quic_override_u64("QUIC_MAX_IDLE_TIMEOUT_SECS", WT_QUIC_MAX_IDLE_TIMEOUT_DEFAULT_SECS);

    static ref QUIC_KEEP_ALIVE_INTERVAL_SECS: u64 =
        quic_override_u64("QUIC_KEEP_ALIVE_INTERVAL_SECS", WT_QUIC_KEEP_ALIVE_DEFAULT_SECS);

    static ref QUIC_UDP_RECV_BUFFER_BYTES: usize =
        quic_override_usize("QUIC_UDP_RECV_BUFFER_BYTES", WT_QUIC_UDP_BUFFER_DEFAULT_BYTES);

    static ref QUIC_UDP_SEND_BUFFER_BYTES: usize =
        quic_override_usize("QUIC_UDP_SEND_BUFFER_BYTES", WT_QUIC_UDP_BUFFER_DEFAULT_BYTES);

    static ref QUIC_CONGESTION_CONTROLLER: CongestionController =
        CongestionController::resolve(std::env::var("QUIC_CONGESTION_CONTROLLER").ok().as_deref());

    static ref QUIC_SEND_WINDOW_BYTES: u64 =
        quic_override_u64("QUIC_SEND_WINDOW_BYTES", WT_QUIC_SEND_WINDOW_BYTES);

    static ref QUIC_DATAGRAM_SEND_BUFFER_BYTES: usize = quic_override_usize(
        "QUIC_DATAGRAM_SEND_BUFFER_BYTES",
        WT_QUIC_DATAGRAM_SEND_BUFFER_BYTES,
    );
}

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

/// `QUIC_CONGESTION_CONTROLLER` (#2716). BBR is wired up for a shaped-link A/B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CongestionController {
    Cubic,
    Bbr,
}

impl CongestionController {
    pub(crate) fn resolve(raw: Option<&str>) -> Self {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("") | Some("cubic") => Self::Cubic,
            Some("bbr") => Self::Bbr,
            Some(other) => {
                warn!(
                    "QUIC_CONGESTION_CONTROLLER={other:?} is not one of cubic|bbr; \
                     using cubic"
                );
                Self::Cubic
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cubic => "cubic",
            Self::Bbr => "bbr",
        }
    }

    fn apply(self, transport_config: &mut quinn::TransportConfig) {
        match self {
            Self::Cubic => transport_config
                .congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default())),
            Self::Bbr => transport_config
                .congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default())),
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UdpBufferSizes {
    pub(crate) requested_recv: usize,
    pub(crate) effective_recv: usize,
    pub(crate) requested_send: usize,
    pub(crate) effective_send: usize,
}

/// Bind the relay's one UDP socket with explicit `SO_RCVBUF`/`SO_SNDBUF` and
/// publish requested-vs-effective as gauges (#2716). A failed `setsockopt` is
/// not fatal.
pub(crate) fn bind_udp_socket(
    addr: SocketAddr,
    requested_recv: usize,
    requested_send: usize,
) -> Result<(std::net::UdpSocket, UdpBufferSizes)> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))
        .with_context(|| format!("creating UDP socket for {addr}"))?;

    if let Err(e) = socket.set_recv_buffer_size(requested_recv) {
        warn!("SO_RCVBUF={requested_recv} rejected ({e}); keeping the kernel default");
    }
    if let Err(e) = socket.set_send_buffer_size(requested_send) {
        warn!("SO_SNDBUF={requested_send} rejected ({e}); keeping the kernel default");
    }

    socket
        .bind(&addr.into())
        .with_context(|| format!("binding UDP socket to {addr}"))?;

    let effective_recv = socket.recv_buffer_size().unwrap_or(0);
    let effective_send = socket.send_buffer_size().unwrap_or(0);

    let sizes = UdpBufferSizes {
        requested_recv,
        effective_recv,
        requested_send,
        effective_send,
    };
    publish_udp_buffer_sizes(&sizes);

    let doubles = kernel_doubles_buffer_readback(std::env::consts::OS);
    let granted_recv = granted_buffer_bytes(effective_recv, doubles);
    let granted_send = granted_buffer_bytes(effective_send, doubles);
    if granted_recv < requested_recv || granted_send < requested_send {
        warn!(
            "UDP socket buffers were clamped by the kernel: requested \
             rcv={requested_recv} snd={requested_send}, granted \
             rcv={granted_recv} snd={granted_send} (read back as \
             rcv={effective_recv} snd={effective_send}). Raise net.core.rmem_max \
             and net.core.wmem_max on the host to make the request stick."
        );
    }

    Ok((socket.into(), sizes))
}

/// Linux's `getsockopt` reports DOUBLE the accepted buffer (`man 7 socket`).
/// Takes the OS name so the mapping is testable on any host.
fn kernel_doubles_buffer_readback(target_os: &str) -> bool {
    target_os == "linux"
}

/// Bytes the kernel granted. Without it a Linux host whose `rmem_max` is above
/// half the request reads back MORE than it asked and no clamp warning fires.
fn granted_buffer_bytes(readback: usize, kernel_doubles: bool) -> usize {
    if kernel_doubles {
        readback / 2
    } else {
        readback
    }
}

/// Build the relay's quinn [`quinn::TransportConfig`]. Extracted from `start`
/// (#2716) so `transport_config_pins_the_2716_values` can read it back.
fn build_transport_config() -> Result<quinn::TransportConfig> {
    let mut transport_config = quinn::TransportConfig::default();

    // Detect disconnection after inactivity (configurable via QUIC_MAX_IDLE_TIMEOUT_SECS)
    transport_config.max_idle_timeout(Some(
        std::time::Duration::from_secs(*QUIC_MAX_IDLE_TIMEOUT_SECS).try_into()?,
    ));

    // Send keep-alive pings to maintain connection (configurable via QUIC_KEEP_ALIVE_INTERVAL_SECS)
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(
        *QUIC_KEEP_ALIVE_INTERVAL_SECS,
    )));

    // #2716: quinn's 1 MiB default holds seconds of a full room's audio
    // fan-out, and a late datagram is discarded by the receiver's NetEq anyway.
    transport_config.datagram_send_buffer_size(*QUIC_DATAGRAM_SEND_BUFFER_BYTES);

    // #2716: quinn's 10 MB default holds tens of seconds of video unacked
    // before `write_all` parks, which is what the #1638 shed keys on.
    transport_config.send_window(*QUIC_SEND_WINDOW_BYTES);

    QUIC_CONGESTION_CONTROLLER.apply(&mut transport_config);

    // Pinned to quinn's current default: the bridge spawns one reader task per
    // accepted uni stream, each holding up to `MAX_FRAME_SIZE`.
    transport_config.max_concurrent_uni_streams(100u32.into());

    Ok(transport_config)
}

fn publish_udp_buffer_sizes(sizes: &UdpBufferSizes) {
    RELAY_UDP_SOCKET_RECV_BUFFER_BYTES
        .with_label_values(&["requested"])
        .set(sizes.requested_recv as f64);
    RELAY_UDP_SOCKET_RECV_BUFFER_BYTES
        .with_label_values(&["effective"])
        .set(sizes.effective_recv as f64);
    RELAY_UDP_SOCKET_SEND_BUFFER_BYTES
        .with_label_values(&["requested"])
        .set(sizes.requested_send as f64);
    RELAY_UDP_SOCKET_SEND_BUFFER_BYTES
        .with_label_values(&["effective"])
        .set(sizes.effective_send as f64);
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
/// * `shards` - Session arbiters each accepted connection is placed on (#2727)
pub async fn start(
    opt: WebTransportOpt,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
    shards: Arc<SessionShards>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("WebTransportOpt: {opt:#?}");

    // Capture cert path for preflight diagnostics before `opt.certs` is moved
    // into `get_key_and_cert_chain`. Used only to make error messages
    // copy-pasteable; not load-bearing for the parse itself.
    let cert_path_for_diagnostics = opt.certs.cert.display().to_string();

    let (key, certs) = get_key_and_cert_chain(opt.certs)?;

    // Optional dev-only preflight: validates the leaf cert is shaped how
    // Chromium's `serverCertificateHashes` API requires (ECDSA P-256,
    // <=14d validity, SAN includes 127.0.0.1 + localhost). Gated behind
    // WT_DEV_CERT_PREFLIGHT so production cert-manager-issued certs
    // (RSA, multi-week validity) are not affected.
    if cert_preflight::is_enabled() {
        info!(
            "{} is set; running WebTransport dev cert preflight on {}",
            cert_preflight::PREFLIGHT_ENV_VAR,
            cert_path_for_diagnostics,
        );
        if let Err(reason) = cert_preflight::validate_chain(&certs, &cert_path_for_diagnostics) {
            cert_preflight::print_failure(&cert_path_for_diagnostics, &reason);
            return Err(anyhow!(
                "WT_DEV_CERT_PREFLIGHT rejected {}: {}",
                cert_path_for_diagnostics,
                reason
            )
            .into());
        }
        info!("WebTransport dev cert preflight passed");
    }

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

    server_config.transport_config(std::sync::Arc::new(build_transport_config()?));

    // #2716: `Endpoint::server` would bind on the kernel default instead.
    let (socket, udp_buffers) = bind_udp_socket(
        opt.listen,
        *QUIC_UDP_RECV_BUFFER_BYTES,
        *QUIC_UDP_SEND_BUFFER_BYTES,
    )?;
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        Arc::new(quinn::TokioRuntime),
    )?;

    let mut server = web_transport_quinn::Server::new(endpoint);

    info!(
        "listening on {} with {}s idle timeout, {}s keep-alive, {} congestion \
         control, {}B datagram send buffer, {}B send window, UDP buffers \
         rcv {}->{} snd {}->{} (requested->effective)",
        opt.listen,
        *QUIC_MAX_IDLE_TIMEOUT_SECS,
        *QUIC_KEEP_ALIVE_INTERVAL_SECS,
        QUIC_CONGESTION_CONTROLLER.label(),
        *QUIC_DATAGRAM_SEND_BUFFER_BYTES,
        *QUIC_SEND_WINDOW_BYTES,
        udp_buffers.requested_recv,
        udp_buffers.effective_recv,
        udp_buffers.requested_send,
        udp_buffers.effective_send,
    );

    // #2727: the connection is placed on a session arbiter and everything it owns
    // stays there for the session's life; `ShardLease` releases the slot.
    while let Some(request) = server.accept().await {
        trace_span!("New connection being attempted");
        let chat_server = chat_server.clone();
        let nats_client = nats_client.clone();
        let tracker_sender = tracker_sender.clone();
        let session_manager = session_manager.clone();
        let placed = shards.spawn_session(move |lease| async move {
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
            drop(lease);
        });
        if !placed {
            error!("Session arbiter is gone; dropped an inbound WebTransport connection");
        }
    }

    Ok(())
}

/// Identity and room resolved for an inbound WebTransport connection.
#[derive(Debug)]
struct WtConnectIdentity {
    username: String,
    lobby_id: String,
    observer: bool,
    display_name: String,
    is_guest: bool,
    is_host: bool,
}

/// Resolve who is connecting, from the JWT when one is presented and otherwise
/// from the deprecated `/lobby/{user_id}/{room}` path parameters.
///
/// The deprecated arm is reachable ONLY while `FEATURE_MEETING_MANAGEMENT` is
/// off; with the flag on, a tokenless connect is refused (issue #2298). This
/// mirrors the 410 the WebSocket `ws_connect` handler returns.
fn resolve_wt_connect_identity(
    parts: &[&str],
    token: Option<&str>,
) -> anyhow::Result<WtConnectIdentity> {
    if let Some(tok) = token {
        let jwt_secret = std::env::var("JWT_SECRET").unwrap_or_default();
        if jwt_secret.is_empty() {
            return Err(anyhow!("JWT_SECRET not set"));
        }
        let claims = token_validator::decode_room_token(&jwt_secret, tok).map_err(|e| {
            e.log("WT");
            anyhow!("token validation failed: {}", e.client_message())
        })?;
        info!(
            "WT token-based connection: user_id={}, room={}, display_name={}, is_guest={}, observer={}, is_host={}",
            claims.sub, claims.room, claims.display_name, claims.is_guest, claims.observer, claims.is_host
        );
        return Ok(WtConnectIdentity {
            username: claims.sub,
            lobby_id: claims.room,
            observer: claims.observer,
            display_name: claims.display_name,
            is_guest: claims.is_guest,
            is_host: claims.is_host,
        });
    }

    if videocall_types::FeatureFlags::meeting_management_enabled() {
        info!("WT connection rejected: no token provided and meeting management is enabled");
        return Err(anyhow!(
            "room access token is required. Use /lobby?token=<JWT>"
        ));
    }

    if parts.len() != 3 {
        return Err(anyhow!(
            "Invalid path: expected /lobby/{{user_id}}/{{room}} (deprecated) or /lobby?token=<JWT>"
        ));
    }
    let (username, lobby_id) = crate::lobby::deprecated_path_identity(parts[1], parts[2])
        .ok_or_else(|| anyhow!("Invalid path input chars"))?;
    info!(
        "WT deprecated path-based connection: user_id={}, room={}",
        username, lobby_id
    );
    let display_name = username.clone();
    Ok(WtConnectIdentity {
        username,
        lobby_id,
        observer: false,
        display_name,
        is_guest: false,
        is_host: false,
    })
}

/// Query-parameter name a client uses to advertise the #2723 per-publisher
/// downlink protocol it speaks. See `2723-contract.md` §1.
const DOWNLINK_STREAMS_QUERY_PARAM: &str = "ds";

/// Resolve the downlink topology from the connect URL's `ds` parameter (#2723).
///
/// The value is a PROTOCOL VERSION, not a count: `>= 1` selects the highest
/// version this relay implements, and anything else keeps
/// [`DownlinkStreamMode::Single`]. Read from the URL, the EARLIEST point a
/// capability can be observed.
fn resolve_downlink_stream_mode(ds: Option<&str>) -> DownlinkStreamMode {
    match ds.and_then(|raw| raw.parse::<u32>().ok()) {
        Some(version) if version >= 1 => DownlinkStreamMode::PerPublisherV1,
        _ => DownlinkStreamMode::Single,
    }
}

/// Which primitive this receiver's audio takes (#2724). A `Single`-mode
/// receiver keeps datagrams: its one reliable stream carries video, and
/// sharing it would be the cross-media head-of-line blocking #622 rejected.
/// `configured` can only move a `ds=1` receiver BACK to datagrams.
fn resolve_session_audio_lane(
    mode: DownlinkStreamMode,
    configured: AudioDownlinkLane,
) -> AudioDownlinkLane {
    match mode {
        DownlinkStreamMode::PerPublisherV1 => configured,
        DownlinkStreamMode::Single => AudioDownlinkLane::Datagram,
    }
}

/// Resolve one session's audio lane and book it, once, at accept (#2724).
/// `mode` separates a legacy client from a `ds=1` one on a reverted cluster,
/// which both book `lane="datagram"` (#2763).
fn book_audio_downlink_session(
    mode: DownlinkStreamMode,
    configured: AudioDownlinkLane,
) -> AudioDownlinkLane {
    let lane = resolve_session_audio_lane(mode, configured);
    RELAY_WT_AUDIO_DOWNLINK_SESSIONS_TOTAL
        .with_label_values(&[audio_lane_label(lane), downlink_mode_label(mode)])
        .inc();
    lane
}

/// `mode` label for `videocall_relay_wt_audio_downlink_sessions_total`.
fn downlink_mode_label(mode: DownlinkStreamMode) -> &'static str {
    match mode {
        DownlinkStreamMode::Single => "single",
        DownlinkStreamMode::PerPublisherV1 => "per_publisher_v1",
    }
}

/// Label for `videocall_relay_wt_audio_downlink_sessions_total`.
fn audio_lane_label(lane: AudioDownlinkLane) -> &'static str {
    match lane {
        AudioDownlinkLane::Reliable => "reliable",
        AudioDownlinkLane::Datagram => "datagram",
    }
}

/// Format the "received WebTransport request" line with the URL's query string
/// stripped — it carries `?token=<JWT>` since issue #2298.
fn wt_request_log_line(url: &str) -> String {
    format!(
        "received WebTransport request: {}",
        videocall_types::url_log::strip_query_for_log(url)
    )
}

async fn run_webtransport_connection_from_request(
    request: web_transport_quinn::Request,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
) -> anyhow::Result<()> {
    warn!("{}", wt_request_log_line(request.url.as_str()));
    let uri = &request.url;
    let path = urlencoding::decode(uri.path()).unwrap().into_owned();

    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    info!("Parts {:?}", parts);

    // First part must be "lobby"
    if parts.is_empty() || parts[0] != "lobby" {
        return Err(anyhow!("Invalid path: must start with /lobby"));
    }

    // Extract ?token= and ?instance_id= from query string
    let token = uri
        .query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, val)| val.into_owned());

    let instance_id: Option<String> = uri
        .query_pairs()
        .find(|(key, _)| key == "instance_id")
        .map(|(_, val)| val.into_owned());

    // #2723: read BEFORE the session is accepted. Unauthenticated by design — a
    // client that lies only breaks its own downlink, and the relay's cap holds.
    let downlink_streams: Option<String> = uri
        .query_pairs()
        .find(|(key, _)| key == DOWNLINK_STREAMS_QUERY_PARAM)
        .map(|(_, val)| val.into_owned());
    let downlink_mode = resolve_downlink_stream_mode(downlink_streams.as_deref());

    let identity = resolve_wt_connect_identity(&parts, token.as_deref())?;
    let WtConnectIdentity {
        username,
        lobby_id,
        observer,
        display_name,
        is_guest,
        is_host,
    } = identity;

    // `instance_id` binds reconnect adoption to identity; this path has no verified one.
    let instance_id = if token.is_some() { instance_id } else { None };

    // Accept the session.
    let session = request.ok().await.context("failed to accept session")?;
    debug!("accepted session");

    // Run the session with actor
    if let Err(err) = handle_webtransport_session(
        session,
        &username,
        &lobby_id,
        &display_name,
        is_guest,
        chat_server,
        nats_client,
        tracker_sender,
        session_manager,
        observer,
        instance_id,
        is_host,
        downlink_mode,
    )
    .await
    {
        info!("closing session: {}", err);
    }
    Ok(())
}

/// The cadence at which `handle_webtransport_session` drives the per-connection
/// path-stat sampler (#1637).
///
/// In production this is always [`WT_HEARTBEAT_INTERVAL`] (5s) — production
/// behavior is unchanged. Under `#[cfg(test)]` it is overridable via
/// [`set_path_stat_sample_interval_for_test`] so the sampler tests can drive a
/// fast cadence (a 5s tick would never fire inside a test window) and observe a
/// real tick emitted through [`spawn_connection_path_sampler`] (the wiring
/// `handle_webtransport_session` invokes). The override is a test-only seam; the
/// production `#[cfg(not(test))]` arm has no atomic and cannot be changed at
/// runtime. Because [`spawn_connection_path_sampler`] resolves the cadence by
/// CALLING this function, the override flows through the production spawn path.
#[cfg(not(test))]
fn path_stat_sample_interval() -> std::time::Duration {
    WT_HEARTBEAT_INTERVAL
}

#[cfg(test)]
static PATH_STAT_SAMPLE_INTERVAL_MS: AtomicU64 =
    AtomicU64::new(WT_HEARTBEAT_INTERVAL.as_millis() as u64);

#[cfg(test)]
fn path_stat_sample_interval() -> std::time::Duration {
    std::time::Duration::from_millis(PATH_STAT_SAMPLE_INTERVAL_MS.load(Ordering::Relaxed))
}

/// Test-only: override the path-stat sampler cadence (#1637). See
/// [`path_stat_sample_interval`].
#[cfg(test)]
fn set_path_stat_sample_interval_for_test(d: std::time::Duration) {
    PATH_STAT_SAMPLE_INTERVAL_MS.store(d.as_millis() as u64, Ordering::Relaxed);
}

/// Sample relay-measured QUIC path health for one live WT connection (#1637).
///
/// Runs as its own task for the lifetime of a WebTransport session, sampling
/// ~every `sample_interval` (production passes [`path_stat_sample_interval`] =
/// [`WT_HEARTBEAT_INTERVAL`], 5s; the param exists so the integration test can
/// drive a faster cadence and observe a tick within its window). On each tick it
/// reads ONE quinn `stats()` snapshot and hands the sample to
/// [`publish_connection_path_stats`], which sets every per-`session_id` gauge
/// — the LEAD signal set for epic #1636's B-vs-C discrimination (see the metric
/// docs in `metrics.rs` for the full reading: rtt/loss/congestion freeze under a
/// downlink collapse, `sent_packets` keeps climbing, so the two together separate
/// a network collapse from a relay thread stall).
///
/// `datagram_send_calls` is loaded BEFORE the `stats()` snapshot so it cannot run
/// ahead of `frame_tx.datagram` and fake a silent drop (#2712).
///
/// `conn` is a CLONE of the session's `quinn::Connection` (Arc-backed, cheap to
/// clone), taken BEFORE the owning `Session` is moved into the bridge. A
/// `quinn::Connection` clone is a COUNTED handle: quinn auto-closes a connection
/// only when its handle ref-count reaches 0 (the driver task is explicitly
/// excluded from that count — quinn 0.11.9 connection.rs:927-941), so holding this
/// clone DEFERS that implicit close. The connection lifetime and the gauge GC are
/// therefore bounded NOT by quinn auto-close but by [`stop_connection_path_sampler`]'s
/// UNCONDITIONAL `abort()` + `forget_connection_path_stats` at session teardown.
/// (We also break the loop on `close_reason()` so a real peer/idle-timeout close —
/// which the protocol state machine drives independently of ref-count — stops the
/// sampler promptly and avoids one stale final sample; that is a convenience, not
/// the lifetime bound.)
///
/// quinn exposes no "last-ACK age"; the rtt/loss/congestion freeze contrasted
/// against `sent_packets` still climbing carries that intent (see the metric-doc
/// note in `metrics.rs`). All counters are sampled SERVER-SIDE, so this reflects
/// the relay's authoritative view of the downlink even when a client is wedged and
/// stops self-reporting.
async fn sample_connection_path_stats(
    conn: quinn::Connection,
    room: String,
    session_id: String,
    sample_interval: std::time::Duration,
    datagram_send_calls: Arc<AtomicU64>,
) {
    let mut interval = tokio::time::interval(sample_interval);
    // Skip the immediate first tick: on a brand-new connection RTT/stats are not
    // yet meaningful (rtt() would return the unsampled initial_rtt floor), and the
    // first heartbeat-aligned sample is what we want.
    interval.tick().await;
    loop {
        interval.tick().await;

        // If quinn has closed the connection, stop sampling. The teardown path
        // also aborts us + removes the series; this just avoids one stale sample
        // and an idle task between close and abort. This `close_reason()` check is
        // a separate cheap lock acquisition; the gauge reads below share a SINGLE
        // `stats()` snapshot plus one `datagram_send_buffer_space()`.
        if conn.close_reason().is_some() {
            debug!(
                "WT path-stat sampler stopping (connection closed) for session {} in {}",
                session_id, room
            );
            break;
        }

        // Read every path/frame value from ONE `stats()` snapshot so they reflect a single
        // consistent connection state. `ConnectionStats.path.rtt` is the SAME value
        // as `Connection::rtt()` — both return `self.path.rtt.get()`
        // (quinn-proto-0.11.13 connection/mod.rs:1269 vs :1381), the current
        // smoothed round-trip estimate. `lost_packets` / `congestion_events` /
        // `sent_packets` are CUMULATIVE for the connection; the gauges publish the
        // running totals (chart with rate()/increase()). The field→gauge mapping
        // lives in `publish_connection_path_stats` so a host unit test can pin it.
        let calls = datagram_send_calls.load(Ordering::Relaxed);
        let stats = conn.stats();
        let sample = ConnectionPathSample {
            rtt_ms: duration_to_millis_f64(stats.path.rtt),
            lost_packets: stats.path.lost_packets,
            congestion_events: stats.path.congestion_events,
            sent_packets: stats.path.sent_packets,
            cwnd: stats.path.cwnd,
            current_mtu: stats.path.current_mtu,
            black_holes_detected: stats.path.black_holes_detected,
            max_data: stats.frame_rx.max_data,
            max_stream_data: stats.frame_rx.max_stream_data,
            datagram_frames_sent: stats.frame_tx.datagram,
            datagram_send_calls: calls,
            datagram_send_buffer_space: conn.datagram_send_buffer_space() as u64,
        };
        publish_connection_path_stats(&room, &session_id, &sample);
    }
}

/// Spawn the per-connection path-health sampler for one WT connection and return
/// its task handle (#1637).
///
/// The SPAWN half of the sampler lifecycle, extracted so a host test can drive
/// the real wiring without NATS.
///
/// `actix_rt::spawn` keeps the sampler on the arbiter that owns this session
/// (#2727). The cadence is resolved INTERNALLY via [`path_stat_sample_interval`]
/// so the `#[cfg(test)]` override flows through this production call.
pub(crate) fn spawn_connection_path_sampler(
    conn: quinn::Connection,
    room: &str,
    session_id: &str,
    datagram_send_calls: Arc<AtomicU64>,
) -> actix_rt::task::JoinHandle<()> {
    actix_rt::spawn(sample_connection_path_stats(
        conn,
        room.to_string(),
        session_id.to_string(),
        path_stat_sample_interval(),
        datagram_send_calls,
    ))
}

/// Stop the per-connection path-health sampler and remove its per-session gauges
/// (#1637) — the TEARDOWN half of the sampler lifecycle.
///
/// The sampler also self-exits on a closed connection, but this `abort()`s
/// UNCONDITIONALLY so the task cannot outlive the session, then GCs the
/// `session_id`-labeled series. The GC lives here rather than in
/// `SessionLogic::on_stopping` because the sampler is WT-only.
pub(crate) fn stop_connection_path_sampler(
    sampler_handle: actix_rt::task::JoinHandle<()>,
    room: &str,
    session_id: &str,
) {
    sampler_handle.abort();
    forget_connection_path_stats(room, session_id);
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
    is_guest: bool,
    chat_server: Addr<ChatServer>,
    nats_client: async_nats::client::Client,
    tracker_sender: TrackerSender,
    session_manager: SessionManager,
    observer: bool,
    instance_id: Option<String>,
    is_host: bool,
    downlink_mode: DownlinkStreamMode,
) -> anyhow::Result<()> {
    // Create two channels for actor → WebTransport I/O — one per QUIC
    // primitive. Phase 2 split (discussion #756): a stalled persistent
    // uni-stream cannot back up the datagram path because the two
    // channels are drained by independent writer tasks.
    let (unistream_tx, unistream_rx) =
        mpsc::channel::<WtOutboundFrame>(crate::constants::wt_outbound_channel_capacity());
    let (datagram_tx, datagram_rx) =
        mpsc::channel::<WtOutboundFrame>(crate::constants::WT_DATAGRAM_CHANNEL_CAPACITY);

    // #2717: credited on enqueue, debited by the writer's drain.
    let unistream_bytes = Arc::new(SharedQueueByteMeter::default());

    // #2726: owned by this session and dropped with it.
    let escalation = DownlinkShedEscalation::new();

    // #2724: booked once per session, because nothing else tells an operator
    // whether the deploy or a revert took effect on this cluster.
    let audio_lane = book_audio_downlink_session(downlink_mode, wt_audio_downlink_lane());

    // #2716: the datagram writer drops this clone when its loop ends, so the
    // unistream writer still sees the channel close at teardown.
    let unistream_fallback_tx = unistream_tx.clone();

    // Start the WtChatSession actor
    let actor = WtChatSession::new(
        chat_server,
        lobby_id.to_string(),
        username.to_string(),
        display_name.to_string(),
        is_guest,
        unistream_tx,
        datagram_tx,
        unistream_bytes.clone(),
        nats_client,
        tracker_sender,
        session_manager,
        observer,
        instance_id,
        is_host,
        audio_lane,
        escalation.clone(),
    );
    // Capture the canonical per-session id BEFORE `start()` consumes the actor.
    // It is the SAME id `relay_session_drops_total` uses, so the #1637 relay RTT
    // gauge joins with the per-session drop series for this connection.
    let metrics_session_id = actor.session_id().to_string();
    let metrics_room = lobby_id.to_string();
    // #2718: same capture-before-`start()` rule as the session id above.
    let downlink_drops = actor.downlink_drop_sink();

    let actor_addr = actor.start();

    // #1637: start the relay-side QUIC path-health sampler for this connection.
    // Clone the `quinn::Connection` (Arc-backed) BEFORE `session` is moved into
    // the bridge below — `web_transport_quinn::Session: Deref<Target =
    // quinn::Connection>`, so `(*session).clone()` is the underlying connection
    // handle. The spawn + teardown live in `spawn_connection_path_sampler` /
    // `stop_connection_path_sampler` so the WIRING is exercised by a host test
    // (a loopback-quinn test drives the same helpers without NATS); see those fns.
    // #2712: the datagram writer and the sampler share one call counter.
    let datagram_send_calls = Arc::new(AtomicU64::new(0));
    let sampler_handle = spawn_connection_path_sampler(
        (*session).clone(),
        &metrics_room,
        &metrics_session_id,
        datagram_send_calls.clone(),
    );

    // Create bridge (with test callback if in test mode)
    #[cfg(test)]
    let on_packet_sent = {
        let username_for_callback = username.to_string();
        Some(
            Box::new(move || increment_test_packet_counter_for_user(&username_for_callback))
                as bridge::PacketSentCallback,
        )
    };
    #[cfg(not(test))]
    let on_packet_sent: Option<bridge::PacketSentCallback> = None;

    let mut bridge = WebTransportBridge::new_with_callback(
        session,
        actor_addr.clone(),
        unistream_rx,
        datagram_rx,
        unistream_fallback_tx,
        unistream_bytes,
        on_packet_sent,
        datagram_send_calls,
        downlink_drops,
        downlink_mode,
        escalation,
    );
    bridge.wait_for_disconnect().await;
    bridge.shutdown().await;

    // #1637: stop the path-health sampler and remove its per-session gauges.
    stop_connection_path_sampler(sampler_handle, &metrics_room, &metrics_session_id);

    // Signal actor to stop
    actor_addr.do_send(crate::actors::transports::wt_chat_session::StopSession);

    warn!("Finished handling WebTransport session for {username} in {lobby_id}");
    Ok(())
}

// Note: handle_send_connection_started and handle_send_connection_ended are now
// handled by the WtChatSession actor internally via ChatServer coordination.

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message as ProtobufMessage;
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
        use crate::server_diagnostics::ServerDiagnostics;
        use crate::session_manager::SessionManager;
        use crate::webtransport::{self, Certs};
        use actix::Actor;
        use std::net::ToSocketAddrs;

        // Connect to NATS
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        // Start ChatServer actor
        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        // Create SessionManager
        let session_manager = SessionManager::new();

        // Create connection tracker
        let (_, tracker_sender, tracker_receiver) =
            ServerDiagnostics::new_with_channel(nats_client.clone());

        // Clone nats_client for use in both spawned tasks
        let nats_client_for_tracker = nats_client.clone();

        // Start tracker message loop
        actix_rt::spawn(async move {
            let connection_tracker =
                std::sync::Arc::new(ServerDiagnostics::new(nats_client_for_tracker));
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

        // One shard: this harness needs no arbiter threads.
        let shards = std::sync::Arc::new(crate::relay_shards::SessionShards::new(1));
        actix_rt::spawn(async move {
            if let Err(e) = webtransport::start(
                opt,
                chat_server,
                nats_client,
                tracker_sender,
                session_manager,
                shards,
            )
            .await
            {
                eprintln!("WebTransport server error: {e}");
            }
        })
    }

    #[allow(dead_code)]
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

        let client = web_transport_quinn::ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()?;

        Ok(client.connect(url).await?)
    }

    /// Connect via the token-based endpoint: GET /lobby?token=<JWT>
    async fn connect_client_with_token(
        token: &str,
    ) -> Result<web_transport_quinn::Session, Box<dyn std::error::Error>> {
        let base = std::env::var("WEBTRANSPORT_URL")
            .unwrap_or_else(|_| "https://127.0.0.1:4433".to_string());
        let url_str = format!(
            "{}/lobby?token={}",
            base.trim_end_matches('/'),
            urlencoding::encode(token)
        );
        let url = url::Url::parse(&url_str)?;

        let client = web_transport_quinn::ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()?;
        Ok(client.connect(url).await?)
    }

    async fn send_packet(session: &web_transport_quinn::Session, bytes: Vec<u8>) {
        // Phase 2 (discussion #756): the server's UniStream reader now consumes
        // length-prefix-framed packets (`[u32 BE length][payload]`). One frame
        // per stream is still a valid shape — the reader loop reads one frame,
        // then `read_exact` for the next 4-byte header returns EOF when the
        // client finishes the stream, and the per-stream task exits cleanly.
        let mut s = session.open_uni().await.expect("open uni");
        let len: u32 = bytes
            .len()
            .try_into()
            .expect("test packet exceeds u32::MAX bytes");
        s.write_all(&len.to_be_bytes())
            .await
            .expect("write length header");
        s.write_all(&bytes).await.expect("write packet payload");
        // Finish the stream so the server reader cleanly sees EOF at a frame
        // boundary after this single packet. Without finish() the reader's
        // outer `accept_uni` loop would still receive subsequent streams, but
        // the per-stream task would park indefinitely on `read_exact` for the
        // next header — wasting a tokio task per test packet.
        let _ = s.finish();
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
    ///
    /// Returns the persistent `RecvStream` (if one was accepted) so the caller can
    /// reuse it for subsequent reads. The bridge writer uses a single persistent
    /// unidirectional stream for all `UniStream` packets, so `accept_uni()` will
    /// only succeed once per session -- subsequent calls would block forever.
    async fn wait_for_session_ready(
        session: &web_transport_quinn::Session,
        name: &str,
    ) -> Result<Option<web_transport_quinn::RecvStream>, &'static str> {
        println!("Waiting for {name} to receive MEETING_STARTED...");

        let mut persistent_stream: Option<web_transport_quinn::RecvStream> = None;
        let mut accumulated = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let meeting_type: ::protobuf::EnumOrUnknown<VcPacketType> = VcPacketType::MEETING.into();

        while std::time::Instant::now() < deadline {
            let (data, stream) =
                read_more_data(session, persistent_stream, Duration::from_millis(200)).await;
            persistent_stream = stream;

            if data.is_empty() {
                continue;
            }

            // Data from a datagram (no persistent stream involved): try to parse
            // directly since datagrams are self-contained.
            if persistent_stream.is_none() && accumulated.is_empty() {
                if let Ok(wrapper) = VcPacketWrapper::parse_from_bytes(&data) {
                    if wrapper.packet_type == meeting_type {
                        println!("✓ {name} received MEETING_STARTED via datagram - session ready");
                        return Ok(persistent_stream);
                    }
                }
                // Not MEETING_STARTED datagram -- keep waiting
                continue;
            }

            // Data from the persistent stream: accumulate and try to parse.
            // Multiple packets (SESSION_ASSIGNED, MEETING_STARTED) may be
            // concatenated on the same stream. Protobuf's last-wins semantics
            // for scalar fields means that parsing the entire accumulated
            // buffer will yield packet_type = MEETING once MEETING_STARTED
            // has been received (overriding any earlier SESSION_ASSIGNED).
            accumulated.extend_from_slice(&data);

            if let Ok(wrapper) = VcPacketWrapper::parse_from_bytes(&accumulated) {
                if wrapper.packet_type == meeting_type {
                    println!(
                        "✓ {name} received MEETING_STARTED via persistent stream - session ready"
                    );
                    return Ok(persistent_stream);
                }
            }
        }

        Err("Timed out waiting for MEETING_STARTED")
    }

    #[actix_rt::test]
    async fn test_relay_packet_webtransport_between_two_clients() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(std::io::stderr)
            .try_init();

        // FF=off: this test verifies packet relay without JWT.
        // JWT validation is tested separately in jwt_integration_tests.rs.
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        // Wrap entire test with 15 second timeout
        let test_result = tokio::time::timeout(Duration::from_secs(15), async {
            test_relay_packet_impl().await
        })
        .await;

        videocall_types::FeatureFlags::clear_meeting_management_override();

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
        // This confirms NATS subscriptions are established.
        // Capture the persistent RecvStream from session_b so we can reuse it
        // when reading relayed packets later (the bridge writer uses a single
        // persistent uni stream, so accept_uni() only succeeds once).
        let _stream_a = wait_for_session_ready(&session_a, "client A")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let persistent_stream_b = wait_for_session_ready(&session_b, "client B")
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

        // Craft a MEDIA packet that is not RTT and not health.
        // Using VIDEO (not AUDIO) so the packet routes via the persistent
        // UniStream — which is what this test reads from. Phase 4 routes
        // small AUDIO MediaPackets via QUIC datagrams instead, so an AUDIO
        // packet would arrive on a different transport and miss the unistream
        // assertion below.
        let media = VcMediaPacket {
            media_type: VcMediaType::VIDEO.into(),
            user_id: user_a.as_bytes().to_vec(),
            ..Default::default()
        };
        let packet = VcPacketWrapper {
            packet_type: VcPacketType::MEDIA.into(),
            user_id: user_a.as_bytes().to_vec(),
            data: media.write_to_bytes().expect("serialize media"),
            ..Default::default()
        };
        let bytes = packet.write_to_bytes().expect("serialize wrapper");

        println!("Sending packet from A to B (size: {} bytes)", bytes.len());
        send_packet(&session_a, bytes.clone()).await;
        println!("Packet sent from A!");

        // Wait for B to receive the packet.
        //
        // The server uses a persistent unidirectional stream for all
        // UniStream packets (MEDIA, large control), so we read incrementally
        // with `read()` instead of `read_to_end()` (which would block until
        // the stream is finished — i.e., until the session ends).
        // Control packets like PARTICIPANT_JOINED may arrive as datagrams.
        println!("Waiting for packet on B...");
        let mut received: Option<Vec<u8>> = None;
        let mut persistent_stream: Option<web_transport_quinn::RecvStream> = persistent_stream_b;
        let mut accumulated = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);

        while std::time::Instant::now() < deadline && received.is_none() {
            let (data, stream) =
                read_more_data(&session_b, persistent_stream, Duration::from_millis(200)).await;
            persistent_stream = stream;

            if !data.is_empty() {
                // Check if this is a datagram (non-persistent) containing a parseable packet
                if persistent_stream.is_none() && accumulated.is_empty() {
                    // Data likely came from a datagram
                    if let Ok(pkt) = VcPacketWrapper::parse_from_bytes(&data) {
                        let media_type: ::protobuf::EnumOrUnknown<VcPacketType> =
                            VcPacketType::MEDIA.into();
                        if pkt.packet_type == media_type {
                            println!(
                                "Received MEDIA packet on B via datagram (size: {} bytes)",
                                data.len()
                            );
                            received = Some(data);
                            continue;
                        }
                        println!(
                            "Skipping non-MEDIA datagram on B (type: {:?})",
                            pkt.packet_type
                        );
                        continue;
                    }
                }

                // Data from the persistent stream: accumulate and try to parse
                accumulated.extend_from_slice(&data);

                // Try to parse a PacketWrapper from the accumulated data.
                // Since packets are concatenated without framing on the persistent
                // stream, we attempt to parse from the beginning of the buffer.
                if let Ok(pkt) = VcPacketWrapper::parse_from_bytes(&accumulated) {
                    let media_type: ::protobuf::EnumOrUnknown<VcPacketType> =
                        VcPacketType::MEDIA.into();
                    if pkt.packet_type == media_type {
                        println!(
                            "Received MEDIA packet on B via persistent stream (size: {} bytes)",
                            accumulated.len()
                        );
                        received = Some(accumulated.clone());
                    } else {
                        println!(
                            "Skipping non-MEDIA packet on B (type: {:?})",
                            pkt.packet_type
                        );
                        accumulated.clear();
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let received = received.expect("Should receive packet from A on B");

        println!("Packet successfully relayed!");

        // Parse both packets to compare content (server may add session_id)
        let sent_packet = VcPacketWrapper::parse_from_bytes(&bytes).expect("parse sent packet");
        let received_packet =
            VcPacketWrapper::parse_from_bytes(&received).expect("parse received packet");

        // Compare meaningful fields (server adds session_id, so we ignore it)
        assert_eq!(
            sent_packet.packet_type, received_packet.packet_type,
            "packet_type must match"
        );
        assert_eq!(
            sent_packet.user_id, received_packet.user_id,
            "user_id must match"
        );
        assert_eq!(sent_packet.data, received_packet.data, "data must match");
        // Verify that server added session_id (it should not be empty)
        assert!(
            received_packet.session_id != 0,
            "server should add session_id"
        );

        println!("=== INTEGRATION TEST PASSED ===");
        Ok(())
    }

    /// Tests cross-lobby isolation and self-echo prevention.
    ///
    /// This test verifies multi-packet delivery counts using server-side
    /// `TEST_PACKET_COUNTERS`. These counters increment when the bridge
    /// writer successfully writes a packet to the persistent unidirectional
    /// stream (or datagram). Ordering of packets within a lobby is guaranteed
    /// by the persistent stream (QUIC per-stream ordering); see
    /// `test_persistent_stream_packet_ordering` for explicit client-side
    /// ordering verification.
    #[actix_rt::test]
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

        // Give NATS subscriptions time to fully propagate after all sessions
        // are ready. MEETING_STARTED is only published for the first participant
        // now (the transport actors send it directly to every joiner), so we use
        // a short delay instead of waiting for a cross-session broadcast.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // ========== ACTIVATE ALL CONNECTIONS ==========
        // ActivateConnection (and its PARTICIPANT_JOINED broadcast) is deferred
        // until the first non-RTT data packet. Send an activation packet from
        // each client so these broadcasts happen BEFORE the counting phase.
        println!("\n--- Activating connections ---");
        let activation_a =
            create_test_packet(user_a, VcMediaType::AUDIO, "activation-a".to_string());
        let activation_b =
            create_test_packet(user_b, VcMediaType::AUDIO, "activation-b".to_string());
        let activation_c =
            create_test_packet(user_c, VcMediaType::AUDIO, "activation-c".to_string());
        send_packet(&session_a, activation_a).await;
        send_packet(&session_b, activation_b).await;
        send_packet(&session_c, activation_c).await;
        println!("✓ Sent activation packets from all clients");

        // Wait for PARTICIPANT_JOINED broadcasts to settle.
        // Alice and Charlie are in the same room, so each receives the other's
        // PARTICIPANT_JOINED. Allow time for the async broadcast pipeline
        // (ActivateConnection -> NATS publish -> subscription delivery).
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Reset counters so the counting phase starts from zero.
        reset_test_packet_counters();
        println!("✓ Reset packet counters after activation");

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
            user_id: sender.as_bytes().to_vec(),
            ..Default::default()
        };
        let packet = VcPacketWrapper {
            packet_type: VcPacketType::MEDIA.into(),
            user_id: sender.as_bytes().to_vec(),
            data: media.write_to_bytes().expect("serialize media"),
            ..Default::default()
        };
        packet.write_to_bytes().expect("serialize wrapper")
    }

    /// Create a MEDIA packet with a sequence marker embedded in `MediaPacket.data`.
    /// The marker is `SEQ:<seq_num>:END` so it can be located in a concatenated byte
    /// stream without needing protobuf framing.
    fn create_sequenced_test_packet(sender: &str, seq_num: u32) -> Vec<u8> {
        let marker = format!("SEQ:{seq_num:04}:END");
        // Use VIDEO (not AUDIO) so the packet routes via the persistent
        // UniStream — Phase 4 sends small AUDIO MediaPackets via QUIC
        // datagrams which would bypass the ordered-stream assertions.
        let media = VcMediaPacket {
            media_type: VcMediaType::VIDEO.into(),
            user_id: sender.as_bytes().to_vec(),
            data: marker.into_bytes(),
            ..Default::default()
        };
        let packet = VcPacketWrapper {
            packet_type: VcPacketType::MEDIA.into(),
            user_id: sender.as_bytes().to_vec(),
            data: media.write_to_bytes().expect("serialize media"),
            ..Default::default()
        };
        packet.write_to_bytes().expect("serialize wrapper")
    }

    /// Read a single length-prefixed frame from a persistent uni stream.
    ///
    /// Wire format: `[4-byte BE length][payload]`
    ///
    /// Returns the deframed payload bytes, or `None` if the stream is finished
    /// or encounters an error (in which case the stream should be dropped).
    async fn read_length_prefixed_frame(
        stream: &mut web_transport_quinn::RecvStream,
    ) -> Option<Vec<u8>> {
        // Read the 4-byte big-endian length header.
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).await.is_err() {
            return None;
        }
        let payload_len = u32::from_be_bytes(len_buf) as usize;
        if payload_len == 0 {
            return Some(Vec::new());
        }
        // Read exactly `payload_len` bytes.
        let mut payload = vec![0u8; payload_len];
        if stream.read_exact(&mut payload).await.is_err() {
            return None;
        }
        Some(payload)
    }

    /// Accept a new uni stream or datagram from the session and read one frame.
    ///
    /// For uni streams the server uses length-prefix framing, so we read one
    /// complete frame (4-byte header + payload). Datagrams are self-contained
    /// and returned as-is.
    async fn read_available_data(
        session: &web_transport_quinn::Session,
    ) -> Option<(Vec<u8>, Option<web_transport_quinn::RecvStream>)> {
        tokio::select! {
            Ok(mut stream) = session.accept_uni() => {
                read_length_prefixed_frame(&mut stream)
                    .await
                    .map(|payload| (payload, Some(stream)))
            }
            Ok(datagram) = session.read_datagram() => {
                Some((datagram.to_vec(), None))
            }
            else => None,
        }
    }

    /// Read one complete packet from an already-accepted persistent uni stream,
    /// or fall back to accepting a new stream / datagram.
    ///
    /// The persistent stream uses length-prefix framing (`[4-byte BE len][payload]`),
    /// so this reads one full frame and returns the deframed payload.
    ///
    /// Returns the data read and optionally the (possibly updated) persistent
    /// stream handle.
    async fn read_more_data(
        session: &web_transport_quinn::Session,
        persistent: Option<web_transport_quinn::RecvStream>,
        timeout: Duration,
    ) -> (Vec<u8>, Option<web_transport_quinn::RecvStream>) {
        if let Some(mut stream) = persistent {
            // Try reading one length-prefixed frame from the persistent stream.
            match tokio::time::timeout(timeout, read_length_prefixed_frame(&mut stream)).await {
                Ok(Some(payload)) => {
                    return (payload, Some(stream));
                }
                Ok(None) => {
                    // Stream finished or error, drop it
                    return (Vec::new(), None);
                }
                Err(_) => {
                    // Timeout - no data available yet, return the stream for next time
                    return (Vec::new(), Some(stream));
                }
            }
        }

        // No persistent stream yet, accept one or read a datagram
        match tokio::time::timeout(timeout, read_available_data(session)).await {
            Ok(Some((d, stream))) => (d, stream),
            _ => (Vec::new(), None),
        }
    }

    /// Extract sequence markers from a byte buffer. Searches for `SEQ:NNNN:END`
    /// patterns and returns the sequence numbers in the order they appear.
    fn extract_sequence_markers(data: &[u8]) -> Vec<u32> {
        let mut markers = Vec::new();
        let pattern_prefix = b"SEQ:";
        let pattern_suffix = b":END";

        let mut i = 0;
        while i + pattern_prefix.len() + 4 + pattern_suffix.len() <= data.len() {
            if &data[i..i + pattern_prefix.len()] == pattern_prefix {
                let num_start = i + pattern_prefix.len();
                let num_end = num_start + 4;
                if num_end + pattern_suffix.len() <= data.len()
                    && &data[num_end..num_end + pattern_suffix.len()] == pattern_suffix
                {
                    if let Ok(s) = std::str::from_utf8(&data[num_start..num_end]) {
                        if let Ok(n) = s.parse::<u32>() {
                            markers.push(n);
                            i = num_end + pattern_suffix.len();
                            continue;
                        }
                    }
                }
            }
            i += 1;
        }
        markers
    }

    fn get_all_user_counts(users: &[&str]) -> Vec<u64> {
        users
            .iter()
            .map(|user| get_test_packet_counter_for_user(user))
            .collect()
    }

    // ==========================================================================
    // Persistent Stream Ordering Test (WebTransport)
    //
    // Validates that multiple MEDIA packets sent through the bridge writer's
    // persistent unidirectional stream arrive at the client in the order they
    // were written. QUIC guarantees per-stream ordering, and the persistent
    // stream change (bridge.rs `spawn_writer`) relies on this to deliver
    // packets without reordering -- unlike the old per-packet-stream model
    // where independent streams had no ordering guarantee.
    // ==========================================================================

    #[actix_rt::test]
    async fn test_persistent_stream_packet_ordering() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(std::io::stderr)
            .try_init();

        // FF=off: test packet ordering without JWT.
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        let test_result = tokio::time::timeout(Duration::from_secs(20), async {
            test_persistent_stream_packet_ordering_impl().await
        })
        .await;

        videocall_types::FeatureFlags::clear_meeting_management_override();

        match test_result {
            Ok(Ok(())) => println!("Test completed successfully"),
            Ok(Err(e)) => panic!("Test failed: {e}"),
            Err(_) => panic!("Test timed out after 20 seconds"),
        }
    }

    async fn test_persistent_stream_packet_ordering_impl() -> anyhow::Result<()> {
        println!("=== STARTING PERSISTENT STREAM ORDERING TEST ===");

        reset_test_packet_counters();
        println!("Starting WebTransport server...");
        let _wt_handle = start_webtransport_server().await;
        wait_for_server_ready().await;
        println!("Server ready");

        let meeting = "ordering-meeting";
        let user_sender = "sender";
        let user_receiver = "receiver";
        let num_packets: u32 = 10;

        // Connect both clients
        println!("Connecting sender...");
        let session_sender = connect_client(user_sender, meeting)
            .await
            .expect("connect sender");
        println!("Connecting receiver...");
        let session_receiver = connect_client(user_receiver, meeting)
            .await
            .expect("connect receiver");

        // Wait for both sessions to be fully ready (MEETING_STARTED).
        // Capture the persistent RecvStream from the receiver so we can reuse
        // it when reading sequenced packets later.
        let _stream_sender = wait_for_session_ready(&session_sender, "sender")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let persistent_stream_receiver = wait_for_session_ready(&session_receiver, "receiver")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        // Keep connections alive
        let sender_keep = session_sender.clone();
        let receiver_keep = session_receiver.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                keep_alive(&sender_keep).await;
            }
        });
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                keep_alive(&receiver_keep).await;
            }
        });

        // Allow NATS subscriptions to propagate
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Send activation packets so ActivateConnection fires before counting
        let activation = create_test_packet(
            user_sender,
            VcMediaType::AUDIO,
            "activation-sender".to_string(),
        );
        send_packet(&session_sender, activation).await;
        let activation = create_test_packet(
            user_receiver,
            VcMediaType::AUDIO,
            "activation-receiver".to_string(),
        );
        send_packet(&session_receiver, activation).await;

        // Wait for activation broadcasts to settle
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Reset counters so we count only our sequenced packets
        reset_test_packet_counters();
        println!("Sending {num_packets} sequenced MEDIA packets from sender...");

        // Send numbered packets from sender
        for seq in 0..num_packets {
            let packet = create_sequenced_test_packet(user_sender, seq);
            send_packet(&session_sender, packet).await;
        }
        println!("All {num_packets} packets sent from sender");

        // Wait for server-side counters to confirm all packets were written
        // to the receiver's persistent stream
        let expected_count = u64::from(num_packets);
        wait_for_condition_bool(
            || async move { get_test_packet_counter_for_user(user_receiver) >= expected_count },
            Duration::from_secs(10),
            Duration::from_millis(50),
        )
        .await
        .expect("Server should write all packets to receiver");

        println!(
            "Server wrote {} packets to receiver (expected {})",
            get_test_packet_counter_for_user(user_receiver),
            num_packets
        );

        // Read from receiver's session: accumulate bytes from the persistent
        // uni stream and extract sequence markers. Reuse the stream handle
        // returned by wait_for_session_ready (the bridge writer opens only one).
        println!("Reading data from receiver session...");
        let mut accumulated = Vec::new();
        let mut persistent_stream: Option<web_transport_quinn::RecvStream> =
            persistent_stream_receiver;
        let deadline = std::time::Instant::now() + Duration::from_secs(8);

        while std::time::Instant::now() < deadline {
            let (data, stream) = read_more_data(
                &session_receiver,
                persistent_stream,
                Duration::from_millis(200),
            )
            .await;
            persistent_stream = stream;
            if !data.is_empty() {
                accumulated.extend_from_slice(&data);
            }

            // Check if we have all markers yet
            let markers = extract_sequence_markers(&accumulated);
            if markers.len() >= num_packets as usize {
                break;
            }
        }

        // Extract and verify ordering
        let markers = extract_sequence_markers(&accumulated);
        println!("Received {} sequence markers: {:?}", markers.len(), markers);

        assert!(
            markers.len() >= num_packets as usize,
            "Expected at least {num_packets} sequence markers, got {} (accumulated {} bytes)",
            markers.len(),
            accumulated.len()
        );

        // Verify strict ordering: each marker must be greater than the previous
        for window in markers.windows(2) {
            assert!(
                window[0] < window[1],
                "Ordering violation: sequence {} appeared before {} -- \
                 persistent stream should guarantee QUIC in-order delivery. \
                 Full sequence: {:?}",
                window[0],
                window[1],
                markers
            );
        }

        // Verify all expected sequence numbers are present
        let expected: Vec<u32> = (0..num_packets).collect();
        assert_eq!(
            markers, expected,
            "Expected sequences {:?} but got {:?}",
            expected, markers,
        );

        println!("=== PERSISTENT STREAM ORDERING TEST PASSED ===");
        println!(
            "Verified {num_packets} packets arrived in order via the persistent \
             uni stream. QUIC per-stream ordering guarantees make this possible."
        );
        Ok(())
    }

    // ==========================================================================
    // Meeting Lifecycle Integration Test (WebTransport)
    // Tests: meeting creation, participant join/leave, meeting end
    // ==========================================================================

    #[actix_rt::test]
    #[serial_test::serial]
    async fn test_meeting_lifecycle_webtransport() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(std::io::stderr)
            .try_init();

        // FF=off: this test verifies basic session lifecycle without JWT.
        // JWT validation is tested separately in jwt_integration_tests.rs.
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        let test_result = tokio::time::timeout(Duration::from_secs(30), async {
            test_meeting_lifecycle_impl().await
        })
        .await;

        videocall_types::FeatureFlags::clear_meeting_management_override();

        match test_result {
            Ok(Ok(())) => println!("Test completed successfully"),
            Ok(Err(e)) => panic!("Test failed: {e}"),
            Err(_) => panic!("Test timed out after 30 seconds"),
        }
    }

    async fn test_meeting_lifecycle_impl() -> anyhow::Result<()> {
        println!("=== STARTING SESSION LIFECYCLE TEST (WebTransport) ===");

        let room_id = "wt-meeting-lifecycle-test";

        println!("Starting WebTransport server...");
        let _wt_handle = start_webtransport_server().await;
        wait_for_server_ready().await;
        println!("✓ Server ready");

        // ========== STEP 1: First user connects ==========
        println!("\n--- Step 1: Alice connects (first participant) ---");

        let session_alice = connect_client("alice", room_id)
            .await
            .expect("connect alice");
        wait_for_session_ready(&session_alice, "Alice")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Alice connected");

        // ========== STEP 2: Second user connects ==========
        println!("\n--- Step 2: Bob connects (second participant) ---");

        let session_bob = connect_client("bob", room_id).await.expect("connect bob");
        wait_for_session_ready(&session_bob, "Bob")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Bob connected");

        // ========== STEP 3: Third user connects ==========
        println!("\n--- Step 3: Charlie connects (third participant) ---");

        let session_charlie = connect_client("charlie", room_id)
            .await
            .expect("connect charlie");
        wait_for_session_ready(&session_charlie, "Charlie")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("✓ Charlie connected");

        // ========== STEP 4: Charlie disconnects ==========
        println!("\n--- Step 4: Charlie disconnects ---");
        session_charlie.close(0u32, b"test disconnect");
        drop(session_charlie);
        tokio::time::sleep(Duration::from_millis(500)).await;
        println!("✓ Charlie disconnected");

        // ========== STEP 5: Bob disconnects ==========
        println!("\n--- Step 5: Bob disconnects ---");
        session_bob.close(0u32, b"test disconnect");
        drop(session_bob);
        tokio::time::sleep(Duration::from_millis(500)).await;
        println!("✓ Bob disconnected");

        // ========== STEP 6: Alice (last) disconnects ==========
        println!("\n--- Step 6: Alice disconnects - session ends ---");
        session_alice.close(0u32, b"test disconnect");
        drop(session_alice);
        tokio::time::sleep(Duration::from_millis(500)).await;
        println!("✓ Alice disconnected");

        println!("\n=== SESSION LIFECYCLE TEST PASSED (WebTransport) ===");
        Ok(())
    }

    /// Test helper: create a database pool for future JWT flow integration tests.
    #[allow(dead_code)]
    async fn get_test_pool() -> sqlx::PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this test");
        sqlx::PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    // =====================================================================
    // JWT token-based WebTransport tests
    // =====================================================================

    const JWT_SECRET: &str = "test-secret-for-integration-tests";
    const TOKEN_TTL_SECS: i64 = 600;

    /// Set up env and start the real WT server for JWT tests.
    async fn setup_jwt_wt() {
        std::env::set_var("JWT_SECRET", JWT_SECRET);
        videocall_types::FeatureFlags::clear_meeting_management_override();
        let _handle = start_webtransport_server().await;
        wait_for_server_ready().await;
    }

    #[actix_rt::test]
    async fn test_wt_valid_token_connects() {
        setup_jwt_wt().await;

        let token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            TOKEN_TTL_SECS,
            "alice@test.com",
            "wt-jwt-room-1",
            true,
            "Alice",
            false, // end_on_host_leave
            false, // is_guest
        )
        .expect("generate token");

        let session = connect_client_with_token(&token)
            .await
            .expect("valid token should connect via WebTransport");
        wait_for_session_ready(&session, "alice")
            .await
            .expect("should receive MEETING_STARTED");
        session.close(0u32, b"done");
    }

    #[actix_rt::test]
    async fn test_wt_expired_token_rejected() {
        setup_jwt_wt().await;

        let token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            -120,
            "alice@test.com",
            "wt-jwt-room-2",
            false,
            "Alice",
            false, // end_on_host_leave
            false, // is_guest
        )
        .expect("generate token");

        let result = connect_client_with_token(&token).await;
        assert!(
            result.is_err(),
            "expired token should be rejected via WebTransport"
        );
    }

    #[actix_rt::test]
    async fn test_wt_wrong_secret_rejected() {
        setup_jwt_wt().await;

        let token = meeting_api::token::generate_room_token(
            "completely-different-secret",
            TOKEN_TTL_SECS,
            "alice@test.com",
            "wt-jwt-room-3",
            false,
            "Alice",
            false, // end_on_host_leave
            false, // is_guest
        )
        .expect("generate token");

        let result = connect_client_with_token(&token).await;
        assert!(
            result.is_err(),
            "wrong-secret token should be rejected via WebTransport"
        );
    }

    #[actix_rt::test]
    async fn test_wt_garbage_token_rejected() {
        setup_jwt_wt().await;

        let result = connect_client_with_token("not.a.real.jwt").await;
        assert!(
            result.is_err(),
            "garbage token should be rejected via WebTransport"
        );
    }

    #[actix_rt::test]
    async fn test_wt_token_identity_extracted() {
        setup_jwt_wt().await;

        let token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            TOKEN_TTL_SECS,
            "bob@example.com",
            "wt-special-room",
            false,
            "Bob",
            false, // end_on_host_leave
            false, // is_guest
        )
        .expect("generate token");

        let session = connect_client_with_token(&token)
            .await
            .expect("token with identity should connect");
        wait_for_session_ready(&session, "bob")
            .await
            .expect("should receive MEETING_STARTED");
        session.close(0u32, b"done");
    }

    #[actix_rt::test]
    async fn test_wt_deprecated_endpoint_works_ff_off() {
        setup_jwt_wt().await;
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        let session = connect_client("alice", "wt-jwt-room-6")
            .await
            .expect("deprecated endpoint with FF=off should work");
        wait_for_session_ready(&session, "alice")
            .await
            .expect("should receive MEETING_STARTED");
        session.close(0u32, b"done");

        videocall_types::FeatureFlags::clear_meeting_management_override();
    }

    /// Deprecated path-based endpoint, claiming an `instance_id`.
    async fn connect_client_with_instance_id(
        user: &str,
        meeting: &str,
        instance_id: &str,
    ) -> Result<web_transport_quinn::Session, Box<dyn std::error::Error>> {
        let base = std::env::var("WEBTRANSPORT_URL")
            .unwrap_or_else(|_| "https://127.0.0.1:4433".to_string());
        let url_str = format!(
            "{}/lobby/{}/{}?instance_id={}",
            base.trim_end_matches('/'),
            user,
            meeting,
            urlencoding::encode(instance_id)
        );
        let url = url::Url::parse(&url_str)?;

        let client = web_transport_quinn::ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()?;

        Ok(client.connect(url).await?)
    }

    /// Count MEETING packets of `event_type` arriving on `session` within `window`.
    async fn count_meeting_events(
        session: &web_transport_quinn::Session,
        persistent: Option<web_transport_quinn::RecvStream>,
        window: Duration,
        event_type: videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType,
    ) -> usize {
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let wanted: ::protobuf::EnumOrUnknown<_> = event_type.into();
        let meeting_type: ::protobuf::EnumOrUnknown<VcPacketType> = VcPacketType::MEETING.into();

        // The server picks either channel per packet, so both must be watched.
        // Frames go via a channel because `read_exact` is not cancel-safe: losing
        // a `select!` race mid-frame would desync the stream for every later read.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let forwarder = persistent.map(|mut stream| {
            tokio::spawn(async move {
                while let Some(frame) = read_length_prefixed_frame(&mut stream).await {
                    if tx.send(frame).is_err() {
                        break;
                    }
                }
            })
        });

        let mut count = 0usize;
        let _ = tokio::time::timeout(window, async {
            loop {
                let data = tokio::select! {
                    Some(frame) = rx.recv() => frame,
                    Ok(dg) = session.read_datagram() => dg.to_vec(),
                    else => break,
                };
                if let Ok(wrapper) = VcPacketWrapper::parse_from_bytes(&data) {
                    if wrapper.packet_type == meeting_type {
                        if let Ok(meeting) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                            if meeting.event_type == wanted {
                                count += 1;
                            }
                        }
                    }
                }
            }
        })
        .await;

        if let Some(handle) = forwarder {
            handle.abort();
        }
        count
    }

    /// #2270: the deprecated path must ignore a client-supplied `instance_id`.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn test_wt_deprecated_endpoint_ignores_client_instance_id() {
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;

        setup_jwt_wt().await;
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        let room = "wt-2270-deprecated-instance-id";
        let stolen_iid = "iid-2270-shared";

        let watcher = connect_client("watcher", room)
            .await
            .expect("watcher should connect");
        let watcher_stream = wait_for_session_ready(&watcher, "watcher")
            .await
            .expect("watcher should receive MEETING_STARTED");
        // PARTICIPANT_JOINED/LEFT fire only for Active sessions; a session
        // activates on its first non-RTT packet.
        send_packet(
            &watcher,
            create_test_packet("watcher", VcMediaType::AUDIO, "activate".to_string()),
        )
        .await;

        let first = connect_client_with_instance_id("victim", room, stolen_iid)
            .await
            .expect("first victim session should connect");
        wait_for_session_ready(&first, "victim-1")
            .await
            .expect("first victim session should receive MEETING_STARTED");
        send_packet(
            &first,
            create_test_packet("victim", VcMediaType::AUDIO, "activate".to_string()),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        first.close(0u32, b"transport drop");
        drop(first);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let second = connect_client_with_instance_id("victim", room, stolen_iid)
            .await
            .expect("second session should connect");
        wait_for_session_ready(&second, "victim-2")
            .await
            .expect("second session should receive MEETING_STARTED");
        send_packet(
            &second,
            create_test_packet("victim", VcMediaType::AUDIO, "activate".to_string()),
        )
        .await;

        // Watch past RECONNECT_GRACE_PERIOD (3s).
        let left = count_meeting_events(
            &watcher,
            watcher_stream,
            Duration::from_secs(5),
            MeetingEventType::PARTICIPANT_LEFT,
        )
        .await;

        second.close(0u32, b"done");
        watcher.close(0u32, b"done");
        videocall_types::FeatureFlags::clear_meeting_management_override();

        assert_eq!(
            left, 1,
            "the dropped session's PARTICIPANT_LEFT must still fire; 0 means the \
             second connect adopted its pending departure via the claimed instance_id"
        );
    }

    #[actix_rt::test]
    async fn test_wt_deprecated_endpoint_rejected_ff_on() {
        setup_jwt_wt().await;
        videocall_types::FeatureFlags::set_meeting_management_override(true);

        let result = connect_client("alice", "wt-jwt-room-7").await;
        assert!(
            result.is_err(),
            "deprecated endpoint with FF=on should be rejected"
        );

        videocall_types::FeatureFlags::clear_meeting_management_override();
    }

    #[actix_rt::test]
    async fn test_wt_host_and_attendee_tokens_both_connect() {
        setup_jwt_wt().await;

        let room = "wt-standup";

        let host_token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            TOKEN_TTL_SECS,
            "host@co.com",
            room,
            true,
            "Host",
            false, // end_on_host_leave
            false, // is_guest
        )
        .expect("generate host token");

        let attendee_token = meeting_api::token::generate_room_token(
            JWT_SECRET,
            TOKEN_TTL_SECS,
            "attendee@co.com",
            room,
            false,
            "Attendee",
            false, // end_on_host_leave
            false, // is_guest
        )
        .expect("generate attendee token");

        let host = connect_client_with_token(&host_token)
            .await
            .expect("host token should connect");
        wait_for_session_ready(&host, "host")
            .await
            .expect("host should receive MEETING_STARTED");

        let attendee = connect_client_with_token(&attendee_token)
            .await
            .expect("attendee token should connect");
        wait_for_session_ready(&attendee, "attendee")
            .await
            .expect("attendee should receive MEETING_STARTED");

        host.close(0u32, b"done");
        attendee.close(0u32, b"done");
    }

    /// RAII guard that restores the process-global path-stat sampler cadence to
    /// production ([`WT_HEARTBEAT_INTERVAL`]) on drop — on the NORMAL path AND on
    /// UNWIND. The sampler tests run inside a `tokio::time::timeout(...).await`
    /// block; an assert panic there unwinds PAST any manual restore line, which
    /// would leak the fast 50ms cadence into later `#[serial]` tests in the same
    /// binary. Constructing this guard right after the override (and deleting the
    /// manual restore) makes the restore panic-safe via `Drop`.
    struct PathStatIntervalGuard;
    impl Drop for PathStatIntervalGuard {
        fn drop(&mut self) {
            set_path_stat_sample_interval_for_test(WT_HEARTBEAT_INTERVAL);
        }
    }

    /// RAII guard that clears the process-global meeting-management FeatureFlag
    /// override on drop — on the NORMAL path AND on UNWIND, same rationale as
    /// [`PathStatIntervalGuard`]. The harness test sets the override to run the
    /// deprecated path-based connect; an assert panic inside its timeout block
    /// would otherwise leak the flag to later `#[serial]` tests.
    struct MeetingMgmtOverrideGuard;
    impl Drop for MeetingMgmtOverrideGuard {
        fn drop(&mut self) {
            videocall_types::FeatureFlags::clear_meeting_management_override();
        }
    }

    /// True iff a `videocall_relay_connection_rtt_ms` series currently exists in
    /// the default Prometheus registry whose `room` label equals `room`.
    ///
    /// The relay generates the per-connection `session_id` server-side, so the
    /// test cannot predict it; it filters on the `room` label (= the meeting name
    /// the test DOES control) via `prometheus::gather()`. Used by the #1637
    /// sampler-wiring regression test to assert the gauge appears for a live
    /// session and disappears after teardown.
    fn rtt_series_exists_for_room(room: &str) -> bool {
        prometheus::gather().iter().any(|mf| {
            mf.get_name() == "videocall_relay_connection_rtt_ms"
                && mf.get_metric().iter().any(|m| {
                    m.get_label()
                        .iter()
                        .any(|l| l.get_name() == "room" && l.get_value() == room)
                })
        })
    }

    /// #1637 regression test: the per-connection path-stat sampler is actually
    /// SPAWNED and emitting through the REAL end-to-end session path
    /// (`handle_webtransport_session` -> `spawn_connection_path_sampler`), and its
    /// per-session series is GC'd at teardown.
    ///
    /// This is the END-TO-END belt-and-suspenders for the wiring: it proves the
    /// sampler fires through the real `handle_webtransport_session` with a real
    /// actor + real session accept. RUNS ONLY IN NATS-PROVIDING CI: like every
    /// other test in this module it depends on `start_webtransport_server`, which
    /// blocks on `ChatServer::new`'s NATS connect — so it CANNOT run (nor be
    /// mutation-verified) in a bare environment with no `NATS_URL`. The
    /// deterministic, locally-runnable guard for the same wiring is
    /// `path_stat_sampler_emits_and_gcs_over_loopback_quinn` below, which drives the
    /// extracted `spawn_connection_path_sampler` / `stop_connection_path_sampler`
    /// over a loopback quinn connection with NO NATS and IS mutation-verified.
    ///
    /// Cadence: the production sampler ticks every `WT_HEARTBEAT_INTERVAL` (5s),
    /// far too slow for a test, so we override it to 50ms via the `#[cfg(test)]`
    /// seam `set_path_stat_sample_interval_for_test` (production stays 5s). One
    /// live WT client suffices — the sampler runs per-connection regardless of how
    /// many peers are present.
    ///
    /// `#[serial]`: reads the process-global Prometheus registry and mutates the
    /// process-global sampler-interval override, so it must not race other tests.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn path_stat_sampler_emits_through_real_session_and_gcs_on_teardown() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init();

        // Deprecated path-based connect (connect_client) requires FF=off. Drive the
        // sampler fast enough to observe a tick inside the test window. Both
        // process-global overrides are restored by RAII guards on the normal path
        // AND on an assert-panic unwind inside the timeout block below.
        videocall_types::FeatureFlags::set_meeting_management_override(false);
        let _ff_guard = MeetingMgmtOverrideGuard;
        set_path_stat_sample_interval_for_test(Duration::from_millis(50));
        let _interval_guard = PathStatIntervalGuard;

        let result = tokio::time::timeout(Duration::from_secs(20), async {
            let _wt_handle = start_webtransport_server().await;
            wait_for_server_ready().await;

            // A room name unique to this test so the registry scan cannot collide
            // with series left by other harness tests in the same process.
            let meeting = "rtt-sampler-1637";
            let session = connect_client("rtt-probe-user", meeting)
                .await
                .expect("connect client");
            wait_for_session_ready(&session, "rtt-probe-user")
                .await
                .map_err(|e| anyhow::anyhow!(e))?;

            // Wait for at least one 50ms sampler tick to fire and publish the
            // gauge for this live session's room. Poll up to ~3s so a slow CI box
            // (QUIC handshake + actor spin-up) still sees the first tick; the
            // sampler skips its immediate first tick, so the earliest emit is ~2
            // intervals in.
            let emitted = wait_for_condition_bool(
                || async { rtt_series_exists_for_room(meeting) },
                Duration::from_secs(3),
                Duration::from_millis(25),
            )
            .await
            .is_ok();
            assert!(
                emitted,
                "the path-stat sampler must emit videocall_relay_connection_rtt_ms \
                 for room={meeting} of a live WT session — if this never appears, \
                 the sample_connection_path_stats spawn in handle_webtransport_session \
                 is missing"
            );

            // Tear the session down and confirm the per-session series is removed
            // (forget_connection_path_stats fired at teardown). Closing the client
            // drives the bridge's wait_for_disconnect() to return.
            session.close(0u32, b"done");
            drop(session);

            let gced = wait_for_condition_bool(
                || async { !rtt_series_exists_for_room(meeting) },
                Duration::from_secs(5),
                Duration::from_millis(50),
            )
            .await
            .is_ok();
            assert!(
                gced,
                "the path-stat series for room={meeting} must be removed at session \
                 teardown (forget_connection_path_stats) — a lingering series is the \
                 #996 per-session_id leak"
            );

            Ok::<(), anyhow::Error>(())
        })
        .await;

        // Cadence + FF override restores are handled by `_interval_guard` /
        // `_ff_guard` Drop (panic-safe — runs even if an assert inside the timeout
        // block above unwound past this point).

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("test failed: {e}"),
            Err(_) => panic!("test timed out after 20s"),
        }
    }

    /// A `rustls` server-cert verifier that accepts EVERYTHING — test-only, for the
    /// loopback quinn client below. The loopback server presents the shipped
    /// self-signed dev cert; we are not testing TLS trust, only that the sampler
    /// runs over a real `quinn::Connection`, so the client skips verification.
    #[derive(Debug)]
    struct SkipServerVerification(std::sync::Arc<rustls::crypto::CryptoProvider>);

    impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    /// #1637 DETERMINISTIC regression test for the path-stat sampler's spawn +
    /// teardown WIRING, runnable in a bare environment with NO NATS.
    ///
    /// It stands up a real loopback `quinn` connection (server + client `Endpoint`
    /// on 127.0.0.1, no `ChatServer`/actor/NATS) and drives the EXACT production
    /// helpers `handle_webtransport_session` uses — [`spawn_connection_path_sampler`]
    /// and [`stop_connection_path_sampler`] — against the live server-side
    /// `quinn::Connection`. It asserts the per-session gauge series APPEARS while
    /// the sampler runs, then DISAPPEARS after the stop helper's GC.
    ///
    /// MUTATION-GUARANTEE (the Codex P1): because the test calls the real helpers,
    /// deleting the `actix_rt::spawn(...)` inside `spawn_connection_path_sampler`
    /// makes no series ever appear (the "emitted" assert fails), and deleting the
    /// `forget_connection_path_stats` inside `stop_connection_path_sampler` leaves
    /// the series present (the "GC'd" assert fails). The end-to-end harness test
    /// above proves the same wiring through `handle_webtransport_session` but only
    /// runs in NATS-providing CI; THIS test is the locally-runnable, mutation-
    /// verified guard.
    ///
    /// `#[serial]`: process-global Prometheus registry + sampler-interval override.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn path_stat_sampler_emits_and_gcs_over_loopback_quinn() {
        // Match production: install the ring crypto provider before building TLS
        // configs (ignore error if another test already installed it).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());

        // Override the sampler cadence and arm the RAII guard that restores the
        // production cadence on BOTH the normal path and an assert-panic unwind
        // (the timeout block below panics on assert failure).
        set_path_stat_sample_interval_for_test(Duration::from_millis(50));
        let _interval_guard = PathStatIntervalGuard;

        let result = tokio::time::timeout(Duration::from_secs(15), async {
            // --- Server endpoint from the shipped DER dev cert/key ---
            let cert_der = rustls::pki_types::CertificateDer::from(
                std::fs::read("certs/localhost.der").expect("read certs/localhost.der"),
            );
            let key_der = rustls::pki_types::PrivateKeyDer::try_from(
                std::fs::read("certs/localhost_key.der").expect("read certs/localhost_key.der"),
            )
            .expect("parse localhost_key.der");

            let mut server_crypto = rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .expect("server tls13")
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)
                .expect("server single cert");
            server_crypto.alpn_protocols = vec![b"h3".to_vec()];
            let server_config = quinn::ServerConfig::with_crypto(std::sync::Arc::new(
                quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
                    .expect("quic server config"),
            ));
            let server_endpoint =
                quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap())
                    .expect("server endpoint");
            let server_addr = server_endpoint.local_addr().expect("server addr");

            // --- Client endpoint that skips cert verification ---
            let mut client_crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .expect("client tls13")
                .dangerous()
                .with_custom_certificate_verifier(std::sync::Arc::new(SkipServerVerification(
                    provider.clone(),
                )))
                .with_no_client_auth();
            client_crypto.alpn_protocols = vec![b"h3".to_vec()];
            let mut client_endpoint =
                quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client endpoint");
            client_endpoint.set_default_client_config(quinn::ClientConfig::new(
                std::sync::Arc::new(
                    quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
                        .expect("quic client config"),
                ),
            ));

            // --- Handshake: both sides must be driven concurrently. The client
            // `connect` future only resolves once the FULL handshake completes,
            // which requires the server to accept AND establish the `Incoming` at
            // the same time — so drive the server's accept->establish in its own
            // future and `join!` it with the client connect (awaiting only
            // `accept()` without establishing the `Incoming` would deadlock).
            let server_establish = async {
                server_endpoint
                    .accept()
                    .await
                    .expect("server incoming")
                    .await
                    .expect("server established")
            };
            let client_connect = async {
                client_endpoint
                    .connect(server_addr, "localhost")
                    .expect("client connect")
                    .await
                    .expect("client established")
            };
            let (server_conn, _client_conn) = tokio::join!(server_establish, client_connect);

            // A room unique to this test so the registry scan cannot collide.
            let room = "loopback-rtt-1637";
            let session_id = "loopback-session-1";

            // Drive the REAL production spawn helper against the live server conn.
            let handle = spawn_connection_path_sampler(
                server_conn.clone(),
                room,
                session_id,
                Arc::new(AtomicU64::new(0)),
            );

            // Wait for a sampler tick to publish the gauge (skips first tick, so
            // earliest emit is ~2 * 50ms; poll generously for slow CI).
            let emitted = wait_for_condition_bool(
                || async { rtt_series_exists_for_room(room) },
                Duration::from_secs(3),
                Duration::from_millis(20),
            )
            .await
            .is_ok();
            assert!(
                emitted,
                "spawn_connection_path_sampler must emit \
                 videocall_relay_connection_rtt_ms for room={room}; if it never \
                 appears the actix_rt::spawn wiring inside the helper is gone"
            );

            // VALUE assert (not just existence): the sampler must publish the LIVE
            // climbing sent_packets, not a hardcoded/zeroed value nor a mis-mapped
            // always-zero field (lost_packets / congestion_events are 0 on a quiet
            // loopback link). On a real QUIC connection the relay sends packets
            // during the handshake + sampling window, so path.sent_packets is
            // reliably > 0 by sample time — non-flaky. This catches a mutation
            // INSIDE sample_connection_path_stats that existence-only asserts miss.
            let published_sent = crate::metrics::RELAY_CONNECTION_PATH_SENT_PACKETS
                .with_label_values(&[room, session_id])
                .get();
            assert!(
                published_sent > 0.0,
                "sampler must publish the live climbing sent_packets, not a \
                 hardcoded/zeroed or mis-mapped (always-zero) field; got {published_sent}"
            );
            // Cross-check the gauge tracks the REAL monotonic counter: the gauge
            // holds the value from the last sampler tick, and sent_packets only
            // climbs, so the live counter read now must be >= the published gauge.
            // (Upper-bound only; no exact/lower-bound match — that would race the
            // next tick / further sends. RTT is deliberately NOT value-asserted —
            // loopback RTT varies and would be flaky.)
            let live_sent = server_conn.stats().path.sent_packets as f64;
            assert!(
                published_sent <= live_sent,
                "published sent_packets gauge ({published_sent}) must not exceed the \
                 live monotonic counter ({live_sent}) — the gauge was sampled at or \
                 before now and the counter only climbs"
            );

            // Drive the REAL production teardown helper and confirm GC.
            stop_connection_path_sampler(handle, room, session_id);
            assert!(
                !rtt_series_exists_for_room(room),
                "stop_connection_path_sampler must remove the per-session series \
                 for room={room} (forget_connection_path_stats); a lingering \
                 series is the #996 leak"
            );

            // Keep the connection handles alive until here so the conn is not
            // implicitly closed before the sampler ran.
            drop(server_conn);
            client_endpoint.close(0u32.into(), b"done");
            server_endpoint.close(0u32.into(), b"done");

            Ok::<(), anyhow::Error>(())
        })
        .await;

        // Cadence restore is handled by `_interval_guard`'s Drop (panic-safe).

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("loopback test failed: {e}"),
            Err(_) => panic!("loopback test timed out after 15s"),
        }
    }

    // =====================================================================
    // Tokenless-join guard for the deprecated WT path (issue #2298)
    // =====================================================================

    fn default_udp_buffer_sizes() -> (usize, usize) {
        let plain = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind plain UDP socket");
        let sock = socket2::SockRef::from(&plain);
        (
            sock.recv_buffer_size().expect("read default SO_RCVBUF"),
            sock.send_buffer_size().expect("read default SO_SNDBUF"),
        )
    }

    /// #2716. BITES: it reads 786896, not 4194304.
    #[test]
    #[serial_test::serial]
    fn udp_socket_buffers_are_raised_and_published() {
        let (default_recv, default_send) = default_udp_buffer_sizes();
        let requested = WT_QUIC_UDP_BUFFER_DEFAULT_BYTES;

        let (_socket, sizes) =
            bind_udp_socket("127.0.0.1:0".parse().unwrap(), requested, requested)
                .expect("bind the relay UDP socket");

        assert_eq!(sizes.requested_recv, requested);
        assert_eq!(sizes.requested_send, requested);

        assert!(
            sizes.effective_recv >= requested || sizes.effective_recv > default_recv,
            "SO_RCVBUF stayed at the kernel default: requested {requested}, \
             effective {}, default-bound socket {default_recv}. The relay is \
             running on the default buffer — `set_recv_buffer_size` did not take.",
            sizes.effective_recv,
        );
        assert!(
            sizes.effective_send >= requested || sizes.effective_send > default_send,
            "SO_SNDBUF stayed at the kernel default: requested {requested}, \
             effective {}, default-bound socket {default_send}. The relay is \
             running on the default buffer — `set_send_buffer_size` did not take.",
            sizes.effective_send,
        );

        let gauge = |metric: &prometheus::GaugeVec, setting: &str| {
            metric.with_label_values(&[setting]).get()
        };
        assert_eq!(
            gauge(&RELAY_UDP_SOCKET_RECV_BUFFER_BYTES, "effective"),
            sizes.effective_recv as f64,
            "the effective SO_RCVBUF gauge must publish the kernel read-back",
        );
        assert_eq!(
            gauge(&RELAY_UDP_SOCKET_RECV_BUFFER_BYTES, "requested"),
            requested as f64,
        );
        assert_eq!(
            gauge(&RELAY_UDP_SOCKET_SEND_BUFFER_BYTES, "effective"),
            sizes.effective_send as f64,
            "the effective SO_SNDBUF gauge must publish the kernel read-back",
        );
        assert_eq!(
            gauge(&RELAY_UDP_SOCKET_SEND_BUFFER_BYTES, "requested"),
            requested as f64,
        );
    }

    /// #2716: quinn must accept the socket2-built socket.
    #[actix_rt::test]
    async fn bound_udp_socket_builds_a_quinn_endpoint() {
        let (socket, _sizes) = bind_udp_socket(
            "127.0.0.1:0".parse().unwrap(),
            WT_QUIC_UDP_BUFFER_DEFAULT_BYTES,
            WT_QUIC_UDP_BUFFER_DEFAULT_BYTES,
        )
        .expect("bind the relay UDP socket");

        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )
        .expect("quinn must accept the socket bind_udp_socket produced");

        assert!(
            endpoint.local_addr().expect("local addr").port() > 0,
            "the endpoint must be bound to a real ephemeral port",
        );
    }

    /// #2716: the values must reach the `TransportConfig` the server installs,
    /// not merely exist as constants. BITES: drop the `send_window` call.
    #[test]
    fn transport_config_pins_the_2716_values() {
        let rendered = format!(
            "{:?}",
            build_transport_config().expect("the idle timeout must convert")
        );

        for expected in [
            format!(
                "datagram_send_buffer_size: {}",
                *QUIC_DATAGRAM_SEND_BUFFER_BYTES
            ),
            format!("send_window: {}", *QUIC_SEND_WINDOW_BYTES),
            format!(
                "keep_alive_interval: Some({}s)",
                *QUIC_KEEP_ALIVE_INTERVAL_SECS
            ),
            format!("max_concurrent_uni_streams: {}", 100),
        ] {
            assert!(
                rendered.contains(&expected),
                "the relay TransportConfig is missing {expected:?}; quinn rendered {rendered}",
            );
        }
    }

    /// #2763, through the production booking call. BITES: drop the `mode` label.
    #[test]
    #[serial_test::serial]
    fn the_audio_lane_counter_separates_a_legacy_client_from_a_reverted_one() {
        fn booked(lane: &str, mode: &str) -> f64 {
            RELAY_WT_AUDIO_DOWNLINK_SESSIONS_TOTAL
                .with_label_values(&[lane, mode])
                .get()
        }

        let legacy_before = booked("datagram", "single");
        let reverted_before = booked("datagram", "per_publisher_v1");
        let reliable_before = booked("reliable", "per_publisher_v1");

        assert_eq!(
            book_audio_downlink_session(DownlinkStreamMode::Single, AudioDownlinkLane::Reliable),
            AudioDownlinkLane::Datagram,
            "a single-stream receiver's one reliable stream carries video, so \
             audio must stay on datagrams whatever the cluster is set to",
        );
        assert_eq!(
            book_audio_downlink_session(
                DownlinkStreamMode::PerPublisherV1,
                AudioDownlinkLane::Datagram,
            ),
            AudioDownlinkLane::Datagram,
        );
        assert_eq!(
            book_audio_downlink_session(
                DownlinkStreamMode::PerPublisherV1,
                AudioDownlinkLane::Reliable,
            ),
            AudioDownlinkLane::Reliable,
        );

        assert_eq!(booked("datagram", "single") - legacy_before, 1.0);
        assert_eq!(
            booked("datagram", "per_publisher_v1") - reverted_before,
            1.0,
            "a ds=1 client on a reverted cluster must be readable apart from a \
             legacy build",
        );
        assert_eq!(
            booked("reliable", "per_publisher_v1") - reliable_before,
            1.0
        );
    }

    /// #2742 item 4. BITES: restore `.parse().ok().unwrap_or(default)`.
    #[test]
    fn a_quic_override_rejects_zero_and_unparsable_values() {
        const VAR: &str = "QUIC_SEND_WINDOW_BYTES";
        const DEFAULT: u64 = WT_QUIC_SEND_WINDOW_BYTES;

        for absent in [None, Some(""), Some("   ")] {
            assert_eq!(
                resolve_positive_quic_override(VAR, absent, DEFAULT),
                DEFAULT,
                "{absent:?} means the operator set nothing",
            );
        }
        assert_eq!(
            resolve_positive_quic_override(VAR, Some("0"), DEFAULT),
            DEFAULT,
            "a zero send window makes quinn's write_limit() zero, so every \
             stream write on the relay parks forever from startup",
        );
        for garbage in ["1MiB", "-1", "1_048_576", "0x100"] {
            assert_eq!(
                resolve_positive_quic_override(VAR, Some(garbage), DEFAULT),
                DEFAULT,
                "{garbage:?} must take the default, loudly",
            );
        }
        assert_eq!(
            resolve_positive_quic_override(VAR, Some(" 2097152 "), DEFAULT),
            2_097_152,
            "a valid override must still take effect",
        );
    }

    /// #2716: the keep-alive must stay inside a typical UDP conntrack timeout,
    /// fire several times within `max_idle_timeout`, and not out-wake the app
    /// heartbeat. Reads the resolved values, so env overrides too.
    #[test]
    fn keep_alive_default_is_nat_safe_and_inside_the_idle_timeout() {
        const SHORTEST_COMMON_UDP_CONNTRACK_SECS: u64 = 30;
        let keep_alive = *QUIC_KEEP_ALIVE_INTERVAL_SECS;
        let idle = *QUIC_MAX_IDLE_TIMEOUT_SECS;

        assert!(
            keep_alive * 2 <= SHORTEST_COMMON_UDP_CONNTRACK_SECS,
            "a {keep_alive}s keep-alive leaves no margin against a \
             {SHORTEST_COMMON_UDP_CONNTRACK_SECS}s UDP conntrack timeout",
        );
        assert!(
            keep_alive * 3 <= idle,
            "a {keep_alive}s keep-alive must fire at least three times inside \
             the {idle}s idle timeout so one lost PING cannot close a live \
             connection",
        );
        assert!(
            keep_alive >= 5,
            "a {keep_alive}s keep-alive wakes a mobile radio more often than \
             the session's own {:?} application heartbeat does",
            WT_HEARTBEAT_INTERVAL,
        );
    }

    /// #2716 review: comparing the raw read-back to the request hides a real
    /// clamp on Linux. Drives both branches, which `cfg!` cannot from one host.
    #[test]
    fn granted_buffer_bytes_undoes_the_linux_doubling() {
        const REQUESTED: usize = 4 * 1024 * 1024;
        let clamped_readback = 5 * 1024 * 1024;

        assert_eq!(granted_buffer_bytes(clamped_readback, true), 2_621_440);
        assert!(
            granted_buffer_bytes(clamped_readback, true) < REQUESTED,
            "a Linux read-back of {clamped_readback} means a 2.5 MiB grant \
             against a {REQUESTED} request — the clamp warning must fire",
        );
        assert_eq!(granted_buffer_bytes(REQUESTED, false), REQUESTED);

        assert!(kernel_doubles_buffer_readback("linux"));
        for other in ["macos", "freebsd", "netbsd", "windows"] {
            assert!(
                !kernel_doubles_buffer_readback(other),
                "{other} reports the granted SO_RCVBUF/SO_SNDBUF as-is; halving \
                 it would under-report the buffer by 2x",
            );
        }
    }

    /// #2716: the two gauges replacing the dead `frame_tx_*_blocked` pair must be
    /// REACHABLE. The client's windows are small on purpose: a default quinn peer
    /// never sends MAX_DATA at all, while browsers do.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn peer_credit_grant_stats_are_reachable_over_loopback() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        set_path_stat_sample_interval_for_test(Duration::from_millis(50));
        let _interval_guard = PathStatIntervalGuard;

        const STREAM_WINDOW: u32 = 64 * 1024;
        const CONNECTION_WINDOW: u32 = 128 * 1024;
        // Several windows' worth, so the peer must replenish credit repeatedly.
        const PAYLOAD: usize = 2 * 1024 * 1024;

        let outcome = tokio::time::timeout(Duration::from_secs(30), async {
            let cert_der = rustls::pki_types::CertificateDer::from(
                std::fs::read("certs/localhost.der").expect("read certs/localhost.der"),
            );
            let key_der = rustls::pki_types::PrivateKeyDer::try_from(
                std::fs::read("certs/localhost_key.der").expect("read certs/localhost_key.der"),
            )
            .expect("parse localhost_key.der");

            let mut server_crypto = rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .expect("server tls13")
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)
                .expect("server single cert");
            server_crypto.alpn_protocols = vec![b"h3".to_vec()];
            let server_config = quinn::ServerConfig::with_crypto(std::sync::Arc::new(
                quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
                    .expect("quic server config"),
            ));
            let server_endpoint =
                quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap())
                    .expect("server endpoint");
            let server_addr = server_endpoint.local_addr().expect("server addr");

            let mut client_crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .expect("client tls13")
                .dangerous()
                .with_custom_certificate_verifier(std::sync::Arc::new(SkipServerVerification(
                    provider.clone(),
                )))
                .with_no_client_auth();
            client_crypto.alpn_protocols = vec![b"h3".to_vec()];
            let mut client_config = quinn::ClientConfig::new(std::sync::Arc::new(
                quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
                    .expect("quic client config"),
            ));
            let mut client_transport = quinn::TransportConfig::default();
            client_transport.stream_receive_window(STREAM_WINDOW.into());
            client_transport.receive_window(CONNECTION_WINDOW.into());
            client_config.transport_config(Arc::new(client_transport));

            let mut client_endpoint =
                quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client endpoint");
            client_endpoint.set_default_client_config(client_config);

            let server_establish = async {
                server_endpoint
                    .accept()
                    .await
                    .expect("server incoming")
                    .await
                    .expect("server established")
            };
            let client_connect = async {
                client_endpoint
                    .connect(server_addr, "localhost")
                    .expect("client connect")
                    .await
                    .expect("client established")
            };
            let (server_conn, client_conn) = tokio::join!(server_establish, client_connect);

            // Client drains everything, which is what makes it issue credit. The
            // drainer gets a CLONE so the connection outlives it.
            let drainer_conn = client_conn.clone();
            let drainer = tokio::spawn(async move {
                let mut recv = drainer_conn.accept_uni().await.expect("accept uni");
                let mut buf = vec![0u8; 64 * 1024];
                let mut read = 0usize;
                while let Ok(Some(n)) = recv.read(&mut buf).await {
                    read += n;
                }
                read
            });

            let mut send = server_conn.open_uni().await.expect("open uni");
            send.write_all(&vec![0x5A; PAYLOAD])
                .await
                .expect("write payload");
            send.finish().expect("finish stream");

            let drained = drainer.await.expect("join drainer");
            assert_eq!(
                drained, PAYLOAD,
                "test setup failed: the peer must drain the whole transfer, or it \
                 never replenishes flow-control credit",
            );

            // The final grants can land a round trip after the last read.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut frame_rx = server_conn.stats().frame_rx;
            while (frame_rx.max_data == 0 || frame_rx.max_stream_data == 0)
                && std::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
                frame_rx = server_conn.stats().frame_rx;
            }

            assert!(
                frame_rx.max_stream_data > 0,
                "frame_rx.max_stream_data stayed 0 after draining {PAYLOAD}B \
                 through a {STREAM_WINDOW}B stream window — the gauge replacing \
                 frame_tx_stream_data_blocked would be just as dead",
            );
            assert!(
                frame_rx.max_data > 0,
                "frame_rx.max_data stayed 0 after draining {PAYLOAD}B through a \
                 {CONNECTION_WINDOW}B connection window — the gauge replacing \
                 frame_tx_data_blocked would be just as dead",
            );

            // quinn-proto has no TX construction site for the frames this pair
            // replaces, so frame_tx stays 0 even here.
            let frame_tx = server_conn.stats().frame_tx;
            assert_eq!(
                (frame_tx.data_blocked, frame_tx.stream_data_blocked),
                (0, 0),
                "quinn-proto started transmitting DATA_BLOCKED/STREAM_DATA_BLOCKED; \
                 the #2716 rationale for dropping those gauges no longer holds",
            );

            // Drive the PRODUCTION sampler over the same connection: a gauge that
            // stays 0 here was sampled from the dead `frame_tx` side.
            let room = "credit-grants-2716";
            let session_id = "credit-session-1";
            let sampler = spawn_connection_path_sampler(
                server_conn.clone(),
                room,
                session_id,
                Arc::new(AtomicU64::new(0)),
            );

            let published_deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut conn_credit = 0.0_f64;
            let mut stream_credit = 0.0_f64;
            while (conn_credit <= 0.0 || stream_credit <= 0.0)
                && std::time::Instant::now() < published_deadline
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
                conn_credit = crate::metrics::RELAY_CONNECTION_FRAME_RX_MAX_DATA
                    .with_label_values(&[room, session_id])
                    .get();
                stream_credit = crate::metrics::RELAY_CONNECTION_FRAME_RX_MAX_STREAM_DATA
                    .with_label_values(&[room, session_id])
                    .get();
            }
            stop_connection_path_sampler(sampler, room, session_id);

            // Cumulative and still climbing, so the gauges are >= the snapshot
            // taken above rather than equal to it.
            assert!(
                conn_credit >= frame_rx.max_data as f64
                    && stream_credit >= frame_rx.max_stream_data as f64
                    && conn_credit > 0.0
                    && stream_credit > 0.0,
                "the sampler must publish the frame_RX credit grants (read \
                 conn={conn_credit} stream={stream_credit}, snapshot \
                 conn={} stream={}); reading the frame_tx side leaves both at 0",
                frame_rx.max_data,
                frame_rx.max_stream_data,
            );

            drop(client_conn);
            drop(server_conn);
            server_endpoint.wait_idle().await;
        })
        .await;

        outcome.expect("credit-grant reachability test timed out after 30s");
    }

    /// #2716: BBR only on an exact opt-in, never on a typo.
    #[test]
    fn congestion_controller_defaults_to_cubic_unless_bbr_is_requested() {
        assert_eq!(
            CongestionController::resolve(None),
            CongestionController::Cubic
        );
        assert_eq!(
            CongestionController::resolve(Some("")),
            CongestionController::Cubic
        );
        assert_eq!(
            CongestionController::resolve(Some("cubic")),
            CongestionController::Cubic
        );
        assert_eq!(
            CongestionController::resolve(Some("reno")),
            CongestionController::Cubic,
            "an unrecognised controller must fall back to cubic, not fail startup",
        );
        assert_eq!(
            CongestionController::resolve(Some(" BBR ")),
            CongestionController::Bbr,
            "the opt-in must survive surrounding whitespace and case",
        );
        assert_eq!(CongestionController::Bbr.label(), "bbr");
        assert_eq!(CongestionController::Cubic.label(), "cubic");
    }

    const TOKENLESS_JWT_SECRET: &str = "test-secret-for-wt-identity-tests";

    #[test]
    #[serial_test::serial]
    fn wt_rejects_a_tokenless_connect_when_meeting_management_is_enabled() {
        videocall_types::FeatureFlags::set_meeting_management_override(true);

        let err = resolve_wt_connect_identity(&["lobby", "alice", "room-1"], None)
            .expect_err("FF=on must refuse a tokenless WT connect");

        videocall_types::FeatureFlags::clear_meeting_management_override();
        assert!(
            err.to_string().contains("room access token is required"),
            "expected a token-required rejection — got: {err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn wt_accepts_the_deprecated_path_when_meeting_management_is_disabled() {
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        let identity = resolve_wt_connect_identity(&["lobby", "alice", "room-1"], None);

        videocall_types::FeatureFlags::clear_meeting_management_override();
        let identity = identity.expect("FF=off must keep the deprecated WT path working");
        assert_eq!(identity.username, "alice");
        assert_eq!(identity.lobby_id, "room-1");
        assert_eq!(identity.display_name, "alice");
        assert!(!identity.is_host);
        assert!(!identity.is_guest);
        assert!(!identity.observer);
    }

    #[test]
    #[serial_test::serial]
    fn wt_rejects_nats_unsafe_identifiers_on_the_deprecated_path() {
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        let err = resolve_wt_connect_identity(&["lobby", "alice.evil", "room-1"], None);

        videocall_types::FeatureFlags::clear_meeting_management_override();
        assert!(
            err.is_err(),
            "a dot in the identity must not reach the deprecated WT path"
        );
    }

    #[test]
    #[serial_test::serial]
    fn wt_deprecated_path_rejects_a_spaced_room_and_accepts_a_tilde_room() {
        videocall_types::FeatureFlags::set_meeting_management_override(false);

        let spaced = resolve_wt_connect_identity(&["lobby", "alice", "a b"], None);
        let tilde = resolve_wt_connect_identity(&["lobby", "alice", "a~b"], None);

        videocall_types::FeatureFlags::clear_meeting_management_override();
        assert!(
            spaced.is_err(),
            "a space in the room must be rejected, not rewritten to '_'"
        );
        assert_eq!(tilde.expect("a~b is a valid room").lobby_id, "a~b");
    }

    #[test]
    #[serial_test::serial]
    fn wt_token_path_accepts_a_tilde_room() {
        videocall_types::FeatureFlags::set_meeting_management_override(true);
        std::env::set_var("JWT_SECRET", TOKENLESS_JWT_SECRET);

        let token = meeting_api::token::generate_room_token(
            TOKENLESS_JWT_SECRET,
            60,
            "carol",
            "a~b",
            false,
            "Carol",
            true,
            false,
        )
        .expect("should generate a room token");
        let identity = resolve_wt_connect_identity(&["lobby"], Some(&token));

        std::env::remove_var("JWT_SECRET");
        videocall_types::FeatureFlags::clear_meeting_management_override();

        assert_eq!(identity.expect("a~b is a valid room").lobby_id, "a~b");
    }

    #[test]
    #[serial_test::serial]
    fn wt_takes_identity_from_the_token_not_the_path() {
        videocall_types::FeatureFlags::set_meeting_management_override(true);
        std::env::set_var("JWT_SECRET", TOKENLESS_JWT_SECRET);

        let token = meeting_api::token::generate_room_token(
            TOKENLESS_JWT_SECRET,
            60,
            "carol",
            "claimed-room",
            false,
            "Carol",
            true,
            false,
        )
        .expect("should generate a room token");

        let identity =
            resolve_wt_connect_identity(&["lobby", "spoofed", "spoofed-room"], Some(&token));

        std::env::remove_var("JWT_SECRET");
        videocall_types::FeatureFlags::clear_meeting_management_override();

        let identity = identity.expect("a valid token must be accepted with FF=on");
        assert_eq!(identity.username, "carol");
        assert_eq!(identity.lobby_id, "claimed-room");
    }

    /// Serving per-publisher streams to a client that cannot read them breaks
    /// its downlink, so anything uninterpretable must stay on one stream.
    #[test]
    fn only_a_parseable_ds_version_selects_per_publisher_streams() {
        assert_eq!(
            resolve_downlink_stream_mode(Some("1")),
            DownlinkStreamMode::PerPublisherV1,
        );
        assert_eq!(
            resolve_downlink_stream_mode(Some("2")),
            DownlinkStreamMode::PerPublisherV1,
            "a newer client than this relay is served the highest version the \
             relay implements",
        );
        for absent_or_bad in [
            None,
            Some(""),
            Some("0"),
            Some("-1"),
            Some("yes"),
            Some("1.0"),
        ] {
            assert_eq!(
                resolve_downlink_stream_mode(absent_or_bad),
                DownlinkStreamMode::Single,
                "{absent_or_bad:?} must leave the client on the pre-#2723 downlink",
            );
        }
    }

    #[test]
    fn only_a_ds1_receiver_can_take_the_reliable_audio_stream() {
        assert_eq!(
            resolve_session_audio_lane(
                DownlinkStreamMode::PerPublisherV1,
                AudioDownlinkLane::Reliable
            ),
            AudioDownlinkLane::Reliable,
        );
        assert_eq!(
            resolve_session_audio_lane(
                DownlinkStreamMode::PerPublisherV1,
                AudioDownlinkLane::Datagram
            ),
            AudioDownlinkLane::Datagram,
            "the operator revert must reach a ds=1 receiver",
        );
        for configured in [AudioDownlinkLane::Reliable, AudioDownlinkLane::Datagram] {
            assert_eq!(
                resolve_session_audio_lane(DownlinkStreamMode::Single, configured),
                AudioDownlinkLane::Datagram,
                "a legacy receiver keeps datagram audio whatever {configured:?} says",
            );
        }
    }

    #[test]
    fn the_audio_lane_labels_name_their_primitive() {
        assert_eq!(audio_lane_label(AudioDownlinkLane::Reliable), "reliable");
        assert_eq!(audio_lane_label(AudioDownlinkLane::Datagram), "datagram");
    }

    /// Renaming it on one side silently downgrades every session.
    #[test]
    fn the_capability_parameter_is_named_ds() {
        assert_eq!(DOWNLINK_STREAMS_QUERY_PARAM, "ds");
        let url = url::Url::parse("https://relay.example.com/lobby?token=t&ds=1&instance_id=i")
            .expect("parse url");
        let value = url
            .query_pairs()
            .find(|(key, _)| key == DOWNLINK_STREAMS_QUERY_PARAM)
            .map(|(_, v)| v.into_owned());
        assert_eq!(value.as_deref(), Some("1"));
        assert_eq!(
            resolve_downlink_stream_mode(value.as_deref()),
            DownlinkStreamMode::PerPublisherV1,
        );
    }

    #[test]
    fn wt_request_log_line_never_prints_the_room_token() {
        let token = meeting_api::token::generate_room_token(
            TOKENLESS_JWT_SECRET,
            86_400,
            "dave",
            "room-1",
            false,
            "Dave",
            true,
            false,
        )
        .expect("should generate a room token");
        let url = format!("https://relay.example.com/lobby?token={token}&instance_id=abc");

        let line = wt_request_log_line(&url);

        assert!(
            !line.contains(&token),
            "the room token must not reach the log line — got: {line}"
        );
        assert!(
            !line.contains("token="),
            "no query parameter may survive into the log line — got: {line}"
        );
        assert_eq!(
            line,
            "received WebTransport request: https://relay.example.com/lobby"
        );
    }

    #[test]
    fn wt_request_log_line_keeps_a_tokenless_url_intact() {
        assert_eq!(
            wt_request_log_line("https://relay.example.com/lobby/alice/room-1"),
            "received WebTransport request: https://relay.example.com/lobby/alice/room-1"
        );
    }
}
