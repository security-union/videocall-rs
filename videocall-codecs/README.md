# Rust Video Decoder Jitter Buffer

This project is a high-fidelity, cross-platform video decoder jitter buffer implemented in pure Rust. It is designed to mirror the core architectural concepts of the jitter buffer found in Google's `libwebrtc`, but with a simplified, modern, and idiomatic Rust API.

The primary goal of this crate is to provide a robust mechanism for receiving encoded video frames from an unreliable network, reordering them, buffering them to smooth out network jitter, and preparing them for a decoder in a continuous, monotonic sequence.

## Core Features

- **Jitter Buffering**: Correctly reorders out-of-order video frames based on their sequence number.
- **Adaptive Playout Delay**: Dynamically adjusts the buffering delay based on a running estimate of network jitter (calculated using an RFC 3550-style algorithm). This allows the buffer to be resilient to changing network conditions while keeping latency as low as possible.
- **Frame Loss Recovery**: Recovers from dropped packets by waiting for the next available keyframe and dropping any intermediate, now-unusable delta frames.
- **Cross-Platform by Design**:
    - **Native**: On desktop targets, the decoder runs in a dedicated `std::thread`, communicating with the jitter buffer via a high-performance MPSC channel.
    - **WebAssembly (WASM)**: For browser targets, the decoder is designed to run inside a **Web Worker**. It communicates with the main thread via `postMessage` and interfaces with the browser's built-in **WebCodecs API** for efficient, often hardware-accelerated, video decoding.
- **Extensive Test Suite**: A comprehensive set of unit tests validates the buffer's logic across a wide range of scenarios, including packet reordering, loss, and edge cases.

## Architecture

The system is split into two main components, decoupled by a trait abstraction:

1.  **`JitterBuffer`**: The heart of the crate. This component receives `FrameBuffer` objects, measures network jitter, manages the adaptive playout delay, and holds frames until they are ready to be decoded. It is completely agnostic to the underlying platform.

2.  **`Decoder` (via the `Decodable` trait)**: This component is responsible for the actual decoding of a frame. The `Decodable` trait provides a simple API (`new` and `decode`). This crate provides two implementations:
    - **`NativeDecoder`**: A wrapper around a `std::thread` for native builds.
    - **`WasmDecoder`**: A wrapper around a Web Worker that uses `wasm-bindgen` and `web-sys` to communicate with the browser's WebCodecs API.

This design ensures that the complex jitter buffer logic is written once and shared, while the platform-specific implementation details are cleanly encapsulated.

```mermaid
graph TD
    subgraph Your Application
        A[JitterBuffer]
    end

    subgraph "Decoder Abstraction (Trait)"
        B(Decodable<br/>trait)
    end

    subgraph "Native Implementation"
        C(NativeDecoder)
        D[std::thread]
        E[mpsc::channel]
        C -- spawns --> D
        C -- sends frames via --> E
        D -- receives from --> E
    end

    subgraph "WASM Implementation"
        F(WasmDecoder)
        G[Web Worker]
        H[WebCodecs API]
        F -- spawns --> G
        G -- calls --> H
        F -- sends frames via --> G
    end

    A -- owns --> B
    B -- implemented by --> C
    B -- implemented by --> F
```

## Usage

### In a Native Rust Project

Add this crate to your `Cargo.toml`:

```toml
[dependencies]
video-decoder = { path = "path/to/video-decoder" }
```

Then, instantiate and use the system:

```rust
use video_decoder::{
    decoder::{Decodable, Decoder},
    jitter_buffer::JitterBuffer
};
use std::sync::{Arc, Mutex};

// 1. Define a callback for when frames are decoded.
let on_decoded_frame = |frame| {
    println!("Render frame #{}", frame.sequence_number);
};

// 2. Create the platform-specific decoder.
let decoder = Decoder::new(Box::new(on_decoded_frame));

// 3. Create the JitterBuffer, giving it the decoder.
let jitter_buffer = Arc::new(Mutex::new(JitterBuffer::new(Box::new(decoder))));

// 4. In your network loop, insert frames as they arrive.
// let mut jb = jitter_buffer.lock().unwrap();
// jb.insert_frame(video_frame, arrival_time_ms);
```

### For the Web (WebAssembly)

This crate is configured for use with [`wasm-pack`](https://rustwasm.github.io/wasm-pack/).

1.  **Build the WASM modules**:

    ```bash
    wasm-pack build --target web --out-dir pkg -- --features wasm
    ```

    This command compiles both the main library and the worker binary, placing the output (`.wasm` and `.js` glue files) into a `pkg` directory.

2.  **Use in your Web App**:

    In your main application's JavaScript, you can now import and use the library. The `WasmDecoder` will automatically handle loading the worker.

    ```javascript
    import init, { JitterBuffer, Decoder } from './pkg/video_decoder.js';

    async function main() {
        await init();

        const onDecodedFrame = (frame) => {
            console.log("Render frame:", frame.sequence_number);
            // Here you would render the decoded frame data to a canvas
        };

        const decoder = Decoder.new(onDecodedFrame);
        const jitterBuffer = new JitterBuffer(decoder);

        // Feed frames into the jitter buffer from your network source (e.g., WebSocket)
        // jitterBuffer.insert_frame(videoFrame, arrivalTimeMs);
    }

    main();
    ```

## Running the Project

### Test Suite

To run the comprehensive test suite for the native target:

```bash
cargo test
```

### Simulation Binary

The project includes a terminal-based simulation that demonstrates the jitter buffer in action, complete with simulated packet loss and reordering.

To run the simulation:

```bash
cargo run --bin main
``` 