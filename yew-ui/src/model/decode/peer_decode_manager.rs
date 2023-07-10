use crate::model::MediaPacketWrapper;
use std::collections::HashMap;
use std::sync::Arc;
use types::protos::media_packet::media_packet::MediaType;
use types::protos::media_packet::MediaPacket;
use yew::prelude::Callback;

use super::{AudioPeerDecoder, VideoPeerDecoder};

pub struct MultiDecoder {
    pub audio: AudioPeerDecoder,
    pub video: VideoPeerDecoder,
    pub screen: VideoPeerDecoder,
}

impl MultiDecoder {
    fn new(video_canvas_id: String, screen_canvas_id: String) -> Self {
        Self {
            audio: AudioPeerDecoder::new(),
            video: VideoPeerDecoder::new(&video_canvas_id),
            screen: VideoPeerDecoder::new(&screen_canvas_id),
        }
    }

    /// Result is:
    ///     Ok(media_type, Some(true)) on first frame,
    ///     Ok(media_type, Some(false)) on other frames
    ///     Ok(media_type, None) on failure to render
    ///     Err(e) problem with the packet
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<(MediaType, Option<bool>), i32> {
        let media_type = packet.media_type.enum_value()?;
        match media_type {
            MediaType::VIDEO => Ok((media_type, self.video.decode(packet).ok())),
            MediaType::AUDIO => Ok((media_type, self.audio.decode(packet).ok())),
            MediaType::SCREEN => Ok((media_type, self.screen.decode(packet).ok())),
            MediaType::HEARTBEAT => Ok((media_type, Some(false))),
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

    pub fn decode(&mut self, response: MediaPacketWrapper) -> Result<(), i32> {
        let packet = Arc::new(response.0);
        let email = packet.email.clone();
        if !self.connected_peers.contains_key(&email) {
            self.add_peer(&email);
        }
        let peer = self.connected_peers.get_mut(&email).unwrap();
        let (media_type, decoded) = peer.decode(&packet)?;
        match decoded {
            Some(first_frame) => {
                if first_frame {
                    self.on_first_frame.emit((email.clone(), media_type));
                }
            }
            None => {
                self.reset_peer(&email);
            }
        }
        Ok(())
    }

    fn add_peer(&mut self, email: &String) {
        self.insert_peer(&email);
        self.on_peer_added.emit(email.clone())
    }

    fn insert_peer(&mut self, email: &String) {
        self.connected_peers.insert(
            email.clone(),
            MultiDecoder::new(
                self.get_video_canvas_id.emit(email.clone()),
                self.get_screen_canvas_id.emit(email.clone()),
            ),
        );
        self.sorted_connected_peers_keys.push(email.clone());
        self.sorted_connected_peers_keys.sort();
    }

    fn delete_peer(&mut self, email: &String) {
        self.connected_peers.remove(email);
        if let Ok(index) = self.sorted_connected_peers_keys.binary_search(&email) {
            self.sorted_connected_peers_keys.remove(index);
        }
        self.insert_peer(&email);
    }

    fn reset_peer(&mut self, email: &String) {
        self.delete_peer(&email);
        self.insert_peer(&email);
    }
}
