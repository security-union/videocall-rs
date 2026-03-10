# Identity Cleanup: Rename `email` to `user_id` / `username` to `display_name`

**Branch:** `fix/uuid`
**76 files changed**, 1318 insertions, 870 deletions

---

## Summary

Renamed internal variables and protobuf fields to clearly separate three identity concepts:

| Concept | Before (variable names) | After |
|---------|------------------------|-------|
| **User identity** | `email`, `userid`, `user_id`, `sub`, `creator_id` | `user_id` everywhere |
| **Display name** | `username`, `email` (sometimes) | `display_name` |
| **Session** | `session_id` | `session_id` (unchanged) |

The `email` field in protobuf messages was misleading — it carried user identity (not always an email). Display name was used for identification in some places, which is fragile. This refactor enforces clean separation.

---

## Wire Compatibility

Protobuf field **numbers** are unchanged. Only field **names** changed. Binary wire format is fully backward-compatible. Old and new clients/servers can interoperate during rollout.

---

## Proto Schema Changes (5 files)

| File | Change |
|------|--------|
| `protobuf/types/packet_wrapper.proto` | `string email = 2` → `string user_id = 2` |
| `protobuf/types/media_packet.proto` | `string email = 2` → `string user_id = 2`; added `bool is_speaking = 4` to `HeartbeatMetadata` |
| `protobuf/types/meeting_packet.proto` | `string target_email = 7` → `string target_user_id = 7` |
| `protobuf/types/health_packet.proto` | `string reporting_peer = 3` → `string reporting_user_id = 3` |
| `protobuf/types/server_connection_packet.proto` | `string customer_email = 2` → `string user_id = 2` (in `ConnectionMetadata`) |

Generated Rust types were regenerated from protos via `cd protobuf && make build`.

---

## Generated Types (5 files)

| File | Notes |
|------|-------|
| `videocall-types/src/protos/packet_wrapper.rs` | Regenerated — `.email` → `.user_id` |
| `videocall-types/src/protos/media_packet.rs` | Regenerated — `.email` → `.user_id`, added `is_speaking` field |
| `videocall-types/src/protos/meeting_packet.rs` | Regenerated — `.target_email` → `.target_user_id` |
| `videocall-types/src/protos/health_packet.rs` | Regenerated — `.reporting_peer` → `.reporting_user_id` |
| `videocall-types/src/protos/server_connection_packet.rs` | Regenerated — `.customer_email` → `.user_id` |

---

## Shared Type Crates (4 files)

| File | Change |
|------|--------|
| `videocall-types/src/lib.rs` | `SYSTEM_USER_EMAIL` → `SYSTEM_USER_ID` |
| `videocall-meeting-types/src/responses.rs` | `ParticipantStatusResponse.email` → `.user_id` |
| `videocall-meeting-types/src/requests.rs` | `AdmitRequest.email` → `.user_id` |
| `videocall-meeting-types/src/token.rs` | Updated doc comment on `sub` claim |

---

## Media Server — actix-api (13 files)

| File | Key Changes |
|------|-------------|
| `actors/session_logic.rs` | Type alias `Email` → `UserId`; field `email` → `user_id` |
| `actors/mod.rs` | Re-export `UserId` |
| `actors/chat_server.rs` | `SYSTEM_USER_EMAIL` → `SYSTEM_USER_ID`; message struct fields; test assertions |
| `actors/transports/ws_chat_session.rs` | `email` param → `user_id` |
| `actors/transports/wt_chat_session.rs` | `email` param → `user_id` |
| `session_manager.rs` | `SYSTEM_USER_EMAIL` → `SYSTEM_USER_ID`; `ReservedUserEmail` → `ReservedUserId` |
| `lobby.rs` | JWT extraction: `email` → `user_id` throughout |
| `token_validator.rs` | Doc comments updated |
| `server_diagnostics.rs` | `customer_email` → `user_id` |
| `client_diagnostics.rs` | `reporting_peer` → `reporting_user_id` |
| `bin/metrics_server.rs` | `reporting_peer` → `reporting_user_id` |
| `bin/metrics_server_snapshot.rs` | `customer_email` → `user_id` |
| `webtransport/mod.rs` | Log messages and test assertions |

---

## Meeting API (11 files)

| File | Key Changes |
|------|-------------|
| `auth.rs` | `AuthUser.email` → `.user_id` |
| `token.rs` | Function param `email` → `user_id` |
| `error.rs` | Param name updated |
| `nats_events.rs` | `SYSTEM_EMAIL` → `SYSTEM_USER_ID`; `target_email` → `target_user_id` |
| `db/participants.rs` | Struct field, SQL column refs, function params |
| `routes/participants.rs` | `AuthUser { email }` → `{ user_id }` throughout |
| `routes/meetings.rs` | Same pattern |
| `routes/waiting_room.rs` | Same pattern |
| `routes/oauth.rs` | `get_profile` destructure updated |
| `tests/observer_token_tests.rs` | JSON `"email"` → `"user_id"`; assertions `.email` → `.user_id` |
| `tests/participant_tests.rs` | Same |
| `tests/waiting_room_tests.rs` | Same |

---

## Client Library — videocall-client (10 files)

| File | Key Changes |
|------|-------------|
| `client/video_call_client.rs` | `VideoCallClientOptions.userid` → `.user_id`; `get_peer_email()` → `get_peer_user_id()`; `SYSTEM_USER_EMAIL` → `SYSTEM_USER_ID`; all `.email` on protos → `.user_id` |
| `connection/connection.rs` | Heartbeat packet: `email:` → `user_id:` |
| `connection/connection_manager.rs` | Self-filter: `packet.email` → `packet.user_id` |
| `encode/transform.rs` | `MediaPacket` and `PacketWrapper` field renames |
| `encode/microphone_encoder.rs` | Same |
| `encode/camera_encoder.rs` | Same |
| `encode/screen_encoder.rs` | Same |
| `decode/peer_decode_manager.rs` | `Peer.email` → `.user_id`; method params |
| `health_reporter.rs` | `reporting_peer` → `reporting_user_id` |
| `lib.rs` | Doc comment updates |

---

## Dioxus UI (15 files)

| File | Key Changes |
|------|-------------|
| `src/context.rs` | `UsernameCtx` → `DisplayNameCtx`; storage key `vc_username` → `vc_display_name`; functions `load/save/clear_username_*` → `*_display_name_*`; added `MeetingHost` struct |
| `src/main.rs` | Updated imports and context provider |
| `src/pages/home.rs` | Variable renames `username_*` → `display_name_*`; storage function calls |
| `src/pages/meeting.rs` | `current_user_email` → `current_user_id`; response `.email` → `.user_id` |
| `src/components/attendants.rs` | `email` prop → `user_id`; `VideoCallClientOptions { userid: }` → `{ user_id: }` |
| `src/components/host_controls.rs` | `participant.email` → `.user_id` |
| `src/components/peer_list.rs` | `get_peer_email` → `get_peer_user_id`; context type |
| `src/components/canvas_generator.rs` | `get_peer_email` → `get_peer_user_id` |
| `src/components/diagnostics.rs` | Same |
| `src/components/host.rs` | Storage function renames |
| `src/components/waiting_room.rs` | `email` prop → `user_id` |
| `README.md` | `UsernameCtx` → `DisplayNameCtx` |
| `tests/home_integration.rs` | Imports + storage key updated |
| `tests/context_unit.rs` | Function names + storage key updated |

---

## Yew UI (7 files — minimal compatibility updates)

| File | Key Changes |
|------|-------------|
| `components/canvas_generator.rs` | `get_peer_email` → `get_peer_user_id` |
| `components/peer_list.rs` | Same |
| `components/host_controls.rs` | `p.email` → `p.user_id` on response types |
| `components/attendants.rs` | `userid:` → `user_id:` in options |
| `components/waiting_room.rs` | Same |
| `pages/meeting.rs` | Response `.email` → `.user_id`; options `userid:` → `user_id:` |
| `pages/home.rs` | `profile.email` → `profile.user_id` |

---

## CLI (3 files)

| File | Key Changes |
|------|-------------|
| `videocall-cli/src/producers/camera.rs` | Proto field renames |
| `videocall-cli/src/producers/microphone.rs` | Same |
| `videocall-cli/src/consumers/webtransport.rs` | Same |

---

## Database Migration (1 new file)

**`dbmate/db/migrations/20260307000001_rename_email_to_user_id.sql`**

```sql
-- migrate:up
ALTER TABLE meeting_participants RENAME COLUMN email TO user_id;

-- migrate:down
ALTER TABLE meeting_participants RENAME COLUMN user_id TO email;
```

---

## OAuth Boundary — NOT Renamed

These use the OIDC standard `email` claim from identity providers and were correctly left as-is:

- `meeting-api/src/oauth.rs` — `IdTokenClaims.email`
- `meeting-api/src/routes/oauth.rs` — `claims.email` extraction from provider
- `actix-api/src/auth/mod.rs` — OAuth claims processing
- `actix-api/src/bin/websocket_server.rs` — OAuth cookie handling

---

## Remaining Work (deferred)

1. **`host_user_id` in API responses** — `MeetingHost` context exists in dioxus-ui but `host_user_id` is not yet in `ParticipantStatusResponse` or populated by meeting-api routes
2. **Non-OAuth UUID generation** — `get_or_create_local_user_id()` not yet implemented; users without OAuth still lack stable identity
3. **localStorage migration** — Old `vc_username` key not auto-migrated to `vc_display_name` for returning users

---

## Compilation Verified

| Crate | Target | Status |
|-------|--------|--------|
| `videocall-types` | native | Pass |
| `videocall-meeting-types` | native | Pass |
| `meeting-api` (+ all tests) | native | Pass |
| `videocall-api` (actix-api) | native | Pass |
| `videocall-client` (default) | wasm32 | Pass |
| `videocall-client` (no-default-features) | wasm32 | Pass |
| `videocall-cli` | native | Pass |
| `videocall-ui-dioxus` | wasm32 | Pass |
| `videocall-ui` (yew) | wasm32 | Pass |
