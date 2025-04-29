# VideoCall Mobile Bindings

[![License: MIT/Apache-2.0](https://img.shields.io/badge/License-MIT%2FApache--2.0-blue.svg)](https://opensource.org/licenses/MIT)

Native mobile bindings for the VideoCall project, providing iOS (Swift) and Android (Kotlin) interfaces to the core WebTransport functionality.

## Overview

**Disclaimer:** This crate is currently intended for internal use within the VideoCall project. It is a work in progress, and API stability or long-term compatibility is not guaranteed at this stage.

This crate extends the VideoCall project by providing native bindings for iOS and Android platforms using the [UniFFI](https://github.com/mozilla/uniffi-rs) library. It allows mobile developers to easily integrate the VideoCall WebTransport functionality into their iOS and Android applications.

The bindings expose the core VideoCall functionality through platform-specific interfaces, handling the complexities of cross-language communication while maintaining the performance benefits of the Rust implementation.

## Features

- **Native Mobile Integration**: Seamless integration with iOS and Android applications
- **WebTransport Support**: Access to the VideoCall WebTransport functionality from mobile platforms
- **Datagram Management**: Efficient handling of datagrams for real-time data transmission
- **Error Handling**: Comprehensive error handling with detailed error messages
- **Thread-Safe**: Built with thread safety in mind for concurrent operations
- **Simple API**: Easy-to-use API for connecting, sending data, and managing connections

## Installation

### iOS

1. Add the XCFramework to your Xcode project
2. Import the Swift module in your code
3. Use the provided API to connect and send/receive data

```swift
import VideoCallKit

// Create a client
let client = WebTransportClient()

// Connect to a server
try client.connect(url: "https://example.com")

// Send data
try client.sendDatagram(data: [1, 2, 3, 4])

// Create a queue for receiving data
let queue = DatagramQueue()

// Subscribe to datagrams
try client.subscribeToDatagrams(queue: queue)

// Check if datagrams are available
if try queue.hasDatagrams() {
    // Receive a datagram
    let data = try queue.receiveDatagram()
    // Process the data
}
```

### Android

1. Add the AAR to your Android project
2. Import the Kotlin module in your code
3. Use the provided API to connect and send/receive data

```kotlin
import com.videocall.uniffi.WebTransportClient
import com.videocall.uniffi.DatagramQueue

// Create a client
val client = WebTransportClient()

// Connect to a server
client.connect("https://example.com")

// Send data
client.sendDatagram(byteArrayOf(1, 2, 3, 4))

// Create a queue for receiving data
val queue = DatagramQueue()

// Subscribe to datagrams
client.subscribeToDatagrams(queue)

// Check if datagrams are available
if (queue.hasDatagrams()) {
    // Receive a datagram
    val data = queue.receiveDatagram()
    // Process the data
}
```

## Building from Source

### Prerequisites

- Rust (latest stable)
- Android NDK (for Android builds)
- Xcode (for iOS builds)
- Java 17 (for Android builds)
- UniFFI CLI (`cargo install uniffi`)

### Building for iOS

```bash
cd videocall-sdk
./build_ios.sh
```

This will generate:
- An XCFramework at `VideoCallKit/Frameworks/VideoCallIOS.xcframework`
- Swift bindings at `VideoCallKit/Sources/VideoCallKit/videocall.swift`

### Building for Android

```bash
cd videocall-sdk
./build_android.sh
```

This will generate:
- An AAR file at `target/videocall-sdk.aar`
- Kotlin bindings at `target/kotlin/com/videocall/uniffi/videocall.kt`

## How It Works

This crate uses UniFFI to generate language-specific bindings for the VideoCall core functionality:

1. **Interface Definition**: The `videocall.udl` file defines the interface that will be exposed to Swift and Kotlin
2. **Code Generation**: UniFFI generates the necessary code to bridge between Rust and the target languages
3. **Build Process**: The build scripts compile the Rust code and generate the platform-specific artifacts

## Error Handling

The library provides comprehensive error handling through the `WebTransportError` enum:

- `ConnectionError`: Errors related to the connection
- `TlsError`: TLS-related errors
- `StreamError`: Errors related to data streams
- `InvalidUrl`: Invalid URL format
- `RuntimeError`: Runtime-related errors
- `CertificateError`: Certificate-related errors
- `ClientError`: Errors related to the client
- `QueueError`: Errors related to the datagram queue

## License

This project is licensed under either of the following licenses:

- MIT License ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
