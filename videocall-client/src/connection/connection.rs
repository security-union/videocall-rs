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

///
/// Connection struct wraps the lower-level "Task" (task.rs), providing a heartbeat and keeping
/// track of connection status.
///
use super::task::Task;
use super::ConnectOptions;
use crate::crypto::aes::Aes128State;
use gloo::timers::callback::Interval;
use protobuf::Message;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;

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
    session_id: Rc<RefCell<Option<u64>>>,
    userid: RefCell<Option<String>>,
    url: String,
}

impl Connection {
    pub fn connect(
        webtransport: bool,
        options: ConnectOptions,
        aes: Rc<Aes128State>,
    ) -> anyhow::Result<Self> {
        let mut new_options = options.clone();
        let status = Rc::new(Cell::new(Status::Connecting));

        let url = if webtransport {
            new_options.webtransport_url.clone()
        } else {
            new_options.websocket_url.clone()
        };

        let on_connected_tap = {
            let status = Rc::clone(&status);
            Callback::from(move |_| status.set(Status::Connected))
        };
        new_options.on_connected = tap_callback(new_options.on_connected, on_connected_tap);

        let on_lost_tap = {
            let status = Rc::clone(&status);
            Callback::from(move |_| status.set(Status::Closed))
        };
        new_options.on_connection_lost = tap_callback(new_options.on_connection_lost, on_lost_tap);

        let monitor = new_options.peer_monitor.clone();
        let task = Task::connect(webtransport, new_options)?;

        let connection = Self {
            task: Rc::new(task),
            heartbeat: None,
            heartbeat_monitor: Some(Interval::new(5000, move || {
                monitor.emit(());
            })),
            status,
            aes,
            audio_enabled: Rc::new(AtomicBool::new(false)),
            video_enabled: Rc::new(AtomicBool::new(false)),
            screen_enabled: Rc::new(AtomicBool::new(false)),
            session_id: Rc::new(RefCell::new(None)),
            userid: RefCell::new(None),
            url,
        };

        Ok(connection)
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.status.get(), Status::Connected)
    }

    pub fn start_heartbeat(&mut self, userid: String) {
        *self.userid.borrow_mut() = Some(userid.clone());
        let task = Rc::clone(&self.task);
        let status = Rc::clone(&self.status);
        let aes = Rc::clone(&self.aes);
        let video_enabled = Rc::clone(&self.video_enabled);
        let audio_enabled = Rc::clone(&self.audio_enabled);
        let screen_enabled = Rc::clone(&self.screen_enabled);
        let session_id = Rc::clone(&self.session_id);
        self.heartbeat = Some(Interval::new(1000, move || {
            if let Some(packet_wrapper) = build_heartbeat_packet(
                &userid,
                &video_enabled,
                &audio_enabled,
                &screen_enabled,
                &aes,
                &session_id,
            ) {
                if let Status::Connected = status.get() {
                    task.send_packet(packet_wrapper);
                }
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
        let prev = self
            .video_enabled
            .swap(enabled, std::sync::atomic::Ordering::Relaxed);
        if prev != enabled {
            log::debug!("Video enabled changed: {prev} -> {enabled}");
            self.send_immediate_heartbeat();
        }
    }

    pub fn set_audio_enabled(&self, enabled: bool) {
        let prev = self
            .audio_enabled
            .swap(enabled, std::sync::atomic::Ordering::Relaxed);
        if prev != enabled {
            log::debug!("Audio enabled changed: {prev} -> {enabled}");
            self.send_immediate_heartbeat();
        }
    }

    pub fn set_screen_enabled(&self, enabled: bool) {
        let prev = self
            .screen_enabled
            .swap(enabled, std::sync::atomic::Ordering::Relaxed);
        if prev != enabled {
            log::debug!("Screen enabled changed: {prev} -> {enabled}");
            self.send_immediate_heartbeat();
        }
    }

    /// Send a heartbeat packet immediately so peers learn about state changes
    /// without waiting for the next 1-second heartbeat tick.
    fn send_immediate_heartbeat(&self) {
        let userid = match self.userid.borrow().as_ref() {
            Some(id) => id.clone(),
            None => return, // heartbeat not started yet
        };

        if !matches!(self.status.get(), Status::Connected) {
            return;
        }

        if let Some(packet_wrapper) = build_heartbeat_packet(
            &userid,
            &self.video_enabled,
            &self.audio_enabled,
            &self.screen_enabled,
            &self.aes,
            &self.session_id,
        ) {
            self.task.send_packet(packet_wrapper);
        }
    }

    pub fn set_session_id(&self, session_id: u64) {
        *self.session_id.borrow_mut() = Some(session_id);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        log::debug!("Dropping Connection to {}", self.url);
        self.stop_heartbeat();
    }
}

fn build_heartbeat_packet(
    userid: &str,
    video_enabled: &AtomicBool,
    audio_enabled: &AtomicBool,
    screen_enabled: &AtomicBool,
    aes: &Aes128State,
    session_id: &RefCell<Option<u64>>,
) -> Option<PacketWrapper> {
    let heartbeat_metadata = HeartbeatMetadata {
        video_enabled: video_enabled.load(std::sync::atomic::Ordering::Relaxed),
        audio_enabled: audio_enabled.load(std::sync::atomic::Ordering::Relaxed),
        screen_enabled: screen_enabled.load(std::sync::atomic::Ordering::Relaxed),
        ..Default::default()
    };

    let packet = MediaPacket {
        media_type: MediaType::HEARTBEAT.into(),
        email: userid.to_owned(),
        timestamp: js_sys::Date::now(),
        heartbeat_metadata: Some(heartbeat_metadata).into(),
        ..Default::default()
    };

    let data = aes_encrypt_heartbeat(aes, &packet).ok()?;
    let mut packet_wrapper = PacketWrapper {
        data,
        email: userid.to_owned(),
        packet_type: PacketType::MEDIA.into(),
        ..Default::default()
    };

    if let Some(sid) = session_id.borrow().as_ref() {
        packet_wrapper.session_id = *sid;
    }

    Some(packet_wrapper)
}

fn aes_encrypt_heartbeat(aes: &Aes128State, packet: &MediaPacket) -> Result<Vec<u8>, ()> {
    let bytes = packet.write_to_bytes().map_err(|e| {
        log::error!("Failed to serialize heartbeat packet: {e}");
    })?;
    aes.encrypt(&bytes).map_err(|e| {
        log::error!("Failed to encrypt heartbeat packet: {e:?}");
    })
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
