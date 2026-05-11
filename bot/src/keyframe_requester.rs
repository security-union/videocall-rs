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
 */

//! Keyframe request logic for the synthetic bot.
//!
//! When a browser joins a meeting it sends a `KEYFRAME_REQUEST` to each peer
//! to get an IDR frame so video can start decoding immediately. The bot mimics
//! this behavior: after first seeing a new peer, it sends exactly one
//! KEYFRAME_REQUEST for VIDEO to that peer.
//!
//! Wire format matches the browser client at
//! `videocall-client/src/decode/peer_decode_manager.rs:1139`:
//!
//! ```text
//! MediaPacket {
//!     media_type: KEYFRAME_REQUEST,
//!     user_id: target_peer.as_bytes(),  // WHO to request from
//!     data: b"VIDEO",                   // which stream
//!     ..
//! }
//! PacketWrapper {
//!     packet_type: MEDIA,
//!     user_id: self_user_id.as_bytes(), // WHO is requesting
//!     data: <serialized MediaPacket>,
//!     ..
//! }
//! ```

use protobuf::Message;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tracing::{info, warn};

use crate::transport::{MediaTypeLabel, OutboundFrame};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Tracks which peers have received a keyframe request and sends requests for
/// newly discovered peers.
pub struct KeyframeRequester {
    /// Our own user id (the requester).
    self_user_id: String,
    /// Set of peers we have already sent a keyframe request to.
    requested_peers: HashSet<String>,
    /// Channel to send outbound packets.
    packet_tx: Sender<OutboundFrame>,
    /// Counter for total keyframe requests sent (shared with health reporter).
    pub requests_sent: Arc<AtomicU64>,
}

impl KeyframeRequester {
    /// Create a new keyframe requester.
    pub fn new(self_user_id: String, packet_tx: Sender<OutboundFrame>) -> Self {
        Self {
            self_user_id,
            requested_peers: HashSet::new(),
            packet_tx,
            requests_sent: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns the shared atomic counter for keyframe requests sent.
    pub fn requests_sent_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.requests_sent)
    }

    /// Called when a packet from a peer is observed. If this is the first time
    /// we see this peer, sends a KEYFRAME_REQUEST for VIDEO.
    pub fn on_peer_seen(&mut self, peer_user_id: &str) {
        // Don't request keyframes from ourselves.
        if peer_user_id == self.self_user_id {
            return;
        }

        if self.requested_peers.contains(peer_user_id) {
            return;
        }

        match build_keyframe_request(&self.self_user_id, peer_user_id) {
            Ok(bytes) => {
                let frame = OutboundFrame::new(MediaTypeLabel::Other, bytes);
                if self.packet_tx.try_send(frame).is_err() {
                    warn!(
                        "[{}] Failed to send KEYFRAME_REQUEST to {} (channel full, will retry)",
                        self.self_user_id, peer_user_id
                    );
                } else {
                    self.requested_peers.insert(peer_user_id.to_string());
                    self.requests_sent.fetch_add(1, Ordering::Relaxed);
                    info!(
                        "[{}] Sent KEYFRAME_REQUEST to peer {}",
                        self.self_user_id, peer_user_id
                    );
                }
            }
            Err(e) => {
                warn!(
                    "[{}] Failed to build KEYFRAME_REQUEST for {}: {}",
                    self.self_user_id, peer_user_id, e
                );
            }
        }
    }
}

/// Build a serialized KEYFRAME_REQUEST `PacketWrapper`.
fn build_keyframe_request(self_user_id: &str, target_peer_id: &str) -> anyhow::Result<Vec<u8>> {
    let media_packet = MediaPacket {
        media_type: MediaType::KEYFRAME_REQUEST.into(),
        user_id: target_peer_id.as_bytes().to_vec(),
        data: b"VIDEO".to_vec(),
        ..Default::default()
    };
    let media_data = media_packet.write_to_bytes()?;

    let wrapper = PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        user_id: self_user_id.as_bytes().to_vec(),
        data: media_data,
        ..Default::default()
    };

    Ok(wrapper.write_to_bytes()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn build_keyframe_request_produces_valid_packet() {
        let bytes = build_keyframe_request("bot-1", "alice").expect("build");
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).expect("parse wrapper");
        assert_eq!(wrapper.packet_type.enum_value(), Ok(PacketType::MEDIA));
        assert_eq!(wrapper.user_id, b"bot-1");

        let media = MediaPacket::parse_from_bytes(&wrapper.data).expect("parse media");
        assert_eq!(
            media.media_type.enum_value(),
            Ok(MediaType::KEYFRAME_REQUEST)
        );
        assert_eq!(media.user_id, b"alice");
        assert_eq!(media.data, b"VIDEO");
    }

    #[tokio::test]
    async fn requester_sends_once_per_peer() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut req = KeyframeRequester::new("bot-1".to_string(), tx);

        // First time seeing alice — should send
        req.on_peer_seen("alice");
        assert!(rx.try_recv().is_ok());
        assert_eq!(req.requests_sent.load(Ordering::Relaxed), 1);

        // Second time seeing alice — should NOT send
        req.on_peer_seen("alice");
        assert!(rx.try_recv().is_err());
        assert_eq!(req.requests_sent.load(Ordering::Relaxed), 1);

        // First time seeing bob — should send
        req.on_peer_seen("bob");
        assert!(rx.try_recv().is_ok());
        assert_eq!(req.requests_sent.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn requester_does_not_request_from_self() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut req = KeyframeRequester::new("bot-1".to_string(), tx);

        req.on_peer_seen("bot-1");
        assert!(rx.try_recv().is_err());
        assert_eq!(req.requests_sent.load(Ordering::Relaxed), 0);
    }
}
