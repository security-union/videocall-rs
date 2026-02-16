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

//! Integration tests for health_reporter module.

use serde_json::json;
use videocall_client::health_reporter::{HealthReporter, PeerHealthData};
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn test_peer_health_data_new() {
    let data = PeerHealthData::new("user123".to_string());
    assert_eq!(data.peer_id, "user123");
    assert!(data.last_neteq_stats.is_none());
    assert!(data.last_video_stats.is_none());
    assert!(!data.can_listen);
    assert!(!data.can_see);
    assert_eq!(data.last_update_ms, 0);
}

#[wasm_bindgen_test]
fn test_peer_health_data_update_audio_stats() {
    let mut data = PeerHealthData::new("peer1".to_string());
    let neteq_stats = json!({
        "current_buffer_size_ms": 50.0,
        "packets_awaiting_decode": 3.0
    });

    data.update_audio_stats(neteq_stats.clone());

    assert!(data.can_listen);
    assert!(data.last_neteq_stats.is_some());
    assert!(data.last_update_ms > 0);

    let stats = data.last_neteq_stats.unwrap();
    assert_eq!(stats["current_buffer_size_ms"], 50.0);
}

#[wasm_bindgen_test]
fn test_peer_health_data_update_video_stats() {
    let mut data = PeerHealthData::new("peer1".to_string());
    let video_stats = json!({
        "fps_received": 30.0,
        "frames_decoded": 1000
    });

    data.update_video_stats(video_stats.clone());

    assert!(data.can_see);
    assert!(data.last_video_stats.is_some());
    assert!(data.last_update_ms > 0);

    let stats = data.last_video_stats.unwrap();
    assert_eq!(stats["fps_received"], 30.0);
}

#[wasm_bindgen_test]
fn test_peer_health_data_mark_audio_timeout() {
    let mut data = PeerHealthData::new("peer1".to_string());
    data.can_listen = true;

    data.mark_audio_timeout();

    assert!(!data.can_listen);
}

#[wasm_bindgen_test]
fn test_peer_health_data_mark_video_timeout() {
    let mut data = PeerHealthData::new("peer1".to_string());
    data.can_see = true;

    data.mark_video_timeout();

    assert!(!data.can_see);
}

#[wasm_bindgen_test]
fn test_peer_health_data_clone() {
    let mut data = PeerHealthData::new("peer1".to_string());
    data.can_listen = true;
    data.can_see = true;
    data.last_update_ms = 12345;

    let cloned = data.clone();

    assert_eq!(cloned.peer_id, data.peer_id);
    assert_eq!(cloned.can_listen, data.can_listen);
    assert_eq!(cloned.can_see, data.can_see);
    assert_eq!(cloned.last_update_ms, data.last_update_ms);
}

#[wasm_bindgen_test]
fn test_health_reporter_new() {
    let reporter = HealthReporter::new(
        "session123".to_string(),
        "user@example.com".to_string(),
        5000,
    );

    // HealthReporter fields are private, so we test via debug output
    let debug_str = format!("{:?}", reporter);
    assert!(debug_str.contains("session123"));
    assert!(debug_str.contains("user@example.com"));
    assert!(debug_str.contains("5000"));
}

#[wasm_bindgen_test]
fn test_health_reporter_set_meeting_id() {
    let mut reporter = HealthReporter::new(
        "session123".to_string(),
        "user@example.com".to_string(),
        5000,
    );

    reporter.set_meeting_id("meeting456".to_string());

    let debug_str = format!("{:?}", reporter);
    assert!(debug_str.contains("meeting456"));
}

#[wasm_bindgen_test]
fn test_health_reporter_set_health_interval() {
    let mut reporter = HealthReporter::new(
        "session123".to_string(),
        "user@example.com".to_string(),
        5000,
    );

    reporter.set_health_interval(10000);

    let debug_str = format!("{:?}", reporter);
    assert!(debug_str.contains("10000"));
}

#[wasm_bindgen_test]
fn test_health_reporter_set_reporting_audio_enabled() {
    let reporter = HealthReporter::new(
        "session123".to_string(),
        "user@example.com".to_string(),
        5000,
    );

    // Initially disabled
    reporter.set_reporting_audio_enabled(true);
    reporter.set_reporting_audio_enabled(false);
    // No panic means success - internal state is private
}

#[wasm_bindgen_test]
fn test_health_reporter_set_reporting_video_enabled() {
    let reporter = HealthReporter::new(
        "session123".to_string(),
        "user@example.com".to_string(),
        5000,
    );

    reporter.set_reporting_video_enabled(true);
    reporter.set_reporting_video_enabled(false);
    // No panic means success - internal state is private
}

#[wasm_bindgen_test]
fn test_health_reporter_remove_peer() {
    let reporter = HealthReporter::new(
        "session123".to_string(),
        "user@example.com".to_string(),
        5000,
    );

    // Remove a non-existent peer should not panic
    reporter.remove_peer("peer1");
}

#[wasm_bindgen_test]
fn test_health_reporter_get_health_summary_empty() {
    let reporter = HealthReporter::new(
        "session123".to_string(),
        "user@example.com".to_string(),
        5000,
    );

    let summary = reporter.get_health_summary();

    // Should return Some with empty object
    assert!(summary.is_some());
    let obj = summary.unwrap();
    assert!(obj.as_object().unwrap().is_empty());
}

#[wasm_bindgen_test]
fn test_health_reporter_debug() {
    let reporter = HealthReporter::new(
        "session123".to_string(),
        "user@example.com".to_string(),
        5000,
    );

    let debug_str = format!("{:?}", reporter);

    assert!(debug_str.contains("HealthReporter"));
    assert!(debug_str.contains("session123"));
    assert!(debug_str.contains("user@example.com"));
    assert!(debug_str.contains("5000"));
}
