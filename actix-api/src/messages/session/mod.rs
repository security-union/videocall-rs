pub mod command;
use actix::Message as ActixMessage;
use types::protos::rust::media_packet::MediaPacket;

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Message {
    pub nickname: Option<String>,
    pub msg: MediaPacket,
}
