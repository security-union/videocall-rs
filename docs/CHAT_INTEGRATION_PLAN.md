# Chat Integration Plan

## Overview

This document outlines the design and implementation plan for adding external chat service integration to the videocall-rs application. The chat feature is **not built into videocall-rs** — it integrates with an external, service-agnostic chat provider (e.g., HCL Smatter, Discord) through a fully configuration-driven adapter layer.

When the chat feature is not enabled, no chat UI surfaces in the application.

---

## Design Principles

1. **Service-agnostic**: No hardcoded chat service names or service-specific logic. The entire integration is driven by configuration.
2. **Feature-flagged**: Chat is enabled/disabled via a single configuration property. When disabled, the UI contains zero chat-related elements.
3. **Identity passthrough**: The videocall app passes user identity to the chat service using whatever identity mechanism the chat service supports, configured at deploy time.
4. **Meeting-scoped**: Each video call meeting gets its own isolated chat room/channel, deterministically derived from the meeting ID.
5. **External ownership**: The chat backend is not owned by videocall-rs. Room lifecycle, message storage, and user management belong to the chat service.

---

## Current Architecture Context

- **Configuration system**: Runtime configuration lives in `window.__APP_CONFIG` (a frozen JS object loaded from `/config.js`). The Rust side parses this in `dioxus-ui/src/constants.rs` via a `RuntimeConfig` struct deserialized with `serde_wasm_bindgen`.
- **Feature flags**: Features like OAuth, E2EE, and WebTransport are string-valued booleans in `__APP_CONFIG` (e.g., `oauthEnabled: "true"`), evaluated via a `truthy()` helper.
- **UI structure**: The meeting page renders an `AttendantsComponent` with a bottom toolbar containing: MicButton, CameraButton, ScreenShareButton, PeerListButton, DiagnosticsButton, DeviceSettingsButton, HangUpButton. Side panels use a slide-in pattern with a `visible` CSS class.
- **Identity**: Users are identified by `user_id` (from OAuth or locally generated) and `display_name`. The JWT `RoomAccessTokenClaims` contains `sub` (user_id), `display_name`, `room` (meeting_id), and `is_host`.
- **Meeting lifecycle**: Meeting join goes through `meeting_api::join_meeting()` which returns a `ParticipantStatusResponse` with status, tokens, and meeting metadata.

---

## Configuration Schema

### Frontend Configuration (`window.__APP_CONFIG`)

All chat configuration properties are added to the existing `__APP_CONFIG` object. All are optional; chat is disabled when `chatEnabled` is absent or `"false"`.

| Property | Type | Default | Description |
|---|---|---|---|
| `chatEnabled` | `string` | `"false"` | Feature flag. Must be `"true"` to enable chat UI. |
| `chatApiBaseUrl` | `string` | `""` | Chat service API root URL (e.g., `"https://chat.example.com/api/v1"`) |
| `chatAuthMode` | `string` | `""` | Authentication mode: `"bearer"`, `"cookie"`, `"header"`, or `"query"` |
| `chatAuthTokenEndpoint` | `string` | `""` | Optional meeting-api endpoint for exchanging videocall identity for a chat service token |
| `chatAuthHeaderName` | `string` | `""` | For `"header"` auth mode: custom header name (e.g., `"X-Chat-User-Id"`) |
| `chatAuthQueryParam` | `string` | `""` | For `"query"` auth mode: query parameter name |
| `chatCreateRoomEndpoint` | `string` | `""` | POST endpoint to create/join a room (e.g., `"/rooms"`) |
| `chatMessagesEndpoint` | `string` | `""` | GET/POST endpoint for messages, supports `{roomId}` template (e.g., `"/rooms/{roomId}/messages"`) |
| `chatWebSocketUrl` | `string` | `""` | WebSocket URL for real-time message streaming (e.g., `"wss://chat.example.com/ws"`) |
| `chatRoomPrefix` | `string` | `""` | Prefix for meeting-scoped room IDs (e.g., `"videocall-"`) |
| `chatExtraHeaders` | `string` | `""` | JSON-encoded key-value pairs for service-specific HTTP headers |
| `chatExtraParams` | `string` | `""` | JSON-encoded key-value pairs for service-specific query parameters |
| `chatPollIntervalMs` | `number` | `3000` | Fallback polling interval (ms) when WebSocket is not configured |
| `chatProtocol` | `string` | `""` | Protocol adapter: `"rest"` (default) or `"jmap"`. JMAP is required for Smatter-compatible chat servers. |

### Backend Configuration (meeting-api environment variables)

These are only needed when the meeting-api acts as a token exchange proxy (i.e., when `chatAuthMode` is `"bearer"` and `chatAuthTokenEndpoint` is set).

| Variable | Description |
|---|---|
| `CHAT_SERVICE_URL` | Chat service API URL for server-to-server calls |
| `CHAT_SERVICE_API_KEY` | Server-side API key for authenticating with the chat service |

---

## Identity Mapping

The videocall app passes user identity to the chat service based on the configured `chatAuthMode`:

| Auth Mode | Mechanism | Use Case |
|---|---|---|
| `bearer` | Frontend calls meeting-api token exchange endpoint. Meeting-api validates the user session, calls the chat service's token API with server-side credentials, and returns a chat-specific bearer token. Frontend uses `Authorization: Bearer <token>` on all chat requests. | Most common. Chat service has its own token system. |
| `cookie` | Same-origin `fetch` with `credentials: include` passes browser cookies automatically. | Same-origin or shared-cookie deployments. |
| `header` | User's `user_id` and `display_name` are sent via custom headers (named by `chatAuthHeaderName`). | Simple internal chat services that trust the caller. |
| `query` | User identity appended as a query parameter (named by `chatAuthQueryParam`). | WebSocket connections or services requiring identity in URLs. |

### Bearer Token Exchange Flow (most common)

```
┌──────────┐      POST /api/v1/chat/token       ┌─────────────┐     POST /auth/token      ┌──────────────┐
│  Browser  │  ───────────────────────────────>  │  meeting-api │  ─────────────────────>  │ Chat Service │
│ (dioxus)  │  { meeting_id }                    │              │  { api_key, user_id }    │  (Smatter /  │
│           │  <───────────────────────────────  │              │  <─────────────────────  │   Discord)   │
│           │  { token, room_id, expires_at }    │              │  { chat_token }          │              │
└──────────┘                                     └─────────────┘                           └──────────────┘
```

---

## Meeting-Scoped Chat Rooms

### Room ID Derivation

Room IDs are deterministic: `chatRoomPrefix + meeting_id`

For example, with `chatRoomPrefix: "videocall-"` and `meeting_id: "standup-2024"`, the room ID is `"videocall-standup-2024"`.

All participants in the same meeting derive the same room ID, ensuring chat isolation per meeting.

### Room Lifecycle

1. **Join**: When a user is admitted to a meeting, the chat adapter POSTs to `chatCreateRoomEndpoint` with the derived room ID. This call is idempotent (create if not exists, return existing if already created by another participant).
2. **During meeting**: Messages are sent/received via `chatMessagesEndpoint` (HTTP) and optionally via WebSocket for real-time delivery.
3. **Leave**: The adapter's `disconnect()` is called when the user hangs up or the meeting ends. The videocall app does **not** delete the room — room cleanup is the chat service's responsibility.

---

## Implementation Plan

### Phase 1 — Configuration Plumbing

**Goal**: Add all chat configuration fields to the existing configuration pipeline. No user-visible changes.

**Changes**:

| File | Change |
|---|---|
| `dioxus-ui/src/constants.rs` | Add chat fields to `RuntimeConfig` struct. Add `chat_enabled()` accessor function. |
| `dioxus-ui/scripts/config.js` | Add default chat properties (all disabled/empty). |
| `docker/start-dioxus.sh` | Add env-var-to-config.js mapping for `CHAT_*` variables. |
| `docker/.env-sample` | Add commented-out `CHAT_*` environment variables. |
| `docker/docker-compose.yaml` | Add `CHAT_*` environment variables to dioxus-ui service. |
| `helm/videocall-ui/values.yaml` | Add chat fields to `runtimeConfig` (all defaulting to empty/false). |

---

### Phase 2 — Chat Adapter Layer

**Goal**: Build the internal adapter module that abstracts external chat service communication. No UI yet.

**New module**: `dioxus-ui/src/chat/`

| File | Purpose |
|---|---|
| `chat/mod.rs` | Module root, public exports. |
| `chat/types.rs` | `ChatMessage`, `ChatRoom`, `ChatError` structs. |
| `chat/adapter.rs` | `ChatServiceAdapter` trait definition. |
| `chat/generic_adapter.rs` | Config-driven REST adapter implementing the trait. |
| `chat/jmap_adapter.rs` | JMAP protocol adapter for Smatter-compatible servers. |
| `chat/context.rs` | Dioxus context types for chat state management. |

**Trait definition**:

```rust
#[async_trait(?Send)]
pub trait ChatServiceAdapter {
    async fn authenticate(&mut self, user_id: &str, display_name: &str) -> Result<(), ChatError>;
    async fn join_room(&mut self, meeting_id: &str) -> Result<ChatRoom, ChatError>;
    async fn send_message(&self, room_id: &str, content: &str) -> Result<ChatMessage, ChatError>;
    async fn get_messages(&self, room_id: &str, since: Option<f64>) -> Result<Vec<ChatMessage>, ChatError>;
    async fn subscribe(&mut self, room_id: &str) -> Result<UnboundedReceiver<ChatMessage>, ChatError>;
    async fn disconnect(&mut self) -> Result<(), ChatError>;
}
```

The `GenericChatAdapter` implements this trait using:
- `reqwest` for HTTP calls (already a project dependency, works in WASM via `fetch`)
- `web_sys::WebSocket` for real-time streaming (when `chatWebSocketUrl` is configured)
- Polling via `gloo_timers::Interval` as fallback (when no WebSocket URL is configured)
- URL template substitution at runtime (e.g., `{roomId}` replaced with actual room ID)

**Token refresh**: The adapter catches 401 responses and re-calls `authenticate()` before retrying the request.

#### JMAP Protocol Adapter

The `JmapChatAdapter` implements the same `ChatServiceAdapter` trait for JMAP-based chat servers such as HCL Smatter. It is selected at runtime when `chatProtocol` is set to `"jmap"`. The adapter uses `ChatAdapterKind` enum dispatch in `chat_panel.rs` to choose between REST and JMAP without trait objects.

**Key differences from the REST adapter:**

| Operation | REST Adapter | JMAP Adapter |
|---|---|---|
| Create/join room | `POST chatCreateRoomEndpoint` | `Conversation/query` to find existing room by topic, then `Conversation/create` if not found |
| Add participant | Implicit (room join) | `Conversation/setMembers` to add the joining user |
| Send message | `POST chatMessagesEndpoint` | `ChatMessage/set` with `create` map |
| Get messages | `GET chatMessagesEndpoint` | `ChatMessage/query` + `ChatMessage/get` batched (uses JMAP result references) |
| Auth | Configurable (bearer/cookie/header/query) | Bearer token via `/auth/login` or token exchange endpoint |
| Endpoint | Multiple REST endpoints | Single `POST /jmap` with batched method calls |

All JMAP operations go through `POST {chatApiBaseUrl}/jmap` using the JMAP envelope format:
```json
{
  "using": ["urn:ietf:params:jmap:core"],
  "methodCalls": [["MethodName", { ...arguments }, "callId"]]
}
```

When `chatProtocol` is `"jmap"`, the `chatCreateRoomEndpoint` and `chatMessagesEndpoint` config fields are not required (they are REST-specific).

**Smatter configuration example (Docker `.env`):**
```bash
CHAT_ENABLED=true
CHAT_PROTOCOL=jmap
CHAT_API_BASE_URL=https://smatterchat.fnxlabs.com:8443
CHAT_AUTH_MODE=bearer
CHAT_ROOM_PREFIX=videocall-
```

---

### Phase 3 — Backend Token Exchange (Optional)

**Goal**: Add a token exchange endpoint to meeting-api for the `"bearer"` auth mode.

**New endpoint**: `POST /api/v1/chat/token`

| Aspect | Detail |
|---|---|
| Request body | `{ "meeting_id": "standup-2024" }` |
| Auth | Requires valid session cookie (same as other meeting-api endpoints) |
| Behavior | Validates session, calls chat service's token API with server-side credentials, returns chat-specific token |
| Response | `{ "success": true, "result": { "token": "...", "room_id": "videocall-standup-2024", "expires_at": 1234567890 } }` |

**Changes**:

| File | Change |
|---|---|
| `meeting-api/src/config.rs` | Add optional `chat_service_url` and `chat_service_api_key` fields. |
| `meeting-api/src/routes/chat.rs` | New file: token exchange handler. |
| `meeting-api/src/routes/mod.rs` | Register `chat` module. |
| `meeting-api/src/lib.rs` | Mount `/api/v1/chat/token` route. |
| `videocall-meeting-types/src/requests.rs` | Add `ChatTokenRequest` struct. |
| `videocall-meeting-types/src/responses.rs` | Add `ChatTokenResponse` struct. |
| `videocall-meeting-client/src/lib.rs` | Add `get_chat_token()` client method. |

This phase is deferrable — it is only needed when the chat service requires server-side token exchange.

---

### Phase 4 — UI Components

**Goal**: Add the chat button to the toolbar and the chat panel sidebar.

#### ChatButton Component

Added to `dioxus-ui/src/components/video_control_buttons.rs`, following the existing button pattern:

- Positioned in the toolbar **after ScreenShareButton** and **before PeerListButton**
- Conditionally rendered: only appears when `chat_enabled()` returns `true`
- Displays an unread message badge when the panel is closed and new messages arrive
- Toggles the chat panel open/closed on click

#### ChatPanel Component

New file: `dioxus-ui/src/components/chat_panel.rs`

Structure:
- **Header**: Title ("Chat") and close button, matching `.sidebar-header` style
- **Message list**: Scrollable area displaying messages with sender name, timestamp, and content
- **Input area**: Text input with send button at the bottom

Follows the same slide-in sidebar pattern as the peer list container (`#peer-list-container`) with a `visible` CSS class toggle.

#### Integration in AttendantsComponent

Changes to `dioxus-ui/src/components/attendants.rs`:

- New signals: `chat_open: Signal<bool>`, `unread_count: Signal<u32>`, `chat_room_id: Signal<Option<String>>`
- Chat adapter initialization on meeting join (after admission)
- Chat adapter disconnect on hangup and meeting-ended callbacks
- Mutual exclusivity: opening the chat panel closes the diagnostics panel (and vice versa), matching the existing UX pattern

#### CSS

Changes to `dioxus-ui/static/style.css`:
- `#chat-panel-container` styles (cloned from `#peer-list-container`)
- `.chat-message`, `.chat-input`, `.chat-badge` styles

---

### Phase 5 — E2E Tests

**Mock chat service**: A lightweight mock HTTP server added to the E2E docker stack that implements the minimum chat endpoints.

**New test file**: `e2e/tests/chat.spec.ts`

| Test | Scenario |
|---|---|
| Chat button hidden when disabled | Navigate to meeting with `chatEnabled: "false"`. Verify no chat button in toolbar. |
| Chat button visible when enabled | Set `chatEnabled: "true"` with mock chat service. Verify chat button appears next to screen share. |
| Chat panel opens/closes | Click chat button, verify panel slides in. Click again, verify it slides out. |
| Panel mutual exclusivity | Open peer list, click chat button. Verify peer list closes and chat panel opens. |
| Message send and display | Send a message via chat input. Verify it appears in the message list. |
| Unread badge | Receive a message while chat panel is closed. Verify badge counter increments. |

**Changes**:

| File | Change |
|---|---|
| `e2e/tests/chat.spec.ts` | New file: chat feature E2E tests. |
| `e2e/helpers/mock-chat-server/` | New directory: minimal mock chat server. |
| `docker/docker-compose.e2e.yaml` | Add mock-chat service, set `CHAT_ENABLED=true` on dioxus-ui. |

---

### Phase 6 — Deployment Configuration

| File | Change |
|---|---|
| `docker/.env-sample` | Add commented-out `CHAT_*` env vars with documentation. |
| `docker/docker-compose.yaml` | Add `CHAT_*` to dioxus-ui and meeting-api services. |
| `docker/docker-compose.e2e.yaml` | Add `CHAT_*` vars and mock-chat service. |
| `helm/videocall-ui/values.yaml` | Add chat fields to `runtimeConfig`. |
| `helm/meeting-api/values.yaml` | Add optional `CHAT_SERVICE_URL` and `CHAT_SERVICE_API_KEY`. |

No changes needed to `Dockerfile.dioxus` — `config.js` is mounted at runtime, not baked into the image.

---

## Implementation Sequence

```
Phase 1 (Config)  ──────────────────────────────────────>
Phase 2 (Adapter) ──────────────────────────────────────>   (parallel with Phase 1)
Phase 3 (Backend) ────────────────────>                      (can start after Phase 2 types are defined)
Phase 4 (UI)                           ─────────────────>   (after Phase 1 + 2)
Phase 5 (E2E)                                           ──> (after Phase 4)
Phase 6 (Deploy)  ──────────────────────────────────────>   (parallel, incremental)
```

Phases 1, 2, and 6 can proceed in parallel. Phase 4 depends on Phases 1 and 2. Phase 5 depends on Phase 4.

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| **CORS blocking** | Browser blocks direct requests to external chat service from videocall domain | Use meeting-api as a proxy via `chatAuthTokenEndpoint`, or require chat service operator to configure CORS. Document this. |
| **WASM networking constraints** | No raw TCP/UDP in browser WASM | Use `reqwest` (fetch-based, already a dependency) and `web_sys::WebSocket`. Both work in WASM. |
| **No WebSocket on chat service** | Real-time delivery not available | Polling fallback via `chatPollIntervalMs`. The adapter's `subscribe()` uses `gloo_timers::Interval` when no WebSocket URL is configured. |
| **Chat token expiry** | Requests fail with 401 mid-meeting | Adapter catches 401, re-calls `authenticate()`, retries the request. Follows the same pattern as `schedule_reconnect` in `attendants.rs`. |
| **WASM bundle size increase** | Larger download for users | Chat module uses existing dependencies (`reqwest`, `web_sys`, `serde`). No new heavy crates needed. |
| **Chat service downtime** | Chat unavailable during a meeting | Chat errors are non-fatal. The meeting continues normally; the chat panel shows an error state with retry option. |
| **JMAP protocol differences** | JMAP uses a single endpoint with batched method calls, unlike REST | `JmapChatAdapter` handles JMAP envelope format and result references. Selected via `chatProtocol: "jmap"`. |
| **JMAP self-signed certs** | Smatter dev instances may use self-signed TLS certificates | Browser `fetch` (used by `reqwest` in WASM) will reject self-signed certs. Requires proper CA or browser trust override. |

---

## Out of Scope

- **Chat service backend**: videocall-rs does not build or host a chat server.
- **Room cleanup**: Deleting old/stale chat rooms is the chat service's responsibility.
- **File sharing / media in chat**: Initial implementation is text-only messages.
- **Chat history persistence across meetings**: Each meeting creates a fresh room. Historical messages are the chat service's domain.
- **Rate limiting**: Handled by the external chat service, not by videocall-rs.
