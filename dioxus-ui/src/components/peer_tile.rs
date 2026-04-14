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

use std::cell::RefCell;
use std::rc::Rc;

use crate::components::canvas_generator::{generate_for_peer, AudioLevels, TileMode};
use crate::components::signal_quality::{PeerSignalHistory, SampleData, SignalInfo};
use crate::context::{MeetingTimeCtx, PeerSignalHistoryMap, VideoCallClientCtx};
use dioxus::prelude::*;
use futures::future::AbortHandle;
use futures::future::Abortable;
use gloo_timers::callback::Timeout;
use videocall_client::audio_constants::{MIC_HOLD_DURATION_MS, UI_AUDIO_LEVEL_DELTA};
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};
use wasm_bindgen::JsCast;

#[component]
pub fn PeerTile(
    peer_id: String,
    #[props(default = false)] full_bleed: bool,
    #[props(default)] host_user_id: Option<String>,
    #[props(default)] render_mode: TileMode,
    #[props(default)] my_peer_id: Option<String>,
    #[props(default)] pinned_peer_id: Option<String>,
    on_toggle_pin: EventHandler<String>,
) -> Element {
    let client = use_context::<VideoCallClientCtx>();

    let mut audio_enabled = use_signal(|| false);
    let mut video_enabled = use_signal(|| false);
    let mut screen_enabled = use_signal(|| false);
    let mut audio_level = use_signal(|| 0.0_f32);
    // Separate signal for mic icon: holds the last positive value for 1s after
    // audio drops to zero, so the mic stays green briefly after speech ends.
    let mut mic_audio_level = use_signal(|| 0.0_f32);
    // Holds the pending silence timeout so it can be cancelled if new audio arrives.
    let mic_hold_timeout: Rc<RefCell<Option<Timeout>>> = use_hook(|| Rc::new(RefCell::new(None)));

    // Signal quality tracking: raw metrics from diagnostics events
    let mut fps_received = use_signal(|| 0.0_f64);
    let mut expand_rate = use_signal(|| 0.0_f64);
    let mut video_bitrate = use_signal(|| 0.0_f64);
    let mut audio_bitrate = use_signal(|| 0.0_f64);
    let mut audio_buffer_ms = use_signal(|| 0.0_f64);
    let mut screen_fps = use_signal(|| 0.0_f64);
    let mut screen_bitrate = use_signal(|| 0.0_f64);
    let mut latency_ms = use_signal(|| 0.0_f64);
    let mut video_resolution = use_signal(String::new);
    // Look up or create this peer's signal history in the shared context.
    // The history lives in a context-provided map so it survives PeerTile
    // remounts caused by layout switches (e.g., grid -> split on screen share).
    let mut history_map = use_context::<PeerSignalHistoryMap>();
    let signal_history: Rc<RefCell<PeerSignalHistory>> = {
        let mut map = history_map.write();
        map.entry(peer_id.clone())
            .or_insert_with(|| Rc::new(RefCell::new(PeerSignalHistory::new())))
            .clone()
    };
    let show_signal_popup = use_signal(|| false);
    // Counter that increments each time a sample is pushed. Reading this
    // Dioxus Signal triggers re-renders, compensating for the fact that
    // Rc<RefCell<PeerSignalHistory>> is not reactive.
    let mut sample_counter = use_signal(|| 0u32);
    // Track last sample timestamp to throttle to ~1 sample/second
    let last_sample_ts: Rc<RefCell<f64>> = use_hook(|| Rc::new(RefCell::new(0.0)));

    // Initialize from client snapshot and subscribe to diagnostics
    let peer_id_owned = peer_id.clone();
    let effect_client = client.clone();
    let prev_abort_handle = use_hook(|| Rc::new(RefCell::new(None::<AbortHandle>)));
    let mic_hold_for_effect = mic_hold_timeout.clone();
    let last_sample_for_effect = last_sample_ts.clone();
    let signal_history_for_effect = signal_history.clone();
    use_effect(move || {
        // Abort previous subscription
        if let Some(h) = prev_abort_handle.borrow_mut().take() {
            h.abort();
        }

        // Initialize from client snapshot
        audio_enabled.set(effect_client.is_audio_enabled_for_peer(&peer_id_owned));
        video_enabled.set(effect_client.is_video_enabled_for_peer(&peer_id_owned));
        screen_enabled.set(effect_client.is_screen_share_enabled_for_peer(&peer_id_owned));
        let initial_level = effect_client.audio_level_for_peer(&peer_id_owned);
        audio_level.set(initial_level);
        mic_audio_level.set(initial_level);

        let peer_id_inner = peer_id_owned.clone();
        let mic_hold = mic_hold_for_effect.clone();
        let last_sample = last_sample_for_effect.clone();
        // Clone the Rc for the async block so the outer FnMut closure can be
        // called again without consuming the captured value.
        let signal_hist = signal_history_for_effect.clone();

        // Subscribe to global diagnostics for peer_status updates
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        *prev_abort_handle.borrow_mut() = Some(abort_handle);

        let fut = async move {
            let mut rx = subscribe();
            while let Ok(evt) = rx.recv().await {
                handle_diagnostics_event(
                    &evt,
                    &peer_id_inner,
                    &mut audio_enabled,
                    &mut video_enabled,
                    &mut screen_enabled,
                    &mut audio_level,
                    &mut mic_audio_level,
                    &mic_hold,
                    &mut fps_received,
                    &mut expand_rate,
                    &mut video_bitrate,
                    &mut audio_bitrate,
                    &mut audio_buffer_ms,
                    &mut screen_fps,
                    &mut screen_bitrate,
                    &mut latency_ms,
                    &mut video_resolution,
                );
                // Push a signal quality sample at most once per second,
                // piggybacking on the diagnostics event stream.
                // If resolution is unknown from diagnostics, read it from the
                // canvas element. Skip 300x150 (HTML default before decoder
                // renders the first frame).
                let mut res = video_resolution.peek().clone();
                if res.is_empty() && *video_enabled.peek() {
                    if let Some(canvas) = gloo_utils::document()
                        .get_element_by_id(&peer_id_inner)
                        .and_then(|el| el.dyn_into::<web_sys::HtmlCanvasElement>().ok())
                    {
                        let w = canvas.width();
                        let h = canvas.height();
                        if w > 0 && h > 0 && !(w == 300 && h == 150) {
                            res = format!("{w}x{h}");
                            video_resolution.set(res.clone());
                        }
                    }
                }
                let data = SampleData {
                    video_fps: *fps_received.peek(),
                    video_bitrate_kbps: *video_bitrate.peek(),
                    video_resolution: res,
                    audio_bitrate_kbps: *audio_bitrate.peek(),
                    audio_expand_rate: *expand_rate.peek(),
                    audio_buffer_ms: *audio_buffer_ms.peek(),
                    screen_enabled: *screen_enabled.peek(),
                    screen_fps: *screen_fps.peek(),
                    screen_bitrate_kbps: *screen_bitrate.peek(),
                    latency_ms: *latency_ms.peek(),
                    audio_enabled: *audio_enabled.peek(),
                    video_enabled: *video_enabled.peek(),
                };
                maybe_push_signal_sample(&last_sample, &signal_hist, &data, &mut sample_counter);
            }
        };
        let abortable = Abortable::new(fut, abort_reg);
        spawn(async move {
            let _ = abortable.await;
        });
    });

    let host_uid = host_user_id.as_deref();

    // Re-read signals to trigger reactive re-renders
    let audio_en = audio_enabled();
    let video_en = video_enabled();
    let screen_en = screen_enabled();
    let level = audio_level();
    let mic_level = mic_audio_level();

    // Read signal history and derive current signal level.
    // Only clone the full sample history when the popup is visible to avoid
    // copying ~3.4 MB/s of data when 20 peers update at ~2 Hz.
    let sig_history = signal_history.borrow();
    let sig_level = sig_history.current_level(audio_en, video_en, screen_en);
    let sig_samples = if show_signal_popup() {
        // Reading sample_counter subscribes this component to updates from the
        // diagnostics task, ensuring the chart re-renders when new samples arrive.
        let _ = sample_counter();
        sig_history.samples_vec()
    } else {
        Vec::new()
    };
    drop(sig_history);

    generate_for_peer(
        &client,
        &peer_id,
        full_bleed,
        AudioLevels {
            raw: level,
            mic: mic_level,
        },
        host_uid,
        render_mode,
        my_peer_id.as_deref(),
        SignalInfo {
            level: sig_level,
            history: sig_samples,
            meeting_start_ms: {
                let mt = use_context::<MeetingTimeCtx>();
                mt().meeting_start_time.unwrap_or_else(js_sys::Date::now)
            },
        },
        show_signal_popup,
        pinned_peer_id.as_deref(),
        on_toggle_pin,
    )
}

/// Extract the audio level from a diagnostics event, falling back to
/// the boolean `is_speaking` flag when the float metric is absent.
fn resolve_audio_level(audio_lvl: Option<f32>, speaking: Option<bool>) -> Option<f32> {
    if let Some(lvl) = audio_lvl {
        Some(lvl)
    } else {
        speaking.map(|s| if s { 1.0 } else { 0.0 })
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_diagnostics_event(
    evt: &DiagEvent,
    peer_id: &str,
    audio_enabled: &mut Signal<bool>,
    video_enabled: &mut Signal<bool>,
    screen_enabled: &mut Signal<bool>,
    audio_level: &mut Signal<f32>,
    mic_audio_level: &mut Signal<f32>,
    mic_hold_timeout: &Rc<RefCell<Option<Timeout>>>,
    fps_received: &mut Signal<f64>,
    expand_rate: &mut Signal<f64>,
    video_bitrate: &mut Signal<f64>,
    _audio_bitrate: &mut Signal<f64>,
    audio_buffer_ms: &mut Signal<f64>,
    screen_fps: &mut Signal<f64>,
    screen_bitrate: &mut Signal<f64>,
    latency_ms: &mut Signal<f64>,
    video_resolution: &mut Signal<String>,
) {
    match evt.subsystem {
        "peer_status" => {
            let mut to_peer: Option<String> = None;
            let mut audio: Option<bool> = None;
            let mut video: Option<bool> = None;
            let mut screen: Option<bool> = None;
            let mut audio_lvl: Option<f32> = None;
            let mut speaking: Option<bool> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                    ("audio_enabled", MetricValue::U64(v)) => audio = Some(*v != 0),
                    ("video_enabled", MetricValue::U64(v)) => video = Some(*v != 0),
                    ("screen_enabled", MetricValue::U64(v)) => screen = Some(*v != 0),
                    ("audio_level", MetricValue::F64(v)) => audio_lvl = Some(*v as f32),
                    ("is_speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
                    _ => {}
                }
            }
            if to_peer.as_deref() != Some(peer_id) {
                return;
            }
            if let Some(a) = audio {
                if a != *audio_enabled.peek() {
                    audio_enabled.set(a);
                }
            }
            if let Some(v) = video {
                if v != *video_enabled.peek() {
                    video_enabled.set(v);
                }
            }
            if let Some(s) = screen {
                if s != *screen_enabled.peek() {
                    screen_enabled.set(s);
                }
            }
            // Prefer the float audio_level metric; fall back to boolean is_speaking
            let resolved_level = resolve_audio_level(audio_lvl, speaking);
            if let Some(lvl) = resolved_level {
                let prev = *audio_level.peek();
                if (lvl == 0.0 && prev != 0.0) || (lvl - prev).abs() > UI_AUDIO_LEVEL_DELTA {
                    audio_level.set(lvl);
                }
                update_mic_audio_level(lvl, mic_audio_level, mic_hold_timeout);
            }
        }
        "peer_speaking" => {
            // Fast-path speaking updates from decoded audio frames
            let mut to_peer: Option<String> = None;
            let mut audio_lvl: Option<f32> = None;
            let mut speaking: Option<bool> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                    ("audio_level", MetricValue::F64(v)) => audio_lvl = Some(*v as f32),
                    ("speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
                    _ => {}
                }
            }
            if to_peer.as_deref() != Some(peer_id) {
                return;
            }
            let resolved_level = resolve_audio_level(audio_lvl, speaking);
            if let Some(lvl) = resolved_level {
                let prev = *audio_level.peek();
                if (lvl == 0.0 && prev != 0.0) || (lvl - prev).abs() > UI_AUDIO_LEVEL_DELTA {
                    audio_level.set(lvl);
                }
                update_mic_audio_level(lvl, mic_audio_level, mic_hold_timeout);
            }
        }
        "video" => {
            // Extract fps_received, bitrate_kbps, and media_type for quality scoring.
            let mut to_peer: Option<String> = None;
            let mut fps: Option<f64> = None;
            let mut bitrate: Option<f64> = None;
            let mut media_type_str: Option<String> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                    ("fps_received", MetricValue::F64(v)) => fps = Some(*v),
                    ("bitrate_kbps", MetricValue::F64(v)) => bitrate = Some(*v),
                    ("media_type", MetricValue::Text(t)) => media_type_str = Some(t.clone()),
                    _ => {}
                }
            }
            if to_peer.as_deref() != Some(peer_id) {
                return;
            }
            let is_screen = media_type_str.as_deref() == Some("SCREEN");
            if is_screen {
                if let Some(f) = fps {
                    screen_fps.set(f);
                }
                if let Some(b) = bitrate {
                    screen_bitrate.set(b);
                }
            } else {
                if let Some(f) = fps {
                    fps_received.set(f);
                }
                if let Some(b) = bitrate {
                    video_bitrate.set(b);
                }
            }
        }
        "neteq" => {
            // Extract expand_rate and audio_buffer_ms from neteq metrics.
            let mut target_peer: Option<String> = None;
            let mut er: Option<f64> = None;
            let mut buf_ms: Option<f64> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("target_peer", MetricValue::Text(p)) => target_peer = Some(p.clone()),
                    ("expand_rate", MetricValue::F64(v)) => er = Some(*v),
                    ("audio_buffer_ms", MetricValue::U64(v)) => buf_ms = Some(*v as f64),
                    _ => {}
                }
            }
            if target_peer.as_deref() != Some(peer_id) {
                return;
            }
            if let Some(rate) = er {
                // Convert from Q14 to per-mille: value / 16.384
                expand_rate.set(rate / 16.384);
            }
            if let Some(b) = buf_ms {
                audio_buffer_ms.set(b);
            }
        }
        "video_resolution" => {
            // Track video resolution changes broadcast by the decoder.
            let mut to_peer: Option<String> = None;
            let mut res_w: Option<u64> = None;
            let mut res_h: Option<u64> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                    ("resolution_width", MetricValue::U64(w)) => res_w = Some(*w),
                    ("resolution_height", MetricValue::U64(h)) => res_h = Some(*h),
                    _ => {}
                }
            }
            if to_peer.as_deref() != Some(peer_id) {
                return;
            }
            if let (Some(w), Some(h)) = (res_w, res_h) {
                let res = format!("{w}x{h}");
                if *video_resolution.peek() != res {
                    video_resolution.set(res);
                }
            }
        }
        "connection_manager" => {
            // RTT is a global metric (not per-peer), but we store it per-sample
            // so the chart can show latency alongside quality lines.
            let mut rtt: Option<f64> = None;
            for m in &evt.metrics {
                if let ("active_server_rtt", MetricValue::F64(v)) = (m.name, &m.value) {
                    rtt = Some(*v);
                }
            }
            if let Some(r) = rtt {
                latency_ms.set(r);
            }
        }
        _ => {}
    }
}

/// Push a signal quality sample at most once per second.
/// Increments `sample_counter` so the UI re-renders when the popup is open.
fn maybe_push_signal_sample(
    last_ts: &Rc<RefCell<f64>>,
    signal_history: &Rc<RefCell<PeerSignalHistory>>,
    data: &SampleData,
    sample_counter: &mut Signal<u32>,
) {
    let now = js_sys::Date::now();
    let prev = *last_ts.borrow();
    if now - prev < 1000.0 {
        return;
    }
    *last_ts.borrow_mut() = now;
    signal_history.borrow_mut().push_sample(data);
    let prev_count = *sample_counter.peek();
    sample_counter.set(prev_count.wrapping_add(1));
}

/// Update `mic_audio_level` with a 1-second hold: when audio drops to zero the
/// mic signal keeps its last positive value for 1 s so the icon stays green.
/// If new audio arrives before the timeout fires the pending timeout is cancelled.
fn update_mic_audio_level(
    level: f32,
    mic_audio_level: &mut Signal<f32>,
    mic_hold_timeout: &Rc<RefCell<Option<Timeout>>>,
) {
    if level > 0.0 {
        // Cancel any pending silence timeout — speaker is still active.
        mic_hold_timeout.borrow_mut().take();
        let prev = *mic_audio_level.peek();
        if (level - prev).abs() > UI_AUDIO_LEVEL_DELTA {
            mic_audio_level.set(level);
        }
    } else {
        // Audio dropped to zero. If already silent (or timeout already pending), skip.
        if *mic_audio_level.peek() == 0.0 {
            return;
        }
        if mic_hold_timeout.borrow().is_some() {
            // A timeout is already queued — let it fire.
            return;
        }
        let mut sig = *mic_audio_level;
        let timeout = Timeout::new(MIC_HOLD_DURATION_MS, move || {
            sig.set(0.0);
        });
        *mic_hold_timeout.borrow_mut() = Some(timeout);
    }
}
