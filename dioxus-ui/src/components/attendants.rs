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

//! AttendantsComponent - Main meeting orchestrator
//!
//! This component manages the VideoCallClient connection, peer state,
//! and coordinates all the sub-components (Host, PeerTile, etc.)

use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use gloo_utils::window;
use videocall_client::{MediaDeviceAccess, ScreenShareEvent, VideoCallClient};
use web_sys::HtmlAudioElement;

use crate::components::browser_compatibility::BrowserCompatibility;
use crate::components::diagnostics::Diagnostics;
use crate::components::host::Host;
use crate::components::host_controls::HostControls;
use crate::components::meeting_ended_overlay::MeetingEndedOverlay;
use crate::components::peer_list::PeerList;
use crate::components::peer_tile::PeerTile;
use crate::components::video_control_buttons::{
    CameraButton, DeviceSettingsButton, DiagnosticsButton, HangUpButton, MicButton, PeerListButton,
    ScreenShareButton,
};
use crate::constants::{users_allowed_to_stream, CANVAS_LIMIT};
use crate::context::{MeetingTime, MeetingTimeCtx, VideoCallClientCtx};
use crate::hooks::use_video_call_client::{
    build_lobby_urls, create_video_call_client, VideoCallClientConfig, VideoCallEvent,
};
use videocall_client::utils::is_ios;

#[derive(Clone, Debug, PartialEq)]
pub enum ScreenShareState {
    Idle,
    Requesting,
    Active,
}

impl ScreenShareState {
    pub fn is_sharing(&self) -> bool {
        !matches!(self, ScreenShareState::Idle)
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct AttendantsComponentProps {
    #[props(default)]
    pub id: String,
    #[props(default)]
    pub email: String,
    pub e2ee_enabled: bool,
    pub webtransport_enabled: bool,
    #[props(default)]
    pub user_name: Option<String>,
    #[props(default)]
    pub user_email: Option<String>,
    #[props(default)]
    pub on_logout: Option<EventHandler<()>>,
    #[props(default)]
    pub host_display_name: Option<String>,
    #[props(default = false)]
    pub auto_join: bool,
    #[props(default = false)]
    pub is_owner: bool,
    #[props(default)]
    pub room_token: String,
}

#[component]
pub fn AttendantsComponent(props: AttendantsComponentProps) -> Element {
    // Core state
    let mut client: Signal<Option<VideoCallClient>> = use_signal(|| None);
    let mut media_device_access: Signal<Option<MediaDeviceAccess>> = use_signal(|| None);

    // UI state
    let mut screen_share_state = use_signal(|| ScreenShareState::Idle);
    let mut mic_enabled = use_signal(|| false);
    let mut video_enabled = use_signal(|| false);
    let mut peer_list_open = use_signal(|| false);
    let mut diagnostics_open = use_signal(|| false);
    let mut device_settings_open = use_signal(|| false);
    let mut meeting_info_open = use_signal(|| false);

    // Connection state
    let mut error = use_signal(|| None::<String>);
    let mut meeting_joined = use_signal(|| false);
    let mut user_error = use_signal(|| None::<String>);

    // Peer state
    let mut peers = use_signal(Vec::<String>::new);

    // Meeting timing
    let mut call_start_time = use_signal(|| None::<f64>);
    let mut meeting_start_time_server = use_signal(|| None::<f64>);

    // Meeting ended
    let mut meeting_ended_message = use_signal(|| None::<String>);

    // Dropdown state
    let mut show_dropdown = use_signal(|| false);

    // Show copy toast
    let mut show_copy_toast = use_signal(|| false);

    // Initialize MediaDeviceAccess
    let email = props.email.clone();
    let id = props.id.clone();
    let room_token = props.room_token.clone();
    let e2ee_enabled = props.e2ee_enabled;
    let webtransport_enabled = props.webtransport_enabled;
    let auto_join = props.auto_join;

    use_effect(move || {
        // Create MediaDeviceAccess
        let mut mda = MediaDeviceAccess::new();

        mda.on_granted = {
            // Clone values for use in the callback
            let email_for_granted = email.clone();
            let id_for_granted = id.clone();
            let room_token_for_granted = room_token.clone();

            yew::Callback::from(move |_| {
                log::info!("DIOXUS-UI: Media permissions granted");
                // Clone again for the async block
                let email = email_for_granted.clone();
                let id = id_for_granted.clone();
                let room_token = room_token_for_granted.clone();

                // Use spawn_local to defer signal mutations (makes callback Fn instead of FnMut)
                wasm_bindgen_futures::spawn_local(async move {
                    // Create and connect client after permissions granted
                    let config = VideoCallClientConfig {
                        userid: email,
                        meeting_id: id,
                        room_token,
                        e2ee_enabled,
                        webtransport_enabled,
                    };

                    let mut vcc = create_video_call_client(config, move |event| {
                        // Each event handler also uses spawn_local to defer mutations
                        match event {
                            VideoCallEvent::Connected => {
                                log::info!("DIOXUS-UI: Connected!");
                                wasm_bindgen_futures::spawn_local(async move {
                                    error.set(None);
                                    call_start_time.set(Some(js_sys::Date::now()));
                                });
                            }
                            VideoCallEvent::ConnectionLost => {
                                log::warn!("DIOXUS-UI: Connection lost");
                                wasm_bindgen_futures::spawn_local(async move {
                                    error.set(Some("Connection lost, reconnecting...".to_string()));
                                });
                                // Schedule reconnection
                                Timeout::new(1000, move || {
                                    if let Some(ref mut c) = *client.write() {
                                        if let Err(e) = c.connect() {
                                            log::error!("Reconnection failed: {e:?}");
                                        }
                                    }
                                })
                                .forget();
                            }
                            VideoCallEvent::PeerAdded(peer_id) => {
                                log::info!("DIOXUS-UI: Peer added: {peer_id}");
                                wasm_bindgen_futures::spawn_local(async move {
                                    peers.write().push(peer_id);
                                });
                                // Play notification sound
                                play_user_joined();
                            }
                            VideoCallEvent::PeerRemoved(peer_id) => {
                                log::info!("DIOXUS-UI: Peer removed: {peer_id}");
                                wasm_bindgen_futures::spawn_local(async move {
                                    peers.write().retain(|p| p != &peer_id);
                                });
                            }
                            VideoCallEvent::FirstFrame(_peer_id, _media_type) => {
                                // Handled by PeerTile
                            }
                            VideoCallEvent::EncoderSettingsUpdated(_settings) => {
                                // Handled by Host
                            }
                            VideoCallEvent::MeetingInfo(start_time) => {
                                log::info!("DIOXUS-UI: Meeting info received, start_time: {start_time}");
                                wasm_bindgen_futures::spawn_local(async move {
                                    meeting_start_time_server.set(Some(start_time as f64));
                                });
                            }
                            VideoCallEvent::MeetingEnded(message) => {
                                log::info!("DIOXUS-UI: Meeting ended");
                                wasm_bindgen_futures::spawn_local(async move {
                                    meeting_ended_message.set(Some(message));
                                });
                            }
                        }
                    });

                    // Connect
                    if let Err(e) = vcc.connect() {
                        log::error!("Connection failed: {e:?}");
                        error.set(Some(format!("Connection failed: {e:?}")));
                    }

                    client.set(Some(vcc));
                    meeting_joined.set(true);
                });
            })
        };

        mda.on_denied = {
            yew::Callback::from(move |e| {
                let msg = format!("Error requesting permissions: Please make sure to allow access to both camera and microphone. ({e:?})");
                log::error!("{msg}");
                // Use spawn_local to defer signal mutations
                wasm_bindgen_futures::spawn_local(async move {
                    error.set(Some(msg));
                    meeting_joined.set(false);
                });
            })
        };

        media_device_access.set(Some(mda));

        // Auto-join if requested
        if auto_join {
            if let Some(ref mda) = *media_device_access.read() {
                mda.request();
            }
        }
    });

    // Action handlers
    let request_media_permissions = move |_| {
        if let Some(ref mda) = *media_device_access.read() {
            mda.request();
        }
    };

    let toggle_mic = move |_| {
        let new_state = !*mic_enabled.read();
        if new_state {
            if media_device_access
                .read()
                .as_ref()
                .map(|m| m.is_granted())
                .unwrap_or(false)
            {
                mic_enabled.set(true);
            } else {
                if let Some(ref mda) = *media_device_access.read() {
                    mda.request();
                }
            }
        } else {
            mic_enabled.set(false);
        }
    };

    let toggle_video = move |_| {
        let new_state = !*video_enabled.read();
        if new_state {
            if media_device_access
                .read()
                .as_ref()
                .map(|m| m.is_granted())
                .unwrap_or(false)
            {
                video_enabled.set(true);
            } else {
                if let Some(ref mda) = *media_device_access.read() {
                    mda.request();
                }
            }
        } else {
            video_enabled.set(false);
        }
    };

    let toggle_screen_share = move |_| {
        if matches!(*screen_share_state.read(), ScreenShareState::Idle) {
            screen_share_state.set(ScreenShareState::Requesting);
        } else {
            screen_share_state.set(ScreenShareState::Idle);
        }
    };

    let on_screen_share_state_change = move |event: ScreenShareEvent| {
        match event {
            ScreenShareEvent::Started => {
                screen_share_state.set(ScreenShareState::Active);
            }
            ScreenShareEvent::Cancelled | ScreenShareEvent::Stopped => {
                screen_share_state.set(ScreenShareState::Idle);
            }
            ScreenShareEvent::Failed(msg) => {
                log::error!("Screen share failed: {msg}");
                screen_share_state.set(ScreenShareState::Idle);
                user_error.set(Some(format!("Screen share failed: {msg}")));
            }
        }
    };

    let hang_up = {
        let meeting_id = props.id.clone();
        move |_| {
            log::info!("Hanging up - resetting to initial state");

            if let Some(ref c) = *client.read() {
                if c.is_connected() {
                    if let Err(e) = c.disconnect() {
                        log::error!("Error disconnecting: {e}");
                    }
                }
            }

            meeting_joined.set(false);
            mic_enabled.set(false);
            video_enabled.set(false);
            call_start_time.set(None);
            meeting_start_time_server.set(None);

            // Call leave_meeting API and redirect
            let meeting_id = meeting_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = crate::meeting_api::leave_meeting(&meeting_id).await {
                    log::error!("Error leaving meeting: {e}");
                }
                let _ = window().location().set_href("/");
            });
        }
    };

    // Meeting link for invitation
    let meeting_link = {
        let origin = window()
            .location()
            .origin()
            .unwrap_or_else(|_| String::new());
        format!("{}/meeting/{}", origin, props.id)
    };

    let copy_meeting_link = {
        let meeting_link = meeting_link.clone();
        move |_| {
            if let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard()) {
                let _ = clipboard.write_text(&meeting_link);
                show_copy_toast.set(true);
                Timeout::new(1640, move || {
                    show_copy_toast.set(false);
                })
                .forget();
            }
        }
    };

    // Compute display values
    let display_peers = peers.read().clone();
    let num_display_peers = display_peers.len();
    let num_peers_for_styling = num_display_peers.min(CANVAS_LIMIT);
    let media_access_granted = media_device_access
        .read()
        .as_ref()
        .map(|m| m.is_granted())
        .unwrap_or(false);
    let is_connected = client
        .read()
        .as_ref()
        .map(|c| c.is_connected())
        .unwrap_or(false);

    // Show Join Meeting button if user hasn't joined yet
    if !*meeting_joined.read() {
        return rsx! {
            div { id: "main-container", class: "meeting-page",
                BrowserCompatibility {}
                div {
                    id: "join-meeting-container",
                    style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000; z-index: 1000;",

                    // Logout dropdown
                    if let (Some(name), Some(email_addr)) = (&props.user_name, &props.user_email) {
                        {
                            let logout_handler = props.on_logout.clone();
                            rsx! {
                                div {
                                    style: "position: absolute; top: 1rem; right: 1rem; z-index: 1001;",
                                    button {
                                        onclick: move |_| { let v = *show_dropdown.read(); show_dropdown.set(!v) },
                                        style: "display: flex; align-items: center; gap: 0.5rem; padding: 0.5rem 1rem; background: #1f2937; border-radius: 0.5rem; color: white; font-size: 0.875rem; border: none; cursor: pointer;",
                                        span { "{name}" }
                                        svg {
                                            style: "width: 1rem; height: 1rem;",
                                            fill: "none",
                                            stroke: "currentColor",
                                            view_box: "0 0 24 24",
                                            path {
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
                                                stroke_width: "2",
                                                d: "M19 9l-7 7-7-7"
                                            }
                                        }
                                    }

                                    if *show_dropdown.read() {
                                        div {
                                            style: "position: absolute; right: 0; margin-top: 0.5rem; width: 14rem; background: white; border-radius: 0.5rem; box-shadow: 0 10px 15px -3px rgba(0, 0, 0, 0.1); border: 1px solid #e5e7eb; padding: 0.25rem 0;",
                                            div {
                                                style: "padding: 0.75rem 1rem; border-bottom: 1px solid #e5e7eb;",
                                                p { style: "font-size: 0.875rem; font-weight: 500; color: #111827; margin: 0;", "{name}" }
                                                p { style: "font-size: 0.75rem; color: #6b7280; margin: 0; overflow: hidden; text-overflow: ellipsis;", "{email_addr}" }
                                            }
                                            button {
                                                onclick: move |_| {
                                                    if let Some(ref handler) = logout_handler {
                                                        handler.call(());
                                                    }
                                                },
                                                style: "width: 100%; text-align: left; padding: 0.5rem 1rem; font-size: 0.875rem; color: #dc2626; background: transparent; border: none; cursor: pointer;",
                                                "Sign out"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div {
                        style: "text-align: center; color: white; margin-bottom: 2rem;",
                        h2 { "Ready to join the meeting?" }
                        p { "Click the button below to join and start listening to others." }
                        if let Some(err) = error.read().as_ref() {
                            p { style: "color: #ff6b6b; margin-top: 1rem;", "{err}" }
                        }
                    }
                    button {
                        class: "btn-apple btn-primary",
                        onclick: request_media_permissions,
                        if props.is_owner { "Start Meeting" } else { "Join Meeting" }
                    }
                }
            }
        };
    }

    // Create MeetingTime for context
    let meeting_time = MeetingTime {
        call_start_time: *call_start_time.read(),
        meeting_start_time: *meeting_start_time_server.read(),
    };

    let container_style = format!(
        "position: absolute; inset: 0; width: 100%; height: 100%; --num-peers: {};",
        num_peers_for_styling.max(1)
    );

    let host_display_name = props.host_display_name.clone();

    rsx! {
        // Provide context for child components
        {
            if let Some(ref c) = *client.read() {
                // Use context provider when client exists
                rsx! {
                    div { id: "main-container", class: "meeting-page",
                        BrowserCompatibility {}
                        div {
                            id: "grid-container",
                            "data-peers": "{num_peers_for_styling}",
                            style: "{container_style}",

                            // Peer tiles
                            for (i, peer_id) in display_peers.iter().take(CANVAS_LIMIT).enumerate() {
                                {
                                    let full_bleed = display_peers.len() == 1 && !c.is_screen_share_enabled_for_peer(peer_id);
                                    rsx! {
                                        PeerTile {
                                            key: "{i}-{peer_id}",
                                            peer_id: peer_id.clone(),
                                            full_bleed: full_bleed,
                                            host_display_name: host_display_name.clone()
                                        }
                                    }
                                }
                            }

                            // Invitation overlay when no peers
                            if num_display_peers == 0 {
                                div {
                                    id: "invite-overlay",
                                    class: "card-apple",
                                    style: "position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); width: 90%; max-width: 420px; z-index: 0; text-align: center;",
                                    h4 { style: "margin-top:0;", "Your meeting is ready!" }
                                    p { style: "font-size: 0.9rem; opacity: 0.8;", "Share this meeting link with others you want in the meeting" }
                                    div { style: "display:flex; align-items:center; margin-top: 0.75rem; margin-bottom: 0.75rem;",
                                        input {
                                            id: "meeting-link-input",
                                            value: "{meeting_link}",
                                            readonly: true,
                                            class: "input-apple",
                                            style: "flex:1; overflow:hidden; text-overflow: ellipsis;"
                                        }
                                        button {
                                            class: if *show_copy_toast.read() { "btn-apple btn-primary btn-sm copy-button btn-pop-animate" } else { "btn-apple btn-primary btn-sm copy-button" },
                                            style: "margin-left: 0.5rem;",
                                            onclick: copy_meeting_link,
                                            "Copy"
                                            if *show_copy_toast.read() {
                                                div { class: "sparkles", aria_hidden: "true",
                                                    span { class: "sparkle" }
                                                    span { class: "sparkle" }
                                                    span { class: "sparkle" }
                                                    span { class: "sparkle" }
                                                }
                                            }
                                        }
                                    }
                                    p { style: "font-size: 0.8rem; opacity: 0.7;", "People who use this meeting link must get your permission before they can join." }
                                    div {
                                        class: if *show_copy_toast.read() { "copy-toast copy-toast--visible" } else { "copy-toast" },
                                        role: "alert",
                                        aria_live: "assertive",
                                        "Link copied to clipboard"
                                    }
                                }
                            }

                            // Host controls nav
                            {
                                let allowed = users_allowed_to_stream().unwrap_or_default();
                                if allowed.is_empty() || allowed.iter().any(|host| host == &props.email) {
                                    rsx! {
                                        nav { class: "host",
                                            div { class: "controls",
                                                nav { class: "video-controls-container",
                                                    MicButton {
                                                        enabled: *mic_enabled.read(),
                                                        onclick: toggle_mic
                                                    }
                                                    CameraButton {
                                                        enabled: *video_enabled.read(),
                                                        onclick: toggle_video
                                                    }
                                                    if !is_ios() {
                                                        ScreenShareButton {
                                                            active: matches!(*screen_share_state.read(), ScreenShareState::Active),
                                                            disabled: matches!(*screen_share_state.read(), ScreenShareState::Requesting),
                                                            onclick: toggle_screen_share
                                                        }
                                                    }
                                                    PeerListButton {
                                                        open: *peer_list_open.read(),
                                                        onclick: move |_| {
                                                            { let v = *peer_list_open.read(); peer_list_open.set(!v) };
                                                            if *peer_list_open.read() {
                                                                diagnostics_open.set(false);
                                                            }
                                                        }
                                                    }
                                                    DiagnosticsButton {
                                                        open: *diagnostics_open.read(),
                                                        onclick: move |_| {
                                                            { let v = *diagnostics_open.read(); diagnostics_open.set(!v) };
                                                            if *diagnostics_open.read() {
                                                                peer_list_open.set(false);
                                                            }
                                                        }
                                                    }
                                                    DeviceSettingsButton {
                                                        open: *device_settings_open.read(),
                                                        onclick: move |_| {
                                                            { let v = *device_settings_open.read(); device_settings_open.set(!v) };
                                                            if *device_settings_open.read() {
                                                                peer_list_open.set(false);
                                                                diagnostics_open.set(false);
                                                            }
                                                        }
                                                    }
                                                    HangUpButton {
                                                        onclick: hang_up
                                                    }
                                                }
                                            }

                                            // User error modal
                                            if let Some(err) = user_error.read().as_ref() {
                                                div { class: "glass-backdrop",
                                                    div { class: "card-apple", style: "width: 380px;",
                                                        h4 { style: "margin-top:0;", "Error" }
                                                        p { style: "margin-top:0.5rem;", "{err}" }
                                                        div { style: "display:flex; gap:8px; justify-content:flex-end; margin-top:12px;",
                                                            button {
                                                                class: "btn-apple btn-primary btn-sm",
                                                                onclick: move |_| user_error.set(None),
                                                                "OK"
                                                            }
                                                        }
                                                    }
                                                }
                                            }

                                            // Host component for encoders
                                            if media_access_granted {
                                                Host {
                                                    share_screen: screen_share_state.read().is_sharing(),
                                                    mic_enabled: *mic_enabled.read(),
                                                    video_enabled: *video_enabled.read(),
                                                    on_encoder_settings_update: |_settings: String| {},
                                                    device_settings_open: *device_settings_open.read(),
                                                    on_device_settings_toggle: move |_| { let v = *device_settings_open.read(); device_settings_open.set(!v) },
                                                    on_microphone_error: move |err: String| {
                                                        mic_enabled.set(false);
                                                        user_error.set(Some(format!("Microphone error: {err}")));
                                                    },
                                                    on_camera_error: move |err: String| {
                                                        video_enabled.set(false);
                                                        user_error.set(Some(format!("Camera error: {err}")));
                                                    },
                                                    on_screen_share_state: on_screen_share_state_change
                                                }
                                            }

                                            // Connection LED
                                            div {
                                                class: if is_connected { "connection-led connected" } else { "connection-led connecting" },
                                                title: if is_connected { "Connected" } else { "Connecting" }
                                            }
                                        }
                                    }
                                } else {
                                    rsx! {}
                                }
                            }
                        }

                        // Peer list sidebar
                        div {
                            id: "peer-list-container",
                            class: if *peer_list_open.read() { "visible" } else { "" },
                            if *peer_list_open.read() {
                                PeerList {
                                    peers: display_peers.clone(),
                                    onclose: move |_| peer_list_open.set(false),
                                    show_meeting_info: *meeting_info_open.read(),
                                    room_id: props.id.clone(),
                                    num_participants: num_display_peers,
                                    is_active: *meeting_joined.read() && meeting_ended_message.read().is_none(),
                                    on_toggle_meeting_info: move |_| { let v = *meeting_info_open.read(); meeting_info_open.set(!v) },
                                    host_display_name: props.host_display_name.clone()
                                }
                            }
                        }

                        // Host controls for waiting room
                        HostControls {
                            meeting_id: props.id.clone(),
                            is_admitted: true
                        }

                        // Meeting ended overlay
                        if let Some(message) = meeting_ended_message.read().as_ref() {
                            MeetingEndedOverlay { message: message.clone() }
                        }

                        // Diagnostics sidebar
                        if *diagnostics_open.read() {
                            Diagnostics {
                                is_open: true,
                                on_close: move |_| diagnostics_open.set(false),
                                video_enabled: *video_enabled.read(),
                                mic_enabled: *mic_enabled.read(),
                                share_screen: screen_share_state.read().is_sharing()
                            }
                        }
                    }
                }
            } else {
                rsx! {
                    div { id: "main-container", class: "meeting-page",
                        BrowserCompatibility {}
                        div {
                            style: "position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); text-align: center; color: white;",
                            p { "Connecting..." }
                        }
                    }
                }
            }
        }
    }
}

fn play_user_joined() {
    if let Some(_window) = web_sys::window() {
        if let Ok(audio) = HtmlAudioElement::new_with_src("/assets/hi.wav") {
            audio.set_volume(0.4);
            if let Err(e) = audio.play() {
                log::warn!("Failed to play notification sound: {e:?}");
            }
        } else {
            log::warn!("Failed to create audio element for notification sound");
        }
    }
}
