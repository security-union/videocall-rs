// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trait defining the interface for external chat service adapters.
//!
//! Implementations of [`ChatServiceAdapter`] encapsulate all communication
//! with an external chat backend. The trait is designed for a WASM
//! environment where all I/O goes through browser `fetch` / WebSocket.

use super::types::{ChatError, ChatMessage, ChatRoom};

/// Adapter interface for external chat service communication.
///
/// All methods are async because they involve network I/O. The trait is
/// `!Send` by default (WASM is single-threaded), so implementations do
/// not need `Send` bounds on their futures.
pub trait ChatServiceAdapter {
    /// Authenticate with the chat service using the current user's identity.
    ///
    /// Depending on the configured auth mode this may:
    /// - Exchange the videocall session for a chat-specific bearer token
    /// - Store user identity for header/query-based auth
    /// - Be a no-op (cookie mode)
    fn authenticate(
        &mut self,
        user_id: &str,
        display_name: &str,
    ) -> impl std::future::Future<Output = Result<(), ChatError>>;

    /// Join (or create) the chat room for the given meeting.
    ///
    /// The room ID is derived from the configured prefix and meeting ID.
    fn join_room(
        &mut self,
        meeting_id: &str,
    ) -> impl std::future::Future<Output = Result<ChatRoom, ChatError>>;

    /// Send a text message to the specified room.
    fn send_message(
        &self,
        room_id: &str,
        content: &str,
    ) -> impl std::future::Future<Output = Result<ChatMessage, ChatError>>;

    /// Retrieve messages from the specified room.
    ///
    /// When `since` is `Some(timestamp)`, only messages newer than that
    /// timestamp (milliseconds since epoch) are returned.
    fn get_messages(
        &self,
        room_id: &str,
        since: Option<f64>,
    ) -> impl std::future::Future<Output = Result<Vec<ChatMessage>, ChatError>>;

    /// Disconnect from the chat service and clean up resources.
    fn disconnect(&mut self) -> impl std::future::Future<Output = Result<(), ChatError>>;
}
