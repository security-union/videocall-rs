use actix::Message as ActixMessage;
use bytestring::ByteString;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::convert::Into;

#[derive(Serialize, Deserialize, ActixMessage)]
#[rtype(result = "()")]
pub struct WsMessage {
    pub ty: MessageType,
    pub data: Value,
}

impl WsMessage {
    pub fn err(msg: String) -> Self {
        WsMessage {
            ty: MessageType::Err,
            data: json!(msg),
        }
    }

    pub fn info(msg: String) -> Self {
        WsMessage {
            ty: MessageType::Info,
            data: json!(msg),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub enum MessageType {
    Join,
    Leave,
    Msg,
    Err,
    Info,
}

// impl Into<ByteString> for WsMessage {
//     fn into(self) -> ByteString {
//         ByteString::from(serde_json::to_string(&self).unwrap())
//     }
// }
