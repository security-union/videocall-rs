# videocall.rs Architecture

This document provides a comprehensive overview of the videocall.rs architecture, explaining how the various components interact to deliver a scalable, real-time video conferencing solution.

## Table of Contents

- [System Overview](#system-overview)
- [Key Components](#key-components)
- [Connection Flows](#connection-flows)
- [Message Handling](#message-handling)
- [Horizontal Scaling](#horizontal-scaling)
- [Media Processing](#media-processing)
- [Security Architecture](#security-architecture)

## System Overview

videocall.rs is designed as a distributed system with multiple specialized components that work together to provide real-time video conferencing. The architecture supports horizontal scaling through a pub/sub messaging system.

```mermaid
graph TD
    Clients[Clients<br>Browsers, Mobile, CLI] -->|WebSocket| ActixAPI[Actix API<br>WebSocket]
    Clients -->|WebTransport| WebTransportServer[WebTransport<br>Server]
    ActixAPI --> NATS[NATS<br>Messaging]
    WebTransportServer --> NATS
```

## Key Components

### 1. Client Applications

- **Web Client**: Built with Yew (Rust-to-WebAssembly framework)
- **CLI Client**: Native Rust client for headless devices
- **Mobile Clients**: Native mobile applications (in development)

### 2. Transport Servers

- **Actix API Server**: Handles WebSocket connections
  - Built with Actix Web framework
  - Manages session state and room coordination
  - Processes signaling messages

- **WebTransport Server**: Handles WebTransport connections
  - Uses QUIC protocol for faster, more reliable connections
  - Better performance for high-packet-loss environments
  - Requires Chrome/Chromium with WebTransport support

### 3. Messaging System

- **NATS**: High-performance message broker
  - Enables horizontal scaling of backend servers
  - Handles inter-server communication
  - Manages pub/sub for room events and signaling

## Connection Flows

### WebSocket Connection Flow

```mermaid
sequenceDiagram
    participant Client
    participant ActixAPI as Actix API
    participant NATS
    participant OtherServers as Other Servers
    
    Client->>ActixAPI: WebSocket Connect
    Client->>ActixAPI: Authentication
    ActixAPI-->>Client: Authentication Response
    ActixAPI->>NATS: Subscribe to room
    NATS->>OtherServers: Message broadcast
    Client->>ActixAPI: Media & Data
    ActixAPI-->>Client: Media & Data
```

### WebTransport Connection Flow

```mermaid
sequenceDiagram
    participant Client
    participant WebTransportServer as WebTransport Server
    participant NATS
    participant OtherServers as Other Servers
    
    Client->>WebTransportServer: HTTP/3 Handshake
    Client->>WebTransportServer: WebTransport Setup
    WebTransportServer-->>Client: WebTransport Setup Response
    Client->>WebTransportServer: Create Streams
    WebTransportServer->>NATS: Subscribe to room
    NATS->>OtherServers: Message broadcast
    Client->>WebTransportServer: Media & Data
    WebTransportServer-->>Client: Media & Data
```

### Message Flow

1. **Client Generates Message**: A client creates a message (e.g., chat message, video frame)
2. **Transport Layer**: Message is sent via WebSocket or WebTransport to the respective server
3. **Server Processing**: The server validates and processes the message
4. **NATS Publication**: The server publishes the message to the appropriate NATS subject
5. **Distribution**: All servers subscribed to that subject receive the message
6. **Client Delivery**: Servers forward the message to connected clients in the same room

## Horizontal Scaling

videocall.rs achieves horizontal scaling through its NATS-based architecture:

```mermaid
graph TB
    NATS((NATS<br>Messaging))
    
    Server1[Server 1<br>Actix] --> NATS
    Server2[Server 2<br>Actix] --> NATS
    Server3[Server 3<br>Actix] --> NATS
    
    NATS --> Server4[Server 4<br>WebTransport]
    NATS --> Server5[Server 5<br>WebTransport]
    NATS --> Server6[Server 6<br>WebTransport]
    
    classDef actix fill:#333,stroke:#666,color:white
    classDef webtransport fill:#222,stroke:#666,color:white
    classDef nats fill:#444,stroke:#888,stroke-width:1px,color:white
    
    class Server1,Server2,Server3 actix
    class Server4,Server5,Server6 webtransport
    class NATS nats
```

### Scaling Characteristics

1. **Client Distribution**: Clients can connect to any available server
2. **Room Coordination**: All servers in a room coordinate through NATS subjects
3. **Load Balancing**: Front-end load balancers distribute client connections
4. **Server Independence**: Servers can be added or removed without disrupting service
5. **Failover**: If a server fails, clients can reconnect to another server

## Media Processing

The media processing component handles the encoding and decoding of video streams. It supports various codecs and formats, including H.264, VP8, and VP9.

## Security Architecture

The security architecture ensures that the system is secure and protects against common security threats. It includes features such as encryption, authentication, and access control.
