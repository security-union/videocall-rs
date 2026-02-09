# yew-ui Testing Guide

This document describes how to write and run **component and integration tests**
for the `yew-ui` crate. All tests in this layer run inside a single headless
Chrome instance via `wasm-bindgen-test`. They validate component rendering, DOM
output, and browser API integration (e.g. media devices, runtime config).

> **Scope note:** This testing infrastructure covers component-level and
> single-browser integration tests only. Multi-browser end-to-end tests (e.g.
> two participants joining the same meeting and verifying video tiles appear)
> require a browser automation framework like **Playwright** with the full
> backend stack running. That work is **TBD** — see
> [Future: Multi-browser E2E tests](#future-multi-browser-e2e-tests) at the
> bottom of this document.

## Why we test this way

Yew does not ship a built-in component testing library comparable to React's
`@testing-library/react` or Vue Test Utils. Yew's own
[testing documentation](https://yew.rs/docs/more/testing) states:

> *We are working on making it easy to test components, but this is currently
> a work in progress.*

The framework exposes a limited `yew::tests::layout_tests` module for snapshot
testing (added via [yewstack/yew#1413](https://github.com/yewstack/yew/issues/1413)
/ [yewstack/yew#2310](https://github.com/yewstack/yew/pull/2310)), but there is
no public API for constructing a `Context<Self>`, calling `Component::create` or
`Component::view` in isolation, or doing shallow rendering. This gap is well
illustrated by the unanswered community question
[How to unit test components? (Discussion #3651)](https://github.com/yewstack/yew/discussions/3651).

**What Yew's own framework does instead:** the Yew repository tests its
components using
[`wasm-bindgen-test`](https://rustwasm.github.io/wasm-bindgen/wasm-bindgen-test/index.html)
running in a real headless browser. You can see this pattern throughout Yew's
own test suite at
[`packages/yew/tests/`](https://github.com/yewstack/yew/tree/master/packages/yew/tests).
The approach is:

1. Compile the test to WebAssembly.
2. Launch a headless Chrome via `wasm-bindgen-test-runner` + `chromedriver`.
3. Create a `<div>` mount point in the document body.
4. Render the component into that div with `yew::Renderer::with_root()`.
5. Yield to Yew's internal scheduler (`sleep(Duration::ZERO).await`) so the
   component completes its first render.
6. Query the real DOM and assert on the rendered output.
7. Clean up the mount point.

We adopt this exact pattern because:

- **Yew components compile to WASM and depend on browser APIs** (`web-sys`,
  `js-sys`, DOM manipulation). They cannot be tested outside a browser
  environment.
- **It is the approach the Yew maintainers themselves use**, so it will track
  any future framework changes naturally.
- **Our components interact heavily with browser-only APIs** like
  `navigator.mediaDevices.enumerateDevices()` and `getUserMedia()`, which only
  exist in a real browser context.

For components that rely on media device enumeration, we additionally configure
Chrome with
[`--use-fake-device-for-media-stream`](https://webrtc.github.io/webrtc-org/testing/)
via `webdriver.json`, giving us real `MediaDeviceInfo` objects without requiring
physical hardware.

## Overview

Tests are organised into a three-layer pyramid (all single-browser):

| Layer | Scope | Where | Runs In |
|-------|-------|-------|---------|
| **1 — Unit** | `MediaDeviceList` + `MockMediaDevicesProvider` | `videocall-client/src/media_devices/media_device_list.rs` | `wasm-bindgen-test` (headless Chrome) |
| **2 — Component** | Individual Yew components with mock data | `yew-ui/tests/device_selector.rs`, `yew-ui/tests/video_control_buttons.rs` | `wasm-bindgen-test` (headless Chrome) |
| **3 — Integration** | Full browser API → component rendering; app startup with runtime config | `yew-ui/tests/device_integration.rs`, `yew-ui/tests/home_integration.rs` | `wasm-bindgen-test` (headless Chrome with fake devices / injected config) |
| **4 — E2E** *(TBD)* | Multi-browser, multi-participant meeting flows | *Not yet implemented* — planned with Playwright | Playwright + full backend stack |

## Prerequisites

### Option A: Docker (no local deps)

Just Docker. Run `make yew-tests-docker` and you're done.

### Option B: Native

- **Chrome** — any recent version
- **chromedriver** — must match your Chrome major version
  - macOS: `brew install chromedriver`
  - Linux: `sudo apt-get install chromium-chromedriver`
  - Or download from https://googlechromelabs.github.io/chrome-for-testing/
- **wasm-bindgen-cli** — must match the version in `Cargo.lock` (currently
  `0.2.106`): `cargo install wasm-bindgen-cli --version 0.2.106 --locked`
- **wasm32-unknown-unknown target**: `rustup target add wasm32-unknown-unknown`

## Quick start

```bash
# Native — headless
make yew-tests

# Native — visible browser (useful for debugging)
make yew-tests HEADED=1

# Docker — zero local deps
make yew-tests-docker

# Run a single test file
cd yew-ui && CHROMEDRIVER=$(which chromedriver) \
  cargo test --target wasm32-unknown-unknown --test device_selector

# Run a single test by name
cd yew-ui && CHROMEDRIVER=$(which chromedriver) \
  cargo test --target wasm32-unknown-unknown --test device_selector \
  -- device_selector_preselects_correct_device
```

## Shared test harness

`yew-ui/tests/support/mod.rs` contains helpers shared across all test files:

- **`create_mount_point()`** — creates a `<div>`, appends it to `<body>`, returns it.
- **`cleanup(mount)`** — removes the mount div from `<body>`.
- **`create_mock_device(id, kind, label)`** — builds a mock `MediaDeviceInfo` object with plain JS properties (structural getters).
- **`mock_mic(id, label)`**, **`mock_camera(id, label)`**, **`mock_speaker(id, label)`** — convenience wrappers.
- **`enumerate_fake_devices()`** — calls `getUserMedia` and `enumerateDevices` to obtain real `MediaDeviceInfo` objects from Chrome's fake-device infrastructure.

## Writing a new test

1. **Create a new test file** in `yew-ui/tests/` (e.g. `my_component.rs`).
2. **Add the required header:**

   ```rust
   #![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

   mod support;

   use support::{cleanup, create_mount_point};
   use wasm_bindgen_test::*;
   use yew::platform::time::sleep;
   use yew::prelude::*;
   use std::time::Duration;

   wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);
   ```

   The `cfg` guard ensures the test only compiles for the WASM browser target
   (not native or WASI). The `run_in_browser` macro tells `wasm-bindgen-test`
   to launch a real browser instead of running in Node.js.

3. **Write a test.** Most components need props, so you'll typically create a
   thin wrapper component that forwards the data you want to control:

   ```rust
   #[wasm_bindgen_test]
   async fn my_component_shows_label() {
       // 1. Define a wrapper that passes props to the component under test
       #[derive(Properties, PartialEq)]
       struct Props { label: String }

       #[function_component(Wrapper)]
       fn wrapper(props: &Props) -> Html {
           html! { <MyComponent label={props.label.clone()} /> }
       }

       // 2. Mount and render
       let mount = create_mount_point();
       yew::Renderer::<Wrapper>::with_root_and_props(
           mount.clone(),
           Props { label: "Hello".into() },
       ).render();

       // 3. Yield once so Yew's scheduler flushes the first render.
       //    Duration::ZERO is enough for synchronous components.
       //    If your component does async work in use_effect, you may need
       //    sleep(Duration::from_millis(50)).await instead.
       sleep(Duration::ZERO).await;

       // 4. Query the DOM and assert
       let el = mount.query_selector(".my-label").unwrap().unwrap();
       assert_eq!(el.text_content().unwrap(), "Hello");

       // 5. Clean up so subsequent tests start with a fresh DOM
       cleanup(&mount);
   }
   ```

4. **For integration tests** that use real Chrome fake devices, call
   `enumerate_fake_devices().await` at the start of the test. This requires the
   `webdriver.json` file to be present (it configures Chrome with
   `--use-fake-device-for-media-stream`).

## Chrome fake devices (`webdriver.json`)

The file `yew-ui/webdriver.json` configures `wasm-bindgen-test-runner` to launch
Chrome with `--use-fake-device-for-media-stream` and
`--use-fake-ui-for-media-stream`. This provides:

- At least 1 fake audioinput and 1 fake videoinput device
- Auto-granted permissions (no user prompts)
- Populated device labels

These flags only apply when running through `wasm-bindgen-test-runner`; they
have no effect on production builds.

## Mock vs real devices

| Use case | Tool | When to pick it |
|----------|------|-----------------|
| Control exact device count, IDs, labels | `create_mock_device` / `mock_mic` / etc. | Component tests — you need 0, 3, or 5 devices, or specific empty labels |
| Verify the full browser API pipeline | `enumerate_fake_devices` | Integration tests — proving that real `MediaDeviceInfo` objects render correctly |

If your test cares about *what the component does with specific input*, use
mocks. If it cares about *whether the browser API → component pipeline works
end-to-end*, use real fake devices.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Error: failed to spawn "" binary` | `CHROMEDRIVER` env var not set or chromedriver not in PATH | Use `make yew-tests` (auto-detects) or set `CHROMEDRIVER=/path/to/chromedriver` |
| `error: the wasm32-unknown-unknown target is not installed` | Missing WASM target | `rustup target add wasm32-unknown-unknown` |
| `it looks like the Rust project used to create this wasm file was linked against version of wasm-bindgen that uses a different bindgen format` | `wasm-bindgen-cli` version doesn't match `Cargo.lock` | `cargo install wasm-bindgen-cli --version 0.2.106 --locked` |
| Test hangs or times out | Component does async work that never resolves | Increase the `sleep` duration or check for missing `.await` in the component |
| `SessionNotCreated: Could not start a new session` | chromedriver version doesn't match Chrome | Install a chromedriver that matches your Chrome major version |

## CI

Tests are executed in CI via the `.github/workflows/wasm-test.yaml` workflow.
The CI job runs natively on `ubuntu-latest` — it installs `chromedriver` via
[`nanasess/setup-chromedriver`](https://github.com/nanasess/setup-chromedriver)
and `wasm-bindgen-cli`, then runs
`CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown`.
No Docker is involved in CI for UI tests.

## Future: Multi-browser E2E tests

The tests described above all run inside a **single** browser tab. They validate
that components render correctly and interact with browser APIs, but they cannot
test multi-participant scenarios such as:

- Two users joining the same meeting and seeing each other's video tiles
- Screen sharing appearing in a remote participant's view
- Connection/disconnection flows between peers

These scenarios require an **external test orchestrator** that controls multiple
browser contexts while the full backend stack (actix-api + NATS + PostgreSQL)
is running. The recommended approach is:

1. **Playwright** (TypeScript or Python) — supports multiple `BrowserContext`
   objects in a single test, has built-in WebRTC/media mocking, and runs
   headlessly in CI.
2. A `docker-compose` profile that boots the complete stack (backend + `trunk
   serve` for the UI).
3. Playwright tests navigate each browser context to the served UI, join a
   meeting room, and assert on cross-browser state (video tiles, connection
   indicators, etc.).

This work is **not yet implemented**. When it is, it will live in a separate
directory (e.g. `e2e/`) with its own `package.json` and Playwright config.
