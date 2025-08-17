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

use crate::connection_tracker::GLOBAL_TRACKER;
use crate::diagnostics::health_processor;
use crate::messages::server::{ClientMessage, Packet};
use crate::messages::session::Message;
use crate::{actors::chat_server::ChatServer, constants::CLIENT_TIMEOUT};
use std::sync::Arc;

use crate::{
    constants::HEARTBEAT_INTERVAL,
    messages::server::{Connect, Disconnect, JoinRoom},
};
use actix::ActorFutureExt;
use actix::{
    clock::Instant, fut, ActorContext, ContextFutureSpawner, Handler, Running, StreamHandler,
    WrapFuture,
};
use actix::{Actor, Addr, AsyncContext};
use actix_web_actors::ws::{self, WebsocketContext};
use protobuf::Message as ProtobufMessage;
use tracing::{error, info, trace};
use uuid::Uuid;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub type RoomId = String;
pub type Email = String;
pub type SessionId = String;

pub struct WsChatSession {
    pub id: SessionId,
    pub room: RoomId,
    pub addr: Addr<ChatServer>,
    pub heartbeat: Instant,
    pub email: Email,
    pub nats_client: async_nats::client::Client,
}

impl WsChatSession {
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        email: String,
        nats_client: async_nats::client::Client,
    ) -> Self {
        info!("new session with room {} and email {}", room, email);

        WsChatSession {
            id: Uuid::new_v4().to_string(),
            heartbeat: Instant::now(),
            room,
            email,
            addr,
            nats_client,
        }
    }

    /// Check if the binary data is an RTT packet that should be echoed back
    fn is_rtt_packet(&self, data: &[u8]) -> bool {
        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            if packet_wrapper.packet_type == PacketType::MEDIA.into() {
                if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
                    return media_packet.media_type == MediaType::RTT.into();
                }
            }
        }
        false
    }

    fn heartbeat(&self, ctx: &mut WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                // heartbeat timed out
                println!("Websocket Client heartbeat failed, disconnecting!");
                // notify chat server
                act.addr.do_send(Disconnect {
                    session: act.id.clone(),
                });
                // stop actor
                error!("hearbeat timeout");
                ctx.stop();
                // don't try to send a ping
                return;
            }
            ctx.ping(b"");
        });
    }
}

impl Actor for WsChatSession {
    type Context = WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Track connection start for metrics
        GLOBAL_TRACKER.connection_started(
            self.id.clone(),
            self.email.clone(),
            self.room.clone(),
            "websocket".to_string(),
        );

        self.heartbeat(ctx);
        let addr = ctx.address();
        self.addr
            .send(Connect {
                id: self.id.clone(),
                addr: addr.recipient(),
            })
            .into_actor(self)
            .then(|res, _act, ctx| {
                if let Err(err) = res {
                    error!("error {:?}", err);
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);
        self.join(self.room.clone(), ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        // Track connection end for metrics
        GLOBAL_TRACKER.connection_ended(&self.id);

        // notify chat server
        self.addr.do_send(Disconnect {
            session: self.id.clone(),
        });
        Running::Stop
    }
}

impl Handler<Message> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) -> Self::Result {
        // Track sent data when forwarding messages to clients
        GLOBAL_TRACKER.track_data_sent(&self.id, msg.msg.len() as u64);
        ctx.binary(msg.msg);
    }
}

impl Handler<Packet> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Packet, _ctx: &mut Self::Context) -> Self::Result {
        let room_id = self.room.clone();
        trace!(
            "got message and sending to chat session {} email {} room {}",
            self.id.clone(),
            self.email.clone(),
            room_id
        );
        self.addr.do_send(ClientMessage {
            session: self.id.clone(),
            user: self.email.clone(),
            room: room_id,
            msg,
        });
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsChatSession {
    fn handle(&mut self, item: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(msg) => msg,
            Err(err) => {
                error!("protocol error 2 {:?}", err);
                // ctx.text(WsMessage::err(err.to_string()));
                ctx.stop();
                return;
            }
        };

        match msg {
            ws::Message::Binary(msg) => {
                let msg_bytes = msg.to_vec();

                // Track received data
                GLOBAL_TRACKER.track_data_received(&self.id, msg_bytes.len() as u64);

                // Check if this is an RTT packet that should be echoed back
                if self.is_rtt_packet(&msg_bytes) {
                    trace!("Echoing RTT packet back to sender: {}", self.email);
                    // Track sent data for echo
                    GLOBAL_TRACKER.track_data_sent(&self.id, msg_bytes.len() as u64);
                    ctx.binary(msg_bytes);
                } else if health_processor::is_health_packet_bytes(&msg_bytes) {
                    // Process health packet for diagnostics (don't relay to other peers)
                    health_processor::process_health_packet_bytes(
                        &msg_bytes,
                        self.nats_client.clone(),
                    );
                } else {
                    // Normal packet processing - forward to chat server
                    ctx.notify(Packet {
                        data: Arc::new(msg_bytes),
                    });
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
                ctx.close(reason);
                error!("socket closed");
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

impl WsChatSession {
    fn join(&self, room_id: String, ctx: &mut WebsocketContext<Self>) {
        let join_room = self.addr.send(JoinRoom {
            room: room_id.clone(),
            session: self.id.clone(),
        });
        let join_room = join_room.into_actor(self);
        join_room
            .then(move |response, act, ctx| {
                match response {
                    Ok(res) if res.is_ok() => {
                        act.room = room_id;
                    }
                    Ok(res) => {
                        error!("error {:?}", res);
                    }
                    Err(err) => {
                        error!("error {:?}", err);
                        ctx.stop();
                    }
                }
                fut::ready(())
            })
            .wait(ctx);
    }
}
