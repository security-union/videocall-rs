// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dioxus context types for chat state management.
//!
//! Provides reactive signals that components can consume to render chat
//! UI elements. The signals are created once and shared via Dioxus context
//! providers.

use dioxus::prelude::*;

use super::types::ChatMessage;

/// Shared chat state accessible via Dioxus context.
///
/// Each field is a [`Signal`] so individual components can subscribe to
/// only the slice of state they care about, avoiding unnecessary
/// re-renders.
#[derive(Clone, Copy, PartialEq)]
pub struct ChatState {
    /// All messages in the current room, ordered by timestamp.
    pub messages: Signal<Vec<ChatMessage>>,
    /// Whether the adapter is connected and authenticated.
    pub is_connected: Signal<bool>,
    /// The room ID the user is currently in, if any.
    pub current_room_id: Signal<Option<String>>,
    /// The most recent error message, if any.
    pub error: Signal<Option<String>>,
}

impl ChatState {
    /// Create a new `ChatState` with default (empty) values.
    fn new() -> Self {
        Self {
            messages: Signal::new(Vec::new()),
            is_connected: Signal::new(false),
            current_room_id: Signal::new(None),
            error: Signal::new(None),
        }
    }
}

/// Hook that provides a [`ChatState`] through the Dioxus context system.
///
/// On first call this creates the state and provides it. Subsequent calls
/// (from child components) consume the existing context.
pub fn use_chat_state() -> ChatState {
    // Try to consume an existing context first. If none exists, create
    // and provide one.
    match try_consume_context::<ChatState>() {
        Some(state) => state,
        None => {
            let state = ChatState::new();
            provide_context(state);
            state
        }
    }
}
