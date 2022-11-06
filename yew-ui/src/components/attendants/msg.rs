use crate::model::MediaPacketWrapper;
use types::protos::media_packet::MediaPacket;
use web_sys::AudioData;

// This is important https://plnkr.co/edit/1yQd8ozGXlV9bwK6?preview
// https://github.com/WebAudio/web-audio-api-v2/issues/133
#[derive(Debug)]
pub enum WsAction {
    Connect,
    Connected,
    Disconnect,
    Lost,
}

pub enum Msg {
    WsAction(WsAction),
    OnInboundMedia(MediaPacketWrapper),
    OnOutboundVideoPacket(MediaPacket),
    OnOutboundAudioPacket(AudioData),
}

impl From<WsAction> for Msg {
    fn from(action: WsAction) -> Self {
        Msg::WsAction(action)
    }
}
