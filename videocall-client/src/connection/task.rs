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
// Generic Task that can be a WebSocketTask or WebTransportTask.
//
// Handles rollover of connection from WebTransport to WebSocket
//
use log::debug;
use videocall_transport::websocket::WebSocketTask;
use videocall_transport::webtransport::WebTransportTask;
use videocall_types::protos::packet_wrapper::PacketWrapper;

use super::webmedia::{ConnectOptions, MediaStreamKey, WebMedia};

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(super) enum Task {
    WebSocket(WebSocketTask),
    WebTransport(WebTransportTask),
}

impl Task {
    pub fn connect(webtransport: bool, options: ConnectOptions) -> anyhow::Result<Self> {
        if webtransport {
            debug!("Task::connect trying WebTransport");
            WebTransportTask::connect(options).map(Task::WebTransport)
        } else {
            debug!("Task::connect trying WebSocket");
            WebSocketTask::connect(options).map(Task::WebSocket)
        }
    }

    /// Send a packet via the reliable per-media-type stream selected by
    /// `stream_key`.  WebSocket ignores the key (single TCP stream);
    /// WebTransport routes to the matching persistent QUIC stream.
    pub fn send_packet(&self, packet: PacketWrapper, stream_key: MediaStreamKey) {
        match self {
            Task::WebSocket(ws) => ws.send_packet(packet, stream_key),
            Task::WebTransport(wt) => wt.send_packet(packet, stream_key),
        }
    }

    /// Send a packet via datagram (unreliable, low-latency) when supported.
    ///
    /// For WebTransport, this uses datagrams for small packets and falls back
    /// to the Control persistent stream for oversized packets.  For
    /// WebSocket, this routes through the single TCP stream (the key is
    /// ignored by the WS impl).
    pub fn send_packet_datagram(&self, packet: PacketWrapper) {
        match self {
            // WebSocket has no datagram concept — fall back to reliable
            // delivery on the Control stream-key (ignored by WS).
            Task::WebSocket(ws) => ws.send_packet(packet, MediaStreamKey::Control),
            Task::WebTransport(wt) => wt.send_packet_datagram(packet),
        }
    }

    pub fn get_send_queue_depth(&self) -> Option<u64> {
        match self {
            Task::WebSocket(ws) => ws.get_buffered_amount(),
            Task::WebTransport(_) => None, // WebTransport doesn't expose bufferedAmount
        }
    }

    /// Raw byte send on the reliable path. Phase 3b (netsim): used by
    /// the async `Delay` / `DelayAndDuplicate` paths in `netsim_hook`
    /// to re-enter the send pipeline after the simulated delay. The
    /// re-entrancy flag inside `netsim_hook` short-circuits hook
    /// consultation on this second pass so we never recurse.
    ///
    /// Only compiled in when the `netsim` feature is on so production
    /// builds carry zero extra surface.
    #[cfg(feature = "netsim")]
    pub fn send_raw_bytes(&self, bytes: Vec<u8>, stream_key: MediaStreamKey) {
        match self {
            Task::WebSocket(ws) => ws.send_bytes(bytes, stream_key),
            Task::WebTransport(wt) => wt.send_bytes(bytes, stream_key),
        }
    }

    /// Raw byte send on the datagram path (with WebSocket
    /// reliable-fallback baked in via the trait default). Companion
    /// to [`Self::send_raw_bytes`] for the netsim async-delay path.
    #[cfg(feature = "netsim")]
    pub fn send_raw_bytes_datagram(&self, bytes: Vec<u8>) {
        match self {
            Task::WebSocket(ws) => ws.send_bytes_datagram(bytes),
            Task::WebTransport(wt) => wt.send_bytes_datagram(bytes),
        }
    }
}
