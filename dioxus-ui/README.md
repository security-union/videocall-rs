# dioxus-ui

Dioxus-based frontend for **videocall-rs**, a Rust video calling platform. This app compiles to WebAssembly and runs entirely in the browser, providing real-time video conferencing via WebTransport and WebSockets.

## Architecture

### Tech Stack

| Layer | Technology |
|---|---|
| UI Framework | [Dioxus 0.7](https://dioxuslabs.com/) (web target) |
| Language | Rust, compiled to `wasm32-unknown-unknown` |
| Build Tool | [Trunk](https://trunkrs.dev/) |
| Styling | Tailwind CSS + custom CSS |
| Media | WebRTC `getUserMedia`, WebCodecs, MediaStreamTrackProcessor |
| Transport | WebTransport (primary), WebSocket (fallback) |
| Audio | NetEq jitter buffer (separate WASM worker) |
| Video Decoding | WebCodecs decoder (separate WASM worker) |

### Crate Dependencies

```
dioxus-ui
  |- videocall-client      (framework-agnostic client library, no yew-compat feature)
  |- videocall-types        (protobuf message types)
  |- videocall-meeting-types (meeting API response types)
  |- videocall-meeting-client (meeting API HTTP client)
  |- videocall-diagnostics   (performance diagnostics)
  |- neteq                   (audio jitter buffer, web worker)
  |- videocall-codecs        (video decoder, web worker)
  |- matomo-logger           (analytics)
```

`videocall-client` is the core library shared with the yew-ui frontend. The dioxus-ui uses it **without** the `yew-compat` feature, meaning callbacks use `Rc<dyn Fn(T)>` closures instead of Yew's `Callback<T>`.

### Project Structure

```
dioxus-ui/
  src/
    main.rs              # Entry point: console_error_panic_hook, logging, launch
    lib.rs               # Library root for integration tests
    routing.rs           # Dioxus Router route definitions
    constants.rs         # Runtime config (window.__APP_CONFIG), feature flags
    context.rs           # Signal-based context providers (username, meeting time)
    auth.rs              # Authentication helpers
    meeting_api.rs       # Meeting API integration
    types.rs             # Shared types (DeviceInfo)
    components/
      attendants.rs      # Main meeting UI: video grid, controls, media access
      host.rs            # Camera/mic/screen encoder lifecycle management
      host_controls.rs   # Waiting room management (host-only)
      waiting_room.rs    # Waiting room polling (non-host participants)
      video_control_buttons.rs  # Mic, Camera, ScreenShare, HangUp buttons
      device_selector.rs        # Audio/video/speaker device dropdowns
      device_settings_modal.rs  # Modal wrapper for device selector
      login.rs           # OAuth login with provider-branded buttons
      google_sign_in_button.rs  # Google GSI Material Design button
      okta_sign_in_button.rs    # Okta branded button
      meetings_list.rs   # Active meetings list with CRUD
      meeting_ended_overlay.rs  # Post-meeting overlay
      meeting_info.rs    # Meeting ID display + copy to clipboard
      top_bar.rs         # Top navigation bar
      browser_compatibility.rs  # Browser feature detection
      config_error.rs    # Error display when __APP_CONFIG is missing
      call_timer.rs      # Meeting duration timer
      diagnostics.rs     # Real-time performance stats
      neteq_chart.rs     # Audio jitter buffer visualization
      canvas_generator.rs      # Canvas element management for peer video
      peer_tile.rs       # Individual peer video tile
      peer_list.rs       # Peer sidebar list
      peer_list_item.rs  # Single peer in the list
      icons/             # SVG icon components
    pages/
      home.rs            # Landing page: username + meeting ID form
      meeting.rs         # Meeting page: join flow, state machine, media setup
  tests/
    support/mod.rs               # Shared test harness
    context_unit.rs              # Username validation + localStorage
    video_control_buttons.rs     # Button rendering + CSS classes
    meeting_ended_overlay.rs     # Overlay rendering
    device_selector.rs           # Device dropdowns + settings modal
    device_integration.rs        # Real Chrome fake device integration
    home_integration.rs          # Home page rendering + auth states
    login_provider_logo.rs       # OAuth provider branding
  index.html           # Trunk entry point with WebCodecs polyfills
  webdriver.json       # Chrome flags for test device simulation
  static/              # CSS files (symlinked from yew-ui)
  scripts/             # Runtime config.js, Opus encoder/decoder workers
  assets/              # Audio files, images
```

### Application Flow

1. **App Launch** (`main.rs`): Sets up panic hook, logging, `UsernameCtx` context provider, and `Router<Route>`.

2. **Home Page** (`pages/home.rs`): User enters username and meeting ID. Validates input, saves username to localStorage, navigates to meeting route.

3. **Meeting Page** (`pages/meeting.rs`): State machine with stages:
   - `NotJoined` - Initial state, requests media device access
   - `Joining` - Connecting to the server
   - `Waiting` - In waiting room (non-host), polls for admission
   - `WaitingForMeeting` - Host waiting for meeting to start
   - `Admitted` - In the meeting, renders `AttendantsComponent`
   - `Rejected` / `Error` - Terminal states

4. **AttendantsComponent** (`components/attendants.rs`): The main meeting view. Manages:
   - Video grid layout with peer tiles
   - Media control buttons (mic, camera, screen share, hang up)
   - Device settings modal
   - `Host` component for encoder lifecycle
   - `HostControls` for waiting room management (host only)

5. **Host Component** (`components/host.rs`): Manages camera, microphone, and screen encoders from `videocall-client`. Reacts to prop changes in the component body (not `use_effect`) due to Dioxus 0.7's signal semantics.

### Runtime Configuration

The app reads `window.__APP_CONFIG` (injected by `scripts/config.js`) at startup:

```javascript
window.__APP_CONFIG = Object.freeze({
    apiBaseUrl: "https://api.example.com",
    wsUrl: "wss://api.example.com",
    webTransportHost: "https://wt.example.com:4433",
    oauthEnabled: "false",
    e2eeEnabled: "false",
    webTransportEnabled: "true",
    firefoxEnabled: "false",
    usersAllowedToStream: "",
    oauthProvider: "",           // "google", "okta", or ""
    serverElectionPeriodMs: 2000,
    audioBitrateKbps: 65,
    videoBitrateKbps: 100,
    screenBitrateKbps: 100,
});
```

## Development

### Prerequisites

- Rust stable with `wasm32-unknown-unknown` target
- [Trunk](https://trunkrs.dev/) (`cargo install trunk`)
- [tailwindcss](https://tailwindcss.com/blog/standalone-cli) standalone CLI
- [wasm-bindgen-cli](https://rustwasm.github.io/wasm-bindgen/) (`cargo install wasm-bindgen-cli`)

Or use the Nix devShell (includes all tools):

```bash
nix develop .#yew-ui
```

### Running Locally

```bash
cd dioxus-ui

# Generate tailwind CSS (run in background)
tailwindcss -i ./static/tailwind.css -o ./static/tailwind.css --watch --minify &

# Start the dev server
trunk serve --address 0.0.0.0 --port 3001
```

The app will be available at `http://localhost:3001`. Edit `scripts/config.js` to point to your backend API.

### Building for Production

```bash
cd dioxus-ui
tailwindcss -i ./static/tailwind.css -o ./static/tailwind.css --minify
trunk build --release
```

Output goes to `dioxus-ui/dist/`.

### Docker (Development)

```bash
docker compose -f docker/docker-compose.yaml up dioxus-ui
```

This mounts the source code and runs Trunk in watch mode.

## Testing

### Overview

The test suite uses [`wasm-bindgen-test`](https://rustwasm.github.io/wasm-bindgen/wasm-bindgen-test/index.html) to run integration tests inside a real browser (Chrome via ChromeDriver). Tests render actual Dioxus components into the DOM, then assert on the resulting HTML elements, CSS classes, and text content.

### Test Structure

Tests live in `dioxus-ui/tests/` and share a common harness in `tests/support/mod.rs`.

**Test pattern:**
1. Create a mount-point `<div>` attached to `<body>`
2. Render a Dioxus component into the div via `render_into()`
3. Yield to the renderer with `yield_now().await` (double `requestAnimationFrame`)
4. Query the DOM with `mount.query_selector()` and assert
5. Clean up the mount-point

**Test harness helpers** (`tests/support/mod.rs`):
- `create_mount_point()` / `cleanup()` - DOM lifecycle
- `render_into()` - Mounts a Dioxus component into an element
- `yield_now()` - Async yield for Dioxus to flush mutations
- `inject_app_config()` / `remove_app_config()` - Runtime config injection
- `mock_fetch_401()` / `mock_fetch_meetings_empty()` / `restore_fetch()` - Network mocking
- `mock_mic()` / `mock_camera()` / `mock_speaker()` - Synthetic `MediaDeviceInfo` objects
- `enumerate_fake_devices()` - Real Chrome fake device enumeration

### Test Suites

| File | Tests | Description |
|---|---|---|
| `context_unit.rs` | 5 | Username validation rules, localStorage round-trip |
| `video_control_buttons.rs` | 6 | MicButton, CameraButton, ScreenShareButton, HangUpButton rendering and CSS |
| `meeting_ended_overlay.rs` | 4 | Overlay heading, message, button, backdrop |
| `device_selector.rs` | 8 | DeviceSelector dropdowns, labels, preselect, empty states; DeviceSettingsModal visibility and close button |
| `device_integration.rs` | 3 | Real Chrome fake device enumeration, rendering with genuine MediaDeviceInfo, device ID verification |
| `home_integration.rs` | 4 | Home page rendering, sign-in prompt on 401, empty meetings on 200, ConfigError on missing config |
| `login_provider_logo.rs` | 4 | Google, Okta, generic, and unknown provider button branding |

**Total: 34 tests**

### Running Tests

**Prerequisites:**
- ChromeDriver matching your Chrome version
- `wasm-bindgen-cli` installed (`cargo install wasm-bindgen-cli`)

On macOS, install ChromeDriver and allow it through Gatekeeper:

```bash
brew install --cask chromedriver
xattr -d com.apple.quarantine "$(brew --prefix)/Caskroom/chromedriver/*/chromedriver-mac-arm64/chromedriver"
```

Then approve it in **System Settings > Privacy & Security** if prompted.

**Run all tests headless (CI mode):**

```bash
cd dioxus-ui
CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown
```

**Run all tests in a visible browser (useful for debugging):**

```bash
cd dioxus-ui
WASM_BINDGEN_TEST_NO_HEADLESS=1 CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown
```

**Run a single test suite:**

```bash
cd dioxus-ui
CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --test device_selector
```

**Run a single test by name:**

```bash
cd dioxus-ui
CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --test device_selector device_settings_modal_close_button_present
```

### Using Nix

If you use the Nix devShell, all tooling is pre-installed:

```bash
nix develop .#yew-ui
cd dioxus-ui
CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown
```

### How Device Integration Tests Work

The `device_integration.rs` tests use real Chrome APIs with fake devices. The `webdriver.json` file configures Chrome with:

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

This tells Chrome to:
- Provide synthetic audio/video devices (no real hardware needed)
- Auto-grant media permissions (no user prompt)

The tests call `navigator.mediaDevices.getUserMedia()` and `enumerateDevices()` to get genuine `MediaDeviceInfo` objects, then render them through `DeviceSelector` to verify the full pipeline.

### CI Pipeline

Tests run automatically in GitHub Actions (`.github/workflows/wasm-test.yaml`) on pushes to `main` and pull requests touching `dioxus-ui/**` or shared crate paths. The CI job:

1. Checks out the repo
2. Installs Nix and sets up the devShell
3. Caches cargo dependencies
4. Installs ChromeDriver
5. Runs `cargo test --target wasm32-unknown-unknown` in headless mode

### Writing New Tests

1. Create a new file in `tests/` (e.g., `tests/my_component.rs`)
2. Add the standard preamble:

```rust
#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::my_component::MyComponent;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);
```

3. Write async test functions:

```rust
#[wasm_bindgen_test]
async fn my_component_renders_correctly() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! { MyComponent { prop: "value".to_string() } }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let text = mount.text_content().unwrap_or_default();
    assert!(text.contains("expected text"));

    cleanup(&mount);
}
```

Key points:
- The `wrapper` function must be `fn() -> Element` (not a closure) because `render_into` requires a function pointer.
- Use `yield_now().await` after `render_into` to let Dioxus flush its mutations.
- For components that do async work (fetch, timers), add extra yields with `setTimeout` delays.
- Always call `cleanup(&mount)` to prevent DOM leaks between tests.
- Mock external dependencies (fetch, config) and restore them in cleanup.
