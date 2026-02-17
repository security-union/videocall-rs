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

use dioxus::prelude::*;
use futures::future::{AbortHandle, Abortable};
use videocall_client::VideoCallClient;
use videocall_diagnostics::{subscribe, MetricValue};

use crate::components::canvas_generator::generate_for_peer;
use crate::context::VideoCallClientCtx;

#[derive(Props, Clone, PartialEq)]
pub struct PeerTileProps {
    pub peer_id: String,
    /// True when layout has only this peer and no screen share; affects styling
    #[props(default = false)]
    pub full_bleed: bool,
    /// Display name (username) of the meeting host (for displaying crown icon)
    #[props(default)]
    pub host_display_name: Option<String>,
}

#[component]
pub fn PeerTile(props: PeerTileProps) -> Element {
    let client: Option<VideoCallClient> = try_use_context::<VideoCallClientCtx>();

    let mut audio_enabled = use_signal(|| false);
    let mut video_enabled = use_signal(|| false);
    let mut screen_enabled = use_signal(|| false);

    // Initialize from client snapshot and subscribe to diagnostics
    let peer_id = props.peer_id.clone();
    let client_for_effect = client.clone();
    use_effect(move || {
        if let Some(ref client) = client_for_effect {
            // Initialize from client snapshot to avoid waiting for first diagnostic
            audio_enabled.set(client.is_audio_enabled_for_peer(&peer_id));
            video_enabled.set(client.is_video_enabled_for_peer(&peer_id));
            screen_enabled.set(client.is_screen_share_enabled_for_peer(&peer_id));

            // Subscribe to global diagnostics for peer_status updates
            let peer_id_clone = peer_id.clone();
            let (abort_handle, abort_reg) = AbortHandle::new_pair();

            let fut = async move {
                let mut rx = subscribe();
                while let Ok(evt) = rx.recv().await {
                    if evt.subsystem != "peer_status" {
                        continue;
                    }

                    // Parse peer_status metrics
                    let mut to_peer: Option<String> = None;
                    let mut audio_val: Option<bool> = None;
                    let mut video_val: Option<bool> = None;
                    let mut screen_val: Option<bool> = None;

                    for m in &evt.metrics {
                        match (m.name, &m.value) {
                            ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                            ("audio_enabled", MetricValue::U64(v)) => audio_val = Some(*v != 0),
                            ("video_enabled", MetricValue::U64(v)) => video_val = Some(*v != 0),
                            ("screen_enabled", MetricValue::U64(v)) => screen_val = Some(*v != 0),
                            _ => {}
                        }
                    }

                    if to_peer.as_deref() != Some(&peer_id_clone) {
                        continue;
                    }

                    if let Some(a) = audio_val {
                        if a != *audio_enabled.read() {
                            audio_enabled.set(a);
                        }
                    }
                    if let Some(v) = video_val {
                        if v != *video_enabled.read() {
                            video_enabled.set(v);
                        }
                    }
                    if let Some(s) = screen_val {
                        if s != *screen_enabled.read() {
                            screen_enabled.set(s);
                        }
                    }
                }
            };

            let abortable = Abortable::new(fut, abort_reg);
            wasm_bindgen_futures::spawn_local(async move {
                let _ = abortable.await;
            });

            // Return cleanup function
            // Note: Dioxus handles cleanup differently than Yew
            // We store the abort handle but cleanup is automatic on unmount
        }
    });

    // Render using the canvas generator
    if let Some(ref client) = client {
        generate_for_peer(
            client,
            &props.peer_id,
            props.full_bleed,
            props.host_display_name.as_deref(),
        )
    } else {
        rsx! {}
    }
}
