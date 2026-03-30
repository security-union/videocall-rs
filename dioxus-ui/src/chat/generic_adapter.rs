// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration-driven chat adapter that works with any external chat
//! service whose API follows a standard REST pattern.
//!
//! All HTTP requests go through `reqwest` which compiles to browser `fetch`
//! on `wasm32-unknown-unknown`.

use std::collections::HashMap;

use reqwest::Client;
use serde::Deserialize;

use super::adapter::ChatServiceAdapter;
use super::types::{ChatAuthMode, ChatConfig, ChatError, ChatMessage, ChatRoom};

/// Generic adapter that communicates with an external chat service based
/// entirely on the deploy-time [`ChatConfig`].
pub struct GenericChatAdapter {
    config: ChatConfig,
    http: Client,
    /// Bearer token obtained from the token exchange endpoint.
    auth_token: Option<String>,
    /// User identity stored for header/query auth modes.
    user_id: Option<String>,
    /// Display name stored for header/query auth modes.
    display_name: Option<String>,
    /// Current room ID (set after `join_room`).
    current_room_id: Option<String>,
}

/// Response shape expected from the meeting-api token exchange endpoint,
/// which wraps the result in an `APIResponse` envelope.
#[derive(Debug, Deserialize)]
struct ApiTokenResponse {
    result: TokenResult,
}

#[derive(Debug, Deserialize)]
struct TokenResult {
    token: String,
}

/// Response shape expected from the create-room endpoint.
#[derive(Debug, Deserialize)]
struct CreateRoomResponse {
    id: String,
    #[serde(default)]
    name: String,
}

/// Response shape expected from the send-message endpoint.
#[derive(Debug, Deserialize)]
struct SendMessageResponse {
    id: String,
    #[serde(default)]
    sender_id: String,
    #[serde(default)]
    sender_name: String,
    content: String,
    #[serde(default)]
    timestamp: f64,
}

/// Response shape expected from the get-messages endpoint.
#[derive(Debug, Deserialize)]
struct MessagesResponse {
    messages: Vec<ChatMessage>,
}

impl GenericChatAdapter {
    /// Create a new adapter from the given configuration.
    pub fn new(config: ChatConfig) -> Self {
        Self {
            config,
            http: Client::new(),
            auth_token: None,
            user_id: None,
            display_name: None,
            current_room_id: None,
        }
    }

    /// Build the full URL for a chat service endpoint path.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.api_base_url, path)
    }

    /// Replace `{roomId}` in an endpoint template with the actual room ID.
    fn resolve_endpoint(&self, template: &str, room_id: &str) -> String {
        template.replace("{roomId}", room_id)
    }

    /// Apply authentication, extra headers, and extra query params to a
    /// request builder.
    fn apply_auth(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.config.auth_mode {
            ChatAuthMode::Bearer => {
                if let Some(ref token) = self.auth_token {
                    builder =
                        builder.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
                }
            }
            ChatAuthMode::Cookie => {
                // On wasm32 reqwest uses fetch; credentials are included
                // automatically when the request targets the same origin.
                // For cross-origin we need fetch_credentials_include.
                #[cfg(target_arch = "wasm32")]
                {
                    builder = builder.fetch_credentials_include();
                }
            }
            ChatAuthMode::Header => {
                if let Some(ref header_name) = self.config.auth_header_name {
                    if let Some(ref uid) = self.user_id {
                        builder = builder.header(header_name, uid.as_str());
                    }
                }
            }
            ChatAuthMode::Query => {
                if let Some(ref param) = self.config.auth_query_param {
                    if let Some(ref uid) = self.user_id {
                        builder = builder.query(&[(param.as_str(), uid.as_str())]);
                    }
                }
            }
        }

        // Extra headers from config.
        for (key, value) in &self.config.extra_headers {
            builder = builder.header(key, value);
        }

        // Extra query params from config.
        if !self.config.extra_params.is_empty() {
            let pairs: Vec<(&str, &str)> = self
                .config
                .extra_params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            builder = builder.query(&pairs);
        }

        builder
    }

    /// Check if an HTTP response was a 401 and log a warning.
    /// Returns `true` if the response was a 401.
    fn check_unauthorized(status: reqwest::StatusCode) -> bool {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            log::warn!("Chat service returned 401 — re-authentication may be needed");
            true
        } else {
            false
        }
    }
}

impl ChatServiceAdapter for GenericChatAdapter {
    async fn authenticate(&mut self, user_id: &str, display_name: &str) -> Result<(), ChatError> {
        self.user_id = Some(user_id.to_string());
        self.display_name = Some(display_name.to_string());

        match self.config.auth_mode {
            ChatAuthMode::Bearer => {
                let endpoint = self.config.auth_token_endpoint.as_deref().ok_or_else(|| {
                    ChatError::InvalidConfig(
                        "chatAuthTokenEndpoint is required for bearer auth mode".into(),
                    )
                })?;

                // The token endpoint is a full URL pointing at the meeting-api,
                // not a path relative to chatApiBaseUrl.
                let url = endpoint.to_string();
                let mut body = HashMap::new();
                body.insert("user_id", user_id);
                body.insert("display_name", display_name);

                let request = self.http.post(&url).json(&body);
                let request = self.apply_auth(request);
                let response = request
                    .send()
                    .await
                    .map_err(|e| ChatError::NetworkError(e.to_string()))?;

                if Self::check_unauthorized(response.status()) {
                    return Err(ChatError::AuthError("Token exchange returned 401".into()));
                }

                if !response.status().is_success() {
                    return Err(ChatError::AuthError(format!(
                        "Token exchange failed with status {}",
                        response.status()
                    )));
                }

                let api_resp: ApiTokenResponse = response
                    .json()
                    .await
                    .map_err(|e| ChatError::NetworkError(e.to_string()))?;

                self.auth_token = Some(api_resp.result.token);
                log::info!("Chat: bearer token obtained via token exchange");
            }
            ChatAuthMode::Cookie => {
                // No-op: cookies are sent automatically by the browser.
                log::info!("Chat: using cookie auth (no explicit authentication step)");
            }
            ChatAuthMode::Header | ChatAuthMode::Query => {
                // Identity is stored above; it will be applied to every request.
                log::info!(
                    "Chat: using {:?} auth mode, identity stored for user {user_id}",
                    self.config.auth_mode
                );
            }
        }

        Ok(())
    }

    async fn join_room(&mut self, meeting_id: &str) -> Result<ChatRoom, ChatError> {
        let room_id = format!("{}{}", self.config.room_prefix, meeting_id);
        let url = self.url(&self.config.create_room_endpoint.clone());

        let mut body = HashMap::new();
        body.insert("room_id", room_id.as_str());

        let request = self.http.post(&url).json(&body);
        let request = self.apply_auth(request);
        let response = request
            .send()
            .await
            .map_err(|e| ChatError::NetworkError(e.to_string()))?;

        if Self::check_unauthorized(response.status()) {
            return Err(ChatError::AuthError("Create room returned 401".into()));
        }

        if !response.status().is_success() {
            return Err(ChatError::NetworkError(format!(
                "Create room failed with status {}",
                response.status()
            )));
        }

        let room_resp: CreateRoomResponse = response
            .json()
            .await
            .map_err(|e| ChatError::NetworkError(e.to_string()))?;

        let room = ChatRoom {
            id: room_resp.id,
            name: if room_resp.name.is_empty() {
                room_id.clone()
            } else {
                room_resp.name
            },
        };

        self.current_room_id = Some(room.id.clone());
        log::info!("Chat: joined room {} ({})", room.id, room.name);

        Ok(room)
    }

    async fn send_message(&self, room_id: &str, content: &str) -> Result<ChatMessage, ChatError> {
        let path = self.resolve_endpoint(&self.config.messages_endpoint, room_id);
        let url = self.url(&path);

        let mut body = HashMap::new();
        body.insert("content", content);
        if let Some(ref uid) = self.user_id {
            body.insert("sender_id", uid.as_str());
        }
        if let Some(ref name) = self.display_name {
            body.insert("sender_name", name.as_str());
        }

        let request = self.http.post(&url).json(&body);
        let request = self.apply_auth(request);
        let response = request
            .send()
            .await
            .map_err(|e| ChatError::NetworkError(e.to_string()))?;

        if Self::check_unauthorized(response.status()) {
            return Err(ChatError::AuthError("Send message returned 401".into()));
        }

        if !response.status().is_success() {
            return Err(ChatError::NetworkError(format!(
                "Send message failed with status {}",
                response.status()
            )));
        }

        let msg_resp: SendMessageResponse = response
            .json()
            .await
            .map_err(|e| ChatError::NetworkError(e.to_string()))?;

        Ok(ChatMessage {
            id: msg_resp.id,
            sender_id: if msg_resp.sender_id.is_empty() {
                self.user_id.clone().unwrap_or_default()
            } else {
                msg_resp.sender_id
            },
            sender_name: if msg_resp.sender_name.is_empty() {
                self.display_name.clone().unwrap_or_default()
            } else {
                msg_resp.sender_name
            },
            content: msg_resp.content,
            timestamp: msg_resp.timestamp,
        })
    }

    async fn get_messages(
        &self,
        room_id: &str,
        since: Option<f64>,
    ) -> Result<Vec<ChatMessage>, ChatError> {
        let path = self.resolve_endpoint(&self.config.messages_endpoint, room_id);
        let url = self.url(&path);

        let mut request = self.http.get(&url);

        if let Some(ts) = since {
            request = request.query(&[("since", ts.to_string())]);
        }

        let request = self.apply_auth(request);
        let response = request
            .send()
            .await
            .map_err(|e| ChatError::NetworkError(e.to_string()))?;

        if Self::check_unauthorized(response.status()) {
            return Err(ChatError::AuthError("Get messages returned 401".into()));
        }

        if !response.status().is_success() {
            return Err(ChatError::NetworkError(format!(
                "Get messages failed with status {}",
                response.status()
            )));
        }

        let msgs: MessagesResponse = response
            .json()
            .await
            .map_err(|e| ChatError::NetworkError(e.to_string()))?;

        Ok(msgs.messages)
    }

    async fn disconnect(&mut self) -> Result<(), ChatError> {
        log::info!("Chat: disconnecting (room: {:?})", self.current_room_id);
        self.auth_token = None;
        self.current_room_id = None;
        self.user_id = None;
        self.display_name = None;
        Ok(())
    }
}
