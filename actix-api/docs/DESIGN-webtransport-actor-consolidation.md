# Design: WebTransport Actor Consolidation

## Overview

This document describes the plan to refactor the WebTransport server to use the actix actor model, mirroring the WebSocket implementation architecture.

## Current State

### Two Different Architectures

**WebSocket Server** (`bin/websocket_server.rs`):
- Uses `HttpServer` (actix-web) for connections
- `WsChatSession` actor handles individual sessions
- `ChatServer` actor coordinates rooms and routes messages via NATS
- Clean actor-based lifecycle management

**WebTransport Server** (`bin/webtransport_server.rs`):
- Uses `web_transport_quinn` (quinn) for QUIC/H3 connections
- Handles sessions directly with async tasks (no actors)
- Bypasses `ChatServer` entirely
- Direct NATS pub/sub for message routing

### Problems

1. **Architectural Inconsistency**: WebSocket uses actors, WebTransport doesn't
2. **Code Duplication**: Session lifecycle logic duplicated in both implementations
3. **Divergent Patterns**: Features added to one transport may not exist in the other
4. **Maintenance Burden**: Two different patterns to understand and maintain

## Proposed Solution

### Keep Separate Binaries

We will maintain two separate server binaries:
- `websocket_server` - HTTP/WebSocket on port 8080
- `webtransport_server` - QUIC/WebTransport on port 4433

**Rationale**: Multiple ChatServer instances work correctly because NATS handles cross-instance routing. This is already proven by horizontal scaling of WebSocket servers.

### Make WebTransport Use Actors

Create `WtChatSession` actor that mirrors `WsChatSession`:

```
┌─────────────────────────────────────────────────┐
│ Quinn Server (web_transport_quinn, NOT an actor)│
│   - Accepts QUIC/WebTransport connections       │
└─────────────────────┬───────────────────────────┘
                      │ spawns on each connection
                      ▼
┌─────────────────────────────────────────────────┐
│ WtChatSession (Actor) ← NEW                     │
│   - One per WebTransport connection             │
│   - Handles messages for that session           │
│   - Uses channels for I/O with quinn Session    │
│   - Communicates with ChatServer                │
└─────────────────────┬───────────────────────────┘
                      │ Connect/JoinRoom/ClientMessage
                      ▼
┌─────────────────────────────────────────────────┐
│ ChatServer (Actor)                              │
│   - Same implementation as WebSocket            │
│   - Tracks sessions, routes via NATS            │
└─────────────────────────────────────────────────┘
```

## Implementation Details

### New File: `actors/wt_chat_session.rs`

```rust
pub struct WtChatSession {
    pub id: SessionId,
    pub room: RoomId,
    pub email: Email,
    pub addr: Addr<ChatServer>,
    pub heartbeat: Instant,
    
    // Channels to send data back to WebTransport session
    pub unistream_tx: mpsc::Sender<Bytes>,  // Reliable, ordered (most packets)
    pub datagram_tx: mpsc::Sender<Bytes>,   // Unreliable, low-latency (RTT echo)
    
    pub tracker_sender: TrackerSender,
    pub session_manager: SessionManager,
    pub nats_client: async_nats::client::Client,  // For health packet processing
}

impl Actor for WtChatSession {
    type Context = Context<Self>;  // Regular context, NOT WebsocketContext
}
```

### Key Difference from WsChatSession

`WsChatSession` uses `WebsocketContext` which provides integrated stream handling. `WtChatSession` uses regular `Context` with explicit channel-based I/O:

1. **Inbound**: Spawned task reads from quinn Session → sends `WtInbound` message to actor
2. **Outbound**: Actor receives `Message` from ChatServer → sends to channel → spawned task writes to quinn Session

### Message Flow

```
[Quinn Session] ──read──→ [Task] ──WtInbound──→ [WtChatSession Actor]
                                                        │
                                                        ▼
                                                 [ChatServer Actor]
                                                        │
                                                        ▼ (via NATS)
                                                 [Other Sessions]
                                                        │
                                                        ▼
[Quinn Session] ←─write── [Task] ←──channel─── [WtChatSession Actor]
```

### New Message Types

```rust
#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct WtInbound {
    pub data: Bytes,
    pub source: WtInboundSource,
}

#[derive(Debug, Clone)]
pub enum WtInboundSource {
    UniStream,
    Datagram,
}
```

### WebTransport I/O: UniStreams and Datagrams

WebTransport has two distinct channels for data, unlike WebSocket's single bidirectional stream:

| Channel | Reliability | Ordering | Use Case |
|---------|-------------|----------|----------|
| **UniStreams** | Reliable | Ordered | Most packets (media, control) |
| **Datagrams** | Unreliable | Unordered | Low-latency data, keep-alive |

#### Current Implementation (in `webtransport/mod.rs`)

```rust
// Three concurrent tasks per session:
// 1. NATS receive → write to UniStream
// 2. UniStream receive → process/publish to NATS
// 3. Datagram receive → process/publish to NATS
```

#### New Implementation with Actors

We spawn **two reader tasks** that feed into the actor, and **one writer task** that drains the outbound channel:

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Quinn Session                                │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌──────────────────┐              ┌──────────────────┐             │
│  │ UniStream Reader │              │ Datagram Reader  │             │
│  │ session.accept_  │              │ session.read_    │             │
│  │ uni().await      │              │ datagram().await │             │
│  └────────┬─────────┘              └────────┬─────────┘             │
│           │                                 │                       │
│           │ WtInbound(UniStream)            │ WtInbound(Datagram)   │
│           └────────────┬────────────────────┘                       │
│                        ▼                                            │
│           ┌────────────────────────┐                                │
│           │   WtChatSession Actor  │                                │
│           │   - Handle inbound     │                                │
│           │   - RTT echo           │                                │
│           │   - Health packets     │                                │
│           │   - Forward to ChatSvr │                                │
│           └────────────┬───────────┘                                │
│                        │                                            │
│                        │ outbound_tx channel                        │
│                        ▼                                            │
│           ┌────────────────────────┐                                │
│           │   UniStream Writer     │                                │
│           │   session.open_uni()   │                                │
│           │   stream.write_all()   │                                │
│           └────────────────────────┘                                │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

#### Outbound Path Decision: UniStream vs Datagram

Currently, outbound messages from NATS always go via **UniStream** (reliable). The actor will maintain this behavior:

```rust
impl Handler<Message> for WtChatSession {
    fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) {
        // Always send via UniStream (reliable) - matches current behavior
        let _ = self.outbound_tx.try_send(msg.msg.into());
    }
}
```

#### RTT Echo Path

RTT packets need special handling - they should echo back via the **same channel** they arrived on:

```rust
impl Handler<WtInbound> for WtChatSession {
    fn handle(&mut self, msg: WtInbound, _ctx: &mut Self::Context) {
        if is_rtt_packet(&msg.data) {
            match msg.source {
                WtInboundSource::UniStream => {
                    // Echo via UniStream
                    let _ = self.outbound_tx.try_send(msg.data);
                }
                WtInboundSource::Datagram => {
                    // Echo via Datagram - need separate channel
                    let _ = self.datagram_tx.try_send(msg.data);
                }
            }
            return;
        }
        // ... handle other packets
    }
}
```

This means `WtChatSession` needs **two outbound channels**:
- `outbound_tx: mpsc::Sender<Bytes>` - for UniStream writes
- `datagram_tx: mpsc::Sender<Bytes>` - for Datagram writes

#### Keep-Alive Handling

Datagram keep-alive pings (`b"ping"`) are handled locally and not forwarded:

```rust
if msg.source == WtInboundSource::Datagram && msg.data.as_ref() == b"ping" {
    // Keep-alive - just update heartbeat, don't forward
    self.heartbeat = Instant::now();
    return;
}
```

## Implementation Steps

### Step 1: Create `WtChatSession` Actor

New file `actors/wt_chat_session.rs`:
- Mirror `WsChatSession` structure
- Same lifecycle: started → JoinRoom → handle messages → stopping
- Implement handlers: `Message` (from ChatServer), `WtInbound` (from quinn)
- Use channels for outbound communication

### Step 2: Update `actors/mod.rs`

Add export for new module:
```rust
pub mod wt_chat_session;
```

### Step 3: Refactor `webtransport/mod.rs`

- Accept `Addr<ChatServer>` parameter
- On connection: spawn `WtChatSession` actor
- Bridge quinn Session to actor via channels
- Remove direct NATS handling (ChatServer does this)

### Step 4: Update `bin/webtransport_server.rs`

- Create NATS connection
- Create database pool
- Start `ChatServer` actor
- Pass `Addr<ChatServer>` to WebTransport handler

### Step 5: Extract Common Helpers

Move duplicated code to shared location:
- `is_rtt_packet()` - currently in both implementations
- Packet handling utilities

## Testing Strategy

### Existing Tests Should Pass

The integration tests in `webtransport/mod.rs` test external behavior:
- `test_relay_packet_webtransport_between_two_clients`
- `test_lobby_isolation`
- `test_meeting_lifecycle_webtransport`

These tests connect as clients and verify behavior. They don't test internal implementation. If the actors are correct, **tests pass unchanged**.

### Only Test Helper Changes

The `start_webtransport_server()` test helper needs to also start `ChatServer`:

```rust
// Before
async fn start_webtransport_server() {
    webtransport::start(opt).await
}

// After
async fn start_webtransport_server() {
    let chat = ChatServer::new(nats_client, pool).await.start();
    webtransport::start(opt, chat).await
}
```

## Files Changed

| File | Action |
|------|--------|
| `actors/wt_chat_session.rs` | **Create** - New WebTransport session actor |
| `actors/mod.rs` | **Modify** - Add export |
| `webtransport/mod.rs` | **Modify** - Use actor, remove direct NATS |
| `bin/webtransport_server.rs` | **Modify** - Start ChatServer |

## Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| Actor overhead for high-throughput | Channels are efficient; benchmark if needed |
| Mixing tokio/actix runtimes | Already works - actix-rt is built on tokio |
| Breaking existing behavior | Integration tests verify external behavior |

## Success Criteria

1. All existing integration tests pass
2. WebTransport uses `ChatServer` actor like WebSocket
3. `WtChatSession` mirrors `WsChatSession` structure
4. No duplicate session lifecycle code

## Future Considerations

- Extract more common code between `WsChatSession` and `WtChatSession`
- Consider trait-based abstraction for transport-agnostic session handling
- Potential single-binary option if operational benefits emerge
