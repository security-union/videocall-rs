use super::hash_map_with_ordered_keys::HashMapWithOrderedKeys;
use crate::diagnostics;
use log::debug;
use protobuf::Message;
use std::{fmt::Display, sync::Arc};
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::{
    media_packet::media_packet::MediaType, packet_wrapper::PacketWrapper,
};
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
    NoMediaType,
    NoPacketType,
    PacketParseError,
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
            PeerDecodeError::NoMediaType => write!(f, "No media_type"),
            PeerDecodeError::NoPacketType => write!(f, "No packet_type"),
            PeerDecodeError::PacketParseError => {
                write!(f, "Failed to parse to protobuf MediaPacket")
            }
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
    heartbeat_count: u8,
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
            heartbeat_count: 1,
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
            .map_err(|_| PeerDecodeError::NoPacketType)?
            != PacketType::MEDIA
        {
            return Err(PeerDecodeError::IncorrectPacketType);
        }

        let packet = match self.aes {
            Some(aes) => {
                let data = aes
                    .decrypt(&packet.data)
                    .map_err(|_| PeerDecodeError::AesDecryptError)?;
                parse_media_packet(&data)?
            }
            None => parse_media_packet(&packet.data)?,
        };

        let media_type = packet
            .media_type
            .enum_value()
            .map_err(|_| PeerDecodeError::NoMediaType)?;
        match media_type {
            MediaType::VIDEO => Ok((
                media_type,
                self.video
                    .decode(&packet)
                    .map_err(|_| PeerDecodeError::VideoDecodeError)?,
            )),
            MediaType::AUDIO => Ok((
                media_type,
                self.audio
                    .decode(&packet)
                    .map_err(|_| PeerDecodeError::AudioDecodeError)?,
            )),
            MediaType::SCREEN => Ok((
                media_type,
                self.screen
                    .decode(&packet)
                    .map_err(|_| PeerDecodeError::ScreenDecodeError)?,
            )),
            MediaType::HEARTBEAT => Ok((
                media_type,
                DecodeStatus {
                    _rendered: false,
                    first_frame: false,
                },
            )),
        }
    }

    fn on_heartbeat(&mut self) {
        self.heartbeat_count += 1;
    }

    pub fn check_heartbeat(&mut self) -> bool {
        if self.heartbeat_count != 0 {
            self.heartbeat_count = 0;
            return true;
        }
        debug!(
            "---@@@--- detected heartbeat stop for {}",
            self.email.clone()
        );
        false
    }
}

fn parse_media_packet(data: &[u8]) -> Result<Arc<MediaPacket>, PeerDecodeError> {
    Ok(Arc::new(
        MediaPacket::parse_from_bytes(data).map_err(|_| PeerDecodeError::PacketParseError)?,
    ))
}

#[derive(Debug)]
pub struct PeerDecodeManager {
    connected_peers: HashMapWithOrderedKeys<String, Peer>,
    pub on_first_frame: Callback<(String, MediaType)>,
    pub get_video_canvas_id: Callback<String, String>,
    pub get_screen_canvas_id: Callback<String, String>,
}

impl PeerDecodeManager {
    pub fn new() -> Self {
        Self {
            connected_peers: HashMapWithOrderedKeys::new(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
        }
    }

    pub fn sorted_keys(&self) -> &Vec<String> {
        self.connected_peers.ordered_keys()
    }

    pub fn get(&self, key: &String) -> Option<&Peer> {
        self.connected_peers.get(key)
    }

    pub fn run_peer_monitor(&mut self) {
        let pred = |peer: &mut Peer| peer.check_heartbeat();
        self.connected_peers.remove_if(pred);
    }

    pub fn decode(&mut self, response: PacketWrapper) -> Result<(), PeerDecodeError> {
        let packet = Arc::new(response);
        let email = packet.email.clone();
        if let Some(peer) = self.connected_peers.get_mut(&email) {
            match peer.decode(&packet) {
                Ok((MediaType::HEARTBEAT, _)) => {
                    peer.on_heartbeat();
                    Ok(())
                }
                Ok((media_type, decode_status)) => {
                    // Record diagnostics information
                    // Record packet size for all media types
                    diagnostics::record_packet(&email, packet.data.len());
                    
                    // For video/screen, get dimensions from the decoded frame
                    // Since VideoMetadata doesn't have width/height, we'll use default values for now
                    if media_type == MediaType::VIDEO || media_type == MediaType::SCREEN {
                        // Use constants or default values since we don't have access to actual frame dimensions
                        let default_width = 640;  // Default HD width
                        let default_height = 480; // Default HD height
                        
                        diagnostics::record_video_frame(
                            &email,
                            default_width,
                            default_height,
                        );
                    }
                    
                    if decode_status.first_frame {
                        self.on_first_frame.emit((email.clone(), media_type));
                    }
                    Ok(())
                }
                Err(e) => {
                    // Record packet loss diagnostics
                    diagnostics::record_packet_lost(&email);
                    
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
    }

    pub fn delete_peer(&mut self, email: &String) {
        self.connected_peers.remove(email);
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
