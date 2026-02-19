use super::*;
use yew::Callback;

impl CameraEncoder {
    /// Construct a camera encoder, with arguments:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    ///
    /// * `video_elem_id` - the the ID of an `HtmlVideoElement` to which the camera will be connected.  It does not need to currently exist.
    ///
    /// * `initial_bitrate` - the initial bitrate for the encoder, in kbps.
    ///
    /// * `on_encoder_settings_update` - a callback that will be called when the encoder settings change.
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    /// The encoder is created without a camera selected, [`encoder.select(device_id)`](Self::select) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        video_elem_id: &str,
        initial_bitrate: u32,
        on_encoder_settings_update: Callback<String>,
        on_error: Callback<String>,
    ) -> Self {
        Self {
            client,
            video_elem_id: video_elem_id.to_string(),
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(initial_bitrate)),
            current_fps: Rc::new(AtomicU32::new(0)),
            on_encoder_settings_update,
            on_error: Some(on_error),
        }
    }

    pub fn set_encoder_control(
        &mut self,
        diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        let cb = self.on_encoder_settings_update.clone();
        let wrapped: Rc<dyn Fn(String)> = Rc::new(move |v| cb.emit(v));
        run_encoder_control(
            diagnostics_receiver,
            self.current_bitrate.clone(),
            self.current_fps.clone(),
            wrapped,
            self.state.enabled.clone(),
        );
    }

    /// Start encoding and sending the data to the client connection (if it's currently connected).
    ///
    /// This will not do anything if [`encoder.set_enabled(true)`](Self::set_enabled) has not been
    /// called, or if [`encoder.select(device_id)`](Self::select) has not been called.
    pub fn start(&mut self) {
        let on_error = self.on_error.as_ref().map(|cb| {
            let cb = cb.clone();
            Rc::new(move |v: String| cb.emit(v)) as Rc<dyn Fn(String)>
        });
        start_camera_encoding(
            self.client.clone(),
            self.video_elem_id.clone(),
            self.state.clone(),
            self.current_bitrate.clone(),
            self.current_fps.clone(),
            on_error,
        );
    }
}
