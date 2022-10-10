pub mod command;
pub mod wsmessage;
use actix::Message as ActixMessage;
use types::protos::media_packet::MediaPacket;

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Message {
    pub nickname: Option<String>,
    pub msg: MediaPacket,
}
