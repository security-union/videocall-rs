//
// Connection struct wraps the lower-level "Task" (task.rs), providing a heartbeat and keeping
// track of connection status.
//
use super::task::Task;
use super::ConnectOptions;
use gloo::timers::callback::Interval;
use std::cell::Cell;
use std::sync::Arc;
use types::protos::media_packet::media_packet::MediaType;
use types::protos::media_packet::MediaPacket;
use yew::prelude::Callback;

#[derive(Clone, Copy)]
enum Status {
    Connecting,
    Connected,
    Closed,
}

pub struct Connection {
    task: Arc<Task>,
    heartbeat: Option<Interval>,
    status: Arc<Cell<Status>>,
}

impl Connection {
    pub fn connect(webtransport: bool, options: ConnectOptions) -> anyhow::Result<Self> {
        let mut options = options.clone();
        let userid = options.userid.clone();
        let status = Arc::new(Cell::new(Status::Connecting));
        {
            let status = Arc::clone(&status);
            options.on_connected = tap_callback(
                options.on_connected,
                Callback::from(move |_| status.set(Status::Connected)),
            );
        }
        {
            let status = Arc::clone(&status);
            options.on_connection_lost = tap_callback(
                options.on_connection_lost,
                Callback::from(move |_| status.set(Status::Closed)),
            );
        }
        let mut connection = Self {
            task: Arc::new(Task::connect(webtransport, options)?),
            heartbeat: None,
            status,
        };
        connection.start_heartbeat(userid);
        Ok(connection)
    }

    pub fn is_connected(&self) -> bool {
        match self.status.get() {
            Status::Connected => true,
            _ => false,
        }
    }

    fn start_heartbeat(&mut self, userid: String) {
        let task = Arc::clone(&self.task);
        let status = Arc::clone(&self.status);
        self.heartbeat = Some(Interval::new(1000, move || {
            let packet = MediaPacket {
                media_type: MediaType::HEARTBEAT.into(),
                email: userid.clone(),
                timestamp: js_sys::Date::now(),
                ..Default::default()
            };
            if let Status::Connected = status.get() {
                task.send_packet(packet);
            }
        }));
    }

    fn stop_heartbeat(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.cancel();
        }
    }

    pub fn send_packet(&self, packet: MediaPacket) {
        if let Status::Connected = self.status.get() {
            self.task.send_packet(packet);
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

fn tap_callback<IN: 'static, OUT: 'static>(
    callback: Callback<IN, OUT>,
    tap: Callback<()>,
) -> Callback<IN, OUT> {
    Callback::from(move |arg| {
        tap.emit(());
        callback.emit(arg)
    })
}
