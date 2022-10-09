use crate::{
    actors::chat_server::ChatServer,
    constants::CLIENT_TIMEOUT,
    models::{RoomId, SessionId, UserInfo},
};
use crate::{
    constants::HEARTBEAT_INTERVAL,
    messages::{
        server::{ClientMessage, Connect, CreateRoom, Disconnect, JoinRoom, Leave},
        session::{
            command::Command,
            wsmessage::{MessageType, WsMessage},
            Message,
        },
    },
};
use actix::{
    clock::Instant, fut, ActorContext, ActorFuture, ContextFutureSpawner, Handler, Running,
    StreamHandler, WrapFuture,
};
use actix::{Actor, Addr, AsyncContext};
use actix_web_actors::ws::{self, WebsocketContext};
use serde_json::json;
use std::str::FromStr;
use uuid::Uuid;

pub struct WsChatSession {
    pub id: SessionId,
    pub room: Option<RoomId>,
    pub addr: Addr<ChatServer>,
    pub hb: Instant,
    pub user: UserInfo,
}

impl WsChatSession {
    pub fn new(addr: Addr<ChatServer>) -> Self {
        WsChatSession {
            id: Uuid::new_v4(),
            room: None,
            hb: Instant::now(),
            user: UserInfo::default(),
            addr,
        }
    }

    fn hb(&self, ctx: &mut WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.hb) > CLIENT_TIMEOUT {
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
        self.hb(ctx);
        let addr = ctx.address();
        self.addr
            .send(Connect {
                id: self.id.clone(),
                addr: addr.recipient(),
            })
            .into_actor(self)
            .then(|res, _act, ctx| {
                if let Err(err) = res {
                    ctx.text(WsMessage::err(err.to_string()));
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);
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
        ctx.text(WsMessage {
            ty: MessageType::Msg,
            data: json!(msg),
        });
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsChatSession {
    fn handle(&mut self, item: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(msg) => msg,
            Err(err) => {
                ctx.text(WsMessage::err(err.to_string()));
                ctx.stop();
                return;
            }
        };

        match msg {
            ws::Message::Text(msg) => match serde_json::from_str::<WsMessage>(&msg) {
                Ok(content) => ctx.notify(content),
                Err(err) => ctx.text(WsMessage::err(err.to_string())),
            },
            ws::Message::Ping(msg) => {
                self.hb = Instant::now();
                ctx.pong(&msg);
            }
            ws::Message::Pong(_) => {
                self.hb = Instant::now();
            }
            ws::Message::Close(reason) => {
                ctx.close(reason);
                ctx.stop();
            }
            _ => (),
        }
    }
}

impl Handler<WsMessage> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: WsMessage, ctx: &mut Self::Context) -> Self::Result {
        let data = msg.data.as_str().unwrap();
        match msg.ty {
            MessageType::Create => self.create(ctx),
            MessageType::Join => match Uuid::from_str(&data) {
                Ok(uuid) => self.join(uuid, ctx),
                Err(err) => ctx.text(WsMessage::err(err.to_string())),
            },
            MessageType::Msg => self.msg(data.into(), ctx),
            MessageType::Leave => self.leave(ctx),
            _ => (),
        }
    }
}

impl WsChatSession {
    fn create(&self, ctx: &mut WebsocketContext<Self>) {
        let send_create = self.addr.send(CreateRoom {
            session: self.id.clone(),
        });
        let send_create = send_create.into_actor(self);
        send_create
            .then(move |res, act, ctx| {
                // Actor's state updated here
                match res {
                    Ok(res) => {
                        act.room = Some(res.clone());
                        ctx.text(WsMessage::info(res.to_string()));
                    }
                    // something is wrong with chat server
                    Err(err) => {
                        ctx.text(WsMessage::err(err.to_string()));
                        ctx.stop();
                    }
                }
                fut::ready(())
            })
            .wait(ctx);
    }

    fn join(&self, room_id: Uuid, ctx: &mut WebsocketContext<Self>) {
        let join_room = self.addr.send(JoinRoom {
            room: room_id,
            session: self.id.clone(),
        });
        let join_room = join_room.into_actor(self);
        join_room
            .then(move |response, act, ctx| {
                match response {
                    Ok(res) if res.is_ok() => {
                        act.room = Some(room_id.clone());
                        ctx.text(WsMessage {
                            ty: MessageType::Msg,
                            data: json!("Joined!"),
                        })
                    }
                    Ok(res) => ctx.text(WsMessage::err(res.unwrap_err().to_string())),
                    Err(err) => {
                        ctx.text(WsMessage::err(err.to_string()));
                        ctx.stop();
                    }
                }
                fut::ready(())
            })
            .wait(ctx);
    }

    fn msg(&self, msg: String, ctx: &mut WebsocketContext<Self>) {
        match Command::from_str(&msg) {
            Ok(cmd) => ctx.notify(cmd),
            Err(err) => ctx.text(WsMessage::err(err.to_string())),
        }
    }

    fn leave(&self, ctx: &mut WebsocketContext<Self>) {
        self.addr
            .send(Leave {
                session: self.id.clone(),
            })
            .into_actor(self)
            .then(move |res, act, ctx| {
                match res {
                    Ok(_) => {
                        act.room = None;
                        ctx.text(WsMessage::info("Room leaved".into()))
                    }
                    // something is wrong with chat server
                    Err(err) => {
                        ctx.text(WsMessage::err(err.to_string()));
                        ctx.stop();
                    }
                }
                fut::ready(())
            })
            .wait(ctx);
    }
}

impl Handler<Command> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Command, ctx: &mut Self::Context) -> Self::Result {
        if let None = self.room {
            ctx.text(WsMessage::err("You are not in a room".into()));
            return;
        }
        let room_id = self.room.clone().unwrap();
        match msg {
            Command::Msg(msg) => {
                self.addr.do_send(ClientMessage {
                    session: self.id.clone(),
                    user: self.user.nickname.clone(),
                    room: room_id,
                    msg,
                });
            }
            Command::SetName(name) => self.user.nickname = name,
            Command::GetRoomId => {
                ctx.text(WsMessage::info(room_id.to_string()));
            }
        }
    }
}
