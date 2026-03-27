# CLAUDE.md

## Project Overview

`videocall-rs` is a Rust-based video calling platform. The main crates are:

- **videocall-client** - Client library targeting `wasm32-unknown-unknown`.
- **dioxus-ui** - Dioxus-based frontend (the sole UI, uses `videocall-client`)
- **videocall-types** - Shared protobuf types
- **videocall-codecs** - Audio/video codec wrappers

## Build Commands

```bash
# Check framework-agnostic mode (no yew)
cargo check --target wasm32-unknown-unknown --no-default-features -p videocall-client

# Check default mode
cargo check --target wasm32-unknown-unknown -p videocall-client
```

## E2E Tests (Playwright)

Browser-based end-to-end tests in `e2e/` using Playwright. Tests run against the Dioxus UI (port 3001). Auth is bypassed via JWT cookie injection. See the `e2e-*` targets in the `Makefile` for available commands.

Key files:
- `docker/docker-compose.e2e.yaml` — Stack definition (Dioxus UI + shared backend)
- `e2e/playwright.config.ts` — Project configuration
- `e2e/helpers/auth.ts` — JWT session cookie injection

## Agent Usage Policy

Always delegate work to the specialized roster agents instead of making changes directly. Use the appropriate agent for each task:

**This includes technical decisions, not just code edits.** Before making any recommendation or assessment about transport protocols (WebTransport, WebSocket, QUIC, datagrams, reliability), security design, or performance trade-offs, delegate to the relevant specialist agent first. Do not reason about domain-specific behavior independently — use the agent's expertise, then relay its findings. Getting the answer wrong because you skipped the expert is worse than taking an extra minute to ask.

- **frontend-rust-webtransport-and-websocket** — All Dioxus UI changes (components, pages, styling, state management)
- **backend-rust-streaming** — All backend/API changes (Axum routes, DB queries, server logic)
- **code-reviewer** — Review all code changes before committing
- **performance-reviewer** — Performance review for low-power devices and low-bandwidth networks. Audit payload sizes, unnecessary re-renders, uncompressed assets, polling intervals, missing pagination, protobuf message sizes, bundle sizes, memory leaks, and any patterns that degrade on constrained hardware or slow connections. Should be run after substantive code changes alongside code-reviewer.
- **web-security-auditor** — Full-scope application security: backend auth/authz, API endpoints, input validation, XSS/injection, CSRF, UI trust indicators (e.g. host badges, role icons, permission displays), identity comparison logic, token handling, phishing vectors, architectural security review. Must audit both server-side AND client-side code — rendering code that conveys trust or authority is security-critical.
- **database-reviewer** — Review schema, migration, and query changes
- **integration-test-writer** — Write integration tests for new or changed features
- **deploy-sync-expert** — Update Docker/K8s configs when services or dependencies change
- **e2e-test-sync** — Create/update E2E tests when user-facing behavior changes
- **ux-ui-expert** — UI/UX design guidance, component design, visual polish, accessibility

Run agents in parallel when tasks are independent. Always run `code-reviewer` after substantive code changes. Always run `e2e-test-sync` after any change that affects user-facing behavior — E2E tests must be updated to cover the change and must pass before the work is considered complete.

**Never generate your own general-purpose agents.** Only use the agents listed on this roster. If no roster agent fits the task, stop everything and ask the user for direction.

## Source Code Rules

- **No symlinks or hardlinks for source files.** Each crate/UI must own its files independently. Do not use symlinks between source directories.

## Linter & Formatter Rules

**All code changes MUST pass project linters before being considered complete.** Agents must run the appropriate linter/formatter after editing any file:

- **Rust code:** Run `cargo fmt` on changed crates. Run `cargo clippy` to catch warnings and fix them.
- **TypeScript / JS (e2e/):** Run `cd e2e && npx prettier --write <files> && npx eslint <files> && npx tsc --noEmit` to match the CI `ci:lint` check.
- **General:** No unused imports, no unused variables, follow existing code style. Respect all project lint configs (`.eslintrc`, `rustfmt.toml`, `.prettierrc`, etc.).

This is mandatory for every agent making code changes — not optional. CI will reject PRs that fail linting.
