// SPDX-License-Identifier: MIT OR Apache-2.0

//! JMAP protocol chat adapter for Smatter chat server integration.
//!
//! Implements [`ChatServiceAdapter`] using the JMAP protocol to communicate
//! with a Smatter-compatible chat backend. All JMAP-specific types are
//! private to this module.

use std::collections::HashMap;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::adapter::ChatServiceAdapter;
use super::types::{ChatAuthMode, ChatConfig, ChatError, ChatMessage, ChatRoom};

// ---------------------------------------------------------------------------
// JMAP request/response types (private to this module)
// ---------------------------------------------------------------------------

/// Top-level JMAP request envelope.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JmapRequest {
    using: Vec<String>,
    method_calls: Vec<(String, Value, String)>,
}

/// Top-level JMAP response envelope.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmapResponse {
    method_responses: Vec<(String, Value, String)>,
}

/// Response from `POST /auth/login`.
#[derive(Deserialize)]
struct AuthLoginResponse {
    token: String,
    user: AuthUser,
    #[allow(dead_code)]
    expires_in: i64,
}

/// User object returned from the login endpoint.
#[derive(Deserialize)]
struct AuthUser {
    id: String,
}

/// A Smatter conversation object (subset of fields we need).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmapConversation {
    id: String,
    topic: String,
}

/// Wrapper for `Conversation/get` list response.
#[derive(Deserialize)]
struct ConversationGetResponse {
    list: Vec<JmapConversation>,
}

/// Wrapper for `Conversation/query` response.
#[derive(Deserialize)]
struct ConversationQueryResponse {
    ids: Vec<String>,
}

/// Wrapper for `Conversation/create` response (contains the created conversation).
#[derive(Deserialize)]
struct ConversationCreateResponse {
    list: Vec<JmapConversation>,
}

/// A Smatter chat message object (subset of fields we need).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmapChatMessage {
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    conversation_id: String,
    from: JmapParticipant,
    sent_at: String,
    text_body: String,
}

/// Participant reference within a message.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmapParticipant {
    user_id: String,
    display_name: Option<String>,
}

/// Response body for `ChatMessage/set` — `created` map.
#[derive(Deserialize)]
struct SetMessageResponse {
    #[serde(default)]
    created: HashMap<String, JmapChatMessage>,
}

/// Response body for `ChatMessage/get` — `list` field.
#[derive(Deserialize)]
struct GetMessagesResponse {
    list: Vec<JmapChatMessage>,
}

/// Response body for `ChatMessage/query` — `ids` field.
#[derive(Deserialize)]
struct QueryMessagesResponse {
    ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// JMAP protocol adapter for Smatter chat server.
pub struct JmapChatAdapter {
    config: ChatConfig,
    http: Client,
    /// Bearer token for authenticating JMAP requests.
    auth_token: Option<String>,
    /// Smatter user ID (obtained during authentication).
    user_id: String,
    /// Display name for the current user.
    display_name: String,
    /// Current conversation ID (set after `join_room`).
    current_room_id: Option<String>,
}

impl JmapChatAdapter {
    /// Create a new JMAP adapter from the given configuration.
    pub fn new(config: ChatConfig) -> Self {
        Self {
            config,
            http: Client::new(),
            auth_token: None,
            user_id: String::new(),
            display_name: String::new(),
            current_room_id: None,
        }
    }

    /// Build the JMAP endpoint URL.
    fn jmap_url(&self) -> String {
        format!("{}/jmap", self.config.api_base_url)
    }

    /// Apply authentication, extra headers, and extra query params to a
    /// request builder.
    fn apply_auth(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref token) = self.auth_token {
            builder = builder.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
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

    /// Send a JMAP request and return the parsed response.
    async fn jmap_call(
        &self,
        method_calls: Vec<(String, Value, String)>,
    ) -> Result<JmapResponse, ChatError> {
        let body = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core".to_string()],
            method_calls,
        };

        let request = self.http.post(self.jmap_url()).json(&body);
        let request = self.apply_auth(request);

        let response = request
            .send()
            .await
            .map_err(|e| ChatError::NetworkError(e.to_string()))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ChatError::AuthError("JMAP request returned 401".into()));
        }

        if !response.status().is_success() {
            return Err(ChatError::NetworkError(format!(
                "JMAP request failed with status {}",
                response.status()
            )));
        }

        response
            .json::<JmapResponse>()
            .await
            .map_err(|e| ChatError::NetworkError(format!("Failed to parse JMAP response: {e}")))
    }

    /// Extract the response value from a specific method call index.
    fn extract_response(resp: &JmapResponse, index: usize) -> Result<&Value, ChatError> {
        resp.method_responses
            .get(index)
            .map(|(_, value, _)| value)
            .ok_or_else(|| {
                ChatError::NetworkError(format!(
                    "JMAP response missing method response at index {index}"
                ))
            })
    }

    /// Convert a Smatter `sentAt` ISO-8601 timestamp string to milliseconds since epoch.
    fn parse_timestamp(sent_at: &str) -> f64 {
        // Try parsing ISO-8601 with chrono-like manual approach.
        // js_sys::Date can parse ISO strings reliably in the browser.
        let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(sent_at));
        let ms = date.get_time();
        if ms.is_nan() {
            0.0
        } else {
            ms
        }
    }

    /// Convert a JMAP message to our internal ChatMessage type.
    fn to_chat_message(msg: &JmapChatMessage) -> ChatMessage {
        ChatMessage {
            id: msg.id.clone(),
            sender_id: msg.from.user_id.clone(),
            sender_name: msg
                .from
                .display_name
                .clone()
                .unwrap_or_else(|| msg.from.user_id.clone()),
            content: msg.text_body.clone(),
            timestamp: Self::parse_timestamp(&msg.sent_at),
        }
    }
}

impl ChatServiceAdapter for JmapChatAdapter {
    async fn authenticate(&mut self, user_id: &str, display_name: &str) -> Result<(), ChatError> {
        self.user_id = user_id.to_string();
        self.display_name = display_name.to_string();

        match self.config.auth_mode {
            ChatAuthMode::Bearer => {
                // If a token endpoint is configured, exchange the videocall
                // session for a Smatter bearer token (same pattern as REST adapter).
                if let Some(ref endpoint) = self.config.auth_token_endpoint {
                    let mut body = HashMap::new();
                    body.insert("user_id", user_id);
                    body.insert("display_name", display_name);

                    let response = self
                        .http
                        .post(endpoint)
                        .json(&body)
                        .send()
                        .await
                        .map_err(|e| ChatError::NetworkError(e.to_string()))?;

                    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                        return Err(ChatError::AuthError("Token exchange returned 401".into()));
                    }
                    if !response.status().is_success() {
                        return Err(ChatError::AuthError(format!(
                            "Token exchange failed with status {}",
                            response.status()
                        )));
                    }

                    // The Smatter login endpoint returns { token, user, expires_in }.
                    let auth_resp: AuthLoginResponse = response
                        .json()
                        .await
                        .map_err(|e| ChatError::NetworkError(e.to_string()))?;

                    self.auth_token = Some(auth_resp.token);
                    self.user_id = auth_resp.user.id;
                    log::info!("Chat JMAP: bearer token obtained via token exchange");
                } else {
                    log::warn!("Chat: no auth token endpoint configured for JMAP; requests will be unauthenticated");
                }
            }
            ChatAuthMode::Cookie => {
                log::info!("Chat JMAP: using cookie auth (no explicit authentication step)");
            }
            ChatAuthMode::Header | ChatAuthMode::Query => {
                log::info!(
                    "Chat JMAP: using {:?} auth mode, identity stored for user {user_id}",
                    self.config.auth_mode
                );
            }
        }

        Ok(())
    }

    async fn join_room(&mut self, meeting_id: &str) -> Result<ChatRoom, ChatError> {
        let topic = format!("{}{}", self.config.room_prefix, meeting_id);

        // Step 1: Query for existing conversations the user participates in.
        let query_args = serde_json::json!({
            "accountId": self.user_id,
        });

        let resp = self
            .jmap_call(vec![(
                "Conversation/query".to_string(),
                query_args,
                "q1".to_string(),
            )])
            .await?;

        let query_value = Self::extract_response(&resp, 0)?;
        let query_resp: ConversationQueryResponse = serde_json::from_value(query_value.clone())
            .map_err(|e| {
                ChatError::NetworkError(format!("Failed to parse Conversation/query response: {e}"))
            })?;

        // Step 2: If we got conversation IDs, fetch them and look for a topic match.
        if !query_resp.ids.is_empty() {
            let get_args = serde_json::json!({
                "accountId": self.user_id,
                "ids": query_resp.ids,
            });

            let resp = self
                .jmap_call(vec![(
                    "Conversation/get".to_string(),
                    get_args,
                    "g1".to_string(),
                )])
                .await?;

            let get_value = Self::extract_response(&resp, 0)?;
            let get_resp: ConversationGetResponse = serde_json::from_value(get_value.clone())
                .map_err(|e| {
                    ChatError::NetworkError(format!(
                        "Failed to parse Conversation/get response: {e}"
                    ))
                })?;

            // Look for a conversation whose topic matches our meeting room topic.
            if let Some(conv) = get_resp.list.iter().find(|c| c.topic == topic) {
                let room = ChatRoom {
                    id: conv.id.clone(),
                    name: conv.topic.clone(),
                };
                self.current_room_id = Some(room.id.clone());
                log::info!(
                    "Chat JMAP: found existing conversation {} ({})",
                    room.id,
                    room.name
                );

                // Best-effort: ensure the current user is a participant.
                let mut update = serde_json::Map::new();
                update.insert(
                    self.user_id.clone(),
                    serde_json::json!({
                        "userId": self.user_id,
                        "displayName": self.display_name,
                        "role": "member"
                    }),
                );
                let _ = self
                    .jmap_call(vec![(
                        "Conversation/setMembers".to_string(),
                        serde_json::json!({
                            "accountId": self.user_id,
                            "conversationId": conv.id,
                            "update": update,
                        }),
                        "sm1".to_string(),
                    )])
                    .await;

                return Ok(room);
            }
        }

        // Step 3: No existing conversation found — create a new one.
        let create_args = serde_json::json!({
            "creatorId": self.user_id,
            "topic": topic,
            "isDirectMessage": false,
        });

        let resp = self
            .jmap_call(vec![(
                "Conversation/create".to_string(),
                create_args,
                "c1".to_string(),
            )])
            .await?;

        let create_value = Self::extract_response(&resp, 0)?;

        // The Conversation/create response wraps the result in a `list` array
        // (like a GetResponse) containing the created conversation.
        let create_resp: ConversationCreateResponse = serde_json::from_value(create_value.clone())
            .map_err(|e| {
                ChatError::NetworkError(format!(
                    "Failed to parse Conversation/create response: {e}"
                ))
            })?;

        let conv = create_resp.list.first().ok_or_else(|| {
            ChatError::NetworkError("Conversation/create returned empty list".into())
        })?;

        let room = ChatRoom {
            id: conv.id.clone(),
            name: conv.topic.clone(),
        };

        self.current_room_id = Some(room.id.clone());
        log::info!(
            "Chat JMAP: created conversation {} ({})",
            room.id,
            room.name
        );

        Ok(room)
    }

    async fn send_message(&self, room_id: &str, content: &str) -> Result<ChatMessage, ChatError> {
        let create_msg = serde_json::json!({
            "conversationId": room_id,
            "textBody": content,
            "messageType": "user",
        });

        let set_args = serde_json::json!({
            "senderId": self.user_id,
            "create": {
                "msg-1": create_msg,
            },
        });

        let resp = self
            .jmap_call(vec![(
                "ChatMessage/set".to_string(),
                set_args,
                "m1".to_string(),
            )])
            .await?;

        let set_value = Self::extract_response(&resp, 0)?;
        let set_resp: SetMessageResponse =
            serde_json::from_value(set_value.clone()).map_err(|e| {
                ChatError::NetworkError(format!("Failed to parse ChatMessage/set response: {e}"))
            })?;

        let msg = set_resp.created.get("msg-1").ok_or_else(|| {
            ChatError::NetworkError("ChatMessage/set did not return created message".into())
        })?;

        Ok(Self::to_chat_message(msg))
    }

    async fn get_messages(
        &self,
        room_id: &str,
        since: Option<f64>,
    ) -> Result<Vec<ChatMessage>, ChatError> {
        // Build query args. We query messages for the conversation and then
        // fetch the full objects. Both calls are batched into a single request.
        let mut query_args = serde_json::json!({
            "accountId": self.user_id,
            "conversationId": room_id,
            "limit": 50,
        });

        // If `since` is provided, we add a position filter. Since Smatter's
        // ChatMessage/query doesn't have a native `after` timestamp filter,
        // we fetch all and filter client-side for now. A future optimisation
        // could use ChatMessage/changes with a state token.
        if since.is_none() {
            // Fetch from the beginning for the initial load.
            query_args["position"] = serde_json::json!(0);
        }

        let resp = self
            .jmap_call(vec![
                (
                    "ChatMessage/query".to_string(),
                    query_args,
                    "q1".to_string(),
                ),
                (
                    "ChatMessage/get".to_string(),
                    serde_json::json!({
                        "accountId": self.user_id,
                        "#ids": {
                            "resultOf": "q1",
                            "path": "/ids"
                        },
                    }),
                    "g1".to_string(),
                ),
            ])
            .await?;

        // First check if query returned any IDs.
        let query_value = Self::extract_response(&resp, 0)?;
        let query_resp: QueryMessagesResponse = serde_json::from_value(query_value.clone())
            .map_err(|e| {
                ChatError::NetworkError(format!("Failed to parse ChatMessage/query response: {e}"))
            })?;

        if query_resp.ids.is_empty() {
            return Ok(Vec::new());
        }

        // Parse the full message objects.
        let get_value = Self::extract_response(&resp, 1)?;
        let get_resp: GetMessagesResponse =
            serde_json::from_value(get_value.clone()).map_err(|e| {
                ChatError::NetworkError(format!("Failed to parse ChatMessage/get response: {e}"))
            })?;

        let mut messages: Vec<ChatMessage> =
            get_resp.list.iter().map(Self::to_chat_message).collect();

        // If `since` was provided, filter out messages older than the threshold.
        if let Some(ts) = since {
            messages.retain(|m| m.timestamp > ts);
        }

        Ok(messages)
    }

    async fn disconnect(&mut self) -> Result<(), ChatError> {
        log::info!(
            "Chat JMAP: disconnecting (conversation: {:?})",
            self.current_room_id
        );
        self.auth_token = None;
        self.current_room_id = None;
        self.user_id = String::new();
        self.display_name = String::new();
        Ok(())
    }
}
