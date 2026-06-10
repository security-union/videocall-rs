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
use super::url_log::strip_query_for_log;
use super::webmedia::MediaStreamKey;
use super::ConnectOptions;
use crate::adaptive_quality_constants::HEARTBEAT_KEEPALIVE_INTERVAL_MS;
use crate::crypto::aes::Aes128State;
use gloo::timers::callback::{Interval, Timeout};
use protobuf::Message;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket, TransportType};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;

/// Delay before re-sending a media-state heartbeat exactly once after a
/// mute / camera-off transition.
///
/// A state-change heartbeat can be SUPPRESSED on the receiver by its
/// per-stream media-freshness window: a mute fires immediately after a fresh
/// audio/video frame, so the receiver keeps the stale `enabled = true` until
/// that frame ages out of the window — and absent any retry it would only
/// re-evaluate on the next 5s keepalive, producing the ~5s mute/camera-off
/// lag. We therefore re-send the heartbeat once, just after the receiver's
/// continuous-stream freshness window (`LIVE_STREAM_FRESH_WINDOW_MS = 500ms`
/// in `decode::peer_decode_manager`) has elapsed; by the time this copy
/// lands, no fresh frame remains and the `false` is applied.
///
/// MUST exceed `LIVE_STREAM_FRESH_WINDOW_MS`. 600ms = that 500ms window plus
/// a 100ms margin for clock skew and network jitter. The resend reads LIVE
/// state at fire time, so a mute-then-unmute within the delay re-sends the
/// final (unmuted) state and causes no false-mute flicker. Delivery is
/// reliable (Control stream), so a single resend suffices.
pub(crate) const STATE_CHANGE_RESEND_DELAY_MS: u32 = 600;

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
    is_speaking: Rc<AtomicBool>,
    session_id: Rc<RefCell<Option<u64>>>,
    /// Not wrapped in `Rc` because it is only accessed via `&self` methods,
    /// unlike `session_id` which is shared with the heartbeat `Interval` closure.
    userid: RefCell<Option<String>>,
    /// Pending one-shot resend of a state-change heartbeat (see
    /// `schedule_state_resend`). Held so it can be cancelled if the
    /// connection tears down within `STATE_CHANGE_RESEND_DELAY_MS`; a new
    /// state change replaces (and thereby cancels) any still-pending resend.
    state_resend: RefCell<Option<Timeout>>,
    url: String,
    /// Transport announced to peers in our outgoing heartbeats. This is a
    /// passive label of the locally-chosen transport; it does not affect
    /// connection selection.
    transport_type: TransportType,
}

impl Connection {
    pub fn connect(
        webtransport: bool,
        options: ConnectOptions,
        aes: Rc<Aes128State>,
    ) -> anyhow::Result<Self> {
        // Phase 3c (discussion #793): on the first call per tab,
        // parse `?netsim=<profile>` from `window.location` and
        // install the matching `NetSimShim` in the per-tab hook slot.
        // `std::sync::Once` makes this idempotent across reconnects —
        // a re-election or session drop that calls `Connection::connect`
        // again must not reinstall and reset the hook. Safe on
        // `wasm32-unknown-unknown` because (a) a browser tab is
        // single-threaded, so `Once`'s "first call wins" is race-free
        // and maps to "first connect per tab wins", and (b)
        // `window.location` query params are immutable for the tab
        // lifetime (a real URL change is a navigation that tears down
        // this wasm instance), so re-parsing on reconnect would yield
        // the same profile — skipping it is an optimization, not a
        // behavior change. See `connection/netsim_url.rs`.
        #[cfg(feature = "netsim")]
        {
            static NETSIM_URL_INSTALL: std::sync::Once = std::sync::Once::new();
            NETSIM_URL_INSTALL.call_once(|| {
                let _ = super::netsim_url::try_install_from_url();
            });
        }

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

        let transport_type = if webtransport {
            TransportType::TRANSPORT_WEBTRANSPORT
        } else {
            TransportType::TRANSPORT_WEBSOCKET
        };

        let task = Rc::new(task);

        // Phase 3b (discussion #793): register a `Weak<Task>` with
        // the per-tab netsim hook so the async `Delay` /
        // `DelayAndDuplicate` paths can re-enter the send pipeline
        // after the simulated delay. See
        // `connection/netsim_hook.rs` for the full design.
        #[cfg(feature = "netsim")]
        super::netsim_hook::install_task(Some(Rc::downgrade(&task)));

        let connection = Self {
            task,
            heartbeat: None,
            heartbeat_monitor: Some(Interval::new(5000, move || {
                monitor.emit(());
            })),
            status,
            aes,
            audio_enabled: Rc::new(AtomicBool::new(false)),
            video_enabled: Rc::new(AtomicBool::new(false)),
            screen_enabled: Rc::new(AtomicBool::new(false)),
            is_speaking: Rc::new(AtomicBool::new(false)),
            session_id: Rc::new(RefCell::new(None)),
            userid: RefCell::new(None),
            state_resend: RefCell::new(None),
            url,
            transport_type,
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
        let is_speaking = Rc::clone(&self.is_speaking);
        let session_id = Rc::clone(&self.session_id);
        let transport_type = self.transport_type;

        self.heartbeat = Some(Interval::new(HEARTBEAT_KEEPALIVE_INTERVAL_MS, move || {
            if let Some(packet_wrapper) = build_heartbeat_packet(
                &userid,
                &video_enabled,
                &audio_enabled,
                &screen_enabled,
                &is_speaking,
                &aes,
                &session_id,
                transport_type,
            ) {
                if let Status::Connected = status.get() {
                    // Heartbeats are periodic and expendable — use datagrams
                    // for lower overhead. A missed heartbeat is harmless; the
                    // next one arrives within HEARTBEAT_KEEPALIVE_INTERVAL_MS.
                    task.send_packet_datagram(packet_wrapper);
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
        // Cancel any pending state-change resend so it can't fire on a
        // torn-down task after the connection closes.
        if let Some(resend) = self.state_resend.borrow_mut().take() {
            resend.cancel();
        }
    }

    /// Send a packet via the reliable stream selected by `stream_key`.
    ///
    /// `stream_key` selects which persistent QUIC stream the packet rides
    /// on under WebTransport (one per media type to prevent head-of-line
    /// blocking).  Ignored by WebSocket.
    pub fn send_packet(&self, packet: PacketWrapper, stream_key: MediaStreamKey) {
        if let Status::Connected = self.status.get() {
            self.task.send_packet(packet, stream_key);
        }
    }

    /// Send a packet via datagram (unreliable, low-latency) when supported.
    ///
    /// Used for control packets (heartbeats, RTT probes, diagnostics) that are
    /// periodic and expendable — lower overhead matters more than guaranteed
    /// delivery. Falls back to reliable stream for WebSocket connections or
    /// oversized packets.
    pub fn send_packet_datagram(&self, packet: PacketWrapper) {
        if let Status::Connected = self.status.get() {
            self.task.send_packet_datagram(packet);
        }
    }

    pub fn set_video_enabled(&self, enabled: bool) {
        let prev = self
            .video_enabled
            .swap(enabled, std::sync::atomic::Ordering::Relaxed);
        if prev != enabled {
            log::debug!("Video enabled changed: {prev} -> {enabled}");
            self.send_immediate_heartbeat();
            // Camera-off is suppressed on the receiver while the last video
            // frame is still within its freshness window; resend once after
            // it expires so the change isn't stuck until the 5s keepalive.
            self.schedule_state_resend();
        }
    }

    pub fn set_audio_enabled(&self, enabled: bool) {
        let prev = self
            .audio_enabled
            .swap(enabled, std::sync::atomic::Ordering::Relaxed);
        if prev != enabled {
            log::debug!("Audio enabled changed: {prev} -> {enabled}");
            self.send_immediate_heartbeat();
            // Mute is suppressed on the receiver while the last audio frame is
            // still within its freshness window; resend once after it expires
            // so the change isn't stuck until the 5s keepalive.
            self.schedule_state_resend();
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
    /// (mute/unmute, camera on/off, screen-share on/off) without waiting for
    /// the next keepalive heartbeat tick.
    ///
    /// Unlike the PERIODIC keepalive — which is genuinely expendable and rides
    /// an unreliable datagram (a dropped one is replaced by the next tick
    /// within HEARTBEAT_KEEPALIVE_INTERVAL_MS) — an immediate heartbeat is
    /// EDGE-TRIGGERED: it carries a one-shot state transition. If it is lost,
    /// nothing re-sends that edge; remote peers keep showing the stale state
    /// until the next keepalive up to HEARTBEAT_KEEPALIVE_INTERVAL_MS (5s)
    /// later — and on a lossy WebTransport link that keepalive datagram can
    /// drop too, compounding the lag. So these are sent on the reliable,
    /// ordered `Control` QUIC stream to guarantee delivery. WebSocket already
    /// routes datagrams over its reliable TCP stream, so this is a no-op there.
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
            &self.is_speaking,
            &self.aes,
            &self.session_id,
            self.transport_type,
        ) {
            // Reliable, ordered delivery: a state-change edge must not be lost
            // on an unreliable datagram (see this fn's doc comment).
            self.task
                .send_packet(packet_wrapper, MediaStreamKey::Control);
        }
    }

    /// Re-send the current media-state heartbeat exactly once after
    /// `STATE_CHANGE_RESEND_DELAY_MS`, so a mute / camera-off lands on the
    /// receiver AFTER its short media-freshness window expires instead of
    /// waiting for the 5s keepalive. See `STATE_CHANGE_RESEND_DELAY_MS`.
    ///
    /// Captures shared state by `Rc` and reads it at FIRE time (not now), so
    /// a mute-then-unmute within the delay re-sends the final state and never
    /// produces a false mute. Only meaningful for audio/camera-video, whose
    /// receiver window is short; screen uses a deliberately long window and
    /// is intentionally not resent here.
    ///
    /// Audio and camera-video SHARE the single `state_resend` slot: a second
    /// near-simultaneous toggle reschedules the one timer, so its resend can
    /// drift toward (never past) the keepalive. This is safe — the resend
    /// carries the full live state (audio + video + screen) read at fire time,
    /// so both transitions are covered by whichever resend lands, and the
    /// worst case is still far better than the old ~5s keepalive-only path.
    fn schedule_state_resend(&self) {
        // No heartbeat identity yet → nothing to resend.
        let userid = match self.userid.borrow().as_ref() {
            Some(id) => id.clone(),
            None => return,
        };
        let task = Rc::clone(&self.task);
        let status = Rc::clone(&self.status);
        let aes = Rc::clone(&self.aes);
        let video_enabled = Rc::clone(&self.video_enabled);
        let audio_enabled = Rc::clone(&self.audio_enabled);
        let screen_enabled = Rc::clone(&self.screen_enabled);
        let is_speaking = Rc::clone(&self.is_speaking);
        let session_id = Rc::clone(&self.session_id);
        let transport_type = self.transport_type;

        let timeout = Timeout::new(STATE_CHANGE_RESEND_DELAY_MS, move || {
            if !matches!(status.get(), Status::Connected) {
                return;
            }
            if let Some(packet_wrapper) = build_heartbeat_packet(
                &userid,
                &video_enabled,
                &audio_enabled,
                &screen_enabled,
                &is_speaking,
                &aes,
                &session_id,
                transport_type,
            ) {
                task.send_packet(packet_wrapper, MediaStreamKey::Control);
            }
        });
        *self.state_resend.borrow_mut() = Some(timeout);
    }

    pub fn set_speaking(&self, speaking: bool) {
        let prev = self
            .is_speaking
            .swap(speaking, std::sync::atomic::Ordering::Relaxed);
        if prev != speaking {
            log::debug!("Speaking changed: {prev} -> {speaking}");
            self.send_immediate_heartbeat();
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn is_webtransport(&self) -> bool {
        self.transport_type == TransportType::TRANSPORT_WEBTRANSPORT
    }

    pub fn set_session_id(&self, session_id: u64) {
        *self.session_id.borrow_mut() = Some(session_id);
    }

    /// Get send queue depth (bufferedAmount for WebSocket, not available for WebTransport)
    pub fn get_send_queue_depth(&self) -> Option<u64> {
        self.task.get_send_queue_depth()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        log::debug!("Dropping Connection to {}", strip_query_for_log(&self.url));
        self.stop_heartbeat();
    }
}

#[cfg(test)]
impl Connection {
    pub(crate) fn new_for_test() -> Self {
        let key = vec![1u8; 16];
        let iv = vec![2u8; 16];
        Self {
            task: Rc::new(Task::stub()),
            heartbeat: None,
            heartbeat_monitor: None,
            status: Rc::new(Cell::new(Status::Connected)),
            aes: Rc::new(Aes128State::from_vecs(key, iv, true)),
            audio_enabled: Rc::new(AtomicBool::new(false)),
            video_enabled: Rc::new(AtomicBool::new(false)),
            screen_enabled: Rc::new(AtomicBool::new(false)),
            is_speaking: Rc::new(AtomicBool::new(false)),
            session_id: Rc::new(RefCell::new(None)),
            userid: RefCell::new(Some("test-user".to_string())),
            state_resend: RefCell::new(None),
            url: "test://stub".to_string(),
            transport_type: TransportType::TRANSPORT_WEBSOCKET,
        }
    }

    pub(crate) fn new_for_test_without_userid() -> Self {
        let conn = Self::new_for_test();
        *conn.userid.borrow_mut() = None;
        conn
    }

    pub(crate) fn state_resend_is_pending(&self) -> bool {
        self.state_resend.borrow().is_some()
    }

    pub(crate) fn stop_heartbeat_for_test(&mut self) {
        self.stop_heartbeat();
    }
}

#[allow(clippy::too_many_arguments)]
fn build_heartbeat_packet(
    userid: &str,
    video_enabled: &AtomicBool,
    audio_enabled: &AtomicBool,
    screen_enabled: &AtomicBool,
    is_speaking: &AtomicBool,
    aes: &Aes128State,
    session_id: &RefCell<Option<u64>>,
    transport_type: TransportType,
) -> Option<PacketWrapper> {
    let heartbeat_metadata = HeartbeatMetadata {
        video_enabled: video_enabled.load(std::sync::atomic::Ordering::Relaxed),
        audio_enabled: audio_enabled.load(std::sync::atomic::Ordering::Relaxed),
        screen_enabled: screen_enabled.load(std::sync::atomic::Ordering::Relaxed),
        is_speaking: is_speaking.load(std::sync::atomic::Ordering::Relaxed),
        transport_type: ::protobuf::EnumOrUnknown::new(transport_type),
        special_fields: ::protobuf::SpecialFields::new(),
    };

    let packet = MediaPacket {
        media_type: MediaType::HEARTBEAT.into(),
        user_id: userid.as_bytes().to_vec(),
        timestamp: js_sys::Date::now(),
        heartbeat_metadata: Some(heartbeat_metadata).into(),
        ..Default::default()
    };

    let data = aes_encrypt_heartbeat(aes, &packet)
        .map_err(|e| {
            log::error!("{e}");
            let _ = videocall_diagnostics::global_sender().try_broadcast(
                videocall_diagnostics::DiagEvent {
                    subsystem: "heartbeat",
                    stream_id: None,
                    ts_ms: videocall_diagnostics::now_ms(),
                    metrics: vec![videocall_diagnostics::metric!("encryption_failure", 1u64)],
                },
            );
        })
        .ok()?;
    let mut packet_wrapper = PacketWrapper {
        data,
        user_id: userid.as_bytes().to_vec(),
        packet_type: PacketType::MEDIA.into(),
        ..Default::default()
    };

    if let Some(sid) = session_id.borrow().as_ref() {
        packet_wrapper.session_id = *sid;
    }

    Some(packet_wrapper)
}

fn aes_encrypt_heartbeat(aes: &Aes128State, packet: &MediaPacket) -> Result<Vec<u8>, String> {
    let bytes = packet
        .write_to_bytes()
        .map_err(|e| format!("Failed to serialize heartbeat packet: {e}"))?;
    aes.encrypt(&bytes)
        .map_err(|e| format!("Failed to encrypt heartbeat packet: {e:?}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::task::StubSendKind;
    use crate::connection::webmedia::MediaStreamKey;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test;

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test]
    fn state_resend_scheduled_on_audio_toggle() {
        let conn = Connection::new_for_test();
        assert!(
            !conn.state_resend_is_pending(),
            "no resend before a state transition"
        );
        conn.set_audio_enabled(true);
        assert!(
            conn.state_resend_is_pending(),
            "audio toggle must schedule the one-shot state resend"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test]
    fn state_resend_not_scheduled_without_userid() {
        let conn = Connection::new_for_test_without_userid();
        conn.set_audio_enabled(true);
        assert!(
            !conn.state_resend_is_pending(),
            "schedule_state_resend must no-op when heartbeat identity is unset"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test]
    fn state_resend_cleared_on_teardown() {
        let mut conn = Connection::new_for_test();
        conn.set_audio_enabled(true);
        assert!(conn.state_resend_is_pending());
        conn.stop_heartbeat_for_test();
        assert!(
            !conn.state_resend_is_pending(),
            "stop_heartbeat must take and cancel the pending resend timer"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test]
    fn state_resend_slot_replaced_on_second_transition() {
        let conn = Connection::new_for_test();
        conn.set_audio_enabled(true);
        assert!(conn.state_resend_is_pending());
        conn.set_video_enabled(true);
        assert!(
            conn.state_resend_is_pending(),
            "a second media transition must keep the single resend slot occupied \
             (prior Timeout dropped on replace)"
        );
    }

    /// Sender resend must fire AFTER the receiver's short suppression window.
    /// Constants live in different modules; pin the ordering so a future edit
    /// cannot silently re-introduce the ~5s mute/camera-off lag.
    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test]
    fn state_change_resend_delay_exceeds_live_stream_fresh_window() {
        use crate::decode::peer_decode_manager::LIVE_STREAM_FRESH_WINDOW_MS;
        assert!(
            STATE_CHANGE_RESEND_DELAY_MS as u64 > LIVE_STREAM_FRESH_WINDOW_MS,
            "STATE_CHANGE_RESEND_DELAY_MS must exceed LIVE_STREAM_FRESH_WINDOW_MS \
             or the one-shot resend lands inside the suppression window"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test]
    fn immediate_heartbeat_uses_control_stream_not_datagram() {
        let conn = Connection::new_for_test();
        conn.task.clear_last_send_for_test();

        conn.set_audio_enabled(true);

        let (kind, stream_key) = conn
            .task
            .take_last_send_for_test()
            .expect("immediate heartbeat must send on state change");
        assert_eq!(
            kind,
            StubSendKind::Reliable,
            "edge-triggered heartbeats must use the reliable path, not datagrams"
        );
        assert_eq!(
            stream_key,
            MediaStreamKey::Control,
            "immediate heartbeat must ride the Control stream"
        );
        assert!(
            conn.task.take_last_send_for_test().is_none(),
            "periodic keepalive datagram path must not run without start_heartbeat"
        );
    }
}
