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
//! This module provides `WtChatSession`, an actor that handles individual WebTransport
//! connections. It mirrors `WsChatSession` but uses channel-based I/O instead of
//! `WebsocketContext` since WebTransport uses quinn's async Session API.

use crate::actors::chat_server::ChatServer;
use crate::actors::packet_handler::{classify_packet, PacketKind};
use crate::client_diagnostics::health_processor;
use crate::constants::CLIENT_TIMEOUT;
use crate::messages::server::{ClientMessage, Connect, Disconnect, JoinRoom, Packet};
use crate::messages::session::Message;
use crate::server_diagnostics::{
    send_connection_ended, send_connection_started, DataTracker, TrackerSender,
};
use crate::session_manager::SessionManager;
use actix::{
    fut, Actor, ActorContext, ActorFutureExt, Addr, AsyncContext, Context, ContextFutureSpawner,
    Handler, Message as ActixMessage, Running, WrapFuture,
};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, trace, warn};
use uuid::Uuid;

pub use crate::actors::chat_session::{Email, RoomId, SessionId};

/// Heartbeat interval for WebTransport sessions
const WT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Keep-alive ping data
const KEEP_ALIVE_PING: &[u8] = b"ping";

/// Outbound message with transport type specification
#[derive(Debug, Clone)]
pub enum WtOutbound {
    /// Send via UniStream (reliable, ordered)
    UniStream(Bytes),
    /// Send via Datagram (unreliable, low-latency)
    Datagram(Bytes),
}

/// Source of inbound WebTransport data
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
/// Handles an individual WebTransport connection, mirroring `WsChatSession`.
/// Uses channel-based I/O instead of `WebsocketContext`.
pub struct WtChatSession {
    pub id: SessionId,
    pub room: RoomId,
    pub email: Email,
    pub addr: Addr<ChatServer>,
    pub heartbeat: actix::clock::Instant,

    /// Channel to send data back to WebTransport session
    pub outbound_tx: mpsc::Sender<WtOutbound>,

    pub tracker_sender: TrackerSender,
    pub session_manager: SessionManager,
    pub nats_client: async_nats::client::Client,
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
        let session_id = Uuid::new_v4().to_string();
        info!(
            "new WebTransport session with room {} and email {} and session_id {:?}",
            room, email, session_id
        );

        WtChatSession {
            id: session_id,
            heartbeat: actix::clock::Instant::now(),
            room,
            email: email.clone(),
            addr,
            outbound_tx,
            nats_client,
            tracker_sender,
            session_manager,
        }
    }

    /// Start heartbeat check
    fn start_heartbeat(&self, ctx: &mut Context<Self>) {
        ctx.run_interval(WT_HEARTBEAT_INTERVAL, |act, ctx| {
            if actix::clock::Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                warn!(
                    "WebTransport client heartbeat failed, disconnecting session {}",
                    act.id
                );
                // notify chat server
                act.addr.do_send(Disconnect {
                    session: act.id.clone(),
                    room: act.room.clone(),
                    user_id: act.email.clone(),
                });
                // stop actor
                ctx.stop();
            }
        });
    }

    /// Send outbound message via the channel
    fn send_outbound(&self, msg: WtOutbound) {
        if let Err(e) = self.outbound_tx.try_send(msg) {
            error!("Failed to send outbound message: {}", e);
        }
    }
}

impl Actor for WtChatSession {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Track connection start for metrics
        send_connection_started(
            &self.tracker_sender,
            self.id.clone(),
            self.email.clone(),
            self.room.clone(),
            "webtransport".to_string(),
        );

        // Start session using SessionManager
        let session_manager = self.session_manager.clone();
        let room_id = self.room.clone();
        let email = self.email.clone();

        ctx.wait(
            async move {
                match session_manager.start_session(&room_id, &email).await {
                    Ok(result) => Ok((result.start_time_ms, result.creator_id)),
                    Err(e) => {
                        error!("failed to start session: {}", e);
                        Err(e.to_string())
                    }
                }
            }
            .into_actor(self)
            .map(move |result, act, ctx| {
                match result {
                    Ok((start_time_ms, actual_creator_id)) => {
                        // Send MEETING_STARTED packet via UniStream
                        let bytes = SessionManager::build_meeting_started_packet(
                            &act.room,
                            start_time_ms,
                            &actual_creator_id,
                        );
                        act.send_outbound(WtOutbound::UniStream(bytes.into()));
                    }
                    Err(error_msg) => {
                        // Send error to client and close connection
                        let bytes = SessionManager::build_meeting_ended_packet(
                            &act.room,
                            &format!("Session rejected: {error_msg}"),
                        );
                        act.send_outbound(WtOutbound::UniStream(bytes.into()));
                        ctx.stop();
                    }
                }
            }),
        );

        // Start heartbeat
        self.start_heartbeat(ctx);

        // Register with ChatServer
        let addr = ctx.address();
        self.addr
            .send(Connect {
                id: self.id.clone(),
                addr: addr.recipient(),
            })
            .into_actor(self)
            .then(|res, _act, ctx| {
                if let Err(err) = res {
                    error!("error connecting to ChatServer: {:?}", err);
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);

        // Join the room
        self.join_room(ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        info!(
            "WebTransport session stopping: {} in room {}",
            self.id, self.room
        );
        // Track connection end for metrics
        send_connection_ended(&self.tracker_sender, self.id.clone());

        // Notify chat server
        self.addr.do_send(Disconnect {
            session: self.id.clone(),
            room: self.room.clone(),
            user_id: self.email.clone(),
        });

        Running::Stop
    }
}

/// Handle messages from ChatServer (forwarded from NATS)
impl Handler<Message> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) -> Self::Result {
        // Track sent data
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_sent(&self.id, msg.msg.len() as u64);

        // Send via UniStream (reliable, ordered)
        self.send_outbound(WtOutbound::UniStream(msg.msg.into()));
    }
}

/// Handle inbound data from WebTransport session
impl Handler<WtInbound> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: WtInbound, ctx: &mut Self::Context) -> Self::Result {
        // Update heartbeat on any inbound data
        self.heartbeat = actix::clock::Instant::now();

        // Track received data
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_received(&self.id, msg.data.len() as u64);

        // Handle keep-alive ping (datagram only, WebTransport-specific)
        if msg.source == WtInboundSource::Datagram && msg.data.as_ref() == KEEP_ALIVE_PING {
            trace!("Received keep-alive ping for session {}", self.id);
            return;
        }

        // Classify and handle packet using shared logic
        match classify_packet(&msg.data) {
            PacketKind::Rtt => {
                trace!("Echoing RTT packet back to sender: {}", self.email);
                let data_tracker = DataTracker::new(self.tracker_sender.clone());
                data_tracker.track_sent(&self.id, msg.data.len() as u64);

                // Echo via the same channel it arrived on
                let outbound = match msg.source {
                    WtInboundSource::UniStream => WtOutbound::UniStream(msg.data),
                    WtInboundSource::Datagram => WtOutbound::Datagram(msg.data),
                };
                self.send_outbound(outbound);
            }
            PacketKind::Health => {
                trace!("Processing health packet for session {}", self.id);
                health_processor::process_health_packet_bytes(&msg.data, self.nats_client.clone());
            }
            PacketKind::Data => {
                // Forward to ChatServer for room routing
                ctx.notify(Packet {
                    data: Arc::new(msg.data.to_vec()),
                });
            }
        }
    }
}

/// Handle outbound packets (forwarding to ChatServer)
impl Handler<Packet> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: Packet, _ctx: &mut Self::Context) -> Self::Result {
        trace!(
            "Forwarding packet to ChatServer: session {} room {}",
            self.id,
            self.room
        );
        self.addr.do_send(ClientMessage {
            session: self.id.clone(),
            user: self.email.clone(),
            room: self.room.clone(),
            msg,
        });
    }
}

/// Handle stop signal - stops the actor
impl Handler<StopSession> for WtChatSession {
    type Result = ();

    fn handle(&mut self, _msg: StopSession, ctx: &mut Self::Context) -> Self::Result {
        info!(
            "Received stop signal for WebTransport session {} in room {}",
            self.id, self.room
        );
        ctx.stop();
    }
}

impl WtChatSession {
    fn join_room(&self, ctx: &mut Context<Self>) {
        let join_room = self.addr.send(JoinRoom {
            room: self.room.clone(),
            session: self.id.clone(),
            user_id: self.email.clone(),
        });
        let join_room = join_room.into_actor(self);
        join_room
            .then(|response, act, ctx| {
                match response {
                    Ok(Ok(())) => {
                        info!(
                            "Successfully joined room {} for session {}",
                            act.room, act.id
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
