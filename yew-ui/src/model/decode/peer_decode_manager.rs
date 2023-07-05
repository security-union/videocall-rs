use crate::model::MediaPacketWrapper;
use std::collections::HashMap;
use std::sync::Arc;
use types::protos::media_packet::media_packet::MediaType;
use types::protos::media_packet::MediaPacket;

use super::{AudioPeerDecoder, VideoPeerDecoder};

pub struct Callback<IN, OUT = ()> {
    func: Option<Box<dyn FnMut(IN) -> OUT + 'static>>,
}

impl<IN, OUT: std::default::Default> Callback<IN, OUT> {
    fn new() -> Self {
        Self { func: None }
    }

    pub fn set(&mut self, func: impl FnMut(IN) -> OUT + 'static) {
        self.func = Some(Box::new(func));
    }

    fn call(&mut self, arg: IN) -> OUT {
        match &mut self.func {
            Some(func) => func(arg),
            None => Default::default(),
        }
    }
}

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

    fn decode(&mut self, packet: &Arc<MediaPacket>) -> Result<Option<()>, i32> {
        match packet.media_type.enum_value()? {
            MediaType::VIDEO => Ok(self.video.decode(packet).ok()),
            MediaType::AUDIO => Ok(self.audio.decode(packet).ok()),
            MediaType::SCREEN => Ok(self.screen.decode(packet).ok()),
            MediaType::HEARTBEAT => Ok(Some(())),
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
            on_peer_added: Callback::new(),
            on_first_frame: Callback::new(),
            get_video_canvas_id: Callback::new(),
            get_screen_canvas_id: Callback::new(),
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
        if let None = peer.decode(&packet)? {
            self.reset_peer(&email);
        }
        Ok(())
    }

    fn add_peer(&mut self, email: &String) {
        self.insert_peer(&email);
        self.on_peer_added.call(email.clone())
    }

    fn insert_peer(&mut self, email: &String) {
        self.connected_peers.insert(
            email.clone(),
            MultiDecoder::new(
                self.get_video_canvas_id.call(email.clone()),
                self.get_screen_canvas_id.call(email.clone()),
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
