# Create Meeting API

This document describes the Create Meeting API architecture and usage.

## Overview

The Create Meeting API allows authenticated users to create meetings with metadata stored at request time (not at meeting start time). This enables pre-scheduling meetings with specific attendees and optional password protection.

## Feature Flag

The API requires the `FEATURE_MEETING_MANAGEMENT` environment variable to be enabled. When disabled, the endpoint returns `503 Service Unavailable`.

```bash
export FEATURE_MEETING_MANAGEMENT=true
```

## Endpoint

```
POST /api/meetings
```

## Authentication

The API uses cookie-based authentication. The host's identity is extracted from the `email` cookie. Requests without this cookie receive a `401 Unauthorized` response.

## Request

### Headers

```
Content-Type: application/json
Cookie: email=<user-id>
```

### Body

```json
{
  "meetingId": "optional-custom-id",
  "attendees": ["user1", "user2"],
  "password": "optional-password"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `meetingId` | string | No | Custom meeting ID. If omitted, a UUID is generated. Must match pattern `^[a-zA-Z0-9_-]*$` |
| `attendees` | string[] | No | List of pre-registered attendee user IDs. Maximum 100 attendees. |
| `password` | string | No | Meeting password. Stored as bcrypt hash. |

## Response

### Success (201 Created)

```json
{
  "meetingId": "abc123",
  "metadata": {
    "host": "host-user-id",
    "createdTimestamp": 1706540400,
    "state": "idle",
    "attendees": ["user1", "user2"],
    "hasPassword": true
  }
}
```

| Field | Description |
|-------|-------------|
| `meetingId` | The meeting identifier (provided or system-generated) |
| `metadata.host` | User ID of the meeting creator |
| `metadata.createdTimestamp` | Unix timestamp (seconds) when meeting was created |
| `metadata.state` | Meeting state: `idle` (created but not started) or `active` |
| `metadata.attendees` | List of pre-registered attendee IDs |
| `metadata.hasPassword` | Whether the meeting requires a password |

### Error Responses

| Status | Code | Description |
|--------|------|-------------|
| 401 | `AUTH_REQUIRED` | No authentication cookie provided |
| 400 | `INVALID_MEETING_ID` | Meeting ID contains invalid characters |
| 400 | `INVALID_ATTENDEE_ID` | An attendee ID contains invalid characters |
| 400 | `TOO_MANY_ATTENDEES` | More than 100 attendees specified |
| 409 | `MEETING_EXISTS` | A meeting with this ID already exists |
| 500 | `INTERNAL_ERROR` | Database or server error |
| 503 | `FEATURE_DISABLED` | Meeting management feature is not enabled |

Error response format:

```json
{
  "error": "Human-readable error message",
  "code": "ERROR_CODE"
}
```

## Architecture

### Data Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                        POST /api/meetings                        │
└─────────────────────────────────────────────────────────────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │   Feature Flag Check   │
                    │  (503 if disabled)     │
                    └───────────────────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │   Authentication       │
                    │  (email cookie)        │
                    └───────────────────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │   Input Validation     │
                    │  - Meeting ID format   │
                    │  - Attendee IDs        │
                    │  - Attendee count      │
                    └───────────────────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │   Duplicate Check      │
                    │  (409 if exists)       │
                    └───────────────────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │   Password Hashing     │
                    │  (bcrypt)              │
                    └───────────────────────┘
                                │
                                ▼
         ┌──────────────────────┼──────────────────────┐
         │                      │                      │
         ▼                      ▼                      ▼
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
│    meetings     │  │ meeting_owners  │  │meeting_attendees│
│    (INSERT)     │  │    (INSERT)     │  │    (INSERT)     │
└─────────────────┘  └─────────────────┘  └─────────────────┘
```

### Database Schema

#### meetings

| Column | Type | Description |
|--------|------|-------------|
| id | SERIAL | Primary key |
| room_id | VARCHAR | Unique meeting identifier |
| started_at | TIMESTAMP | When meeting was created/started |
| ended_at | TIMESTAMP | When meeting ended (NULL if active) |
| created_at | TIMESTAMP | Record creation time |
| updated_at | TIMESTAMP | Record update time |
| deleted_at | TIMESTAMP | Soft delete timestamp |
| creator_id | VARCHAR | Host user ID |
| meeting_title | VARCHAR | Optional title |
| password_hash | VARCHAR | Bcrypt password hash |
| waiting_room_enabled | BOOLEAN | Waiting room feature flag |
| meeting_status | VARCHAR | Current state (idle, active, ended) |

#### meeting_owners

| Column | Type | Description |
|--------|------|-------------|
| id | SERIAL | Primary key |
| meeting_id | VARCHAR | References meetings.room_id |
| user_id | VARCHAR | Owner user ID |
| delegated_by | VARCHAR | User who delegated ownership |
| delegated_at | TIMESTAMP | When ownership was delegated |
| is_active | BOOLEAN | Whether this ownership is active |
| created_at | TIMESTAMP | Record creation time |
| updated_at | TIMESTAMP | Record update time |

#### meeting_attendees

| Column | Type | Description |
|--------|------|-------------|
| id | SERIAL | Primary key |
| meeting_id | VARCHAR | References meetings.room_id |
| user_id | VARCHAR | Attendee user ID |
| created_at | TIMESTAMP | Record creation time |

### Key Components

| File | Purpose |
|------|---------|
| `actix-api/src/meeting_api.rs` | HTTP handler and request/response types |
| `actix-api/src/models/meeting.rs` | Meeting model and database operations |
| `actix-api/src/models/meeting_owner.rs` | Ownership tracking with delegation |
| `actix-api/src/models/meeting_attendee.rs` | Pre-registered attendee management |
| `videocall-types/src/feature_flags.rs` | Feature flag implementation |

## Usage Examples

### Create a meeting with auto-generated ID

```bash
curl -X POST http://localhost:8080/api/meetings \
  -H "Content-Type: application/json" \
  -H "Cookie: email=john_doe" \
  -d '{}'
```

### Create a meeting with custom ID and attendees

```bash
curl -X POST http://localhost:8080/api/meetings \
  -H "Content-Type: application/json" \
  -H "Cookie: email=john_doe" \
  -d '{
    "meetingId": "weekly-standup",
    "attendees": ["alice", "bob", "charlie"]
  }'
```

### Create a password-protected meeting

```bash
curl -X POST http://localhost:8080/api/meetings \
  -H "Content-Type: application/json" \
  -H "Cookie: email=john_doe" \
  -d '{
    "meetingId": "private-meeting",
    "password": "secret123",
    "attendees": ["alice"]
  }'
```

## Meeting States

| State | Description |
|-------|-------------|
| `idle` | Meeting created but not yet started (no participants have joined) |
| `active` | Meeting is in progress (participants present) |
| `ended` | Meeting has concluded |

## Limitations

- Maximum 100 attendees per meeting
- Meeting IDs must contain only alphanumeric characters, underscores, and hyphens
- Spaces in IDs are automatically converted to underscores
- Password is hashed with bcrypt (cannot be retrieved, only verified)

## Implementation Notes

> **Note:** This section documents current implementation details that may change in future versions.

### Authentication (Subject to Change)

The API currently uses cookie-based authentication, extracting the host identity from an `email` cookie. This approach is a temporary implementation and will likely be replaced with a proper authentication mechanism such as:

- JWT tokens
- OAuth 2.0 / OpenID Connect
- API keys

**Current behavior:**
- Host ID is read from `Cookie: email=<user-id>`
- No token validation or expiration
- No integration with external identity providers

Plan accordingly if integrating with this API, as the authentication method will change.

### Database Operations

The implementation uses a mix of synchronous and asynchronous database clients:

| Operation | Client | Notes |
|-----------|--------|-------|
| Meeting creation | sqlx (async) | Preferred approach |
| Meeting existence check | sqlx (async) | |
| Attendee management | sqlx (async) | |
| Owner creation | postgres (sync) | Uses `spawn_blocking`, may be migrated to async |

### Partial Failure Handling

The endpoint does not use database transactions across all operations. If a failure occurs mid-request:

| Step | Failure Behavior |
|------|------------------|
| Meeting creation fails | Returns error, no data persisted |
| Owner record creation fails | Meeting exists, owner not recorded (logged, returns success) |
| Attendee creation fails | Meeting exists, attendees not recorded (logged, returns success) |

The `creator_id` field on the meeting record serves as a fallback for ownership if the `meeting_owners` record fails to create.

### ID Validation

All IDs (meeting, host, attendee) are validated against the pattern `^[a-zA-Z0-9_-]*$`. Spaces are automatically converted to underscores before validation.
