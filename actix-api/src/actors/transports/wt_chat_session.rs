/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! WebTransport Chat Session Actor
//!
//! This is a thin transport adapter that delegates all business logic
//! to `SessionLogic`. It handles WebTransport-specific I/O via channels.

use crate::actors::chat_server::ChatServer;
use crate::actors::session_logic::{InboundAction, SessionLogic};
use crate::constants::CLIENT_TIMEOUT;
use crate::messages::server::{ActivateConnection, ClientMessage, Packet};
use crate::messages::session::Message;
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use actix::{
    fut, Actor, ActorContext, ActorFutureExt, Addr, AsyncContext, Context, ContextFutureSpawner,
    Handler, Message as ActixMessage, Running, WrapFuture,
};
use bytes::Bytes;
use protobuf::Message as ProtobufMessage;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, trace, warn};
use videocall_types::protos::packet_wrapper::packet_wrapper::ConnectionPhase;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub use crate::actors::session_logic::{Email, RoomId, SessionId};

/// Heartbeat interval for WebTransport sessions
const WT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Keep-alive ping data (WebTransport-specific)
const KEEP_ALIVE_PING: &[u8] = b"ping";

/// Outbound message with transport type specification
#[derive(Debug, Clone)]
pub enum WtOutbound {
    /// Send via UniStream (reliable, ordered)
    UniStream(Bytes),
    /// Send via Datagram (unreliable, unordered, low latency)
    Datagram(Bytes),
}

/// Source of inbound data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtInboundSource {
    UniStream,
    Datagram,
}

/// Inbound message from WebTransport session
#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct WtInbound {
    pub data: Bytes,
    pub source: WtInboundSource,
}

/// Signal to stop the session (sent when I/O tasks end)
#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct StopSession;

/// WebTransport Chat Session Actor
///
/// A thin transport adapter that delegates business logic to `SessionLogic`.
/// Handles WebTransport-specific I/O via channels.
pub struct WtChatSession {
    /// Shared session logic (business logic)
    logic: SessionLogic,

    /// Heartbeat tracking (transport-specific timing)
    heartbeat: actix::clock::Instant,

    /// Channel to send data back to WebTransport session
    outbound_tx: mpsc::Sender<WtOutbound>,

    /// Track if ActivateConnection has been sent
    activated: bool,
}

impl WtChatSession {
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        email: String,
        outbound_tx: mpsc::Sender<WtOutbound>,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
    ) -> Self {
        let logic = SessionLogic::new(
            addr,
            room,
            email,
            nats_client,
            tracker_sender,
            session_manager,
        );

        WtChatSession {
            logic,
            heartbeat: actix::clock::Instant::now(),
            outbound_tx,
            activated: false,
        }
    }

    /// Send outbound message via the channel.
    /// Returns false if the channel is closed (connection dead).
    fn send(&self, data: Vec<u8>) -> bool {
        match self
            .outbound_tx
            .try_send(WtOutbound::UniStream(data.into()))
        {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    "Outbound channel closed for session {}, connection dead",
                    self.logic.id
                );
                false
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                error!(
                    "Outbound channel full for session {}, dropping message",
                    self.logic.id
                );
                true // Channel still open, just full
            }
        }
    }

    /// Check if the outbound channel is closed
    fn is_connection_dead(&self) -> bool {
        self.outbound_tx.is_closed()
    }

    /// Start heartbeat check (WebTransport-specific timing)
    fn start_heartbeat(&self, ctx: &mut Context<Self>) {
        ctx.run_interval(WT_HEARTBEAT_INTERVAL, |act, ctx| {
            // Check if connection is dead (channel closed)
            if act.is_connection_dead() {
                warn!(
                    "WebTransport connection dead (channel closed), stopping session {}",
                    act.logic.id
                );
                ctx.stop();
                return;
            }

            // Check heartbeat timeout
            if actix::clock::Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                warn!(
                    "WebTransport client heartbeat failed, disconnecting session {}",
                    act.logic.id
                );
                ctx.stop();
            }
        });
    }
}

// =============================================================================
// Actor Implementation
// =============================================================================

impl Actor for WtChatSession {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Track connection start
        self.logic.track_connection_start("webtransport");

        // Start session via SessionManager
        let session_manager = self.logic.session_manager.clone();
        let room = self.logic.room.clone();
        let email = self.logic.email.clone();
        let session_id = self.logic.id.clone();

        ctx.wait(
            async move {
                session_manager
                    .start_session(&room, &email, &session_id)
                    .await
            }
            .into_actor(self)
            .map(|result, act, ctx| match result {
                Ok(result) => {
                    let bytes = act
                        .logic
                        .build_meeting_started(result.start_time_ms, &result.creator_id);
                    act.send(bytes);
                }
                Err(e) => {
                    error!("Failed to start session: {}", e);
                    let bytes = act
                        .logic
                        .build_meeting_ended(&format!("Session rejected: {e}"));
                    act.send(bytes);
                    ctx.stop();
                }
            }),
        );

        // Start heartbeat
        self.start_heartbeat(ctx);

        // Register with ChatServer
        let addr = ctx.address();
        self.logic
            .addr
            .send(self.logic.create_connect_message(addr.recipient()))
            .into_actor(self)
            .then(|res, _act, ctx| {
                if let Err(err) = res {
                    error!("Failed to connect to ChatServer: {:?}", err);
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);

        // Join room
        self.join_room(ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        self.logic.on_stopping();
        Running::Stop
    }
}

// =============================================================================
// Message Handlers
// =============================================================================

/// Handle outbound messages from ChatServer
impl Handler<Message> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) -> Self::Result {
        let bytes = self.logic.handle_outbound(&msg);
        if !self.send(bytes) {
            // Channel closed - connection is dead, stop the actor
            ctx.stop();
        }
    }
}

/// Handle inbound data from WebTransport session
impl Handler<WtInbound> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: WtInbound, ctx: &mut Self::Context) -> Self::Result {
        // Update heartbeat
        self.heartbeat = actix::clock::Instant::now();

        // Handle keep-alive ping (WebTransport-specific)
        if msg.source == WtInboundSource::Datagram && msg.data.as_ref() == KEEP_ALIVE_PING {
            trace!("Received keep-alive ping for session {}", self.logic.id);
            return;
        }

        // Check connection_phase from inbound packet
        if !self.activated {
            if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(msg.data.as_ref()) {
                if let Ok(phase) = packet_wrapper.connection_phase.enum_value() {
                    match phase {
                        ConnectionPhase::ACTIVE => {
                            // First ACTIVE packet - activate connection
                            self.logic.addr.do_send(ActivateConnection {
                                session: self.logic.id.clone(),
                            });
                            self.activated = true;
                            info!("Session {} activated on first ACTIVE packet", self.logic.id);
                        }
                        ConnectionPhase::CONNECTION_PHASE_UNSPECIFIED => {
                            // Activate immediately for old clients (backward compatibility)
                            self.logic.addr.do_send(ActivateConnection {
                                session: self.logic.id.clone(),
                            });
                            self.activated = true;
                            info!(
                                "Session {} activated on UNSPECIFIED (old client)",
                                self.logic.id
                            );
                        }
                        ConnectionPhase::PROBING => {
                            // Do not activate during probing phase
                        }
                    }
                }
            }
        }

        // Delegate to shared logic
        match self.logic.handle_inbound(&msg.data) {
            InboundAction::Echo(data) => {
                // Echo via the same channel it arrived on
                let outbound = match msg.source {
                    WtInboundSource::UniStream => {
                        WtOutbound::UniStream(Bytes::from(data.as_ref().clone()))
                    }
                    WtInboundSource::Datagram => {
                        WtOutbound::Datagram(Bytes::from(data.as_ref().clone()))
                    }
                };
                match self.outbound_tx.try_send(outbound) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        warn!(
                            "Outbound channel closed while echoing RTT for session {}",
                            self.logic.id
                        );
                        ctx.stop();
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        error!(
                            "Outbound channel full, dropping RTT echo for session {}",
                            self.logic.id
                        );
                    }
                }
            }
            InboundAction::Forward(data) => {
                ctx.notify(Packet { data });
            }
            InboundAction::Processed | InboundAction::KeepAlive => {
                // Already handled
            }
        }
    }
}

/// Handle stop signal
impl Handler<StopSession> for WtChatSession {
    type Result = ();

    fn handle(&mut self, _msg: StopSession, ctx: &mut Self::Context) -> Self::Result {
        info!(
            "Received stop signal for WebTransport session {} in room {}",
            self.logic.id, self.logic.room
        );
        ctx.stop();
    }
}

/// Handle outbound packets (forwarding to ChatServer)
impl Handler<Packet> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: Packet, _ctx: &mut Self::Context) -> Self::Result {
        trace!(
            "Forwarding packet to ChatServer: session {} room {}",
            self.logic.id,
            self.logic.room
        );
        self.logic.addr.do_send(ClientMessage {
            session: self.logic.id.clone(),
            user: self.logic.email.clone(),
            room: self.logic.room.clone(),
            msg,
        });
    }
}

// =============================================================================
// Helper Methods
// =============================================================================

impl WtChatSession {
    fn join_room(&self, ctx: &mut Context<Self>) {
        let join_room = self.logic.addr.send(self.logic.create_join_room_message());
        let join_room = join_room.into_actor(self);
        join_room
            .then(|response, act, ctx| {
                match response {
                    Ok(Ok(())) => {
                        info!(
                            "Successfully joined room {} for session {}",
                            act.logic.room, act.logic.id
                        );
                    }
                    Ok(Err(e)) => {
                        error!("Failed to join room: {}", e);
                        ctx.stop();
                    }
                    Err(err) => {
                        error!("Error sending JoinRoom: {:?}", err);
                        ctx.stop();
                    }
                }
                fut::ready(())
            })
            .wait(ctx);
    }
}
