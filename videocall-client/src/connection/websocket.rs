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

//
// This submodule implements our WebMedia trait for WebSocketTask.
//
use super::connection_lost_reason::ConnectionLostReason;
use super::url_log::strip_query_for_log;
use super::webmedia::{ConnectOptions, InboundLane, MediaStreamKey, ReceivedAtMs, WebMedia};
use log::debug;
use std::cell::Cell;
use std::rc::Rc;
use videocall_transport::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;

impl WebMedia<WebSocketTask> for WebSocketTask {
    fn connect(options: ConnectOptions) -> anyhow::Result<WebSocketTask> {
        // Phase 3c: the netsim shim is installed once-per-tab from
        // `?netsim=<profile>` by `Connection::connect` via
        // `super::netsim_url::try_install_from_url`. We deliberately do
        // **not** install or clear the hook here — doing so would
        // overwrite the URL-driven slot with the unused, hardcoded
        // `ConnectOptions::netsim_hook` placeholder and silently
        // disable the simulator on every reconnect. The
        // `Connection::connect` caller still registers the
        // `Weak<Task>` used by the async-delay path.

        // Track whether the handshake (Opened event) has completed, so that
        // subsequent Close/Error events can be classified correctly.
        let handshake_complete = Rc::new(Cell::new(false));
        // Guard against emitting connection_lost more than once per connection
        // (browser may fire both Close and Error for the same failure).
        let ws_fired = Rc::new(Cell::new(false));

        let hs_flag = handshake_complete.clone();
        let fired = ws_fired.clone();
        let notification = Callback::from(move |status| match status {
            WebSocketStatus::Opened => {
                hs_flag.set(true);
                options.on_connected.emit(());
            }
            WebSocketStatus::Closed(close_info) => {
                if fired.replace(true) {
                    return; // already emitted
                }
                let msg = match close_info {
                    Some((code, ref reason)) if !reason.is_empty() => {
                        format!("WebSocket closed: code={code}, reason={reason}")
                    }
                    Some((code, _)) => format!("WebSocket closed: code={code}"),
                    None => "WebSocket closed".to_string(),
                };
                if handshake_complete.get() {
                    options
                        .on_connection_lost
                        .emit(ConnectionLostReason::SessionDropped(msg));
                } else {
                    options
                        .on_connection_lost
                        .emit(ConnectionLostReason::HandshakeFailed(msg));
                }
            }
            WebSocketStatus::Error => {
                if fired.replace(true) {
                    return; // already emitted
                }
                let msg = "WebSocket error".to_string();
                if handshake_complete.get() {
                    options
                        .on_connection_lost
                        .emit(ConnectionLostReason::SessionDropped(msg));
                } else {
                    options
                        .on_connection_lost
                        .emit(ConnectionLostReason::HandshakeFailed(msg));
                }
            }
        });
        debug!(
            "WebSocket connecting to {}",
            strip_query_for_log(&options.websocket_url)
        );
        let on_inbound_media = reliable_lane_adapter(options.on_inbound_media.clone());
        let task =
            WebSocketService::connect(&options.websocket_url, on_inbound_media, notification)?;
        debug!("WebSocket task created (connection pending)");
        Ok(task)
    }

    /// WebSocket has a single TCP stream, so the media key does not affect
    /// routing. It is retained for backpressure-counter attribution.
    fn send_bytes(&self, bytes: Vec<u8>, stream_key: MediaStreamKey) {
        // Phase 3b (discussion #793). When the `netsim` feature is
        // off this entire block is compiled out and the send path is
        // byte-for-byte identical to pre-3b. When on, the per-tab
        // hook may instruct us to drop, delay, or duplicate the
        // packet; the helper returns `true` in those cases and the
        // sync `send_binary` below is skipped.
        #[cfg(feature = "netsim")]
        {
            if super::netsim_hook::shape_uplink_reliable(&bytes, stream_key) {
                return;
            }
        }
        self.send_binary_for_stream(bytes, stream_key.as_u8());
    }
}

/// One TCP socket, no datagram lane: every WebSocket packet is reliable (#2720).
fn reliable_lane_adapter(
    callback: Callback<(PacketWrapper, InboundLane, ReceivedAtMs)>,
) -> Callback<PacketWrapper> {
    Callback::from(move |packet: PacketWrapper| {
        callback.emit((
            packet,
            InboundLane::Reliable,
            ReceivedAtMs(videocall_transport::clock::now_ms()),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn websocket_inbound_packets_are_tagged_reliable() {
        let seen: Rc<RefCell<Vec<InboundLane>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = {
            let seen = Rc::clone(&seen);
            Callback::from(
                move |(_, lane, _): (PacketWrapper, InboundLane, ReceivedAtMs)| {
                    seen.borrow_mut().push(lane);
                },
            )
        };

        reliable_lane_adapter(sink).emit(PacketWrapper::new());

        assert_eq!(
            *seen.borrow(),
            vec![InboundLane::Reliable],
            "WebSocket has no datagram lane, so its packets must count as reliable liveness"
        );
    }
}
