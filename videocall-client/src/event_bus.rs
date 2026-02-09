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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Global event bus for framework-agnostic client events.
//!
//! This module provides a MPMC (multi-producer, multi-consumer) broadcast channel
//! for client events. Any component can subscribe to receive events, and any
//! component can emit events.
//!
//! # Example
//!
//! ```ignore
//! use videocall_client::{subscribe_client_events, emit_client_event, ClientEvent};
//!
//! // Subscribe to events
//! let mut rx = subscribe_client_events();
//! wasm_bindgen_futures::spawn_local(async move {
//!     while let Ok(event) = rx.recv().await {
//!         match event {
//!             ClientEvent::PeerAdded(peer_id) => {
//!                 // Handle peer added
//!             }
//!             ClientEvent::Connected => {
//!                 // Handle connected
//!             }
//!             _ => {}
//!         }
//!     }
//! });
//!
//! // Emit an event
//! emit_client_event(ClientEvent::Connected);
//! ```

use crate::events::ClientEvent;
use async_broadcast::{broadcast, Receiver, Sender};
use once_cell::sync::Lazy;
use std::ops::Deref;

/// Capacity of the event bus channel
const EVENT_BUS_CAPACITY: usize = 256;

/// Global sender for client events
static SENDER: Lazy<Sender<ClientEvent>> = Lazy::new(|| {
    let (s, r) = broadcast(EVENT_BUS_CAPACITY);

    // Create a background task that keeps a receiver active
    // This prevents the channel from closing when there are no active receivers
    #[cfg(target_arch = "wasm32")]
    {
        let mut receiver = r;
        wasm_bindgen_futures::spawn_local(async move {
            // Keep the receiver alive to prevent channel closure
            // This receiver will consume messages but not process them
            while (receiver.recv().await).is_ok() {
                // Intentionally discard messages in the background receiver
                // This keeps the channel open for other receivers
            }
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        // For native targets, just drop the receiver
        std::mem::drop(r);
    }

    s
});

/// Get the global sender for emitting client events.
///
/// This is used internally by the client to emit events.
pub fn global_client_sender() -> Sender<ClientEvent> {
    SENDER.deref().clone()
}

/// Subscribe to client events.
///
/// Returns a receiver that will receive all future client events.
/// Each subscriber receives all events independently (broadcast pattern).
///
/// # Example
///
/// ```ignore
/// use videocall_client::{subscribe_client_events, ClientEvent};
///
/// let mut rx = subscribe_client_events();
/// wasm_bindgen_futures::spawn_local(async move {
///     while let Ok(event) = rx.recv().await {
///         match event {
///             ClientEvent::PeerAdded(peer_id) => println!("Peer joined: {}", peer_id),
///             _ => {}
///         }
///     }
/// });
/// ```
pub fn subscribe_client_events() -> Receiver<ClientEvent> {
    SENDER.deref().new_receiver()
}

/// Emit a client event to all subscribers.
///
/// This is a non-blocking operation. If the channel is full, the oldest
/// message will be dropped to make room (overflow behavior).
///
/// # Example
///
/// ```ignore
/// use videocall_client::{emit_client_event, ClientEvent};
///
/// emit_client_event(ClientEvent::Connected);
/// ```
pub fn emit_client_event(event: ClientEvent) {
    let _ = global_client_sender().try_broadcast(event);
}
