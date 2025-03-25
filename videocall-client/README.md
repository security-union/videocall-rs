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

### Device access permission:

```rust
let media_device_access = MediaDeviceAccess::new();
media_device_access.on_granted = ...; // callback
media_device_access.on_denied = ...; // callback
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