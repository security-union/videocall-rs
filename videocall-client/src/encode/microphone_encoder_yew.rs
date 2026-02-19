use super::*;
use yew::Callback;

impl MicrophoneEncoder {
    pub fn new(
        client: VideoCallClient,
        _bitrate_kbps: u32,
        on_encoder_settings_update: Callback<String>,
        on_error: Callback<String>,
    ) -> Self {
        Self {
            client,
            state: EncoderState::new(),
            _on_encoder_settings_update: Some(on_encoder_settings_update),
            codec: AudioWorkletCodec::default(),
            on_error: Some(on_error),
        }
    }

    pub fn set_error_callback(&mut self, on_error: Callback<String>) {
        self.on_error = Some(on_error);
    }

    pub fn start(&mut self) {
        let device_id = if let Some(mic) = &self.state.selected {
            mic.to_string()
        } else {
            return;
        };

        if !self.state.is_enabled() {
            log::debug!("Microphone encoder start() called but encoder is not enabled");
            return;
        }

        if self.state.switching.load(Ordering::Acquire) && self.codec.is_instantiated() {
            self.stop();
        }
        if self.state.is_enabled() && self.codec.is_instantiated() {
            return;
        }

        let user_id = self.client.userid().clone();
        let client = self.client.clone();
        let aes = client.aes();
        let on_error: Option<Rc<dyn Fn(String)>> = self.on_error.as_ref().map(|cb| {
            let cb = cb.clone();
            Rc::new(move |v: String| cb.emit(v)) as Rc<dyn Fn(String)>
        });
        let state = self.state.clone();
        let codec = self.codec.clone();

        start_microphone_encoding(user_id, client, device_id, aes, on_error, state, codec);
    }
}
