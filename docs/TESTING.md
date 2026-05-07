# Testing Overview

This repository uses three main automated test layers. They serve different
purposes and are intentionally designed with different tradeoffs.

## 1. Playwright E2E tests

Location:
- `e2e/tests/`
- `e2e/helpers/`
- `e2e/playwright.config.ts`
- `docker/docker-compose.e2e.yaml`

These are the true end-to-end browser tests.

They run:
- a real Chromium browser through Playwright
- the real `dioxus-ui`
- the real `meeting-api`
- the real `actix-api` / websocket server
- supporting services such as Postgres and NATS

CI brings this stack up with Docker Compose, waits for the services to become
reachable, and then runs `npx playwright test`.

### Design goals

- exercise real UI navigation and browser behavior
- exercise real backend APIs and realtime meeting behavior
- keep setup deterministic by seeding state directly when useful
- avoid external auth and hardware dependencies

### Common coding patterns

- Use signed session cookies instead of driving a full login flow.
  - See `e2e/helpers/auth.ts`
  - See `e2e/helpers/auth-context.ts`
- Seed meetings and participants directly through the meeting API when the test
  only cares about later UI behavior.
  - See `e2e/helpers/meeting-api.ts`
- Use multiple browser instances or contexts for multi-user scenarios.
- Use fake camera/mic flags so tests run headlessly in CI.
- Wait on stable UI states such as:
  - route changes
  - `#grid-container`
  - waiting room text
  - host control buttons

### Representative specs

- `e2e/tests/two-users-meeting.spec.ts`
  - host and guest both join and see each other
- `e2e/tests/guest-waiting-room.spec.ts`
  - waiting room admission flows
- `e2e/tests/meeting-settings.spec.ts`
  - host configuration and ownership behavior

### When to add a Playwright test

Use Playwright when the behavior depends on:
- real routing
- multiple users
- WebSocket or WebTransport behavior
- meeting lifecycle transitions
- layout and page-level interaction across the full stack

## 2. Dioxus browser integration tests

Location:
- `dioxus-ui/tests/`
- `dioxus-ui/tests/support/mod.rs`
- `.github/workflows/pr-check-dioxus-ui-hcl.yaml`

These tests use `wasm-bindgen-test` to run Rust/WASM test binaries in a real
browser with ChromeDriver.

They are not full end-to-end tests. They mount Dioxus components or routed page
fragments inside a browser test harness and mock only the pieces they need.

### Design goals

- catch frontend regressions quickly
- test browser-specific behavior without booting the full Docker stack
- keep assertions close to individual components or single-page flows

### Common coding patterns

- Use shared helpers from `dioxus-ui/tests/support/mod.rs` for:
  - mount-point creation
  - render helpers
  - fetch mocking
  - runtime config injection
  - local/session storage reset
  - fake browser capability injection
- Poll for DOM selectors rather than assuming immediate render completion.
- Keep tests scoped to one component or one routed page behavior.
- Mock fetch responses with the exact JSON shape the UI expects.

### Important limitation

`wasm-bindgen-test` runs each test binary in one browser session. That means
global browser state can leak between tests if the harness does not explicitly
reset:

- `window.fetch`
- `localStorage`
- `sessionStorage`
- router history
- injected runtime config

This is the main source of flake in the Dioxus browser test layer.

### Running Dioxus browser tests locally

Prerequisites:
- Rust toolchain with the `wasm32-unknown-unknown` target installed
- Chrome or Chromium
- `chromedriver` on your `PATH`

Install the wasm target if needed:

```bash
rustup target add wasm32-unknown-unknown
```

Check that ChromeDriver is available:

```bash
which chromedriver
```

Run one browser test binary locally:

```bash
cd dioxus-ui
CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --test home_integration
```

Build all Dioxus wasm test binaries without launching Chrome:

```bash
cd dioxus-ui
cargo test --target wasm32-unknown-unknown --no-run
```

Mirror the PR CI sequence locally:

```bash
cd dioxus-ui
export CHROMEDRIVER=$(which chromedriver)

cargo fmt --all --check
cargo test --target wasm32-unknown-unknown --lib
cargo test --target wasm32-unknown-unknown --test context_unit
cargo test --target wasm32-unknown-unknown --test device_selector
cargo test --target wasm32-unknown-unknown --test device_integration
cargo test --target wasm32-unknown-unknown --test home_integration
cargo test --target wasm32-unknown-unknown --test login_provider_logo
cargo test --target wasm32-unknown-unknown --test meeting_ended_overlay
cargo test --target wasm32-unknown-unknown --test meetings_list_owner_gating
cargo test --target wasm32-unknown-unknown --test screen_share_state
cargo test --target wasm32-unknown-unknown --test speaking_indicators
cargo test --target wasm32-unknown-unknown --test video_control_buttons
```

If you want to match the CI workflow even more closely, kill leftover browser
processes between binaries:

```bash
pkill -f chromedriver || true
pkill -f chrome || true
sleep 1
```

Use that cleanup between `cargo test` invocations if you see renderer hangs or
cross-test browser state issues locally.

### When to add a Dioxus browser integration test

Use this layer when the behavior depends on:
- a browser DOM
- a specific Dioxus page or component
- local browser APIs
- lightweight routing behavior

Do not use it for large multi-user or full-stack meeting scenarios. Those
belong in Playwright.

## 3. Backend integration tests

Location:
- `meeting-api/tests/`
- `actix-api/tests/`

These are Rust integration tests for API, persistence, and protocol behavior.

They validate:
- meeting lifecycle rules
- waiting room semantics
- auth and JWT behavior
- ownership and feed semantics
- invariants around participant rows and state transitions

Use this layer when the bug is fundamentally backend state or API semantics and
does not require a browser.

## Choosing the right layer

Use backend integration tests when:
- the behavior is mostly API or database semantics

Use Dioxus browser integration tests when:
- the behavior is frontend-only or page-local
- you want fast feedback in PR CI

Use Playwright E2E when:
- the behavior crosses UI, backend, and realtime boundaries
- the scenario involves multiple participants or true browser flows

## CI mapping

- Playwright E2E:
  - `.github/workflows/push-e2e-hcl.yaml`
- Dioxus browser integration:
  - `.github/workflows/pr-check-dioxus-ui-hcl.yaml`
- backend and crate-specific integration checks:
  - crate-specific PR workflows under `.github/workflows/`

## Practical rule

Prefer the lowest layer that can prove the behavior correctly.

- If a unit or backend integration test can catch the bug, use that.
- If browser rendering or local browser APIs matter, use the Dioxus browser
  tests.
- If the bug only appears with real navigation, real services, or multiple
  users, use Playwright E2E.

## Running Playwright locally

Bring up the local E2E stack:

```bash
docker compose -p videocall-e2e -f docker/docker-compose.e2e.yaml up -d
```

Install browser-test dependencies and run the suite:

```bash
cd e2e
npm ci
npx playwright install --with-deps chromium
npx playwright test
```

Tear the stack down when done:

```bash
docker compose -p videocall-e2e -f docker/docker-compose.e2e.yaml down
```
