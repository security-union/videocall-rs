//! A service to connect to a server through the
//! [`WebSocket` Protocol](https://tools.ietf.org/html/rfc6455).
//!
//! Forked from yew-websocket (MIT licensed, Copyright (c) 2017 Denis Kolodin),
//! adapted to use `videocall_types::Callback` instead of `yew::Callback`.

use anyhow::Error;
use std::fmt;
use thiserror::Error as ThisError;
use videocall_types::Callback;

use gloo::events::EventListener;
use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, Event, MessageEvent, WebSocket};

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
    Closed,
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
    pub fn connect<OUT: 'static>(
        url: &str,
        callback: Callback<OUT>,
        notification: Callback<WebSocketStatus>,
    ) -> Result<WebSocketTask, WebSocketError>
    where
        OUT: From<Text> + From<Binary>,
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
    pub fn connect_binary<OUT: 'static>(
        url: &str,
        callback: Callback<OUT>,
        notification: Callback<WebSocketStatus>,
    ) -> Result<WebSocketTask, WebSocketError>
    where
        OUT: From<Binary>,
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
    pub fn connect_text<OUT: 'static>(
        url: &str,
        callback: Callback<OUT>,
        notification: Callback<WebSocketStatus>,
    ) -> Result<WebSocketTask, WebSocketError>
    where
        OUT: From<Text>,
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
        let listener_close = move |_: &Event| {
            notify.emit(WebSocketStatus::Closed);
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

fn process_binary<OUT: 'static>(event: &MessageEvent, callback: &Callback<OUT>)
where
    OUT: From<Binary>,
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

fn process_text<OUT: 'static>(event: &MessageEvent, callback: &Callback<OUT>)
where
    OUT: From<Text>,
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

fn process_both<OUT: 'static>(event: &MessageEvent, callback: &Callback<OUT>)
where
    OUT: From<Text> + From<Binary>,
{
    let is_text = event.data().is_string();
    if is_text {
        process_text(event, callback);
    } else {
        process_binary(event, callback);
    }
}

impl WebSocketTask {
    /// Sends data to a WebSocket connection.
    pub fn send(&mut self, data: String) {
        let result = self.ws.send_with_str(&data);

        if result.is_err() {
            self.notification.emit(WebSocketStatus::Error);
        }
    }

    /// Sends binary data to a WebSocket connection.
    pub fn send_binary(&self, data: Vec<u8>) {
        let result = self.ws.send_with_u8_array(&data);

        if result.is_err() {
            self.notification.emit(WebSocketStatus::Error);
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
