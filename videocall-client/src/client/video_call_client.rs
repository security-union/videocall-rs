use super::super::connection::{ConnectOptions, Connection};
use super::super::decode::{PeerDecodeManager, PeerStatus};
use super::diagnostics::DiagnosticsManager;
use crate::crypto::aes::Aes128State;
use crate::crypto::rsa::RsaWrapper;
use anyhow::{anyhow, Result};
use log::{debug, error};
use protobuf::Message;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::RsaPublicKey;
use std::cell::RefCell;
use std::rc::Rc;
use videocall_types::protos::aes_packet::AesPacket;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::media_packet::MediaPacket;
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
    diagnostics_manager: Option<DiagnosticsManager>,
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
        let inner_options = InnerOptions {
            enable_e2ee: options.enable_e2ee,
            userid: options.userid.clone(),
            on_peer_added: options.on_peer_added.clone(),
        };
        let inner = Rc::new(RefCell::new(Inner {
            options: inner_options,
            connection: None,
            aes: Rc::clone(&aes),
            rsa: Rc::new(RsaWrapper::new(options.enable_e2ee)),
            peer_decode_manager: PeerDecodeManager::new(
                options.on_peer_first_frame.clone(),
                options.get_peer_video_canvas_id.clone(),
                options.get_peer_screen_canvas_id.clone(),
            ),
            diagnostics_manager: Some(DiagnosticsManager::new(options.userid.clone())),
        }));

        Self {
            options,
            inner,
            aes,
        }
    }

    /// Create a connection using the client options.
    ///
    /// This can be used to set up a connection after the [VideoCallClient] struct is created.
    ///
    pub fn connect(&self) -> Result<()> {
        let options = ConnectOptions {
            userid: self.options.userid.clone(),
            websocket_url: self.options.websocket_url.clone(),
            webtransport_url: self.options.webtransport_url.clone(),
            on_connected: self.options.on_connected.clone(),
            on_connection_lost: self.options.on_connection_lost.clone(),
            peer_monitor: Callback::from({
                let inner = Rc::clone(&self.inner);
                move |_| {
                    if let Ok(inner) = inner.try_borrow_mut() {
                        inner.send_public_key();
                    }
                }
            }),
            on_inbound_media: Callback::from({
                let inner = Rc::clone(&self.inner);
                move |response| {
                    if let Ok(mut inner) = inner.try_borrow_mut() {
                        inner.on_inbound_media(response);
                    }
                }
            }),
        };

        let mut inner = self.inner.borrow_mut();

        // Create the connection
        inner.connection = Some(Connection::connect(
            self.options.enable_webtransport,
            options,
            Rc::clone(&self.aes),
        )?);
        
        // Setup diagnostics timer
        let inner_weak = Rc::downgrade(&self.inner);
        gloo::timers::callback::Interval::new(2000, move || {
            if let Some(inner_rc) = inner_weak.upgrade() {
                if let Ok(inner_ref) = inner_rc.try_borrow() {
                    // Check if we should send diagnostics
                    let should_send = if let Some(ref dm) = inner_ref.diagnostics_manager {
                        dm.should_send_diagnostics()
                    } else {
                        false
                    };
                    
                    // Get packets if we should send
                    let packets = if should_send {
                        if let Some(ref dm) = inner_ref.diagnostics_manager {
                            dm.create_diagnostics_packets()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    };
                    
                    // Send packets if we got any
                    if !packets.is_empty() {
                        if let Some(ref conn) = inner_ref.connection {
                            for (_, packet) in &packets {
                                conn.send_packet(packet.clone());
                            }
                        }
                    }
                    
                    // Mark as sent if we sent anything
                    if should_send {
                        drop(inner_ref);
                        if let Ok(mut inner_mut) = inner_rc.try_borrow_mut() {
                            if let Some(ref mut dm) = inner_mut.diagnostics_manager {
                                dm.mark_diagnostics_sent();
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    fn create_peer_decoder_manager(opts: &VideoCallClientOptions) -> PeerDecodeManager {
        PeerDecodeManager::new(
            opts.on_peer_first_frame.clone(),
            opts.get_peer_video_canvas_id.clone(),
            opts.get_peer_screen_canvas_id.clone(),
        )
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

    /// Tries to get a clone of the DiagnosticsManager if available
    pub fn try_get_diagnostics_manager(&self) -> Result<DiagnosticsManager> {
        match self.inner.try_borrow() {
            Ok(inner) => {
                if let Some(ref diag) = inner.diagnostics_manager {
                    Ok(diag.clone())
                } else {
                    Err(anyhow!("DiagnosticsManager not available"))
                }
            }
            Err(_) => Err(anyhow!("Could not borrow inner"))
        }
    }

    fn send_heartbeat_and_diagnostics(&self, user_id: String, elapsed_seconds: i32) {
        // First, check if we can borrow
        if let Ok(inner) = self.inner.try_borrow() {
            // Send heartbeat (existing functionality)
            if let Some(connection) = &inner.connection {
                let packet = MediaPacket {
                    media_type: MediaType::HEARTBEAT.into(),
                    email: user_id.clone(),
                    timestamp: js_sys::Date::now(),
                    ..Default::default()
                };
                if let Ok(data) = inner.aes.encrypt(&packet.write_to_bytes().unwrap()) {
                    let packet = PacketWrapper {
                        data,
                        email: user_id.clone(),
                        packet_type: PacketType::MEDIA.into(),
                        ..Default::default()
                    };
                    
                    connection.send_packet(packet);
                }
            }
            
            // Check if it's time to send diagnostics
            let should_send = inner.diagnostics_manager.as_ref()
                .map(|dm| dm.should_send_diagnostics())
                .unwrap_or(false);
                
            if should_send {
                // Need to get diagnostics packets before modifying manager
                let packets = if let Some(dm) = &inner.diagnostics_manager {
                    dm.create_diagnostics_packets()
                } else {
                    Vec::new()
                };
                
                // Send the packets
                if let Some(connection) = &inner.connection {
                    for (target_peer, packet) in packets {
                        connection.send_packet(packet);
                    }
                }
                
                // Drop the immutable borrow and create a mutable one to update timestamp
                drop(inner);
                if let Ok(mut inner) = self.inner.try_borrow_mut() {
                    if let Some(ref mut dm) = inner.diagnostics_manager {
                        dm.mark_diagnostics_sent();
                    }
                }
            }
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
                if let Err(e) = self.peer_decode_manager.decode(
                    response,
                    self.diagnostics_manager.as_mut()
                ) {
                    error!("error decoding packet: {}", e.to_string());
                    self.peer_decode_manager.delete_peer(&email);
                }
            }
            Ok(PacketType::CONNECTION) => {
                error!("Not implemented: CONNECTION packet type");
            }
            Ok(PacketType::DIAGNOSTICS) => {
                if let Some(ref mut diagnostics_manager) = self.diagnostics_manager {
                    match DiagnosticsPacket::parse_from_bytes(&response.data) {
                        Ok(packet) => {
                            diagnostics_manager.process_diagnostics(&response.email, packet);
                        }
                        Err(e) => {
                            error!("Failed to parse diagnostics packet: {}", e);
                        }
                    }
                }
            }
            Err(_) => {}
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
