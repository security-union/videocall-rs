# yew-ui Testing Guide

This document describes how to write and run component tests for the `yew-ui`
crate.

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

Tests are organised into a three-layer pyramid:

| Layer | Scope | Where | Runs In |
|-------|-------|-------|---------|
| **1 — Unit** | `MediaDeviceList` + `MockMediaDevicesProvider` | `videocall-client/src/media_devices/media_device_list.rs` | `wasm-bindgen-test` (headless Chrome) |
| **2 — Component** | Individual Yew components with mock data | `yew-ui/tests/device_selector.rs`, `yew-ui/tests/video_control_buttons.rs` | `wasm-bindgen-test` (headless Chrome) |
| **3 — Integration** | Full browser API → component rendering | `yew-ui/tests/device_integration.rs` | `wasm-bindgen-test` (headless Chrome with fake devices) |

## Quick start

### Native (requires Chrome + chromedriver)

```bash
make yew-tests              # headless
make yew-tests HEADED=1     # with visible browser window
```

### Docker (zero local deps)

```bash
make yew-tests-docker
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

3. **Write an async test:**

   ```rust
   #[wasm_bindgen_test]
   async fn my_component_renders_correctly() {
       let mount = create_mount_point();
       yew::Renderer::<MyComponent>::with_root(mount.clone()).render();
       sleep(Duration::ZERO).await;

       // Assert on the rendered DOM
       let el = mount.query_selector(".my-class").unwrap().unwrap();
       assert_eq!(el.text_content().unwrap(), "expected text");

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

- **Mock devices** (`create_mock_device`) are used in Layer 2 component tests
  where you need precise control over device count, labels, and IDs.
- **Real fake devices** (`enumerate_fake_devices`) are used in Layer 3
  integration tests to verify the full pipeline from browser API to rendered UI.

## CI

Tests are executed in CI via the `.github/workflows/wasm-test.yaml` workflow.
The CI job runs natively on `ubuntu-latest` — it installs `chromedriver` via
[`nanasess/setup-chromedriver`](https://github.com/nanasess/setup-chromedriver)
and `wasm-bindgen-cli`, then runs
`CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown`.
No Docker is involved in CI for UI tests.
