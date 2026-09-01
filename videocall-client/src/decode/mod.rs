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
pub mod layer_chooser;
pub mod media_decoder_trait;
pub mod neteq_audio_decoder;
pub mod peer_decode_manager;
pub mod peer_decoder;
pub mod pli_budget;
pub mod video_decoder_wrapper;

pub use layer_chooser::{
    max_layers_for_kind, quality_state, size_cap_layer, DegradeReason, KindLayerBounds,
    PrefMediaKind, QualityState, ReceiveLayerBounds, ReceivedLayerSnapshot, TileHint,
};
pub use peer_decode_manager::{PeerDecodeManager, PeerDeviceInfo, PeerReceiveDiag, PeerStatus};
pub use peer_decoder::VideoPeerDecoder;

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

impl DecodeStatus {
    /// A no-op status for packets that were intentionally skipped (stragglers,
    /// heartbeats, RTT, etc.).
    pub const SKIPPED: Self = Self {
        rendered: false,
        first_frame: false,
    };
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

    /// Called immediately before this decoder is dropped in favour of a freshly
    /// built one covering the **same peer** (`Peer::reset_for_decode_error`
    /// after an AUDIO decode error), as opposed to the peer genuinely going
    /// away.
    ///
    /// Issue 2174 follow-up: a decoder that owns a speaking indicator announces
    /// `speaking: 0` from its `Drop` so a glow left lit by the edge-triggered
    /// VAD is cleared when the worker dies. `Drop` cannot tell teardown from
    /// replacement, so without this hook rebuilding the decoder would blank the
    /// speaking indicator of a peer that is still mid-sentence — and repeated
    /// rebuilds would turn that into a blinking glow. Implementations must make
    /// the subsequent `Drop` silent.
    ///
    /// Issue 2225 narrowed *when* a replacement happens — a VIDEO or SCREEN
    /// decode error no longer rebuilds the audio decoder at all — but did not
    /// remove the case: an AUDIO decode error still replaces it mid-episode, so
    /// this hook is still load-bearing.
    ///
    /// The default is a no-op: a decoder with no speaking indicator (see
    /// [`StandardAudioPeerDecoder`]) has nothing to retire.
    fn retire_for_replacement(&mut self) {}
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

/// Factory function to create the appropriate audio peer decoder based on platform detection
pub fn create_audio_peer_decoder(
    speaker_device_id: Option<String>,
    peer_id: String,
    vad_threshold: Option<f32>,
) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
    // NetEq decoders should start muted by default (peers start with audio_enabled=false)
    NetEqAudioPeerDecoder::new_with_mute_state(speaker_device_id, peer_id, true, vad_threshold)
}
