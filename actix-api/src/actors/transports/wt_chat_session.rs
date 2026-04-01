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
use crate::actors::packet_handler::DATAGRAM_MAX_SIZE;
use crate::actors::session_logic::{InboundAction, SessionLogic};
use crate::constants::CLIENT_TIMEOUT;
use crate::messages::server::{ActivateConnection, Packet};
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
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub use crate::actors::session_logic::{RoomId, SessionId, UserId};

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

/// Result of attempting to send an outbound message to the WebTransport channel.
enum WtSendResult {
    /// Message sent successfully.
    Sent,
    /// Channel is full; message was dropped.
    Dropped,
    /// Channel is closed; connection is dead.
    Dead,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        user_id: String,
        display_name: String,
        outbound_tx: mpsc::Sender<WtOutbound>,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
        observer: bool,
        instance_id: Option<String>,
    ) -> Self {
        let logic = SessionLogic::new(
            addr,
            room,
            user_id,
            display_name,
            nats_client,
            tracker_sender,
            session_manager,
            observer,
            instance_id,
        );

        WtChatSession {
            logic,
            heartbeat: actix::clock::Instant::now(),
            outbound_tx,
            activated: false,
        }
    }

    /// Send outbound message via the channel (reliable unidirectional stream).
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

    /// Send outbound message, automatically choosing datagram or stream.
    ///
    /// Control packets (heartbeats, RTT probes, diagnostics) that fit within
    /// the datagram MTU are sent via unreliable datagrams — they are periodic
    /// and expendable, so lower overhead matters more than guaranteed delivery.
    ///
    /// Media packets (VIDEO, AUDIO, SCREEN) use reliable unidirectional streams
    /// to avoid visual/audio artifacts from packet loss.
    ///
    /// The `is_media` hint is pre-computed by the caller from an already-parsed
    /// `PacketWrapper`, avoiding a redundant protobuf parse on every outbound
    /// packet.
    fn send_auto(&self, data: Vec<u8>, is_media: bool) -> WtSendResult {
        let outbound = if !is_media && data.len() <= DATAGRAM_MAX_SIZE {
            WtOutbound::Datagram(data.into())
        } else {
            WtOutbound::UniStream(data.into())
        };

        match self.outbound_tx.try_send(outbound) {
            Ok(()) => WtSendResult::Sent,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    "Outbound channel closed for session {}, connection dead",
                    self.logic.id
                );
                WtSendResult::Dead
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                error!(
                    "Outbound channel full for session {}, dropping message",
                    self.logic.id
                );
                WtSendResult::Dropped
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
        let user_id = self.logic.user_id.clone();
        let session_id = self.logic.id;

        ctx.wait(
            async move {
                session_manager
                    .start_session(&room, &user_id, session_id)
                    .await
            }
            .into_actor(self)
            .map(|result, act, ctx| match result {
                Ok(result) => {
                    act.send(act.logic.build_session_assigned());
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

        // Start heartbeat AFTER all initialization is complete to avoid
        // premature timeout if Connect/JoinRoom are slow under load.
        self.start_heartbeat(ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        self.logic.on_stopping();
        Running::Stop
    }
}

// =============================================================================
// Message Handlers
// =============================================================================

/// Handle outbound messages from ChatServer.
///
/// Uses `send_auto` to route control packets (heartbeats, RTT, diagnostics)
/// via datagrams (periodic and expendable) and media packets (VIDEO, AUDIO,
/// SCREEN) via reliable streams (avoids visual/audio artifacts).
///
/// The outbound `msg.msg` is a serialized `PacketWrapper`. We parse it once
/// to extract both the sender's `session_id` (for congestion tracking) and
/// the `packet_type` (for datagram vs. stream routing), avoiding a second
/// parse inside `send_auto`.
///
/// Note: `msg.session` is the **receiver's** session ID (set by
/// `chat_server::handle_msg`), NOT the sender's. The sender's session ID
/// lives inside the serialized `PacketWrapper.session_id` field.
impl Handler<Message> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) -> Self::Result {
        let bytes = self.logic.handle_outbound(&msg);

        // Parse the PacketWrapper once to extract the sender's session_id
        // and packet_type. This avoids a redundant parse in send_auto and
        // ensures congestion tracking targets the correct (sender) session.
        let parsed = PacketWrapper::parse_from_bytes(&msg.msg).ok();
        let sender_session_id = parsed.as_ref().map(|pw| pw.session_id).unwrap_or(0);
        let is_media = parsed
            .as_ref()
            .map(|pw| pw.packet_type == PacketType::MEDIA.into())
            .unwrap_or(false);

        match self.send_auto(bytes, is_media) {
            WtSendResult::Sent => {}
            WtSendResult::Dead => {
                ctx.stop();
            }
            WtSendResult::Dropped => {
                // Outbound channel full -- record the drop for the actual sender
                // so we can send CONGESTION feedback when the threshold is exceeded.
                if sender_session_id != 0 {
                    self.logic.on_outbound_drop(sender_session_id);
                }
            }
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

        let action = self.logic.handle_inbound(&msg.data);

        if !self.activated && SessionLogic::should_activate_on_action(&action) {
            self.logic.addr.do_send(ActivateConnection {
                session: self.logic.id,
            });
            self.activated = true;
            info!(
                "Session {} activated on first non-RTT packet",
                self.logic.id
            );
        }

        match action {
            InboundAction::Echo(data) => {
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
            InboundAction::Processed | InboundAction::KeepAlive => {}
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
        self.logic
            .addr
            .do_send(self.logic.create_client_message(msg));
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
                if act.logic.handle_join_room_result(response) {
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);
    }
}
