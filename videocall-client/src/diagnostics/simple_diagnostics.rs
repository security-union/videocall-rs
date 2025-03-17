use std::collections::HashMap;
use std::cell::RefCell;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use log::{debug, info, warn};
use protobuf::Message;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::packet_wrapper::{PacketWrapper, packet_wrapper::PacketType};
use videocall_types::protos::diagnostics_packet::{
    DiagnosticsPacket, VideoMetrics, AudioMetrics, QualityHints,
    diagnostics_packet::MediaType as DiagMediaType
};
use js_sys::Date;

/// A simple struct to track metrics for an individual stream
#[derive(Debug, Clone)]
pub struct StreamMetrics {
    // Common metrics
    pub peer_id: String,
    pub packet_count: u32,
    pub packets_lost: u32,
    pub last_packet_time: Option<f64>,
    pub bytes_received: u64,
    pub last_bytes_check: f64,
    
    // Video metrics
    pub frames_received: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub freeze_count: u32,
    
    // Estimated metrics
    pub estimated_bandwidth_kbps: u32,
    pub packet_loss_percent: f32,
    pub jitter_ms: u32,
    pub media_type: MediaType,
}

impl StreamMetrics {
    pub fn new(peer_id: String, media_type: MediaType) -> Self {
        debug!("Creating new StreamMetrics for peer: {}, media_type: {:?}", peer_id, media_type);
        Self {
            peer_id,
            media_type,
            packet_count: 0,
            packets_lost: 0,
            last_packet_time: None,
            bytes_received: 0,
            last_bytes_check: Date::now(),
            frames_received: 0,
            frame_width: 0,
            frame_height: 0,
            freeze_count: 0,
            estimated_bandwidth_kbps: 0,
            packet_loss_percent: 0.0,
            jitter_ms: 0,
        }
    }
    
    /// Update metrics when a packet is received
    pub fn update_packet_received(&mut self, size: usize) {
        self.packet_count += 1;
        self.bytes_received += size as u64;
        
        // Update estimated bandwidth
        let now = Date::now();
        let elapsed_ms = now - self.last_bytes_check;
        if elapsed_ms > 1000.0 {
            let elapsed_sec = elapsed_ms / 1000.0;
            self.estimated_bandwidth_kbps = ((self.bytes_received * 8) / 1024) as u32 / elapsed_sec.max(1.0) as u32;
            debug!(
                "Bandwidth estimate updated - Peer: {}, MediaType: {:?}, Bandwidth: {} kbps, Bytes: {}, Time: {:.2}s",
                self.peer_id, self.media_type, self.estimated_bandwidth_kbps, self.bytes_received, elapsed_sec
            );
            self.last_bytes_check = now;
        }
        
        self.last_packet_time = Some(now);

        if self.packet_count % 100 == 0 {
            debug!(
                "Received {} packets from peer {} ({:?}), size: {} bytes, total: {} bytes",
                self.packet_count, self.peer_id, self.media_type, size, self.bytes_received
            );
        }
    }
    
    /// Update metrics when a video frame is decoded
    pub fn update_video_frame(&mut self, width: u32, height: u32) {
        self.frames_received += 1;
        self.frame_width = width;
        self.frame_height = height;
        
        if self.frames_received % 30 == 0 {
            debug!(
                "Received {} frames from peer {} at resolution {}x{}", 
                self.frames_received, self.peer_id, width, height
            );
        }
    }
    
    /// Update metrics when a packet is lost
    pub fn update_packet_lost(&mut self) {
        self.packets_lost += 1;
        self.update_packet_loss_percent();
        
        debug!(
            "Packet loss for peer {} ({:?}): {}/{} packets ({}%)", 
            self.peer_id, self.media_type, self.packets_lost, 
            self.packet_count + self.packets_lost, self.packet_loss_percent
        );
    }
    
    /// Calculate the packet loss percentage
    fn update_packet_loss_percent(&mut self) {
        if self.packet_count == 0 {
            self.packet_loss_percent = 0.0;
        } else {
            self.packet_loss_percent = (self.packets_lost as f32 / (self.packets_lost + self.packet_count) as f32) * 100.0;
        }
    }
    
    /// Convert metrics to DiagnosticsPacket
    pub fn to_diagnostics_packet(&self, sender_id: &str) -> DiagnosticsPacket {
        let mut packet = DiagnosticsPacket::new();
        
        // Convert MediaType to DiagMediaType
        let diag_media_type = match self.media_type {
            MediaType::VIDEO => DiagMediaType::VIDEO,
            MediaType::AUDIO => DiagMediaType::AUDIO,
            MediaType::SCREEN => DiagMediaType::SCREEN,
            _ => DiagMediaType::VIDEO, // Default
        };

        // Set basic fields
        packet.stream_id = format!("{}:{:?}", self.peer_id, self.media_type);
        packet.sender_id = sender_id.to_string();
        packet.target_id = self.peer_id.clone();
        
        // Use js_sys::Date now
        packet.timestamp_ms = Date::now() as u64;
        
        packet.media_type = diag_media_type.into();
        packet.packet_loss_percent = self.packet_loss_percent;
        packet.jitter_ms = self.jitter_ms;
        packet.estimated_bandwidth_kbps = self.estimated_bandwidth_kbps;
        
        // Add media-specific metrics
        match self.media_type {
            MediaType::VIDEO | MediaType::SCREEN => {
                let mut video_metrics = VideoMetrics::new();
                video_metrics.width = self.frame_width;
                video_metrics.height = self.frame_height;
                // Calculate FPS based on frames received (simple approximation)
                let fps = if self.frames_received > 0 { 30.0 } else { 0.0 };
                video_metrics.fps_received = fps;
                video_metrics.freeze_count = self.freeze_count;
                packet.video_metrics = Some(video_metrics).into();
            },
            MediaType::AUDIO => {
                let mut audio_metrics = AudioMetrics::new();
                audio_metrics.packets_lost = self.packets_lost;
                packet.audio_metrics = Some(audio_metrics).into();
            },
            _ => {}
        }
        
        // Add quality hints
        let mut quality_hints = QualityHints::new();
        quality_hints.target_bitrate_kbps = self.estimated_bandwidth_kbps;
        packet.quality_hints = Some(quality_hints).into();
        
        debug!(
            "Created diagnostics packet for peer {} ({:?}): loss: {}%, bandwidth: {} kbps", 
            self.peer_id, self.media_type, packet.packet_loss_percent, packet.estimated_bandwidth_kbps
        );
        
        packet
    }
}

/// A simple diagnostics manager that collects metrics for peers
#[derive(Debug)]
pub struct SimpleDiagnostics {
    metrics: RefCell<HashMap<String, StreamMetrics>>,
    enabled: bool,
}

impl SimpleDiagnostics {
    pub fn new(enabled: bool) -> Self {
        info!("Initializing SimpleDiagnostics, enabled: {}", enabled);
        Self {
            metrics: RefCell::new(HashMap::new()),
            enabled,
        }
    }
    
    /// Record a received packet from a peer
    pub fn record_packet(&self, peer_id: &str, size: usize) {
        if !self.enabled {
            return;
        }
        
        let mut metrics = self.metrics.borrow_mut();
        
        // Update video metrics
        let key = format!("{}:VIDEO", peer_id);
        let entry = metrics
            .entry(key.clone())
            .or_insert_with(|| StreamMetrics::new(peer_id.to_string(), MediaType::VIDEO));
        entry.update_packet_received(size);
        
        // Also update audio metrics (we don't know what type it is)
        let key = format!("{}:AUDIO", peer_id);
        let entry = metrics
            .entry(key.clone())
            .or_insert_with(|| StreamMetrics::new(peer_id.to_string(), MediaType::AUDIO));
        entry.update_packet_received(size);
    }
    
    /// Record a decoded video frame from a peer
    pub fn record_video_frame(&self, peer_id: &str, width: u32, height: u32) {
        if !self.enabled {
            return;
        }
        
        let mut metrics = self.metrics.borrow_mut();
        let key = format!("{}:VIDEO", peer_id);
        let entry = metrics
            .entry(key.clone())
            .or_insert_with(|| StreamMetrics::new(peer_id.to_string(), MediaType::VIDEO));
        
        entry.update_video_frame(width, height);
    }
    
    /// Record a lost packet from a peer
    pub fn record_packet_lost(&self, peer_id: &str) {
        if !self.enabled {
            return;
        }
        
        let mut metrics = self.metrics.borrow_mut();
        
        // Update both video and audio metrics since we don't know the type
        let key = format!("{}:VIDEO", peer_id);
        let entry = metrics
            .entry(key.clone())
            .or_insert_with(|| StreamMetrics::new(peer_id.to_string(), MediaType::VIDEO));
        entry.update_packet_lost();
        
        let key = format!("{}:AUDIO", peer_id);
        let entry = metrics
            .entry(key.clone())
            .or_insert_with(|| StreamMetrics::new(peer_id.to_string(), MediaType::AUDIO));
        entry.update_packet_lost();
    }
    
    /// Get metrics for a specific peer
    pub fn get_metrics(&self, peer_id: &str) -> Vec<StreamMetrics> {
        if !self.enabled {
            return Vec::new();
        }
        
        let metrics = self.metrics.borrow();
        let mut result = Vec::new();
        
        // Look up various possible metrics for this peer
        for media_type in [MediaType::VIDEO, MediaType::AUDIO, MediaType::SCREEN] {
            let key = format!("{}:{:?}", peer_id, media_type);
            if let Some(metric) = metrics.get(&key) {
                result.push(metric.clone());
            }
        }
        
        result
    }
    
    /// Enable or disable diagnostics collection
    pub fn set_enabled(&mut self, enabled: bool) {
        info!("Setting diagnostics collection to: {}", enabled);
        self.enabled = enabled;
    }
    
    /// Create a PacketWrapper with a DiagnosticsPacket
    pub fn create_packet_wrapper(&self, peer_id: &str, sender_id: &str) -> Option<PacketWrapper> {
        if !self.enabled {
            return None;
        }
        
        let metrics = self.get_metrics(peer_id);
        if metrics.is_empty() {
            debug!("No metrics found for peer {}, can't create packet", peer_id);
            return None;
        }
        
        // Select the most appropriate metric to send (prefer video)
        let metric = metrics.iter().find(|m| m.media_type == MediaType::VIDEO)
            .or_else(|| metrics.iter().find(|m| m.media_type == MediaType::AUDIO))
            .or_else(|| metrics.first())?;
        
        let diagnostics_packet = metric.to_diagnostics_packet(sender_id);
        let mut packet_wrapper = PacketWrapper::new();
        
        // Set packet type using the EnumOrUnknown
        packet_wrapper.packet_type = PacketType::DIAGNOSTICS.into();
        packet_wrapper.data = diagnostics_packet.write_to_bytes().unwrap_or_default();
        packet_wrapper.email = peer_id.to_string();
        
        info!(
            "Created diagnostics packet wrapper for peer: {}, media_type: {:?}, size: {} bytes", 
            peer_id, metric.media_type, packet_wrapper.data.len()
        );
        
        Some(packet_wrapper)
    }
    
    /// Get a summary of metrics for all peers
    pub fn get_metrics_summary(&self) -> String {
        if !self.enabled {
            return "Diagnostics disabled".to_string();
        }
        
        let metrics = self.metrics.borrow();
        if metrics.is_empty() {
            return "No metrics collected yet".to_string();
        }
        
        let mut summary = String::new();
        let mut peers = std::collections::HashSet::new();
        
        // Gather unique peer IDs
        for key in metrics.keys() {
            if let Some(idx) = key.find(':') {
                peers.insert(key[..idx].to_string());
            }
        }
        
        // Generate summary for each peer
        for peer in peers {
            let mut peer_summary = format!("Peer {}: ", peer);
            
            // Check for video metrics
            let video_key = format!("{}:VIDEO", peer);
            if let Some(metric) = metrics.get(&video_key) {
                peer_summary.push_str(&format!(
                    "Video: {}x{}, {} frames, {} kbps, {}% loss | ",
                    metric.frame_width, metric.frame_height, metric.frames_received,
                    metric.estimated_bandwidth_kbps, metric.packet_loss_percent
                ));
            }
            
            // Check for audio metrics
            let audio_key = format!("{}:AUDIO", peer);
            if let Some(metric) = metrics.get(&audio_key) {
                peer_summary.push_str(&format!(
                    "Audio: {} packets, {} kbps, {}% loss | ",
                    metric.packet_count, metric.estimated_bandwidth_kbps,
                    metric.packet_loss_percent
                ));
            }
            
            // Check for screen metrics
            let screen_key = format!("{}:SCREEN", peer);
            if let Some(metric) = metrics.get(&screen_key) {
                peer_summary.push_str(&format!(
                    "Screen: {}x{}, {} frames, {} kbps | ",
                    metric.frame_width, metric.frame_height, metric.frames_received,
                    metric.estimated_bandwidth_kbps
                ));
            }
            
            summary.push_str(&peer_summary);
            summary.push('\n');
        }
        
        info!("Generated metrics summary:\n{}", summary);
        summary
    }
} 