use crate::messages::{
    server::{ClientMessage, Connect, CreateRoom, Disconnect, JoinRoom, Leave},
    session::Message,
};
use crate::models::{RoomId, SessionId};
use actix::{Actor, Context, Handler, MessageResult, Recipient};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

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

    fn is_empty(&self, room_id: &Uuid) -> bool {
        self.rooms
            .get(&room_id)
            .map(|sessions| sessions.is_empty())
            .unwrap_or(false)
    }

    pub fn send_message(&self, room: &Uuid, message: &str, skip_id: &Uuid, user: Option<String>) {
        self.rooms.get(room).map(|sessions| {
            sessions.iter().for_each(|id| {
                if id != skip_id {
                    self.sessions.get(id).map(|addr| {
                        addr.do_send(Message {
                            nickname: user.clone(),
                            msg: message.into(),
                        })
                    });
                }
            });
        });
    }

    pub fn leave_rooms(&mut self, session_id: &Uuid) {
        let mut rooms = Vec::new();
        // remove session from all rooms
        for (id, sessions) in &mut self.rooms {
            if sessions.remove(&session_id) {
                rooms.push(id.to_owned());
            }
        }
        // send message to other users
        for room in rooms {
            self.send_message(&room, "Someone disconnected", &session_id, None);
            if self.is_empty(&room) {
                self.rooms.remove(&room);
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
        self.send_message(&room, &msg, &session, Some(user));
    }
}

impl Handler<CreateRoom> for ChatServer {
    type Result = MessageResult<CreateRoom>;

    fn handle(&mut self, msg: CreateRoom, _ctx: &mut Self::Context) -> Self::Result {
        let CreateRoom { session } = msg;
        let room_id = RoomId::new_v4();
        self.leave_rooms(&session);
        self.rooms.insert(
            room_id,
            vec![session].into_iter().collect::<HashSet<Uuid>>(),
        );
        MessageResult(room_id)
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

        let result: Result<(), String> = self
            .rooms
            .get_mut(&room)
            .map(|sessions| sessions.insert(session))
            .map(|_| self.send_message(&room, "Someone connected", &session, None))
            .ok_or("The room doesn't exists".into());

        MessageResult(result)
    }
}
