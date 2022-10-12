use actix::Message as ActixMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, ActixMessage)]
#[rtype(result = "()")]
pub struct WsMessage {
    pub ty: MessageType,
    pub data: Value,
}

#[derive(Serialize, Deserialize)]
pub enum MessageType {
    Join,
    Leave,
    Msg,
    Err,
    Info,
}
