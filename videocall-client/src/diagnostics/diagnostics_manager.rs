use log::{info, debug};
use wasm_bindgen_futures::spawn_local;
use gloo::timers::callback::Interval;
use std::rc::Rc;
use std::cell::RefCell;
use std::sync::mpsc::{channel, Sender, Receiver};

use crate::diagnostics::simple_diagnostics::{SimpleDiagnostics, DiagnosticsMessage};
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// DiagnosticsManager coordinates diagnostic data collection and reporting
pub struct DiagnosticsManager {
    sender: Sender<DiagnosticsMessage>,
    reporting_interval_ms: u32,
    diagnostics_timer: Option<Interval>,
    enabled: bool,
}

impl DiagnosticsManager {
    pub fn new(reporting_interval_ms: u32, enabled: bool) -> Self {
        let (sender, receiver) = channel();
        
        // Create the diagnostics worker
        spawn_local(async move {
            let mut diagnostics = SimpleDiagnostics::new(enabled);
            
            while let Ok(message) = receiver.recv() {
                match message {
                    DiagnosticsMessage::RecordPacket { peer_id, size } => {
                        diagnostics.record_packet(&peer_id, size);
                    },
                    DiagnosticsMessage::RecordVideoFrame { peer_id, width, height } => {
                        diagnostics.record_video_frame(&peer_id, width, height);
                    },
                    DiagnosticsMessage::RecordPacketLost { peer_id } => {
                        diagnostics.record_packet_lost(&peer_id);
                    },
                    DiagnosticsMessage::GetMetrics { peer_id, response_channel } => {
                        let metrics = diagnostics.get_metrics(&peer_id);
                        let _ = response_channel.send(metrics);
                    },
                    DiagnosticsMessage::GetMetricsSummary { response_channel } => {
                        let summary = diagnostics.get_metrics_summary();
                        let _ = response_channel.send(summary);
                    },
                    DiagnosticsMessage::SetEnabled { enabled } => {
                        diagnostics.set_enabled(enabled);
                    },
                    DiagnosticsMessage::CreatePacketWrapper { peer_id, sender_id, response_channel } => {
                        let packet = diagnostics.create_packet_wrapper(&peer_id, &sender_id);
                        let _ = response_channel.send(packet);
                    },
                }
            }
        });
        
        Self {
            sender,
            reporting_interval_ms,
            diagnostics_timer: None,
            enabled,
        }
    }

    pub fn enable(&mut self, enabled: bool) {
        self.enabled = enabled;
        let _ = self.sender.send(DiagnosticsMessage::SetEnabled { enabled });
        info!("Diagnostics {} for reporting every {}ms", 
              if enabled { "enabled" } else { "disabled" },
              self.reporting_interval_ms);
    }
    
    pub fn record_packet(&self, peer_id: &str, size: usize) {
        if !self.enabled {
            return;
        }
        
        let _ = self.sender.send(DiagnosticsMessage::RecordPacket {
            peer_id: peer_id.to_string(),
            size,
        });
    }
    
    pub fn record_video_frame(&self, peer_id: &str, width: u32, height: u32) {
        if !self.enabled {
            return;
        }
        
        let _ = self.sender.send(DiagnosticsMessage::RecordVideoFrame {
            peer_id: peer_id.to_string(),
            width,
            height,
        });
    }
    
    pub fn record_packet_lost(&self, peer_id: &str) {
        if !self.enabled {
            return;
        }
        
        let _ = self.sender.send(DiagnosticsMessage::RecordPacketLost {
            peer_id: peer_id.to_string(),
        });
    }
    
    pub fn get_metrics_summary(&self) -> String {
        if !self.enabled {
            return "Diagnostics disabled".to_string();
        }
        
        let (sender, receiver) = channel();
        let _ = self.sender.send(DiagnosticsMessage::GetMetricsSummary {
            response_channel: sender,
        });
        
        receiver.recv().unwrap_or_else(|_| "Error retrieving metrics summary".to_string())
    }
    
    pub fn create_packet_wrapper(&self, peer_id: &str, sender_id: &str) -> Option<PacketWrapper> {
        if !self.enabled {
            return None;
        }
        
        let (sender, receiver) = channel();
        let _ = self.sender.send(DiagnosticsMessage::CreatePacketWrapper {
            peer_id: peer_id.to_string(),
            sender_id: sender_id.to_string(),
            response_channel: sender,
        });
        
        receiver.recv().unwrap_or(None)
    }
    
    pub fn start_reporting(&mut self) {
        if !self.enabled {
            return;
        }
        
        info!("Starting diagnostics reporting every {}ms", self.reporting_interval_ms);
        let sender = self.sender.clone();
        
        // Create a new timer to periodically process diagnostics
        let timer = Interval::new(self.reporting_interval_ms, move || {
            debug!("Diagnostics timer fired, processing metrics");
            // Add periodic processing logic here if needed
        });
        
        self.diagnostics_timer = Some(timer);
    }
} 