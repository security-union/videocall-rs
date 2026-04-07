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
use super::webmedia::{ConnectOptions, WebMedia};
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
use web_sys::WebTransportCloseInfo;
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
                WebTransportStatus::Closed(error) => connection_lost_callback.emit(error),
                WebTransportStatus::Error(error) => connection_lost_callback.emit(error),
            })
        };
        info!("WebTransport connecting to {}", &options.webtransport_url);
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

    fn send_bytes(&self, bytes: Vec<u8>) {
        WebTransportTask::send_unidirectional_stream(self.transport.clone(), bytes);
    }

    fn send_bytes_datagram(&self, bytes: Vec<u8>) {
        use crate::adaptive_quality_constants::DATAGRAM_MAX_SIZE;

        if bytes.len() <= DATAGRAM_MAX_SIZE {
            // Packet fits within the datagram MTU -- send as unreliable datagram
            // for lower latency and no head-of-line blocking.
            WebTransportTask::send_datagram(self.transport.clone(), bytes);
        } else {
            // Packet exceeds datagram size limit (e.g., a keyframe).
            // Fall back to a per-packet unidirectional stream so the server's
            // `read_to_end` can receive it as a complete frame.
            debug!(
                "Packet size {} exceeds datagram MTU {}, falling back to unistream",
                bytes.len(),
                DATAGRAM_MAX_SIZE
            );
            WebTransportTask::send_unidirectional_stream(self.transport.clone(), bytes);
        }
    }
}

/// Reads from a unidirectional QUIC stream, handling two framing modes:
///
/// 1. **Legacy per-packet streams**: The sender opens a stream, writes one
///    packet, and closes the stream.  `done` becomes truthy after the first
///    (or only) read and any buffered data is emitted as a single packet.
///
/// 2. **Persistent length-prefixed streams** (server -> client): The server
///    keeps the stream open and prefixes every packet with a 4-byte big-endian
///    length header.  The reader accumulates chunks and extracts complete
///    `[length][payload]` frames as they arrive, emitting each immediately.
///
/// The two modes are distinguished at runtime: if we can extract at least one
/// complete length-prefixed frame from the buffer we stay in framed mode;
/// otherwise, when `done` is signalled we fall back to emitting the raw buffer
/// (legacy mode).
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
        // For per-packet (legacy) streams this typically holds a single chunk.
        // For persistent streams it may span multiple length-prefixed frames.
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
                            // Corrupt or oversized frame -- not a framed stream.
                            // This can happen on a legacy per-packet stream whose
                            // first 4 bytes happen to decode to a bad length.
                            // We will emit the raw buffer when `done` fires.
                            error!(
                                "Frame length {} invalid (max {}), treating as non-framed stream",
                                len, MAX_INBOUND_STREAM_SIZE
                            );
                            break;
                        }

                        if pending.len() < 4 + len {
                            break; // need more data from the next read
                        }

                        // Extract the complete packet payload, advance the buffer.
                        let packet_data = pending[4..4 + len].to_vec();
                        pending = pending[4 + len..].to_vec();
                        callback.emit(packet_data);
                    }

                    if done {
                        // Stream finished. Any remaining data in `pending` is
                        // from a legacy per-packet stream (single packet, no
                        // length prefix) -- emit it as-is.
                        if !pending.is_empty() {
                            callback.emit(std::mem::take(&mut pending));
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
                Err(e) => {
                    let reason = WebTransportCloseInfo::default();
                    reason.set_reason(format!("Failed to read incoming bidistream {e:?}").as_str());
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
    // Convert the Uint8Array into a Vec<u8>
    let mut temp_vec = vec![0; js_array.length() as usize];
    js_array.copy_to(&mut temp_vec);

    // Append it to the existing Rust Vec<u8>
    rust_vec.append(&mut temp_vec);
}
