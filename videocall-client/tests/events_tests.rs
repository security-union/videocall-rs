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

//! Integration tests for events module (ClientEvent enum).

use videocall_client::ClientEvent;
use videocall_types::protos::media_packet::media_packet::MediaType;
use wasm_bindgen_test::*;

// ScreenShareEvent is not exported from videocall_client, so we need to use the encode module
// For now, we'll skip the ScreenShareEvent tests or use a workaround

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn test_client_event_clone_connected() {
    let event = ClientEvent::Connected;
    let cloned = event.clone();
    assert!(matches!(cloned, ClientEvent::Connected));
}

#[wasm_bindgen_test]
fn test_client_event_clone_connection_lost() {
    let event = ClientEvent::ConnectionLost("network error".to_string());
    let cloned = event.clone();
    match cloned {
        ClientEvent::ConnectionLost(msg) => assert_eq!(msg, "network error"),
        _ => panic!("Expected ConnectionLost variant"),
    }
}

#[wasm_bindgen_test]
fn test_client_event_clone_peer_added() {
    let event = ClientEvent::PeerAdded("user@example.com".to_string());
    let cloned = event.clone();
    match cloned {
        ClientEvent::PeerAdded(peer_id) => assert_eq!(peer_id, "user@example.com"),
        _ => panic!("Expected PeerAdded variant"),
    }
}

#[wasm_bindgen_test]
fn test_client_event_clone_peer_removed() {
    let event = ClientEvent::PeerRemoved("user@example.com".to_string());
    let cloned = event.clone();
    match cloned {
        ClientEvent::PeerRemoved(peer_id) => assert_eq!(peer_id, "user@example.com"),
        _ => panic!("Expected PeerRemoved variant"),
    }
}

#[wasm_bindgen_test]
fn test_client_event_clone_peer_first_frame() {
    let event = ClientEvent::PeerFirstFrame {
        peer_id: "peer1".to_string(),
        media_type: MediaType::VIDEO,
    };
    let cloned = event.clone();
    match cloned {
        ClientEvent::PeerFirstFrame { peer_id, media_type } => {
            assert_eq!(peer_id, "peer1");
            assert_eq!(media_type, MediaType::VIDEO);
        }
        _ => panic!("Expected PeerFirstFrame variant"),
    }
}

#[wasm_bindgen_test]
fn test_client_event_clone_meeting_info() {
    let event = ClientEvent::MeetingInfo(1234567890.0);
    let cloned = event.clone();
    match cloned {
        ClientEvent::MeetingInfo(time) => assert_eq!(time, 1234567890.0),
        _ => panic!("Expected MeetingInfo variant"),
    }
}

#[wasm_bindgen_test]
fn test_client_event_clone_meeting_ended() {
    let event = ClientEvent::MeetingEnded {
        end_time_ms: 1234567890.0,
        message: "Meeting has ended".to_string(),
    };
    let cloned = event.clone();
    match cloned {
        ClientEvent::MeetingEnded { end_time_ms, message } => {
            assert_eq!(end_time_ms, 1234567890.0);
            assert_eq!(message, "Meeting has ended");
        }
        _ => panic!("Expected MeetingEnded variant"),
    }
}

#[wasm_bindgen_test]
fn test_client_event_clone_encoder_settings_update() {
    let event = ClientEvent::EncoderSettingsUpdate {
        encoder: "video".to_string(),
        settings: "bitrate=1000kbps".to_string(),
    };
    let cloned = event.clone();
    match cloned {
        ClientEvent::EncoderSettingsUpdate { encoder, settings } => {
            assert_eq!(encoder, "video");
            assert_eq!(settings, "bitrate=1000kbps");
        }
        _ => panic!("Expected EncoderSettingsUpdate variant"),
    }
}

#[wasm_bindgen_test]
fn test_client_event_clone_devices_loaded() {
    let event = ClientEvent::DevicesLoaded;
    let cloned = event.clone();
    assert!(matches!(cloned, ClientEvent::DevicesLoaded));
}

#[wasm_bindgen_test]
fn test_client_event_clone_devices_changed() {
    let event = ClientEvent::DevicesChanged;
    let cloned = event.clone();
    assert!(matches!(cloned, ClientEvent::DevicesChanged));
}

#[wasm_bindgen_test]
fn test_client_event_clone_permission_granted() {
    let event = ClientEvent::PermissionGranted;
    let cloned = event.clone();
    assert!(matches!(cloned, ClientEvent::PermissionGranted));
}

#[wasm_bindgen_test]
fn test_client_event_clone_permission_denied() {
    let event = ClientEvent::PermissionDenied("User denied permission".to_string());
    let cloned = event.clone();
    match cloned {
        ClientEvent::PermissionDenied(msg) => assert_eq!(msg, "User denied permission"),
        _ => panic!("Expected PermissionDenied variant"),
    }
}

#[wasm_bindgen_test]
fn test_client_event_debug_connected() {
    let event = ClientEvent::Connected;
    let debug_str = format!("{:?}", event);
    assert_eq!(debug_str, "Connected");
}

#[wasm_bindgen_test]
fn test_client_event_debug_connection_lost() {
    let event = ClientEvent::ConnectionLost("timeout".to_string());
    let debug_str = format!("{:?}", event);
    assert!(debug_str.contains("ConnectionLost"));
    assert!(debug_str.contains("timeout"));
}

#[wasm_bindgen_test]
fn test_client_event_debug_peer_added() {
    let event = ClientEvent::PeerAdded("alice".to_string());
    let debug_str = format!("{:?}", event);
    assert!(debug_str.contains("PeerAdded"));
    assert!(debug_str.contains("alice"));
}

#[wasm_bindgen_test]
fn test_client_event_debug_peer_first_frame() {
    let event = ClientEvent::PeerFirstFrame {
        peer_id: "bob".to_string(),
        media_type: MediaType::AUDIO,
    };
    let debug_str = format!("{:?}", event);
    assert!(debug_str.contains("PeerFirstFrame"));
    assert!(debug_str.contains("bob"));
}

#[wasm_bindgen_test]
fn test_client_event_debug_meeting_ended() {
    let event = ClientEvent::MeetingEnded {
        end_time_ms: 100.0,
        message: "Ended by host".to_string(),
    };
    let debug_str = format!("{:?}", event);
    assert!(debug_str.contains("MeetingEnded"));
    assert!(debug_str.contains("100"));
    assert!(debug_str.contains("Ended by host"));
}

#[wasm_bindgen_test]
fn test_client_event_debug_encoder_settings() {
    let event = ClientEvent::EncoderSettingsUpdate {
        encoder: "camera".to_string(),
        settings: "fps=30".to_string(),
    };
    let debug_str = format!("{:?}", event);
    assert!(debug_str.contains("EncoderSettingsUpdate"));
    assert!(debug_str.contains("camera"));
    assert!(debug_str.contains("fps=30"));
}

#[wasm_bindgen_test]
fn test_client_event_debug_devices_loaded() {
    let event = ClientEvent::DevicesLoaded;
    let debug_str = format!("{:?}", event);
    assert_eq!(debug_str, "DevicesLoaded");
}

#[wasm_bindgen_test]
fn test_client_event_debug_permission_granted() {
    let event = ClientEvent::PermissionGranted;
    let debug_str = format!("{:?}", event);
    assert_eq!(debug_str, "PermissionGranted");
}

#[wasm_bindgen_test]
fn test_client_event_debug_permission_denied() {
    let event = ClientEvent::PermissionDenied("denied".to_string());
    let debug_str = format!("{:?}", event);
    assert!(debug_str.contains("PermissionDenied"));
    assert!(debug_str.contains("denied"));
}
