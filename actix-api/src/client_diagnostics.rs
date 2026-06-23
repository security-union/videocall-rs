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

//! Health packet processing and Prometheus metrics collection

pub mod health_processor {
    use actix_web::{HttpResponse, Responder};
    use prometheus::{Encoder, TextEncoder};
    use protobuf::Message;
    use tracing::{debug, warn};

    // Health data structure matching RFC design
    // Use protobuf HealthPacket on the wire; keep simple structs only if needed internally.
    use videocall_types::protos::health_packet::HealthPacket as PbHealthPacket;
    use videocall_types::user_id_bytes_to_string;

    /// Process a health packet and update Prometheus metrics
    pub fn process_health_packet(health_data: &PbHealthPacket, client: async_nats::client::Client) {
        debug!(
            "Publishing health report from {} in session {} for meeting {} to NATS",
            user_id_bytes_to_string(&health_data.reporting_user_id),
            health_data.session_id,
            health_data.meeting_id
        );

        // Publish to NATS instead of processing locally
        publish_health_to_nats(health_data.clone(), client);
    }

    fn publish_health_to_nats(health_data: PbHealthPacket, client: async_nats::client::Client) {
        tokio::spawn(async move {
            if let Err(e) = publish_health_to_nats_async(health_data, client).await {
                warn!("Failed to publish health data to NATS: {}", e);
            }
        });
    }

    async fn publish_health_to_nats_async(
        mut health_data: PbHealthPacket,
        client: async_nats::client::Client,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let region = std::env::var("REGION").unwrap_or_else(|_| "us-east".to_string());
        let server_id = std::env::var("SERVER_ID").unwrap_or_else(|_| "server-1".to_string());
        let service_type =
            std::env::var("SERVICE_TYPE").unwrap_or_else(|_| "websocket".to_string());

        let topic = format!("health.diagnostics.{region}.{service_type}.{server_id}");

        // SECURITY: scrub any URL fields the client may have populated. The client
        // should NOT include the lobby URL here (it carries the JWT in the query
        // string), but defense-in-depth requires the relay to strip it
        // unconditionally — older or non-conformant clients may still emit it.
        // See PR #570 (Phase 1) and security-audit follow-up F4.
        scrub_client_supplied_urls(&mut health_data);

        let payload = health_data.write_to_bytes()?;
        client.publish(topic.clone(), payload.into()).await?;
        debug!("Published health data to NATS topic: {}", topic);
        Ok(())
    }

    /// Strip any client-supplied URL fields from a `HealthPacket` before it is
    /// re-published onto NATS. The lobby URL carries the room JWT in its query
    /// string; republishing it would leak credentials to anyone with read
    /// access to the telemetry pipeline. This is defense-in-depth against
    /// stale or non-conformant clients that may still populate the field.
    ///
    /// `active_server_url` is the only URL-typed field defined in
    /// `health_packet.proto` today. If new URL-typed fields are added, scrub
    /// them here as well.
    pub(crate) fn scrub_client_supplied_urls(pb: &mut PbHealthPacket) {
        pb.active_server_url = String::new();
    }

    /// Parse health packet from JSON bytes
    pub fn parse_health_packet(data: &[u8]) -> Result<PbHealthPacket, protobuf::Error> {
        PbHealthPacket::parse_from_bytes(data)
    }

    /// Prometheus metrics endpoint handler
    pub async fn metrics_handler() -> impl Responder {
        let encoder = TextEncoder::new();
        let metric_families = prometheus::gather();
        let mut buffer = Vec::new();

        match encoder.encode(&metric_families, &mut buffer) {
            Ok(()) => HttpResponse::Ok()
                .content_type("text/plain; version=0.0.4")
                .body(buffer),
            Err(e) => {
                warn!("Error encoding metrics: {}", e);
                HttpResponse::InternalServerError().body("Error encoding metrics")
            }
        }
    }

    /// Check if a packet is a health packet
    pub fn is_health_packet(
        packet_wrapper: &videocall_types::protos::packet_wrapper::PacketWrapper,
    ) -> bool {
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        packet_wrapper.packet_type == PacketType::HEALTH.into()
    }

    /// Check if the binary data is a health packet that should be processed
    pub fn is_health_packet_bytes(data: &[u8]) -> bool {
        use protobuf::Message;
        use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            return packet_wrapper.packet_type == PacketType::HEALTH.into();
        }
        false
    }

    /// Process health packet for diagnostics collection from binary data
    pub fn process_health_packet_bytes(data: &[u8], nc: async_nats::client::Client) {
        use protobuf::Message;
        use tracing::{debug, error};
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            if is_health_packet(&packet_wrapper) {
                debug!("Processing health packet");

                match parse_health_packet(&packet_wrapper.data) {
                    Ok(health_data) => {
                        process_health_packet(&health_data, nc.clone());
                        debug!(
                            "Successfully processed health data for session {}",
                            health_data.session_id
                        );
                    }
                    Err(e) => {
                        error!("Failed to parse health packet: {}", e);
                    }
                }
            }
        }
    }

    /// Build the PEER-FACING trimmed copy of a HEALTH `PacketWrapper`'s bytes
    /// (#1543).
    ///
    /// #1482 (PR #1540) forwards every peer's full [`PbHealthPacket`] to every
    /// OTHER peer so each receiver can render the sender's self-reported
    /// device/hardware metrics ("Device" UI). But the full packet carries a
    /// heavy per-peer `peer_stats` map (one [`PeerStats`] per remote peer, each
    /// with nested NetEq/Video stats) plus dozens of encoder/transport telemetry
    /// fields the PEER UI never reads. Fanning all of that out is O(N²) aggregate
    /// relay egress at scale (LOW at N<=10, MEDIUM at N>=20).
    ///
    /// The peer receiver reads EXACTLY the seven device fields below — see
    /// `videocall-client`'s `video_call_client::handle_inbound_packet`
    /// `PacketType::HEALTH` arm, which populates `PeerDeviceInfo` and nothing
    /// else from a forwarded HealthPacket (it keys by the OUTER
    /// `PacketWrapper.session_id`, not any inner field). So this builds a fresh
    /// HealthPacket carrying ONLY those seven fields (an ALLOWLIST, not a
    /// `peer_stats`-only denylist): any future heavy field added to the proto is
    /// excluded by default rather than silently re-introducing the N² fan-out.
    ///
    /// KEPT (the peer UI reads these):
    ///   * `client_cores`             (field 56)
    ///   * `client_architecture`      (field 57)
    ///   * `client_os`                (field 87)
    ///   * `client_device_type`       (field 88)
    ///   * `client_main_thread_load`  (field 89)
    ///   * `memory_used_bytes`        (field 12 -> client_memory_used_mb)
    ///   * `client_device_memory_gb`  (field 90)
    ///
    /// DROPPED (NOT read by the peer UI): `peer_stats` (the heavy map) and every
    /// other field, including identity scalars (session_id/meeting_id/
    /// reporting_user_id/timestamp_ms) — the receiver keys device info by the
    /// outer wrapper's session_id, so the inner identity fields are unused.
    ///
    /// This is the PEER fan-out path ONLY. The server-side NATS telemetry path
    /// ([`process_health_packet_bytes`] -> [`process_health_packet`]) parses the
    /// ORIGINAL `data` independently and keeps the FULL packet (operators rely on
    /// `peer_stats`). This function never touches that path.
    ///
    /// Cost: ONE parse + ONE re-serialize per inbound HEALTH (O(1)), regardless
    /// of room size. The trimmed bytes are produced ONCE here, BEFORE the relay's
    /// per-recipient fan-out (`ChatServer::handle<ClientMessage>` publishes the
    /// single payload onto NATS), so the trim is never repeated per recipient.
    ///
    /// Fail-safe: if `data` is not a parseable HEALTH `PacketWrapper`, the
    /// original bytes are returned unchanged so forwarding still works.
    pub fn trim_health_packet_for_peers(data: &[u8]) -> Vec<u8> {
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let Ok(mut wrapper) = PacketWrapper::parse_from_bytes(data) else {
            return data.to_vec();
        };
        if !is_health_packet(&wrapper) {
            return data.to_vec();
        }
        let Ok(full) = parse_health_packet(&wrapper.data) else {
            return data.to_vec();
        };

        // ALLOWLIST: copy ONLY the seven device fields the peer UI reads.
        let mut trimmed = PbHealthPacket::new();
        trimmed.client_cores = full.client_cores;
        trimmed.client_architecture = full.client_architecture;
        trimmed.client_os = full.client_os;
        trimmed.client_device_type = full.client_device_type;
        trimmed.client_main_thread_load = full.client_main_thread_load;
        trimmed.memory_used_bytes = full.memory_used_bytes;
        trimmed.client_device_memory_gb = full.client_device_memory_gb;

        match trimmed.write_to_bytes() {
            Ok(inner_bytes) => {
                wrapper.data = inner_bytes;
                wrapper
                    .write_to_bytes()
                    // Fail-safe: a re-serialize failure (should never happen for
                    // an already-parsed wrapper) falls back to the full bytes so
                    // device metrics still reach peers.
                    .unwrap_or_else(|_| data.to_vec())
            }
            Err(_) => data.to_vec(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use protobuf::Message;
        use std::collections::HashMap;
        use videocall_types::protos::health_packet::{
            HealthPacket as PbHealthPacket, NetEqStats as PbNetEqStats, PeerStats as PbPeerStats,
            VideoStats as PbVideoStats,
        };

        /// Build a fully populated HealthPacket — including a JWT-bearing
        /// `active_server_url` — so tests can assert that the scrubber clears
        /// only the URL field while leaving every other field untouched.
        ///
        /// This fixture intentionally exercises every string-typed field in
        /// the proto so the round-trip pass-through assertion is meaningful.
        fn make_packet_with_jwt_url() -> PbHealthPacket {
            let mut hp = PbHealthPacket::new();
            hp.session_id = "session-abc-123".to_string();
            hp.meeting_id = "meeting-xyz-789".to_string();
            hp.reporting_user_id = b"alice@example.com".to_vec();
            hp.timestamp_ms = 1_700_000_000_000;
            hp.reporting_audio_enabled = true;
            hp.reporting_video_enabled = true;

            // The leak we are defending against: the lobby URL embeds a JWT
            // in the query string and would otherwise be republished onto NATS.
            hp.active_server_url =
                "https://relay.example.com/lobby/m1/u1?token=eyJhbGciOiJIUzI1NiJ9.payload.sig"
                    .to_string();
            hp.active_server_type = "webtransport".to_string();
            hp.active_server_rtt_ms = 42.5;

            hp.is_tab_visible = true;
            hp.is_tab_throttled = false;
            hp.display_name = Some("Alice on Laptop".to_string());

            // One peer-stats entry to make sure nested message fields survive.
            let mut peer = PbPeerStats::new();
            peer.can_listen = true;
            peer.can_see = true;
            peer.audio_enabled = true;
            peer.video_enabled = true;
            let mut neteq = PbNetEqStats::new();
            neteq.current_buffer_size_ms = 75.0;
            neteq.target_delay_ms = 50.0;
            peer.neteq_stats = protobuf::MessageField::some(neteq);
            let mut video = PbVideoStats::new();
            video.fps_received = 29.5;
            video.bitrate_kbps = 1200;
            peer.video_stats = protobuf::MessageField::some(video);

            let mut peer_stats = HashMap::new();
            peer_stats.insert("peer-bob-456".to_string(), peer);
            hp.peer_stats = peer_stats;

            hp
        }

        /// F4 regression test — simulating the relay path: a stale or
        /// non-conformant client populates `active_server_url` with a
        /// JWT-bearing URL; the scrubber MUST blank it before serialization
        /// so the bytes published to NATS contain no credential material.
        #[test]
        fn scrub_clears_active_server_url_before_serialization() {
            let mut hp = make_packet_with_jwt_url();
            assert!(
                hp.active_server_url.contains("token="),
                "test fixture should embed a token to make the assertion meaningful"
            );

            scrub_client_supplied_urls(&mut hp);

            // The bytes that would be published to NATS:
            let bytes = hp.write_to_bytes().expect("serialize scrubbed packet");

            // Round-trip parse and assert the URL field is empty.
            let parsed =
                PbHealthPacket::parse_from_bytes(&bytes).expect("parse round-tripped bytes");
            assert!(
                parsed.active_server_url.is_empty(),
                "active_server_url must be empty after scrub, got: {:?}",
                parsed.active_server_url
            );

            // Belt-and-braces: the raw bytes must not contain the JWT string
            // anywhere — protobuf encodes string fields with their UTF-8 bytes,
            // so a substring match catches accidental leaks via any unexpected
            // string field too.
            let bytes_as_str = String::from_utf8_lossy(&bytes);
            assert!(
                !bytes_as_str.contains("token="),
                "serialized bytes must not contain 'token=' anywhere"
            );
            assert!(
                !bytes_as_str.contains("eyJhbGciOiJIUzI1NiJ9"),
                "serialized bytes must not contain the JWT header"
            );
        }

        /// Asserts that the scrub only touches `active_server_url` and leaves
        /// every other field — including non-URL string fields, numeric
        /// fields, booleans, optionals, and nested messages — passing through
        /// unchanged. Without this guarantee the relay scrub could silently
        /// degrade unrelated diagnostics.
        #[test]
        fn scrub_preserves_non_url_fields() {
            let original = make_packet_with_jwt_url();

            let mut scrubbed = original.clone();
            scrub_client_supplied_urls(&mut scrubbed);

            // Identifiers / metadata
            assert_eq!(scrubbed.session_id, original.session_id);
            assert_eq!(scrubbed.meeting_id, original.meeting_id);
            assert_eq!(scrubbed.reporting_user_id, original.reporting_user_id);
            assert_eq!(scrubbed.timestamp_ms, original.timestamp_ms);

            // Reporting flags
            assert_eq!(
                scrubbed.reporting_audio_enabled,
                original.reporting_audio_enabled
            );
            assert_eq!(
                scrubbed.reporting_video_enabled,
                original.reporting_video_enabled
            );

            // Active connection fields OTHER than the URL
            assert_eq!(scrubbed.active_server_type, original.active_server_type);
            assert_eq!(scrubbed.active_server_rtt_ms, original.active_server_rtt_ms);

            // Tab / display
            assert_eq!(scrubbed.is_tab_visible, original.is_tab_visible);
            assert_eq!(scrubbed.is_tab_throttled, original.is_tab_throttled);
            assert_eq!(scrubbed.display_name, original.display_name);

            // Nested per-peer stats survive intact (proves the scrub is
            // surgical, not a wholesale wipe).
            assert_eq!(scrubbed.peer_stats.len(), original.peer_stats.len());
            let scrubbed_peer = scrubbed
                .peer_stats
                .get("peer-bob-456")
                .expect("peer entry preserved");
            let original_peer = original
                .peer_stats
                .get("peer-bob-456")
                .expect("peer entry in original");
            assert_eq!(scrubbed_peer.can_listen, original_peer.can_listen);
            assert_eq!(scrubbed_peer.can_see, original_peer.can_see);
            assert_eq!(scrubbed_peer.audio_enabled, original_peer.audio_enabled);
            assert_eq!(scrubbed_peer.video_enabled, original_peer.video_enabled);
            assert_eq!(
                scrubbed_peer.neteq_stats.target_delay_ms,
                original_peer.neteq_stats.target_delay_ms
            );
            assert_eq!(
                scrubbed_peer.video_stats.bitrate_kbps,
                original_peer.video_stats.bitrate_kbps
            );

            // And the only thing that changed is the URL.
            assert!(scrubbed.active_server_url.is_empty());
            assert!(!original.active_server_url.is_empty());
        }

        // ==================================================================
        // #1543: peer-facing HEALTH trim
        // ==================================================================

        use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

        /// Build a HEALTH `PacketWrapper` whose inner `HealthPacket` carries
        /// BOTH the heavy `peer_stats` map AND all seven device fields the peer
        /// UI reads, so the trim can be asserted to drop the former and keep the
        /// latter. Returns the serialized wrapper bytes (what the relay forwards).
        fn make_health_wrapper_with_peer_stats_and_device_fields() -> Vec<u8> {
            let mut hp = PbHealthPacket::new();
            hp.session_id = "session-abc-123".to_string();
            hp.meeting_id = "meeting-xyz-789".to_string();
            hp.reporting_user_id = b"alice@example.com".to_vec();
            hp.timestamp_ms = 1_700_000_000_000;

            // Heavy field that must be DROPPED for peers.
            let mut peer = PbPeerStats::new();
            peer.can_listen = true;
            peer.can_see = true;
            let mut neteq = PbNetEqStats::new();
            neteq.current_buffer_size_ms = 75.0;
            peer.neteq_stats = protobuf::MessageField::some(neteq);
            let mut video = PbVideoStats::new();
            video.fps_received = 29.5;
            video.bitrate_kbps = 1200;
            peer.video_stats = protobuf::MessageField::some(video);
            let mut peer_stats = HashMap::new();
            peer_stats.insert("peer-bob-456".to_string(), peer);
            hp.peer_stats = peer_stats;

            // The seven device fields the peer UI reads — must be PRESERVED.
            hp.client_cores = Some(8);
            hp.client_architecture = Some("arm".to_string());
            hp.client_os = Some("macOS 14.5".to_string());
            hp.client_device_type = Some("desktop".to_string());
            hp.client_main_thread_load = Some(0.42);
            hp.memory_used_bytes = Some(123_456_789);
            hp.client_device_memory_gb = Some(8.0);

            let inner = hp.write_to_bytes().expect("serialize inner HealthPacket");
            let mut wrapper = PacketWrapper::new();
            wrapper.packet_type = PacketType::HEALTH.into();
            wrapper.data = inner;
            wrapper.session_id = 42;
            wrapper.write_to_bytes().expect("serialize HEALTH wrapper")
        }

        /// The peer-facing trim MUST drop the heavy `peer_stats` map while
        /// preserving every device field the peer UI renders (#1543).
        ///
        /// Mutation coverage:
        ///   * If the trim is removed (peer fan-out keeps the full bytes), the
        ///     trimmed inner would still carry `peer_stats` and the
        ///     `peer_stats.is_empty()` assert fails.
        ///   * If any device field is wrongly dropped from the allowlist, its
        ///     `Some(..)` assert fails.
        ///   * The byte-size assert fails if the trim is a no-op (full >> trimmed).
        #[test]
        fn trim_drops_peer_stats_keeps_device_fields() {
            let full_bytes = make_health_wrapper_with_peer_stats_and_device_fields();
            let trimmed_bytes = trim_health_packet_for_peers(&full_bytes);

            // The trimmed wire form must be materially smaller than the full one.
            assert!(
                trimmed_bytes.len() < full_bytes.len(),
                "trimmed wrapper ({} bytes) must be smaller than full ({} bytes)",
                trimmed_bytes.len(),
                full_bytes.len(),
            );

            // The outer wrapper must still be a parseable HEALTH packet.
            let trimmed_wrapper = PacketWrapper::parse_from_bytes(&trimmed_bytes)
                .expect("trimmed wrapper must parse");
            assert_eq!(trimmed_wrapper.packet_type, PacketType::HEALTH.into());
            // Peer-attribution contract (#1543): the receiver keys PeerDeviceInfo
            // by the OUTER PacketWrapper.session_id, so the trim must preserve it.
            // Regression insurance: a refactor that rebuilds the wrapper from
            // scratch (dropping session_id) instead of reusing the parsed one
            // would fail here.
            assert_eq!(trimmed_wrapper.session_id, 42);

            let trimmed = PbHealthPacket::parse_from_bytes(&trimmed_wrapper.data)
                .expect("trimmed inner HealthPacket must parse");

            // DROPPED: the heavy per-peer map must be gone.
            assert!(
                trimmed.peer_stats.is_empty(),
                "peer_stats must be empty in the peer-facing trimmed packet"
            );

            // KEPT: every device field the peer UI reads.
            assert_eq!(trimmed.client_cores, Some(8));
            assert_eq!(trimmed.client_architecture, Some("arm".to_string()));
            assert_eq!(trimmed.client_os, Some("macOS 14.5".to_string()));
            assert_eq!(trimmed.client_device_type, Some("desktop".to_string()));
            assert_eq!(trimmed.client_main_thread_load, Some(0.42));
            assert_eq!(trimmed.memory_used_bytes, Some(123_456_789));
            assert_eq!(trimmed.client_device_memory_gb, Some(8.0));
        }

        /// The trim must NOT mutate the caller's bytes: the SAME `data` is passed
        /// to the server-side NATS telemetry path, which must still see the FULL
        /// packet (operators rely on `peer_stats`). Proven by re-parsing the
        /// original bytes after trimming and asserting `peer_stats` survives.
        #[test]
        fn trim_does_not_mutate_full_packet_for_nats_path() {
            let full_bytes = make_health_wrapper_with_peer_stats_and_device_fields();
            let _ = trim_health_packet_for_peers(&full_bytes);

            // The NATS path re-parses the ORIGINAL bytes; peer_stats must remain.
            let wrapper =
                PacketWrapper::parse_from_bytes(&full_bytes).expect("full wrapper parses");
            let full = PbHealthPacket::parse_from_bytes(&wrapper.data)
                .expect("full inner HealthPacket parses");
            assert_eq!(
                full.peer_stats.len(),
                1,
                "the original full packet (NATS telemetry path) must keep peer_stats"
            );
            assert!(full.peer_stats.contains_key("peer-bob-456"));
        }

        /// Fail-safe: a non-HEALTH or unparseable payload is forwarded UNCHANGED
        /// so the trim never breaks the forward path for other packet types.
        #[test]
        fn trim_passes_through_non_health_and_unparseable() {
            // Unparseable garbage round-trips unchanged.
            let garbage = vec![0xFF, 0x00, 0x13, 0x37];
            assert_eq!(trim_health_packet_for_peers(&garbage), garbage);

            // A well-formed but NON-HEALTH wrapper is forwarded unchanged.
            let mut media = PacketWrapper::new();
            media.packet_type = PacketType::MEDIA.into();
            media.data = vec![1, 2, 3, 4];
            media.session_id = 7;
            let media_bytes = media.write_to_bytes().expect("serialize MEDIA wrapper");
            assert_eq!(trim_health_packet_for_peers(&media_bytes), media_bytes);
        }
    }
}
