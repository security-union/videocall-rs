# videocall.rs

[![GitHub Stars](https://img.shields.io/github/stars/security-union/videocall-rs?style=social)](https://github.com/security-union/videocall-rs)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Discord](https://img.shields.io/discord/1234567890?color=7289DA&label=Discord&logo=discord&logoColor=white)](https://discord.gg/JP38NRe4CJ)

An open-source, high-performance video conferencing platform built with Rust, providing real-time communication with low latency and end-to-end encryption.

**[Website](https://videocall.rs)** | **[Documentation](https://docs.videocall.rs)** | **[Discord Community](https://discord.gg/JP38NRe4CJ)**

## Table of Contents

- [Overview](#overview)
- [Features](#features)
- [System Architecture](#system-architecture)
- [Getting Started](#getting-started)
  - [Prerequisites](#prerequisites)
  - [Docker Setup](#docker-setup)
  - [Manual Setup](#manual-setup)
- [Usage](#usage)
- [Performance](#performance)
- [Security](#security)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [Project Structure](#project-structure)
- [Demos and Media](#demos-and-media)
- [Contributors](#contributors)
- [License](#license)

## Overview

videocall.rs is a modern, open-source video conferencing system written entirely in Rust, designed for developers who need reliable, scalable, and secure real-time communication capabilities. It provides a foundation for building custom video communication solutions, with support for both browser-based and native clients.

**Project Status:** Beta - Actively developed and suitable for non-critical production use

## Features

- **High Performance:** Built with Rust for optimal resource utilization and low latency
- **Multiple Transport Protocols:** Support for WebSockets and WebTransport 
- **End-to-End Encryption (E2EE):** Optional secure communications between peers
- **Scalable Architecture:** Designed with a pub/sub model using NATS for horizontal scaling
- **Cross-Platform Support:** Works on major browsers (Chrome/Chromium, with Safari support in development)
- **Native Client Support:** CLI tool for headless video streaming from devices like Raspberry Pi
- **Open Source:** MIT licensed for maximum flexibility

## System Architecture

videocall.rs follows a microservices architecture with these primary components:

1. **actix-api:** Rust-based backend server using Actix Web framework
2. **yew-ui:** Web frontend built with the Yew framework and compiled to WebAssembly
3. **videocall-types:** Shared data types and protocol definitions
4. **videocall-client:** Client library for native integration
5. **videocall-cli:** Command-line interface for headless video streaming
6. **videocall-daemon:** Background service for system integration

![Architecture Diagram](https://videocall.rs/architecture.png)

## Getting Started

### Prerequisites

- Modern Linux distribution, macOS, or Windows 10/11
- [Docker](https://docs.docker.com/engine/install/) and Docker Compose (for containerized setup)
- [Rust toolchain](https://rustup.rs/) 1.70+ (for manual setup)
- Chrome/Chromium browser for frontend access

### Docker Setup

The quickest way to get started is with our Docker-based setup:

1. Clone the repository:
   ```
   git clone https://github.com/security-union/videocall-rs.git
   cd videocall-rs
   ```

2. Start the server (replace `<server-ip>` with your machine's IP address):
   ```
   ACTIX_UI_BACKEND_URL=ws://<server-ip>:8080 make up
   ```

3. Open Chrome using the provided script for local WebTransport:
   ```
   ./launch_chrome.sh
   ```

4. Access the application at:
   ```
   http://<server-ip>/meeting/<username>/<meeting-id>
   ```

### Manual Setup

For development or custom deployments:

1. Create a PostgreSQL database:
   ```
   createdb actix-api-db
   ```

2. Install required tools:
   ```
   # Install NATS server
   curl -L https://github.com/nats-io/nats-server/releases/download/v2.9.8/nats-server-v2.9.8-linux-amd64.tar.gz | tar xz
   sudo mv nats-server-v2.9.8-linux-amd64/nats-server /usr/local/bin
   
   # Install trurl
   cargo install trurl
   ```

3. Start the development environment:
   ```
   ./start_dev.sh
   ```

4. Connect to:
   ```
   http://localhost:8081/meeting/<username>/<meeting-id>
   ```

For detailed configuration options, see our [setup documentation](https://docs.videocall.rs/setup).

## Usage

### Browser-Based Clients

1. Navigate to your deployed instance or localhost setup:
   ```
   http://<server-address>/meeting/<username>/<meeting-id>
   ```

2. Grant camera and microphone permissions when prompted

3. Click "Connect" to join the meeting

### CLI-Based Streaming

For headless devices like Raspberry Pi:

```bash
# Install the CLI tool
cargo install videocall-cli

# Stream from a camera
videocall-cli stream \
  --user-id <your-user-id> \
  --video-device-index 0 \
  --meeting-id <meeting-id> \
  --resolution 1280x720 \
  --fps 30 \
  --frame-format NV12 \
  --bitrate-kbps 500
```

For more usage examples, see our [usage documentation](https://docs.videocall.rs/usage).

## Performance

videocall.rs has been benchmarked and optimized for the following scenarios:

- **1-on-1 Calls:** Minimal resource utilization with <100ms latency on typical connections
- **Small Groups (3-10):** Efficient mesh topology with adaptive quality based on network conditions
- **Large Conferences:** Tested with up to 1000 participants using selective forwarding architecture

Performance metrics and tuning guidelines are available in our [performance documentation](https://docs.videocall.rs/performance).

## Security

Security is a core focus of videocall.rs:

- **Transport Security:** All communications use TLS/HTTPS
- **End-to-End Encryption:** Optional E2EE between peers with no server access to content
- **Authentication:** Flexible integration with identity providers
- **Access Controls:** Fine-grained permission system for meeting rooms

For details on our security model and best practices, see our [security documentation](https://docs.videocall.rs/security).

## Roadmap

| Version | Target Date | Key Features |
|---------|------------|--------------|
| 0.5.0   | Q2 2023    | ✅ End-to-End Encryption |
| 0.6.0   | Q3 2023    | ✅ Safari Browser Support |
| 0.7.0   | Q4 2023    | ✅ Native Mobile SDKs |
| 0.8.0   | Q1 2024    | 🔄 Screen Sharing Improvements |
| 1.0.0   | Q2 2024    | 🔄 Production Release with Full API Stability |

See our [detailed roadmap](https://github.com/security-union/videocall-rs/issues?q=is%3Aopen+is%3Aissue+label%3Aroadmap) for more information.

## Contributing

We welcome contributions from the community! Here's how to get involved:

1. **Issues:** Report bugs or suggest features via [GitHub Issues](https://github.com/security-union/videocall-rs/issues)

2. **Pull Requests:** Submit PRs for bug fixes or enhancements

3. **RFC Process:** For significant changes, participate in our [RFC process](/rfc)

4. **Community:** Join our [Discord server](https://discord.gg/JP38NRe4CJ) to discuss development

See our [Contributing Guidelines](CONTRIBUTING.md) for more detailed information.

## Project Structure

```
videocall-rs/
├── actix-api/        # Backend server implementation
├── yew-ui/           # Web frontend (Yew/WebAssembly)
├── videocall-types/  # Shared type definitions
├── videocall-client/ # Client library
├── videocall-cli/    # Command-line interface
├── videocall-daemon/ # System service
├── protobuf/         # Protocol buffer definitions
└── rfc/              # Request for Comments process
```

## Demos and Media

### Technical Presentations

- [Scaling to 1000 Users Per Call](https://youtu.be/LWwOSZJwEJI)
- [Initial Proof of Concept (2022)](https://www.youtube.com/watch?v=kZ9isFw1TQ8)

### Channels

- [YouTube Channel](https://www.youtube.com/@securityunion)
- [Developer Blog](https://blog.videocall.rs)

## Contributors

<table>
<tr>
<td align="center"><a href="https://github.com/darioalessandro"><img src="https://avatars0.githubusercontent.com/u/1176339?s=400&v=4" width="100" alt=""/><br /><sub><b>Dario Lencina</b></sub></a></td>
<td align="center"><a href="https://github.com/griffobeid"><img src="https://avatars1.githubusercontent.com/u/12220672?s=400&u=639c5cafe1c504ee9c68ad3a5e09d1b2c186462c&v=4" width="100" alt=""/><br /><sub><b>Griffin Obeid</b></sub></a></td>    
<td align="center"><a href="https://github.com/ronen"><img src="https://avatars.githubusercontent.com/u/125620?v=4" width="100" alt=""/><br /><sub><b>Ronen Barzel</b></sub></a></td>
<td align="center"><a href="https://github.com/leon3s"><img src="https://avatars.githubusercontent.com/u/7750950?v=4" width="100" alt=""/><br /><sub><b>Leone</b></sub></a></td>
<td align="center"><a href="https://github.com/JasterV"><img src="https://avatars3.githubusercontent.com/u/49537445?v=4" width="100" alt=""/><br /><sub><b>Victor Martínez</b></sub></a></td>
</tr>
</table>

Special thanks to [JasterV](https://github.com/JasterV) for the Actix websocket implementation which contains fragments from the [chat-rooms-actix](https://github.com/JasterV/chat-rooms-actix) project.

## License

This project is licensed under the MIT License - see the [LICENSE.md](LICENSE.md) file for details.
