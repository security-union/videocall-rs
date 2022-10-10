use crate::actors::chat_session::{RoomId, SessionId};

use super::session::Message;
use actix::{Message as ActixMessage, Recipient};
use types::protos::media_packet::MediaPacket;

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct ClientMessage {
    pub session: SessionId,
    pub user: String,
    pub room: RoomId,
    pub msg: MediaPacketUpdate,
}

#[derive(ActixMessage)]
#[rtype(result = "Result<(), String>")]
pub struct JoinRoom {
    pub session: SessionId,
    pub room: RoomId,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Connect {
    pub id: SessionId,
    pub addr: Recipient<Message>,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct MediaPacketUpdate {
    pub mediaPacket: MediaPacket,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Disconnect {
    pub session: SessionId,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Leave {
    pub session: SessionId,
}
