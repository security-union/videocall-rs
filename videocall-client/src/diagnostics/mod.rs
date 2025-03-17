pub mod simple_diagnostics;

use futures::channel::mpsc::{channel, Sender};
use futures::channel::oneshot;
use futures::stream::StreamExt;
use log::{debug, info};
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
    // Create the channel with a reasonable buffer size
    let (sender, mut receiver) = channel::<DiagnosticsMessage>(100);
    
    // Store the sender in thread_local storage
    DIAGNOSTICS_SENDER.with(|cell| {
        *cell.borrow_mut() = Some(sender);
    });
    
    info!("Initializing diagnostics system, enabled: {}, interval: {}ms", enabled, reporting_interval_ms);
    
    // Start the diagnostics processor in a background task
    spawn_local(async move {
        let mut diagnostics = SimpleDiagnostics::new(enabled);
        
        while let Some(message) = receiver.next().await {
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
                DiagnosticsMessage::GetMetricsSummary { response_channel } => {
                    let summary = diagnostics.get_metrics_summary();
                    let _ = response_channel.send(summary);
                },
                DiagnosticsMessage::SetEnabled { enabled } => {
                    diagnostics.set_enabled(enabled);
                    debug!("Diagnostics collection enabled: {}", enabled);
                },
                DiagnosticsMessage::CreatePacketWrapper { peer_id, sender_id, response_channel } => {
                    let packet = diagnostics.create_packet_wrapper(&peer_id, &sender_id);
                    let _ = response_channel.send(packet);
                },
            }
        }
        
        info!("Diagnostics processor task terminated");
    });
}

// Helper function to send diagnostic messages
pub fn send_diagnostics_message(message: DiagnosticsMessage) -> bool {
    let mut success = false;
    DIAGNOSTICS_SENDER.with(|cell| {
        if let Some(sender) = &mut *cell.borrow_mut() {
            success = sender.try_send(message).is_ok();
        }
    });
    success
}

// Get a diagnostics summary asynchronously
pub async fn get_diagnostics_summary_async() -> String {
    let (sender, receiver) = oneshot::channel();
    
    let sent = send_diagnostics_message(DiagnosticsMessage::GetMetricsSummary {
        response_channel: sender,
    });
    
    if !sent {
        return "Diagnostics system not initialized".to_string();
    }
    
    match receiver.await {
        Ok(summary) => summary,
        Err(_) => "Error retrieving metrics summary".to_string()
    }
}

// Synchronous wrapper for getting diagnostics summary
pub fn get_diagnostics_summary() -> String {
    let (sender, receiver) = oneshot::channel();
    
    let sent = send_diagnostics_message(DiagnosticsMessage::GetMetricsSummary {
        response_channel: sender,
    });
    
    if !sent {
        return "Diagnostics system not initialized".to_string();
    }
    
    // Since we don't have a direct non-blocking way to check for immediate results,
    // we'll just return a message that the user should use the async version
    "For diagnostics summary, use the async get_diagnostics_summary_async() function".to_string()
}

// Enable or disable diagnostics collection
pub fn enable_diagnostics(enabled: bool) -> bool {
    send_diagnostics_message(DiagnosticsMessage::SetEnabled { enabled })
}

// Record a video frame for diagnostics
pub fn record_video_frame(peer_id: &str, width: u32, height: u32) -> bool {
    send_diagnostics_message(DiagnosticsMessage::RecordVideoFrame {
        peer_id: peer_id.to_string(),
        width,
        height,
    })
}

// Record a packet for diagnostics
pub fn record_packet(peer_id: &str, size: usize) -> bool {
    send_diagnostics_message(DiagnosticsMessage::RecordPacket {
        peer_id: peer_id.to_string(),
        size,
    })
}

// Record a packet loss for diagnostics
pub fn record_packet_lost(peer_id: &str) -> bool {
    send_diagnostics_message(DiagnosticsMessage::RecordPacketLost {
        peer_id: peer_id.to_string(),
    })
}
