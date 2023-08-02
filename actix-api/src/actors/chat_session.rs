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
use tracing::{error, info, trace};
use uuid::Uuid;

pub type RoomId = String;
pub type Email = String;
pub type SessionId = String;

pub struct WsChatSession {
    pub id: SessionId,
    pub room: RoomId,
    pub addr: Addr<ChatServer>,
    pub heartbeat: Instant,
    pub email: Email,
}

impl WsChatSession {
    pub fn new(addr: Addr<ChatServer>, room: String, email: String) -> Self {
        info!("new session with room {} and email {}", room, email);

        WsChatSession {
            id: Uuid::new_v4().to_string(),
            heartbeat: Instant::now(),
            room,
            email,
            addr,
        }
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
                ctx.notify(Packet {
                    data: Arc::new(msg.to_vec()),
                });
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
