# CLAUDE.md

## Project Overview

`videocall-rs` is a Rust-based video calling platform. The main crates are:

- **videocall-client** - Client library targeting `wasm32-unknown-unknown`. Supports two modes via the `yew-compat` cargo feature (enabled by default).
- **yew-ui** - Yew-based frontend (uses `videocall-client` with `yew-compat`)
- **dioxus-ui** - Dioxus-based frontend (uses `videocall-client` without `yew-compat`)
- **videocall-types** - Shared protobuf types
- **videocall-codecs** - Audio/video codec wrappers

## Build Commands

```bash
# Check framework-agnostic mode (no yew)
cargo check --target wasm32-unknown-unknown --no-default-features -p videocall-client

# Check yew mode (default)
cargo check --target wasm32-unknown-unknown -p videocall-client

# Full integration tests
make yew-tests-docker
```

## E2E Tests (Playwright)

Browser-based end-to-end tests in `e2e/` using Playwright. Tests run against both the Dioxus UI (port 3001) and Yew UI (port 80) via two Playwright projects. Auth is bypassed via JWT cookie injection. See the `e2e-*` targets in the `Makefile` for available commands.

Key files:
- `docker/docker-compose.e2e.yaml` — Stack definition (both UIs + shared backend)
- `e2e/playwright.config.ts` — Project configuration (dioxus + yew)
- `e2e/helpers/auth.ts` — JWT session cookie injection

## Architecture: Yew Separation Pattern

The `videocall-client` crate uses a companion file pattern to separate yew-specific code from framework-agnostic code. All yew code is gated behind the `yew-compat` cargo feature.

### Pattern

Each file with yew-specific code has a companion `*_yew.rs` file declared at the bottom:

```rust
// At the bottom of camera_encoder.rs:
#[cfg(feature = "yew-compat")]
#[path = "camera_encoder_yew.rs"]
mod yew_compat;
```

Companion files use `use super::*;` to access parent types and do NOT need individual `#[cfg(feature = "yew-compat")]` guards since the entire module is conditionally compiled.

### What stays in the main file
- Struct definitions (with `#[cfg]` on fields that differ between modes)
- `#[cfg(not(feature = "yew-compat"))]` impl blocks (framework-agnostic)
- Shared/ungated impl blocks and functions

### What goes in the `_yew.rs` companion file
- All `#[cfg(feature = "yew-compat")]` impl blocks
- Yew-specific imports (`use yew::Callback;`)

### Companion files

| Main File | Companion File |
|---|---|
| `src/encode/camera_encoder.rs` | `camera_encoder_yew.rs` |
| `src/encode/microphone_encoder.rs` | `microphone_encoder_yew.rs` |
| `src/encode/screen_encoder.rs` | `screen_encoder_yew.rs` |
| `src/encode/mod.rs` | `yew_compat.rs` (re-exports `MicrophoneEncoderTrait`, `create_microphone_encoder`) |
| `src/media_devices/media_device_access.rs` | `media_device_access_yew.rs` |
| `src/media_devices/media_device_list.rs` | `media_device_list_yew.rs` |
| `src/decode/peer_decode_manager.rs` | `peer_decode_manager_yew.rs` |
| `src/health_reporter.rs` | `health_reporter_yew.rs` |
| `src/client/video_call_client.rs` | `video_call_client_yew.rs` |

The `connection/` module was already properly separated before this refactoring.

### Key difference: yew vs non-yew
- Yew mode uses `yew::Callback<T>` for event callbacks
- Non-yew mode uses `Rc<dyn Fn(T)>` or `Box<dyn Fn(T)>` closures
- Non-yew mode uses `CanvasIdProvider` trait instead of yew `Callback` for canvas IDs
- Non-yew mode uses `emit_client_event()` / `ClientEvent` event bus for framework-agnostic eventin

## Agent Usage Policy

Always delegate work to the specialized roster agents instead of making changes directly. Use the appropriate agent for each task:

- **frontend-rust-webtransport-and-websocket** — All Dioxus/Yew UI changes (components, pages, styling, state management)
- **backend-rust-streaming** — All backend/API changes (Axum routes, DB queries, server logic)
- **code-reviewer** — Review all code changes before committing
- **web-security-auditor** — Full-scope application security: backend auth/authz, API endpoints, input validation, XSS/injection, CSRF, UI trust indicators (e.g. host badges, role icons, permission displays), identity comparison logic, token handling, phishing vectors, architectural security review. Must audit both server-side AND client-side code — rendering code that conveys trust or authority is security-critical.
- **database-reviewer** — Review schema, migration, and query changes
- **integration-test-writer** — Write integration tests for new or changed features
- **deploy-sync-expert** — Update Docker/K8s configs when services or dependencies change
- **e2e-test-sync** — Create/update E2E tests when user-facing behavior changes
- **ux-ui-expert** — UI/UX design guidance, component design, visual polish, accessibility

Run agents in parallel when tasks are independent. Always run `code-reviewer` after substantive code changes.

**Never generate your own general-purpose agents.** Only use the agents listed on this roster. If no roster agent fits the task, stop everything and ask the user for direction.

## Source Code Rules

- **No symlinks or hardlinks for source files.** Each crate/UI must own its files independently. Do not use symlinks between `dioxus-ui/` and `yew-ui/` static assets or any other source directories. If both UIs need shared CSS, copy the shared base and maintain framework-specific additions separately.

## Linter & Formatter Rules

**All code changes MUST pass project linters before being considered complete.** Agents must run the appropriate linter/formatter after editing any file:

- **Rust code:** Run `cargo fmt` on changed crates. Run `cargo clippy` to catch warnings and fix them.
- **TypeScript / JS (e2e/):** Run `cd e2e && npx prettier --write <files> && npx eslint <files> && npx tsc --noEmit` to match the CI `ci:lint` check.
- **General:** No unused imports, no unused variables, follow existing code style. Respect all project lint configs (`.eslintrc`, `rustfmt.toml`, `.prettierrc`, etc.).

This is mandatory for every agent making code changes — not optional. CI will reject PRs that fail linting.

