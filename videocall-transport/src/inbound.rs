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

use crate::downlink_stream::{classify_prelude, Prelude, StreamKey};
use crate::read_loop_lag::DatagramReadLoopLagTracker;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use js_sys::Uint8Array;
use log::{debug, error, warn};
use protobuf::Message;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex as StdMutex;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::ReadableStreamDefaultReader;
use web_sys::WebTransportBidirectionalStream;
use web_sys::WebTransportReceiveStream;

/// When the TRANSPORT received a packet, in the `performance.now()` domain of
/// whichever thread stamped it.
///
/// On the in-page and WebSocket paths that is main's. On the Worker path it is
/// the Worker's, and [`crate::worker_session`] converts it with
/// [`crate::clock::convert_to_domain`] before dispatching the frame, so
/// everything above the transport sees one domain.
#[derive(Copy, Clone, Debug, PartialEq, PartialOrd)]
pub struct ReceivedAtMs(pub f64);

/// One complete inbound frame as the transport hands it upward.
pub struct InboundFrame {
    pub bytes: Vec<u8>,
    pub lane: InboundLane,
    pub received_at: ReceivedAtMs,
}

/// Which downlink lane delivered an inbound packet.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum InboundLane {
    Reliable,
    Datagram,
}

/// Maximum size for an inbound stream buffer (4 MB), matching the server's MAX_FRAME_SIZE.
const MAX_INBOUND_STREAM_SIZE: usize = 4_000_000;

thread_local! {
    static KEY_DONE: JsString = JsString::from("done");
    static KEY_VALUE: JsString = JsString::from("value");
}

fn reflect_done(
    result: &wasm_bindgen::JsValue,
) -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue> {
    KEY_DONE.with(|key| Reflect::get(result, key))
}

fn reflect_value(
    result: &wasm_bindgen::JsValue,
) -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue> {
    KEY_VALUE.with(|key| Reflect::get(result, key))
}

/// How long a queued inbound unistream waits for the reader of ITS OWN key to
/// exit before it starts anyway; covers the superseded stream's RESET_STREAM.
/// stream whose key has no reader running never waits.
const READER_HANDOVER_GRACE_MS: u32 =
    (videocall_types::wt_downlink::WT_UNISTREAM_WRITE_DEADLINE_MS / 2) as u32;

const _: () = assert!(
    READER_HANDOVER_GRACE_MS == 500,
    "the hand-over grace is no longer 500 ms: either the relay's shed deadline \
     moved and halving it is no longer right, or this line was re-pinned to a \
     literal. Both need a human."
);

/// Hard ceiling on readers emitting into one session's callback. Each key can
const MAX_CONCURRENT_INBOUND_READERS: usize = 128;

/// Receiver-wide ceiling on the framing buffers of ALL inbound streams, 32 MiB.
const MAX_INBOUND_SESSION_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum BufferBound {
    Stream,
    Session,
}

/// The one place both inbound buffer ceilings are decided.
fn buffer_over_budget(stream_bytes: usize, session_bytes: usize) -> Option<BufferBound> {
    if stream_bytes > MAX_INBOUND_STREAM_SIZE {
        Some(BufferBound::Stream)
    } else if session_bytes > MAX_INBOUND_SESSION_BYTES {
        Some(BufferBound::Session)
    } else {
        None
    }
}

/// Live total of the UNREAD bytes buffered on one session — not resident
/// capacity, which `PendingFrames` reclaims without shrinking.
#[derive(Debug, Default)]
struct InboundByteMeter {
    total: std::cell::Cell<usize>,
}

impl InboundByteMeter {
    /// Replace — not add to — one stream's contribution, and report the total.
    fn recharge(&self, charged: &mut usize, now: usize) -> usize {
        let total = self
            .total
            .get()
            .saturating_sub(*charged)
            .saturating_add(now);
        self.total.set(total);
        *charged = now;
        total
    }

    fn release(&self, charged: usize) {
        self.total.set(self.total.get().saturating_sub(charged));
    }

    #[cfg(test)]
    fn total(&self) -> usize {
        self.total.get()
    }
}

/// Warn on the first occurrence only; later ones are counted, not logged.
#[derive(Debug, Default)]
struct OnceLog(std::cell::Cell<bool>);

impl OnceLog {
    fn should_log(&self) -> bool {
        !self.0.replace(true)
    }
}

/// Everything one WebTransport session's inbound readers share.
#[derive(Default)]
pub struct InboundSession {
    gate: RefCell<InboundReaderGate<ParkedStream>>,
    bytes: Rc<InboundByteMeter>,
    over_budget_logged: OnceLog,
    ceiling_refusal_logged: OnceLog,
}

#[derive(Debug, PartialEq, Eq)]
enum Admission<S> {
    Start(S),
    /// `superseded` is the same-key stream this replaced in the queue of one.
    Queued {
        seq: u64,
        superseded: Option<S>,
    },
    /// Past [`MAX_CONCURRENT_INBOUND_READERS`]; the caller retires the stream.
    Refused(S),
}

/// One key's slot. Present in the map exactly while a reader runs for it, so
/// an absent key is one that starts immediately.
#[derive(Debug)]
struct KeyState<S> {
    active_readers: usize,
    /// Queue of ONE: a second replacement supersedes the first.
    pending: Option<(u64, S)>,
}

/// Schedules one session's inbound-unistream readers PER KEY (#2723): other
/// keys never wait, same-key hand-over keeps #2722's ordering.
#[derive(Debug)]
struct InboundReaderGate<S> {
    keys: std::collections::HashMap<StreamKey, KeyState<S>>,
    active_readers: usize,
    next_seq: u64,
    forced_starts: u64,
    evicted: u64,
    /// Streams turned away at the reader ceiling. A canary: a conforming relay
    /// cannot open enough streams to reach it, so the expected value is zero.
    refused: u64,
}

impl<S> Default for InboundReaderGate<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> InboundReaderGate<S> {
    fn new() -> Self {
        Self {
            keys: std::collections::HashMap::new(),
            active_readers: 0,
            next_seq: 0,
            forced_starts: 0,
            evicted: 0,
            refused: 0,
        }
    }

    fn admit(&mut self, key: StreamKey, stream: S) -> Admission<S> {
        if let Some(state) = self.keys.get_mut(&key) {
            let seq = self.next_seq;
            self.next_seq += 1;
            let superseded = state.pending.replace((seq, stream)).map(|(_, s)| s);
            if superseded.is_some() {
                self.evicted += 1;
            }
            return Admission::Queued { seq, superseded };
        }
        if self.active_readers >= MAX_CONCURRENT_INBOUND_READERS {
            self.refused += 1;
            return Admission::Refused(stream);
        }
        self.active_readers += 1;
        self.keys.insert(
            key,
            KeyState {
                active_readers: 1,
                pending: None,
            },
        );
        Admission::Start(stream)
    }

    /// Take over this key's queued stream if this was its last running reader.
    fn reader_exited(&mut self, key: StreamKey) -> Option<S> {
        self.active_readers = self.active_readers.saturating_sub(1);
        let state = self.keys.get_mut(&key)?;
        state.active_readers = state.active_readers.saturating_sub(1);
        if state.active_readers > 0 {
            return None;
        }
        match state.pending.take() {
            Some((_, next)) => {
                state.active_readers = 1;
                self.active_readers += 1;
                Some(next)
            }
            None => {
                self.keys.remove(&key);
                None
            }
        }
    }

    /// Release the stream admitted as `seq`, and ONLY that one, whose grace
    /// expired while its key's reader still runs. `Gone` once that entry is
    /// gone (taken or superseded), which stops a stale timer from releasing a
    /// younger stream.
    ///
    /// Past [`MAX_CONCURRENT_INBOUND_READERS`] it still leaves the queue, handed
    /// back as `Refused`.
    fn force_start(&mut self, key: StreamKey, seq: u64) -> ForceStart<S> {
        let at_ceiling = self.active_readers >= MAX_CONCURRENT_INBOUND_READERS;
        let outcome = match self.keys.get_mut(&key) {
            Some(state) if state.pending.as_ref().map(|(s, _)| *s) == Some(seq) => {
                match state.pending.take() {
                    Some((_, stream)) if at_ceiling => ForceStart::Refused(stream),
                    Some((_, stream)) => {
                        state.active_readers += 1;
                        ForceStart::Started(stream)
                    }
                    None => ForceStart::Gone,
                }
            }
            _ => ForceStart::Gone,
        };
        match &outcome {
            ForceStart::Started(_) => {
                self.active_readers += 1;
                self.forced_starts += 1;
            }
            ForceStart::Refused(_) => self.refused += 1,
            ForceStart::Gone => {}
        }
        outcome
    }
}

/// What a grace timer's expiry did with the stream it was armed for.
#[derive(Debug, PartialEq, Eq)]
enum ForceStart<S> {
    Started(S),
    /// Already taken or superseded; there is nothing to release.
    Gone,
    /// Turned away at the ceiling and out of the queue; the caller cancels it.
    Refused(S),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MessageType {
    Datagram,
    UnidirectionalStream,
    BidirectionalStream,
}

impl MessageType {
    pub const fn lane(self) -> InboundLane {
        match self {
            MessageType::Datagram => InboundLane::Datagram,
            MessageType::UnidirectionalStream | MessageType::BidirectionalStream => {
                InboundLane::Reliable
            }
        }
    }
}

/// Reads from a **persistent length-prefixed unidirectional QUIC stream**
/// (server -> client) and emits each complete frame to `on_inbound_media`.
///
/// The server keeps the stream open and prefixes every packet with a 4-byte
/// big-endian length header (see `actix-api/src/webtransport/bridge.rs::
/// spawn_unistream_writer`).  This reader accumulates chunks across QUIC
/// chunk boundaries and extracts complete `[length][payload]` frames as
/// they arrive, emitting each immediately.
///
/// **Per-key hand-over (#2723).** A v1 header names the stream's key. Other
/// keys read concurrently; a SAME-key replacement is queued and picked up by
/// that key's reader when its loop ends, so no frame of the new stream
/// precedes the last frame of the old one (#2722). No header means a legacy
/// relay: one key, the #2722 path unchanged.
pub fn handle_unidirectional_stream(
    stream: WebTransportReceiveStream,
    on_frame: Callback<(StreamKey, Vec<u8>)>,
    session: InboundSessionRef,
) {
    if stream.is_undefined() {
        debug!("stream is undefined");
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        let mut parked = ParkedStream::new(stream.get_reader().unchecked_into(), &session);
        let Some(key) = classify_inbound_stream(&mut parked).await else {
            return;
        };
        let admission = session.gate.borrow_mut().admit(key, parked);
        match admission {
            Admission::Start(parked) => spawn_unistream_reader(key, parked, on_frame, session),
            Admission::Queued { seq, superseded } => {
                if let Some(parked) = superseded {
                    retire_stream(parked, "superseded before it was read", true);
                }
                arm_handover_grace(key, seq, on_frame, session);
            }
            Admission::Refused(parked) => retire_stream(
                parked,
                "turned away at the concurrent-reader ceiling",
                session.ceiling_refusal_logged.should_log(),
            ),
        }
    });
}

/// Identify the key and CONSUME a v1 header; `None` when not yet decisive.
fn take_classified_key(pending: &mut PendingFrames) -> Option<StreamKey> {
    match classify_prelude(pending.unread()) {
        Prelude::Header { key, consumed } => {
            pending.skip(consumed);
            Some(key)
        }
        Prelude::Legacy => Some(StreamKey::Legacy),
        Prelude::NeedMore => None,
    }
}

/// Read until the head of the stream identifies its key, consuming a v1 header
/// if one is there. `None` when the stream ended or failed first, in which case
/// no key was ever claimed.
async fn classify_inbound_stream(parked: &mut ParkedStream) -> Option<StreamKey> {
    loop {
        match take_classified_key(&mut parked.pending) {
            Some(key) => return Some(key),
            None => match read_chunk(&parked.reader, &mut parked.pending).await {
                ReadStep::Chunk => {
                    parked.recharge();
                    continue;
                }
                ReadStep::Failed => return None,
                ReadStep::Done => {
                    if !parked.pending.is_empty() {
                        warn!(
                            "Framed unistream EOF with {} unconsumed bytes before its header; dropping",
                            parked.pending.len()
                        );
                    }
                    return None;
                }
            },
        }
    }
}

/// Discard a stream the gate did not take: `cancel()` releases its
/// flow-control window, and it books a reset.
fn retire_stream(parked: ParkedStream, reason: &str, log: bool) {
    crate::webtransport::record_inbound_unistream_reset();
    if log {
        warn!(
            "inbound unistream {reason}; {} discarded so far",
            crate::webtransport::inbound_unistream_reset_count()
        );
    }
    cancel_inbound_stream(&parked.reader);
}

fn cancel_inbound_stream(reader: &ReadableStreamDefaultReader) {
    let cancelled = reader.cancel();
    wasm_bindgen_futures::spawn_local(async move {
        let _ = JsFuture::from(cancelled).await;
    });
}

pub type InboundSessionRef = Rc<InboundSession>;

/// A locked inbound stream plus whatever bytes arrived with its header. The
#[derive(Debug)]
struct ParkedStream {
    reader: ReadableStreamDefaultReader,
    pending: PendingFrames,
    meter: Rc<InboundByteMeter>,
    charged: usize,
}

impl ParkedStream {
    fn new(reader: ReadableStreamDefaultReader, session: &InboundSession) -> Self {
        Self {
            reader,
            pending: PendingFrames::default(),
            meter: session.bytes.clone(),
            charged: 0,
        }
    }

    fn recharge(&mut self) -> usize {
        self.meter.recharge(&mut self.charged, self.pending.len())
    }
}

impl Drop for ParkedStream {
    /// EOF, reset, cancel or teardown: every ending returns the charge here.
    fn drop(&mut self) {
        self.meter.release(self.charged);
    }
}

/// Read `parked`, then every same-key stream queued behind it, in ONE task.
fn spawn_unistream_reader(
    key: StreamKey,
    parked: ParkedStream,
    on_frame: Callback<(StreamKey, Vec<u8>)>,
    session: InboundSessionRef,
) {
    let keyed = on_frame.clone();
    let callback = Callback::from(move |bytes: Vec<u8>| keyed.emit((key, bytes)));
    let audio_lane = is_audio_lane(key);
    wasm_bindgen_futures::spawn_local(async move {
        let mut current = parked;
        loop {
            read_framed_unistream(&mut current, &callback, &session, audio_lane).await;
            let next = session.gate.borrow_mut().reader_exited(key);
            match next {
                Some(queued) => current = queued,
                None => break,
            }
        }
    });
}

/// Start the stream admitted as `seq` once ITS OWN grace expires, so a wedged
/// reader cannot hold its key's stream shut.
fn arm_handover_grace(
    key: StreamKey,
    seq: u64,
    on_frame: Callback<(StreamKey, Vec<u8>)>,
    session: InboundSessionRef,
) {
    wasm_bindgen_futures::spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(READER_HANDOVER_GRACE_MS).await;
        let forced = session.gate.borrow_mut().force_start(key, seq);
        match forced {
            ForceStart::Started(parked) => {
                warn!(
                    "inbound unistream reader for {key:?} did not exit within {}ms; starting the queued stream alongside it",
                    READER_HANDOVER_GRACE_MS
                );
                spawn_unistream_reader(key, parked, on_frame, session);
            }
            ForceStart::Refused(parked) => retire_stream(
                parked,
                "refused at the concurrent-reader ceiling when its grace expired",
                session.ceiling_refusal_logged.should_log(),
            ),
            ForceStart::Gone => {}
        }
    });
}

/// Worst gap between successive `.read()` resolutions on the #2724 class-3
/// reliable AUDIO stream, plus the session high-water of the same quantity.
struct AudioLaneLag {
    /// Same arithmetic as the datagram loop's tracker, drained per health tick.
    window: DatagramReadLoopLagTracker,
    /// Never drained, so the diagnostics read-back cannot steal a window.
    session_max_ms: f64,
}

impl AudioLaneLag {
    const fn new() -> Self {
        Self {
            window: DatagramReadLoopLagTracker::new(),
            session_max_ms: 0.0,
        }
    }
}

static AUDIO_LANE_LAG: StdMutex<AudioLaneLag> = StdMutex::new(AudioLaneLag::new());

/// Record one audio-lane `.read()` resolution at `now_ms`.
pub fn record_audio_lane_read(now_ms: f64) {
    if let Ok(mut lag) = AUDIO_LANE_LAG.lock() {
        let gap = lag.window.record(now_ms);
        if gap > lag.session_max_ms {
            lag.session_max_ms = gap;
        }
    }
}

/// 0.0 when no audio stream has been read, which lets the gauge recover.
pub fn take_audio_lane_max_gap_ms() -> f64 {
    let in_page = AUDIO_LANE_LAG
        .lock()
        .map(|mut lag| lag.window.take_max_gap_ms())
        .unwrap_or(0.0);
    in_page.max(crate::worker_session::take_audio_lane_max_gap_ms())
}

/// Non-draining read of the session high-water, for the diagnostics seam.
pub fn peek_audio_lane_session_max_gap_ms() -> f64 {
    let in_page = AUDIO_LANE_LAG
        .lock()
        .map(|lag| lag.session_max_ms)
        .unwrap_or(0.0);
    in_page.max(crate::worker_session::audio_lane_session_max_gap_ms())
}

/// So a reconnect does not book the downtime as one enormous gap (#2031).
pub fn reset_audio_lane_anchor() {
    if let Ok(mut lag) = AUDIO_LANE_LAG.lock() {
        lag.window.reset_anchor();
        lag.session_max_ms = 0.0;
    }
}

/// Record one `.read()` resolution, but only for the #2724 audio lane.
pub fn record_read_if_audio_lane(audio_lane: bool, now_ms: impl FnOnce() -> f64) {
    if audio_lane {
        record_audio_lane_read(now_ms());
    }
}

/// True when this key is the #2724 receiver-scoped audio stream.
fn is_audio_lane(key: StreamKey) -> bool {
    matches!(
        key,
        StreamKey::V1 {
            class: crate::downlink_stream::stream_class::AUDIO,
            ..
        }
    )
}

/// Outcome of one `read()` on an inbound stream.
#[derive(Debug, PartialEq, Eq)]
enum ReadStep {
    Chunk,
    Done,
    Failed,
}

/// One `read()`, appending whatever it yielded to `pending`.
async fn read_chunk(reader: &ReadableStreamDefaultReader, pending: &mut PendingFrames) -> ReadStep {
    match JsFuture::from(reader.read()).await {
        Err(e) => {
            crate::webtransport::record_inbound_unistream_reset();
            warn!("Unistream read error: {:?}", e);
            ReadStep::Failed
        }
        Ok(result) => {
            let done = reflect_done(&result)
                .map(|v| v.unchecked_into::<Boolean>().is_truthy())
                .unwrap_or(true);

            if let Ok(value) = reflect_value(&result) {
                if !value.is_undefined() {
                    let chunk: Uint8Array = value.unchecked_into();
                    pending.append_chunk(&chunk);
                }
            }

            if done {
                ReadStep::Done
            } else {
                ReadStep::Chunk
            }
        }
    }
}

/// Emit each complete frame until the stream ends. Drains before its first
async fn read_framed_unistream(
    parked: &mut ParkedStream,
    callback: &Callback<Vec<u8>>,
    session: &InboundSession,
    audio_lane: bool,
) {
    let mut finished = false;
    loop {
        if let FrameDrain::CorruptLength(len) =
            drain_complete_frames(&mut parked.pending, |payload| callback.emit(payload))
        {
            error!(
                "Frame length {} invalid (max {}), dropping framed unistream",
                len, MAX_INBOUND_STREAM_SIZE
            );
            crate::webtransport::record_inbound_unistream_reset();
            cancel_inbound_stream(&parked.reader);
            return;
        }

        if finished {
            if !parked.pending.is_empty() {
                warn!(
                    "Framed unistream EOF with {} unconsumed bytes (truncated frame); dropping",
                    parked.pending.len()
                );
            }
            return;
        }

        let session_bytes = parked.recharge();
        match buffer_over_budget(parked.pending.len(), session_bytes) {
            Some(BufferBound::Stream) => {
                error!(
                    "Inbound unistream buffer exceeded {} bytes (got {}), dropping stream",
                    MAX_INBOUND_STREAM_SIZE,
                    parked.pending.len()
                );
                crate::webtransport::record_inbound_unistream_reset();
                cancel_inbound_stream(&parked.reader);
                return;
            }
            Some(BufferBound::Session) => {
                crate::webtransport::record_inbound_unistream_reset();
                if session.over_budget_logged.should_log() {
                    error!(
                        "Inbound framing buffers across all keys exceeded {} bytes (got {}); \
                         cancelling this stream. Later occurrences are counted, not logged",
                        MAX_INBOUND_SESSION_BYTES, session_bytes
                    );
                }
                cancel_inbound_stream(&parked.reader);
                return;
            }
            None => {}
        }

        let step = read_chunk(&parked.reader, &mut parked.pending).await;
        record_read_if_audio_lane(audio_lane, crate::clock::now_ms);
        match step {
            ReadStep::Chunk => {}
            ReadStep::Done => finished = true,
            ReadStep::Failed => return,
        }
    }
}
/// Inbound read buffer with a consumed-prefix cursor. Taking a frame only
/// advances `consumed`.
#[derive(Debug, Default)]
struct PendingFrames {
    buf: Vec<u8>,
    consumed: usize,
}

impl PendingFrames {
    fn len(&self) -> usize {
        self.buf.len() - self.consumed
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn unread(&self) -> &[u8] {
        &self.buf[self.consumed..]
    }

    fn append_chunk(&mut self, chunk: &Uint8Array) {
        self.compact();
        append_uint8_array_to_vec(&mut self.buf, chunk);
    }

    /// Drop `n` unread bytes without copying them, for a frame consumed by the
    /// transport itself rather than emitted upward.
    fn skip(&mut self, n: usize) {
        self.consumed = (self.consumed + n).min(self.buf.len());
    }

    /// One memcpy. Caller must have checked `self.len() >= 4 + len`.
    fn take_frame(&mut self, len: usize) -> Vec<u8> {
        let start = self.consumed + 4;
        let payload = self.buf[start..start + len].to_vec();
        self.consumed = start + len;
        payload
    }

    fn compact(&mut self) {
        if self.consumed > 0 && self.consumed >= self.len() {
            self.buf.drain(..self.consumed);
            self.consumed = 0;
        }
    }

    #[cfg(test)]
    fn extend_for_test(&mut self, bytes: &[u8]) {
        self.compact();
        self.buf.extend_from_slice(bytes);
    }
}

/// Why [`drain_complete_frames`] stopped. `CorruptLength` is unrecoverable.
#[derive(Debug, PartialEq, Eq)]
enum FrameDrain {
    NeedMore,
    CorruptLength(usize),
}

/// Emit every complete `[4-byte BE length][payload]` frame at the head of
fn drain_complete_frames(pending: &mut PendingFrames, mut emit: impl FnMut(Vec<u8>)) -> FrameDrain {
    while pending.len() >= 4 {
        let head = pending.unread();
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if len == 0 || len > MAX_INBOUND_STREAM_SIZE {
            return FrameDrain::CorruptLength(len);
        }
        if pending.len() < 4 + len {
            break; // need more data from the next read
        }
        emit(pending.take_frame(len));
    }
    FrameDrain::NeedMore
}

pub fn handle_bidirectional_stream(
    stream: WebTransportBidirectionalStream,
    on_frame: Callback<Vec<u8>>,
) {
    debug!("OnBidiStream: {:?}", &stream);
    if stream.is_undefined() {
        debug!("stream is undefined");
        return;
    }
    let readable: ReadableStreamDefaultReader = stream.readable().get_reader().unchecked_into();
    let callback = on_frame;
    wasm_bindgen_futures::spawn_local(async move {
        let mut buffer: Vec<u8> = vec![];
        loop {
            debug!("reading from stream");
            let read_result = JsFuture::from(readable.read()).await;

            match read_result {
                Err(_) => {
                    break;
                }
                Ok(result) => {
                    let done = match reflect_done(&result) {
                        Ok(val) => val.unchecked_into::<Boolean>(),
                        Err(e) => {
                            warn!("Failed to read 'done' from bidistream result: {:?}", e);
                            break;
                        }
                    };
                    let value = match reflect_value(&result) {
                        Ok(val) => val,
                        Err(e) => {
                            warn!("Failed to read 'value' from bidistream result: {:?}", e);
                            break;
                        }
                    };
                    if !value.is_undefined() {
                        let value: Uint8Array = value.unchecked_into();
                        append_uint8_array_to_vec(&mut buffer, &value);
                        if buffer.len() > MAX_INBOUND_STREAM_SIZE {
                            error!(
                                "Inbound bidistream exceeded {} bytes (got {}), dropping",
                                MAX_INBOUND_STREAM_SIZE,
                                buffer.len()
                            );
                            break;
                        }
                    }
                    if done.is_truthy() {
                        callback.emit(buffer);
                        break;
                    }
                }
            }
        }
        debug!("readable stream closed");
    });
}

pub fn emit_packet(
    bytes: Vec<u8>,
    message_type: MessageType,
    received_at: ReceivedAtMs,
    callback: Callback<(PacketWrapper, InboundLane, ReceivedAtMs)>,
) {
    match PacketWrapper::parse_from_bytes(&bytes) {
        Ok(media_packet) => callback.emit((media_packet, message_type.lane(), received_at)),
        Err(_) => {
            let message_type = format!("{message_type:?}");
            error!("failed to parse media packet {message_type:?}");
        }
    }
}

fn append_uint8_array_to_vec(rust_vec: &mut Vec<u8>, js_array: &Uint8Array) {
    let start = rust_vec.len();
    let len = js_array.length() as usize;
    rust_vec.reserve(len);
    js_array.copy_to_uninit(&mut rust_vec.spare_capacity_mut()[..len]);
    // SAFETY: `reserve(len)` guarantees `len` spare slots and `copy_to_uninit`
    // initialised exactly that prefix (it panics rather than under-fill).
    unsafe { rust_vec.set_len(start + len) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    #[test]
    fn every_webtransport_primitive_maps_to_its_lane() {
        assert_eq!(
            MessageType::Datagram.lane(),
            InboundLane::Datagram,
            "heartbeats and RTT echoes arrive as datagrams — and so does audio \
             from a relay predating #2724 — and must NOT count as reliable-lane \
             liveness"
        );
        assert_eq!(
            MessageType::UnidirectionalStream.lane(),
            InboundLane::Reliable,
            "the persistent unistream carries video, screen, control and — from \
             a #2724 relay — audio"
        );
        assert_eq!(
            MessageType::BidirectionalStream.lane(),
            InboundLane::Reliable,
            "a bidirectional stream is reliable and ordered like the unistream"
        );
    }

    use crate::downlink_stream::{stream_class, WT_MAX_DOWNLINK_STREAMS};

    fn video(session: u64) -> StreamKey {
        StreamKey::V1 {
            class: stream_class::PUBLISHER,
            publisher_session_id: session,
            media_kind: 1,
        }
    }

    const CONTROL: StreamKey = StreamKey::V1 {
        class: stream_class::CONTROL,
        publisher_session_id: 0,
        media_kind: 0,
    };

    fn queue(
        gate: &mut InboundReaderGate<&'static str>,
        key: StreamKey,
        stream: &'static str,
    ) -> u64 {
        match gate.admit(key, stream) {
            Admission::Queued { seq, superseded } => {
                assert!(superseded.is_none(), "{stream} should not have evicted one");
                seq
            }
            other => panic!("{stream} was expected to queue, got {other:?}"),
        }
    }

    #[test]
    fn streams_with_distinct_keys_all_start_immediately() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(video(1), "alice"), Admission::Start("alice"));
        assert_eq!(
            gate.admit(video(2), "bob"),
            Admission::Start("bob"),
            "a second publisher must not queue behind the first"
        );
        assert_eq!(
            gate.admit(CONTROL, "control"),
            Admission::Start("control"),
            "the receiver-scoped control stream is its own key"
        );
        assert_eq!(gate.active_readers, 3);
        assert_eq!(gate.evicted, 0, "nothing may be cancelled across keys");
        assert_eq!(gate.forced_starts, 0, "and nothing waits out a grace");
    }

    #[test]
    fn the_overflow_class_is_scheduled_like_any_other_key() {
        let overflow = StreamKey::V1 {
            class: stream_class::OVERFLOW,
            publisher_session_id: 0,
            media_kind: 0,
        };
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(CONTROL, "control"), Admission::Start("control"));
        assert_eq!(
            gate.admit(overflow, "overflow"),
            Admission::Start("overflow"),
            "overflow must not queue behind control, or bulk unattributable \
             media is back in front of #2718's Critical control"
        );
        assert_eq!(gate.admit(video(1), "camera"), Admission::Start("camera"));
        assert_eq!(gate.active_readers, 3);
        assert_eq!(gate.evicted, 0);

        queue(&mut gate, overflow, "replacement");
        assert_eq!(
            gate.reader_exited(overflow),
            Some("replacement"),
            "and hands over within its own key exactly like a publisher key"
        );
    }

    const AUDIO: StreamKey = StreamKey::V1 {
        class: stream_class::AUDIO,
        publisher_session_id: 0,
        media_kind: 2,
    };

    #[test]
    fn the_audio_class_reads_concurrently_with_every_other_lane() {
        let overflow = StreamKey::V1 {
            class: stream_class::OVERFLOW,
            publisher_session_id: 0,
            media_kind: 0,
        };
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(CONTROL, "control"), Admission::Start("control"));
        assert_eq!(
            gate.admit(overflow, "overflow"),
            Admission::Start("overflow")
        );
        assert_eq!(gate.admit(video(1), "camera"), Admission::Start("camera"));
        assert_eq!(
            gate.admit(AUDIO, "audio"),
            Admission::Start("audio"),
            "audio must not queue behind a camera stream or a join's \
             keyframe-request burst on control — waiting on another lane is the \
             head-of-line blocking #2724 exists to remove"
        );
        assert_eq!(gate.active_readers, 4);
        assert_eq!(gate.evicted, 0, "and nothing may be cancelled across keys");
        assert_eq!(gate.forced_starts, 0, "nor wait out a grace");
    }

    #[test]
    fn an_audio_stream_reopened_after_a_reset_starts_at_once_and_repeats_nothing() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(AUDIO, "first"), Admission::Start("first"));
        assert_eq!(
            gate.reader_exited(AUDIO),
            None,
            "the read error ends that reader with nothing queued behind it"
        );
        assert!(
            gate.keys.is_empty(),
            "and retires the key rather than holding it open"
        );
        assert_eq!(
            gate.admit(AUDIO, "reopened"),
            Admission::Start("reopened"),
            "so the replacement starts now: waiting out the 500ms hand-over \
             grace for a reader already gone would be a real audio gap"
        );
        assert_eq!(gate.forced_starts, 0);

        let packet = vec![0x08u8, 0x03, 0x22, 0x02, 0xAB, 0xCD];
        let whole = framed(&packet);
        let mut interrupted = PendingFrames::default();
        interrupted.extend_for_test(&whole[..whole.len() - 2]);
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        assert_eq!(
            drain_complete_frames(&mut interrupted, |p| emitted.push(p)),
            FrameDrain::NeedMore
        );
        assert!(
            emitted.is_empty(),
            "the reset stream's half-written frame must never reach the callback"
        );

        let mut resent = PendingFrames::default();
        resent.extend_for_test(&whole);
        drain_complete_frames(&mut resent, |p| emitted.push(p));
        assert_eq!(
            emitted,
            vec![packet],
            "so the re-send is the packet's first delivery, not a duplicate"
        );
    }

    fn reader_turn(
        pending: &mut PendingFrames,
        meter: &InboundByteMeter,
        charged: &mut usize,
        chunk: &[u8],
        emit: &mut impl FnMut(Vec<u8>),
    ) -> (Option<BufferBound>, usize) {
        assert_eq!(
            drain_complete_frames(pending, &mut *emit),
            FrameDrain::NeedMore,
            "a well-formed audio stream never hits the corrupt-length break"
        );
        let session_bytes = meter.recharge(charged, pending.len());
        let verdict = buffer_over_budget(pending.len(), session_bytes);
        pending.extend_for_test(chunk);
        (verdict, session_bytes)
    }

    #[test]
    fn a_five_second_stall_delivers_every_audio_frame_in_order_and_complete_at_the_transport() {
        const SPEAKERS: usize = 25;
        const PACKETS_PER_SEC: usize = 50;
        const STALL_SECS: usize = 5;
        const OPUS_WIRE_BYTES: usize = 110;
        const CHUNK: usize = 1400;
        let total = SPEAKERS * PACKETS_PER_SEC * STALL_SECS;

        let mut header = 15u32.to_be_bytes().to_vec();
        header.extend_from_slice(b"VCDS\x01\x03");
        header.extend_from_slice(&0u64.to_be_bytes());
        header.push(2);

        let sent: Vec<PacketWrapper> = (0..total)
            .map(|i| {
                let mut packet = PacketWrapper::new();
                packet.session_id = i as u64;
                packet.data = vec![(i % 251) as u8; OPUS_WIRE_BYTES];
                packet
            })
            .collect();
        let mut wire = header.clone();
        let mut widest_frame = 0usize;
        for packet in &sent {
            let bytes = packet.write_to_bytes().unwrap();
            widest_frame = widest_frame.max(4 + bytes.len());
            wire.extend_from_slice(&framed(&bytes));
        }

        let mut pending = PendingFrames::default();
        pending.extend_for_test(&wire[..CHUNK]);
        let Prelude::Header { key, consumed } = classify_prelude(pending.unread()) else {
            panic!("the relay's class-3 header must classify as a v1 header");
        };
        assert_eq!(key, AUDIO);
        pending.skip(consumed);

        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(key, "audio"), Admission::Start("audio"));

        let delivered: Rc<RefCell<Vec<(PacketWrapper, InboundLane)>>> = Rc::default();
        let sink = delivered.clone();
        let on_inbound_media = Callback::from(
            move |(packet, lane, _at): (PacketWrapper, InboundLane, ReceivedAtMs)| {
                sink.borrow_mut().push((packet, lane))
            },
        );
        let drained = std::cell::Cell::new(0usize);
        let mut emit = |payload: Vec<u8>| {
            drained.set(drained.get() + 1);
            emit_packet(
                payload,
                MessageType::UnidirectionalStream,
                ReceivedAtMs(0.0),
                on_inbound_media.clone(),
            )
        };

        let meter = InboundByteMeter::default();
        let mut charged = 0usize;
        let mut peak_unread = 0usize;
        for chunk in wire[CHUNK..].chunks(CHUNK) {
            let (verdict, _) = reader_turn(&mut pending, &meter, &mut charged, chunk, &mut emit);
            assert_eq!(
                verdict, None,
                "a stalled receiver's audio backlog must never trip an inbound \
                 byte guard; cancelling here would reproduce #1878 on the \
                 reliable lane"
            );
            peak_unread = peak_unread.max(pending.len());
        }
        reader_turn(&mut pending, &meter, &mut charged, &[], &mut emit);

        assert_eq!(
            drained.get(),
            total,
            "the 15-byte class-3 header is consumed by the transport and never \
             re-framed as a packet, so the drain yields exactly the audio frames"
        );
        let delivered = delivered.borrow();
        assert_eq!(
            delivered.len(),
            total,
            "every one of {STALL_SECS}s of audio from {SPEAKERS} speakers arrives"
        );
        assert!(
            delivered
                .iter()
                .zip(sent.iter())
                .all(|((got, lane), want)| got == want && *lane == InboundLane::Reliable),
            "each packet whole, in publish order, on the reliable lane"
        );
        assert!(
            pending.is_empty(),
            "with nothing stranded in the framing buffer"
        );
        assert!(
            peak_unread < CHUNK + widest_frame,
            "the drain runs before the guard, so the buffer holds one chunk plus \
             at most a partial frame ({peak_unread}) — not the whole stall"
        );
        assert!(
            buffer_over_budget(wire.len(), wire.len()).is_none(),
            "and even the pessimistic model that buffers the entire {} bytes of \
             the stall clears both budgets",
            wire.len()
        );
    }

    fn audio_lane_test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::worker_session::telemetry_test_lock()
    }

    #[test]
    fn the_audio_lane_tracker_drains_its_window_and_keeps_its_session_high_water() {
        let _guard = audio_lane_test_lock();
        reset_audio_lane_anchor();
        let _ = take_audio_lane_max_gap_ms();
        assert_eq!(
            take_audio_lane_max_gap_ms(),
            0.0,
            "a WebSocket session never reads an audio stream, and 0 is the \
             correct 'no starvation' value"
        );

        record_audio_lane_read(1000.0);
        record_audio_lane_read(1020.0);
        record_audio_lane_read(4020.0);

        assert_eq!(
            peek_audio_lane_session_max_gap_ms(),
            3000.0,
            "the stall is the worst gap between successive .read() resolutions"
        );
        assert_eq!(take_audio_lane_max_gap_ms(), 3000.0);
        assert_eq!(
            take_audio_lane_max_gap_ms(),
            0.0,
            "the reporting window drains"
        );
        assert_eq!(
            peek_audio_lane_session_max_gap_ms(),
            3000.0,
            "but the seam's non-draining read is unaffected, so a spec cannot \
             steal a window from the health tick"
        );

        reset_audio_lane_anchor();
        assert_eq!(
            peek_audio_lane_session_max_gap_ms(),
            0.0,
            "a reconnect starts clean rather than booking the downtime"
        );
        record_audio_lane_read(90_000.0);
        assert_eq!(
            peek_audio_lane_session_max_gap_ms(),
            0.0,
            "and the first read after the reset has no predecessor, so it is \
             not a gap spanning the reconnect"
        );
    }

    #[test]
    fn a_non_audio_lane_never_reads_the_clock() {
        let _guard = audio_lane_test_lock();
        reset_audio_lane_anchor();

        let reads = std::cell::Cell::new(0_u32);
        record_read_if_audio_lane(false, || {
            reads.set(reads.get() + 1);
            1000.0
        });
        assert_eq!(
            reads.get(),
            0,
            "a video or screen stream must not pay the clock crossing"
        );

        record_read_if_audio_lane(true, || {
            reads.set(reads.get() + 1);
            1000.0
        });
        assert_eq!(
            reads.get(),
            1,
            "and the audio lane still reads it exactly once"
        );
    }

    #[test]
    fn a_non_audio_stream_contributes_nothing_to_the_audio_lane_tracker() {
        let _guard = audio_lane_test_lock();
        reset_audio_lane_anchor();

        record_read_if_audio_lane(false, || 1000.0);
        record_read_if_audio_lane(false, || 9000.0);
        assert_eq!(
            peek_audio_lane_session_max_gap_ms(),
            0.0,
            "measuring every stream would report a camera GOP's cadence as \
             audio starvation and make the #2728 threshold meaningless"
        );

        record_read_if_audio_lane(true, || 1000.0);
        record_read_if_audio_lane(true, || 4000.0);
        assert_eq!(
            peek_audio_lane_session_max_gap_ms(),
            3000.0,
            "and dropping the gate the other way records nothing at all, which \
             would leave the acceptance signal permanently reading 0"
        );
        reset_audio_lane_anchor();
        let _ = take_audio_lane_max_gap_ms();
    }

    #[test]
    fn only_the_class_three_audio_stream_is_measured() {
        assert!(is_audio_lane(StreamKey::V1 {
            class: stream_class::AUDIO,
            publisher_session_id: 0,
            media_kind: 2,
        }));
        for class in [
            stream_class::CONTROL,
            stream_class::PUBLISHER,
            stream_class::OVERFLOW,
        ] {
            assert!(
                !is_audio_lane(StreamKey::V1 {
                    class,
                    publisher_session_id: 1,
                    media_kind: 1,
                }),
                "class {class} is not the #2724 audio lane; measuring it would \
                 report a camera GOP's cadence as audio starvation"
            );
        }
        assert!(
            !is_audio_lane(StreamKey::Legacy),
            "a legacy relay has no separate audio stream to measure"
        );
    }

    #[test]
    fn one_publishers_media_kinds_do_not_serialise_against_each_other() {
        let mut gate = InboundReaderGate::new();
        let screen = StreamKey::V1 {
            class: stream_class::PUBLISHER,
            publisher_session_id: 1,
            media_kind: 3,
        };
        assert_eq!(gate.admit(video(1), "camera"), Admission::Start("camera"));
        assert_eq!(gate.admit(screen, "screen"), Admission::Start("screen"));
    }

    #[test]
    fn a_same_key_replacement_waits_for_the_superseded_reader() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(
            gate.admit(video(1), "first"),
            Admission::Start("first"),
            "with no reader on this key the first stream starts at once"
        );
        queue(&mut gate, video(1), "second");
        assert_eq!(
            gate.reader_exited(video(1)),
            Some("second"),
            "the running reader picks the queued stream up when its loop ends"
        );
        assert_eq!(
            gate.reader_exited(video(1)),
            None,
            "with the queue drained the last reader simply ends"
        );
    }

    #[test]
    fn a_finished_key_is_retired_so_a_later_same_key_stream_starts_at_once() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(video(1), "first"), Admission::Start("first"));
        assert_eq!(gate.reader_exited(video(1)), None);
        assert!(
            gate.keys.is_empty(),
            "a key with no reader and nothing queued must not stay in the map"
        );
        assert_eq!(
            gate.admit(video(1), "reopened"),
            Admission::Start("reopened"),
            "a later stream on a retired key must not serve a grace period"
        );
    }

    #[test]
    fn a_second_replacement_supersedes_the_first_and_is_handed_back() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(video(1), "active"), Admission::Start("active"));
        queue(&mut gate, video(1), "queued");
        assert_eq!(
            gate.admit(video(1), "newest"),
            Admission::Queued {
                seq: 1,
                superseded: Some("queued")
            },
            "overflow must hand the superseded stream back so the caller can \
             retire it, not drop it silently"
        );
        assert_eq!(gate.evicted, 1, "exactly one overflow must be booked");
        assert_eq!(
            gate.reader_exited(video(1)),
            Some("newest"),
            "the newest stream is the live one; the older queued stream is gone"
        );
    }

    #[test]
    fn a_legacy_relays_streams_all_collapse_onto_one_key() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(
            gate.admit(StreamKey::Legacy, "first"),
            Admission::Start("first")
        );
        queue(&mut gate, StreamKey::Legacy, "second");
        assert_eq!(
            gate.active_readers, 1,
            "a legacy relay must never run two readers at once"
        );
        assert_eq!(gate.reader_exited(StreamKey::Legacy), Some("second"));
    }

    #[test]
    fn a_wedged_reader_cannot_hold_its_keys_queued_stream_forever() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(video(1), "wedged"), Admission::Start("wedged"));
        let fresh = queue(&mut gate, video(1), "fresh");
        assert_eq!(
            gate.force_start(video(1), fresh),
            ForceStart::Started("fresh"),
            "the grace timer must release the queued stream past the deadline"
        );
        assert_eq!(gate.forced_starts, 1);
        assert_eq!(
            gate.reader_exited(video(1)),
            None,
            "the wedged reader exiting later must not re-hand a stream already forced out"
        );
        assert_eq!(
            gate.force_start(video(1), fresh),
            ForceStart::Gone,
            "a grace timer that fires after the queue drained is a no-op"
        );
    }

    #[test]
    fn the_first_of_two_same_key_readers_to_exit_must_not_take_the_queue() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(video(1), "wedged"), Admission::Start("wedged"));
        let fresh = queue(&mut gate, video(1), "fresh");
        assert_eq!(
            gate.force_start(video(1), fresh),
            ForceStart::Started("fresh"),
            "anti-vacuity: the grace release is what puts TWO readers on one key"
        );
        queue(&mut gate, video(1), "third");

        assert_eq!(
            gate.reader_exited(video(1)),
            None,
            "the first of two same-key readers to exit must NOT take the queue"
        );
        assert_eq!(
            gate.keys[&video(1)].active_readers,
            1,
            "and the surviving reader must still be counted on the key"
        );
        assert_eq!(
            gate.reader_exited(video(1)),
            Some("third"),
            "the LAST reader out is the one that hands the queued stream on"
        );
    }

    #[test]
    fn a_stale_grace_timer_cannot_force_start_a_younger_stream() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(video(1), "a"), Admission::Start("a"));
        let b = queue(&mut gate, video(1), "b"); // timer armed for b
        assert_eq!(
            gate.reader_exited(video(1)),
            Some("b"),
            "a's reader takes b"
        );

        let c = queue(&mut gate, video(1), "c"); // its own timer pends
        assert_eq!(
            gate.force_start(video(1), b),
            ForceStart::Gone,
            "b's expired timer must not release c, which has barely begun its grace"
        );
        assert_eq!(gate.forced_starts, 0);
        assert_eq!(
            gate.force_start(video(1), c),
            ForceStart::Started("c"),
            "c's own timer is the only one that may release c"
        );
    }

    #[test]
    fn a_grace_timer_cannot_release_another_keys_queued_stream() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(video(1), "alice"), Admission::Start("alice"));
        assert_eq!(gate.admit(video(2), "bob"), Admission::Start("bob"));
        let alice_queued = queue(&mut gate, video(1), "alice2");
        assert_eq!(
            gate.force_start(video(2), alice_queued),
            ForceStart::Gone,
            "bob's key holds nothing, so alice's sequence number must find nothing there"
        );
        assert_eq!(gate.forced_starts, 0);
        assert_eq!(
            gate.force_start(video(1), alice_queued),
            ForceStart::Started("alice2")
        );
    }

    #[test]
    fn a_timer_for_a_superseded_stream_releases_nothing() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(video(1), "active"), Admission::Start("active"));
        let superseded = queue(&mut gate, video(1), "s1");
        assert!(matches!(
            gate.admit(video(1), "s2"),
            Admission::Queued {
                superseded: Some("s1"),
                ..
            }
        ));
        assert_eq!(gate.force_start(video(1), superseded), ForceStart::Gone);
        assert_eq!(gate.forced_starts, 0);
    }

    #[test]
    fn readers_cannot_exceed_the_concurrent_reader_ceiling() {
        let mut gate = InboundReaderGate::new();
        let mut started = 0;
        for session in 0..(4 * WT_MAX_DOWNLINK_STREAMS as u64) {
            if let Admission::Start(_) = gate.admit(video(session), "fresh") {
                started += 1;
            }
        }
        assert_eq!(
            started, MAX_CONCURRENT_INBOUND_READERS,
            "a relay opening more keys than the ceiling must not start a reader per key"
        );
        assert_eq!(gate.active_readers, MAX_CONCURRENT_INBOUND_READERS);
        let refused_at_admit =
            4 * WT_MAX_DOWNLINK_STREAMS as u64 - MAX_CONCURRENT_INBOUND_READERS as u64;
        assert_eq!(
            gate.refused, refused_at_admit,
            "every stream past the ceiling is handed back for the caller to cancel"
        );

        let seq = queue(&mut gate, video(0), "behind-a-wedge");
        assert_eq!(
            gate.force_start(video(0), seq),
            ForceStart::Refused("behind-a-wedge"),
            "the grace timer must not push the reader count past the ceiling either"
        );
        assert_eq!(gate.active_readers, MAX_CONCURRENT_INBOUND_READERS);
    }

    #[test]
    fn a_grace_timer_refused_at_the_ceiling_hands_its_stream_back_and_frees_the_slot() {
        let mut gate = InboundReaderGate::new();
        for session in 0..(MAX_CONCURRENT_INBOUND_READERS as u64) {
            assert_eq!(
                gate.admit(video(session), "reading"),
                Admission::Start("reading")
            );
        }
        let seq = queue(&mut gate, video(0), "stranded");

        assert_eq!(
            gate.force_start(video(0), seq),
            ForceStart::Refused("stranded"),
            "at the ceiling the stream must be handed back, not silently kept"
        );
        assert_eq!(gate.refused, 1, "and booked, so it is not invisible");
        assert_eq!(gate.forced_starts, 0, "it did not start a reader");
        assert!(
            gate.keys[&video(0)].pending.is_none(),
            "the queue slot must be free again: the timer fires once, so a stream              left queued here would never be looked at by anything"
        );
        assert_eq!(
            gate.admit(video(0), "later"),
            Admission::Queued {
                seq: 1,
                superseded: None
            },
            "a later same-key stream takes the freed slot without evicting anything"
        );
    }

    #[test]
    fn the_reader_ceiling_clears_the_relays_stream_cap_with_room_for_handover() {
        let ceiling = MAX_CONCURRENT_INBOUND_READERS;
        let relay_cap = WT_MAX_DOWNLINK_STREAMS;
        assert!(
            ceiling >= 2 * relay_cap,
            "each key can run its superseded reader alongside the forced-start \
             replacement, so a conforming relay's worst case is {} readers; a \
             ceiling below that cancels real streams",
            2 * relay_cap
        );
    }

    #[test]
    fn a_late_grace_timer_does_not_double_start_a_stream() {
        let mut gate = InboundReaderGate::new();
        assert_eq!(gate.admit(video(1), "first"), Admission::Start("first"));
        let second = queue(&mut gate, video(1), "second");
        assert_eq!(gate.reader_exited(video(1)), Some("second"));
        assert_eq!(
            gate.force_start(video(1), second),
            ForceStart::Gone,
            "the queue is empty, so the expired grace period has nothing to release"
        );
        assert_eq!(gate.forced_starts, 0);
    }

    #[test]
    fn the_key_map_empties_as_readers_end() {
        let mut gate = InboundReaderGate::new();
        for session in 0..32u64 {
            assert_eq!(gate.admit(video(session), "s"), Admission::Start("s"));
        }
        assert_eq!(gate.keys.len(), 32);
        for session in 0..32u64 {
            assert_eq!(gate.reader_exited(video(session)), None);
        }
        assert!(
            gate.keys.is_empty() && gate.active_readers == 0,
            "the map tracks live readers only, so a long session cannot leak keys"
        );
    }

    #[test]
    fn the_stream_header_is_consumed_and_only_the_packets_behind_it_are_emitted() {
        let mut header = 15u32.to_be_bytes().to_vec();
        header.extend_from_slice(b"VCDS\x01\x01");
        header.extend_from_slice(&7u64.to_be_bytes());
        header.push(1);

        let packet = vec![0x08u8, 0x03, 0x22, 0x02, 0xAB, 0xCD];
        let mut wire = header.clone();
        wire.extend_from_slice(&framed(&packet));

        let mut pending = PendingFrames::default();
        pending.extend_for_test(&wire);

        assert_eq!(
            take_classified_key(&mut pending),
            Some(video(7)),
            "a well-formed v1 header must classify as one"
        );

        let mut emitted: Vec<Vec<u8>> = Vec::new();
        assert_eq!(
            drain_complete_frames(&mut pending, |p| emitted.push(p)),
            FrameDrain::NeedMore
        );
        assert_eq!(
            emitted,
            vec![packet],
            "the 15-byte header must be swallowed and the packet behind it emitted whole"
        );
    }

    #[test]
    fn the_session_budget_trips_on_the_aggregate_while_every_stream_is_under_its_own_cap() {
        let per_stream = MAX_INBOUND_STREAM_SIZE;

        let meter = InboundByteMeter::default();
        let mut charged = [0usize; 9];
        let mut tripped_at = None;
        let mut tripped_on = None;
        for (index, charge) in charged.iter_mut().enumerate() {
            let total = meter.recharge(charge, per_stream);
            if let Some(bound) = buffer_over_budget(per_stream, total) {
                if tripped_at.is_none() {
                    tripped_at = Some(index);
                    tripped_on = Some(bound);
                }
            }
        }
        assert_eq!(
            tripped_at,
            Some(8),
            "nine streams of an eighth of the budget each must trip on the ninth"
        );
        assert_eq!(
            tripped_on,
            Some(BufferBound::Session),
            "and it must be the session bound that names it, not the per-stream one"
        );
    }

    #[test]
    fn the_per_stream_cap_still_fires_and_takes_precedence() {
        assert_eq!(
            buffer_over_budget(MAX_INBOUND_STREAM_SIZE + 1, 0),
            Some(BufferBound::Stream),
            "one runaway stream is still dropped on its own cap"
        );
        assert_eq!(
            buffer_over_budget(MAX_INBOUND_STREAM_SIZE, MAX_INBOUND_SESSION_BYTES),
            None,
            "both bounds are inclusive; exactly at the limit is still legal"
        );
    }

    #[test]
    fn recharging_replaces_a_streams_contribution_rather_than_adding_to_it() {
        let meter = InboundByteMeter::default();
        let mut alice = 0usize;
        let mut bob = 0usize;

        assert_eq!(meter.recharge(&mut alice, 1_000), 1_000);
        assert_eq!(meter.recharge(&mut bob, 400), 1_400);
        assert_eq!(
            meter.recharge(&mut alice, 1_500),
            1_900,
            "alice's re-read replaces her 1000, it does not stack on it"
        );
        assert_eq!(
            meter.recharge(&mut alice, 0),
            400,
            "a drained buffer leaves only bob's bytes charged"
        );
    }

    #[test]
    fn releasing_a_streams_charge_returns_it_to_the_session_budget() {
        let meter = InboundByteMeter::default();
        let mut charged = 0usize;
        meter.recharge(&mut charged, 2_048);
        assert_eq!(meter.total(), 2_048);

        meter.release(charged);
        assert_eq!(
            meter.total(),
            0,
            "a stream that ends any way at all must give its bytes back, or a long              session leaks the budget until every new stream is cancelled"
        );
        meter.release(charged);
        assert_eq!(meter.total(), 0, "a double release must not underflow");
    }

    #[test]
    fn the_session_budget_clears_every_streams_largest_legal_frame() {
        let budget = MAX_INBOUND_SESSION_BYTES;
        let per_stream = MAX_INBOUND_STREAM_SIZE;
        assert!(
            budget > per_stream,
            "one maximum-size frame must never trip the session bound on its own"
        );
        assert_eq!(
            budget / per_stream,
            8,
            "32 MiB is eight simultaneous maximum-size frames; lowering this without              lowering the per-stream cap would drop legal traffic"
        );
    }

    #[test]
    fn a_legacy_streams_first_frame_is_emitted_rather_than_swallowed() {
        let packet = vec![0x08u8, 0x03, 0x22, 0x02, 0xAB, 0xCD];
        let mut pending = PendingFrames::default();
        pending.extend_for_test(&framed(&packet));

        assert_eq!(
            classify_prelude(pending.unread()),
            Prelude::Legacy,
            "no header means a relay that predates #2723"
        );

        let mut emitted: Vec<Vec<u8>> = Vec::new();
        drain_complete_frames(&mut pending, |p| emitted.push(p));
        assert_eq!(
            emitted,
            vec![packet],
            "classification must not consume a legacy relay's first packet"
        );
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut out = (payload.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn the_reframe_yields_exact_payloads_at_odd_lengths() {
        let payloads: Vec<Vec<u8>> = vec![
            vec![0x01],
            vec![0xAB; 7],
            (0u8..=254).collect(),
            vec![0x5A; 1501],
        ];
        let wire: Vec<u8> = payloads.iter().flat_map(|p| framed(p)).collect();
        let mut pending = PendingFrames::default();
        pending.extend_for_test(&wire);
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        assert_eq!(
            drain_complete_frames(&mut pending, |p| emitted.push(p)),
            FrameDrain::NeedMore
        );
        assert_eq!(
            emitted, payloads,
            "every payload must survive byte for byte"
        );
        assert!(
            pending.is_empty(),
            "a clean frame boundary consumes the buffer"
        );
    }

    #[test]
    fn draining_a_chunk_of_frames_moves_no_bytes() {
        let payloads: Vec<Vec<u8>> = (0..40u8).map(|i| vec![i; 1600]).collect();
        let wire: Vec<u8> = payloads.iter().flat_map(|p| framed(p)).collect();
        let mut pending = PendingFrames::default();
        pending.extend_for_test(&wire);
        let buffered = pending.buf.len();

        let mut emitted = 0usize;
        assert_eq!(
            drain_complete_frames(&mut pending, |_| emitted += 1),
            FrameDrain::NeedMore
        );

        assert_eq!(emitted, payloads.len());
        assert_eq!(
            pending.buf.len(),
            buffered,
            "taking a frame must only advance the cursor; a shrinking buffer means \
             the remainder was memmoved once per frame"
        );
        assert_eq!(pending.consumed, buffered, "the whole chunk is consumed");
        assert!(pending.is_empty(), "and nothing is left to read");
    }

    #[test]
    fn the_consumed_prefix_is_reclaimed_on_the_next_append() {
        let mut pending = PendingFrames::default();
        pending.extend_for_test(&framed(&[7u8; 900]));
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        drain_complete_frames(&mut pending, |p| emitted.push(p));
        assert_eq!(emitted.len(), 1);
        assert_eq!(pending.consumed, 904);

        pending.extend_for_test(&framed(&[8u8; 10]));
        assert_eq!(pending.consumed, 0, "the spent prefix must be reclaimed");
        assert_eq!(pending.buf.len(), 14, "only the new frame remains buffered");
    }

    #[test]
    fn a_frame_split_across_reads_is_emitted_once_whole() {
        let payload: Vec<u8> = (0u8..200).collect();
        let wire = framed(&payload);
        let (head, tail) = wire.split_at(50);

        let mut pending = PendingFrames::default();
        pending.extend_for_test(head);
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        assert_eq!(
            drain_complete_frames(&mut pending, |p| emitted.push(p)),
            FrameDrain::NeedMore
        );
        assert!(
            emitted.is_empty(),
            "no frame may be emitted until its payload is fully buffered"
        );

        pending.extend_for_test(tail);
        assert_eq!(
            drain_complete_frames(&mut pending, |p| emitted.push(p)),
            FrameDrain::NeedMore
        );
        assert_eq!(
            emitted,
            vec![payload],
            "the completed frame is emitted whole"
        );
    }

    #[test]
    fn an_empty_read_emits_nothing_and_keeps_the_partial_frame() {
        let partial = framed(&[9u8; 40])[..10].to_vec();
        let mut pending = PendingFrames::default();
        pending.extend_for_test(&partial);
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        assert_eq!(
            drain_complete_frames(&mut pending, |p| emitted.push(p)),
            FrameDrain::NeedMore
        );
        assert!(emitted.is_empty());
        assert_eq!(
            pending.unread(),
            partial.as_slice(),
            "a partial frame must be kept intact"
        );

        let mut empty = PendingFrames::default();
        assert_eq!(
            drain_complete_frames(&mut empty, |p| emitted.push(p)),
            FrameDrain::NeedMore
        );
        assert!(emitted.is_empty());
    }

    #[test]
    fn a_corrupt_length_header_stops_the_drain() {
        let mut pending = PendingFrames::default();
        pending.extend_for_test(&0u32.to_be_bytes());
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        assert_eq!(
            drain_complete_frames(&mut pending, |p| emitted.push(p)),
            FrameDrain::CorruptLength(0)
        );

        let oversized = MAX_INBOUND_STREAM_SIZE + 1;
        let mut pending = PendingFrames::default();
        pending.extend_for_test(&(oversized as u32).to_be_bytes());
        assert_eq!(
            drain_complete_frames(&mut pending, |p| emitted.push(p)),
            FrameDrain::CorruptLength(oversized)
        );
        assert!(emitted.is_empty());
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn appending_a_chunk_preserves_the_existing_buffer() {
        let mut buffer = vec![1u8, 2, 3];
        let chunk = Uint8Array::new_with_length(4);
        chunk.copy_from(&[4u8, 5, 6, 7]);

        append_uint8_array_to_vec(&mut buffer, &chunk);
        assert_eq!(buffer, vec![1, 2, 3, 4, 5, 6, 7]);

        let empty = Uint8Array::new_with_length(0);
        append_uint8_array_to_vec(&mut buffer, &empty);
        assert_eq!(buffer, vec![1, 2, 3, 4, 5, 6, 7]);
    }

    #[cfg(target_arch = "wasm32")]
    fn parked_over(chunks: &[Vec<u8>], session: &InboundSession) -> ParkedStream {
        let arrays = js_sys::Array::new();
        for chunk in chunks {
            let array = Uint8Array::new_with_length(chunk.len() as u32);
            array.copy_from(chunk);
            arrays.push(&array);
        }
        let factory = js_sys::Function::new_with_args(
            "chunks",
            "return new ReadableStream({ start(c) { for (const ch of chunks) c.enqueue(ch); \
             c.close(); } });",
        );
        let stream = factory
            .call1(&wasm_bindgen::JsValue::NULL, &arrays)
            .unwrap();
        ParkedStream::new(
            stream
                .unchecked_into::<web_sys::ReadableStream>()
                .get_reader()
                .unchecked_into(),
            session,
        )
    }

    /// Yields `chunk`, then closes on the NEXT pull, so a reader that cancels
    /// finds it open while one that never does still terminates. Counts cancels
    /// under a caller-unique global name.
    #[cfg(target_arch = "wasm32")]
    fn uncancelled_parked_over(chunk: &[u8], session: &InboundSession, flag: &str) -> ParkedStream {
        let array = Uint8Array::new_with_length(chunk.len() as u32);
        array.copy_from(chunk);
        let factory = js_sys::Function::new_with_args(
            "chunk, flag",
            "globalThis[flag] = 0; let pulls = 0; \
             return new ReadableStream({ \
             pull(c) { pulls += 1; if (pulls === 1) { c.enqueue(chunk); } \
             else { c.close(); } }, \
             cancel() { globalThis[flag] += 1; } }, { highWaterMark: 0 });",
        );
        let stream = factory
            .call2(&wasm_bindgen::JsValue::NULL, &array, &JsString::from(flag))
            .unwrap();
        ParkedStream::new(
            stream
                .unchecked_into::<web_sys::ReadableStream>()
                .get_reader()
                .unchecked_into(),
            session,
        )
    }

    #[cfg(target_arch = "wasm32")]
    fn cancel_count(flag: &str) -> u32 {
        Reflect::get(&js_sys::global(), &JsString::from(flag))
            .unwrap()
            .as_f64()
            .unwrap() as u32
    }

    /// Promises more than it delivers, so the bytes stay unread.
    #[cfg(target_arch = "wasm32")]
    fn incomplete_frame() -> Vec<u8> {
        let mut wire = 4_096u32.to_be_bytes().to_vec();
        wire.extend_from_slice(&[0x5Au8; 512]);
        wire
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn a_parked_partial_frame_is_charged_to_the_session_and_released_on_drop() {
        let session = InboundSession::default();
        let mut parked = parked_over(&[incomplete_frame()], &session);
        assert_eq!(session.bytes.total(), 0, "nothing is charged before a read");

        let (callback, seen) = collector();
        read_framed_unistream(&mut parked, &callback, &session, false).await;

        assert!(
            seen.borrow().is_empty(),
            "an incomplete frame emits nothing"
        );
        assert_eq!(
            session.bytes.total(),
            516,
            "the unread prefix and payload must be charged to the session while              this reader holds them, or 64 keys can each hold 4 MB unmeasured"
        );

        drop(parked);
        assert_eq!(
            session.bytes.total(),
            0,
            "and handed back when the stream goes away, or a long session leaks              the budget until every new stream is cancelled"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn a_stream_that_breaches_the_session_budget_is_cancelled_and_booked() {
        let session = InboundSession::default();
        let mut elsewhere = 0usize;
        session
            .bytes
            .recharge(&mut elsewhere, MAX_INBOUND_SESSION_BYTES);

        let flag = "__vc2723_budget_cancels";
        let mut parked = uncancelled_parked_over(&incomplete_frame(), &session, flag);
        assert_eq!(cancel_count(flag), 0);

        let before = crate::webtransport::inbound_unistream_reset_count();
        let (callback, _seen) = collector();
        read_framed_unistream(&mut parked, &callback, &session, false).await;
        gloo_timers::future::TimeoutFuture::new(0).await;

        assert_eq!(
            cancel_count(flag),
            1,
            "the reader must cancel its stream so the relay stops sending on it"
        );
        assert_eq!(
            crate::webtransport::inbound_unistream_reset_count() - before,
            1,
            "and book exactly one reset, so a breach is visible to ops"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn an_invalid_frame_length_cancels_the_stream_and_books_a_reset() {
        let session = InboundSession::default();
        let flag = "__vc2755_corrupt_len_cancels";
        let mut parked = uncancelled_parked_over(&0u32.to_be_bytes(), &session, flag);
        assert_eq!(cancel_count(flag), 0);

        let before = crate::webtransport::inbound_unistream_reset_count();
        let (callback, seen) = collector();
        read_framed_unistream(&mut parked, &callback, &session, false).await;
        gloo_timers::future::TimeoutFuture::new(0).await;

        assert!(seen.borrow().is_empty(), "a corrupt length emits nothing");
        assert_eq!(
            cancel_count(flag),
            1,
            "an unrecoverable framing error must cancel, or the relay writes into \
             a locked reader until it sheds"
        );
        assert_eq!(
            crate::webtransport::inbound_unistream_reset_count() - before,
            1,
            "and book exactly one reset, so the discard is visible to ops"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn a_per_stream_buffer_overflow_cancels_the_stream_and_books_a_reset() {
        let session = InboundSession::default();
        let flag = "__vc2755_stream_bound_cancels";
        let mut wire = (MAX_INBOUND_STREAM_SIZE as u32).to_be_bytes().to_vec();
        wire.resize(MAX_INBOUND_STREAM_SIZE + 1, 0x5A);
        let mut parked = uncancelled_parked_over(&wire, &session, flag);
        assert_eq!(cancel_count(flag), 0);

        let before = crate::webtransport::inbound_unistream_reset_count();
        let (callback, seen) = collector();
        read_framed_unistream(&mut parked, &callback, &session, false).await;
        gloo_timers::future::TimeoutFuture::new(0).await;

        assert!(seen.borrow().is_empty(), "no frame ever completed");
        assert_eq!(
            cancel_count(flag),
            1,
            "a stream that outgrew its own ceiling must cancel, not merely stop \
             reading"
        );
        assert_eq!(
            crate::webtransport::inbound_unistream_reset_count() - before,
            1,
            "and book exactly one reset"
        );
    }

    #[cfg(target_arch = "wasm32")]
    type EmittedPayloads = Rc<RefCell<Vec<Vec<u8>>>>;

    #[cfg(target_arch = "wasm32")]
    fn collector() -> (Callback<Vec<u8>>, EmittedPayloads) {
        let seen: EmittedPayloads = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        (
            Callback::from(move |payload: Vec<u8>| sink.borrow_mut().push(payload)),
            seen,
        )
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn a_v1_stream_yields_its_key_and_only_the_packets_behind_the_header() {
        let mut header = 15u32.to_be_bytes().to_vec();
        header.extend_from_slice(b"VCDS\x01\x01");
        header.extend_from_slice(&42u64.to_be_bytes());
        header.push(3);

        let first = vec![0x08u8, 0x03, 0x22, 0x02, 0x01, 0x02];
        let second = vec![0x08u8, 0x03, 0x22, 0x01, 0xFF];
        let mut opening = header.clone();
        opening.extend_from_slice(&framed(&first));

        let session = InboundSession::default();
        let mut parked = parked_over(&[opening, framed(&second)], &session);
        let key = classify_inbound_stream(&mut parked).await;
        assert_eq!(
            key,
            Some(StreamKey::V1 {
                class: stream_class::PUBLISHER,
                publisher_session_id: 42,
                media_kind: 3,
            })
        );

        let (callback, seen) = collector();
        read_framed_unistream(&mut parked, &callback, &session, false).await;
        assert_eq!(
            *seen.borrow(),
            vec![first, second],
            "the header must never reach the decoder, and a packet sharing its \
             chunk must not be lost with it"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn a_legacy_stream_classifies_as_one_key_and_keeps_its_first_packet() {
        let packet = vec![0x08u8, 0x03, 0x22, 0x02, 0xAB, 0xCD];
        let session = InboundSession::default();
        let mut parked = parked_over(&[framed(&packet)], &session);

        assert_eq!(
            classify_inbound_stream(&mut parked).await,
            Some(StreamKey::Legacy)
        );

        let (callback, seen) = collector();
        read_framed_unistream(&mut parked, &callback, &session, false).await;
        assert_eq!(*seen.borrow(), vec![packet]);
        drop(parked);
        assert_eq!(
            session.bytes.total(),
            0,
            "a finished reader must hand its framing bytes back to the session budget"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn a_reset_stream_books_exactly_one_inbound_reset() {
        let factory = js_sys::Function::new_no_args(
            "return new ReadableStream({ start(c) { c.error(new Error('RESET_STREAM')); } });",
        );
        let stream = factory.call0(&wasm_bindgen::JsValue::NULL).unwrap();
        let session = InboundSession::default();
        let mut parked = ParkedStream::new(
            stream
                .unchecked_into::<web_sys::ReadableStream>()
                .get_reader()
                .unchecked_into(),
            &session,
        );

        let before = crate::webtransport::inbound_unistream_reset_count();
        assert_eq!(
            read_chunk(&parked.reader, &mut parked.pending).await,
            ReadStep::Failed
        );
        assert_eq!(
            crate::webtransport::inbound_unistream_reset_count() - before,
            1,
            "a RESET_STREAM must still book one reset per stream"
        );
    }
}
