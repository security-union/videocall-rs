// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core types for the chat adapter layer.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::constants::RuntimeConfig;

/// A single chat message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Unique message identifier (assigned by the chat service).
    pub id: String,
    /// User ID of the sender.
    pub sender_id: String,
    /// Display name of the sender.
    pub sender_name: String,
    /// Message text content.
    pub content: String,
    /// Timestamp as milliseconds since Unix epoch.
    pub timestamp: f64,
}

/// A chat room associated with a meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRoom {
    /// Room identifier (typically `chatRoomPrefix + meeting_id`).
    pub id: String,
    /// Human-readable room name.
    pub name: String,
}

/// Errors that can occur during chat operations.
#[derive(Debug, Clone)]
pub enum ChatError {
    /// A network / HTTP request failed.
    NetworkError(String),
    /// Authentication with the chat service failed.
    AuthError(String),
    /// The adapter is not connected (e.g. `authenticate` was not called).
    #[allow(dead_code)]
    NotConnected,
    /// The chat configuration is missing or invalid.
    InvalidConfig(String),
}

impl fmt::Display for ChatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatError::NetworkError(msg) => write!(f, "Chat network error: {msg}"),
            ChatError::AuthError(msg) => write!(f, "Chat auth error: {msg}"),
            ChatError::NotConnected => write!(f, "Chat adapter not connected"),
            ChatError::InvalidConfig(msg) => write!(f, "Chat config error: {msg}"),
        }
    }
}

/// How the chat adapter authenticates with the external service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatAuthMode {
    /// Exchange videocall session for a chat-specific bearer token.
    Bearer,
    /// Same-origin cookies are sent automatically.
    Cookie,
    /// User identity sent via a custom HTTP header.
    Header,
    /// User identity appended as a query parameter.
    Query,
}

impl ChatAuthMode {
    /// Parse the string value from the runtime configuration.
    pub fn from_config(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "bearer" => Some(Self::Bearer),
            "cookie" => Some(Self::Cookie),
            "header" => Some(Self::Header),
            "query" => Some(Self::Query),
            _ => None,
        }
    }
}

/// Typed configuration extracted from `RuntimeConfig` chat fields.
///
/// Constructed via `ChatConfig::from_runtime_config` which validates
/// required fields and provides defaults for optional ones.
#[derive(Debug, Clone)]
pub struct ChatConfig {
    /// Base URL for the external chat service API.
    pub api_base_url: String,
    /// Authentication mode to use.
    pub auth_mode: ChatAuthMode,
    /// Endpoint on the meeting API to exchange a videocall session for a
    /// chat bearer token (only used when `auth_mode` is `Bearer`).
    pub auth_token_endpoint: Option<String>,
    /// Custom header name for `Header` auth mode.
    pub auth_header_name: Option<String>,
    /// Query parameter name for `Query` auth mode.
    pub auth_query_param: Option<String>,
    /// POST endpoint to create / join a room.
    pub create_room_endpoint: String,
    /// GET/POST endpoint template for messages (supports `{roomId}` placeholder).
    pub messages_endpoint: String,
    /// Optional WebSocket URL for real-time message streaming.
    #[allow(dead_code)]
    pub web_socket_url: Option<String>,
    /// Prefix prepended to meeting IDs to form room IDs.
    pub room_prefix: String,
    /// Extra HTTP headers to include on every request (JSON-decoded).
    pub extra_headers: HashMap<String, String>,
    /// Extra query parameters to include on every request (JSON-decoded).
    pub extra_params: HashMap<String, String>,
    /// Polling interval in milliseconds when WebSocket is not configured.
    pub poll_interval_ms: u32,
}

impl ChatConfig {
    /// Build a `ChatConfig` from the runtime configuration.
    ///
    /// Returns `Err(ChatError::InvalidConfig)` when required fields are
    /// missing or malformed.
    pub fn from_runtime_config(cfg: &RuntimeConfig) -> Result<Self, ChatError> {
        let api_base_url = cfg
            .chat_api_base_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ChatError::InvalidConfig("chatApiBaseUrl is required".into()))?
            .to_string();

        let auth_mode_str = cfg
            .chat_auth_mode
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ChatError::InvalidConfig("chatAuthMode is required".into()))?;
        let auth_mode = ChatAuthMode::from_config(auth_mode_str).ok_or_else(|| {
            ChatError::InvalidConfig(format!("Unknown chatAuthMode: {auth_mode_str}"))
        })?;

        let create_room_endpoint = cfg
            .chat_create_room_endpoint
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ChatError::InvalidConfig("chatCreateRoomEndpoint is required".into()))?
            .to_string();

        let messages_endpoint = cfg
            .chat_messages_endpoint
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ChatError::InvalidConfig("chatMessagesEndpoint is required".into()))?
            .to_string();

        let extra_headers = parse_json_map(cfg.chat_extra_headers.as_deref());
        let extra_params = parse_json_map(cfg.chat_extra_params.as_deref());

        Ok(Self {
            api_base_url,
            auth_mode,
            auth_token_endpoint: cfg
                .chat_auth_token_endpoint
                .clone()
                .filter(|s| !s.is_empty()),
            auth_header_name: cfg.chat_auth_header_name.clone().filter(|s| !s.is_empty()),
            auth_query_param: cfg.chat_auth_query_param.clone().filter(|s| !s.is_empty()),
            create_room_endpoint,
            messages_endpoint,
            web_socket_url: cfg.chat_web_socket_url.clone().filter(|s| !s.is_empty()),
            room_prefix: cfg.chat_room_prefix.as_deref().unwrap_or("").to_string(),
            extra_headers,
            extra_params,
            poll_interval_ms: cfg.chat_poll_interval_ms.unwrap_or(3000),
        })
    }
}

/// Parse a JSON-encoded string map, returning an empty map on failure.
fn parse_json_map(raw: Option<&str>) -> HashMap<String, String> {
    raw.filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(s).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auth_modes() {
        assert_eq!(
            ChatAuthMode::from_config("bearer"),
            Some(ChatAuthMode::Bearer)
        );
        assert_eq!(
            ChatAuthMode::from_config("Bearer"),
            Some(ChatAuthMode::Bearer)
        );
        assert_eq!(
            ChatAuthMode::from_config("COOKIE"),
            Some(ChatAuthMode::Cookie)
        );
        assert_eq!(
            ChatAuthMode::from_config("header"),
            Some(ChatAuthMode::Header)
        );
        assert_eq!(
            ChatAuthMode::from_config("query"),
            Some(ChatAuthMode::Query)
        );
        assert_eq!(ChatAuthMode::from_config("unknown"), None);
    }

    #[test]
    fn parse_json_map_success() {
        let result = parse_json_map(Some(r#"{"X-Custom":"value"}"#));
        assert_eq!(result.get("X-Custom").unwrap(), "value");
    }

    #[test]
    fn parse_json_map_empty() {
        assert!(parse_json_map(None).is_empty());
        assert!(parse_json_map(Some("")).is_empty());
        assert!(parse_json_map(Some("not json")).is_empty());
    }
}
