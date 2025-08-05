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
//! Only compiled when the "diagnostics" feature is enabled

#[cfg(feature = "diagnostics")]
pub mod health_processor {
    use actix_web::{HttpResponse, Responder};
    use lazy_static::lazy_static;
    use prometheus::{
        register_counter, register_gauge, register_histogram, Counter, Encoder, Gauge, Histogram,
        TextEncoder,
    };
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use tracing::{debug, warn};

    // Health data structure matching RFC design
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PeerHealthData {
        pub session_id: String,
        pub reporting_peer: String,
        pub timestamp_ms: u64,
        pub peer_stats: HashMap<String, PeerStats>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PeerStats {
        pub can_listen: bool,
        pub can_see: bool,
        pub neteq_stats: Option<serde_json::Value>,
        pub video_stats: Option<serde_json::Value>,
    }

    // Prometheus metrics
    lazy_static! {
        static ref HEALTH_REPORTS_TOTAL: Counter = register_counter!(
            "videocall_health_reports_total",
            "Total number of health reports received"
        )
        .unwrap();
        static ref ACTIVE_SESSIONS: Gauge = register_gauge!(
            "videocall_active_sessions_total",
            "Number of active video call sessions"
        )
        .unwrap();
        static ref PEER_CONNECTIONS: Gauge = register_gauge!(
            "videocall_peer_connections_total",
            "Number of active peer connections"
        )
        .unwrap();
        static ref AUDIO_RECEPTION_FAILURES: Counter = register_counter!(
            "videocall_audio_reception_failures_total",
            "Number of peers unable to receive audio"
        )
        .unwrap();
        static ref VIDEO_RECEPTION_FAILURES: Counter = register_counter!(
            "videocall_video_reception_failures_total",
            "Number of peers unable to receive video"
        )
        .unwrap();
        static ref SESSION_QUALITY: Histogram = register_histogram!(
            "videocall_session_quality_score",
            "Overall session quality scores (0.0-1.0)",
            vec![0.1, 0.3, 0.5, 0.7, 0.9, 1.0]
        )
        .unwrap();
    }

    /// Process a health packet and update Prometheus metrics
    pub fn process_health_packet(health_data: &PeerHealthData) {
        HEALTH_REPORTS_TOTAL.inc();

        debug!(
            "Processing health report from {} in session {} with {} peer connections",
            health_data.reporting_peer,
            health_data.session_id,
            health_data.peer_stats.len()
        );

        // Update peer connection count
        PEER_CONNECTIONS.set(health_data.peer_stats.len() as f64);

        // Analyze connectivity and update metrics
        let mut audio_failures = 0;
        let mut video_failures = 0;
        let mut quality_scores = Vec::new();

        for (remote_peer, stats) in &health_data.peer_stats {
            if !stats.can_listen {
                audio_failures += 1;
                AUDIO_RECEPTION_FAILURES.inc();
                debug!(
                    "Audio reception failure: {} cannot hear {}",
                    health_data.reporting_peer, remote_peer
                );
            }

            if !stats.can_see {
                video_failures += 1;
                VIDEO_RECEPTION_FAILURES.inc();
                debug!(
                    "Video reception failure: {} cannot see {}",
                    health_data.reporting_peer, remote_peer
                );
            }

            // Calculate quality score from NetEQ stats if available
            if let Some(neteq_stats) = &stats.neteq_stats {
                if let Some(quality) = extract_audio_quality(neteq_stats) {
                    quality_scores.push(quality);
                }
            }
        }

        // Record overall session quality (average of peer qualities)
        if !quality_scores.is_empty() {
            let avg_quality: f64 = quality_scores.iter().sum::<f64>() / quality_scores.len() as f64;
            SESSION_QUALITY.observe(avg_quality);
        }

        debug!(
            "Health metrics updated: {} audio failures, {} video failures, quality_scores: {:?}",
            audio_failures, video_failures, quality_scores
        );
    }

    /// Extract audio quality score from NetEQ stats (0.0 = poor, 1.0 = excellent)
    fn extract_audio_quality(neteq_stats: &serde_json::Value) -> Option<f64> {
        // Extract expand_rate and accel_rate from NetEQ stats
        let expand_rate = neteq_stats.get("expand_rate")?.as_f64().unwrap_or(0.0);
        let accel_rate = neteq_stats.get("accel_rate")?.as_f64().unwrap_or(0.0);

        // Quality score: 1.0 - (expand + accelerate rates as fraction)
        // expand_rate and accel_rate are in per-mille (‰), so divide by 1000
        let quality = 1.0 - ((expand_rate + accel_rate) / 1000.0).min(1.0);
        Some(quality.max(0.0))
    }

    /// Parse health packet from JSON bytes
    pub fn parse_health_packet(data: &[u8]) -> Result<PeerHealthData, serde_json::Error> {
        serde_json::from_slice(data)
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
    pub fn process_health_packet_bytes(data: &[u8]) {
        use protobuf::Message;
        use tracing::{debug, error};
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            if is_health_packet(&packet_wrapper) {
                debug!("Processing health packet");

                match parse_health_packet(&packet_wrapper.data) {
                    Ok(health_data) => {
                        process_health_packet(&health_data);
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
}

#[cfg(not(feature = "diagnostics"))]
pub mod health_processor {
    use actix_web::{HttpResponse, Responder};
    use serde_json;

    // Minimal stub for when diagnostics is disabled
    #[derive(Debug)]
    pub struct PeerHealthData {
        pub session_id: String,
    }

    /// Parse health packet but return minimal data when disabled
    pub fn parse_health_packet(_data: &[u8]) -> Result<PeerHealthData, serde_json::Error> {
        Ok(PeerHealthData {
            session_id: "disabled".to_string(),
        })
    }

    /// No-op when diagnostics feature is disabled
    pub fn process_health_packet(_data: &PeerHealthData) {
        // No-op when diagnostics disabled
    }

    /// Return empty metrics when disabled
    pub async fn metrics_handler() -> impl Responder {
        HttpResponse::Ok()
            .content_type("text/plain")
            .body("# Diagnostics feature disabled\n")
    }

    /// Check if a packet is a health packet (stub)
    pub fn is_health_packet_bytes(_data: &[u8]) -> bool {
        false
    }

    /// Process health packet for diagnostics collection from binary data (stub)
    pub fn process_health_packet_bytes(_data: &[u8]) {
        // No-op when diagnostics disabled
    }
}
