# WebSocket Bot for videocall.rs

A high-performance WebSocket bot client built with Rust and Tokio for the [videocall.rs](https://videocall.rs) platform. This bot connects to a specified WebSocket endpoint and echoes messages from a designated user, making it ideal for testing, monitoring, or automating interaction with videocall.rs rooms.

## Features

- **Asynchronous architecture** using Tokio runtime
- **Configurable client scaling** with support for multiple concurrent connections
- **Flexible deployment** using environment variables or `.env` configuration
- **Minimal resource footprint** typical of Rust applications

## Quick Start

### Prerequisites

- Rust and Cargo installed
- A running videocall.rs server instance or other compatible WebSocket endpoint

### Configuration

Configure the bot using environment variables:

| Variable | Description | Example |
|----------|-------------|---------|
| `N_CLIENTS` | Number of concurrent bot clients to spawn | `1` |
| `ENDPOINT` | WebSocket server endpoint URL | `ws://localhost:3030` |
| `ROOM` | Room identifier to join | `redrum` |
| `ECHO_USER` | User ID whose messages will be echoed | `test` |

### Running the Bot

```bash
N_CLIENTS=1 ENDPOINT=ws://localhost:3030 ROOM=redrum ECHO_USER=test cargo run
```

### Using .env File

Create a `.env` file in the project root with the following content:

```
N_CLIENTS=1
ENDPOINT=ws://localhost:3030
ROOM=redrum
ECHO_USER=test
```

Then simply run:

```bash
cargo run
```

## Contributing

This bot is part of the [videocall.rs](https://github.com/security-union/videocall-rs) project, an open-source, high-performance video conferencing platform built with Rust. Contributions are welcome!

## License

MIT License - See the main project repository for details.
