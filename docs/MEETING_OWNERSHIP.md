# Meeting Ownership and Architecture

This document describes the system architecture, meeting ownership model, token-based access control, and user workflows in videocall.rs.

## Table of Contents

- [Shared Types Crate](#shared-types-crate)
- [System Architecture](#system-architecture)
- [Room Access Token](#room-access-token)
- [Meeting Ownership](#meeting-ownership)
- [Meeting Lifecycle](#meeting-lifecycle)
- [User Interface Workflows](#user-interface-workflows)
- [My Meetings List](#my-meetings-list)
- [Host Identification](#host-identification)
- [Waiting Room](#waiting-room)
- [Database Schema](#database-schema)

---

## Shared Types Crate

The `videocall-meeting-types` crate defines all API types shared between the Meeting Backend and its consumers. It is framework-agnostic -- no actix-web, no database dependencies.

**Key types**:

- `APIResponse<A>` -- Generic envelope: `{ "success": bool, "result": A }`. Every endpoint uses this.
- `APIError` -- Error payload with `code`, `message`, and optional `engineering_error` for debugging.
- `RoomAccessTokenClaims` -- JWT claims struct for room access tokens (used by both the Meeting Backend to sign and the Media Server to validate).
- Request types (`CreateMeetingRequest`, `JoinMeetingRequest`, `AdmitRequest`, `ListMeetingsQuery`).
- Response types (`CreateMeetingResponse`, `MeetingInfoResponse`, `ParticipantStatusResponse`, `WaitingRoomResponse`, `AdmitAllResponse`, `DeleteMeetingResponse`, `ListMeetingsResponse`, `MeetingSummary`).

The Meeting Backend depends on this crate for serialization. The Media Server depends on it only for `RoomAccessTokenClaims` (to validate JWT tokens). Clients and integration tests depend on it for both request and response types.

---

## System Architecture

videocall.rs is composed of two independent services that communicate through a shared JWT secret:

```
┌──────────────────────────────────┐   ┌──────────────────────────────────┐
│       Meeting Backend            │   │         Media Server             │
│       (Port 8081)                │   │         (Port 8080)              │
│   (Standalone deployment)        │   │   (Includes meeting API routes)  │
│                                  │   │                                  │
│  ┌────────────┐  ┌────────────┐  │   │  ┌────────────┐  ┌───────────┐  │
│  │ OAuth      │  │ REST API   │  │   │  │ WebSocket  │  │ WebTrans- │  │
│  │ Login      │  │ /api/v1/   │  │   │  │ Endpoint   │  │ port      │  │
│  └────────────┘  └─────┬──────┘  │   │  └─────┬──────┘  └─────┬─────┘  │
│                        │         │   │        │                │        │
│  ┌─────────────────────┴──────┐  │   │  ┌─────┴────────────────┴─────┐  │
│  │ Meeting Management         │  │   │  │ JWT Validator               │  │
│  │ - CRUD, waiting room       │  │   │  │ - Verify signature          │  │
│  │ - Admission decisions      │  │   │  │ - Extract room + identity   │  │
│  │ - Participant state        │  │   │  │ - Reject invalid tokens     │  │
│  └─────────────┬──────────────┘  │   │  └────────────────────────────┘  │
│                │                 │   │                                  │
│  ┌─────────────┴──────────────┐  │   │  ┌────────────────────────────┐  │
│  │ JWT Token Generator        │  │   │  │ NATS Pub/Sub               │  │
│  │ - Signs room access tokens │  │   │  │ - Media relay              │  │
│  └────────────────────────────┘  │   │  └────────────────────────────┘  │
│                                  │   │                                  │
│  ┌────────────────────────────┐  │   │                                  │
│  │ PostgreSQL                 │  │   │                                  │
│  │ - meetings                 │  │   │                                  │
│  │ - meeting_participants     │  │   │                                  │
│  └────────────────────────────┘  │   │                                  │
└──────────────────────────────────┘   └──────────────────────────────────┘
         │                                         ▲
         │          Shared JWT Secret               │
         └─────────────────────────────────────────┘
```

### Service Responsibilities

**Meeting Backend** (separate binary, its own process and port):
- Handles OAuth login and user authentication (signed session JWT in HttpOnly cookie or Bearer header)
- Manages all meeting CRUD operations (create, list, get, delete)
- Manages the waiting room, admission, and participant state
- Issues signed **room access tokens** (JWTs) when a participant is admitted
- Owns the `meetings` and `meeting_participants` database tables
- Is the **single source of truth** for meeting state and participant status

**Media Server** (existing WebSocket/WebTransport server):
- Handles real-time audio/video/data transport
- When `FEATURE_MEETING_MANAGEMENT=true`, validates room access tokens on every connection attempt
- Extracts identity, room, and permissions from the JWT claims
- **Rejects connections without a valid, signed token** (when meeting management is enabled)
- When `FEATURE_MEETING_MANAGEMENT=false` (default), allows connections without a token for backward compatibility
- Does not create, manage, or track meetings -- it is stateless with respect to meeting lifecycle
- Relays media via NATS pub/sub

### Feature Flag

JWT validation on the Media Server is gated behind the `FEATURE_MEETING_MANAGEMENT` environment variable:

| Value | Behavior |
|-------|----------|
| `true` | JWT validation is **enforced**. Connections without a valid token are rejected. |
| `false` (default) | JWT validation is **disabled**. Connections are accepted without a token (backward compatible). |

This allows the meeting management system to be deployed incrementally. Once the UI is updated to obtain and present tokens, the feature flag can be flipped to `true` to enforce token-based access.

### Local Development Setup

For local development, `docker-compose.yaml` spins up two separate services:

- **meeting-api (port 8081)**: Handles OAuth login, session JWTs, and all Meeting REST API routes (`/api/v1/meetings/*`)
- **websocket-api (port 8080)**: Handles WebSocket media connections (`/lobby`)
- **webtransport-api (port 4433)**: Handles WebTransport media connections (`/lobby`)

The UI's `apiBaseUrl` defaults to `http://localhost:8081` (the meeting-api). The media server URLs (`wsUrl`, `webTransportHost`) point to the websocket-api and webtransport-api respectively.

> **Important**: The `COOKIE_DOMAIN` environment variable should be set to `localhost` to ensure session cookies are sent correctly across ports during local development.

### Why Two Services?

Separating the Meeting Backend from the Media Server provides:

- **Enforced access control**: When meeting management is enabled, a client cannot connect to a media session without first going through the Meeting Backend's admission flow. There is no way to bypass the waiting room.
- **Single source of truth**: All meeting state lives in the Meeting Backend's database. The Media Server does not maintain its own parallel participant tracking.
- **Independent scaling**: The Meeting Backend (REST API + database) and Media Server (real-time transport) have different scaling characteristics and can be scaled independently.
- **Clean separation of concerns**: Business logic (meetings, ownership, admission) is fully separated from transport logic (media relay, codecs, NATS).

---

## Session Authentication

The Meeting Backend authenticates API requests using a **signed session JWT** (HMAC-SHA256). This replaces the legacy plaintext email cookie with a cryptographically verified token.

### Two Token Types

The system uses two separate JWTs with different purposes and delivery mechanisms:

| Token | Purpose | Delivery | Lifetime | HttpOnly |
|-------|---------|----------|----------|----------|
| **Session JWT** | Authenticates user to the Meeting Backend | `Set-Cookie` (HttpOnly, Secure, SameSite=Lax) or `Authorization: Bearer` | Configurable (default: long-lived) | Yes (cookie) |
| **Room Access JWT** | Authorizes room join on the Media Server | JSON response body | Configurable TTL (short) | N/A |

### Session JWT Claims

| Claim | Description |
|-------|-------------|
| `sub` | User email (identity principal) |
| `name` | Display name |
| `exp` | Expiration (Unix timestamp) |
| `iat` | Issued-at (Unix timestamp) |
| `iss` | `"videocall-meeting-backend"` |

### Session Token Flow

1. User completes OAuth login with Google
2. Meeting Backend issues a signed session JWT and sets it as an `HttpOnly; Secure; SameSite=Lax` cookie
3. The browser sends the cookie automatically with every request to the Meeting Backend
4. JavaScript cannot read the cookie (XSS protection)
5. Non-browser clients can use `Authorization: Bearer <session_jwt>` instead

### Cookie Properties

- `HttpOnly` -- JavaScript cannot read the cookie, preventing XSS token theft
- `Secure` -- Cookie is only sent over HTTPS (configurable via `COOKIE_SECURE=false` for local dev)
- `SameSite=Lax` -- Cookie is sent on top-level navigations (so meeting links from Slack/email work) but not on cross-site sub-requests

### CORS and Deployment Topology

The Meeting Backend enforces CORS on all responses. The behavior depends on the `CORS_ALLOWED_ORIGIN` environment variable:

| Environment | `CORS_ALLOWED_ORIGIN` | Behavior |
|---|---|---|
| **Production** | `https://app.videocall.rs` | Only the specified origin can make credentialed requests |
| **Development** | unset / empty | Mirrors the request `Origin` header (any origin accepted) |

**Production deployment recommendations:**

- **Same registrable domain** (e.g. `app.videocall.rs` + `api.videocall.rs`): Set `COOKIE_DOMAIN=.videocall.rs` so the session cookie is sent to both subdomains. `SameSite=Lax` works because both subdomains share the same eTLD+1.
- **Reverse proxy** (e.g. `videocall.rs/` for frontend, `videocall.rs/api/` proxied to meeting-api): No CORS needed at all -- same origin. `SameSite=Lax` just works.
- **Different domains** (e.g. `videocall-app.com` + `videocall-api.com`): **Not recommended.** `SameSite=Lax` cookies will not be sent on cross-site `fetch()` requests. Would require `SameSite=None; Secure` which opens CSRF surface.

### Meeting Backend Environment Variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `DATABASE_URL` | Yes | -- | PostgreSQL connection string |
| `JWT_SECRET` | Yes | -- | Shared HMAC-SHA256 secret (must match Media Server) |
| `LISTEN_ADDR` | No | `0.0.0.0:8081` | HTTP bind address |
| `TOKEN_TTL_SECS` | No | `600` | Room access token lifetime (seconds) |
| `SESSION_TTL_SECS` | No | `315360000` (~10y) | Session JWT lifetime (seconds) |
| `COOKIE_DOMAIN` | No | -- | Cookie `Domain` attribute (e.g. `.videocall.rs`) |
| `COOKIE_SECURE` | No | `true` | Set `false` for local HTTP development |
| `CORS_ALLOWED_ORIGIN` | No | -- | Production: exact frontend origin. Unset for dev. |
| `OAUTH_CLIENT_ID` | No | -- | Google OAuth client ID (disables OAuth if unset) |
| `OAUTH_SECRET` | Cond. | -- | Google OAuth client secret (required if `OAUTH_CLIENT_ID` set) |
| `OAUTH_REDIRECT_URL` | Cond. | -- | OAuth callback URL (required if `OAUTH_CLIENT_ID` set) |
| `OAUTH_AUTH_URL` | No | Google default | OAuth authorization endpoint |
| `OAUTH_TOKEN_URL` | No | Google default | OAuth token endpoint |
| `AFTER_LOGIN_URL` | No | `/` | Redirect target after successful OAuth login |

---

## Room Access Token

The room access token is a signed JWT that bridges the Meeting Backend and the Media Server. When meeting management is enabled, it is the **only way** to connect to a media session.

### Token Flow

```
 Client                  Meeting Backend              Media Server
   │                          │                            │
   │  1. POST /join           │                            │
   │  (session JWT auth)      │                            │
   │ ────────────────────────>│                            │
   │                          │                            │
   │  2. APIResponse:         │                            │
   │  {success: true,         │                            │
   │   result.status:         │                            │
   │     "waiting"}           │                            │
   │ <────────────────────────│                            │
   │                          │                            │
   │  3. GET /status (poll)   │                            │
   │ ────────────────────────>│                            │
   │                          │                            │
   │  4. APIResponse:         │                            │
   │  {success: true,         │                            │
   │   result.status:         │                            │
   │     "admitted",          │                            │
   │   result.room_token:     │                            │
   │     "ey.."}              │                            │
   │ <────────────────────────│                            │
   │                          │                            │
   │  5. Connect with token   │                            │
   │ ─────────────────────────────────────────────────────>│
   │                          │                            │
   │                          │       6. Validate JWT      │
   │                          │       (RoomAccessToken-    │
   │                          │        Claims)             │
   │                          │                            │
   │  7. Connection accepted  │                            │
   │ <─────────────────────────────────────────────────────│
```

### Token Structure

The room access token is a standard JWT signed with a shared secret (HMAC-SHA256). Its payload contains:

> **Rust type**: `RoomAccessTokenClaims` (defined in `videocall-meeting-types::token`)

```json
{
  "sub": "user@example.com",
  "room": "standup-2024",
  "room_join": true,
  "is_host": true,
  "display_name": "Alice",
  "exp": 1707004800,
  "iss": "videocall-meeting-backend"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `sub` | string | Participant's email (unique identity) |
| `room` | string | The room/meeting ID the participant is authorized to join |
| `room_join` | boolean | Must be `true` for the Media Server to accept the connection |
| `is_host` | boolean | Whether this participant is the meeting host |
| `display_name` | string | Participant's chosen display name for this meeting |
| `exp` | integer | Expiration timestamp (Unix seconds). Token is rejected after this time. |
| `iss` | string | Issuer identifier (`videocall-meeting-backend`). Constant: `RoomAccessTokenClaims::ISSUER` |

### Token Lifecycle

- **Issued**: When a participant's status becomes `admitted` (hosts are auto-admitted on join)
- **Delivered**: Included in the response to `POST /join` (for hosts) or `GET /status` (for admitted attendees)
- **Used**: Client presents the token when connecting to the Media Server
- **Expires**: After a configurable TTL (e.g., 10 minutes). Expiration applies only to the initial connection; active sessions are not disconnected when the token expires.
- **Not reusable across meetings**: Each token is scoped to a specific room

### Connection Endpoint

The Media Server has two connection endpoints:

**Primary (token-based)**:
```
GET /lobby?token=<JWT>
```

- **WebSocket**: `ws://host:8080/lobby?token=<JWT>`
- **WebTransport**: `https://host:4433/lobby?token=<JWT>`

The identity (email) and room are extracted from the JWT claims (`sub` and `room`). There are no email or room parameters in the URL. The token is the sole source of truth.

The Media Server validates:
1. JWT signature (HMAC-SHA256 with shared `JWT_SECRET`)
2. Expiration (`exp` claim)
3. `room_join == true`
4. Issuer matches `videocall-meeting-backend`

Invalid, expired, or unauthorized tokens are rejected with HTTP 401.

**Deprecated (path-based, unauthenticated)**:
```
GET /lobby/{email}/{room}
```

> **Deprecated**: This endpoint exists only for backward compatibility when `FEATURE_MEETING_MANAGEMENT=false`. When `FEATURE_MEETING_MANAGEMENT=true`, it returns **HTTP 410 Gone**. Clients should migrate to the token-based endpoint.

- **WebSocket**: `ws://host:8080/lobby/{email}/{room}`
- **WebTransport**: `https://host:4433/lobby/{email}/{room}`

No authentication is performed. The email and room are taken directly from the URL path.

---

## Meeting Ownership

### How Ownership is Assigned

1. **Explicit Creation**: When a user calls `POST /api/v1/meetings`, they become the owner
2. **Implicit Creation**: When a user joins a meeting that doesn't exist (`POST /api/v1/meetings/{id}/join`), the meeting is created and they become the owner

### Owner Identification

- Ownership is stored in the `creator_id` field of the `meetings` table
- The `creator_id` contains the owner's email address
- This is set at meeting creation time and never changes

### Owner vs Host Display Name

There is an important distinction between:

- **Owner (creator_id)**: The email address of the user who owns the meeting (permanent, used for authorization)
- **Host Display Name**: The display name shown in the UI for the host (dynamic, looked up from participants)

The host display name is resolved by looking up the owner's email in the `meeting_participants` table to find their chosen display name for that meeting.

---

## Meeting Lifecycle

### Meeting States

| State | Description |
|-------|-------------|
| `idle` | Meeting created but owner hasn't joined yet |
| `active` | Owner has joined and been issued a room token; meeting is in progress |
| `ended` | Meeting has ended (all participants left or host left) |

### State Transitions

```
  [Create Meeting]
        │
        ▼
     ┌──────┐
     │ idle │
     └──┬───┘
        │
        │ [Owner joins via REST API]
        │ [Room access token issued]
        ▼
    ┌────────┐
    │ active │ ◄───────────────────┐
    └───┬────┘                     │
        │                         │
        │ [All participants      │ [Owner rejoins,
        │  leave / host leaves]  │  new token issued]
        ▼                         │
    ┌───────┐                     │
    │ ended │ ────────────────────┘
    └───┬───┘
        │
        │ [Owner deletes]
        ▼
    ┌─────────┐
    │ deleted │ (soft delete: deleted_at set)
    └─────────┘
```

The key transition is from `idle` to `active`: this is when the host's room access token is issued. The meeting only becomes joinable by attendees after the host has activated it. Attendees who are admitted also receive their own room access tokens, which is what allows them to connect to the Media Server.

### Soft Delete vs Hard Delete

Meetings use **soft deletion**:
- When deleted, `deleted_at` timestamp is set (not physically removed)
- Soft-deleted meetings don't appear in "My Meetings"
- The same meeting ID can be reused after deletion (partial unique index)

---

## User Interface Workflows

### Owner Starting a Meeting

1. Owner navigates to a meeting URL (e.g., `/meeting/my-standup`)
2. Owner enters their display name
3. Owner clicks **"Start Meeting"**
4. Meeting is created (if new) or activated (if existing idle/ended meeting)
5. Meeting Backend returns a **room access token**
6. Client connects to the Media Server using the token
7. Owner enters the meeting room

### Participant Joining a Meeting

1. Participant navigates to meeting URL
2. Participant enters their display name
3. Participant clicks **"Join Meeting"**
4. If meeting is active:
   - Participant enters the waiting room (status: `waiting`)
   - UI polls `GET /status` until status changes
   - Host admits or rejects participant
   - If admitted, response includes a **room access token**
   - Client auto-connects to the Media Server using the token
5. If meeting doesn't exist:
   - Meeting is created with participant as owner
   - Participant becomes the host and receives a room access token immediately

### Owner Deleting a Meeting

1. Owner goes to the home page
2. Owner expands "My Meetings" section
3. Owner clicks the delete icon (trash) next to their meeting
4. Confirmation dialog appears
5. Meeting is soft-deleted and removed from the list

---

## My Meetings List

The "My Meetings" list on the home page shows all meetings owned by the current user.

### Features

- **Filtered by Owner**: Only shows meetings where `creator_id` matches the user's email
- **Includes Ended Meetings**: Ended meetings remain visible until deleted
- **Excludes Deleted Meetings**: Soft-deleted meetings are hidden
- **Shows Meeting Status**: Active, idle, or ended state displayed
- **Delete Button**: Owners can delete their meetings directly from the list

### Meeting Summary Information

Each meeting in the list shows:
- Meeting ID (clickable to join)
- Current state (active/idle/ended)
- Host email
- Participant count
- Password indicator (if password-protected)
- Delete button (for owner only)

### API Endpoint

```
GET /api/v1/meetings?limit=20&offset=0
```

Returns only meetings owned by the authenticated user.

---

## Host Identification

### In the Meeting Room

The host is identified in the UI with:
- **(Host)** text displayed after the host's display name
- Tooltip showing "Host: [name]" on hover

### How Host Display Name is Resolved

1. The room access token contains the `is_host` and `display_name` claims
2. The Media Server makes these available to connected clients
3. The UI uses `is_host` to show the "(Host)" indicator

### Visual Indicators

| Location | Host Indicator |
|----------|----------------|
| Video tile | "(Host)" after name |
| Peer list | "(Host)" after name |
| Hover tooltip | "Host: [display name]" |

---

## Waiting Room

### Overview

The waiting room provides controlled access to meetings:
- Non-owners enter the waiting room when joining an active meeting
- The host (or any admitted participant) can manage the waiting room
- Participants poll for status changes while waiting
- **No room access token is issued until a participant is admitted**, so there is no way to bypass the waiting room and connect to the Media Server directly

### Participant Management

Any admitted participant can:
- View the waiting room list
- Admit individual participants
- Admit all waiting participants at once
- Reject participants

### Admission and Token Issuance

When a participant is admitted from the waiting room:
1. Their status changes to `admitted` in the database
2. A room access token (`RoomAccessTokenClaims`) is generated and signed for them
3. The UI detects the status change via polling (`GET /status`)
4. The poll response is an `APIResponse<ParticipantStatusResponse>` with `room_token` populated
5. The client connects to the Media Server using the token
6. The participant enters the meeting automatically

### API Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /meetings/{id}/waiting` | List waiting participants |
| `POST /meetings/{id}/admit` | Admit one participant (token generated for them) |
| `POST /meetings/{id}/admit-all` | Admit all waiting (tokens generated for each) |
| `POST /meetings/{id}/reject` | Reject a participant |
| `GET /meetings/{id}/status` | Check your own status; includes `room_token` when admitted |

---

## Database Schema

All meeting and participant state is owned by the Meeting Backend. The Media Server does not read or write to these tables.

### meetings Table

| Column | Type | Description |
|--------|------|-------------|
| id | SERIAL | Primary key |
| room_id | VARCHAR(255) | Meeting identifier (unique among non-deleted) |
| creator_id | VARCHAR(255) | Owner's email address |
| state | VARCHAR(50) | `idle`, `active`, `ended` |
| password_hash | VARCHAR(255) | Argon2 hashed password (optional) |
| started_at | TIMESTAMPTZ | When meeting was created |
| ended_at | TIMESTAMPTZ | When meeting ended |
| deleted_at | TIMESTAMPTZ | Soft delete timestamp |
| host_display_name | VARCHAR(255) | Cached host display name |
| attendees | JSONB | Pre-registered attendee emails |

### meeting_participants Table

This is the **single source of truth** for participant state. The `session_participants` table from the legacy system is eliminated.

| Column | Type | Description |
|--------|------|-------------|
| id | SERIAL | Primary key |
| meeting_id | INTEGER | Foreign key to meetings.id |
| email | VARCHAR(255) | Participant's email |
| display_name | VARCHAR(255) | Participant's chosen display name |
| status | VARCHAR(50) | `waiting`, `admitted`, `rejected`, `left` |
| is_host | BOOLEAN | True if this is the meeting owner |
| joined_at | TIMESTAMPTZ | When joined/entered waiting room |
| admitted_at | TIMESTAMPTZ | When admitted to meeting |
| left_at | TIMESTAMPTZ | When left the meeting |

### Key Indexes

```sql
-- Unique meeting IDs among non-deleted meetings
CREATE UNIQUE INDEX idx_meetings_room_id_unique_active
ON meetings(room_id) WHERE deleted_at IS NULL;

-- Fast lookup by owner
CREATE INDEX idx_meetings_creator_id ON meetings(creator_id);

-- Fast lookup by state
CREATE INDEX idx_meetings_state ON meetings(state);
```

---

## Related Documentation

- [Meeting API Documentation](MEETING_API.md) - Detailed API endpoint reference with request/response examples
- `videocall-meeting-types` crate (`videocall-meeting-types/src/`) - Rust source of truth for all API types
