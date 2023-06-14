use crate::messages::{
    server::{ClientMessage, Connect, Disconnect, JoinRoom, Leave},
    session::Message,
};

use actix::{Actor, Context, Handler, MessageResult, Recipient};
use log::{debug, info};
use protobuf::Message as ProtobufMessage;
use std::{
    collections::HashMap,
    sync::Arc,
};
use types::protos::media_packet::MediaPacket;

use super::chat_session::{RoomId, SessionId};

pub struct ChatServer {
    nc: nats::Connection,
    sessions: HashMap<SessionId, Recipient<Message>>,
    active_subs: HashMap<SessionId, nats::Handler>,
}

impl ChatServer {
    pub fn new() -> Self {
        let nc = nats::Options::new().with_name("actix-api").connect(std::env::var("NATS_URL").expect("NATS_URL env var must be defined")).unwrap();
        ChatServer {
            nc,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    pub fn send_message(
        &self,
        room: &RoomId,
        message: Arc<MediaPacket>,
        skip_id: &String,
        user: String,
    ) {
        let subject = format!("room.{}.{}", room, user);
        self.nc.publish(&subject, message.write_to_bytes().unwrap()).unwrap();
    }

    pub fn leave_rooms(&mut self, session_id: &SessionId) {
        if let Some(sub) = self.active_subs.remove(session_id) {
            sub.unsubscribe().unwrap();
        }
    }
}

impl Actor for ChatServer {
    type Context = Context<Self>;
}

impl Handler<Connect> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: Connect, _ctx: &mut Self::Context) -> Self::Result {
        let Connect { id, addr } = msg;
        self.sessions.insert(id, addr);
    }
}

impl Handler<Disconnect> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        Disconnect { session }: Disconnect,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.leave_rooms(&session);
        let _ = self.sessions.remove(&session);
    }
}

impl Handler<Leave> for ChatServer {
    type Result = ();

    fn handle(&mut self, Leave { session }: Leave, _ctx: &mut Self::Context) -> Self::Result {
        self.leave_rooms(&session);
    }
}

impl Handler<ClientMessage> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: ClientMessage, _ctx: &mut Self::Context) -> Self::Result {
        let ClientMessage {
            session,
            user,
            room,
            msg,
        } = msg;
        debug!("got message in server room {} session {}", room, session);
        self.send_message(&room, msg.media_packet, &session, user);
    }
}

impl Handler<JoinRoom> for ChatServer {
    type Result = MessageResult<JoinRoom>;
    fn handle(
        &mut self,
        JoinRoom { session, room }: JoinRoom,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.leave_rooms(&session);

        let subject = format!("room.{}.*", room);
        let queue = format!("{}-{}", session, room);
        let session_recipient = self.sessions.get(&session).unwrap().clone();
        let session_clone = session.clone();
        let sub = self.nc.queue_subscribe(&subject, &queue).unwrap().with_handler(move |msg| {
            let msg = Message {
                nickname: Arc::new(None),
                msg: Arc::new(MediaPacket::parse_from_bytes(&msg.data).unwrap()),
            };
            match session_recipient.try_send(msg) {
                Ok(_) => Ok(()),
                Err(e) => {
                    log::error!("error sending message to session: {}", e);
                    Ok(())
                }
            }
        });
        debug!("Subscribed to subject {} with queue {}", subject, queue);

        let result = self.active_subs.insert(session.clone(), sub).map(|_| ()).ok_or("The session is already subscribed".into());
        info!(
            "someone connected to room {} with session {} result {:?}",
            room.clone(),
            session.clone().trim(),
            result
        );
        MessageResult(result)
    }
}
