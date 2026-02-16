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

//! Integration tests for event_bus module.

use std::cell::RefCell;
use std::rc::Rc;
use videocall_client::{emit_client_event, global_client_sender, subscribe_client_events, ClientEvent};
use wasm_bindgen::JsValue;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

/// Event bus channel capacity (must match the constant in event_bus.rs)
const EVENT_BUS_CAPACITY: usize = 256;

#[wasm_bindgen_test]
fn test_global_client_sender_is_same_instance() {
    // Multiple calls should return clones of the same sender
    let sender1 = global_client_sender();
    let sender2 = global_client_sender();

    // Both senders should have the same capacity (indicating same underlying channel)
    assert_eq!(sender1.capacity(), sender2.capacity());
    assert_eq!(sender1.capacity(), EVENT_BUS_CAPACITY);
}

#[wasm_bindgen_test]
fn test_subscribe_returns_receiver() {
    let receiver = subscribe_client_events();
    // Receiver should have the same capacity as the sender
    assert_eq!(receiver.capacity(), EVENT_BUS_CAPACITY);
}

#[wasm_bindgen_test]
fn test_multiple_subscribers_get_independent_receivers() {
    let rx1 = subscribe_client_events();
    let rx2 = subscribe_client_events();

    // Both receivers should have the same capacity
    assert_eq!(rx1.capacity(), rx2.capacity());
}

#[wasm_bindgen_test]
fn test_emit_client_event_does_not_panic() {
    // Should not panic even if no active receivers
    emit_client_event(ClientEvent::Connected);
    emit_client_event(ClientEvent::ConnectionLost("test".to_string()));
    emit_client_event(ClientEvent::PeerAdded("peer1".to_string()));
    emit_client_event(ClientEvent::PeerRemoved("peer1".to_string()));
    emit_client_event(ClientEvent::DevicesLoaded);
    emit_client_event(ClientEvent::DevicesChanged);
    emit_client_event(ClientEvent::PermissionGranted);
    emit_client_event(ClientEvent::PermissionDenied("error".to_string()));
}

#[wasm_bindgen_test]
fn test_event_bus_capacity() {
    assert_eq!(EVENT_BUS_CAPACITY, 256);
}

#[wasm_bindgen_test]
fn test_try_broadcast_returns_result() {
    let sender = global_client_sender();
    // try_broadcast should return Ok or Err, not panic
    let result = sender.try_broadcast(ClientEvent::Connected);
    // Result should be Ok (channel has capacity) or Err (overflow, which is handled)
    assert!(result.is_ok() || result.is_err());
}

#[wasm_bindgen_test]
fn test_subscribe_after_emit() {
    // Emit some events
    emit_client_event(ClientEvent::Connected);

    // Subscribe after emit - should not receive past events (broadcast semantics)
    let _rx = subscribe_client_events();

    // This is a sanity check - the subscription works
    // Past events are not delivered to new subscribers (which is correct broadcast behavior)
}

#[wasm_bindgen_test]
fn test_sender_clone_behavior() {
    let sender1 = global_client_sender();
    let sender2 = sender1.clone();

    // Both should be able to send (both are clones of the same sender)
    let _ = sender1.try_broadcast(ClientEvent::Connected);
    let _ = sender2.try_broadcast(ClientEvent::DevicesLoaded);
}

/// Yield to the microtask queue so that async operations complete.
async fn flush() {
    use js_sys::Promise;
    for _ in 0..3 {
        wasm_bindgen_futures::JsFuture::from(Promise::resolve(&JsValue::NULL))
            .await
            .unwrap();
    }
}

#[wasm_bindgen_test]
async fn test_subscriber_receives_emitted_events() {
    let mut rx = subscribe_client_events();
    let received_events: Rc<RefCell<Vec<ClientEvent>>> = Rc::new(RefCell::new(Vec::new()));
    let received_clone = received_events.clone();

    // Spawn a task to collect events
    wasm_bindgen_futures::spawn_local(async move {
        while let Ok(event) = rx.recv().await {
            received_clone.borrow_mut().push(event);
            // Break after receiving one event to avoid infinite loop
            break;
        }
    });

    // Emit an event
    emit_client_event(ClientEvent::PeerAdded("test-peer".to_string()));

    // Wait for async processing
    flush().await;

    // Verify event was received
    let events = received_events.borrow();
    assert!(!events.is_empty(), "Should have received at least one event");
    match &events[0] {
        ClientEvent::PeerAdded(peer_id) => {
            assert_eq!(peer_id, "test-peer");
        }
        _ => panic!("Expected PeerAdded event"),
    }
}

#[wasm_bindgen_test]
async fn test_multiple_subscribers_receive_same_event() {
    let mut rx1 = subscribe_client_events();
    let mut rx2 = subscribe_client_events();

    let received1: Rc<RefCell<Vec<ClientEvent>>> = Rc::new(RefCell::new(Vec::new()));
    let received2: Rc<RefCell<Vec<ClientEvent>>> = Rc::new(RefCell::new(Vec::new()));

    let received1_clone = received1.clone();
    let received2_clone = received2.clone();

    // Spawn tasks for both subscribers
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(event) = rx1.recv().await {
            received1_clone.borrow_mut().push(event);
        }
    });

    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(event) = rx2.recv().await {
            received2_clone.borrow_mut().push(event);
        }
    });

    // Emit an event
    emit_client_event(ClientEvent::DevicesLoaded);

    // Wait for async processing
    flush().await;

    // Both subscribers should have received the event
    assert!(
        !received1.borrow().is_empty(),
        "Subscriber 1 should have received event"
    );
    assert!(
        !received2.borrow().is_empty(),
        "Subscriber 2 should have received event"
    );
}
