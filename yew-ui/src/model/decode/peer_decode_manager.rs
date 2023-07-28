use anyhow::anyhow;
use gloo_console::log;
use protobuf::Message;
use std::collections::HashMap;
use std::sync::Arc;
use types::protos::media_packet::MediaPacket;
use types::protos::packet_wrapper::packet_wrapper::PacketType;
use types::protos::{media_packet::media_packet::MediaType, packet_wrapper::PacketWrapper};
use yew::prelude::Callback;

use crate::crypto::aes::Aes128State;

use super::peer_decoder::{AudioPeerDecoder, DecodeStatus, PeerDecode, VideoPeerDecoder};

pub struct MultiDecoder {
    pub audio: AudioPeerDecoder,
    pub video: VideoPeerDecoder,
    pub screen: VideoPeerDecoder,
    pub email: String,
    pub aes: Aes128State,
}

impl MultiDecoder {
    fn new(
        video_canvas_id: String,
        screen_canvas_id: String,
        email: String,
        aes: Aes128State,
    ) -> Self {
        Self {
            audio: AudioPeerDecoder::new(),
            video: VideoPeerDecoder::new(&video_canvas_id),
            screen: VideoPeerDecoder::new(&screen_canvas_id),
            email,
            aes,
        }
    }

    // Note: arbitrarily using error code 0 for decoder failure, since it doesn't provide any error value
    fn decode(&mut self, packet: &Arc<PacketWrapper>) -> anyhow::Result<(MediaType, DecodeStatus)> {
        if packet
            .packet_type
            .enum_value()
            .map_err(|_e| anyhow!("No packet_type"))?
            != PacketType::MEDIA
        {
            return Err(anyhow!("Incorrect packet type"));
        }
        log!("Decoding media packet for email ", &self.email);
        let packet = self
            .aes
            .decrypt(&packet.data)
            .map_err(|_e| anyhow!("Failed to decrypt with aes"))?;
        let packet = Arc::new(
            MediaPacket::parse_from_bytes(&packet)
                .map_err(|_e| anyhow!("Failed to parse to protobuf MediaPacket"))?,
        );
        let media_type = packet
            .media_type
            .enum_value()
            .map_err(|_e| anyhow!("No media_type"))?;
        match media_type {
            MediaType::VIDEO => Ok((
                media_type,
                self.video
                    .decode(&packet)
                    .map_err(|_e| anyhow!("Failed to decode video"))?,
            )),
            MediaType::AUDIO => Ok((
                media_type,
                self.audio
                    .decode(&packet)
                    .map_err(|_e| anyhow!("Failed to decode audio"))?,
            )),
            MediaType::SCREEN => Ok((
                media_type,
                self.screen
                    .decode(&packet)
                    .map_err(|_e| anyhow!("Failed to decode screen"))?,
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

    pub fn decode(&mut self, response: PacketWrapper, aes: &Aes128State) -> anyhow::Result<()> {
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
            Err(anyhow!("No peer found"))
        }
    }

    fn add_peer(&mut self, email: &String, aes: &Aes128State) {
        self.insert_peer(email, aes);
        self.on_peer_added.emit(email.clone())
    }

    fn insert_peer(&mut self, email: &String, aes: &Aes128State) {
        self.connected_peers.insert(
            email.clone(),
            MultiDecoder::new(
                self.get_video_canvas_id.emit(email.clone()),
                self.get_screen_canvas_id.emit(email.clone()),
                email.clone(),
                *aes,
            ),
        );
        self.sorted_connected_peers_keys.push(email.clone());
        self.sorted_connected_peers_keys.sort();
    }

    fn delete_peer(&mut self, email: &String) {
        self.connected_peers.remove(email);
        if let Ok(index) = self.sorted_connected_peers_keys.binary_search(email) {
            self.sorted_connected_peers_keys.remove(index);
        }
    }

    fn reset_peer(&mut self, email: &String, aes: &Aes128State) {
        self.delete_peer(email);
        self.insert_peer(email, aes);
    }
}
