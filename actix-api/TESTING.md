# actix-api Testing Guide

This document describes how to run and write backend tests for `actix-api`.

## Overview

All tests live inside the source files as `#[cfg(test)]` modules — there is no
separate `tests/` directory. Tests run against real PostgreSQL and NATS
instances provided by Docker Compose.

## Prerequisites

- **Docker** and **Docker Compose** (v2)

That's it. The test runner, database, and message broker all run in containers.

## Quick start

```bash
# Build + run all backend tests (spins up PostgreSQL + NATS in Docker)
make tests_run

# Tear down test containers and volumes
make tests_down

# Rebuild the test Docker image (after Dockerfile changes)
make tests_build
```

`make tests_run` does two things in sequence: first it brings up the Docker
Compose stack (`postgres`, `nats`, `rust-tests`), then it runs the `rust-tests`
service which applies database migrations and executes `cargo test`.

To run a **single test** you can override the command:

```bash
docker compose -f docker/docker-compose.integration.yaml run --rm rust-tests \
  bash -c "cd /app/dbmate && dbmate wait && dbmate up && \
           cd /app/actix-api && cargo test test_meeting_creation -- --nocapture --test-threads=1"
```

## Infrastructure

Tests are orchestrated by `docker/docker-compose.integration.yaml`, which
provides three services:

| Service | Image | Purpose |
|---------|-------|---------|
| `postgres` | `postgres:12` | Database for meetings, sessions, and participants |
| `nats` | `nats:2.10-alpine` | Message broker with JetStream enabled |
| `rust-tests` | Built from `docker/Dockerfile.actix` | Test runner container |

The `rust-tests` container mounts the repo at `/app` and runs:

1. `dbmate wait && dbmate up` — waits for PostgreSQL, applies migrations
2. `cargo test -p videocall-api -- --nocapture --test-threads=1`

Note: `make tests_run` (from the Makefile) additionally runs `cargo clippy`,
`cargo fmt --check`, and `cargo machete` before tests. The docker-compose
command in the yaml file runs only the test step.

Tests execute single-threaded (`--test-threads=1`) because they share a
database and use `#[serial_test::serial]` to prevent race conditions.

## Testing patterns

### Database cleanup

Tests that touch the database clean up at the start and end using a helper like
this (from `src/webtransport/mod.rs`):

```rust
async fn cleanup_room(pool: &sqlx::PgPool, room_id: &str) {
    let _ = sqlx::query("DELETE FROM session_participants WHERE room_id = $1")
        .bind(room_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .execute(pool)
        .await;
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
3. Create unique room/user IDs to avoid collisions with other tests.
4. Clean up database rows at the start and end of the test.
5. Run `make tests_run` to verify.
