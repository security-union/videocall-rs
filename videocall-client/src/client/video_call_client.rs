use super::super::connection::{ConnectOptions, Connection};
use super::super::decode::{PeerDecodeManager, PeerStatus};
use crate::crypto::aes::Aes128State;
use crate::crypto::rsa::RsaWrapper;
use crate::diagnostics::simple_diagnostics::SimpleDiagnostics;
use anyhow::{anyhow, Result};
use gloo::timers::callback::Interval;
use js_sys::Date;
use log::{debug, error, info, warn};
use protobuf::Message;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::RsaPublicKey;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use videocall_types::protos::aes_packet::AesPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::rsa_packet::RsaPacket;
use wasm_bindgen::JsValue;
use yew::prelude::Callback;

/// Options struct for constructing a client via [VideoCallClient::new(options)][VideoCallClient::new]
#[derive(Clone, Debug, PartialEq)]
pub struct VideoCallClientOptions {
    /// `true` to use end-to-end encription; `false` to send data unencrypted
    pub enable_e2ee: bool,

    /// `true` to use webtransport, `false` to use websocket
    pub enable_webtransport: bool,

    /// Callback will be called as `callback(peer_userid)` when a new peer is added
    pub on_peer_added: Callback<String>,

    /// Callback will be called as `callback(peer_userid, media_type)` immediately after the first frame of a given peer & media type is decoded
    pub on_peer_first_frame: Callback<(String, MediaType)>,

    /// Callback will be called as `callback(peer_userid)` and must return the DOM id of the
    /// `HtmlCanvasElement` into which the peer video should be rendered
    pub get_peer_video_canvas_id: Callback<String, String>,

    /// Callback will be called as `callback(peer_userid)` and must return the DOM id of the
    /// `HtmlCanvasElement` into which the peer screen image should be rendered
    pub get_peer_screen_canvas_id: Callback<String, String>,

    /// The current client's userid.  This userid will appear as this client's `peer_userid` in the
    /// remote peers' clients.
    pub userid: String,

    /// The url to which WebSocket connections should be made
    pub websocket_url: String,

    /// The url to which WebTransport connections should be made
    pub webtransport_url: String,

    /// Callback will be called as `callback(())` after a new connection is made
    pub on_connected: Callback<()>,

    /// Callback will be called as `callback(())` if a connection gets dropped
    pub on_connection_lost: Callback<JsValue>,
}

#[derive(Debug)]
struct InnerOptions {
    enable_e2ee: bool,
    userid: String,
    on_peer_added: Callback<String>,
}

#[derive(Debug)]
struct Inner {
    options: InnerOptions,
    connection: Option<Connection>,
    aes: Rc<Aes128State>,
    rsa: Rc<RsaWrapper>,
    peer_decode_manager: PeerDecodeManager,
    diagnostics: SimpleDiagnostics,
    diagnostics_timer: Option<Interval>,
}

/// The client struct for a video call connection.
///
/// To use it, first construct the struct using [new(options)][Self::new].  Then when/if desired,
/// create the connection using [connect()][Self::connect].  Once connected, decoding of media from
/// remote peers will start immediately.
///
#[derive(Clone, Debug)]
pub struct VideoCallClient {
    options: VideoCallClientOptions,
    inner: Rc<RefCell<Inner>>,
    aes: Rc<Aes128State>,
}

impl PartialEq for VideoCallClient {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner) && self.options == other.options
    }
}

impl VideoCallClient {
    /// Constructor for the client struct.
    ///
    /// See [VideoCallClientOptions] for description of the options.
    ///
    pub fn new(options: VideoCallClientOptions) -> Self {
        let aes = Rc::new(Aes128State::new(options.enable_e2ee));
        let inner = Rc::new(RefCell::new(Inner {
            options: InnerOptions {
                enable_e2ee: options.enable_e2ee,
                userid: options.userid.clone(),
                on_peer_added: options.on_peer_added.clone(),
            },
            connection: None,
            aes: aes.clone(),
            rsa: Rc::new(RsaWrapper::new(options.enable_e2ee)),
            peer_decode_manager: Self::create_peer_decoder_manager(&options),
            diagnostics: SimpleDiagnostics::new(true),
            diagnostics_timer: None,
        }));
        Self {
            options,
            aes,
            inner,
        }
    }

    /// Initiates a connection to a videocall server.
    ///
    /// Initiates a connection using WebTransport (to
    /// [`options.webtransport_url`](VideoCallClientOptions::webtransport_url)) or WebSocket (to
    /// [`options.websocket_url`](VideoCallClientOptions::websocket_url)), based on the value of
    /// [`options.enable_webtransport`](VideoCallClientOptions::enable_webtransport).
    ///
    /// Note that this method's success means only that it succesfully *attempted* initiation of the
    /// connection.  The connection cannot actually be considered to have been succesful until the
    /// [`options.on_connected`](VideoCallClientOptions::on_connected) callback has been invoked.
    ///
    /// If the connection does not succeed, the
    /// [`options.on_connection_lost`](VideoCallClientOptions::on_connection_lost) callback will be
    /// invoked.
    ///
    pub fn connect(&mut self) -> anyhow::Result<()> {
        let options = ConnectOptions {
            userid: self.options.userid.clone(),
            websocket_url: self.options.websocket_url.clone(),
            webtransport_url: self.options.webtransport_url.clone(),
            on_inbound_media: {
                let inner = Rc::downgrade(&self.inner);
                Callback::from(move |packet| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        match inner.try_borrow_mut() {
                            Ok(mut inner) => inner.on_inbound_media(packet),
                            Err(_) => {
                                error!(
                                    "Unable to borrow inner -- dropping receive packet {:?}",
                                    packet
                                );
                            }
                        }
                    }
                })
            },
            on_connected: {
                let inner = Rc::downgrade(&self.inner);
                let callback = self.options.on_connected.clone();
                let client_weak = Rc::downgrade(&self.inner); // For starting diagnostics
                Callback::from(move |_| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        match inner.try_borrow() {
                            Ok(inner) => inner.send_public_key(),
                            Err(_) => {
                                error!("Unable to borrow inner -- not sending public key");
                            }
                        }
                    }
                    
                    // Auto-start diagnostics when connected
                    if let Some(client_ref) = Weak::upgrade(&client_weak) {
                        if let Ok(client_borrowed) = client_ref.try_borrow() {
                            let self_id = client_borrowed.options.userid.clone();
                            error!("DIAGNOSTICS: Auto-starting for user {}", self_id);
                            drop(client_borrowed); // Release the borrow before trying again
                            
                            if let Ok(mut client_borrowed_mut) = client_ref.try_borrow_mut() {
                                client_borrowed_mut.diagnostics.set_enabled(true);
                                error!("DIAGNOSTICS: Successfully enabled collection");
                            }
                        }
                    }
                    
                    callback.emit(());
                })
            },
            on_connection_lost: self.options.on_connection_lost.clone(),
            peer_monitor: {
                let inner = Rc::downgrade(&self.inner);
                let on_connection_lost = self.options.on_connection_lost.clone();
                Callback::from(move |_| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        match inner.try_borrow_mut() {
                            Ok(mut inner) => {
                                inner.peer_decode_manager.run_peer_monitor();
                            }
                            Err(_) => {
                                on_connection_lost.emit(JsValue::from_str(
                                    "Unable to borrow inner -- not starting peer monitor",
                                ));
                            }
                        }
                    }
                })
            },
        };
        info!(
            "webtransport connect = {}",
            self.options.enable_webtransport
        );
        info!(
            "end to end encryption enabled = {}",
            self.options.enable_e2ee
        );

        let mut borrowed = self.inner.try_borrow_mut()?;
        borrowed.connection.replace(Connection::connect(
            self.options.enable_webtransport,
            options,
            self.aes.clone(),
        )?);
        info!("Connected to server");
        
        // Start the diagnostics timer after establishing connection
        drop(borrowed); // Release the borrow before trying to start diagnostics
        error!("DIAGNOSTICS: Starting diagnostics timer after connection");
        self.start_diagnostics(); // Start collecting diagnostics
        
        Ok(())
    }

    fn create_peer_decoder_manager(opts: &VideoCallClientOptions) -> PeerDecodeManager {
        let mut peer_decode_manager = PeerDecodeManager::new();
        peer_decode_manager.on_first_frame = opts.on_peer_first_frame.clone();
        peer_decode_manager.get_video_canvas_id = opts.get_peer_video_canvas_id.clone();
        peer_decode_manager.get_screen_canvas_id = opts.get_peer_screen_canvas_id.clone();
        peer_decode_manager
    }

    pub(crate) fn send_packet(&self, media: PacketWrapper) {
        match self.inner.try_borrow() {
            Ok(inner) => inner.send_packet(media),
            Err(_) => {
                error!("Unable to borrow inner -- dropping send packet {:?}", media)
            }
        }
    }

    /// Returns `true` if the client is currently connected to a server.
    pub fn is_connected(&self) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(connection) = &inner.connection {
                return connection.is_connected();
            }
        };
        false
    }

    /// Returns a vector of the userids of the currently connected remote peers, sorted alphabetically.
    pub fn sorted_peer_keys(&self) -> Vec<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner.peer_decode_manager.sorted_keys().to_vec(),
            Err(_) => Vec::<String>::new(),
        }
    }

    /// Hacky function that returns true if the given peer has yet to send a frame of screen share.
    ///
    /// No reason for this function to exist, it should be deducible from the
    /// [`options.on_peer_first_frame(key, MediaType::Screen)`](VideoCallClientOptions::on_peer_first_frame)
    /// callback.   Or if polling is really necessary, instead of being hardwired for screen, it'd
    /// be more elegant to at least pass a `MediaType`.
    ///
    pub fn is_awaiting_peer_screen_frame(&self, key: &String) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(key) {
                return peer.screen.is_waiting_for_keyframe();
            }
        }
        false
    }

    pub(crate) fn aes(&self) -> Rc<Aes128State> {
        self.aes.clone()
    }

    /// Returns a reference to a copy of [`options.userid`](VideoCallClientOptions::userid)
    pub fn userid(&self) -> &String {
        &self.options.userid
    }

    /// Returns a summary of the diagnostics information collected
    pub fn get_diagnostics_summary(&self) -> String {
        info!("Getting diagnostics summary");
        error!("DIAGNOSTICS: Attempting to generate diagnostics summary");
        
        // Use message passing to collect and process diagnostics data
        let diagnostic_data = match self.inner.try_borrow() {
            Ok(inner) => inner.peer_decode_manager.collect_diagnostic_data(),
            Err(_) => {
                warn!("Failed to access peer decode manager for diagnostics");
                error!("DIAGNOSTICS: Failed to access peer decode manager");
                return "Error: Could not collect diagnostics data".to_string();
            }
        };
        
        error!("DIAGNOSTICS: Collected {} data points", diagnostic_data.len());
        
        // Process the collected data with the diagnostics module
        // No need for a mutable borrow of the entire inner struct
        let mut diagnostics = SimpleDiagnostics::new(true);
        diagnostics.process_batch(diagnostic_data);
        
        // Get the summary from our temporary diagnostics instance
        let summary = diagnostics.get_metrics_summary();
        debug!("Diagnostics summary generated, length: {} chars", summary.len());
        error!("DIAGNOSTICS: Summary generated, length: {} chars", summary.len());
        summary
    }

    /// Enable or disable diagnostics collection
    pub fn set_diagnostics_enabled(&self, enabled: bool) {
        info!("Setting diagnostics enabled: {}", enabled);
        error!("DIAGNOSTICS: Setting enabled state to {}", enabled);
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.diagnostics.set_enabled(enabled);
            debug!("Diagnostics collection set to {}", enabled);
        } else {
            warn!("Failed to set diagnostics enabled state to {}", enabled);
        }
    }

    pub fn start_diagnostics(&self) {
        // Set up a timer to periodically update the diagnostics
        info!("Starting diagnostics collection and reporting");
        error!("DIAGNOSTICS: Starting collection and reporting");
        let inner_weak = Rc::downgrade(&self.inner);
        
        // Use a fixed interval of 5000ms (5 seconds)
        const DIAGNOSTICS_INTERVAL_MS: u32 = 5000;
        
        let timer = Interval::new(DIAGNOSTICS_INTERVAL_MS, move || {
            error!("DIAGNOSTICS: Timer fired at {}", js_sys::Date::now());
            if let Some(inner) = Weak::upgrade(&inner_weak) {
                // Collect diagnostic data
                let diagnostic_data = match inner.try_borrow() {
                    Ok(inner_ref) => inner_ref.peer_decode_manager.collect_diagnostic_data(),
                    Err(_) => {
                        warn!("Could not access peer decode manager for diagnostics");
                        error!("DIAGNOSTICS: Could not access peer decode manager in timer");
                        return;
                    }
                };
                
                if diagnostic_data.is_empty() {
                    debug!("No diagnostic data to process");
                    error!("DIAGNOSTICS: No data to process in timer");
                    return;
                }
                
                error!("DIAGNOSTICS: Processing {} data points", diagnostic_data.len());
                
                // Process data and prepare packets in a separate mutable borrow
                let packets_to_send: Vec<PacketWrapper> = match inner.try_borrow_mut() {
                    Ok(mut inner_mut) => {
                        // Update the diagnostics module with collected data
                        inner_mut.diagnostics.process_batch(diagnostic_data);
                        
                        // Create diagnostic packets for each peer
                        let mut packets = Vec::new();
                        let userid = inner_mut.options.userid.clone();
                        
                        for peer_id in inner_mut.peer_decode_manager.sorted_keys() {
                            if let Some(packet) = inner_mut.diagnostics.create_packet_wrapper(peer_id, &userid) {
                                debug!("Created diagnostic packet for peer {}", peer_id);
                                packets.push(packet);
                            }
                        }
                        
                        error!("DIAGNOSTICS: Created {} packets to send", packets.len());
                        packets
                    },
                    Err(_) => {
                        warn!("Could not get mutable access to process diagnostics");
                        error!("DIAGNOSTICS: Could not get mutable access in timer");
                        return;
                    }
                };
                
                // Send the prepared packets in a separate borrow
                if !packets_to_send.is_empty() {
                    debug!("Sending {} diagnostic packets", packets_to_send.len());
                    let packet_count = packets_to_send.len(); // Store the count before moving
                    match inner.try_borrow() {
                        Ok(inner_ref) => {
                            for packet in packets_to_send {
                                inner_ref.send_packet(packet);
                            }
                            info!("Sent diagnostic packets to peers");
                            error!("DIAGNOSTICS: Successfully sent {} packets", packet_count); // Use stored count
                        },
                        Err(_) => {
                            warn!("Could not send diagnostic packets");
                            error!("DIAGNOSTICS: Could not send packets");
                        }
                    }
                }
            } else {
                warn!("Diagnostic timer fired but inner reference is gone");
                error!("DIAGNOSTICS: Inner reference is gone in timer");
            }
        });
        
        // Store the timer
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.diagnostics_timer = Some(timer);
            info!("Diagnostics timer started, reporting every {}ms", DIAGNOSTICS_INTERVAL_MS);
            error!("DIAGNOSTICS: Timer started with interval {}ms", DIAGNOSTICS_INTERVAL_MS);
        } else {
            warn!("Failed to store diagnostics timer");
            error!("DIAGNOSTICS: Failed to store timer");
        }
    }

    /// Helper method for testing logging levels
    pub fn test_log_levels(&self) {
        error!("DIAGNOSTICS TEST: This is an ERROR level message");
        warn!("DIAGNOSTICS TEST: This is a WARN level message");
        info!("DIAGNOSTICS TEST: This is an INFO level message");
        debug!("DIAGNOSTICS TEST: This is a DEBUG level message");
        
        // Also test with the diagnostics timer
        error!("DIAGNOSTICS: Forcing timer cycle at {}", Date::now());
        if let Ok(inner) = self.inner.try_borrow() {
            let data = inner.peer_decode_manager.collect_diagnostic_data();
            error!("DIAGNOSTICS TEST: Collected {} diagnostic data points", data.len());
        }
    }
}

impl Inner {
    fn send_packet(&self, media: PacketWrapper) {
        if let Some(connection) = &self.connection {
            connection.send_packet(media);
        }
    }

    fn on_inbound_media(&mut self, response: PacketWrapper) {
        debug!(
            "<< Received {:?} from {}",
            response.packet_type.enum_value(),
            response.email
        );
        
        // Record packet for diagnostics
        self.diagnostics.record_packet(&response.email, response.data.len());
        
        let peer_status = self.peer_decode_manager.ensure_peer(&response.email);
        match response.packet_type.enum_value() {
            Ok(PacketType::AES_KEY) => {
                if !self.options.enable_e2ee {
                    return;
                }
                if let Ok(bytes) = self.rsa.decrypt(&response.data) {
                    debug!("Decrypted AES_KEY from {}", response.email);
                    match AesPacket::parse_from_bytes(&bytes) {
                        Ok(aes_packet) => {
                            if let Err(e) = self.peer_decode_manager.set_peer_aes(
                                &response.email,
                                Aes128State::from_vecs(
                                    aes_packet.key,
                                    aes_packet.iv,
                                    self.options.enable_e2ee,
                                ),
                            ) {
                                error!("Failed to set peer aes: {}", e.to_string());
                            }
                        }
                        Err(e) => {
                            error!("Failed to parse aes packet: {}", e.to_string());
                            // Record packet loss for diagnostics
                            self.diagnostics.record_packet_lost(&response.email);
                        }
                    }
                }
            }
            Ok(PacketType::RSA_PUB_KEY) => {
                if !self.options.enable_e2ee {
                    return;
                }
                let encrypted_aes_packet = parse_rsa_packet(&response.data)
                    .and_then(parse_public_key)
                    .and_then(|pub_key| {
                        self.serialize_aes_packet()
                            .map(|aes_packet| (aes_packet, pub_key))
                    })
                    .and_then(|(aes_packet, pub_key)| {
                        self.encrypt_aes_packet(&aes_packet, &pub_key)
                    });

                match encrypted_aes_packet {
                    Ok(data) => {
                        debug!(">> {} sending AES key", self.options.userid);
                        self.send_packet(PacketWrapper {
                            packet_type: PacketType::AES_KEY.into(),
                            email: self.options.userid.clone(),
                            data,
                            ..Default::default()
                        });
                    }
                    Err(e) => {
                        error!("Failed to send AES_KEY to peer: {}", e.to_string());
                    }
                }
            }
            Ok(PacketType::MEDIA) => {
                let email = response.email.clone();
                
                // Check if this is a diagnostics packet in a MEDIA wrapper
                let data_str = std::str::from_utf8(&response.data);
                if let Ok(data_str) = data_str {
                    if data_str.starts_with("DIAGNOSTICS:") {
                        debug!("Received diagnostics data from {}: {}", email, data_str);
                        // In a future version, we'd parse and use this data for adaptation
                        return;
                    }
                }
                
                if let Err(e) = self.peer_decode_manager.decode(response) {
                    error!("error decoding packet: {}", e.to_string());
                    self.peer_decode_manager.delete_peer(&email);
                    // Record packet loss for diagnostics
                    self.diagnostics.record_packet_lost(&email);
                }
            }
            Ok(PacketType::CONNECTION) => {
                error!("Not implemented: CONNECTION packet type");
            }
            // Handle any other packet types including DIAGNOSTICS
            Ok(_) => {
                debug!("Received unknown packet type from {}", response.email);
            }
            Err(_) => {
                // Record packet loss for diagnostics
                self.diagnostics.record_packet_lost(&response.email);
            }
        }
        if let PeerStatus::Added(peer_userid) = peer_status {
            debug!("added peer {}", peer_userid);
            self.send_public_key();
            self.options.on_peer_added.emit(peer_userid);
        }
    }

    fn send_public_key(&self) {
        if !self.options.enable_e2ee {
            return;
        }
        let userid = self.options.userid.clone();
        let rsa = &*self.rsa;
        match rsa.pub_key.to_public_key_der() {
            Ok(public_key_der) => {
                let packet = RsaPacket {
                    username: userid.clone(),
                    public_key_der: public_key_der.to_vec(),
                    ..Default::default()
                };
                match packet.write_to_bytes() {
                    Ok(data) => {
                        debug!(">> {} sending public key", userid);
                        self.send_packet(PacketWrapper {
                            packet_type: PacketType::RSA_PUB_KEY.into(),
                            email: userid,
                            data,
                            ..Default::default()
                        });
                    }
                    Err(e) => {
                        error!("Failed to serialize rsa packet: {}", e.to_string());
                    }
                }
            }
            Err(e) => {
                error!("Failed to export rsa public key to der: {}", e.to_string());
            }
        }
    }

    fn serialize_aes_packet(&self) -> Result<Vec<u8>> {
        AesPacket {
            key: self.aes.key.to_vec(),
            iv: self.aes.iv.to_vec(),
            ..Default::default()
        }
        .write_to_bytes()
        .map_err(|e| anyhow!("Failed to serialize aes packet: {}", e.to_string()))
    }

    fn encrypt_aes_packet(&self, aes_packet: &[u8], pub_key: &RsaPublicKey) -> Result<Vec<u8>> {
        self.rsa
            .encrypt_with_key(aes_packet, pub_key)
            .map_err(|e| anyhow!("Failed to encrypt aes packet: {}", e.to_string()))
    }
}

fn parse_rsa_packet(response_data: &[u8]) -> Result<RsaPacket> {
    RsaPacket::parse_from_bytes(response_data)
        .map_err(|e| anyhow!("Failed to parse rsa packet: {}", e.to_string()))
}

fn parse_public_key(rsa_packet: RsaPacket) -> Result<RsaPublicKey> {
    RsaPublicKey::from_public_key_der(&rsa_packet.public_key_der)
        .map_err(|e| anyhow!("Failed to parse rsa public key: {}", e.to_string()))
}
