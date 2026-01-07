/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use std::sync::Arc;

use crate::actors::chat_session::{RoomId, SessionId};

use super::session::Message;
use actix::{Message as ActixMessage, Recipient};

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct ClientMessage {
    pub session: SessionId,
    pub user: String,
    pub room: RoomId,
    pub msg: Packet,
}

#[derive(ActixMessage)]
#[rtype(result = "Result<(), String>")]
pub struct JoinRoom {
    pub session: SessionId,
    pub room: RoomId,
    pub user_id: String,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Connect {
    pub id: SessionId,
    pub addr: Recipient<Message>,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Packet {
    pub data: Arc<Vec<u8>>,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Disconnect {
    pub session: SessionId,
    pub room: RoomId,
    pub user_id: String,
}

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Leave {
    pub session: SessionId,
    pub room: RoomId,
    pub user_id: String,
}
