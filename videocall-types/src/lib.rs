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

pub mod callback;
pub mod feature_flags;
pub mod protos;

pub use callback::Callback;
pub use feature_flags::FeatureFlags;
use protobuf::Message;

/// A representation of a value which can be stored and restored as a text.
pub type Text = Result<String, anyhow::Error>;

/// A representation of a value which can be stored and restored as a binary.
pub type Binary = Result<Vec<u8>, anyhow::Error>;

/// System username used for server-generated messages (meeting info, meeting started/ended).
/// This is not a real user and should be filtered out in UI/peer management.
pub const SYSTEM_USER_EMAIL: &str = "system-&^%$#@!";

impl std::fmt::Display for protos::media_packet::media_packet::MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            protos::media_packet::media_packet::MediaType::MEDIA_TYPE_UNKNOWN => {
                write!(f, "UNKNOWN")
            }
            protos::media_packet::media_packet::MediaType::AUDIO => write!(f, "audio"),
            protos::media_packet::media_packet::MediaType::VIDEO => write!(f, "video"),
            protos::media_packet::media_packet::MediaType::SCREEN => write!(f, "screen"),
            protos::media_packet::media_packet::MediaType::HEARTBEAT => write!(f, "heartbeat"),
            protos::media_packet::media_packet::MediaType::RTT => write!(f, "rtt"),
        }
    }
}

impl std::fmt::Display for protos::packet_wrapper::packet_wrapper::PacketType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            protos::packet_wrapper::packet_wrapper::PacketType::PACKET_TYPE_UNKNOWN => {
                write!(f, "UNKNOWN")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::AES_KEY => write!(f, "AES_KEY"),
            protos::packet_wrapper::packet_wrapper::PacketType::RSA_PUB_KEY => {
                write!(f, "RSA_PUB_KEY")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::MEDIA => write!(f, "MEDIA"),
            protos::packet_wrapper::packet_wrapper::PacketType::CONNECTION => {
                write!(f, "CONNECTION")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::DIAGNOSTICS => {
                write!(f, "DIAGNOSTICS")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::HEALTH => {
                write!(f, "HEALTH")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::MEETING => {
                write!(f, "MEETING")
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

pub fn truthy(s: Option<&str>) -> bool {
    if let Some(s) = s {
        ["true".to_string(), "1".to_string()].contains(&s.to_lowercase())
    } else {
        false
    }
}
