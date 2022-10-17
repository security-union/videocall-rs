use actix::Addr;

use crate::actors::chat_server::ChatServer;

pub struct AppState {
    pub chat: Addr<ChatServer>,
}
