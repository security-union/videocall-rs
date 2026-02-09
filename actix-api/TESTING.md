# actix-api Testing Guide

This document describes how to run and write backend tests for `actix-api`.

## Overview

All tests live inside the source files as `#[cfg(test)]` modules — there is no
separate `tests/` directory. Tests run against real PostgreSQL and NATS
instances provided by Docker Compose.

## Test inventory

| File | Count | What it covers |
|------|-------|----------------|
| `src/session_manager.rs` | 24 | Meeting creation, multi-user join/leave, host controls, feature flag on/off, system email rejection |
| `src/bin/metrics_server.rs` | 17 | Session tracking, health metrics export, stale session cleanup, concurrent access, RTT/NetEQ metrics |
| `src/webtransport/mod.rs` | 6 | Full meeting lifecycle over WebTransport (connect, join, leave, meeting end) |
| `src/actors/packet_handler.rs` | 4 | Packet classification — empty, garbage, and RTT detection |
| `src/actors/chat_server.rs` | 4 | Room join/rejection, system email validation |
| `src/actors/transports/ws_chat_session.rs` | 2 | Meeting lifecycle over WebSocket connections |
| `src/actors/session_logic.rs` | 1 | Action debug formatting |

## Quick start

```bash
# Build + run all backend tests (spins up PostgreSQL + NATS in Docker)
make tests_run

# Tear down test containers and volumes
make tests_down

# Rebuild the test Docker image (after Dockerfile changes)
make tests_build
```

## Infrastructure

Tests are orchestrated by `docker/docker-compose.integration.yaml`, which
provides three services:

| Service | Image | Purpose |
|---------|-------|---------|
| `postgres` | `postgres:12` | Database for meetings, sessions, and participants |
| `nats` | `nats:2.10-alpine` | Message broker with JetStream enabled |
| `rust-tests` | Built from `docker/Dockerfile.actix` | Test runner container |

The `rust-tests` container:

1. Mounts the entire repo at `/app`
2. Runs `dbmate wait && dbmate up` to apply database migrations
3. Runs `cargo clippy -- -D warnings` (lint check)
4. Runs `cargo fmt --check` (formatting check)
5. Runs `cargo machete` (unused dependency check)
6. Runs `cargo test -p videocall-api -- --nocapture --test-threads=1`

Tests execute single-threaded (`--test-threads=1`) because they share a
database and use `#[serial_test::serial]` to prevent race conditions.

## Testing patterns

### Database cleanup

Tests that touch the database include cleanup helpers:

```rust
async fn cleanup_room(pool: &PgPool, room: &str) {
    sqlx::query("DELETE FROM session_participants WHERE meeting_id IN (SELECT id FROM meetings WHERE room_id = $1)")
        .bind(room).execute(pool).await.ok();
    sqlx::query("DELETE FROM meetings WHERE room_id = $1")
        .bind(room).execute(pool).await.ok();
}
```

### Feature flag testing

Tests toggle feature flags explicitly and clean up after themselves:

```rust
FeatureFlags::set_meeting_management_override(true);
// ... test logic ...
FeatureFlags::clear_meeting_management_override();
```

### Test isolation

Each test uses a unique room ID (e.g. `"test-room-create-1"`,
`"test-room-join-2"`) to avoid conflicts when tests run sequentially.

### Integration test helpers

Several helpers simplify integration testing:

- `get_test_pool()` — creates a database connection pool from `DATABASE_URL`
- `wait_for_participant_count(pool, room, expected, timeout)` — polls until
  the expected number of participants is reached
- `start_webtransport_server()` / `start_websocket_server()` — starts test
  servers on ephemeral ports

## CI

Tests run automatically via `.github/workflows/cargo-test.yaml`, triggered on
PRs that touch `actix-api/`, `videocall-types/`, or `protobuf/`. The workflow
calls `make tests_run` and always tears down with `make tests_down`.

## Writing a new test

1. Add a `#[cfg(test)]` module at the bottom of the source file being tested.
2. Use `#[serial_test::serial]` if the test touches the database or shared
   state.
3. Create unique room/user IDs to avoid collisions.
4. Clean up database rows at the start and end of the test.
5. Run `make tests_run` to verify.
