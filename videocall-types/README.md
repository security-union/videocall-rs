
# videocall-types

`videocall-types` is a Rust library that provides the shared types and definitions for the [videocall.rs](https://videocall.rs) teleconferencing system. This crate contains essential data structures and type definitions used across the system, enabling seamless communication and interoperability between components.

## Features

- **Common Data Models**: Standardized types for messages, user sessions, room configurations, and more.
- **Serialization/Deserialization**: Implementations for  Protobuf to enable efficient data transfer (well better than JSON).
- **Type Safety**: Strongly-typed structures to reduce errors in communication.

## Usage

To use `videocall-types`, add it to your `Cargo.toml`:

```toml
[dependencies]
videocall-types = "0.1"
```

Then, import and use the types in your project:

```rust
use videocall_types::protos::{
    connection_packet::ConnectionPacket,
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};
```

## About `videocall.rs`

The `videocall.rs` system is an open-source, real-time teleconferencing platform built with Rust, WebTransport, and HTTP/3, designed for high-performance and low-latency communication.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for more details.
