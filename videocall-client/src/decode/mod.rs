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

pub mod audio_decoder_wrapper;
pub mod config;
pub mod hash_map_with_ordered_keys;
pub mod media_decoder_trait;
pub mod neteq_audio_decoder;
pub mod peer_decode_manager;
pub mod peer_decoder;
#[cfg(not(feature = "neteq_ff"))]
pub mod safari;
pub mod video_decoder_wrapper;
#[cfg(not(feature = "neteq_ff"))]
use safari::audio_decoder::{
    AudioPeerDecoder as SafariAudioPeerDecoder, PeerDecode as SafariPeerDecodeTrait,
};

pub use peer_decode_manager::{PeerDecodeManager, PeerStatus};
pub use peer_decoder::VideoPeerDecoder;

use crate::audio::SharedPeerAudioDecoder;
#[cfg(feature = "neteq_ff")]
use neteq_audio_decoder::NetEqAudioPeerDecoder;
use peer_decoder::{
    DecodeStatus as StandardDecodeStatus, PeerDecode as StandardPeerDecodeTrait,
    StandardAudioPeerDecoder,
};
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;

// Common DecodeStatus for the trait
#[derive(Debug, Clone, Copy)]
pub struct DecodeStatus {
    pub rendered: bool,
    pub first_frame: bool,
}

impl From<StandardDecodeStatus> for DecodeStatus {
    fn from(status: StandardDecodeStatus) -> Self {
        DecodeStatus {
            rendered: status._rendered, // Note the underscore, matches StandardAudioPeerDecoder's field
            first_frame: status.first_frame,
        }
    }
}

/// Trait to abstract over different audio peer decoder implementations
pub trait AudioPeerDecoderTrait {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus>;
    fn flush(&mut self);
    fn set_muted(&mut self, muted: bool);
    fn is_muted(&self) -> bool {
        false
    } // Default implementation
    fn set_volume(&mut self, _volume: f32) {} // Default implementation
    fn get_volume(&self) -> f32 {
        1.0
    } // Default implementation
}

// Implement trait for standard audio peer decoder
impl AudioPeerDecoderTrait for StandardAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        StandardPeerDecodeTrait::decode(self, packet).map(|status| status.into())
    }

    fn flush(&mut self) {
        // For standard decoder, we can flush the decoder state
        if let Err(e) = self.decoder.flush() {
            log::error!("Failed to flush standard audio decoder: {e:?}");
        }
    }

    fn set_muted(&mut self, _muted: bool) {
        // Standard decoder doesn't support muting at the decoder level
        log::debug!("set_muted called on standard audio decoder (no-op)");
    }
}

#[cfg(not(feature = "neteq_ff"))]
impl AudioPeerDecoderTrait for SafariAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        let decode_status = SafariPeerDecodeTrait::decode(self, packet)?;
        Ok(DecodeStatus {
            rendered: decode_status.rendered,
            first_frame: decode_status.first_frame,
        })
    }

    fn flush(&mut self) {
        // For Safari decoder, we can flush the worklet - but we don't have direct access
        // to the decoder field, so we'll just log for now
        log::debug!("Flush called on Safari audio decoder");
    }

    fn set_muted(&mut self, _muted: bool) {
        // Safari decoder doesn't support muting at the decoder level
        log::debug!("set_muted called on Safari audio decoder (no-op)");
    }
}

#[cfg(feature = "neteq_ff")]
/// Factory function to create the appropriate audio peer decoder based on platform detection
pub fn create_audio_peer_decoder(
    speaker_device_id: Option<String>,
    peer_id: String,
) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
    // NetEq decoders should start muted by default (peers start with audio_enabled=false)
    NetEqAudioPeerDecoder::new_with_mute_state(speaker_device_id, peer_id, true)
}

#[cfg(not(feature = "neteq_ff"))]
pub fn create_audio_peer_decoder(
    speaker_device_id: Option<String>,
    peer_id: String,
) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
    // 🚀 REVOLUTION: Use the shared audio decoder for optimal performance!
    log::info!("🚀 Creating revolutionary shared audio decoder for peer: {peer_id}");

    // The SharedPeerAudioDecoder automatically handles the shared context
    wasm_bindgen_futures::spawn_local(async move {
        match SharedPeerAudioDecoder::new(speaker_device_id, peer_id, true).await {
            Ok(decoder) => {
                log::info!("✅ Shared audio decoder created successfully");
                // This will be returned in the future
            }
            Err(e) => {
                log::error!("❌ Failed to create shared audio decoder: {:?}", e);
                // Fallback to old system if needed
            }
        }
    });

    // For now, we'll return the old decoder while the async creation happens
    // TODO: Refactor to fully async factory function
    use crate::utils::is_ios;
    if is_ios() {
        log::info!(
            "Platform detection: Using Safari (AudioWorklet) audio peer decoder for iOS device"
        );
        Ok(Box::new(SafariAudioPeerDecoder::new_with_speaker(
            speaker_device_id,
        )))
    } else {
        log::info!("Platform detection: Using standard (AudioDecoder API) audio peer decoder");
        StandardAudioPeerDecoder::new(speaker_device_id)
            .map(|decoder| Box::new(decoder) as Box<dyn AudioPeerDecoderTrait>)
    }
}

/// 🚀 Revolutionary factory function for creating shared audio peer decoders
///
/// This is the Fame Labs Inc. breakthrough function that creates audio decoders
/// using the shared AudioContext system instead of individual contexts per peer.
/// This function is async to properly initialize the shared system.
pub async fn create_shared_audio_peer_decoder(
    speaker_device_id: Option<String>,
    peer_id: String,
    initial_muted: bool,
) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
    log::info!("🌟 Creating Fame Labs revolutionary shared audio decoder for peer: {peer_id}");

    SharedPeerAudioDecoder::new(speaker_device_id, peer_id, initial_muted).await
}

// No need to re-export PeerDecode or VideoPeerDecodeTrait here as they are specific to their implementations.
// VideoPeerDecoder is re-exported above for direct use.
