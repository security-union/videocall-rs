use crate::messages::{
    server::{ClientMessage, Connect, Disconnect, JoinRoom, Leave},
    session::Message,
};

use actix::{Actor, Context, Handler, MessageResult, Recipient};
use std::collections::{HashMap, HashSet};
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

    fn is_empty(&self, room_id: &RoomId) -> bool {
        self.rooms
            .get(room_id)
            .map(|sessions| sessions.is_empty())
            .unwrap_or(false)
    }

    pub fn send_message(
        &self,
        room: &RoomId,
        message: &MediaPacket,
        skip_id: &String,
        user: Option<String>,
    ) {
        self.rooms.get(room).map(|sessions| {
            sessions.iter().for_each(|id| {
                if id != skip_id {
                    self.sessions.get(id).map(|addr| {
                        addr.do_send(Message {
                            nickname: user.clone(),
                            msg: message.clone(),
                        })
                    });
                }
            });
        });
    }

    pub fn leave_rooms(&mut self, session_id: &SessionId) {
        let mut rooms = Vec::new();
        // remove session from all rooms
        for (id, sessions) in &mut self.rooms {
            if sessions.remove(session_id) {
                rooms.push(id.to_owned());
            }
        }
        // send message to other users
        // for room in rooms {
        //     self.send_message(&room, "Someone disconnected", &session_id, None);
        //     if self.is_empty(&room) {
        //         self.rooms.remove(&room);
        //     }
        // }
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
        self.send_message(&room, &msg.media_packet, &session, Some(user));
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

        // TODO lazily create room:
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
            // .map(|_| self.send_message(&room, "Someone connected", &session, None))
            .ok_or("The room doesn't exists".into());

        MessageResult(result)
    }
}
