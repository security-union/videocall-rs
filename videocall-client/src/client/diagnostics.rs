use log::{debug, error, info, warn};
use protobuf::{Message, MessageField};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};
use videocall_types::protos::diagnostics_packet::{
    AudioMetrics, DiagnosticsPacket, QualityHints, VideoMetrics,
};
use videocall_types::protos::diagnostics_packet::diagnostics_packet::MediaType;
use videocall_types::protos::media_packet::media_packet::MediaType as MediaPacketType;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::prelude::*;

// Constants for diagnostics collection
const DIAGNOSTICS_INTERVAL_MS: u32 = 2000; // Send diagnostics every 2 seconds
const MAX_HISTORY_SIZE: usize = 30; // Keep track of 30 frames for calculating metrics

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
    
    #[wasm_bindgen(js_namespace = self)]
    fn performance_now() -> f64;
}

/// Stream metrics tracked for a specific peer and media type
#[derive(Debug)]
struct StreamMetrics {
    peer_id: String, 
    media_type: MediaPacketType,
    frame_timestamps: Vec<f64>,
    decode_times: Vec<u32>,
    packet_loss_count: u32,
    jitter_measurements: Vec<u32>,
    last_frame_received: Option<f64>,
    resolution_width: u32,
    resolution_height: u32,
    bitrate_kbps: u32,
    freeze_count: u32,
    last_received_sequence: Option<u64>,
    sample_rate: Option<u32>,
    channels: Option<u32>,
    estimated_bandwidth_kbps: u32,
}

impl StreamMetrics {
    fn new(peer_id: String, media_type: MediaPacketType) -> Self {
        Self {
            peer_id,
            media_type,
            frame_timestamps: Vec::with_capacity(MAX_HISTORY_SIZE),
            decode_times: Vec::with_capacity(MAX_HISTORY_SIZE),
            packet_loss_count: 0,
            jitter_measurements: Vec::with_capacity(MAX_HISTORY_SIZE),
            last_frame_received: None,
            resolution_width: 0,
            resolution_height: 0,
            bitrate_kbps: 0,
            freeze_count: 0,
            last_received_sequence: None,
            sample_rate: None,
            channels: None,
            estimated_bandwidth_kbps: 0,
        }
    }

    fn add_frame_timestamp(&mut self, timestamp: f64) {
        if self.frame_timestamps.len() >= MAX_HISTORY_SIZE {
            self.frame_timestamps.remove(0);
        }
        self.frame_timestamps.push(timestamp);
        
        // Calculate jitter from variation in inter-frame intervals
        if self.frame_timestamps.len() >= 2 {
            let prev = self.frame_timestamps[self.frame_timestamps.len() - 2];
            let current = self.frame_timestamps[self.frame_timestamps.len() - 1];
            let interval = (current - prev) as u32;
            
            if self.jitter_measurements.len() >= MAX_HISTORY_SIZE {
                self.jitter_measurements.remove(0);
            }
            self.jitter_measurements.push(interval);
        }
        
        // Track freezes (when time between frames is too large)
        if let Some(last_time) = self.last_frame_received {
            let gap = timestamp - last_time;
            // If gap > 500ms, consider it a freeze
            if gap > 500.0 && matches!(self.media_type, MediaPacketType::VIDEO | MediaPacketType::SCREEN) {
                self.freeze_count += 1;
            }
        }
        self.last_frame_received = Some(timestamp);
    }
    
    fn add_decode_time(&mut self, decode_time: u32) {
        if self.decode_times.len() >= MAX_HISTORY_SIZE {
            self.decode_times.remove(0);
        }
        self.decode_times.push(decode_time);
    }

    fn add_sequence(&mut self, sequence: u64) {
        if let Some(last_seq) = self.last_received_sequence {
            if sequence > last_seq + 1 {
                // We missed packets
                self.packet_loss_count += (sequence - last_seq - 1) as u32;
            }
        }
        self.last_received_sequence = Some(sequence);
    }
    
    fn update_resolution(&mut self, width: u32, height: u32) {
        self.resolution_width = width;
        self.resolution_height = height;
    }
    
    fn update_audio_params(&mut self, sample_rate: u32, channels: u32) {
        self.sample_rate = Some(sample_rate);
        self.channels = Some(channels);
    }
    
    fn update_bitrate(&mut self, bitrate_kbps: u32) {
        self.bitrate_kbps = bitrate_kbps;
    }

    fn update_bandwidth_estimate(&mut self, bandwidth_kbps: u32) {
        self.estimated_bandwidth_kbps = bandwidth_kbps;
    }
    
    fn calculate_fps(&self) -> f32 {
        if self.frame_timestamps.len() < 2 {
            return 0.0;
        }
        
        let first = self.frame_timestamps[0];
        let last = self.frame_timestamps[self.frame_timestamps.len() - 1];
        let time_span = (last - first) / 1000.0; // Convert to seconds
        
        if time_span <= 0.0 {
            return 0.0;
        }
        
        (self.frame_timestamps.len() as f32) / time_span as f32
    }
    
    fn calculate_median_latency(&self) -> u32 {
        // In a real implementation, this would measure actual latency
        // Here we're providing a placeholder implementation
        100 // Placeholder value in ms
    }
    
    fn calculate_jitter(&self) -> u32 {
        if self.jitter_measurements.len() < 2 {
            return 0;
        }
        
        // Calculate standard deviation of inter-frame intervals
        let mean = self.jitter_measurements.iter().sum::<u32>() as f32 / self.jitter_measurements.len() as f32;
        let variance = self.jitter_measurements.iter()
            .map(|&x| {
                let diff = x as f32 - mean;
                diff * diff
            })
            .sum::<f32>() / self.jitter_measurements.len() as f32;
        
        variance.sqrt() as u32
    }
    
    fn calculate_packet_loss_percent(&self) -> f32 {
        if let Some(last_seq) = self.last_received_sequence {
            if last_seq == 0 {
                return 0.0;
            }
            return (self.packet_loss_count as f32 / last_seq as f32) * 100.0;
        }
        0.0
    }

    fn calculate_avg_decode_time(&self) -> u32 {
        if self.decode_times.is_empty() {
            return 0;
        }
        self.decode_times.iter().sum::<u32>() / self.decode_times.len() as u32
    }
    
    fn create_diagnostics_packet(&self, sender_id: String) -> DiagnosticsPacket {
        let mut packet = DiagnosticsPacket::new();
        
        // Basic identification
        packet.stream_id = format!("{}:{:?}", self.peer_id, self.media_type);
        packet.sender_id = sender_id;
        packet.target_id = self.peer_id.clone();
        packet.timestamp_ms = (js_sys::Date::now() as u64);
        
        // Media type
        packet.media_type = match self.media_type {
            MediaPacketType::VIDEO => MediaType::VIDEO,
            MediaPacketType::AUDIO => MediaType::AUDIO,
            MediaPacketType::SCREEN => MediaType::SCREEN,
            _ => MediaType::VIDEO, // Default to VIDEO for other types
        }.into();
        
        // Common metrics
        packet.packet_loss_percent = self.calculate_packet_loss_percent();
        packet.median_latency_ms = self.calculate_median_latency();
        packet.jitter_ms = self.calculate_jitter();
        packet.estimated_bandwidth_kbps = self.estimated_bandwidth_kbps;
        packet.round_trip_time_ms = 0; // To be updated with real RTT measurement
        
        // Media-specific metrics
        match self.media_type {
            MediaPacketType::VIDEO | MediaPacketType::SCREEN => {
                let mut video_metrics = VideoMetrics::new();
                video_metrics.fps_received = self.calculate_fps();
                video_metrics.width = self.resolution_width;
                video_metrics.height = self.resolution_height;
                video_metrics.bitrate_kbps = self.bitrate_kbps;
                video_metrics.decode_time_ms = self.calculate_avg_decode_time();
                video_metrics.freeze_count = self.freeze_count;
                video_metrics.keyframes_received = 0; // Placeholder, needs real tracking
                
                packet.video_metrics = MessageField::some(video_metrics);
            }
            MediaPacketType::AUDIO => {
                let mut audio_metrics = AudioMetrics::new();
                audio_metrics.audio_level = 0.0; // Placeholder, needs real audio level
                audio_metrics.sample_rate = self.sample_rate.unwrap_or(0);
                audio_metrics.bitrate_kbps = self.bitrate_kbps;
                audio_metrics.channels = self.channels.unwrap_or(0);
                audio_metrics.packets_lost = self.packet_loss_count;
                
                packet.audio_metrics = MessageField::some(audio_metrics);
            }
            _ => {}
        }
        
        // Quality hints - simple version for now
        let mut quality_hints = QualityHints::new();
        quality_hints.target_bitrate_kbps = if self.estimated_bandwidth_kbps > 0 {
            // Suggest slightly lower than estimated to have headroom
            (self.estimated_bandwidth_kbps as f32 * 0.8) as u32
        } else {
            // Default suggestions based on media type
            match self.media_type {
                MediaPacketType::VIDEO => 800,
                MediaPacketType::SCREEN => 1200,
                MediaPacketType::AUDIO => 64,
                _ => 500,
            }
        };
        
        packet.quality_hints = MessageField::some(quality_hints);
        
        packet
    }
}

/// Manages diagnostics for all streams
#[derive(Debug)]
pub struct DiagnosticsManager {
    stream_metrics: HashMap<String, StreamMetrics>,
    own_user_id: String,
    rtt_measurements: HashMap<String, u32>,
    last_diagnostics_sent: Instant,
    adaptation_params: AdaptationParameters,
}

/// Parameters used for adaptive streaming decisions
#[derive(Debug)]
pub struct AdaptationParameters {
    pub video_bitrate_kbps: u32,
    pub video_width: u32,
    pub video_height: u32,
    pub video_fps_target: u32,
    pub audio_bitrate_kbps: u32,
    pub keyframe_interval: u32,
}

impl Default for AdaptationParameters {
    fn default() -> Self {
        Self {
            video_bitrate_kbps: 1000,
            video_width: 640,
            video_height: 480,
            video_fps_target: 30,
            audio_bitrate_kbps: 64,
            keyframe_interval: 150, // Frames
        }
    }
}

impl DiagnosticsManager {
    pub fn new(user_id: String) -> Self {
        Self {
            stream_metrics: HashMap::new(),
            own_user_id: user_id,
            rtt_measurements: HashMap::new(),
            last_diagnostics_sent: Instant::now(),
            adaptation_params: AdaptationParameters::default(),
        }
    }
    
    pub fn get_stream_key(peer_id: &str, media_type: MediaPacketType) -> String {
        format!("{}:{:?}", peer_id, media_type)
    }

    pub fn on_frame_received(&mut self, peer_id: &str, media_type: MediaPacketType, timestamp: f64, 
                             decode_time_ms: u32, sequence: Option<u64>) {
        let key = Self::get_stream_key(peer_id, media_type);
        
        let metrics = self.stream_metrics.entry(key)
            .or_insert_with(|| StreamMetrics::new(peer_id.to_string(), media_type));
            
        metrics.add_frame_timestamp(timestamp);
        metrics.add_decode_time(decode_time_ms);
        
        if let Some(seq) = sequence {
            metrics.add_sequence(seq);
        }
    }
    
    pub fn update_video_resolution(&mut self, peer_id: &str, media_type: MediaPacketType, 
                              width: u32, height: u32) {
        let key = Self::get_stream_key(peer_id, media_type);
        
        if let Some(metrics) = self.stream_metrics.get_mut(&key) {
            metrics.update_resolution(width, height);
        }
    }
    
    pub fn update_audio_params(&mut self, peer_id: &str, sample_rate: u32, channels: u32) {
        let key = Self::get_stream_key(peer_id, MediaPacketType::AUDIO);
        
        if let Some(metrics) = self.stream_metrics.get_mut(&key) {
            metrics.update_audio_params(sample_rate, channels);
        }
    }
    
    pub fn update_bitrate(&mut self, peer_id: &str, media_type: MediaPacketType, bitrate_kbps: u32) {
        let key = Self::get_stream_key(peer_id, media_type);
        
        if let Some(metrics) = self.stream_metrics.get_mut(&key) {
            metrics.update_bitrate(bitrate_kbps);
        }
    }
    
    pub fn update_rtt(&mut self, peer_id: &str, rtt_ms: u32) {
        self.rtt_measurements.insert(peer_id.to_string(), rtt_ms);
    }
    
    pub fn update_bandwidth_estimate(&mut self, peer_id: &str, media_type: MediaPacketType, bandwidth_kbps: u32) {
        let key = Self::get_stream_key(peer_id, media_type);
        
        if let Some(metrics) = self.stream_metrics.get_mut(&key) {
            metrics.update_bandwidth_estimate(bandwidth_kbps);
        }
    }
    
    /// Called when it's time to send diagnostics to peers
    pub fn create_diagnostics_packets(&self) -> Vec<(String, PacketWrapper)> {
        let mut packets = Vec::new();
        
        // Create a diagnostic packet for each stream we're tracking
        for (_, metrics) in &self.stream_metrics {
            let packet = metrics.create_diagnostics_packet(self.own_user_id.clone());
            
            // Add RTT measurement if available
            let mut packet_with_rtt = packet.clone();
            if let Some(&rtt) = self.rtt_measurements.get(&metrics.peer_id) {
                packet_with_rtt.round_trip_time_ms = rtt;
            }
            
            // Wrap in PacketWrapper
            match packet_with_rtt.write_to_bytes() {
                Ok(data) => {
                    let wrapper = PacketWrapper {
                        packet_type: PacketType::DIAGNOSTICS.into(),
                        email: self.own_user_id.clone(),
                        data,
                        ..Default::default()
                    };
                    
                    // Add to list with target peer ID
                    packets.push((metrics.peer_id.clone(), wrapper));
                }
                Err(e) => {
                    error!("Failed to serialize diagnostics packet: {}", e);
                }
            }
        }
        
        packets
    }
    
    /// Process incoming diagnostics from other peers
    pub fn process_diagnostics(&mut self, from_peer: &str, packet: DiagnosticsPacket) {
        // This peer is the target of these diagnostics (we are the sender they're analyzing)
        if packet.target_id == self.own_user_id {
            debug!("Received diagnostics from {} about our stream", from_peer);
            
            // Update our adaptation parameters based on the diagnostics
            self.adapt_stream_quality(&packet);
        }
    }
    
    /// Adapt streaming parameters based on received diagnostics
    fn adapt_stream_quality(&mut self, packet: &DiagnosticsPacket) {
        // Implement the lowest common denominator approach
        
        // Extract key metrics for adaptation
        let packet_loss = packet.packet_loss_percent;
        let rtt = packet.round_trip_time_ms;
        let jitter = packet.jitter_ms;
        let bandwidth = packet.estimated_bandwidth_kbps;
        
        // Video-specific adaptation
        if let Some(ref video_metrics) = packet.video_metrics.as_ref() {
            let fps = video_metrics.fps_received;
            
            // Adaptive logic for video (simple version)
            if packet_loss > 5.0 || rtt > 300 || jitter > 50 {
                // Network congestion detected - reduce quality
                self.adaptation_params.video_bitrate_kbps = 
                    (self.adaptation_params.video_bitrate_kbps as f32 * 0.8) as u32;
                
                if fps < 15.0 && self.adaptation_params.video_fps_target > 15 {
                    // FPS too low, reduce resolution instead of framerate
                    if self.adaptation_params.video_width >= 640 {
                        self.adaptation_params.video_width = 480;
                        self.adaptation_params.video_height = 360;
                    } else if self.adaptation_params.video_width >= 480 {
                        self.adaptation_params.video_width = 320;
                        self.adaptation_params.video_height = 240;
                    }
                }
                
                info!("Adapting video quality down due to network conditions: {}kbps, {}x{} @ {}fps", 
                      self.adaptation_params.video_bitrate_kbps,
                      self.adaptation_params.video_width,
                      self.adaptation_params.video_height,
                      self.adaptation_params.video_fps_target);
            } else if packet_loss < 1.0 && rtt < 150 && jitter < 20 && 
                      bandwidth > (self.adaptation_params.video_bitrate_kbps as f32 * 1.5) as u32 {
                // Network conditions good - can increase quality gradually
                if self.adaptation_params.video_width < 640 {
                    self.adaptation_params.video_width = 640;
                    self.adaptation_params.video_height = 480;
                } else if self.adaptation_params.video_bitrate_kbps < 1500 {
                    self.adaptation_params.video_bitrate_kbps = 
                        (self.adaptation_params.video_bitrate_kbps as f32 * 1.1) as u32;
                }
                
                info!("Adapting video quality up due to good network conditions: {}kbps, {}x{} @ {}fps", 
                      self.adaptation_params.video_bitrate_kbps,
                      self.adaptation_params.video_width,
                      self.adaptation_params.video_height,
                      self.adaptation_params.video_fps_target);
            }
        }
        
        // Audio-specific adaptation
        if let Some(ref audio_metrics) = packet.audio_metrics.as_ref() {
            // Simple audio adaptation
            if packet_loss > 4.0 || rtt > 300 {
                // Reduce audio quality
                self.adaptation_params.audio_bitrate_kbps = 
                    std::cmp::max(24, (self.adaptation_params.audio_bitrate_kbps as f32 * 0.8) as u32);
                
                info!("Adapting audio quality down: {}kbps", self.adaptation_params.audio_bitrate_kbps);
            } else if packet_loss < 1.0 && rtt < 200 && self.adaptation_params.audio_bitrate_kbps < 64 {
                // Increase audio quality
                self.adaptation_params.audio_bitrate_kbps = 
                    std::cmp::min(64, (self.adaptation_params.audio_bitrate_kbps as f32 * 1.1) as u32);
                
                info!("Adapting audio quality up: {}kbps", self.adaptation_params.audio_bitrate_kbps);
            }
        }
        
        // Consider quality hints from receiver
        if let Some(ref hints) = packet.quality_hints.as_ref() {
            if hints.target_bitrate_kbps > 0 && 
               hints.target_bitrate_kbps < self.adaptation_params.video_bitrate_kbps {
                // Respect receiver's bandwidth limitation
                self.adaptation_params.video_bitrate_kbps = hints.target_bitrate_kbps;
                debug!("Adjusting to receiver's target bitrate: {}kbps", hints.target_bitrate_kbps);
            }
        }
    }
    
    /// Get the current adaptation parameters for encoding
    pub fn get_adaptation_params(&self) -> AdaptationParameters {
        self.adaptation_params.clone()
    }
    
    /// Check if it's time to send diagnostics
    pub fn should_send_diagnostics(&self) -> bool {
        self.last_diagnostics_sent.elapsed() >= Duration::from_millis(DIAGNOSTICS_INTERVAL_MS as u64)
    }
    
    /// Mark diagnostics as sent
    pub fn mark_diagnostics_sent(&mut self) {
        self.last_diagnostics_sent = Instant::now();
    }
}

impl Clone for AdaptationParameters {
    fn clone(&self) -> Self {
        Self {
            video_bitrate_kbps: self.video_bitrate_kbps,
            video_width: self.video_width,
            video_height: self.video_height,
            video_fps_target: self.video_fps_target,
            audio_bitrate_kbps: self.audio_bitrate_kbps,
            keyframe_interval: self.keyframe_interval,
        }
    }
}
