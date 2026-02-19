use super::*;
use yew::prelude::Callback;

impl Default for PeerDecodeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerDecodeManager {
    pub fn new() -> Self {
        Self {
            connected_peers: HashMapWithOrderedKeys::new(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: None,
            on_peer_removed: Callback::noop(),
        }
    }

    pub fn new_with_diagnostics(diagnostics: Rc<DiagnosticManager>) -> Self {
        Self {
            connected_peers: HashMapWithOrderedKeys::new(),
            on_first_frame: Callback::noop(),
            get_video_canvas_id: Callback::from(|key| format!("video-{}", &key)),
            get_screen_canvas_id: Callback::from(|key| format!("screen-{}", &key)),
            diagnostics: Some(diagnostics),
            on_peer_removed: Callback::noop(),
        }
    }
}

impl PeerDecodeManager {
    pub fn run_peer_monitor(&mut self) {
        let removed = self
            .connected_peers
            .remove_if_and_return_keys(|peer| peer.check_heartbeat());
        for k in removed {
            // Emit to event bus
            emit_client_event(ClientEvent::PeerRemoved(k.clone()));
            // Call Yew callback
            self.on_peer_removed.emit(k);
        }
    }

    pub fn decode(&mut self, response: PacketWrapper, userid: &str) -> Result<(), PeerDecodeError> {
        let packet = Arc::new(response);
        let email = packet.email.clone();
        if let Some(peer) = self.connected_peers.get_mut(&email) {
            // Set worker diagnostics context once per peer
            if !peer.context_initialized {
                peer.video
                    .set_stream_context(userid.to_string(), email.clone());
                peer.screen
                    .set_stream_context(userid.to_string(), email.clone());
                peer.context_initialized = true;
            }
            match peer.decode(&packet) {
                Ok((MediaType::HEARTBEAT, _)) => {
                    peer.on_heartbeat();
                    Ok(())
                }
                Ok((media_type, decode_status)) => {
                    if media_type != MediaType::RTT && packet.email == userid {
                        return Err(PeerDecodeError::SameUserPacket(email.clone()));
                    }
                    if let Some(diagnostics) = &self.diagnostics {
                        diagnostics.track_frame(&email, media_type, packet.data.len() as u64);
                    }

                    if decode_status.first_frame {
                        // Emit to event bus
                        emit_client_event(ClientEvent::PeerFirstFrame {
                            peer_id: email.clone(),
                            media_type,
                        });
                        // Call Yew callback
                        self.on_first_frame.emit((email.clone(), media_type));
                    }

                    Ok(())
                }
                Err(e) => peer.reset().map_err(|_| e),
            }
        } else {
            Err(PeerDecodeError::NoSuchPeer(email.clone()))
        }
    }

    pub(super) fn add_peer(&mut self, email: &str, aes: Option<Aes128State>) -> Result<(), JsValue> {
        debug!("Adding peer {email}");
        self.connected_peers.insert(
            email.to_owned(),
            Peer::new(
                self.get_video_canvas_id.emit(email.to_owned()),
                self.get_screen_canvas_id.emit(email.to_owned()),
                email.to_owned(),
                aes,
            )?,
        );
        Ok(())
    }

    pub fn delete_peer(&mut self, email: &String) {
        self.connected_peers.remove(email);
        // Emit to event bus
        emit_client_event(ClientEvent::PeerRemoved(email.clone()));
        // Call Yew callback
        self.on_peer_removed.emit(email.clone());
    }
}
