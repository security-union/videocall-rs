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

    /// Process a health packet and update Prometheus metrics
    pub fn process_health_packet(health_data: &PbHealthPacket, client: async_nats::client::Client) {
        debug!(
            "Publishing health report from {} in session {} for meeting {} to NATS",
            health_data.reporting_peer, health_data.session_id, health_data.meeting_id
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
        health_data: PbHealthPacket,
        client: async_nats::client::Client,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let region = std::env::var("REGION").unwrap_or_else(|_| "us-east".to_string());
        let server_id = std::env::var("SERVER_ID").unwrap_or_else(|_| "server-1".to_string());
        let service_type =
            std::env::var("SERVICE_TYPE").unwrap_or_else(|_| "websocket".to_string());

        let topic = format!("health.diagnostics.{region}.{service_type}.{server_id}");

        let payload = health_data.write_to_bytes()?;
        client.publish(topic.clone(), payload.into()).await?;
        debug!("Published health data to NATS topic: {}", topic);
        Ok(())
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
}
