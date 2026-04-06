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
///
/// When dropped, the underlying `WebTransport` is closed, which causes all
/// reader loops (datagrams, unidirectional, bidirectional) to terminate because
/// their `reader.read()` futures resolve with errors on a closed transport.
#[must_use = "the connection will be closed when the task is dropped"]
pub struct WebTransportTask {
    pub transport: Rc<WebTransport>,
    #[allow(dead_code)]
    notification: Callback<WebTransportStatus>,
    #[allow(dead_code)]
    listeners: [Promise; 2],
    /// Stored so the closures live as long as the task and are properly dropped
    /// instead of being leaked via `forget()`. The closed closure is wrapped in
    /// `Rc` because it is shared across multiple promise chains (`ready.catch`,
    /// `closed.then`, `closed.catch`).
    #[allow(dead_code)]
    opened_closure: Closure<dyn FnMut(JsValue)>,
    #[allow(dead_code)]
    closed_closure: Rc<Closure<dyn FnMut(JsValue)>>,
}

impl WebTransportTask {
    fn new(
        transport: Rc<WebTransport>,
        notification: Callback<WebTransportStatus>,
        listeners: [Promise; 2],
        opened_closure: Closure<dyn FnMut(JsValue)>,
        closed_closure: Rc<Closure<dyn FnMut(JsValue)>>,
    ) -> WebTransportTask {
        WebTransportTask {
            transport,
            notification,
            listeners,
            opened_closure,
            closed_closure,
        }
    }
}

impl Drop for WebTransportTask {
    fn drop(&mut self) {
        // Close the underlying WebTransport session. This causes the reader
        // loops (datagrams, unidirectional streams, bidirectional streams) to
        // break out of their `reader.read()` await — the futures resolve with
        // errors on a closed transport, allowing the spawn_local tasks and
        // their captured Rc<WebTransport> clones to be cleaned up.
        self.transport.close();
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
        let ConnectCommon(transport, listeners, opened_closure, closed_closure) =
            Self::connect_common(url, &notification)?;
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

        Ok(WebTransportTask::new(
            transport,
            notification,
            listeners,
            opened_closure,
            closed_closure,
        ))
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
                        let done = match Reflect::get(&result, &JsString::from("done")) {
                            Ok(val) => val.unchecked_into::<Boolean>(),
                            Err(e) => {
                                log!(
                                    "Failed to read 'done' from unidirectional stream result",
                                    &e
                                );
                                break;
                            }
                        };
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
                        let done = match Reflect::get(&result, &JsString::from("done")) {
                            Ok(val) => val.unchecked_into::<Boolean>(),
                            Err(e) => {
                                log!("Failed to read 'done' from datagram result", &e);
                                break;
                            }
                        };
                        if done.is_truthy() {
                            break;
                        }
                        let value: Uint8Array =
                            match Reflect::get(&result, &JsString::from("value")) {
                                Ok(val) => val.unchecked_into(),
                                Err(e) => {
                                    log!("Failed to read 'value' from datagram result", &e);
                                    break;
                                }
                            };
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
                        let done = match Reflect::get(&result, &JsString::from("done")) {
                            Ok(val) => val.unchecked_into::<Boolean>(),
                            Err(e) => {
                                log!("Failed to read 'done' from bidirectional stream result", &e);
                                break;
                            }
                        };
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

        // Both closures are stored in the WebTransportTask struct so they are
        // dropped when the task is dropped, instead of being leaked via
        // `forget()`. Previously, every reconnection/re-election cycle would
        // permanently leak two closures into WASM linear memory.
        let opened_closure = Closure::wrap(Box::new(move |_: JsValue| {
            notify.emit(WebTransportStatus::Opened);
        }) as Box<dyn FnMut(JsValue)>);
        let notify = notification.clone();
        // `closed_closure` is shared via `Rc` because it is referenced by
        // multiple promise chains (`ready.catch`, `closed.then`, `closed.catch`).
        let closed_closure = Rc::new(Closure::wrap(Box::new(move |e: JsValue| {
            notify.emit(WebTransportStatus::Closed(e));
        }) as Box<dyn FnMut(JsValue)>));
        let ready = transport
            .ready()
            .then(&opened_closure)
            .catch(&closed_closure);
        let closed = transport
            .closed()
            .then(&closed_closure)
            .catch(&closed_closure);

        {
            let listeners = [ready, closed];
            Ok(ConnectCommon(
                transport,
                listeners,
                opened_closure,
                closed_closure,
            ))
        }
    }
}
struct ConnectCommon(
    WebTransport,
    [Promise; 2],
    Closure<dyn FnMut(JsValue)>,
    Rc<Closure<dyn FnMut(JsValue)>>,
);

pub fn process_binary(bytes: &Uint8Array, callback: &Callback<Vec<u8>>) {
    let data = bytes.to_vec();
    callback.emit(data);
}

impl WebTransportTask {
    /// Sends data to a WebTransport connection via datagram.
    ///
    /// Datagrams are unreliable and expendable by design (heartbeats, RTT probes,
    /// diagnostics). If the writable side is already locked by a concurrent write,
    /// the packet is silently dropped instead of killing the entire transport
    /// connection. Only fatal errors (transport closed, write failure after
    /// acquiring the lock) close the transport.
    pub fn send_datagram(transport: Rc<WebTransport>, data: Vec<u8>) {
        wasm_bindgen_futures::spawn_local(async move {
            let stream = transport.datagrams();
            let writable: WritableStream = stream.writable();
            if writable.locked() {
                log!("datagram dropped (stream busy)");
                return;
            }
            let writer = match writable.get_writer() {
                Ok(w) => w,
                Err(e) => {
                    log!("error: ", format!("{e:?}"));
                    transport.close();
                    return;
                }
            };
            let data = Uint8Array::from(data.as_slice());
            let result = match JsFuture::from(writer.ready()).await {
                Ok(_) => JsFuture::from(writer.write_with_chunk(&data)).await,
                err => err,
            };
            writer.release_lock();
            if let Err(e) = result {
                log!(
                    "datagram write failed, closing transport:",
                    format!("{e:?}")
                );
                transport.close();
            }
        });
    }

    /// Sends data to a WebTransport connection via a unidirectional stream.
    ///
    /// Stream errors (creation failure, write backpressure, QUIC congestion) are
    /// transient -- they affect only this single frame send. The transport is NOT
    /// closed on failure; if the transport is genuinely dead, the reader loops and
    /// the `closed` promise will detect it independently.
    pub fn send_unidirectional_stream(transport: Rc<WebTransport>, data: Vec<u8>) {
        wasm_bindgen_futures::spawn_local(async move {
            let result: Result<(), anyhow::Error> = async {
                JsFuture::from(transport.ready())
                    .await
                    .map_err(|e| anyhow!("{:?}", e))?;
                let stream: WritableStream =
                    JsFuture::from(transport.create_unidirectional_stream())
                        .await
                        .map_err(|e| anyhow!("failed to create Writeable stream {:?}", e))?
                        .unchecked_into();
                let writer = stream
                    .get_writer()
                    .map_err(|e| anyhow!("Error getting writer {:?}", e))?;
                let data = Uint8Array::from(data.as_slice());
                JsFuture::from(writer.ready())
                    .await
                    .map_err(|e| anyhow!("Error getting writer ready {:?}", e))?;
                JsFuture::from(writer.write_with_chunk(&data))
                    .await
                    .map_err(|e| anyhow!("Error writing to stream: {:?}", e))?;
                writer.release_lock();
                JsFuture::from(stream.close())
                    .await
                    .map_err(|e| anyhow!("Error closing stream {:?}", e))?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                // Transient stream error -- log and drop the packet. Do NOT
                // close the transport; a single failed frame should not kill
                // the entire connection for all participants.
                log!(
                    "unidirectional stream send failed (frame dropped):",
                    e.to_string()
                );
            }
        });
    }

    /// Sends data to a WebTransport connection via a bidirectional stream and
    /// reads the response.
    ///
    /// Stream errors are transient -- they affect only this single stream
    /// exchange. The transport is NOT closed on failure; if the transport is
    /// genuinely dead, the reader loops and the `closed` promise will detect it
    /// independently. The inner reader task will terminate naturally when the
    /// stream's readable side ends or errors out.
    pub fn send_bidirectional_stream(
        transport: Rc<WebTransport>,
        data: Vec<u8>,
        callback: Callback<Vec<u8>>,
    ) {
        wasm_bindgen_futures::spawn_local(async move {
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
                                    // Stream read error -- log and stop reading.
                                    // Do NOT close the transport; this is a
                                    // single-stream failure.
                                    log!(
                                        "bidirectional stream read error (stopping reader):",
                                        format!("{e:?}")
                                    );
                                    break;
                                }
                                Ok(result) => {
                                    let done =
                                        match Reflect::get(&result, &JsString::from("done")) {
                                            Ok(val) => val.unchecked_into::<Boolean>(),
                                            Err(e) => {
                                                log!(
                                                    "Failed to read 'done' from bidi send reader result",
                                                    &e
                                                );
                                                break;
                                            }
                                        };
                                    if done.is_truthy() {
                                        break;
                                    }
                                    let value: Uint8Array =
                                        match Reflect::get(&result, &JsString::from("value")) {
                                            Ok(val) => val.unchecked_into(),
                                            Err(e) => {
                                                log!(
                                                    "Failed to read 'value' from bidi send reader result",
                                                    &e
                                                );
                                                break;
                                            }
                                        };
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
                // Transient stream error -- log and drop the packet. Do NOT
                // close the transport; a single failed frame should not kill
                // the entire connection for all participants. The inner reader
                // task (if spawned) will terminate when the stream ends.
                log!(
                    "bidirectional stream send failed (frame dropped):",
                    e.to_string()
                );
            }
        });
    }
}
