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

//! WebSocket Chat Session Actor
//!
//! This is a thin transport adapter that delegates all business logic
//! to `SessionLogic`. It handles WebSocket-specific I/O via `WebsocketContext`.

use crate::actors::chat_server::ChatServer;
use crate::actors::session_logic::{InboundAction, SessionLogic};
use crate::constants::{CLIENT_TIMEOUT, HEARTBEAT_INTERVAL};
use crate::messages::server::{ActivateConnection, Packet};
use crate::messages::session::Message;
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use actix::ActorFutureExt;
use actix::{
    clock::Instant, fut, Actor, ActorContext, Addr, AsyncContext, ContextFutureSpawner, Handler,
    Running, StreamHandler, WrapFuture,
};
use actix_web_actors::ws::{self, WebsocketContext};
use tracing::{error, info, trace};

pub use crate::actors::session_logic::{RoomId, SessionId, UserId};

/// WebSocket Chat Session Actor
///
/// A thin transport adapter that delegates business logic to `SessionLogic`.
/// Handles WebSocket-specific I/O via `WebsocketContext`.
pub struct WsChatSession {
    /// Shared session logic (business logic)
    logic: SessionLogic,

    /// Heartbeat tracking (transport-specific timing)
    heartbeat: Instant,

    /// Track if ActivateConnection has been sent
    activated: bool,
}

impl WsChatSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        user_id: String,
        display_name: String,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
        observer: bool,
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
        );

        WsChatSession {
            logic,
            heartbeat: Instant::now(),
            activated: false,
        }
    }

    /// Start heartbeat check (WebSocket-specific: uses ping frames)
    fn start_heartbeat(&self, ctx: &mut WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                error!("WebSocket client heartbeat failed, disconnecting!");
                ctx.stop();
                return;
            }
            ctx.ping(b"");
        });
    }
}

// =============================================================================
// Actor Implementation
// =============================================================================

impl Actor for WsChatSession {
    type Context = WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Track connection start
        self.logic.track_connection_start("websocket");

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
                    ctx.binary(act.logic.build_session_assigned());
                    let bytes = act
                        .logic
                        .build_meeting_started(result.start_time_ms, &result.creator_id);
                    ctx.binary(bytes);
                }
                Err(e) => {
                    error!("Failed to start session: {}", e);
                    let bytes = act
                        .logic
                        .build_meeting_ended(&format!("Session rejected: {e}"));
                    ctx.binary(bytes);
                    ctx.close(Some(ws::CloseReason {
                        code: ws::CloseCode::Policy,
                        description: Some("Session rejected".to_string()),
                    }));
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
impl Handler<Message> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) -> Self::Result {
        let bytes = self.logic.handle_outbound(&msg);
        ctx.binary(bytes);
    }
}

/// Handle outbound packets (forwarding to ChatServer)
impl Handler<Packet> for WsChatSession {
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
// WebSocket Stream Handler
// =============================================================================

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsChatSession {
    fn handle(&mut self, item: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(msg) => msg,
            Err(err) => {
                error!("WebSocket protocol error: {:?}", err);
                ctx.stop();
                return;
            }
        };

        match msg {
            ws::Message::Binary(data) => {
                self.heartbeat = Instant::now();

                let action = self.logic.handle_inbound(&data);

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
                    InboundAction::Echo(bytes) => {
                        ctx.binary(bytes.as_ref().clone());
                    }
                    InboundAction::Forward(bytes) => {
                        ctx.notify(Packet { data: bytes });
                    }
                    InboundAction::Processed | InboundAction::KeepAlive => {}
                }
            }
            ws::Message::Ping(msg) => {
                self.heartbeat = Instant::now();
                ctx.pong(&msg);
            }
            ws::Message::Pong(_) => {
                self.heartbeat = Instant::now();
            }
            ws::Message::Close(reason) => {
                info!(
                    "Close received for session {} in room {}",
                    self.logic.id, self.logic.room
                );
                // Do NOT send Leave here. ctx.stop() triggers stopping() which
                // sends Disconnect with the correct observer flag. A separate
                // Leave would bypass the observer check and emit a spurious
                // PARTICIPANT_LEFT for observer (waiting-room) sessions.
                ctx.close(reason);
                ctx.stop();
            }
            _ => (),
        }
    }

    fn started(&mut self, _ctx: &mut Self::Context) {}

    fn finished(&mut self, ctx: &mut Self::Context) {
        ctx.stop()
    }
}

// =============================================================================
// Helper Methods
// =============================================================================

impl WsChatSession {
    fn join_room(&self, ctx: &mut WebsocketContext<Self>) {
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
