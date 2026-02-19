use super::*;
use yew::Callback;
use crate::VideoCallClient;

/// Trait to abstract over different microphone encoder implementations
pub trait MicrophoneEncoderTrait {
    fn start(&mut self);
    fn stop(&mut self);
    fn select(&mut self, device_id: String) -> bool;
    fn set_enabled(&mut self, enabled: bool) -> bool;
    fn set_error_callback(&mut self, on_error: yew::Callback<String>);
    fn set_encoder_control(
        &mut self,
        rx: futures::channel::mpsc::UnboundedReceiver<
            videocall_types::protos::diagnostics_packet::DiagnosticsPacket,
        >,
    );
}

// Implement trait for microphone encoder (yew-compat)
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

    fn set_error_callback(&mut self, on_error: yew::Callback<String>) {
        self.set_error_callback(on_error)
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
    on_error: Callback<String>,
) -> Box<dyn MicrophoneEncoderTrait> {
    Box::new(MicrophoneEncoder::new(
        client,
        bitrate_kbps,
        on_encoder_settings_update,
        on_error,
    ))
}
