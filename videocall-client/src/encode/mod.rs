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

mod camera_encoder;
pub(crate) mod classify_encode_error;
mod encoder_state;
mod microphone_encoder;
mod screen_encoder;
mod transform;

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;

use crate::VideoCallClient;
use videocall_types::Callback;

pub(crate) use camera_encoder::layer_ceiling_to_count;
pub use camera_encoder::{
    camera_encoder_errors_closed_codec, camera_encoder_errors_configure_fatal,
    camera_encoder_errors_generic, camera_encoder_errors_vpx_mem_alloc,
    camera_encoder_frames_submitted_ok, camera_encoder_layers_torn_down,
    camera_encoder_restarts_closed_codec, camera_encoder_restarts_configure,
    camera_encoder_restarts_memory, camera_encoder_restarts_other, CameraEncoder,
    LiveQualitySnapshot, QualityTierBounds, SimulcastLayerInfo, SimulcastSendSnapshot,
};
pub use microphone_encoder::MicrophoneEncoder;
pub use screen_encoder::{
    screen_encoder_errors_closed_codec, screen_encoder_errors_configure_fatal,
    screen_encoder_errors_generic, screen_encoder_errors_vpx_mem_alloc,
    screen_encoder_frames_submitted_ok, screen_encoder_layers_torn_down,
    screen_encoder_restarts_closed_codec, screen_encoder_restarts_configure,
    screen_encoder_restarts_memory, screen_encoder_restarts_other, ScreenEncoder,
    ScreenQualitySnapshot, ScreenQualityTierBounds, ScreenShareEvent,
};

/// Trait to abstract over different microphone encoder implementations
pub trait MicrophoneEncoderTrait {
    fn start(&mut self);
    fn stop(&mut self);
    fn select(&mut self, device_id: String) -> bool;
    fn set_enabled(&mut self, enabled: bool) -> bool;
    fn set_error_callback(&mut self, on_error: Callback<String>);
    /// Set the user's SEND audio layer-ceiling (the perf-panel "layers published"
    /// control). `ceiling` is a layer COUNT (`None` = Auto / full ladder). Applied
    /// LIVE with no mic-encoder restart — see
    /// [`MicrophoneEncoder::set_user_layer_ceiling`]. `&self` (interior mutability
    /// via a shared atomic) so it can be called through the trait object the
    /// `Host` holds.
    fn set_user_layer_ceiling(&self, ceiling: Option<u32>);
    /// The current user SEND audio layer-ceiling (layer COUNT), or `None` for Auto.
    fn user_layer_ceiling(&self) -> Option<u32>;
    /// Share the CONGESTION-driven audio layer-ceiling atom (issue #621) so a
    /// self-targeted server CONGESTION signal cuts the audio simulcast ladder to
    /// base-only. See [`MicrophoneEncoder::set_congestion_layer_ceiling`].
    fn set_congestion_layer_ceiling(&mut self, ceiling: Arc<AtomicU32>);
    /// Share the single-layer audio BITRATE floor atom (issue #1398). The
    /// client owns it to reset on reconnect; the mic-side uplink-distress detector
    /// writes it. See [`MicrophoneEncoder::set_congestion_bitrate_floor`].
    fn set_congestion_bitrate_floor(&mut self, floor: Arc<AtomicU32>);
    /// Share the CAMERA's enabled flag (issue #1398) so the mic-side uplink
    /// distress detector can gate itself to the camera being off, and so the FEC
    /// reconfig timer can select the effective single-layer audio bitrate by
    /// camera state. See [`MicrophoneEncoder::set_camera_active_signal`].
    fn set_camera_active_signal(&mut self, camera_active: Arc<AtomicBool>);
    /// Share the connection RECONNECT-reseed flag (issue #1398 reconnect P1) so the
    /// mic-side uplink-distress detector forces a window re-seed on every
    /// (re)connect, preventing a cross-reconnect counter delta from cashing a
    /// spurious cut. See [`MicrophoneEncoder::set_reconnect_reseed_signal`].
    fn set_reconnect_reseed_signal(&mut self, reconnect_reseed: Arc<AtomicBool>);
    /// Share the camera's video-at-floor flag (issue #1611, lever 2).
    /// See [`MicrophoneEncoder::set_camera_video_exhausted_signal`].
    fn set_camera_video_exhausted_signal(&mut self, flag: Arc<AtomicBool>);
    /// Share the screen's video-at-floor flag (issue #1611, lever 3).
    /// See [`MicrophoneEncoder::set_screen_video_exhausted_signal`].
    fn set_screen_video_exhausted_signal(&mut self, flag: Arc<AtomicBool>);
    /// Share the screen-sharing-active flag (issue #1611, lever 3).
    /// See [`MicrophoneEncoder::set_screen_sharing_active_signal`].
    fn set_screen_sharing_active_signal(&mut self, flag: Arc<AtomicBool>);
    /// Returns the effective audio simulcast layer count (#1561).
    fn effective_audio_layers(&self) -> u32;
    /// Returns the shared CONGESTION audio layer-ceiling atom (#1561).
    fn congestion_layer_ceiling(&self) -> Arc<AtomicU32>;
    /// Returns the shared USER audio layer-ceiling atom (#1561).
    fn shared_user_layer_ceiling(&self) -> Rc<AtomicU32>;
}

// Implement trait for Safari microphone encoder
impl MicrophoneEncoderTrait for MicrophoneEncoder {
    fn start(&mut self) {
        self.start();
    }

    fn stop(&mut self) {
        self.stop();
    }

    fn select(&mut self, device_id: String) -> bool {
        self.select(device_id)
    }

    fn set_enabled(&mut self, enabled: bool) -> bool {
        self.set_enabled(enabled)
    }

    fn set_error_callback(&mut self, on_error: Callback<String>) {
        self.set_error_callback(on_error)
    }

    fn set_user_layer_ceiling(&self, ceiling: Option<u32>) {
        self.set_user_layer_ceiling(ceiling)
    }

    fn user_layer_ceiling(&self) -> Option<u32> {
        self.user_layer_ceiling()
    }

    fn set_congestion_layer_ceiling(&mut self, ceiling: Arc<AtomicU32>) {
        self.set_congestion_layer_ceiling(ceiling)
    }

    fn set_congestion_bitrate_floor(&mut self, floor: Arc<AtomicU32>) {
        self.set_congestion_bitrate_floor(floor)
    }

    fn set_camera_active_signal(&mut self, camera_active: Arc<AtomicBool>) {
        self.set_camera_active_signal(camera_active)
    }

    fn set_reconnect_reseed_signal(&mut self, reconnect_reseed: Arc<AtomicBool>) {
        self.set_reconnect_reseed_signal(reconnect_reseed)
    }

    fn set_camera_video_exhausted_signal(&mut self, flag: Arc<AtomicBool>) {
        self.set_camera_video_exhausted_signal(flag)
    }

    fn set_screen_video_exhausted_signal(&mut self, flag: Arc<AtomicBool>) {
        self.set_screen_video_exhausted_signal(flag)
    }

    fn set_screen_sharing_active_signal(&mut self, flag: Arc<AtomicBool>) {
        self.set_screen_sharing_active_signal(flag)
    }

    fn effective_audio_layers(&self) -> u32 {
        self.effective_audio_layers()
    }

    fn congestion_layer_ceiling(&self) -> Arc<AtomicU32> {
        self.congestion_layer_ceiling()
    }

    fn shared_user_layer_ceiling(&self) -> Rc<AtomicU32> {
        self.shared_user_layer_ceiling()
    }
}

/// Factory function to create the appropriate microphone encoder based on platform detection.
///
/// `shared_audio_tier_bitrate`, `shared_audio_tier_fec`, and
/// `shared_audio_tier_index` are optional shared atomics from the
/// `CameraEncoder`. When provided, the microphone encoder reads the audio
/// quality tier from the camera encoder's quality manager instead of creating
/// its own `EncoderBitrateController`. `shared_audio_tier_index` (issue #1567)
/// additionally drives the live Opus FEC ctl-reconfig on a mid-call tier change.
#[allow(clippy::too_many_arguments)]
pub fn create_microphone_encoder(
    client: VideoCallClient,
    bitrate_kbps: u32,
    on_encoder_settings_update: Callback<String>,
    on_error: Callback<String>,
    vad_threshold: Option<f32>,
    shared_audio_tier_bitrate: Option<Rc<AtomicU32>>,
    shared_audio_tier_fec: Option<Rc<AtomicBool>>,
    shared_audio_tier_index: Option<Rc<AtomicU32>>,
    max_layers: u32,
) -> Box<dyn MicrophoneEncoderTrait> {
    Box::new(MicrophoneEncoder::new(
        client,
        bitrate_kbps,
        on_encoder_settings_update,
        on_error,
        vad_threshold,
        shared_audio_tier_bitrate,
        shared_audio_tier_fec,
        shared_audio_tier_index,
        max_layers,
    ))
}
