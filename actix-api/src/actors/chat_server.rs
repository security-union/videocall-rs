use crate::messages::{
    server::{ClientMessage, Connect, Disconnect, JoinRoom, Leave},
    session::Message,
};

use actix::{Actor, Context, Handler, MessageResult, Recipient};
use log::{debug, error, info, trace};
use protobuf::Message as ProtobufMessage;
use std::{collections::HashMap, sync::Arc};
use types::protos::media_packet::MediaPacket;

use super::chat_session::{RoomId, SessionId};

pub struct ChatServer {
    nats_connection: nats::Connection,
    sessions: HashMap<SessionId, Recipient<Message>>,
    active_subs: HashMap<SessionId, nats::Handler>,
}

impl ChatServer {
    pub fn new() -> Self {
        let nc = nats::Options::new()
            .with_name("actix-api")
            .connect(std::env::var("NATS_URL").expect("NATS_URL env var must be defined"))
            .unwrap();
        ChatServer {
            nats_connection: nc,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    pub fn send_message(&self, room: &RoomId, message: Arc<MediaPacket>, session_id: SessionId) {
        let subject = format!("room.{}.{}", room, session_id);
        if let Ok(message) = message.write_to_bytes() {
            match self.nats_connection.publish(&subject, message) {
                Ok(_) => trace!("published message to {}", subject),
                Err(e) => error!("error publishing message to {}: {}", subject, e),
            }
        }
    }

    pub fn leave_rooms(&mut self, session_id: &SessionId) {
        if let Some(sub) = self.active_subs.remove(session_id) {
            let _ = sub.unsubscribe();
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
            room,
            msg,
            user: _,
        } = msg;
        trace!("got message in server room {} session {}", room, session);
        self.send_message(&room, msg.media_packet, session);
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

        let (subject, queue) = build_subject_and_queue(&room, &session);
        let session_recipient = match self.sessions.get(&session) {
            Some(recipient) => recipient.clone(),
            None => {
                let err = format!("session {} is not connected", session);
                error!("{}", err);
                return MessageResult(Err(err));
            }
        };

        let sub = match self
            .nats_connection
            .queue_subscribe(&subject, &queue)
            .map_err(|e| handle_subscription_error(e, &subject))
        {
            Ok(sub) => sub,
            Err(e) => return MessageResult(Err(e)),
        };

        let handler = sub.with_handler(build_handler(
            session_recipient,
            room.clone(),
            session.clone(),
        ));

        debug!("Subscribed to subject {} with queue {}", subject, queue);

        let result = self
            .active_subs
            .insert(session.clone(), handler)
            .map(|_| ())
            .ok_or("The session is already subscribed".into());

        info!(
            "someone connected to room {} with session {} result {:?}",
            room,
            session.trim(),
            result
        );

        MessageResult(result)
    }
}

fn build_subject_and_queue(room: &str, session: &str) -> (String, String) {
    (format!("room.{}.*", room), format!("{}-{}", session, room))
}

fn handle_subscription_error(e: impl std::fmt::Display, subject: &str) -> String {
    let err = format!("error subscribing to subject {}: {}", subject, e);
    error!("{}", err);
    err
}

fn build_handler(
    session_recipient: Recipient<Message>, // Assuming Recipient is a type
    room: String,
    session: String,
) -> impl Fn(nats::Message) -> Result<(), std::io::Error> {
    move |msg| {
        if msg.subject == format!("room.{}.{}", room, session) {
            return Ok(());
        }

        let media_packet = match MediaPacket::parse_from_bytes(&msg.data) {
            Ok(media_packet) => media_packet,
            Err(e) => {
                error!("error parsing message: {}", e);
                return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
            }
        };

        let message = Message {
            nickname: Arc::new(Some(media_packet.email.clone())),
            msg: Arc::new(media_packet),
        };

        session_recipient.try_send(message).map_err(|e| {
            error!("error sending message to session {}: {}", session, e);
            std::io::Error::new(std::io::ErrorKind::Other, e)
        })
    }
}
