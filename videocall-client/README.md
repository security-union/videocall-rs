# videocall-client

A Rust client library for the [videocall.rs](https://videocall.rs) project that handles client-side browser media I/O for video calls.

This crate provides:
- Media encoding for local camera, microphone, and screen sharing
- Rendering of remote peers' media
- Media device enumeration and access management

Currently supports Chromium based browsers only, including Chrome, Edge, and Brave.

[GitHub Repository](https://github.com/videocall-rs/videocall-rs)

## Quick Start

### Client creation and connection:
```rust
let options = VideoCallClientOptions {...}; // set parameters and callbacks for various events
let client = VideoCallClient::new(options);

client.connect();
```

### Encoder creation:
```rust
let camera = CameraEncoder.new(client, video_element_id);
let microphone = MicrophoneEncoder.new(client);
let screen = ScreenEncoder.new(client);

camera.select(video_device);
camera.start();
camera.stop();
microphone.select(video_device);
microphone.start();
microphone.stop();
screen.start();
screen.stop();
```

### Host page requirement: the microphone encoder worker

The microphone is encoded to Opus (pure Rust, ropus) in a Web Worker built
from this crate's `audio_encoder_worker` bin. The host page has to build it
and point to it with a `<link id="audio-encoder-worker">`. With trunk:

```html
<link data-trunk rel="rust" href="path/to/videocall-client/Cargo.toml"
      data-bin="audio_encoder_worker" data-type="worker" data-loader-shim />
<link id="audio-encoder-worker" href="/audio_encoder_worker_loader.js" />
```

Without it, starting the microphone reports an error through the encoder's
`on_error` callback. [dioxus-ui's index.html](https://github.com/videocall-rs/videocall-rs/blob/main/dioxus-ui/index.html) is a
working example; it also builds the worker with a size-tuned cargo profile
(`audio-worker-release` in the workspace `Cargo.toml`).

### Device access permission:

```rust
let media_device_access = MediaDeviceAccess::new();
media_device_access.on_result = ...; // callback with audio and video state
media_device_access.request();
```

### Device query and listing:
```rust
let media_device_list = MediaDeviceList::new();
media_device_list.audio_inputs.on_selected = ...; // callback
media_device_access.video_inputs.on_selected = ...; // callback

media_device_list.load();

let microphones = media_device_list.audio_inputs.devices();
let cameras = media_device_list.video_inputs.devices();
media_device_list.audio_inputs.select(&microphones[i].device_id);
media_device_list.video_inputs.select(&cameras[i].device_id);
```