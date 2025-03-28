///
/// Connection struct wraps the lower-level "Task" (task.rs), providing a heartbeat and keeping
/// track of connection status.
///
use super::task::Task;
use super::ConnectOptions;
use crate::crypto::aes::Aes128State;
use gloo::timers::callback::Interval;
use protobuf::Message;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use yew::prelude::Callback;

#[derive(Clone, Copy, Debug)]
enum Status {
    Connecting,
    Connected,
    Closed,
}

#[derive(Debug)]
pub struct Connection {
    task: Rc<Task>,
    heartbeat: Option<Interval>,
    heartbeat_monitor: Option<Interval>,
    status: Rc<Cell<Status>>,
    aes: Rc<Aes128State>,
    video_enabled: Rc<AtomicBool>,
    audio_enabled: Rc<AtomicBool>,
    screen_enabled: Rc<AtomicBool>,
}

impl Connection {
    pub fn connect(
        webtransport: bool,
        options: ConnectOptions,
        aes: Rc<Aes128State>,
    ) -> anyhow::Result<Self> {
        let mut options = options;
        let userid = options.userid.clone();
        let status = Rc::new(Cell::new(Status::Connecting));
        {
            let status = Rc::clone(&status);
            options.on_connected = tap_callback(
                options.on_connected,
                Callback::from(move |_| status.set(Status::Connected)),
            );
        }
        {
            let status = Rc::clone(&status);
            options.on_connection_lost = tap_callback(
                options.on_connection_lost,
                Callback::from(move |_| status.set(Status::Closed)),
            );
        }
        let monitor = options.peer_monitor.clone();
        let mut connection = Self {
            task: Rc::new(Task::connect(webtransport, options)?),
            heartbeat: None,
            heartbeat_monitor: Some(Interval::new(5000, move || {
                monitor.emit(());
            })),
            status,
            aes,
            audio_enabled: Rc::new(AtomicBool::new(false)),
            video_enabled: Rc::new(AtomicBool::new(false)),
            screen_enabled: Rc::new(AtomicBool::new(false)),
        };
        connection.start_heartbeat(userid);

        Ok(connection)
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.status.get(), Status::Connected)
    }

    fn start_heartbeat(&mut self, userid: String) {
        let task = Rc::clone(&self.task);
        let status = Rc::clone(&self.status);
        let aes = Rc::clone(&self.aes);
        let video_enabled = Rc::clone(&self.video_enabled);
        let audio_enabled = Rc::clone(&self.audio_enabled);
        let screen_enabled = Rc::clone(&self.screen_enabled);
        self.heartbeat = Some(Interval::new(1000, move || {
            let heartbeat_metadata = HeartbeatMetadata {
                video_enabled: video_enabled.load(std::sync::atomic::Ordering::Relaxed),
                audio_enabled: audio_enabled.load(std::sync::atomic::Ordering::Relaxed),
                screen_enabled: screen_enabled.load(std::sync::atomic::Ordering::Relaxed),
                ..Default::default()
            };

            let packet = MediaPacket {
                media_type: MediaType::HEARTBEAT.into(),
                email: userid.clone(),
                timestamp: js_sys::Date::now(),
                heartbeat_metadata: Some(heartbeat_metadata).into(),
                ..Default::default()
            };
            let data = aes.encrypt(&packet.write_to_bytes().unwrap()).unwrap();
            let packet = PacketWrapper {
                data,
                email: userid.clone(),
                packet_type: PacketType::MEDIA.into(),
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
        if let Some(heartbeat_monitor) = self.heartbeat_monitor.take() {
            heartbeat_monitor.cancel();
        }
    }

    pub fn send_packet(&self, packet: PacketWrapper) {
        if let Status::Connected = self.status.get() {
            self.task.send_packet(packet);
        }
    }

    pub fn set_video_enabled(&self, enabled: bool) {
        self.video_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_audio_enabled(&self, enabled: bool) {
        self.audio_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_screen_enabled(&self, enabled: bool) {
        self.screen_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
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
