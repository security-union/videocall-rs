# Design: WebTransport Actor Consolidation

## Overview

This document describes the refactoring of the WebTransport server to use the Actix actor model, mirroring the WebSocket implementation architecture.

## Problem

### Two Different Architectures

| Transport | Architecture | Message Routing |
|-----------|--------------|-----------------|
| **WebSocket** | `WsChatSession` actor → `ChatServer` actor | Clean actor lifecycle |
| **WebTransport** | Direct async tasks (no actors) | Bypassed `ChatServer` |

### Issues

1. **Architectural Inconsistency** - WebSocket uses actors, WebTransport didn't
2. **Code Duplication** - Session lifecycle logic duplicated
3. **Maintenance Burden** - Two patterns to understand and maintain

## Solution

### Keep Separate Binaries

Two server binaries remain separate:
- `websocket_server` - HTTP/WebSocket on port 8080
- `webtransport_server` - QUIC/WebTransport on port 4433

Multiple `ChatServer` instances work correctly because NATS handles cross-instance routing.

### Architecture

```
┌─────────────────────────────────────────────────┐
│ Quinn Server (NOT an actor)                     │
│   - Accepts QUIC/WebTransport connections       │
└─────────────────────┬───────────────────────────┘
                      │ spawns WebTransportBridge
                      ▼
┌─────────────────────────────────────────────────┐
│ WtChatSession (Actor)                           │
│   - One per connection                          │
│   - Owns SessionLogic for business logic        │
│   - Uses channels for I/O with quinn            │
└─────────────────────┬───────────────────────────┘
                      │
                      ▼
┌─────────────────────────────────────────────────┐
│ ChatServer (Actor)                              │
│   - Same as WebSocket implementation            │
│   - Tracks sessions, routes via NATS            │
└─────────────────────────────────────────────────┘
```

### WebTransport I/O Bridge

Quinn uses pure tokio async; actors use Actix's LocalSet. `WebTransportBridge` bridges them:

```
UniStream Reader ─┐
                  ├──→ WtChatSession Actor ──→ outbound channel ──→ Writer Task
Datagram Reader ──┘
```

| Channel | Reliability | Use Case |
|---------|-------------|----------|
| **UniStreams** | Reliable, ordered | Most packets (media, control) |
| **Datagrams** | Unreliable | Low-latency (RTT echo, keep-alive) |

## Final Architecture

### Directory Structure

```
actors/
  chat_server.rs           # Room management, NATS routing
  session_logic.rs         # Shared business logic
  packet_handler.rs        # Packet classification
  transports/
    mod.rs
    ws_chat_session.rs     # WebSocket adapter
    wt_chat_session.rs     # WebTransport adapter

webtransport/
  mod.rs                   # Server, connection handling
  bridge.rs                # Quinn ↔ Actor I/O bridge
```

### The Abstraction

```
                    ┌─────────────────────────────────────┐
                    │           SessionLogic              │
                    │    (transport-agnostic logic)       │
                    │  • handle_inbound() → InboundAction │
                    │  • handle_outbound() → bytes        │
                    │  • track_connection_start/end()     │
                    │  • on_stopping()                    │
                    └──────────────────┬──────────────────┘
                                       │ owns
                      ┌────────────────┴────────────────┐
                      ▼                                 ▼
            ┌─────────────────┐               ┌─────────────────┐
            │  WsChatSession  │               │  WtChatSession  │
            │  (thin adapter) │               │  (thin adapter) │
            │  ctx.binary()   │               │  tx.send()      │
            └────────┬────────┘               └────────┬────────┘
                     └──────────────┬───────────────────┘
                                    ▼
                      ┌─────────────────────────────────┐
                      │          ChatServer             │
                      └─────────────────────────────────┘
```

## Adding a New Transport

1. **Create adapter** in `actors/transports/new_transport_session.rs`
2. **Own a `SessionLogic`** instance - gets all business logic for free
3. **Implement `Actor` trait** - call `logic.track_connection_start()`, `logic.on_stopping()`
4. **Handle inbound** - call `logic.handle_inbound()`, act on returned `InboundAction`
5. **Handle outbound** - call `logic.handle_outbound()`, send via your transport
6. **Export** from `transports/mod.rs`

### What SessionLogic Provides

| Feature | Provided |
|---------|----------|
| Session ID generation | ✅ |
| Connection tracking (metrics) | ✅ |
| RTT packet echo logic | ✅ |
| Health packet processing | ✅ |
| Packet classification | ✅ |
| Meeting lifecycle (start/end packets) | ✅ |
| ChatServer integration | ✅ |
| Cleanup on disconnect | ✅ |

### What You Implement

| Feature | Your Code |
|---------|-----------|
| Transport I/O (read/write) | ✅ |
| Keep-alive mechanism | ✅ |
| Transport-specific message types | ✅ |

## Transport Differences (By Design)

| Aspect | WebSocket | WebTransport |
|--------|-----------|--------------|
| I/O Model | `WebsocketContext` | `mpsc` channels |
| Keep-alive | WS ping/pong frames | Custom datagram ping |
| Binary send | `ctx.binary(bytes)` | `tx.send(WtOutbound)` |

These differences are inherent to the protocols and should not be consolidated.
