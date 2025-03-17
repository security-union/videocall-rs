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
use std::sync::mpsc::Sender;

/// Define the messages that can be sent to the diagnostics system
pub enum DiagnosticsMessage {
    RecordPacket { peer_id: String, size: usize },
    RecordVideoFrame { peer_id: String, width: u32, height: u32 },
    RecordPacketLost { peer_id: String },
    GetMetrics { peer_id: String, response_channel: Sender<Vec<StreamMetrics>> },
    GetMetricsSummary { response_channel: Sender<String> },
    SetEnabled { enabled: bool },
    CreatePacketWrapper { peer_id: String, sender_id: String, response_channel: Sender<Option<PacketWrapper>> },
}

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

/// Simple diagnostics collector that tracks video frame dimensions and packet sizes
#[derive(Debug)]
pub struct SimpleDiagnostics {
    enabled: bool,
    video_frames: HashMap<String, (u32, u32)>,
    packet_counts: HashMap<String, usize>,
    packet_sizes: HashMap<String, usize>,
    lost_packets: HashMap<String, usize>,
}

impl SimpleDiagnostics {
    /// Create a new diagnostics collector
    pub fn new(enabled: bool) -> Self {
        info!("Creating new SimpleDiagnostics, enabled: {}", enabled);
        Self {
            enabled,
            video_frames: HashMap::new(),
            packet_counts: HashMap::new(),
            packet_sizes: HashMap::new(),
            lost_packets: HashMap::new(),
        }
    }

    /// Enable or disable diagnostics collection
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        debug!("Diagnostics collection enabled: {}", enabled);
    }

    /// Record a video frame for the given peer ID with the given dimensions
    pub fn record_video_frame(&mut self, peer_id: &str, width: u32, height: u32) {
        if !self.enabled {
            return;
        }
        self.video_frames.insert(peer_id.to_string(), (width, height));
        debug!(
            "Recorded video frame for {}: {} x {}",
            peer_id, width, height
        );
    }

    /// Record a packet received from the given peer ID with the given size
    pub fn record_packet(&mut self, peer_id: &str, size: usize) {
        if !self.enabled {
            return;
        }
        *self.packet_counts.entry(peer_id.to_string()).or_insert(0) += 1;
        *self.packet_sizes.entry(peer_id.to_string()).or_insert(0) += size;
        debug!(
            "Recorded packet from {}, size: {} bytes",
            peer_id, size
        );
    }

    /// Record a packet loss from the given peer ID
    pub fn record_packet_lost(&mut self, peer_id: &str) {
        if !self.enabled {
            return;
        }
        *self.lost_packets.entry(peer_id.to_string()).or_insert(0) += 1;
        debug!("Recorded packet loss from {}", peer_id);
    }

    /// Process a batch of diagnostic data entries
    pub fn process_batch(&mut self, diagnostic_data: Vec<(String, u32, u32, usize)>) {
        if !self.enabled {
            return;
        }
        
        debug!("Processing batch of {} diagnostic entries", diagnostic_data.len());
        for (peer_id, width, height, packet_size) in diagnostic_data {
            self.record_video_frame(&peer_id, width, height);
            self.record_packet(&peer_id, packet_size);
        }
    }

    /// Get a summary of the metrics collected
    pub fn get_metrics_summary(&self) -> String {
        if !self.enabled {
            return "Diagnostics disabled".to_string();
        }

        let mut summary = String::new();
        summary.push_str("Diagnostics Summary:\n");

        // Video frames
        summary.push_str("Video Frames:\n");
        for (peer_id, (width, height)) in &self.video_frames {
            summary.push_str(&format!("  {}: {}x{}\n", peer_id, width, height));
        }

        // Packet statistics
        summary.push_str("Packet Statistics:\n");
        for (peer_id, count) in &self.packet_counts {
            let size = self.packet_sizes.get(peer_id).unwrap_or(&0);
            let lost = self.lost_packets.get(peer_id).unwrap_or(&0);
            summary.push_str(&format!(
                "  {}: {} packets, {} bytes total, {} packets lost\n",
                peer_id, count, size, lost
            ));
        }

        debug!("Generated metrics summary: {} bytes", summary.len());
        summary
    }

    /// Create a packet wrapper containing diagnostic data for a peer
    pub fn create_packet_wrapper(&self, peer_id: &str, self_id: &str) -> Option<PacketWrapper> {
        if !self.enabled {
            return None;
        }

        // Check if we have data for this peer
        if !self.video_frames.contains_key(peer_id) && !self.packet_counts.contains_key(peer_id) {
            return None;
        }

        // Create a simple text-based diagnostics packet
        let mut data = String::new();
        data.push_str("DIAGNOSTICS:");

        // Add video frame data if available
        if let Some((width, height)) = self.video_frames.get(peer_id) {
            data.push_str(&format!("video:{}x{}", width, height));
        }

        // Add packet stats if available
        if let Some(count) = self.packet_counts.get(peer_id) {
            let size = self.packet_sizes.get(peer_id).unwrap_or(&0);
            let lost = self.lost_packets.get(peer_id).unwrap_or(&0);
            data.push_str(&format!(";packets:{},{},{}", count, size, lost));
        }

        // Create and return the packet wrapper
        Some(PacketWrapper {
            packet_type: PacketType::DIAGNOSTICS.into(),
            email: self_id.to_string(),
            data: data.into_bytes(),
            ..Default::default()
        })
    }
} 