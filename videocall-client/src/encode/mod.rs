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

use crate::VideoCallClient;
use yew::Callback;

pub use camera_encoder::CameraEncoder;
pub use microphone_encoder::MicrophoneEncoder;
pub use screen_encoder::ScreenEncoder;

/// Trait to abstract over different microphone encoder implementations
pub trait MicrophoneEncoderTrait {
    fn start(&mut self);
    fn stop(&mut self);
    fn select(&mut self, device_id: String) -> bool;
    fn set_enabled(&mut self, enabled: bool) -> bool;
    fn set_encoder_control(
        &mut self,
        rx: futures::channel::mpsc::UnboundedReceiver<
            videocall_types::protos::diagnostics_packet::DiagnosticsPacket,
        >,
    );
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

    fn set_encoder_control(
        &mut self,
        rx: futures::channel::mpsc::UnboundedReceiver<
            videocall_types::protos::diagnostics_packet::DiagnosticsPacket,
        >,
    ) {
        self.set_encoder_control(rx);
    }
}

/// Factory function to create the appropriate microphone encoder based on platform detection
pub fn create_microphone_encoder(
    client: VideoCallClient,
    bitrate_kbps: u32,
    on_encoder_settings_update: Callback<String>,
) -> Box<dyn MicrophoneEncoderTrait> {
    Box::new(MicrophoneEncoder::new(
        client,
        bitrate_kbps,
        on_encoder_settings_update,
    ))
}
