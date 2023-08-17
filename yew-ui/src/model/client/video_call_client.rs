use super::super::connection::{ConnectOptions, Connection};
use super::super::decode::PeerDecodeManager;
use crate::crypto::aes::Aes128State;
use crate::crypto::rsa::RsaWrapper;
use anyhow::{anyhow, Result};
use log::{debug, error, info};
use protobuf::Message;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::RsaPublicKey;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use types::protos::aes_packet::AesPacket;
use types::protos::media_packet::media_packet::MediaType;
use types::protos::packet_wrapper::packet_wrapper::PacketType;
use types::protos::packet_wrapper::PacketWrapper;
use types::protos::rsa_packet::RsaPacket;
use yew::prelude::Callback;
#[derive(Clone, Debug, PartialEq)]
pub struct VideoCallClientOptions {
    pub enable_e2ee: bool,
    pub enable_webtransport: bool,
    pub on_peer_added: Callback<String>,
    pub on_peer_first_frame: Callback<(String, MediaType)>,
    pub get_peer_video_canvas_id: Callback<String, String>,
    pub get_peer_screen_canvas_id: Callback<String, String>,
    pub userid: String,
    pub websocket_url: String,
    pub webtransport_url: String,
    pub on_connected: Callback<()>,
    pub on_connection_lost: Callback<()>,
}

#[derive(Debug)]
struct InnerOptions {
    enable_e2ee: bool,
    userid: String,
}

#[derive(Debug)]
struct Inner {
    options: InnerOptions,
    connection: Option<Connection>,
    aes: Arc<Aes128State>,
    rsa: Arc<RsaWrapper>,
    peer_decode_manager: PeerDecodeManager,
}

#[derive(Clone, Debug)]
pub struct VideoCallClient {
    options: VideoCallClientOptions,
    inner: Rc<RefCell<Inner>>,
}

impl PartialEq for VideoCallClient {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner) && self.options == other.options
    }
}

impl VideoCallClient {
    pub fn new(options: VideoCallClientOptions) -> Self {
        let inner = Rc::new(RefCell::new(Inner {
            options: InnerOptions {
                enable_e2ee: options.enable_e2ee,
                userid: options.userid.clone(),
            },
            connection: None,
            aes: Arc::new(Aes128State::new(options.enable_e2ee)),
            rsa: Arc::new(RsaWrapper::new(options.enable_e2ee)),
            peer_decode_manager: Self::create_peer_decoder_manager(&options),
        }));
        {
            let mut borrowed = inner.borrow_mut();
            borrowed.peer_decode_manager.on_peer_added = {
                let inner = Rc::downgrade(&inner);
                let callback = options.on_peer_added.clone();
                Callback::from(move |key| {
                    debug!("New peer arrived.");
                    if let Some(inner) = Weak::upgrade(&inner) {
                        if let Ok(inner) = inner.try_borrow() {
                            inner.send_public_key()
                        }
                    };
                    callback.emit(key);
                })
            };
        }
        Self { options, inner }
    }

    pub fn connect(&mut self) -> anyhow::Result<()> {
        let options = ConnectOptions {
            userid: self.options.userid.clone(),
            websocket_url: self.options.websocket_url.clone(),
            webtransport_url: self.options.webtransport_url.clone(),
            on_inbound_media: {
                let inner = Rc::downgrade(&self.inner);
                Callback::from(move |packet| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        if let Ok(mut inner) = inner.try_borrow_mut() {
                            inner.on_inbound_media(packet)
                        }
                    }
                })
            },
            on_connected: {
                let inner = Rc::downgrade(&self.inner);
                let callback = self.options.on_connected.clone();
                Callback::from(move |_| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        if let Ok(inner) = inner.try_borrow() {
                            inner.send_public_key()
                        }
                    }
                    callback.emit(());
                })
            },
            on_connection_lost: {
                let inner = Rc::downgrade(&self.inner);
                let callback = self.options.on_connection_lost.clone();
                Callback::from(move |_| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        if let Ok(mut inner) = inner.try_borrow_mut() {
                            inner.disconnect()
                        }
                    }
                    callback.emit(());
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
        let aes = borrowed.aes.clone();
        borrowed.connection.replace(Connection::connect(
            self.options.enable_webtransport,
            options,
            aes,
        )?);
        Ok(())
    }

    fn create_peer_decoder_manager(opts: &VideoCallClientOptions) -> PeerDecodeManager {
        let mut peer_decode_manager = PeerDecodeManager::new();
        peer_decode_manager.on_first_frame = opts.on_peer_first_frame.clone();
        peer_decode_manager.get_video_canvas_id = opts.get_peer_video_canvas_id.clone();
        peer_decode_manager.get_screen_canvas_id = opts.get_peer_screen_canvas_id.clone();
        peer_decode_manager
    }

    pub fn send_packet(&self, media: PacketWrapper) {
        if let Ok(inner) = self.inner.try_borrow() {
            inner.send_packet(media)
        }
    }

    pub fn is_connected(&self) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(connection) = &inner.connection {
                return connection.is_connected();
            }
        };
        false
    }

    pub fn sorted_peer_keys(&self) -> Vec<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner.peer_decode_manager.sorted_keys().to_vec(),
            Err(_) => Vec::<String>::new(),
        }
    }

    pub fn is_awaiting_peer_screen_frame(&self, key: &String) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(key) {
                return peer.screen.is_waiting_for_keyframe();
            }
        }
        false
    }

    pub fn aes(&self) -> Arc<Aes128State> {
        self.inner.borrow().aes.clone()
    }
}

impl Inner {
    fn disconnect(&mut self) {
        self.connection.take();
    }

    fn send_packet(&self, media: PacketWrapper) {
        if let Some(connection) = &self.connection {
            connection.send_packet(media);
        }
    }

    fn on_inbound_media(&mut self, response: PacketWrapper) {
        match response.packet_type.enum_value() {
            Ok(PacketType::AES_KEY) => {
                if !self.options.enable_e2ee {
                    return;
                }
                debug!("Received AES_KEY {}", &response.email);
                if let Ok(bytes) = self.rsa.decrypt(&response.data) {
                    match AesPacket::parse_from_bytes(&bytes) {
                        Ok(aes_packet) => {
                            self.peer_decode_manager.set_peer_aes(
                                &response.email,
                                Aes128State::from_vecs(aes_packet.key, aes_packet.iv, self.options.enable_e2ee),
                                );
                        }
                        Err(e) => {
                            error!("Failed to parse aes packet: {}", e.to_string());
                        }
                    }
                }
                return;
            }
            Ok(PacketType::RSA_PUB_KEY) => {
                if !self.options.enable_e2ee {
                    return;
                }
                debug!("Received RSA_PUB_KEY");
                let encrypted_aes_packet = parse_rsa_packet(&response.data)
                    .and_then(parse_public_key)
                    .and_then(|pub_key| {
                        self.serialize_aes_packet()
                            .and_then(|aes_packet| Ok((aes_packet, pub_key)))
                    })
                    .and_then(|(aes_packet, pub_key)| {
                        self.encrypt_aes_packet(&aes_packet, &pub_key)
                    });

                match encrypted_aes_packet {
                    Ok(data) => {
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
                if let Err(e) = self.peer_decode_manager.decode(response) {
                    error!("error decoding packet: {}", e.to_string());
                    self.peer_decode_manager.delete_peer(&email);
                }
            }
            Err(_) => {}
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
