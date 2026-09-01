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

// Indexed by `MediaStreamKey::as_u8()`; slot 0 unused so a key indexes itself.
const STREAM_COUNTER_SLOTS: usize = videocall_types::limits::MEDIA_STREAM_COUNTER_SLOTS;

// A `const` initializer would trip clippy::declare_interior_mutable_const.
const fn zeroed_slots() -> [AtomicU64; STREAM_COUNTER_SLOTS] {
    [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ]
}

/// Every WebSocket send counter, grouped so the billing seam can be driven
/// against storage the caller owns. Production bills the one `WS_COUNTERS`
/// static; each test owns an instance, so parallel runs cannot collide.
struct WsCounters {
    aggregate_drops: AtomicU64,
    drops: [AtomicU64; STREAM_COUNTER_SLOTS],
    offered_bytes: [AtomicU64; STREAM_COUNTER_SLOTS],
    dropped_bytes: [AtomicU64; STREAM_COUNTER_SLOTS],
    inactive_drops: [AtomicU64; STREAM_COUNTER_SLOTS],
    inactive_dropped_bytes: [AtomicU64; STREAM_COUNTER_SLOTS],
    /// Every inactive discard, including keys `stream_slot` rejects, split by the
    /// `ready_state` that caused it.
    inactive_drops_closing: AtomicU64,
    inactive_drops_closed: AtomicU64,
}

impl WsCounters {
    const fn new() -> Self {
        Self {
            aggregate_drops: AtomicU64::new(0),
            drops: zeroed_slots(),
            offered_bytes: zeroed_slots(),
            dropped_bytes: zeroed_slots(),
            inactive_drops: zeroed_slots(),
            inactive_dropped_bytes: zeroed_slots(),
            inactive_drops_closing: AtomicU64::new(0),
            inactive_drops_closed: AtomicU64::new(0),
        }
    }

    fn record_drop(&self, stream_key: u8) {
        self.aggregate_drops.fetch_add(1, Ordering::Relaxed);
        if let Some(counter) = stream_slot(&self.drops, stream_key) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Bills a discard caused by a non-live socket. Deliberately touches neither
    /// `drops` nor `dropped_bytes`: those two carry the backpressure signal only.
    fn record_inactive_drop(&self, stream_key: u8, bytes: u64, ready_state: u16) {
        let state_counter = match ready_state {
            WebSocket::CLOSING => &self.inactive_drops_closing,
            WebSocket::CLOSED => &self.inactive_drops_closed,
            // readyState defines no other non-live value; fold rather than lose it.
            _ => &self.inactive_drops_closed,
        };
        state_counter.fetch_add(1, Ordering::Relaxed);
        if let Some(counter) = stream_slot(&self.inactive_drops, stream_key) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(counter) = stream_slot(&self.inactive_dropped_bytes, stream_key) {
            counter.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn record_offered(&self, stream_key: u8, bytes: u64) {
        if let Some(counter) = stream_slot(&self.offered_bytes, stream_key) {
            counter.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn record_dropped_bytes(&self, stream_key: u8, bytes: u64) {
        if let Some(counter) = stream_slot(&self.dropped_bytes, stream_key) {
            counter.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn aggregate_drop_count(&self) -> u64 {
        self.aggregate_drops.load(Ordering::Relaxed)
    }

    fn drop_count_for_stream(&self, stream_key: u8) -> u64 {
        load_slot(&self.drops, stream_key)
    }

    fn offered_bytes_for_stream(&self, stream_key: u8) -> u64 {
        load_slot(&self.offered_bytes, stream_key)
    }

    fn dropped_bytes_for_stream(&self, stream_key: u8) -> u64 {
        load_slot(&self.dropped_bytes, stream_key)
    }

    fn inactive_dropped_frames_for_stream(&self, stream_key: u8) -> u64 {
        load_slot(&self.inactive_drops, stream_key)
    }

    fn inactive_dropped_bytes_for_stream(&self, stream_key: u8) -> u64 {
        load_slot(&self.inactive_dropped_bytes, stream_key)
    }

    fn inactive_dropped_frames_closing(&self) -> u64 {
        self.inactive_drops_closing.load(Ordering::Relaxed)
    }

    fn inactive_dropped_frames_closed(&self) -> u64 {
        self.inactive_drops_closed.load(Ordering::Relaxed)
    }
}

static WS_COUNTERS: WsCounters = WsCounters::new();

fn stream_slot(counters: &[AtomicU64; STREAM_COUNTER_SLOTS], stream_key: u8) -> Option<&AtomicU64> {
    match stream_key {
        1..=videocall_types::limits::MAX_MEDIA_STREAM_KEY => counters.get(usize::from(stream_key)),
        _ => None,
    }
}

fn load_slot(counters: &[AtomicU64; STREAM_COUNTER_SLOTS], stream_key: u8) -> u64 {
    stream_slot(counters, stream_key)
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// What `send_binary_for_stream` must do with a frame once it has been billed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendOutcome {
    /// Socket is neither CONNECTING nor OPEN: discard, billed as inactive.
    Inactive,
    /// Send buffer is over `MAX_BUFFERED_AMOUNT`: discard and bill the drop.
    Dropped,
    /// Hand the frame to the socket.
    Deliver,
}

fn ready_state_is_active(ready_state: u16) -> bool {
    matches!(ready_state, WebSocket::CONNECTING | WebSocket::OPEN)
}

/// Bills one frame and decides its fate. Offered is billed before either gate,
/// so it stays live through an overflow.
fn bill_send(
    counters: &WsCounters,
    stream_key: u8,
    len: usize,
    ready_state: u16,
    buffered: u32,
) -> SendOutcome {
    counters.record_offered(stream_key, len as u64);
    if !ready_state_is_active(ready_state) {
        counters.record_inactive_drop(stream_key, len as u64, ready_state);
        return SendOutcome::Inactive;
    }
    if buffered > MAX_BUFFERED_AMOUNT {
        counters.record_drop(stream_key);
        counters.record_dropped_bytes(stream_key, len as u64);
        return SendOutcome::Dropped;
    }
    SendOutcome::Deliver
}

/// Returns the total number of WebSocket packets dropped due to backpressure since process start.
pub fn websocket_drop_count() -> u64 {
    WS_COUNTERS.aggregate_drop_count()
}

/// Returns WebSocket backpressure drops attributed to one media stream key.
/// WebSocket still uses one TCP socket; attribution records which attempted
/// send encountered the full shared buffer.
pub fn websocket_drop_count_for_stream(stream_key: u8) -> u64 {
    WS_COUNTERS.drop_count_for_stream(stream_key)
}

/// Includes bytes a gate later discarded.
pub fn websocket_offered_bytes_for_stream(stream_key: u8) -> u64 {
    WS_COUNTERS.offered_bytes_for_stream(stream_key)
}

/// Excludes a failed `send` and a closed socket.
pub fn websocket_dropped_bytes_for_stream(stream_key: u8) -> u64 {
    WS_COUNTERS.dropped_bytes_for_stream(stream_key)
}

/// Frames discarded for one media stream because the socket was neither
/// CONNECTING nor OPEN. Disjoint from [`websocket_drop_count_for_stream`].
pub fn websocket_inactive_dropped_frames_for_stream(stream_key: u8) -> u64 {
    WS_COUNTERS.inactive_dropped_frames_for_stream(stream_key)
}

/// The byte companion to [`websocket_inactive_dropped_frames_for_stream`]. Disjoint
/// from [`websocket_dropped_bytes_for_stream`], and a subset of
/// [`websocket_offered_bytes_for_stream`].
pub fn websocket_inactive_dropped_bytes_for_stream(stream_key: u8) -> u64 {
    WS_COUNTERS.inactive_dropped_bytes_for_stream(stream_key)
}

/// Inactive discards billed while `ready_state` was CLOSING.
pub fn websocket_inactive_dropped_frames_closing() -> u64 {
    WS_COUNTERS.inactive_dropped_frames_closing()
}

/// Inactive discards billed at the one remaining non-live `ready_state`, CLOSED.
pub fn websocket_inactive_dropped_frames_closed() -> u64 {
    WS_COUNTERS.inactive_dropped_frames_closed()
}

/// NETSIM-ONLY: synthetically bump the WS send-buffer drop counter by `n` (issue
/// #1398). The real increment fires when `bufferedAmount > MAX_BUFFERED_AMOUNT`,
/// which an e2e test cannot reliably induce on a localhost loopback. This
/// feature-gated bumper records audio-attributed drops for the microphone
/// detector while preserving the aggregate counter used by camera/screen AQ.
/// Zero production cost: compiled out unless the `netsim` feature is on.
#[cfg(feature = "netsim")]
pub fn force_websocket_drop(n: u64) {
    force_websocket_drop_for_stream(1, n);
}

/// NETSIM-ONLY: synthetically record buffered-send drops for one media stream.
/// Used by regression tests that verify cross-stream isolation.
#[cfg(feature = "netsim")]
pub fn force_websocket_drop_for_stream(stream_key: u8, n: u64) {
    WS_COUNTERS.aggregate_drops.fetch_add(n, Ordering::Relaxed);
    if let Some(counter) = stream_slot(&WS_COUNTERS.drops, stream_key) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

/// NETSIM-ONLY: synthetically bill offered/dropped bytes for one media stream.
#[cfg(feature = "netsim")]
pub fn force_websocket_bytes_for_stream(stream_key: u8, offered: u64, dropped: u64) {
    WS_COUNTERS.record_offered(stream_key, offered);
    WS_COUNTERS.record_dropped_bytes(stream_key, dropped);
}

/// NETSIM-ONLY: synthetically bill one inactive-socket discard of `bytes` for one
/// media stream, attributed to CLOSING when `closing` and to CLOSED otherwise.
/// Bills `offered` first, as `bill_send` does, so the subset relation between the
/// two byte counters is preserved.
#[cfg(feature = "netsim")]
pub fn force_websocket_inactive_drop_for_stream(stream_key: u8, bytes: u64, closing: bool) {
    let ready_state = if closing {
        WebSocket::CLOSING
    } else {
        WebSocket::CLOSED
    };
    WS_COUNTERS.record_offered(stream_key, bytes);
    WS_COUNTERS.record_inactive_drop(stream_key, bytes, ready_state);
}

/// NETSIM-ONLY: zero the inactive-socket counters. The process-global statics have
/// no production reset; a test asserting the ZERO reading needs one.
#[cfg(feature = "netsim")]
pub fn reset_websocket_inactive_counters_for_test() {
    WS_COUNTERS
        .inactive_drops_closing
        .store(0, Ordering::Relaxed);
    WS_COUNTERS
        .inactive_drops_closed
        .store(0, Ordering::Relaxed);
    for slot in &WS_COUNTERS.inactive_drops {
        slot.store(0, Ordering::Relaxed);
    }
    for slot in &WS_COUNTERS.inactive_dropped_bytes {
        slot.store(0, Ordering::Relaxed);
    }
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
    /// Receive-only: `WebSocketTask` has no text sender.
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

    /// Sends binary data to a WebSocket connection.
    ///
    /// If the browser's send buffer already exceeds [`MAX_BUFFERED_AMOUNT`],
    /// the packet is silently dropped to prevent unbounded memory growth on
    /// slow networks. This mirrors the congestion-drop behavior used on the
    /// WebTransport datagram path.
    pub fn send_binary(&self, data: Vec<u8>) {
        self.send_binary_for_stream(data, 0);
    }

    /// Sends binary data while retaining the caller's media key for
    /// backpressure-counter attribution. The key does not affect WS routing.
    pub fn send_binary_for_stream(&self, data: Vec<u8>, stream_key: u8) {
        let buffered = self.ws.buffered_amount();
        match bill_send(
            &WS_COUNTERS,
            stream_key,
            data.len(),
            self.ws.ready_state(),
            buffered,
        ) {
            SendOutcome::Inactive => return,
            SendOutcome::Dropped => {
                warn!(
                    "WebSocket backpressure: dropping {} byte packet (buffered: {} bytes, threshold: {} bytes)",
                    data.len(),
                    buffered,
                    MAX_BUFFERED_AMOUNT,
                );
                return;
            }
            SendOutcome::Deliver => {}
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
        ready_state_is_active(self.ws.ready_state())
    }
}

impl Drop for WebSocketTask {
    fn drop(&mut self) {
        if self.is_active() {
            self.ws.close().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_backpressure_drops_do_not_increment_audio_counter() {
        let counters = WsCounters::new();

        counters.record_drop(3);

        assert_eq!(counters.aggregate_drop_count(), 1);
        assert_eq!(counters.drop_count_for_stream(3), 1);
        assert_eq!(
            counters.drop_count_for_stream(1),
            0,
            "screen backpressure must not enter the microphone's audio counter",
        );
    }

    #[test]
    fn offered_bytes_are_attributed_to_their_own_stream() {
        let counters = WsCounters::new();

        counters.record_offered(2, 1500);

        assert_eq!(counters.offered_bytes_for_stream(2), 1500);
        assert_eq!(
            counters.offered_bytes_for_stream(1),
            0,
            "video bytes must not be billed to audio",
        );
    }

    #[test]
    fn dropped_bytes_are_separate_from_offered() {
        let counters = WsCounters::new();

        counters.record_offered(3, 900);
        counters.record_dropped_bytes(3, 400);

        assert_eq!(counters.offered_bytes_for_stream(3), 900);
        assert_eq!(counters.dropped_bytes_for_stream(3), 400);
    }

    #[test]
    fn a_packet_drop_records_no_offered_bytes() {
        let counters = WsCounters::new();

        counters.record_drop(4);

        assert_eq!(counters.offered_bytes_for_stream(4), 0);
    }

    #[test]
    fn unknown_stream_key_records_nothing() {
        let counters = WsCounters::new();

        counters.record_offered(99, 64);
        counters.record_dropped_bytes(99, 64);

        for key in 1..=videocall_types::limits::MAX_MEDIA_STREAM_KEY {
            assert_eq!(
                counters.offered_bytes_for_stream(key),
                0,
                "an out-of-range key must not spill into key {key}"
            );
            assert_eq!(
                counters.dropped_bytes_for_stream(key),
                0,
                "an out-of-range key must not spill into key {key}"
            );
        }
    }

    #[test]
    fn every_wire_key_has_its_own_slot() {
        let counters = WsCounters::new();
        for key in 1..=videocall_types::limits::MAX_MEDIA_STREAM_KEY {
            counters.record_offered(key, u64::from(key));
            counters.record_dropped_bytes(key, u64::from(key) * 10);
        }
        for key in 1..=videocall_types::limits::MAX_MEDIA_STREAM_KEY {
            assert_eq!(
                counters.offered_bytes_for_stream(key),
                u64::from(key),
                "key {key} shares its offered-bytes slot with another key",
            );
            assert_eq!(
                counters.dropped_bytes_for_stream(key),
                u64::from(key) * 10,
                "key {key} shares its dropped-bytes slot with another key",
            );
        }
    }

    #[test]
    fn an_overflowing_send_bills_offered_and_the_drop() {
        let counters = WsCounters::new();

        let outcome = bill_send(&counters, 2, 1500, WebSocket::OPEN, MAX_BUFFERED_AMOUNT + 1);

        assert_eq!(outcome, SendOutcome::Dropped);
        assert_eq!(
            counters.offered_bytes_for_stream(2),
            1500,
            "offered must be billed before the backpressure gate",
        );
        assert_eq!(counters.dropped_bytes_for_stream(2), 1500);
        assert_eq!(counters.drop_count_for_stream(2), 1);
        assert_eq!(counters.aggregate_drop_count(), 1);
        assert_eq!(
            counters.inactive_dropped_frames_for_stream(2),
            0,
            "backpressure must not be readable as a socket close",
        );
        assert_eq!(counters.inactive_dropped_bytes_for_stream(2), 0);
        assert_eq!(counters.inactive_dropped_frames_closing(), 0);
        assert_eq!(counters.inactive_dropped_frames_closed(), 0);
    }

    #[test]
    fn a_send_at_the_cap_is_delivered_and_bills_offered_only() {
        let counters = WsCounters::new();

        let outcome = bill_send(&counters, 3, 700, WebSocket::OPEN, MAX_BUFFERED_AMOUNT);

        assert_eq!(outcome, SendOutcome::Deliver);
        assert_eq!(counters.offered_bytes_for_stream(3), 700);
        assert_eq!(counters.dropped_bytes_for_stream(3), 0);
        assert_eq!(counters.aggregate_drop_count(), 0);
        assert_eq!(counters.inactive_dropped_frames_for_stream(3), 0);
        assert_eq!(counters.inactive_dropped_bytes_for_stream(3), 0);
        assert_eq!(counters.inactive_dropped_frames_closed(), 0);
    }

    #[test]
    fn a_closed_socket_bills_offered_and_the_inactive_discard_but_no_backpressure_drop() {
        let counters = WsCounters::new();

        let outcome = bill_send(&counters, 1, 640, WebSocket::CLOSED, 0);

        assert_eq!(outcome, SendOutcome::Inactive);
        assert_eq!(
            counters.offered_bytes_for_stream(1),
            640,
            "offered must be billed before the liveness gate",
        );
        assert_eq!(counters.inactive_dropped_frames_for_stream(1), 1);
        assert_eq!(counters.inactive_dropped_bytes_for_stream(1), 640);
        assert_eq!(counters.inactive_dropped_frames_closed(), 1);
        assert_eq!(counters.inactive_dropped_frames_closing(), 0);
        assert_eq!(
            counters.dropped_bytes_for_stream(1),
            0,
            "a socket close is not backpressure and must not enter the AQ shed signal",
        );
        assert_eq!(counters.drop_count_for_stream(1), 0);
        assert_eq!(counters.aggregate_drop_count(), 0);
    }

    #[test]
    fn a_closing_socket_bills_the_closing_state_not_the_closed_one() {
        let counters = WsCounters::new();

        let outcome = bill_send(&counters, 2, 320, WebSocket::CLOSING, 0);

        assert_eq!(outcome, SendOutcome::Inactive);
        assert_eq!(counters.inactive_dropped_frames_closing(), 1);
        assert_eq!(
            counters.inactive_dropped_frames_closed(),
            0,
            "a CLOSING socket must stay distinguishable from a CLOSED one",
        );
        assert_eq!(counters.inactive_dropped_bytes_for_stream(2), 320);
    }

    #[test]
    fn an_inactive_discard_stays_out_of_every_other_streams_counters() {
        let counters = WsCounters::new();

        bill_send(&counters, 2, 1200, WebSocket::CLOSED, 0);

        for key in 1..=videocall_types::limits::MAX_MEDIA_STREAM_KEY {
            if key == 2 {
                continue;
            }
            assert_eq!(
                counters.inactive_dropped_frames_for_stream(key),
                0,
                "video's inactive discard leaked into key {key}"
            );
            assert_eq!(
                counters.inactive_dropped_bytes_for_stream(key),
                0,
                "video's inactive bytes leaked into key {key}"
            );
        }
    }

    #[test]
    fn an_out_of_range_key_bills_the_state_split_but_no_stream_slot() {
        let counters = WsCounters::new();

        let outcome = bill_send(&counters, 0, 64, WebSocket::CLOSED, 0);

        assert_eq!(outcome, SendOutcome::Inactive);
        assert_eq!(
            counters.inactive_dropped_frames_closed(),
            1,
            "an unattributable discard must still be countable somewhere",
        );
        for key in 1..=videocall_types::limits::MAX_MEDIA_STREAM_KEY {
            assert_eq!(
                counters.inactive_dropped_frames_for_stream(key),
                0,
                "an out-of-range key must not spill into key {key}"
            );
            assert_eq!(counters.inactive_dropped_bytes_for_stream(key), 0);
        }
    }

    #[test]
    fn every_wire_key_has_its_own_inactive_slot() {
        let counters = WsCounters::new();

        for key in 1..=videocall_types::limits::MAX_MEDIA_STREAM_KEY {
            for _ in 0..key {
                bill_send(&counters, key, usize::from(key) * 10, WebSocket::CLOSED, 0);
            }
        }

        for key in 1..=videocall_types::limits::MAX_MEDIA_STREAM_KEY {
            assert_eq!(
                counters.inactive_dropped_frames_for_stream(key),
                u64::from(key),
                "key {key} shares its inactive-frame slot with another key",
            );
            assert_eq!(
                counters.inactive_dropped_bytes_for_stream(key),
                u64::from(key) * u64::from(key) * 10,
                "key {key} shares its inactive-bytes slot with another key",
            );
        }
    }

    #[test]
    fn a_connecting_socket_still_delivers() {
        let counters = WsCounters::new();

        assert_eq!(
            bill_send(&counters, 1, 10, WebSocket::CONNECTING, 0),
            SendOutcome::Deliver
        );
        assert_eq!(counters.inactive_dropped_frames_for_stream(1), 0);
        assert_eq!(counters.inactive_dropped_frames_closing(), 0);
        assert_eq!(counters.inactive_dropped_frames_closed(), 0);

        assert_eq!(
            bill_send(&counters, 1, 10, WebSocket::CLOSING, 0),
            SendOutcome::Inactive
        );
        assert_eq!(counters.inactive_dropped_frames_for_stream(1), 1);
        assert_eq!(counters.inactive_dropped_bytes_for_stream(1), 10);
        assert_eq!(counters.inactive_dropped_frames_closing(), 1);
    }

    /// The only test that touches the process-global `WS_COUNTERS`; every other
    /// test here owns its counters, so these deltas cannot race. Four distinct
    /// magnitudes, so a getter reading the wrong counter fails.
    #[test]
    fn public_getters_read_their_own_global_counter() {
        let offered_before = websocket_offered_bytes_for_stream(3);
        let dropped_before = websocket_dropped_bytes_for_stream(3);
        let drops_before = websocket_drop_count_for_stream(3);
        let aggregate_before = websocket_drop_count();

        let inactive_frames_before = websocket_inactive_dropped_frames_for_stream(3);
        let inactive_bytes_before = websocket_inactive_dropped_bytes_for_stream(3);
        let closing_before = websocket_inactive_dropped_frames_closing();
        let closed_before = websocket_inactive_dropped_frames_closed();

        WS_COUNTERS.record_offered(3, 900);
        WS_COUNTERS.record_dropped_bytes(3, 400);
        WS_COUNTERS.record_drop(3);
        // Out of range: lands in the aggregate only, so its delta differs from key 3's.
        WS_COUNTERS.record_drop(99);
        WS_COUNTERS.record_inactive_drop(3, 250, WebSocket::CLOSED);
        WS_COUNTERS.record_inactive_drop(3, 150, WebSocket::CLOSING);
        WS_COUNTERS.record_inactive_drop(3, 50, WebSocket::CLOSING);

        assert_eq!(websocket_offered_bytes_for_stream(3) - offered_before, 900);
        assert_eq!(websocket_dropped_bytes_for_stream(3) - dropped_before, 400);
        assert_eq!(websocket_drop_count_for_stream(3) - drops_before, 1);
        assert_eq!(websocket_drop_count() - aggregate_before, 2);
        assert_eq!(
            websocket_inactive_dropped_frames_for_stream(3) - inactive_frames_before,
            3
        );
        assert_eq!(
            websocket_inactive_dropped_bytes_for_stream(3) - inactive_bytes_before,
            450
        );
        assert_eq!(
            websocket_inactive_dropped_frames_closing() - closing_before,
            2,
            "the CLOSING getter must not read the CLOSED counter",
        );
        assert_eq!(
            websocket_inactive_dropped_frames_closed() - closed_before,
            1
        );
    }
}
