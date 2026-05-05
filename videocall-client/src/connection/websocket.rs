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
use super::webmedia::{ConnectOptions, WebMedia};
use log::debug;
use std::cell::Cell;
use std::rc::Rc;
use videocall_transport::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};
use videocall_types::Callback;

impl WebMedia<WebSocketTask> for WebSocketTask {
    fn connect(options: ConnectOptions) -> anyhow::Result<WebSocketTask> {
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
        debug!("WebSocket connecting to {}", &options.websocket_url);
        let task = WebSocketService::connect(
            &options.websocket_url,
            options.on_inbound_media,
            notification,
        )?;
        debug!("WebSocket task created (connection pending)");
        Ok(task)
    }

    fn send_bytes(&self, bytes: Vec<u8>) {
        self.send_binary(bytes);
    }
}
