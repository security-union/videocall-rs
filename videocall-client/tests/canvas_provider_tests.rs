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

//! Integration tests for canvas_provider module.

use std::rc::Rc;
use videocall_client::{
    create_canvas_provider, CanvasIdProvider, DefaultCanvasIdProvider, DirectCanvasIdProvider,
};
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn test_default_canvas_id_provider_video() {
    let provider = DefaultCanvasIdProvider;
    assert_eq!(provider.get_video_canvas_id("user123"), "video-user123");
    assert_eq!(
        provider.get_video_canvas_id("alice@example.com"),
        "video-alice@example.com"
    );
    assert_eq!(provider.get_video_canvas_id(""), "video-");
}

#[wasm_bindgen_test]
fn test_default_canvas_id_provider_screen() {
    let provider = DefaultCanvasIdProvider;
    assert_eq!(provider.get_screen_canvas_id("user123"), "screen-user123");
    assert_eq!(
        provider.get_screen_canvas_id("alice@example.com"),
        "screen-alice@example.com"
    );
    assert_eq!(provider.get_screen_canvas_id(""), "screen-");
}

#[wasm_bindgen_test]
fn test_direct_canvas_id_provider_video() {
    let provider = DirectCanvasIdProvider;
    assert_eq!(provider.get_video_canvas_id("user123"), "user123");
    assert_eq!(
        provider.get_video_canvas_id("alice@example.com"),
        "alice@example.com"
    );
    assert_eq!(provider.get_video_canvas_id(""), "");
}

#[wasm_bindgen_test]
fn test_direct_canvas_id_provider_screen() {
    let provider = DirectCanvasIdProvider;
    assert_eq!(
        provider.get_screen_canvas_id("user123"),
        "screen-share-user123"
    );
    assert_eq!(
        provider.get_screen_canvas_id("alice@example.com"),
        "screen-share-alice@example.com"
    );
    assert_eq!(provider.get_screen_canvas_id(""), "screen-share-");
}

#[wasm_bindgen_test]
fn test_create_canvas_provider_with_closures() {
    let provider = create_canvas_provider(
        |peer_id| format!("custom-video-{}", peer_id),
        |peer_id| format!("custom-screen-{}", peer_id),
    );

    assert_eq!(provider.get_video_canvas_id("user1"), "custom-video-user1");
    assert_eq!(
        provider.get_screen_canvas_id("user1"),
        "custom-screen-user1"
    );
}

#[wasm_bindgen_test]
fn test_create_canvas_provider_captures_state() {
    let prefix = "prefix".to_string();
    let prefix_clone = prefix.clone();

    let provider = create_canvas_provider(
        move |peer_id| format!("{}-video-{}", prefix, peer_id),
        move |peer_id| format!("{}-screen-{}", prefix_clone, peer_id),
    );

    assert_eq!(provider.get_video_canvas_id("peer"), "prefix-video-peer");
    assert_eq!(provider.get_screen_canvas_id("peer"), "prefix-screen-peer");
}

#[wasm_bindgen_test]
fn test_default_canvas_id_provider_clone() {
    let provider = DefaultCanvasIdProvider;
    let cloned = provider.clone();
    assert_eq!(
        provider.get_video_canvas_id("test"),
        cloned.get_video_canvas_id("test")
    );
}

#[wasm_bindgen_test]
fn test_direct_canvas_id_provider_clone() {
    let provider = DirectCanvasIdProvider;
    let cloned = provider.clone();
    assert_eq!(
        provider.get_video_canvas_id("test"),
        cloned.get_video_canvas_id("test")
    );
}

#[wasm_bindgen_test]
fn test_default_canvas_id_provider_debug() {
    let provider = DefaultCanvasIdProvider;
    let debug_str = format!("{:?}", provider);
    assert!(debug_str.contains("DefaultCanvasIdProvider"));
}

#[wasm_bindgen_test]
fn test_direct_canvas_id_provider_debug() {
    let provider = DirectCanvasIdProvider;
    let debug_str = format!("{:?}", provider);
    assert!(debug_str.contains("DirectCanvasIdProvider"));
}

#[wasm_bindgen_test]
fn test_fn_canvas_id_provider_debug() {
    let provider = create_canvas_provider(
        |peer_id| peer_id.to_string(),
        |peer_id| peer_id.to_string(),
    );
    let debug_str = format!("{:?}", provider);
    assert!(debug_str.contains("FnCanvasIdProvider"));
    assert!(debug_str.contains("<closure>"));
}

#[wasm_bindgen_test]
fn test_canvas_provider_as_trait_object() {
    let default: Rc<dyn CanvasIdProvider> = Rc::new(DefaultCanvasIdProvider);
    let direct: Rc<dyn CanvasIdProvider> = Rc::new(DirectCanvasIdProvider);

    // Both should work as trait objects
    assert_eq!(default.get_video_canvas_id("test"), "video-test");
    assert_eq!(direct.get_video_canvas_id("test"), "test");
}

#[wasm_bindgen_test]
fn test_canvas_provider_with_special_characters() {
    let provider = DefaultCanvasIdProvider;

    // Test with various special characters that might appear in peer IDs
    assert_eq!(provider.get_video_canvas_id("user+tag"), "video-user+tag");
    assert_eq!(provider.get_video_canvas_id("user/path"), "video-user/path");
    assert_eq!(provider.get_video_canvas_id("user name"), "video-user name");
    assert_eq!(provider.get_video_canvas_id("用户"), "video-用户");
}
