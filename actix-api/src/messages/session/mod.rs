use std::sync::Arc;

use actix::Message as ActixMessage;
use types::protos::media_packet::MediaPacket;

#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Message {
    pub nickname: Arc<Option<String>>,
    pub msg: Arc<MediaPacket>,
}
