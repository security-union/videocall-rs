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

        let peer_health_data = Rc::downgrade(&self.peer_health_data);
        let session_id = self.session_id.clone();
        let meeting_id = self.meeting_id.clone();
        let reporting_peer = self.reporting_peer.clone();
        let send_callback = self.send_packet_callback.clone().unwrap();
        let interval_ms = self.health_interval_ms;
        let audio_enabled = Rc::downgrade(&self.reporting_audio_enabled);
        let video_enabled = Rc::downgrade(&self.reporting_video_enabled);
        let active_server_url = Rc::downgrade(&self.active_server_url);
        let active_server_type = Rc::downgrade(&self.active_server_type);
        let active_server_rtt_ms = Rc::downgrade(&self.active_server_rtt_ms);

        spawn_local(async move {
            debug!("Started health reporting with interval: {interval_ms}ms");

            loop {
                // Wait for the interval
                gloo_timers::future::TimeoutFuture::new(interval_ms as u32).await;

                if let Some(peer_health_data) = Weak::upgrade(&peer_health_data) {
                    if let Ok(health_map) = peer_health_data.try_borrow() {
                        let self_audio_enabled = Weak::upgrade(&audio_enabled)
                            .and_then(|ae| ae.try_borrow().ok().map(|v| *v))
                            .unwrap_or(false);
                        let self_video_enabled = Weak::upgrade(&video_enabled)
                            .and_then(|ve| ve.try_borrow().ok().map(|v| *v))
                            .unwrap_or(false);
                        // Snapshot active connection info for this tick
                        let active_url = Weak::upgrade(&active_server_url)
                            .and_then(|rc| rc.try_borrow().ok().and_then(|v| v.clone()));
                        let active_type = Weak::upgrade(&active_server_type)
                            .and_then(|rc| rc.try_borrow().ok().and_then(|v| v.clone()));
                        let active_rtt = Weak::upgrade(&active_server_rtt_ms)
                            .and_then(|rc| rc.try_borrow().ok().and_then(|v| *v));
                        let health_packet = Self::create_health_packet(
                            &session_id,
                            &meeting_id,
                            &reporting_peer,
                            &health_map,
                            self_audio_enabled,
                            self_video_enabled,
                            active_url,
                            active_type,
                            active_rtt,
                        );

                        if let Some(packet) = health_packet {
                            send_callback.emit(packet);
                            debug!("Sent health packet for session: {session_id}");
                        }
                    }
                } else {
                    debug!("HealthReporter dropped, stopping health reporting");
                    break;
                }
            }
        });
    }
}
