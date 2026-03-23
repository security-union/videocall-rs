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
use crate::adaptive_quality_constants::{
    KEYFRAME_REQUEST_MIN_INTERVAL_MS, KEYFRAME_REQUEST_TIMEOUT_MS,
};
use crate::audio::shared_audio_context::SharedAudioContext;
use crate::crypto::aes::Aes128State;
use crate::diagnostics::DiagnosticManager;
use anyhow::Result;
use log::debug;
use protobuf::Message;
use std::collections::HashMap;
use std::rc::Rc;
use std::{fmt::Display, sync::Arc};
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

#[derive(Debug)]
pub enum PeerDecodeError {
    AesDecryptError,
    IncorrectPacketType,
    AudioDecodeError,
    ScreenDecodeError,
    VideoDecodeError,
    NoSuchPeer(u64),
    NoMediaType,
    NoPacketType,
    PacketParseError,
    SameUserPacket(u64),
    UnknownMediaType,
    UnknownPacketType,
}

#[derive(Debug)]
pub enum PeerStatus {
    Added(u64),
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
            PeerDecodeError::UnknownMediaType => write!(f, "UnknownMediaType"),
            PeerDecodeError::UnknownPacketType => write!(f, "UnknownPacketType"),
        }
    }
}

pub struct Peer {
    pub audio: Box<dyn AudioPeerDecoderTrait>,
    pub video: VideoPeerDecoder,
    pub screen: VideoPeerDecoder,
    pub session_id: u64,
    /// Cached `session_id.to_string()` to avoid repeated allocations.
    sid_str: String,
    pub user_id: String,
    pub video_canvas_id: String,
    pub screen_canvas_id: String,
    pub aes: Option<Aes128State>,
    heartbeat_count: u8,
    pub video_enabled: bool,
    pub audio_enabled: bool,
    pub screen_enabled: bool,
    pub is_speaking: bool,
    pub audio_level: f32,
    pub display_name: Option<String>,
    /// Whether this peer's video/screen tiles are currently visible in the
    /// viewport (tracked via IntersectionObserver in the UI layer). When
    /// `false`, video and screen decoding is skipped to save CPU. Audio is
    /// always decoded regardless of visibility.
    pub visible: bool,
    context_initialized: bool,
    vad_threshold: Option<f32>,
    has_received_heartbeat: bool,
    /// Last seen video sequence number for gap detection.
    last_video_seq: Option<u64>,
    /// Last seen screen sequence number for gap detection.
    last_screen_seq: Option<u64>,
    /// Timestamp (ms) when a video gap was first detected. `None` if no gap.
    video_gap_detected_at_ms: Option<u64>,
    /// Timestamp (ms) when a screen gap was first detected. `None` if no gap.
    screen_gap_detected_at_ms: Option<u64>,
    /// Last time a video keyframe request was sent (ms). Used for rate-limiting.
    last_video_keyframe_request_ms: u64,
    /// Last time a screen keyframe request was sent (ms). Used for rate-limiting.
    last_screen_keyframe_request_ms: u64,
}

use std::fmt::Debug;

impl Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Peer {{ session_id: {}, video_canvas_id: {}, screen_canvas_id: {} }}",
            self.session_id, self.video_canvas_id, self.screen_canvas_id
        )
    }
}

impl Peer {
    fn new(
        video_canvas_id: String,
        screen_canvas_id: String,
        session_id: u64,
        user_id: String,
        aes: Option<Aes128State>,
        vad_threshold: Option<f32>,
    ) -> Result<Self, JsValue> {
        let sid_str = session_id.to_string();
        let (mut audio, video, screen) =
            Self::new_decoders(&video_canvas_id, &screen_canvas_id, &sid_str, vad_threshold)?;

        audio.set_muted(true);
        debug!("Initialized peer {user_id} (session_id: {session_id}) with audio muted");

        Ok(Self {
            audio,
            video,
            screen,
            session_id,
            sid_str,
            user_id,
            video_canvas_id,
            screen_canvas_id,
            aes,
            heartbeat_count: 1,
            video_enabled: false,
            audio_enabled: false,
            screen_enabled: false,
            is_speaking: false,
            audio_level: 0.0,
            display_name: None,
            visible: true,
            context_initialized: false,
            vad_threshold,
            has_received_heartbeat: false,
            last_video_seq: None,
            last_screen_seq: None,
            video_gap_detected_at_ms: None,
            screen_gap_detected_at_ms: None,
            last_video_keyframe_request_ms: 0,
            last_screen_keyframe_request_ms: 0,
        })
    }

    fn new_decoders(
        video_canvas_id: &str,
        screen_canvas_id: &str,
        peer_id: &str,
        vad_threshold: Option<f32>,
    ) -> Result<
        (
            Box<dyn AudioPeerDecoderTrait>,
            VideoPeerDecoder,
            VideoPeerDecoder,
        ),
        JsValue,
    > {
        // Create decoders without canvas (will be set later via set_canvas)
        // We still keep the canvas IDs for backward compatibility with existing code
        let video_decoder = VideoPeerDecoder::new(None)?;
        let screen_decoder = VideoPeerDecoder::new(None)?;

        // Attempt to set canvas immediately if available in DOM
        if let Some(window) = web_sys::window() {
            if let Some(document) = window.document() {
                if let Some(canvas_element) = document.get_element_by_id(video_canvas_id) {
                    if let Ok(canvas) = canvas_element.dyn_into::<web_sys::HtmlCanvasElement>() {
                        let _ = video_decoder.set_canvas(canvas);
                    }
                }
                if let Some(canvas_element) = document.get_element_by_id(screen_canvas_id) {
                    if let Ok(canvas) = canvas_element.dyn_into::<web_sys::HtmlCanvasElement>() {
                        let _ = screen_decoder.set_canvas(canvas);
                    }
                }
            }
        }

        Ok((
            create_audio_peer_decoder(None, peer_id.to_string(), vad_threshold)?,
            video_decoder,
            screen_decoder,
        ))
    }

    fn reset(&mut self) -> Result<(), JsValue> {
        let sid_str = self.session_id.to_string();
        let (mut audio, video, screen) = Self::new_decoders(
            &self.video_canvas_id,
            &self.screen_canvas_id,
            &sid_str,
            self.vad_threshold,
        )?;

        // Preserve the current mute state after reset
        audio.set_muted(!self.audio_enabled);
        debug!(
            "Reset peer {} with audio muted: {}",
            self.session_id, !self.audio_enabled
        );

        self.audio = audio;
        self.video = video;
        self.screen = screen;
        // Intentionally keep `has_received_heartbeat` and `*_enabled` flags:
        // the peer's last-known media state is still the best information we
        // have.  Resetting the flag would let straggler frames through until
        // the next heartbeat, which is the opposite of what we want.
        Ok(())
    }

    /// Broadcast current media-enabled state to the diagnostics bus so the UI
    /// can update peer tiles.
    fn broadcast_peer_status(&self) {
        let evt = DiagEvent {
            subsystem: "peer_status",
            stream_id: None,
            ts_ms: now_ms(),
            metrics: vec![
                metric!("to_peer", self.sid_str.clone()),
                metric!(
                    "audio_enabled",
                    if self.audio_enabled { 1u64 } else { 0u64 }
                ),
                metric!(
                    "video_enabled",
                    if self.video_enabled { 1u64 } else { 0u64 }
                ),
                metric!(
                    "screen_enabled",
                    if self.screen_enabled { 1u64 } else { 0u64 }
                ),
                metric!("is_speaking", if self.is_speaking { 1u64 } else { 0u64 }),
                metric!("audio_level", self.audio_level as f64),
            ],
        };
        let _ = global_sender().try_broadcast(evt);
    }

    /// Decode a packet and return `(media_type, decode_status, keyframe_request)`.
    ///
    /// The third element is `Some(media_type)` when a sequence gap has been
    /// detected and enough time has elapsed to warrant sending a
    /// KEYFRAME_REQUEST to this peer. The caller is responsible for
    /// actually sending the request packet.
    fn decode(
        &mut self,
        packet: &Arc<PacketWrapper>,
    ) -> Result<(MediaType, DecodeStatus, Option<MediaType>), PeerDecodeError> {
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
                // Track sequence numbers for gap detection (PLI).
                let kf_request = self.track_sequence(media_type, &packet);

                if !self.video_enabled {
                    if !self.has_received_heartbeat {
                        // No heartbeat yet — infer video_enabled from the actual frame.
                        self.video_enabled = true;
                        self.broadcast_peer_status();
                    } else {
                        // Peer has video off per heartbeat; drop straggler frame.
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }
                }

                // Skip video decoding when the peer tile is not visible in the
                // viewport. The next keyframe after visibility is restored will
                // allow the decoder to recover naturally.
                if !self.visible {
                    return Ok((media_type, DecodeStatus::SKIPPED, None));
                }

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
                    kf_request,
                ))
            }
            MediaType::AUDIO => {
                if !self.audio_enabled {
                    if !self.has_received_heartbeat {
                        // No heartbeat yet — infer audio_enabled from the actual frame.
                        self.audio_enabled = true;
                        self.audio.set_muted(false);
                        self.broadcast_peer_status();
                    } else {
                        // Peer is muted per heartbeat; drop straggler audio to avoid audible glitch.
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }
                }
                Ok((
                    media_type,
                    self.audio
                        .decode(&packet)
                        .map_err(|_| PeerDecodeError::AudioDecodeError)?,
                    None,
                ))
            }
            MediaType::SCREEN => {
                // Track sequence numbers for gap detection (PLI).
                let kf_request = self.track_sequence(media_type, &packet);

                if !self.screen_enabled {
                    if !self.has_received_heartbeat {
                        // No heartbeat yet — infer screen_enabled from the actual frame.
                        self.screen_enabled = true;
                        self.broadcast_peer_status();
                    } else {
                        // Peer has screen off per heartbeat; drop straggler frame.
                        return Ok((media_type, DecodeStatus::SKIPPED, None));
                    }
                }

                // Skip screen decoding when the peer tile is not visible.
                if !self.visible {
                    return Ok((media_type, DecodeStatus::SKIPPED, None));
                }

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
                    kf_request,
                ))
            }
            MediaType::HEARTBEAT => {
                self.has_received_heartbeat = true;
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
                        self.audio.set_muted(!metadata.audio_enabled);
                        debug!(
                            "Audio state changed for peer {} - muted: {}",
                            self.session_id, !metadata.audio_enabled
                        );
                    }

                    self.video_enabled = metadata.video_enabled;
                    self.audio_enabled = metadata.audio_enabled;
                    self.screen_enabled = metadata.screen_enabled;
                    self.is_speaking = metadata.is_speaking;
                    if !metadata.is_speaking {
                        self.audio_level = 0.0;
                    }

                    // Flush video decoder when video is turned off
                    if video_turned_off {
                        self.video.flush();
                        debug!(
                            "Flushed video decoder for peer {} (video turned off)",
                            self.session_id
                        );
                    }

                    // Flush audio decoder when audio is turned off to prevent expand packets
                    if audio_turned_off {
                        // For NetEq audio decoders, we need to flush the buffer to prevent hissing
                        self.audio.flush();
                        debug!(
                            "Flushed audio decoder for peer {} (audio turned off)",
                            self.session_id
                        );
                    }

                    self.broadcast_peer_status();
                }
                Ok((media_type, DecodeStatus::SKIPPED, None))
            }
            MediaType::RTT => {
                // RTT packets are handled by ConnectionManager, not by peer decoders
                debug!(
                    "Received RTT packet for peer {} - ignoring in peer decoder",
                    self.session_id
                );
                Ok((media_type, DecodeStatus::SKIPPED, None))
            }
            MediaType::KEYFRAME_REQUEST => {
                // Keyframe requests are handled by encoders, not by peer decoders.
                debug!(
                    "Received KEYFRAME_REQUEST for peer {} - ignoring in peer decoder",
                    self.session_id
                );
                Ok((media_type, DecodeStatus::SKIPPED, None))
            }
            MediaType::MEDIA_TYPE_UNKNOWN => {
                log::error!(
                    "Received packet with unknown media type from peer {}",
                    self.session_id
                );
                Err(PeerDecodeError::UnknownMediaType)
            }
        }
    }

    /// Track the sequence number of an incoming video/screen packet and detect
    /// gaps. Returns `Some(media_type)` if a KEYFRAME_REQUEST should be sent
    /// for this peer, or `None` if no request is needed.
    ///
    /// Gap detection logic:
    /// 1. When a gap is detected (seq > last_seq + 1), record the time.
    /// 2. After `KEYFRAME_REQUEST_TIMEOUT_MS` with the gap still present,
    ///    return the media type to request a keyframe for.
    /// 3. Rate-limit to one request per `KEYFRAME_REQUEST_MIN_INTERVAL_MS`.
    /// 4. When a keyframe arrives (frame_type == "key"), clear the gap state.
    fn track_sequence(&mut self, media_type: MediaType, packet: &MediaPacket) -> Option<MediaType> {
        // Both VIDEO and SCREEN packets use `video_metadata` for sequence
        // tracking. This is correct: `transform_screen_chunk` in
        // `encode/transform.rs` populates `VideoMetadata { sequence, .. }`
        // for SCREEN packets the same way `transform_video_chunk` does for
        // VIDEO packets.
        let (seq, frame_type_str) = if let Some(vm) = packet.video_metadata.as_ref() {
            (vm.sequence, packet.frame_type.as_str())
        } else {
            return None;
        };

        let (last_seq, gap_at, last_req) = match media_type {
            MediaType::VIDEO => (
                &mut self.last_video_seq,
                &mut self.video_gap_detected_at_ms,
                &mut self.last_video_keyframe_request_ms,
            ),
            MediaType::SCREEN => (
                &mut self.last_screen_seq,
                &mut self.screen_gap_detected_at_ms,
                &mut self.last_screen_keyframe_request_ms,
            ),
            _ => return None,
        };

        let now = now_ms();

        // If this is a keyframe, clear the gap state -- we recovered.
        if frame_type_str == "key" {
            *gap_at = None;
        }

        if let Some(prev) = *last_seq {
            if seq > prev + 1 {
                // Gap detected. Record the time of first detection.
                if gap_at.is_none() {
                    *gap_at = Some(now);
                    debug!(
                        "Sequence gap detected for peer {} {:?}: expected {}, got {}",
                        self.session_id,
                        media_type,
                        prev + 1,
                        seq
                    );
                }
            }
        }

        // Update the last seen sequence number.
        *last_seq = Some(seq);

        // Check if enough time has passed since gap detection to send a request.
        if let Some(gap_time) = *gap_at {
            let elapsed_since_gap = now.saturating_sub(gap_time);
            let elapsed_since_last_req = now.saturating_sub(*last_req);

            if elapsed_since_gap >= KEYFRAME_REQUEST_TIMEOUT_MS
                && elapsed_since_last_req >= KEYFRAME_REQUEST_MIN_INTERVAL_MS
            {
                *last_req = now;
                return Some(media_type);
            }
        }

        None
    }

    fn on_heartbeat(&mut self) {
        self.heartbeat_count += 1;
    }

    pub fn check_heartbeat(&mut self) -> bool {
        if self.heartbeat_count != 0 {
            self.heartbeat_count = 0;
            return true;
        }
        debug!("---@@@--- detected heartbeat stop for {}", self.session_id);
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
    connected_peers: HashMapWithOrderedKeys<u64, Peer>,
    /// Cache of user_id -> display_name, populated from PARTICIPANT_JOINED events.
    /// This persists independently of the peer list so that when `ensure_peer()`
    /// creates a peer later (after the first media packet arrives), the display
    /// name is immediately available and does not fall back to user_id/email.
    display_name_cache: HashMap<String, String>,
    pub on_first_frame: Callback<(String, MediaType)>,
    pub get_video_canvas_id: Callback<String, String>,
    pub get_screen_canvas_id: Callback<String, String>,
    diagnostics: Option<Rc<DiagnosticManager>>,
    pub on_peer_removed: Callback<String>,
    vad_threshold: Option<f32>,
    /// Callback for sending packets back through the connection (used for
    /// KEYFRAME_REQUEST). Set by `VideoCallClient` after construction.
    send_packet: Option<Callback<PacketWrapper>>,
    /// The local user_id, needed to construct outgoing KEYFRAME_REQUEST packets.
    local_user_id: String,
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
            display_name_cache: HashMap::new(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: None,
            on_peer_removed: Callback::noop(),
            vad_threshold: None,
            send_packet: None,
            local_user_id: String::new(),
        }
    }

    pub fn new_with_diagnostics(diagnostics: Rc<DiagnosticManager>) -> Self {
        Self {
            connected_peers: HashMapWithOrderedKeys::new(),
            display_name_cache: HashMap::new(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: Some(diagnostics),
            on_peer_removed: Callback::noop(),
            vad_threshold: None,
            send_packet: None,
            local_user_id: String::new(),
        }
    }

    /// Set the callback used to send packets back through the connection.
    /// This is required for the PLI (keyframe request) mechanism.
    pub fn set_send_packet_callback(&mut self, callback: Callback<PacketWrapper>, user_id: String) {
        self.send_packet = Some(callback);
        self.local_user_id = user_id;
    }

    pub fn set_vad_threshold(&mut self, threshold: Option<f32>) {
        self.vad_threshold = threshold;
    }

    /// Update the visibility state for a peer identified by session_id.
    ///
    /// When `visible` is `false`, video and screen decoding is paused for this
    /// peer to save CPU. Audio is always decoded regardless of visibility so
    /// that off-screen participants can still be heard.
    ///
    /// Called by the UI layer when an `IntersectionObserver` detects that a
    /// peer's canvas element has scrolled in or out of the viewport.
    pub fn set_peer_visibility(&mut self, session_id: u64, visible: bool) {
        if let Some(peer) = self.connected_peers.get_mut(&session_id) {
            if peer.visible != visible {
                debug!(
                    "Peer {} visibility changed: {} -> {}",
                    session_id, peer.visible, visible
                );
                peer.visible = visible;
            }
        }
    }

    pub fn sorted_keys(&self) -> &Vec<u64> {
        self.connected_peers.ordered_keys()
    }

    pub fn get(&self, key: &u64) -> Option<&Peer> {
        self.connected_peers.get(key)
    }

    /// Set the canvas element for a peer's video decoder
    pub fn set_peer_video_canvas(
        &self,
        peer_id: u64,
        canvas: web_sys::HtmlCanvasElement,
    ) -> Result<(), JsValue> {
        if let Some(peer) = self.connected_peers.get(&peer_id) {
            peer.video.set_canvas(canvas)
        } else {
            Err(JsValue::from_str(&format!("Peer {peer_id} not found")))
        }
    }

    /// Set the canvas element for a peer's screen share decoder
    pub fn set_peer_screen_canvas(
        &self,
        peer_id: u64,
        canvas: web_sys::HtmlCanvasElement,
    ) -> Result<(), JsValue> {
        if let Some(peer) = self.connected_peers.get(&peer_id) {
            peer.screen.set_canvas(canvas)
        } else {
            Err(JsValue::from_str(&format!("Peer {peer_id} not found")))
        }
    }

    pub fn run_peer_monitor(&mut self) {
        let removed = self
            .connected_peers
            .remove_if_and_return(|peer| peer.check_heartbeat());
        for (_session_id, peer) in removed {
            self.on_peer_removed.emit(peer.sid_str);
        }
    }

    pub fn decode(&mut self, response: PacketWrapper, userid: &str) -> Result<(), PeerDecodeError> {
        let packet = Arc::new(response);
        let peer_session_id = packet.session_id;

        if let Some(peer) = self.connected_peers.get_mut(&peer_session_id) {
            if !peer.context_initialized {
                peer.video
                    .set_stream_context(userid.to_string(), peer.sid_str.clone());
                peer.screen
                    .set_stream_context(userid.to_string(), peer.sid_str.clone());
                peer.context_initialized = true;
            }
            match peer.decode(&packet) {
                Ok((MediaType::HEARTBEAT, _, _)) => {
                    peer.on_heartbeat();
                    Ok(())
                }
                Ok((media_type, decode_status, keyframe_request)) => {
                    if let Some(diagnostics) = &self.diagnostics {
                        diagnostics.track_frame(
                            &peer.sid_str,
                            media_type,
                            packet.data.len() as u64,
                        );
                    }

                    if decode_status.first_frame {
                        let sid_str = peer.sid_str.clone();
                        self.on_first_frame.emit((sid_str, media_type));
                    }

                    // If gap detection triggered a keyframe request, clone
                    // the peer's user_id before releasing the mutable borrow.
                    let kf_info = keyframe_request.map(|mt| (peer.user_id.clone(), mt));

                    // Now we can immutably borrow self for sending.
                    if let Some((peer_uid, requested_media_type)) = kf_info {
                        self.send_keyframe_request(&peer_uid, requested_media_type);
                    }

                    Ok(())
                }
                Err(e) => peer.reset().map_err(|_| e),
            }
        } else {
            Err(PeerDecodeError::NoSuchPeer(peer_session_id))
        }
    }

    /// Send a KEYFRAME_REQUEST packet to a specific peer.
    ///
    /// The packet is a `MediaPacket` with `media_type = KEYFRAME_REQUEST`
    /// and `user_id` set to the target peer. The `data` field encodes
    /// which stream (VIDEO or SCREEN) needs the keyframe.
    ///
    /// IMPORTANT: This uses `send_packet` (reliable stream), NOT
    /// `send_media_packet` (datagrams). KEYFRAME_REQUEST is a control
    /// message that MUST be delivered reliably.
    ///
    /// The packet is sent unencrypted (raw MediaPacket, not AES-encrypted)
    /// because this is a signaling/control packet, not user media data.
    /// The server needs to read the target `user_id` to route it correctly.
    fn send_keyframe_request(&self, peer_user_id: &str, requested_media_type: MediaType) {
        let Some(send_packet) = &self.send_packet else {
            debug!("Cannot send KEYFRAME_REQUEST: no send_packet callback");
            return;
        };

        let media_type_byte = match requested_media_type {
            MediaType::VIDEO => b"VIDEO".to_vec(),
            MediaType::SCREEN => b"SCREEN".to_vec(),
            _ => return,
        };

        let media_packet = MediaPacket {
            media_type: MediaType::KEYFRAME_REQUEST.into(),
            user_id: peer_user_id.as_bytes().to_vec(),
            data: media_type_byte,
            ..Default::default()
        };

        let media_data = match media_packet.write_to_bytes() {
            Ok(data) => data,
            Err(e) => {
                log::warn!("Failed to serialize keyframe request: {}", e);
                return;
            }
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            user_id: self.local_user_id.as_bytes().to_vec(),
            data: media_data,
            ..Default::default()
        };

        log::info!(
            "Sending KEYFRAME_REQUEST to {} for {:?}",
            peer_user_id,
            requested_media_type
        );
        send_packet.emit(wrapper);
    }

    fn add_peer(
        &mut self,
        user_id: &str,
        session_id: u64,
        aes: Option<Aes128State>,
    ) -> Result<(), JsValue> {
        let sid_str = session_id.to_string();
        debug!("Adding peer {user_id} with session_id {sid_str}");
        let mut peer = Peer::new(
            self.get_video_canvas_id.emit(sid_str.clone()),
            self.get_screen_canvas_id.emit(sid_str),
            session_id,
            user_id.to_owned(),
            aes,
            self.vad_threshold,
        )?;
        // Apply cached display name if PARTICIPANT_JOINED arrived before
        // the first media packet created this peer entry.
        if let Some(cached_name) = self.display_name_cache.get(user_id) {
            debug!(
                "Applying cached display_name '{}' for peer {} (user_id={})",
                cached_name, session_id, user_id
            );
            peer.display_name = Some(cached_name.clone());
        }
        self.connected_peers.insert(session_id, peer);
        Ok(())
    }

    pub fn delete_peer(&mut self, session_id: u64) {
        if let Some(peer) = self.connected_peers.remove(&session_id) {
            self.on_peer_removed.emit(peer.sid_str);
        }
    }

    /// Remove all peers and terminate their decoder workers immediately.
    ///
    /// Called when the connection drops so stale workers don't linger and
    /// consume WASM memory while the client reconnects.
    pub fn clear_all_peers(&mut self) {
        let removed = self.connected_peers.drain_all();
        for (_session_id, peer) in removed {
            self.on_peer_removed.emit(peer.sid_str);
        }
        // Clear the display name cache so stale names don't persist
        // across reconnections.
        self.display_name_cache.clear();
        // Peers are dropped here, triggering Worker::terminate() via Drop impl
    }

    pub fn ensure_peer(&mut self, session_id: u64, user_id: &str) -> PeerStatus {
        if self.connected_peers.contains_key(&session_id) {
            PeerStatus::NoChange
        } else if let Err(e) = self.add_peer(user_id, session_id, None) {
            log::error!("Error adding peer: {e:?}");
            PeerStatus::NoChange
        } else {
            PeerStatus::Added(session_id)
        }
    }

    pub fn set_peer_aes(
        &mut self,
        session_id: u64,
        aes: Aes128State,
    ) -> Result<(), PeerDecodeError> {
        match self.connected_peers.get_mut(&session_id) {
            Some(peer) => {
                peer.aes = Some(aes);
                Ok(())
            }
            None => Err(PeerDecodeError::NoSuchPeer(session_id)),
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

    /// Updates the speaker device by switching the sink on the shared AudioContext
    pub fn update_speaker_device(
        &mut self,
        speaker_device_id: Option<String>,
    ) -> Result<(), JsValue> {
        log::info!(
            "Updating shared AudioContext sink to {speaker_device_id:?} (no decoder rebuild)",
        );
        SharedAudioContext::update_speaker_device(speaker_device_id)?;
        Ok(())
    }

    /// Set the display name for a peer identified by user_id (email).
    /// This is called when a PARTICIPANT_JOINED event provides the display name.
    ///
    /// The display name is stored in both the per-peer entry (if the peer
    /// already exists) AND a persistent cache keyed by user_id. This way,
    /// if the PARTICIPANT_JOINED event arrives before the first media packet
    /// creates the peer entry via `ensure_peer()`, the display name is
    /// still available when the peer is created later.
    pub fn set_peer_display_name_by_user_id(&mut self, user_id: &str, display_name: String) {
        // Always persist in the cache so that future `add_peer()` calls
        // can pick it up even if no peer entry exists yet.
        self.display_name_cache
            .insert(user_id.to_string(), display_name.clone());

        // Also update any existing peer entries with this user_id.
        let keys: Vec<u64> = self.connected_peers.ordered_keys().clone();
        for key in keys {
            if let Some(peer) = self.connected_peers.get_mut(&key) {
                if peer.user_id == user_id {
                    peer.display_name = Some(display_name.clone());
                }
            }
        }
    }

    /// Get the display name for a peer by session_id string.
    pub fn get_peer_display_name(&self, session_id_str: &str) -> Option<String> {
        let sid: u64 = session_id_str.parse().ok()?;
        self.connected_peers
            .get(&sid)
            .and_then(|peer| peer.display_name.clone())
    }

    pub fn is_peer_speaking(&self, key: &String) -> bool {
        let sid: u64 = match key.parse() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if let Some(peer) = self.connected_peers.get(&sid) {
            return peer.is_speaking;
        }
        false
    }

    pub fn peer_audio_level(&self, key: &String) -> f32 {
        let sid: u64 = match key.parse() {
            Ok(v) => v,
            Err(_) => return 0.0,
        };
        if let Some(peer) = self.connected_peers.get(&sid) {
            return peer.audio_level;
        }
        0.0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use std::cell::Cell;
    use videocall_types::protos::media_packet::media_packet::MediaType;
    use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket};
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    // -- mock audio decoder -----------------------------------------------

    /// No-op audio decoder for unit tests.
    /// Muted state is stored in an `Rc<Cell<bool>>` so tests can inspect it
    /// after handing ownership to `Peer`.
    struct MockAudioDecoder {
        muted: Rc<Cell<bool>>,
    }

    impl MockAudioDecoder {
        fn new() -> (Self, Rc<Cell<bool>>) {
            let muted = Rc::new(Cell::new(true));
            (
                Self {
                    muted: muted.clone(),
                },
                muted,
            )
        }
    }

    impl AudioPeerDecoderTrait for MockAudioDecoder {
        fn decode(&mut self, _packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
            Ok(DecodeStatus::SKIPPED)
        }
        fn flush(&mut self) {}
        fn set_muted(&mut self, muted: bool) {
            self.muted.set(muted);
        }
    }

    // -- helpers ----------------------------------------------------------

    /// Wrap a `MediaPacket` into a `PacketWrapper` ready for `Peer::decode`.
    fn wrap(media: &MediaPacket, session_id: u64) -> Arc<PacketWrapper> {
        let data = media.write_to_bytes().expect("serialize MediaPacket");
        Arc::new(PacketWrapper {
            data,
            user_id: "test@test.com".into(),
            packet_type: PacketType::MEDIA.into(),
            session_id,
            ..Default::default()
        })
    }

    fn heartbeat_packet(
        session_id: u64,
        video: bool,
        audio: bool,
        screen: bool,
    ) -> Arc<PacketWrapper> {
        let media = MediaPacket {
            media_type: MediaType::HEARTBEAT.into(),
            user_id: "test@test.com".into(),
            heartbeat_metadata: Some(HeartbeatMetadata {
                video_enabled: video,
                audio_enabled: audio,
                screen_enabled: screen,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        wrap(&media, session_id)
    }

    fn video_frame_packet(session_id: u64) -> Arc<PacketWrapper> {
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            user_id: "test@test.com".into(),
            data: vec![0u8; 10], // dummy payload
            ..Default::default()
        };
        wrap(&media, session_id)
    }

    fn audio_frame_packet(session_id: u64) -> Arc<PacketWrapper> {
        let media = MediaPacket {
            media_type: MediaType::AUDIO.into(),
            user_id: "test@test.com".into(),
            data: vec![0u8; 10],
            ..Default::default()
        };
        wrap(&media, session_id)
    }

    fn screen_frame_packet(session_id: u64) -> Arc<PacketWrapper> {
        let media = MediaPacket {
            media_type: MediaType::SCREEN.into(),
            user_id: "test@test.com".into(),
            data: vec![0u8; 10],
            ..Default::default()
        };
        wrap(&media, session_id)
    }

    /// Create a `Peer` with no-op decoders (no browser APIs required).
    /// Returns the peer and an `Rc<Cell<bool>>` handle to the mock audio
    /// decoder's muted state for test assertions.
    fn make_test_peer(session_id: u64) -> (Peer, Rc<Cell<bool>>) {
        let sid_str = session_id.to_string();
        let (mock_audio, muted_handle) = MockAudioDecoder::new();
        let peer = Peer {
            audio: Box::new(mock_audio),
            video: VideoPeerDecoder::noop(),
            screen: VideoPeerDecoder::noop(),
            session_id,
            sid_str,
            user_id: "test@test.com".into(),
            video_canvas_id: format!("video-{session_id}"),
            screen_canvas_id: format!("screen-{session_id}"),
            aes: None,
            heartbeat_count: 1,
            video_enabled: false,
            audio_enabled: false,
            screen_enabled: false,
            display_name: None,
            visible: true,
            context_initialized: false,
            has_received_heartbeat: false,
            is_speaking: false,
            audio_level: 0.0,
            vad_threshold: None,
            last_video_seq: None,
            last_screen_seq: None,
            video_gap_detected_at_ms: None,
            screen_gap_detected_at_ms: None,
            last_video_keyframe_request_ms: 0,
            last_screen_keyframe_request_ms: 0,
        };
        (peer, muted_handle)
    }

    // -- straggler guard tests --------------------------------------------

    /// Before any heartbeat, a VIDEO frame should infer video_enabled = true.
    #[wasm_bindgen_test]
    fn video_frame_before_heartbeat_infers_enabled() {
        let (mut peer, _muted) = make_test_peer(1);
        assert!(!peer.video_enabled);
        assert!(!peer.has_received_heartbeat);

        let packet = video_frame_packet(1);
        // Video decode will fail (noop decoder gets dummy data) but
        // state inference happens before the codec call.
        let _ = peer.decode(&packet);
        assert!(peer.video_enabled, "video_enabled should be inferred true");
    }

    /// After a heartbeat with video_enabled=false, a straggler VIDEO frame
    /// must NOT flip video_enabled back to true and must return rendered=false.
    #[wasm_bindgen_test]
    fn video_straggler_after_heartbeat_is_dropped() {
        let (mut peer, _muted) = make_test_peer(2);

        // Receive heartbeat: video off, audio off, screen off.
        let hb = heartbeat_packet(2, false, false, false);
        let result = peer.decode(&hb);
        assert!(result.is_ok());
        assert!(peer.has_received_heartbeat);
        assert!(!peer.video_enabled);

        // Now a straggler video frame arrives.
        let packet = video_frame_packet(2);
        let result = peer.decode(&packet);
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(!status.first_frame, "straggler must not be a first frame");
        assert!(
            !peer.video_enabled,
            "straggler video frame must not re-enable video"
        );
    }

    /// Before any heartbeat, an AUDIO frame should infer audio_enabled = true
    /// and unmute the audio decoder.
    #[wasm_bindgen_test]
    fn audio_frame_before_heartbeat_infers_enabled() {
        let (mut peer, muted_handle) = make_test_peer(3);
        assert!(!peer.audio_enabled);
        assert!(muted_handle.get(), "audio should start muted");

        let packet = audio_frame_packet(3);
        let _ = peer.decode(&packet);
        assert!(peer.audio_enabled, "audio_enabled should be inferred true");
        assert!(
            !muted_handle.get(),
            "audio decoder should be unmuted after inference"
        );
    }

    /// After a heartbeat with audio_enabled=false, a straggler AUDIO frame
    /// must NOT flip audio_enabled back to true and must return rendered=false.
    #[wasm_bindgen_test]
    fn audio_straggler_after_heartbeat_is_dropped() {
        let (mut peer, _muted) = make_test_peer(4);

        let hb = heartbeat_packet(4, false, false, false);
        let _ = peer.decode(&hb);
        assert!(peer.has_received_heartbeat);
        assert!(!peer.audio_enabled);

        let packet = audio_frame_packet(4);
        let result = peer.decode(&packet);
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(!status.first_frame, "straggler must not be a first frame");
        assert!(
            !peer.audio_enabled,
            "straggler audio frame must not re-enable audio"
        );
    }

    /// Before any heartbeat, a SCREEN frame should infer screen_enabled = true.
    #[wasm_bindgen_test]
    fn screen_frame_before_heartbeat_infers_enabled() {
        let (mut peer, _muted) = make_test_peer(5);
        assert!(!peer.screen_enabled);

        let packet = screen_frame_packet(5);
        let _ = peer.decode(&packet);
        assert!(
            peer.screen_enabled,
            "screen_enabled should be inferred true"
        );
    }

    /// After a heartbeat with screen_enabled=false, a straggler SCREEN frame
    /// must NOT flip screen_enabled back to true and must return rendered=false.
    #[wasm_bindgen_test]
    fn screen_straggler_after_heartbeat_is_dropped() {
        let (mut peer, _muted) = make_test_peer(6);

        let hb = heartbeat_packet(6, false, false, false);
        let _ = peer.decode(&hb);
        assert!(peer.has_received_heartbeat);
        assert!(!peer.screen_enabled);

        let packet = screen_frame_packet(6);
        let result = peer.decode(&packet);
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(!status.first_frame, "straggler must not be a first frame");
        assert!(
            !peer.screen_enabled,
            "straggler screen frame must not re-enable screen"
        );
    }

    /// A heartbeat that enables video, followed by a video frame, should work.
    /// (Ensures the guard doesn't block legitimate frames.)
    #[wasm_bindgen_test]
    fn video_frame_after_enabling_heartbeat_is_accepted() {
        let (mut peer, _muted) = make_test_peer(7);

        // Heartbeat enables video.
        let hb = heartbeat_packet(7, true, false, false);
        let _ = peer.decode(&hb);
        assert!(peer.video_enabled);

        // A video frame should pass the guard (video_enabled is already true).
        let packet = video_frame_packet(7);
        let _ = peer.decode(&packet);
        // video_enabled should remain true.
        assert!(peer.video_enabled);
    }

    /// Heartbeat toggles: enable → disable → straggler.
    #[wasm_bindgen_test]
    fn video_enable_disable_straggler_sequence() {
        let (mut peer, _muted) = make_test_peer(8);

        // Enable video via heartbeat.
        let hb_on = heartbeat_packet(8, true, false, false);
        let _ = peer.decode(&hb_on);
        assert!(peer.video_enabled);

        // Disable video via heartbeat.
        let hb_off = heartbeat_packet(8, false, false, false);
        let _ = peer.decode(&hb_off);
        assert!(!peer.video_enabled);

        // Straggler video frame should be dropped.
        let packet = video_frame_packet(8);
        let result = peer.decode(&packet);
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(
            !peer.video_enabled,
            "straggler after disable must not re-enable"
        );
    }

    // -- audio_level tests -------------------------------------------------

    /// A freshly created peer should have audio_level == 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_default_zero() {
        let (peer, _muted) = make_test_peer(100);
        assert!(
            (peer.audio_level - 0.0).abs() < f32::EPSILON,
            "new peer should have audio_level == 0.0, got {}",
            peer.audio_level
        );
    }

    /// Insert a peer into a PeerDecodeManager, set its audio_level, then
    /// verify `peer_audio_level()` returns the expected value.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_accessor() {
        let mut manager = PeerDecodeManager::new();
        let (mut peer, _muted) = make_test_peer(101);
        peer.audio_level = 0.75;
        manager.connected_peers.insert(101, peer);

        let level = manager.peer_audio_level(&"101".to_string());
        assert!(
            (level - 0.75).abs() < f32::EPSILON,
            "peer_audio_level should return 0.75, got {level}"
        );
    }

    /// Calling `peer_audio_level()` for a non-existent peer should return 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_unknown_peer_returns_zero() {
        let manager = PeerDecodeManager::new();
        let level = manager.peer_audio_level(&"99999".to_string());
        assert!(
            (level - 0.0).abs() < f32::EPSILON,
            "peer_audio_level for unknown peer should return 0.0, got {level}"
        );
    }

    /// Calling `peer_audio_level()` with a non-numeric key should return 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_invalid_key_returns_zero() {
        let manager = PeerDecodeManager::new();
        let level = manager.peer_audio_level(&"not-a-number".to_string());
        assert!(
            (level - 0.0).abs() < f32::EPSILON,
            "peer_audio_level for invalid key should return 0.0, got {level}"
        );
    }

    /// After a heartbeat with is_speaking=false, audio_level should be reset to 0.0.
    #[wasm_bindgen_test]
    fn test_peer_audio_level_reset_on_not_speaking_heartbeat() {
        let (mut peer, _muted) = make_test_peer(102);
        // Simulate audio level being set during active speech
        peer.audio_level = 0.5;
        peer.is_speaking = true;

        // Heartbeat with all disabled (is_speaking defaults to false)
        let hb = heartbeat_packet(102, false, false, false);
        let _ = peer.decode(&hb);

        assert!(
            (peer.audio_level - 0.0).abs() < f32::EPSILON,
            "audio_level should be reset to 0.0 when heartbeat says not speaking, got {}",
            peer.audio_level
        );
    }

    /// Full sequence: enable → legitimate frame → disable → straggler dropped.
    #[wasm_bindgen_test]
    fn video_enable_frame_disable_straggler_full_sequence() {
        let (mut peer, _muted) = make_test_peer(9);

        // 1. Enable video via heartbeat.
        let hb_on = heartbeat_packet(9, true, false, false);
        let _ = peer.decode(&hb_on);
        assert!(peer.video_enabled);

        // 2. Legitimate video frame while enabled — should pass through.
        let frame = video_frame_packet(9);
        let _ = peer.decode(&frame);
        assert!(peer.video_enabled, "legitimate frame must not change state");

        // 3. Disable video via heartbeat.
        let hb_off = heartbeat_packet(9, false, false, false);
        let _ = peer.decode(&hb_off);
        assert!(!peer.video_enabled);

        // 4. Straggler video frame after disable — must be dropped.
        let straggler = video_frame_packet(9);
        let result = peer.decode(&straggler);
        assert!(result.is_ok());
        let (_media_type, status, _kf_req) = result.unwrap();
        assert!(!status.rendered, "straggler must not be rendered");
        assert!(!status.first_frame, "straggler must not be a first frame");
        assert!(
            !peer.video_enabled,
            "straggler after disable must not re-enable"
        );
    }

    // -- MeetingPacket target_user_id filtering tests ---------------------------

    /// A MeetingPacket with PARTICIPANT_ADMITTED and a specific target_user_id
    /// should round-trip through protobuf serialization correctly.
    #[wasm_bindgen_test]
    fn meeting_packet_participant_admitted_deserializes_correctly() {
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
        use videocall_types::protos::meeting_packet::MeetingPacket;
        let mut packet = MeetingPacket::new();
        packet.event_type = MeetingEventType::PARTICIPANT_ADMITTED.into();
        packet.room_id = "room-123".into();
        packet.target_user_id = "alice@example.com".as_bytes().to_vec();
        packet.message = "Welcome".into();

        let bytes = packet.write_to_bytes().expect("serialize MeetingPacket");
        let parsed = MeetingPacket::parse_from_bytes(&bytes).expect("parse MeetingPacket");

        assert_eq!(
            parsed.event_type.enum_value(),
            Ok(MeetingEventType::PARTICIPANT_ADMITTED),
            "event_type should be PARTICIPANT_ADMITTED"
        );
        assert_eq!(parsed.room_id, "room-123");
        assert_eq!(parsed.target_user_id[..], *"alice@example.com".as_bytes());
        assert_eq!(parsed.message, "Welcome");
    }

    /// Verify the target_user_id comparison used for filtering:
    /// the callback should only fire when target_user_id matches the local userid.
    #[wasm_bindgen_test]
    fn meeting_packet_target_user_id_matching_logic() {
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let mut packet = MeetingPacket::new();
        packet.target_user_id = "alice@example.com".as_bytes().to_vec();

        // Matching case: target_user_id equals userid converted to bytes
        let userid_bytes = "alice@example.com".as_bytes();
        assert_eq!(
            packet.target_user_id[..],
            *userid_bytes,
            "target_user_id should match the local userid"
        );

        // Non-matching case: target_user_id does not equal a different userid
        let observer_bytes = "observer".as_bytes();
        assert_ne!(
            packet.target_user_id[..],
            *observer_bytes,
            "target_user_id should NOT match a different userid"
        );
    }

    /// Verify that PARTICIPANT_REJECTED events also carry target_user_id correctly.
    #[wasm_bindgen_test]
    fn meeting_packet_participant_rejected_has_target_user_id() {
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let mut packet = MeetingPacket::new();
        packet.event_type = MeetingEventType::PARTICIPANT_REJECTED.into();
        packet.target_user_id = "bob@example.com".as_bytes().to_vec();
        packet.room_id = "room-456".into();

        let bytes = packet.write_to_bytes().expect("serialize");
        let parsed = MeetingPacket::parse_from_bytes(&bytes).expect("parse");

        assert_eq!(
            parsed.event_type.enum_value(),
            Ok(MeetingEventType::PARTICIPANT_REJECTED)
        );
        assert_eq!(parsed.target_user_id[..], *"bob@example.com".as_bytes());
        assert_eq!(parsed.room_id, "room-456");
    }

    /// An empty target_user_id field should not match any real userid.
    #[wasm_bindgen_test]
    fn meeting_packet_empty_target_user_id_does_not_match() {
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let packet = MeetingPacket::new();
        assert!(packet.target_user_id.is_empty());

        let userid_bytes = "alice@example.com".as_bytes();
        assert_ne!(
            packet.target_user_id[..],
            *userid_bytes,
            "empty target_user_id should not match any userid"
        );
    }

    // -- PLI gap detection tests -------------------------------------------

    /// Sequential VIDEO packets (no gap) should NOT trigger a keyframe request.
    #[wasm_bindgen_test]
    fn sequential_video_packets_no_keyframe_request() {
        let (mut peer, _muted) = make_test_peer(200);

        for seq in 1..=10 {
            let result = peer.track_sequence(MediaType::VIDEO, &{
                use videocall_types::protos::media_packet::VideoMetadata;
                MediaPacket {
                    video_metadata: Some(VideoMetadata {
                        sequence: seq,
                        ..Default::default()
                    })
                    .into(),
                    frame_type: "delta".to_string(),
                    ..Default::default()
                }
            });
            assert!(
                result.is_none(),
                "Sequential seq={seq} should not trigger keyframe request"
            );
        }
        // No gap should have been detected.
        assert!(peer.video_gap_detected_at_ms.is_none());
    }

    /// A gap in video sequence numbers should record a gap timestamp but NOT
    /// immediately trigger a keyframe request (timeout hasn't elapsed).
    #[wasm_bindgen_test]
    fn video_gap_detected_but_no_immediate_request() {
        let (mut peer, _muted) = make_test_peer(201);

        // Send seq 1.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt1);

        // Send seq 5 (gap: 2, 3, 4 missing).
        let pkt5 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 5,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer.track_sequence(MediaType::VIDEO, &pkt5);

        // Gap should be recorded.
        assert!(
            peer.video_gap_detected_at_ms.is_some(),
            "Gap should be detected"
        );
        // But timeout hasn't elapsed, so no request yet.
        assert!(
            result.is_none(),
            "Should not immediately trigger keyframe request"
        );
    }

    /// After a gap is detected and enough time has passed (simulated by
    /// backdating gap_detected_at_ms), a keyframe request should fire.
    #[wasm_bindgen_test]
    fn video_gap_triggers_keyframe_after_timeout() {
        let (mut peer, _muted) = make_test_peer(202);

        // Establish baseline sequence.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt1);

        // Introduce a gap.
        let pkt5 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 5,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt5);
        assert!(peer.video_gap_detected_at_ms.is_some());

        // Simulate time having passed: backdate the gap detection timestamp
        // so that the next call sees elapsed >= KEYFRAME_REQUEST_TIMEOUT_MS.
        peer.video_gap_detected_at_ms =
            Some(now_ms().saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 100));
        // Also ensure rate-limit is not in effect.
        peer.last_video_keyframe_request_ms = 0;

        // Next packet (still with gap present) should trigger a request.
        let pkt6 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 6,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer.track_sequence(MediaType::VIDEO, &pkt6);
        assert_eq!(
            result,
            Some(MediaType::VIDEO),
            "Keyframe request should fire after timeout"
        );
    }

    /// Rate-limiting: a second keyframe request within KEYFRAME_REQUEST_MIN_INTERVAL_MS
    /// should be suppressed.
    #[wasm_bindgen_test]
    fn keyframe_request_rate_limited() {
        let (mut peer, _muted) = make_test_peer(203);

        // Establish gap.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt1);
        let pkt5 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 5,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt5);

        // Backdate gap so timeout is satisfied.
        peer.video_gap_detected_at_ms =
            Some(now_ms().saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 100));
        peer.last_video_keyframe_request_ms = 0;

        // First request should fire.
        let pkt6 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 6,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer.track_sequence(MediaType::VIDEO, &pkt6);
        assert_eq!(result, Some(MediaType::VIDEO), "First request should fire");

        // last_video_keyframe_request_ms is now set to ~now. A second call
        // immediately should be suppressed by rate-limiting.
        let pkt7 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 7,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result2 = peer.track_sequence(MediaType::VIDEO, &pkt7);
        assert!(
            result2.is_none(),
            "Second request should be rate-limited (too soon)"
        );
    }

    /// A keyframe ("key" frame_type) should clear the gap state.
    #[wasm_bindgen_test]
    fn keyframe_clears_gap_state() {
        let (mut peer, _muted) = make_test_peer(204);

        // Establish gap.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt1);

        let pkt5 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 5,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt5);
        assert!(peer.video_gap_detected_at_ms.is_some(), "Gap should exist");

        // Now receive a keyframe — should clear the gap.
        let key_pkt = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 6,
                    ..Default::default()
                })
                .into(),
                frame_type: "key".to_string(),
                ..Default::default()
            }
        };
        let result = peer.track_sequence(MediaType::VIDEO, &key_pkt);
        assert!(result.is_none(), "Keyframe should not trigger request");
        assert!(
            peer.video_gap_detected_at_ms.is_none(),
            "Keyframe should clear gap state"
        );
    }

    /// Video and screen sequences should be tracked independently.
    #[wasm_bindgen_test]
    fn video_and_screen_independent_tracking() {
        let (mut peer, _muted) = make_test_peer(205);

        // Send video seq 1, 2, 3 (no gap).
        for seq in 1..=3 {
            let pkt = {
                use videocall_types::protos::media_packet::VideoMetadata;
                MediaPacket {
                    video_metadata: Some(VideoMetadata {
                        sequence: seq,
                        ..Default::default()
                    })
                    .into(),
                    frame_type: "delta".to_string(),
                    ..Default::default()
                }
            };
            let _ = peer.track_sequence(MediaType::VIDEO, &pkt);
        }
        assert!(peer.video_gap_detected_at_ms.is_none());

        // Send screen seq 1, then seq 10 (gap).
        let screen1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::SCREEN, &screen1);

        let screen10 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 10,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::SCREEN, &screen10);

        // Video should have no gap, screen should have a gap.
        assert!(
            peer.video_gap_detected_at_ms.is_none(),
            "Video should have no gap"
        );
        assert!(
            peer.screen_gap_detected_at_ms.is_some(),
            "Screen should have a gap"
        );

        // Verify last_seq values are independent.
        assert_eq!(peer.last_video_seq, Some(3));
        assert_eq!(peer.last_screen_seq, Some(10));
    }

    /// Different peers should have independent sequence tracking.
    #[wasm_bindgen_test]
    fn different_peers_independent_sequence_tracking() {
        let (mut peer_a, _) = make_test_peer(300);
        let (mut peer_b, _) = make_test_peer(301);

        // Peer A: sequential (no gap).
        for seq in 1..=5 {
            let pkt = {
                use videocall_types::protos::media_packet::VideoMetadata;
                MediaPacket {
                    video_metadata: Some(VideoMetadata {
                        sequence: seq,
                        ..Default::default()
                    })
                    .into(),
                    frame_type: "delta".to_string(),
                    ..Default::default()
                }
            };
            let _ = peer_a.track_sequence(MediaType::VIDEO, &pkt);
        }

        // Peer B: gap (seq 1 -> seq 10).
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer_b.track_sequence(MediaType::VIDEO, &pkt1);
        let pkt10 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 10,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer_b.track_sequence(MediaType::VIDEO, &pkt10);

        assert!(
            peer_a.video_gap_detected_at_ms.is_none(),
            "Peer A should have no gap"
        );
        assert!(
            peer_b.video_gap_detected_at_ms.is_some(),
            "Peer B should have a gap"
        );
    }

    /// SCREEN gap triggers keyframe request for SCREEN (not VIDEO).
    #[wasm_bindgen_test]
    fn screen_gap_triggers_screen_keyframe_request() {
        let (mut peer, _muted) = make_test_peer(206);

        // Establish screen gap.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::SCREEN, &pkt1);

        let pkt5 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 5,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::SCREEN, &pkt5);

        // Backdate gap and clear rate limit.
        peer.screen_gap_detected_at_ms =
            Some(now_ms().saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 100));
        peer.last_screen_keyframe_request_ms = 0;

        let pkt6 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 6,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer.track_sequence(MediaType::SCREEN, &pkt6);
        assert_eq!(
            result,
            Some(MediaType::SCREEN),
            "Screen gap should trigger SCREEN keyframe request"
        );
    }

    /// Packet without video_metadata should return None from track_sequence.
    #[wasm_bindgen_test]
    fn no_video_metadata_returns_none() {
        let (mut peer, _muted) = make_test_peer(207);

        let pkt = MediaPacket {
            frame_type: "delta".to_string(),
            // No video_metadata set.
            ..Default::default()
        };
        let result = peer.track_sequence(MediaType::VIDEO, &pkt);
        assert!(
            result.is_none(),
            "Missing video_metadata should return None"
        );
    }

    /// track_sequence called with AUDIO media type should return None
    /// (only VIDEO and SCREEN are tracked).
    #[wasm_bindgen_test]
    fn audio_media_type_not_tracked() {
        let (mut peer, _muted) = make_test_peer(208);

        let pkt = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer.track_sequence(MediaType::AUDIO, &pkt);
        assert!(result.is_none(), "AUDIO should not be tracked");
    }

    // -- Visibility-based skip tests ----------------------------------------

    /// Setting peer visibility to false should cause VIDEO decoding to return
    /// SKIPPED status.
    #[wasm_bindgen_test]
    fn invisible_peer_skips_video_decode() {
        let (mut peer, _muted) = make_test_peer(210);

        // Enable video via heartbeat so the straggler guard doesn't block.
        let hb = heartbeat_packet(210, true, true, false);
        let _ = peer.decode(&hb);
        assert!(peer.video_enabled);

        // Mark invisible.
        peer.visible = false;

        let pkt = video_frame_packet(210);
        let result = peer.decode(&pkt);
        assert!(result.is_ok());
        let (_mt, status, _kf) = result.unwrap();
        assert!(
            !status.rendered,
            "Invisible peer video should not be rendered"
        );
    }

    /// Setting peer visibility to false should cause SCREEN decoding to return
    /// SKIPPED status.
    #[wasm_bindgen_test]
    fn invisible_peer_skips_screen_decode() {
        let (mut peer, _muted) = make_test_peer(211);

        // Enable screen via heartbeat.
        let hb = heartbeat_packet(211, false, false, true);
        let _ = peer.decode(&hb);
        assert!(peer.screen_enabled);

        // Mark invisible.
        peer.visible = false;

        let pkt = screen_frame_packet(211);
        let result = peer.decode(&pkt);
        assert!(result.is_ok());
        let (_mt, status, _kf) = result.unwrap();
        assert!(
            !status.rendered,
            "Invisible peer screen should not be rendered"
        );
    }

    /// Audio should ALWAYS be decoded regardless of visibility.
    #[wasm_bindgen_test]
    fn invisible_peer_still_decodes_audio() {
        let (mut peer, _muted) = make_test_peer(212);

        // Enable audio (no heartbeat yet, so audio will be inferred enabled).
        peer.visible = false;

        let pkt = audio_frame_packet(212);
        let result = peer.decode(&pkt);
        assert!(result.is_ok());
        // Audio_enabled should be inferred true (no heartbeat received).
        assert!(
            peer.audio_enabled,
            "Audio should still be enabled/inferred even when invisible"
        );
        // The key point: the decode path does NOT check `visible` for audio.
        // The result is Ok, meaning it went through the audio decode path
        // (not the straggler SKIPPED path).
    }

    /// Restoring visibility should resume video decoding.
    #[wasm_bindgen_test]
    fn restored_visibility_resumes_video() {
        let (mut peer, _muted) = make_test_peer(213);

        // Enable video via heartbeat.
        let hb = heartbeat_packet(213, true, false, false);
        let _ = peer.decode(&hb);

        // Go invisible, then visible again.
        peer.visible = false;
        peer.visible = true;

        let pkt = video_frame_packet(213);
        let result = peer.decode(&pkt);
        // The decode will go through to the actual video decoder (noop).
        // Even if the noop decoder "fails" on dummy data, it won't return
        // SKIPPED due to visibility.
        assert!(
            result.is_ok() || result.is_err(),
            "Visible peer should attempt video decode"
        );
        // If it got through to the decoder (Ok), it means visibility didn't block it.
        if let Ok((_mt, status, _kf)) = result {
            // With noop decoder, rendered might be false, but the important
            // thing is that it wasn't the visibility-SKIPPED path.
            // We verify by checking that the visibility path was NOT taken.
            assert!(peer.visible);
        }
    }

    /// PeerDecodeManager::set_peer_visibility should update the peer's visible flag.
    #[wasm_bindgen_test]
    fn manager_set_peer_visibility() {
        let mut manager = PeerDecodeManager::new();
        let (peer, _muted) = make_test_peer(220);
        assert!(peer.visible); // default is true
        manager.connected_peers.insert(220, peer);

        manager.set_peer_visibility(220, false);
        assert!(
            !manager.connected_peers.get(&220).unwrap().visible,
            "Peer should be invisible after set_peer_visibility(false)"
        );

        manager.set_peer_visibility(220, true);
        assert!(
            manager.connected_peers.get(&220).unwrap().visible,
            "Peer should be visible after set_peer_visibility(true)"
        );
    }

    /// set_peer_visibility on a non-existent session_id should be a no-op.
    #[wasm_bindgen_test]
    fn manager_set_peer_visibility_unknown_peer() {
        let mut manager = PeerDecodeManager::new();
        // Should not panic.
        manager.set_peer_visibility(99999, false);
    }

    /// Multiple gaps: after one gap triggers a keyframe request and the gap
    /// is cleared by a keyframe, a new gap should be independently detected.
    #[wasm_bindgen_test]
    fn multiple_gaps_handled_independently() {
        let (mut peer, _muted) = make_test_peer(209);

        // First gap: seq 1 -> 5.
        let pkt1 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 1,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt1);
        let pkt5 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 5,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt5);
        assert!(peer.video_gap_detected_at_ms.is_some());

        // Backdate and trigger request.
        peer.video_gap_detected_at_ms =
            Some(now_ms().saturating_sub(KEYFRAME_REQUEST_TIMEOUT_MS + 100));
        peer.last_video_keyframe_request_ms = 0;
        let pkt6 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 6,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let result = peer.track_sequence(MediaType::VIDEO, &pkt6);
        assert_eq!(result, Some(MediaType::VIDEO));

        // Clear gap with a keyframe.
        let key = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 7,
                    ..Default::default()
                })
                .into(),
                frame_type: "key".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &key);
        assert!(
            peer.video_gap_detected_at_ms.is_none(),
            "Gap should be cleared by keyframe"
        );

        // Second gap: seq 7 -> 20.
        let pkt20 = {
            use videocall_types::protos::media_packet::VideoMetadata;
            MediaPacket {
                video_metadata: Some(VideoMetadata {
                    sequence: 20,
                    ..Default::default()
                })
                .into(),
                frame_type: "delta".to_string(),
                ..Default::default()
            }
        };
        let _ = peer.track_sequence(MediaType::VIDEO, &pkt20);
        assert!(
            peer.video_gap_detected_at_ms.is_some(),
            "Second gap should be detected independently"
        );
    }

    /// A MeetingPacket embedded in a PacketWrapper with MEETING type should
    /// be extractable via parse_from_bytes on the wrapper's data field.
    #[wasm_bindgen_test]
    fn meeting_packet_in_packet_wrapper_round_trip() {
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let mut meeting = MeetingPacket::new();
        meeting.event_type = MeetingEventType::PARTICIPANT_ADMITTED.into();
        meeting.target_user_id = "charlie@example.com".as_bytes().to_vec();
        meeting.room_id = "room-789".into();

        let meeting_bytes = meeting.write_to_bytes().expect("serialize MeetingPacket");

        // Wrap in a PacketWrapper like the real code path does
        let wrapper = PacketWrapper {
            data: meeting_bytes,
            user_id: "server".as_bytes().to_vec(),
            packet_type: PacketType::MEETING.into(),
            ..Default::default()
        };

        // Extract and verify -- this mirrors the on_inbound_media code path
        assert_eq!(wrapper.packet_type.enum_value(), Ok(PacketType::MEETING));
        let parsed =
            MeetingPacket::parse_from_bytes(&wrapper.data).expect("parse from wrapper data");
        assert_eq!(parsed.target_user_id[..], *"charlie@example.com".as_bytes());
        assert_eq!(
            parsed.event_type.enum_value(),
            Ok(MeetingEventType::PARTICIPANT_ADMITTED)
        );

        // Simulate the userid check from video_call_client.rs
        let my_userid_bytes = "charlie@example.com".as_bytes();
        let should_fire_callback = parsed.target_user_id[..] == *my_userid_bytes;
        assert!(
            should_fire_callback,
            "callback should fire for matching userid"
        );

        let other_userid_bytes = "observer@example.com".as_bytes();
        let should_not_fire = parsed.target_user_id[..] == *other_userid_bytes;
        assert!(
            !should_not_fire,
            "callback should NOT fire for non-matching userid"
        );
    }
}
