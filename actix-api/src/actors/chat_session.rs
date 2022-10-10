use crate::messages;
use crate::messages::server::MediaPacketUpdate;
use crate::{actors::chat_server::ChatServer, constants::CLIENT_TIMEOUT};

use crate::{
    constants::HEARTBEAT_INTERVAL,
    messages::server::{ClientMessage, Connect, Disconnect, JoinRoom, Leave},
};
use actix::ActorFutureExt;
use actix::{
    clock::Instant, fut, ActorContext, ContextFutureSpawner, Handler, Running, StreamHandler,
    WrapFuture,
};
use actix::{Actor, Addr, AsyncContext};
use actix_web_actors::ws::{self, WebsocketContext};
use log::{error, info};
use protobuf::Message;
use serde_json::json;
use std::str::FromStr;
use types::protos::media_packet::MediaPacket;
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
        let session = WsChatSession {
            id: Uuid::new_v4().to_string(),
            heartbeat: Instant::now(),
            room,
            email,
            addr,
        };

        session
    }

    fn heartbeat(&self, ctx: &mut WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                // heartbeat timed out
                println!("Websocket Client heartbeat failed, disconnecting!");
                // notify chat server
                act.addr.do_send(Disconnect { session: act.id });
                // stop actor
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
                    // ctx.text(WsMessage::err(err.to_string()));
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);
        self.join(self.room, ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        // notify chat server
        self.addr.do_send(Disconnect {
            session: self.id.clone(),
        });
        Running::Stop
    }
}

impl Handler<messages::session::Message> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: messages::session::Message, ctx: &mut Self::Context) -> Self::Result {
        let media_packet = msg.msg.write_to_bytes().unwrap();
        ctx.binary(media_packet);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsChatSession {
    fn handle(&mut self, item: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(msg) => msg,
            Err(err) => {
                // ctx.text(WsMessage::err(err.to_string()));
                ctx.stop();
                return;
            }
        };

        match msg {
            ws::Message::Binary(msg) => {
                let message: protobuf::Result<MediaPacket> =
                    protobuf::Message::parse_from_bytes(&msg);
                match message {
                    Ok(mediaPacket) => ctx.notify(MediaPacketUpdate { mediaPacket }),
                    Err(err) => {
                        error!("error {:?}", err);
                        // ctx.text(WsMessage::err(err.to_string()))
                    }
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
                ctx.stop();
            }
            _ => (),
        }
    }

    fn started(&mut self, ctx: &mut Self::Context) {}

    fn finished(&mut self, ctx: &mut Self::Context) {
        ctx.stop()
    }
}

// impl Handler<WsMessage> for WsChatSession {
//     type Result = ();

//     fn handle(&mut self, msg: WsMessage, ctx: &mut Self::Context) -> Self::Result {
//         let data = msg.data.as_str().unwrap();
//         match msg.ty {
//             MessageType::Msg => self.msg(data.into(), ctx),
//             MessageType::Leave => self.leave(ctx),
//             _ => (),
//         }
//     }
// }

impl WsChatSession {
    fn join(&self, room_id: String, ctx: &mut WebsocketContext<Self>) {
        let join_room = self.addr.send(JoinRoom {
            room: room_id,
            session: self.id.clone(),
        });
        let join_room = join_room.into_actor(self);
        join_room
            .then(move |response, act, ctx| {
                match response {
                    Ok(res) if res.is_ok() => {
                        act.room = room_id.clone();
                        // ctx.text(WsMessage {
                        //     ty: MessageType::Msg,
                        //     data: json!("Joined!"),
                        // })
                    }
                    Ok(res) => {
                        error!("error {:?}", res);
                        // ctx.text(WsMessage::err(res.unwrap_err().to_string())),
                    }
                    Err(err) => {
                        // ctx.text(WsMessage::err(err.to_string()));
                        ctx.stop();
                    }
                }
                fut::ready(())
            })
            .wait(ctx);
    }

    // fn msg(&self, msg: String, ctx: &mut WebsocketContext<Self>) {
    //     match Command::from_str(&msg) {
    //         Ok(cmd) => ctx.notify(cmd),
    //         Err(err) => {
    //             error!("error {:?}", err);
    //             // ctx.text(WsMessage::err(err.to_string())),
    //         }
    //     }
    // }

    fn leave(&self, ctx: &mut WebsocketContext<Self>) {
        self.addr
            .send(Leave {
                session: self.id.clone(),
            })
            .into_actor(self)
            .then(move |res, act, ctx| {
                match res {
                    Ok(_) => {
                        info!("info leaved");
                        // ctx.text(WsMessage::info("Room leaved".into()))
                    }
                    // something is wrong with chat server
                    Err(err) => {
                        // ctx.text(WsMessage::err(err.to_string()));
                        ctx.stop();
                    }
                }
                fut::ready(())
            })
            .wait(ctx);
    }
}

impl Handler<MediaPacketUpdate> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: MediaPacketUpdate, ctx: &mut Self::Context) -> Self::Result {
        let room_id = self.room.clone();
        self.addr.do_send(ClientMessage {
            session: self.id.clone(),
            user: self.email.clone(),
            room: room_id,
            msg,
        });
    }
}
