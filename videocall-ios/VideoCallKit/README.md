# VideoCallKit

A Swift package for WebTransport communication, built on top of a Rust implementation.

## Features

- WebTransport client for iOS and macOS
- Datagram-based communication
- Thread-safe queue for receiving datagrams
- Simple Swift API

## Requirements

- iOS 15.0+ / macOS 12.0+
- Swift 5.9+

## Installation

### Swift Package Manager

Add VideoCallKit to your project using Swift Package Manager:

1. In Xcode, select "File" > "Add Packages..."
2. Enter the repository URL: `https://github.com/yourusername/VideoCallKit.git`
3. Click "Add Package"

Or add it to your `Package.swift` file:

```swift
dependencies: [
    .package(url: "https://github.com/yourusername/VideoCallKit.git", from: "1.0.0")
]
```

## Usage

### Basic Usage

```swift
import VideoCallKit

// Create a client
let client = WebTransportClient()

// Connect to a server
try client.connect(url: "https://example.com")

// Send a text message
try client.sendTextMessage("Hello, WebTransport!")

// Start listening for datagrams
try client.startListening()

// Check for datagrams
if try client.hasDatagrams() {
    // Receive a text message
    if let message = try client.receiveTextMessage() {
        print("Received: \(message)")
    }
}

// Stop listening
try client.stopListening()
```

### Advanced Usage

```swift
import VideoCallKit

// Create a client
let client = WebTransportClient()

// Connect to a server
try client.connect(url: "https://example.com")

// Send binary data
let data = Data([0x01, 0x02, 0x03, 0x04])
try client.sendDatagram(data: data)

// Start listening for datagrams
try client.startListening()

// Poll for datagrams
Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { timer in
    do {
        while try client.hasDatagrams() {
            let data = try client.receiveDatagram()
            print("Received \(data.count) bytes")
        }
    } catch {
        print("Error: \(error)")
        timer.invalidate()
    }
}
```

## License

This project is licensed under the MIT License - see the LICENSE file for details. 