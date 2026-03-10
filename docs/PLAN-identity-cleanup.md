# Plan: Identity Cleanup — Session ID, User ID, and Display Name

> **Goal:** Clean up how the app identifies users and sessions. Stop using display name for identification. Use session_id in packets, user_id (from identity service) for user identity, and display_name only for UI display.

---

## Current State (Problems)

### Three identifiers, all tangled

| Concept | Variable Names Used | Actual Value | Where Used |
|---------|-------------------|--------------|------------|
| **Display Name** | `username`, `email`, `user_id`, `userid` | User-entered text ("John Doe") | localStorage, UI, sometimes packets |
| **User Identity** | `email`, `sub`, `creator_id`, `user_id` | OAuth identifier (email or provider ID) | JWT claims, DB, meeting host check |
| **Session** | `session_id`, `id` | Server-assigned u64 | NATS routing, echo filtering, heartbeats |

### Specific problems

1. **`email` field in PacketWrapper and MediaPacket** — Named "email" but sometimes contains a display name (when OAuth is disabled). Used as the primary sender identifier in every packet.

2. **`email` duplicated in every MEDIA packet** — Present in both `PacketWrapper.email` (outer, unencrypted) and `MediaPacket.email` (inner, encrypted). Both carry the same value.

3. **Display name used for identification** — When OAuth is off, the user-entered display name becomes the `userid` passed to `VideoCallClient`, which then becomes `PacketWrapper.email`. Two users with the same display name would collide.

4. **Host identified by display name comparison** — `canvas_generator.rs` compares `host_display_name` against peer's `email` field to show the host crown. If the host changes their display name, or if `email` != display name, the crown disappears.

5. **Variable naming chaos** — The same value is called `email`, `userid`, `user_id`, `sub`, `creator_id` depending on where you look. The field `email` in protobuf messages should be `user_id` since it's not always an email.

6. **No stable user_id when OAuth is disabled** — Without OAuth, there's no persistent identity. The display name IS the identity, which is fragile.

---

## Target State

### Clean separation of three concerns

| Concept | Field Name | Type | Source | Sent in Packets? |
|---------|-----------|------|--------|-------------------|
| **Session ID** | `session_id` | `u64` | Server-assigned on connect | Yes — in `PacketWrapper.session_id` (already exists) |
| **User ID** | `user_id` | `string` | Identity service (OAuth `sub` claim) | Yes — in `PacketWrapper` (replaces `email` field) |
| **Display Name** | `display_name` | `string` | User-entered, stored per meeting | No — only in JWT, DB, and UI. NOT in media packets. |

### Key design decisions

1. **`session_id` is the primary packet identifier** — The server already assigns it and fills it in. Clients should use it to identify peers, not the email/user_id string.

2. **`user_id` replaces `email` everywhere** — Rename the protobuf field, variable names, and DB columns. The value comes from the identity service `sub` claim.

3. **Display name is display-only** — Never used for routing, identification, or packet filtering. Only appears in JWT (for initial display), DB (per-participant), and UI rendering.

4. **Multi-device same user** — Same `user_id`, different `session_id`s. Host check uses `user_id` (not display name). All sessions with the host's `user_id` show the host indicator.

5. **Non-OAuth mode** — Generate a client-side UUID as `user_id` when OAuth is disabled. Store in localStorage alongside display name. This provides stable identity without auth.

---

## Phase 1: Protobuf Schema Changes

### `protobuf/types/packet_wrapper.proto`

| Change | Before | After |
|--------|--------|-------|
| Rename field 2 | `string email = 2;` | `string user_id = 2;` |

> **Wire compatibility:** Renaming a protobuf field does NOT break wire format — only the field number matters. Field 2 stays field 2. Old and new clients can interop during rollout.

### `protobuf/types/media_packet.proto`

| Change | Before | After |
|--------|--------|-------|
| Remove field 2 | `string email = 2;` | `reserved 2; reserved "email";` |

> **Rationale:** `MediaPacket.email` is redundant with `PacketWrapper.user_id`. Removing it saves bytes on every media packet (the highest-volume packet type). The outer wrapper already identifies the sender. Reserve the field number to prevent future reuse.

### `protobuf/types/health_packet.proto`

| Change | Before | After |
|--------|--------|-------|
| Rename | `string reporting_peer = X;` | `string reporting_user_id = X;` |

### `protobuf/types/meeting_packet.proto`

| Change | Before | After |
|--------|--------|-------|
| Rename | `string creator_id = X;` | Keep as-is (already descriptive) |
| Rename | `string target_email = X;` | `string target_user_id = X;` |

### `protobuf/types/server_connection_packet.proto`

| Change | Before | After |
|--------|--------|-------|
| Rename | `string customer_email = X;` | `string user_id = X;` |

### Performance impact on PacketWrapper

| Field | Size (typical) | Change |
|-------|---------------|--------|
| `packet_type` | 1-2 bytes | No change |
| `email`→`user_id` | 20-50 bytes (email string) | Same size (UUID string ~36 bytes, or same email) |
| `data` | Variable | No change |
| `session_id` | 8 bytes | No change |
| **MediaPacket.email removed** | **-20 to -50 bytes per media packet** | **Savings** |

> **Net effect:** Every media packet (audio, video, screen — the vast majority of traffic) gets ~20-50 bytes smaller by removing the redundant inner `email` field.

---

## Phase 2: Backend Changes

### A. Database Schema

**New migration: Rename `email` → `user_id` in meeting_participants**

```sql
ALTER TABLE meeting_participants RENAME COLUMN email TO user_id;
-- Update unique constraint
ALTER TABLE meeting_participants DROP CONSTRAINT meeting_participants_meeting_id_email_key;
ALTER TABLE meeting_participants ADD CONSTRAINT meeting_participants_meeting_id_user_id_key UNIQUE (meeting_id, user_id);
```

**New migration: Rename `creator_id` → `host_user_id` in meetings (optional)**

```sql
ALTER TABLE meetings RENAME COLUMN creator_id TO host_user_id;
```

> **Note:** `host_display_name` column stays — it caches the display name for API responses.

**Users table:** `email` column is the primary key. Renaming to `user_id` is optional since the identity service provides this value as the OAuth `sub` claim, which is typically an email.

### B. Meeting API (`meeting-api/`)

| File | Changes |
|------|---------|
| `src/routes/participants.rs` | Rename `email` variables to `user_id`; update DB queries |
| `src/db/participants.rs` | Column references: `email` → `user_id` |
| `src/db/meetings.rs` | `creator_id` → `host_user_id` (if renamed) |
| `src/token.rs` | `SessionTokenClaims.sub` → document as `user_id` |
| `src/auth.rs` | `AuthUser` extractor: rename `email` field to `user_id` |
| `src/nats_events.rs` | `SYSTEM_EMAIL` → `SYSTEM_USER_ID` |

### C. Media Server (`actix-api/`)

| File | Changes |
|------|---------|
| `src/actors/session_logic.rs` | `email: Email` → `user_id: String` |
| `src/session_manager.rs` | `SYSTEM_USER_EMAIL` → `SYSTEM_USER_ID`; rename email params |
| `src/actors/chat_server.rs` | `email` references → `user_id` |
| `src/lobby.rs` | JWT extraction: `sub` → stored as `user_id` |
| `src/token_validator.rs` | Return `user_id` instead of `email` |
| `src/actors/transports/ws_chat_session.rs` | `email` → `user_id` |
| `src/actors/transports/wt_chat_session.rs` | `email` → `user_id` |
| `src/models/mod.rs` | `JoinRoom.user_id` already correct; check `ClientMessage` |
| `src/actors/packet_handler.rs` | Any email references → `user_id` |

### D. JWT Token Claims

**`videocall-meeting-types/src/token.rs` — RoomAccessTokenClaims:**

| Field | Before | After | Notes |
|-------|--------|-------|-------|
| `sub` | Email string | User ID string | Same wire value, semantic rename |
| `display_name` | Display name | Display name | No change |
| `is_host` | Boolean | Boolean | No change |

> The `sub` claim value doesn't change — it's still the OAuth provider's identifier. We just stop calling it "email" in code.

---

## Phase 3: Client Library Changes (`videocall-client/`)

### A. VideoCallClientOptions

| File | Change |
|------|--------|
| `src/client/video_call_client.rs:57-103` | Rename `userid` field → `user_id` |

### B. Connection Manager

| File | Change |
|------|--------|
| `src/connection/connection_manager.rs:302` | Self-filter: `packet.email` → `packet.user_id` |
| `src/connection/connection.rs:274` | Heartbeat: `email: userid` → `user_id: userid` |

### C. Encoder / Transform

| File | Change |
|------|--------|
| `src/encode/transform.rs` | Stop setting `MediaPacket.email`; only set `PacketWrapper.user_id` |
| `src/encode/microphone_encoder.rs:85-90` | `PacketWrapper.email` → `PacketWrapper.user_id` |

### D. Decoder / Peer Management

| File | Change |
|------|--------|
| `src/decode/peer_decode_manager.rs` | `Peer.email` → `Peer.user_id`; peer lookup uses `session_id` (already does) |
| `src/client/video_call_client.rs:488-502` | `get_peer_email()` → `get_peer_user_id()` |
| `src/client/video_call_client.rs:801-805` | System user filter: `SYSTEM_USER_EMAIL` → `SYSTEM_USER_ID` |

### E. Health Reporter

| File | Change |
|------|--------|
| `src/health_reporter.rs` | `reporting_peer` → `reporting_user_id` (matches proto change) |

---

## Phase 4: Frontend Changes (`dioxus-ui/`, `yew-ui/`)

### A. Identity Flow Cleanup

**Current flow (broken):**
```
User enters display name → stored as "username" → sent to API as display_name
→ API returns "email" (which is OAuth sub or generated ID) → used as "userid" in packets
→ BUT when OAuth is off, display name IS the userid (collision risk)
```

**Target flow:**
```
User enters display name → stored in localStorage as "vc_display_name"
Identity service provides user_id → stored in localStorage as "vc_user_id"
  - OAuth on:  user_id = JWT sub claim (email or provider ID)
  - OAuth off: user_id = client-generated UUID (persistent in localStorage)
Both sent to meeting API → API returns participant status with user_id confirmed
user_id used in VideoCallClient → appears in PacketWrapper.user_id
display_name used only for UI rendering
```

### B. Variable Renames

| File(s) | Before | After |
|---------|--------|-------|
| `dioxus-ui/src/context.rs` | `vc_username` storage key | `vc_display_name` |
| `dioxus-ui/src/context.rs` | `UsernameCtx` | `DisplayNameCtx` |
| `dioxus-ui/src/context.rs` | `load_username_from_storage()` | `load_display_name_from_storage()` |
| `dioxus-ui/src/context.rs` | `save_username_to_storage()` | `save_display_name_to_storage()` |
| `dioxus-ui/src/pages/home.rs` | `username_value`, `username_error` | `display_name_value`, `display_name_error` |
| `dioxus-ui/src/pages/meeting.rs` | `current_user_email` | `current_user_id` |
| `dioxus-ui/src/components/attendants.rs` | `userid: email.clone()` | `user_id: user_id.clone()` |
| Same changes in `yew-ui/` equivalents | | |

### C. Non-OAuth User ID Generation

**New: `dioxus-ui/src/context.rs`**

```rust
const USER_ID_STORAGE_KEY: &str = "vc_user_id";

pub fn get_or_create_user_id() -> String {
    if let Some(id) = load_from_storage(USER_ID_STORAGE_KEY) {
        return id;
    }
    let id = uuid::Uuid::new_v4().to_string();
    save_to_storage(USER_ID_STORAGE_KEY, &id);
    id
}
```

> When OAuth is enabled, the meeting API response provides the user_id (from JWT sub). When OAuth is disabled, the client generates a UUID and persists it. Either way, a stable user_id exists.

### D. Host Identification Fix

**Current (broken):** `canvas_generator.rs` compares `host_display_name` against peer's `email` field.

**Target:** Compare `host_user_id` against peer's `user_id`.

| File | Before | After |
|------|--------|-------|
| `yew-ui/src/components/canvas_generator.rs:42-44` | `host_display_name == peer_email` | `host_user_id == peer_user_id` |
| `yew-ui/src/pages/meeting.rs:66` | `host_display_name: Option<String>` | Add `host_user_id: Option<String>` (keep display name for UI) |
| Meeting API response | `host_display_name` only | Add `host_user_id` field |

**Multi-device host:** All sessions from the same `user_id` show the host crown, regardless of which device they're on.

---

## Phase 5: Meeting API Response Changes

### `ParticipantStatusResponse` (`videocall-meeting-types/src/responses.rs`)

| Field | Before | After | Notes |
|-------|--------|-------|-------|
| `email` | User's email | Rename to `user_id` | Primary identifier |
| `display_name` | Display name | Keep | For UI |
| `is_host` | Boolean | Keep | Host flag |
| `room_token` | JWT string | Keep | Media server auth |
| — | — | Add `host_user_id: String` | So clients can identify host across sessions |

### `MeetingInfoResponse`

| Field | Before | After |
|-------|--------|-------|
| `host_display_name` | Host's display name | Keep (for UI display) |
| — | Add `host_user_id: String` | For programmatic host identification |

---

## Migration / Backwards Compatibility

### Wire format safety
- Protobuf field renames don't break the wire format (field numbers unchanged)
- Adding `reserved` for removed fields prevents accidental reuse
- Old clients sending `email` (field 2) will be read as `user_id` (field 2) by new servers

### Rollout order
1. **Backend first** — Accept both old and new field semantics (field 2 is field 2)
2. **Client library** — Update to use new field names
3. **Frontend** — Update variable names and add UUID generation
4. **Database migration** — Rename columns (can be done anytime with proper migration)

### localStorage migration
- On app load, if `vc_username` exists but `vc_display_name` doesn't, copy value over
- If `vc_user_id` doesn't exist, generate UUID
- Clean up old keys after migration period

---

## Performance Considerations

### PacketWrapper (sent with every packet)

| Aspect | Impact |
|--------|--------|
| Rename `email` → `user_id` | Zero impact — same protobuf field number, same wire bytes |
| Keep `session_id` as primary routing key | Already the case server-side; no change |

### MediaPacket (highest volume — audio/video/screen)

| Aspect | Impact |
|--------|--------|
| **Remove `email` field** | **Saves 20-50 bytes per media packet** |
| At 50 packets/sec video + 50 packets/sec audio per user | **~2-5 KB/sec savings per user** |
| 10-user meeting | **~20-50 KB/sec total savings** |

### Routing
- Server already routes by `session_id` via NATS subjects (`room.{room}.{session}`)
- Echo filtering already uses `session_id` (not email)
- No routing logic needs to change

---

## Files Changed Summary

| Area | Files | Scope |
|------|-------|-------|
| Protobuf schemas | 5 `.proto` files | Field renames, 1 field removal |
| Generated types | Auto-generated from protos | Rebuild |
| Meeting API | ~8 files | Variable renames, 1 new response field |
| Media Server | ~10 files | Variable renames |
| videocall-client | ~8 files | Variable renames, remove MediaPacket.email usage |
| videocall-meeting-types | ~3 files | Response struct changes, token docs |
| dioxus-ui | ~6 files | Context/variable renames, UUID generation |
| yew-ui | ~6 files (if still active) | Same as dioxus-ui |
| DB migrations | 1-2 new migration files | Column renames |
| videocall-types | ~2 files | Constant renames |

---

## Verification Checklist

- [ ] Protobuf field numbers unchanged (wire compatible)
- [ ] `MediaPacket.email` removed and reserved
- [ ] No code path uses display name for identification or routing
- [ ] Host identified by `user_id`, not `display_name`
- [ ] Multi-device same user: all sessions show host crown if user is host
- [ ] Non-OAuth mode generates persistent UUID as `user_id`
- [ ] localStorage migration handles old `vc_username` key
- [ ] All backend tests pass with renamed fields
- [ ] All client tests pass
- [ ] E2E tests pass (meeting join, host identification, multi-device)
- [ ] No performance regression in packet size or throughput
