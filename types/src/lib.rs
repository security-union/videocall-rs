pub mod protos;

use protobuf::Message;
use yew_websocket::websocket::{Binary, Text};

impl std::fmt::Display for protos::media_packet::media_packet::MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            protos::media_packet::media_packet::MediaType::AUDIO => write!(f, "audio"),
            protos::media_packet::media_packet::MediaType::VIDEO => write!(f, "video"),
            protos::media_packet::media_packet::MediaType::SCREEN => write!(f, "screen"),
            protos::media_packet::media_packet::MediaType::HEARTBEAT => write!(f, "heartbeat"),
        }
    }
}

impl std::fmt::Display for protos::media_management_packet::media_management_packet::EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            protos::media_management_packet::media_management_packet::EventType::SUBSCRIBE => {
                write!(f, "SUBSCRIBE")
            }
            protos::media_management_packet::media_management_packet::EventType::UNSUBSCRIBE => {
                write!(f, "UNSUBSCRIBE")
            }
            protos::media_management_packet::media_management_packet::EventType::ONGOING_STREAM => {
                write!(f, "ONGOING_STREAM")
            }
        }
    }
}

impl std::fmt::Display for protos::packet_wrapper::packet_wrapper::PacketType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            protos::packet_wrapper::packet_wrapper::PacketType::AES_KEY => write!(f, "AES_KEY"),
            protos::packet_wrapper::packet_wrapper::PacketType::RSA_PUB_KEY => {
                write!(f, "RSA_PUB_KEY")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::MEDIA_OPTIONAL => {
                write!(f, "MEDIA_OPTIONAL")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::MEDIA_MANDATORY => {
                write!(f, "MEDIA_MANDATORY")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::MEDIA_MANAGEMENT => {
                write!(f, "MEDIA_MANAGEMENT")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::CONNECTION => {
                write!(f, "CONNECTION")
            }
        }
    }
}

impl From<Text> for protos::packet_wrapper::PacketWrapper {
    fn from(t: Text) -> Self {
        protos::packet_wrapper::PacketWrapper::parse_from_bytes(&t.unwrap().into_bytes()).unwrap()
    }
}

impl From<Binary> for protos::packet_wrapper::PacketWrapper {
    fn from(bin: Binary) -> Self {
        protos::packet_wrapper::PacketWrapper::parse_from_bytes(&bin.unwrap()).unwrap()
    }
}
