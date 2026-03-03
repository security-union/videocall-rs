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
use crate::context::{PeerMediaState, PeerStatusMap, VideoCallClientCtx};
use dioxus::prelude::*;

#[component]
pub fn PeerTile(
    peer_id: String,
    #[props(default = false)] full_bleed: bool,
    #[props(default)] host_display_name: Option<String>,
) -> Element {
    let client = use_context::<VideoCallClientCtx>();
    let peer_status_map = use_context::<PeerStatusMap>();

    // Read peer state from the shared map (populated by a single subscriber
    // in the parent AttendantsComponent). Falls back to client snapshot for
    // peers that haven't sent a heartbeat yet.
    let state = peer_status_map
        .read()
        .get(&peer_id)
        .cloned()
        .unwrap_or_else(|| PeerMediaState {
            audio_enabled: client.is_audio_enabled_for_peer(&peer_id),
            video_enabled: client.is_video_enabled_for_peer(&peer_id),
            screen_enabled: client.is_screen_share_enabled_for_peer(&peer_id),
        });

    // Bind to locals so Dioxus tracks the reactive dependency on peer_status_map.
    let _ = state.audio_enabled;
    let _ = state.video_enabled;
    let _ = state.screen_enabled;

    let host_dn = host_display_name.as_deref();
    generate_for_peer(&client, &peer_id, full_bleed, host_dn)
}
