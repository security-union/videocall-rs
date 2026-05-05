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

use crate::components::{
    browser_compatibility::BrowserCompatibility,
    canvas_generator::{speak_style, TileMode},
    connection_quality_indicator::ConnectionQualityIndicator,
    diagnostics::Diagnostics,
    host::Host,
    host_controls::HostControls,
    meeting_ended_overlay::MeetingEndedOverlay,
    peer_list::PeerList,
    peer_tile::PeerTile,
    pre_join_settings_card::PreJoinSettingsCard,
    update_display_name_modal::UpdateDisplayNameModal,
    video_control_buttons::{
        CameraButton, DensityModeButton, DeviceSettingsButton, DiagnosticsButton, HangUpButton,
        MicButton, MockPeersButton, PeerListButton, ScreenShareButton,
    },
};
use crate::console_log_collector::{flush_console_logs, set_console_log_context};
use crate::constants::actix_websocket_base;
use crate::constants::{
    mock_peers_enabled, server_election_period_ms, users_allowed_to_stream, webtransport_host_base,
    CANVAS_LIMIT,
};
use crate::context::{
    load_appearance_settings_from_storage, resolve_transport_config,
    save_appearance_settings_to_storage, save_display_name_to_storage, validate_display_name,
    AppearanceSettingsCtx, DisplayNameCtx, LocalAudioLevelCtx, MeetingTime, PeerMediaState,
    PeerSignalHistoryMap, PeerStatusMap, TransportPreference, TransportPreferenceCtx,
};
use dioxus::prelude::Element as DioxusElement;
use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use gloo_timers::callback::Timeout;
use gloo_utils::window;
use log::error;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use videocall_client::utils::is_ios;
use videocall_client::Callback as VcCallback;
use videocall_client::{
    ConnectionLostReason, MediaAccessKind, MediaDeviceAccess, MediaPermission,
    MediaPermissionsErrorState, PermissionState, ScreenShareEvent, VideoCallClient,
    VideoCallClientOptions,
};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

#[derive(Clone, Debug, PartialEq)]
pub enum ScreenShareState {
    Idle,
    /// The browser's screen picker is open (getDisplayMedia Promise in flight).
    /// The button is disabled but we have NOT yet told Host to start encoding.
    Requesting,
    /// A MediaStream has been pre-acquired and stored in the shared
    /// [`PreAcquiredScreenStream`] cell.  Host should consume the stream and
    /// begin encoding via `ScreenEncoder::start_with_stream()`.
    StreamReady,
    Active,
}

/// Shared cell that holds a pre-acquired `MediaStream` from `getDisplayMedia()`.
///
/// Safari requires `getDisplayMedia()` to be called synchronously within a
/// user-gesture handler.  The onclick handler obtains the stream and stores it
/// here; the `Host` component takes it out and passes it to
/// `ScreenEncoder::start_with_stream()`.
pub type PreAcquiredScreenStream = Rc<RefCell<Option<web_sys::MediaStream>>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DockPosition {
    Bottom,
    Left,
    Right,
}

impl DockPosition {
    fn css_class(self) -> &'static str {
        match self {
            DockPosition::Bottom => "dock-bottom",
            DockPosition::Left => "dock-left",
            DockPosition::Right => "dock-right",
        }
    }

    #[allow(dead_code)]
    fn next(self) -> Self {
        match self {
            DockPosition::Bottom => DockPosition::Left,
            DockPosition::Left => DockPosition::Right,
            DockPosition::Right => DockPosition::Bottom,
        }
    }
}

pub enum MediaErrorState {
    NoDevice,
    PermissionDenied,
    Other,
}

const SUBTLE_HELP_TEXT_STYLE: &str = "font-size: 0.9rem; opacity: 0.8;";
const SUBTLE_FOOTNOTE_TEXT_STYLE: &str = "font-size: 0.8rem; opacity: 0.7;";

fn render_single_device_error(device: &str, err: &MediaErrorState) -> Element {
    match err {
        MediaErrorState::NoDevice => rsx! {
            p { " {device} not found on this device." }
        },
        MediaErrorState::Other => rsx! {
            p { " {device} has an unexpected problem." }
        },
        MediaErrorState::PermissionDenied => rsx! {
            p { " {device} is blocked in your browser." }
            p { style: "{SUBTLE_HELP_TEXT_STYLE}",
                "Please click the lock icon in your browser's address bar and allow access if you want to use it."
            }
        },
    }
}

impl ScreenShareState {
    /// Returns `true` when a stream is ready or actively encoding -- i.e. when
    /// the `Host` component should have `share_screen = true`.
    ///
    /// `Requesting` (picker dialog open) intentionally returns `false` so that
    /// `Host` does not start the encoder before a stream is available.
    pub fn is_sharing(&self) -> bool {
        matches!(
            self,
            ScreenShareState::StreamReady | ScreenShareState::Active
        )
    }
}

/// Build the WebSocket and WebTransport lobby URLs for the media server.
#[allow(unused_variables)]
fn build_lobby_urls(token: &str, user_id: &str, id: &str) -> (Vec<String>, Vec<String>) {
    #[cfg(feature = "media-server-jwt-auth")]
    let lobby_url = |base: &str| format!("{base}/lobby?token={token}");

    #[cfg(not(feature = "media-server-jwt-auth"))]
    let lobby_url = |base: &str| format!("{base}/lobby/{user_id}/{id}");

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

fn play_user_joined() {
    // Ascending two-tone chime: C5 -> E5 (pleasant, welcoming)
    play_tone_pair(523.25, 659.25, 0.12, 0.35);
}

fn play_user_left() {
    // Descending two-tone: E5 -> A4 (subtle, muted)
    play_tone_pair(659.25, 440.0, 0.12, 0.25);
}

/// Play two short sine-wave tones in sequence using the Web Audio API.
/// `freq1` / `freq2` are the frequencies in Hz, `duration` is how long each
/// tone lasts (seconds), and `volume` is the peak gain (0.0 – 1.0).
fn play_tone_pair(freq1: f64, freq2: f64, duration: f64, volume: f64) {
    let Some(_window) = web_sys::window() else {
        return;
    };
    let ctx = match web_sys::AudioContext::new() {
        Ok(ctx) => ctx,
        Err(_) => return,
    };
    let now = ctx.current_time();

    // First tone
    if let (Ok(osc), Ok(gain)) = (ctx.create_oscillator(), ctx.create_gain()) {
        let _ = osc.connect_with_audio_node(&gain);
        let _ = gain.connect_with_audio_node(&ctx.destination());
        osc.set_type(web_sys::OscillatorType::Triangle);
        let _ = osc.frequency().set_value_at_time(freq1 as f32, now);
        let _ = gain.gain().set_value_at_time(volume as f32, now);
        let _ = gain
            .gain()
            .exponential_ramp_to_value_at_time(0.01, now + duration);
        let _ = osc.start();
        let _ = osc.stop_with_when(now + duration);
    }

    // Second tone (starts after first ends)
    if let (Ok(osc), Ok(gain)) = (ctx.create_oscillator(), ctx.create_gain()) {
        let _ = osc.connect_with_audio_node(&gain);
        let _ = gain.connect_with_audio_node(&ctx.destination());
        osc.set_type(web_sys::OscillatorType::Triangle);
        let _ = osc
            .frequency()
            .set_value_at_time(freq2 as f32, now + duration);
        let _ = gain.gain().set_value_at_time(volume as f32, now + duration);
        let _ = gain
            .gain()
            .exponential_ramp_to_value_at_time(0.01, now + duration * 2.0);
        let _ = osc.start_with_when(now + duration);
        let _ = osc.stop_with_when(now + duration * 2.0);
    }

    // Close the AudioContext after playback completes to avoid resource leaks.
    let duration_ms = (duration * 2.0 * 1000.0) as u32 + 100;
    Timeout::new(duration_ms, move || {
        let _ = ctx.close();
    })
    .forget();
}

/// Maximum number of reconnection attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// Compute the reconnect delay for the given attempt using exponential backoff
/// with ±25% random jitter.  Returns `None` when `attempt >= MAX_RECONNECT_ATTEMPTS`.
fn reconnect_delay_ms(attempt: u32) -> Option<u32> {
    const MAX_DELAY_MS: u32 = 16_000;
    if attempt >= MAX_RECONNECT_ATTEMPTS {
        return None;
    }
    let base = (1000u32.saturating_mul(2u32.saturating_pow(attempt))).min(MAX_DELAY_MS);
    let jitter = (js_sys::Math::random() * 0.5 - 0.25) * base as f64;
    Some((base as f64 + jitter).max(500.0) as u32)
}

/// Schedule a reconnection attempt with exponential backoff and jitter.
///
/// Refreshes the room token, rebuilds lobby URLs, updates the client, and
/// reconnects.  On failure it retries with increasing delay (1s → 2s → 4s →
/// 8s → 16s cap) plus ±25% random jitter.  Gives up after 10 attempts.
#[cfg(feature = "media-server-jwt-auth")]
fn schedule_reconnect(
    client_cell: Rc<RefCell<Option<VideoCallClient>>>,
    meeting_id: String,
    current_display_name: Signal<String>,
    mut connection_error: Signal<Option<String>>,
    mut meeting_ended_message: Signal<Option<String>>,
    transport_pref_signal: Signal<TransportPreference>,
    attempt: u32,
) {
    let delay_ms = match reconnect_delay_ms(attempt) {
        Some(d) => d,
        None => {
            connection_error.set(Some(
                "Unable to reconnect after multiple attempts. Please refresh the page.".into(),
            ));
            return;
        }
    };

    log::info!(
        "Scheduling reconnect attempt {}/{} in {}ms",
        attempt + 1,
        MAX_RECONNECT_ATTEMPTS,
        delay_ms
    );

    Timeout::new(delay_ms, move || {
        wasm_bindgen_futures::spawn_local(async move {
            match crate::meeting_api::refresh_room_token(&meeting_id).await {
                Ok(new_token) => {
                    log::info!("Room token refreshed, reconnecting with new token");
                    let latest_display_name = current_display_name();
                    let (ws, wt) = build_lobby_urls(&new_token, &latest_display_name, &meeting_id);

                    // Apply the user's transport preference so the reconnection
                    // honours the same protocol selection as the initial connection.
                    let pref = transport_pref_signal();
                    let server_wt_enabled =
                        crate::constants::webtransport_enabled().unwrap_or(false);
                    let (_enable_wt, ws, wt) =
                        resolve_transport_config(pref, server_wt_enabled, ws, wt);

                    if let Some(client) = client_cell.borrow_mut().as_mut() {
                        client.update_server_urls(ws, wt);
                        if let Err(e) = client.connect() {
                            log::error!("Reconnection with refreshed token failed: {e:?}");
                        }
                    }
                    connection_error.set(None);
                }
                Err(crate::meeting_api::JoinError::MeetingNotActive) => {
                    meeting_ended_message.set(Some("The meeting has ended.".to_string()));
                }
                Err(e) => {
                    connection_error.set(Some(format!("Connection lost, retrying... ({e})")));
                    schedule_reconnect(
                        client_cell,
                        meeting_id,
                        current_display_name,
                        connection_error,
                        meeting_ended_message,
                        transport_pref_signal,
                        attempt + 1,
                    );
                }
            }
        });
    })
    .forget();
}

/// Schedule a reconnection attempt with exponential backoff (non-JWT path).
///
/// Retries with increasing delay (1s → 2s → 4s → 8s → 16s cap) plus ±25%
/// random jitter.  Gives up after 10 attempts.
#[cfg(not(feature = "media-server-jwt-auth"))]
fn schedule_reconnect_no_jwt(
    client_cell: Rc<RefCell<Option<VideoCallClient>>>,
    mut connection_error: Signal<Option<String>>,
    attempt: u32,
) {
    let delay_ms = match reconnect_delay_ms(attempt) {
        Some(d) => d,
        None => {
            connection_error.set(Some(
                "Unable to reconnect after multiple attempts. Please refresh the page.".into(),
            ));
            return;
        }
    };

    log::info!(
        "Scheduling reconnect attempt {}/{} in {}ms",
        attempt + 1,
        MAX_RECONNECT_ATTEMPTS,
        delay_ms
    );

    Timeout::new(delay_ms, move || {
        let reconnect_needed = {
            if let Some(client) = client_cell.borrow_mut().as_mut() {
                if let Err(e) = client.connect() {
                    log::error!("Reconnection failed: {e:?}");
                    true
                } else {
                    connection_error.set(None);
                    false
                }
            } else {
                true
            }
        };

        if reconnect_needed {
            schedule_reconnect_no_jwt(client_cell, connection_error, attempt + 1);
        }
    })
    .forget();
}

/// Google Meet–style layout: try every column count, compute the maximum
/// 16:9 tile size for each, and pick the variant with the largest tile area.
/// Returns `(cols, rows, tile_width)`.
fn compute_layout(n: usize, w: f64, h: f64, gap: f64) -> (usize, usize, f64) {
    if n == 0 {
        return (1, 1, w);
    }
    let mut best_cols = 1_usize;
    let mut best_rows = 1_usize;
    let mut best_area = 0.0_f64;
    let mut best_tw = 0.0_f64;
    let ar: f64 = 16.0 / 9.0;

    for cols in 1..=n {
        let rows = n.div_ceil(cols);

        let avail_w = (w - (cols as f64 - 1.0) * gap).max(0.0);
        let avail_h = (h - (rows as f64 - 1.0) * gap).max(0.0);

        let mut tw = avail_w / cols as f64;
        let mut th = tw / ar;

        if th * rows as f64 > avail_h {
            th = avail_h / rows as f64;
            tw = th * ar;
        }

        let area = tw * th;
        if area > best_area {
            best_area = area;
            best_cols = cols;
            best_rows = rows;
            best_tw = tw;
        }
    }

    (best_cols, best_rows, best_tw)
}

/// Promote overflow speakers into the visible portion of a tile list.
///
/// When there are more tiles than fit on screen, tiles beyond `visible_count`
/// are "overflow". If an overflow peer spoke within `active_ms` of `now_ms`,
/// swap them with the least-recently-active visible peer that is NOT speaking.
/// The loudest overflow speaker (most recent) gets priority.
fn promote_speakers(
    tiles: &mut [String],
    visible_count: usize,
    speech_map: &HashMap<String, f64>,
    join_map: &HashMap<String, f64>,
    now_ms: f64,
    active_ms: f64,
) {
    if visible_count >= tiles.len() {
        return;
    }

    // Effective timestamp: last speech time if exists, else join time.
    let eff_ts = |peer: &str| -> f64 {
        speech_map
            .get(peer)
            .copied()
            .unwrap_or_else(|| join_map.get(peer).copied().unwrap_or(0.0))
    };

    // Overflow tiles that are actively speaking (most recent first).
    let mut overflow_speakers: Vec<(usize, f64)> = Vec::new();
    for (i, peer) in tiles.iter().enumerate().skip(visible_count) {
        if let Some(&ts) = speech_map.get(peer) {
            if now_ms - ts < active_ms {
                overflow_speakers.push((i, ts));
            }
        }
    }
    overflow_speakers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Visible non-speaking tiles as swap candidates (least recently active first).
    let mut swap_candidates: Vec<(usize, f64)> = (0..visible_count)
        .filter(|&i| {
            speech_map
                .get(&tiles[i])
                .is_none_or(|&ts| now_ms - ts >= active_ms)
        })
        .map(|i| (i, eff_ts(&tiles[i])))
        .collect();
    swap_candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    // Swap pairs — all indices are disjoint so order doesn't matter.
    let num_swaps = overflow_speakers.len().min(swap_candidates.len());
    for i in 0..num_swaps {
        tiles.swap(swap_candidates[i].0, overflow_speakers[i].0);
    }
}

use super::density::{DensityMode, DENSITY_MODES};

#[component]
pub fn AttendantsComponent(
    #[props(default)] id: String,
    #[props(default)] display_name: String,
    e2ee_enabled: bool,
    #[props(default)] user_name: Option<String>,
    #[props(default)] user_id: Option<String>,
    #[props(default)] host_display_name: Option<String>,
    #[props(default)] host_user_id: Option<String>,
    #[props(default)] auto_join: bool,
    #[props(default)] is_owner: bool,
    #[props(default)] is_guest: bool,
    #[props(default)] room_token: String,
    #[props(default)] status_observer_token: String,
    #[props(default = true)] waiting_room_enabled: bool,
    #[props(default)] admitted_can_admit: bool,
    #[props(default = true)] end_on_host_leave: bool,
    #[props(default = false)] allow_guests: bool,
) -> DioxusElement {
    // Clone props that will be used in multiple closures
    let id_for_peer_list = id.clone();
    let meeting_id_for_settings_refresh = id.clone();
    let status_observer_token_for_settings_refresh = status_observer_token.clone();

    // --- State signals ---
    let mut screen_share_state = use_signal(|| ScreenShareState::Idle);

    let mut mic_enabled = use_signal(|| false);
    let mut video_enabled = use_signal(|| false);
    let mut peer_list_open = use_signal(|| false);
    let mut diagnostics_open = use_signal(|| false);
    let mut mock_peers_open = use_signal(|| false);
    let mut controls_visible = use_signal(|| true);
    let mut controls_expanded = use_signal(|| true);
    let mut dock_position: Signal<DockPosition> = use_signal(|| DockPosition::Bottom);
    let mut dock_menu_open = use_signal(|| false);
    let encoder_settings = use_signal(|| None::<String>);
    let mut debug_peer_count = use_signal(|| 0u32);
    // Per-peer speech priority: session_id → last-spoke timestamp (ms).
    // Peers that spoke recently sort higher in the grid.
    let mut peer_speech_priority: Signal<HashMap<String, f64>> = use_signal(HashMap::new);
    // Per-peer join time: session_id → first-seen timestamp (ms).
    // Used as fallback ordering when no speech data exists.
    let mut peer_join_time: Signal<HashMap<String, f64>> = use_signal(HashMap::new);
    let mut density_mode: Signal<DensityMode> = use_signal(|| DensityMode::Auto);
    let mut density_open = use_signal(|| false);
    // Viewport size signal — updated on window resize so layout recomputes.
    let mut viewport_version = use_signal(|| 0u32);
    {
        let _ = viewport_version();
    }
    use_hook(move || {
        let win = window();
        let cb = Closure::<dyn FnMut()>::new(move || {
            viewport_version.with_mut(|v| *v = v.wrapping_add(1));
        });
        let _ = win.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        // Keep the closure alive for the component's lifetime.
        // Runs once (use_hook), so no accumulation on re-renders.
        cb.forget();
    });

    // Dock-style auto-hide: narrow after 1s, hide after 4s of inactivity.
    use_hook(move || {
        let win = window();
        // Two timer handles: one for narrowing (1s), one for hiding (4s).
        let narrow_timer: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));
        let hide_timer: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));

        // mousemove listener
        let nt1 = narrow_timer.clone();
        let ht1 = hide_timer.clone();
        let win1 = win.clone();
        let mouse_cb = Closure::<dyn FnMut()>::new(move || {
            controls_visible.set(true);
            controls_expanded.set(true);
            // Clear existing timers
            if let Some(id) = nt1.borrow_mut().take() {
                win1.clear_timeout_with_handle(id);
            }
            if let Some(id) = ht1.borrow_mut().take() {
                win1.clear_timeout_with_handle(id);
            }
            // Narrow after 1s
            let nt_inner = nt1.clone();
            let narrow_cb = Closure::<dyn FnMut()>::once(move || {
                nt_inner.borrow_mut().take();
                controls_expanded.set(false);
            });
            let id = win1
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    narrow_cb.as_ref().unchecked_ref(),
                    1_000,
                )
                .unwrap_or(0);
            nt1.borrow_mut().replace(id);
            narrow_cb.forget();
            // Hide after 4s
            let ht_inner = ht1.clone();
            let hide_cb = Closure::<dyn FnMut()>::once(move || {
                ht_inner.borrow_mut().take();
                controls_visible.set(false);
            });
            let id = win1
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    hide_cb.as_ref().unchecked_ref(),
                    4_000,
                )
                .unwrap_or(0);
            ht1.borrow_mut().replace(id);
            hide_cb.forget();
        });
        let _ =
            win.add_event_listener_with_callback("mousemove", mouse_cb.as_ref().unchecked_ref());
        mouse_cb.forget();

        // touchstart listener
        let nt2 = narrow_timer.clone();
        let ht2 = hide_timer.clone();
        let win2 = win.clone();
        let touch_cb = Closure::<dyn FnMut()>::new(move || {
            controls_visible.set(true);
            controls_expanded.set(true);
            if let Some(id) = nt2.borrow_mut().take() {
                win2.clear_timeout_with_handle(id);
            }
            if let Some(id) = ht2.borrow_mut().take() {
                win2.clear_timeout_with_handle(id);
            }
            let nt_inner = nt2.clone();
            let narrow_cb = Closure::<dyn FnMut()>::once(move || {
                nt_inner.borrow_mut().take();
                controls_expanded.set(false);
            });
            let id = win2
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    narrow_cb.as_ref().unchecked_ref(),
                    1_000,
                )
                .unwrap_or(0);
            nt2.borrow_mut().replace(id);
            narrow_cb.forget();
            let ht_inner = ht2.clone();
            let hide_cb = Closure::<dyn FnMut()>::once(move || {
                ht_inner.borrow_mut().take();
                controls_visible.set(false);
            });
            let id = win2
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    hide_cb.as_ref().unchecked_ref(),
                    4_000,
                )
                .unwrap_or(0);
            ht2.borrow_mut().replace(id);
            hide_cb.forget();
        });
        let _ =
            win.add_event_listener_with_callback("touchstart", touch_cb.as_ref().unchecked_ref());
        touch_cb.forget();

        // Initial timers
        let nt_init = narrow_timer.clone();
        let narrow_init = Closure::<dyn FnMut()>::once(move || {
            nt_init.borrow_mut().take();
            controls_expanded.set(false);
        });
        let id = win
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                narrow_init.as_ref().unchecked_ref(),
                1_000,
            )
            .unwrap_or(0);
        narrow_timer.borrow_mut().replace(id);
        narrow_init.forget();

        let ht_init = hide_timer.clone();
        let hide_init = Closure::<dyn FnMut()>::once(move || {
            ht_init.borrow_mut().take();
            controls_visible.set(false);
        });
        let id = win
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                hide_init.as_ref().unchecked_ref(),
                4_000,
            )
            .unwrap_or(0);
        hide_timer.borrow_mut().replace(id);
        hide_init.forget();
    });

    let mut device_settings_open = use_signal(|| false);
    let mut connection_error = use_signal(|| None::<String>);
    let mut user_error = use_signal(|| None::<String>);
    let mut display_name_modal_open = use_signal(|| false);
    let current_display_name = use_signal(|| display_name.clone());
    let display_name_ctx = use_context::<DisplayNameCtx>();
    let display_name_ctx_signal = display_name_ctx.0;
    let mut meeting_joined = use_signal(|| false);
    let mut show_copy_toast = use_signal(|| false);
    let mut meeting_start_time_server = use_signal(|| None::<f64>);
    let mut call_start_time = use_signal(|| None::<f64>);
    let meeting_ended_message = use_signal(|| None::<String>);
    let mut meeting_info_open = use_signal(|| false);
    let peer_list_version = use_signal(|| 0u32);
    let mut screen_share_version = use_signal(|| 0u32);
    let media_access_granted = use_signal(|| false);
    let mic_error = use_signal(|| None::<MediaErrorState>);
    let video_error = use_signal(|| None::<MediaErrorState>);
    let mut show_device_warning = use_signal(|| false);
    let reload_devices_counter = use_signal(|| 0u32);
    let mut device_was_denied = use_signal(|| false);
    let session_loaded = use_signal(|| false);
    let connecting = use_signal(|| false);
    let local_speaking = use_signal(|| false);
    let local_audio_level = use_signal(|| 0.0f32);
    let mut pinned_peer_id: Signal<Option<String>> = use_signal(|| None);
    // Screen-share to participants panel ratio. Default 0.667 gives a 2:1 split.
    // Clamped to [0.3, 0.85] by the resize handle (screen share 30%–85% of width).
    let mut screen_share_ratio: Signal<f64> = use_signal(|| 0.667);
    // True while the user is actively dragging the resize handle.
    let mut ss_resizing: Signal<bool> = use_signal(|| false);
    let mut pending_mic_enable = use_signal(|| false);
    let mut pending_video_enable = use_signal(|| false);
    let mut waiting_room_toggle = use_signal(move || waiting_room_enabled);
    let mut admitted_can_admit_toggle = use_signal(move || admitted_can_admit);
    let mut end_on_host_leave_toggle = use_signal(move || end_on_host_leave);
    let mut allow_guests_toggle = use_signal(move || allow_guests);
    let saving = use_signal(|| false);
    let toggle_error = use_signal(|| None::<String>);
    let waiting_room_version = use_signal(|| 0u64);
    let mut host_el = use_signal(|| Option::<web_sys::Element>::None);
    let peer_toasts: Signal<Vec<(u64, String, String, bool)>> = use_signal(Vec::new);
    let toast_counter: Signal<u64> = use_signal(|| 0);
    let toast_version: Signal<u32> = use_signal(|| 0);
    let peer_display_name_version = use_signal(|| 0u32);

    // Create the peer status map signal early so it can be captured by the
    // on_peer_removed callback inside use_hook below.
    let mut peer_status_map: PeerStatusMap = use_signal(HashMap::new);

    // Create the shared signal history map early so on_peer_removed can clean
    // up departed peers' histories. Provided as context alongside PeerStatusMap.
    let peer_signal_history_map: PeerSignalHistoryMap = use_signal(HashMap::new);

    // Read transport preference from context BEFORE use_hook (hooks must not
    // be called inside the hook closure).
    let transport_pref_ctx = use_context::<TransportPreferenceCtx>();
    let transport_pref = (transport_pref_ctx.0)();

    // Create VideoCallClient and MediaDeviceAccess once.
    // We use an Rc<RefCell<Option<VideoCallClient>>> so the on_connection_lost
    // callback can access the client for reconnection. The cell is populated
    // right after VideoCallClient::new().
    let client = use_hook(|| {
        #[cfg(feature = "media-server-jwt-auth")]
        let token = {
            let t = room_token.clone();
            assert!(
                !t.is_empty(),
                "media-server-jwt-auth is enabled but room_token is empty"
            );
            t
        };
        #[cfg(not(feature = "media-server-jwt-auth"))]
        let token = String::new();

        let initial_display_name = current_display_name();
        let (websocket_urls, webtransport_urls) =
            build_lobby_urls(&token, &initial_display_name, &id);

        // Apply user's transport preference
        let server_wt_enabled = crate::constants::webtransport_enabled().unwrap_or(false);
        let (effective_wt_enabled, websocket_urls, webtransport_urls) = resolve_transport_config(
            transport_pref,
            server_wt_enabled,
            websocket_urls,
            webtransport_urls,
        );

        log::info!(
            "DIOXUS-UI: Creating VideoCallClient for {} in meeting {}",
            initial_display_name,
            id
        );

        let client_for_reconnect: Rc<RefCell<Option<VideoCallClient>>> =
            Rc::new(RefCell::new(None));

        let user_id_for_display_name_changed = user_id.clone();

        let opts = VideoCallClientOptions {
            user_id: user_id
                .clone()
                .unwrap_or_else(|| initial_display_name.clone()),
            display_name: initial_display_name.clone(),
            is_guest,
            meeting_id: id.clone(),
            websocket_urls,
            webtransport_urls,
            enable_e2ee: e2ee_enabled,
            enable_webtransport: effective_wt_enabled,
            on_connected: {
                let meeting_id_for_log = id.clone();
                // Slugify the fallback display name so it passes SAFE_USER_ID_RE
                // on the server (spaces and other chars would cause a 400).
                let user_id_for_log = user_id.clone().unwrap_or_else(|| {
                    initial_display_name
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '.' || c == '@' || c == '-' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect()
                });
                VcCallback::from(move |_| {
                    log::info!("DIOXUS-UI: Connection established");
                    let mut connection_error = connection_error;
                    let mut call_start_time = call_start_time;
                    let mut session_loaded = session_loaded;
                    connection_error.set(None);
                    call_start_time.set(Some(js_sys::Date::now()));
                    session_loaded.set(true);
                    // Activate console log collection if enabled in config.
                    if crate::constants::console_log_upload_enabled().unwrap_or(false) {
                        // Raise the WASM log level to Debug so uploaded logs
                        // capture detailed diagnostic output. We use Debug
                        // rather than Trace (as ticket #307 mentions) because
                        // Trace is prohibitively noisy in WASM — every
                        // wasm-bindgen call and Dioxus re-render generates
                        // trace spans that would overwhelm the upload buffer.
                        log::set_max_level(log::LevelFilter::Debug);
                        let dn = current_display_name();
                        set_console_log_context(&meeting_id_for_log, &user_id_for_log, &dn);
                    }
                })
            },
            on_connection_lost: {
                let id = id.clone();
                let client_cell = client_for_reconnect.clone();
                VcCallback::from(move |reason: ConnectionLostReason| {
                    log::warn!(
                        "DIOXUS-UI: Connection lost ({}): {}",
                        reason.label(),
                        reason.message()
                    );
                    let mut connection_error = connection_error;
                    let meeting_ended_message = meeting_ended_message;
                    connection_error.set(Some("Connection lost, reconnecting...".to_string()));

                    #[cfg(feature = "media-server-jwt-auth")]
                    {
                        let client_cell = client_cell.clone();
                        let meeting_id = id.clone();
                        let current_display_name = current_display_name;
                        schedule_reconnect(
                            client_cell,
                            meeting_id,
                            current_display_name,
                            connection_error,
                            meeting_ended_message,
                            transport_pref_ctx.0,
                            0,
                        );
                    }

                    #[cfg(not(feature = "media-server-jwt-auth"))]
                    {
                        let client_cell = client_cell.clone();
                        schedule_reconnect_no_jwt(client_cell, connection_error, 0);
                    }
                })
            },
            on_peer_added: VcCallback::from(move |session_id: String| {
                log::info!("New user joined: {session_id}");
                // Record join time for the new peer immediately (in callback,
                // not during render) to avoid signal-writes-during-render.
                let mut jt = peer_join_time;
                jt.write()
                    .entry(session_id)
                    .or_insert_with(js_sys::Date::now);
                // Sound is played by on_peer_joined which has display name context.
                let mut v = peer_list_version;
                v.set(v() + 1);
            }),
            on_peer_first_frame: VcCallback::noop(),
            on_peer_removed: Some(VcCallback::from(move |peer_id: String| {
                log::info!("Peer removed: {peer_id}");
                // Write to signals directly. In single-threaded WASM, timer
                // callbacks (where PeerDecodeManager::run_peer_monitor fires
                // this) cannot overlap with async tasks, so there is no
                // re-entrant borrow risk. Using dioxus::spawn() here would
                // panic because the callback runs outside any Dioxus runtime
                // scope (from a setInterval timer).
                //
                // Note: we rebind to a local `mut` copy so the closure stays
                // `Fn` (Signal is Copy; only the local is mutated each call).
                let mut map = peer_status_map;
                map.write().remove(&peer_id);
                // Also remove the departed peer's signal history so the shared
                // map does not grow unboundedly over long meetings.
                let mut hist_map = peer_signal_history_map;
                hist_map.write().remove(&peer_id);
                let mut speech_map = peer_speech_priority;
                speech_map.write().remove(&peer_id);
                let mut jt_map = peer_join_time;
                jt_map.write().remove(&peer_id);
                let mut v = peer_list_version;
                v.set(v() + 1);
            })),
            get_peer_video_canvas_id: VcCallback::from(|id| id),
            get_peer_screen_canvas_id: VcCallback::from(|id| format!("screen-share-{}", &id)),
            enable_diagnostics: true,
            diagnostics_update_interval_ms: Some(1000),
            enable_health_reporting: true,
            health_reporting_interval_ms: Some(5000),
            on_encoder_settings_update: None,
            rtt_testing_period_ms: server_election_period_ms().unwrap_or(2000),
            rtt_probe_interval_ms: Some(200),
            on_meeting_info: Some(VcCallback::from(move |start_time_ms: f64| {
                log::info!("Meeting started at Unix timestamp: {start_time_ms}");
                let mut meeting_start_time_server = meeting_start_time_server;
                meeting_start_time_server.set(Some(start_time_ms));
            })),
            on_meeting_ended: Some(VcCallback::from(
                move |(end_time_ms, message): (f64, String)| {
                    log::info!("Meeting ended at Unix timestamp: {end_time_ms}");
                    let mut meeting_start_time_server = meeting_start_time_server;
                    let mut meeting_ended_message = meeting_ended_message;
                    meeting_start_time_server.set(Some(end_time_ms));
                    meeting_ended_message.set(Some(message));
                },
            )),
            on_speaking_changed: Some(VcCallback::from(move |speaking: bool| {
                let mut s = local_speaking;
                s.set(speaking);
            })),
            on_audio_level_changed: Some(VcCallback::from(move |level: f32| {
                let mut s = local_audio_level;
                s.set(level);
            })),
            vad_threshold: crate::constants::vad_threshold().ok(),
            on_meeting_activated: None,
            on_participant_admitted: None,
            on_participant_rejected: None,
            on_waiting_room_updated: Some(VcCallback::from(move |_| {
                log::info!("Waiting room updated push received");
                let mut v = waiting_room_version;
                v.set(v() + 1);
            })),
            on_meeting_settings_updated: Some(VcCallback::from(move |_| {
                log::info!("Meeting settings updated push received");

                let meeting_id = meeting_id_for_settings_refresh.clone();
                let observer_token = status_observer_token_for_settings_refresh.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match crate::meeting_api::fetch_participant_status(
                        &meeting_id,
                        &observer_token,
                        is_guest,
                    )
                    .await
                    {
                        Ok(status) => {
                            if waiting_room_toggle() != status.waiting_room_enabled {
                                waiting_room_toggle.set(status.waiting_room_enabled);
                            }

                            if admitted_can_admit_toggle() != status.admitted_can_admit {
                                admitted_can_admit_toggle.set(status.admitted_can_admit);
                            }

                            if end_on_host_leave_toggle() != status.end_on_host_leave {
                                end_on_host_leave_toggle.set(status.end_on_host_leave);
                            }

                            if allow_guests_toggle() != status.allow_guests {
                                allow_guests_toggle.set(status.allow_guests);
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to refresh meeting settings after push update: {e}");
                        }
                    }
                });
            })),
            on_peer_left: {
                Some(VcCallback::from(
                    move |(display_name, user_id): (String, String)| {
                        log::debug!("TOAST-RX: peer left: {} ({})", display_name, user_id);

                        let mut toast_counter = toast_counter;
                        let mut peer_toasts = peer_toasts;
                        let mut toast_version = toast_version;
                        let id = *toast_counter.peek();
                        toast_counter.set(id + 1);
                        let mut current = peer_toasts.peek().clone();
                        current.push((id, display_name, user_id, false));
                        peer_toasts.set(current);
                        {
                            let v = *toast_version.peek();
                            toast_version.set(v + 1);
                        }
                        // Defer the leave sound: only play if the toast still exists
                        // after 500ms (i.e. no join event cancelled it).
                        Timeout::new(500, move || {
                            if peer_toasts.peek().iter().any(|(tid, _, _, _)| *tid == id) {
                                play_user_left();
                            }
                        })
                        .forget();
                        // Schedule toast removal after 8 seconds.
                        Timeout::new(8_000, move || {
                            let updated: Vec<_> = peer_toasts
                                .peek()
                                .iter()
                                .filter(|(tid, _, _, _)| *tid != id)
                                .cloned()
                                .collect();
                            peer_toasts.set(updated);
                            {
                                let v = *toast_version.peek();
                                toast_version.set(v + 1);
                            }
                        })
                        .forget();
                    },
                ))
            },
            on_peer_joined: {
                let client_cell = client_for_reconnect.clone();
                Some(VcCallback::from(
                    move |(display_name, user_id): (String, String)| {
                        log::debug!("TOAST-RX: peer joined: {} ({})", display_name, user_id);

                        let suppress_toast = if let Some(ref client) = *client_cell.borrow() {
                            if client.is_reconnecting() {
                                log::debug!(
                                    "Suppressing join toast for {} (reconnecting)",
                                    user_id
                                );
                                true
                            } else if client.has_peer_with_user_id(&user_id) {
                                log::debug!(
                                    "Suppressing join toast for {} (already in peer list)",
                                    user_id
                                );
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        let mut toast_counter = toast_counter;
                        let mut peer_toasts = peer_toasts;
                        let mut toast_version = toast_version;
                        let mut current = peer_toasts.peek().clone();
                        current.retain(|(_, _, uid, is_joined)| *is_joined || uid != &user_id);

                        if !suppress_toast {
                            play_user_joined();
                            let id = *toast_counter.peek();
                            toast_counter.set(id + 1);
                            current.push((id, display_name, user_id, true));
                            peer_toasts.set(current);
                            {
                                let v = *toast_version.peek();
                                toast_version.set(v + 1);
                            }
                            Timeout::new(8_000, move || {
                                let updated: Vec<_> = peer_toasts
                                    .peek()
                                    .iter()
                                    .filter(|(tid, _, _, _)| *tid != id)
                                    .cloned()
                                    .collect();
                                peer_toasts.set(updated);
                                {
                                    let v = *toast_version.peek();
                                    toast_version.set(v + 1);
                                }
                            })
                            .forget();
                        } else {
                            peer_toasts.set(current);
                        }

                        {
                            let mut v = peer_list_version;
                            v.set(v() + 1);
                        }
                    },
                ))
            },
            on_display_name_changed: Some(VcCallback::from(
                move |(changed_user_id, new_display_name): (String, String)| {
                    log::info!(
                        "DIOXUS-UI: DISPLAY_NAME_CHANGED received: user={} new_name=\"{}\"",
                        changed_user_id,
                        new_display_name,
                    );

                    if user_id_for_display_name_changed.as_deref() == Some(changed_user_id.as_str())
                    {
                        match validate_display_name(&new_display_name) {
                            Ok(validated_name) => {
                                log::info!(
                                    "DIOXUS-UI: Local user display name confirmed by server: {}",
                                    validated_name
                                );
                                save_display_name_to_storage(&validated_name);
                                let mut current_display_name = current_display_name;
                                current_display_name.set(validated_name.clone());
                                let mut dn_ctx = display_name_ctx_signal;
                                dn_ctx.set(Some(validated_name));
                                log::debug!("DIOXUS-UI: current_display_name signal updated");
                            }
                            Err(e) => {
                                log::warn!(
                                    "DIOXUS-UI: Ignoring invalid display name from server: {:?} ({})",
                                    new_display_name,
                                    e
                                );
                            }
                        }
                    }

                    let mut v = peer_display_name_version;
                    v.set(v() + 1);
                    log::debug!("DIOXUS-UI: peer_display_name_version bumped");
                },
            )),
            // Full call participant: decode and play all inbound media.
            decode_media: true,
        };

        let client = VideoCallClient::new(opts);
        *client_for_reconnect.borrow_mut() = Some(client.clone());
        client
    });

    // Tear the VideoCallClient down synchronously when this component
    // unmounts (Hangup button, browser back-nav, route push, route replace,
    // tab close — every path Dioxus surfaces as a scope drop).
    //
    // The client is `Clone` and shares state through `Rc` handles. Several
    // internal callbacks captured during `VideoCallClient::new` hold strong
    // clones of the client (peer_decode_manager.send_packet,
    // diagnostics.packet_handler, health_reporter's spawn_local future),
    // forming `Rc` cycles that prevent `Inner` from ever dropping on its
    // own. Without this hook an in-tab SPA route swap on the meeting page
    // leaks the entire `VideoCallClient` — transports, encoders, atomics
    // — for tens of seconds, until the server eventually tears the
    // session down. That leak caused the cc7tp meeting incident on
    // 2026-05-01 (UI panics, dropped media packets, ghost participant,
    // spurious MEETING_ENDED broadcast).
    //
    // `disconnect()` is idempotent and safe to call even when the client
    // never connected, and it kicks off async transport teardown via
    // `ConnectionController::disconnect` while returning synchronously,
    // so the next mount cannot race a still-running predecessor.
    {
        let client_for_drop = client.clone();
        use_drop(move || {
            log::info!("DIOXUS-UI: AttendantsComponent unmounted - disconnecting VideoCallClient");
            if let Err(e) = client_for_drop.disconnect() {
                log::warn!("DIOXUS-UI: VideoCallClient disconnect on unmount failed: {e}");
            }
        });
    }

    let mda = use_hook(|| {
        let mut mda = MediaDeviceAccess::new();
        let client_cell = RefCell::new(client.clone());
        mda.on_result = VcCallback::from(move |permit: MediaPermission| {
            let mut connection_error = connection_error;
            let mut media_access_granted = media_access_granted;
            let mut meeting_joined = meeting_joined;
            let mut mic_enabled = mic_enabled;
            let mut video_enabled = video_enabled;
            let mut pending_mic_enable = pending_mic_enable;
            let mut pending_video_enable = pending_video_enable;
            let mut mic_error = mic_error;
            let mut video_error = video_error;
            let mut show_device_warning = show_device_warning;
            let mut reload_devices_counter = reload_devices_counter;
            let mut device_was_denied = device_was_denied;

            connection_error.set(None);
            mic_error.set(None);
            video_error.set(None);
            media_access_granted.set(true);

            // Fulfil any pending mic/camera enables that triggered the permission request.
            if matches!(permit.audio, PermissionState::Granted) && pending_mic_enable() {
                mic_enabled.set(true);
                pending_mic_enable.set(false);
            }
            if matches!(permit.video, PermissionState::Granted) && pending_video_enable() {
                video_enabled.set(true);
                pending_video_enable.set(false);
            }

            match &permit.audio {
                PermissionState::Denied(MediaPermissionsErrorState::NoDevice) => {
                    mic_error.set(Some(MediaErrorState::NoDevice));
                }
                PermissionState::Denied(MediaPermissionsErrorState::PermissionDenied) => {
                    mic_error.set(Some(MediaErrorState::PermissionDenied));
                }
                PermissionState::Denied(MediaPermissionsErrorState::Other(_)) => {
                    mic_error.set(Some(MediaErrorState::Other));
                }
                _ => {}
            }

            match &permit.video {
                PermissionState::Denied(MediaPermissionsErrorState::NoDevice) => {
                    video_error.set(Some(MediaErrorState::NoDevice));
                }
                PermissionState::Denied(MediaPermissionsErrorState::PermissionDenied) => {
                    video_error.set(Some(MediaErrorState::PermissionDenied));
                }
                PermissionState::Denied(MediaPermissionsErrorState::Other(_)) => {
                    video_error.set(Some(MediaErrorState::Other));
                }
                _ => {}
            }

            if session_loaded() || connecting() {
                if mic_error.read().is_some() {
                    mic_enabled.set(false);
                    pending_mic_enable.set(false);
                }
                if video_error.read().is_some() {
                    video_enabled.set(false);
                    pending_video_enable.set(false);
                }
            } else if mic_error.read().is_some() || video_error.read().is_some() {
                show_device_warning.set(true);
                meeting_joined.set(false);
            } else {
                let mut connecting = connecting;
                connecting.set(true);
                if let Err(e) = client_cell.borrow_mut().connect() {
                    log::error!("Connection failed: {e:?}");
                }
                meeting_joined.set(true);
            }

            if device_was_denied() {
                device_was_denied.set(false);
                reload_devices_counter.set(reload_devices_counter() + 1);
            }
        });
        Rc::new(RefCell::new(mda))
    });

    // Re-check permissions when the window regains focus, mirroring Yew behavior.
    {
        let mda = mda.clone();
        use_effect(move || {
            let value = mda.clone();
            let closure = Closure::wrap(Box::new(move |_event: web_sys::Event| {
                if session_loaded() || connecting() {
                    return;
                }

                let mic_denied = matches!(
                    mic_error.read().as_ref(),
                    Some(MediaErrorState::PermissionDenied)
                );
                let video_denied = matches!(
                    video_error.read().as_ref(),
                    Some(MediaErrorState::PermissionDenied)
                );

                if mic_denied || video_denied {
                    device_was_denied.set(true);
                }
                value.borrow().request();
            }) as Box<dyn FnMut(_)>);

            if let Some(win) = web_sys::window() {
                let _ =
                    win.add_event_listener_with_callback("focus", closure.as_ref().unchecked_ref());
            }
            closure.forget();
        });
    }

    // Provide contexts for child components
    use_context_provider(|| client.clone());
    let mut meeting_time_signal = use_signal(MeetingTime::default);
    use_context_provider(|| meeting_time_signal);
    let local_audio_level_ctx = use_context_provider(|| LocalAudioLevelCtx(local_audio_level));
    let _ = local_audio_level_ctx.0;
    let appearance_settings = use_signal(load_appearance_settings_from_storage);
    use_context_provider(|| AppearanceSettingsCtx(appearance_settings));
    let appearance_save_timeout: Rc<RefCell<Option<Timeout>>> =
        use_hook(|| Rc::new(RefCell::new(None)));

    // Persist local-only appearance preferences for this viewer.
    let appearance_save_timeout_effect = appearance_save_timeout.clone();
    use_effect(move || {
        let settings = appearance_settings();
        if let Some(timeout) = appearance_save_timeout_effect.borrow_mut().take() {
            timeout.cancel();
        }

        let timeout_cell = appearance_save_timeout_effect.clone();
        let timeout = Timeout::new(300, move || {
            save_appearance_settings_to_storage(&settings);
            timeout_cell.borrow_mut().take();
        });
        *appearance_save_timeout_effect.borrow_mut() = Some(timeout);
    });
    // Cancel any pending appearance-save timeout when the component unmounts
    // to avoid a storage write racing with navigation away from the meeting.
    use_drop(move || {
        if let Some(timeout) = appearance_save_timeout.borrow_mut().take() {
            timeout.cancel();
        }
    });

    // Shared cell for passing a pre-acquired screen share MediaStream from
    // the click handler to the Host component.  Safari requires getDisplayMedia()
    // to be called synchronously inside a user gesture, so we obtain the stream
    // in the button onclick and hand it off to the encoder via this cell.
    let pre_acquired_screen_stream: PreAcquiredScreenStream =
        use_hook(|| Rc::new(RefCell::new(None)));
    use_context_provider(|| pre_acquired_screen_stream.clone());

    // Provide the peer status map as context for child PeerTile components.
    // The signal was created earlier so on_peer_removed can capture it.
    use_context_provider(|| peer_status_map);

    // Provide the shared signal history map so PeerTile components can look up
    // (or create) their history entry. This survives PeerTile remounts caused
    // by layout switches (grid -> split when screen sharing starts).
    use_context_provider(|| peer_signal_history_map);

    // Single diagnostics subscriber shared by all PeerTile components.
    // Instead of each PeerTile spawning its own async task, one task
    // dispatches peer_status events into a shared HashMap.
    let mut diagnostics_task: Signal<Option<dioxus_core::Task>> = use_signal(|| None);
    use_effect(move || {
        let task = spawn(async move {
            let mut rx = videocall_diagnostics::subscribe();
            while let Ok(evt) = rx.recv().await {
                if evt.subsystem == "peer_speaking" {
                    // Track speech activity for priority sorting.
                    // Only update the map (and trigger a re-sort) when the
                    // speaker is new or their last timestamp is >5 s stale.
                    // This prevents grid thrashing when multiple people talk
                    // at the same time — tiles stay stable instead of
                    // constantly swapping positions.
                    if let Some(peer_id) = parse_speaking_peer(&evt) {
                        let now = js_sys::Date::now();
                        let should_update = {
                            let map = peer_speech_priority.read();
                            match map.get(&peer_id) {
                                None => true,
                                Some(&prev) => now - prev > 5_000.0,
                            }
                        };
                        if should_update {
                            peer_speech_priority.write().insert(peer_id, now);
                            let mut v = peer_list_version;
                            let next = *v.peek() + 1;
                            v.set(next);
                        }
                    }
                    continue;
                }
                if evt.subsystem != "peer_status" {
                    continue;
                }
                if let Some((peer_id, state)) = parse_peer_status_event(&evt) {
                    // Check if this peer already has a signal.
                    let existing = peer_status_map.read().get(&peer_id).copied();
                    if let Some(mut sig) = existing {
                        // Update the per-peer signal only if the state changed.
                        if *sig.peek() != state {
                            // If screen-sharing state changed, bump the layout version.
                            if sig.peek().screen_enabled != state.screen_enabled {
                                let next = *screen_share_version.peek() + 1;
                                screen_share_version.set(next);
                            }
                            sig.set(state);
                        }
                    } else {
                        // First event for this peer — create a new signal.
                        let screen_enabled = state.screen_enabled;
                        let sig = Signal::new(state);
                        if screen_enabled {
                            let next = *screen_share_version.peek() + 1;
                            screen_share_version.set(next);
                        }
                        peer_status_map.write().insert(peer_id, sig);
                    }
                }
            }
        });
        diagnostics_task.write().replace(task);
    });
    use_drop(move || {
        if let Some(task) = diagnostics_task.peek().as_ref() {
            task.cancel();
        }
    });

    // Host self-view speaking glow — update DOM directly to avoid re-rendering
    // the entire meeting view on every audio-level tick.
    // Note: host glow is intentionally not suppressed by pin state so the local
    // user always has visible speaking feedback on their own self-view.
    use_effect(move || {
        let audio_level = local_audio_level();
        let speaking = local_speaking();
        let appearance = appearance_settings();
        let style = speak_style(audio_level, speaking, &appearance);
        if let Some(el) = host_el() {
            let cl = el.class_list();
            if speaking {
                let _ = cl.add_1("speaking-tile");
            } else {
                let _ = cl.remove_1("speaking-tile");
            }
            let _ = el.set_attribute("style", &style);
        }
    });

    // Check for config errors
    use_effect(move || {
        if let Err(e) = crate::constants::app_config() {
            log::error!("{e:?}");
            connection_error.set(Some(e));
        }
    });

    // Auto-join on first render if requested
    {
        let mda = mda.clone();
        use_effect(move || {
            if auto_join {
                mda.borrow().request();
            }
        });
    }

    // --- Derived values ---
    let _ = peer_list_version(); // subscribe to trigger re-renders when peers change
    let _ = toast_version(); // subscribe to trigger re-renders when toasts change
    let _ = screen_share_version(); // subscribe to trigger re-renders when screen-share state changes
    let _ = peer_display_name_version();
    let all_peers = client.sorted_peer_keys();
    // Filter out the local user's own session to prevent a phantom peer tile.
    // Compare by session_id (unique per connection), not user_id (shared when
    // the same account joins from multiple browsers/tabs).
    let own_session = client.get_own_session_id().unwrap_or_default();
    let display_peers: Vec<String> = all_peers
        .into_iter()
        .filter(|session_id| *session_id != own_session)
        .collect();
    // Assign synthetic join times to mock peers for local testing.
    // Real peers get their join time recorded in the on_peer_added callback
    // (not during render) to avoid signal-writes-during-render loops.
    {
        let mock_count_val = debug_peer_count() as usize;
        let has_new_mock = {
            let jt = peer_join_time.read();
            (0..mock_count_val).any(|i| !jt.contains_key(&format!("mock-{i}")))
        };
        if has_new_mock {
            let now = js_sys::Date::now();
            let mut jt = peer_join_time.write();
            for i in 0..mock_count_val {
                let mock_id = format!("mock-{i}");
                jt.entry(mock_id).or_insert(now + i as f64);
            }
        }
    }
    let num_display_peers = display_peers.len();
    let mock_count = debug_peer_count() as usize;
    // CANVAS_LIMIT caps real peers (each drives a canvas + diagnostics task).
    // Mock peers are layout-only placeholders and don't carry that cost.
    let capped_real = num_display_peers.min(CANVAS_LIMIT);
    let total_tiles = capped_real + mock_count;

    // --- Viewport dimensions (needed for min-tile-size check & grid style) ---
    let vw = window()
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1024.0);
    let vh = window()
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(768.0);
    // Gap/padding must match #grid-container in style.css.
    // Breakpoint (568px) must match @media (max-width: 568px) in style.css.
    let (gap, pad_top, pad_right, pad_bottom, pad_left) = if vw < 568.0 {
        (8.0, 8.0, 8.0, 72.0, 8.0)
    } else {
        (16.0, 20.0, 20.0, 84.0, 20.0)
    };
    let avail_w = (vw - pad_left - pad_right).max(0.0);
    let avail_h = (vh - pad_top - pad_bottom).max(0.0);

    // --- Count active speakers for auto-density escalation ---
    // A peer is "actively speaking" if they spoke within the last 30 seconds.
    const SPEAKER_ACTIVE_MS: f64 = 30_000.0;
    let now_ms = js_sys::Date::now();
    let active_speaker_count = {
        let speech_map = peer_speech_priority.read();
        display_peers
            .iter()
            .filter(|p| {
                speech_map
                    .get(*p)
                    .is_some_and(|&ts| now_ms - ts < SPEAKER_ACTIVE_MS)
            })
            .count()
    };

    // --- Determine effective density mode ---
    // If the user's chosen mode can't show all active speakers, auto-escalate
    // to a denser mode so every speaker is always visible.
    let user_mode = density_mode();
    let modes_by_density = [
        DensityMode::Standard,
        DensityMode::Auto,
        DensityMode::Dense,
        DensityMode::Maximum,
    ];
    let user_rank = modes_by_density
        .iter()
        .position(|&m| m == user_mode)
        .unwrap_or(1);

    let effective_mode = if active_speaker_count > 0 {
        let mut chosen = user_mode;
        for &mode in &modes_by_density[user_rank..] {
            chosen = mode;
            let mtw = mode.min_tile_width(vw);
            // Find the max tile count where compute_layout yields tw >= mtw.
            // Since tile_width is monotonically non-increasing as tile count
            // grows, we can scan downward from total_tiles (fast in practice
            // since the breakpoint is usually close to total_tiles).
            let capacity = {
                let mut t = total_tiles;
                while t > 1 {
                    let (_c, _r, tw) = compute_layout(t, avail_w, avail_h, gap);
                    if tw >= mtw {
                        break;
                    }
                    t -= 1;
                }
                t
            };
            let vis = if total_tiles > capacity {
                capacity.saturating_sub(1).max(1)
            } else {
                total_tiles
            };
            let vis_real = num_display_peers.min(vis);
            if vis_real >= active_speaker_count {
                break;
            }
        }
        chosen
    } else {
        user_mode
    };

    // --- Determine visible tile count ---
    let min_tw = effective_mode.min_tile_width(vw);
    let effective_visible = {
        let mut t = total_tiles;
        while t > 1 {
            let (_c, _r, tw) = compute_layout(t, avail_w, avail_h, gap);
            if tw >= min_tw {
                break;
            }
            t -= 1;
        }
        t
    };

    // Show density selector only when modes would produce different results.
    // If even Standard (most restrictive) can show all tiles, hide it.
    let show_density_selector = {
        let std_min = DensityMode::Standard.min_tile_width(vw);
        let (_c, _r, tw) = compute_layout(total_tiles, avail_w, avail_h, gap);
        tw < std_min // Standard can't fit everyone → modes matter
    };

    let (visible_tile_count, overflow_count) = if total_tiles > effective_visible {
        let visible = effective_visible.saturating_sub(1).max(1);
        (visible, total_tiles - visible)
    } else {
        (total_tiles, 0)
    };
    // --- Build unified tile list (real + mock peers) sorted by join time ---
    // Tiles are ordered by join time (earliest first) rather than by speech
    // activity. This provides a stable, predictable grid that doesn't shuffle
    // chaotically as people take turns speaking. Active speakers who overflow
    // off-screen are promoted via the swap logic below, so they remain visible
    // without disrupting the order of the rest of the grid.
    //
    // Real peers and mock peers are interleaved by the order they appeared, so
    // a user who joins after mock tiles are added will appear AFTER the mocks
    // — not always at the front.

    // Pre-build mock IDs once to avoid repeated format!() in the hot path.
    let mock_ids: Vec<String> = (0..mock_count).map(|i| format!("mock-{i}")).collect();

    let mut all_tiles: Vec<String> = Vec::with_capacity(total_tiles);
    for peer_id in display_peers.iter().take(capped_real) {
        all_tiles.push(peer_id.clone());
    }
    all_tiles.extend_from_slice(&mock_ids);
    // Stable sort by join time (earliest first).
    {
        let join_map = peer_join_time.read();
        all_tiles.sort_by(|a, b| {
            let jt_a = join_map.get(a).copied().unwrap_or(0.0);
            let jt_b = join_map.get(b).copied().unwrap_or(0.0);
            jt_a.partial_cmp(&jt_b).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    // --- Overflow speaker promotion (see promote_speakers() docs) ---
    {
        let speech_map = peer_speech_priority.read();
        let join_map = peer_join_time.read();
        promote_speakers(
            &mut all_tiles,
            visible_tile_count,
            &speech_map,
            &join_map,
            now_ms,
            SPEAKER_ACTIVE_MS,
        );
    }

    // The visible portion of the unified tile list (used by the normal grid layout).
    let visible_tiles: Vec<String> = all_tiles.iter().take(visible_tile_count).cloned().collect();

    // Map session IDs to user IDs for display (peer list sidebar — real peers only).
    let peers_for_display: Vec<String> = display_peers
        .iter()
        .map(|session_id| {
            client
                .get_peer_user_id(session_id)
                .unwrap_or_else(|| session_id.clone())
        })
        .collect();

    // --- Screen share stack: tracks the order of peer screen shares (LIFO) ---
    let mut screen_share_stack: Signal<Vec<String>> = use_signal(Vec::new);
    let previous_active_decode_set: Rc<RefCell<HashSet<u64>>> =
        use_hook(|| Rc::new(RefCell::new(HashSet::new())));
    let active_screen_sharer: Option<String> = {
        let mut stack = screen_share_stack.write();
        // Remove peers who stopped sharing or left
        stack.retain(|pid| {
            display_peers.contains(pid) && client.is_screen_share_enabled_for_peer(pid)
        });
        // Add new sharers to the end (most recent = last)
        for pid in &display_peers {
            if client.is_screen_share_enabled_for_peer(pid) && !stack.contains(pid) {
                // Skip self — local screen share is shown in the host preview
                let peer_user_id = client.get_peer_user_id(pid).unwrap_or_else(|| pid.clone());
                if user_id.as_deref() != Some(peer_user_id.as_str()) {
                    stack.push(pid.clone());
                }
            }
        }
        stack.last().cloned()
    };
    let has_screen_share = active_screen_sharer.is_some();

    // --- Screen-share right panel: separate capacity & speaker promotion ---
    // The right panel uses a 2-column grid of compact tiles. We compute how
    // many fit based on the available height, then run speaker promotion
    // independently of the normal grid's visible_tile_count.
    //
    // Layout constants must stay in sync with container_style below:
    //   - SS_FLEX_RATIO: right panel gets 1/(2+1) of the container width
    //   - SS_OUTER_PAD: padding: 16px 16px 80px 16px → left + right = 32px
    //   - SS_GAP: gap between left/right panels = 10px (container gap)
    //   - SS_GRID_GAP: gap between tiles in the 2-col grid = 8px
    //   - SS_GRID_PAD: padding inside the right panel div = 6px each side
    //   - SS_BOTTOM_PAD: padding-bottom (80px) from container_style
    //   - SS_TOP_PAD: padding-top (16px) + right panel padding (6px*2)
    //
    // Tile sizing: height is fixed to fit 4 tiles per column regardless of panel width.
    // Column count collapses to 1 when right_ratio <= 0.25 or panel is too narrow.
    // Actual tile width is controlled by the CSS grid (1fr columns), not computed here.
    const SS_GRID_GAP: f64 = 8.0;
    const SS_BOTTOM_PAD: f64 = 80.0;
    const SS_VERT_PAD: f64 = 28.0; // top padding (16) + panel padding (6*2)
    let right_ratio = 1.0 - screen_share_ratio();
    let ss_panel_width = (right_ratio * (vw - 42.0) - 12.0).max(100.0); // ≈ right_ratio * (vw - outer_pad - gap) - grid_pad
    let ss_cols = if right_ratio <= 0.25 || ss_panel_width < 180.0 {
        1.0_f64 // single column
    } else {
        2.0_f64 // two columns
    };
    let ss_avail_h = vh - SS_BOTTOM_PAD - SS_VERT_PAD;
    // Tile height: always sized to fit exactly 4 tiles per column (independent of panel width resize).
    let ss_tile_h = ((ss_avail_h - 3.0 * SS_GRID_GAP) / 4.0).max(40.0);
    // Natural tile width at 16:9: ss_tile_h * 16.0 / 9.0 (actual width follows grid columns).
    // Max rows is always 4 (height is sized for exactly 4 tiles per column).
    let ss_max_rows = 4_usize;
    let ss_max_tiles = ss_max_rows * ss_cols as usize;

    // Build a separate tile list for the screen-share right panel.
    // The grid's promotion used visible_tile_count which differs from the
    // screen-share panel's capacity, so we rebuild from scratch and re-promote.
    let (ss_tiles, ss_overflow_count) = if has_screen_share {
        let mut ss_all: Vec<String> = Vec::with_capacity(total_tiles);
        for peer_id in display_peers.iter().take(capped_real) {
            ss_all.push(peer_id.clone());
        }
        ss_all.extend_from_slice(&mock_ids);
        {
            let join_map = peer_join_time.read();
            ss_all.sort_by(|a, b| {
                let jt_a = join_map.get(a).copied().unwrap_or(0.0);
                let jt_b = join_map.get(b).copied().unwrap_or(0.0);
                jt_a.partial_cmp(&jt_b).unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        let (ss_vis_count, ss_ovf) = if ss_all.len() > ss_max_tiles {
            let vis = ss_max_tiles.saturating_sub(1).max(1); // reserve 1 slot for badge
            (vis, ss_all.len() - vis)
        } else {
            (ss_all.len(), 0)
        };

        {
            let speech_map = peer_speech_priority.read();
            let join_map = peer_join_time.read();
            promote_speakers(
                &mut ss_all,
                ss_vis_count,
                &speech_map,
                &join_map,
                now_ms,
                SPEAKER_ACTIVE_MS,
            );
        }

        let tiles: Vec<String> = ss_all.into_iter().take(ss_vis_count).collect();
        (tiles, ss_ovf)
    } else {
        (Vec::new(), 0)
    };

    // ORDERING INVARIANT: the active decode set is built in 3 phases:
    //   1. Visible layout peers (here)
    //   2. Active screen sharer (here)
    //   3. Pinned peer (below, after tile rendering)
    // The dedup check against previous_active_decode_set must run AFTER all
    // three phases. Moving any insertion after the dedup will silently desync.
    let mut active_decode_set: HashSet<u64> = if has_screen_share {
        // In screen share mode, decode only the tiles visible in the right panel.
        ss_tiles
            .iter()
            .filter_map(|pid| pid.parse::<u64>().ok())
            .collect()
    } else {
        // Use visible_tiles (post-promotion) so promoted speakers are decoded.
        // .parse::<u64>() naturally filters out mock-N IDs.
        visible_tiles
            .iter()
            .filter_map(|id| id.parse::<u64>().ok())
            .collect()
    };
    if let Some(active_peer) = active_screen_sharer.as_ref() {
        if let Ok(session_id) = active_peer.parse::<u64>() {
            active_decode_set.insert(session_id);
        }
    }

    let container_style = if has_screen_share {
        // Screen-share panel on the left, participant panel on the right (ratio draggable 0.3–0.85)
        "position: absolute; inset: 0; width: 100%; height: 100%; \
         display: flex; flex-direction: row; flex-wrap: nowrap; gap: 10px; \
         padding: 16px 16px 80px 16px; \
         align-items: center; box-sizing: border-box; \
         grid-template-columns: none; grid-template-rows: none;"
            .to_string()
    } else {
        // Google Meet–style grid: reuse vw/vh/gap/avail computed above.
        // Explicitly reset all flex properties so the transition from
        // screen-share (flex) back to normal (grid) is clean.
        let tile_count = visible_tile_count + if overflow_count > 0 { 1 } else { 0 };
        let (cols, rows, tw) = compute_layout(tile_count, avail_w, avail_h, gap);
        let th = tw / (16.0 / 9.0);
        format!(
            "display: grid; \
             position: absolute; inset: 0; \
             gap: {gap:.0}px; \
             padding: {pad_top:.0}px {pad_right:.0}px {pad_bottom:.0}px {pad_left:.0}px; \
             box-sizing: border-box; overflow: hidden; \
             flex-direction: unset; flex-wrap: unset; align-items: unset; \
             width: 100%; height: 100%; \
             grid-template-columns: repeat({cols}, 1fr); grid-template-rows: repeat({rows}, 1fr); \
             --tile-w: {tw:.0}px; --tile-h: {th:.0}px;"
        )
    };

    let meeting_link = {
        let origin = window().location().origin().unwrap_or_default();
        format!("{}/meeting/{}", origin, id)
    };

    let is_allowed = users_allowed_to_stream().unwrap_or_default();
    let latest_display_name = current_display_name();
    let effective_user_id = user_id.as_deref().unwrap_or(&latest_display_name);
    let can_stream =
        is_allowed.is_empty() || is_allowed.iter().any(|host| host == effective_user_id);
    // --- Pre-join screen ---
    if !meeting_joined() {
        return rsx! {
            div { id: "main-container", class: "meeting-page",
                BrowserCompatibility {}
                div { id: "join-meeting-container", class: "hero-container",
                    div { class: "floating-element floating-element-1" }
                    div { class: "floating-element floating-element-2" }
                    div { class: "floating-element floating-element-3" }
                    div { class: "hero-content",
                        PreJoinSettingsCard {
                            is_owner,
                            meeting_id: id.clone(),
                            waiting_room_toggle,
                            admitted_can_admit_toggle,
                            end_on_host_leave_toggle,
                            allow_guests_toggle,
                            saving,
                            toggle_error,
                            connection_error,
                            on_join: move |_| {
                                mda.borrow().request();
                            },
                        }
                    }
                }
                if show_device_warning() {
                    div { class: "modal-overlay",
                        div { class: "modal-window",
                            h3 { "Device access problem" }
                            if let Some(err) = mic_error.read().as_ref() {
                                {render_single_device_error("Microphone", err)}
                            }
                            if let Some(err) = video_error.read().as_ref() {
                                {render_single_device_error("Camera", err)}
                            }
                            {
                                let mut client = client.clone();
                                rsx! {
                                    button {
                                        class: "btn-apple btn-primary",
                                        style: "margin-top: 1.5rem;",
                                        onclick: move |_| {
                                            show_device_warning.set(false);
                                            if let Err(e) = client.connect() {
                                                error!("Connection failed: {e:?}");
                                            }
                                            meeting_joined.set(true);
                                        },
                                        "Ok"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };
    }

    // --- Meeting view ---
    // Update the meeting time context signal
    meeting_time_signal.set(MeetingTime {
        call_start_time: call_start_time(),
        meeting_start_time: meeting_start_time_server(),
    });

    info!("Rendering meeting view with {} peers", display_peers.len());

    // Clear stale pin: if the pinned peer left the meeting, reset to None so
    // that is_speaking_suppressed() no longer suppresses glow for everyone.
    {
        let current_pinned = pinned_peer_id();
        if let Some(ref pid) = current_pinned {
            let still_exists = display_peers
                .iter()
                .any(|peer_id| client.get_peer_user_id(peer_id).as_deref() == Some(pid));
            if !still_exists {
                pinned_peer_id.set(None);
            }
        }
    }

    // Phase 3 of active_decode_set construction (see ordering invariant above).
    let current_pinned = pinned_peer_id();
    if let Some(pinned_user_id) = current_pinned.as_deref() {
        if let Some(pinned_session_id) = display_peers
            .iter()
            .find(|peer_id| client.get_peer_user_id(peer_id).as_deref() == Some(pinned_user_id))
            .and_then(|peer_id| peer_id.parse::<u64>().ok())
        {
            active_decode_set.insert(pinned_session_id);
        }
    }
    {
        // Dedup: only push to client when the set actually changed.
        let mut previous_active_decode_set = previous_active_decode_set.borrow_mut();
        if *previous_active_decode_set != active_decode_set {
            client.set_active_decode_set(&active_decode_set);
            *previous_active_decode_set = active_decode_set.clone();
        }
    }

    let toggle_pin = {
        let client = client.clone();
        move |pid: String| {
            // pid is already a user_id from canvas_generator.rs.
            // Keep normalization defensive in case a session_id is passed in the future.
            let normalized = client.get_peer_user_id(&pid).unwrap_or_else(|| pid.clone());

            let cur = pinned_peer_id();
            if cur.as_deref() == Some(normalized.as_str()) {
                pinned_peer_id.set(None);
            } else {
                pinned_peer_id.set(Some(normalized));
            }
        }
    };

    rsx! {
        div {
            // Provide MeetingTime context
            // Provide VideoCallClient context
            div { id: "main-container", class: "meeting-page",
                BrowserCompatibility {}

                // "participant joined/left" toast notifications
                if !peer_toasts().is_empty() {
                    div { class: "peer-toasts",
                        for (id , display_name , _ , is_joined) in peer_toasts().iter().cloned() {
                            {
                                let variant_class = if is_joined {
                                    "peer-toast toast-joined"
                                } else {
                                    "peer-toast toast-left"
                                };
                                let action_text = if is_joined {
                                    "joined the meeting"
                                } else {
                                    "left the meeting"
                                };
                                rsx! {
                                    div { key: "{id}", class: "{variant_class}",
                                        span { class: "toast-icon",
                                            if is_joined {
                                                svg {
                                                    width: "16",
                                                    height: "16",
                                                    view_box: "0 0 24 24",
                                                    fill: "none",
                                                    stroke: "currentColor",
                                                    stroke_width: "2",
                                                    stroke_linecap: "round",
                                                    stroke_linejoin: "round",
                                                    path { d: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" }
                                                    circle { cx: "9", cy: "7", r: "4" }
                                                    line {
                                                        x1: "19",
                                                        y1: "8",
                                                        x2: "19",
                                                        y2: "14",
                                                    }
                                                    line {
                                                        x1: "22",
                                                        y1: "11",
                                                        x2: "16",
                                                        y2: "11",
                                                    }
                                                }
                                            } else {
                                                svg {
                                                    width: "16",
                                                    height: "16",
                                                    view_box: "0 0 24 24",
                                                    fill: "none",
                                                    stroke: "currentColor",
                                                    stroke_width: "2",
                                                    stroke_linecap: "round",
                                                    stroke_linejoin: "round",
                                                    path { d: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" }
                                                    circle { cx: "9", cy: "7", r: "4" }
                                                    line {
                                                        x1: "22",
                                                        y1: "11",
                                                        x2: "16",
                                                        y2: "11",
                                                    }
                                                }
                                            }
                                        }
                                        span { class: "toast-text",
                                            span { class: "toast-name", "{display_name}" }
                                            br {}
                                            span { class: "toast-action", "{action_text}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                div { id: "grid-container", style: "{container_style}",
                    onmousemove: move |evt| {
                        if ss_resizing() {
                            let native = evt.as_web_event();
                            if let Some(target) = native.current_target() {
                                use wasm_bindgen::JsCast;
                                if let Ok(el) = target.dyn_into::<web_sys::HtmlElement>() {
                                    let rect = el.get_bounding_client_rect();
                                    let x = native.client_x() as f64 - rect.left();
                                    let w = rect.width();
                                    if w > 0.0 {
                                        let ratio = (x / w).clamp(0.3, 0.85);
                                        screen_share_ratio.set(ratio);
                                    }
                                }
                            }
                        }
                    },
                    onmouseup: move |evt: MouseEvent| {
                        if ss_resizing() {
                            evt.prevent_default();
                            ss_resizing.set(false);
                        }
                    },
                    onmouseleave: move |evt: MouseEvent| {
                        if ss_resizing() {
                            evt.prevent_default();
                            ss_resizing.set(false);
                        }
                    },

                    if has_screen_share {
                        // ---- Split layout: active screen share (left) + peer videos (right) ----
                        {
                            let left_pct = screen_share_ratio() * 100.0;
                            let right_pct = (1.0 - screen_share_ratio()) * 100.0 - 0.4; // account for handle
                            let handle_class = if ss_resizing() {
                                "screen-share-resize-handle dragging"
                            } else {
                                "screen-share-resize-handle"
                            };
                            rsx! {
                                // Left panel — ONLY the most recent (active) screen sharer
                                div { style: "width: {left_pct:.2}%; min-width: 0; height: 100%; display: flex; flex-direction: column; \
                                            align-items: center; justify-content: center; overflow: hidden;",
                                    if let Some(ref active_peer) = active_screen_sharer {
                                        PeerTile {
                                            key: "ss-active-{active_peer}",
                                            peer_id: active_peer.clone(),
                                            full_bleed: true,
                                            host_user_id: host_user_id.clone(),
                                            render_mode: TileMode::ScreenOnly,
                                            my_peer_id: user_id.clone(),
                                            pinned_peer_id: current_pinned.clone(),
                                            on_toggle_pin: toggle_pin.clone(),
                                        }
                                    }
                                }
                                // Resize handle
                                div {
                                    class: "{handle_class}",
                                    onmousedown: move |evt| {
                                        evt.prevent_default();
                                        ss_resizing.set(true);
                                    },
                                }
                                // Right panel — 1 or 2-column grid of compact peer tiles
                                div {
                                    style: {
                                        let grid_cols = if ss_cols > 1.0 { "1fr 1fr" } else { "1fr" };
                                        format!("width: {right_pct:.2}%; min-width: 0; height: 100%; \
                                                display: grid; grid-template-columns: {grid_cols}; \
                                                grid-auto-rows: {ss_tile_h:.0}px; \
                                                gap: 8px; padding: 6px; \
                                                align-content: start; overflow: visible;")
                                    },
                                    for tile_id in ss_tiles.iter() {
                                        {
                                            let is_mock = tile_id.starts_with("mock-");
                                            if is_mock {
                                                rsx! {
                                                    PeerTile {
                                                        key: "tile-{tile_id}",
                                                        peer_id: tile_id.clone(),
                                                        full_bleed: false,
                                                        host_user_id: host_user_id.clone(),
                                                        render_mode: TileMode::VideoOnly,
                                                        my_peer_id: user_id.clone(),
                                                        on_toggle_pin: move |_: String| {},
                                                    }
                                                }
                                            } else {
                                                rsx! {
                                                    PeerTile {
                                                        key: "tile-{tile_id}",
                                                        peer_id: tile_id.clone(),
                                                        full_bleed: false,
                                                        host_user_id: host_user_id.clone(),
                                                        render_mode: TileMode::VideoOnly,
                                                        my_peer_id: user_id.clone(),
                                                        pinned_peer_id: current_pinned.clone(),
                                                        on_toggle_pin: toggle_pin.clone(),
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    if ss_overflow_count > 0 {
                                        div {
                                            class: "grid-overflow-badge",
                                            "+{ss_overflow_count}"
                                            span { "more in meeting" }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        // ---- Normal grid layout ----
                        for tile_id in visible_tiles.iter() {
                            {
                                let is_mock = tile_id.starts_with("mock-");
                                let full_bleed = !is_mock
                                    && visible_tile_count == 1
                                    && !client.is_screen_share_enabled_for_peer(tile_id);
                                if is_mock {
                                    rsx! {
                                        PeerTile {
                                            key: "tile-{tile_id}",
                                            peer_id: tile_id.clone(),
                                            full_bleed: false,
                                            host_user_id: host_user_id.clone(),
                                            my_peer_id: user_id.clone(),
                                            on_toggle_pin: move |_: String| {},
                                        }
                                    }
                                } else {
                                    rsx! {
                                        PeerTile {
                                            key: "tile-{tile_id}",
                                            peer_id: tile_id.clone(),
                                            full_bleed,
                                            host_user_id: host_user_id.clone(),
                                            my_peer_id: user_id.clone(),
                                            pinned_peer_id: current_pinned.clone(),
                                            on_toggle_pin: toggle_pin.clone(),
                                        }
                                    }
                                }
                            }
                        }

                        if overflow_count > 0 {
                            div {
                                class: "grid-overflow-badge",
                                "+{overflow_count}"
                                span { "more in meeting" }
                            }
                        }

                        // Invitation overlay when no peers
                        if visible_tiles.is_empty() {
                            div {
                                id: "invite-overlay",
                                class: "card-apple",
                                style: "position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); width: 90%; max-width: 420px; z-index: 0; text-align: center;",
                                h4 { style: "margin-top:0;", "Your meeting is ready!" }
                                p { style: "{SUBTLE_HELP_TEXT_STYLE}",
                                    "Share this meeting link with others you want in the meeting"
                                }
                                div { style: "display:flex; align-items:center; margin-top: 0.75rem; margin-bottom: 0.75rem;",
                                    input {
                                        id: "meeting-link-input",
                                        value: "{meeting_link}",
                                        readonly: true,
                                        class: "input-apple",
                                        style: "flex:1; overflow:hidden; text-overflow: ellipsis;",
                                    }
                                    button {
                                        class: if show_copy_toast() { "btn-apple btn-primary btn-sm copy-button btn-pop-animate" } else { "btn-apple btn-primary btn-sm copy-button" },
                                        style: "margin-left: 0.5rem;",
                                        onclick: {
                                            let meeting_link = meeting_link.clone();
                                            move |_| {
                                                if let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard())
                                                {
                                                    let _ = clipboard.write_text(&meeting_link);
                                                    show_copy_toast.set(true);
                                                    Timeout::new(
                                                            1640,
                                                            move || {
                                                                show_copy_toast.set(false);
                                                            },
                                                        )
                                                        .forget();
                                                }
                                            }
                                        },
                                        "Copy"
                                        if show_copy_toast() {
                                            div {
                                                class: "sparkles",
                                                "aria-hidden": "true",
                                                span { class: "sparkle" }
                                                span { class: "sparkle" }
                                                span { class: "sparkle" }
                                                span { class: "sparkle" }
                                                span { class: "sparkle" }
                                                span { class: "sparkle" }
                                                span { class: "sparkle" }
                                                span { class: "sparkle" }
                                            }
                                        }
                                    }
                                }
                                p { style: "{SUBTLE_FOOTNOTE_TEXT_STYLE}",
                                    "People who use this meeting link must get your permission before they can join."
                                }
                                div {
                                    class: if show_copy_toast() { "copy-toast copy-toast--visible" } else { "copy-toast" },
                                    role: "alert",
                                    "aria-live": "assertive",
                                    "Link copied to clipboard"
                                }
                            }
                        }
                    } // end of else (normal grid layout)

                    // Controls nav
                    if can_stream {
                        nav { id: "host-controls-nav",
                            class: "host",
                            style: "box-shadow: none; transition: border-color 0.3s ease-out, box-shadow 1.5s ease-out;",
                            onmounted: move |evt| {
                                if let Some(elem) = evt.try_as_web_event() {
                                    host_el.set(Some(elem));
                                }
                            },
                            div { class: "controls",
                                nav {
                                    class: {
                                        let pos = dock_position().css_class();
                                        let hidden = if controls_visible() { "" } else { " controls-hidden" };
                                        let expanded = if controls_expanded() { " controls-expanded" } else { "" };
                                        format!("video-controls-container {pos}{hidden}{expanded}")
                                    },
                                    // Primary: Mic button - always visible
                                    {
                                        let mda_mic = mda.clone();
                                        rsx! {
                                            MicButton {
                                                enabled: mic_enabled(),
                                                available: mic_error.read().is_none(),
                                                onclick: move |_| {
                                                    if !mic_enabled() {
                                                        if mda_mic.borrow().is_granted(MediaAccessKind::AudioCheck) {
                                                            mic_enabled.set(true);
                                                        } else {
                                                            pending_mic_enable.set(true);
                                                            mda_mic.borrow().request();
                                                        }
                                                    } else {
                                                        mic_enabled.set(false);
                                                    }
                                                },
                                            }
                                        }
                                    }
                                    // Primary: Camera button - always visible
                                    {
                                        let mda_cam = mda.clone();
                                        rsx! {
                                            CameraButton {
                                                enabled: video_enabled(),
                                                available: video_error.read().is_none(),
                                                onclick: move |_| {
                                                    if !video_enabled() {
                                                        if mda_cam.borrow().is_granted(MediaAccessKind::VideoCheck) {
                                                            video_enabled.set(true);
                                                            // "Warm up" the video element in this user-gesture
                                                            // call stack.  Safari blocks play() outside user
                                                            // gestures; calling it here marks the element as
                                                            // user-activated so later srcObject + autoplay works.
                                                            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                                                                if let Some(elem) = doc.get_element_by_id("webcam") {
                                                                    use wasm_bindgen::JsCast;
                                                                    if let Ok(v) = elem.dyn_into::<web_sys::HtmlVideoElement>() {
                                                                        let _ = v.play();
                                                                    }
                                                                }
                                                            }
                                                        } else {
                                                            pending_video_enable.set(true);
                                                            mda_cam.borrow().request();
                                                        }
                                                    } else {
                                                        video_enabled.set(false);
                                                    }
                                                },
                                            }
                                        }
                                    }
                                    // (в) Secondary buttons — hidden by default, expand on hover
                                    div { class: "controls-secondary",
                                        if !is_ios() {
                                            {
                                                let is_active = matches!(screen_share_state(), ScreenShareState::Active);
                                                let is_disabled = matches!(screen_share_state(), ScreenShareState::Requesting | ScreenShareState::StreamReady);
                                                rsx! {
                                                    ScreenShareButton {
                                                        active: is_active,
                                                        disabled: is_disabled,
                                                        onclick: {
                                                            let stream_cell = pre_acquired_screen_stream.clone();
                                                            move |_| {
                                                                if matches!(screen_share_state(), ScreenShareState::Idle) {
                                                                    // Call getDisplayMedia synchronously within the user
                                                                    // gesture (click) handler.  Safari rejects the call if
                                                                    // it crosses an async boundary (spawn_local / Timeout).
                                                                    // Obtaining the Promise is synchronous and satisfies the
                                                                    // gesture requirement; only the await happens later.
                                                                    let navigator = gloo_utils::window().navigator();
                                                                    let media_devices = match navigator.media_devices() {
                                                                        Ok(md) => md,
                                                                        Err(e) => {
                                                                            log::error!("Failed to get media devices: {e:?}");
                                                                            return;
                                                                        }
                                                                    };

                                                                    // Build constraints identical to those in ScreenEncoder::start()
                                                                    let width_constraint = js_sys::Object::new();
                                                                    let _ = js_sys::Reflect::set(
                                                                        &width_constraint,
                                                                        &JsValue::from_str("ideal"),
                                                                        &JsValue::from_f64(1920.0),
                                                                    );
                                                                    let height_constraint = js_sys::Object::new();
                                                                    let _ = js_sys::Reflect::set(
                                                                        &height_constraint,
                                                                        &JsValue::from_str("ideal"),
                                                                        &JsValue::from_f64(1080.0),
                                                                    );
                                                                    let framerate_constraint = js_sys::Object::new();
                                                                    let _ = js_sys::Reflect::set(
                                                                        &framerate_constraint,
                                                                        &JsValue::from_str("ideal"),
                                                                        &JsValue::from_f64(10.0),
                                                                    );
                                                                    let video_constraints = js_sys::Object::new();
                                                                    let _ = js_sys::Reflect::set(
                                                                        &video_constraints,
                                                                        &JsValue::from_str("width"),
                                                                        &width_constraint.into(),
                                                                    );
                                                                    let _ = js_sys::Reflect::set(
                                                                        &video_constraints,
                                                                        &JsValue::from_str("height"),
                                                                        &height_constraint.into(),
                                                                    );
                                                                    let _ = js_sys::Reflect::set(
                                                                        &video_constraints,
                                                                        &JsValue::from_str("frameRate"),
                                                                        &framerate_constraint.into(),
                                                                    );

                                                                    let constraints = web_sys::DisplayMediaStreamConstraints::new();
                                                                    constraints.set_video(&video_constraints.into());
                                                                    constraints.set_audio(&JsValue::FALSE);

                                                                    // This call happens synchronously in the click handler --
                                                                    // the browser returns a Promise without rejecting.
                                                                    let promise = match media_devices.get_display_media_with_constraints(&constraints) {
                                                                        Ok(p) => p,
                                                                        Err(e) => {
                                                                            log::error!("getDisplayMedia failed synchronously: {e:?}");
                                                                            return;
                                                                        }
                                                                    };

                                                                    // Mark as requesting immediately so the button shows disabled
                                                                    screen_share_state.set(ScreenShareState::Requesting);

                                                                    // Now await the promise asynchronously -- this is fine,
                                                                    // the gesture requirement was satisfied above.
                                                                    let cell = stream_cell.clone();
                                                                    wasm_bindgen_futures::spawn_local(async move {
                                                                        match JsFuture::from(promise).await {
                                                                            Ok(stream) => {
                                                                                let media_stream: web_sys::MediaStream = stream.unchecked_into();
                                                                                cell.borrow_mut().replace(media_stream);
                                                                                // StreamReady causes is_sharing() to return true,
                                                                                // which gives Host share_screen=true so it picks
                                                                                // up the pre-acquired stream and starts encoding.
                                                                                screen_share_state.set(ScreenShareState::StreamReady);
                                                                            }
                                                                            Err(e) => {
                                                                                // User cancelled or browser denied
                                                                                let is_cancel = js_sys::Reflect::get(&e, &JsValue::from_str("name"))
                                                                                    .ok()
                                                                                    .and_then(|v| v.as_string())
                                                                                    .map(|n| n == "NotAllowedError")
                                                                                    .unwrap_or(false);
                                                                                if is_cancel {
                                                                                    log::info!("User cancelled screen sharing");
                                                                                } else {
                                                                                    log::error!("getDisplayMedia rejected: {e:?}");
                                                                                }
                                                                                screen_share_state.set(ScreenShareState::Idle);
                                                                            }
                                                                        }
                                                                    });
                                                                } else {
                                                                    screen_share_state.set(ScreenShareState::Idle);
                                                                }
                                                            }
                                                        },
                                                    }
                                                }
                                            }
                                        }
                                        PeerListButton {
                                            open: peer_list_open(),
                                            onclick: move |_| {
                                                peer_list_open.set(!peer_list_open());
                                                if peer_list_open() {
                                                    diagnostics_open.set(false);
                                                }
                                            },
                                        }
                                        if show_density_selector && !has_screen_share {
                                            DensityModeButton {
                                                label: density_mode().label().to_string(),
                                                open: density_open(),
                                                onclick: move |_| {
                                                    density_open.set(!density_open());
                                                },
                                            }
                                        }
                                        if mock_peers_enabled() {
                                            MockPeersButton {
                                                open: mock_peers_open(),
                                                onclick: move |_| {
                                                    mock_peers_open.set(!mock_peers_open());
                                                },
                                            }
                                        }
                                        DiagnosticsButton {
                                            open: diagnostics_open(),
                                            onclick: move |_| {
                                                diagnostics_open.set(!diagnostics_open());
                                                if diagnostics_open() {
                                                    peer_list_open.set(false);
                                                }
                                            },
                                        }
                                        DeviceSettingsButton {
                                            open: device_settings_open(),
                                            onclick: move |_| {
                                                device_settings_open.set(!device_settings_open());
                                                if device_settings_open() {
                                                    peer_list_open.set(false);
                                                    diagnostics_open.set(false);
                                                }
                                            },
                                        }
                                    }
                                    // (а) Dock position dropdown — glass-select style
                                    div { class: "dock-position-wrapper",
                                        div { class: if dock_menu_open() { "glass-select open" } else { "glass-select" },
                                            button {
                                                class: if dock_menu_open() { "video-control-button active" } else { "video-control-button" },
                                                title: "Dock position",
                                                r#type: "button",
                                                "aria-haspopup": "listbox",
                                                "aria-expanded": if dock_menu_open() { "true" } else { "false" },
                                                onclick: move |e| {
                                                    e.stop_propagation();
                                                    dock_menu_open.set(!dock_menu_open());
                                                },
                                                svg {
                                                    xmlns: "http://www.w3.org/2000/svg",
                                                    width: "20",
                                                    height: "20",
                                                    view_box: "0 0 24 24",
                                                    fill: "none",
                                                    stroke: "currentColor",
                                                    stroke_width: "2",
                                                    stroke_linecap: "round",
                                                    stroke_linejoin: "round",
                                                    rect { x: "3", y: "3", width: "7", height: "7" }
                                                    rect { x: "14", y: "3", width: "7", height: "7" }
                                                    rect { x: "3", y: "14", width: "7", height: "7" }
                                                    rect { x: "14", y: "14", width: "7", height: "7" }
                                                }
                                            }
                                            if dock_menu_open() {
                                                div {
                                                    class: "glass-select-menu",
                                                    role: "listbox",
                                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                                    div {
                                                        class: if dock_position() == DockPosition::Bottom { "glass-select-option selected" } else { "glass-select-option" },
                                                        role: "option",
                                                        onclick: move |e: MouseEvent| {
                                                            e.stop_propagation();
                                                            dock_position.set(DockPosition::Bottom);
                                                            dock_menu_open.set(false);
                                                        },
                                                        "Bottom"
                                                    }
                                                    div {
                                                        class: if dock_position() == DockPosition::Left { "glass-select-option selected" } else { "glass-select-option" },
                                                        role: "option",
                                                        onclick: move |e: MouseEvent| {
                                                            e.stop_propagation();
                                                            dock_position.set(DockPosition::Left);
                                                            dock_menu_open.set(false);
                                                        },
                                                        "Left"
                                                    }
                                                    div {
                                                        class: if dock_position() == DockPosition::Right { "glass-select-option selected" } else { "glass-select-option" },
                                                        role: "option",
                                                        onclick: move |e: MouseEvent| {
                                                            e.stop_propagation();
                                                            dock_position.set(DockPosition::Right);
                                                            dock_menu_open.set(false);
                                                        },
                                                        "Right"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    {
                                        let hangup_client = client.clone();
                                        let hangup_id = id.clone();
                                        let hangup_is_guest = is_guest;
                                        let hangup_room_token = room_token.clone();
                                        rsx! {
                                            HangUpButton {
                                                onclick: move |_| {
                                                    log::info!("Hanging up - resetting to initial state");
                                                    // Flush console logs before disconnecting so the
                                                    // final chunk reaches the server while the session
                                                    // is still active.
                                                    if crate::constants::console_log_upload_enabled()
                                                        .unwrap_or(false)
                                                    {
                                                        flush_console_logs();
                                                    }
                                                    if hangup_client.is_connected() {
                                                        if let Err(e) = hangup_client.disconnect() {
                                                            log::error!("Error disconnecting: {e}");
                                                        }
                                                    }
                                                    meeting_joined.set(false);
                                                    mic_enabled.set(false);
                                                    video_enabled.set(false);
                                                    pending_mic_enable.set(false);
                                                    pending_video_enable.set(false);
                                                    call_start_time.set(None);
                                                    meeting_start_time_server.set(None);

                                                    let meeting_id = hangup_id.clone();
                                                    let room_token = hangup_room_token.clone();
                                                    wasm_bindgen_futures::spawn_local(async move {
                                                        if hangup_is_guest {
                                                            let _ = crate::meeting_api::leave_meeting_as_guest(&meeting_id, &room_token).await;
                                                        } else if let Err(e) = crate::meeting_api::leave_meeting(&meeting_id).await {
                                                            log::error!("Error leaving meeting: {e}");
                                                        }
                                                        let _ = window().location().set_href("/");
                                                    });
                                                },
                                            }
                                        }
                                    }
                                }
                            }
                            // User error dialog
                            if let Some(err) = user_error() {
                                {
                                    let displayed: String = err.chars().take(200).collect();
                                    rsx! {
                                        div { class: "glass-backdrop",
                                            div { class: "card-apple", style: "width: 380px;",
                                                h4 { style: "margin-top:0;", "Error" }
                                                p { style: "margin-top:0.5rem;", "{displayed}" }
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
                                }
                            }
                            // Host component (encoders)
                            if media_access_granted() {
                                Host {
                                    share_screen: screen_share_state().is_sharing(),
                                    mic_enabled: mic_enabled(),
                                    video_enabled: video_enabled(),
                                    on_encoder_settings_update: move |_s: String| {},
                                    device_settings_open: device_settings_open(),
                                    on_device_settings_toggle: move |_| {
                                        device_settings_open.set(!device_settings_open());
                                    },
                                    on_microphone_error: move |err: String| {
                                        log::error!("Microphone error: {err}");
                                        mic_enabled.set(false);
                                        user_error.set(Some(format!("Microphone error: {err}")));
                                    },
                                    on_camera_error: move |err: String| {
                                        log::error!("Camera error: {err}");
                                        video_enabled.set(false);
                                        user_error.set(Some(format!("Camera error: {err}")));
                                    },
                                    on_screen_share_state: move |event: ScreenShareEvent| {
                                        log::info!("Screen share state changed: {event:?}");
                                        match event {
                                            ScreenShareEvent::Started(_stream) => {
                                                screen_share_state.set(ScreenShareState::Active);
                                            }
                                            ScreenShareEvent::Cancelled | ScreenShareEvent::Stopped => {
                                                screen_share_state.set(ScreenShareState::Idle);
                                            }
                                            ScreenShareEvent::Failed(ref msg) => {
                                                log::error!("Screen share failed: {msg}");
                                                screen_share_state.set(ScreenShareState::Idle);
                                                user_error.set(Some(format!("Screen share failed: {msg}")));
                                            }
                                        }
                                    },
                                    reload_devices_counter: reload_devices_counter(),
                                }
                            }
                            if is_guest {
                                div { class: "guest-badge-preview", "Guest" }
                            }
                            {
                                let status_client = client.clone();
                                rsx! {
                                    div {
                                        class: if status_client.is_connected() { "connection-led connected" } else { "connection-led connecting" },
                                        title: if status_client.is_connected() { "Connected" } else { "Connecting" },
                                    }
                                }
                            }
                            ConnectionQualityIndicator {}
                        }
                    }
                }

                // Peer list sidebar
                div {
                    id: "peer-list-container",
                    class: if peer_list_open() { "visible" } else { "" },
                    if peer_list_open() {
                        PeerList {
                            peers: peers_for_display.clone(),
                            onclose: move |_| peer_list_open.set(false),
                            self_muted: !mic_enabled(),
                            self_speaking: local_speaking(),
                            show_meeting_info: meeting_info_open(),
                            room_id: id_for_peer_list.clone(),
                            num_participants: num_display_peers,
                            is_active: meeting_joined() && meeting_ended_message().is_none(),
                            on_toggle_meeting_info: move |_| {
                                meeting_info_open.set(!meeting_info_open());
                                if meeting_info_open() {
                                    diagnostics_open.set(false);
                                    device_settings_open.set(false);
                                }
                            },
                            host_display_name: host_display_name.clone(),
                            host_user_id: host_user_id.clone(),
                            local_user_display_name: current_display_name(),
                            on_edit_self_name: {move |_| {
                                display_name_modal_open.set(true);
                            }},
                        }
                    }
                }

                // Waiting room controls (host or admitted participants when allowed)
                if is_owner || admitted_can_admit_toggle() {
                    HostControls {
                        meeting_id: id.clone(),
                        is_admitted: true,
                        waiting_room_version,
                    }
                }

                if display_name_modal_open() {
                    UpdateDisplayNameModal {
                        current_display_name: current_display_name(),
                        meeting_id: id.clone(),
                        on_close: move |_| {
                            display_name_modal_open.set(false);
                        },
                        on_success: move |new_name: String| {
                            // Update local UI immediately — do NOT wait for server broadcast.
                            // The server will broadcast PARTICIPANT_DISPLAY_NAME_CHANGED moments later,
                            // which will be handled by on_display_name_changed callback and will
                            // confirm the same value. This ensures no perceived lag for the user.
                            log::info!("RENAME: on_success called with new_name: {}", new_name);
                            let mut current_name = current_display_name;
                            current_name.set(new_name.clone());
                            let mut dn_ctx = display_name_ctx_signal;
                            dn_ctx.set(Some(new_name.clone()));
                            display_name_modal_open.set(false);
                        },
                    }
                }

                // Meeting ended overlay
                if let Some(message) = meeting_ended_message() {
                    MeetingEndedOverlay { message }
                }

                // Diagnostics sidebar
                if diagnostics_open() {
                    Diagnostics {
                        is_open: true,
                        on_close: move |_| diagnostics_open.set(false),
                        video_enabled: video_enabled(),
                        mic_enabled: mic_enabled(),
                        share_screen: screen_share_state().is_sharing(),
                        encoder_settings: encoder_settings(),
                    }
                }

                // Mock peers popover (only shown when env-gated)
                if mock_peers_enabled() && mock_peers_open() {
                    div { class: "mock-peers-popover",
                        div { class: "mock-peers-popover-header",
                            span { "Mock Peers" }
                            button {
                                class: "mock-peers-popover-close",
                                onclick: move |_| mock_peers_open.set(false),
                                "\u{2715}"
                            }
                        }
                        div { class: "mock-peers-popover-body",
                            label { r#for: "mock-count-input", "Count (0\u{2013}100)" }
                            div { class: "mock-peers-count-row",
                                button {
                                    class: "mock-peers-step",
                                    onclick: move |_| {
                                        let c = debug_peer_count().saturating_sub(1);
                                        debug_peer_count.set(c);
                                    },
                                    "\u{2212}"
                                }
                                input {
                                    id: "mock-count-input",
                                    r#type: "number",
                                    min: "0",
                                    max: "100",
                                    value: "{debug_peer_count()}",
                                    oninput: move |e| {
                                        let n = e.value().parse::<u32>().unwrap_or(0).min(100);
                                        debug_peer_count.set(n);
                                    },
                                }
                                button {
                                    class: "mock-peers-step",
                                    onclick: move |_| {
                                        let c = (debug_peer_count() + 1).min(100);
                                        debug_peer_count.set(c);
                                    },
                                    "+"
                                }
                            }
                        }
                    }
                }

                // Density mode popover
                if show_density_selector && !has_screen_share && density_open() {
                    div { class: "density-popover",
                        for mode in DENSITY_MODES {
                            div {
                                key: "{mode.label()}",
                                class: if density_mode() == mode { "density-option active" } else { "density-option" },
                                onclick: move |_| {
                                    density_mode.set(mode);
                                    density_open.set(false);
                                },
                                span { class: "density-option-label", "{mode.label()}" }
                                span { class: "density-option-range", "{mode.description()}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Parse a `peer_status` diagnostics event into a `(peer_id, PeerMediaState)`.
fn parse_peer_status_event(
    evt: &videocall_diagnostics::DiagEvent,
) -> Option<(String, PeerMediaState)> {
    use videocall_diagnostics::MetricValue;

    let mut to_peer: Option<String> = None;
    let mut audio = false;
    let mut video = false;
    let mut screen = false;
    for m in &evt.metrics {
        match (m.name, &m.value) {
            ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
            ("audio_enabled", MetricValue::U64(v)) => audio = *v != 0,
            ("video_enabled", MetricValue::U64(v)) => video = *v != 0,
            ("screen_enabled", MetricValue::U64(v)) => screen = *v != 0,
            _ => {}
        }
    }
    to_peer.map(|id| {
        (
            id,
            PeerMediaState {
                audio_enabled: audio,
                video_enabled: video,
                screen_enabled: screen,
            },
        )
    })
}

/// Parse a `peer_speaking` diagnostics event. Returns `Some(peer_id)` when
/// the event indicates the peer is actively speaking (audio_level > 0 or
/// speaking flag set).
fn parse_speaking_peer(evt: &videocall_diagnostics::DiagEvent) -> Option<String> {
    use videocall_diagnostics::MetricValue;

    let mut to_peer: Option<String> = None;
    let mut audio_lvl: Option<f64> = None;
    let mut speaking: Option<bool> = None;
    for m in &evt.metrics {
        match (m.name, &m.value) {
            ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
            ("audio_level", MetricValue::F64(v)) => audio_lvl = Some(*v),
            ("speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
            _ => {}
        }
    }
    let is_speaking = audio_lvl.map(|l| l > 0.0).unwrap_or(false) || speaking.unwrap_or(false);
    if is_speaking {
        to_peer
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests for reconnect_delay_ms
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    /// attempt=0 should return Some(d) where d is in the base range.
    /// base = 1000, jitter in [-25%, +25%), so delay in [750, 1250).
    /// The .max(500) clamp does not bind here.
    #[wasm_bindgen_test]
    fn reconnect_delay_attempt_0_returns_value_in_expected_range() {
        for _ in 0..50 {
            let delay = reconnect_delay_ms(0);
            assert!(delay.is_some(), "attempt 0 should return Some");
            let d = delay.unwrap();
            assert!(d >= 750, "attempt 0 delay {d} should be >= 750");
            assert!(d <= 1250, "attempt 0 delay {d} should be <= 1250");
        }
    }

    /// attempt=9 should return Some(d) with base capped at MAX_DELAY_MS (16000).
    /// jitter in [-25%, +25%), so delay in [12000, 20000).
    #[wasm_bindgen_test]
    fn reconnect_delay_attempt_9_returns_capped_value() {
        for _ in 0..50 {
            let delay = reconnect_delay_ms(9);
            assert!(delay.is_some(), "attempt 9 should return Some");
            let d = delay.unwrap();
            assert!(d >= 12000, "attempt 9 delay {d} should be >= 12000");
            assert!(d <= 20000, "attempt 9 delay {d} should be <= 20000");
        }
    }

    /// attempt=10 exceeds MAX_RECONNECT_ATTEMPTS and should return None.
    #[wasm_bindgen_test]
    fn reconnect_delay_attempt_10_returns_none() {
        assert!(
            reconnect_delay_ms(10).is_none(),
            "attempt 10 should return None"
        );
    }

    /// Attempts beyond 10 should also return None.
    #[wasm_bindgen_test]
    fn reconnect_delay_attempt_beyond_max_returns_none() {
        assert!(reconnect_delay_ms(11).is_none());
        assert!(reconnect_delay_ms(100).is_none());
        assert!(reconnect_delay_ms(u32::MAX).is_none());
    }

    /// Backoff should roughly double each attempt (accounting for jitter).
    /// We compare the midpoints of the expected ranges for successive attempts.
    /// attempt 0: base=1000, attempt 1: base=2000, attempt 2: base=4000, etc.
    #[wasm_bindgen_test]
    fn reconnect_delay_backoff_roughly_doubles() {
        // Collect many samples per attempt and check the average is near the expected base.
        let samples = 200;
        for attempt in 0..4u32 {
            let expected_base =
                (1000u32.saturating_mul(2u32.saturating_pow(attempt))).min(16_000) as f64;
            let sum: f64 = (0..samples)
                .map(|_| reconnect_delay_ms(attempt).unwrap() as f64)
                .sum();
            let avg = sum / samples as f64;
            // Average should be close to the base (jitter is symmetric around 0,
            // mean multiplier is ~0). Allow 15% tolerance for randomness.
            let tolerance = expected_base * 0.15;
            assert!(
                (avg - expected_base).abs() < tolerance,
                "attempt {attempt}: avg {avg:.0} should be near expected base {expected_base:.0} (tolerance {tolerance:.0})"
            );
        }
    }

    /// The minimum possible return value is 500 (enforced by .max(500.0)).
    /// For attempt 0 with base=1000, the lowest jitter gives 750, so the
    /// .max(500) clamp should never bind. Verify no value goes below 500.
    #[wasm_bindgen_test]
    fn reconnect_delay_never_below_500() {
        for attempt in 0..10u32 {
            for _ in 0..20 {
                let d = reconnect_delay_ms(attempt).unwrap();
                assert!(d >= 500, "attempt {attempt}: delay {d} must be >= 500");
            }
        }
    }
}
