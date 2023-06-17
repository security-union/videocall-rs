use actix::Addr;

use crate::actors::chat_server::ChatServer;

pub struct AppState {
    pub chat: Addr<ChatServer>,
}

pub struct AppConfig {
    pub oauth_client_id: String,
    pub oauth_secret: String,
    pub oauth_redirect_url: String,
    pub oauth_auth_url: String,
    pub oauth_token_url: String,
    pub after_login_url: String,
}
