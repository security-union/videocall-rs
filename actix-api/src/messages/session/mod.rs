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

use crate::actors::session_logic::SessionId;
use actix::Message as ActixMessage;
use bytes::Bytes;

/// An outbound payload fanned out from the relay to a single receiver session.
///
/// `msg` is a `bytes::Bytes` (not an owned `Vec<u8>`) so that the SINGLE NATS
/// payload allocation is shared (atomic-refcounted) across every receiver in a
/// room's fan-out: `chat_server::handle_msg` clones the `Bytes` handle once per
/// recipient (`O(1)`, a refcount bump) instead of deep-copying the multi-KB
/// frame per recipient (issue #1063). With the WT mailbox now 1024+ slots, the
/// old `.to_vec()`-per-receiver pattern meant worst-case transient memory of
/// `recipients × mailbox_depth × frame_size` distinct copies; sharing the one
/// underlying allocation collapses that to a single buffer plus cheap handles.
///
/// The materialization back to owned bytes for the per-transport outbound
/// channel happens at most once per receiver inside
/// `SessionLogic::handle_outbound`, exactly as before — there is NO change to
/// delivery behaviour.
#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct Message {
    pub session: SessionId,
    pub msg: Bytes,
}
