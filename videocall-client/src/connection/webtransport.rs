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
use protobuf::Message;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::ReadableStreamDefaultReader;
use web_sys::WebTransportBidirectionalStream;
use web_sys::WebTransportCloseInfo;
use web_sys::WebTransportReceiveStream;
use yew::prelude::Callback;
use yew_webtransport::webtransport::{WebTransportService, WebTransportStatus, WebTransportTask};

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
}

fn handle_unidirectional_stream(
    stream: WebTransportReceiveStream,
    on_inbound_media: Callback<PacketWrapper>,
) {
    if stream.is_undefined() {
        debug!("stream is undefined");
        return;
    }
    let incoming_unistreams: ReadableStreamDefaultReader = stream.get_reader().unchecked_into();
    let callback = Callback::from(move |d| {
        emit_packet(
            d,
            MessageType::UnidirectionalStream,
            on_inbound_media.clone(),
        )
    });
    wasm_bindgen_futures::spawn_local(async move {
        let mut buffer: Vec<u8> = vec![];
        loop {
            let read_result = JsFuture::from(incoming_unistreams.read()).await;
            match read_result {
                Err(e) => {
                    let reason = WebTransportCloseInfo::default();
                    reason.set_reason(format!("Failed to read incoming unistream {e:?}").as_str());
                    break;
                }
                Ok(result) => {
                    let done = Reflect::get(&result, &JsString::from("done"))
                        .unwrap()
                        .unchecked_into::<Boolean>();

                    let value = Reflect::get(&result, &JsString::from("value")).unwrap();
                    if !value.is_undefined() {
                        let value: Uint8Array = value.unchecked_into();
                        append_uint8_array_to_vec(&mut buffer, &value);
                    }

                    if done.is_truthy() {
                        callback.emit(buffer);
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
                    let done = Reflect::get(&result, &JsString::from("done"))
                        .unwrap()
                        .unchecked_into::<Boolean>();
                    let value = Reflect::get(&result, &JsString::from("value")).unwrap();
                    if !value.is_undefined() {
                        let value: Uint8Array = value.unchecked_into();
                        append_uint8_array_to_vec(&mut buffer, &value);
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
