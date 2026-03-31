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
use super::webmedia::{ConnectOptions, WebMedia};
use log::debug;
use videocall_transport::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};
use videocall_types::Callback;
use wasm_bindgen::JsValue;

impl WebMedia<WebSocketTask> for WebSocketTask {
    fn connect(options: ConnectOptions) -> anyhow::Result<WebSocketTask> {
        let notification = Callback::from(move |status| match status {
            WebSocketStatus::Opened => options.on_connected.emit(()),
            WebSocketStatus::Closed(close_info) => {
                let msg = match close_info {
                    Some((code, ref reason)) if !reason.is_empty() => {
                        format!("WebSocket closed: code={code}, reason={reason}")
                    }
                    Some((code, _)) => format!("WebSocket closed: code={code}"),
                    None => "WebSocket closed".to_string(),
                };
                options.on_connection_lost.emit(JsValue::from_str(&msg));
            }
            WebSocketStatus::Error => options
                .on_connection_lost
                .emit(JsValue::from_str("WebSocket error")),
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
