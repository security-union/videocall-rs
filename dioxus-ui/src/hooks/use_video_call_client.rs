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

//! VideoCallClient hook and Yew-to-Dioxus callback bridge
//!
//! This module provides the bridge between Yew callbacks (used by videocall-client)
//! and Dioxus signals for reactive state updates.

use dioxus::prelude::*;
use videocall_client::{VideoCallClient, VideoCallClientOptions};
use videocall_types::protos::media_packet::media_packet::MediaType;

use crate::constants::{
    actix_websocket_base, server_election_period_ms, webtransport_host_base,
};

/// Configuration for creating a VideoCallClient
#[derive(Clone)]
pub struct VideoCallClientConfig {
    pub userid: String,
    pub meeting_id: String,
    pub room_token: String,
    pub e2ee_enabled: bool,
    pub webtransport_enabled: bool,
}

/// Events emitted by the VideoCallClient that need to be handled by the UI
#[derive(Clone, Debug)]
pub enum VideoCallEvent {
    Connected,
    ConnectionLost,
    PeerAdded(String),
    PeerRemoved(String),
    FirstFrame(String, MediaType),
    EncoderSettingsUpdated(String),
    MeetingInfo(u64),
    MeetingEnded(String),
}

/// Build the WebSocket and WebTransport lobby URLs for the media server.
#[allow(unused_variables)]
pub fn build_lobby_urls(token: &str, email: &str, id: &str) -> (Vec<String>, Vec<String>) {
    #[cfg(feature = "media-server-jwt-auth")]
    let lobby_url = |base: &str| format!("{base}/lobby?token={token}");

    #[cfg(not(feature = "media-server-jwt-auth"))]
    let lobby_url = |base: &str| format!("{base}/lobby/{email}/{id}");

    let websocket_urls = actix_websocket_base()
        .unwrap_or_default()
        .split(',')
        .map(lobby_url)
        .collect::<Vec<String>>();
    let webtransport_urls = webtransport_host_base()
        .unwrap_or_default()
        .split(',')
        .map(lobby_url)
        .collect::<Vec<String>>();

    (websocket_urls, webtransport_urls)
}

/// Creates a VideoCallClient with Dioxus-compatible callbacks
///
/// This function bridges between Yew Callbacks (used by videocall-client)
/// and Dioxus signals for reactive updates.
pub fn create_video_call_client(
    config: VideoCallClientConfig,
    on_event: impl Fn(VideoCallEvent) + 'static + Clone,
) -> VideoCallClient {
    #[cfg(feature = "media-server-jwt-auth")]
    let token = {
        let t = config.room_token.clone();
        assert!(
            !t.is_empty(),
            "media-server-jwt-auth is enabled but room_token is empty"
        );
        t
    };

    #[cfg(not(feature = "media-server-jwt-auth"))]
    let token = String::new();

    let (websocket_urls, webtransport_urls) =
        build_lobby_urls(&token, &config.userid, &config.meeting_id);

    log::info!(
        "DIOXUS-UI: Creating VideoCallClient for {} in meeting {} with webtransport_enabled={}, jwt_auth={}",
        config.userid,
        config.meeting_id,
        config.webtransport_enabled,
        cfg!(feature = "media-server-jwt-auth"),
    );

    if websocket_urls.is_empty() || webtransport_urls.is_empty() {
        log::error!("Runtime config missing or invalid: wsUrl or webTransportHost not set");
    }

    log::info!("DIOXUS-UI: WebSocket URLs: {websocket_urls:?}");
    log::info!("DIOXUS-UI: WebTransport URLs: {webtransport_urls:?}");

    // Create Yew callbacks that forward to the Dioxus event handler
    let on_connected = {
        let on_event = on_event.clone();
        yew::Callback::from(move |_| {
            log::info!("DIOXUS-UI: Connection established");
            on_event(VideoCallEvent::Connected);
        })
    };

    let on_connection_lost = {
        let on_event = on_event.clone();
        yew::Callback::from(move |_| {
            log::warn!("DIOXUS-UI: Connection lost");
            on_event(VideoCallEvent::ConnectionLost);
        })
    };

    let on_peer_added = {
        let on_event = on_event.clone();
        yew::Callback::from(move |peer_id: String| {
            log::info!("DIOXUS-UI: Peer added: {peer_id}");
            on_event(VideoCallEvent::PeerAdded(peer_id));
        })
    };

    let on_peer_first_frame = {
        let on_event = on_event.clone();
        yew::Callback::from(move |(peer_id, media_type): (String, MediaType)| {
            log::info!("DIOXUS-UI: First frame from peer {peer_id}");
            on_event(VideoCallEvent::FirstFrame(peer_id, media_type));
        })
    };

    let on_peer_removed = {
        let on_event = on_event.clone();
        yew::Callback::from(move |peer_id: String| {
            log::info!("DIOXUS-UI: Peer removed: {peer_id}");
            on_event(VideoCallEvent::PeerRemoved(peer_id));
        })
    };

    let on_encoder_settings_update = {
        let on_event = on_event.clone();
        yew::Callback::from(move |settings: String| {
            on_event(VideoCallEvent::EncoderSettingsUpdated(settings));
        })
    };

    let on_meeting_info = {
        let on_event = on_event.clone();
        yew::Callback::from(move |start_time_ms: f64| {
            log::info!("DIOXUS-UI: Meeting started at Unix timestamp: {start_time_ms}");
            on_event(VideoCallEvent::MeetingInfo(start_time_ms as u64));
        })
    };

    let on_meeting_ended = {
        let on_event = on_event.clone();
        yew::Callback::from(move |(_end_time_ms, message): (f64, String)| {
            log::info!("DIOXUS-UI: Meeting ended");
            on_event(VideoCallEvent::MeetingEnded(message));
        })
    };

    let opts = VideoCallClientOptions {
        userid: config.userid.clone(),
        meeting_id: config.meeting_id.clone(),
        websocket_urls,
        webtransport_urls,
        enable_e2ee: config.e2ee_enabled,
        enable_webtransport: config.webtransport_enabled,
        on_connected,
        on_connection_lost,
        on_peer_added,
        on_peer_first_frame,
        on_peer_removed: Some(on_peer_removed),
        get_peer_video_canvas_id: yew::Callback::from(|email| email),
        get_peer_screen_canvas_id: yew::Callback::from(|email| format!("screen-share-{}", &email)),
        enable_diagnostics: true,
        diagnostics_update_interval_ms: Some(1000),
        enable_health_reporting: true,
        health_reporting_interval_ms: Some(5000),
        on_encoder_settings_update: Some(on_encoder_settings_update),
        rtt_testing_period_ms: server_election_period_ms().unwrap_or(2000),
        rtt_probe_interval_ms: Some(200),
        on_meeting_info: Some(on_meeting_info),
        on_meeting_ended: Some(on_meeting_ended),
    };

    VideoCallClient::new(opts)
}

/// A wrapper around Signal that provides a simple interface for the VideoCallClient
/// peer state updates.
#[derive(Clone, Default)]
pub struct PeerState {
    pub peers: Vec<String>,
    pub screen_sharing_peers: Vec<String>,
}

/// Hook to manage peer state updates from VideoCallClient events
pub fn use_peer_state() -> (Signal<PeerState>, impl FnMut(VideoCallEvent)) {
    let mut peer_state = use_signal(PeerState::default);

    let handle_event = move |event: VideoCallEvent| {
        match event {
            VideoCallEvent::PeerAdded(peer_id) => {
                peer_state.write().peers.push(peer_id);
            }
            VideoCallEvent::PeerRemoved(peer_id) => {
                peer_state.write().peers.retain(|p| p != &peer_id);
                peer_state
                    .write()
                    .screen_sharing_peers
                    .retain(|p| p != &peer_id);
            }
            VideoCallEvent::FirstFrame(peer_id, media_type) => {
                if matches!(media_type, MediaType::SCREEN) {
                    let mut state = peer_state.write();
                    if !state.screen_sharing_peers.contains(&peer_id) {
                        state.screen_sharing_peers.push(peer_id);
                    }
                }
            }
            _ => {}
        }
    };

    (peer_state, handle_event)
}
