use crate::messages::{
    server::{ClientMessage, Connect, Disconnect, JoinRoom, Leave},
    session::Message,
};

use actix::{Actor, Context, Handler, MessageResult, Recipient};
use log::{debug, info};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use types::protos::media_packet::MediaPacket;

use super::chat_session::{RoomId, SessionId};

pub struct ChatServer {
    sessions: HashMap<SessionId, Recipient<Message>>,
    rooms: HashMap<RoomId, HashSet<SessionId>>,
}

impl ChatServer {
    pub fn new() -> Self {
        ChatServer {
            sessions: HashMap::new(),
            rooms: HashMap::new(),
        }
    }

    pub fn send_message(
        &self,
        room: &RoomId,
        message: Arc<MediaPacket>,
        skip_id: &String,
        user: Arc<Option<String>>,
    ) {
        if let Some(sessions) = self.rooms.get(room) {
            sessions.iter().for_each(|id| {
                if id != skip_id {
                    if let Some(addr) = self.sessions.get(id) {
                        addr.do_send(Message {
                            nickname: user.clone(),
                            msg: message.clone(),
                        })
                    }
                }
            });
        }
    }

    pub fn leave_rooms(&mut self, session_id: &SessionId) {
        let mut rooms = Vec::new();
        // remove session from all rooms
        for (id, sessions) in &mut self.rooms {
            if sessions.remove(session_id) {
                rooms.push(id.to_owned());
            }
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
        let nickname = Arc::new(Some(user));
        self.send_message(&room, msg.media_packet, &session, nickname);
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

        if !self.rooms.contains_key(&room) {
            self.rooms.insert(
                room.clone(),
                vec![session.clone()]
                    .into_iter()
                    .collect::<HashSet<String>>(),
            );
        }

        let result: Result<(), String> = self
            .rooms
            .get_mut(&room)
            .map(|sessions| sessions.insert(session.clone()))
            .map(|_| ())
            .ok_or("The room doesn't exists".into());
        info!(
            "someone connected to room {} with session {} result {:?}",
            room.clone(),
            session.clone().trim(),
            result
        );
        MessageResult(result)
    }
}
