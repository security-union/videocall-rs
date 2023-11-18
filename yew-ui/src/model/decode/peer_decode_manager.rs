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
pub enum MultiDecoderError {
    AesDecryptError,
    IncorrectPacketType,
    AudioDecodeError,
    ScreenDecodeError,
    VideoDecodeError,
    Other(String),
}

impl Display for MultiDecoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MultiDecoderError::AesDecryptError => write!(f, "AesDecryptError"),
            MultiDecoderError::IncorrectPacketType => write!(f, "IncorrectPacketType"),
            MultiDecoderError::AudioDecodeError => write!(f, "AudioDecodeError"),
            MultiDecoderError::ScreenDecodeError => write!(f, "ScreenDecodeError"),
            MultiDecoderError::VideoDecodeError => write!(f, "VideoDecodeError"),
            MultiDecoderError::Other(s) => write!(f, "Other: {s}"),
        }
    }
}

pub struct MultiDecoder {
    pub audio: AudioPeerDecoder,
    pub video: VideoPeerDecoder,
    pub screen: VideoPeerDecoder,
    pub email: String,
    pub aes: Option<Aes128State>,
}

impl MultiDecoder {
    fn new(
        video_canvas_id: String,
        screen_canvas_id: String,
        email: String,
        aes: Option<Aes128State>,
    ) -> Self {
        Self {
            audio: AudioPeerDecoder::new(),
            video: VideoPeerDecoder::new(&video_canvas_id),
            screen: VideoPeerDecoder::new(&screen_canvas_id),
            email,
            aes,
        }
    }

    fn decode(
        &mut self,
        packet: &Arc<PacketWrapper>,
    ) -> Result<(MediaType, DecodeStatus), MultiDecoderError> {
        if packet
            .packet_type
            .enum_value()
            .map_err(|_e| MultiDecoderError::Other(String::from("No packet_type")))?
            != PacketType::MEDIA
        {
            return Err(MultiDecoderError::IncorrectPacketType);
        }

        let packet = {
            if let Some(aes) = self.aes {
                let data = aes
                    .decrypt(&packet.data)
                    .map_err(|_e| MultiDecoderError::AesDecryptError)?;
                Arc::new(MediaPacket::parse_from_bytes(&data).map_err(|_e| {
                    MultiDecoderError::Other(String::from(
                        "Failed to parse to protobuf MediaPacket",
                    ))
                })?)
            } else {
                Arc::new(MediaPacket::parse_from_bytes(&packet.data).map_err(|_e| {
                    MultiDecoderError::Other(String::from(
                        "Failed to parse to protobuf MediaPacket",
                    ))
                })?)
            }
        };

        let media_type = packet
            .media_type
            .enum_value()
            .map_err(|_e| MultiDecoderError::Other(String::from("No media_type")))?;
        match media_type {
            MediaType::VIDEO => Ok((
                media_type,
                self.video
                    .decode(&packet)
                    .map_err(|_e| MultiDecoderError::VideoDecodeError)?,
            )),
            MediaType::AUDIO => Ok((
                media_type,
                self.audio
                    .decode(&packet)
                    .map_err(|_e| MultiDecoderError::AudioDecodeError)?,
            )),
            MediaType::SCREEN => Ok((
                media_type,
                self.screen
                    .decode(&packet)
                    .map_err(|_e| MultiDecoderError::ScreenDecodeError)?,
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

pub struct PeerDecodeManager {
    connected_peers: HashMap<String, MultiDecoder>,
    sorted_connected_peers_keys: Vec<String>,
    pub on_peer_added: Callback<String>,
    pub on_first_frame: Callback<(String, MediaType)>,
    pub get_video_canvas_id: Callback<String, String>,
    pub get_screen_canvas_id: Callback<String, String>,
}

impl PeerDecodeManager {
    pub fn new() -> Self {
        Self {
            connected_peers: HashMap::new(),
            sorted_connected_peers_keys: vec![],
            on_peer_added: Callback::noop(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
        }
    }

    pub fn sorted_keys(&self) -> &Vec<String> {
        &self.sorted_connected_peers_keys
    }

    pub fn get(&self, key: &String) -> Option<&MultiDecoder> {
        self.connected_peers.get(key)
    }

    pub fn decode(
        &mut self,
        response: PacketWrapper,
        aes: Option<Aes128State>,
    ) -> Result<(), MultiDecoderError> {
        let packet = Arc::new(response);
        let email = packet.email.clone();
        if !self.connected_peers.contains_key(&email) {
            self.add_peer(&email, aes);
        }
        if let Some(peer) = self.connected_peers.get_mut(&email) {
            match peer.decode(&packet) {
                Ok((media_type, decode_status)) => {
                    if decode_status.first_frame {
                        self.on_first_frame.emit((email.clone(), media_type));
                    }
                    Ok(())
                }
                Err(e) => {
                    self.reset_peer(&email, aes);
                    Err(e)
                }
            }
        } else {
            Err(MultiDecoderError::Other(String::from("No peer found")))
        }
    }

    fn add_peer(&mut self, email: &str, aes: Option<Aes128State>) {
        debug!("Adding peer {}", email);
        self.insert_peer(email, aes);
        self.on_peer_added.emit(email.to_owned())
    }

    fn insert_peer(&mut self, email: &str, aes: Option<Aes128State>) {
        self.connected_peers.insert(
            email.to_owned(),
            MultiDecoder::new(
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

    fn reset_peer(&mut self, email: &String, aes: Option<Aes128State>) {
        self.delete_peer(email);
        self.insert_peer(email, aes);
    }
}
