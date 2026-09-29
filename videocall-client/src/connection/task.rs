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
use videocall_transport::webtransport::FrameDropMeta;
use videocall_transport::webtransport::WebTransportTask;
use videocall_types::protos::packet_wrapper::PacketWrapper;

use super::webmedia::{ConnectOptions, MediaStreamKey, WebMedia};

#[cfg(test)]
use std::cell::RefCell;

/// Separated from [`Task`] so the choice is one function the stub shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UplinkTransport {
    WebSocket,
    WebTransport,
}

/// The `send_queue_bytes` reading for one transport (#2722). WebSocket gives
/// its socket's `bufferedAmount`; WebTransport has no such counter and gives the
/// persistent-unistream buried backlog, which the WS-only accessor could not.
pub(super) fn uplink_queue_depth_for(
    transport: UplinkTransport,
    ws_buffered_amount: Option<u64>,
) -> Option<u64> {
    match transport {
        UplinkTransport::WebSocket => ws_buffered_amount,
        UplinkTransport::WebTransport => {
            Some(videocall_transport::webtransport::unistream_queue_depth_bytes())
        }
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(super) enum Task {
    WebSocket(WebSocketTask),
    WebTransport(WebTransportTask),
    /// No-op send path for unit tests (records last send kind + stream key).
    #[cfg(test)]
    Stub(StubTask),
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StubSendKind {
    Reliable,
    Datagram,
}

#[cfg(test)]
#[derive(Debug)]
pub(super) struct StubTask {
    last_send: RefCell<Option<(StubSendKind, MediaStreamKey)>>,
    /// Every send since the last drain, in order — `last_send` keeps only the
    sends: RefCell<Vec<(StubSendKind, MediaStreamKey)>>,
    transport: UplinkTransport,
    buffered_amount: std::cell::Cell<Option<u64>>,
}

#[cfg(test)]
impl StubTask {
    pub(super) fn new() -> Self {
        Self::for_transport(UplinkTransport::WebSocket, None)
    }

    pub(super) fn for_transport(transport: UplinkTransport, buffered_amount: Option<u64>) -> Self {
        Self {
            last_send: RefCell::new(None),
            sends: RefCell::new(Vec::new()),
            transport,
            buffered_amount: std::cell::Cell::new(buffered_amount),
        }
    }

    fn record(&self, kind: StubSendKind, stream_key: MediaStreamKey) {
        *self.last_send.borrow_mut() = Some((kind, stream_key));
        self.sends.borrow_mut().push((kind, stream_key));
    }

    pub(super) fn take_last_send_for_test(&self) -> Option<(StubSendKind, MediaStreamKey)> {
        self.last_send.borrow_mut().take()
    }

    pub(super) fn clear_last_send_for_test(&self) {
        *self.last_send.borrow_mut() = None;
        self.sends.borrow_mut().clear();
    }

    pub(super) fn take_sends_for_test(&self) -> Vec<(StubSendKind, MediaStreamKey)> {
        std::mem::take(&mut *self.sends.borrow_mut())
    }

    pub(super) fn set_send_queue_depth_for_test(&self, bytes: u64) {
        self.buffered_amount.set(Some(bytes));
    }
}

impl Task {
    #[cfg(test)]
    pub(super) fn stub() -> Self {
        Task::Stub(StubTask::new())
    }

    #[cfg(test)]
    pub(super) fn stub_for_transport(webtransport: bool, ws_buffered_amount: Option<u64>) -> Self {
        let transport = if webtransport {
            UplinkTransport::WebTransport
        } else {
            UplinkTransport::WebSocket
        };
        Task::Stub(StubTask::for_transport(transport, ws_buffered_amount))
    }

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
        self.send_packet_with_drop_meta(packet, stream_key, None);
    }

    pub fn send_packet_with_drop_meta(
        &self,
        packet: PacketWrapper,
        stream_key: MediaStreamKey,
        meta: Option<FrameDropMeta>,
    ) {
        match self {
            Task::WebSocket(ws) => ws.send_packet(packet, stream_key),
            Task::WebTransport(wt) => wt.send_packet_with_drop_meta(packet, stream_key, meta),
            #[cfg(test)]
            Task::Stub(stub) => {
                let _ = packet;
                stub.record(StubSendKind::Reliable, stream_key);
            }
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
            #[cfg(test)]
            Task::Stub(stub) => {
                let _ = packet;
                stub.record(StubSendKind::Datagram, MediaStreamKey::Control);
            }
        }
    }

    /// WebSocket `bufferedAmount`, or `None` on WebTransport. Deliberately
    pub fn get_send_queue_depth(&self) -> Option<u64> {
        match self {
            Task::WebSocket(ws) => ws.get_buffered_amount(),
            Task::WebTransport(_) => None, // WebTransport doesn't expose bufferedAmount
            #[cfg(test)]
            Task::Stub(stub) => match stub.transport {
                UplinkTransport::WebSocket => stub.buffered_amount.get(),
                UplinkTransport::WebTransport => None,
            },
        }
    }

    /// Bytes queued in the active transport's uplink, for the `send_queue_bytes`
    /// health field (#2722). Both arms are an INSTANTANEOUS GAUGE in bytes, so
    /// one Grafana panel reads correctly for either transport. The WebTransport
    /// arm is a PER-TAB total, not this connection's own: its four counters are
    /// process-global, so during election candidates' #2721 Control probes
    /// contribute and it converges once elected.
    pub fn uplink_queue_depth_bytes(&self) -> Option<u64> {
        match self {
            Task::WebSocket(ws) => {
                uplink_queue_depth_for(UplinkTransport::WebSocket, ws.get_buffered_amount())
            }
            Task::WebTransport(_) => uplink_queue_depth_for(UplinkTransport::WebTransport, None),
            #[cfg(test)]
            Task::Stub(stub) => uplink_queue_depth_for(stub.transport, stub.buffered_amount.get()),
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
            #[cfg(test)]
            Task::Stub(_) => {
                let _ = (bytes, stream_key);
            }
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
            #[cfg(test)]
            Task::Stub(_) => {
                let _ = bytes;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn take_last_send_for_test(&self) -> Option<(StubSendKind, MediaStreamKey)> {
        match self {
            Task::Stub(stub) => stub.take_last_send_for_test(),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn clear_last_send_for_test(&self) {
        if let Task::Stub(stub) = self {
            stub.clear_last_send_for_test();
        }
    }

    #[cfg(test)]
    pub(super) fn set_send_queue_depth_for_test(&self, bytes: u64) {
        if let Task::Stub(stub) = self {
            stub.set_send_queue_depth_for_test(bytes);
        }
    }

    #[cfg(test)]
    pub(super) fn take_sends_for_test(&self) -> Vec<(StubSendKind, MediaStreamKey)> {
        match self {
            Task::Stub(stub) => stub.take_sends_for_test(),
            _ => Vec::new(),
        }
    }
}
