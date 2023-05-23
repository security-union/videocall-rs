use crate::messages::{
    server::{ClientMessage, Connect, Disconnect, JoinRoom, Leave},
    session::Message,
};

use actix::{Actor, Context, Handler, MessageResult, Recipient};
use log::{debug, info};
use std::collections::{HashMap, HashSet};
use types::protos::media_packet::{MediaPacket, media_packet::CommandType};

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

    pub fn send_media_message(
        &self,
        room: RoomId,
        message: MediaPacket,
        skip_id: String,
        user: Option<String>,
    ) {
        self.rooms.get(&room).map(|sessions| {
            sessions.iter().for_each(|id| {
                if id != &skip_id {
                    self.sessions.get(id).map(|addr| {
                        addr.do_send(Message {
                            nickname: user.clone(),
                            msg: message.clone(),
                        });
                    });
                }
            });
        });
    }

    pub fn send_session_command(&self, command_type: CommandType, session_id: SessionId) {
        self.sessions.get(&session_id).map(|addr| {
            let mut msg = MediaPacket::new();
            msg.command_type = command_type.into();
            addr.do_send(Message { msg, nickname: None });
        });
    }

    pub fn leave_rooms(&mut self, session_id: &SessionId) {
        // remove session from all rooms
        for (_, sessions) in &mut self.rooms {
            sessions.remove(session_id);
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
        self.sessions.remove(&session);
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
        self.send_media_message(room, msg.media_packet, session, Some(user));
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

        // If the room doesn't exist, create it
        if !self.rooms.contains_key(&room) {
            self.rooms.insert(
                room.clone(),
                vec![session.clone()]
                    .into_iter()
                    .collect::<HashSet<String>>(),
            );
        }

        // If there are 5 or more sessions in the room, mute the new session
        if self.rooms.get(&room).unwrap().len() >= 5 {
            info!(
                "Room {} already has 5 attendants, automatically muting session {}",
                room, session
            );
            self.send_session_command(CommandType::MUTE, session.clone());
        }

        let result: Result<(), String> = self
            .rooms
            .get_mut(&room)
            .map(|sessions| sessions.insert(session.clone()))
            .map(|_| ())
            .ok_or("The room doesn't exist".into());
        info!(
            "someone connected to room {} with session {} result {:?}",
            room, session, result
        );
        MessageResult(result)
    }
}
