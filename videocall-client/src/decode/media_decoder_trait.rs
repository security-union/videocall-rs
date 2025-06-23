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

use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;
use web_sys::CodecState;

/// A general trait for media decoders (audio and video)
pub trait MediaDecoderTrait {
    /// Type of initialization parameters required for this decoder
    type InitType;

    /// Type of configuration parameters required for this decoder
    type ConfigType;

    /// Create a new decoder instance
    fn new(init: &Self::InitType) -> Result<Self, JsValue>
    where
        Self: Sized;

    /// Configure the decoder with codec-specific settings
    fn configure(&self, config: &Self::ConfigType) -> Result<(), JsValue>;

    /// Decode a media packet
    fn decode(&self, packet: Arc<MediaPacket>) -> Result<(), JsValue>;

    /// Get the current state of the decoder
    fn state(&self) -> CodecState;

    /// Get the sequence number from a packet
    fn get_sequence_number(&self, packet: &MediaPacket) -> u64;

    /// Determine if a packet contains a keyframe
    fn is_keyframe(&self, packet: &MediaPacket) -> bool;
}
