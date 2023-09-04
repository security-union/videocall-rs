use log::debug;
use protobuf::Message;
use std::collections::HashMap;
use std::{fmt::Display, sync::Arc};
use types::protos::media_packet::MediaPacket;
use types::protos::packet_wrapper::packet_wrapper::PacketType;
use types::protos::{media_packet::media_packet::MediaType, packet_wrapper::PacketWrapper};
use yew::prelude::Callback;

use crate::crypto::aes::Aes128State;

use super::peer_decoder::{AudioPeerDecoder, DecodeStatus, PeerDecode, VideoPeerDecoder};

#[derive(Debug)]
pub enum PeerDecodeError {
    AesDecryptError,
    IncorrectPacketType,
    AudioDecodeError,
    ScreenDecodeError,
    VideoDecodeError,
    NoSuchPeer(String),
    Other(String),
}

#[derive(Debug)]
pub enum PeerStatus {
    Added(String),
    NoChange,
}

impl Display for PeerDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerDecodeError::AesDecryptError => write!(f, "AesDecryptError"),
            PeerDecodeError::IncorrectPacketType => write!(f, "IncorrectPacketType"),
            PeerDecodeError::AudioDecodeError => write!(f, "AudioDecodeError"),
            PeerDecodeError::ScreenDecodeError => write!(f, "ScreenDecodeError"),
            PeerDecodeError::VideoDecodeError => write!(f, "VideoDecodeError"),
            PeerDecodeError::NoSuchPeer(s) => write!(f, "Peer Not Found: {s}"),
            PeerDecodeError::Other(s) => write!(f, "Other: {s}"),
        }
    }
}

#[derive(Debug)]
pub struct Peer {
    pub audio: AudioPeerDecoder,
    pub video: VideoPeerDecoder,
    pub screen: VideoPeerDecoder,
    pub email: String,
    pub video_canvas_id: String,
    pub screen_canvas_id: String,
    pub aes: Option<Aes128State>,
}

impl Peer {
    fn new(
        video_canvas_id: String,
        screen_canvas_id: String,
        email: String,
        aes: Option<Aes128State>,
    ) -> Self {
        let (audio, video, screen) = Self::new_decoders(&video_canvas_id, &screen_canvas_id);
        Self {
            audio,
            video,
            screen,
            email,
            video_canvas_id,
            screen_canvas_id,
            aes,
        }
    }

    fn new_decoders(
        video_canvas_id: &str,
        screen_canvas_id: &str,
    ) -> (AudioPeerDecoder, VideoPeerDecoder, VideoPeerDecoder) {
        (
            AudioPeerDecoder::new(),
            VideoPeerDecoder::new(video_canvas_id),
            VideoPeerDecoder::new(screen_canvas_id),
        )
    }

    fn reset(&mut self) {
        let (audio, video, screen) =
            Self::new_decoders(&self.video_canvas_id, &self.screen_canvas_id);
        self.audio = audio;
        self.video = video;
        self.screen = screen;
    }

    fn decode(
        &mut self,
        packet: &Arc<PacketWrapper>,
    ) -> Result<(MediaType, DecodeStatus), PeerDecodeError> {
        if packet
            .packet_type
            .enum_value()
            .map_err(|_e| PeerDecodeError::Other(String::from("No packet_type")))?
            != PacketType::MEDIA
        {
            return Err(PeerDecodeError::IncorrectPacketType);
        }

        let packet = {
            if let Some(aes) = self.aes {
                let data = aes
                    .decrypt(&packet.data)
                    .map_err(|_e| PeerDecodeError::AesDecryptError)?;
                Arc::new(MediaPacket::parse_from_bytes(&data).map_err(|_e| {
                    PeerDecodeError::Other(String::from("Failed to parse to protobuf MediaPacket"))
                })?)
            } else {
                Arc::new(MediaPacket::parse_from_bytes(&packet.data).map_err(|_e| {
                    PeerDecodeError::Other(String::from("Failed to parse to protobuf MediaPacket"))
                })?)
            }
        };

        let media_type = packet
            .media_type
            .enum_value()
            .map_err(|_e| PeerDecodeError::Other(String::from("No media_type")))?;
        match media_type {
            MediaType::VIDEO => Ok((
                media_type,
                self.video
                    .decode(&packet)
                    .map_err(|_e| PeerDecodeError::VideoDecodeError)?,
            )),
            MediaType::AUDIO => Ok((
                media_type,
                self.audio
                    .decode(&packet)
                    .map_err(|_e| PeerDecodeError::AudioDecodeError)?,
            )),
            MediaType::SCREEN => Ok((
                media_type,
                self.screen
                    .decode(&packet)
                    .map_err(|_e| PeerDecodeError::ScreenDecodeError)?,
            )),
            MediaType::HEARTBEAT => Ok((
                media_type,
                DecodeStatus {
                    rendered: false,
                    first_frame: false,
                },
            )),
        }
    }
}

#[derive(Debug)]
pub struct PeerDecodeManager {
    connected_peers: HashMap<String, Peer>,
    sorted_connected_peers_keys: Vec<String>,
    pub on_first_frame: Callback<(String, MediaType)>,
    pub get_video_canvas_id: Callback<String, String>,
    pub get_screen_canvas_id: Callback<String, String>,
}

impl PeerDecodeManager {
    pub fn new() -> Self {
        Self {
            connected_peers: HashMap::new(),
            sorted_connected_peers_keys: vec![],
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
        }
    }

    pub fn sorted_keys(&self) -> &Vec<String> {
        &self.sorted_connected_peers_keys
    }

    pub fn get(&self, key: &String) -> Option<&Peer> {
        self.connected_peers.get(key)
    }

    pub fn decode(&mut self, response: PacketWrapper) -> Result<(), PeerDecodeError> {
        let packet = Arc::new(response);
        let email = packet.email.clone();
        if let Some(peer) = self.connected_peers.get_mut(&email) {
            match peer.decode(&packet) {
                Ok((media_type, decode_status)) => {
                    if decode_status.first_frame {
                        self.on_first_frame.emit((email.clone(), media_type));
                    }
                    Ok(())
                }
                Err(e) => {
                    peer.reset();
                    Err(e)
                }
            }
        } else {
            Err(PeerDecodeError::NoSuchPeer(email.clone()))
        }
    }

    fn add_peer(&mut self, email: &str, aes: Option<Aes128State>) {
        debug!("Adding peer {}", email);
        self.connected_peers.insert(
            email.to_owned(),
            Peer::new(
                self.get_video_canvas_id.emit(email.to_owned()),
                self.get_screen_canvas_id.emit(email.to_owned()),
                email.to_owned(),
                aes,
            ),
        );
        self.sorted_connected_peers_keys.push(email.to_owned());
        self.sorted_connected_peers_keys.sort();
    }

    pub fn delete_peer(&mut self, email: &String) {
        self.connected_peers.remove(email);
        if let Ok(index) = self.sorted_connected_peers_keys.binary_search(email) {
            self.sorted_connected_peers_keys.remove(index);
        }
    }

    pub fn ensure_peer(&mut self, email: &String) -> PeerStatus {
        if self.connected_peers.contains_key(email) {
            PeerStatus::NoChange
        } else {
            self.add_peer(email, None);
            PeerStatus::Added(email.clone())
        }
    }

    pub fn set_peer_aes(
        &mut self,
        email: &String,
        aes: Aes128State,
    ) -> Result<(), PeerDecodeError> {
        match self.connected_peers.get_mut(email) {
            Some(peer) => {
                peer.aes = Some(aes);
                Ok(())
            }
            None => Err(PeerDecodeError::NoSuchPeer(email.clone())),
        }
    }
}
