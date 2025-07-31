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

use super::hash_map_with_ordered_keys::HashMapWithOrderedKeys;
use super::peer_decoder::{PeerDecode, VideoPeerDecoder};
use super::{create_audio_peer_decoder, AudioPeerDecoderTrait, DecodeStatus};
use crate::crypto::aes::Aes128State;
use crate::diagnostics::DiagnosticManager;
use anyhow::Result;
use log::debug;
use protobuf::Message;
use std::rc::Rc;
use std::{fmt::Display, sync::Arc};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::JsValue;
use yew::prelude::Callback;

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
    SameUserPacket(String),
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
            PeerDecodeError::SameUserPacket(s) => write!(f, "SameUserPacket: {s}"),
        }
    }
}

pub struct Peer {
    pub audio: Box<dyn AudioPeerDecoderTrait>,
    pub video: VideoPeerDecoder,
    pub screen: VideoPeerDecoder,
    pub email: String,
    pub video_canvas_id: String,
    pub screen_canvas_id: String,
    pub aes: Option<Aes128State>,
    heartbeat_count: u8,
    pub video_enabled: bool,
    pub audio_enabled: bool,
    pub screen_enabled: bool,
}

use std::fmt::Debug;

impl Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Peer {{ email: {}, video_canvas_id: {}, screen_canvas_id: {} }}",
            self.email, self.video_canvas_id, self.screen_canvas_id
        )
    }
}

impl Peer {
    fn new(
        video_canvas_id: String,
        screen_canvas_id: String,
        email: String,
        aes: Option<Aes128State>,
    ) -> Result<Self, JsValue> {
        let (mut audio, video, screen) =
            Self::new_decoders(&video_canvas_id, &screen_canvas_id, &email)?;

        // Initialize with explicit mute state (audio_enabled starts as false, so muted=true)
        audio.set_muted(true);
        debug!("Initialized peer {email} with audio muted");

        Ok(Self {
            audio,
            video,
            screen,
            email,
            video_canvas_id,
            screen_canvas_id,
            aes,
            heartbeat_count: 1,
            video_enabled: false,
            audio_enabled: false,
            screen_enabled: false,
        })
    }

    fn new_decoders(
        video_canvas_id: &str,
        screen_canvas_id: &str,
        peer_id: &str,
    ) -> Result<
        (
            Box<dyn AudioPeerDecoderTrait>,
            VideoPeerDecoder,
            VideoPeerDecoder,
        ),
        JsValue,
    > {
        Ok((
            create_audio_peer_decoder(None, peer_id.to_string())?,
            VideoPeerDecoder::new(video_canvas_id)?,
            VideoPeerDecoder::new(screen_canvas_id)?,
        ))
    }

    fn reset(&mut self) -> Result<(), JsValue> {
        let (mut audio, video, screen) =
            Self::new_decoders(&self.video_canvas_id, &self.screen_canvas_id, &self.email)?;

        // Preserve the current mute state after reset
        audio.set_muted(!self.audio_enabled);
        debug!(
            "Reset peer {} with audio muted: {}",
            self.email, !self.audio_enabled
        );

        self.audio = audio;
        self.video = video;
        self.screen = screen;
        Ok(())
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
            MediaType::VIDEO => {
                let video_status = self
                    .video
                    .decode(&packet)
                    .map_err(|_| PeerDecodeError::VideoDecodeError)?;
                Ok((
                    media_type,
                    DecodeStatus {
                        rendered: video_status._rendered,
                        first_frame: video_status.first_frame,
                    },
                ))
            }
            MediaType::AUDIO => {
                if !self.audio_enabled {
                    // Peer is muted, don't send packet to NetEq to avoid expand packets (hissing sound)
                    debug!("Peer {} is muted, skipping audio packet", self.email);
                    Ok((
                        media_type,
                        DecodeStatus {
                            rendered: false,
                            first_frame: false,
                        },
                    ))
                } else {
                    Ok((
                        media_type,
                        self.audio
                            .decode(&packet)
                            .map_err(|_| PeerDecodeError::AudioDecodeError)?,
                    ))
                }
            }
            MediaType::SCREEN => {
                let screen_status = self
                    .screen
                    .decode(&packet)
                    .map_err(|_| PeerDecodeError::ScreenDecodeError)?;
                Ok((
                    media_type,
                    DecodeStatus {
                        rendered: screen_status._rendered,
                        first_frame: screen_status.first_frame,
                    },
                ))
            }
            MediaType::HEARTBEAT => {
                // update state using heartbeat metadata
                if let Some(metadata) = packet.heartbeat_metadata.as_ref() {
                    // Check if video is being turned off (on -> off transition)
                    let video_turned_off = self.video_enabled && !metadata.video_enabled;
                    // Check if audio is being turned off (on -> off transition)
                    let audio_turned_off = self.audio_enabled && !metadata.audio_enabled;
                    // Check if audio state changed at all
                    let audio_state_changed = self.audio_enabled != metadata.audio_enabled;

                    // Set mute state on audio decoder when audio state changes (before updating state)
                    if audio_state_changed {
                        log::info!("[MUTE DEBUG] Audio state changed for peer {} - audio_enabled: {} -> {}", 
                                                         self.email, self.audio_enabled, metadata.audio_enabled);
                        self.audio.set_muted(!metadata.audio_enabled);
                        debug!(
                            "Set audio decoder muted state for peer {} to {}",
                            self.email, !metadata.audio_enabled
                        );
                        log::info!(
                            "🔇 Setting peer {} muted to {}",
                            self.email,
                            !metadata.audio_enabled
                        );
                    }

                    self.video_enabled = metadata.video_enabled;
                    self.audio_enabled = metadata.audio_enabled;
                    self.screen_enabled = metadata.screen_enabled;

                    // Flush video decoder when video is turned off
                    if video_turned_off {
                        self.video.flush();
                        debug!(
                            "Flushed video decoder for peer {} (video turned off)",
                            self.email
                        );
                    }

                    // Flush audio decoder when audio is turned off to prevent expand packets
                    if audio_turned_off {
                        // For NetEq audio decoders, we need to flush the buffer to prevent hissing
                        self.audio.flush();
                        debug!(
                            "Flushed audio decoder for peer {} (audio turned off)",
                            self.email
                        );
                    }
                }
                Ok((
                    media_type,
                    DecodeStatus {
                        rendered: false,
                        first_frame: false,
                    },
                ))
            }
            MediaType::RTT => {
                // RTT packets are handled by ConnectionManager, not by peer decoders
                debug!(
                    "Received RTT packet for peer {} - ignoring in peer decoder",
                    self.email
                );
                Ok((
                    media_type,
                    DecodeStatus {
                        rendered: false,
                        first_frame: false,
                    },
                ))
            }
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
    diagnostics: Option<Rc<DiagnosticManager>>,
}

impl Default for PeerDecodeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerDecodeManager {
    pub fn new() -> Self {
        Self {
            connected_peers: HashMapWithOrderedKeys::new(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: None,
        }
    }

    pub fn new_with_diagnostics(diagnostics: Rc<DiagnosticManager>) -> Self {
        Self {
            connected_peers: HashMapWithOrderedKeys::new(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: Some(diagnostics),
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

    pub fn decode(&mut self, response: PacketWrapper, userid: &str) -> Result<(), PeerDecodeError> {
        let packet = Arc::new(response);
        let email = packet.email.clone();
        if let Some(peer) = self.connected_peers.get_mut(&email) {
            match peer.decode(&packet) {
                Ok((MediaType::HEARTBEAT, _)) => {
                    peer.on_heartbeat();
                    Ok(())
                }
                Ok((media_type, decode_status)) => {
                    if media_type != MediaType::RTT && packet.email == userid {
                        return Err(PeerDecodeError::SameUserPacket(email.clone()));
                    }
                    if let Some(diagnostics) = &self.diagnostics {
                        diagnostics.track_frame(&email, media_type, packet.data.len() as u64);
                    }

                    if decode_status.first_frame {
                        self.on_first_frame.emit((email.clone(), media_type));
                    }

                    Ok(())
                }
                Err(e) => peer.reset().map_err(|_| e),
            }
        } else {
            Err(PeerDecodeError::NoSuchPeer(email.clone()))
        }
    }

    fn add_peer(&mut self, email: &str, aes: Option<Aes128State>) -> Result<(), JsValue> {
        debug!("Adding peer {email}");
        self.connected_peers.insert(
            email.to_owned(),
            Peer::new(
                self.get_video_canvas_id.emit(email.to_owned()),
                self.get_screen_canvas_id.emit(email.to_owned()),
                email.to_owned(),
                aes,
            )?,
        );
        Ok(())
    }

    pub fn delete_peer(&mut self, email: &String) {
        self.connected_peers.remove(email);
    }

    pub fn ensure_peer(&mut self, email: &String) -> PeerStatus {
        if self.connected_peers.contains_key(email) {
            PeerStatus::NoChange
        } else if let Err(e) = self.add_peer(email, None) {
            log::error!("Error adding peer: {e:?}");
            PeerStatus::NoChange
        } else {
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

    pub fn get_fps(&self, _peer_id: &str, _media_type: MediaType) -> f64 {
        // FPS tracking is now handled by the DiagnosticManager internally
        // We return 0.0 here as we can't get real-time FPS immediately
        0.0
    }

    pub fn get_all_fps_stats(&self) -> Option<String> {
        None
    }

    /// Updates the speaker device for all connected peers by rebuilding their audio decoders
    pub fn update_speaker_device(
        &mut self,
        speaker_device_id: Option<String>,
    ) -> Result<(), JsValue> {
        let keys: Vec<String> = self.connected_peers.ordered_keys().clone();

        for key in keys {
            if let Some(peer) = self.connected_peers.get_mut(&key) {
                // Preserve current mute state
                let current_muted = !peer.audio_enabled;

                // Create a new audio decoder with the new speaker device
                match create_audio_peer_decoder(speaker_device_id.clone(), key.clone()) {
                    Ok(mut new_audio_decoder) => {
                        // Restore the mute state to the new decoder
                        new_audio_decoder.set_muted(current_muted);

                        // Replace the old decoder with the new one
                        peer.audio = new_audio_decoder;
                        log::info!("Successfully rebuilt audio decoder for peer: {key} with speaker device (muted: {current_muted})");
                    }
                    Err(e) => {
                        log::error!(
                            "Failed to rebuild audio decoder for peer: {key}, error: {e:?}"
                        );
                        // Keep the old decoder rather than breaking audio completely
                    }
                }
            }
        }
        Ok(())
    }
}
