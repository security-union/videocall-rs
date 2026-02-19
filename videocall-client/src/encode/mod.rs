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
mod encoder_state;
mod microphone_encoder;
mod screen_encoder;
mod transform;

#[cfg(not(feature = "yew-compat"))]
use crate::VideoCallClient;

pub use camera_encoder::CameraEncoder;
pub use microphone_encoder::MicrophoneEncoder;
pub use screen_encoder::{ScreenEncoder, ScreenShareEvent};

/// Trait to abstract over different microphone encoder implementations (framework-agnostic)
#[cfg(not(feature = "yew-compat"))]
pub trait MicrophoneEncoderTrait {
    fn start(&mut self);
    fn stop(&mut self);
    fn select(&mut self, device_id: String) -> bool;
    fn set_enabled(&mut self, enabled: bool) -> bool;
    fn set_error_callback_fn(&mut self, on_error: Box<dyn Fn(String)>);
    fn set_encoder_control(
        &mut self,
        rx: futures::channel::mpsc::UnboundedReceiver<
            videocall_types::protos::diagnostics_packet::DiagnosticsPacket,
        >,
    );
}

// Implement trait for microphone encoder (framework-agnostic)
#[cfg(not(feature = "yew-compat"))]
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

    fn set_error_callback_fn(&mut self, on_error: Box<dyn Fn(String)>) {
        self.set_error_callback_fn(on_error)
    }

    fn set_encoder_control(
        &mut self,
        rx: futures::channel::mpsc::UnboundedReceiver<
            videocall_types::protos::diagnostics_packet::DiagnosticsPacket,
        >,
    ) {
        self.set_encoder_control(rx);
    }
}

/// Factory function to create the appropriate microphone encoder (framework-agnostic)
#[cfg(not(feature = "yew-compat"))]
pub fn create_microphone_encoder(
    client: VideoCallClient,
    bitrate_kbps: u32,
    on_encoder_settings_update: Box<dyn Fn(String)>,
    on_error: Box<dyn Fn(String)>,
) -> Box<dyn MicrophoneEncoderTrait> {
    let mut encoder = MicrophoneEncoder::new(client, bitrate_kbps);
    encoder.set_encoder_settings_callback_fn(on_encoder_settings_update);
    encoder.set_error_callback_fn(on_error);
    Box::new(encoder)
}

#[cfg(feature = "yew-compat")]
#[path = "yew_compat.rs"]
mod yew_compat_mod;

#[cfg(feature = "yew-compat")]
pub use yew_compat_mod::{MicrophoneEncoderTrait, create_microphone_encoder};
