# Dioxus-UI Test Framework

Integration and unit tests for the `dioxus-ui` crate, ported from the `yew-ui` test suite and adapted for the Dioxus rendering model.

## Overview

| Metric | Value |
|--------|-------|
| **Total tests** | 25 |
| **Test files** | 5 |
| **Support module** | `tests/support/mod.rs` (15 public helpers) |
| **Runner** | `wasm-bindgen-test` in headless Chrome |
| **Components covered** | MicButton, CameraButton, ScreenShareButton, HangUpButton, PeerListButton, MeetingEndedOverlay, DeviceSelector |
| **Pure functions covered** | `is_valid_username`, `MeetingHost::is_host` |

## Running the Tests

```bash
cd dioxus-ui
wasm-pack test --headless --chrome
```

Or via the project Makefile:

```bash
make dioxus-tests-docker
```

## Architecture

### How Dioxus Component Tests Work

Each test follows a five-step pattern:

1. **Mount** — Create a `<div>` and attach it to `<body>`.
2. **Render** — Call `dioxus::web::launch_cfg(wrapper_fn, (), Config::new().rootelement(div))`.
3. **Yield** — Await `gloo_timers::future::TimeoutFuture::new(0)` so the Dioxus scheduler flushes the render.
4. **Assert** — Query the real DOM with `query_selector` and check elements, classes, text, and attributes.
5. **Cleanup** — Remove the mount `<div>` from `<body>`.

### Dioxus vs Yew Test Differences

| Aspect | yew-ui | dioxus-ui |
|--------|--------|-----------|
| Mount component | `Renderer::<W>::with_root(el).render()` | `dioxus::web::launch_cfg(wrapper, (), Config::new().rootelement(el))` |
| Yield to scheduler | `yew::platform::time::sleep(Duration::ZERO).await` | `gloo_timers::future::TimeoutFuture::new(0).await` |
| Wrapper component | `#[function_component] fn w() -> Html { html!{} }` | `fn w() -> Element { rsx!{} }` |
| Passing props | `with_root_and_props(el, props)` | `thread_local!` + read inside wrapper fn |
| No-op handler | `Callback::noop()` | `move \|_\| {}` |

### Passing Props via `thread_local!`

Dioxus's `launch_cfg` takes a bare `fn() -> Element` — there is no `with_root_and_props` equivalent. Tests that need dynamic data use the `thread_local!` pattern:

```rust
thread_local! {
    static TEST_DATA: RefCell<Vec<MediaDeviceInfo>> = RefCell::new(Vec::new());
}

// Set before rendering
TEST_DATA.with(|d| *d.borrow_mut() = my_devices);

// Read inside the wrapper
fn wrapper() -> Element {
    let devices = TEST_DATA.with(|d| d.borrow().clone());
    rsx! { DeviceSelector { microphones: devices, /* ... */ } }
}
```

Tests with constant props (e.g. `MicButton { enabled: true }`) skip `thread_local!` entirely and hardcode values in the wrapper.

---

## Test Layers

### Layer 1 — Unit Tests (no DOM)

Pure-function tests that run in WASM but do not render any components.

### Layer 2 — Component Tests (mock devices)

Render individual components with **constructed** `MediaDeviceInfo` mock objects. This gives full control over device count, labels, and IDs without depending on browser hardware.

### Layer 3 — Integration Tests (real Chrome fake devices)

Use Chrome's `--use-fake-device-for-media-stream` flag (configured in `webdriver.json`) to enumerate **real** `MediaDeviceInfo` objects from the browser, then render components with those devices.

---

## File Reference

### `tests/support/mod.rs` — Shared Test Harness (275 lines)

Reusable helpers included by each test file via `mod support;`.

| Function | Category | Description |
|----------|----------|-------------|
| `create_mount_point()` | DOM | Creates a `<div>`, attaches to `<body>`, returns it |
| `cleanup(mount)` | DOM | Removes mount-point from `<body>` |
| `yield_now()` | Dioxus | Yields to WASM scheduler to flush pending renders |
| `mount_dioxus(app, mount)` | Dioxus | Mounts a component into an element and yields |
| `inject_app_config()` | Config | Sets `window.__APP_CONFIG` (OAuth disabled) |
| `inject_app_config_with_provider(p)` | Config | Sets `window.__APP_CONFIG` (OAuth enabled, given provider) |
| `remove_app_config()` | Config | Deletes `window.__APP_CONFIG` |
| `mock_fetch_401()` | Fetch | Overrides `window.fetch` to return 401 |
| `mock_fetch_meetings_empty()` | Fetch | Overrides `window.fetch` to return empty meetings |
| `restore_fetch()` | Fetch | Restores original `window.fetch` |
| `create_mock_device(id, kind, label)` | Mock | Builds a `MediaDeviceInfo`-compatible JS object |
| `mock_mic(id, label)` | Mock | Shortcut for `create_mock_device` with `audioinput` |
| `mock_camera(id, label)` | Mock | Shortcut for `create_mock_device` with `videoinput` |
| `mock_speaker(id, label)` | Mock | Shortcut for `create_mock_device` with `audiooutput` |
| `enumerate_fake_devices()` | Chrome | Calls `getUserMedia` + `enumerateDevices` for real fake devices |

---

### `tests/video_control_buttons.rs` — 7 Tests

Component tests for the video call toolbar buttons.

| Test | Component | Asserts |
|------|-----------|---------|
| `mic_button_enabled_shows_mute_tooltip` | `MicButton` | Tooltip text is "Mute"; button has `active` class |
| `mic_button_disabled_shows_unmute_tooltip` | `MicButton` | Tooltip text is "Unmute"; button lacks `active` class |
| `camera_button_enabled_shows_stop_video_tooltip` | `CameraButton` | Tooltip text is "Stop Video" |
| `camera_button_disabled_shows_start_video_tooltip` | `CameraButton` | Tooltip text is "Start Video" |
| `screen_share_button_disabled_prop_renders_disabled_attribute` | `ScreenShareButton` | `disabled` HTML attribute set; `disabled` CSS class present |
| `hang_up_button_has_danger_class` | `HangUpButton` | Has `danger` + `video-control-button` classes; tooltip is "Hang Up" |
| `peer_list_button_open_shows_close_peers` | `PeerListButton` | Tooltip text is "Close Peers"; button has `active` class |

---

### `tests/meeting_ended_overlay.rs` — 4 Tests

Component tests for the meeting-ended full-screen overlay.

| Test | Asserts |
|------|---------|
| `overlay_renders_message_and_heading` | Contains "Meeting Ended" heading and the message prop text |
| `overlay_has_return_home_button` | `.meeting-ended-home-btn` button exists with text "Return to Home" |
| `overlay_has_glass_backdrop` | Root element has `.glass-backdrop.meeting-ended-overlay` classes |
| `overlay_displays_custom_message` | `.meeting-ended-message` element contains the custom message string |

---

### `tests/device_selector.rs` — 7 Tests

Component rendering tests for `DeviceSelector` using mock devices.

| Test | Layer | Asserts |
|------|-------|---------|
| `device_selector_renders_all_three_dropdowns` | 2 | `#audio-select`, `#video-select`, `#speaker-select` are present |
| `device_selector_renders_multiple_device_labels` | 2 | Audio dropdown has 3 options with correct label text in order |
| `device_selector_preselects_correct_device` | 2 | `option[value='m2']` has `selected` attribute |
| `device_selector_empty_list_renders_empty_dropdown` | 2 | Audio dropdown has 0 options |
| `device_selector_empty_labels_render_empty_option_text` | 2 | Option text is empty when device label is empty |
| `device_selector_onchange_fires_microphone_callback` | 2 | Programmatic `change` event fires `on_microphone_select` with correct `device_id` |

---

### `tests/device_integration.rs` — 3 Tests

Integration tests using Chrome's real fake-device infrastructure.

| Test | Layer | Asserts |
|------|-------|---------|
| `enumerate_real_fake_devices_returns_labeled_devices` | 3 | At least one mic and one camera enumerated with non-empty labels |
| `device_selector_renders_real_fake_devices` | 3 | Audio and video dropdowns populated with real fake device labels |
| `device_selector_real_device_ids_match_option_values` | 3 | Each `<option>` value matches the corresponding `MediaDeviceInfo.device_id` |

---

### `tests/context_unit.rs` — 5 Tests

Pure unit tests for functions in `src/context.rs`. No DOM rendering.

| Test | Function Under Test | Asserts |
|------|---------------------|---------|
| `valid_username_alphanumeric` | `is_valid_username` | `"alice123"` is valid |
| `valid_username_underscore` | `is_valid_username` | `"my_name"` is valid |
| `invalid_username_empty` | `is_valid_username` | `""` is invalid |
| `invalid_username_spaces` | `is_valid_username` | `"has space"` is invalid |
| `meeting_host_is_host` | `MeetingHost::is_host` | Correct host returns true; wrong email and `None` return false |

---

## Dev-Dependencies

Added to `Cargo.toml` for test support:

```toml
[dev-dependencies]
wasm-bindgen-test = "0.3.37"
wasm-bindgen-futures = { workspace = true }
gloo-utils = "0.1"
gloo-timers = { version = "0.2.6", features = ["futures"] }
js-sys = "0.3.72"
web-sys = { version = "0.3.72", features = [
    "HtmlButtonElement",
    "HtmlOptionElement",
    "HtmlSelectElement",
    "DomTokenList",
] }
```

## WebDriver Configuration

`webdriver.json` configures Chrome for headless testing with fake media devices:

```json
{
  "goog:chromeOptions": {
    "args": [
      "--use-fake-device-for-media-stream",
      "--use-fake-ui-for-media-stream"
    ]
  }
}
```

These flags allow the Layer 3 integration tests to call `getUserMedia` and `enumerateDevices` without real hardware or user-facing permission prompts.
