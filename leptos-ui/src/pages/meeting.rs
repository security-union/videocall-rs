// SPDX-License-Identifier: MIT OR Apache-2.0

use leptos::prelude::*;
use leptos::web_sys;
use leptos_router::hooks::use_params_map;

use crate::components::browser_compatibility::BrowserCompatibility;
use crate::constants::{
    actix_websocket_base, e2ee_enabled, server_election_period_ms, users_allowed_to_stream,
    webtransport_enabled, webtransport_host_base, CANVAS_LIMIT,
};
use crate::context::{is_valid_username, load_username_from_storage, save_username_to_storage, use_username_context};
use videocall_client::utils::is_ios;
use videocall_client::{MediaDeviceAccess, VideoCallClient, VideoCallClientOptions};
use videocall_types::protos::media_packet::media_packet::MediaType;
use wasm_bindgen::JsValue;

#[component]
pub fn MeetingRoute() -> impl IntoView {
    let params = use_params_map();
    let id = params.with_untracked(|p| p.get("id").cloned().unwrap_or_default());
    view! { <MeetingPage id/> }
}

#[component]
pub fn MeetingPage(id: String) -> impl IntoView {
    // Signals
    let username_state = use_username_context();
    let error_state: RwSignal<Option<String>> = RwSignal::new(None);

    // Read username from context or localStorage
    let initial_username = username_state
        .get()
        .or_else(|| load_username_from_storage())
        .unwrap_or_default();

    let input_value_state = RwSignal::new(initial_username);

    // UI state signals
    let share_screen = RwSignal::new(false);
    let mic_enabled = RwSignal::new(false);
    let video_enabled = RwSignal::new(false);
    let peer_list_open = RwSignal::new(false);
    let diagnostics_open = RwSignal::new(false);
    let device_settings_open = RwSignal::new(false);
    let meeting_joined = RwSignal::new(false);
    let show_copy_toast = RwSignal::new(false);

    // Create VideoCallClient on demand using a memo so options update reactively
    let client = create_rw_signal::<Option<VideoCallClient>>(None);

    // Media permission helper (wraps callbacks that use yew::Callback internally)
    let media_device_access = create_memo(move |_| {
        let mut access = MediaDeviceAccess::new();
        let on_granted = {
            let meeting_joined = meeting_joined;
            let client = client;
            leptos::prelude::Callback::new(move |_| {
                meeting_joined.set(true);
                // Connect now that permissions are granted
                if let Some(ref mut cl) = client.try_update(|c| c.clone()) {
                    let _ = cl.connect();
                }
            })
        };
        let on_denied = {
            let error_state = error_state;
            leptos::prelude::Callback::new(move |e: JsValue| {
                let msg = format!("Error requesting permissions: Please make sure to allow access to both camera and microphone. ({e:?})");
                error_state.set(Some(msg));
                meeting_joined.set(false);
            })
        };
        access.on_granted = on_granted.into();
        access.on_denied = on_denied.into();
        access
    });

    // Build client options reactively and keep a client in signal
    let build_client = {
        let id = id.clone();
        let input_value_state = input_value_state;
        move || {
            let email = input_value_state.get();
            let websocket_urls = actix_websocket_base()
                .unwrap_or_default()
                .split(',')
                .map(|s| format!("{s}/lobby/{email}/{id}"))
                .collect::<Vec<String>>();
            let webtransport_urls = webtransport_host_base()
                .unwrap_or_default()
                .split(',')
                .map(|s| format!("{s}/lobby/{email}/{id}"))
                .collect::<Vec<String>>();
            let opts = VideoCallClientOptions {
                userid: email.clone(),
                meeting_id: id.clone(),
                websocket_urls,
                webtransport_urls,
                enable_e2ee: e2ee_enabled().unwrap_or(false),
                enable_webtransport: webtransport_enabled().unwrap_or(false),
                on_connected: leptos::prelude::Callback::new(move |_| {
                    log::info!("LEPTOS-UI: Connection established");
                }),
                on_connection_lost: leptos::prelude::Callback::new(move |_| {
                    log::warn!("LEPTOS-UI: Connection lost");
                }),
                on_peer_added: leptos::prelude::Callback::new(|email: String| {
                    log::info!("New user joined: {email}");
                    if let Some(_window) = web_sys::window() {
                        if let Ok(audio) = web_sys::HtmlAudioElement::new_with_src("/assets/hi.wav") {
                            let _ = audio.play();
                        }
                    }
                }),
                on_peer_first_frame: leptos::prelude::Callback::new(|(_email, _media_type)| {}),
                on_peer_removed: None,
                get_peer_video_canvas_id: leptos::prelude::Callback::new(|email| email),
                get_peer_screen_canvas_id: leptos::prelude::Callback::new(|email| format!("screen-share-{email}")),
                enable_diagnostics: true,
                diagnostics_update_interval_ms: Some(1000),
                enable_health_reporting: true,
                health_reporting_interval_ms: Some(5000),
                on_encoder_settings_update: None,
                rtt_testing_period_ms: server_election_period_ms().unwrap_or(2000),
                rtt_probe_interval_ms: Some(200),
            };
            VideoCallClient::new(opts)
        }
    };

    // Join flow handlers
    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let value = input_value_state.get();
        if is_valid_username(&value) {
            save_username_to_storage(&value);
            // Check reset flag
            if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
                if let Ok(Some(flag)) = storage.get_item("vc_username_reset") {
                    if flag == "1" {
                        let _ = storage.remove_item("vc_username_reset");
                        if let Some(win) = web_sys::window() { let _ = win.location().reload(); }
                        return;
                    }
                }
            }
            username_state.set(Some(value));
            error_state.set(None);
        } else {
            error_state.set(Some("Please enter a valid username (letters, numbers, underscore).".to_string()));
        }
    };

    let on_input = move |ev: leptos::ev::InputEvent| {
        input_value_state.set(event_target_value(&ev));
    };

    let on_keydown = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Enter" {
            let value = input_value_state.get();
            if is_valid_username(&value) {
                save_username_to_storage(&value);
                if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
                    if let Ok(Some(flag)) = storage.get_item("vc_username_reset") {
                        if flag == "1" {
                            let _ = storage.remove_item("vc_username_reset");
                            if let Some(win) = web_sys::window() { let _ = win.location().reload(); }
                            ev.prevent_default();
                            return;
                        }
                    }
                }
                username_state.set(Some(value));
                error_state.set(None);
            } else {
                error_state.set(Some("Please enter a valid username (letters, numbers, underscore).".to_string()));
            }
            ev.prevent_default();
        }
    };

    // Meeting join action - request permissions, then connect
    let request_permissions = {
        let media_device_access = media_device_access;
        move |_| {
            media_device_access.get().request();
        }
    };

    // Build meeting link signal
    let meeting_link = create_memo(move |_| {
        let origin = web_sys::window()
            .and_then(|w| w.location().origin().ok())
            .unwrap_or_default();
        format!("{origin}/meeting/{id}")
    });

    let copy_meeting_link = {
        let meeting_link = meeting_link;
        let show_copy_toast = show_copy_toast;
        move |_| {
            if let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard()) {
                let _ = clipboard.write_text(&meeting_link.get());
                show_copy_toast.set(true);
                // Basic timeout via window.set_timeout
                let toast = show_copy_toast;
                let _ = web_sys::window().map(|w| {
                    let cb = wasm_bindgen::closure::Closure::once_into_js(move || toast.set(false));
                    // Ignore error result for timeout
                    let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 1640);
                    // leak closure intentionally; it runs once
                    drop(cb);
                });
            }
        }
    };

    // Render
    view! {
        <div id="main-container" class="meeting-page">
            <BrowserCompatibility/>
            {move || if username_state.get().is_some() {
                // Connected/connecting view
                view!{
                    <div id="grid-container" class={move || if force_desktop_grid_on_mobile() { "force-desktop-grid" } else { "" }}
                        data-peers={move || compute_num_peers_for_styling(client.get().as_ref())}
                        style={move || grid_container_style(client.get().as_ref())}>
                        {move || render_peer_tiles(client.get().as_ref())}

                        {move || if client.get().as_ref().map(|c| c.sorted_peer_keys().is_empty()).unwrap_or(true) {
                            view!{ <InviteOverlay meeting_link=meeting_link.get() on_copy=copy_meeting_link.clone() show_copy_toast=show_copy_toast.get() /> }.into_view()
                        } else { ().into_view() }}

                        {view!{ <HostControls
                            email={input_value_state.get()}
                            meeting_id=id.clone()
                            share_screen
                            mic_enabled
                            video_enabled
                            peer_list_open
                            diagnostics_open
                            device_settings_open
                            client
                        /> }}

                        <div class={move || if client.get().as_ref().map(|c| c.is_connected()).unwrap_or(false) { "connection-led connected" } else { "connection-led connecting" }} title={move || if client.get().as_ref().map(|c| c.is_connected()).unwrap_or(false) { "Connected".to_string() } else { "Connecting".to_string() }}></div>
                    </div>
                }.into_view()
            } else {
                // Username prompt view
                view!{
                    <div id="username-prompt" class="username-prompt-container">
                        <form on:submit=on_submit class="username-form">
                            <h1>{"Choose a username"}</h1>
                            <input class="username-input" placeholder="Your name" pattern="^[a-zA-Z0-9_]*$" required autofocus on:keydown=on_keydown on:input=on_input prop:value=input_value_state.get()/>
                            {move || error_state.get().map(|e| view!{ <p class="error">{e}</p> })}
                            <button class="cta-button" type="submit">{"Continue"}</button>
                        </form>
                    </div>
                }.into_view()
            }}

            {move || if !meeting_joined.get() {
                view!{
                    <div id="join-meeting-container" style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000; z-index: 1000;">
                        <div style="text-align: center; color: white; margin-bottom: 2rem;">
                            <h2>{"Ready to join the meeting?"}</h2>
                            <p>{"Click the button below to join and start listening to others."}</p>
                            {move || error_state.get().map(|e| view!{ <p style="color: #ff6b6b; margin-top: 1rem;">{e}</p> }).unwrap_or_default()}
                        </div>
                        <button class="btn-apple btn-primary" on:click=request_permissions>{"Join Meeting"}</button>
                    </div>
                }.into_view()
            } else { ().into_view() }}
        </div>
    }
}

fn compute_num_peers_for_styling(client: Option<&VideoCallClient>) -> String {
    let count = client
        .map(|c| c.sorted_peer_keys().len())
        .unwrap_or(0)
        .min(CANVAS_LIMIT)
        .max(1);
    count.to_string()
}

fn grid_container_style(client: Option<&VideoCallClient>) -> String {
    let n = client.map(|c| c.sorted_peer_keys().len()).unwrap_or(0).min(CANVAS_LIMIT).max(1);
    format!("position: absolute; inset: 0; width: 100%; height: 100%; --num-peers: {};", n)
}

fn render_peer_tiles(client: Option<&VideoCallClient>) -> View {
    use crate::components::peer_tiles::generate_for_peer_view;
    if let Some(c) = client {
        let mut peers = c.sorted_peer_keys();
        let num = peers.len();
        let rows = peers
            .drain(..)
            .take(CANVAS_LIMIT)
            .enumerate()
            .map(|(i, id)| {
                let full_bleed = num == 1 && !c.is_screen_share_enabled_for_peer(&id);
                generate_for_peer_view(c.clone(), id, full_bleed)
            })
            .collect::<Vec<_>>();
        View::from(rows)
    } else {
        ().into_view()
    }
}

fn force_desktop_grid_on_mobile() -> bool {
    // Keep default true as in Yew variant
    true
}

#[component]
fn InviteOverlay(meeting_link: String, on_copy: Callback<()>, show_copy_toast: bool) -> impl IntoView {
    view! {
        <div id="invite-overlay" class="card-apple" style="position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); width: 90%; max-width: 420px; z-index: 0; text-align: center;">
            <h4 style="margin-top:0;">{"Your meeting is ready!"}</h4>
            <p style="font-size: 0.9rem; opacity: 0.8;">{"Share this meeting link with others you want in the meeting"}</p>
            <div style="display:flex; align-items:center; margin-top: 0.75rem; margin-bottom: 0.75rem;">
                <input id="meeting-link-input" value=meeting_link readonly class="input-apple" style="flex:1; overflow:hidden; text-overflow: ellipsis;"/>
                <button class=move || if show_copy_toast { "btn-apple btn-primary btn-sm copy-button btn-pop-animate" } else { "btn-apple btn-primary btn-sm copy-button" } style="margin-left: 0.5rem;" on:click=move |_| on_copy(()) >
                    {"Copy"}
                    {move || if show_copy_toast { view!{<div class="sparkles" aria-hidden="true">
                        <span class="sparkle"></span><span class="sparkle"></span><span class="sparkle"></span><span class="sparkle"></span><span class="sparkle"></span><span class="sparkle"></span><span class="sparkle"></span><span class="sparkle"></span>
                    </div>} } else { ().into_view() }}
                </button>
            </div>
            <p style="font-size: 0.8rem; opacity: 0.7;">{"People who use this meeting link must get your permission before they can join."}</p>
            <div class=move || if show_copy_toast { "copy-toast copy-toast--visible" } else { "copy-toast" } role="alert" aria-live="assertive" aria-hidden=move || (!show_copy_toast).to_string()>
                {"Link copied to clipboard"}
            </div>
        </div>
    }
}

#[component]
fn HostControls(
    email: String,
    meeting_id: String,
    share_screen: RwSignal<bool>,
    mic_enabled: RwSignal<bool>,
    video_enabled: RwSignal<bool>,
    peer_list_open: RwSignal<bool>,
    diagnostics_open: RwSignal<bool>,
    device_settings_open: RwSignal<bool>,
    client: RwSignal<Option<VideoCallClient>>,
) -> impl IntoView {
    // Toggle actions
    let toggle_peer_list = move |_| peer_list_open.update(|v| *v = !*v);
    let toggle_diagnostics = move |_| diagnostics_open.update(|v| *v = !*v);
    let toggle_device_settings = move |_| device_settings_open.update(|v| *v = !*v);

    // Join connect on first click handled upstream

    view! {
        {move || if users_allowed_to_stream().unwrap_or_default().iter().any(|host| host == &email) || users_allowed_to_stream().unwrap_or_default().is_empty() {
            view!{
                <nav class="host">
                    <div class="controls">
                        <nav class="video-controls-container">
                            <button class=move || if mic_enabled.get() { "video-control-button active" } else { "video-control-button" }
                                on:click=move |_| {
                                    if !mic_enabled.get() { mic_enabled.set(true); } else { mic_enabled.set(false); }
                                    if let Some(c) = client.get() { c.set_audio_enabled(mic_enabled.get()); }
                                }>
                                <span class="tooltip">{move || if mic_enabled.get() { "Mute" } else { "Unmute" }}</span>
                            </button>
                            <button class=move || if video_enabled.get() { "video-control-button active" } else { "video-control-button" }
                                on:click=move |_| {
                                    if !video_enabled.get() { video_enabled.set(true); } else { video_enabled.set(false); }
                                    if let Some(c) = client.get() { c.set_video_enabled(video_enabled.get()); }
                                }>
                                <span class="tooltip">{move || if video_enabled.get() { "Stop Video" } else { "Start Video" }}</span>
                            </button>
                            {move || if !is_ios() { view!{
                                <button class=move || if share_screen.get() { "video-control-button active" } else { "video-control-button" }
                                    on:click=move |_| {
                                        if !share_screen.get() { share_screen.set(true); } else { share_screen.set(false); }
                                        if let Some(c) = client.get() { c.set_screen_enabled(share_screen.get()); }
                                    }>
                                    <span class="tooltip">{move || if share_screen.get() { "Stop Screen Share" } else { "Share Screen" }}</span>
                                </button>
                            }.into_view() } else { ().into_view() }}
                            <button class=move || if peer_list_open.get() { "video-control-button active" } else { "video-control-button" } on:click=toggle_peer_list>
                                <span class="tooltip">{move || if peer_list_open.get() { "Close Peers" } else { "Open Peers" }}</span>
                            </button>
                            <button class=move || if diagnostics_open.get() { "video-control-button active" } else { "video-control-button" } on:click=toggle_diagnostics>
                                <span class="tooltip">{move || if diagnostics_open.get() { "Close Diagnostics" } else { "Open Diagnostics" }}</span>
                            </button>
                            <button class=move || if device_settings_open.get() { "video-control-button mobile-only-device-settings active" } else { "video-control-button mobile-only-device-settings" } on:click=toggle_device_settings>
                                <span class="tooltip">{move || if device_settings_open.get() { "Close Settings" } else { "Device Settings" }}</span>
                            </button>
                            <button class="video-control-button danger" on:click=move |_| {
                                if let Some(win) = web_sys::window() { let _ = win.location().reload(); }
                            }>
                                <span class="tooltip">{"Hang Up"}</span>
                            </button>
                        </nav>
                    </div>
                </nav>
            }.into_view()
        } else { ().into_view() }}
    }
}
