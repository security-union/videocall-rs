mod camera_encoder;
mod encoder_state;
mod microphone_encoder;
pub mod safari;
mod screen_encoder;
mod transform;

use crate::utils::is_ios;
use crate::VideoCallClient;
use yew::Callback;

pub use camera_encoder::CameraEncoder;
pub use microphone_encoder::MicrophoneEncoder;
pub use safari::microphone_encoder::MicrophoneEncoder as SafariMicrophoneEncoder;
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

// Implement trait for standard microphone encoder
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

// Implement trait for Safari microphone encoder
impl MicrophoneEncoderTrait for SafariMicrophoneEncoder {
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
    // First determine if we're on iOS using our enhanced detection
    let ios_detected = is_ios();

    if ios_detected {
        log::warn!("Using Safari microphone encoder: AudioEncoder API may not be available on this platform");
        Box::new(SafariMicrophoneEncoder::new(
            client,
            bitrate_kbps,
            on_encoder_settings_update,
        ))
    } else {
        log::info!("Using standard microphone encoder with AudioEncoder API");
        Box::new(MicrophoneEncoder::new(
            client,
            bitrate_kbps,
            on_encoder_settings_update,
        ))
    }
}
