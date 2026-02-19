use super::*;
use yew::prelude::Callback;
use videocall_types::protos::packet_wrapper::PacketWrapper;

impl HealthReporter {
    /// Set the callback for sending packets (Yew-compat only)
    pub fn set_send_packet_callback(&mut self, callback: Callback<PacketWrapper>) {
        self.send_packet_callback = Some(callback);
    }

    /// Start periodic health reporting (Yew-compat version)
    pub fn start_health_reporting(&self) {
        if self.send_packet_callback.is_none() {
            warn!("Cannot start health reporting: no send packet callback set");
            return;
        }
        let cb = self.send_packet_callback.clone().unwrap();
        let send_fn: Rc<dyn Fn(PacketWrapper)> = Rc::new(move |p| cb.emit(p));
        run_health_reporting_loop(
            self.peer_health_data.clone(),
            self.session_id.clone(),
            self.meeting_id.clone(),
            self.reporting_peer.clone(),
            send_fn,
            self.health_interval_ms,
            self.reporting_audio_enabled.clone(),
            self.reporting_video_enabled.clone(),
            self.active_server_url.clone(),
            self.active_server_type.clone(),
            self.active_server_rtt_ms.clone(),
        );
    }
}
