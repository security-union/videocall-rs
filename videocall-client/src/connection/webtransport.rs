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

// This submodule implements our WebMedia trait for WebTransportTask
//
// Sets up all the stream handling to support the callbacks on_connected, on_connection_lost, and
// on_inbound_media
//
use super::connection_lost_reason::ConnectionLostReason;
use super::url_log::strip_query_for_log;
use super::webmedia::{ConnectOptions, MediaStreamKey, WebMedia};
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use js_sys::Uint8Array;
use log::debug;
use log::error;
use log::info;
use log::warn;
use protobuf::Message;
use videocall_transport::webtransport::{
    WebTransportService, WebTransportStatus, WebTransportTask,
};
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::ReadableStreamDefaultReader;
use web_sys::WebTransportBidirectionalStream;
use web_sys::WebTransportReceiveStream;

/// Maximum size for an inbound stream buffer (4 MB), matching the server's MAX_FRAME_SIZE.
/// Prevents a malicious or misbehaving peer from consuming all WASM memory by sending
/// an arbitrarily large stream payload.
const MAX_INBOUND_STREAM_SIZE: usize = 4_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
enum MessageType {
    Datagram,
    UnidirectionalStream,
    BidirectionalStream,
    // Unknown,
}

impl WebMedia<WebTransportTask> for WebTransportTask {
    fn connect(options: ConnectOptions) -> anyhow::Result<WebTransportTask> {
        // Phase 3b: stash the optional netsim shim in the per-tab
        // thread-local before constructing the transport task. See
        // `connection/netsim_hook.rs` for the design notes (Option
        // A — thread-local hook + re-entrancy flag).
        #[cfg(feature = "netsim")]
        super::netsim_hook::install_hook(options.netsim_hook.clone());

        let on_datagram = {
            let callback = options.on_inbound_media.clone();
            Callback::from(move |bytes: Vec<u8>| {
                emit_packet(bytes, MessageType::Datagram, callback.clone())
            })
        };

        let on_unidirectional_stream = {
            let callback = options.on_inbound_media.clone();
            Callback::from(move |stream: WebTransportReceiveStream| {
                handle_unidirectional_stream(stream, callback.clone())
            })
        };

        let on_bidirectional_stream = {
            let callback = options.on_inbound_media.clone();
            Callback::from(move |stream: WebTransportBidirectionalStream| {
                handle_bidirectional_stream(stream, callback.clone())
            })
        };

        let notification = {
            let connected_callback = options.on_connected.clone();
            let connection_lost_callback = options.on_connection_lost.clone();
            Callback::from(move |status| match status {
                WebTransportStatus::Opened => connected_callback.emit(()),
                WebTransportStatus::ClosedBeforeReady(msg) => {
                    connection_lost_callback.emit(ConnectionLostReason::HandshakeFailed(msg));
                }
                WebTransportStatus::ClosedAfterReady(msg) => {
                    connection_lost_callback.emit(ConnectionLostReason::SessionDropped(msg));
                }
                // Legacy variants — these should no longer fire with the updated
                // transport, but keep them as a defensive fallback.
                WebTransportStatus::Closed(e) => {
                    let msg = format!("{e:?}");
                    connection_lost_callback.emit(ConnectionLostReason::SessionDropped(msg));
                }
                WebTransportStatus::Error(e) => {
                    let msg = format!("{e:?}");
                    connection_lost_callback.emit(ConnectionLostReason::SessionDropped(msg));
                }
            })
        };
        info!(
            "WebTransport connecting to {}",
            strip_query_for_log(&options.webtransport_url)
        );
        let task = WebTransportService::connect(
            &options.webtransport_url,
            on_datagram,
            on_unidirectional_stream,
            on_bidirectional_stream,
            notification,
        )?;
        info!("WebTransport connection success");
        Ok(task)
    }

    /// Reliable media-packet send path.
    ///
    /// Phase 2 of the WebTransport freeze fix: every reliable packet rides on
    /// a **persistent** per-media-type QUIC unidirectional stream rather than
    /// opening a fresh stream per packet (~80 streams/sec/sender in the legacy
    /// pattern).  Stream identity is `stream_key.as_u8()`; the server-side
    /// reader at `actix-api/src/webtransport/bridge.rs` reads length-prefixed
    /// frames from each stream in a loop until EOF and routes by the MediaType
    /// inside the encrypted protobuf payload.
    fn send_bytes(&self, bytes: Vec<u8>, stream_key: MediaStreamKey) {
        // Phase 3b: consult the per-tab netsim shim. When the
        // `netsim` feature is off the entire block compiles out and
        // the send path is byte-for-byte identical to pre-3b.
        #[cfg(feature = "netsim")]
        {
            if super::netsim_hook::shape_uplink_reliable(&bytes, stream_key) {
                return;
            }
        }
        WebTransportTask::send_on_persistent_stream(
            self.transport.clone(),
            self.persistent_streams.clone(),
            stream_key.as_u8(),
            bytes,
        );
    }

    fn send_bytes_datagram(&self, bytes: Vec<u8>) {
        use crate::adaptive_quality_constants::DATAGRAM_MAX_SIZE;

        // Phase 3b: consult the per-tab netsim shim. See
        // `send_bytes` above for the no-feature compile-out.
        #[cfg(feature = "netsim")]
        {
            if super::netsim_hook::shape_uplink_datagram(&bytes) {
                return;
            }
        }

        if bytes.len() <= DATAGRAM_MAX_SIZE {
            // Packet fits within the datagram MTU -- send as unreliable datagram
            // for lower latency and no head-of-line blocking.  Datagrams are
            // on a separate primitive from persistent streams and are NOT
            // length-prefix framed.
            WebTransportTask::send_datagram(self.transport.clone(), bytes);
        } else {
            // Packet exceeds datagram size limit (e.g., a keyframe).
            // Fall back to the Control persistent stream so the server's
            // framed reader can receive it as a complete frame without
            // application-layer fragmentation.
            debug!(
                "Packet size {} exceeds datagram MTU {}, falling back to Control persistent stream",
                bytes.len(),
                DATAGRAM_MAX_SIZE
            );
            WebTransportTask::send_on_persistent_stream(
                self.transport.clone(),
                self.persistent_streams.clone(),
                MediaStreamKey::Control.as_u8(),
                bytes,
            );
        }
    }
}

/// Reads from a **persistent length-prefixed unidirectional QUIC stream**
/// (server -> client) and emits each complete frame to `on_inbound_media`.
///
/// The server keeps the stream open and prefixes every packet with a 4-byte
/// big-endian length header (see `actix-api/src/webtransport/bridge.rs::
/// spawn_unistream_writer`).  This reader accumulates chunks across QUIC
/// chunk boundaries and extracts complete `[length][payload]` frames as
/// they arrive, emitting each immediately.
///
/// **Framed-only.** Issue #776: the legacy per-packet "emit raw buffer on
/// done" fallback was removed alongside the rest of PR #772 — both ends of
/// the WebTransport unidirectional path are framed-only.  A truncated
/// frame at EOF (server crash mid-write, or a corrupt-length break) leaves
/// `pending` with bytes that cannot be parsed as a `PacketWrapper`; we
/// drop them on the floor with a warning rather than emit garbage to the
/// decoder.
fn handle_unidirectional_stream(
    stream: WebTransportReceiveStream,
    on_inbound_media: Callback<PacketWrapper>,
) {
    if stream.is_undefined() {
        debug!("stream is undefined");
        return;
    }
    let incoming_unistreams: ReadableStreamDefaultReader = stream.get_reader().unchecked_into();
    let callback = Callback::from(move |d: Vec<u8>| {
        emit_packet(
            d,
            MessageType::UnidirectionalStream,
            on_inbound_media.clone(),
        )
    });
    wasm_bindgen_futures::spawn_local(async move {
        // Buffer for accumulating partial reads across QUIC chunk boundaries.
        // May span multiple length-prefixed frames within a single chunk,
        // or split a single frame across multiple chunks.
        let mut pending: Vec<u8> = Vec::new();

        loop {
            let read_result = JsFuture::from(incoming_unistreams.read()).await;
            match read_result {
                Err(e) => {
                    warn!("Unistream read error: {:?}", e);
                    break;
                }
                Ok(result) => {
                    let done = Reflect::get(&result, &JsString::from("done"))
                        .map(|v| v.unchecked_into::<Boolean>().is_truthy())
                        .unwrap_or(true);

                    if let Ok(value) = Reflect::get(&result, &JsString::from("value")) {
                        if !value.is_undefined() {
                            let chunk: Uint8Array = value.unchecked_into();
                            append_uint8_array_to_vec(&mut pending, &chunk);
                        }
                    }

                    // Try to extract complete length-prefixed frames:
                    //   [4-byte big-endian length][payload of that length]
                    while pending.len() >= 4 {
                        let len =
                            u32::from_be_bytes([pending[0], pending[1], pending[2], pending[3]])
                                as usize;

                        if len == 0 || len > MAX_INBOUND_STREAM_SIZE {
                            // Corrupt or oversized length header on a
                            // framed-only stream — the server should
                            // never emit this.  Drop the rest of the
                            // buffer and stop reading from this stream.
                            error!(
                                "Frame length {} invalid (max {}), dropping framed unistream",
                                len, MAX_INBOUND_STREAM_SIZE
                            );
                            return;
                        }

                        if pending.len() < 4 + len {
                            break; // need more data from the next read
                        }

                        // Extract the complete packet payload, advance the buffer.
                        // drain() avoids a second Vec allocation for the remainder.
                        let packet_data: Vec<u8> = pending.drain(..4 + len).skip(4).collect();
                        callback.emit(packet_data);
                    }

                    if done {
                        // Stream finished.  With both ends framed-only, a
                        // non-empty `pending` here means either (a) the
                        // server crashed mid-frame leaving a truncated
                        // `[len][partial-payload]` on the wire, or (b) the
                        // loop above broke out of frame extraction because
                        // pending.len() < 4 + len for the current header.
                        // In either case the bytes are not a complete
                        // `PacketWrapper`; drop them and log.  Issue #776
                        // removed the legacy "emit raw buffer" fallback —
                        // emitting truncated bytes would only have produced
                        // a downstream parse failure.
                        if !pending.is_empty() {
                            warn!(
                                "Framed unistream EOF with {} unconsumed bytes (truncated frame); dropping",
                                pending.len()
                            );
                        }
                        break;
                    }

                    // Guard against unbounded buffer growth on a persistent
                    // stream that stops yielding valid frames.
                    if pending.len() > MAX_INBOUND_STREAM_SIZE {
                        error!(
                            "Inbound unistream buffer exceeded {} bytes (got {}), dropping stream",
                            MAX_INBOUND_STREAM_SIZE,
                            pending.len()
                        );
                        break;
                    }
                }
            }
        }
    });
}

fn handle_bidirectional_stream(
    stream: WebTransportBidirectionalStream,
    on_inbound_media: Callback<PacketWrapper>,
) {
    debug!("OnBidiStream: {:?}", &stream);
    if stream.is_undefined() {
        debug!("stream is undefined");
        return;
    }
    let readable: ReadableStreamDefaultReader = stream.readable().get_reader().unchecked_into();
    let callback = Callback::from(move |d| {
        emit_packet(
            d,
            MessageType::BidirectionalStream,
            on_inbound_media.clone(),
        )
    });
    wasm_bindgen_futures::spawn_local(async move {
        let mut buffer: Vec<u8> = vec![];
        loop {
            debug!("reading from stream");
            let read_result = JsFuture::from(readable.read()).await;

            match read_result {
                Err(_) => {
                    // Expected when the transport is closed (Drop or network
                    // failure).
                    break;
                }
                Ok(result) => {
                    let done = match Reflect::get(&result, &JsString::from("done")) {
                        Ok(val) => val.unchecked_into::<Boolean>(),
                        Err(e) => {
                            warn!("Failed to read 'done' from bidistream result: {:?}", e);
                            break;
                        }
                    };
                    let value = match Reflect::get(&result, &JsString::from("value")) {
                        Ok(val) => val,
                        Err(e) => {
                            warn!("Failed to read 'value' from bidistream result: {:?}", e);
                            break;
                        }
                    };
                    if !value.is_undefined() {
                        let value: Uint8Array = value.unchecked_into();
                        append_uint8_array_to_vec(&mut buffer, &value);
                        if buffer.len() > MAX_INBOUND_STREAM_SIZE {
                            error!(
                                "Inbound bidistream exceeded {} bytes (got {}), dropping",
                                MAX_INBOUND_STREAM_SIZE,
                                buffer.len()
                            );
                            break;
                        }
                    }
                    if done.is_truthy() {
                        callback.emit(buffer);
                        break;
                    }
                }
            }
        }
        debug!("readable stream closed");
    });
}

fn emit_packet(bytes: Vec<u8>, message_type: MessageType, callback: Callback<PacketWrapper>) {
    match PacketWrapper::parse_from_bytes(&bytes) {
        Ok(media_packet) => callback.emit(media_packet),
        Err(_) => {
            let message_type = format!("{message_type:?}");
            error!("failed to parse media packet {message_type:?}");
        }
    }
}

fn append_uint8_array_to_vec(rust_vec: &mut Vec<u8>, js_array: &Uint8Array) {
    let start = rust_vec.len();
    let len = js_array.length() as usize;
    rust_vec.resize(start + len, 0);
    js_array.copy_to(&mut rust_vec[start..]);
}
