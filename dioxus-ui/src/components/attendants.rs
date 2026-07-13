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

use crate::components::action_bar_layout::{
    apply_keyboard_reorder, load_action_bar_layout, remove_action_bar_layout,
    save_action_bar_layout, ActionBarSlot, DEFAULT_SLOTS,
};
use crate::components::decode_budget::{
    decide_step, effective_cap, expand_decoded_for_requested, ios_decode_tile_ceiling,
    is_sole_real_tile, merge_pinned_decode, merge_user_requested_decode, partition_camera_tiles,
    presenter_cap_ceiling, presenter_extra_shed_pressure, promote_pinned_into_decoded,
    promote_requested_into_decoded, should_clear_force_decode_on_override_change, BudgetSample,
    BudgetState, BudgetStep, MIN_CAP,
};
use crate::components::decode_budget_banner::DecodeBudgetBanner;
use crate::components::decode_paused_pill::DecodePausedPill;
use crate::components::pre_join_preview::PreviewEngine;
use crate::components::signal_quality::SignalMeterMode;
use crate::components::{
    browser_compatibility::BrowserCompatibility,
    canvas_generator::{speak_style, TileMode},
    connection_quality_indicator::ConnectionQualityIndicator,
    diagnostics::Diagnostics,
    host::Host,
    host_controls::HostControls,
    media_metrics_overlay::{MediaMetricsOverlayCtx, MEDIA_METRICS_OVERLAY_KEY},
    meeting_ended_overlay::MeetingEndedOverlay,
    meeting_options_controls::MeetingOptionsControls,
    peer_list::{PeerList, PeerListEntry},
    peer_tile::PeerTile,
    performance_settings::{DiagnosticsReader, PerfControlsHandle},
    pre_join_settings_card::PreJoinSettingsCard,
    update_display_name_modal::UpdateDisplayNameModal,
    video_control_buttons::{
        CameraButton, DensityModeButton, DeviceSettingsButton, DiagnosticsButton, HangUpButton,
        MeetingOptionsButton, MicButton, MockPeersButton, PeerListButton, ScreenShareButton,
    },
};
use crate::console_log_collector::{
    flush_console_logs, set_console_log_auth_token, set_console_log_context,
};
use crate::constants::actix_websocket_base;
use crate::constants::{
    mock_peers_enabled, server_election_period_ms, users_allowed_to_stream, webtransport_host_base,
    CANVAS_LIMIT,
};
use crate::context::{
    html_media_set_sink_id_supported, load_appearance_settings_from_storage,
    load_decode_budget_override, load_density_mode, load_dock_autohide, load_dock_position,
    load_preferred_camera_on, load_preferred_device_ids, load_preferred_mic_on,
    resolve_initial_enabled, resolve_transport_config, restore_device_id,
    save_appearance_settings_to_storage, save_density_mode, save_display_name_to_storage,
    save_dock_autohide, save_dock_position, save_preferred_camera_id, save_preferred_camera_on,
    save_preferred_mic_id, save_preferred_mic_on, save_preferred_speaker_id, validate_display_name,
    AppearanceSettingsCtx, AutohideCtx, CroppedTilesCtx, DecodeBudgetCtx, DecodeBudgetOverride,
    DensityModeCtx, DisplayNameCtx, DockPosition, DockPositionCtx, HostRefreshNonceCtx, HostSetCtx,
    LocalAudioLevelCtx, MeetingTime, PeerMediaState, PeerSignalHistoryMap, PeerStatusMap,
    SignalPopupStateMap, TransportPreference, TransportPreferenceCtx, UserRequestedDecodeCtx,
};
use crate::local_storage::{load_bool, load_f64, save_f64};
use crate::types::DeviceInfo;
use dioxus::prelude::Element as DioxusElement;
use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use gloo_timers::callback::Timeout;
use gloo_utils::window;
use log::error;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use videocall_client::utils::is_ios;
use videocall_client::Callback as VcCallback;
use videocall_client::MediaDeviceList;
use videocall_client::{
    ConnectionLostReason, MediaAccessKind, MediaDeviceAccess, MediaPermission,
    MediaPermissionsErrorState, PermissionState, ScreenShareEvent, VideoCallClient,
    VideoCallClientOptions,
};
#[cfg(feature = "media-server-jwt-auth")]
use videocall_client::{RefreshRoomTokenCallback, RefreshedTokens};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

/// Minimum width (px) a drawer can be dragged to. Below this the panel chrome
/// (headers, controls) stops being usable. The floor is driven by the
/// connection-manager section's Progress `.status-item` row (a `.progress-container`
/// with min-width 120px plus its "Progress:" label) — see issue 1482; 300px keeps
/// that row from overflowing inside the section/sidebar padding chrome.
const DRAWER_MIN_WIDTH: f64 = 300.0;
/// Absolute maximum drawer width (px). The per-side cap is the smaller of this
/// and 50% of the viewport (see `max_for_side` in the render body).
const DRAWER_MAX_ABS: f64 = 720.0;

/// Which drawer (if any) is currently being resized by a pointer drag. Tracked
/// at the meeting-view level because the width signals live here; the pointer
/// listeners (onpointerdown/move/up) live on each drawer's own
/// `.drawer-resize-handle` and use pointer capture (`set_pointer_capture`) so
/// pointermove/up route to the handle even when the cursor is over the drawer
/// body or the video tiles. The `#grid-container` onmousemove/up/leave handlers
/// only drive the screen-share split (`ss_resizing`), NOT drawer resize.
#[derive(Clone, Copy, PartialEq)]
enum ResizingDrawer {
    None,
    Left,
    Right,
}

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

/// UI state of the screen-share visibility toast (HCL issue 893). @token-exempt
///
/// Walks through `Starting` -> `SuccessfullyShared` (on the first
/// `PEER_EVENT(screen_decode_started)` ack from any peer) or
/// `Failed(message)` (on a 10s timeout with no ack).
#[derive(Clone, Debug, PartialEq)]
pub enum ScreenShareToastState {
    /// Local screen-share has started but no peer has confirmed visibility yet.
    Starting,
    /// At least one peer has acknowledged decoding our screen-share.
    SuccessfullyShared,
    /// The visibility window elapsed without any peer ack.
    Failed(String),
}

/// Shared cell that holds a pre-acquired `MediaStream` from `getDisplayMedia()`.
///
/// Safari requires `getDisplayMedia()` to be called synchronously within a
/// user-gesture handler.  The onclick handler obtains the stream and stores it
/// here; the `Host` component takes it out and passes it to
/// `ScreenEncoder::start_with_stream()`.
pub type PreAcquiredScreenStream = Rc<RefCell<Option<web_sys::MediaStream>>>;

#[derive(Debug, PartialEq, Eq)]
pub enum MediaErrorState {
    NoDevice,
    PermissionDenied,
    /// Device exists and the site is permitted, but another application is
    /// holding it (`getUserMedia` → `NotReadableError`). Recoverable: the app
    /// auto-retries this case in the background (see `should_auto_retry`), so the
    /// message below explicitly promises reconnection. Keep the copy in sync with
    /// that behavior.
    DeviceInUse,
    Other,
}

/// Whether the background auto-retry loop should keep polling a blocked device.
///
/// Only `DeviceInUse` is auto-retried: the OS/browser will let `getUserMedia`
/// succeed once the other app releases the device, with no user action. We do
/// NOT auto-retry `PermissionDenied` (a site-level deny is not silently
/// re-granted — retrying via JS is a no-op until the user changes browser
/// settings) nor `NoDevice`/`Other`.
///
/// This is deliberately NOT gated on any "user wants the device on" intent. A
/// background probe can ONLY ever CLEAR a stale `DeviceInUse` error (it never
/// sets `pending_*_enable`, so it can never auto-start capture — see the retry
/// tick closure), so keeping the badge accurate is always safe regardless of
/// whether the user has ever touched that device's button. Gating on intent
/// only left the "blocked" badge stuck forever for a device the user never
/// toggled on at pre-join and never clicked in-meeting, which the retry loop is
/// specifically meant to recover.
fn should_auto_retry(error: Option<&MediaErrorState>) -> bool {
    matches!(error, Some(MediaErrorState::DeviceInUse))
}

/// Outcome of ONE background auto-retry interval tick.
#[derive(Debug, PartialEq, Eq)]
struct RetryTickDecision {
    /// Whether THIS tick should issue a `getUserMedia` probe.
    probe: bool,
    /// The elapsed-ticks counter to store for the next tick.
    since: u32,
    /// The required gap (in ticks) to store for the next tick.
    gap: u32,
}

/// Pure backoff decision for the background auto-retry loop, extracted from the
/// `Interval` closure so the LONG-RUN schedule is unit-testable without a
/// browser or the Dioxus runtime (the closure itself is untestable off-target).
///
/// The interval fires on a fixed cadence (every `RETRY_BASE_INTERVAL_MS`). Only
/// ticks whose elapsed count has caught up to the current `gap` issue an actual
/// probe; the rest just advance the counter. On a probe, the counter resets and
/// the gap doubles, capped at `max_gap`. Starting from `(since=0, gap=1)` this
/// yields probes at ticks 1, 3, 7, 15, then every 15 ticks forever (gaps of
/// 1, 2, 4, 8, 15, 15, … ticks → 4s, 8s, 16s, 32s, 60s, 60s … at a 4s base).
///
/// Crucially, once `gap` reaches `max_gap` the schedule is a stable, unbounded
/// stream of probes exactly `max_gap` ticks apart — it can never enter a state
/// that stops issuing probes (see `retry_backoff_never_wedges`). This is the
/// long-run property that a hand-trace of only the first few ticks cannot prove.
fn retry_tick_decision(since: u32, gap: u32, max_gap: u32) -> RetryTickDecision {
    let next_since = since + 1;
    if next_since < gap {
        // Off-schedule tick: bump the counter, no probe.
        RetryTickDecision {
            probe: false,
            since: next_since,
            gap,
        }
    } else {
        // Probe tick: reset the counter and grow the gap (capped) for next time.
        RetryTickDecision {
            probe: true,
            since: 0,
            gap: (gap * 2).min(max_gap),
        }
    }
}

/// Whether the blocking "Device access problem" modal should auto-close.
///
/// The modal renders whenever `show_device_warning` is true, INDEPENDENT of the
/// current error signals (see `render_device_warning_modal`). A background
/// auto-retry can clear `mic_error`/`video_error` while the user has left the
/// modal open, which would strand them on an empty dialog with no error rows.
/// So once a probe result leaves BOTH sides error-free, close the modal if it is
/// still showing. `warning_shown` gates the write so we never call `.set(false)`
/// on an already-closed modal, and the both-sides-clear check means a probe that
/// recovers one side while the other is still (or newly) failing keeps the modal
/// up to display the remaining error.
fn should_auto_close_device_warning(
    mic_error_is_none: bool,
    video_error_is_none: bool,
    warning_shown: bool,
) -> bool {
    warning_shown && mic_error_is_none && video_error_is_none
}

/// Map the client-layer permission error into the UI-layer error state that
/// drives the modal copy. Kept 1:1 with the `on_result` classification so a
/// failure surfaced through the live encoder callback renders the exact same
/// message as one surfaced through the pre-flight permission probe.
fn map_permission_error(err: &MediaPermissionsErrorState) -> MediaErrorState {
    match err {
        MediaPermissionsErrorState::NoDevice => MediaErrorState::NoDevice,
        MediaPermissionsErrorState::PermissionDenied => MediaErrorState::PermissionDenied,
        MediaPermissionsErrorState::DeviceInUse => MediaErrorState::DeviceInUse,
        MediaPermissionsErrorState::Other(_) => MediaErrorState::Other,
    }
}

/// Target UI error state for ONE side after a permission probe.
///
/// `Denied(err)` maps to the corresponding [`MediaErrorState`]; any granted (or
/// otherwise non-denied) outcome maps to `None`. Callers invoke this only for a
/// side that was actually probed and pair it with a set-if-changed guard, so a
/// repeated probe that finds the same blocked state performs ZERO signal writes
/// (Dioxus `Signal::set` marks dirty unconditionally — no value dedupe), which
/// avoids a full-component re-render and a retry-effect re-run on every failed
/// background auto-retry tick. `Unknown` ("not probed this call") also maps to
/// `None`, but the call site never writes for an un-probed side, so that mapping
/// is never used to clobber a live error on the other side.
fn permission_probe_error_target(state: &PermissionState) -> Option<MediaErrorState> {
    match state {
        PermissionState::Denied(err) => Some(map_permission_error(err)),
        _ => None,
    }
}

const SUBTLE_HELP_TEXT_STYLE: &str = "font-size: 0.9rem; opacity: 0.8;";

fn render_single_device_error(device: &str, err: &MediaErrorState) -> Element {
    match err {
        MediaErrorState::NoDevice => rsx! {
            p { " {device} not found on this device." }
        },
        MediaErrorState::Other => rsx! {
            p { " {device} has an unexpected problem." }
        },
        MediaErrorState::DeviceInUse => rsx! {
            p { " {device} is being used by another application. Close whatever else is using it and it will reconnect automatically." }
        },
        MediaErrorState::PermissionDenied => rsx! {
            p { " {device} is blocked in your browser." }
            p { style: "{SUBTLE_HELP_TEXT_STYLE}",
                "Please click the lock icon in your browser's address bar and allow access if you want to use it."
            }
        },
    }
}

/// The blocking "Device access problem" modal, reused for BOTH the pre-join
/// failure path and the in-meeting failure path. The only difference between the
/// two call sites is what dismissing does (`on_dismiss`): pre-join connects +
/// joins, in-meeting just closes the modal. Kept as a plain free function (like
/// [`render_single_device_error`]) so both call sites share one source of truth.
fn render_device_warning_modal(
    mic_error: Option<&MediaErrorState>,
    video_error: Option<&MediaErrorState>,
    on_dismiss: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "modal-overlay", "data-testid": "device-warning-modal",
            div { class: "modal-window",
                h3 { "Device access problem" }
                if let Some(err) = mic_error {
                    {render_single_device_error("Microphone", err)}
                }
                if let Some(err) = video_error {
                    {render_single_device_error("Camera", err)}
                }
                button {
                    class: "btn-apple btn-primary",
                    style: "margin-top: 1.5rem;",
                    onclick: move |_| on_dismiss.call(()),
                    "Ok"
                }
            }
        }
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

/// Pure helper: combine the lobby URLs with the user's transport preference
/// and the *current* server-side WebTransport-enabled flag.
///
/// Factored out so both component init AND `schedule_reconnect` go through
/// exactly the same code path. Crucially, this means a reconnect happening
/// after the runtime config finally loaded will see `server_wt_enabled ==
/// true` even if the initial `use_hook` saw `false` — the previously empty
/// WT URL list will populate, fixing the "stranded on a single server"
/// regression from discussion 562 (Phase 7).
///
/// Returns `(effective_wt_enabled, websocket_urls, webtransport_urls)`.
fn current_transport_urls(
    token: &str,
    user_id: &str,
    id: &str,
    pref: TransportPreference,
    server_wt_enabled: bool,
) -> (bool, Vec<String>, Vec<String>) {
    let (ws, wt) = build_lobby_urls(token, user_id, id);
    current_transport_urls_from_lists(pref, server_wt_enabled, ws, wt)
}

/// Pure-logic core of [`current_transport_urls`] — takes already-built URL
/// lists so it can be unit-tested without `window().__APP_CONFIG` being
/// initialised. The wasm-only `current_transport_urls` is a thin wrapper
/// that pairs `build_lobby_urls` with this function.
fn current_transport_urls_from_lists(
    pref: TransportPreference,
    server_wt_enabled: bool,
    ws_urls: Vec<String>,
    wt_urls: Vec<String>,
) -> (bool, Vec<String>, Vec<String>) {
    resolve_transport_config(pref, server_wt_enabled, ws_urls, wt_urls)
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
                    // Keep console-log uploads authenticated after a reconnect
                    // (the token rotated). O(1); token value is never logged.
                    set_console_log_auth_token(&new_token);
                    let latest_display_name = current_display_name();

                    // Re-evaluate `webtransport_enabled()` at reconnect time —
                    // not just at component init. If runtime config hadn't
                    // loaded when `use_hook` ran, the WT URL list would have
                    // been empty and `total_server_count() == 1` would have
                    // suppressed re-election (see discussion 562, Phase 7).
                    // Going through the same `current_transport_urls` helper
                    // here means a delayed runtime config load now flows back
                    // into the manager via `update_server_urls`.
                    let pref = transport_pref_signal();
                    let server_wt_enabled =
                        crate::constants::webtransport_enabled().unwrap_or(false);
                    let (_enable_wt, ws, wt) = current_transport_urls(
                        &new_token,
                        &latest_display_name,
                        &meeting_id,
                        pref,
                        server_wt_enabled,
                    );

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

use super::attendants_layout::{
    compute_effective_density, compute_layout, promote_speakers, TILE_AR,
};
use super::density::{DensityMode, DENSITY_MODES};

/// Bump the host-event counter from the HOST_GRANTED/HOST_REVOKED handlers, so
/// the roster seed can tell a host event landed during its in-flight fetch and
/// skip a stale overwrite. NATS-handler safe (uses `peek`, no reactive read).
fn bump_host_event_seq(mut seq: Signal<u64>) {
    let next = seq.peek().wrapping_add(1);
    seq.set(next);
}

/// Should this `on_connected` emission reconcile host state? Returns
/// `false` on the first emission (the mount seed covers the initial connect)
/// and for guests, `true` on every reconnect. Flips `first_connect` true→false.
fn should_reconcile_host_on_connect(first_connect: &Cell<bool>, is_guest: bool) -> bool {
    let was_first = first_connect.replace(false);
    !was_first && !is_guest
}

/// Decide what a completed `/participants` roster read should do to the host
/// set: return `Some(hosts)` to apply, or `None` to discard the read.
///
/// This is the security-critical seq-recheck guard. When the host-event counter
/// advanced during the in-flight fetch (`current_seq != seq_at_start`), a live
/// HOST_GRANTED/HOST_REVOKED landed mid-fetch and is fresher than this roster
/// read, so we return `None` and the caller leaves `host_set_signal` untouched —
/// preventing a stale roster from clobbering a just-applied live revoke (which
/// would re-introduce a false host badge). `host_event_seq` is bumped with a
/// wrapping add, so any change (including the `u64::MAX → 0` wrap) counts as "a
/// host event landed." When the seq is unchanged the roster read is
/// authoritative, and `Some` carries exactly the participants flagged `is_host`.
fn resolve_host_set_from_roster(
    parts: Vec<videocall_meeting_types::responses::ParticipantStatusResponse>,
    seq_at_start: u64,
    current_seq: u64,
) -> Option<HashSet<String>> {
    if current_seq != seq_at_start {
        return None;
    }
    Some(
        parts
            .into_iter()
            .filter(|p| p.is_host)
            .map(|p| p.user_id)
            .collect(),
    )
}

/// Replace `host_set_signal` with the current hosts from the `/participants`
/// roster — the source of truth at (re)connect time, since a HOST_GRANTED/
/// HOST_REVOKED event can be swallowed during a reconnect. Skips the
/// replace if a live host event landed during the fetch (`host_event_seq`),
/// which is fresher than the roster read.
fn reseed_host_set_from_roster(
    meeting_id: String,
    mut host_set_signal: Signal<HashSet<String>>,
    host_event_seq: Signal<u64>,
) {
    let seq_at_start = *host_event_seq.peek();
    wasm_bindgen_futures::spawn_local(async move {
        match crate::constants::meeting_api_client() {
            Ok(client) => match client.list_participants(&meeting_id).await {
                Ok(parts) => {
                    if let Some(hosts) =
                        resolve_host_set_from_roster(parts, seq_at_start, *host_event_seq.peek())
                    {
                        host_set_signal.set(hosts);
                    }
                }
                Err(e) => log::debug!("host-set roster seed failed: {e}"),
            },
            Err(e) => log::debug!("meeting_api_client error during host-set seed: {e}"),
        }
    });
}

/// Show the one-shot host-change toast and dismiss it ~6s later. NATS-handler
/// safe; a host change re-renders in place (no remount), so the toast is driven
/// directly via these signals.
fn show_host_change_toast(
    message: &str,
    mut toast: Signal<Option<String>>,
    mut timer: Signal<Option<gloo_timers::callback::Timeout>>,
) {
    toast.set(Some(message.to_string()));
    timer.set(None);
    timer.set(Some(gloo_timers::callback::Timeout::new(
        6_000,
        move || {
            toast.set(None);
            timer.set(None);
        },
    )));
}

/// Where a settings deep link should land. The Performance controls moved out of
/// the Settings modal into the Diagnostics drawer (#1131 unify), so an incoming
/// `"performance"` section opens the DRAWER, not the modal; every other section
/// (or none) opens the modal as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsDeepLink {
    /// Open the right-side Diagnostics drawer (the new home of the Performance
    /// controls). Do NOT open the Settings modal.
    Drawer,
    /// Open the Settings modal (optionally on a requested tab).
    Modal,
}

/// Classify a requested settings-section string into where it should open. Pure
/// so the routing contract is host-testable. The match is case-sensitive,
/// mirroring `DeviceSettingsModal`'s own `initial_section` mapping.
pub(crate) fn classify_settings_deep_link(section: Option<&str>) -> SettingsDeepLink {
    match section {
        Some("performance") => SettingsDeepLink::Drawer,
        _ => SettingsDeepLink::Modal,
    }
}

/// Read the device capability score the console-log preamble already benchmarked
/// and stashed on `window.__videocall_capability_score` (issue #1558).
///
/// This is a cheap JS property read — it does NOT re-run the 100 ms capability
/// benchmark (`videocall_capability_score()` does, and must never be called on
/// the ~1 Hz budget loop). Returns `None` when the value is absent or not a
/// finite non-negative number, in which case the protective-mode low-cap trigger
/// stays conservatively off.
fn read_cached_capability_score() -> Option<u32> {
    let window = web_sys::window()?;
    let val =
        js_sys::Reflect::get(&window, &JsValue::from_str("__videocall_capability_score")).ok()?;
    let score = val.as_f64()?;
    if score.is_finite() && score >= 0.0 {
        Some(score as u32)
    } else {
        None
    }
}

/// Resolve the action-bar's actual axis gap (column-gap when horizontal,
/// row-gap when vertical) from computed style instead of hard-coding a value
/// that drifts whenever the CSS changes. Falls back to the design default
/// (1.2rem ≈ 19.2px) when the lookup fails.
fn read_nav_axis_gap_px(nav: &web_sys::Element, is_vertical: bool) -> f64 {
    let prop = if is_vertical { "row-gap" } else { "column-gap" };
    web_sys::window()
        .and_then(|w| w.get_computed_style(nav).ok().flatten())
        .and_then(|s| s.get_property_value(prop).ok())
        .and_then(|s| s.trim().trim_end_matches("px").parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(19.2)
}

// ─── Shared `.glass-select-menu` keyboard-navigation helpers ────────────────
// WCAG 2.1.1 keyboard equivalents for the dock-position dropdown that houses
// Bottom/Left/Right, autohide, Customize, and Reset to Default. Before these
// helpers the menu options were `<div role="option">` with only `onclick` —
// a keyboard user could not enter customize mode or reset the bar at all.
// The helpers are reusable across any wrapper that hosts a
// `.glass-select-menu` with `.glass-select-option` children (and optional
// `.glass-select-separator` interlopers, which are naturally skipped by the
// `.glass-select-option` class selector).

/// Focus the element with the given `id`. Silently no-op if the id is not
/// present in the document or the element is not focusable — this lets
/// callers restore focus to their trigger without knowing whether the
/// trigger was swapped out by the action they just fired (e.g. entering
/// customize mode replaces the dock trigger with the Done button).
fn focus_element_by_id(id: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}

/// True when a click originated inside the action bar (`.video-controls-container`).
/// The in-meeting `#main-container` background-click handler uses this to leave the
/// side panels (peer list, diagnostics) open when the click landed on an action-bar
/// control — the panel toggles themselves, mic, camera, etc. — rather than on the
/// video grid. Any failure to resolve the click target defaults to `false`
/// ("not in the action bar"), the safe default that lets a genuine background click
/// still light-dismiss the panels. Uses the file's established
/// `target().closest(".video-controls-container")` idiom.
fn click_within_action_bar(evt: &MouseEvent) -> bool {
    evt.as_web_event()
        .target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .and_then(|el| el.closest(".video-controls-container").ok().flatten())
        .is_some()
}

/// Which side panel an Escape keypress should close. Precedence is diagnostics
/// FIRST, then the peer list: diagnostics is the visually topmost drawer (mobile
/// z-index 9301 vs the peer list's 9300), so Escape peels the top layer first —
/// the same popover-before-panel layering the dock/density handlers follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscCloseTarget {
    Diagnostics,
    PeerList,
}

impl EscCloseTarget {
    /// The id of the action-bar toggle button that owns this panel. Escape-close
    /// restores focus here (WAI-ARIA APG disclosure pattern) so keyboard focus
    /// never falls back to `<body>`. These strings are the fn's contract with the
    /// rendered `id:` attributes on `PeerListButton` / `DiagnosticsButton`; the
    /// rendered-id side of that contract is guarded by the e2e `activeElement`
    /// assertions in `popup-layering.spec.ts`.
    fn trigger_id(self) -> &'static str {
        match self {
            EscCloseTarget::Diagnostics => "diagnostics-trigger",
            EscCloseTarget::PeerList => "peer-list-trigger",
        }
    }
}

/// Decide which side panel Escape should close, given each panel's open state.
/// Returns `None` when neither panel is open — Escape is then a no-op here and the
/// popover handlers (density / mock-peers / dock menu) keep today's behavior.
/// Diagnostics takes precedence over the peer list when both are open (topmost
/// drawer closes first).
fn esc_panel_close_target(diagnostics_open: bool, peer_list_open: bool) -> Option<EscCloseTarget> {
    if diagnostics_open {
        Some(EscCloseTarget::Diagnostics)
    } else if peer_list_open {
        Some(EscCloseTarget::PeerList)
    } else {
        None
    }
}

/// Focus the first (`last=false`) or last (`last=true`) `.glass-select-option`
/// inside the `.glass-select-menu` that lives under `wrapper_selector`
/// (e.g. `.dock-position-wrapper`). Used by trigger's ArrowDown/ArrowUp and
/// by the menu's Home/End handlers.
fn focus_glass_option_at(wrapper_selector: &str, last: bool) {
    let selector = format!("{wrapper_selector} .glass-select-menu .glass-select-option");
    if let Some(nodes) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector_all(&selector).ok())
    {
        let n = nodes.length();
        if n == 0 {
            return;
        }
        let idx = if last { n - 1 } else { 0 };
        if let Some(node) = nodes.item(idx) {
            if let Ok(html) = node.dyn_into::<web_sys::HtmlElement>() {
                let _ = html.focus();
            }
        }
    }
}

/// Move focus to the next (`delta = +1`) or previous (`delta = -1`)
/// `.glass-select-option` relative to the currently-focused element.
/// Separators are skipped by walking sibling-by-sibling and matching on the
/// option class. Focus wraps at the ends (APG listbox convention). No-op if
/// nothing in the document has focus or the active element isn't inside a
/// menu.
fn focus_glass_option_relative(delta: i32) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(active) = doc.active_element() else {
        return;
    };
    let step_forward = delta >= 0;
    let mut cursor = if step_forward {
        active.next_element_sibling()
    } else {
        active.previous_element_sibling()
    };
    while let Some(el) = cursor {
        let next = if step_forward {
            el.next_element_sibling()
        } else {
            el.previous_element_sibling()
        };
        if el.class_list().contains("glass-select-option") {
            if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
                let _ = html.focus();
            }
            return;
        }
        cursor = next;
    }
    // Reached the end without finding another option → wrap to first/last.
    if let Some(parent) = active.parent_element() {
        if let Ok(nodes) = parent.query_selector_all(".glass-select-option") {
            let n = nodes.length();
            if n == 0 {
                return;
            }
            let idx = if step_forward { 0 } else { n - 1 };
            if let Some(node) = nodes.item(idx) {
                if let Ok(html) = node.dyn_into::<web_sys::HtmlElement>() {
                    let _ = html.focus();
                }
            }
        }
    }
}

/// Focus the first element matching `selector`.
fn focus_by_selector(selector: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector(selector).ok().flatten())
    {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}

/// Focus the first actionable slot button in the action bar. Prefer Mic
/// (`data-slot="mic"`) so keyboard navigation starts at the default first
/// user-facing control; fall back to the first enabled (or first present)
/// slot button if Mic is unavailable.
fn focus_first_action_bar_button() {
    let selectors = [
        ".video-controls-container .action-bar-slot-wrapper[data-slot=\"mic\"] > button.video-control-button:not([disabled])",
        ".video-controls-container .action-bar-slot-wrapper[data-slot] > button.video-control-button:not([disabled])",
        ".video-controls-container .action-bar-slot-wrapper[data-slot=\"mic\"] > button.video-control-button",
        ".video-controls-container .action-bar-slot-wrapper[data-slot] > button.video-control-button",
    ];
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    for selector in selectors {
        if let Ok(Some(el)) = doc.query_selector(selector) {
            if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
                let _ = html.focus();
                return;
            }
        }
    }
}

fn is_action_bar_slot_visible(
    slot: ActionBarSlot,
    customize_mode: bool,
    ios_device: bool,
    has_screen_share: bool,
    is_owner: bool,
) -> bool {
    match slot {
        ActionBarSlot::ScreenShare => customize_mode || !ios_device,
        ActionBarSlot::DensityMode => customize_mode || !has_screen_share,
        ActionBarSlot::MeetingOptions => is_owner,
        _ => true,
    }
}

fn visible_action_bar_slots(
    slots: &[ActionBarSlot],
    customize_mode: bool,
    ios_device: bool,
    has_screen_share: bool,
    is_owner: bool,
) -> Vec<ActionBarSlot> {
    slots
        .iter()
        .copied()
        .filter(|slot| {
            is_action_bar_slot_visible(
                *slot,
                customize_mode,
                ios_device,
                has_screen_share,
                is_owner,
            )
        })
        .collect()
}

fn merge_visible_action_bar_slots(
    full_slots: &[ActionBarSlot],
    reordered_visible_slots: &[ActionBarSlot],
    customize_mode: bool,
    ios_device: bool,
    has_screen_share: bool,
    is_owner: bool,
) -> Vec<ActionBarSlot> {
    let mut visible_idx = 0usize;
    full_slots
        .iter()
        .copied()
        .map(|slot| {
            if is_action_bar_slot_visible(
                slot,
                customize_mode,
                ios_device,
                has_screen_share,
                is_owner,
            ) {
                let mapped_slot = reordered_visible_slots
                    .get(visible_idx)
                    .copied()
                    .unwrap_or(slot);
                visible_idx += 1;
                mapped_slot
            } else {
                slot
            }
        })
        .collect()
}

/// Returns true if the keyboard event carries an "activate this option" key:
/// Enter or Space. On a `<div role="option">` the browser does NOT synthesise
/// a click for either key (unlike a `<button>`), so callers must handle the
/// activation themselves.
fn is_option_activate_key(evt: &Event<KeyboardData>) -> bool {
    let key = evt.key();
    key == Key::Enter || matches!(&key, Key::Character(s) if s == " ")
}

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
    let screen_share_toast_state: Signal<Option<ScreenShareToastState>> = use_signal(|| None);
    let screen_share_toast_timer: Signal<Option<Timeout>> = use_signal(|| None);

    let mut mic_enabled = use_signal(|| false);
    let mut video_enabled = use_signal(|| false);
    let mut peer_list_open = use_signal(|| false);
    let mut diagnostics_open = use_signal(|| false);
    // Latch: set true the first time the Diagnostics drawer is opened, never
    // reset. Once the drawer has been opened at least once, CLOSING it keeps a
    // lightweight `#diagnostics-sidebar` placeholder in the DOM (without the
    // `visible` class) instead of unmounting the element entirely — symmetric with
    // `#peer-list-container`, whose outer div always renders. This is what lets the
    // both-open close flow observe the drawer LOSE `visible` rather than vanish
    // (a `:not(.visible)` assertion can't match an element that no longer exists).
    // Gating the placeholder on "ever opened" preserves the never-opened contract
    // (the element must NOT exist until the drawer is first opened). The heavy
    // `Diagnostics` component is still only mounted while actually open, so no
    // diagnostics work runs for the closed placeholder. (issue 1296 both-open close)
    let mut diagnostics_was_opened = use_signal(|| false);
    // Drawer width state. Both drawers are overlay-only: they float over the
    // tiles and never reflow the grid. Their widths are drag-resizable and
    // persisted to localStorage. Widths are clamped on load in case a value
    // persisted by an older/incompatible release no longer satisfies the current
    // min/max.
    let mut left_width = use_signal(|| {
        load_f64("vc_drawer_left_width", 320.0).clamp(DRAWER_MIN_WIDTH, DRAWER_MAX_ABS)
    });
    let mut right_width = use_signal(|| {
        load_f64("vc_drawer_right_width", 560.0).clamp(DRAWER_MIN_WIDTH, DRAWER_MAX_ABS)
    });
    let mut resizing_drawer = use_signal(|| ResizingDrawer::None);
    // Viewport width snapshotted at drag-start so the move handler does NOT
    // re-read `window().inner_width()` on every mousemove. (#1296)
    let mut drag_start_vw = use_signal(|| 0.0f64);
    // Non-reactive rAF coalescing stash for drawer resize (#1296 perf).
    // Holds the latest pointer client_x and a "rAF scheduled" flag so a fast
    // drag writes the width signal at most ONCE per painted frame instead of
    // once per coalesced pointermove (each width write re-runs the whole
    // meeting-view body + every keyed PeerTile, stealing the wasm main thread
    // from live video decode). use_hook => one stable instance across renders;
    // a signal here would defeat the throttle by triggering a re-render itself.
    let left_raf_x: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));
    let left_raf_pending: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));
    let right_raf_x: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));
    let right_raf_pending: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));
    // Per-drawer "a real move happened this drag" flag. Reset to false on
    // pointerdown / on_resize_start; set true the first time a pointermove
    // arrives. The pointerup / on_resize_end flush is GATED on this so a
    // no-move interaction (accidental click or focus tap on the handle) does
    // NOT overwrite the current width with the default stash value (0.0 → min
    // on the left, vw*0.5 on the right) and does NOT persist that bogus value.
    // Resetting on drag-start also stops a PRIOR drag's stash from leaking into
    // a later no-move click. (#1296 regression fix)
    let left_raf_valid: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));
    let right_raf_valid: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));
    let mut mock_peers_open = use_signal(|| false);
    let mut controls_visible = use_signal(|| true);
    let mut controls_expanded = use_signal(|| true);
    let mut dock_position: Signal<DockPosition> = use_signal(load_dock_position);
    let mut dock_menu_open = use_signal(|| false);
    let mut autohide_enabled = use_signal(load_dock_autohide);
    let mut customize_mode = use_signal(|| false);
    // Text pushed into the customize-mode aria-live region on keyboard reorder
    // (WCAG 2.1.1). Empty when there is nothing new to announce; a fresh string
    // — even the SAME logical position revisited — must be produced so screen
    // readers re-announce it. Reset to empty when customize mode exits so the
    // region is silent between edit sessions.
    let mut action_bar_announce: Signal<String> = use_signal(String::new);
    // Action-bar layout is persisted as a (visible_slots, hidden_slots) pair so
    // that user-removed slots stay removed across reloads. `load_action_bar_layout`
    // returns the migrated pair; both signals must be kept in lock-step on every
    // mutation (drag-reorder, remove button, reset-to-default).
    let (initial_slots, initial_hidden) = load_action_bar_layout();
    let mut action_bar_slots: Signal<Vec<ActionBarSlot>> = use_signal(|| initial_slots);
    let mut action_bar_hidden: Signal<Vec<ActionBarSlot>> = use_signal(|| initial_hidden);
    let mut dragging_slot: Signal<Option<ActionBarSlot>> = use_signal(|| None);
    let mut drag_pointer_x: Signal<f64> = use_signal(|| 0.0);
    let mut drag_pointer_y: Signal<f64> = use_signal(|| 0.0);
    let mut drag_insertion_idx: Signal<Option<usize>> = use_signal(|| None);
    let drag_slot_size: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));
    let drag_grab_dx: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));
    let drag_grab_dy: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));
    let mut drag_started: Signal<bool> = use_signal(|| false);
    let mut drag_orig_layout: Signal<Vec<ActionBarSlot>> = use_signal(Vec::new);
    let drag_pointer_id: Rc<Cell<i32>> = use_hook(|| Rc::new(Cell::new(0)));
    let drag_start_x: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));
    let drag_start_y: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));
    let drag_nav_left: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));
    let drag_nav_top: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));

    // Tear down any in-flight drag state when customize mode exits. Without this,
    // a pointer release missed by the nav (capture never set, off-nav release,
    // mode toggled mid-drag) would leave `dragging_slot = Some(..)` and wedge the
    // floating preview on next entry to customize mode.
    {
        let drag_slot_size_reset = drag_slot_size.clone();
        let drag_pointer_id_reset = drag_pointer_id.clone();
        let drag_grab_dx_reset = drag_grab_dx.clone();
        let drag_grab_dy_reset = drag_grab_dy.clone();
        use_effect(move || {
            if !customize_mode() {
                dragging_slot.set(None);
                drag_started.set(false);
                drag_insertion_idx.set(None);
                drag_slot_size_reset.set(0.0);
                drag_pointer_id_reset.set(0);
                drag_grab_dx_reset.set(0.0);
                drag_grab_dy_reset.set(0.0);
                // Silence the keyboard-reorder live region between edit
                // sessions so a stale message doesn't get re-announced when
                // customize mode is re-entered.
                action_bar_announce.set(String::new());
            }
        });
    }

    // Always seed keyboard focus on customize-mode entry at the first
    // actionable slot button (Mic/Sound by default). This runs on every
    // false->true transition so re-entering customize mode is consistent,
    // independent of which menu activation path was used.
    use_effect(move || {
        if customize_mode() {
            Timeout::new(0, || {
                focus_first_action_bar_button();
            })
            .forget();
        }
    });

    // DOM reorder shim removed: slots are now rendered in
    // `action_bar_slots` order directly, so DOM order IS visual order
    // and keyboard Tab order follows naturally without post-render
    // DOM manipulation.
    let encoder_settings = use_signal(|| None::<String>);
    // Last peer count logged by the meeting-view render log below — lets that
    // log fire only on changes (edge-trigger) instead of every re-render.
    let mut last_logged_peer_count = use_signal(|| None::<usize>);
    let mut debug_peer_count = use_signal(|| 0u32);
    // Per-peer speech priority: session_id → last-spoke timestamp (ms).
    // Peers that spoke recently sort higher in the grid.
    let mut peer_speech_priority: Signal<HashMap<String, f64>> = use_signal(HashMap::new);
    // Per-peer join time: session_id → first-seen timestamp (ms).
    // Used as fallback ordering when no speech data exists.
    let mut peer_join_time: Signal<HashMap<String, f64>> = use_signal(HashMap::new);
    let mut density_mode: Signal<DensityMode> = use_signal(load_density_mode);
    let mut density_open = use_signal(|| false);
    // --- Adaptive decode budget (issue #987, task 1a.3) ---
    // Manual override for the adaptive controller. `Auto` runs the loop;
    // `Fixed(n)` is a hard override that bypasses the loop entirely. Task 1a.5
    // (settings UI) mutates this through the `DecodeBudgetCtx` provided below.
    // Loaded first so the initial `decode_budget_cap` can honour a persisted
    // `Fixed(n)` immediately (no first-render flash; HCL #987 review FIX 2).
    let decode_budget_override = use_signal(load_decode_budget_override);
    // `decode_budget_cap` is the control-loop-owned cap: the maximum number of
    // video tiles the layout may decode AFTER the loop has measured real
    // pressure. It is NOT the actuator on its own — see the render-side
    // `effective_cap` derivation (HCL #987 review FIX 1 + FIX 2).
    //
    // Cap-ownership model (replaces the old one-shot seed latches):
    //   - `Fixed(n)`: the render-side `effective_cap` clamps `n` directly, so a
    //     manual "show N tiles" choice takes effect on the NEXT render with NO
    //     dependency on a `client_render_fps` diagnostics event. The loop also
    //     clamps `decode_budget_cap` as a backstop / state bookkeeping.
    //   - `Auto`, never pressured: `effective_cap` tracks the live natural tile
    //     count EXACTLY (== `total_tiles`, ∩ CANVAS_LIMIT). A healthy machine
    //     therefore shows ALL natural tiles from the first render and keeps
    //     showing every peer that joins later (staggered joins) with ZERO
    //     avatars — independent of any FPS-event timing. `decode_budget_cap` is
    //     NOT consulted on this path; the loop keeps `state.cap` synced to
    //     `natural` so the first down-step starts from the displayed value.
    //   - `Auto`, pressured: once the loop has measured sustained pressure and
    //     taken a down-step, `decode_budget_pressured` latches true and the loop
    //     becomes the sole owner of `decode_budget_cap`, applying its
    //     conservative anti-oscillation growth. `effective_cap` then reads
    //     `decode_budget_cap`.
    let initial_cap = match *decode_budget_override.peek() {
        DecodeBudgetOverride::Fixed(n) => n.clamp(MIN_CAP, CANVAS_LIMIT),
        // Issue #1466: `All` is count-free — the render-side `effective_cap`
        // returns natural-capped regardless of `decode_budget_cap`, so this
        // initial seed is not load-bearing for All. Seed at CANVAS_LIMIT
        // (decode all the layout could show) so any incidental reader sees the
        // permissive value rather than the MIN_CAP floor.
        DecodeBudgetOverride::All => CANVAS_LIMIT,
        DecodeBudgetOverride::Auto => MIN_CAP,
    };
    let mut decode_budget_cap = use_signal(|| initial_cap);
    // "Has a pressure-driven down-step occurred this Auto session?" (HCL #987
    // review FIX 1 + FIX 2). While `false`, an Auto cap tracks the natural tile
    // count exactly (render-side `effective_cap`), so a capable machine shows
    // every peer — including staggered joins — with no avatars and no dependence
    // on the ~1 Hz control-loop cadence. The control loop latches this `true`
    // the first time `decide_step` returns `Down`, after which the loop owns the
    // cap with its conservative growth. The latch stays set for the rest of the
    // Auto session (a machine that has demonstrated it can struggle stays in
    // conservative adaptive mode — safe, and only affects machines that genuinely
    // hit pressure). It is reset to `false` on a Fixed -> Auto transition so
    // resuming Auto re-reveals all natural tiles immediately.
    let mut decode_budget_pressured = use_signal(|| false);
    // Previous value of `decode_budget_override`, tracked in render scope so a
    // render-driven `use_effect` can detect a Fixed -> Auto transition and reset
    // the pressured latch IMMEDIATELY, with no dependence on the ~1 Hz control
    // loop (HCL #987 review). Seeded to the current override so the very first
    // render observes no transition. Read via `.peek()` inside the effect (never
    // reactively) so writing it back cannot self-retrigger the effect.
    let mut prev_override = use_signal(|| *decode_budget_override.peek());
    // The uncapped layout tile count (`total_tiles`), republished from render
    // so the async control loop can pass it to `decide_step` as `natural_count`
    // and never raise the cap above what the layout would actually show.
    // Seeded at MIN_CAP (matching the Auto cap seed pre-join); render overwrites
    // it with the live `total_tiles` on the first frame, after which the loop
    // seeds/tracks the cap up to it.
    let mut decode_budget_natural = use_signal(|| MIN_CAP);
    // Issue #1558: protective-mode report. The decode-budget control loop is the
    // sole WRITER (it latches protective mode and computes the encoder send-layer
    // self-shed ceiling); `Host` is the consumer (it applies the ceiling to the
    // local encoders, composed with the user's persisted preference). Provided as
    // a context below so the child `Host` can read it.
    let mut protective_mode_report = use_signal(crate::context::ProtectiveModeReport::default);
    // Shared "is the decode-budget banner ACTUALLY on screen right now" flag,
    // owned here so the banner (writer) and the persistent paused pill (reader)
    // — sibling children below — agree on a single source of truth. The banner
    // publishes its TRUE effective visibility (`damper_visible && !dismissed &&
    // avatar_count > 0`) into this; the pill reads it to stay exactly mutually
    // exclusive with the banner, including after the user DISMISSES the banner
    // while tiles are still paused (#1142 Gap 1) — a case the old shadow-damper
    // proxy could not observe.
    let banner_on_screen = use_signal(|| false);
    // Device-class decode-tile ceiling (issue #1286 / #1289). Computed ONCE per
    // mount (it depends only on the platform + core count, neither of which
    // changes within a session) and reused by BOTH `effective_cap` call sites
    // (render + telemetry) and the control loop's growth clamp. `Some(n)` on
    // iOS (mobile WebKit) where the absence of any valid main-thread-saturation
    // signal means an unbounded cap can collapse the device; `None` elsewhere
    // (no extra ceiling). Gated on `is_ios()` specifically, NOT all WebKit:
    // desktop Safari on capable hardware should not be clamped to a phone-class
    // budget, and Part A's longtask-blind conservatism already prevents the
    // unjustified growth there.
    let device_decode_ceiling: Option<usize> = use_hook(|| {
        ios_decode_tile_ceiling(
            is_ios(),
            videocall_client::utils::hardware_concurrency_cores(),
        )
    });
    // Last decode-budget snapshot published to the diagnostics bus (for the
    // HEALTH packet, #987 P3): (effective_cap, natural_capped, pressured,
    // is_fixed, fixed_n). Tracked so the publisher effect only emits a bus
    // event when the decision actually moves, never per unrelated render. Read
    // via `.peek()` inside that effect (never reactively) so writing it back
    // cannot self-retrigger the effect.
    let mut prev_db_snapshot = use_signal(|| (MIN_CAP, MIN_CAP, false, false, 0usize));
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
                if autohide_enabled() {
                    controls_expanded.set(false);
                }
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
                if autohide_enabled() {
                    controls_visible.set(false);
                }
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
                if autohide_enabled() {
                    controls_expanded.set(false);
                }
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
                if autohide_enabled() {
                    controls_visible.set(false);
                }
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
            if autohide_enabled() {
                controls_expanded.set(false);
            }
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
            if autohide_enabled() {
                controls_visible.set(false);
            }
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

    use_effect(move || {
        if !autohide_enabled() {
            controls_visible.set(true);
            controls_expanded.set(true);
        }
    });

    let mut device_settings_open = use_signal(|| false);
    let mut device_settings_initial_section: Signal<Option<String>> = use_signal(|| None);
    let mut device_settings_generation = use_signal(|| 0u32);
    // In-call Meeting Options panel (host-only). Lets the owner change meeting
    // options live without leaving the call.
    let mut meeting_options_open = use_signal(|| false);
    // Host publishes its live diagnostics reader handle here once on mount, so the
    // Diagnostics sidebar (a sibling of Host that can't reach the encoders) can
    // render the "Simulcast layers" section from the live SEND snapshots. (#1095)
    let diagnostics_reader_sink: Signal<Option<DiagnosticsReader>> = use_signal(|| None);
    // Host also publishes its Performance controls handle here, so the Diagnostics
    // drawer can mount the `PerformanceSettingsPanel` (sliders/Auto/meters) — the
    // panel moved out of the Settings modal into the drawer (#1131 unify).
    let perf_controls_sink: Signal<Option<PerfControlsHandle>> = use_signal(|| None);
    // Issue 1768: "Show media metrics on tiles" preference. Seeded from
    // localStorage (default off), toggled by the diagnostics-drawer checkbox, and
    // read by every PeerTile via `MediaMetricsOverlayCtx`. A single shared signal
    // so toggling the checkbox shows/hides every tile's overlay reactively.
    let media_metrics_overlay_enabled: Signal<bool> =
        use_signal(|| load_bool(MEDIA_METRICS_OVERLAY_KEY, false));

    // Deep-link interception (#1131 unify): a requested `"performance"` section no
    // longer has a Settings tab — the Performance controls live in the Diagnostics
    // drawer now. Catch it BEFORE the modal opens: route to the drawer and clear
    // the requested section so the (closed) modal doesn't keep a dangling
    // "performance" target. Other sections fall through untouched. The classifier
    // is the pure `classify_settings_deep_link` (host-tested); this effect only
    // applies its verdict to the open-state signals.
    use_effect(move || {
        if classify_settings_deep_link(device_settings_initial_section().as_deref())
            == SettingsDeepLink::Drawer
        {
            device_settings_open.set(false);
            device_settings_initial_section.set(None);
            diagnostics_open.set(true);
        }
    });

    // Latch `diagnostics_was_opened` the first time the drawer opens, via ANY path
    // (toolbar button, deep-link redirect above, etc.). Reading `diagnostics_open`
    // here subscribes this effect to it; the write is guarded so it only fires once
    // (and never resets), so it cannot loop. After this latches, closing the drawer
    // renders the persistent placeholder instead of unmounting. (issue 1296)
    use_effect(move || {
        if diagnostics_open() && !diagnostics_was_opened() {
            diagnostics_was_opened.set(true);
        }
    });

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
    // Phase 6 render-storm fix: shared "bump pending?" flag for the
    // `peer_speaking` event handler. Lives on a `use_hook` so it survives
    // across renders. See `schedule_throttled_bump`.
    let peer_list_bump_pending: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));
    let mut screen_share_version = use_signal(|| 0u32);
    let media_access_granted = use_signal(|| false);
    let mut mic_error = use_signal(|| None::<MediaErrorState>);
    let mut video_error = use_signal(|| None::<MediaErrorState>);
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
    // Read-and-cleared once per background auto-retry tick. When a background
    // tick's `on_result` reports another failure, we suppress re-popping the
    // blocking modal (it's already been shown for the initial failure); only a
    // user-initiated request or the first failure surfaces it.
    //
    // INVARIANT: this flag is only read-and-cleared inside the
    // `session_loaded() || connecting()` branch of `on_result`. The background
    // retry loop fires for ANY `DeviceInUse` error (see `should_auto_retry`),
    // both pre-join and post-join, so a retry tick's `on_result` can land
    // pre-join. The flag-clearing stays safe there: a background retry result
    // pre-join lands in the `else if !join_requested()` (pre-join preview)
    // branch, which neither reads/clears this flag NOR pops the modal — so a
    // flag left `true` by a pre-join retry tick is inert (nothing pre-join
    // consumes it) and no modal-suppression is needed there (that branch never
    // pops the modal in the first place). When the user dismisses the failure
    // modal into the meeting, control moves to the `session_loaded() ||
    // connecting()` branch, which clears the flag and OR-s it with the
    // single-device-probe inference, so in-meeting modal suppression remains
    // correct regardless of the flag's stale pre-join value.
    let is_background_retry: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));
    // True only when the user explicitly clicked Join/Start (vs. granting
    // permission just to preview devices). Gates the auto-connect in the
    // MediaDeviceAccess callback so a preview-permission grant does NOT join
    // the meeting. (issue #959)
    let mut join_requested = use_signal(|| false);

    // ── Pre-join device preview state (issue #959) ─────────────────────
    // Device lists + selections for the pre-join screen. Populated once
    // getUserMedia permission is granted (labels are empty before that).
    let prejoin_cameras = use_signal(Vec::<web_sys::MediaDeviceInfo>::new);
    let prejoin_microphones = use_signal(Vec::<web_sys::MediaDeviceInfo>::new);
    let prejoin_speakers = use_signal(Vec::<web_sys::MediaDeviceInfo>::new);
    let mut prejoin_selected_camera = use_signal(|| None::<String>);
    let mut prejoin_selected_mic = use_signal(|| None::<String>);
    let mut prejoin_selected_speaker = use_signal(|| None::<String>);
    // Restore the persisted on/off choices so they round-trip across visits.
    let mut prejoin_camera_on = use_signal(load_preferred_camera_on);
    let mut prejoin_mic_on = use_signal(load_preferred_mic_on);
    // setSinkId support is fixed per-browser; probe once.
    let speaker_supported = use_hook(html_media_set_sink_id_supported);

    // The imperative preview engine owns the camera + mic preview hardware and
    // drives the level meter. The meter is updated by DIRECT DOM writes (no
    // Dioxus signal), so it never re-diffs the card per frame. One instance for
    // the component's lifetime.
    let preview_engine = use_hook(|| {
        use crate::components::pre_join_settings_card::{
            PREVIEW_MIC_METER_FILL_ID, PREVIEW_MIC_METER_ID, PREVIEW_VIDEO_ID,
        };
        PreviewEngine::new(
            PREVIEW_VIDEO_ID,
            PREVIEW_MIC_METER_ID,
            PREVIEW_MIC_METER_FILL_ID,
        )
    });

    // Pre-join device list (separate from the in-meeting Host's list). Loaded
    // after permission is granted; restores persisted device-id selections.
    let prejoin_devices: Rc<RefCell<MediaDeviceList>> =
        use_hook(|| Rc::new(RefCell::new(MediaDeviceList::new())));

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
    let show_muted_toast: Signal<bool> = use_signal(|| false);
    let toast_timer: Signal<Option<gloo_timers::callback::Timeout>> = use_signal(|| None);
    let show_video_off_toast: Signal<bool> = use_signal(|| false);
    let video_off_toast_timer: Signal<Option<gloo_timers::callback::Timeout>> = use_signal(|| None);
    let peer_display_name_version = use_signal(|| 0u32);

    // Host set: the `user_id`(s) currently holding host (single-host, so ≤1).
    // Seeded ONLY from `is_owner` (our own /status flag), NOT from `host_user_id`
    // — that's the meeting CREATOR and goes stale once host is transferred away
    // (seeding it would paint a wrong crown on the creator). Other peers' current
    // host is filled in by the `/participants` roster seed below and kept live by
    // HOST_GRANTED/HOST_REVOKED. Consumed by `peer_list` / `canvas_generator`.
    let host_set_signal: Signal<std::collections::HashSet<String>> = {
        let user_id = user_id.clone();
        use_signal(move || {
            let mut set = std::collections::HashSet::new();
            if is_owner {
                if let Some(uid) = user_id.clone() {
                    set.insert(uid);
                }
            }
            set
        })
    };
    // Provided by `MeetingPage`. Bumping this nonce re-fetches our participant
    // status and remounts this component, so a freshly granted/revoked host gets
    // a rebuilt media client with the correct `is_owner` and a fresh room token.
    // `None` when rendered without the provider (e.g. isolated component tests).
    let host_refresh_nonce = try_use_context::<HostRefreshNonceCtx>();

    // Monotonic counter bumped on every HOST_GRANTED/HOST_REVOKED. The roster
    // seed below snapshots it before its async fetch and skips the overwrite if a
    // host event landed meanwhile (live events are fresher) — keeping the replace
    // race-free.
    let host_event_seq: Signal<u64> = use_signal(|| 0u64);

    // Seed the host set from the `/participants` roster so the current host shows
    // a "(Host)" for everyone, including late joiners and rejoins after a
    // transfer — live events only cover changes seen while connected, so the
    // roster is the source of truth at (re)connect time. Replaces the set
    // wholesale (self-correcting), but skips the replace when a host event arrived
    // during the fetch (see `host_event_seq`). Re-runs when `host_refresh_nonce`
    // bumps (after our own host change).
    {
        let meeting_id = id.clone();
        use_effect(move || {
            // Track the nonce so a self host-change re-seeds from the roster.
            let _ = host_refresh_nonce.map(|c| c.0());
            if is_guest {
                return;
            }
            reseed_host_set_from_roster(meeting_id.clone(), host_set_signal, host_event_seq);
        });
    }

    // One-shot toast shown to the local user on grant, driven directly via
    // these signals from the HOST_GRANTED/HOST_REVOKED handler.
    // See `show_host_change_toast`.
    let host_change_toast: Signal<Option<String>> = use_signal(|| None);
    let host_change_toast_timer: Signal<Option<gloo_timers::callback::Timeout>> =
        use_signal(|| None);

    // Create the peer status map signal early so it can be captured by the
    // on_peer_removed callback inside use_hook below.
    let mut peer_status_map: PeerStatusMap = use_signal(HashMap::new);

    // Create the shared signal history map early so on_peer_removed can clean
    // up departed peers' histories. Provided as context alongside PeerStatusMap.
    let peer_signal_history_map: PeerSignalHistoryMap = use_signal(HashMap::new);

    // HCL bug #8 + #9: per-(peer, mode) signal-popup state map, owned by
    // the parent so PeerTile remounts (peer leaves, layout switches) do
    // not unmount the popup containers and accidentally close every
    // other peer's open popup. Cleaned up alongside
    // `peer_signal_history_map` when peers leave so we don't leak
    // entries for departed peers.
    let signal_popup_state_map: SignalPopupStateMap = use_signal(HashMap::new);

    // Per-tile crop state — created early so on_peer_removed can clean up.
    let cropped_tiles_signal: Signal<HashMap<String, bool>> = use_signal(HashMap::new);

    // Read transport preference from context BEFORE use_hook (hooks must not
    // be called inside the hook closure).
    let transport_pref_ctx = use_context::<TransportPreferenceCtx>();
    let transport_pref = (transport_pref_ctx.0)();

    // Create the appearance settings signal on_peer_joined / on_peer_left callbacks
    let appearance_settings = use_signal(load_appearance_settings_from_storage);

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

        // Apply user's transport preference. The `webtransport_enabled()`
        // read here is the *initial* value — runtime config may not have
        // loaded yet, in which case it returns `false` and `Auto` resolves
        // to a WS-only URL list. The reconnect path goes through the same
        // `current_transport_urls` helper so a later runtime-config load
        // can repopulate the WT list and recover the user from the
        // "stranded on a single server" state (discussion 562, Phase 7).
        let server_wt_enabled = crate::constants::webtransport_enabled().unwrap_or(false);
        let (effective_wt_enabled, websocket_urls, webtransport_urls) = current_transport_urls(
            &token,
            &initial_display_name,
            &id,
            transport_pref,
            server_wt_enabled,
        );

        log::info!(
            "DIOXUS-UI: Creating VideoCallClient for {} in meeting {}",
            initial_display_name,
            id
        );

        let client_for_reconnect: Rc<RefCell<Option<VideoCallClient>>> =
            Rc::new(RefCell::new(None));
        let client_for_kick = client_for_reconnect.clone();

        let user_id_for_display_name_changed = user_id.clone();
        // The local user's authoritative id (resolved like `opts.user_id` below),
        // so HOST_GRANTED/HOST_REVOKED can tell a self-change from an observer one.
        let user_id_for_host_events = user_id
            .clone()
            .unwrap_or_else(|| initial_display_name.clone());

        // Tracks the first `on_connected` so the reconcile below skips
        // the initial connect (already seeded by the mount effect).
        let host_reconcile_first_connect = Rc::new(Cell::new(true));

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
                // Initial room_token for console-log upload auth. The collector
                // re-receives a fresh token on every refresh via
                // refresh_room_token_callback below, so this only needs to seed
                // the value at connect time. Empty only in non-JWT dev builds:
                // the collector treats "" as no token, so the upload is then
                // unauthenticated — the server has no cookie fallback.
                let room_token_for_logs = room_token.clone();
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
                // re-sync host_set_signal from the roster on every
                // reconnect (see `host_reconcile_first_connect` above).
                let host_reconcile_first_connect = host_reconcile_first_connect.clone();
                let host_reconcile_meeting_id = id.clone();
                VcCallback::from(move |_| {
                    log::info!("DIOXUS-UI: Connection established");
                    let mut connection_error = connection_error;
                    let mut call_start_time = call_start_time;
                    let mut session_loaded = session_loaded;
                    connection_error.set(None);
                    call_start_time.set(Some(js_sys::Date::now()));
                    session_loaded.set(true);

                    // Re-seed host state from the roster on reconnect, so
                    // host controls drifted by a swallowed HOST_REVOKED are fixed
                    // here rather than on the next /status poll.
                    if should_reconcile_host_on_connect(&host_reconcile_first_connect, is_guest) {
                        reseed_host_set_from_roster(
                            host_reconcile_meeting_id.clone(),
                            host_set_signal,
                            host_event_seq,
                        );
                    }
                    // Activate console log collection if enabled in config.
                    if crate::constants::console_log_upload_enabled().unwrap_or(false) {
                        // Raise the WASM log level so uploaded logs capture
                        // detailed diagnostic output.
                        //
                        // PRECEDENCE (console-log perf fix):
                        //  - If the operator EXPLICITLY set `logLevel` in config.js
                        //    (ANY value, INCLUDING "info"), honour it as the ceiling
                        //    — e.g. `logLevel: "info"` or `"warn"` deliberately
                        //    REDUCES capture to cut per-packet log volume on a hot
                        //    deployment, and `logLevel: "trace"` opts INTO the
                        //    per-packet hot-path logs (which are emitted at trace!).
                        //  - Otherwise (key ABSENT → `log_level_explicit()` is None),
                        //    bump to Debug — the historical collection behaviour,
                        //    preserved so existing meeting analysis keeps working
                        //    unchanged. `Option<String>` lets us tell "absent" apart
                        //    from an explicit "info" (a defaulted String could not).
                        //
                        // We use Debug rather than Trace by default (per ticket
                        // #307) because Trace is prohibitively noisy in WASM and
                        // the genuine per-packet spam now lives at trace!, off by
                        // default even when collecting.
                        let effective_level = crate::constants::log_level_explicit()
                            .unwrap_or(crate::constants::COLLECTION_LOG_LEVEL_FALLBACK);
                        log::set_max_level(effective_level);
                        let dn = current_display_name();
                        // Hand the collector the room_token BEFORE setContext:
                        // setContext starts the upload timer, so the token must
                        // already be in place for the very first upload to carry
                        // `Authorization: Bearer`. Never log the token value.
                        set_console_log_auth_token(&room_token_for_logs);
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
                //
                // Phase 6 (cc7tp 2026-05-06): the `peer_list_version` bump
                // moved to `on_peers_removed_batch` below so that a 5-peer
                // watchdog cascade fires one re-render instead of five.
                // Per-peer cleanup of side-maps stays here — these are O(1)
                // and observers may rely on the per-peer fan-out.
                let mut map = peer_status_map;
                map.write().remove(&peer_id);
                // Also remove the departed peer's signal history so the shared
                // map does not grow unboundedly over long meetings.
                let mut hist_map = peer_signal_history_map;
                hist_map.write().remove(&peer_id);
                // HCL bug #8: drop only this peer's open signal-meter popup
                // entries; every other peer's popup state stays intact so
                // their popups remain visible across the parent re-render.
                let mut popup_map = signal_popup_state_map;
                popup_map.write().retain(|(pid, _mode), _| pid != &peer_id);
                let mut speech_map = peer_speech_priority;
                speech_map.write().remove(&peer_id);
                let mut jt_map = peer_join_time;
                jt_map.write().remove(&peer_id);
                let mut ct_map = cropped_tiles_signal;
                ct_map.write().remove(&peer_id);
                ct_map.write().remove(&format!("screen-share-{peer_id}"));
            })),
            on_peers_removed_batch: Some(VcCallback::from(move |peer_ids: Vec<String>| {
                // Phase 6 fix: bump `peer_list_version` exactly once per
                // removal pass, not once per dead peer. When the
                // PeerDecodeManager watchdog times out N peers in a single
                // tick (cc7tp incident: N=5), the per-peer
                // `on_peer_removed` callback still fires N times for
                // side-map cleanup, but the version-driven re-render
                // happens only once.
                if peer_ids.is_empty() {
                    return;
                }
                log::info!(
                    "Batched peer removal: {} peer(s) removed in one pass",
                    peer_ids.len()
                );
                let mut v = peer_list_version;
                let next = *v.peek() + 1;
                v.set(next);
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
                    let mut mic_enabled = mic_enabled;
                    let mut video_enabled = video_enabled;
                    let mut pending_mic_enable = pending_mic_enable;
                    let mut pending_video_enable = pending_video_enable;
                    let mut screen_share_state = screen_share_state;
                    meeting_start_time_server.set(Some(end_time_ms));
                    meeting_ended_message.set(Some(message));
                    mic_enabled.set(false);
                    video_enabled.set(false);
                    pending_mic_enable.set(false);
                    pending_video_enable.set(false);
                    screen_share_state.set(ScreenShareState::Idle);
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
            on_host_mute: Some(VcCallback::from({
                let self_uid = user_id_for_host_events.clone();
                move |_: ()| {
                    if host_set_signal.peek().contains(&self_uid) {
                        log::info!("HOST_MUTE: ignored — local user is currently a host");
                        return;
                    }
                    log::info!("HOST_MUTE: muting local microphone on host request");
                    let mut mic_enabled = mic_enabled;
                    let mut show_muted_toast = show_muted_toast;
                    let mut toast_timer = toast_timer;
                    mic_enabled.set(false);
                    show_muted_toast.set(true);
                    // Cancel any pending dismiss timer before scheduling a new one.
                    toast_timer.set(None);
                    toast_timer.set(Some(Timeout::new(6_000, move || {
                        let mut show_muted_toast = show_muted_toast;
                        let mut toast_timer = toast_timer;
                        show_muted_toast.set(false);
                        toast_timer.set(None);
                    })));
                }
            })),
            // Host's own client must NOT disable its own camera on
            // disable-video-all. Self-protection:
            //   • disable-video-all → client-side: this check stops a host
            //     disabling its own camera on its own broadcast.
            //   • disable-video (single-target) → server-side: routes/host.rs
            //     rejects a request where body.user_id == the caller's user_id.
            on_host_disable_video: Some(VcCallback::from({
                let self_uid = user_id_for_host_events.clone();
                move |_: ()| {
                    if host_set_signal.peek().contains(&self_uid) {
                        log::info!("HOST_DISABLE_VIDEO: ignored — local user is currently a host");
                        return;
                    }
                    log::info!("HOST_DISABLE_VIDEO: disabling local camera on host request");
                    let mut video_enabled = video_enabled;
                    let mut show_video_off_toast = show_video_off_toast;
                    let mut video_off_toast_timer = video_off_toast_timer;
                    video_enabled.set(false);
                    show_video_off_toast.set(true);
                    video_off_toast_timer.set(None);
                    video_off_toast_timer.set(Some(Timeout::new(6_000, move || {
                        let mut show_video_off_toast = show_video_off_toast;
                        let mut video_off_toast_timer = video_off_toast_timer;
                        show_video_off_toast.set(false);
                        video_off_toast_timer.set(None);
                    })));
                }
            })),
            on_participant_kicked: Some(VcCallback::from({
                let self_uid = user_id_for_host_events.clone();
                move |_: ()| {
                    if host_set_signal.peek().contains(&self_uid) {
                        log::warn!(
                            "PARTICIPANT_KICKED: ignored — local user is currently the host"
                        );
                        return;
                    }
                    let mut meeting_ended_message = meeting_ended_message;
                    let mut mic_enabled = mic_enabled;
                    let mut video_enabled = video_enabled;
                    let mut pending_mic_enable = pending_mic_enable;
                    let mut pending_video_enable = pending_video_enable;
                    let mut screen_share_state = screen_share_state;
                    meeting_ended_message.set(Some(
                        "You have been removed from the meeting by the host.".to_string(),
                    ));
                    mic_enabled.set(false);
                    video_enabled.set(false);
                    pending_mic_enable.set(false);
                    pending_video_enable.set(false);
                    screen_share_state.set(ScreenShareState::Idle);
                    log::info!("PARTICIPANT_KICKED: removed from meeting by host");
                    if let Some(client) = client_for_kick.borrow().as_ref() {
                        if let Err(e) = client.disconnect() {
                            log::warn!("PARTICIPANT_KICKED: disconnect failed: {e}");
                        }
                    }
                }
            })),
            // Broadcast to the whole room. Always update the live host set so the
            // promoted peer's "(Host)" indicator updates on every client
            // (including their own self row) without a reload. For self user_id:
            // also show a toast and bump the refresh nonce so `MeetingPage`
            // re-fetches our status and flips `is_owner`.
            on_host_granted: Some(VcCallback::from({
                let local_uid = user_id_for_host_events.clone();
                move |target: String| {
                    log::info!("HOST_GRANTED received for target=\"{target}\"");
                    {
                        let mut host_set_signal = host_set_signal;
                        host_set_signal.write().insert(target.clone());
                    }
                    bump_host_event_seq(host_event_seq);
                    if target == local_uid {
                        show_host_change_toast(
                            "You are now a host",
                            host_change_toast,
                            host_change_toast_timer,
                        );
                        if let Some(ctx) = host_refresh_nonce {
                            let mut n = ctx.0;
                            n.set(n() + 1);
                        }
                    }
                }
            })),
            on_host_revoked: Some(VcCallback::from({
                let local_uid = user_id_for_host_events.clone();
                move |target: String| {
                    log::info!("HOST_REVOKED received for target=\"{target}\"");
                    {
                        let mut host_set_signal = host_set_signal;
                        host_set_signal.write().remove(&target);
                    }
                    bump_host_event_seq(host_event_seq);
                    if target == local_uid {
                        show_host_change_toast(
                            "You are no longer a host",
                            host_change_toast,
                            host_change_toast_timer,
                        );
                        if let Some(ctx) = host_refresh_nonce {
                            let mut n = ctx.0;
                            n.set(n() + 1);
                        }
                    }
                }
            })),
            on_peer_event: Some(VcCallback::from(
                move |(source_user_id, event_type, _stream_id): (String, String, String)| {
                    if event_type != videocall_client::PEER_EVENT_SCREEN_DECODE_STARTED {
                        log::debug!("Ignoring PEER_EVENT with unknown event_type: {event_type}");
                        return;
                    }
                    log::info!("PEER_EVENT screen_decode_started received from {source_user_id}");
                    let mut screen_share_toast_state = screen_share_toast_state;
                    let mut screen_share_toast_timer = screen_share_toast_timer;
                    if !matches!(
                        screen_share_toast_state.peek().as_ref(),
                        Some(ScreenShareToastState::Starting)
                    ) {
                        return;
                    }
                    screen_share_toast_state.set(Some(ScreenShareToastState::SuccessfullyShared));
                    screen_share_toast_timer.set(Some(Timeout::new(4_000, move || {
                        let mut s = screen_share_toast_state;
                        if matches!(
                            s.peek().as_ref(),
                            Some(ScreenShareToastState::SuccessfullyShared)
                        ) {
                            s.set(None);
                        }
                    })));
                },
            )),
            on_peer_left: {
                let client_cell = client_for_reconnect.clone();
                Some(VcCallback::from(
                    move |(display_name, user_id, _session_id): (String, String, String)| {
                        log::debug!("TOAST-RX: peer left: {} ({})", display_name, user_id);

                        // Suppress replayed "left" events during a transport reconnect.
                        // The server replays the member list on reconnect (see issue 244),
                        // which would otherwise fire a spurious leave toast + sound that
                        // a following replayed "joined" cancels - ~30 toasts in a 15-person
                        // meeting after a network blip. Mirrors the on_peer_joined guard.
                        if let Some(ref client) = *client_cell.borrow() {
                            if client.is_reconnecting() {
                                log::debug!(
                                    "Suppressing leave toast for {} (reconnecting)",
                                    display_name
                                );
                                return;
                            }
                        }

                        let settings = appearance_settings.peek();
                        let show_toast = settings.show_exit_notifications;
                        let play_sound = settings.play_exit_sound;
                        drop(settings);

                        if show_toast {
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
                                if play_sound
                                    && peer_toasts.peek().iter().any(|(tid, _, _, _)| *tid == id)
                                {
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
                        } else if play_sound {
                            play_user_left();
                        }
                    },
                ))
            },
            on_peer_joined: {
                let client_cell = client_for_reconnect.clone();
                Some(VcCallback::from(
                    move |(display_name, user_id, session_id): (String, String, String)| {
                        log::debug!(
                            "TOAST-RX: peer joined: {} ({}, session={})",
                            display_name,
                            user_id,
                            session_id
                        );

                        let settings = appearance_settings.peek();
                        let show_toast = settings.show_entry_notifications;
                        let play_sound = settings.play_entry_sound;
                        drop(settings);

                        let suppress_toast = if let Some(ref client) = *client_cell.borrow() {
                            if client.is_reconnecting() {
                                log::debug!(
                                    "Suppressing join toast for {} (reconnecting)",
                                    user_id
                                );
                                true
                            } else if !session_id.is_empty()
                                && client.has_peer_with_session_id(&session_id)
                            {
                                // Suppress when THIS exact session is already
                                // tracked — e.g. a reconnect replays the
                                // PARTICIPANT_JOINED for an existing session
                                // we still hold in the peer list. Sibling
                                // same-user sessions have a distinct
                                // session_id and therefore still surface a
                                // toast (HCL issue 828).
                                log::debug!(
                                    "Suppressing join toast for {} (session {} already in peer list)",
                                    user_id,
                                    session_id
                                );
                                true
                            } else if session_id.is_empty()
                                && client.has_peer_with_user_id(&user_id)
                            {
                                // Legacy fallback: if the server didn't stamp
                                // a session_id, fall back to user-id-only
                                // suppression to preserve pre-issue-828 behaviour.
                                log::debug!(
                                    "Suppressing join toast for {} (already in peer list, no session_id)",
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

                        if !suppress_toast && show_toast {
                            if play_sound {
                                play_user_joined();
                            }
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
                            if !suppress_toast && play_sound {
                                play_user_joined();
                            }
                            peer_toasts.set(current);
                        }

                        {
                            let mut v = peer_list_version;
                            v.set(v() + 1);
                        }
                    },
                ))
            },
            on_display_name_changed: Some(VcCallback::from({
                // The client is installed into this cell after
                // `VideoCallClient::new` returns, so it is guaranteed to be
                // `Some` by the time any DISPLAY_NAME_CHANGED broadcast arrives.
                let client_cell = client_for_reconnect.clone();
                move |(changed_user_id, new_display_name, event_session_id): (
                    String,
                    String,
                    u64,
                )| {
                    log::info!(
                        "DIOXUS-UI: DISPLAY_NAME_CHANGED received: user={} new_name=\"{}\" session_id={}",
                        changed_user_id,
                        new_display_name,
                        event_session_id,
                    );

                    if user_id_for_display_name_changed.as_deref() == Some(changed_user_id.as_str())
                    {
                        // Resolve the local tab's own session_id (assigned by
                        // SESSION_ASSIGNED). Parsed to u64 to compare against
                        // the wire-format session_id from the meeting packet.
                        let own_session_id: Option<u64> = client_cell
                            .borrow()
                            .as_ref()
                            .and_then(|c| c.get_own_session_id())
                            .as_deref()
                            .and_then(|s| s.parse::<u64>().ok());

                        // Gate the local-self update on session_id so sibling
                        // tabs of the same authenticated user don't overwrite
                        // their own self display name. `event_session_id == 0`
                        // is the legacy broadcast (applies to all sessions).
                        let is_for_this_session = if event_session_id == 0 {
                            true
                        } else {
                            own_session_id == Some(event_session_id)
                        };

                        if is_for_this_session {
                            match validate_display_name(&new_display_name) {
                                Ok(validated_name) => {
                                    log::info!(
                                        "DIOXUS-UI: Local user display name confirmed by server (session match): {}",
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
                        } else {
                            log::info!(
                                "DIOXUS-UI: Skipping local-self update — rename event \
                                 targets sibling session {} (our session: {:?})",
                                event_session_id,
                                own_session_id,
                            );
                        }
                    }

                    let mut v = peer_display_name_version;
                    v.set(v() + 1);
                    log::debug!("DIOXUS-UI: peer_display_name_version bumped");
                }
            })),
            // Full call participant: decode and play all inbound media.
            decode_media: true,
            // Honour user transport preference: only allow the connection
            // manager's post-rebase re-election retry when the user is on
            // the default `WebTransport` mode (which advertises BOTH URL
            // lists to the manager). A manual `WebSocket` selection is a
            // deliberate single-transport choice and the retry must not
            // override it — the single-candidate state in that mode is
            // intentional, not a recoverable system condition.
            allow_post_rebase_retry: transport_pref == TransportPreference::WebTransport,
            // Phase 3 / AUTH-2 — discussion 562: let the connection
            // manager preempt token expiry from inside its internal
            // re-election. Without this, the manager re-uses the cached
            // server URLs (with the original JWT in the query string) and
            // every candidate gets rejected by the relay once the token
            // has expired; only the UI-level `schedule_reconnect` path
            // would eventually refresh — by which time the user has
            // already perceived a disconnect.
            //
            // We supply this callback ONLY in the JWT-auth build, which is
            // the build with token expiry at all. The non-JWT build keeps
            // re-election simple (no refresh exists to perform).
            #[cfg(feature = "media-server-jwt-auth")]
            refresh_room_token_callback: {
                let meeting_id_for_refresh = id.clone();
                let display_name_signal = current_display_name;
                let transport_pref_signal = transport_pref_ctx.0;
                Some(RefreshRoomTokenCallback::from(move || {
                    let meeting_id = meeting_id_for_refresh.clone();
                    async move {
                        match crate::meeting_api::refresh_room_token(&meeting_id).await {
                            Ok(new_token) => {
                                // Keep console-log uploads authenticated across
                                // token refreshes on long calls. O(1), and the
                                // token value is never logged.
                                set_console_log_auth_token(&new_token);
                                let dn = display_name_signal();
                                let (ws, wt) = build_lobby_urls(&new_token, &dn, &meeting_id);
                                // Apply the user's transport preference so
                                // the refreshed URL list matches what
                                // the initial connection (and the existing
                                // schedule_reconnect path) would build.
                                let pref = transport_pref_signal();
                                let server_wt_enabled =
                                    crate::constants::webtransport_enabled().unwrap_or(false);
                                let (_enable_wt, ws, wt) =
                                    resolve_transport_config(pref, server_wt_enabled, ws, wt);
                                log::info!(
                                    "DIOXUS-UI: refresh_room_token_callback succeeded — providing {} ws / {} wt URLs to ConnectionManager",
                                    ws.len(),
                                    wt.len(),
                                );
                                Some(RefreshedTokens {
                                    websocket_urls: ws,
                                    webtransport_urls: wt,
                                })
                            }
                            Err(e) => {
                                log::warn!(
                                    "DIOXUS-UI: refresh_room_token_callback failed ({e}); ConnectionManager will re-election with cached URLs"
                                );
                                None
                            }
                        }
                    }
                }))
            },
            #[cfg(not(feature = "media-server-jwt-auth"))]
            refresh_room_token_callback: None,
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

    // Release any pre-join preview hardware (camera + mic) and close the
    // AudioContext on unmount, covering the route-change / tab-close paths the
    // join handler's explicit shutdown does not. Idempotent. (issue #959)
    {
        let preview_engine_for_drop = preview_engine.clone();
        use_drop(move || {
            preview_engine_for_drop.shutdown();
        });
    }

    let mda = use_hook(|| {
        let mut mda = MediaDeviceAccess::new();
        let client_cell = RefCell::new(client.clone());
        let preview_engine_for_mda = preview_engine.clone();
        // Read-and-cleared once per `on_result` (in the in-meeting branch) to
        // decide whether THIS result came from a background auto-retry tick (in
        // which case we must NOT re-pop the blocking modal).
        let is_background_retry = is_background_retry.clone();
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
            let mut join_requested = join_requested;

            // A single-device background retry tick leaves the OTHER side as
            // `PermissionState::Unknown` (a "not probed this call" sentinel).
            // Only clear/reset state for the side(s) actually probed, so a mic-
            // only tick can never stomp a healthy live camera's error/enabled
            // state (or vice versa). `connection_error`/`media_access_granted`
            // are meaningful ONLY for a genuine full request (pre-join grant or
            // a manual button click that probes BOTH sides) — resetting them on
            // a single-device health-check tick every ~4s could mask a real,
            // concurrent connection-error banner, so they are gated on both
            // sides being probed.
            let audio_probed = !matches!(permit.audio, PermissionState::Unknown);
            let video_probed = !matches!(permit.video, PermissionState::Unknown);

            // Set-if-changed to avoid a background-retry re-render storm.
            // `mic_error`/`video_error` are read in this ~9000-line component's
            // render scope (the device-warning modal, button badges, …) AND the
            // retry `use_effect` subscribes to them, so an unconditional
            // clear-then-reclassify would re-render the whole component on EVERY
            // failed probe — even one that reports the exact same blocked state as
            // the previous tick (`Signal::set` marks dirty unconditionally; it does
            // NOT dedupe by value — that is `use_memo`, not `Signal`). Compute the
            // target for each PROBED side and write only on a genuine change, so a
            // steady-state blocked device performs zero ERROR-signal writes per
            // retry tick — avoiding a full-component re-render and a needless re-run
            // of the retry effect. (The retry `Interval` itself is now built
            // once-per-episode behind an `is_none()` guard, so even a re-run of that
            // effect no longer rebuilds the timer; this dedupe is a re-render
            // optimization, no longer load-bearing for retry-cadence correctness.)
            // (The
            // in-meeting branch below may still write other signals such as
            // `pending_*_enable` on a failed tick; those don't drive the retry
            // effect, so they're out of scope for this dedupe.)
            // Gated on `*_probed` so an un-probed (`Unknown`) side — the sentinel a
            // single-device retry leaves behind — is never clobbered. `.peek()`
            // reads without subscribing (correct here: this is an event-handler
            // closure, not render, matching `.peek()` use elsewhere in this file).
            if audio_probed {
                let target = permission_probe_error_target(&permit.audio);
                if mic_error.peek().as_ref() != target.as_ref() {
                    mic_error.set(target);
                }
            }
            if video_probed {
                let target = permission_probe_error_target(&permit.video);
                if video_error.peek().as_ref() != target.as_ref() {
                    video_error.set(target);
                }
            }
            // Only a genuine full request (both sides probed) touches these; a
            // single-device background retry tick never reaches here, so no
            // set-if-changed dedupe is needed (they don't churn on retry ticks).
            if audio_probed && video_probed {
                connection_error.set(None);
                media_access_granted.set(true);
            }

            // Fulfil any pending mic/camera enables that triggered the permission request.
            if matches!(permit.audio, PermissionState::Granted) && pending_mic_enable() {
                mic_enabled.set(true);
                pending_mic_enable.set(false);
            }
            if matches!(permit.video, PermissionState::Granted) && pending_video_enable() {
                video_enabled.set(true);
                pending_video_enable.set(false);
            }

            // If a probe (typically a background auto-retry tick) has left BOTH
            // sides error-free while the blocking modal is still open — the user
            // never dismissed it because the retry loop is designed to recover
            // without user action — auto-close it. Otherwise the user is stranded
            // on an empty "Device access problem" dialog with no error rows, just
            // an "Ok" button. This runs AFTER the per-side set-if-changed writes
            // above so the reads see the final error state for THIS result; the
            // both-None gate means a probe that recovers one side while the other
            // is still (or newly) failing keeps the modal up to show the remainder.
            // Placed before the branch below so it applies uniformly to the
            // in-meeting, pre-join-preview, and join flows that all share this
            // handler.
            if should_auto_close_device_warning(
                mic_error.read().is_none(),
                video_error.read().is_none(),
                show_device_warning(),
            ) {
                show_device_warning.set(false);
            }

            if session_loaded() || connecting() {
                // In-meeting result (initial retry click, focus re-check, or a
                // background auto-retry tick). Read-and-clear the background flag
                // EXACTLY once here, on both success and failure, so it can never
                // stay stuck: a success clears it (so the next genuine failure
                // pops the modal) and a background failure clears it (so it stays
                // silent this tick).
                //
                // A single-device probe (exactly one side left `Unknown`) is the
                // AUTHORITATIVE background signal: only the auto-retry tick issues
                // `request_audio_only`/`request_video_only`, so a lone `Unknown`
                // side means "background". We OR it with the flag because a
                // both-device retry tick fires TWO single-device probes → TWO
                // `on_result`s, and the read-and-clear flag would only cover the
                // first; the inference covers the second so it can never re-pop
                // the modal. A manual full request probes BOTH sides (no
                // `Unknown`), so it is never misclassified as background.
                let single_device_probe = audio_probed != video_probed;
                let was_background_retry =
                    is_background_retry.replace(false) || single_device_probe;
                // Diagnostic (step 4): record the CLASSIFIED outcome of every
                // in-meeting probe — especially the background auto-retry ticks —
                // so a recurrence of the "badge never clears after the app releases
                // the device" report leaves concrete console evidence of WHAT the
                // browser actually returned on each attempt (granted vs which error
                // variant), rather than forcing another static-only investigation.
                if was_background_retry {
                    log::info!(
                        "[media-retry] probe result (background): audio={:?} video={:?}",
                        permit.audio,
                        permit.video,
                    );
                }
                let mic_failed = mic_error.read().is_some();
                let video_failed = video_error.read().is_some();
                // Only mutate a side's enabled/pending state if THIS result
                // actually probed it — an audio-only tick must never touch the
                // camera's enabled state (and vice versa), even if the other
                // side happens to carry a stale error signal.
                if audio_probed && mic_failed {
                    mic_enabled.set(false);
                    pending_mic_enable.set(false);
                }
                if video_probed && video_failed {
                    video_enabled.set(false);
                    pending_video_enable.set(false);
                }
                // Surface the blocking modal for a user-initiated failure (or the
                // first failure), but NOT for a background auto-retry tick that
                // failed again — the modal is already up from the initial failure.
                if (mic_failed || video_failed) && !was_background_retry {
                    show_device_warning.set(true);
                }
            } else if !join_requested() {
                // Permission was requested just to PREVIEW devices (issue #959).
                // Do NOT connect — the pre-join enumeration effect will pick up
                // the granted permission and populate the device list. The user
                // must click Join/Start to actually enter the meeting.
                log::info!("Media permission granted for pre-join preview (not joining yet)");
            } else if mic_error.read().is_some() || video_error.read().is_some() {
                // Initial JOIN attempt failed because a device was blocked
                // (e.g. `DeviceInUse`/`NotReadableError`). The background
                // auto-retry loop arms itself off the `*_error` signal alone
                // (see `should_auto_retry`), so there is nothing to seed here:
                // any `DeviceInUse` error — including one detected at the very
                // first JOIN-time probe, for a device the user never toggled on
                // pre-join and never clicks in-meeting — is retried until the
                // blocking app releases it and the badge clears on its own.
                //
                // (This supersedes an earlier, narrower fix that seeded a
                // `*_want_on` intent signal from the raw pre-join toggle here so
                // the loop could arm for a blocked device. That only closed the
                // gap when the user had toggled the device ON pre-join; a device
                // blocked with the toggle OFF still sat "blocked" forever. Since
                // a background probe can only ever CLEAR the error — never
                // auto-start capture — gating retry on intent protected nothing,
                // so the intent signal was removed and retry is now
                // unconditional.)
                show_device_warning.set(true);
                meeting_joined.set(false);
                join_requested.set(false);
            } else {
                // Real join. Apply the pre-join camera/mic on-off choices.
                //
                // Each track is honored only if permission for it was granted
                // AND a device exists, so we never try to enable capture we
                // can't perform (`resolve_initial_enabled`).
                //
                // We set `mic_enabled`/`video_enabled` DIRECTLY rather than via
                // `pending_*_enable`: the pending flags are consumed earlier in
                // THIS same `on_result` invocation (the granted-pending check
                // above), and `request()` fires `on_result` exactly once with
                // nothing re-firing post-connect. Writing them here would be too
                // late — the Host reads `mic_enabled`/`video_enabled` as props
                // and acts on them when it mounts/renders, which is what drives
                // `client.set_audio_enabled` / `set_video_enabled`. (issue #959)
                let audio_ok = matches!(permit.audio, PermissionState::Granted);
                let video_ok = matches!(permit.video, PermissionState::Granted);
                let want_mic = resolve_initial_enabled(
                    prejoin_mic_on(),
                    audio_ok,
                    !prejoin_microphones.read().is_empty(),
                );
                let want_cam = resolve_initial_enabled(
                    prejoin_camera_on(),
                    video_ok,
                    !prejoin_cameras.read().is_empty(),
                );

                // Release the preview hardware BEFORE the real encoders start so
                // there is no double-capture of the camera/mic. (issue #959)
                preview_engine_for_mda.shutdown();

                mic_enabled.set(want_mic);
                video_enabled.set(want_cam);
                // Clear any stale pending writes so a later focus re-check
                // can't resurrect them.
                pending_mic_enable.set(false);
                pending_video_enable.set(false);

                let mut connecting = connecting;
                connecting.set(true);
                if let Err(e) = client_cell.borrow_mut().connect() {
                    log::error!("Connection failed: {e:?}");
                }
                meeting_joined.set(true);
                join_requested.set(false);
            }

            if device_was_denied() {
                device_was_denied.set(false);
                reload_devices_counter.set(reload_devices_counter() + 1);
            }
        });
        Rc::new(RefCell::new(mda))
    });

    // ── Background auto-retry loop for a device blocked by `DeviceInUse` ──
    //
    // While a device is blocked by another application
    // (`MediaErrorState::DeviceInUse`), re-probe `getUserMedia` in the background
    // so the device reconnects on its own the moment the other app releases it —
    // no click required. Local `getUserMedia` calls fail/succeed near-instantly
    // (no network round-trip). Retry is NOT gated on any "user wants it on"
    // intent: a background probe can only ever CLEAR the error (it never sets
    // `pending_*_enable`, so it can never auto-start capture), so keeping the
    // badge accurate is always safe — see `should_auto_retry`.
    //
    // BACKOFF (approach: fixed-interval timer + on-schedule probing). A single
    // fixed 4s `gloo` `Interval` fires every 4s, but the actual `getUserMedia`
    // PROBE only runs on ticks that match an exponentially-growing schedule:
    // 4s → 8s → 16s → 32s → 60s (held), so a device that is never released does
    // not poll forever at 4s (battery/CPU tax on low-power devices over a long
    // meeting). Off-schedule ticks are a near-free empty closure invocation
    // (a couple of signal reads + a counter bump), NOT a `getUserMedia` call.
    // The `Interval` is built EXACTLY ONCE per retry episode. This effect re-runs
    // on any change to a signal it subscribes to (`*_error`), and
    // some re-runs happen mid-episode while `want_retry` stays true — e.g. the
    // live encoder's restart loop fires `on_*_permission_error` several times over
    // ~5s. The `if cell.borrow().is_none()` guard below therefore gates BOTH the
    // backoff-counter reset AND the `Interval::new(...)` call (and the arm returns
    // early otherwise): a mid-episode re-run leaves the RUNNING timer and its
    // backoff untouched. This is deliberate — rebuilding the `Interval` restarts
    // its fixed countdown from zero, so rebuilding on every churned re-run could
    // push the first probe out indefinitely under sustained error-signal writes,
    // leaving the device "stuck failing" long after the blocking app released it.
    // (The `on_result` probe writes are additionally set-if-changed via
    // `permission_probe_error_target`, and the in-meeting `on_*_permission_error`
    // handlers are likewise set-if-changed, so a repeated-same-state failure does
    // not even re-run this effect — but correctness no longer DEPENDS on that,
    // because the once-per-episode guard makes the cadence deterministic
    // regardless of how often the effect re-runs.) The single live `Interval` is
    // dropped (cancelling its `setInterval`) only when `want_retry` goes false
    // (else-branch) or on unmount (`use_drop`), so at most one timer is ever live.
    // Backoff lives in the separate `Rc<Cell<u32>>` counters below, not inside the
    // `Interval`. `retry_gap_ticks` is the required number of 4s
    // ticks between probes; it resets to 1 (→ 4s) on a FRESH retry episode
    // (interval slot empty) and doubles up to `RETRY_MAX_GAP_TICKS` (15 ≈ 60s)
    // after each probe that still finds the device blocked. Success clears the
    // error signal → `want_retry` goes false → the interval is dropped → the
    // next fresh failure starts a new episode back at 4s. A user re-toggling
    // the device off then on again likewise ends the episode (want_off) and the
    // next on-with-failure starts fresh at 4s.
    //
    // MOBILE SAFETY: each probe touches ONLY the side(s) that still need
    // retrying via `request_audio_only`/`request_video_only`, never the combined
    // `request()` — re-probing a healthy, live camera every few seconds while
    // only the mic is blocked risks glitching the live stream on constrained
    // devices (iOS Safari / some Android WebViews).
    //
    // The interval lives in a `use_hook` cell and is created/dropped inside a
    // `use_effect` gated on the retry condition; `use_drop` cancels it on
    // unmount. We deliberately do NOT `.forget()` it — dropping the `Interval`
    // must provably cancel the underlying `setInterval`.
    {
        // 4s base cadence; ceiling of 15 ticks (× 4s = 60s) between probes.
        const RETRY_BASE_INTERVAL_MS: u32 = 4000;
        const RETRY_MAX_GAP_TICKS: u32 = 15;

        type IntervalCell = Rc<RefCell<Option<gloo_timers::callback::Interval>>>;
        let cell: IntervalCell = use_hook(|| Rc::new(RefCell::new(None)));
        // 4s ticks elapsed since the last probe fired, and the current required
        // gap (in ticks) between probes. Persist across effect re-runs so the
        // backoff is NOT reset when a background failure rewrites an error signal
        // mid-episode (which re-runs this effect); reset only on a fresh episode.
        let retry_since_probe: Rc<Cell<u32>> = use_hook(|| Rc::new(Cell::new(0)));
        let retry_gap_ticks: Rc<Cell<u32>> = use_hook(|| Rc::new(Cell::new(1)));
        let cell_effect = cell.clone();
        let mda_effect = mda.clone();
        let is_background_retry_effect = is_background_retry.clone();
        let since_probe_effect = retry_since_probe.clone();
        let gap_ticks_effect = retry_gap_ticks.clone();
        use_effect(move || {
            // Subscribe to the gating signals so this effect re-runs (starting or
            // stopping the interval) whenever error state changes.
            let want_retry = should_auto_retry(mic_error.read().as_ref())
                || should_auto_retry(video_error.read().as_ref());
            if want_retry {
                // Fresh episode ONLY (no interval currently armed): reset the
                // backoff so the first probe lands at ~4s, and BUILD the interval.
                //
                // If an interval is ALREADY armed we must do NOTHING here — neither
                // reset the backoff nor rebuild the timer. This effect re-runs on
                // any change to a signal it subscribes to (`*_error`).
                // Some of those re-runs happen mid-episode while
                // `want_retry` stays true — e.g. the live encoder's restart loop
                // fires `on_*_permission_error` up to 5 times over ~5s, each doing a
                // (now set-if-changed, but historically unconditional) error write.
                // Rebuilding the `Interval` on every such re-run would restart its
                // fixed countdown from zero each time, so the FIRST probe could be
                // pushed out indefinitely under sustained churn — the device would
                // appear "stuck failing" long after the blocking app released it.
                // Guarding creation behind `is_none()` makes the cadence
                // deterministic: once armed, the timer ticks undisturbed until the
                // error clears (want_retry → false, else-branch drops it) or the
                // component unmounts. The tick closure re-reads all signals fresh
                // each fire, so it stays correct without being rebuilt.
                if cell_effect.borrow().is_none() {
                    since_probe_effect.set(0);
                    gap_ticks_effect.set(1);
                    log::info!(
                        "[media-retry] arming auto-retry loop (mic_err={:?} video_err={:?})",
                        mic_error.peek().as_ref(),
                        video_error.peek().as_ref(),
                    );
                } else {
                    // Already armed; leave the running timer and its backoff intact.
                    return;
                }
                let mda_tick = mda_effect.clone();
                let is_background_retry_tick = is_background_retry_effect.clone();
                let since_probe_tick = since_probe_effect.clone();
                let gap_ticks_tick = gap_ticks_effect.clone();
                let interval = gloo_timers::callback::Interval::new(
                    RETRY_BASE_INTERVAL_MS,
                    move || {
                        // Recompute the retry condition FRESH each tick — signals
                        // may have changed since the interval was created (e.g. one
                        // device recovered so its error cleared).
                        let mic_retry = should_auto_retry(mic_error.read().as_ref());
                        let video_retry = should_auto_retry(video_error.read().as_ref());
                        if !(mic_retry || video_retry) {
                            return;
                        }
                        // Backoff gate: the pure `retry_tick_decision` decides
                        // whether enough base-cadence ticks have elapsed for the
                        // current gap. Off-schedule ticks just advance the counter
                        // and return without issuing `getUserMedia`; probe ticks
                        // reset the counter and grow the (capped) gap. Keeping the
                        // math in a pure fn lets the long-run schedule be unit-tested.
                        let gap_before = gap_ticks_tick.get();
                        let decision = retry_tick_decision(
                            since_probe_tick.get(),
                            gap_before,
                            RETRY_MAX_GAP_TICKS,
                        );
                        since_probe_tick.set(decision.since);
                        gap_ticks_tick.set(decision.gap);
                        if !decision.probe {
                            // Deliberately NOT logged: a skip-tick is a near-free
                            // no-op and logging every one would spam the console
                            // over a long meeting (step 4 keeps volume reasonable).
                            return;
                        }
                        // Probe attempt — log it (and the backoff state) so a
                        // recurrence of the "badge never clears" report leaves real
                        // console evidence: whether the loop is ticking at all,
                        // which side(s) it probes, and the gap it is now at.
                        log::info!(
                            "[media-retry] probe tick: mic_retry={mic_retry} video_retry={video_retry} \
                             gap_ticks_before={gap_before} next_gap_ticks={}",
                            decision.gap,
                        );
                        // Mark the upcoming `on_result`(s) as background-originated
                        // so a repeated failure does NOT re-pop the blocking modal.
                        // (A both-device retry fires two single-device probes → two
                        // `on_result`s; `on_result` additionally infers "background"
                        // from a lone `Unknown` side, covering the second one.)
                        is_background_retry_tick.set(true);
                        // Deliberately DO NOT set `pending_mic_enable`/
                        // `pending_video_enable` here. A background recovery probe
                        // must only CLEAR the blocked-state error (removing the
                        // warning badge and auto-closing the modal); it must NOT
                        // signal an intent to enable the device. Setting the
                        // pending flags would make `on_result`'s fulfilment logic
                        // flip `mic_enabled`/`video_enabled` to `true` and start
                        // capture the instant the other app releases the device —
                        // turning the camera/mic on with no immediate user action,
                        // which is both surprising and a privacy concern. Starting
                        // capture must require a fresh, explicit user click; the
                        // manual-click path (button `onclick`) still sets the
                        // pending flags so a "click on while blocked → later probe
                        // succeeds → device turns on" flow keeps working.
                        // Probe ONLY the still-blocked side(s). Each single-device
                        // probe leaves the OTHER side `Unknown` in its `on_result`,
                        // so it cannot stomp a healthy live device's state. When
                        // BOTH are blocked, both devices are already down (no live
                        // stream to glitch), so two independent single-device
                        // probes are safe. Success flows through `on_result`, which
                        // clears the error signal (dropping this interval on the
                        // next effect run) and auto-closes the modal — but does NOT
                        // fulfil any pending-enable, because this path never sets
                        // one, so the device stays OFF until the user clicks.
                        if mic_retry {
                            mda_tick.borrow().request_audio_only();
                        }
                        if video_retry {
                            mda_tick.borrow().request_video_only();
                        }
                    },
                );
                *cell_effect.borrow_mut() = Some(interval);
            } else {
                // No device needs retrying → cancel the timer. The next fresh
                // failure will re-arm it with the backoff reset to 4s. Log only the
                // real armed→disarmed transition (recovery / user opted-off), not
                // every steady re-run that was already disarmed.
                if cell_effect.borrow().is_some() {
                    log::info!("[media-retry] disarming auto-retry loop (recovered or opted off)");
                }
                *cell_effect.borrow_mut() = None;
            }
        });
        use_drop(move || {
            *cell.borrow_mut() = None;
        });
    }

    // ── Pre-join device enumeration + preview wiring (issue #959) ───────
    //
    // Once getUserMedia permission is granted (so device labels are populated)
    // and we are still on the pre-join screen, enumerate devices, restore the
    // persisted selections, and start the preview for any track the user chose
    // to begin with ON. Re-runs when `reload_devices_counter` bumps (hot-plug /
    // re-grant). The effect is a no-op after the meeting actually starts —
    // teardown happens in the join handler and on unmount.
    // Tracks the last (granted, reload_counter) key we ran `dev.load()` for, so
    // the effect re-running for unrelated reasons does not re-enumerate and
    // re-register the device-change listener every time. (code-review item 10)
    let prejoin_loaded_key: Rc<Cell<Option<u32>>> = use_hook(|| Rc::new(Cell::new(None)));
    {
        let prejoin_devices = prejoin_devices.clone();
        let preview_engine = preview_engine.clone();
        let prejoin_loaded_key = prejoin_loaded_key.clone();
        use_effect(move || {
            // Subscribe to the reactive triggers.
            let granted = media_access_granted();
            let reload = reload_devices_counter();
            if !granted || meeting_joined() {
                return;
            }
            // Run `load()` once per grant, plus once per `reload_devices_counter`
            // bump (hot-plug / re-grant). The counter is the load key.
            if prejoin_loaded_key.get() == Some(reload) {
                return;
            }
            prejoin_loaded_key.set(Some(reload));
            let preview_engine = preview_engine.clone();
            let mut dev = prejoin_devices.borrow_mut();

            // Copy enumerated devices into signals and apply restored
            // selections. Shared by the initial load and hot-plug changes.
            let apply = {
                let prejoin_devices = prejoin_devices.clone();
                let preview_engine = preview_engine.clone();
                move || {
                    // Rebind Copy signals as local `mut` so this closure stays
                    // `Fn` (required by VcCallback) — `.set()` mutates the local
                    // handle, not a captured variable.
                    let mut prejoin_cameras = prejoin_cameras;
                    let mut prejoin_microphones = prejoin_microphones;
                    let mut prejoin_speakers = prejoin_speakers;
                    let mut prejoin_selected_camera = prejoin_selected_camera;
                    let mut prejoin_selected_mic = prejoin_selected_mic;
                    let mut prejoin_selected_speaker = prejoin_selected_speaker;
                    let mut prejoin_camera_on = prejoin_camera_on;
                    let mut prejoin_mic_on = prejoin_mic_on;

                    let mut dev = prejoin_devices.borrow_mut();
                    let cams = dev.video_inputs.devices();
                    let mics = dev.audio_inputs.devices();
                    let spks = dev.audio_outputs.devices();

                    let (stored_cam, stored_mic, stored_spk) = load_preferred_device_ids();
                    let cam_ids: Vec<String> = cams.iter().map(|d| d.device_id()).collect();
                    let mic_ids: Vec<String> = mics.iter().map(|d| d.device_id()).collect();
                    let spk_ids: Vec<String> = spks.iter().map(|d| d.device_id()).collect();

                    let cam_sel = restore_device_id(stored_cam.as_deref(), &cam_ids);
                    let mic_sel = restore_device_id(stored_mic.as_deref(), &mic_ids);
                    let spk_sel = restore_device_id(stored_spk.as_deref(), &spk_ids);

                    // Reflect the resolved selection back into the device list so
                    // selected()/select() stay consistent with what we show.
                    if let Some(id) = cam_sel.as_deref() {
                        dev.video_inputs.select(id);
                    }
                    if let Some(id) = mic_sel.as_deref() {
                        dev.audio_inputs.select(id);
                    }
                    if let Some(id) = spk_sel.as_deref() {
                        dev.audio_outputs.select(id);
                    }
                    drop(dev);

                    prejoin_cameras.set(cams);
                    prejoin_microphones.set(mics);
                    prejoin_speakers.set(spks);
                    prejoin_selected_camera.set(cam_sel.clone());
                    prejoin_selected_mic.set(mic_sel.clone());
                    prejoin_selected_speaker.set(spk_sel);

                    // Persist resolved selections (so a fallback after an
                    // unplugged device becomes the new remembered choice).
                    if let Some(id) = cam_sel.as_deref() {
                        save_preferred_camera_id(id);
                    }
                    if let Some(id) = mic_sel.as_deref() {
                        save_preferred_mic_id(id);
                    }

                    // Start preview for tracks the user wants ON, gated on a
                    // device actually being present.
                    if prejoin_camera_on() {
                        if let Some(id) = cam_sel {
                            preview_engine.start_camera(id);
                        } else {
                            prejoin_camera_on.set(false);
                        }
                    }
                    if prejoin_mic_on() {
                        if let Some(id) = mic_sel {
                            preview_engine.start_mic_meter(id);
                        } else {
                            prejoin_mic_on.set(false);
                        }
                    }
                }
            };

            dev.on_loaded = VcCallback::from({
                let apply = apply.clone();
                move |_| apply()
            });
            dev.on_devices_changed = VcCallback::from(move |_| apply());
            dev.load();
        });
    }

    // ── Auto-request media permission for pre-join preview (issue 1134) ──────
    //
    // So the camera/mic device selectors appear automatically when the user
    // lands on the pre-join screen, fire a single permission request on mount.
    // This fires ONLY on the manual pre-join path (`auto_join == false`); on the
    // auto-join path (waiting-room admission / direct-URL) the auto-join effect
    // already owns the single permission request that proceeds to connect, so
    // this effect early-returns there to avoid a redundant getUserMedia.
    // This is PREVIEW-ONLY and never auto-joins: it does NOT set
    // `join_requested`, so `on_result` takes the `else if !join_requested()`
    // branch (logs the preview message, does not connect), preserving the
    // issue 933 no-auto-start invariant.
    //
    // `request()` is async (spawn_local in media_device_access.rs), so when it
    // resolves `on_result` sets `media_access_granted = true`. That write
    // invalidates this effect's subscription and re-runs it once; the
    // `Rc<Cell<bool>>` one-shot guard — set BEFORE calling `request()` — makes
    // that re-run a no-op, so the request fires exactly once. (This effect does
    // NOT re-run on window `focus`: it reads none of the signals a focus event
    // touches — the separate focus listener, which early-returns while
    // `meeting_joined` is false, is what keeps focus from acting in pre-join.)
    // The request is also gated on the live signals so it never fires while in
    // a meeting, while connecting, or once access is already granted.
    let auto_requested = use_hook(|| Rc::new(Cell::new(false)));
    {
        let mda = mda.clone();
        let auto_requested = auto_requested.clone();
        use_effect(move || {
            // Manual pre-join path only: on the auto-join path the auto-join
            // effect already owns the single permission request that proceeds
            // to connect, so skipping here avoids a redundant getUserMedia.
            if auto_join {
                return;
            }
            // Subscribe to the reactive triggers by reading them in the closure.
            let joined = meeting_joined();
            let granted = media_access_granted();
            let requested = join_requested();
            // Fire exactly once on a clean pre-join mount: not in a meeting, not
            // already granted, no join in flight, and the guard not yet tripped.
            if joined || granted || requested || auto_requested.get() {
                return;
            }
            // Set the guard BEFORE requesting: once `request()` resolves and
            // `on_result` flips `media_access_granted`, this effect re-runs —
            // the guard makes that re-run return early, so request() fires once.
            auto_requested.set(true);
            // Preview-only: deliberately do NOT set `join_requested` — keeps
            // `on_result` on the preview branch (no connect).
            mda.borrow().request();
        });
    }

    // Keep each pre-join <select>'s DOM `.value` in sync with the restored
    // selection signal once devices are enumerated. (issue #959 restore bug)
    //
    // The selects first render with no options (pre-enumeration), so the browser
    // settles their `.value` on the implicit "default" option. When `apply()`
    // later populates the option list AND the restored `prejoin_selected_*`
    // signal, Dioxus patches the `<option selected>` attribute — but a browser
    // `<select>` does NOT re-derive `.value` from a post-parse attribute
    // mutation, so the control stays stuck on "default". This effect reads the
    // selection + device-list SIGNALS (so it re-runs reactively after `apply`)
    // and sets the IDL `value` directly, which reliably moves the selection.
    // It lives here (not in the card) because Dioxus 0.7 `use_effect` only
    // re-runs on Signal reads, not on plain prop-value changes.
    {
        use crate::components::pre_join_settings_card::{
            sync_select_value, PREVIEW_CAMERA_SELECT_ID, PREVIEW_MIC_SELECT_ID,
            PREVIEW_SPEAKER_SELECT_ID,
        };
        use_effect(move || {
            // Reactive deps: selection ids + whether option lists are populated.
            let cam = prejoin_selected_camera();
            let mic = prejoin_selected_mic();
            let spk = prejoin_selected_speaker();
            let has_cams = !prejoin_cameras.read().is_empty();
            let has_mics = !prejoin_microphones.read().is_empty();
            let has_spks = !prejoin_speakers.read().is_empty();
            if !media_access_granted() || meeting_joined() {
                return;
            }
            if has_cams {
                sync_select_value(PREVIEW_CAMERA_SELECT_ID, cam.as_deref());
            }
            if has_mics {
                sync_select_value(PREVIEW_MIC_SELECT_ID, mic.as_deref());
            }
            if has_spks && speaker_supported {
                sync_select_value(PREVIEW_SPEAKER_SELECT_ID, spk.as_deref());
            }
        });
    }

    // Re-check permissions when the window regains focus, mirroring Yew behavior.
    // Only fires for users already in-meeting who had a prior denial — on the
    // pre-join screen (meeting_joined=false) this is a no-op.
    {
        let mda = mda.clone();
        use_effect(move || {
            let value = mda.clone();
            let closure = Closure::wrap(Box::new(move |_event: web_sys::Event| {
                if !meeting_joined() || session_loaded() || connecting() {
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
    // Issue #1558: publish the protective-mode report so `Host` can actuate the
    // LOCAL encoder send-layer self-shed (stage 3).
    use_context_provider(|| crate::context::ProtectiveModeCtx(protective_mode_report));
    // Provide the host set so peer-list rows and video tiles render a host
    // indicator for the current host.
    use_context_provider(|| HostSetCtx(host_set_signal));
    let mut meeting_time_signal = use_signal(MeetingTime::default);
    use_context_provider(|| meeting_time_signal);
    let local_audio_level_ctx = use_context_provider(|| LocalAudioLevelCtx(local_audio_level));
    let _ = local_audio_level_ctx.0;
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

    // HCL bug #8 + #9: provide the popup-state map so PeerTile can look up
    // each popup's open/free-position state. Surviving the parent re-render
    // is what makes peer leaves stop tearing down every other open popup.
    use_context_provider(|| signal_popup_state_map);

    // Per-tile crop state — signal created early (near peer_status_map) so
    // on_peer_removed can clean up; context provided here for child access.
    use_context_provider(|| CroppedTilesCtx(cropped_tiles_signal));

    // Issue 1768: media-metrics overlay toggle. `MediaMetricsOverlayCtx` is the
    // enabled flag every PeerTile (remote overlays) and `Host` (the self overlay)
    // consult. `Host` sources the local SEND metrics from its own live snapshot
    // reader, so no separate send-snapshot context is needed here.
    use_context_provider(|| MediaMetricsOverlayCtx(media_metrics_overlay_enabled));

    // Action bar dock position and autohide — exposed via context so that
    // the AppearanceSettingsPanel can read/write them.
    use_context_provider(|| DockPositionCtx(dock_position));
    use_context_provider(|| AutohideCtx(autohide_enabled));
    use_context_provider(|| DensityModeCtx(density_mode));
    // Provide the decode-budget override so the settings UI (task 1a.5) can read
    // and mutate it. Exposed exactly like density: a single shared signal.
    use_context_provider(|| DecodeBudgetCtx(decode_budget_override));
    // Issue #1466: per-session "force-decode this peer" requests. Created in the
    // PARENT render scope (so a per-tile PLAY click that writes it re-renders the
    // parent and recomputes the partition / active_decode_set — see the `.read()`
    // in the phase-4 merge + the promotion step below) and shared via context so
    // the per-tile PLAY button (threaded down as `on_request_decode`) can toggle
    // it. Empty at mount; NOT persisted (see `UserRequestedDecodeCtx` doc).
    let mut user_requested_decode = use_signal(HashSet::<String>::new);
    use_context_provider(|| UserRequestedDecodeCtx(user_requested_decode));

    // Single diagnostics subscriber shared by all PeerTile components.
    // Instead of each PeerTile spawning its own async task, one task
    // dispatches peer_status events into a shared HashMap.
    let mut diagnostics_task: Signal<Option<dioxus_core::Task>> = use_signal(|| None);
    let bump_pending_for_effect = peer_list_bump_pending.clone();
    use_effect(move || {
        let bump_pending = bump_pending_for_effect.clone();
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
                            // Phase 6 render-storm fix (cc7tp 2026-05-06):
                            // coalesce bursty speaker activity into one
                            // `peer_list_version` bump per 50 ms window
                            // instead of per-event. Without this, multiple
                            // active speakers triggered 3-5 full meeting-
                            // view re-renders per second, which on 2-core
                            // hardware compounded into 5 s main-thread
                            // stalls.
                            //
                            // Signal<u32> is `Copy`, so we move a copy into
                            // the throttled callback and re-bind it as
                            // `mut` inside to satisfy the `Fn` bound on
                            // the boxed closure.
                            let v = peer_list_version;
                            schedule_throttled_bump(
                                bump_pending.clone(),
                                PEER_LIST_VERSION_THROTTLE_MS,
                                Rc::new(move || {
                                    let mut v = v;
                                    let next = *v.peek() + 1;
                                    v.set(next);
                                }),
                            );
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
                            // #1465 reactivity gap fix: when an EXISTING peer's
                            // `video_enabled` flips, the parent MUST re-render so
                            // the DecodeBudget partition (which runs in the
                            // `AttendantsComponent` render body and classifies each
                            // peer via the NON-reactive `is_video_enabled_for_peer`)
                            // re-runs and re-derives the `active_decode_set`.
                            //
                            // #1465 excluded camera-OFF peers from the budget, so a
                            // camera-OFF peer is NOT in `active_decode_set` and never
                            // gets `peer.visible = true`. The video decode path then
                            // SKIPs its frames (`peer_decode_manager.rs`, `if
                            // !self.visible { return SKIPPED }`). The MAJORITY case is
                            // a peer joining camera-OFF (the default,
                            // `load_preferred_camera_on()` == false) and turning the
                            // camera ON mid-call: without this bump the parent never
                            // re-renders, `active_decode_set` stays stale, `visible`
                            // stays false, and the tile shows a blank/frozen canvas
                            // until some UNRELATED re-render happens to fire. The
                            // per-peer PeerTile self-heals its own canvas subscription,
                            // but the active_decode_set is owned by the parent.
                            //
                            // Use the THROTTLED `peer_list_version` bump (mirroring the
                            // speaker-activity path above), NOT a direct set: during a
                            // reconnection wave many peers' media flags settle at once,
                            // and a direct set per peer would drive a re-render storm.
                            // 50 ms coalescing collapses that into one re-render.
                            //
                            // NOTE: audio_enabled deliberately does NOT trigger a bump.
                            // Audio is independent of the decode-set/`visible`
                            // partition (it plays through NetEQ, not gated by
                            // `visible`), so bumping on audio toggles would add
                            // re-renders for no rendering benefit.
                            if sig.peek().video_enabled != state.video_enabled {
                                let v = peer_list_version;
                                schedule_throttled_bump(
                                    bump_pending.clone(),
                                    PEER_LIST_VERSION_THROTTLE_MS,
                                    Rc::new(move || {
                                        let mut v = v;
                                        let next = *v.peek() + 1;
                                        v.set(next);
                                    }),
                                );
                            }
                            sig.set(state);
                        }
                    } else {
                        // First event for this peer — create a new signal.
                        //
                        // #1465: no `peer_list_version` bump is needed here even when
                        // this first event carries `video_enabled = true`. A brand-new
                        // peer is created by `ensure_peer()` in
                        // `video_call_client::on_inbound_media`, which returns
                        // `PeerStatus::Added` and (at the END of the same
                        // synchronous call) emits `on_peer_added`, whose callback
                        // (~attendants.rs:1275) bumps `peer_list_version` directly.
                        // That bump fires AFTER the decode step that may have flipped
                        // `video_enabled` true, and `is_video_enabled_for_peer` reads
                        // the live `peer_decode_manager` state, so the parent
                        // re-render the `on_peer_added` bump triggers already sees the
                        // correct video state and partitions the new peer correctly.
                        // This `peer_status` event is delivered asynchronously
                        // (`global_sender().try_broadcast` -> `rx.recv().await`)
                        // strictly AFTER that bump, so it is redundant for the
                        // partition. Adding a second bump here would only add an
                        // extra re-render on every join.
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

    // --- Adaptive decode-budget control loop (issue #987, task 1a.3) ---
    //
    // A single lifecycle-scoped async task subscribes to the videocall
    // diagnostics bus and drives the decode-budget actuator. It is modelled on
    // the shared `peer_status` subscriber above: `subscribe()` inside a
    // `spawn`, then `while let Ok(evt) = rx.recv().await`.
    //
    // It consumes exactly two `client_perf` metrics (see
    // `videocall-client/src/{render_fps,long_tasks}.rs`):
    //   - `client_render_fps`           (f64, emitted at ~1 Hz)  → sample cadence
    //   - `client_longtask_duration_ms` (f64, event-driven)      → summed per bucket
    //
    // Every render-fps tick we close the current 1-second bucket: build a
    // `BudgetSample` from the latest FPS plus the *sum* of long-task durations
    // observed in that bucket (= longtask_ms_per_sec), push it into a rolling
    // ~5-sample window, then call `decide_step`. The control loop — not
    // `decide_step` — owns `BudgetState`: it increments `direction_hold` on each
    // consecutive recovery-qualifying sample, resets it to 0 the moment recovery
    // breaks, and on an applied Down/Up step updates `cap` and sets
    // `last_step_ms = now_ms`. Cadence is driven off the 1 Hz render-fps event,
    // never the 5 s health-reporter tick.
    //
    // Pressured-latch cap-ownership model (HCL #987 review FIX 1 + FIX 2,
    // replaces the old one-shot seed latches):
    //   - While NOT pressured (Auto): the render-side `effective_cap` tracks the
    //     live `natural` tile count exactly, so a capable machine shows ALL
    //     tiles (including staggered joins) with no avatars and NO dependence on
    //     this loop's cadence. This loop does NOT write `decode_budget_cap` on
    //     that path; it only keeps `state.cap` synced to `natural` so that, when
    //     pressure first hits, `decide_step`'s down-step starts from the value
    //     actually on screen rather than a stale seed. The FIRST time
    //     `decide_step` returns `Down`, the loop latches `decode_budget_pressured`
    //     true, applies the down-step, and writes `decode_budget_cap`.
    //   - While pressured (Auto): this loop is the SOLE owner of
    //     `decode_budget_cap`, running the existing Down/Up/Hold-growth logic
    //     (including the non-distress growth gate, which now correctly governs
    //     only RE-growth after real pressure). The latch stays set for the
    //     session (a machine that demonstrated it can struggle stays in
    //     conservative adaptive mode). It is reset on a Fixed -> Auto transition.
    let mut decode_budget_task: Signal<Option<dioxus_core::Task>> = use_signal(|| None);
    let client_for_budget = client.clone();
    use_effect(move || {
        let client_for_budget = client_for_budget.clone();
        let task = spawn(async move {
            let client_for_budget = client_for_budget.clone();
            use crate::components::decode_budget::{
                cascade_action, decide_step_with_median, in_distress, lower_layer_cap,
                median_render_fps, next_layer_drop_ms, non_distress_growth_allowed,
                non_distress_growth_qualifying, protective_emergency_cap,
                protective_encoder_layer_ceiling, re_arm_cascade_after_recovery,
                recovery_qualifying, settle_window_elapsed, severe_label, suppress_growth_step,
                tick_protective_mode, CascadeAction, DistressSignals, ProtectiveModeState,
                ProtectiveTransition, STEP_UP_COOLDOWN_MS, SUSTAIN_SAMPLES,
            };
            use crate::context::ProtectiveModeReport;
            use videocall_diagnostics::{now_ms, MetricValue};

            /// Rolling window length (~5 s at 1 Hz). Must be >= SUSTAIN_SAMPLES so
            /// `decide_step` always has a full sustain window once warmed up.
            const WINDOW: usize = 5;

            let mut rx = videocall_diagnostics::subscribe();
            // Rolling window of finished 1-second samples (most-recent-last).
            let mut samples: Vec<BudgetSample> = Vec::with_capacity(WINDOW);
            // Sum of long-task durations observed in the *current* (open) bucket.
            let mut longtask_bucket_ms: f64 = 0.0;
            // Issue #1558: MAX per-peer `audio_buffer_ms` observed in the current
            // (open) bucket, from "neteq" bus events. `None` until the first
            // reading arrives; reset on each fps tick (bucket close). Feeds the
            // protective-mode audio-distress + emergency triggers.
            let mut audio_buffer_bucket_ms_max: Option<f64> = None;
            // Issue #1558: the most recent closed-bucket audio-buffer max, carried
            // across ticks so the per-tick distress predicate has a value even on
            // ticks where no fresh neteq event arrived this second.
            let mut last_audio_buffer_ms_max: Option<f64> = None;
            // Issue #1558: latched protective-mode state, owned by THIS loop across
            // ticks (mirrors how the loop owns `BudgetState`). Drives the encoder
            // self-shed + emergency stages on top of the #1557 cascade.
            let mut protective = ProtectiveModeState::default();
            // Issue #1558: device capability score, read ONCE from the cached value
            // the console-log preamble stashed on `window.__videocall_capability_score`
            // (a benchmark already run at page load). We do NOT re-run the 100 ms
            // benchmark on this hot loop. `None` if the value is absent/unreadable —
            // the low-cap+crowded trigger then never fires (conservative).
            let cap_score: Option<u32> = read_cached_capability_score();
            // Issue #1558: escalating protective severity counter — how many
            // consecutive ticks protective mode has been active AND still at the
            // cascade floor. Drives the encoder self-shed from 2→1 layers. Reset to
            // 0 whenever protective mode is inactive or the cascade leaves floor.
            let mut protective_severity: u32 = 0;
            // #1286: is the Long Tasks API available on THIS browser at all?
            // Computed ONCE — it cannot change within a session. On WebKit
            // (desktop Safari + ALL iOS browsers) the `"longtask"`
            // PerformanceObserver entry type is unimplemented, so
            // `long_tasks::LongTaskObserver::start` returns `None` and NO
            // `client_longtask_duration_ms` events are ever emitted. There the
            // open bucket stays a perpetual 0.0 — indistinguishable from a
            // genuinely idle Chromium second. So we use the platform capability
            // (NOT the bucket value) as the discriminator: when the observer is
            // unsupported, every sample's `longtask` is `None` ("no telemetry"),
            // and the gate functions treat that conservatively (never confirm
            // idle / not-busy → never permit growth) instead of reading the
            // blind 0.0 as healthy. On a supported browser we emit
            // `Some(bucket_sum)` for every sample, including a legitimate idle
            // 0.0.
            //
            // TODO(#1024/#1025/#1020): WebKit/iOS has NO main-thread-saturation
            // signal today; the controller relies on FPS + the device-class
            // tile ceiling (`ios_decode_tile_ceiling`) there. A real iOS-valid
            // backpressure signal (decode-queue depth) is tracked by
            // #1024/#1025/#1020 — wire it in when it lands.
            let longtask_supported = !videocall_client::utils::is_webkit();
            // Controller-owned budget state. `cap` is seeded from the current
            // actuator value; switching the override Auto -> Fixed -> Auto
            // re-seeds it from whatever the cap is at that moment.
            let mut state = BudgetState {
                cap: *decode_budget_cap.peek(),
                last_step_ms: 0.0,
                direction_hold: 0,
                // #1557: cascade state starts clean on loop init. NOTE: this loop is
                // built ONCE (use_effect) and persists across reconnects — it is NOT
                // rebuilt per call session, and an in-place `client.connect()` reconnect
                // does not remount this component. So this initializer alone does NOT
                // protect against settle/at-floor leaking across a reconnect; that is
                // handled by the SESSION-RESET re-arm below (peer count collapsing to
                // MIN_CAP on `clear_all_peers`), which fires on BOTH WT and WS.
                last_layer_drop_ms: 0.0,
                layers_at_floor: false,
            };
            // Tracks the last override we acted on so we can detect a transition
            // back to Auto and cleanly re-seed `state` from the live cap.
            let mut last_override = *decode_budget_override.peek();
            // #1557: tracks the previous live tile count so the loop can detect a
            // SESSION-RESET edge (peer count collapsing to the MIN_CAP floor). On a
            // reconnect the client clears all peers (`clear_all_peers` runs on
            // ConnectionState::Failed, transport-agnostic), so `decode_budget_natural`
            // collapses to MIN_CAP and peers re-join FRESH at top layer; we re-arm the
            // cascade on that edge so stale at-floor/settle timing cannot leak across
            // the reconnect. Seeded from the live count so a session that starts with
            // peers already present does not spuriously fire on the first tick.
            let mut prev_natural = *decode_budget_natural.peek();

            while let Ok(evt) = rx.recv().await {
                // Issue #1558: the protective-mode predicate needs a per-peer
                // audio-buffer signal. The NetEQ audio decoder broadcasts
                // `audio_buffer_ms` on the SAME diagnostics bus, under subsystem
                // "neteq" (see neteq_audio_decoder::emit_buffer_metrics). We
                // observe it here WITHOUT a new control loop or refactor: track
                // the MAX buffer seen across peers in the current open bucket
                // (reset on each fps tick, exactly like the longtask bucket). A
                // "neteq" event never closes a bucket — only a render-fps event
                // does — so this just feeds the audio sub-signal.
                if evt.subsystem == "neteq" {
                    for m in &evt.metrics {
                        if m.name == "audio_buffer_ms" {
                            // The decoder emits this as a u64 ms value.
                            let buf = match &m.value {
                                MetricValue::U64(v) => Some(*v as f64),
                                MetricValue::F64(v) => Some(*v),
                                _ => None,
                            };
                            if let Some(buf) = buf {
                                audio_buffer_bucket_ms_max = Some(
                                    audio_buffer_bucket_ms_max.map_or(buf, |m: f64| m.max(buf)),
                                );
                            }
                        }
                    }
                    continue;
                }
                if evt.subsystem != "client_perf" {
                    continue;
                }

                // Accumulate long-task durations into the open bucket. These
                // arrive asynchronously between render-fps ticks.
                for m in &evt.metrics {
                    if m.name == "client_longtask_duration_ms" {
                        if let MetricValue::F64(dur) = &m.value {
                            longtask_bucket_ms += *dur;
                        }
                    }
                }

                // Only a render-fps event closes a bucket and advances the loop.
                let render_fps = evt.metrics.iter().find_map(|m| {
                    if m.name == "client_render_fps" {
                        if let MetricValue::F64(v) = &m.value {
                            return Some(*v);
                        }
                    }
                    None
                });
                let Some(fps) = render_fps else {
                    continue;
                };

                // Close the 1-second bucket into a sample and reset the bucket.
                // #1286: emit `None` (signal unavailable) on browsers where the
                // Long Tasks API is unsupported — NOT a blind `Some(0.0)`. The
                // discriminator is platform capability (`longtask_supported`,
                // computed once above), not the bucket value: an idle Chromium
                // second also produces a 0.0 bucket, so the value cannot tell
                // "no telemetry" from "idle".
                let sample = BudgetSample {
                    render_fps: Some(fps),
                    longtask: if longtask_supported {
                        Some(longtask_bucket_ms)
                    } else {
                        None
                    },
                };
                longtask_bucket_ms = 0.0;
                samples.push(sample);
                if samples.len() > WINDOW {
                    let overflow = samples.len() - WINDOW;
                    samples.drain(0..overflow);
                }
                // Issue #1558: close the audio-buffer bucket. Carry the last-seen
                // max forward when this second produced no fresh neteq reading, so
                // the distress predicate has a value on every tick. Reset the open
                // bucket for the next second.
                if audio_buffer_bucket_ms_max.is_some() {
                    last_audio_buffer_ms_max = audio_buffer_bucket_ms_max;
                }
                audio_buffer_bucket_ms_max = None;

                // ---- Override handling (DECISION: hard override) ----
                let current_override = *decode_budget_override.peek();
                let natural = *decode_budget_natural.peek();

                // Detect a return to Auto and re-seed BudgetState from the live
                // cap so the loop resumes cleanly without a phantom step.
                if current_override != last_override {
                    // User override engaging: distinguish user-chosen caps from
                    // auto-shed in triage. Fixed(n) = manual hard cap; Auto = resume.
                    match (last_override, current_override) {
                        (DecodeBudgetOverride::Auto, DecodeBudgetOverride::Fixed(n)) => {
                            log::info!(
                                "DecodeBudget: override=fixed n={} prev=auto natural={}",
                                n,
                                natural,
                            )
                        }
                        (DecodeBudgetOverride::Fixed(prev_n), DecodeBudgetOverride::Fixed(n)) => {
                            log::info!(
                                "DecodeBudget: override=fixed n={} prev=fixed prev_n={} natural={}",
                                n,
                                prev_n,
                                natural,
                            )
                        }
                        (DecodeBudgetOverride::Fixed(prev_n), DecodeBudgetOverride::Auto) => {
                            log::info!(
                                "DecodeBudget: override=auto prev=fixed prev_n={} natural={} cap={}",
                                prev_n,
                                natural,
                                *decode_budget_cap.peek(),
                            )
                        }
                        // Issue #1466: into/out of the `All` override.
                        (DecodeBudgetOverride::Auto, DecodeBudgetOverride::All) => {
                            log::info!("DecodeBudget: override=all prev=auto natural={natural}")
                        }
                        (DecodeBudgetOverride::Fixed(prev_n), DecodeBudgetOverride::All) => {
                            log::info!(
                                "DecodeBudget: override=all prev=fixed prev_n={prev_n} natural={natural}"
                            )
                        }
                        (DecodeBudgetOverride::All, DecodeBudgetOverride::Auto) => {
                            log::info!(
                                "DecodeBudget: override=auto prev=all natural={} cap={}",
                                natural,
                                *decode_budget_cap.peek(),
                            )
                        }
                        (DecodeBudgetOverride::All, DecodeBudgetOverride::Fixed(n)) => {
                            log::info!(
                                "DecodeBudget: override=fixed n={n} prev=all natural={natural}"
                            )
                        }
                        (DecodeBudgetOverride::All, DecodeBudgetOverride::All) => {}
                        (DecodeBudgetOverride::Auto, DecodeBudgetOverride::Auto) => {}
                    }
                    if current_override == DecodeBudgetOverride::Auto {
                        state = BudgetState {
                            cap: *decode_budget_cap.peek(),
                            last_step_ms: now_ms() as f64,
                            direction_hold: 0,
                            // #1557: reset cascade state on the Fixed->Auto resume
                            // so settle timing does not leak across the override
                            // transition (no phantom escalation to PauseTiles on
                            // the first re-pressured tick after resuming Auto).
                            last_layer_drop_ms: 0.0,
                            layers_at_floor: false,
                        };
                        // Loop-local hygiene: re-seed BudgetState so the loop
                        // resumes cleanly without a phantom step. The pressured
                        // latch reset now happens RENDER-SIDE (a `use_effect`
                        // watching `decode_budget_override`), so resuming Auto
                        // re-reveals all natural tiles immediately without waiting
                        // for this FPS-gated loop to advance (HCL #987 review).
                        // The loop re-latches pressured=true below only if it
                        // measures fresh pressure after the Auto resume.
                    }
                    last_override = current_override;
                }

                // Hard overrides (Fixed and All — issue #1466) bypass decide_step
                // entirely. `forced_cap` is the clamped target for each:
                //   - Fixed(n): clamp `n` into [MIN_CAP, natural ∩ CANVAS_LIMIT].
                //   - All:      the full natural count, clamped to
                //               [MIN_CAP, CANVAS_LIMIT] (decode all the layout
                //               shows). This parallels the render-side
                //               `effective_cap` All arm, which ignores `pressured`
                //               and the loop cap, so `All` reveals every tile on
                //               the next render without touching the latch.
                // The upper bound (natural ∩ CANVAS_LIMIT) is floored at MIN_CAP
                // so `clamp` can never see `max < min` (natural may be 0 before
                // peers join). MIN_CAP (1) < CANVAS_LIMIT, both consts.
                let forced_cap: Option<usize> = match current_override {
                    DecodeBudgetOverride::Fixed(n) => {
                        let upper = natural.clamp(MIN_CAP, CANVAS_LIMIT);
                        Some(n.clamp(MIN_CAP, upper))
                    }
                    DecodeBudgetOverride::All => Some(natural.clamp(MIN_CAP, CANVAS_LIMIT)),
                    DecodeBudgetOverride::Auto => None,
                };
                if let Some(forced) = forced_cap {
                    if *decode_budget_cap.peek() != forced {
                        decode_budget_cap.set(forced);
                    }
                    // Keep state.cap in sync so an Auto resume starts here.
                    state.cap = forced;
                    continue;
                }

                // ---- Auto path ----
                let now = now_ms() as f64;

                // #1557 reconnect re-arm: the budget loop is built ONCE (use_effect)
                // and lives across reconnects — it is NOT rebuilt per call session, and
                // the in-place `client.connect()` reconnect does NOT remount this
                // component. So `state` (incl. `layers_at_floor` / `last_layer_drop_ms`)
                // would otherwise PERSIST across a reconnect. On a reconnect the client
                // clears all peers, so `natural` collapses to the MIN_CAP floor and the
                // peers re-join fresh at top layer. Re-arm the cascade on that collapse
                // edge — `re_arm_cascade_after_recovery` clears `layers_at_floor` and
                // re-anchors `last_layer_drop_ms = now` (the SAME reset used by the Up
                // recovery arm) — so the next Down edge after re-join re-enters at
                // LowerLayer instead of routing straight to PauseTiles on stale at-floor
                // state. This is transport-agnostic: `clear_all_peers` runs on
                // ConnectionState::Failed for BOTH WebTransport and WebSocket. The same
                // edge also fires when the LAST peer legitimately leaves — which is
                // equally a correct moment to re-arm (no peers => nothing pressured =>
                // the cascade should be clean for the next arrival).
                if natural <= MIN_CAP && prev_natural > MIN_CAP {
                    re_arm_cascade_after_recovery(&mut state, now);
                    // Issue #1558: reset protective mode on the SAME session-reset
                    // edge as the #1557 cascade re-arm (transport-agnostic — fires
                    // on both WT and WS reconnect, and when the last peer leaves).
                    // Clear the latch, severity, and the carried audio reading so a
                    // fresh session starts un-protected with no stale encoder shed,
                    // and publish the cleared report so `Host` restores the user's
                    // encoder ceiling immediately.
                    protective = ProtectiveModeState::default();
                    protective_severity = 0;
                    last_audio_buffer_ms_max = None;
                    audio_buffer_bucket_ms_max = None;
                    let cleared = ProtectiveModeReport::default();
                    if *protective_mode_report.peek() != cleared {
                        protective_mode_report.set(cleared);
                    }
                }
                prev_natural = natural;

                let pressured = *decode_budget_pressured.peek();
                // Presenter-aware shedding (issue #1559): is the LOCAL user
                // screen-sharing right now? Read every tick so the bias appears
                // on share-start and vanishes on share-stop with no leaked state.
                // `is_sharing()` is true only for StreamReady/Active (the same
                // states that drive the screen ENCODER), so the extra shedding is
                // active exactly while the encoder is competing for CPU.
                let sharing = screen_share_state.peek().is_sharing();

                // ---- Issue #1558: protective mode (audio-first, speaker-priority) ----
                //
                // A THIN layer composed on top of the #1557 cascade, driven off
                // this SAME 1 Hz Auto tick (NOT a second loop). Each tick we build
                // the broader distress predicate, advance the latched protective
                // state with asymmetric hysteresis, and — when active — publish the
                // LOCAL encoder send-layer self-shed ceiling (stage 3, applied by
                // `Host`) and prime the emergency decode-cap clamp (stage 4, applied
                // at the end of this tick). The cascade (stages 1-2: layers→pause)
                // is UNCHANGED below; protective mode never reimplements it.
                let median_for_distress = median_render_fps(&samples, SUSTAIN_SAMPLES);
                let longtask_for_distress = if samples.len() >= SUSTAIN_SAMPLES {
                    // Sustained-window longtask: the MIN across the window (every
                    // sample must be heavy for sustained saturation). A `None`
                    // (WebKit/iOS) in the window ⇒ `None` (cannot confirm), exactly
                    // like `decide_step`'s conservative handling.
                    let window = &samples[samples.len() - SUSTAIN_SAMPLES..];
                    window
                        .iter()
                        .map(|s| s.longtask)
                        .try_fold(f64::INFINITY, |acc, lt| lt.map(|v| acc.min(v)))
                } else {
                    None
                };
                let participant_count = client_for_budget.peer_count().unwrap_or(0);
                let distress_signals = DistressSignals {
                    median_fps: median_for_distress,
                    longtask_ms_per_sec: longtask_for_distress,
                    max_peer_audio_buffer_ms: last_audio_buffer_ms_max,
                    // Deferred bus signal — see PROTECTIVE_NETEQ_ACCELERATE_DISTRESS_PER_SEC.
                    neteq_accelerate_per_sec: None,
                    cap_score,
                    participant_count,
                };
                let distressed = in_distress(distress_signals);
                let transition = tick_protective_mode(&mut protective, distressed);

                // Severity escalation: once protective mode is active AND the
                // cascade has reached floor (received layers at base + tiles
                // pausing), each consecutive such tick escalates the encoder shed
                // (2 layers → 1). Reset to 0 when inactive or off-floor so the next
                // episode re-enters at the gentler 2-layer shed.
                if protective.active && state.layers_at_floor {
                    protective_severity = protective_severity.saturating_add(1);
                } else {
                    protective_severity = 0;
                }
                // Stage 3 lever: the LOCAL encoder send-layer ceiling. `None` until
                // active AND at floor (ordered after stages 1-2). `Host` composes
                // this with the user's persisted "layers published" preference.
                let encoder_ceiling = protective_encoder_layer_ceiling(
                    protective.active,
                    state.layers_at_floor,
                    protective_severity.saturating_sub(1),
                );

                // Publish the report (change-gated so a child re-render only fires
                // on a real change — dioxus-signals 0.7.3 does not dedupe writes).
                let report = ProtectiveModeReport {
                    active: protective.active,
                    encoder_layer_ceiling: encoder_ceiling,
                };
                if *protective_mode_report.peek() != report {
                    protective_mode_report.set(report);
                }

                // Metric + log on the entry/exit edge (issue #1558 item 6),
                // following the #1566 diagnostics-bus pattern. The gauge is 1 while
                // active (0 on exit), with the naming the analysis tooling reads.
                match transition {
                    ProtectiveTransition::Entered => {
                        // Name the dominant trigger for triage. Order mirrors the
                        // severity intent: audio first (the thing being protected),
                        // then renderer/main-thread, then the structural low-cap case.
                        let trigger = if last_audio_buffer_ms_max
                            .map(|b| b > crate::components::decode_budget::PROTECTIVE_AUDIO_BUFFER_DISTRESS_MS)
                            .unwrap_or(false)
                        {
                            "audio_buffer"
                        } else if median_for_distress
                            .map(|m| m < crate::components::decode_budget::PROTECTIVE_FPS_DISTRESS)
                            .unwrap_or(false)
                        {
                            "fps"
                        } else if longtask_for_distress
                            .map(|lt| lt >= crate::components::decode_budget::PROTECTIVE_LONGTASK_DISTRESS_MS_PER_SEC)
                            .unwrap_or(false)
                        {
                            "longtask"
                        } else {
                            "low_cap_crowded"
                        };
                        log::warn!(
                            "ProtectiveMode: ENTERED trigger={trigger} median_fps={} longtask_ms_per_sec={} audio_buffer_ms={} cap_score={} participants={}",
                            median_for_distress.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                            longtask_for_distress.map(|l| format!("{l:.0}")).unwrap_or_else(|| "none".into()),
                            last_audio_buffer_ms_max.map(|b| format!("{b:.0}")).unwrap_or_else(|| "none".into()),
                            cap_score.map(|c| c.to_string()).unwrap_or_else(|| "none".into()),
                            participant_count,
                        );
                        videocall_diagnostics::global_sender()
                            .try_broadcast(videocall_diagnostics::DiagEvent {
                                subsystem: "protective_mode",
                                stream_id: None,
                                ts_ms: videocall_diagnostics::now_ms(),
                                metrics: vec![
                                    videocall_diagnostics::metric!("protective_mode_active", 1u64),
                                    videocall_diagnostics::metric!(
                                        "protective_mode_participants",
                                        participant_count as u64
                                    ),
                                ],
                            })
                            .ok();
                    }
                    ProtectiveTransition::Exited => {
                        log::info!(
                            "ProtectiveMode: EXITED median_fps={} audio_buffer_ms={} participants={}",
                            median_for_distress.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                            last_audio_buffer_ms_max.map(|b| format!("{b:.0}")).unwrap_or_else(|| "none".into()),
                            participant_count,
                        );
                        videocall_diagnostics::global_sender()
                            .try_broadcast(videocall_diagnostics::DiagEvent {
                                subsystem: "protective_mode",
                                stream_id: None,
                                ts_ms: videocall_diagnostics::now_ms(),
                                metrics: vec![videocall_diagnostics::metric!(
                                    "protective_mode_active",
                                    0u64
                                )],
                            })
                            .ok();
                    }
                    ProtectiveTransition::None => {}
                }

                // Issue #1558: audio-driven ownership latch. Protective mode can be
                // triggered by AUDIO alone (a backed-up jitter buffer) with the
                // renderer still healthy — so `decide_step` never returns Down and
                // the loop never latches `pressured` on its own. But the emergency
                // decode-pause (stage 4) can only ACTUATE while pressured (the
                // render-side `effective_cap` reads the loop cap only then). So when
                // protective mode is active AND audio is past the EMERGENCY mark,
                // latch the controller into cap ownership here, exactly as a
                // measured down-step would. This is the audio-first principle: a
                // healthy renderer with starving audio MUST still shed video to
                // protect audio. Mirrors the #1557 latch contract (the loop owns the
                // cap once latched); reset on the same session-reset edge.
                let emergency_now = protective.active
                    && last_audio_buffer_ms_max
                        .map(|b| {
                            b > crate::components::decode_budget::PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS
                        })
                        .unwrap_or(false);
                let pressured = if !pressured && emergency_now {
                    decode_budget_pressured.set(true);
                    log::warn!(
                        "ProtectiveMode: audio-driven pressured latch (renderer healthy, audio starving) audio_buffer_ms={}",
                        last_audio_buffer_ms_max
                            .map(|b| format!("{b:.0}"))
                            .unwrap_or_else(|| "none".into()),
                    );
                    true
                } else {
                    pressured
                };

                if !pressured {
                    // NOT-pressured path (HCL #987 review FIX 1 + FIX 2). The
                    // render-side `effective_cap` already shows ALL natural tiles
                    // here (including staggered joins), so this loop does NOT
                    // write `decode_budget_cap`. It only keeps `state.cap` synced
                    // to the live `natural` so that, when pressure FIRST hits,
                    // `decide_step` computes the down-step from the value actually
                    // on screen rather than a stale MIN_CAP seed. Likewise keep
                    // `direction_hold` book-keeping live so the strict-recovery
                    // gate is warm if we ever do step down then recover.
                    state.cap = natural.clamp(MIN_CAP, CANVAS_LIMIT);
                    // #1286 belt-and-suspenders: bind the device-class ceiling
                    // on the loop-owned state too, so `decode_budget_cap` can
                    // never internally ratchet above what `effective_cap` would
                    // display on iOS. Floored at MIN_CAP.
                    if let Some(ceiling) = device_decode_ceiling {
                        state.cap = state.cap.min(ceiling.max(MIN_CAP));
                    }

                    let qualifies = recovery_qualifying(&samples, SUSTAIN_SAMPLES);
                    if qualifies {
                        state.direction_hold = state.direction_hold.saturating_add(1);
                    } else {
                        state.direction_hold = 0;
                    }

                    // Only a DOWN decision matters before pressure: it latches
                    // the controller into ownership of the cap. Up/Hold are
                    // irrelevant here because the un-pressured cap already equals
                    // natural (the maximum the loop would ever grow to).
                    //
                    // Presenter-aware "step down sooner" (issue #1559): the normal
                    // latch fires on `decide_step -> Down` (median FPS < 24, the
                    // distress floor). While SHARING, a presenter also latches on
                    // the milder 24-30 band via `presenter_extra_shed_pressure` —
                    // synthesised as a single-tile `Down(1)` so the existing
                    // latch/log path is reused unchanged. The normal trigger is
                    // never weakened (it is OR-ed in, taking priority and keeping
                    // its proportional magnitude); the presenter branch only ADDS
                    // the milder band, and ONLY while sharing — when sharing stops
                    // `presenter_extra_shed_pressure` returns false and the normal
                    // trigger is the sole latch path again. Pressure-gated: a
                    // presenter at a healthy >= 30 fps satisfies neither trigger.
                    // One-time pressured-latch EDGE (not per tick): use the plain
                    // `decide_step` wrapper here — its internal median recompute is
                    // irrelevant on this rare edge (contrast the per-tick pressured
                    // path below, which threads the hoisted `median_for_distress`
                    // into `decide_step_with_median`, issue #1558 / #1001).
                    let latch_step = match decide_step(&samples, &state, natural, now) {
                        BudgetStep::Down(magnitude) => Some(magnitude),
                        _ if presenter_extra_shed_pressure(&samples, sharing)
                            && state.cap > MIN_CAP =>
                        {
                            Some(1)
                        }
                        _ => None,
                    };
                    if let Some(magnitude) = latch_step {
                        // Telemetry for the decision logs below. Reuses the
                        // single per-tick `median_for_distress` (issue #1558 perf
                        // hoist — same value, no second `median_render_fps`
                        // alloc+sort here). #1286: render a `None` longtask (signal
                        // unavailable on WebKit/iOS) as "none", mirroring how
                        // `median`/`cur_fps` render their missing case, so the log
                        // never implies a healthy 0.0 where there is simply no
                        // telemetry. All three are used in BOTH cascade arms' logs
                        // below, so none is ever unused.
                        let median = median_for_distress;
                        let cur_fps = samples.last().and_then(|s| s.render_fps);
                        let longtask = samples.last().and_then(|s| s.longtask);

                        // Issue #1557: latch the controller on the FIRST measured
                        // down-pressure REGARDLESS of cascade stage. The latch means
                        // the controller now owns the cap; it MUST latch the moment
                        // pressure is first measured — even on a LowerLayer tick that
                        // does not touch the cap — otherwise the loop never re-enters
                        // this pressured arm to keep cascading (it would fall back to
                        // the un-pressured branch and re-reveal natural tiles).
                        decode_budget_pressured.set(true);

                        // Issue #1557 — tier-before-pause cascade. A Down edge first
                        // lowers RECEIVED simulcast layers (resolution) and only
                        // escalates to PAUSING (capping) tiles once those layers are
                        // at floor AND a settle window has elapsed.
                        let down_pressure = true;
                        let settle_elapsed = settle_window_elapsed(now, state.last_layer_drop_ms);
                        let action =
                            cascade_action(down_pressure, state.layers_at_floor, settle_elapsed);
                        match action {
                            CascadeAction::LowerLayer => {
                                // Stage 1: drop received layers ONLY — NO tile is
                                // paused on this edge.
                                //
                                // #1557 BLOCKER FIX: the un-pressured phase keeps only
                                // the LOCAL `state.cap` synced to natural and never
                                // writes the `decode_budget_cap` SIGNAL (see the
                                // `!pressured` block above), so the signal still holds
                                // the Auto seed `MIN_CAP` (1) at this first Down edge.
                                // The render reads the SIGNAL: `effective_cap` would
                                // return MIN_CAP and pause N-1 of N tiles — inverting
                                // #1557. So on a LowerLayer outcome we PIN the cap to
                                // the displayed (natural) value and WRITE the signal,
                                // using the SAME device-ceiling clamp the un-pressured
                                // sync uses (NOT `presenter_cap_ceiling`, which belongs
                                // to the PauseTiles arm and sheds tiles). Result:
                                // `effective_cap` returns natural — no tile paused —
                                // while only received layers drop.
                                // #1557 perf: dioxus-signals 0.7.3 does NOT dedupe
                                // equal writes, so an unconditional `.set` every ~1s
                                // forces an AttendantsComponent re-render each tick.
                                // Change-gate the write. On the FIRST LowerLayer tick
                                // the signal still holds the Auto-seed MIN_CAP while
                                // `new_cap == natural > MIN_CAP`, so the guard is true
                                // and the write still fires.
                                let new_cap = lower_layer_cap(natural, device_decode_ceiling);
                                if new_cap != *decode_budget_cap.peek() {
                                    decode_budget_cap.set(new_cap);
                                }
                                state.cap = new_cap;
                                // The chooser steps each peer down a rung; when it
                                // reports nothing moved (`!stepped`) every received
                                // layer is at base, so the next Down edge can escalate.
                                let stepped = match client_for_budget
                                    .apply_local_cpu_pressure_congestion()
                                {
                                    Some(s) => {
                                        state.layers_at_floor = !s;
                                        // #1557 CRITICAL: advance the settle clock ONLY
                                        // when a layer ACTUALLY moved (see the steady
                                        // Down arm for the full rationale): freezing the
                                        // timestamp at floor lets STEP_DOWN_COOLDOWN_MS
                                        // accumulate so the cascade can escalate to
                                        // PauseTiles instead of looping LowerLayer.
                                        state.last_layer_drop_ms =
                                            next_layer_drop_ms(state.last_layer_drop_ms, now, s);
                                        s
                                    }
                                    None => {
                                        // borrow contended: skip this tick, do NOT advance the
                                        // cascade (layers_at_floor + settle clock frozen). `stepped`
                                        // logs false but layers_at_floor was NOT set true — that is
                                        // the intended "no movement" behavior, not an inconsistency.
                                        false
                                    }
                                };
                                log::info!(
                                    "DecodeBudget: cascade=lower_layer pressured_latch=true stepped={} layers_at_floor={} natural={} cap={} median_fps={} current_fps={} longtask_ms_per_sec={}",
                                    stepped,
                                    state.layers_at_floor,
                                    natural,
                                    state.cap,
                                    median.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                                    cur_fps.map(|f| format!("{f:.1}")).unwrap_or_else(|| "none".into()),
                                    longtask.map(|lt| format!("{lt:.0}")).unwrap_or_else(|| "none".into()),
                                );
                            }
                            CascadeAction::PauseTiles => {
                                // Stage 2: received layers are at floor and the settle
                                // window elapsed — NOW pause (cap) a tile. This is the
                                // ORIGINAL latch-edge down-step logic, unchanged.
                                let prev_cap = natural.clamp(MIN_CAP, CANVAS_LIMIT);
                                state.cap = natural.saturating_sub(magnitude).max(MIN_CAP);
                                // Presenter-aware "lower floor" (issue #1559): while
                                // sharing, clamp the just-latched cap to the presenter
                                // ceiling (the fraction `ceil(natural * PRESENTER_SHED_FACTOR)`
                                // bounded above by the absolute `PRESENTER_RESIDUAL_FLOOR`,
                                // so a large meeting sheds to a small fixed residual) — the
                                // first shed already frees substantial peer-decode CPU for
                                // the screen encoder. `None` while not sharing leaves the
                                // cap exactly as the normal down-step produced it.
                                if let Some(ceiling) = presenter_cap_ceiling(natural, sharing) {
                                    state.cap = state.cap.min(ceiling.max(MIN_CAP));
                                }
                                state.last_step_ms = now;
                                state.direction_hold = 0;
                                decode_budget_cap.set(state.cap);

                                // Pressured-latch edge (false->true): the controller now
                                // owns the cap. Trigger is the first measured down-step.
                                log::info!(
                                    "DecodeBudget: pressured_latch=true trigger=down median_fps={} current_fps={} longtask_ms_per_sec={} natural={} cap={}",
                                    median.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                                    cur_fps.map(|f| format!("{f:.1}")).unwrap_or_else(|| "none".into()),
                                    longtask.map(|lt| format!("{lt:.0}")).unwrap_or_else(|| "none".into()),
                                    natural,
                                    state.cap,
                                );
                                // Severe-tier entry: a multi-tile down-step. The label
                                // reproduces `decide_step`'s catastrophic test exactly
                                // (median FPS + SUSTAINED long-task window), NOT a single
                                // closing-sample inference. WITHOUT changing decide_step's
                                // signature.
                                if magnitude > 1 {
                                    log::info!(
                                        "DecodeBudget: severe_step magnitude={} threshold={} median_fps={} longtask_ms_per_sec={}",
                                        magnitude,
                                        severe_label(&samples, median),
                                        median.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                                        longtask.map(|lt| format!("{lt:.0}")).unwrap_or_else(|| "none".into()),
                                    );
                                }
                                // First cap transition (un-pressured -> pressured down-step).
                                // #1001: gate on an actual change (mirroring the steady
                                // Auto arm) so a clamp-collapsed `prev_cap` can never log a
                                // no-op `cap N->N`; the log always reflects real movement.
                                if state.cap != prev_cap {
                                    log::info!(
                                        "DecodeBudget: cap {}->{} dir=down magnitude={} pressured=true median_fps={} current_fps={} longtask_ms_per_sec={} natural={}",
                                        prev_cap,
                                        state.cap,
                                        magnitude,
                                        median.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                                        cur_fps.map(|f| format!("{f:.1}")).unwrap_or_else(|| "none".into()),
                                        longtask.map(|lt| format!("{lt:.0}")).unwrap_or_else(|| "none".into()),
                                        natural,
                                    );
                                }
                                // At-floor nudge: re-issue the layer-drop seed. Harmless
                                // and idempotent — every received chooser is already at
                                // base so `apply` returns false (`layers_at_floor`
                                // stays true), but it keeps the published preference
                                // re-advertised. Advance the settle clock ONLY if a
                                // layer actually moved (at floor it does not, so the
                                // timestamp stays frozen — keeping settle_elapsed true
                                // so sustained pressure keeps shedding tiles).
                                if let Some(stepped) =
                                    client_for_budget.apply_local_cpu_pressure_congestion()
                                {
                                    state.layers_at_floor = !stepped;
                                    state.last_layer_drop_ms =
                                        next_layer_drop_ms(state.last_layer_drop_ms, now, stepped);
                                }
                                // None (borrow contended): leave layers_at_floor + settle clock unchanged
                            }
                            CascadeAction::None => {
                                // unreachable: down_pressure is true here
                            }
                        }
                    }
                    continue;
                }

                // ---- Pressured Auto path: the loop is the sole cap owner ----
                // Reuse the single per-tick `median_for_distress` (issue #1558
                // perf hoist): the protective distress predicate already computed
                // `median_render_fps(&samples, SUSTAIN_SAMPLES)` at the top of this
                // tick, so threading it in here avoids `decide_step` re-running the
                // same `Vec`-alloc+sort a second time on the hot steady-state path
                // (restores the #1001 "one median per tick" contract).
                // Issue #1558 emergency-growth gate (Up arm): coerce a recovery
                // `Up` to `Hold` while the protective EMERGENCY is active this tick
                // (`emergency_now`). `decide_step`'s Up gate is blind to audio, so a
                // healthy renderer with a starving jitter buffer would otherwise
                // raise the cap + call `re_arm_cascade_after_recovery` (clearing
                // `layers_at_floor`), and the emergency clamp below would re-slam the
                // cap to MIN_CAP WITHOUT restoring the floor flag — flipping the
                // stage-3 encoder ceiling to None for a tick (the ~4s flap). The
                // coerced `Hold` then has its own growth vetoed by
                // `non_distress_growth_allowed(.., emergency_now)`, so neither growth
                // path fights the emergency. `suppress_growth_step` is the single
                // source of truth for this Up suppression (shared with the
                // `sim_tick_protective` test model).
                let step = suppress_growth_step(
                    decide_step_with_median(&samples, &state, natural, now, median_for_distress),
                    emergency_now,
                );

                // Controller owns direction_hold: increment per consecutive
                // recovery-qualifying sample, reset to 0 when recovery breaks.
                // We call the SAME `recovery_qualifying` helper that
                // `decide_step` uses for its step-up gate (HCL #987 review
                // FIX 6) so the two can never silently drift apart.
                let qualifies = recovery_qualifying(&samples, SUSTAIN_SAMPLES);
                if qualifies {
                    state.direction_hold = state.direction_hold.saturating_add(1);
                } else {
                    state.direction_hold = 0;
                }

                // Apply the step: the controller owns cap + last_step_ms.
                //
                // #1001: the decision-log telemetry (`median` / `cur_fps` /
                // `longtask`) is computed INSIDE the Down / Up / growth branches
                // that actually emit it — never before this `match`.
                // `median_render_fps` is a `Vec`-alloc + sort (NOT a cheap copy),
                // so hoisting it here would re-run it on every steady-state Hold
                // tick for a value Hold never uses. Each branch computes it only
                // when it is about to log. (`cur_fps` / `longtask` are cheap
                // `Option<f64>` reads; they ride along for locality.)
                match step {
                    BudgetStep::Down(magnitude) => {
                        // Issue #1557 — tier-before-pause cascade on the steady
                        // pressured Down arm. Drop RECEIVED layers first; pause
                        // (cap) tiles only once layers are at floor AND the settle
                        // window has elapsed. `magnitude` is consumed in the
                        // PauseTiles arm (proportional tile-shed); LowerLayer leaves
                        // the cap untouched.
                        let settle_elapsed = settle_window_elapsed(now, state.last_layer_drop_ms);
                        let action = cascade_action(true, state.layers_at_floor, settle_elapsed);
                        match action {
                            CascadeAction::LowerLayer => {
                                // Stage 1: drop received layers ONLY — no tile is
                                // paused on this tick.
                                //
                                // #1557 BLOCKER FIX (mirrors the latch-site arm): PIN
                                // the cap to natural and WRITE `decode_budget_cap` so
                                // `effective_cap` shows ALL natural tiles. The cap may
                                // currently hold a PAUSED value (a prior PauseTiles
                                // dropped it, then recovery cleared `layers_at_floor`
                                // and a fresh Down re-entered LowerLayer); re-pinning
                                // natural here guarantees a LowerLayer tick never leaves
                                // a stale paused cap visible. Same device-ceiling clamp
                                // as the un-pressured sync; NOT `presenter_cap_ceiling`
                                // (that sheds tiles and belongs to the PauseTiles arm).
                                // #1557 perf: dioxus-signals 0.7.3 does NOT dedupe
                                // equal writes, so an unconditional `.set` every ~1s
                                // forces an AttendantsComponent re-render each tick.
                                // Change-gate the write. On the FIRST LowerLayer tick
                                // the signal still holds the Auto-seed MIN_CAP while
                                // `new_cap == natural > MIN_CAP`, so the guard is true
                                // and the write still fires.
                                let new_cap = lower_layer_cap(natural, device_decode_ceiling);
                                if new_cap != *decode_budget_cap.peek() {
                                    decode_budget_cap.set(new_cap);
                                }
                                state.cap = new_cap;
                                let prev_layers_at_floor = state.layers_at_floor;
                                let stepped = match client_for_budget
                                    .apply_local_cpu_pressure_congestion()
                                {
                                    Some(s) => {
                                        state.layers_at_floor = !s;
                                        // #1557 CRITICAL: advance the settle clock ONLY
                                        // when a layer ACTUALLY moved (`s`). Once at
                                        // floor the apply is a no-op every tick; if we
                                        // reset the clock on those no-op ticks the
                                        // `now - last_layer_drop_ms` delta is pinned at
                                        // one tick-gap and STEP_DOWN_COOLDOWN_MS never
                                        // elapses, so PauseTiles would be unreachable.
                                        // By freezing the timestamp at floor, the settle
                                        // window accumulates from the moment the floor
                                        // was first reached and the cascade can escalate
                                        // to pausing tiles.
                                        state.last_layer_drop_ms =
                                            next_layer_drop_ms(state.last_layer_drop_ms, now, s);
                                        s
                                    }
                                    None => {
                                        // borrow contended: skip this tick, do NOT advance the
                                        // cascade (layers_at_floor + settle clock frozen). `stepped`
                                        // logs false but layers_at_floor was NOT set true — that is
                                        // the intended "no movement" behavior, not an inconsistency.
                                        false
                                    }
                                };
                                // A Down edge ends the recovery streak even when only
                                // layers dropped (anti-oscillation): the next up-step
                                // must re-earn RECOVERY_HOLD samples.
                                state.direction_hold = 0;
                                // Refresh last_step_ms so the non-distress growth gate
                                // below is held off by the up-cooldown — a layer-drop
                                // tick must not let the cap re-grow on the same window.
                                state.last_step_ms = now;
                                // #1557 perf: gate the log (and its `median_render_fps`
                                // Vec-alloc + sort, plus the three `format!`s) on a
                                // TRANSITION — a real layer move (`stepped`) or the
                                // floor flip — instead of emitting it every second
                                // under sustained at-floor pressure. This mirrors the
                                // movement-gate discipline of the `cap N->N` logs and
                                // keeps the support-triage signal (each real drop + the
                                // floor transition) without per-tick log/alloc churn.
                                let floor_flipped = state.layers_at_floor != prev_layers_at_floor;
                                if stepped || floor_flipped {
                                    let median = median_for_distress;
                                    let cur_fps = samples.last().and_then(|s| s.render_fps);
                                    let longtask = samples.last().and_then(|s| s.longtask);
                                    log::info!(
                                        "DecodeBudget: cascade=lower_layer pressured_latch=false stepped={} layers_at_floor={} natural={} cap={} median_fps={} current_fps={} longtask_ms_per_sec={}",
                                        stepped,
                                        state.layers_at_floor,
                                        natural,
                                        state.cap,
                                        median.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                                        cur_fps.map(|f| format!("{f:.1}")).unwrap_or_else(|| "none".into()),
                                        longtask.map(|lt| format!("{lt:.0}")).unwrap_or_else(|| "none".into()),
                                    );
                                }
                            }
                            CascadeAction::PauseTiles => {
                                // Stage 2: received layers at floor + settle elapsed —
                                // NOW pause (cap) a tile. This is the ORIGINAL steady
                                // down-step body, unchanged.
                                // Proportional/multi-tile down-step (HCL #987 review
                                // FIX 4): `magnitude` is 1 under mild pressure, larger
                                // under catastrophic pressure. Floor at MIN_CAP.
                                // #1558 perf hoist: reuse the single per-tick
                                // `median_for_distress` (same value) instead of a
                                // second alloc+sort. A Down only fires when
                                // `cap > MIN_CAP` (decide_step guard), so it always
                                // strictly lowers the cap and the cap log below always
                                // fires — these are never unused.
                                let median = median_for_distress;
                                let cur_fps = samples.last().and_then(|s| s.render_fps);
                                let longtask = samples.last().and_then(|s| s.longtask);
                                let prev_cap = state.cap;
                                state.cap = state.cap.saturating_sub(magnitude).max(MIN_CAP);
                                state.last_step_ms = now;
                                // A down-step ends the recovery streak. Because the
                                // last_step_ms is updated here, the non-distress growth
                                // gate below is held off by the up-cooldown, so a machine
                                // that just dropped a tile under pressure cannot
                                // instantly re-add it (anti-oscillation).
                                state.direction_hold = 0;
                                decode_budget_cap.set(state.cap);
                                // Severe-tier entry: multi-tile down-step. The label
                                // reproduces `decide_step`'s catastrophic test exactly
                                // (median FPS + SUSTAINED long-task window), NOT a single
                                // closing-sample inference. No `decide_step` signature
                                // change.
                                if magnitude > 1 {
                                    log::info!(
                                        "DecodeBudget: severe_step magnitude={} threshold={} median_fps={} longtask_ms_per_sec={}",
                                        magnitude,
                                        severe_label(&samples, median),
                                        median.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                                        longtask.map(|lt| format!("{lt:.0}")).unwrap_or_else(|| "none".into()),
                                    );
                                }
                                if state.cap != prev_cap {
                                    log::info!(
                                        "DecodeBudget: cap {}->{} dir=down magnitude={} pressured=true median_fps={} current_fps={} longtask_ms_per_sec={} natural={}",
                                        prev_cap,
                                        state.cap,
                                        magnitude,
                                        median.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                                        cur_fps.map(|f| format!("{f:.1}")).unwrap_or_else(|| "none".into()),
                                        longtask.map(|lt| format!("{lt:.0}")).unwrap_or_else(|| "none".into()),
                                        natural,
                                    );
                                }
                                // At-floor nudge: re-issue the layer-drop seed. Harmless
                                // and idempotent (all received choosers already at base),
                                // keeps the published preference fresh. As in the
                                // LowerLayer arm, advance the settle clock ONLY if a
                                // layer actually moved — at floor this is a no-op so the
                                // timestamp stays frozen, which is correct: once we are
                                // pausing tiles we want settle_elapsed to STAY true so
                                // subsequent ticks keep shedding tiles under sustained
                                // pressure rather than dropping back to LowerLayer.
                                if let Some(stepped) =
                                    client_for_budget.apply_local_cpu_pressure_congestion()
                                {
                                    state.layers_at_floor = !stepped;
                                    state.last_layer_drop_ms =
                                        next_layer_drop_ms(state.last_layer_drop_ms, now, stepped);
                                }
                                // None (borrow contended): leave layers_at_floor + settle clock unchanged
                            }
                            CascadeAction::None => {
                                // unreachable: down_pressure is true here
                            }
                        }
                    }
                    // Issue #1558: this Up arm is never reached during an active
                    // protective emergency — `suppress_growth_step` (above) has
                    // already coerced `Up` to `Hold` when `emergency_now`, so the
                    // cap-raise + `re_arm_cascade_after_recovery` here cannot clear
                    // `layers_at_floor` while audio is starving (the encoder-ceiling
                    // flap). On a normal (non-emergency) tick it runs unchanged.
                    BudgetStep::Up => {
                        // Issue #1557: recovery reverses the cascade order. Tiles
                        // un-pause HERE (the cap raise below); RECEIVED layers
                        // re-grow via the choosers' existing clean-window recovery on
                        // the monitor tick (layer_chooser.rs `choose` clean-window /
                        // sticky cautious recovery) — no explicit layer-raise call is
                        // needed here.
                        //
                        // Recovery RE-ARMS the cascade: clearing `layers_at_floor` and
                        // re-anchoring `last_layer_drop_ms = now` means the NEXT Down
                        // edge starts at the LowerLayer stage and must re-earn the
                        // settle window before pausing a tile — so the re-grown
                        // received layers are dropped FIRST on the next pressure
                        // episode (not paused). Without this re-arm the controller
                        // stays pressured through recovery (the latch is only cleared
                        // on the Fixed/All->Auto override, never here), so the next
                        // Down edge would see a STALE `layers_at_floor == true` plus a
                        // stale `last_layer_drop_ms` and `cascade_action` would route
                        // straight to PauseTiles — inverting the feature on every
                        // cycle after the first. These two resets are UNGATED (run on
                        // every Up-arm execution, NOT behind the `cap != prev_cap` log
                        // gate below): reaching this arm means decide_step chose an
                        // up-step, i.e. recovery is in progress, even on the rare tick
                        // where the cap was already at natural and did not move.
                        // `re_arm_cascade_after_recovery` is the single source of
                        // truth for this reset (shared with the Hold-growth path).
                        re_arm_cascade_after_recovery(&mut state, now);
                        let prev_cap = state.cap;
                        state.cap = (state.cap + 1).min(natural.max(MIN_CAP));
                        // #1286 belt-and-suspenders: never grow past the
                        // device-class ceiling. (On iOS the up-step gate
                        // `recovery_qualifying` already returns false for the
                        // blind longtask, so this rarely fires — but the clamp
                        // guarantees the cap can't exceed the ceiling on any path.)
                        if let Some(ceiling) = device_decode_ceiling {
                            state.cap = state.cap.min(ceiling.max(MIN_CAP));
                        }
                        state.last_step_ms = now;
                        // A consumed up-step resets the recovery streak so the
                        // next up-step must re-earn RECOVERY_HOLD samples.
                        state.direction_hold = 0;
                        decode_budget_cap.set(state.cap);
                        if state.cap != prev_cap {
                            // #1558 perf hoist: reuse the single per-tick
                            // `median_for_distress` (same value), logged only when
                            // the up-step actually moves the cap.
                            let median = median_for_distress;
                            let cur_fps = samples.last().and_then(|s| s.render_fps);
                            let longtask = samples.last().and_then(|s| s.longtask);
                            log::info!(
                                "DecodeBudget: cap {}->{} dir=up magnitude=1 pressured=true median_fps={} current_fps={} longtask_ms_per_sec={} natural={}",
                                prev_cap,
                                state.cap,
                                median.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                                cur_fps.map(|f| format!("{f:.1}")).unwrap_or_else(|| "none".into()),
                                longtask.map(|lt| format!("{lt:.0}")).unwrap_or_else(|| "none".into()),
                                natural,
                            );
                        }
                    }
                    BudgetStep::Hold => {
                        // Non-distress growth gate (HCL #987 review FIX 1).
                        //
                        // `decide_step` returned Hold, which means it did not see
                        // a *strict-recovery* up-step (that path needs median FPS
                        // >= FPS_STEP_UP=30 + RECOVERY_HOLD + up-cooldown). But a
                        // perfectly healthy machine on a 30 Hz panel reports ~29
                        // fps — it sits in the 24-30 hysteresis band forever and
                        // would NEVER reach natural through the strict gate. That
                        // is the dead-band trap the previous warm-up climb was
                        // trying (and failing) to paper over.
                        //
                        // The rule we use to grow the cap toward `natural` here is
                        // the COMPLEMENT of the step-DOWN condition — "not under
                        // measured pressure" — rather than the strict recovery
                        // gate:
                        //
                        //     median_fps >= FPS_STEP_DOWN  (>= 24, the distress
                        //                                   floor; INCLUDES the
                        //                                   24-30 band)
                        //   AND every sample's longtask < LONGTASK_BUSY_MS_PER_SEC
                        //
                        // Why this avoids the dead band: a steady 29 fps idle
                        // machine satisfies `>= FPS_STEP_DOWN`, so the cap can
                        // RE-grow back toward == natural and HOLD there after a
                        // pressure-driven down-step. (This arm only runs once
                        // pressured; the un-pressured cap already equals natural.)
                        //
                        // Why this still preserves anti-oscillation: growth is
                        // rate-limited to one tile per STEP_UP_COOLDOWN_MS using
                        // the SAME `last_step_ms` that the Down arm refreshes. So
                        // a machine that just dropped a tile under real pressure
                        // cannot re-add it until a full up-cooldown has elapsed
                        // with no further down-step — exactly the behaviour the
                        // strict recovery gate gives, without excluding the 24-30
                        // band. A genuinely flapping machine keeps tripping the
                        // down condition (which refreshes last_step_ms and resets
                        // direction_hold), so the up-cooldown never elapses and
                        // the cap does not yo-yo. The strict recovery gate in
                        // `decide_step` is simply the stricter subset of this
                        // rule and remains the path that fires when FPS is in the
                        // healthy >= 30 band.
                        // `non_distress_growth_qualifying` is the single source
                        // of truth for the non-distress condition (and returns
                        // false on a short/incomplete window, so no underflow).
                        // #1286 belt-and-suspenders: lower the growth target to
                        // the device-class ceiling so non-distress growth can
                        // never push the cap past it. (On iOS the gate
                        // `non_distress_growth_qualifying` already returns false
                        // for the blind longtask, so this arm rarely runs there;
                        // capping the target is the guarantee regardless.)
                        let mut target = match device_decode_ceiling {
                            Some(ceiling) => natural.max(MIN_CAP).min(ceiling.max(MIN_CAP)),
                            None => natural.max(MIN_CAP),
                        };
                        // Presenter-aware "lower floor" (issue #1559): while
                        // sharing, cap the non-distress GROWTH target at the
                        // presenter ceiling so the budget does not re-grow peer
                        // tiles back into the CPU the screen encoder needs. When
                        // sharing stops, `presenter_cap_ceiling` returns `None` and
                        // the target reverts to natural (∩ device ceiling), so the
                        // existing growth path re-grows tiles — recovery on stop.
                        if let Some(ceiling) = presenter_cap_ceiling(natural, sharing) {
                            target = target.min(ceiling.max(MIN_CAP));
                        }
                        let up_cooldown_elapsed = (now - state.last_step_ms) >= STEP_UP_COOLDOWN_MS;
                        let not_distressed =
                            non_distress_growth_qualifying(&samples, SUSTAIN_SAMPLES);
                        // Issue #1558 emergency-growth gate: `non_distress_growth_allowed`
                        // is the single source of truth combining the three pre-existing
                        // growth conditions with the `!emergency_now` veto. The growth
                        // gate is blind to audio, so during a SUSTAINED audio-only
                        // emergency it would otherwise raise the cap (1→2) and call
                        // `re_arm_cascade_after_recovery` here — clearing
                        // `layers_at_floor` — only for the emergency clamp below to
                        // re-slam the cap to MIN_CAP WITHOUT restoring the floor flag,
                        // flipping the stage-3 encoder ceiling to None for a tick and
                        // un-shedding the local send-ladder on a ~4s cycle. Vetoing
                        // growth while `emergency_now` holds keeps `layers_at_floor`
                        // stable so the encoder ceiling stays applied; when audio
                        // recovers the veto lifts and growth resumes unchanged.
                        if non_distress_growth_allowed(
                            state.cap < target,
                            up_cooldown_elapsed,
                            not_distressed,
                            emergency_now,
                        ) {
                            let prev_cap = state.cap;
                            state.cap += 1;
                            state.last_step_ms = now;
                            // Issue #1557: non-distress growth is also a recovery
                            // signal (the cap climbs back toward natural while still
                            // latched-pressured), so RE-ARM the cascade exactly as the
                            // BudgetStep::Up arm does — clear `layers_at_floor` and
                            // re-anchor `last_layer_drop_ms = now`. This is reachable
                            // to a later Down edge with a stale flag: the pressured
                            // latch persists through recovery, decide_step can return
                            // Down on a subsequent bad window, and nothing else clears
                            // the flag — so without this the next Down edge would route
                            // straight to PauseTiles. Gated INSIDE this block (an
                            // ACTUAL upward cap move): a steady-state Hold tick that
                            // does NOT grow must NOT re-arm, or it would perpetually
                            // reset the settle clock and mask a real at-floor state.
                            // Shared source of truth with the Up arm.
                            re_arm_cascade_after_recovery(&mut state, now);
                            decode_budget_cap.set(state.cap);
                            // #1558 perf hoist: reuse the single per-tick
                            // `median_for_distress` (same value), logged only on an
                            // actual growth step; a steady-state Hold tick never
                            // reaches here.
                            let median = median_for_distress;
                            let cur_fps = samples.last().and_then(|s| s.render_fps);
                            let longtask = samples.last().and_then(|s| s.longtask);
                            // Non-distress growth: cap re-grows toward natural while
                            // `decide_step` is Holding. dir=growth distinguishes this
                            // from the strict-recovery dir=up step above.
                            log::info!(
                                "DecodeBudget: cap {}->{} dir=growth magnitude=1 pressured=true median_fps={} current_fps={} longtask_ms_per_sec={} natural={}",
                                prev_cap,
                                state.cap,
                                median.map(|m| format!("{m:.1}")).unwrap_or_else(|| "none".into()),
                                cur_fps.map(|f| format!("{f:.1}")).unwrap_or_else(|| "none".into()),
                                longtask.map(|lt| format!("{lt:.0}")).unwrap_or_else(|| "none".into()),
                                natural,
                            );
                        }
                    }
                }

                // Presenter-aware "lower floor" — post-step clamp (issue #1559).
                //
                // Issue #1557: this clamp COMPOSES with the tier-before-pause
                // cascade and is UNCHANGED. On a LowerLayer tick the cap was not
                // lowered, so this clamp may still shed tiles if sharing AND the cap
                // is above the presenter ceiling — that is pre-existing,
                // independent presenter behaviour (it bounds the cap regardless of
                // the cascade stage) and is intentionally left intact.
                //
                // The growth-target cap above prevents the pressured loop from
                // GROWING past the presenter ceiling, but it does not lower a cap
                // that is ALREADY above the ceiling when sharing begins. A user
                // who starts sharing while already pressured (e.g. the loop had
                // settled at cap == natural - 2) must shed down to the presenter
                // ceiling promptly, not wait for FPS to dip further. This clamp
                // lowers `state.cap` to the presenter ceiling whenever sharing and
                // pressured, on EVERY arm (Down/Up/Hold), and republishes so the
                // render-side `effective_cap` (pressured Auto == loop cap) sheds
                // the extra tiles. While NOT sharing it is a no-op (`None`), so the
                // cap recovers via the normal growth path — no leaked state. The
                // active-speaker exemption is preserved downstream: `promote_speakers`
                // runs against the resulting lower `visible_tile_count` and swaps
                // active speakers INTO the decoded window, shedding non-speakers
                // first.
                if let Some(ceiling) = presenter_cap_ceiling(natural, sharing) {
                    let clamped = state.cap.min(ceiling.max(MIN_CAP));
                    if clamped != state.cap {
                        let prev_cap = state.cap;
                        state.cap = clamped;
                        state.last_step_ms = now;
                        // A presenter shed ends any recovery streak so the cap does
                        // not immediately try to re-grow toward natural.
                        state.direction_hold = 0;
                        decode_budget_cap.set(state.cap);
                        log::info!(
                            "DecodeBudget: cap {}->{} dir=presenter_shed magnitude={} pressured=true sharing=true natural={} ceiling={}",
                            prev_cap,
                            state.cap,
                            prev_cap - state.cap,
                            natural,
                            ceiling,
                        );
                    }
                }

                // ---- Issue #1558 stage 4: EMERGENCY non-speaker pause ----
                //
                // Applied LAST, after the cascade + presenter clamps, on the
                // pressured path only (it is only reachable once the cascade has
                // latched and reached floor — the cheaper stages run first). When
                // protective mode is active AND audio is STILL growing past the
                // EMERGENCY water mark, force the decode cap to MIN_CAP: exactly ONE
                // decoded tile, which `promote_speakers` fills with the active
                // speaker downstream. Every other non-speaker tile pauses, freeing
                // decode CPU to protect audio. Returns `None` (no clamp) once audio
                // drains, so the cap recovers via the normal cascade/growth path —
                // the stage reverses on recovery. Audio decode is NEVER touched; this
                // sheds VIDEO precisely to protect audio.
                if let Some(emergency_cap) =
                    protective_emergency_cap(protective.active, last_audio_buffer_ms_max)
                {
                    let clamped = state.cap.min(emergency_cap.max(MIN_CAP));
                    if clamped != state.cap {
                        let prev_cap = state.cap;
                        state.cap = clamped;
                        state.last_step_ms = now;
                        // The emergency shed ends any recovery streak so the cap does
                        // not immediately try to re-grow while audio is still in
                        // distress.
                        state.direction_hold = 0;
                        decode_budget_cap.set(state.cap);
                        log::warn!(
                            "ProtectiveMode: EMERGENCY cap {}->{} (speaker-only) audio_buffer_ms={} natural={}",
                            prev_cap,
                            state.cap,
                            last_audio_buffer_ms_max
                                .map(|b| format!("{b:.0}"))
                                .unwrap_or_else(|| "none".into()),
                            natural,
                        );
                    }
                }
            }
        });
        decode_budget_task.write().replace(task);
    });
    use_drop(move || {
        if let Some(task) = decode_budget_task.peek().as_ref() {
            task.cancel();
        }
    });

    // --- Test-only decode-budget injection hooks (issue #987, task 1a.6) ---
    // Register `window.__videocall_inject_render_fps` /
    // `window.__videocall_inject_longtask` so E2E specs can drive the adaptive
    // control loop synthetically. The registration is itself gated on
    // `MOCK_PEERS_ENABLED`, so it is a no-op (and attaches nothing to `window`)
    // in production where that runtime-config flag is false.
    use_hook(crate::components::decode_budget_inject::register_decode_budget_inject_hooks);

    // Register `window.__videocall_inject_stale_video_backlog` /
    // `window.__videocall_freshness_skips` so an E2E spec can deterministically
    // trip the #1020 jitter-buffer freshness deadline (which runs in the decoder
    // worker) and observe the resulting `freshness_skip` event (#1022). Also gated
    // on `MOCK_PEERS_ENABLED`, so a no-op in production.
    use_hook(crate::components::freshness_inject::register_freshness_inject_hooks);

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

    // Auto-join on first render if requested. Every device joins regardless of
    // capabilities (issue #1054) — there is no pre-join gate.
    {
        let mda = mda.clone();
        use_effect(move || {
            if !auto_join {
                return;
            }
            // Direct-URL auto-join: a real join, so the permission callback must
            // proceed to connect (issue #959).
            join_requested.set(true);
            mda.borrow().request();
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
    // Republish the uncapped layout tile count so the adaptive decode-budget
    // control loop (task 1a.3) can pass it to `decide_step` as `natural_count`
    // and never raise the cap above what the layout would actually render.
    // Writing only on change keeps this off the per-render hot path.
    if *decode_budget_natural.peek() != total_tiles {
        decode_budget_natural.set(total_tiles);
    }

    // Render-driven Fixed -> Auto pressured-reset (HCL #987 review). Reads
    // `decode_budget_override` REACTIVELY so this effect re-runs the instant the
    // override changes — independent of the ~1 Hz control loop. On a transition
    // INTO Auto from a non-Auto value, clear the pressured latch so the
    // render-side `effective_cap` re-reveals ALL natural tiles on the very next
    // render (a previously-pressured machine no longer waits for an FPS tick to
    // drop its reduced `decode_budget_cap`).
    //
    // It fires ONLY on the transition: `prev_override` is peeked (not read
    // reactively), so writing it back does not self-retrigger, and while the
    // override stays Auto `prev == current == Auto` makes the body a no-op — it
    // therefore never fights the loop's mid-Auto pressure latch (the loop sets
    // pressured=true on a real down-step; this effect leaves that alone).
    //
    // Issue #1466: going All -> Auto is covered here (All != Auto, so the latch
    // clears). Going Auto -> All does NOT need to touch the latch: traced through
    // `effective_cap`, the `All` arm returns `natural.min(CANVAS_LIMIT)`
    // UNCONDITIONALLY (it never consults `pressured` or `cap`), so engaging All
    // reveals every tile on the next render even if `pressured` is still latched
    // true — exactly how Fixed achieves immediate reveal. No extra code needed.
    use_effect(move || {
        let current = decode_budget_override();
        let previous = *prev_override.peek();
        if previous != DecodeBudgetOverride::Auto && current == DecodeBudgetOverride::Auto {
            decode_budget_pressured.set(false);
            // Pressured-latch edge (true->false): leaving a Fixed override for Auto
            // clears the latch render-side so all natural tiles re-reveal at once.
            log::info!("DecodeBudget: pressured_latch=false trigger=override_resume_auto");
        }
        // Issue #1466/#1471: returning to Auto from ANY non-Auto state (Fixed/All)
        // discards the per-tile force-decode requests. Auto is "let the adaptive
        // loop decide", so stale PLAY requests must not keep peers pinned-decoded
        // across the mode switch. This same edge fires for BOTH entry points that
        // write `decode_budget_override` — the Settings picker AND the persistent
        // "Back to automatic" toggle (both call `decode_budget_ctx.0.set`) — so a
        // single clear here covers both. The decision lives in the pure
        // `should_clear_force_decode_on_override_change` helper so it is
        // host-testable (an inline `.clear()` was mutation-invisible, #1471). We do
        // NOT clear on a transition to All or Fixed(n): those are explicit manual
        // modes where an existing PLAY request is still meaningful. Guarded on
        // non-empty so we don't trigger a needless write-driven re-render when
        // there was nothing to clear.
        if should_clear_force_decode_on_override_change(previous, current)
            && !user_requested_decode.peek().is_empty()
        {
            user_requested_decode.write().clear();
        }
        if previous != current {
            prev_override.set(current);
        }
    });

    // Publish the adaptive decode-budget decision onto the diagnostics bus so
    // the HealthReporter (videocall-client) can fold it into the periodic
    // HEALTH packet (#987 P3). This mirrors how the AdaptiveQuality tier state
    // already rides the health packet: the controller's decision lives only in
    // client console logs today, so population-scale dashboards are blind to it
    // server-side. We publish the SNAPSHOT (current state), not a drained
    // transition buffer — the snapshot is the must-have for dashboards.
    //
    // Reactive reads (`.read()`/calling the signal) of all four authoritative
    // signals mean this effect re-runs the instant any of them changes, and the
    // change-guard `prev_db_snapshot` ensures we only emit a bus event when the
    // decision actually moved — not on every unrelated render. The effective
    // cap is recomputed with the SAME three-mode logic as the render-side
    // `effective_cap` actuator below (Fixed clamp / un-pressured == natural /
    // pressured == loop-owned cap) so the reported value matches what is on
    // screen. `decode_budget_natural` already equals the live `total_tiles`
    // (written just above), so `natural_capped` here matches the render-side
    // `canvas_capped_natural`.
    use_effect(move || {
        let override_mode = decode_budget_override();
        let pressured = decode_budget_pressured();
        let natural = decode_budget_natural();
        let cap = decode_budget_cap();

        let natural_capped = natural.min(CANVAS_LIMIT);
        // Shared three-mode actuator: identical to the render-side
        // `effective_cap` below, so reported telemetry can never drift from
        // what is on screen (HCL #987 review FIX). The SAME device-class
        // ceiling (#1286) is passed so the reported cap matches the rendered
        // (ceiling-clamped) one on iOS.
        let effective = effective_cap(
            override_mode,
            pressured,
            natural,
            cap,
            device_decode_ceiling,
        );

        // Compact, comparable snapshot. Only emit on a real change so the
        // diagnostics bus (and the health packet) is not spammed per render.
        // `override_fixed_n` is 0 in Auto and meaningless to readers there
        // (the proto enum carries the Auto/Fixed discriminator).
        // Clamp the reported fixed cap to CANVAS_LIMIT so telemetry matches the
        // displayed semantics: `parse_decode_budget_override` accepts any
        // `usize > 0` from localStorage, but a tampered value above u32::MAX
        // would otherwise silently truncate on the `as u64 -> as u32` path in
        // the consumer. `effective_cap` is already clamped, so this only aligns
        // the telemetry `override_fixed_n` with what is actually rendered.
        // Issue #1466: the proto `OverrideMode` enum
        // (videocall-types/.../health_packet.rs) has ONLY UNSPECIFIED=0, AUTO=1,
        // FIXED=2 — there is NO `All` value, and the wire format is NOT changed
        // here. Map `All` onto the FIXED discriminator with `fixed_n =
        // natural_capped` (the count All actually decodes). This is the
        // least-misleading mapping: dashboards see "all N tiles decoded as a hard
        // cap of N", which is exactly what All does, rather than inventing a wire
        // value or mislabelling it Auto (which would imply adaptive shedding).
        let report_as_fixed = matches!(
            override_mode,
            DecodeBudgetOverride::Fixed(_) | DecodeBudgetOverride::All
        );
        let fixed_n = match override_mode {
            DecodeBudgetOverride::Fixed(n) => n.min(CANVAS_LIMIT),
            DecodeBudgetOverride::All => natural_capped,
            DecodeBudgetOverride::Auto => 0,
        };
        let is_fixed = report_as_fixed;
        let snapshot = (effective, natural_capped, pressured, is_fixed, fixed_n);
        if *prev_db_snapshot.peek() == snapshot {
            return;
        }
        prev_db_snapshot.set(snapshot);

        // Override mode encoded as the proto OverrideMode enum's integer value
        // (1 = Auto, 2 = Fixed) so the HealthReporter can map it directly.
        let override_mode_i = if is_fixed { 2u64 } else { 1u64 };
        videocall_diagnostics::global_sender()
            .try_broadcast(videocall_diagnostics::DiagEvent {
                subsystem: "decode_budget",
                stream_id: None,
                ts_ms: videocall_diagnostics::now_ms(),
                metrics: vec![
                    videocall_diagnostics::metric!("decode_budget_effective_cap", effective as u64),
                    videocall_diagnostics::metric!("decode_budget_natural", natural_capped as u64),
                    videocall_diagnostics::metric!(
                        "decode_budget_pressured",
                        if pressured { 1u64 } else { 0u64 }
                    ),
                    videocall_diagnostics::metric!("decode_budget_override_mode", override_mode_i),
                    videocall_diagnostics::metric!(
                        "decode_budget_override_fixed_n",
                        fixed_n as u64
                    ),
                ],
            })
            .ok();
    });

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
    // pad_top: breathing room above top tile row.
    // pad_bottom: pad_top + action-bar zone so tiles are visually centred
    //             in the space ABOVE the action bar (Google Meet style).
    //   Desktop action-bar zone ≈ 99px (79px bar + 20px offset).
    //   Mobile  action-bar zone ≈ 73px (57px bar + 16px offset).
    let (gap, pad_top, pad_right, pad_bottom, pad_left) = match dock_position() {
        DockPosition::Bottom => {
            if vw < 568.0 {
                (8.0, 8.0, 8.0, 80.0, 8.0)
            } else {
                (16.0, 20.0, 20.0, 120.0, 20.0)
            }
        }
        DockPosition::Left => {
            if vw < 568.0 {
                (8.0, 8.0, 8.0, 8.0, 80.0)
            } else {
                (16.0, 20.0, 20.0, 20.0, 120.0)
            }
        }
        DockPosition::Right => {
            if vw < 568.0 {
                (8.0, 8.0, 80.0, 8.0, 8.0)
            } else {
                (16.0, 20.0, 120.0, 20.0, 20.0)
            }
        }
    };
    // Per-side resize cap reused by the drag handler below. The smaller of the
    // absolute max and half the viewport. The DRAWER_MIN_WIDTH lower bound here is
    // inert above the 568px breakpoint (where the CSS hides the resize handle on
    // mobile, vw >= 568 always yields >= 284), but kept for safety if the
    // breakpoint ever changes.
    let max_for_side = (vw * 0.5).clamp(DRAWER_MIN_WIDTH, DRAWER_MAX_ABS);
    // Both drawers are overlay-only — they float over the tiles and never carve
    // horizontal space out of the grid, so the available tile width is just the
    // viewport minus padding.
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
    let user_mode = density_mode();
    let effective_mode = compute_effective_density(
        user_mode,
        total_tiles,
        avail_w,
        avail_h,
        gap,
        active_speaker_count,
        num_display_peers,
        vw,
    );

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

    // --- Adaptive decode-budget actuator (issue #987, task 1a.3) ---
    // `effective_cap` is the real actuator: the ceiling on the number of RENDERED
    // video tiles, folded into the same overflow path that density /
    // min-tile-width already use (one more upper bound on how many tiles fit).
    //
    // It is derived HERE, in render scope, from REACTIVE reads of the override
    // and pressured signals (`.read()`, not `.peek()`), so the render re-runs the
    // instant either changes — this is what makes a manual "show N tiles" choice
    // (HCL #987 review FIX 1) and an un-pressured Auto machine's staggered-join
    // tracking (HCL #987 review FIX 2) take effect on the NEXT render with NO
    // dependency on the ~1 Hz control-loop / `client_render_fps` event:
    //
    //   - `Fixed(n)`           → clamp `n` to [MIN_CAP, total_tiles ∩ CANVAS_LIMIT]
    //   - `Auto`, NOT pressured → `total_tiles ∩ CANVAS_LIMIT` (== natural; tracks
    //                             joins immediately, ZERO avatars)
    //   - `Auto`, pressured     → `decode_budget_cap()` (the loop owns the cap
    //                             with its conservative anti-oscillation growth)
    //
    // On a healthy, unpressured machine (or one showing exactly `n` <= natural
    // tiles) `effective_cap >= layout_limit`, so the `min()` below is a no-op and
    // all displayed peers decode, including the mock-peer / `debug_peer_count`
    // path. Avatars only materialise once Auto has measured real pressure and the
    // loop steps `decode_budget_cap` below the layout capacity.
    //
    // Capping `visible_tile_count` here naturally shrinks `visible_tiles` (the
    // slice below) and therefore the `active_decode_set` derived from it, so we
    // do NOT cap the decode set independently of layout.
    //
    // --- Three buckets (issue #987, task 1a.4) ---
    // The decode-budget cap and the natural layout capacity are SEPARATE
    // ceilings, and they partition the sorted tile list into three buckets:
    //
    //   1. Decoded video tiles  `[0 .. visible_tile_count)`
    //        Bounded by the decode-budget cap. These feed `active_decode_set`
    //        and render live `<canvas>` video via `PeerTile`.
    //   2. Off-budget avatar tiles `[visible_tile_count .. displayed_tile_count)`
    //        Peers that the LAYOUT could show but the budget cap excludes from
    //        decode. They render as initials/avatar placeholders (no video
    //        decode → the CPU saving) but stay on screen so the user still sees
    //        who is present. Audio is untouched (see note below).
    //   3. True overflow `[displayed_tile_count .. total_tiles)`
    //        Peers beyond the natural layout capacity. Folded into the `+N`
    //        badge exactly as before.
    //
    // CRITICAL no-cap invariant: whether the `+N` badge appears depends ONLY on
    // the layout capacity (`effective_visible`), never on the budget cap. When
    // the controller is idle (healthy, unpressured Auto), `effective_cap` equals
    // `total_tiles ∩ CANVAS_LIMIT`, so `budget_cap >= effective_visible` always
    // holds: `decoded_limit == layout_limit`, `avatar_count == 0`, and
    // `visible_tile_count` / `overflow_count` are byte-for-byte identical to the
    // pre-1a.4 values. The avatar tier only materialises when `effective_cap <
    // effective_visible` — i.e. after the loop steps the cap down under measured
    // pressure (Auto, pressured), or under an explicit `Fixed(n)` below natural.
    //
    // Audio note: `active_decode_set` (built below from `visible_tiles`) gates
    // ONLY video decode via `client.set_active_decode_set`. Audio playback runs
    // through the independent NetEQ path and the per-peer diagnostics stream
    // every `PeerTile` subscribes to globally, neither of which consults this
    // set. Avatar-tier (and even +N-overflow) peers therefore remain audible.
    //
    // `effective_cap` derivation (HCL #987 review FIX 1 + FIX 2). Reactive reads
    // (`.read()`) so a change to either signal re-runs render immediately, with
    // no dependence on the 1 Hz control loop.
    // Shared three-mode actuator (HCL #987 review FIX): the SAME function the
    // telemetry producer uses, so the reported cap can never drift from what is
    // rendered here. Fixed(n) clamps into [MIN_CAP, min(natural, CANVAS_LIMIT)];
    // un-pressured Auto == natural (staggered joins decode immediately, no
    // avatars); pressured Auto defers to the loop-owned cap.
    let effective_cap = effective_cap(
        *decode_budget_override.read(),
        decode_budget_pressured(),
        total_tiles,
        decode_budget_cap(),
        // #1286: device-class ceiling binds on every mode, including
        // un-pressured Auto (which otherwise returns the raw natural). Computed
        // once at mount above.
        device_decode_ceiling,
    );
    let budget_cap = effective_cap;
    // Natural layout capacity (already bounded by CANVAS_LIMIT through
    // `total_tiles`/`capped_real`). This decides the +N badge boundary.
    let layout_limit = effective_visible;
    // Decode-budget ceiling: how many of the displayed tiles may decode video.
    let decoded_limit = layout_limit.min(budget_cap.max(MIN_CAP));
    // Tiles actually placed in the grid (video + avatar), and the +N count.
    // Reserve one grid slot for the badge only when there is true overflow
    // beyond the layout capacity — identical to the prior badge logic.
    let (displayed_tile_count, overflow_count) = if total_tiles > layout_limit {
        let displayed = layout_limit.saturating_sub(1).max(1);
        (displayed, total_tiles - displayed)
    } else {
        (total_tiles, 0)
    };
    // Bucket 1 / bucket 2 split within the displayed tiles. `base_visible_tile_count`
    // is the count of DECODED video tiles BEFORE user PLAY requests expand it.
    // The final `visible_tile_count` (and `avatar_tile_count`) are computed
    // AFTER `all_tiles` is built — once we can count how many user-requested
    // peers fall OUTSIDE this base window (issue #1466). The split itself
    // (visible vs off-budget avatar) is unchanged; only the boundary may move
    // outward to admit explicit force-decode requests, still bounded by the
    // device ceiling (#1286), the canvas limit, and `displayed_tile_count`.
    let base_visible_tile_count = displayed_tile_count.min(decoded_limit);
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

    // --- Camera-on / camera-off partition (issue #1465) ---
    // A camera-OFF peer produces zero video to decode, so it must NOT consume a
    // decode-budget slot and must NOT land in the dashed off-budget avatar
    // bucket (it would look "paused" / sheddable when there is nothing to shed).
    // Partition the capped real peers up front: camera-ON real peers feed the
    // decode-budget split (alongside mocks); camera-OFF real peers render in a
    // separate plain-avatar group (no `force_avatar`, no dash).
    //
    // Camera-on predicate (applied uniformly here and in canvas_generator.rs):
    //   mock peer                     → treated camera-ON (it is a layout-only
    //                                   placeholder that exercises the decode path)
    //   real peer, video_enabled true → camera-ON
    //   real peer, video_enabled false→ camera-OFF
    // `is_video_enabled_for_peer` returns false for any non-numeric key (incl.
    // mock-N), which is why mocks are handled by the `take(capped_real)` slice
    // here (they are not in `display_peers`) and need no explicit OR.
    let camera_candidates: Vec<(String, bool)> = display_peers
        .iter()
        .take(capped_real)
        .map(|peer_id| (peer_id.clone(), client.is_video_enabled_for_peer(peer_id)))
        .collect();
    let (camera_on_real, mut camera_off_real) = partition_camera_tiles(&camera_candidates);
    // Stable join-order sort for the camera-off group so its render order is
    // deterministic and matches the rest of the grid's earliest-first ordering.
    {
        let join_map = peer_join_time.read();
        camera_off_real.sort_by(|a, b| {
            let jt_a = join_map.get(a).copied().unwrap_or(0.0);
            let jt_b = join_map.get(b).copied().unwrap_or(0.0);
            jt_a.partial_cmp(&jt_b).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    // `all_tiles` now holds ONLY the peers with video to decode: camera-ON real
    // peers + mock placeholders. The decode-budget split (visible/avatar) below
    // operates on this shrunken population. The LAYOUT counters (`total_tiles`,
    // `displayed_tile_count`, `overflow_count`, `tile_count`) are UNCHANGED by the
    // #1465 partition — they still size over the FULL population so the grid
    // geometry and the +N badge are identical; only which peers feed the decode
    // split changes. (The decode-split counters `visible_tile_count` /
    // `avatar_tile_count` ARE recomputed below: the #1466 expansion may move the
    // visible/avatar boundary outward within `displayed_tile_count` to admit PLAY
    // requests — that does not touch the layout counters above.)
    let mut all_tiles: Vec<String> = Vec::with_capacity(camera_on_real.len() + mock_count);
    all_tiles.extend_from_slice(&camera_on_real);
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

    // --- User-requested decode bucket expansion (issues #1466 / #1286) ---
    // A user who taps PLAY on a paused tile is explicitly asking to decode that
    // peer. Count the DISTINCT requested peers that are present in `all_tiles`
    // but ranked AT/AFTER `base_visible_tile_count` (i.e. the ones the budget did
    // NOT already decode), then EXPAND the decoded window to admit them so they
    // render live (`force_avatar = false`) rather than decoded-but-shown-paused.
    //
    // The expansion is clamped by `expand_decoded_for_requested` to the device
    // ceiling (#1286 — a phone can't be forced past its hardware tile ceiling)
    // and the canvas limit, and then re-clamped here to `displayed_tile_count`
    // (we cannot show more decoded tiles than there are grid cells; requests in
    // the true-overflow region beyond `displayed_tile_count` fold into the +N
    // badge and stay paused — the phase-4 merge keeps them OUT of the decode set
    // so decode⇄render still agree). `layout_limit` / `displayed_tile_count` /
    // `overflow_count` / the +N badge are UNCHANGED — they key off layout, not
    // budget. With no requests, `requested_off_budget == 0` and
    // `expand_decoded_for_requested` returns `base_visible_tile_count` verbatim,
    // so the unpressured / no-PLAY path is byte-identical to before.
    let requested_off_budget = {
        let requested = user_requested_decode.read();
        if requested.is_empty() {
            0
        } else {
            // Only count requested peers in the DISPLAYED off-budget window
            // `[base_visible_tile_count, displayed_tile_count)`. Requests in the
            // true-overflow region (`idx >= displayed_tile_count`) have no grid
            // cell to render in, so they must NOT expand the decoded window —
            // they fold into the +N badge and stay paused (see the promotion
            // loop's POST-EXPANSION INVARIANT below).
            all_tiles
                .iter()
                .skip(base_visible_tile_count)
                .take(displayed_tile_count - base_visible_tile_count)
                .filter(|tile_id| requested.contains(*tile_id))
                .count()
        }
    };
    let visible_tile_count = expand_decoded_for_requested(
        base_visible_tile_count,
        requested_off_budget,
        device_decode_ceiling,
        CANVAS_LIMIT,
    )
    .min(displayed_tile_count);
    // Off-budget avatar tiles are the displayed remainder after the (possibly
    // expanded) decoded window.
    let avatar_tile_count = displayed_tile_count - visible_tile_count;

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

    // --- Pinned-peer promotion (HCL #987 review FIX 7; bounded per issue #1470) ---
    // A pinned peer is force-added to `active_decode_set` (phase 3, below) when it
    // got a decoded slot this render. If that peer is ranked in the displayed
    // off-budget window, it would otherwise land in `avatar_tiles` and render with
    // `force_avatar = true` ("Video paused") while it is in fact being decoded —
    // wasted decode AND a misleading UI. `promote_pinned_into_decoded` swaps it
    // into the LAST decoded slot so decode and render agree, BOUNDED to
    // `[visible_tile_count, displayed_tile_count)` so a true-overflow pin can't
    // evict a displayed tile off the grid (issue #1470 — the same defect bounded
    // on the PLAY path). A true-overflow pin is NOT promoted, gets no decoded
    // slot, and so phase 3's `decoded_bucket` intersection (#1489) keeps it OUT of
    // the decode set — it is neither decoded nor shown live (decode⇄render agree).
    if visible_tile_count > 0 && visible_tile_count < all_tiles.len() {
        if let Some(pinned_user_id) = pinned_peer_id.peek().as_deref() {
            // `all_tiles` holds session_ids; the pin is keyed by user_id. Find
            // the pinned peer's tile index by mapping each session_id back to
            // its user_id. Mock tiles ("mock-N") never match a real user_id.
            let pinned_idx = all_tiles.iter().position(|tile_id| {
                client.get_peer_user_id(tile_id).as_deref() == Some(pinned_user_id)
            });
            if let Some(idx) = pinned_idx {
                promote_pinned_into_decoded(
                    &mut all_tiles,
                    visible_tile_count,
                    displayed_tile_count,
                    idx,
                );
            }
        }
    }

    // --- User-requested decode promotion (issue #1466 / #1286) ---
    // `visible_tile_count` was just EXPANDED above to admit the user's PLAY
    // requests, so the decoded window already has room for the requested peers
    // (up to the device ceiling, the canvas limit, and `displayed_tile_count`).
    // This step swaps each requested peer that is still ranked beyond the window
    // INWARD into a decoded slot, so it renders live (`force_avatar = false`)
    // instead of decoded-but-shown-paused — the SAME render-must-agree-with-
    // decode lesson as the pinned peer above (a peer that is decoded but rendered
    // as a paused avatar wastes decode AND shows a misleading "Video paused"
    // placeholder).
    //
    // CRITICAL — distinct slots: several peers may be requested at once, so we
    // must NOT reuse `visible_tile_count - 1` for every one (that would overwrite
    // a previously-promoted requested peer). We walk a `next_free_slot` cursor
    // DOWN from `visible_tile_count - 1`, the same end of the decoded region the
    // pin-swap targets, filling distinct slots toward index 0. The cursor skips
    // the slot now holding the pinned peer (`visible_tile_count - 1` after the
    // pin-swap, if a pin was promoted) so we never evict the pin.
    //
    // POST-EXPANSION INVARIANT (issue #1466 / #1286): the expansion sized
    // `visible_tile_count` to fit every requested off-budget peer EXCEPT those it
    // could not admit — the requests beyond the device ceiling (#1286) or beyond
    // `displayed_tile_count` (true overflow → +N badge). For those un-admittable
    // requests there is deliberately NO decoded slot: the cursor runs out and the
    // peer correctly STAYS a paused avatar. This is NOT the old "decode-but-show-
    // paused" bug: phase 4 below intersects the merge with the decoded bucket, so
    // an un-promoted requested peer is NOT placed in `active_decode_set` either —
    // decode and render agree (it is neither decoded nor shown live). On a phone
    // this is exactly the desired hardware-ceiling behaviour: PLAY cannot force
    // more simultaneous decodes than the device can sustain.
    //
    // Reading `user_requested_decode.read()` HERE is one of the two parent-scope
    // reactive reads (the other is the phase-4 merge) that make a per-tile PLAY
    // click re-render the parent — see the reactivity note on the signal.
    {
        let requested = user_requested_decode.read();
        // The slot the pinned peer occupies after the pin-swap (if it was
        // promoted into the decoded region), so the cursor can skip it. Resolved
        // here (needs `client`) and passed into the pure promotion helper.
        let pinned_slot: Option<usize> = if visible_tile_count > 0 && !requested.is_empty() {
            pinned_peer_id.peek().as_deref().and_then(|pu| {
                all_tiles
                    .iter()
                    .take(visible_tile_count)
                    .position(|tile_id| client.get_peer_user_id(tile_id).as_deref() == Some(pu))
            })
        } else {
            None
        };
        promote_requested_into_decoded(
            &mut all_tiles,
            visible_tile_count,
            displayed_tile_count,
            &requested,
            pinned_slot,
        );
    }

    // Bucket 1: the DECODED portion of the unified tile list. These render live
    // video and seed `active_decode_set` below. (Used by the normal grid layout.)
    let visible_tiles: Vec<String> = all_tiles.iter().take(visible_tile_count).cloned().collect();
    // Bucket 2 (issue #987, task 1a.4): off-budget avatar tiles. These are the
    // tiles the layout could show but the decode-budget cap excludes from video
    // decode. They render as initials/avatar placeholders so the user still sees
    // who is present (and keeps hearing them — audio is independent of the decode
    // set). Empty unless the budget cap is below the natural layout capacity, so
    // the no-cap path produces an empty slice and is unchanged.
    let avatar_tiles: Vec<String> = all_tiles
        .iter()
        .skip(visible_tile_count)
        .take(avatar_tile_count)
        .cloned()
        .collect();

    // --- Camera-off group displayed window (issue #1465) ---
    // The layout reserves `displayed_tile_count` real grid cells (the rest fold
    // into the +N badge). Camera-ON + mock tiles fill the first
    // `visible_tiles.len() + avatar_tiles.len()` of those cells; camera-OFF peers
    // fill the REMAINING displayed cells (camera-on peers get priority for the
    // displayed window since they carry video). Any camera-off peers past that
    // belong to the overflow region and must NOT render as tiles — they are
    // already accounted for in `overflow_count` / the +N badge.
    //
    // Arithmetic proof that rendered-tile-count == `tile_count` (the value that
    // drives `participants-N` + `compute_layout`):
    //   rendered_on   = visible_tiles.len() + avatar_tiles.len()
    //                 = min(all_tiles.len(), displayed_tile_count)   [take/skip]
    //   off_to_render = displayed_tile_count - rendered_on           [below]
    //   rendered      = rendered_on + off_to_render + (overflow ? 1 : 0)
    //                 = displayed_tile_count + (overflow ? 1 : 0)
    //                 = tile_count                                    ∎
    // No-cap byte-identity (issue #1465 invariant 1): when EVERY peer is
    // camera-on, `camera_off_real` is empty, `all_tiles` equals the pre-#1465
    // list, `rendered_on == displayed_tile_count`, so `off_to_render == 0` and
    // `camera_off_tiles` is empty — output is byte-identical to before.
    let rendered_on = visible_tiles.len() + avatar_tiles.len();
    let off_to_render = displayed_tile_count.saturating_sub(rendered_on);
    let camera_off_tiles: Vec<String> = camera_off_real
        .iter()
        .take(off_to_render)
        .cloned()
        .collect();

    // --- Lone-peer full-bleed predicate (issues #1465, #508) ---
    // The #508 single-peer presentation renders the SOLE remote peer full-bleed
    // (its content — live video, or the "Camera Off" placeholder — filling the
    // tile). Before #1465 that was keyed off `visible_tile_count == 1`, because
    // a single on-screen tile could only ever be a decoded video tile.
    //
    // The #1465 partition broke that assumption: camera-OFF real peers are no
    // longer in `visible_tiles`/`avatar_tiles` — they render from the separate
    // `camera_off_tiles` group. So `visible_tile_count == 1` no longer means the
    // peer is alone on screen: a camera-on peer (visible) can render ALONGSIDE a
    // camera-off peer (camera_off), giving `visible == 1` while two tiles are
    // actually shown. Keying full-bleed off `visible_tile_count` would then make
    // BOTH the lone-camera-on rule and the camera-off rule believe they are
    // alone, full-bleeding two tiles at once.
    //
    // The correct key is the TOTAL displayed real-peer tiles across all three
    // render groups. `is_sole_real_tile` computes exactly that sum; the
    // visible_tiles loop and the camera_off_tiles loop below both gate full-bleed
    // on this single shared value, so at most one tile can ever be full-bleed.
    //
    // No-cap byte-identity invariant (#1465): with exactly one camera-ON peer and
    // zero camera-off peers the cap is inactive, so `visible_tiles.len() == 1`
    // while `avatar_tiles` and `camera_off_tiles` are empty. The sum is 1 →
    // `sole_real_tile` is true → that lone peer is full-bleed, exactly as before
    // the partition. (Mocks are excluded from full-bleed separately via the
    // `!is_mock` guard on the visible_tiles rule.)
    let sole_real_tile = is_sole_real_tile(
        visible_tiles.len(),
        avatar_tiles.len(),
        camera_off_tiles.len(),
    );

    // Build the peer-list sidebar entries keyed by `session_id` so each open
    // browser tab is its own row. `user_id` is carried alongside only for
    // host-action callbacks (mute / disable video), which remain per-user.
    let peers_for_display: Vec<PeerListEntry> = display_peers
        .iter()
        .map(|session_id| {
            let user_id = client
                .get_peer_user_id(session_id)
                .unwrap_or_else(|| session_id.clone());
            PeerListEntry {
                session_id: session_id.clone(),
                user_id,
            }
        })
        .collect();

    // --- Screen share stack: tracks the order of peer screen shares (LIFO) ---
    let mut screen_share_stack: Signal<Vec<String>> = use_signal(Vec::new);
    let previous_active_decode_set: Rc<RefCell<HashSet<u64>>> =
        use_hook(|| Rc::new(RefCell::new(HashSet::new())));
    // #1256 Phase 1: last pushed per-peer tile-size hints, so we only call
    // `set_peer_tile_hints` when the map actually changes (join/leave/pin/resize),
    // not on every render. Sibling of `previous_active_decode_set`.
    let previous_peer_tile_hints: Rc<RefCell<HashMap<u64, videocall_client::TileHint>>> =
        use_hook(|| Rc::new(RefCell::new(HashMap::new())));
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
    //
    // Screen-share right panel: compact tiles via CSS grid layout.
    // All visual sizing is handled purely by CSS (.ss-peer-panel).
    //
    // ALL participants are rendered in the DOM (vertical scroll handles
    // overflow), but only the first `ss_decoded_limit` get live video
    // decode. The rest render as avatar-tier tiles (`force_avatar: true`)
    // — same pattern as the normal grid's decode-budget (issue #987).
    // This prevents a 30-person meeting from spinning up 30 decoders
    // during screen share on constrained hardware.

    // Build a separate tile list for the screen-share right panel.
    // (issue #1465) Same partition as the normal grid: only camera-ON real peers
    // + mocks consume the SS decode budget; camera-OFF peers render in a separate
    // plain-avatar group (`ss_camera_off_tiles`), never dashed, never budgeted.
    // The SS panel renders ALL tiles in the DOM (vertical scroll, no +N badge),
    // so the camera-off group is the WHOLE `camera_off_real` set here — there is
    // no displayed-window cap to apply.
    let (ss_decoded_tiles, ss_avatar_tiles, ss_camera_off_tiles) = if has_screen_share {
        let mut ss_all: Vec<String> = Vec::with_capacity(camera_on_real.len() + mock_count);
        ss_all.extend_from_slice(&camera_on_real);
        ss_all.extend_from_slice(&mock_ids);
        {
            let join_map = peer_join_time.read();
            ss_all.sort_by(|a, b| {
                let jt_a = join_map.get(a).copied().unwrap_or(0.0);
                let jt_b = join_map.get(b).copied().unwrap_or(0.0);
                jt_a.partial_cmp(&jt_b).unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        // Base decoded window before user PLAY requests expand it.
        let ss_base_budget = budget_cap.max(MIN_CAP).min(ss_all.len());
        // --- SS user-requested decode bucket expansion (issues #1466 / #1286) ---
        // Mirrors the normal-grid expansion: count the DISTINCT user-requested
        // peers present in `ss_all` but ranked AT/AFTER `ss_base_budget`, then
        // expand the decoded window to admit them so they render live rather than
        // decoded-but-shown-paused. Clamped by the device ceiling (#1286) and the
        // canvas limit, then by `ss_all.len()` — the SS panel renders ALL tiles
        // (vertical scroll, no +N badge), so the displayed-window clamp is the
        // full `ss_all` length, NOT `displayed_tile_count`.
        let ss_requested_off_budget = {
            let requested = user_requested_decode.read();
            if requested.is_empty() {
                0
            } else {
                ss_all
                    .iter()
                    .skip(ss_base_budget)
                    .filter(|tile_id| requested.contains(*tile_id))
                    .count()
            }
        };
        let ss_budget = expand_decoded_for_requested(
            ss_base_budget,
            ss_requested_off_budget,
            device_decode_ceiling,
            CANVAS_LIMIT,
        )
        .min(ss_all.len());

        // Promote active speakers into the (possibly expanded) decoded window.
        {
            let speech_map = peer_speech_priority.read();
            let join_map = peer_join_time.read();
            promote_speakers(
                &mut ss_all,
                ss_budget,
                &speech_map,
                &join_map,
                now_ms,
                SPEAKER_ACTIVE_MS,
            );
        }

        // --- SS pin-swap (mirrors the normal grid's pin-swap at lines above) ---
        // If the pinned peer is ranked beyond `ss_budget`, swap it into the
        // last decoded slot so it renders with live video instead of avatar.
        // The SS panel renders ALL tiles (no +N badge), so this swap always lands
        // the pin in `ss_decoded_tiles` → `decoded_bucket`, so phase 3's #1489
        // intersection admits it. Without the swap a pinned off-budget SS peer
        // would render as avatar despite being decoded (wasted decode +
        // misleading UI).
        if ss_budget > 0 && ss_budget < ss_all.len() {
            if let Some(pinned_user_id) = pinned_peer_id.peek().as_deref() {
                let pinned_idx = ss_all.iter().position(|tile_id| {
                    client.get_peer_user_id(tile_id).as_deref() == Some(pinned_user_id)
                });
                if let Some(idx) = pinned_idx {
                    if idx >= ss_budget {
                        ss_all.swap(ss_budget - 1, idx);
                    }
                }
            }
        }

        // --- SS user-requested decode promotion (issue #1466 / #1286) ---
        // Mirrors the normal-grid user-requested promotion: `ss_budget` was just
        // EXPANDED above to admit the PLAY requests, so the decoded window has
        // room for them. Any requested peer still ranked beyond `ss_budget` is
        // swapped into a DISTINCT decoded slot (cursor walking down from
        // `ss_budget - 1`, skipping the pinned slot) so render agrees with the
        // phase-4 decode set. Same distinct-slot discipline so multiple requested
        // peers never overwrite each other or the pinned peer. Requests the
        // expansion could NOT admit (beyond the device ceiling #1286) have no
        // slot, correctly stay paused avatars, and are kept OUT of
        // `active_decode_set` by the decoded-bucket-intersecting phase-4 merge.
        {
            let requested = user_requested_decode.read();
            // Resolve the pinned peer's post-swap decoded slot (needs `client`, not
            // host-testable) and pass it into the shared pure helper. The SS panel renders ALL
            // tiles (vertical scroll, no +N badge), so `displayed_tile_count = ss_all.len()` —
            // every off-budget tile is renderable, so the helper's true-overflow bound (#1470)
            // never excludes an SS peer, preserving the prior inline-loop behaviour.
            // `ss_budget < ss_all.len()` mirrors the helper's own early-return bound, so we skip
            // the `get_peer_user_id` scan in the budget-covers-all-tiles case (where the helper
            // does nothing anyway).
            let pinned_slot: Option<usize> =
                if ss_budget > 0 && ss_budget < ss_all.len() && !requested.is_empty() {
                    pinned_peer_id.peek().as_deref().and_then(|pu| {
                        ss_all.iter().take(ss_budget).position(|tile_id| {
                            client.get_peer_user_id(tile_id).as_deref() == Some(pu)
                        })
                    })
                } else {
                    None
                };
            let ss_displayed = ss_all.len();
            promote_requested_into_decoded(
                &mut ss_all,
                ss_budget,
                ss_displayed,
                &requested,
                pinned_slot,
            );
        }

        // Split: first ss_budget tiles get video decode, rest get avatars.
        let decoded: Vec<String> = ss_all.iter().take(ss_budget).cloned().collect();
        let avatars: Vec<String> = ss_all.iter().skip(ss_budget).cloned().collect();
        // Camera-off peers (issue #1465): plain avatars, never dashed/budgeted.
        (decoded, avatars, camera_off_real.clone())
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    // ORDERING INVARIANT: the active decode set is built in 4 phases:
    //   1. Visible layout peers (here)
    //   2. Active screen sharer (here)
    //   3. Pinned peer (below, after tile rendering) — INTERSECTED with the
    //      decoded bucket (issue #1489) so a true-overflow pin with no decoded
    //      slot is not decoded-but-invisible (mirrors phase 4).
    //   4. User-requested force-decode peers (below, issue #1466) — the
    //      `merge_user_requested_decode` call after the stale-request prune,
    //      INTERSECTED with the decoded bucket so it can only re-affirm peers
    //      already decoded (it never force-adds a paused avatar).
    // The dedup check against previous_active_decode_set must run AFTER all
    // four phases. Moving any insertion after the dedup will silently desync.
    //
    // `decoded_bucket` is the session_ids of the tiles ACTUALLY rendering live
    // video this frame (the expanded/promoted visible window for the active
    // path). It is the same source that seeds `active_decode_set`, captured
    // separately so the phase-4 merge can intersect against it (issue #1466 /
    // #1286: a requested peer that did not get a decoded slot — e.g. it exceeded
    // the device ceiling — must NOT enter the decode set while it renders as a
    // paused avatar).
    let decoded_bucket: HashSet<u64> = if has_screen_share {
        // In screen share mode, decode only the budget-capped tiles.
        // Avatar-tier tiles are rendered but not decoded.
        ss_decoded_tiles
            .iter()
            .filter_map(|pid| pid.parse::<u64>().ok())
            .collect()
    } else {
        // Use visible_tiles (post-expansion/promotion) so promoted speakers and
        // PLAY-requested peers are decoded. .parse::<u64>() filters out mock-N.
        visible_tiles
            .iter()
            .filter_map(|id| id.parse::<u64>().ok())
            .collect()
    };
    let mut active_decode_set: HashSet<u64> = decoded_bucket.clone();
    if let Some(active_peer) = active_screen_sharer.as_ref() {
        if let Ok(session_id) = active_peer.parse::<u64>() {
            active_decode_set.insert(session_id);
        }
    }

    // Tile count drives the `participants-N` class modifier on the grid
    // container AND the `compute_layout` cell sizing, which lets CSS branch
    // layout behavior (see `.participants-1 .grid-item.full-bleed` rule in
    // style.css that drops the 3:2 cap on the lone tile for the 2-peer meeting
    // case — HCL #7).
    //
    // Must count BOTH decoded video tiles AND off-budget avatar tiles (task
    // 1a.4), because both occupy real grid cells. `avatar_tile_count` is 0 when
    // no budget cap is active, so `displayed_tile_count == visible_tile_count`
    // and this is identical to the pre-1a.4 value.
    let tile_count = displayed_tile_count + if overflow_count > 0 { 1 } else { 0 };

    let container_style = if has_screen_share {
        // Screen-share panel on the left, participant panel on the right (ratio draggable 0.3–0.85).
        // The container is full-bleed; the overlay drawers float over it without reflowing it.
        "position: absolute; left: 0; right: 0; top: 0; bottom: 0; height: 100%; \
         display: flex; flex-direction: row; flex-wrap: nowrap; gap: 10px; \
         padding: 16px 16px 80px 16px; \
         align-items: stretch; box-sizing: border-box; \
         grid-template-columns: none; grid-template-rows: none;"
            .to_string()
    } else {
        // Google Meet–style grid: reuse vw/vh/gap/avail computed above.
        // Explicitly reset all flex properties so the transition from
        // screen-share (flex) back to normal (grid) is clean.
        let (cols, rows, tw) = compute_layout(tile_count, avail_w, avail_h, gap);
        // Cell height tracks the same 3:2 ratio `.grid-item` is capped at, so
        // the cell exactly fits the tile and `place-self: center` has no
        // surplus to distribute. Using a wider ratio here would leave
        // `tw - th * TILE_AR` of internal padding on every cell.
        let th = tw / TILE_AR;
        // 1-tile case (HCL #7, 2-peer meeting): let the lone remote tile
        // stretch to fill the entire grid area. The `.participants-1
        // .grid-item.full-bleed` CSS rule drops the 3:2 cap on this lone
        // tile so the remote peer fills the viewport — combined with `1fr`
        // tracks and `stretch` packing, the tile reaches edge-to-edge.
        // 2+ tiles (HCL #6): size tracks to the natural 3:2 tile dimensions
        // and pack left/top so surplus viewport width sits on the right
        // edge as empty space instead of being distributed between tiles.
        // This is the only way to guarantee the 3:2 aspect holds in narrow
        // viewports where `1fr` cells would be taller than `cell_w * 2/3`
        // and `.grid-item { height: 100% }` would otherwise stretch the
        // tile vertically. See HCL bug report for the 3-peer-aspect issue.
        let (track_cols, track_rows, pack) = if tile_count == 1 {
            (
                format!("repeat({cols}, 1fr)"),
                format!("repeat({rows}, 1fr)"),
                "justify-content: stretch; align-content: stretch;",
            )
        } else {
            (
                format!("repeat({cols}, var(--tile-w))"),
                format!("repeat({rows}, var(--tile-h))"),
                "justify-content: start; align-content: start;",
            )
        };
        format!(
            "display: grid; \
             position: absolute; top: 0; bottom: 0; left: 0; right: 0; \
             gap: {gap:.0}px; \
             padding: {pad_top:.0}px {pad_right:.0}px {pad_bottom:.0}px {pad_left:.0}px; \
             box-sizing: border-box; overflow: hidden; \
             flex-direction: unset; flex-wrap: unset; align-items: unset; \
             height: 100%; \
             grid-template-columns: {track_cols}; grid-template-rows: {track_rows}; \
             {pack} \
             --tile-w: {tw:.0}px; --tile-h: {th:.0}px;"
        )
    };

    // `participants-N` modifier; CSS uses `.participants-1 .grid-item.full-bleed`
    // (HCL #7) to drop the 3:2 cap on the lone remote tile in a 2-peer
    // meeting so it fills the viewport. 2+ tiles keep the cap and the tile
    // size is driven by `--tile-w` / `--tile-h` (set above) — see the
    // `tile_count == 1` branch in `container_style`.
    // Append `has-screen-share` so CSS can re-anchor the decode-paused pill
    // (issue 1142): in SS mode the controls dock is not the bottom anchor, so
    // the pill moves to top:12px via `#grid-container.has-screen-share
    // .decode-paused-pill`. `container_class` is consumed only at the
    // `#grid-container` `class:` binding below — nothing keys off the exact
    // string — so appending the modifier is safe.
    let container_class = if has_screen_share {
        format!("participants-{tile_count} has-screen-share")
    } else {
        format!("participants-{tile_count}")
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
        // Every device joins regardless of capabilities (issue #1054): the
        // lobby always renders the PreJoinSettingsCard and the join button is
        // always enabled. No pre-join capability gate.
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
                            media_access_granted: media_access_granted(),
                            speaker_selection_supported: speaker_supported,
                            cameras: prejoin_cameras(),
                            microphones: prejoin_microphones(),
                            speakers: prejoin_speakers(),
                            selected_camera_id: prejoin_selected_camera(),
                            selected_microphone_id: prejoin_selected_mic(),
                            selected_speaker_id: prejoin_selected_speaker(),
                            camera_on: prejoin_camera_on,
                            mic_on: prejoin_mic_on,
                            on_request_permission: {
                                let mda = mda.clone();
                                let auto_requested = auto_requested.clone();
                                move |_| {
                                    // Preview-only permission request: does NOT
                                    // join (join_requested stays false). Share the
                                    // on-mount auto-request's one-shot guard so a
                                    // manual click while the auto-probe is still
                                    // in flight doesn't fire a second concurrent
                                    // getUserMedia; still works if the auto-effect
                                    // never ran (guard not yet set).
                                    if !auto_requested.get() {
                                        auto_requested.set(true);
                                        mda.borrow().request();
                                    }
                                }
                            },
                            on_camera_toggle: {
                                let preview_engine = preview_engine.clone();
                                move |on: bool| {
                                    prejoin_camera_on.set(on);
                                    save_preferred_camera_on(on);
                                    if on {
                                        let id = prejoin_selected_camera()
                                            .unwrap_or_default();
                                        preview_engine.start_camera(id);
                                    } else {
                                        preview_engine.stop_camera();
                                    }
                                }
                            },
                            on_mic_toggle: {
                                let preview_engine = preview_engine.clone();
                                move |on: bool| {
                                    prejoin_mic_on.set(on);
                                    save_preferred_mic_on(on);
                                    if on {
                                        let id = prejoin_selected_mic().unwrap_or_default();
                                        preview_engine.start_mic_meter(id);
                                    } else {
                                        preview_engine.stop_mic_meter();
                                    }
                                }
                            },
                            on_camera_select: {
                                let preview_engine = preview_engine.clone();
                                move |info: DeviceInfo| {
                                    prejoin_selected_camera.set(Some(info.device_id.clone()));
                                    save_preferred_camera_id(&info.device_id);
                                    // Re-acquire the preview with the new device
                                    // (only while the camera is on).
                                    if prejoin_camera_on() {
                                        preview_engine.start_camera(info.device_id);
                                    }
                                }
                            },
                            on_microphone_select: {
                                let preview_engine = preview_engine.clone();
                                move |info: DeviceInfo| {
                                    prejoin_selected_mic.set(Some(info.device_id.clone()));
                                    save_preferred_mic_id(&info.device_id);
                                    if prejoin_mic_on() {
                                        preview_engine.start_mic_meter(info.device_id);
                                    }
                                }
                            },
                            on_speaker_select: move |info: DeviceInfo| {
                                prejoin_selected_speaker.set(Some(info.device_id.clone()));
                                save_preferred_speaker_id(&info.device_id);
                            },
                            on_join: {
                                let mda = mda.clone();
                                move |_| {
                                    // Mark this as a real join so the permission
                                    // callback proceeds to connect (issue #959).
                                    join_requested.set(true);
                                    mda.borrow().request();
                                }
                            },
                        }
                    }
                }
                if show_device_warning() {
                    {
                        let mut client = client.clone();
                        // Pre-join dismiss: proceed to connect + join, matching the
                        // original inline handler exactly (issue #959).
                        let on_dismiss = EventHandler::new(move |()| {
                            show_device_warning.set(false);
                            if let Err(e) = client.connect() {
                                error!("Connection failed: {e:?}");
                            }
                            meeting_joined.set(true);
                        });
                        render_device_warning_modal(
                            mic_error.read().as_ref(),
                            video_error.read().as_ref(),
                            on_dismiss,
                        )
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

    // Snapshot the local session_id once per render so every PeerTile can pin
    // self-identification on session_id instead of user_id. Two tabs of the
    // same authenticated user share a user_id but always have distinct
    // session_ids — a user-id compare collapses sibling tabs into one "self"
    // tile in split layouts and screen-share paths (HCL issue 828). May be `None`
    // before SESSION_ASSIGNED is received; in that case no tile is treated as
    // self until the assignment arrives.
    let my_session_id: Option<String> = client.get_own_session_id();

    // Edge-triggered: log only when the peer count CHANGES, not on every render.
    // This component re-renders many times per second (signals, speech priority,
    // layout), so an unconditional log here emitted ~44k lines in an 8-min
    // meeting — the single largest console-log contributor after the #1100/#1129
    // per-tick demotions. Logging on transitions preserves the documented
    // `Rendering meeting view with 0 peers` failure signature (the drop to zero
    // is still emitted) while cutting volume to a handful of lines.
    {
        let peer_count = display_peers.len();
        if last_logged_peer_count() != Some(peer_count) {
            last_logged_peer_count.set(Some(peer_count));
            info!("Rendering meeting view with {peer_count} peers");
        }
    }

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
    // INTERSECTED with `decoded_bucket` (issue #1489), mirroring the phase-4 PLAY
    // merge: the pin-swap above already moved a promotable pin
    // (`[visible_tile_count, displayed_tile_count)`) into a decoded slot, so it is
    // in `decoded_bucket` and is admitted. A true-overflow pin
    // (`idx >= displayed_tile_count`) is deliberately NOT promoted (#1470 — it
    // would evict a displayed tile off-grid) and so stays in the +N badge with no
    // decoded slot; gating the insert here keeps it OUT of the decode set rather
    // than decoding it while it renders in no grid bucket (decode⇄render agree).
    // A camera-OFF pin is never in `decoded_bucket` (it is in `camera_off_tiles`,
    // not `visible_tiles`/`ss_decoded_tiles`) so it is intentionally excluded —
    // it has no video to decode and its audio is independent of this set.
    let current_pinned = pinned_peer_id();
    if let Some(pinned_user_id) = current_pinned.as_deref() {
        if let Some(pinned_session_id) = display_peers
            .iter()
            .find(|peer_id| client.get_peer_user_id(peer_id).as_deref() == Some(pinned_user_id))
            .and_then(|peer_id| peer_id.parse::<u64>().ok())
        {
            merge_pinned_decode(&mut active_decode_set, pinned_session_id, &decoded_bucket);
        }
    }

    // Clean stale force-decode requests (issue #1466) — mirrors the stale-pin
    // cleanup above. A PLAY-requested peer that has left the meeting is no longer
    // in `display_peers`, so drop its session_id from the set. BOTH `display_peers`
    // and `user_requested_decode` hold session_ids, so we compare them directly
    // (no user_id mapping, unlike the pin which is user_id-keyed). Pruned BEFORE
    // the phase-4 merge so a stale id can never be force-decoded. Guarded so we
    // only write the signal when something actually changed (avoids a
    // write-triggered re-render loop).
    {
        let stale: Vec<String> = user_requested_decode
            .peek()
            .iter()
            .filter(|session_id| !display_peers.contains(*session_id))
            .cloned()
            .collect();
        if !stale.is_empty() {
            let mut set = user_requested_decode.write();
            for session_id in stale {
                set.remove(&session_id);
            }
        }
    }

    // Phase 4 of active_decode_set construction (issue #1466 / #1286): fold in
    // the user's explicit force-decode requests, INTERSECTED with `decoded_bucket`
    // so we only re-affirm requested peers that actually got a decoded slot this
    // frame. A request the expansion could not admit (beyond the device ceiling
    // or `displayed_tile_count`) is NOT in `decoded_bucket`, so it is skipped and
    // never enters the decode set while rendering as a paused avatar — decode and
    // render agree. Since `decoded_bucket` already seeded `active_decode_set`,
    // this is a redundant-but-explicit guard that pins the invariant. The
    // `.read()` here is the authoritative PARENT-scope reactive read that makes a
    // per-tile PLAY click re-render the parent → recompute expansion + promotion +
    // this merge → `set_active_decode_set` below → peer.visible=true → frames
    // decode → next render `force_avatar` is false for the promoted tile → live
    // canvas.
    merge_user_requested_decode(
        &mut active_decode_set,
        &user_requested_decode.read(),
        &decoded_bucket,
    );
    {
        // Dedup: only push to client when the set actually changed.
        let mut previous_active_decode_set = previous_active_decode_set.borrow_mut();
        if *previous_active_decode_set != active_decode_set {
            // Render actuator: the effective decode-budget cap applied to the
            // visible tile set. Logged at debug to correlate with the info-level
            // cap-transition decisions above without spamming the steady state.
            log::debug!(
                "DecodeBudget: active_decode_set size={} budget_cap={}",
                active_decode_set.len(),
                budget_cap,
            );
            client.set_active_decode_set(&active_decode_set);
            *previous_active_decode_set = active_decode_set.clone();
        }
    }

    // #1256 Phase 1: push the per-peer rendered-tile-size hints so the receiver can
    // LID the requested simulcast layer to the size actually painted. The decode
    // set is now fully settled (phases 1-4 above), so the hint map is keyed over the
    // same `active_decode_set` the relay will receive layers for.
    //
    // Tile device-pixel height: in the grid layout every decoded tile is the same
    // `compute_layout` cell (height = tile_w / TILE_AR), scaled by the device pixel
    // ratio to compare against the layers' NATIVE device-pixel heights. In
    // screen-share mode the participant panel tiles are not the same fixed grid
    // thumbnail, so we apply NO lid (None -> Uncapped) and let the downlink chooser
    // run unconstrained.
    let dpr = window().device_pixel_ratio().max(1.0);
    let tile_device_px_h: Option<u32> = if has_screen_share {
        None
    } else if tile_count == 1 {
        // tile_count == 1 renders FULL-BLEED (.participants-1 .grid-item.full-bleed
        // in style.css drops the 3:2 cap; the lone tile paints at the full cell
        // height = avail_h). Use avail_h so the hint matches the PAINTED height — the
        // 3:2-capped tw/TILE_AR under-estimates in portrait and would over-cap the
        // full-screen 1-on-1 to a blurry low layer (#1256 P3).
        Some((avail_h * dpr).round() as u32)
    } else {
        let (_c, _r, tw) = compute_layout(tile_count, avail_w, avail_h, gap);
        let th = tw / TILE_AR;
        Some((th * dpr).round() as u32)
    };

    let peer_tile_hints: HashMap<u64, videocall_client::TileHint> = {
        use videocall_client::TileHint;
        // The pinned peer is held by USER_ID; resolve it to the session_id present
        // in `active_decode_set` so the (Uncapped) pin exemption matches a real peer.
        let pinned_session: Option<u64> = pinned_peer_id.peek().as_deref().and_then(|pu| {
            active_decode_set
                .iter()
                .copied()
                .find(|sid| client.get_peer_user_id(&sid.to_string()).as_deref() == Some(pu))
        });
        // `active_screen_sharer` is a SESSION_ID string (it comes from the
        // session-id-keyed `screen_share_stack`).
        let screen_session: Option<u64> = active_screen_sharer
            .as_ref()
            .and_then(|s| s.parse::<u64>().ok());
        active_decode_set
            .iter()
            .map(|&sid| {
                // Pinned and screen-share peers are NEVER size-capped — they render
                // large, so the receiver should pull the full downlink-sustainable
                // layer for them.
                let uncapped = Some(sid) == pinned_session || Some(sid) == screen_session;
                let hint = match (uncapped, tile_device_px_h) {
                    (true, _) => TileHint::Uncapped,
                    (false, Some(h)) => TileHint::Capped { device_px_h: h },
                    (false, None) => TileHint::Uncapped,
                };
                (sid, hint)
            })
            .collect()
    };
    {
        // Dedup: only push when the hint map actually changed (join/leave/pin/resize).
        // Only record the map as delivered when the push was actually APPLIED — if
        // set_peer_tile_hints dropped it on a transient `inner` borrow conflict (returns
        // false), leave `previous_peer_tile_hints` UNCHANGED so the next render re-attempts
        // the push (the map still differs from prev). Otherwise a dropped resize-to-small
        // push would strand a stale cap and the small tile would keep pulling the high
        // layer until the next layout change (#1256). `&&` short-circuits left-to-right, so
        // set_peer_tile_hints is only called when the map actually changed.
        let mut prev = previous_peer_tile_hints.borrow_mut();
        if *prev != peer_tile_hints && client.set_peer_tile_hints(peer_tile_hints.clone()) {
            *prev = peer_tile_hints.clone();
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

    // Issue #1466: toggle a peer's force-decode request. Mirrors `toggle_pin`
    // but is keyed on the tile's SESSION_ID (the `key`/`peer_id` the avatar tile
    // passes to `on_request_decode`), NOT user_id — `user_requested_decode` and
    // `display_peers` both hold session_ids and the phase-4 merge parses them to
    // u64. Toggle semantics: a second click removes the id so the budget may
    // re-pause the peer (the PLAY button is not a one-way latch). Writing this
    // signal re-renders the parent (it is `.read()` in the promotion + phase-4
    // merge above), which recomputes the partition and pushes the new
    // active_decode_set.
    let toggle_request_decode: EventHandler<String> =
        EventHandler::new(move |session_id: String| {
            let mut set = user_requested_decode.write();
            if set.contains(&session_id) {
                set.remove(&session_id);
            } else {
                set.insert(session_id);
            }
        });

    rsx! {
        div {
            // Provide MeetingTime context
            // Provide VideoCallClient context
            style:"display:flex;gap:var(--space-2)",
            div { id: "main-container", class: "meeting-page",
                onclick: move |evt: MouseEvent| {
                    dock_menu_open.set(false);
                    density_open.set(false);
                    mock_peers_open.set(false);
                    // Background (video-grid) clicks also light-dismiss the open
                    // side panels — the peer list and diagnostics drawer (issue
                    // #1790). Clicks on the action bar (`.video-controls-container`)
                    // are excluded so the panel toggles, mic and camera keep the
                    // panels open; clicks INSIDE a panel are stopped on the panel
                    // container itself, so they never reach this handler. Closing
                    // by flipping the signal runs the SAME teardown as the toggle:
                    // the diagnostics drawer is only mounted while `diagnostics_open`
                    // is true, so setting it false unmounts it and runs its cleanup;
                    // the peer list has no extra close-time work.
                    if !click_within_action_bar(&evt) {
                        peer_list_open.set(false);
                        diagnostics_open.set(false);
                    }
                },
                onkeydown: move |evt: Event<KeyboardData>| {
                    // Escape light-dismisses the topmost transient surface and
                    // restores focus to its action-bar toggle (WAI-ARIA APG
                    // disclosure pattern) so focus never drops to `<body>`. Popovers
                    // (density, mock-peers) are more transient than the side panels,
                    // so Escape peels them FIRST (#1777); only when no popover is
                    // open does it close the topmost side panel — diagnostics, then
                    // the peer list (issue #1790). The `else if` chain guarantees
                    // each Escape closes EXACTLY one surface. The dock menu keeps its
                    // own Esc handler (with stop_propagation).
                    //
                    // The chat drawer is DELIBERATELY excluded from this
                    // light-dismiss: its message composer means a stray background
                    // Escape must not risk discarding an in-progress draft. That is
                    // out of issue-1790 scope.
                    if evt.key() == Key::Escape {
                        if density_open() {
                            evt.stop_propagation();
                            evt.prevent_default();
                            density_open.set(false);
                            focus_element_by_id("density-mode-trigger");
                        } else if mock_peers_open() {
                            evt.stop_propagation();
                            evt.prevent_default();
                            mock_peers_open.set(false);
                            focus_element_by_id("mock-peers-trigger");
                        } else if let Some(target) =
                            esc_panel_close_target(diagnostics_open(), peer_list_open())
                        {
                            evt.prevent_default();
                            match target {
                                EscCloseTarget::Diagnostics => diagnostics_open.set(false),
                                EscCloseTarget::PeerList => peer_list_open.set(false),
                            }
                            // The trigger button always persists in the action bar,
                            // so focus restore is synchronous (no deferral needed —
                            // unlike the dock trigger, which is swapped for Done in
                            // customize mode).
                            focus_element_by_id(target.trigger_id());
                        }
                    }
                },
                BrowserCompatibility {}

                // "participant joined/left" toast notifications
                if !peer_toasts().is_empty()
                    || show_muted_toast()
                    || show_video_off_toast()
                    || host_change_toast().is_some()
                    || screen_share_toast_state().is_some()
                {
                    div { class: "peer-toasts",
                        // Screen-share visibility toast (HCL issue 893). @token-exempt
                        // Rendered first so it sits above other transient toasts.
                        {
                            let toast = screen_share_toast_state.read().clone();
                            match toast {
                                Some(ScreenShareToastState::Starting) => rsx! {
                                    div {
                                        class: "peer-toast toast-loading screen-share-toast",
                                        role: "status",
                                        aria_live: "polite",
                                        aria_label: "Starting to share content",
                                        span { class: "toast-icon",
                                            svg {
                                                width: "16",
                                                height: "16",
                                                view_box: "0 0 24 24",
                                                fill: "none",
                                                stroke: "currentColor",
                                                stroke_width: "2",
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
                                                path { d: "M21 12a9 9 0 1 1-6.219-8.56" }
                                            }
                                        }
                                        span { class: "toast-text",
                                            span { class: "toast-name",
                                                "Starting to share content..."
                                            }
                                        }
                                    }
                                },
                                Some(ScreenShareToastState::SuccessfullyShared) => rsx! {
                                    div {
                                        class: "peer-toast toast-success screen-share-toast",
                                        role: "status",
                                        aria_live: "polite",
                                        aria_label: "Others can now see your shared content",
                                        span { class: "toast-icon",
                                            svg {
                                                width: "16",
                                                height: "16",
                                                view_box: "0 0 24 24",
                                                fill: "none",
                                                stroke: "currentColor",
                                                stroke_width: "2",
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
                                                polyline { points: "20 6 9 17 4 12" }
                                            }
                                        }
                                        span { class: "toast-text",
                                            span { class: "toast-name",
                                                "Others can now see your shared content"
                                            }
                                        }
                                    }
                                },
                                Some(ScreenShareToastState::Failed(msg)) => rsx! {
                                    div {
                                        class: "peer-toast toast-error screen-share-toast",
                                        role: "alert",
                                        aria_live: "assertive",
                                        aria_label: "Screen share visibility error",
                                        span { class: "toast-icon",
                                            svg {
                                                width: "16",
                                                height: "16",
                                                view_box: "0 0 24 24",
                                                fill: "none",
                                                stroke: "currentColor",
                                                stroke_width: "2",
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
                                                circle { cx: "12", cy: "12", r: "10" }
                                                line {
                                                    x1: "12",
                                                    y1: "8",
                                                    x2: "12",
                                                    y2: "12",
                                                }
                                                line {
                                                    x1: "12",
                                                    y1: "16",
                                                    x2: "12.01",
                                                    y2: "16",
                                                }
                                            }
                                        }
                                        span { class: "toast-text",
                                            span { class: "toast-name", "{msg}" }
                                        }
                                    }
                                },
                                None => rsx! {},
                            }
                        }
                        if show_muted_toast() {
                            div { class: "peer-toast toast-left",
                                span { class: "toast-icon",
                                    svg {
                                        width: "16",
                                        height: "16",
                                        view_box: "0 0 24 24",
                                        fill: "none",
                                        stroke: "currentColor",
                                        stroke_width: "2",
                                        stroke_linecap: "round",
                                        stroke_linejoin: "round",
                                        line {
                                            x1: "1",
                                            y1: "1",
                                            x2: "23",
                                            y2: "23",
                                        }
                                        path { d: "M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V4a3 3 0 0 0-5.94-.6" }
                                        path { d: "M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" }
                                        line {
                                            x1: "12",
                                            y1: "19",
                                            x2: "12",
                                            y2: "23",
                                        }
                                        line {
                                            x1: "8",
                                            y1: "23",
                                            x2: "16",
                                            y2: "23",
                                        }
                                    }
                                }
                                span { class: "toast-text",
                                    span { class: "toast-name", "Host muted your microphone" }
                                    br {}
                                    span { class: "toast-action", "Click the mic button to unmute." }
                                }
                            }
                        }
                        if show_video_off_toast() {
                            div { class: "peer-toast toast-left",
                                span { class: "toast-icon",
                                    svg {
                                        width: "16",
                                        height: "16",
                                        view_box: "0 0 24 24",
                                        fill: "none",
                                        stroke: "currentColor",
                                        stroke_width: "2",
                                        stroke_linecap: "round",
                                        stroke_linejoin: "round",
                                        path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                                        line { x1: "1", y1: "1", x2: "23", y2: "23" }
                                    }
                                }
                                span { class: "toast-text",
                                    span { class: "toast-name", "Host turned off your camera" }
                                    br {}
                                    span { class: "toast-action", "Click the camera button to turn it back on." }
                                }
                            }
                        }
                        if let Some(host_msg) = host_change_toast() {
                            div { class: "peer-toast toast-joined",
                                span { class: "toast-icon",
                                    svg {
                                        width: "16",
                                        height: "16",
                                        view_box: "0 0 24 24",
                                        fill: "none",
                                        stroke: "currentColor",
                                        stroke_width: "2",
                                        stroke_linecap: "round",
                                        stroke_linejoin: "round",
                                        path { d: "M2 18h20l-2-9-4 4-4-7-4 7-4-4-2 9Z" }
                                    }
                                }
                                span { class: "toast-text",
                                    span { class: "toast-name", "{host_msg}" }
                                }
                            }
                        }
                        for (id, display_name, _, is_joined) in peer_toasts().iter().cloned() {
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

                div { id: "grid-container", class: "{container_class}", style: "{container_style}",
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

                    // Meeting-level decode-budget banner (#1142 Phase 1). It owns
                    // its own anti-flap damper, so it is mounted UNCONDITIONALLY —
                    // it self-gates on `pressured`/`avatar_count` and the sustain /
                    // dwell / back-off policy. `natural` is the uncapped layout
                    // tile count (`total_tiles`); "Show all videos" pins the
                    // override to `Fixed(natural)`, which `effective_cap` clamps to
                    // `min(natural, CANVAS_LIMIT)` on the next render. Reading
                    // `decode_budget_pressured()` reactively keeps the props live.
                    DecodeBudgetBanner {
                        pressured: decode_budget_pressured(),
                        // Count only the tiles that ACTUALLY render as paused
                        // video — i.e. the shed camera-ON/mock tiles in
                        // `avatar_tiles`. `avatar_tile_count`
                        // (`displayed_tile_count - visible_tile_count`) also
                        // includes displayed cells filled by camera-OFF peers,
                        // which render as plain (non-paused) avatars from the
                        // separate `camera_off_tiles` group (#1465); counting
                        // those would over-state "N videos paused" and re-surface
                        // the "camera-off looks sheddable" inconsistency #1465
                        // set out to kill. During screen share the active layout
                        // is the SS panel, whose paused-video tiles live in
                        // `ss_avatar_tiles` — use that count so "N videos paused"
                        // matches what the user actually sees (#1472).
                        avatar_count: if has_screen_share {
                            ss_avatar_tiles.len()
                        } else {
                            avatar_tiles.len()
                        },
                        natural: total_tiles,
                        on_screen: banner_on_screen,
                    }

                    // Persistent "N videos paused" pill (#1142 FINAL DESIGN).
                    // Sibling of the banner. The banner is the onset alert
                    // (heavily anti-flapped, backs off); the pill is the
                    // persistent level signpost that holds the affordance for as
                    // long as tiles are paused. The two never co-exist on screen:
                    // the pill reads the banner's PUBLISHED on-screen state
                    // (dismiss-aware) via the shared `banner_on_screen` signal and
                    // suppresses itself while the banner is actually on screen — so
                    // when the banner backs off, hides naturally, OR is dismissed
                    // by the user, the pill takes over the signpost. No shadow
                    // approximation of the damper. Co-existence is prevented
                    // immediately (the pill's render gate reads the banner's
                    // published state reactively, so a banner appearance
                    // suppresses the pill on the same frame); the reverse takeover
                    // when the banner hides has up to ~1 s latency from the pill's
                    // 1 Hz poll — a brief gap, never an overlap.
                    DecodePausedPill {
                        avatar_count: if has_screen_share {
                            ss_avatar_tiles.len()
                        } else {
                            avatar_tiles.len()
                        },
                        natural: total_tiles,
                        banner_on_screen: banner_on_screen,
                    }

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
                                            my_session_id: my_session_id.clone(),
                                            pinned_peer_id: current_pinned.clone(),
                                            // HCL bug #2: the shared-content tile shows
                                            // ONLY the screen-share metric in its popup.
                                            meter_mode: SignalMeterMode::ScreenOnly,
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
                                // Right panel — CSS grid via auto-fill (see .ss-peer-panel in style.css).
                                div {
                                    class: "ss-peer-panel",
                                    style: "width: {right_pct:.2}%;",
                                    // Decoded tiles — live video canvas
                                    for tile_id in ss_decoded_tiles.iter() {
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
                                                        my_session_id: my_session_id.clone(),
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
                                                        my_session_id: my_session_id.clone(),
                                                        pinned_peer_id: current_pinned.clone(),
                                                        on_toggle_pin: toggle_pin.clone(),
                                                        room_id: Some(id.clone()),
                                                        is_current_user_host: is_owner,
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Off-budget avatar tiles — rendered in DOM
                                    // but no video decode (force_avatar: true).
                                    for tile_id in ss_avatar_tiles.iter() {
                                        {
                                            let is_mock = tile_id.starts_with("mock-");
                                            if is_mock {
                                                rsx! {
                                                    PeerTile {
                                                        key: "tile-{tile_id}",
                                                        peer_id: tile_id.clone(),
                                                        full_bleed: false,
                                                        force_avatar: true,
                                                        host_user_id: host_user_id.clone(),
                                                        render_mode: TileMode::VideoOnly,
                                                        my_session_id: my_session_id.clone(),
                                                        on_toggle_pin: move |_: String| {},
                                                    }
                                                }
                                            } else {
                                                rsx! {
                                                    PeerTile {
                                                        key: "tile-{tile_id}",
                                                        peer_id: tile_id.clone(),
                                                        full_bleed: false,
                                                        force_avatar: true,
                                                        host_user_id: host_user_id.clone(),
                                                        render_mode: TileMode::VideoOnly,
                                                        my_session_id: my_session_id.clone(),
                                                        pinned_peer_id: current_pinned.clone(),
                                                        on_toggle_pin: toggle_pin.clone(),
                                                        // Issue #1466: PLAY button force-decodes this SS off-budget peer.
                                                        on_request_decode: toggle_request_decode,
                                                        room_id: Some(id.clone()),
                                                        is_current_user_host: is_owner,
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Camera-off peers (issue #1465): real peers
                                    // with no video to decode. Rendered as PLAIN
                                    // avatars (no `force_avatar` → no dashed
                                    // off-budget outline). Always the real-peer
                                    // arm — these are never mocks.
                                    for tile_id in ss_camera_off_tiles.iter() {
                                        PeerTile {
                                            key: "tile-{tile_id}",
                                            peer_id: tile_id.clone(),
                                            full_bleed: false,
                                            host_user_id: host_user_id.clone(),
                                            render_mode: TileMode::VideoOnly,
                                            my_session_id: my_session_id.clone(),
                                            pinned_peer_id: current_pinned.clone(),
                                            on_toggle_pin: toggle_pin.clone(),
                                            room_id: Some(id.clone()),
                                            is_current_user_host: is_owner,
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
                                // Full-bleed only when this is the single tile on
                                // screen across ALL render groups (issues #1465,
                                // #508) — see `sole_real_tile` above. Was keyed
                                // off `visible_tile_count == 1`, which the #1465
                                // camera-off split made unsafe.
                                let full_bleed = !is_mock
                                    && sole_real_tile
                                    && !client.is_screen_share_enabled_for_peer(tile_id);
                                if is_mock {
                                    rsx! {
                                        PeerTile {
                                            key: "tile-{tile_id}",
                                            peer_id: tile_id.clone(),
                                            full_bleed: false,
                                            host_user_id: host_user_id.clone(),
                                            my_session_id: my_session_id.clone(),
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
                                            my_session_id: my_session_id.clone(),
                                            pinned_peer_id: current_pinned.clone(),
                                            on_toggle_pin: toggle_pin.clone(),
                                            room_id: Some(id.clone()),
                                            is_current_user_host: is_owner,
                                        }
                                    }
                                }
                            }
                        }

                        // ---- Off-budget avatar tiles (issue #987, task 1a.4) ----
                        // Peers the layout could show but the decode-budget cap
                        // excluded from video decode. They render via the SAME
                        // `PeerTile` component with `force_avatar: true`, so they
                        // show the avatar/initials placeholder (no canvas, no
                        // decode) while keeping name, mic state and host controls.
                        // They are NOT in `active_decode_set` (it is built from
                        // `visible_tiles` only), but their audio is untouched.
                        // `avatar_tiles` is empty unless a budget cap is active,
                        // so this loop is a no-op on the default path.
                        for tile_id in avatar_tiles.iter() {
                            {
                                let is_mock = tile_id.starts_with("mock-");
                                if is_mock {
                                    rsx! {
                                        PeerTile {
                                            key: "tile-{tile_id}",
                                            peer_id: tile_id.clone(),
                                            full_bleed: false,
                                            force_avatar: true,
                                            host_user_id: host_user_id.clone(),
                                            my_session_id: my_session_id.clone(),
                                            on_toggle_pin: move |_: String| {},
                                        }
                                    }
                                } else {
                                    rsx! {
                                        PeerTile {
                                            key: "tile-{tile_id}",
                                            peer_id: tile_id.clone(),
                                            full_bleed: false,
                                            force_avatar: true,
                                            host_user_id: host_user_id.clone(),
                                            my_session_id: my_session_id.clone(),
                                            pinned_peer_id: current_pinned.clone(),
                                            on_toggle_pin: toggle_pin.clone(),
                                            // Issue #1466: PLAY button force-decodes this off-budget peer.
                                            on_request_decode: toggle_request_decode,
                                            room_id: Some(id.clone()),
                                            is_current_user_host: is_owner,
                                        }
                                    }
                                }
                            }
                        }

                        // ---- Camera-off peers (issue #1465) ----
                        // Real peers with no video to decode. They occupy the
                        // remaining displayed grid cells (after camera-on +
                        // avatar tiles) and render as PLAIN avatars — NO
                        // `force_avatar`, so NO dashed off-budget outline (the
                        // #1465 fix: a cameraless peer is not "paused", it has
                        // nothing to shed). Capped to `off_to_render` so any
                        // camera-off peers in the overflow region stay folded
                        // into the +N badge and the rendered tile count still
                        // equals `tile_count` (see proof above). Always the
                        // real-peer arm — these are never mocks.
                        //
                        // Full-bleed (issues #1465, #508): a lone camera-off
                        // remote peer renders full-bleed — the "Camera Off"
                        // placeholder fills the tile, matching the pre-#1465
                        // single-peer presentation (canvas_generator renders this
                        // correctly with no change). `sole_real_tile` guarantees
                        // there is no other decoded / avatar / camera-off tile, so
                        // this rule and the visible_tiles rule can never both
                        // believe their tile is alone. These entries are never
                        // mocks, so the `!is_mock` guard the visible rule carries
                        // is unconditionally true here and omitted.
                        for tile_id in camera_off_tiles.iter() {
                            {
                                let full_bleed = sole_real_tile
                                    && !client.is_screen_share_enabled_for_peer(tile_id);
                                rsx! {
                                    PeerTile {
                                        key: "tile-{tile_id}",
                                        peer_id: tile_id.clone(),
                                        full_bleed,
                                        host_user_id: host_user_id.clone(),
                                        my_session_id: my_session_id.clone(),
                                        pinned_peer_id: current_pinned.clone(),
                                        on_toggle_pin: toggle_pin.clone(),
                                        room_id: Some(id.clone()),
                                        is_current_user_host: is_owner,
                                    }
                                }
                            }
                        }

                        if overflow_count > 0 {
                            div { class: "grid-overflow-badge",
                                "+{overflow_count}"
                                span { "more in meeting" }
                            }
                        }

                        // Invitation overlay when no peers (issue #1465).
                        // Previously gated on `visible_tiles.is_empty()`, but a
                        // call where every remote peer is camera-off now has an
                        // empty `visible_tiles` while those peers still render in
                        // `camera_off_tiles` — showing "Your meeting is ready!"
                        // over a populated grid would be wrong. Gate instead on
                        // there being NO peers at all: `all_tiles` (camera-ON real
                        // peers + mock placeholders) AND `camera_off_real` (real
                        // camera-off peers) must both be empty.
                        if all_tiles.is_empty() && camera_off_real.is_empty() {
                            div {
                                id: "invite-overlay",
                                class: "invite-glass-card",

                                h4 { class: "invite-glass-title", "Your meeting is ready!" }

                                button {
                                    class: if show_copy_toast() { "invite-share-button copied" } else { "invite-share-button" },
                                    r#type: "button",
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

                                    span { class: "invite-share-icon", "↗" }
                                    span {
                                        if show_copy_toast() {
                                            "LINK COPIED"
                                        } else {
                                            "SHARE THE LINK"
                                        }
                                    }
                                    span { class: "invite-copy-icon", "⧉" }
                                }

                                div {
                                    class: if show_copy_toast() { "copy-toast copy-toast--visible" } else { "copy-toast" },
                                    role: "alert",
                                    "aria-live": "assertive",
                                    "Link copied"
                                }
                            }
                        }
                    } // end of else (normal grid layout)

                    // Controls nav
                    if can_stream {
                        nav {
                            id: "host-controls-nav",
                            class: "host",
                            style: "box-shadow: none; transition: border-color 0.3s ease-out, box-shadow 1.5s ease-out;",
                            onmounted: move |evt| {
                                if let Some(elem) = evt.try_as_web_event() {
                                    host_el.set(Some(elem));
                                }
                            },
                            div { class: "controls",
                                // Visual-only backdrop stays conditional on
                                // customize_mode — it dims the app while editing.
                                if customize_mode() {
                                    div { class: "customize-backdrop" }
                                }
                                // Enter-customize announcement.  Region is ALWAYS
                                // mounted; only its text content toggles.  Some
                                // older AT (JAWS, some NVDA versions) do not fire
                                // a polite announcement when a live region enters
                                // the DOM already containing text — they only
                                // announce on subsequent text mutations.  Keeping
                                // the region mounted with empty text on load and
                                // filling it on customize-enter (empty → text) is
                                // the mutation shape those readers reliably pick
                                // up.  Cleared back to empty on exit so the
                                // instructions are not re-announced next time.
                                div {
                                    class: "visually-hidden",
                                    role: "status",
                                    "aria-live": "polite",
                                    {
                                        if customize_mode() {
                                            "Customizing action bar. Tab to a button and press arrow keys to move its slot within the bar, or drag with the pointer. Press the minus button to hide a slot, then press Done to finish."
                                        } else {
                                            ""
                                        }
                                    }
                                }
                                // Second live region dedicated to keyboard-reorder
                                // feedback.  `aria-atomic=true` forces the whole
                                // text to be re-announced each time it changes,
                                // even for a single-word delta.  Also always
                                // mounted — `action_bar_announce` is a Signal that
                                // starts empty, is written on each keyboard
                                // reorder, and is reset back to empty in the
                                // customize_mode-exit `use_effect` (search for
                                // "Silence the keyboard-reorder live region").
                                div {
                                    class: "visually-hidden",
                                    role: "status",
                                    "aria-live": "polite",
                                    "aria-atomic": "true",
                                    "{action_bar_announce}"
                                }
                                nav {
                                    class: {
                                        let pos = dock_position().css_class();
                                        let hidden = if controls_visible() { "" } else { " controls-hidden" };
                                        let expanded = if controls_expanded() || customize_mode() { " controls-expanded" } else { "" };
                                        let cust = if customize_mode() { " customize-mode" } else { "" };
                                        let drag_cls = if customize_mode() && dragging_slot().is_some() && drag_started() { " drag-active" } else { "" };
                                        format!("video-controls-container {pos}{hidden}{expanded}{cust}{drag_cls}")
                                    },
                                    // Keyboard reorder (WCAG 2.1.1): with focus on any
                                    // customize-mode slot button, Arrow keys / Home / End move
                                    // the slot within the bar. The handler is on the nav so a
                                    // single closure serves every slot; the target's data-slot
                                    // attribute (present on each wrapper) identifies which slot
                                    // to move. `Event.target` survives after dispatch even in
                                    // Dioxus synthetic events (unlike `currentTarget`), so
                                    // `target().closest(..)` is the reliable lookup path.
                                    //
                                    // Filters (each is a real bug the live tester hit):
                                    // - Modifier held → skip. Cmd/Ctrl/Alt+Arrow generate Home/
                                    //   End on macOS/Chromebook; without this guard, hitting
                                    //   Cmd+ArrowLeft to jump home also jumped the slot to
                                    //   position 1, and Cmd+ArrowRight jumped to position N —
                                    //   the "jumps to 9 first, then walks to 5" report.
                                    // - `KeyboardEvent.repeat` → skip. OS-level auto-repeat
                                    //   fires ~30 events/s while a key is held; a slot would
                                    //   race from 3 to 9 in a blink. Single press = single step.
                                    // - Target inside the remove button → skip. Arrow keys
                                    //   there mean "I'm about to click Remove", not "reorder".
                                    onkeydown: move |evt: Event<KeyboardData>| {
                                        if !customize_mode() { return; }
                                        // Escape exits customize mode entirely (the standard
                                        // modal-ish idiom). Handled BEFORE the modifier check
                                        // so a user pressing plain Escape isn't gated by an
                                        // accidentally-held modifier, and BEFORE the arrow
                                        // match so it's an unambiguous exit path. Save +
                                        // restore focus to the dock-menu trigger, exactly
                                        // like the Done button's onclick above.
                                        if evt.key() == Key::Escape {
                                            evt.stop_propagation();
                                            evt.prevent_default();
                                            customize_mode.set(false);
                                            save_action_bar_layout(&action_bar_slots.read(), &action_bar_hidden.read());
                                            Timeout::new(0, || {
                                                focus_element_by_id("dock-menu-trigger");
                                            })
                                            .forget();
                                            return;
                                        }
                                        // Any modifier held → treat as a browser shortcut
                                        // (Cmd/Ctrl+Arrow = Home/End on macOS, Alt+Arrow =
                                        // history nav, Shift+Arrow = text selection).
                                        let m = evt.modifiers();
                                        if m.contains(Modifiers::CONTROL)
                                            || m.contains(Modifiers::META)
                                            || m.contains(Modifiers::ALT)
                                            || m.contains(Modifiers::SHIFT)
                                        {
                                            return;
                                        }
                                        // Enum match (matches the codebase convention and
                                        // avoids Key::to_string() edge cases across browsers).
                                        let (delta, absolute): (Option<i32>, Option<i32>) = match evt.key() {
                                            Key::ArrowLeft | Key::ArrowUp => (Some(-1), None),
                                            Key::ArrowRight | Key::ArrowDown => (Some(1), None),
                                            Key::Home => (None, Some(0)),
                                            Key::End => (None, Some(i32::MAX)),
                                            _ => return,
                                        };
                                        let ke: web_sys::KeyboardEvent = evt.as_web_event().unchecked_into();
                                        // OS-level auto-repeat → skip. A held key must not
                                        // fast-forward a slot through the whole bar.
                                        if ke.repeat() { return; }
                                        let Some(target) = ke.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return; };
                                        // If focus is on the − remove button inside a slot,
                                        // arrow keys must not reorder that slot; the user is on
                                        // that button to click Remove.
                                        if let Ok(Some(_)) = target.closest(".action-bar-remove-btn") {
                                            return;
                                        }
                                        let Ok(Some(wrapper)) = target.closest(".action-bar-slot-wrapper[data-slot]") else { return; };
                                        let slug = wrapper.get_attribute("data-slot").unwrap_or_default();
                                        let Some(slot) = ActionBarSlot::from_slug(&slug) else { return; };
                                        // Always suppress the browser's default (page scroll on
                                        // arrows, Home/End jump) so a focused slot in customize
                                        // mode never scrolls the meeting view. Even a no-op key
                                        // (already at the edge) should not scroll.
                                        evt.prevent_default();
                                        let ios_device = is_ios();
                                        let current_full = action_bar_slots.read().clone();
                                        let mut visible_slots = visible_action_bar_slots(
                                            &current_full,
                                            customize_mode(),
                                            ios_device,
                                            has_screen_share,
                                            is_owner,
                                        );
                                        let Some(result) = apply_keyboard_reorder(&mut visible_slots, slot, delta, absolute) else { return; };
                                        let len = visible_slots.len();
                                        if result.new_idx == result.old_idx {
                                            action_bar_announce.set(format!(
                                                "{} is already at position {} of {}.",
                                                slot.display_name(),
                                                result.old_idx + 1,
                                                len,
                                            ));
                                            return;
                                        }
                                        let next = merge_visible_action_bar_slots(
                                            &current_full,
                                            &visible_slots,
                                            customize_mode(),
                                            ios_device,
                                            has_screen_share,
                                            is_owner,
                                        );
                                        action_bar_slots.set(next);
                                        save_action_bar_layout(&action_bar_slots.read(), &action_bar_hidden.read());
                                        action_bar_announce.set(format!(
                                            "{} moved to position {} of {}.",
                                            slot.display_name(),
                                            result.new_idx + 1,
                                            len,
                                        ));
                                        // Preserve keyboard continuity after reorder: keep
                                        // focus on the moved slot's primary button so Tab
                                        // continues from the user's current position.
                                        let moved_selector = format!(
                                            ".video-controls-container .action-bar-slot-wrapper[data-slot=\"{}\"] > button.video-control-button",
                                            slot.slug()
                                        );
                                        Timeout::new(0, move || {
                                            focus_by_selector(&moved_selector);
                                        })
                                        .forget();
                                    },
                                    onpointermove: {
                                        let drag_slot_size = drag_slot_size.clone();
                                        let drag_start_x = drag_start_x.clone();
                                        let drag_start_y = drag_start_y.clone();
                                        move |evt: PointerEvent| {
                                            if !customize_mode() || dragging_slot().is_none() { return; }
                                            let pe: web_sys::PointerEvent = evt.as_web_event().unchecked_into();
                                            let cx = pe.client_x() as f64;
                                            let cy = pe.client_y() as f64;
                                            drag_pointer_x.set(cx);
                                            drag_pointer_y.set(cy);
                                            // Nav rect was captured at pointerdown; the nav doesn't
                                            // move during drag, so don't pay the layout cost of
                                            // closest(..) + getBoundingClientRect() on every move.
                                            if !drag_started() {
                                                let dx = (cx - drag_start_x.get()).abs();
                                                let dy = (cy - drag_start_y.get()).abs();
                                                if dx + dy < 5.0 { return; }
                                                drag_started.set(true);
                                            }
                                            let slot_size = drag_slot_size.get();
                                            if slot_size <= 0.0 { return; }
                                            let is_vertical = dock_position() != DockPosition::Bottom;
                                            let dragged = dragging_slot().unwrap();
                                            let ios_device = is_ios();
                                            // Compute insertion index from ORIGINAL visible position +
                                            // cursor delta so drag math follows the rendered bar order.
                                            let orig_slots = drag_orig_layout();
                                            let orig_visible = visible_action_bar_slots(
                                                &orig_slots,
                                                customize_mode(),
                                                ios_device,
                                                has_screen_share,
                                                is_owner,
                                            );
                                            let orig_idx = orig_visible.iter().position(|s| *s == dragged).unwrap_or(0);
                                            let num_slots = orig_visible.len();
                                            if num_slots == 0 { return; }
                                            let cursor = if is_vertical { cy } else { cx };
                                            let origin = if is_vertical { drag_start_y.get() } else { drag_start_x.get() };
                                            let delta = cursor - origin;
                                            let shift_count = (delta / slot_size).round() as i32;
                                            let new_idx = (orig_idx as i32 + shift_count).clamp(0, num_slots as i32 - 1) as usize;
                                            if drag_insertion_idx() != Some(new_idx) {
                                                let current_full = action_bar_slots.read().clone();
                                                let mut next_visible = visible_action_bar_slots(
                                                    &current_full,
                                                    customize_mode(),
                                                    ios_device,
                                                    has_screen_share,
                                                    is_owner,
                                                );
                                                if let Some(cur) = next_visible.iter().position(|s| *s == dragged) {
                                                    next_visible.remove(cur);
                                                    next_visible.insert(new_idx.min(next_visible.len()), dragged);
                                                    let next_full = merge_visible_action_bar_slots(
                                                        &current_full,
                                                        &next_visible,
                                                        customize_mode(),
                                                        ios_device,
                                                        has_screen_share,
                                                        is_owner,
                                                    );
                                                    action_bar_slots.set(next_full);
                                                }
                                                drag_insertion_idx.set(Some(new_idx));
                                            }
                                        }
                                    },
                                    onpointerup: {
                                        let drag_slot_size = drag_slot_size.clone();
                                        let drag_grab_dx = drag_grab_dx.clone();
                                        let drag_grab_dy = drag_grab_dy.clone();
                                        let drag_nav_left = drag_nav_left.clone();
                                        let drag_nav_top = drag_nav_top.clone();
                                        let drag_pointer_id = drag_pointer_id.clone();
                                        let drag_start_x = drag_start_x.clone();
                                        let drag_start_y = drag_start_y.clone();
                                        move |_evt: PointerEvent| {
                                            if !customize_mode() || dragging_slot().is_none() { return; }
                                            // Live-slots: layout is already correct in action_bar_slots
                                            save_action_bar_layout(&action_bar_slots.read(), &action_bar_hidden.read());
                                            dragging_slot.set(None);
                                            drag_pointer_x.set(0.0);
                                            drag_pointer_y.set(0.0);
                                            drag_insertion_idx.set(None);
                                            drag_started.set(false);
                                            drag_slot_size.set(0.0);
                                            drag_grab_dx.set(0.0);
                                            drag_grab_dy.set(0.0);
                                            drag_nav_left.set(0.0);
                                            drag_nav_top.set(0.0);
                                            drag_pointer_id.set(0);
                                            drag_start_x.set(0.0);
                                            drag_start_y.set(0.0);
                                        }
                                    },
                                    onpointercancel: {
                                        let drag_slot_size = drag_slot_size.clone();
                                        let drag_grab_dx = drag_grab_dx.clone();
                                        let drag_grab_dy = drag_grab_dy.clone();
                                        let drag_nav_left = drag_nav_left.clone();
                                        let drag_nav_top = drag_nav_top.clone();
                                        let drag_pointer_id = drag_pointer_id.clone();
                                        let drag_start_x = drag_start_x.clone();
                                        let drag_start_y = drag_start_y.clone();
                                        move |_evt: PointerEvent| {
                                            if dragging_slot().is_some() {
                                                // Revert to original layout on cancel
                                                action_bar_slots.set(drag_orig_layout());
                                                save_action_bar_layout(&action_bar_slots.read(), &action_bar_hidden.read());
                                                dragging_slot.set(None);
                                                drag_pointer_x.set(0.0);
                                                drag_pointer_y.set(0.0);
                                                drag_insertion_idx.set(None);
                                                drag_started.set(false);
                                                drag_slot_size.set(0.0);
                                                drag_grab_dx.set(0.0);
                                                drag_grab_dy.set(0.0);
                                                drag_nav_left.set(0.0);
                                                drag_nav_top.set(0.0);
                                                drag_pointer_id.set(0);
                                                drag_start_x.set(0.0);
                                                drag_start_y.set(0.0);
                                            }
                                        }
                                    },
                                    onlostpointercapture: {
                                        let drag_slot_size = drag_slot_size.clone();
                                        let drag_grab_dx = drag_grab_dx.clone();
                                        let drag_grab_dy = drag_grab_dy.clone();
                                        let drag_nav_left = drag_nav_left.clone();
                                        let drag_nav_top = drag_nav_top.clone();
                                        let drag_pointer_id = drag_pointer_id.clone();
                                        let drag_start_x = drag_start_x.clone();
                                        let drag_start_y = drag_start_y.clone();
                                        move |_evt: PointerEvent| {
                                            if dragging_slot().is_some() {
                                                action_bar_slots.set(drag_orig_layout());
                                                save_action_bar_layout(&action_bar_slots.read(), &action_bar_hidden.read());
                                                dragging_slot.set(None);
                                                drag_pointer_x.set(0.0);
                                                drag_pointer_y.set(0.0);
                                                drag_insertion_idx.set(None);
                                                drag_started.set(false);
                                                drag_slot_size.set(0.0);
                                                drag_grab_dx.set(0.0);
                                                drag_grab_dy.set(0.0);
                                                drag_nav_left.set(0.0);
                                                drag_nav_top.set(0.0);
                                                drag_pointer_id.set(0);
                                                drag_start_x.set(0.0);
                                                drag_start_y.set(0.0);
                                            }
                                        }
                                    },
                                    // Customizable slots: render only the visible subset so every
                                    // emitted sibling is keyed (no keyed+keyless fragment mix).
                                    for slot in visible_action_bar_slots(
                                        &action_bar_slots.read(),
                                        customize_mode(),
                                        is_ios(),
                                        has_screen_share,
                                        is_owner,
                                    ) {
                                        {
                                        let slug = slot.slug();
                                                let tier = match slot {
                                                    ActionBarSlot::Mic | ActionBarSlot::Camera => "slot-primary",
                                                    _ => "slot-secondary",
                                                };
                                                let is_dragging = dragging_slot() == Some(slot) && drag_started();
                                                let wrapper_class = if is_dragging {
                                                    format!("action-bar-slot-wrapper {tier} is-drag-placeholder")
                                                } else {
                                                    format!("action-bar-slot-wrapper {tier}")
                                                };
                                                let drag_slot_size_c = drag_slot_size.clone();
                                                let drag_pointer_id_c = drag_pointer_id.clone();
                                                let drag_grab_dx_c = drag_grab_dx.clone();
                                                let drag_grab_dy_c = drag_grab_dy.clone();
                                                let drag_start_x_c = drag_start_x.clone();
                                                let drag_start_y_c = drag_start_y.clone();
                                                let drag_nav_left_c = drag_nav_left.clone();
                                                let drag_nav_top_c = drag_nav_top.clone();
                                                rsx! {
                                                    div {
                                                        key: "{slug}",
                                                        "data-slot": slug,
                                                        class: "{wrapper_class}",
                                                        onpointerdown: move |evt: PointerEvent| {
                                                            if !customize_mode() { return; }
                                                            let pe: web_sys::PointerEvent = evt.as_web_event().unchecked_into();
                                                            let cx = pe.client_x() as f64;
                                                            let cy = pe.client_y() as f64;
                                                            drag_start_x_c.set(cx);
                                                            drag_start_y_c.set(cy);
                                                            drag_started.set(false);
                                                            drag_pointer_id_c.set(pe.pointer_id());
                                                            let current_full = action_bar_slots.read().clone();
                                                            drag_orig_layout.set(current_full.clone());
                                                            let src_visible = visible_action_bar_slots(
                                                                &current_full,
                                                                customize_mode(),
                                                                is_ios(),
                                                                has_screen_share,
                                                                is_owner,
                                                            );
                                                            let src_idx = src_visible.iter().position(|s| *s == slot).unwrap_or(0);
                                                            dragging_slot.set(Some(slot));
                                                            drag_pointer_x.set(cx);
                                                            drag_pointer_y.set(cy);
                                                            drag_insertion_idx.set(Some(src_idx));
                                                            if let Some(target) = pe.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                                                                if let Ok(Some(el)) = target.closest(".action-bar-slot-wrapper") {
                                                                    let rect = el.get_bounding_client_rect();
                                                                    drag_grab_dx_c.set(cx - rect.left());
                                                                    drag_grab_dy_c.set(cy - rect.top());
                                                                    let is_vertical = dock_position() != DockPosition::Bottom;
                                                                    let size = if is_vertical { rect.height() } else { rect.width() };
                                                                    if let Ok(Some(nav)) = el.closest(".video-controls-container") {
                                                                        let nrect = nav.get_bounding_client_rect();
                                                                        drag_nav_left_c.set(nrect.left());
                                                                        drag_nav_top_c.set(nrect.top());
                                                                        let gap = read_nav_axis_gap_px(&nav, is_vertical);
                                                                        drag_slot_size_c.set(size + gap);
                                                                        let _ = nav.set_pointer_capture(pe.pointer_id());
                                                                    }
                                                                }
                                                            }
                                                        },
                                                        // Slot-specific inner content
                                                        match slot {
                                                            ActionBarSlot::Mic => {
                                                                let mda_mic = mda.clone();
                                                                rsx! {
                                                                    MicButton {
                                                                        enabled: mic_enabled(),
                                                                        available: mic_error.read().is_none(),
                                                                        onclick: move |_| {
                                                                            if customize_mode() { return; }
                                                                            if !mic_enabled() {
                                                                                // Turn the mic ON. If acquisition is blocked
                                                                                // (DeviceInUse), `pending_mic_enable` lets a
                                                                                // later successful probe fulfil the enable;
                                                                                // the background retry loop arms itself off
                                                                                // the error signal alone (see should_auto_retry).
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
                                                            ActionBarSlot::Camera => {
                                                                let mda_cam = mda.clone();
                                                                rsx! {
                                                                    CameraButton {
                                                                        enabled: video_enabled(),
                                                                        available: video_error.read().is_none(),
                                                                        onclick: move |_| {
                                                                            if customize_mode() { return; }
                                                                            if !video_enabled() {
                                                                                // Turn the camera ON. If acquisition is blocked
                                                                                // (DeviceInUse), `pending_video_enable` lets a
                                                                                // later successful probe fulfil the enable;
                                                                                // the background retry loop arms itself off
                                                                                // the error signal alone (see should_auto_retry).
                                                                                if mda_cam.borrow().is_granted(MediaAccessKind::VideoCheck) {
                                                                                    video_enabled.set(true);
                                                                                    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                                                                                        if let Some(elem) = doc.get_element_by_id("webcam") {
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
                                                            ActionBarSlot::ScreenShare => {
                                                                let is_active = matches!(screen_share_state(), ScreenShareState::Active);
                                                                let is_disabled = matches!(
                                                                    screen_share_state(),
                                                                    ScreenShareState::Requesting | ScreenShareState::StreamReady
                                                                );
                                                                let stream_cell = pre_acquired_screen_stream.clone();
                                                                rsx! {
                                                                    ScreenShareButton {
                                                                        active: is_active,
                                                                        disabled: is_disabled,
                                                                        onclick: move |_| {
                                                                            if customize_mode() { return; }
                                                                            if matches!(screen_share_state(), ScreenShareState::Idle) {
                                                                                let navigator = gloo_utils::window().navigator();
                                                                                let media_devices = match navigator.media_devices() {
                                                                                    Ok(md) => md,
                                                                                    Err(e) => {
                                                                                        log::error!("Failed to get media devices: {e:?}");
                                                                                        return;
                                                                                    }
                                                                                };

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

                                                                                let promise = match media_devices
                                                                                    .get_display_media_with_constraints(&constraints)
                                                                                {
                                                                                    Ok(p) => p,
                                                                                    Err(e) => {
                                                                                        log::error!("getDisplayMedia failed synchronously: {e:?}");
                                                                                        return;
                                                                                    }
                                                                                };
                                                                                screen_share_state.set(ScreenShareState::Requesting);
                                                                                let cell = stream_cell.clone();
                                                                                wasm_bindgen_futures::spawn_local(async move {
                                                                                    match JsFuture::from(promise).await {
                                                                                        Ok(stream) => {
                                                                                            let media_stream: web_sys::MediaStream = stream
                                                                                                .unchecked_into();
                                                                                            cell.borrow_mut().replace(media_stream);
                                                                                            screen_share_state.set(ScreenShareState::StreamReady);
                                                                                        }
                                                                                        Err(e) => {
                                                                                            let is_cancel = js_sys::Reflect::get(
                                                                                                    &e,
                                                                                                    &JsValue::from_str("name"),
                                                                                                )
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
                                                                        },
                                                                    }
                                                                }
                                                            }
                                                            ActionBarSlot::PeerList => rsx! {
                                                                PeerListButton {
                                                                    id: "peer-list-trigger",
                                                                    open: peer_list_open(),
                                                                    onclick: move |e: MouseEvent| {
                                                                        if customize_mode() { return; }
                                                                        // Mirror the density toggle: stop the click
                                                                        // reaching `#main-container`'s handler (the
                                                                        // toggle is inside the action bar, so it would
                                                                        // be ignored there anyway, but stopping here
                                                                        // keeps the popover-close identical to density).
                                                                        e.stop_propagation();
                                                                        let opening = !peer_list_open();
                                                                        peer_list_open.set(opening);
                                                                        if opening {
                                                                            density_open.set(false);
                                                                            dock_menu_open.set(false);
                                                                            mock_peers_open.set(false);
                                                                        }
                                                                    },
                                                                }
                                                            },
                                                            ActionBarSlot::DensityMode => rsx! {
                                                                DensityModeButton {
                                                                    label: density_mode().label().to_string(),
                                                                    open: density_open(),
                                                                    onclick: move |e: MouseEvent| {
                                                                        if customize_mode() { return; }
                                                                        e.stop_propagation();
                                                                        let opening = !density_open();
                                                                        density_open.set(opening);
                                                                        if opening {
                                                                            dock_menu_open.set(false);
                                                                            mock_peers_open.set(false);
                                                                        }
                                                                    },
                                                                }
                                                            },
                                                            ActionBarSlot::Diagnostics => rsx! {
                                                                DiagnosticsButton {
                                                                    id: "diagnostics-trigger",
                                                                    open: diagnostics_open(),
                                                                    onclick: move |e: MouseEvent| {
                                                                        if customize_mode() { return; }
                                                                        // Mirror the density toggle (see PeerListButton
                                                                        // above): stop the click reaching
                                                                        // `#main-container`'s background handler.
                                                                        e.stop_propagation();
                                                                        let opening = !diagnostics_open();
                                                                        diagnostics_open.set(opening);
                                                                        if opening {
                                                                            device_settings_open.set(false);
                                                                            density_open.set(false);
                                                                            dock_menu_open.set(false);
                                                                            mock_peers_open.set(false);
                                                                            meeting_options_open.set(false);
                                                                        }
                                                                    },
                                                                }
                                                            },
                                                            ActionBarSlot::DeviceSettings => rsx! {
                                                                DeviceSettingsButton {
                                                                    open: device_settings_open(),
                                                                    onclick: move |_| {
                                                                        if customize_mode() { return; }
                                                                        device_settings_initial_section.set(None);
                                                                        let was_closed = !device_settings_open();
                                                                        device_settings_open.set(!device_settings_open());
                                                                        if was_closed {
                                                                            device_settings_generation
                                                                                .set(device_settings_generation() + 1);
                                                                            peer_list_open.set(false);
                                                                            diagnostics_open.set(false);
                                                                            density_open.set(false);
                                                                            dock_menu_open.set(false);
                                                                            mock_peers_open.set(false);
                                                                            meeting_options_open.set(false);
                                                                        }
                                                                    },
                                                                }
                                                            },
                                                            ActionBarSlot::MeetingOptions => rsx! {
                                                                MeetingOptionsButton {
                                                                    open: meeting_options_open(),
                                                                    onclick: move |_| {
                                                                        if customize_mode() { return; }
                                                                        meeting_options_open.set(!meeting_options_open());
                                                                    },
                                                                }
                                                            },
                                                        }
                                                        // Remove button: shown in customize mode for
                                                        // removable slots only. Mic/Camera are pinned
                                                        // (NON_REMOVABLE_SLOTS) to prevent mid-call loss
                                                        // of mute/camera-mute controls.
                                                        if customize_mode() && slot.is_removable() {
                                                            button {
                                                                class: "action-bar-remove-btn",
                                                                "aria-label": format!("Remove {}", slot.display_name()),
                                                                title: format!("Remove {}", slot.display_name()),
                                                                onpointerdown: move |evt: PointerEvent| {
                                                                    evt.stop_propagation();
                                                                },
                                                                onclick: move |e| {
                                                                    e.stop_propagation();
                                                                    let mut slots = action_bar_slots.read().clone();
                                                                    slots.retain(|s| *s != slot);
                                                                    action_bar_slots.set(slots);
                                                                    let mut hidden = action_bar_hidden.read().clone();
                                                                    if !hidden.contains(&slot) {
                                                                        hidden.push(slot);
                                                                    }
                                                                    action_bar_hidden.set(hidden);
                                                                    save_action_bar_layout(&action_bar_slots.read(), &action_bar_hidden.read());
                                                                },
                                                                "\u{2212}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                    }
                                        // (а) Dock position dropdown — not customizable (houses Customize/Reset)
                                        div { class: "dock-position-wrapper",
                                            style: "order: 90",
                                            if customize_mode() {
                                                button {
                                                    class: "video-control-button action-bar-done-trigger",
                                                    title: "Done customizing",
                                                    "aria-label": "Done customizing",
                                                    r#type: "button",
                                                    onclick: move |e| {
                                                        e.stop_propagation();
                                                        customize_mode.set(false);
                                                        save_action_bar_layout(&action_bar_slots.read(), &action_bar_hidden.read());
                                                        // Done unmounts on the next render (this branch
                                                        // is `if customize_mode()`), so focus would drop
                                                        // to <body> without a restore. Mirror the entry
                                                        // path (Customize option focuses Done via
                                                        // deferred Timeout after `customize_mode.set(true)`):
                                                        // send focus back to the dock-menu trigger which
                                                        // renders in the same slot after re-render.
                                                        Timeout::new(0, || {
                                                            focus_element_by_id("dock-menu-trigger");
                                                        })
                                                        .forget();
                                                    },
                                                    svg {
                                                        xmlns: "http://www.w3.org/2000/svg",
                                                        width: "20",
                                                        height: "20",
                                                        view_box: "0 0 24 24",
                                                        fill: "none",
                                                        stroke: "currentColor",
                                                        stroke_width: "2.5",
                                                        stroke_linecap: "round",
                                                        stroke_linejoin: "round",
                                                        polyline { points: "4 12 10 18 20 6" }
                                                    }
                                                }
                                            } else {
                                            div { class: if dock_menu_open() { "glass-select open" } else { "glass-select" },
                                                button {
                                                    // Stable id so keyboard-close paths (Escape from
                                                    // an option, Enter/Space activation) can return
                                                    // focus here via `focus_element_by_id`. Same id
                                                    // is intentionally NOT reused on the Done button
                                                    // (customize-mode branch above); Customize's
                                                    // activation now shifts focus to the first
                                                    // action-bar slot button (Mic/Sound by default)
                                                    // via the customize-mode entry effect.
                                                    id: "dock-menu-trigger",
                                                    class: if dock_menu_open() { "video-control-button active" } else { "video-control-button" },
                                                    title: "Action bar position",
                                                    r#type: "button",
                                                    "aria-haspopup": "listbox",
                                                    "aria-expanded": if dock_menu_open() { "true" } else { "false" },
                                                    onclick: move |e| {
                                                        e.stop_propagation();
                                                        let opening = !dock_menu_open();
                                                        dock_menu_open.set(opening);
                                                        if opening {
                                                            density_open.set(false);
                                                            mock_peers_open.set(false);
                                                        }
                                                    },
                                                    // WCAG 2.1.1 keyboard entry to the dock menu.
                                                    // Native `<button>` already fires onclick on
                                                    // Enter/Space, so we only need Escape (close if
                                                    // open) and ArrowDown/ArrowUp (open + focus
                                                    // first/last option). Opening requires deferring
                                                    // focus to the next tick because the menu isn't
                                                    // in the DOM until Dioxus re-renders.
                                                    onkeydown: move |evt: Event<KeyboardData>| {
                                                        let key = evt.key();
                                                        if key == Key::Escape && dock_menu_open() {
                                                            evt.stop_propagation();
                                                            evt.prevent_default();
                                                            dock_menu_open.set(false);
                                                        } else if key == Key::ArrowDown {
                                                            evt.stop_propagation();
                                                            evt.prevent_default();
                                                            if dock_menu_open() {
                                                                focus_glass_option_at(".dock-position-wrapper", false);
                                                            } else {
                                                                dock_menu_open.set(true);
                                                                density_open.set(false);
                                                                mock_peers_open.set(false);
                                                                Timeout::new(0, || {
                                                                    focus_glass_option_at(".dock-position-wrapper", false);
                                                                })
                                                                .forget();
                                                            }
                                                        } else if key == Key::ArrowUp {
                                                            evt.stop_propagation();
                                                            evt.prevent_default();
                                                            if dock_menu_open() {
                                                                focus_glass_option_at(".dock-position-wrapper", true);
                                                            } else {
                                                                dock_menu_open.set(true);
                                                                density_open.set(false);
                                                                mock_peers_open.set(false);
                                                                Timeout::new(0, || {
                                                                    focus_glass_option_at(".dock-position-wrapper", true);
                                                                })
                                                                .forget();
                                                            }
                                                        }
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
                                                        rect { x: "2", y: "8", width: "20", height: "8", rx: "4" }
                                                        circle { cx: "8", cy: "12", r: "1.5", fill: "currentColor", stroke: "none" }
                                                        circle { cx: "12", cy: "12", r: "1.5", fill: "currentColor", stroke: "none" }
                                                        circle { cx: "16", cy: "12", r: "1.5", fill: "currentColor", stroke: "none" }
                                                    }
                                                    span { class: "tooltip",
                                                        span { class: "tooltip-title", "Action bar position" }
                                                        span { class: "tooltip-desc", "Move the action bar to the bottom, left, or right edge of the call." }
                                                    }
                                                }
                                                if dock_menu_open() {
                                                    div {
                                                        class: "glass-select-menu",
                                                        role: "listbox",
                                                        onclick: move |e: MouseEvent| e.stop_propagation(),
                                                        // Menu-level keyboard navigation (WCAG 2.1.1).
                                                        // Each option only owns Enter/Space (activation)
                                                        // — Escape, Arrow keys, Home, and End are
                                                        // handled once here so we don't duplicate the
                                                        // navigation logic across every option. The
                                                        // arrow helpers skip `.glass-select-separator`
                                                        // children by matching on `.glass-select-option`.
                                                        onkeydown: move |evt: Event<KeyboardData>| {
                                                            let key = evt.key();
                                                            if key == Key::Escape {
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                dock_menu_open.set(false);
                                                                focus_element_by_id("dock-menu-trigger");
                                                            } else if key == Key::ArrowDown {
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                focus_glass_option_relative(1);
                                                            } else if key == Key::ArrowUp {
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                focus_glass_option_relative(-1);
                                                            } else if key == Key::Home {
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                focus_glass_option_at(".dock-position-wrapper", false);
                                                            } else if key == Key::End {
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                focus_glass_option_at(".dock-position-wrapper", true);
                                                            }
                                                        },
                                                        div {
                                                            class: if dock_position() == DockPosition::Bottom { "glass-select-option selected" } else { "glass-select-option" },
                                                            role: "option",
                                                            tabindex: "0",
                                                            "aria-selected": if dock_position() == DockPosition::Bottom { "true" } else { "false" },
                                                            onclick: move |e: MouseEvent| {
                                                                e.stop_propagation();
                                                                dock_position.set(DockPosition::Bottom);
                                                                save_dock_position(DockPosition::Bottom);
                                                                dock_menu_open.set(false);
                                                            },
                                                            onkeydown: move |evt: Event<KeyboardData>| {
                                                                if !is_option_activate_key(&evt) { return; }
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                dock_position.set(DockPosition::Bottom);
                                                                save_dock_position(DockPosition::Bottom);
                                                                dock_menu_open.set(false);
                                                                focus_element_by_id("dock-menu-trigger");
                                                            },
                                                            "Bottom"
                                                        }
                                                        div {
                                                            class: if dock_position() == DockPosition::Left { "glass-select-option selected" } else { "glass-select-option" },
                                                            role: "option",
                                                            tabindex: "0",
                                                            "aria-selected": if dock_position() == DockPosition::Left { "true" } else { "false" },
                                                            onclick: move |e: MouseEvent| {
                                                                e.stop_propagation();
                                                                dock_position.set(DockPosition::Left);
                                                                save_dock_position(DockPosition::Left);
                                                                dock_menu_open.set(false);
                                                            },
                                                            onkeydown: move |evt: Event<KeyboardData>| {
                                                                if !is_option_activate_key(&evt) { return; }
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                dock_position.set(DockPosition::Left);
                                                                save_dock_position(DockPosition::Left);
                                                                dock_menu_open.set(false);
                                                                focus_element_by_id("dock-menu-trigger");
                                                            },
                                                            "Left"
                                                        }
                                                        div {
                                                            class: if dock_position() == DockPosition::Right { "glass-select-option selected" } else { "glass-select-option" },
                                                            role: "option",
                                                            tabindex: "0",
                                                            "aria-selected": if dock_position() == DockPosition::Right { "true" } else { "false" },
                                                            onclick: move |e: MouseEvent| {
                                                                e.stop_propagation();
                                                                dock_position.set(DockPosition::Right);
                                                                save_dock_position(DockPosition::Right);
                                                                dock_menu_open.set(false);
                                                            },
                                                            onkeydown: move |evt: Event<KeyboardData>| {
                                                                if !is_option_activate_key(&evt) { return; }
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                dock_position.set(DockPosition::Right);
                                                                save_dock_position(DockPosition::Right);
                                                                dock_menu_open.set(false);
                                                                focus_element_by_id("dock-menu-trigger");
                                                            },
                                                            "Right"
                                                        }
                                                        div { class: "glass-select-separator" }
                                                        div {
                                                            class: "glass-select-option",
                                                            role: "option",
                                                            tabindex: "0",
                                                            onclick: move |e: MouseEvent| {
                                                                e.stop_propagation();
                                                                let new_val = !autohide_enabled();
                                                                autohide_enabled.set(new_val);
                                                                save_dock_autohide(new_val);
                                                                dock_menu_open.set(false);
                                                            },
                                                            onkeydown: move |evt: Event<KeyboardData>| {
                                                                if !is_option_activate_key(&evt) { return; }
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                let new_val = !autohide_enabled();
                                                                autohide_enabled.set(new_val);
                                                                save_dock_autohide(new_val);
                                                                dock_menu_open.set(false);
                                                                focus_element_by_id("dock-menu-trigger");
                                                            },
                                                            if autohide_enabled() {
                                                                "Turn Hiding Off"
                                                            } else {
                                                                "Turn Hiding On"
                                                            }
                                                        }
                                                        div { class: "glass-select-separator" }
                                                        div {
                                                            class: "glass-select-option",
                                                            role: "option",
                                                            tabindex: "0",
                                                            onclick: move |e: MouseEvent| {
                                                                e.stop_propagation();
                                                                customize_mode.set(true);
                                                                dock_menu_open.set(false);
                                                            },
                                                            onkeydown: move |evt: Event<KeyboardData>| {
                                                                if !is_option_activate_key(&evt) { return; }
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                customize_mode.set(true);
                                                                dock_menu_open.set(false);
                                                            },
                                                            "Customize"
                                                        }
                                                        div {
                                                            class: "glass-select-option",
                                                            role: "option",
                                                            tabindex: "0",
                                                            onclick: move |e: MouseEvent| {
                                                                e.stop_propagation();
                                                                action_bar_slots.set(DEFAULT_SLOTS.to_vec());
                                                                action_bar_hidden.set(Vec::new());
                                                                remove_action_bar_layout();
                                                                dock_menu_open.set(false);
                                                            },
                                                            onkeydown: move |evt: Event<KeyboardData>| {
                                                                if !is_option_activate_key(&evt) { return; }
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                action_bar_slots.set(DEFAULT_SLOTS.to_vec());
                                                                action_bar_hidden.set(Vec::new());
                                                                remove_action_bar_layout();
                                                                dock_menu_open.set(false);
                                                                focus_element_by_id("dock-menu-trigger");
                                                            },
                                                            "Reset to Default"
                                                        }
                                                        div { class: "glass-select-separator" }
                                                        div {
                                                            class: "glass-select-option",
                                                            role: "option",
                                                            tabindex: "0",
                                                            onclick: move |e: MouseEvent| {
                                                                e.stop_propagation();
                                                                let was_closed = !device_settings_open();
                                                                device_settings_open.set(true);
                                                                if was_closed {
                                                                    device_settings_generation
                                                                        .set(device_settings_generation() + 1);
                                                                    peer_list_open.set(false);
                                                                    diagnostics_open.set(false);
                                                                    density_open.set(false);
                                                                    mock_peers_open.set(false);
                                                                    meeting_options_open.set(false);
                                                                }
                                                                device_settings_initial_section
                                                                    .set(Some("preferences".to_string()));
                                                                dock_menu_open.set(false);
                                                            },
                                                            onkeydown: move |evt: Event<KeyboardData>| {
                                                                if !is_option_activate_key(&evt) { return; }
                                                                evt.stop_propagation();
                                                                evt.prevent_default();
                                                                let was_closed = !device_settings_open();
                                                                device_settings_open.set(true);
                                                                if was_closed {
                                                                    device_settings_generation
                                                                        .set(device_settings_generation() + 1);
                                                                    peer_list_open.set(false);
                                                                    diagnostics_open.set(false);
                                                                    density_open.set(false);
                                                                    mock_peers_open.set(false);
                                                                    meeting_options_open.set(false);
                                                                }
                                                                device_settings_initial_section
                                                                    .set(Some("preferences".to_string()));
                                                                dock_menu_open.set(false);
                                                                // Settings modal manages its own focus on
                                                                // open, so no explicit focus restore.
                                                            },
                                                            "Action Bar\u{2026}"
                                                        }
                                                    }
                                                }
                                            }
                                            }
                                        }
                                        if mock_peers_enabled() {
                                            div {
                                                class: "action-bar-mock-peers-wrapper",
                                                style: "order: 91",
                                                MockPeersButton {
                                                    open: mock_peers_open(),
                                                    onclick: move |e: MouseEvent| {
                                                        if customize_mode() { return; }
                                                        e.stop_propagation();
                                                        let opening = !mock_peers_open();
                                                        mock_peers_open.set(opening);
                                                        if opening {
                                                            density_open.set(false);
                                                            dock_menu_open.set(false);
                                                        }
                                                    },
                                                }
                                            }
                                        }
                                    {
                                        let hangup_client = client.clone();
                                        let hangup_id = id.clone();
                                        let hangup_is_guest = is_guest;
                                        let hangup_room_token = room_token.clone();
                                        rsx! {
                                            div {
                                                class: "hangup-wrapper",
                                                style: "order: 99",
                                                HangUpButton {
                                                    onclick: move |_| {
                                                        if customize_mode() { return; }
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
                                                            let _ = crate::meeting_api::leave_meeting_as_guest(
                                                                &meeting_id,
                                                                &room_token,
                                                            )
                                                            .await;
                                                        } else if let Err(e) =
                                                            crate::meeting_api::leave_meeting(
                                                                &meeting_id,
                                                            )
                                                            .await
                                                        {
                                                            log::error!(
                                                                "Error leaving meeting: {e}"
                                                            );
                                                        }
                                                        let _ = window().location().set_href("/");
                                                    });
                                                },
                                            }
                                            }
                                        }
                                    }
                                    // Floating drag preview (position: absolute child of nav).
                                    // Gated on `customize_mode()` so any leftover `dragging_slot`
                                    // state cannot render the preview over normal UI after Done.
                                    if let Some(dragged_slot) =
                                        dragging_slot().filter(|_| customize_mode())
                                    {
                                        {
                                            let px = drag_pointer_x();
                                            let py = drag_pointer_y();
                                            if drag_started() {
                                                let nav_left = drag_nav_left.get();
                                                let nav_top = drag_nav_top.get();
                                                let grab_dx = drag_grab_dx.get();
                                                let grab_dy = drag_grab_dy.get();
                                                let size = drag_slot_size.get();
                                                let local_x = px - nav_left - grab_dx;
                                                let local_y = py - nav_top - grab_dy;
                                                let preview_style = format!(
                                                    "left: 0; top: 0; width: {size}px; height: {size}px; transform: translate({local_x}px, {local_y}px);"
                                                );
                                                rsx! {
                                                    div {
                                                        class: "action-bar-drag-preview",
                                                        style: "{preview_style}",
                                                        match dragged_slot {
                                                            ActionBarSlot::Mic => rsx! { MicButton { enabled: mic_enabled(), available: mic_error.read().is_none(), onclick: |_| {} } },
                                                            ActionBarSlot::Camera => rsx! { CameraButton { enabled: video_enabled(), available: video_error.read().is_none(), onclick: |_| {} } },
                                                            ActionBarSlot::ScreenShare => rsx! { ScreenShareButton { active: matches!(screen_share_state(), ScreenShareState::Active), disabled: true, onclick: |_| {} } },
                                                            ActionBarSlot::PeerList => rsx! { PeerListButton { open: peer_list_open(), onclick: |_| {} } },
                                                            ActionBarSlot::DensityMode => rsx! { DensityModeButton { label: density_mode().label().to_string(), open: density_open(), onclick: |_: MouseEvent| {} } },
                                                            ActionBarSlot::Diagnostics => rsx! { DiagnosticsButton { open: diagnostics_open(), onclick: |_| {} } },
                                                            ActionBarSlot::DeviceSettings => rsx! { DeviceSettingsButton { open: device_settings_open(), onclick: |_| {} } },
                                                            ActionBarSlot::MeetingOptions => rsx! { MeetingOptionsButton { open: meeting_options_open(), onclick: |_| {} } },
                                                        }
                                                    }
                                                }
                                            } else {
                                                rsx! {}
                                            }
                                        }
                                                                        // Keep ScreenShare focusable in customize mode.
                                                                        // Including `customize_mode` in `disabled` would
                                                                        // remove it from tab order and reintroduce a11y
                                                                        // tab-skip regressions.
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
                                                p { style: "margin-top:var(--space-2);", "{displayed}" }
                                                div { style: "display:flex; gap:var(--space-2); justify-content:flex-end; margin-top:var(--space-3);",
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
                            // In-meeting device-access problem modal. Reuses the
                            // SAME modal as the pre-join path, but dismissing just
                            // closes it — the user is already in the call, so there
                            // is no connect/join to perform. The background
                            // auto-retry loop keeps trying a `DeviceInUse` device.
                            if show_device_warning() {
                                {
                                    let on_dismiss = EventHandler::new(move |()| {
                                        show_device_warning.set(false);
                                    });
                                    render_device_warning_modal(
                                        mic_error.read().as_ref(),
                                        video_error.read().as_ref(),
                                        on_dismiss,
                                    )
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
                                    device_settings_initial_section: device_settings_initial_section(),
                                    device_settings_generation: device_settings_generation(),
                                    on_device_settings_toggle: move |_| {
                                        device_settings_initial_section.set(None);
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
                                    on_microphone_permission_error: move |err: MediaPermissionsErrorState| {
                                        log::warn!("Microphone permission error in-meeting: {err:?}");
                                        // Set-if-changed, mirroring `on_result`'s
                                        // `permission_probe_error_target` path: the live
                                        // encoder's restart loop fires this callback up to
                                        // MAX_RESTARTS times over ~5s, all with the SAME
                                        // classified error. `Signal::set` marks dirty
                                        // unconditionally, so an unconditional write here
                                        // would re-run the retry `use_effect` (which
                                        // subscribes to `mic_error`) several times in a
                                        // burst. Write only on a genuine change so those
                                        // repeat fires are no-ops.
                                        let target = map_permission_error(&err);
                                        if mic_error.peek().as_ref() != Some(&target) {
                                            mic_error.set(Some(target));
                                        }
                                        mic_enabled.set(false);
                                        show_device_warning.set(true);
                                    },
                                    on_camera_permission_error: move |err: MediaPermissionsErrorState| {
                                        log::warn!("Camera permission error in-meeting: {err:?}");
                                        // Set-if-changed — see the microphone handler above.
                                        let target = map_permission_error(&err);
                                        if video_error.peek().as_ref() != Some(&target) {
                                            video_error.set(Some(target));
                                        }
                                        video_enabled.set(false);
                                        show_device_warning.set(true);
                                    },
                                    on_screen_share_state: move |event: ScreenShareEvent| {
                                        log::info!("Screen share state changed: {event:?}");
                                        let mut screen_share_toast_state = screen_share_toast_state;
                                        let mut screen_share_toast_timer = screen_share_toast_timer;
                                        match event {
                                            ScreenShareEvent::Started(_stream) => {
                                                screen_share_state.set(ScreenShareState::Active);
                                                screen_share_toast_state
                                                    .set(Some(ScreenShareToastState::Starting));
                                                screen_share_toast_timer.set(Some(Timeout::new(
                                                    10_000,
                                                    move || {
                                                        let mut s = screen_share_toast_state;
                                                        if matches!(
                                                            s.peek().as_ref(),
                                                            Some(ScreenShareToastState::Starting)
                                                        ) {
                                                            s.set(Some(ScreenShareToastState::Failed(
                                                                "No peers received the shared content within 10 seconds."
                                                                    .to_string(),
                                                            )));
                                                            let mut t = screen_share_toast_timer;
                                                            t.set(Some(Timeout::new(
                                                                6_000,
                                                                move || {
                                                                    let mut s2 =
                                                                        screen_share_toast_state;
                                                                    s2.set(None);
                                                                },
                                                            )));
                                                        }
                                                    },
                                                )));
                                            }
                                            ScreenShareEvent::Cancelled | ScreenShareEvent::Stopped => {
                                                screen_share_state.set(ScreenShareState::Idle);
                                                screen_share_toast_state.set(None);
                                                screen_share_toast_timer.set(None);
                                            }
                                            ScreenShareEvent::Failed(ref msg) => {
                                                log::error!("Screen share failed: {msg}");
                                                screen_share_state.set(ScreenShareState::Idle);
                                                screen_share_toast_state.set(None);
                                                screen_share_toast_timer.set(None);
                                                user_error.set(Some(format!("Screen share failed: {msg}")));
                                            }
                                        }
                                    },
                                    reload_devices_counter: reload_devices_counter(),
                                    publish_diagnostics_reader: diagnostics_reader_sink,
                                    // Host publishes its Performance controls handle
                                    // here so the Diagnostics drawer can mount the
                                    // panel (sliders/Auto/meters). (#1131 unify)
                                    publish_perf_controls: perf_controls_sink,
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
                    // Overlay drawer: floats over the tiles at its (resizable) width.
                    style: format!("width: {}px", left_width()),
                    // Clicks INSIDE the peer list must not bubble to
                    // `#main-container` — otherwise the background light-dismiss
                    // (issue #1790) would treat an in-panel click as an outside
                    // click and close the panel. (Same guard as the diagnostics
                    // drawer root and the density/mock-peers popovers.)
                    onclick: move |e: MouseEvent| e.stop_propagation(),
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
                                    device_settings_initial_section.set(None);
                                }
                            },
                            host_display_name: host_display_name.clone(),
                            host_user_id: host_user_id.clone(),
                            local_user_display_name: current_display_name(),
                            on_edit_self_name: {
                                move |_| {
                                    display_name_modal_open.set(true);
                                }
                            },
                        }
                        div {
                            class: "drawer-resize-handle",
                            role: "separator",
                            aria_orientation: "vertical",
                            aria_label: "Resize panel",
                            tabindex: "0",
                            // keyboard resize is a follow-up
                            // Pointer capture: on pointerdown the handle captures the
                            // pointer so every subsequent pointermove/up is delivered HERE
                            // even when the pointer is over the drawer body or a tile — that
                            // is what makes shrink (drag left, over the drawer) work at all.
                            onpointerdown: {
                                let lv = left_raf_valid.clone();
                                let lp = left_raf_pending.clone();
                                move |evt: PointerEvent| {
                                    evt.prevent_default();
                                    resizing_drawer.set(ResizingDrawer::Left);
                                    // Start a fresh drag with no valid stash yet: the flush in
                                    // pointerup is skipped until a real pointermove sets this.
                                    lv.set(false);
                                    // Defensive: clear any stale pending flag so a fresh drag can always schedule its first rAF (can't start wedged).
                                    lp.set(false);
                                    // No start-vw cache: the left edge sits at viewport x=0, so
                                    // the move handler reads client_x directly. drag_start_vw is
                                    // only needed by the right drawer.
                                    let native = evt.as_web_event();
                                    if let Some(t) = native.target() {
                                        use wasm_bindgen::JsCast;
                                        if let Ok(el) = t.dyn_into::<web_sys::Element>() {
                                            let _ = el.set_pointer_capture(native.pointer_id());
                                        }
                                    }
                                }
                            },
                            // rAF-coalesced move: stash the latest client_x and schedule
                            // at most ONE animation-frame callback per painted frame. The
                            // callback does the single width.set() — so a fast drag that
                            // delivers many coalesced pointermoves still causes only one
                            // re-render per frame, keeping the wasm main thread available
                            // for live video decode. localStorage is persisted ONLY on
                            // pointerup (below) to avoid write churn. (#1296 perf)
                            //
                            // Each `move` closure takes ownership of its captured Rcs, so
                            // we pre-clone a dedicated handle for every handler below.
                            onpointermove: {
                                let lx = left_raf_x.clone();
                                let lp = left_raf_pending.clone();
                                let lv = left_raf_valid.clone();
                                move |evt: PointerEvent| {
                                    if resizing_drawer() == ResizingDrawer::Left {
                                        let client_x = evt.as_web_event().client_x() as f64;
                                        lx.set(client_x);
                                        // A real move occurred: the stash now holds a genuine
                                        // pointer position, so the pointerup flush may apply it.
                                        lv.set(true);
                                        if !lp.get() {
                                            lp.set(true);
                                            let x_cell = lx.clone();
                                            let pending_cell = lp.clone();
                                            let cb = Closure::once_into_js(move |_ts: f64| {
                                                // Guard: if drag ended before this frame fires,
                                                // the resizing-state is already None — skip.
                                                if resizing_drawer() == ResizingDrawer::Left {
                                                    let x = x_cell.get();
                                                    left_width.set(
                                                        x.clamp(DRAWER_MIN_WIDTH, max_for_side),
                                                    );
                                                }
                                                pending_cell.set(false);
                                            });
                                            let _ = window().request_animation_frame(
                                                cb.as_ref().unchecked_ref(),
                                            );
                                        }
                                    }
                                }
                            },
                            onpointerup: {
                                let lx = left_raf_x.clone();
                                let lp = left_raf_pending.clone();
                                let lv = left_raf_valid.clone();
                                move |evt: PointerEvent| {
                                    if resizing_drawer() == ResizingDrawer::Left {
                                        evt.prevent_default();
                                        // Always clear pending so any in-flight rAF callback is
                                        // a no-op (the resizing-state guard also protects it).
                                        lp.set(false);
                                        // Flush ONLY if a real move happened this drag: apply the
                                        // last MOVED position so the drawer settles exact (not one
                                        // frame stale) and persist it. A no-move interaction
                                        // (click / focus tap on the handle) leaves left_width and
                                        // the persisted value untouched — the stash still holds
                                        // its default and must not overwrite the current width.
                                        if lv.get() {
                                            left_width.set(
                                                lx.get().clamp(DRAWER_MIN_WIDTH, max_for_side),
                                            );
                                            // Persist on drag-end only; value is already clamped.
                                            save_f64("vc_drawer_left_width", left_width());
                                        }
                                        resizing_drawer.set(ResizingDrawer::None);
                                    }
                                }
                            },
                            // Pointer stream cancelled (OS gesture, touch interruption,
                            // lost capture): always clear the pending flag (so any in-flight
                            // rAF callback sees ResizingDrawer::None and skips the write —
                            // belt-and-suspenders over the resizing-state guard) and reset
                            // drag state so it can't latch. The flush+persist applies ONLY
                            // the last MOVED position and is skipped entirely when no move
                            // occurred this drag, so a no-move cancel cannot overwrite the
                            // current width with the default stash. When a real move did
                            // happen we persist the (already clamped) cancelled width,
                            // keeping left/right cancel semantics identical.
                            onpointercancel: {
                                let lx = left_raf_x.clone();
                                let lp = left_raf_pending.clone();
                                let lv = left_raf_valid.clone();
                                move |_: PointerEvent| {
                                    if resizing_drawer() == ResizingDrawer::Left {
                                        lp.set(false);
                                        if lv.get() {
                                            left_width.set(
                                                lx.get().clamp(DRAWER_MIN_WIDTH, max_for_side),
                                            );
                                            // Persist on cancel; value is already clamped.
                                            save_f64("vc_drawer_left_width", left_width());
                                        }
                                        resizing_drawer.set(ResizingDrawer::None);
                                    }
                                }
                            },
                            // #1296: lost-capture end-of-drag. onpointerup only fires when the
                            // pointer is released over the captured element; if capture is lost
                            // some other way (release off-element after capture was dropped, OS
                            // interruption, element re-render) the browser fires
                            // `lostpointercapture` on the SAME element that called
                            // set_pointer_capture — here the handle div (capture is taken on
                            // evt.target() in onpointerdown, which IS this div). Without this
                            // handler resizing_drawer would stay Left and a later plain hover over
                            // the handle (its move guard only checks `== Left`) would keep
                            // resizing — the latch the user hit. We MIRROR onpointercancel exactly
                            // so flush/persist/reset semantics are identical: clear pending, flush
                            // + persist only if a real move happened, then reset to None.
                            onlostpointercapture: {
                                let lx = left_raf_x.clone();
                                let lp = left_raf_pending.clone();
                                let lv = left_raf_valid.clone();
                                move |_: PointerEvent| {
                                    if resizing_drawer() == ResizingDrawer::Left {
                                        lp.set(false);
                                        if lv.get() {
                                            left_width.set(
                                                lx.get().clamp(DRAWER_MIN_WIDTH, max_for_side),
                                            );
                                            // Persist on lost-capture; value is already clamped.
                                            save_f64("vc_drawer_left_width", left_width());
                                        }
                                        resizing_drawer.set(ResizingDrawer::None);
                                    }
                                }
                            },
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

                // In-call Meeting Options panel (host-only).
                if meeting_options_open() && is_owner {
                    div {
                        class: "glass-backdrop",
                        onclick: move |_| meeting_options_open.set(false),
                        onkeydown: move |e: Event<KeyboardData>| {
                            if e.key().to_string() == "Escape" {
                                meeting_options_open.set(false);
                            }
                        },
                        div {
                            class: "card-apple",
                            style: "width: 380px; max-width: 92vw;",
                            onclick: move |e| e.stop_propagation(),

                            div {
                                style: "display:flex; align-items:center; justify-content:space-between; margin-bottom:var(--space-2);",
                                h3 { style: "margin:0;", "Meeting Options" }
                                button {
                                    r#type: "button",
                                    class: "btn-apple btn-secondary btn-sm",
                                    "aria-label": "Close meeting options",
                                    onclick: move |_| meeting_options_open.set(false),
                                    "Done"
                                }
                            }
                            p {
                                style: "color: var(--text-secondary); margin-top:0; margin-bottom:var(--space-3); font-size:0.85rem;",
                                "Changes apply to everyone immediately."
                            }

                            MeetingOptionsControls {
                                meeting_id: id.clone(),
                                waiting_room_toggle,
                                admitted_can_admit_toggle,
                                end_on_host_leave_toggle,
                                allow_guests_toggle,
                                saving,
                                toggle_error,
                            }
                        }
                    }
                }

                if display_name_modal_open() {
                    UpdateDisplayNameModal {
                        current_display_name: current_display_name(),
                        meeting_id: id.clone(),
                        // HCL issue 828 follow-up: parse the local session_id
                        // (a numeric string from the client) into u64 so the
                        // rename REST request can identify this tab. Falls back
                        // to None when the session has not yet been assigned or
                        // the value is unparseable — the server then renames
                        // every session of the caller's user_id (legacy
                        // behaviour).
                        session_id: my_session_id
                            .as_deref()
                            .and_then(|s| s.parse::<u64>().ok()),
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

                // Diagnostics sidebar.
                //
                // The `#diagnostics-sidebar` ELEMENT must persist in the DOM whether
                // open or closed — symmetric with `#peer-list-container` above, whose
                // outer div always renders and only toggles the `visible` class. The
                // heavy `Diagnostics` component (live NetEq subscriptions, charts,
                // 4 Hz simulcast interval) is still only MOUNTED while open, so there
                // is no closed-state work; when closed we render a lightweight empty
                // placeholder div with the same id but WITHOUT `visible`. Previously
                // the whole element lived inside `if diagnostics_open()`, so closing
                // it UNMOUNTED `#diagnostics-sidebar` entirely — which broke the
                // both-open close flow (a `:not(.visible)` assertion can't match an
                // element that no longer exists) and was asymmetric with the left
                // drawer. (issue 1296 both-open close)
                if diagnostics_open() {
                    Diagnostics {
                        is_open: true,
                        on_close: move |_| diagnostics_open.set(false),
                        video_enabled: video_enabled(),
                        mic_enabled: mic_enabled(),
                        share_screen: screen_share_state().is_sharing(),
                        encoder_settings: encoder_settings(),
                        // Live SEND simulcast reader (published by Host on mount),
                        // for the "Simulcast layers" section. (#1095 §6)
                        diagnostics_reader: diagnostics_reader_sink(),
                        // Performance controls handle (published by Host on mount),
                        // for the migrated Performance panel in the drawer's
                        // "Quality controls" group. (#1131 unify)
                        perf_controls: perf_controls_sink(),
                        // Overlay drawer: floats over the tiles at its (resizable) width.
                        width: right_width(),
                        // The right handle lives in diagnostics.rs (no access to the width
                        // signals), so it forwards pointer events here where the math runs.
                        on_resize_start: {
                            let rv = right_raf_valid.clone();
                            let rp = right_raf_pending.clone();
                            move |_| {
                                resizing_drawer.set(ResizingDrawer::Right);
                                // Cache start-of-drag viewport width (not re-read per move).
                                drag_start_vw.set(vw);
                                // Start a fresh drag with no valid stash yet: the flush in
                                // on_resize_end is skipped until a real on_resize_move sets it.
                                rv.set(false);
                                // Defensive: clear any stale pending flag so a fresh drag can always schedule its first rAF (can't start wedged).
                                rp.set(false);
                            }
                        },
                        // rAF-coalesced move: stash the raw client_x and schedule at most
                        // ONE animation-frame callback per painted frame. The callback
                        // computes the clamped width from the latest stashed x and the
                        // drag_start_vw captured at drag start — so a fast drag still
                        // causes only one re-render per frame. inner_width is NOT re-read
                        // per move (cached in drag_start_vw at on_resize_start). (#1296 perf)
                        //
                        // Each `move` closure takes ownership of its captured Rcs, so we
                        // pre-clone a dedicated handle for each handler below.
                        on_resize_move: {
                            let rx = right_raf_x.clone();
                            let rp = right_raf_pending.clone();
                            let rv = right_raf_valid.clone();
                            move |client_x: f64| {
                                if resizing_drawer() == ResizingDrawer::Right {
                                    rx.set(client_x);
                                    // A real move occurred: the stash now holds a genuine
                                    // pointer position, so the on_resize_end flush may apply it.
                                    rv.set(true);
                                    if !rp.get() {
                                        rp.set(true);
                                        let x_cell = rx.clone();
                                        let pending_cell = rp.clone();
                                        let start_vw = drag_start_vw;
                                        let cb = Closure::once_into_js(move |_ts: f64| {
                                            // Guard: if drag ended before this frame fires,
                                            // the resizing-state is already None — skip.
                                            if resizing_drawer() == ResizingDrawer::Right {
                                                let x = x_cell.get();
                                                right_width.set(
                                                    (start_vw() - x)
                                                        .clamp(DRAWER_MIN_WIDTH, max_for_side),
                                                );
                                            }
                                            pending_cell.set(false);
                                        });
                                        let _ = window().request_animation_frame(
                                            cb.as_ref().unchecked_ref(),
                                        );
                                    }
                                }
                            }
                        },
                        on_resize_end: {
                            let rx = right_raf_x.clone();
                            let rp = right_raf_pending.clone();
                            let rv = right_raf_valid.clone();
                            move |_| {
                                if resizing_drawer() == ResizingDrawer::Right {
                                    // Always clear pending so any in-flight rAF callback is a
                                    // no-op (the resizing-state guard also protects it).
                                    rp.set(false);
                                    // Flush ONLY if a real move happened this drag: apply the
                                    // last MOVED position so the drawer settles exact (not one
                                    // frame stale) and persist it. A no-move interaction
                                    // (click / focus tap on the handle) leaves right_width and
                                    // the persisted value untouched — the stash still holds its
                                    // default and must not overwrite the current width.
                                    // Note: on_resize_end is also called from diagnostics.rs
                                    // onpointercancel AND onlostpointercapture, so end + cancel
                                    // + lost-capture all share this flush path. (#1296)
                                    if rv.get() {
                                        right_width.set(
                                            (drag_start_vw() - rx.get())
                                                .clamp(DRAWER_MIN_WIDTH, max_for_side),
                                        );
                                        // Persist on drag-end only; value is already clamped.
                                        save_f64("vc_drawer_right_width", right_width());
                                    }
                                    resizing_drawer.set(ResizingDrawer::None);
                                }
                            }
                        },
                    }
                } else if diagnostics_was_opened() {
                    // Closed AFTER having been opened at least once: keep a
                    // lightweight `#diagnostics-sidebar` placeholder in the DOM (no
                    // `visible` class, no children) so the both-open close flow can
                    // observe it lose `visible` rather than vanish. Before the drawer
                    // is EVER opened this branch does not render, so the never-opened
                    // contract (`#diagnostics-sidebar` absent until first open) holds.
                    // The width inline style mirrors the open drawer so a future
                    // reopen has no layout pop; no children → no diagnostics work runs
                    // while closed. (issue 1296 both-open close)
                    div {
                        id: "diagnostics-sidebar",
                        class: "",
                        style: format!("width: {}px", right_width()),
                        // No `role`/`aria-label` on the EMPTY closed placeholder: a
                        // labelled landmark with no content would announce an empty
                        // region to a screen reader. The open `Diagnostics` root
                        // carries the `role="region"` + label; the placeholder is a
                        // pure DOM-presence shim so the both-open close flow can see
                        // the element lose `visible` rather than vanish. (issue 1296)
                    }
                }

                // Mock peers popover (only shown when env-gated)
                if mock_peers_enabled() && mock_peers_open() {
                    div { class: "mock-peers-popover",
                        onclick: move |e: MouseEvent| e.stop_propagation(),
                        onkeydown: move |evt: Event<KeyboardData>| {
                            if evt.key() == Key::Escape {
                                evt.stop_propagation();
                                evt.prevent_default();
                                mock_peers_open.set(false);
                                focus_element_by_id("mock-peers-trigger");
                            }
                        },
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
                if !has_screen_share && density_open() {
                    div { class: "density-popover",
                        onclick: move |e: MouseEvent| e.stop_propagation(),
                        for mode in DENSITY_MODES {
                            div {
                                key: "{mode.label()}",
                                class: if density_mode() == mode { "density-option active" } else { "density-option" },
                                onclick: move |_| {
                                    density_mode.set(mode);
                                    save_density_mode(mode);
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

/// Default 50 ms coalescing window for [`schedule_throttled_bump`].
///
/// Selected as a render-friendly upper bound: at 60 fps a frame is ~16 ms,
/// so 50 ms guarantees at most one re-render per ~3 frames even under a
/// sustained burst of speech events. See Phase 6 render-storm fix
/// (cc7tp 2026-05-06).
pub(crate) const PEER_LIST_VERSION_THROTTLE_MS: u32 = 50;

/// Coalesce a burst of "something tiny changed" events into at most one
/// invocation of `bump` per `delay_ms` window.
///
/// `pending` is an `Rc<Cell<bool>>` that survives across calls (typically
/// stored via `use_hook`). When clear, this function sets it and schedules
/// a [`Timeout`] of `delay_ms` that will:
///   1. Invoke `bump` (the actual work — e.g. a `peer_list_version.set()`).
///   2. Clear `pending` so the next call schedules a new window.
///
/// When `pending` is already set, this call is a no-op — the bump is
/// already inbound. This is the kernel of the Phase 6 render-storm fix:
/// `peer_speaking` events fire 3-5×/sec/speaker on a busy call, and
/// without coalescing each one drove a full meeting-view re-render. With
/// the throttle, bursty speech activity collapses into one re-render
/// every 50 ms regardless of how many speakers are active.
///
/// Note: only "soft" bumps (the ones driven by speech activity, where
/// the peer set is unchanged and the version bump exists purely to nudge
/// memo-keyed children) should go through this throttle. Real peer
/// add/remove events must bump immediately.
pub(crate) fn schedule_throttled_bump(pending: Rc<Cell<bool>>, delay_ms: u32, bump: Rc<dyn Fn()>) {
    if pending.get() {
        return;
    }
    pending.set(true);
    let pending_clone = pending.clone();
    let bump_clone = bump.clone();
    Timeout::new(delay_ms, move || {
        // Clear the flag BEFORE running `bump` so any new event fired
        // synchronously from inside `bump` (or from a subscriber that
        // reacts to the version change) can re-arm the throttle for the
        // next 50 ms window. Otherwise the next event would silently
        // drop and we'd miss a coalescing boundary.
        pending_clone.set(false);
        bump_clone();
    })
    .forget();
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
            ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.to_string()),
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
            ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.to_string()),
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
    use videocall_meeting_types::responses::ParticipantStatusResponse;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    // ── #1790 Escape panel-close precedence ──
    // Plain host `#[test]`s (not browser tests): `esc_panel_close_target` and
    // `trigger_id` are pure (bool/enum in, enum/&str out, no DOM), so they run
    // under native libtest — the gate CI actually executes for this crate
    // (`cargo test -p videocall-ui --lib`; the crate's `#[wasm_bindgen_test]`
    // fns are host-stubbed there and never launch a browser). Same rationale as
    // the `deep_link_*` / `auto_retry_*` classifier tests above.

    /// Both panels open → Escape closes DIAGNOSTICS first, because it is the
    /// topmost drawer (mobile z-index 9301 vs the peer list's 9300). A precedence
    /// flip (peer list first) fails HERE — this is the test that guards the order.
    #[test]
    fn esc_both_open_closes_diagnostics_first() {
        assert_eq!(
            esc_panel_close_target(true, true),
            Some(EscCloseTarget::Diagnostics)
        );
    }

    /// Only the peer list is open → Escape closes the peer list.
    #[test]
    fn esc_only_peer_list_closes_peer_list() {
        assert_eq!(
            esc_panel_close_target(false, true),
            Some(EscCloseTarget::PeerList)
        );
    }

    /// Only diagnostics is open → Escape closes diagnostics.
    #[test]
    fn esc_only_diagnostics_closes_diagnostics() {
        assert_eq!(
            esc_panel_close_target(true, false),
            Some(EscCloseTarget::Diagnostics)
        );
    }

    /// Neither panel open → `None`, so Escape is a no-op in the panel handler and
    /// the popover handlers (density / mock-peers / dock menu) keep today's
    /// behavior.
    #[test]
    fn esc_none_open_returns_none() {
        assert_eq!(esc_panel_close_target(false, false), None);
    }

    /// Lockstep pin on the trigger-button ids this handler restores focus to.
    /// These strings are the fn's OUTPUT contract; the rendered-id side of the
    /// contract (that a DOM element actually carries each id) is guarded by the
    /// e2e `activeElement` assertions in `popup-layering.spec.ts`, not here.
    #[test]
    fn esc_close_target_trigger_ids() {
        assert_eq!(
            EscCloseTarget::Diagnostics.trigger_id(),
            "diagnostics-trigger"
        );
        assert_eq!(EscCloseTarget::PeerList.trigger_id(), "peer-list-trigger");
    }

    // ── Settings deep-link routing (#1131 unify) ──
    // Plain host `#[test]`s (not browser tests): the classifier is pure.

    /// "performance" must route to the Diagnostics DRAWER (its new home), NOT the
    /// Settings modal. If this reverts to `Modal`, the migration regresses: the
    /// old Performance tab no longer exists, so the modal would land on a missing
    /// section. This is the load-bearing assertion of the deep-link repurpose.
    #[test]
    fn deep_link_performance_opens_drawer() {
        assert_eq!(
            classify_settings_deep_link(Some("performance")),
            SettingsDeepLink::Drawer
        );
    }

    /// Every other section — and `None` — opens the Settings modal as before.
    #[test]
    fn deep_link_other_sections_open_modal() {
        for s in ["audio", "video", "network", "appearance"] {
            assert_eq!(
                classify_settings_deep_link(Some(s)),
                SettingsDeepLink::Modal,
                "section {s:?} should open the modal"
            );
        }
        assert_eq!(classify_settings_deep_link(None), SettingsDeepLink::Modal);
        // An unknown section also falls through to the modal (default tab).
        assert_eq!(
            classify_settings_deep_link(Some("bogus")),
            SettingsDeepLink::Modal
        );
    }

    // ── background auto-retry gating (device-in-use) ──

    /// Only a device blocked by `DeviceInUse` should be auto-retried, and this
    /// is unconditional — it does NOT depend on any user "wants it on" intent,
    /// because a background probe can only ever CLEAR the error, never auto-start
    /// capture. Flipping the error variant must change the result.
    #[test]
    fn auto_retry_only_for_device_in_use() {
        // DeviceInUse → retry, regardless of whether the user ever toggled the
        // device on. This is the exact case that was stuck "blocked forever"
        // when retry was gated on intent.
        assert!(should_auto_retry(Some(&MediaErrorState::DeviceInUse)));
        // A DIFFERENT error (permission denied) → do NOT retry: browsers don't
        // silently re-grant a site-level deny.
        assert!(!should_auto_retry(Some(&MediaErrorState::PermissionDenied)));
        // No error at all → nothing to retry.
        assert!(!should_auto_retry(None));
    }

    /// Belt-and-suspenders: the other non-retryable error variants (NoDevice,
    /// Other) must also NOT auto-retry.
    #[test]
    fn auto_retry_excludes_other_error_variants() {
        assert!(!should_auto_retry(Some(&MediaErrorState::NoDevice)));
        assert!(!should_auto_retry(Some(&MediaErrorState::Other)));
    }

    // ── background auto-retry backoff schedule (long-run) ──

    /// Drive `retry_tick_decision` — the exact production backoff logic the retry
    /// `Interval` closure calls each tick — for many ticks, starting from a fresh
    /// episode `(since=0, gap=1)` with the real `RETRY_MAX_GAP_TICKS` cap (15).
    /// Returns the 1-based tick indices on which a probe fired.
    fn simulate_retry_probe_ticks(total_ticks: u32, max_gap: u32) -> Vec<u32> {
        let mut since = 0u32;
        let mut gap = 1u32;
        let mut probe_ticks = Vec::new();
        for tick in 1..=total_ticks {
            let d = retry_tick_decision(since, gap, max_gap);
            since = d.since;
            gap = d.gap;
            if d.probe {
                probe_ticks.push(tick);
            }
        }
        probe_ticks
    }

    /// The documented schedule: probes at ticks 1, 3, 7, 15, then every 15 ticks.
    /// At a 4s base cadence these are 4s, 12s, 28s, 60s, 120s, 180s, … i.e. gaps
    /// of 4s, 8s, 16s, 32s, 60s, 60s, … — the "4s→8s→16s→32s→60s(held)" ladder.
    /// A regression that broke the doubling or the cap (e.g. `>=` vs `<`, or a bad
    /// `min`) would shift these tick indices and fail.
    #[test]
    fn retry_backoff_schedule_matches_documented_ladder() {
        // 45 ticks = 3 minutes at a 4s base — well past the plateau boundary.
        let probes = simulate_retry_probe_ticks(45, 15);
        assert_eq!(
            probes,
            vec![1, 3, 7, 15, 30, 45],
            "probe ticks must follow 1,3,7,15 then every 15"
        );
    }

    /// LONG-RUN, wedge-freedom property — the specific class of bug the manual
    /// "5 minutes and the badge never cleared" report points at: an off-by-one in
    /// the backoff math that, deep into the plateau, leaves `since`/`gap` in a
    /// state that never issues another probe. Simulate 150 ticks (10 minutes at a
    /// 4s base) and assert the loop keeps probing forever at a STABLE 15-tick
    /// cadence once the gap caps — never stalls, never drifts.
    #[test]
    fn retry_backoff_never_wedges_past_the_plateau() {
        let max_gap = 15u32;
        let probes = simulate_retry_probe_ticks(150, max_gap);

        // It must probe many times over 10 simulated minutes (not stop early).
        assert!(
            probes.len() >= 10,
            "expected sustained probing over 150 ticks, got {} probes: {probes:?}",
            probes.len()
        );

        // Once the gap has capped (from tick 15 onward the gap is `max_gap`), every
        // subsequent probe is EXACTLY `max_gap` ticks after the previous one — a
        // steady, unbounded stream. This is the anti-wedge invariant.
        let plateau: Vec<u32> = probes.iter().copied().filter(|&t| t >= 15).collect();
        for pair in plateau.windows(2) {
            assert_eq!(
                pair[1] - pair[0],
                max_gap,
                "plateau probes must be exactly {max_gap} ticks apart; got {plateau:?}"
            );
        }

        // The gap must also never exceed the cap at any point in the run.
        let mut since = 0u32;
        let mut gap = 1u32;
        for _ in 0..150 {
            let d = retry_tick_decision(since, gap, max_gap);
            assert!(d.gap <= max_gap, "gap {} exceeded cap {max_gap}", d.gap);
            since = d.since;
            gap = d.gap;
        }
    }

    // ── device-warning modal auto-close on background recovery ──

    /// The modal auto-closes ONLY when it is currently shown AND both sides are
    /// error-free. Flipping any of the three terms must change the result — this
    /// pins the exact condition used in `on_result` so a stale modal cannot be
    /// left over an empty dialog after a background retry succeeds, and so a
    /// still-failing side keeps the modal up.
    #[test]
    fn auto_close_device_warning_only_when_shown_and_both_clear() {
        // Shown + both sides recovered → close (the bug this guards against).
        assert!(should_auto_close_device_warning(true, true, true));
        // Shown but mic still failing → keep open to display the mic error.
        assert!(!should_auto_close_device_warning(false, true, true));
        // Shown but video still failing → keep open to display the video error.
        assert!(!should_auto_close_device_warning(true, false, true));
        // Shown but BOTH still failing → keep open.
        assert!(!should_auto_close_device_warning(false, false, true));
        // Not shown → nothing to close (avoid a redundant `.set(false)`), even
        // when both sides are clear.
        assert!(!should_auto_close_device_warning(true, true, false));
    }

    // ── probe → target error-state mapping (set-if-changed dedupe) ──

    /// The probe→target mapping used by the set-if-changed writes in `on_result`.
    /// A granted/unknown probe targets `None` (no error); each `Denied` variant
    /// targets its matching `MediaErrorState`. This is the value compared against
    /// the current signal so a repeated identical failure writes nothing — pin it
    /// so a regression that mis-maps a variant (and would therefore either miss a
    /// real change or churn a no-op write) is caught. `Other` carries a `JsValue`
    /// (wasm-only), so it is exercised in-browser, not here.
    #[test]
    fn probe_error_target_maps_each_state() {
        assert!(permission_probe_error_target(&PermissionState::Granted).is_none());
        assert!(permission_probe_error_target(&PermissionState::Unknown).is_none());
        assert_eq!(
            permission_probe_error_target(&PermissionState::Denied(
                MediaPermissionsErrorState::NoDevice
            )),
            Some(MediaErrorState::NoDevice)
        );
        assert_eq!(
            permission_probe_error_target(&PermissionState::Denied(
                MediaPermissionsErrorState::PermissionDenied
            )),
            Some(MediaErrorState::PermissionDenied)
        );
        assert_eq!(
            permission_probe_error_target(&PermissionState::Denied(
                MediaPermissionsErrorState::DeviceInUse
            )),
            Some(MediaErrorState::DeviceInUse)
        );
    }

    // ── reconnect host-state reconcile gate ──

    /// First connect must not reconcile (mount effect already seeded); every
    /// reconnect must. Reverting the `!was_first` guard fails this.
    #[test]
    fn host_reconcile_skips_initial_connect_then_fires_on_reconnect() {
        let first_connect = Cell::new(true);
        assert!(
            !should_reconcile_host_on_connect(&first_connect, false),
            "initial connect must not trigger a reconcile"
        );
        assert!(
            should_reconcile_host_on_connect(&first_connect, false),
            "first reconnect must reconcile"
        );
        assert!(
            should_reconcile_host_on_connect(&first_connect, false),
            "later reconnects must keep reconciling"
        );
    }

    /// Guests hold no host controls, so they never reconcile. Dropping the
    /// `!is_guest` guard fails the reconnect assertion.
    #[test]
    fn host_reconcile_never_fires_for_guests() {
        let first_connect = Cell::new(true);
        assert!(
            !should_reconcile_host_on_connect(&first_connect, true),
            "guest initial connect must not reconcile"
        );
        assert!(
            !should_reconcile_host_on_connect(&first_connect, true),
            "guest reconnect must not reconcile"
        );
    }

    // ── roster-seed seq-recheck clobber guard ──

    /// Build a minimal admitted `ParticipantStatusResponse` for the guard tests.
    /// Only `user_id`/`is_host` matter to `resolve_host_set_from_roster`; the rest are
    /// filler so we exercise the real production struct, not a stand-in.
    fn roster_part(user_id: &str, is_host: bool) -> ParticipantStatusResponse {
        ParticipantStatusResponse {
            user_id: user_id.to_string(),
            display_name: None,
            status: "admitted".to_string(),
            is_host,
            is_guest: false,
            joined_at: 0,
            admitted_at: None,
            room_token: None,
            observer_token: None,
            waiting_room_enabled: true,
            admitted_can_admit: false,
            end_on_host_leave: true,
            host_display_name: None,
            host_user_id: None,
            allow_guests: false,
        }
    }

    /// The security-critical clobber guard: after the in-flight `/participants`
    /// fetch, `reseed_host_set_from_roster` only applies the roster read when the
    /// host-event counter is unchanged. This drives the exact production
    /// `resolve_host_set_from_roster` path the async block calls.
    ///
    /// `None` ⇔ the caller's `if let Some(hosts) = …` body never runs, so
    /// `host_set_signal.set` is never called and the signal is left UNTOUCHED —
    /// the stale roster cannot re-introduce a just-revoked host badge.
    ///
    /// Removing the guard's `if current_seq != seq_at_start { return None; }`
    /// early-return makes the stale case return `Some({alice})` instead of
    /// `None`, failing the first assertion.
    #[wasm_bindgen_test]
    fn stale_roster_discarded_when_host_event_landed_mid_fetch() {
        // Roster still lists `alice` as host — a HOST_REVOKED for alice was in
        // flight when this read was taken.
        let parts = vec![roster_part("alice", true), roster_part("bob", false)];

        // A live host event bumped the counter during the fetch (7 → 8): the
        // roster read is stale and MUST be discarded. `None` means the caller
        // leaves `host_set_signal` untouched, so alice's revoked badge stays gone.
        assert_eq!(
            resolve_host_set_from_roster(parts.clone(), 7, 8),
            None,
            "a host event during the fetch must discard the stale roster read"
        );

        // Wrapping bump case: seq_at_start = u64::MAX, current = 0 is still a
        // change and must also discard.
        assert_eq!(
            resolve_host_set_from_roster(parts.clone(), u64::MAX, 0),
            None,
            "a wrapped seq bump must still discard the stale roster read"
        );

        // No event landed (seq unchanged): the roster read is authoritative and
        // IS applied — proving the discard above is a real guard, not a dead
        // path — carrying exactly the `is_host` participants.
        let applied = resolve_host_set_from_roster(parts, 7, 7)
            .expect("unchanged seq must apply the roster host set");
        let expected: HashSet<String> = [String::from("alice")].into_iter().collect();
        assert_eq!(
            applied, expected,
            "only participants flagged is_host are applied"
        );
    }

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

    // -----------------------------------------------------------------------
    // Tests for current_transport_urls (Phase 7, discussion 562)
    //
    // These exercise the pure-logic core via `current_transport_urls_from_lists`
    // so they don't require `window().__APP_CONFIG` to be set up. The full
    // `current_transport_urls` (with `build_lobby_urls`) is a thin wrapper
    // that just calls into this core.
    // -----------------------------------------------------------------------

    #[wasm_bindgen_test]
    fn current_transport_urls_webtransport_with_wt_enabled_returns_both_lists() {
        // WebTransport pref + server says WT enabled: both lists must come
        // through. This is the WT-with-WS-fallback shape — the connection
        // manager creates candidates for every URL and the election prefers
        // WT, but if every WT candidate fails the WS candidates remain
        // available for the manager to elect. This is the scenario the
        // dioxus-ui hits on a normal reconnect once runtime config has
        // loaded.
        let ws = vec!["wss://ws-1".to_string()];
        let wt = vec!["https://wt-1".to_string()];
        let (enable_wt, ws_out, wt_out) = current_transport_urls_from_lists(
            TransportPreference::WebTransport,
            true,
            ws.clone(),
            wt.clone(),
        );
        assert!(enable_wt, "WebTransport+server-WT-enabled must enable WT");
        assert_eq!(
            ws_out, ws,
            "WebTransport must keep the WS list — it's the fallback if every \
             WT candidate fails its handshake"
        );
        assert_eq!(wt_out, wt, "WT list passed through unchanged");
    }

    #[wasm_bindgen_test]
    fn current_transport_urls_webtransport_with_wt_disabled_returns_only_ws() {
        // WebTransport pref + runtime config hasn't loaded (or WT disabled
        // at the server): the WT list is dropped from the effective config,
        // collapsing to WS-only. This is the *initial* shape that strands
        // the user before the reconnect path re-evaluates.
        let ws = vec!["wss://ws-1".to_string()];
        let wt = vec!["https://wt-1".to_string()];
        let (enable_wt, ws_out, wt_out) = current_transport_urls_from_lists(
            TransportPreference::WebTransport,
            false,
            ws.clone(),
            wt.clone(),
        );
        assert!(
            !enable_wt,
            "WebTransport+server-WT-disabled must disable WT"
        );
        assert_eq!(ws_out, ws, "WS list still populated");
        assert_eq!(
            wt_out, wt,
            "WebTransport preserves the WT list shape (resolve_transport_config returns it as-is); \
             the manager's enable_webtransport=false is what gates use of WT"
        );
    }

    #[wasm_bindgen_test]
    fn current_transport_urls_websocket_drops_wt_list() {
        // Explicit WebSocket preference: WT list must always be empty,
        // regardless of what the server-side flag says.
        let ws = vec!["wss://ws-1".to_string()];
        let wt = vec!["https://wt-1".to_string()];
        let (enable_wt, ws_out, wt_out) =
            current_transport_urls_from_lists(TransportPreference::WebSocket, true, ws.clone(), wt);
        assert!(!enable_wt, "WebSocket must report WT disabled");
        assert_eq!(ws_out, ws);
        assert!(wt_out.is_empty(), "WebSocket must drop the WT list");
    }

    #[wasm_bindgen_test]
    fn current_transport_urls_recovery_path_repopulates_wt_after_runtime_load() {
        // Regression for discussion 562: same input lists, but the user's
        // initial `webtransport_enabled()` returned false (runtime config
        // not loaded) and the reconnect's read returns true (loaded by
        // then). The reconnect call must yield a richer URL set than the
        // initial call — that's what flips `total_server_count() > 1` in
        // the manager and lets the watchdog actually re-elect.
        let ws = vec!["wss://ws-1".to_string()];
        let wt = vec!["https://wt-1".to_string()];

        // Initial call (runtime config still loading)
        let (init_enable_wt, init_ws, _init_wt) = current_transport_urls_from_lists(
            TransportPreference::WebTransport,
            false,
            ws.clone(),
            wt.clone(),
        );
        assert!(!init_enable_wt);
        assert_eq!(init_ws, ws);
        // Note: `WebTransport` keeps the wt list value; it's the bool that gates use.
        // The recovery story is that `init_enable_wt == false` makes the manager
        // treat the WT list as unusable even though it's present in the vec —
        // see `resolve_transport_config`. The bool is the real signal, not the
        // list contents.

        // Reconnect call (runtime config now loaded — WT enabled)
        let (reconn_enable_wt, reconn_ws, reconn_wt) = current_transport_urls_from_lists(
            TransportPreference::WebTransport,
            true,
            ws.clone(),
            wt,
        );
        assert!(reconn_enable_wt, "reconnect path now has WT enabled");
        assert_eq!(reconn_ws, ws, "WS list still populated");
        assert!(
            !reconn_wt.is_empty(),
            "reconnect path repopulates the WT list — this is the recovery \
             from the single-server-stranding state"
        );
    }

    // -----------------------------------------------------------------
    // Phase 6: schedule_throttled_bump tests
    // -----------------------------------------------------------------

    use gloo_timers::future::TimeoutFuture;

    /// 5 calls to `schedule_throttled_bump` within the throttle window
    /// should result in exactly **one** invocation of the bump callback.
    /// Reproduces the cc7tp 2026-05-06 render-storm scenario where 5
    /// `peer_speaking` events from different speakers all coalesce
    /// into a single `peer_list_version` bump.
    #[wasm_bindgen_test]
    async fn throttled_bump_coalesces_burst_into_single_invocation() {
        let pending = Rc::new(Cell::new(false));
        let counter = Rc::new(Cell::new(0u32));

        let make_bump = || -> Rc<dyn Fn()> {
            let counter = counter.clone();
            Rc::new(move || {
                counter.set(counter.get() + 1);
            })
        };

        // Use a 50 ms throttle window. We then issue 5 bumps within ~30
        // ms (roughly back-to-back) and wait long enough for the timer
        // to fire.
        for _ in 0..5 {
            schedule_throttled_bump(pending.clone(), 50, make_bump());
            // Tiny await to make sure each call observes a real `await`
            // boundary but stays inside the 50 ms window.
            TimeoutFuture::new(5).await;
        }

        // Wait for the throttle window plus generous margin.
        TimeoutFuture::new(120).await;

        assert_eq!(
            counter.get(),
            1,
            "5 bumps within the throttle window must coalesce into 1 invocation"
        );
        assert!(
            !pending.get(),
            "pending flag should be cleared after the bump fires"
        );
    }

    /// 5 calls spaced 100 ms apart (well outside a 50 ms throttle
    /// window) must each get their own bump — the throttle re-arms
    /// between windows.
    #[wasm_bindgen_test]
    async fn throttled_bump_does_not_drop_events_outside_window() {
        let pending = Rc::new(Cell::new(false));
        let counter = Rc::new(Cell::new(0u32));

        let make_bump = || -> Rc<dyn Fn()> {
            let counter = counter.clone();
            Rc::new(move || {
                counter.set(counter.get() + 1);
            })
        };

        for _ in 0..5 {
            schedule_throttled_bump(pending.clone(), 50, make_bump());
            // Wait > 50 ms so each call sees an empty `pending` flag and
            // schedules a fresh window.
            TimeoutFuture::new(100).await;
        }

        // Final tail wait so the last scheduled timeout has fired.
        TimeoutFuture::new(100).await;

        assert_eq!(
            counter.get(),
            5,
            "5 bumps spaced > throttle window must each fire their own invocation"
        );
    }

    /// Once the throttle window completes the flag must be clear so
    /// the next event can schedule a new window. Equivalent to: bump,
    /// wait for fire, bump again — both should fire.
    #[wasm_bindgen_test]
    async fn throttled_bump_rearm_after_window_completes() {
        let pending = Rc::new(Cell::new(false));
        let counter = Rc::new(Cell::new(0u32));

        let make_bump = || -> Rc<dyn Fn()> {
            let counter = counter.clone();
            Rc::new(move || {
                counter.set(counter.get() + 1);
            })
        };

        schedule_throttled_bump(pending.clone(), 50, make_bump());
        assert!(pending.get(), "pending should be set after first schedule");

        TimeoutFuture::new(100).await;
        assert_eq!(counter.get(), 1, "first bump should have fired");
        assert!(!pending.get(), "pending should be cleared after fire");

        schedule_throttled_bump(pending.clone(), 50, make_bump());
        TimeoutFuture::new(100).await;
        assert_eq!(
            counter.get(),
            2,
            "second bump should have fired after re-arm"
        );
    }
}
