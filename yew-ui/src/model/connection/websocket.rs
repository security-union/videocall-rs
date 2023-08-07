//
// This submodule implements our WebMedia trait for WebSocketTask.
//
use super::webmedia::{ConnectOptions, WebMedia};
use log::debug;
use yew::prelude::Callback;
use yew_websocket::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};

impl WebMedia<WebSocketTask> for WebSocketTask {
    fn connect(options: ConnectOptions) -> anyhow::Result<WebSocketTask> {
        let notification = Callback::from(move |status| match status {
            WebSocketStatus::Opened => options.on_connected.emit(()),
            WebSocketStatus::Closed | WebSocketStatus::Error => options.on_connection_lost.emit(()),
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
