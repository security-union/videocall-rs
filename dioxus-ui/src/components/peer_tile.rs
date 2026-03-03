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

    // Subscribe to peer_status_map changes so Dioxus re-renders this component
    // when peer state updates. The actual state values are read by
    // generate_for_peer via client.is_*_for_peer(), which reads the same Peer
    // fields that were updated before the diagnostics event was broadcast.
    let _ = peer_status_map.read().get(&peer_id);

    let host_dn = host_display_name.as_deref();
    generate_for_peer(&client, &peer_id, full_bleed, host_dn)
}
