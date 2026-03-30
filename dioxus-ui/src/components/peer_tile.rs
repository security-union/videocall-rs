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
use crate::context::VideoCallClientCtx;
use dioxus::prelude::*;
use futures::future::AbortHandle;
use futures::future::Abortable;
use gloo_timers::callback::Timeout;
use videocall_client::audio_constants::{MIC_HOLD_DURATION_MS, UI_AUDIO_LEVEL_DELTA};
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};

#[component]
pub fn PeerTile(
    peer_id: String,
    #[props(default = false)] full_bleed: bool,
    #[props(default)] host_user_id: Option<String>,
    #[props(default)] render_mode: TileMode,
    #[props(default)] my_peer_id: Option<String>,
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

    // Initialize from client snapshot and subscribe to diagnostics
    let peer_id_owned = peer_id.clone();
    let effect_client = client.clone();
    let prev_abort_handle = use_hook(|| Rc::new(RefCell::new(None::<AbortHandle>)));
    let mic_hold_for_effect = mic_hold_timeout.clone();
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
                );
            }
        };
        let abortable = Abortable::new(fut, abort_reg);
        spawn(async move {
            let _ = abortable.await;
        });
    });

    let host_uid = host_user_id.as_deref();

    // Re-read signals to trigger reactive re-renders
    let _ = audio_enabled();
    let _ = video_enabled();
    let _ = screen_enabled();
    let level = audio_level();
    let mic_level = mic_audio_level();

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
        _ => {}
    }
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
