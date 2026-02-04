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
/// Outbound message with transport type
pub enum WtOutbound {
    UniStream(Bytes),  // Reliable, ordered (most packets)
    Datagram(Bytes),   // Unreliable, low-latency (RTT echo, keep-alive)
}

pub struct WtChatSession {
    pub id: SessionId,
    pub room: RoomId,
    pub email: Email,
    pub addr: Addr<ChatServer>,
    pub heartbeat: Instant,
    
    // Single outbound channel - enum specifies transport type
    pub outbound_tx: mpsc::Sender<WtOutbound>,
    
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
│                        │ outbound_tx: Sender<WtOutbound>            │
│                        ▼                                            │
│           ┌────────────────────────┐                                │
│           │      Writer Task       │                                │
│           │  match msg {           │                                │
│           │    UniStream(d) =>     │                                │
│           │      open_uni()        │                                │
│           │    Datagram(d) =>      │                                │
│           │      send_datagram()   │                                │
│           │  }                     │                                │
│           └────────────────────────┘                                │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

#### Outbound Path Decision: UniStream vs Datagram

The `WtOutbound` enum specifies which transport to use:

```rust
impl Handler<Message> for WtChatSession {
    fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) {
        // NATS messages always via UniStream (reliable)
        let _ = self.outbound_tx.try_send(WtOutbound::UniStream(msg.msg.into()));
    }
}
```

#### RTT Echo Path

RTT packets echo back via the **same channel** they arrived on:

```rust
impl Handler<WtInbound> for WtChatSession {
    fn handle(&mut self, msg: WtInbound, _ctx: &mut Self::Context) {
        if is_rtt_packet(&msg.data) {
            // Echo via same channel it arrived on
            let outbound = match msg.source {
                WtInboundSource::UniStream => WtOutbound::UniStream(msg.data),
                WtInboundSource::Datagram => WtOutbound::Datagram(msg.data),
            };
            let _ = self.outbound_tx.try_send(outbound);
            return;
        }
        // ... handle other packets
    }
}
```

#### Writer Task

Single writer task handles both transport types:

```rust
// Writer task
while let Some(msg) = outbound_rx.recv().await {
    match msg {
        WtOutbound::UniStream(data) => {
            if let Ok(mut stream) = session.open_uni().await {
                let _ = stream.write_all(&data).await;
            }
        }
        WtOutbound::Datagram(data) => {
            let _ = session.send_datagram(data);
        }
    }
}
```

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

## Final Architecture

### Clean Separation of Concerns

```
actors/
  chat_server.rs           # Room management, NATS routing
  session_logic.rs         # Shared business logic (226 lines)
  packet_handler.rs        # Packet classification (103 lines)
  transports/
    mod.rs                 # Transport exports
    ws_chat_session.rs     # WebSocket adapter (~270 lines code)
    wt_chat_session.rs     # WebTransport adapter (~320 lines)
```

### The Abstraction

```
                    ┌─────────────────────────────────────┐
                    │           SessionLogic              │
                    │    (transport-agnostic logic)       │
                    │  ─────────────────────────────────  │
                    │  • handle_inbound() → InboundAction │
                    │  • handle_outbound() → bytes        │
                    │  • track_connection_start/end()     │
                    │  • on_stopping()                    │
                    │  • build_meeting_started/ended()    │
                    └──────────────────┬──────────────────┘
                                       │ owns
                      ┌────────────────┴────────────────┐
                      ▼                                 ▼
            ┌─────────────────┐               ┌─────────────────┐
            │  WsChatSession  │               │  WtChatSession  │
            │  (thin adapter) │               │  (thin adapter) │
            │  ─────────────  │               │  ─────────────  │
            │  ctx.binary()   │               │  tx.send()      │
            └────────┬────────┘               └────────┬────────┘
                     │                                  │
                     └──────────────┬───────────────────┘
                                    ▼
                      ┌─────────────────────────────────┐
                      │          ChatServer             │
                      │  • Room membership              │
                      │  • NATS subscriptions           │
                      │  • Message routing              │
                      └─────────────────────────────────┘
```

### Shared Modules

| Module | Lines | Purpose |
|--------|-------|---------|
| `session_logic.rs` | 226 | All business logic |
| `packet_handler.rs` | 103 | Packet classification (`PacketKind` enum) |

### Adding a New Feature

```rust
// 1. Add to SessionLogic (once)
impl SessionLogic {
    pub fn new_feature(&self) { ... }
}

// 2. Both transports get it automatically!
// (Call self.logic.new_feature() if transport-specific handling needed)
```

---

## Adding a New Transport

To add a new transport (e.g., QUIC raw, WebRTC DataChannel, TCP):

### Step 1: Create the Transport Adapter

Create `actors/transports/new_transport_session.rs`:

```rust
use crate::actors::session_logic::{InboundAction, SessionLogic};

pub struct NewTransportSession {
    /// Shared business logic - gets EVERYTHING for free
    logic: SessionLogic,
    
    /// Transport-specific I/O
    outbound_tx: mpsc::Sender<Bytes>,  // or your transport's channel
}

impl NewTransportSession {
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        email: String,
        outbound_tx: mpsc::Sender<Bytes>,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
    ) -> Self {
        // SessionLogic::new() handles ID generation, logging, etc.
        let logic = SessionLogic::new(
            addr, room, email, nats_client, tracker_sender, session_manager,
        );
        Self { logic, outbound_tx }
    }
}
```

### Step 2: Implement the Actor Trait

```rust
impl Actor for NewTransportSession {
    type Context = Context<Self>;  // or your custom context

    fn started(&mut self, ctx: &mut Self::Context) {
        // 1. Track connection (shared)
        self.logic.track_connection_start("new_transport");

        // 2. Start session via SessionManager (shared)
        let session_manager = self.logic.session_manager.clone();
        let room = self.logic.room.clone();
        let email = self.logic.email.clone();

        ctx.wait(
            async move { session_manager.start_session(&room, &email).await }
                .into_actor(self)
                .map(|result, act, ctx| {
                    match result {
                        Ok(result) => {
                            // Build packet (shared), send (transport-specific)
                            let bytes = act.logic.build_meeting_started(
                                result.start_time_ms,
                                &result.creator_id,
                            );
                            act.send(bytes);  // Your transport's send
                        }
                        Err(e) => {
                            let bytes = act.logic.build_meeting_ended(&format!("Error: {e}"));
                            act.send(bytes);
                            ctx.stop();
                        }
                    }
                }),
        );

        // 3. Register with ChatServer (shared pattern)
        // 4. Join room (shared pattern)
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        // Cleanup is ONE LINE - all logic is shared
        self.logic.on_stopping();
        Running::Stop
    }
}
```

### Step 3: Handle Inbound Data

```rust
impl Handler<YourInboundMessage> for NewTransportSession {
    type Result = ();

    fn handle(&mut self, msg: YourInboundMessage, ctx: &mut Self::Context) {
        // Delegate to shared logic - it handles RTT, health, classification
        match self.logic.handle_inbound(&msg.data) {
            InboundAction::Echo(bytes) => {
                self.send(bytes.to_vec());  // Your transport's send
            }
            InboundAction::Forward(bytes) => {
                ctx.notify(Packet { data: bytes });
            }
            InboundAction::Processed | InboundAction::KeepAlive => {
                // Already handled by SessionLogic
            }
        }
    }
}
```

### Step 4: Handle Outbound from ChatServer

```rust
impl Handler<Message> for NewTransportSession {
    type Result = ();

    fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) {
        // Shared logic tracks metrics, returns bytes to send
        let bytes = self.logic.handle_outbound(&msg);
        self.send(bytes);  // Your transport's send
    }
}
```

### Step 5: Export from `transports/mod.rs`

```rust
pub mod new_transport_session;
pub use new_transport_session::NewTransportSession;
```

### What You Get for Free

| Feature | You Write | Provided by SessionLogic |
|---------|-----------|--------------------------|
| Session ID generation | - | ✅ |
| Connection tracking | - | ✅ |
| RTT packet echo | - | ✅ |
| Health packet processing | - | ✅ |
| Packet classification | - | ✅ |
| Metrics tracking | - | ✅ |
| Meeting lifecycle (start/end) | - | ✅ |
| ChatServer integration | - | ✅ |
| Cleanup on disconnect | - | ✅ |
| **Transport I/O** | ✅ | - |
| **Keep-alive mechanism** | ✅ | - |

---

## Remaining Transport Differences (By Design)

| Aspect | WebSocket | WebTransport | Reason |
|--------|-----------|--------------|--------|
| I/O Model | `WebsocketContext` | `mpsc` channels | Different protocols |
| Keep-alive | WS ping/pong frames | Custom datagram ping | Protocol-specific |
| Binary send | `ctx.binary(bytes)` | `tx.send(WtOutbound)` | API differences |

### Not Recommended to Consolidate

- **Transport I/O**: Fundamentally different APIs (actix-web-actors vs quinn)
- **Single binary**: Separate binaries work well for horizontal scaling via NATS
