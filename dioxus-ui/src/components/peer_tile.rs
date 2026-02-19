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

use crate::components::canvas_generator::generate_for_peer;
use crate::context::VideoCallClientCtx;
use dioxus::prelude::*;
use futures::future::{AbortHandle, Abortable};
use std::cell::RefCell;
use std::rc::Rc;
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};

#[component]
pub fn PeerTile(
    peer_id: String,
    #[props(default = false)] full_bleed: bool,
    #[props(default)] host_display_name: Option<String>,
) -> Element {
    let client = use_context::<VideoCallClientCtx>();

    let mut audio_enabled = use_signal(|| false);
    let mut video_enabled = use_signal(|| false);
    let mut screen_enabled = use_signal(|| false);

    // Initialize from client snapshot and subscribe to diagnostics
    let peer_id_owned = peer_id.clone();
    let effect_client = client.clone();
    let prev_abort_handle = use_hook(|| Rc::new(RefCell::new(None::<AbortHandle>)));
    use_effect(move || {
        // Abort previous subscription
        if let Some(h) = prev_abort_handle.borrow_mut().take() {
            h.abort();
        }

        // Initialize from client snapshot
        audio_enabled.set(effect_client.is_audio_enabled_for_peer(&peer_id_owned));
        video_enabled.set(effect_client.is_video_enabled_for_peer(&peer_id_owned));
        screen_enabled.set(effect_client.is_screen_share_enabled_for_peer(&peer_id_owned));

        let peer_id_inner = peer_id_owned.clone();

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
                );
            }
        };
        let abortable = Abortable::new(fut, abort_reg);
        wasm_bindgen_futures::spawn_local(async move {
            let _ = abortable.await;
        });
    });

    let host_dn = host_display_name.as_deref();

    // Re-read signals to trigger reactive re-renders
    let _ = audio_enabled();
    let _ = video_enabled();
    let _ = screen_enabled();

    generate_for_peer(&client, &peer_id, full_bleed, host_dn)
}

fn handle_diagnostics_event(
    evt: &DiagEvent,
    peer_id: &str,
    audio_enabled: &mut Signal<bool>,
    video_enabled: &mut Signal<bool>,
    screen_enabled: &mut Signal<bool>,
) {
    if evt.subsystem != "peer_status" {
        return;
    }
    let mut to_peer: Option<String> = None;
    let mut audio: Option<bool> = None;
    let mut video: Option<bool> = None;
    let mut screen: Option<bool> = None;
    for m in &evt.metrics {
        match (m.name, &m.value) {
            ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
            ("audio_enabled", MetricValue::U64(v)) => audio = Some(*v != 0),
            ("video_enabled", MetricValue::U64(v)) => video = Some(*v != 0),
            ("screen_enabled", MetricValue::U64(v)) => screen = Some(*v != 0),
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
}
