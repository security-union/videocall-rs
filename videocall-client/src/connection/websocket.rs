//
// This submodule implements our WebMedia trait for WebSocketTask.
//
use super::webmedia::{ConnectOptions, WebMedia};
use log::debug;
use wasm_bindgen::JsValue;
use yew::prelude::Callback;
use yew_websocket::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};

impl WebMedia<WebSocketTask> for WebSocketTask {
    fn connect(options: ConnectOptions) -> anyhow::Result<WebSocketTask> {
        let notification = Callback::from(move |status| match status {
            WebSocketStatus::Opened => options.on_connected.emit(()),
            WebSocketStatus::Closed => options
                .on_connection_lost
                .emit(JsValue::from_str("WebSocket closed")),
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
        debug!("WebSocket connection success");
        Ok(task)
    }

    fn send_bytes(&self, bytes: Vec<u8>) {
        self.send_binary(bytes);
    }
}
