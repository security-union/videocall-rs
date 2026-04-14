//! A service to connect to a server through the
//! [`WebSocket` Protocol](https://tools.ietf.org/html/rfc6455).
//!
//! Forked from yew-websocket (MIT licensed, Copyright (c) 2017 Denis Kolodin),
//! adapted to use `videocall_types::Callback` instead of `yew::Callback`.

use anyhow::Error;
use log::warn;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error as ThisError;
use videocall_types::Callback;

/// Maximum allowed buffered bytes before dropping outbound packets.
/// When the browser's WebSocket send buffer exceeds this threshold, new sends
/// are silently dropped to prevent unbounded memory growth on slow networks.
/// 1 MB matches the congestion-drop behavior used on the WebTransport path.
const MAX_BUFFERED_AMOUNT: u32 = 1_048_576;

/// Cumulative count of packets dropped because the WebSocket send buffer exceeded the threshold.
static WEBSOCKET_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Returns the total number of WebSocket packets dropped due to backpressure since process start.
pub fn websocket_drop_count() -> u64 {
    WEBSOCKET_DROP_COUNT.load(Ordering::Relaxed)
}

use gloo::events::EventListener;
use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

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

/// The status of a WebSocket connection. Used for status notifications.
#[derive(Clone, Debug, PartialEq)]
pub enum WebSocketStatus {
    /// Fired when a WebSocket connection has opened.
    Opened,
    /// Fired when a WebSocket connection has closed.
    ///
    /// Contains an optional `(code, reason)` tuple extracted from the
    /// browser's `CloseEvent`. Well-known codes include:
    /// - 1000: normal closure
    /// - 1006: abnormal closure (network failure, no close frame received)
    /// - 1008: policy violation (e.g. expired JWT)
    /// - 1013: try again later (server overload)
    /// - 4000+: application-specific codes
    Closed(Option<(u16, String)>),
    /// Fired when a WebSocket connection has failed.
    Error,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum WebSocketError {
    #[error("{0}")]
    CreationError(String),
}

/// A handle to control the WebSocket connection.
#[must_use = "the connection will be closed when the task is dropped"]
pub struct WebSocketTask {
    ws: WebSocket,
    notification: Callback<WebSocketStatus>,
    #[allow(dead_code)]
    listeners: [EventListener; 4],
}

impl WebSocketTask {
    fn new(
        ws: WebSocket,
        notification: Callback<WebSocketStatus>,
        listener_0: EventListener,
        listeners: [EventListener; 3],
    ) -> WebSocketTask {
        let [listener_1, listener_2, listener_3] = listeners;
        WebSocketTask {
            ws,
            notification,
            listeners: [listener_0, listener_1, listener_2, listener_3],
        }
    }
}

impl fmt::Debug for WebSocketTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebSocketTask")
    }
}

/// A WebSocket service attached to a user context.
#[derive(Default, Debug)]
pub struct WebSocketService {}

impl WebSocketService {
    /// Connects to a server through a WebSocket connection. Needs two callbacks; one is passed
    /// data, the other is passed updates about the WebSocket's status.
    pub fn connect<OUT>(
        url: &str,
        callback: Callback<OUT>,
        notification: Callback<WebSocketStatus>,
    ) -> Result<WebSocketTask, WebSocketError>
    where
        OUT: From<Text> + From<Binary> + 'static,
    {
        let ConnectCommon(ws, listeners) = Self::connect_common(url, &notification)?;
        let listener = EventListener::new(&ws, "message", move |event: &Event| {
            let event = event.dyn_ref::<MessageEvent>().unwrap();
            process_both(event, &callback);
        });
        Ok(WebSocketTask::new(ws, notification, listener, listeners))
    }

    /// Connects to a server through a WebSocket connection, like connect,
    /// but only processes binary frames. Text frames are silently ignored.
    pub fn connect_binary<OUT>(
        url: &str,
        callback: Callback<OUT>,
        notification: Callback<WebSocketStatus>,
    ) -> Result<WebSocketTask, WebSocketError>
    where
        OUT: From<Binary> + 'static,
    {
        let ConnectCommon(ws, listeners) = Self::connect_common(url, &notification)?;
        let listener = EventListener::new(&ws, "message", move |event: &Event| {
            let event = event.dyn_ref::<MessageEvent>().unwrap();
            process_binary(event, &callback);
        });
        Ok(WebSocketTask::new(ws, notification, listener, listeners))
    }

    /// Connects to a server through a WebSocket connection, like connect,
    /// but only processes text frames. Binary frames are silently ignored.
    pub fn connect_text<OUT>(
        url: &str,
        callback: Callback<OUT>,
        notification: Callback<WebSocketStatus>,
    ) -> Result<WebSocketTask, WebSocketError>
    where
        OUT: From<Text> + 'static,
    {
        let ConnectCommon(ws, listeners) = Self::connect_common(url, &notification)?;
        let listener = EventListener::new(&ws, "message", move |event: &Event| {
            let event = event.dyn_ref::<MessageEvent>().unwrap();
            process_text(event, &callback);
        });
        Ok(WebSocketTask::new(ws, notification, listener, listeners))
    }

    fn connect_common(
        url: &str,
        notification: &Callback<WebSocketStatus>,
    ) -> Result<ConnectCommon, WebSocketError> {
        let ws = WebSocket::new(url);

        let ws = ws.map_err(|ws_error| {
            WebSocketError::CreationError(
                ws_error
                    .unchecked_into::<js_sys::Error>()
                    .to_string()
                    .as_string()
                    .unwrap(),
            )
        })?;

        ws.set_binary_type(BinaryType::Arraybuffer);
        let notify = notification.clone();
        let listener_open = move |_: &Event| {
            notify.emit(WebSocketStatus::Opened);
        };
        let notify = notification.clone();
        let listener_close = move |event: &Event| {
            // Downcast to CloseEvent to extract the close code and reason.
            // The browser always fires a CloseEvent for the "close" event on
            // a WebSocket, but we guard with `dyn_ref` in case of unexpected
            // environments.
            let close_info = event.dyn_ref::<CloseEvent>().map(|ce| {
                let code = ce.code();
                let reason = ce.reason();
                warn!(
                    "WebSocket closed: code={}, reason={:?}, was_clean={}",
                    code,
                    reason,
                    ce.was_clean()
                );
                (code, reason)
            });
            if close_info.is_none() {
                warn!("WebSocket closed: could not extract CloseEvent details");
            }
            notify.emit(WebSocketStatus::Closed(close_info));
        };
        let notify = notification.clone();
        let listener_error = move |_: &Event| {
            notify.emit(WebSocketStatus::Error);
        };
        {
            let listeners = [
                EventListener::new(&ws, "open", listener_open),
                EventListener::new(&ws, "close", listener_close),
                EventListener::new(&ws, "error", listener_error),
            ];
            Ok(ConnectCommon(ws, listeners))
        }
    }
}

struct ConnectCommon(WebSocket, [EventListener; 3]);

fn process_binary<OUT>(event: &MessageEvent, callback: &Callback<OUT>)
where
    OUT: From<Binary> + 'static,
{
    let bytes = if !event.data().is_string() {
        Some(event.data())
    } else {
        None
    };

    let data = if let Some(bytes) = bytes {
        let bytes: Vec<u8> = Uint8Array::new(&bytes).to_vec();
        Ok(bytes)
    } else {
        Err(FormatError::ReceivedTextForBinary.into())
    };

    let out = OUT::from(data);
    callback.emit(out);
}

fn process_text<OUT>(event: &MessageEvent, callback: &Callback<OUT>)
where
    OUT: From<Text> + 'static,
{
    let text = event.data().as_string();

    let data = if let Some(text) = text {
        Ok(text)
    } else {
        Err(FormatError::ReceivedBinaryForText.into())
    };

    let out = OUT::from(data);
    callback.emit(out);
}

fn process_both<OUT>(event: &MessageEvent, callback: &Callback<OUT>)
where
    OUT: From<Text> + From<Binary> + 'static,
{
    let is_text = event.data().is_string();
    if is_text {
        process_text(event, callback);
    } else {
        process_binary(event, callback);
    }
}

impl WebSocketTask {
    /// Returns the number of bytes queued in the browser's WebSocket send buffer.
    pub fn buffered_amount(&self) -> u32 {
        self.ws.buffered_amount()
    }

    /// Get the amount of data in bytes queued to be transmitted (bufferedAmount)
    pub fn get_buffered_amount(&self) -> Option<u64> {
        Some(self.ws.buffered_amount() as u64)
    }

    /// Sends data to a WebSocket connection.
    pub fn send(&mut self, data: String) {
        if !self.is_active() {
            return;
        }
        if self.ws.send_with_str(&data).is_err() {
            // Only emit Error if the socket is no longer open. A transient
            // send failure while OPEN (e.g. GC pause, tab backgrounding) should
            // not cascade into a full disconnect — the browser's own `error`
            // and `close` event listeners will fire if the connection truly dies.
            if self.ws.ready_state() != WebSocket::OPEN {
                self.notification.emit(WebSocketStatus::Error);
            } else {
                warn!("WebSocket send_with_str failed but socket still OPEN; dropping packet");
            }
        }
    }

    /// Sends binary data to a WebSocket connection.
    ///
    /// If the browser's send buffer already exceeds [`MAX_BUFFERED_AMOUNT`],
    /// the packet is silently dropped to prevent unbounded memory growth on
    /// slow networks. This mirrors the congestion-drop behavior used on the
    /// WebTransport datagram path.
    pub fn send_binary(&self, data: Vec<u8>) {
        if !self.is_active() {
            return;
        }
        let buffered = self.ws.buffered_amount();
        if buffered > MAX_BUFFERED_AMOUNT {
            WEBSOCKET_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            warn!(
                "WebSocket backpressure: dropping {} byte packet (buffered: {} bytes, threshold: {} bytes)",
                data.len(),
                buffered,
                MAX_BUFFERED_AMOUNT,
            );
            return;
        }

        if self.ws.send_with_u8_array(&data).is_err() {
            // Only emit Error if the socket is no longer open. A transient
            // send failure while OPEN (e.g. GC pause, tab backgrounding on iOS)
            // should not cascade into a full disconnect — the browser's own
            // `error` and `close` event listeners will fire if the connection
            // truly dies.
            if self.ws.ready_state() != WebSocket::OPEN {
                self.notification.emit(WebSocketStatus::Error);
            } else {
                warn!(
                    "WebSocket send_with_u8_array failed but socket still OPEN; dropping {} byte packet",
                    data.len()
                );
            }
        }
    }
}

impl WebSocketTask {
    fn is_active(&self) -> bool {
        matches!(
            self.ws.ready_state(),
            WebSocket::CONNECTING | WebSocket::OPEN
        )
    }
}

impl Drop for WebSocketTask {
    fn drop(&mut self) {
        if self.is_active() {
            self.ws.close().ok();
        }
    }
}
