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
use crate::context::{PeerStatusMap, VideoCallClientCtx};
use dioxus::prelude::*;

#[component]
pub fn PeerTile(
    peer_id: String,
    #[props(default = false)] full_bleed: bool,
    #[props(default)] host_display_name: Option<String>,
) -> Element {
    let client = use_context::<VideoCallClientCtx>();
    let peer_status_map = use_context::<PeerStatusMap>();

    // Subscribe to map-level changes (so we pick up newly-inserted peer signals)
    // and to this peer's individual signal (so we re-render on state changes).
    // The map-level read causes a re-render when a new peer is added, but that
    // is necessary: a PeerTile may render before the diagnostics task creates
    // the per-peer signal, and without this subscription it would never learn
    // about the signal's creation.
    if let Some(peer_signal) = peer_status_map.read().get(&peer_id).copied() {
        let _ = peer_signal.read();
    }

    let host_dn = host_display_name.as_deref();
    generate_for_peer(&client, &peer_id, full_bleed, host_dn)
}
