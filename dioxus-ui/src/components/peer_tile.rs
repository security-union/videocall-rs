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

    // Use peek() for the outer map lookup to avoid subscribing this tile to
    // the HashMap signal. This prevents O(N²) re-renders during peer join
    // bursts. We only subscribe to the per-peer signal for fine-grained
    // updates. If the per-peer signal doesn't exist yet (peer joined but
    // diagnostics task hasn't created it), this tile will re-render when
    // the parent re-renders from the peer_list_version bump.
    if let Some(peer_signal) = peer_status_map.peek().get(&peer_id).copied() {
        let _ = peer_signal.read();
    }

    let host_dn = host_display_name.as_deref();
    generate_for_peer(&client, &peer_id, full_bleed, host_dn)
}
