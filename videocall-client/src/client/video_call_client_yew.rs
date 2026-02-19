use super::*;
use crate::connection::{ConnectionController, ConnectionManagerOptions, ConnectionState};
use yew::prelude::Callback;

impl VideoCallClient {
    /// Constructor for the client struct.
    ///
    /// See [VideoCallClientOptions] for description of the options.
    ///
    pub fn new(options: VideoCallClientOptions) -> Self {
        let aes = Rc::new(Aes128State::new(options.enable_e2ee));

        // Create diagnostics manager if enabled
        let diagnostics = if options.enable_diagnostics {
            let diagnostics = Rc::new(DiagnosticManager::new(options.userid.clone()));

            // Set update interval if provided
            if let Some(interval) = options.diagnostics_update_interval_ms {
                let mut diag = DiagnosticManager::new(options.userid.clone());
                diag.set_reporting_interval(interval);
                let diagnostics = Rc::new(diag);

                Some(diagnostics)
            } else {
                Some(diagnostics)
            }
        } else {
            None
        };

        // Create sender diagnostics manager if diagnostics are enabled
        let sender_diagnostics = if options.enable_diagnostics {
            let sender_diagnostics = Rc::new(SenderDiagnosticManager::new(options.userid.clone()));

            // Set update interval if provided
            if let Some(interval) = options.diagnostics_update_interval_ms {
                sender_diagnostics.set_reporting_interval(interval);
            }

            Some(sender_diagnostics)
        } else {
            None
        };

        // Create health reporter if enabled
        let health_reporter = if options.enable_health_reporting {
            let session_id = format!(
                "session_{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            );

            let mut reporter = HealthReporter::new(
                session_id,
                options.userid.clone(),
                options.health_reporting_interval_ms.unwrap_or(5000),
            );

            // Set the meeting ID
            reporter.set_meeting_id(options.meeting_id.clone());

            // Set health reporting interval if provided
            if let Some(interval) = options.health_reporting_interval_ms {
                reporter.set_health_interval(interval);
            }

            Some(Rc::new(RefCell::new(reporter)))
        } else {
            None
        };

        let client = Self {
            options: options.clone(),
            inner: Rc::new(RefCell::new(Inner {
                options: InnerOptions {
                    enable_e2ee: options.enable_e2ee,
                    userid: options.userid.clone(),
                    on_peer_added: options.on_peer_added.clone(),
                    on_meeting_ended: options.on_meeting_ended.clone(),
                    on_meeting_info: options.on_meeting_info.clone(),
                },
                connection_controller: None,
                connection_state: ConnectionState::Failed {
                    error: "Not connected".to_string(),
                    last_known_server: None,
                },
                aes: aes.clone(),
                rsa: Rc::new(RsaWrapper::new(options.enable_e2ee)),
                peer_decode_manager: Self::create_peer_decoder_manager(
                    &options,
                    diagnostics.clone(),
                ),
                _diagnostics: diagnostics.clone(),
                sender_diagnostics: sender_diagnostics.clone(),
                health_reporter: health_reporter.clone(),
            })),
            aes,
            _diagnostics: diagnostics.clone(),
        };

        // Set up the packet forwarding from DiagnosticManager to VideoCallClient
        if let Some(diagnostics) = &diagnostics {
            let client_clone = client.clone();
            diagnostics.set_packet_handler(Callback::from(move |packet| {
                client_clone.send_diagnostic_packet(packet);
            }));
        }

        // Set up health reporter with packet sending callback
        if let Some(health_reporter) = &health_reporter {
            if let Ok(mut reporter) = health_reporter.try_borrow_mut() {
                let client_clone = client.clone();
                reporter.set_send_packet_callback(Callback::from(move |packet| {
                    client_clone.send_packet(packet);
                }));

                // Start real diagnostics subscription (not mock channels)
                reporter.start_diagnostics_subscription();

                // Start health reporting
                reporter.start_health_reporting();
                debug!("Health reporting started with real diagnostics subscription");
            }
        }

        client
    }

    /// Initiates a connection to a videocall server with RTT testing (not available without yew-compat).
    ///
    /// Returns an error since WebSocket/WebTransport support requires the yew-compat feature.
    ///
    /// Note that this method's success means only that it succesfully *attempted* initiation of the
    /// connection.  The connection cannot actually be considered to have been succesful until the
    /// `ClientEvent::Connected` event is emitted (or `options.on_connected` callback is invoked
    /// when using yew-compat feature).
    ///
    /// If the connection does not succeed, the `ClientEvent::ConnectionLost` event will be emitted.
    ///
    pub fn connect_with_rtt_testing(&mut self) -> anyhow::Result<()> {
        let websocket_count = self.options.websocket_urls.len();
        let webtransport_count = if self.options.enable_webtransport {
            self.options.webtransport_urls.len()
        } else {
            0 // Don't count WebTransport URLs if WebTransport is disabled
        };
        let total_servers = websocket_count + webtransport_count;

        info!(
            "Starting RTT testing for {total_servers} servers (WebSocket: {websocket_count}, WebTransport: {webtransport_count})"
        );

        if total_servers == 0 {
            return Err(anyhow!("No servers provided for RTT testing"));
        }

        let election_period_ms = self.options.rtt_testing_period_ms;

        info!("RTT testing period: {election_period_ms}ms");

        // Create ConnectionManager which will handle all the RTT testing
        let manager_options = ConnectionManagerOptions {
            websocket_urls: self.options.websocket_urls.clone(),
            webtransport_urls: if self.options.enable_webtransport {
                self.options.webtransport_urls.clone()
            } else {
                Vec::new() // Empty if WebTransport is disabled
            },
            userid: self.options.userid.clone(),
            on_inbound_media: {
                let inner = Rc::downgrade(&self.inner);
                Callback::from(move |packet| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        if let Ok(mut inner) = inner.try_borrow_mut() {
                            // Process the packet
                            inner.on_inbound_media(packet);
                        }
                    }
                })
            },
            on_state_changed: {
                let on_connected = self.options.on_connected.clone();
                let on_connection_lost = self.options.on_connection_lost.clone();
                let inner = Rc::downgrade(&self.inner);
                Callback::from(move |state: ConnectionState| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        if let Ok(mut inner) = inner.try_borrow_mut() {
                            inner.connection_state = state.clone();
                        }
                    }
                    info!("Connection state changed: {state:?} in video call client");

                    match state {
                        ConnectionState::Connected { .. } => {
                            // Emit to event bus
                            emit_client_event(ClientEvent::Connected);
                            // Call Yew callback
                            on_connected.emit(());
                        }
                        ConnectionState::Failed { ref error, .. } => {
                            // Emit to event bus
                            emit_client_event(ClientEvent::ConnectionLost(error.clone()));
                            // Call Yew callback
                            on_connection_lost.emit(JsValue::from_str(error));
                        }
                        _ => {
                            // Other states don't trigger callbacks
                        }
                    }
                })
            },
            peer_monitor: {
                let inner = Rc::downgrade(&self.inner);
                let on_connection_lost = self.options.on_connection_lost.clone();
                Callback::from(move |_| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        match inner.try_borrow_mut() {
                            Ok(mut inner) => {
                                inner.peer_decode_manager.run_peer_monitor();
                            }
                            Err(_) => {
                                on_connection_lost.emit(JsValue::from_str(
                                    "Unable to borrow inner -- not starting peer monitor",
                                ));
                            }
                        }
                    }
                })
            },
            election_period_ms,
        };

        let connection_controller = ConnectionController::new(manager_options, self.aes.clone())?;

        let mut borrowed = self.inner.try_borrow_mut()?;
        borrowed.connection_controller = Some(connection_controller);

        info!("ConnectionManager created with RTT testing and 1Hz diagnostics reporting");
        Ok(())
    }

    fn create_peer_decoder_manager(
        opts: &VideoCallClientOptions,
        diagnostics: Option<Rc<DiagnosticManager>>,
    ) -> PeerDecodeManager {
        match diagnostics {
            Some(diagnostics) => {
                let mut peer_decode_manager = PeerDecodeManager::new_with_diagnostics(diagnostics);
                peer_decode_manager.on_first_frame = opts.on_peer_first_frame.clone();
                peer_decode_manager.get_video_canvas_id = opts.get_peer_video_canvas_id.clone();
                peer_decode_manager.get_screen_canvas_id = opts.get_peer_screen_canvas_id.clone();
                if let Some(cb) = &opts.on_peer_removed {
                    peer_decode_manager.on_peer_removed = cb.clone();
                }
                peer_decode_manager
            }
            None => {
                let mut peer_decode_manager = PeerDecodeManager::new();
                peer_decode_manager.on_first_frame = opts.on_peer_first_frame.clone();
                peer_decode_manager.get_video_canvas_id = opts.get_peer_video_canvas_id.clone();
                peer_decode_manager.get_screen_canvas_id = opts.get_peer_screen_canvas_id.clone();
                if let Some(cb) = &opts.on_peer_removed {
                    peer_decode_manager.on_peer_removed = cb.clone();
                }
                peer_decode_manager
            }
        }
    }

    pub(crate) fn send_packet(&self, media: PacketWrapper) {
        let packet_type = media.packet_type.enum_value();
        match self.inner.try_borrow() {
            Ok(inner) => {
                if let Some(connection_controller) = &inner.connection_controller {
                    if let Err(e) = connection_controller.send_packet(media) {
                        debug!("Failed to send {packet_type:?} packet: {e}");
                    }
                } else {
                    error!("No connection manager available for {packet_type:?} packet");
                }
            }
            Err(_) => {
                error!("Unable to borrow inner -- dropping {packet_type:?} packet {media:?}")
            }
        }
    }

    /// Returns `true` if the client is currently connected to a server.
    pub fn is_connected(&self) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(connection_controller) = &inner.connection_controller {
                return connection_controller.is_connected();
            }
        };
        false
    }

    /// Disconnect from the current server.
    pub fn disconnect(&self) -> anyhow::Result<()> {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            if let Some(connection_controller) = &mut inner.connection_controller {
                let _ = connection_controller.disconnect();
            }

            inner.connection_controller = None;
            inner.connection_state = ConnectionState::Failed {
                error: "Disconnected".to_string(),
                last_known_server: None,
            };
            Ok(())
        } else {
            Err(anyhow::anyhow!("Unable to borrow inner"))
        }
    }

    /// Get current connection state from ConnectionController
    pub fn get_connection_state(&self) -> Option<ConnectionState> {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(connection_controller) = &inner.connection_controller {
                return Some(connection_controller.get_connection_state());
            }
        }
        None
    }

    /// Get RTT measurements from ConnectionController (for debugging)
    pub fn get_rtt_measurements(&self) -> Option<HashMap<String, f64>> {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(connection_controller) = &inner.connection_controller {
                let measurements = connection_controller.get_rtt_measurements_clone();
                let mut result = HashMap::new();
                for (connection_id, measurement) in measurements {
                    if let Some(avg_rtt) = measurement.average_rtt {
                        result.insert(connection_id.clone(), avg_rtt);
                    }
                }
                return Some(result);
            }
        }
        None
    }

    /// Send RTT probes manually (for testing)
    pub fn send_rtt_probes(&self) -> anyhow::Result<()> {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(_connection_controller) = &inner.connection_controller {
                // RTT probes are now handled automatically by ConnectionController timers
                return Ok(());
            }
        }
        Err(anyhow!("No connection controller available"))
    }

    /// Check and complete election if testing period is over
    pub fn check_election_completion(&self) {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(_connection_controller) = &inner.connection_controller {
                // Election completion is now handled automatically by ConnectionController timers
            }
        }
    }

    pub fn set_video_enabled(&self, enabled: bool) {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(connection_controller) = &inner.connection_controller {
                if let Err(e) = connection_controller.set_video_enabled(enabled) {
                    debug!("Failed to set video enabled {enabled}: {e}");
                } else {
                    debug!("Successfully set video enabled: {enabled}");
                    if let Some(hr) = &inner.health_reporter {
                        if let Ok(hrb) = hr.try_borrow() {
                            hrb.set_reporting_video_enabled(enabled);
                        }
                    }
                }
            } else {
                debug!("No connection controller available for set_video_enabled({enabled})");
            }
        } else {
            error!("Unable to borrow inner for set_video_enabled({enabled})");
        }
    }

    pub fn set_audio_enabled(&self, enabled: bool) {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(connection_controller) = &inner.connection_controller {
                if let Err(e) = connection_controller.set_audio_enabled(enabled) {
                    debug!("Failed to set audio enabled {enabled}: {e}");
                } else {
                    debug!("Successfully set audio enabled: {enabled}");
                    if let Some(hr) = &inner.health_reporter {
                        if let Ok(hrb) = hr.try_borrow() {
                            hrb.set_reporting_audio_enabled(enabled);
                        }
                    }
                }
            } else {
                debug!("No connection controller available for set_audio_enabled({enabled})");
            }
        } else {
            error!("Unable to borrow inner for set_audio_enabled({enabled})");
        }
    }

    pub fn set_screen_enabled(&self, enabled: bool) {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(connection_controller) = &inner.connection_controller {
                if let Err(e) = connection_controller.set_screen_enabled(enabled) {
                    debug!("Failed to set screen enabled {enabled}: {e}");
                } else {
                    debug!("Successfully set screen enabled: {enabled}");
                }
            } else {
                debug!("No connection controller available for set_screen_enabled({enabled})");
            }
        } else {
            error!("Unable to borrow inner for set_screen_enabled({enabled})");
        }
    }
}

impl Inner {
    fn on_inbound_media(&mut self, response: PacketWrapper) {
        debug!(
            "<< Received {:?} from {}",
            response.packet_type.enum_value(),
            response.email
        );
        // Skip creating peers for system messages (meeting info, meeting started/ended)
        let peer_status = if response.email == SYSTEM_USER_EMAIL {
            PeerStatus::NoChange
        } else {
            self.peer_decode_manager.ensure_peer(&response.email)
        };
        match response.packet_type.enum_value() {
            Ok(PacketType::AES_KEY) => {
                if !self.options.enable_e2ee {
                    return;
                }
                if let Ok(bytes) = self.rsa.decrypt(&response.data) {
                    debug!("Decrypted AES_KEY from {}", response.email);
                    match AesPacket::parse_from_bytes(&bytes) {
                        Ok(aes_packet) => {
                            if let Err(e) = self.peer_decode_manager.set_peer_aes(
                                &response.email,
                                Aes128State::from_vecs(
                                    aes_packet.key,
                                    aes_packet.iv,
                                    self.options.enable_e2ee,
                                ),
                            ) {
                                error!("Failed to set peer aes: {e}");
                            }
                        }
                        Err(e) => {
                            error!("Failed to parse aes packet: {e}");
                        }
                    }
                }
            }
            Ok(PacketType::RSA_PUB_KEY) => {
                if !self.options.enable_e2ee {
                    return;
                }
                let encrypted_aes_packet = parse_rsa_packet(&response.data)
                    .and_then(parse_public_key)
                    .and_then(|pub_key| {
                        self.serialize_aes_packet()
                            .map(|aes_packet| (aes_packet, pub_key))
                    })
                    .and_then(|(aes_packet, pub_key)| {
                        self.encrypt_aes_packet(&aes_packet, &pub_key)
                    });

                match encrypted_aes_packet {
                    Ok(data) => {
                        debug!(">> {} sending AES key", self.options.userid);

                        // Send AES key packet via ConnectionController
                        if let Some(connection_controller) = &self.connection_controller {
                            let packet = PacketWrapper {
                                packet_type: PacketType::AES_KEY.into(),
                                email: self.options.userid.clone(),
                                data,
                                ..Default::default()
                            };

                            if let Err(e) = connection_controller.send_packet(packet) {
                                error!("Failed to send AES key packet: {e}");
                            }
                        } else {
                            error!("No connection controller available for AES key");
                        }
                    }
                    Err(e) => {
                        error!("Failed to send AES_KEY to peer: {e}");
                    }
                }
            }
            Ok(PacketType::MEDIA) => {
                let email = response.email.clone();

                // RTT responses are now handled directly by the ConnectionManager via individual connection callbacks
                // No need to process them here anymore
                if let Err(e) = self
                    .peer_decode_manager
                    .decode(response, &self.options.userid)
                {
                    error!("error decoding packet: {e}");
                    match e {
                        PeerDecodeError::SameUserPacket(email) => {
                            debug!("Rejecting packet from same user: {email}");
                        }
                        _ => {
                            self.peer_decode_manager.delete_peer(&email);
                        }
                    }
                }
            }
            Ok(PacketType::CONNECTION) => {
                // CONNECTION packets are used for other purposes now
                // Meeting info is sent via MEETING packet type with protobuf
                let data_str = String::from_utf8_lossy(&response.data);
                debug!("Received CONNECTION packet: {data_str}");
            }
            Ok(PacketType::DIAGNOSTICS) => {
                // Parse and handle the diagnostics packet
                if let Ok(diagnostics_packet) = DiagnosticsPacket::parse_from_bytes(&response.data)
                {
                    debug!("Received diagnostics packet: {diagnostics_packet:?}");
                    if let Some(sender_diagnostics) = &self.sender_diagnostics {
                        sender_diagnostics.handle_diagnostic_packet(diagnostics_packet);
                    }
                } else {
                    error!("Failed to parse diagnostics packet");
                }
            }
            Ok(PacketType::HEALTH) => {
                // Health packets are sent from client to server for monitoring
                // Clients should not receive health packets, so we ignore them
                debug!(
                    "Received unexpected health packet from {}, ignoring",
                    response.email
                );
            }
            Ok(PacketType::MEETING) => {
                // Parse MeetingPacket protobuf
                match MeetingPacket::parse_from_bytes(&response.data) {
                    Ok(meeting_packet) => {
                        match meeting_packet.event_type.enum_value() {
                            Ok(MeetingEventType::MEETING_STARTED) => {
                                info!(
                                    "Received MEETING_STARTED: room={}, start_time={}ms, creator={}",
                                    meeting_packet.room_id,
                                    meeting_packet.start_time_ms,
                                    meeting_packet.creator_id
                                );
                                let start_time = meeting_packet.start_time_ms as f64;
                                // Emit to event bus
                                emit_client_event(ClientEvent::MeetingInfo(start_time));
                                // Call Yew callback
                                if let Some(callback) = &self.options.on_meeting_info {
                                    callback.emit(start_time);
                                }
                            }
                            Ok(MeetingEventType::MEETING_ENDED) => {
                                info!(
                                    "Received MEETING_ENDED: room={}, message={}",
                                    meeting_packet.room_id, meeting_packet.message
                                );
                                let end_time_ms = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_millis() as f64)
                                    .unwrap_or(0.0);
                                let message = meeting_packet.message.clone();
                                // Emit to event bus
                                emit_client_event(ClientEvent::MeetingEnded {
                                    end_time_ms,
                                    message: message.clone(),
                                });
                                // Call Yew callback
                                if let Some(callback) = &self.options.on_meeting_ended {
                                    callback.emit((end_time_ms, message));
                                }
                            }
                            Ok(MeetingEventType::PARTICIPANT_JOINED) => {
                                info!(
                                    "Received PARTICIPANT_JOINED: room={}, count={}",
                                    meeting_packet.room_id, meeting_packet.participant_count
                                );
                                // Future: could emit participant joined event
                            }
                            Ok(MeetingEventType::PARTICIPANT_LEFT) => {
                                info!(
                                    "Received PARTICIPANT_LEFT: room={}, count={}",
                                    meeting_packet.room_id, meeting_packet.participant_count
                                );
                                // Future: could emit participant left event
                            }
                            Ok(MeetingEventType::MEETING_EVENT_TYPE_UNKNOWN) => {
                                error!(
                                    "Received meeting packet with unknown event type: room={}",
                                    meeting_packet.room_id
                                );
                            }
                            Err(e) => {
                                error!("Failed to parse MeetingEventType: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse MeetingPacket: {e}");
                    }
                }
            }
            Ok(PacketType::PACKET_TYPE_UNKNOWN) => {
                error!(
                    "Received packet with unknown packet type from {}",
                    response.email
                );
            }
            Err(e) => {
                error!("Failed to parse packet type: {e}");
            }
        }
        if let PeerStatus::Added(peer_userid) = peer_status {
            if peer_userid != self.options.userid {
                // Emit to event bus
                emit_client_event(ClientEvent::PeerAdded(peer_userid.clone()));
                // Call Yew callback
                self.options.on_peer_added.emit(peer_userid);
                self.send_public_key();
            } else {
                log::debug!("Rejecting packet from same user: {peer_userid}");
            }
        }
    }
}
