pub mod simple_diagnostics;

use futures::channel::mpsc::{channel, Sender};
use futures::channel::oneshot;
use futures::stream::StreamExt;
use log::{debug, info, warn};
use simple_diagnostics::{DiagnosticsMessage, SimpleDiagnostics};
use std::cell::RefCell;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

// Global channel sender for diagnostics messages
thread_local! {
    static DIAGNOSTICS_SENDER: RefCell<Option<Sender<DiagnosticsMessage>>> = RefCell::new(None);
}

// Initialize the diagnostics system
pub fn init_diagnostics(enabled: bool, reporting_interval_ms: u32) {
    info!("🔍 DIAGNOSTICS: Initializing system (enabled: {}, interval: {}ms)", enabled, reporting_interval_ms);
    
    // Create the channel with a reasonable buffer size
    let (sender, mut receiver) = channel::<DiagnosticsMessage>(100);
    
    // Store the sender in thread_local storage
    DIAGNOSTICS_SENDER.with(|cell| {
        *cell.borrow_mut() = Some(sender);
        info!("🔍 DIAGNOSTICS: Channel sender stored in thread_local storage");
    });
    
    // Start the diagnostics processor in a background task
    spawn_local(async move {
        info!("🔍 DIAGNOSTICS: Background processor task started");
        let mut diagnostics = SimpleDiagnostics::new(enabled);
        
        while let Some(message) = receiver.next().await {
            match message {
                DiagnosticsMessage::RecordPacket { peer_id, size } => {
                    info!("🔍 DIAGNOSTICS: Recording packet: peer={}, size={}B", peer_id, size);
                    diagnostics.record_packet(&peer_id, size);
                },
                DiagnosticsMessage::RecordVideoFrame { peer_id, width, height } => {
                    info!("🔍 DIAGNOSTICS: Recording video frame: peer={}, dimensions={}x{}", peer_id, width, height);
                    diagnostics.record_video_frame(&peer_id, width, height);
                },
                DiagnosticsMessage::RecordPacketLost { peer_id } => {
                    info!("🔍 DIAGNOSTICS: Recording packet loss: peer={}", peer_id);
                    diagnostics.record_packet_lost(&peer_id);
                },
                DiagnosticsMessage::GetMetricsSummary { response_channel } => {
                    info!("🔍 DIAGNOSTICS: Getting metrics summary");
                    let summary = diagnostics.get_metrics_summary();
                    info!("🔍 DIAGNOSTICS: Metrics summary length: {} chars", summary.len());
                    let _ = response_channel.send(summary);
                },
                DiagnosticsMessage::SetEnabled { enabled } => {
                    info!("🔍 DIAGNOSTICS: Setting enabled state to {}", enabled);
                    diagnostics.set_enabled(enabled);
                },
                DiagnosticsMessage::CreatePacketWrapper { peer_id, sender_id, response_channel } => {
                    info!("🔍 DIAGNOSTICS: Creating packet wrapper: peer={}, sender={}", peer_id, sender_id);
                    let packet = diagnostics.create_packet_wrapper(&peer_id, &sender_id);
                    let _ = response_channel.send(packet);
                },
            }
        }
        
        warn!("🔍 DIAGNOSTICS: Processor task terminated unexpectedly");
    });
}

// Helper function to send diagnostic messages
pub fn send_diagnostics_message(message: DiagnosticsMessage) -> bool {
    let message_type = match &message {
        DiagnosticsMessage::RecordPacket { .. } => "RecordPacket",
        DiagnosticsMessage::RecordVideoFrame { .. } => "RecordVideoFrame",
        DiagnosticsMessage::RecordPacketLost { .. } => "RecordPacketLost",
        DiagnosticsMessage::GetMetricsSummary { .. } => "GetMetricsSummary",
        DiagnosticsMessage::SetEnabled { .. } => "SetEnabled",
        DiagnosticsMessage::CreatePacketWrapper { .. } => "CreatePacketWrapper",
    };
    
    let mut success = false;
    DIAGNOSTICS_SENDER.with(|cell| {
        if let Some(sender) = &mut *cell.borrow_mut() {
            success = sender.try_send(message).is_ok();
            if success {
                info!("🔍 DIAGNOSTICS: Successfully sent {} message", message_type);
            } else {
                warn!("🔍 DIAGNOSTICS: Failed to send {} message - channel might be full", message_type);
            }
        } else {
            warn!("🔍 DIAGNOSTICS: Failed to send {} message - sender not initialized", message_type);
        }
    });
    success
}

// Get a diagnostics summary asynchronously
pub async fn get_diagnostics_summary_async() -> String {
    info!("🔍 DIAGNOSTICS: Requesting async diagnostics summary");
    let (sender, receiver) = oneshot::channel();
    
    let sent = send_diagnostics_message(DiagnosticsMessage::GetMetricsSummary {
        response_channel: sender,
    });
    
    if !sent {
        warn!("🔍 DIAGNOSTICS: Failed to request summary - diagnostics system not initialized");
        return "Diagnostics system not initialized".to_string();
    }
    
    info!("🔍 DIAGNOSTICS: Waiting for async summary response");
    match receiver.await {
        Ok(summary) => {
            info!("🔍 DIAGNOSTICS: Received summary (length: {} chars)", summary.len());
            summary
        },
        Err(_) => {
            warn!("🔍 DIAGNOSTICS: Error receiving summary response");
            "Error retrieving metrics summary".to_string()
        }
    }
}

// Synchronous wrapper for getting diagnostics summary
pub fn get_diagnostics_summary() -> String {
    info!("🔍 DIAGNOSTICS: Requesting synchronous diagnostics summary");
    let (sender, receiver) = oneshot::channel();
    
    let sent = send_diagnostics_message(DiagnosticsMessage::GetMetricsSummary {
        response_channel: sender,
    });
    
    if !sent {
        warn!("🔍 DIAGNOSTICS: Failed to request summary - diagnostics system not initialized");
        return "Diagnostics system not initialized".to_string();
    }
    
    info!("🔍 DIAGNOSTICS: Returning placeholder message - async method should be used instead");
    // Since we don't have a direct non-blocking way to check for immediate results,
    // we'll just return a message that the user should use the async version
    "For diagnostics summary, use the async get_diagnostics_summary_async() function".to_string()
}

// Enable or disable diagnostics collection
pub fn enable_diagnostics(enabled: bool) -> bool {
    info!("🔍 DIAGNOSTICS: Setting enabled state to {}", enabled);
    send_diagnostics_message(DiagnosticsMessage::SetEnabled { enabled })
}

// Record a video frame for diagnostics
pub fn record_video_frame(peer_id: &str, width: u32, height: u32) -> bool {
    debug!("🔍 DIAGNOSTICS: Recording video frame: peer={}, dimensions={}x{}", peer_id, width, height);
    send_diagnostics_message(DiagnosticsMessage::RecordVideoFrame {
        peer_id: peer_id.to_string(),
        width,
        height,
    })
}

// Record a packet for diagnostics
pub fn record_packet(peer_id: &str, size: usize) -> bool {
    debug!("🔍 DIAGNOSTICS: Recording packet: peer={}, size={}B", peer_id, size);
    send_diagnostics_message(DiagnosticsMessage::RecordPacket {
        peer_id: peer_id.to_string(),
        size,
    })
}

// Record a packet loss for diagnostics
pub fn record_packet_lost(peer_id: &str) -> bool {
    debug!("🔍 DIAGNOSTICS: Recording packet loss: peer={}", peer_id);
    send_diagnostics_message(DiagnosticsMessage::RecordPacketLost {
        peer_id: peer_id.to_string(),
    })
}
