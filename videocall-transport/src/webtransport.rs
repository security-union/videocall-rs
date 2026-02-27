//! A service to connect to a server through the
//! [`WebTransport` Protocol](https://datatracker.ietf.org/doc/draft-ietf-webtrans-overview/).
//!
//! Forked from yew-webtransport (MIT licensed, Copyright (c) 2022 Security Union),
//! adapted to use `videocall_types::Callback` instead of `yew::Callback`.

use anyhow::{anyhow, Error};
use futures::channel::oneshot::channel;
use std::{fmt, rc::Rc};
use thiserror::Error as ThisError;
use videocall_types::Callback;
use wasm_bindgen_futures::JsFuture;

use gloo_console::log;
use js_sys::{Boolean, JsString, Promise, Reflect, Uint8Array};
use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WebTransportCloseInfo, WebTransportDatagramDuplexStream, WebTransportReceiveStream,
    WritableStream,
};

/// Represents formatting errors.
#[derive(Debug, ThisError)]
pub enum FormatError {
    #[error("received text for a binary format")]
    ReceivedTextForBinary,
    #[error("received binary for a text format")]
    ReceivedBinaryForText,
    #[error("trying to encode a binary format as Text")]
    CantEncodeBinaryAsText,
}

/// A representation of a value which can be stored and restored as a text.
pub type Text = Result<String, Error>;

/// A representation of a value which can be stored and restored as a binary.
pub type Binary = Result<Vec<u8>, Error>;

/// The status of a WebTransport connection. Used for status notifications.
#[derive(Clone, Debug, PartialEq)]
pub enum WebTransportStatus {
    /// Fired when a WebTransport connection has opened.
    Opened,
    /// Fired when a WebTransport connection has closed.
    Closed(JsValue),
    /// Fired when a WebTransport connection has failed.
    Error(JsValue),
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum WebTransportError {
    #[error("{0}")]
    CreationError(String),
}

/// A handle to control the WebTransport connection.
#[must_use = "the connection will be closed when the task is dropped"]
pub struct WebTransportTask {
    pub transport: Rc<WebTransport>,
    #[allow(dead_code)]
    notification: Callback<WebTransportStatus>,
    #[allow(dead_code)]
    listeners: [Promise; 2],
}

impl WebTransportTask {
    fn new(
        transport: Rc<WebTransport>,
        notification: Callback<WebTransportStatus>,
        listeners: [Promise; 2],
    ) -> WebTransportTask {
        WebTransportTask {
            transport,
            notification,
            listeners,
        }
    }
}

impl fmt::Debug for WebTransportTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebTransportTask")
    }
}

/// A WebTransport service attached to a user context.
#[derive(Default, Debug)]
pub struct WebTransportService {}

impl WebTransportService {
    /// Connects to a server through a WebTransport connection. Needs callbacks for
    /// datagrams, unidirectional streams, bidirectional streams, and status notifications.
    pub fn connect(
        url: &str,
        on_datagram: Callback<Vec<u8>>,
        on_unidirectional_stream: Callback<WebTransportReceiveStream>,
        on_bidirectional_stream: Callback<WebTransportBidirectionalStream>,
        notification: Callback<WebTransportStatus>,
    ) -> Result<WebTransportTask, WebTransportError> {
        let ConnectCommon(transport, listeners) = Self::connect_common(url, &notification)?;
        let transport = Rc::new(transport);

        Self::start_listening_incoming_datagrams(
            transport.clone(),
            transport.datagrams(),
            on_datagram,
        );
        Self::start_listening_incoming_unidirectional_streams(
            transport.clone(),
            transport.incoming_unidirectional_streams(),
            on_unidirectional_stream,
        );

        Self::start_listening_incoming_bidirectional_streams(
            transport.clone(),
            transport.incoming_bidirectional_streams(),
            on_bidirectional_stream,
        );

        Ok(WebTransportTask::new(transport, notification, listeners))
    }

    fn start_listening_incoming_unidirectional_streams(
        transport: Rc<WebTransport>,
        incoming_streams: ReadableStream,
        callback: Callback<WebTransportReceiveStream>,
    ) {
        let read_result: ReadableStreamDefaultReader =
            incoming_streams.get_reader().unchecked_into();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let read_result = JsFuture::from(read_result.read()).await;
                match read_result {
                    Err(e) => {
                        log!("Failed to read incoming unidirectional streams", &e);
                        let reason = WebTransportCloseInfo::default();
                        reason.set_reason(
                            format!("Failed to read incoming unidirectional streams {e:?}")
                                .as_str(),
                        );
                        transport.close_with_close_info(&reason);
                        break;
                    }
                    Ok(result) => {
                        let done = Reflect::get(&result, &JsString::from("done"))
                            .unwrap()
                            .unchecked_into::<Boolean>();
                        if let Ok(value) = Reflect::get(&result, &JsString::from("value")) {
                            if value.is_undefined() {
                                break;
                            }
                            let value: WebTransportReceiveStream = value.unchecked_into();
                            callback.emit(value);
                        }
                        if done.is_truthy() {
                            break;
                        }
                    }
                }
            }
        });
    }

    fn start_listening_incoming_datagrams(
        transport: Rc<WebTransport>,
        datagrams: WebTransportDatagramDuplexStream,
        callback: Callback<Vec<u8>>,
    ) {
        let incoming_datagrams: ReadableStreamDefaultReader =
            datagrams.readable().get_reader().unchecked_into();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let read_result = JsFuture::from(incoming_datagrams.read()).await;
                match read_result {
                    Err(e) => {
                        let reason = WebTransportCloseInfo::default();
                        reason.set_reason(
                            format!("Failed to read incoming datagrams {e:?}").as_str(),
                        );
                        transport.close_with_close_info(&reason);
                        break;
                    }
                    Ok(result) => {
                        let done = Reflect::get(&result, &JsString::from("done"))
                            .unwrap()
                            .unchecked_into::<Boolean>();
                        if done.is_truthy() {
                            break;
                        }
                        let value: Uint8Array = Reflect::get(&result, &JsString::from("value"))
                            .unwrap()
                            .unchecked_into();
                        process_binary(&value, &callback);
                    }
                }
            }
        });
    }

    fn start_listening_incoming_bidirectional_streams(
        transport: Rc<WebTransport>,
        streams: ReadableStream,
        callback: Callback<WebTransportBidirectionalStream>,
    ) {
        let read_result: ReadableStreamDefaultReader = streams.get_reader().unchecked_into();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let read_result = JsFuture::from(read_result.read()).await;
                match read_result {
                    Err(e) => {
                        let reason = WebTransportCloseInfo::default();
                        reason.set_reason(
                            format!("Failed to read incoming bidirectional streams {e:?}").as_str(),
                        );
                        transport.close_with_close_info(&reason);
                        break;
                    }
                    Ok(result) => {
                        let done = Reflect::get(&result, &JsString::from("done"))
                            .unwrap()
                            .unchecked_into::<Boolean>();
                        if let Ok(value) = Reflect::get(&result, &JsString::from("value")) {
                            if value.is_undefined() {
                                break;
                            }
                            let value: WebTransportBidirectionalStream = value.unchecked_into();
                            callback.emit(value);
                        }
                        if done.is_truthy() {
                            break;
                        }
                    }
                }
            }
        });
    }

    fn connect_common(
        url: &str,
        notification: &Callback<WebTransportStatus>,
    ) -> Result<ConnectCommon, WebTransportError> {
        let transport = WebTransport::new(url);
        let transport = transport.map_err(|e| {
            WebTransportError::CreationError(format!("Failed to create WebTransport: {e:?}"))
        })?;

        let notify = notification.clone();

        let opened_closure = Closure::wrap(Box::new(move |_| {
            notify.emit(WebTransportStatus::Opened);
        }) as Box<dyn FnMut(JsValue)>);
        let notify = notification.clone();
        let closed_closure = Closure::wrap(Box::new(move |e: JsValue| {
            notify.emit(WebTransportStatus::Closed(e));
        }) as Box<dyn FnMut(JsValue)>);
        let ready = transport
            .ready()
            .then(&opened_closure)
            .catch(&closed_closure);
        let closed = transport
            .closed()
            .then(&closed_closure)
            .catch(&closed_closure);
        opened_closure.forget();
        closed_closure.forget();

        {
            let listeners = [ready, closed];
            Ok(ConnectCommon(transport, listeners))
        }
    }
}
struct ConnectCommon(WebTransport, [Promise; 2]);

pub fn process_binary(bytes: &Uint8Array, callback: &Callback<Vec<u8>>) {
    let data = bytes.to_vec();
    callback.emit(data);
}

impl WebTransportTask {
    /// Sends data to a WebTransport connection.
    pub fn send_datagram(transport: Rc<WebTransport>, data: Vec<u8>) {
        wasm_bindgen_futures::spawn_local(async move {
            let transport = transport.clone();
            let result: Result<(), anyhow::Error> = {
                let transport = transport.clone();
                async move {
                    let stream = transport.datagrams();
                    let stream: WritableStream = stream.writable();
                    if stream.locked() {
                        return Err(anyhow::anyhow!("Stream is locked"));
                    }
                    let writer = stream.get_writer().map_err(|e| anyhow!("{:?}", e))?;
                    let data = Uint8Array::from(data.as_slice());
                    JsFuture::from(writer.ready())
                        .await
                        .map_err(|e| anyhow!("{:?}", e))?;
                    JsFuture::from(writer.write_with_chunk(&data))
                        .await
                        .map_err(|e| anyhow!("{:?}", e))?;
                    writer.release_lock();
                    Ok(())
                }
            }
            .await;
            if let Err(e) = result {
                let e = e.to_string();
                log!("error: ", e);
                transport.close();
            }
        });
    }

    pub fn send_unidirectional_stream(transport: Rc<WebTransport>, data: Vec<u8>) {
        wasm_bindgen_futures::spawn_local(async move {
            let transport = transport.clone();
            let result: Result<(), anyhow::Error> = {
                let transport = transport.clone();
                async move {
                    let _ = JsFuture::from(transport.ready())
                        .await
                        .map_err(|e| anyhow!("{:?}", e))?;
                    let stream = JsFuture::from(transport.create_unidirectional_stream()).await;
                    let stream: WritableStream = stream
                        .map_err(|e| anyhow!("failed to create Writeable stream {:?}", e))?
                        .unchecked_into();
                    let writer = stream
                        .get_writer()
                        .map_err(|e| anyhow!("Error getting writer {:?}", e))?;
                    let data = Uint8Array::from(data.as_slice());
                    JsFuture::from(writer.ready())
                        .await
                        .map_err(|e| anyhow!("Error getting writer ready {:?}", e))?;
                    let _ = JsFuture::from(writer.write_with_chunk(&data))
                        .await
                        .map_err(|e| anyhow::anyhow!("Error writing to stream: {:?}", e))?;
                    writer.release_lock();
                    JsFuture::from(stream.close())
                        .await
                        .map_err(|e| anyhow::anyhow!("Error closing stream {:?}", e))?;
                    Ok(())
                }
            }
            .await;
            if let Err(e) = result {
                let e = e.to_string();
                log!("error: ", e);
                transport.close();
            }
        });
    }

    pub fn send_bidirectional_stream(
        transport: Rc<WebTransport>,
        data: Vec<u8>,
        callback: Callback<Vec<u8>>,
    ) {
        wasm_bindgen_futures::spawn_local(async move {
            let transport = transport.clone();
            let result: Result<(), anyhow::Error> = {
                let transport = transport.clone();
                async move {
                    let stream = JsFuture::from(transport.create_bidirectional_stream()).await;
                    let stream: WebTransportBidirectionalStream =
                        stream.map_err(|e| anyhow!("{:?}", e))?.unchecked_into();
                    let readable: ReadableStreamDefaultReader =
                        stream.readable().get_reader().unchecked_into();
                    let (sender, receiver) = channel();
                    wasm_bindgen_futures::spawn_local(async move {
                        loop {
                            let read_result = JsFuture::from(readable.read()).await;
                            match read_result {
                                Err(e) => {
                                    let reason = WebTransportCloseInfo::default();
                                    reason.set_reason(
                                        format!("Failed to read incoming stream {e:?}").as_str(),
                                    );
                                    transport.close_with_close_info(&reason);
                                    break;
                                }
                                Ok(result) => {
                                    let done = Reflect::get(&result, &JsString::from("done"))
                                        .unwrap()
                                        .unchecked_into::<Boolean>();
                                    if done.is_truthy() {
                                        break;
                                    }
                                    let value: Uint8Array =
                                        Reflect::get(&result, &JsString::from("value"))
                                            .unwrap()
                                            .unchecked_into();
                                    process_binary(&value, &callback);
                                }
                            }
                        }
                        let _ = sender.send(true);
                    });
                    let writer = stream
                        .writable()
                        .get_writer()
                        .map_err(|e| anyhow!("{:?}", e))?;

                    JsFuture::from(writer.ready())
                        .await
                        .map_err(|e| anyhow!("{:?}", e))?;
                    let data = Uint8Array::from(data.as_slice());
                    let _ = JsFuture::from(writer.write_with_chunk(&data))
                        .await
                        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
                    JsFuture::from(writer.close())
                        .await
                        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
                    let _ = receiver.await?;
                    Ok(())
                }
            }
            .await;
            if let Err(e) = result {
                let e = e.to_string();
                log!("error: {}", e);
                transport.close();
            }
        });
    }
}
