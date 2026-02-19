use super::*;
use yew::Callback;

impl ScreenEncoder {
    /// Construct a screen encoder:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    /// * `bitrate_kbps` - initial bitrate in kilobits per second
    /// * `on_encoder_settings_update` - callback for encoder settings updates (e.g., bitrate changes)
    /// * `on_state_change` - callback for screen share state changes (started, cancelled, stopped)
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
        on_state_change: Callback<ScreenShareEvent>,
    ) -> Self {
        Self {
            client,
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(bitrate_kbps)),
            current_fps: Rc::new(AtomicU32::new(0)),
            on_encoder_settings_update: Some(on_encoder_settings_update),
            on_state_change: Some(on_state_change),
            screen_stream: Rc::new(RefCell::new(None)),
        }
    }

    /// Allows setting a callback to receive encoder settings updates
    pub fn set_encoder_settings_callback(&mut self, callback: Callback<String>) {
        self.on_encoder_settings_update = Some(callback);
    }

    pub fn set_encoder_control(
        &mut self,
        diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        let on_encoder_settings_update: Option<Rc<dyn Fn(String)>> =
            self.on_encoder_settings_update.clone().map(|cb| {
                Rc::new(move |s: String| cb.emit(s)) as Rc<dyn Fn(String)>
            });
        run_screen_encoder_control(
            diagnostics_receiver,
            self.current_bitrate.clone(),
            self.current_fps.clone(),
            on_encoder_settings_update,
            self.state.enabled.clone(),
        );
    }

    /// Start encoding and sending the data to the client connection (if it's currently connected).
    /// The user is prompted by the browser to select which window or screen to encode.
    pub fn start(&mut self) {
        let on_state_change: Option<Rc<dyn Fn(ScreenShareEvent)>> =
            self.on_state_change.clone().map(|cb| {
                Rc::new(move |e: ScreenShareEvent| cb.emit(e)) as Rc<dyn Fn(ScreenShareEvent)>
            });
        start_screen_encoding(
            self.client.clone(),
            self.state.clone(),
            self.current_bitrate.clone(),
            self.current_fps.clone(),
            on_state_change,
            self.screen_stream.clone(),
        );
    }
}
