//! A service to connect to a server through the
//! [`WebTransport` Protocol](https://datatracker.ietf.org/doc/draft-ietf-webtrans-overview/).
//!
//! Forked from yew-webtransport (MIT licensed, Copyright (c) 2022 Security Union),
//! adapted to use `videocall_types::Callback` instead of `yew::Callback`.

use anyhow::{anyhow, Error};
use futures::channel::oneshot::channel;
use futures::lock::Mutex as AsyncMutex;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fmt, rc::Rc};
use thiserror::Error as ThisError;
use videocall_types::Callback;
use wasm_bindgen_futures::JsFuture;

use gloo_console::log;
use js_sys::{Array, Boolean, JsString, Promise, Reflect, Uint8Array};
use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WebTransportDatagramDuplexStream, WebTransportHash, WebTransportOptions,
    WebTransportReceiveStream, WritableStream, WritableStreamDefaultWriter,
};

/// Cumulative count of datagrams dropped because the writable stream was locked.
static DATAGRAM_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Returns the total number of datagrams dropped since process start.
pub fn datagram_drop_count() -> u64 {
    DATAGRAM_DROP_COUNT.load(Ordering::Relaxed)
}

/// Cumulative count of frames dropped on the persistent unidirectional QUIC
/// streams (`send_on_persistent_stream`) because the write failed and the
/// stream had to be evicted.
///
/// This is the client-side WebTransport analogue of
/// [`crate::websocket::websocket_drop_count`]: on WebTransport, audio / video /
/// screen — and the Control stream — all ride persistent unistreams (see
/// `videocall-client/src/connection/webtransport.rs::send_bytes`), so a write
/// failure here means a real frame (overwhelmingly media, since media frames
/// dominate the uplink) was dropped — the genuine uplink-saturation signal.
/// Datagrams, by contrast, carry only periodic control traffic (heartbeats /
/// RTT probes), so [`datagram_drop_count`] is a far sparser, indirect signal.
/// The sender AQ self-congestion trigger (#1104) keys off THIS counter for
/// that reason.
///
/// Note: a unistream write does NOT fail on ordinary send-buffer backpressure.
/// A WHATWG `WritableStream` applies backpressure by leaving `writer.ready()`
/// PENDING (it does not reject); the write itself only rejects on stream /
/// connection TEARDOWN — `STOP_SENDING`, `RESET_STREAM`, or session close. So
/// each increment of this counter is a "the stream was torn down" event, NOT a
/// "the uplink is merely saturated" event. On a genuine bandwidth cliff (link
/// slow but ALIVE, ACKs still flowing) this counter stays FLAT — the slow path
/// just blocks in `writer.ready().await`. That saturation case is detected by
/// the separate time-to-`ready()` stall signal below
/// ([`UNISTREAM_READY_STALL_COUNT`]); the two counters are complementary
/// (teardown vs. saturation) and are consumed by independent AQ windows.
static UNISTREAM_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Returns the total number of persistent-unistream frames dropped since
/// process start. See [`UNISTREAM_DROP_COUNT`].
pub fn unistream_drop_count() -> u64 {
    UNISTREAM_DROP_COUNT.load(Ordering::Relaxed)
}

/// Record one dropped persistent-unistream media frame: increment
/// [`UNISTREAM_DROP_COUNT`] by one.
///
/// This is the single write path for the drop counter, extracted from the
/// `send_on_persistent_stream` error handler so the increment — the exact
/// side effect the encoder AQ self-shed (#1104) consumes — can be exercised
/// by a NATIVE `#[test]` without standing up the wasm-only JS WritableStream
/// machinery. It is the drop-counter sibling of [`record_ready_stall`] (which
/// already had such a seam for the saturation counter); before this extraction
/// the drop counter had no host-testable write path, so a mutation that
/// stopped incrementing it (or incremented the wrong counter) had no native
/// test to catch it. The wasm send path now calls THIS function, so the
/// counter the encoder reads via [`unistream_drop_count`] is the same one the
/// test drives.
///
/// The caller remains responsible for the gate that depends on wasm/`Rc`
/// runtime state — only counting failures at the write stage, i.e. after a
/// writer was acquired (`captured_token.is_some()`) — because that condition
/// cannot be made pure. See the gating rationale at the call site.
fn record_unistream_drop() {
    UNISTREAM_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Cumulative count of "slow `writer.ready()`" events on the persistent
/// unidirectional media streams (`send_on_persistent_stream`): each time a
/// single `JsFuture::from(writer.ready()).await` on an ESTABLISHED media stream
/// takes longer than the effective stall threshold (see
/// [`effective_stall_threshold_ms`]) to resolve, this counter is incremented
/// once.
///
/// ## Why this exists (the gap [`UNISTREAM_DROP_COUNT`] cannot fill)
///
/// On WebTransport the media send path is fully `.await`-blocking and a
/// `WritableStream` signals backpressure by leaving `writer.ready()` PENDING,
/// not by rejecting the write. So when the uplink hits a BANDWIDTH cliff but the
/// connection is still alive (ACKs flowing, no stream reset), no write ever
/// fails and `UNISTREAM_DROP_COUNT` stays flat — the publisher would never
/// self-shed. Measuring how long `ready()` blocks turns that otherwise-silent
/// saturation into an observable, monotonic signal the encoder AQ loop can
/// consume exactly like the drop counter (delta-over-window via
/// `videocall_aq::evaluate_self_congestion`). This is the WebTransport analogue
/// of the WebSocket `bufferedAmount`-based drop on the synchronous WS path,
/// which WT lacks structurally.
///
/// Design choice — a monotonic COUNTER (of slow events), not an EWMA of
/// latency: the existing #1178 consumer already keys off a monotonic
/// `AtomicU64` via a tumbling-window delta test, so a sibling counter reuses
/// that exact machinery (same `evaluate_self_congestion` helper, same
/// independent-window pattern) with no new consumer shape, no decay-tuning, and
/// no floating-point atomic. A counter is also self-evidently lock-free and
/// allocation-free on the hot send path. We deliberately count only the
/// THRESHOLD-CROSSING (one increment per slow `ready()`), so the consumer's
/// "N events within the window" test reads as "the uplink was visibly
/// backpressured at least N times in the last window" — the saturation
/// equivalent of the drop counter's "N frames failed to send."
static UNISTREAM_READY_STALL_COUNT: AtomicU64 = AtomicU64::new(0);

/// Returns the total number of slow-`ready()` (uplink-saturation) events on the
/// persistent media unistreams since process start. See
/// [`UNISTREAM_READY_STALL_COUNT`]. Mirrors [`unistream_drop_count`]; consumed
/// by the encoder AQ loop's WT uplink-saturation self-shed (#1219 prerequisite).
pub fn unistream_ready_stall_count() -> u64 {
    UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed)
}

/// NETSIM-ONLY: synthetically bump the WT uplink-saturation counter by `n`
/// (issue #1398). The real increment happens deep inside the `.await`-blocking
/// media send path on a slow `writer.ready()`, which an e2e test cannot reliably
/// induce on a localhost loopback. This feature-gated bumper lets the netsim e2e
/// harness drive the SAME counter the encoders consult, so the mic-side
/// single-layer audio uplink-distress detector can be exercised deterministically.
/// Zero production cost: compiled out unless the `netsim` feature is on (the
/// dioxus-ui e2e build enables it; the production build does not).
#[cfg(feature = "netsim")]
pub fn force_unistream_ready_stall(n: u64) {
    UNISTREAM_READY_STALL_COUNT.fetch_add(n, Ordering::Relaxed);
}

/// Absolute floor for the uplink-saturation threshold (ms). The effective
/// threshold may be raised above this via [`set_ready_stall_threshold_ms`] when
/// the publisher is dual-streaming (camera + screen), but it can never go below
/// this floor.
///
/// Lives here (not in `videocall-aq`) because it parameterises the PRODUCER-side
/// measurement, not the consumer's window/threshold decision. The consumer's
/// "how many slow events trip a shed" threshold and window live in
/// `videocall-aq` alongside the drop-counter constants.
///
/// Value rationale: the AQ loop ticks at `AQ_TICK_INTERVAL_MS` (1000 ms) and a
/// healthy `ready()` on a live link resolves in well under a frame interval
/// (sub-10 ms once the QUIC congestion window has room). 250 ms is ~8× a
/// 30 fps frame interval (33 ms): long enough that an ordinary bursty-but-
/// recovering link (a few queued frames draining) does NOT cross it, but short
/// enough that a genuine bandwidth cliff — where `ready()` blocks for hundreds
/// of ms to multiple seconds while the send buffer refuses to drain — crosses
/// it on most frames.
const READY_STALL_THRESHOLD_MS_FLOOR: f64 = 250.0;

/// Runtime-configurable uplink-saturation threshold (ms), stored as the bits of
/// an `f64`. Defaults to [`READY_STALL_THRESHOLD_MS_FLOOR`] (250 ms).
///
/// When the publisher activates a second video stream (screen share), the
/// combined uplink burst density is higher: the SAME `writer.ready()` stall
/// catches more concurrent frames (higher K-factor), making the count-gate
/// easier to trip. To compensate, the client raises this threshold to
/// `max(FLOOR, 8 × screen_top_tier_frame_interval_ms)` — a fixed 800 ms bound
/// (10 fps top tier × 8), NOT recomputed as either stream degrades. When the
/// screen share stops, the client resets it back to the floor.
///
/// Stored as `f64::to_bits()` because `AtomicF64` does not exist in std.
/// [`effective_stall_threshold_ms`] reads it; [`set_ready_stall_threshold_ms`]
/// writes it. The floor guarantee is enforced at write time.
///
/// Initial value: bit pattern of 250.0_f64 (IEEE 754). Pinned by unit test
/// `threshold_static_initializer_matches_floor` to the floor constant. Using a
/// literal (not `f64::to_bits()`) because const float operations require Rust
/// 1.83+ and the project is pinned to 1.95 without MSRV enforcement, so the
/// real cost is the un-named magic number, not a declared-MSRV constraint.
const READY_STALL_THRESHOLD_MS_INIT_BITS: u64 = 4_643_000_109_586_448_384;

static READY_STALL_THRESHOLD_MS: AtomicU64 = AtomicU64::new(READY_STALL_THRESHOLD_MS_INIT_BITS);

/// Read the current effective stall threshold (ms). This is the runtime value
/// used by [`is_ready_stall`], which may be higher than the floor when
/// dual-streaming.
#[inline]
fn effective_stall_threshold_ms() -> f64 {
    f64::from_bits(READY_STALL_THRESHOLD_MS.load(Ordering::Relaxed))
}

/// Set the uplink-saturation threshold (ms) for the WT slow-`ready()` signal.
///
/// The effective threshold is `max(floor, ms)` — it can never go below
/// [`READY_STALL_THRESHOLD_MS_FLOOR`] (250 ms). Call this when the active
/// media-stream configuration changes (e.g. screen share starts/stops) so the
/// threshold is frame-rate-aware for dual-stream publishers.
///
/// # Recommended formula
///
/// ```text
/// threshold = max(250.0, 8.0 * max_frame_interval_ms_across_active_streams)
/// ```
///
/// For a single camera at 30 fps: `max(250, 8×33) = 264 ≈ floor`.
/// For camera (30 fps) + screen (10 fps): `max(250, 8×100) = 800 ms`.
///
/// This prevents false-positive saturation events on a healthy link that is
/// simply bursty under dual-stream load (issue #1618). The risk — delaying
/// genuine shed detection by up to one extra 2 s window — is acceptable because
/// the shed is a gentle single-rung `force_video_step_down` with the relay
/// CONGESTION path as backstop.
pub fn set_ready_stall_threshold_ms(ms: f64) {
    let clamped = if ms < READY_STALL_THRESHOLD_MS_FLOOR {
        READY_STALL_THRESHOLD_MS_FLOOR
    } else {
        ms
    };
    READY_STALL_THRESHOLD_MS.store(clamped.to_bits(), Ordering::Relaxed);
}

/// Reset the uplink-saturation threshold back to the floor (250 ms).
///
/// Call this when switching from dual-stream back to single-stream (e.g. screen
/// share stops), or when initializing a fresh encoder to ensure a clean baseline.
pub fn reset_ready_stall_threshold() {
    set_ready_stall_threshold_ms(READY_STALL_THRESHOLD_MS_FLOOR);
}

/// Returns the current effective ready-stall threshold in milliseconds.
/// Useful for diagnostics and testing. See [`set_ready_stall_threshold_ms`].
pub fn ready_stall_threshold_ms() -> f64 {
    effective_stall_threshold_ms()
}

/// Pure threshold predicate for the uplink-saturation signal: returns `true`
/// when a single `writer.ready().await` that took `elapsed_ms` to resolve
/// qualifies as a "slow-ready" (saturation) event.
///
/// Extracted from the `send_on_persistent_stream` hot path so the threshold
/// decision — the one piece of the saturation signal that is pure arithmetic
/// rather than JS-bound I/O — can be unit-tested on the NATIVE host target,
/// sidestepping the `#[wasm_bindgen_test]` browser harness entirely. The
/// surrounding async machinery (`performance.now()` reads bracketing the real
/// `JsFuture::from(writer.ready()).await`) stays at the call site; only this
/// comparison moved. Behaviour is identical: the call site computes
/// `elapsed = end - start` exactly as before and passes it here.
///
/// The comparison is strictly `>` (NOT `>=`): a wait of EXACTLY the threshold
/// is not yet a stall. This boundary is pinned by unit tests.
///
/// The threshold is DYNAMIC: it reads [`effective_stall_threshold_ms`] which
/// defaults to 250 ms (single-stream) but is raised when dual-streaming via
/// [`set_ready_stall_threshold_ms`] (issue #1618).
#[inline]
fn is_ready_stall(elapsed_ms: f64) -> bool {
    elapsed_ms > effective_stall_threshold_ms()
}

/// Record one `writer.ready()` measurement against the saturation threshold:
/// if `elapsed_ms` qualifies as a stall ([`is_ready_stall`]), increment
/// [`UNISTREAM_READY_STALL_COUNT`] once and return `true`; otherwise leave the
/// counter untouched and return `false`.
///
/// This is the exact increment that previously lived inline at the
/// `send_on_persistent_stream` ready-stall site. The caller is still
/// responsible for the two conditions that depend on JS / `Rc` runtime state
/// and therefore cannot be made pure: (1) the established-media-writer gate
/// (`captured_token.is_some()`), and (2) timer availability
/// (`perf_now_ms()` returning `Some` at both ends). When both hold, the caller
/// invokes this with the measured elapsed and the counter behaves identically
/// to the original inline `fetch_add`.
fn record_ready_stall(elapsed_ms: f64) -> bool {
    if is_ready_stall(elapsed_ms) {
        UNISTREAM_READY_STALL_COUNT.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// Read a monotonic high-resolution timestamp (ms) from `performance.now()`.
///
/// Returns `None` when there is no `window` / no `Performance` object (e.g. a
/// non-browser test target). We deliberately use `performance.now()` rather than
/// `Date::now()`: it is monotonic (immune to wall-clock adjustments / NTP steps)
/// and is the standard high-resolution timer for measuring elapsed durations on
/// the hot path. It is two clock reads per frame with no allocation.
fn perf_now_ms() -> Option<f64> {
    web_sys::window()?.performance().map(|p| p.now())
}

/// Name of the JS global that, when set to a non-empty array of base64
/// strings BEFORE the wasm boots, opts the WebTransport constructor into the
/// `serverCertificateHashes` path (W3C WebTransport spec).
///
/// Each string is the base64 encoding of the SHA-256 of the DER-encoded
/// server certificate (NOT the SPKI). Used by the E2E harness so Playwright
/// can talk to the local self-signed dev cert without `--ignore-certificate-
/// errors-spki-list` (which Chromium 145 ignores for QUIC/HTTP-3).
///
/// If the global is unset, undefined, or empty, the WT client falls back to
/// the standard CA path. Production builds never set this global.
const WT_CERT_HASHES_GLOBAL: &str = "__VC_WT_CERT_HASHES__";

/// Read the per-page WebTransport cert-hash override and, if present, build
/// a `WebTransportOptions` dictionary suitable for `new_with_options`.
///
/// Returns `None` when:
///   - There is no `window` (non-browser environment),
///   - `window.__VC_WT_CERT_HASHES__` is undefined / null,
///   - The value is not an array, or
///   - The array is empty.
///
/// Malformed entries (non-strings, undecodable base64) are skipped with a
/// console warning rather than aborting the whole connect — the caller may
/// still succeed via subsequent valid hashes or fall through to the standard
/// CA path. We intentionally do NOT panic / return Err here because the
/// global is dev-only plumbing; a misconfigured E2E harness should still
/// fail with a clear browser-side error rather than killing the wasm.
fn read_wt_cert_hash_options() -> Option<WebTransportOptions> {
    let window = web_sys::window()?;
    let raw = Reflect::get(&window, &JsValue::from_str(WT_CERT_HASHES_GLOBAL)).ok()?;
    if raw.is_undefined() || raw.is_null() {
        return None;
    }
    let array: Array = raw.dyn_into().ok()?;
    if array.length() == 0 {
        return None;
    }

    let hashes = Array::new();
    for entry in array.iter() {
        let Some(b64) = entry.as_string() else {
            log!("WT cert hash override: skipping non-string entry");
            continue;
        };
        let bytes = match base64_decode(&b64) {
            Some(b) => b,
            None => {
                log!("WT cert hash override: failed to base64-decode entry, skipping");
                continue;
            }
        };
        let value = Uint8Array::from(bytes.as_slice());
        let hash = WebTransportHash::new();
        hash.set_algorithm("sha-256");
        // The setter takes `&js_sys::Object`; `Uint8Array` extends `Object`
        // so we can pass it directly. The WebTransport spec accepts any
        // BufferSource (TypedArray or ArrayBuffer) for `value`.
        hash.set_value(value.unchecked_ref());
        hashes.push(&hash);
    }

    if hashes.length() == 0 {
        return None;
    }

    let options = WebTransportOptions::new();
    options.set_server_certificate_hashes(&hashes);
    log!(format!(
        "WebTransport using serverCertificateHashes ({} entr{})",
        hashes.length(),
        if hashes.length() == 1 { "y" } else { "ies" }
    ));
    Some(options)
}

/// Decode a standard-alphabet base64 string into bytes via JS `atob`.
///
/// We cannot pull in the `base64` crate just for this one call (bundle bloat)
/// and `atob` returns a binary-safe "latin-1" JS string — every UTF-16 code
/// unit is in `0..=0xFF`. Going through Rust's `String` would re-encode as
/// UTF-8 and corrupt bytes >= 0x80, so we read the UTF-16 code units directly
/// from the `JsString` and truncate each to a `u8`.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let window = web_sys::window()?;
    let atob = Reflect::get(&window, &JsValue::from_str("atob")).ok()?;
    let atob_fn: js_sys::Function = atob.dyn_into().ok()?;
    let decoded = atob_fn.call1(&JsValue::NULL, &JsValue::from_str(s)).ok()?;
    let js_str: JsString = decoded.dyn_into().ok()?;
    // `JsString::iter` yields UTF-16 code units (`u16`). For atob output
    // every unit is guaranteed to be in 0..=0xFF, so the truncation to u8
    // is safe; we still use `& 0xFF` defensively.
    let bytes: Vec<u8> = js_str.iter().map(|c| (c & 0xFF) as u8).collect();
    Some(bytes)
}

/// Maximum length-prefixed frame payload size on a persistent stream (4 MB).
/// Matches the server's `MAX_FRAME_SIZE` so honest senders never trip the
/// server-side guard.  Frames larger than this are dropped client-side rather
/// than being written and immediately torn down by the receiver.
pub const PERSISTENT_STREAM_MAX_FRAME_SIZE: usize = 4_000_000;

/// Holds a persistent unidirectional stream and its writer so the QUIC stream
/// stays open across multiple sends, preserving packet ordering.
///
/// Each `PersistentSendStream` corresponds to **one** QUIC unidirectional
/// stream.  We open one of these per media type (audio, video, screen,
/// control) so that head-of-line blocking on one media type cannot stall the
/// others.  See `webtransport-client` / Phase 2 architectural fix for
/// background.
pub struct PersistentSendStream {
    /// The underlying `WritableStream` (QUIC send stream).  Kept alive so the
    /// stream is not garbage-collected while the writer is in use.
    _stream: WritableStream,
    /// The writer acquired from `_stream`.  Reused for every reliable send
    /// on this media type.  The writer enforces FIFO ordering of writes; we
    /// do not need to serialise writes ourselves once the writer is created.
    writer: WritableStreamDefaultWriter,
    /// Per-entry identity token used to defeat a concurrent-eviction race
    /// (issue #773).
    ///
    /// The send path captures a clone of this `Rc<()>` *while holding the
    /// map lock*, alongside the writer it is about to use.  If the write
    /// later fails, the error handler must remove this entry — but only if
    /// the entry currently in the map is the *same* entry the failing
    /// writer came from.  Without this token, two concurrent failing
    /// senders for the same key plus a fresh sender opening a new entry in
    /// between can result in the fresh entry being evicted by a stale
    /// error handler.  Identity is compared via [`std::rc::Rc::ptr_eq`].
    identity_token: Rc<()>,
}

/// Map of per-media-type persistent send streams, keyed by an opaque `u8`
/// stream identifier.  The transport layer does not interpret the key — that
/// is the caller's responsibility (see `MediaStreamKey` in
/// `videocall-client/src/connection/webmedia.rs`).
///
/// The map is wrapped in an `AsyncMutex` so that the lazy-creation path is
/// race-free across concurrent `send_on_persistent_stream` invocations.  In
/// single-threaded WASM the mutex is purely a re-entrancy guard across
/// `.await` points; it does **not** block writes once the stream exists.
pub type PersistentStreamMap = Rc<AsyncMutex<HashMap<u8, PersistentSendStream>>>;

/// Construct an empty persistent-stream map.  Stored inside `WebTransportTask`
/// and threaded through `send_on_persistent_stream`.
pub fn new_persistent_stream_map() -> PersistentStreamMap {
    Rc::new(AsyncMutex::new(HashMap::new()))
}

/// Internal abstraction so [`remove_if_token_matches`] can be unit-tested in
/// pure Rust without constructing a real `PersistentSendStream` (which
/// requires JS `WritableStream`/`WritableStreamDefaultWriter` instances and
/// therefore only works under `wasm32-unknown-unknown`).
trait HasIdentityToken {
    fn identity_token(&self) -> &Rc<()>;
}

impl HasIdentityToken for PersistentSendStream {
    fn identity_token(&self) -> &Rc<()> {
        &self.identity_token
    }
}

/// Compare-and-remove for persistent-stream map entries (issue #773).
///
/// Removes the entry at `key` **only** if the entry currently in the map
/// has the same identity token as `captured_token` (as compared via
/// [`std::rc::Rc::ptr_eq`]).  Returns `true` if the entry was removed.
///
/// ## Why this is needed
///
/// The send path takes a brief lock on the map, clones the writer, and
/// releases the lock before awaiting the write.  If the write fails, the
/// error handler must re-acquire the lock and evict the broken entry.
///
/// Without identity tracking, the following race is possible:
///
/// 1. Senders A and B each acquire writers from entry `e1` for the same
///    key.  Both writes are in flight.
/// 2. The underlying stream dies.  Sender A's write returns an error.
/// 3. Sender A re-acquires the lock and removes `e1`.
/// 4. Sender C (a fresh send for the same key) acquires the lock, sees
///    the key vacant, opens a new stream and inserts entry `e2`.
/// 5. Sender B's write returns an error.  Sender B re-acquires the lock
///    and — *without* an identity check — removes `e2`, orphaning the
///    healthy new stream.
///
/// With per-entry identity tokens captured at writer-clone time, sender
/// B sees that `e2.identity_token` is not the token it captured from
/// `e1`, leaves `e2` alone, and the healthy stream survives.
fn remove_if_token_matches<V: HasIdentityToken>(
    map: &mut HashMap<u8, V>,
    key: u8,
    captured_token: &Rc<()>,
) -> bool {
    let matches = map
        .get(&key)
        .is_some_and(|entry| Rc::ptr_eq(entry.identity_token(), captured_token));
    if matches {
        map.remove(&key);
    }
    matches
}

/// Errors raised when attempting to parse a length-prefix-framed payload
/// out of a stream buffer.  Used by `parse_persistent_stream_frame` and
/// (indirectly) by the server-side reader at
/// `actix-api/src/webtransport/bridge.rs`.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameParseError {
    /// Less than 4 header bytes are available — caller should accumulate
    /// more data and retry.
    NeedMoreHeader,
    /// Header is present but the indicated payload is not fully buffered
    /// yet — caller should accumulate more data and retry.
    NeedMorePayload {
        /// Number of payload bytes still missing.
        missing: usize,
    },
    /// Decoded length is zero or exceeds `PERSISTENT_STREAM_MAX_FRAME_SIZE`.
    /// The stream is unrecoverable; caller should close it and drop any
    /// buffered data.
    InvalidLength(usize),
}

/// Encode `payload` as a `[u32 BE length][payload]` frame ready to be
/// written to a persistent WebTransport unidirectional stream.
///
/// The returned `Vec<u8>` is a single chunk — when handed to JS as one
/// `Uint8Array` and written via `writer.write_with_chunk`, the JS
/// WritableStream spec guarantees that the header and body cannot be
/// interleaved with another frame's bytes on the wire.
///
/// `payload.len()` is required to be at most `PERSISTENT_STREAM_MAX_FRAME_SIZE`;
/// callers must enforce this themselves (the send path drops over-sized
/// frames before reaching this helper).
pub fn frame_persistent_stream_payload(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Attempt to extract a complete `[u32 BE length][payload]` frame from
/// `buf`.  On success returns `Ok((payload, rest))` where `rest` is the
/// remaining unconsumed bytes.  This is the symmetric reverse of
/// `frame_persistent_stream_payload`.
///
/// `Err(FrameParseError::NeedMoreHeader)` and
/// `Err(FrameParseError::NeedMorePayload)` indicate that the caller must
/// accumulate more data and retry.  `Err(FrameParseError::InvalidLength)`
/// indicates an unrecoverable framing violation; the stream must be
/// closed.
///
/// The client itself does not currently call this — the client receives
/// framed payloads via the existing `handle_unidirectional_stream` in
/// `videocall-client/src/connection/webtransport.rs` which has its own
/// inline framing parser.  This helper is exported so the server-side
/// implementation and the unit tests can share the same protocol
/// definition.
pub fn parse_persistent_stream_frame(buf: &[u8]) -> Result<(&[u8], &[u8]), FrameParseError> {
    if buf.len() < 4 {
        return Err(FrameParseError::NeedMoreHeader);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len == 0 || len > PERSISTENT_STREAM_MAX_FRAME_SIZE {
        return Err(FrameParseError::InvalidLength(len));
    }
    let frame_end = 4 + len;
    if buf.len() < frame_end {
        return Err(FrameParseError::NeedMorePayload {
            missing: frame_end - buf.len(),
        });
    }
    Ok((&buf[4..frame_end], &buf[frame_end..]))
}

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

/// The status of a WebTransport connection. Used for status notifications.
#[derive(Clone, Debug, PartialEq)]
pub enum WebTransportStatus {
    /// Fired when a WebTransport connection has opened.
    Opened,
    /// Fired when a WebTransport connection has closed.
    Closed(JsValue),
    /// Fired when a WebTransport connection has failed.
    Error(JsValue),
    /// Closed/errored before `ready()` resolved — handshake never completed.
    ClosedBeforeReady(String),
    /// Closed/errored after `ready()` resolved — session was established first.
    ClosedAfterReady(String),
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum WebTransportError {
    #[error("{0}")]
    CreationError(String),
}

/// A handle to control the WebTransport connection.
///
/// When dropped, the underlying `WebTransport` is closed, which causes all
/// reader loops (datagrams, unidirectional, bidirectional) to terminate because
/// their `reader.read()` futures resolve with errors on a closed transport.
#[must_use = "the connection will be closed when the task is dropped"]
pub struct WebTransportTask {
    pub transport: Rc<WebTransport>,
    #[allow(dead_code)]
    notification: Callback<WebTransportStatus>,
    #[allow(dead_code)]
    listeners: [Promise; 2],
    /// Stored so the closures live as long as the task and are properly dropped
    /// instead of being leaked via `forget()`. The closed closure is wrapped in
    /// `Rc` because it is shared across multiple promise chains (`ready.catch`,
    /// `closed.then`, `closed.catch`).
    #[allow(dead_code)]
    opened_closure: Closure<dyn FnMut(JsValue)>,
    #[allow(dead_code)]
    closed_closure: Rc<Closure<dyn FnMut(JsValue)>>,
    /// Per-media-type persistent unidirectional send streams.  Lazily
    /// populated by `send_on_persistent_stream` on first send for each key.
    /// On stream-write error the entry is removed; the next send for that
    /// key opens a fresh stream.
    pub persistent_streams: PersistentStreamMap,
}

impl WebTransportTask {
    fn new(
        transport: Rc<WebTransport>,
        notification: Callback<WebTransportStatus>,
        listeners: [Promise; 2],
        opened_closure: Closure<dyn FnMut(JsValue)>,
        closed_closure: Rc<Closure<dyn FnMut(JsValue)>>,
    ) -> WebTransportTask {
        WebTransportTask {
            transport,
            notification,
            listeners,
            opened_closure,
            closed_closure,
            persistent_streams: new_persistent_stream_map(),
        }
    }
}

impl Drop for WebTransportTask {
    fn drop(&mut self) {
        // Close the underlying WebTransport session. This causes the reader
        // loops (datagrams, unidirectional streams, bidirectional streams) to
        // break out of their `reader.read()` await — the futures resolve with
        // errors on a closed transport, allowing the spawn_local tasks and
        // their captured Rc<WebTransport> clones to be cleaned up.
        self.transport.close();
    }
}

impl fmt::Debug for WebTransportTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebTransportTask")
    }
}

/// A WebTransport service attached to a user context.
#[derive(Default, Debug)]
pub struct WebTransportService {}

impl WebTransportService {
    /// Connects to a server through a WebTransport connection. Needs callbacks for
    /// datagrams, unidirectional streams, bidirectional streams, and status notifications.
    pub fn connect(
        url: &str,
        on_datagram: Callback<Vec<u8>>,
        on_unidirectional_stream: Callback<WebTransportReceiveStream>,
        on_bidirectional_stream: Callback<WebTransportBidirectionalStream>,
        notification: Callback<WebTransportStatus>,
    ) -> Result<WebTransportTask, WebTransportError> {
        let ConnectCommon(transport, listeners, opened_closure, closed_closure) =
            Self::connect_common(url, &notification)?;
        let transport = Rc::new(transport);

        Self::start_listening_incoming_datagrams(transport.datagrams(), on_datagram);
        Self::start_listening_incoming_unidirectional_streams(
            transport.incoming_unidirectional_streams(),
            on_unidirectional_stream,
        );
        Self::start_listening_incoming_bidirectional_streams(
            transport.incoming_bidirectional_streams(),
            on_bidirectional_stream,
        );

        Ok(WebTransportTask::new(
            transport,
            notification,
            listeners,
            opened_closure,
            closed_closure,
        ))
    }

    fn start_listening_incoming_unidirectional_streams(
        incoming_streams: ReadableStream,
        callback: Callback<WebTransportReceiveStream>,
    ) {
        let read_result: ReadableStreamDefaultReader =
            incoming_streams.get_reader().unchecked_into();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let read_result = JsFuture::from(read_result.read()).await;
                match read_result {
                    Err(_) => {
                        // Expected when the transport is closed (Drop or network
                        // failure).  Don't re-close — the transport is already
                        // shut down and a redundant close() throws when the
                        // session is still in "connecting" state.
                        break;
                    }
                    Ok(result) => {
                        let done = match Reflect::get(&result, &JsString::from("done")) {
                            Ok(val) => val.unchecked_into::<Boolean>(),
                            Err(e) => {
                                log!(
                                    "Failed to read 'done' from unidirectional stream result",
                                    &e
                                );
                                break;
                            }
                        };
                        if let Ok(value) = Reflect::get(&result, &JsString::from("value")) {
                            if value.is_undefined() {
                                break;
                            }
                            let value: WebTransportReceiveStream = value.unchecked_into();
                            callback.emit(value);
                        }
                        if done.is_truthy() {
                            break;
                        }
                    }
                }
            }
        });
    }

    fn start_listening_incoming_datagrams(
        datagrams: WebTransportDatagramDuplexStream,
        callback: Callback<Vec<u8>>,
    ) {
        let incoming_datagrams: ReadableStreamDefaultReader =
            datagrams.readable().get_reader().unchecked_into();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let read_result = JsFuture::from(incoming_datagrams.read()).await;
                match read_result {
                    Err(_) => {
                        // Expected when the transport is closed (Drop or network
                        // failure).  Don't re-close — see unidirectional handler.
                        break;
                    }
                    Ok(result) => {
                        let done = match Reflect::get(&result, &JsString::from("done")) {
                            Ok(val) => val.unchecked_into::<Boolean>(),
                            Err(e) => {
                                log!("Failed to read 'done' from datagram result", &e);
                                break;
                            }
                        };
                        if done.is_truthy() {
                            break;
                        }
                        let value: Uint8Array =
                            match Reflect::get(&result, &JsString::from("value")) {
                                Ok(val) => val.unchecked_into(),
                                Err(e) => {
                                    log!("Failed to read 'value' from datagram result", &e);
                                    break;
                                }
                            };
                        process_binary(&value, &callback);
                    }
                }
            }
        });
    }

    fn start_listening_incoming_bidirectional_streams(
        streams: ReadableStream,
        callback: Callback<WebTransportBidirectionalStream>,
    ) {
        let read_result: ReadableStreamDefaultReader = streams.get_reader().unchecked_into();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let read_result = JsFuture::from(read_result.read()).await;
                match read_result {
                    Err(_) => {
                        // Expected when the transport is closed (Drop or network
                        // failure).  Don't re-close — see unidirectional handler.
                        break;
                    }
                    Ok(result) => {
                        let done = match Reflect::get(&result, &JsString::from("done")) {
                            Ok(val) => val.unchecked_into::<Boolean>(),
                            Err(e) => {
                                log!("Failed to read 'done' from bidirectional stream result", &e);
                                break;
                            }
                        };
                        if let Ok(value) = Reflect::get(&result, &JsString::from("value")) {
                            if value.is_undefined() {
                                break;
                            }
                            let value: WebTransportBidirectionalStream = value.unchecked_into();
                            callback.emit(value);
                        }
                        if done.is_truthy() {
                            break;
                        }
                    }
                }
            }
        });
    }

    fn connect_common(
        url: &str,
        notification: &Callback<WebTransportStatus>,
    ) -> Result<ConnectCommon, WebTransportError> {
        // Production path: bare `WebTransport::new(url)` so the browser uses
        // the standard CA / system trust store. E2E and local-dev opt in to
        // `serverCertificateHashes` by setting `window.__VC_WT_CERT_HASHES__`
        // to an array of base64 strings BEFORE the wasm boots (Playwright
        // does this via `addInitScript` in `e2e/helpers/auth-context.ts`).
        // Each entry is the SHA-256 of the DER-encoded server cert, matching
        // the W3C WebTransport `serverCertificateHashes` shape:
        //   { algorithm: "sha-256", value: Uint8Array }
        // If the global is unset, undefined, or not an array of strings, we
        // fall back to bare construction — this MUST remain a no-op for
        // production builds.
        let transport = match read_wt_cert_hash_options() {
            Some(options) => WebTransport::new_with_options(url, &options),
            None => WebTransport::new(url),
        };
        let transport = transport.map_err(|e| {
            WebTransportError::CreationError(format!("Failed to create WebTransport: {e:?}"))
        })?;

        // Track whether the handshake (`ready()`) has completed, so that
        // subsequent close/error events can be classified correctly.
        let handshake_complete = Rc::new(Cell::new(false));
        // Guard against emitting connection-lost more than once per connection
        // (browser may fire both `closed` and `ready.catch` for the same failure).
        let fired = Rc::new(Cell::new(false));

        let notify = notification.clone();
        let hs_flag = handshake_complete.clone();

        // Both closures are stored in the WebTransportTask struct so they are
        // dropped when the task is dropped, instead of being leaked via
        // `forget()`. Previously, every reconnection/re-election cycle would
        // permanently leak two closures into WASM linear memory.
        let opened_closure = Closure::wrap(Box::new(move |_: JsValue| {
            hs_flag.set(true);
            notify.emit(WebTransportStatus::Opened);
        }) as Box<dyn FnMut(JsValue)>);

        let notify = notification.clone();
        let hs_flag_closed = handshake_complete.clone();
        let fired_closed = fired.clone();
        // `closed_closure` is shared via `Rc` because it is referenced by
        // multiple promise chains (`ready.catch`, `closed.then`, `closed.catch`).
        let closed_closure = Rc::new(Closure::wrap(Box::new(move |e: JsValue| {
            if fired_closed.replace(true) {
                return; // already emitted
            }
            let msg = e.as_string().unwrap_or_else(|| format!("{e:?}"));
            if hs_flag_closed.get() {
                notify.emit(WebTransportStatus::ClosedAfterReady(msg));
            } else {
                notify.emit(WebTransportStatus::ClosedBeforeReady(msg));
            }
        }) as Box<dyn FnMut(JsValue)>));
        let ready = transport
            .ready()
            .then(&opened_closure)
            .catch(&closed_closure);
        let closed = transport
            .closed()
            .then(&closed_closure)
            .catch(&closed_closure);

        {
            let listeners = [ready, closed];
            Ok(ConnectCommon(
                transport,
                listeners,
                opened_closure,
                closed_closure,
            ))
        }
    }
}
struct ConnectCommon(
    WebTransport,
    [Promise; 2],
    Closure<dyn FnMut(JsValue)>,
    Rc<Closure<dyn FnMut(JsValue)>>,
);

pub fn process_binary(bytes: &Uint8Array, callback: &Callback<Vec<u8>>) {
    let data = bytes.to_vec();
    callback.emit(data);
}

impl WebTransportTask {
    /// Sends data to a WebTransport connection via datagram.
    ///
    /// Datagrams are unreliable and expendable by design (heartbeats, RTT probes,
    /// diagnostics). If the writable side is already locked by a concurrent write,
    /// the packet is silently dropped instead of killing the entire transport
    /// connection. Only fatal errors (transport closed, write failure after
    /// acquiring the lock) close the transport.
    pub fn send_datagram(transport: Rc<WebTransport>, data: Vec<u8>) {
        wasm_bindgen_futures::spawn_local(async move {
            let stream = transport.datagrams();
            let writable: WritableStream = stream.writable();
            if writable.locked() {
                DATAGRAM_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
                log!("datagram dropped (stream busy)");
                return;
            }
            let writer = match writable.get_writer() {
                Ok(w) => w,
                Err(e) => {
                    log!("error: ", format!("{e:?}"));
                    transport.close();
                    return;
                }
            };
            let data = Uint8Array::from(data.as_slice());
            let result = match JsFuture::from(writer.ready()).await {
                Ok(_) => JsFuture::from(writer.write_with_chunk(&data)).await,
                err => err,
            };
            writer.release_lock();
            if let Err(e) = result {
                log!(
                    "datagram write failed, closing transport:",
                    format!("{e:?}")
                );
                transport.close();
            }
        });
    }

    /// Sends a length-prefix-framed packet on a **persistent** unidirectional
    /// QUIC stream identified by `stream_key`.
    ///
    /// Phase 2 of the WebTransport freeze fix (HCL discussion #756): instead of
    /// opening a fresh QUIC stream per packet (the legacy
    /// `send_unidirectional_stream` behaviour), each media type reuses a
    /// long-lived stream.  This collapses ~80 streams/sec/sender to ~3
    /// streams/connection and eliminates the relay-side `accept_uni` storm and
    /// tokio-scheduler reorder that produced the user's five-minute WT
    /// audio+video freeze.
    ///
    /// ## Framing
    ///
    /// Every frame is written as `[u32 BE length][payload]`.  The length
    /// excludes the 4-byte header.  Both client and server are framed-only:
    /// there is no per-packet-stream fallback.
    ///
    /// The header is emitted as a single `write_with_chunk` together with the
    /// payload so the JS WritableStream cannot interleave the length prefix of
    /// one frame with the payload of another (chunks are atomic — the WebIDL
    /// spec guarantees no sub-chunk interleaving).
    ///
    /// ## Concurrency
    ///
    /// The lazy-creation path is guarded by a per-task `AsyncMutex` so that
    /// two concurrent `send_on_persistent_stream` invocations for the same
    /// key cannot both observe `None` and race to open a stream.  Once the
    /// stream exists, the WritableStream writer enforces FIFO ordering of
    /// writes internally; we do not need to hold the mutex across the
    /// `write_with_chunk` await.
    ///
    /// ## Error handling
    ///
    /// On any write error the entry for `stream_key` is removed from the
    /// map.  The next send for that key will open a fresh stream.  The
    /// transport is NOT closed — a single failed frame must not kill the
    /// session for all participants.  The receiver detects the closed stream
    /// (via EOF) and discards any partial buffer; framing guarantees that a
    /// truncated frame becomes a clean stream-closed event rather than a
    /// silently-corrupted payload.
    pub fn send_on_persistent_stream(
        transport: Rc<WebTransport>,
        streams: PersistentStreamMap,
        stream_key: u8,
        data: Vec<u8>,
    ) {
        // Frame-size and emptiness guards.  Mirrors the server-side
        // `read_length_prefixed_frame` contract: zero-length payloads are
        // treated as malformed (no legitimate caller has a reason to send
        // one), and over-large payloads are rejected up front to avoid
        // writing a bad header that would force an immediate stream restart
        // on the receiver.
        if data.is_empty() {
            log!("persistent stream send dropped: empty payload");
            return;
        }
        if data.len() > PERSISTENT_STREAM_MAX_FRAME_SIZE {
            log!(
                "persistent stream send dropped: payload exceeds max frame size,",
                data.len() as u32,
                ">",
                PERSISTENT_STREAM_MAX_FRAME_SIZE as u32
            );
            return;
        }

        wasm_bindgen_futures::spawn_local(async move {
            // Captured alongside the writer when we acquire it from the
            // map; passed into the error handler so a stale failing send
            // can only evict the entry it actually used.  See the eviction
            // race notes in `remove_if_token_matches` (issue #773).
            let mut captured_token: Option<Rc<()>> = None;
            let result: Result<(), anyhow::Error> = async {
                // --- Wait for the transport handshake ------------------------
                // ready() resolves once the underlying QUIC session is
                // established.  Calling create_unidirectional_stream() before
                // ready() resolves throws.
                JsFuture::from(transport.ready())
                    .await
                    .map_err(|e| anyhow!("transport.ready() failed: {:?}", e))?;

                // --- Ensure a writer exists for this stream_key --------------
                // Lock the map across the create-or-reuse decision so two
                // concurrent senders for the same key cannot both observe
                // `None` and open duplicate streams.
                let writer = {
                    use std::collections::hash_map::Entry;
                    let mut map = streams.lock().await;
                    if let Entry::Vacant(entry) = map.entry(stream_key) {
                        let stream: WritableStream =
                            JsFuture::from(transport.create_unidirectional_stream())
                                .await
                                .map_err(|e| {
                                    anyhow!(
                                        "failed to create unidirectional stream for key {}: {:?}",
                                        stream_key,
                                        e
                                    )
                                })?
                                .unchecked_into();
                        let writer = stream
                            .get_writer()
                            .map_err(|e| anyhow!("error getting writer: {:?}", e))?;
                        entry.insert(PersistentSendStream {
                            _stream: stream,
                            writer,
                            identity_token: Rc::new(()),
                        });
                    }
                    // Clone the writer JsValue so we can release the map
                    // lock before the (potentially long) write await.
                    // Capture the entry's identity token alongside the
                    // writer so the error handler can verify we are
                    // evicting the same entry our writer came from.
                    let entry = map
                        .get(&stream_key)
                        .expect("entry was just inserted or already existed");
                    captured_token = Some(entry.identity_token.clone());
                    entry.writer.clone()
                };

                // --- Build the framed payload --------------------------------
                // [u32 BE length][payload] in a single Uint8Array so the
                // browser cannot split the header off from its body.
                let framed = frame_persistent_stream_payload(&data);
                let chunk = Uint8Array::from(framed.as_slice());

                // --- Write the frame ----------------------------------------
                // writer.ready() resolves when there is backpressure room.
                // writer.write() returns immediately after enqueueing.
                //
                // Uplink-saturation observable (#1219 prerequisite): a slow link
                // signals backpressure by leaving ready() PENDING (it does NOT
                // reject — that only happens on teardown, which the drop counter
                // catches). So we time how long ready() blocks: a wait beyond
                // READY_STALL_THRESHOLD_MS means the uplink is saturated but
                // alive, a case UNISTREAM_DROP_COUNT structurally cannot see.
                //
                // HOW THE COUNTER ACCUMULATES (do NOT serialise this!): each call
                // to `send_on_persistent_stream` spawn_local's its OWN future and
                // the map lock is released above (before this await), so during a
                // stall there are MANY concurrent in-flight frame futures, each
                // holding a clone of the same writer. `WritableStreamDefaultWriter
                // .ready` returns the writer's single current `[[readyPromise]]`,
                // so every concurrent future observes the SAME pending promise and
                // they all resolve together when backpressure clears. Each future
                // independently timed its own (staggered) `start`, so a multi-
                // hundred-ms stall with K frames in flight produces ~K increments
                // when the promise resolves — that is how the consumer's
                // 3-in-2000ms window is reached. If this path is ever refactored
                // into a single serialised send loop, the counter would cap at 1
                // per stall and the saturation signal would silently break. NOTE:
                // because increments are stamped at ready()-RESOLUTION (not stall
                // onset), detection of a sustained cliff lags by up to ~one window.
                //
                // We only measure on the ESTABLISHED media path — `captured_token`
                // is `Some` here because the writer was just acquired above (it
                // is set unconditionally alongside `entry.writer.clone()`), so
                // the gate mirrors the drop counter and excludes handshake /
                // create-stream stalls. `perf_now_ms()` is monotonic
                // (`performance.now()`); if it is unavailable we simply skip the
                // measurement (the wait still happens, just unobserved).
                let ready_wait_start_ms = perf_now_ms();
                JsFuture::from(writer.ready())
                    .await
                    .map_err(|e| anyhow!("writer.ready() failed: {:?}", e))?;
                // captured_token.is_some() is guaranteed true on this line, but
                // assert the gate explicitly so the signal can never be polluted
                // by a control/handshake stream if this code is later refactored.
                if captured_token.is_some() {
                    if let (Some(start), Some(end)) = (ready_wait_start_ms, perf_now_ms()) {
                        // Pure threshold + increment lives in `record_ready_stall`
                        // so the saturation decision is unit-testable natively.
                        record_ready_stall(end - start);
                    }
                }
                JsFuture::from(writer.write_with_chunk(&chunk))
                    .await
                    .map_err(|e| anyhow!("write_with_chunk failed: {:?}", e))?;
                Ok(())
            }
            .await;

            if let Err(e) = result {
                // A media frame just failed to go out on this uplink. Count it
                // so the sender AQ can self-trigger a step-down on WebTransport
                // (#1104) — the unistream analogue of the WebSocket send-buffer
                // drop counter.
                //
                // ONLY count failures at the write stage (`writer.ready()` /
                // `write_with_chunk`), i.e. after a writer was acquired
                // (`captured_token.is_some()`). Failures BEFORE the writer
                // existed (`transport.ready()` or
                // `create_unidirectional_stream()`) are transport teardown /
                // re-election artifacts, NOT uplink saturation: during a WT
                // reconnect the old session enters QUIC draining while frames
                // are still dispatched, and counting those would spuriously
                // trip the self-shed right after a reconnect on flaky /
                // high-latency links. Gating on `captured_token` keeps the
                // signal to genuine send-path backpressure/reset events.
                // Incremented before eviction so the count reflects the dropped
                // frame regardless of the eviction race outcome below.
                if captured_token.is_some() {
                    record_unistream_drop();
                }
                // Stream is broken — remove it from the map so the next
                // send for this key opens a fresh stream.  We compare
                // identity tokens to defeat the concurrent-eviction race
                // (issue #773): if a fresh sender has already opened a
                // new entry while we were awaiting the failing write, we
                // must not evict the fresh entry.
                //
                // The map lock is re-acquired here because the lock taken
                // above to acquire the writer was scoped and has been
                // released — we cannot hold it across the write await.
                let mut map = streams.lock().await;
                let removed = match captured_token {
                    Some(token) => remove_if_token_matches(&mut map, stream_key, &token),
                    None => {
                        // We failed before acquiring a writer (e.g.
                        // transport.ready() or create_unidirectional_stream
                        // failed), so no entry was ever inserted on our
                        // behalf — nothing to evict.
                        false
                    }
                };
                log!(
                    "persistent stream send failed (stream reset, frame dropped):",
                    e.to_string(),
                    "evicted:",
                    removed
                );
            }
        });
    }

    /// Sends data to a WebTransport connection via a unidirectional stream.
    ///
    /// **Legacy per-packet path.** Used only as a fallback for transports that
    /// have not migrated to `send_on_persistent_stream` yet.  Phase 2 of the
    /// WebTransport freeze fix replaces this with persistent per-media-type
    /// streams; new code paths should call `send_on_persistent_stream` instead.
    ///
    /// Stream errors (creation failure, write backpressure, QUIC congestion) are
    /// transient -- they affect only this single frame send. The transport is NOT
    /// closed on failure; if the transport is genuinely dead, the reader loops and
    /// the `closed` promise will detect it independently.
    pub fn send_unidirectional_stream(transport: Rc<WebTransport>, data: Vec<u8>) {
        wasm_bindgen_futures::spawn_local(async move {
            let result: Result<(), anyhow::Error> = async {
                JsFuture::from(transport.ready())
                    .await
                    .map_err(|e| anyhow!("{:?}", e))?;
                let stream: WritableStream =
                    JsFuture::from(transport.create_unidirectional_stream())
                        .await
                        .map_err(|e| anyhow!("failed to create Writeable stream {:?}", e))?
                        .unchecked_into();
                let writer = stream
                    .get_writer()
                    .map_err(|e| anyhow!("Error getting writer {:?}", e))?;
                let data = Uint8Array::from(data.as_slice());
                JsFuture::from(writer.ready())
                    .await
                    .map_err(|e| anyhow!("Error getting writer ready {:?}", e))?;
                JsFuture::from(writer.write_with_chunk(&data))
                    .await
                    .map_err(|e| anyhow!("Error writing to stream: {:?}", e))?;
                writer.release_lock();
                JsFuture::from(stream.close())
                    .await
                    .map_err(|e| anyhow!("Error closing stream {:?}", e))?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                // Transient stream error -- log and drop the packet. Do NOT
                // close the transport; a single failed frame should not kill
                // the entire connection for all participants.
                log!(
                    "unidirectional stream send failed (frame dropped):",
                    e.to_string()
                );
            }
        });
    }

    /// Sends data to a WebTransport connection via a bidirectional stream and
    /// reads the response.
    ///
    /// Stream errors are transient -- they affect only this single stream
    /// exchange. The transport is NOT closed on failure; if the transport is
    /// genuinely dead, the reader loops and the `closed` promise will detect it
    /// independently. The inner reader task will terminate naturally when the
    /// stream's readable side ends or errors out.
    pub fn send_bidirectional_stream(
        transport: Rc<WebTransport>,
        data: Vec<u8>,
        callback: Callback<Vec<u8>>,
    ) {
        wasm_bindgen_futures::spawn_local(async move {
            let result: Result<(), anyhow::Error> = {
                let transport = transport.clone();
                async move {
                    let stream = JsFuture::from(transport.create_bidirectional_stream()).await;
                    let stream: WebTransportBidirectionalStream =
                        stream.map_err(|e| anyhow!("{:?}", e))?.unchecked_into();
                    let readable: ReadableStreamDefaultReader =
                        stream.readable().get_reader().unchecked_into();
                    let (sender, receiver) = channel();
                    wasm_bindgen_futures::spawn_local(async move {
                        loop {
                            let read_result = JsFuture::from(readable.read()).await;
                            match read_result {
                                Err(e) => {
                                    // Stream read error -- log and stop reading.
                                    // Do NOT close the transport; this is a
                                    // single-stream failure.
                                    log!(
                                        "bidirectional stream read error (stopping reader):",
                                        format!("{e:?}")
                                    );
                                    break;
                                }
                                Ok(result) => {
                                    let done =
                                        match Reflect::get(&result, &JsString::from("done")) {
                                            Ok(val) => val.unchecked_into::<Boolean>(),
                                            Err(e) => {
                                                log!(
                                                    "Failed to read 'done' from bidi send reader result",
                                                    &e
                                                );
                                                break;
                                            }
                                        };
                                    if done.is_truthy() {
                                        break;
                                    }
                                    let value: Uint8Array =
                                        match Reflect::get(&result, &JsString::from("value")) {
                                            Ok(val) => val.unchecked_into(),
                                            Err(e) => {
                                                log!(
                                                    "Failed to read 'value' from bidi send reader result",
                                                    &e
                                                );
                                                break;
                                            }
                                        };
                                    process_binary(&value, &callback);
                                }
                            }
                        }
                        let _ = sender.send(true);
                    });
                    let writer = stream
                        .writable()
                        .get_writer()
                        .map_err(|e| anyhow!("{:?}", e))?;

                    JsFuture::from(writer.ready())
                        .await
                        .map_err(|e| anyhow!("{:?}", e))?;
                    let data = Uint8Array::from(data.as_slice());
                    let _ = JsFuture::from(writer.write_with_chunk(&data))
                        .await
                        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
                    JsFuture::from(writer.close())
                        .await
                        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
                    let _ = receiver.await?;
                    Ok(())
                }
            }
            .await;
            if let Err(e) = result {
                // Transient stream error -- log and drop the packet. Do NOT
                // close the transport; a single failed frame should not kill
                // the entire connection for all participants. The inner reader
                // task (if spawned) will terminate when the stream ends.
                log!(
                    "bidirectional stream send failed (frame dropped):",
                    e.to_string()
                );
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests for the length-prefix framing protocol.
//
// The framing helpers (`frame_persistent_stream_payload` and
// `parse_persistent_stream_frame`) are pure-Rust and run on the host target.
// The WebTransport send path itself is WASM-only (it depends on the JS
// WritableStream API) and is exercised by integration tests rather than
// unit tests.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod framing_tests {
    use super::*;

    #[test]
    fn frame_round_trips_byte_for_byte() {
        let payloads: Vec<Vec<u8>> = vec![
            vec![0x00],
            vec![0xFF; 1],
            (0u8..=255).collect(),
            b"the quick brown fox jumps over the lazy dog".to_vec(),
            vec![0xAA; 1500],
            vec![0x55; 64 * 1024],
        ];
        for payload in &payloads {
            let framed = frame_persistent_stream_payload(payload);
            assert_eq!(framed.len(), 4 + payload.len(), "framed length wrong");
            let (parsed, rest) =
                parse_persistent_stream_frame(&framed).expect("frame should parse back cleanly");
            assert_eq!(parsed, payload.as_slice(), "round-trip payload differs");
            assert!(rest.is_empty(), "no trailing bytes expected");
        }
    }

    #[test]
    fn parses_thousand_concatenated_frames_in_order() {
        // Simulate 1000 framed packets accumulated on the wire (the scenario
        // where the JS chunk boundary does not align with frame boundaries).
        // The parser must extract every payload in order and with no
        // length-prefix corruption.
        const N: usize = 1000;
        let mut originals: Vec<Vec<u8>> = Vec::with_capacity(N);
        let mut buffer: Vec<u8> = Vec::new();
        for i in 0..N {
            // Mix of small, medium, and occasionally larger frames to exercise
            // the parser at different boundary alignments.
            let len = match i % 5 {
                0 => 1,
                1 => 80,        // typical Opus audio frame size
                2 => 1200,      // datagram-MTU-sized
                3 => 8 * 1024,  // video delta range
                _ => 64 * 1024, // small keyframe range
            };
            let payload: Vec<u8> = (0..len).map(|j| ((i + j) & 0xFF) as u8).collect();
            buffer.extend_from_slice(&frame_persistent_stream_payload(&payload));
            originals.push(payload);
        }

        // Walk the buffer one frame at a time.  We deliberately use the
        // returned `rest` slice as the next iteration's input so that any
        // off-by-one bug in the consumed-byte count surfaces here.
        let mut cursor: &[u8] = &buffer;
        for (idx, expected) in originals.iter().enumerate() {
            let (parsed, rest) = parse_persistent_stream_frame(cursor)
                .unwrap_or_else(|e| panic!("frame {idx} failed to parse: {e:?}"));
            assert_eq!(parsed, expected.as_slice(), "frame {idx} payload mismatch");
            cursor = rest;
        }
        assert!(cursor.is_empty(), "all bytes should be consumed");
    }

    #[test]
    fn need_more_header_when_buffer_short() {
        for short in 0..4 {
            let buf = vec![0u8; short];
            assert_eq!(
                parse_persistent_stream_frame(&buf),
                Err(FrameParseError::NeedMoreHeader),
            );
        }
    }

    #[test]
    fn need_more_payload_when_body_short() {
        // Header claims 100 bytes but only 50 are present.
        let mut buf = (100u32).to_be_bytes().to_vec();
        buf.extend(std::iter::repeat_n(0u8, 50));
        match parse_persistent_stream_frame(&buf) {
            Err(FrameParseError::NeedMorePayload { missing }) => {
                assert_eq!(missing, 50);
            }
            other => panic!("expected NeedMorePayload, got {other:?}"),
        }
    }

    #[test]
    fn zero_length_is_invalid() {
        let buf = (0u32).to_be_bytes();
        assert_eq!(
            parse_persistent_stream_frame(&buf),
            Err(FrameParseError::InvalidLength(0)),
        );
    }

    #[test]
    fn oversized_length_is_invalid() {
        // One byte over the limit must be rejected.
        let too_big = (PERSISTENT_STREAM_MAX_FRAME_SIZE + 1) as u32;
        let mut buf = too_big.to_be_bytes().to_vec();
        // Pad with bogus bytes; the length check fires before we look at the body.
        buf.extend(std::iter::repeat_n(0u8, 8));
        assert_eq!(
            parse_persistent_stream_frame(&buf),
            Err(FrameParseError::InvalidLength(
                PERSISTENT_STREAM_MAX_FRAME_SIZE + 1
            )),
        );
    }

    #[test]
    fn max_size_payload_is_accepted() {
        // A payload at exactly the max size must round-trip.  We use a small
        // pattern so the test stays fast; the size is what we are validating.
        let payload = vec![0xC3u8; PERSISTENT_STREAM_MAX_FRAME_SIZE];
        let framed = frame_persistent_stream_payload(&payload);
        let (parsed, rest) =
            parse_persistent_stream_frame(&framed).expect("max-size frame must parse");
        assert_eq!(parsed.len(), PERSISTENT_STREAM_MAX_FRAME_SIZE);
        assert!(rest.is_empty());
    }

    /// Property: interleaving the wire bytes of two senders that each wrote
    /// `[len][payload]` as a single chunk must never decode into a corrupt
    /// frame.  The JS WritableStream guarantees no sub-chunk interleaving,
    /// so on the wire we only ever see fully-concatenated frames.  This
    /// test asserts that the *parser* respects that invariant: any input
    /// that is two adjacent valid frames decodes back to those two
    /// payloads exactly.
    #[test]
    fn two_adjacent_frames_decode_to_two_payloads() {
        let a: Vec<u8> = (0u8..=99).collect();
        let b: Vec<u8> = (100u8..=199).collect();
        let mut wire = frame_persistent_stream_payload(&a);
        wire.extend_from_slice(&frame_persistent_stream_payload(&b));

        let (got_a, rest) = parse_persistent_stream_frame(&wire).unwrap();
        assert_eq!(got_a, a.as_slice());
        let (got_b, rest2) = parse_persistent_stream_frame(rest).unwrap();
        assert_eq!(got_b, b.as_slice());
        assert!(rest2.is_empty());
    }

    /// Stream-restart simulation at the protocol level.
    ///
    /// The real WT stream-restart path lives behind the JS WebTransport API
    /// and is not unit-testable.  What *is* testable is the invariant the
    /// restart relies on: a truncated frame (write failed mid-write, stream
    /// reset on the wire) must not be silently consumed by the parser as if
    /// it were a complete frame.  We assert that the parser reports
    /// "NeedMorePayload" — the receiver then sees EOF on the stream and
    /// discards the partial buffer.  When the sender opens a fresh stream
    /// for the next send, the receiver starts a new buffer.  The framing
    /// protocol is what makes this clean.
    #[test]
    fn truncated_frame_is_detected_not_silently_consumed() {
        let payload = vec![0xABu8; 500];
        let framed = frame_persistent_stream_payload(&payload);

        // Drop the last byte to simulate a mid-frame stream reset.
        let truncated = &framed[..framed.len() - 1];

        match parse_persistent_stream_frame(truncated) {
            Err(FrameParseError::NeedMorePayload { missing }) => {
                assert_eq!(missing, 1, "must report the exact shortfall");
            }
            other => panic!("truncated frame must surface as NeedMorePayload, got {other:?}"),
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Eviction-race tests (issue #773).
    //
    // These exercise the pure-Rust `remove_if_token_matches` helper, which
    // implements the compare-and-remove logic used by the
    // `send_on_persistent_stream` error handler.  Because the real
    // `PersistentSendStream` contains JS WritableStream handles we cannot
    // construct on the host, we use a tiny test stand-in that carries only
    // the identity token.
    // ─────────────────────────────────────────────────────────────────────

    /// Minimal `HasIdentityToken` impl for host-target tests.  Real
    /// `PersistentSendStream` instances cannot be constructed without a
    /// live WebTransport session, so we use this to drive the eviction
    /// helper directly.
    struct TestEntry {
        token: Rc<()>,
    }

    impl HasIdentityToken for TestEntry {
        fn identity_token(&self) -> &Rc<()> {
            &self.token
        }
    }

    /// Baseline: a matching token evicts; a non-matching token does not.
    #[test]
    fn remove_if_token_matches_basic() {
        let token = Rc::new(());
        let other = Rc::new(());
        let mut map: HashMap<u8, TestEntry> = HashMap::new();
        map.insert(
            7,
            TestEntry {
                token: token.clone(),
            },
        );

        // Wrong token: must not evict.
        assert!(!remove_if_token_matches(&mut map, 7, &other));
        assert!(map.contains_key(&7));

        // Correct token: evicts.
        assert!(remove_if_token_matches(&mut map, 7, &token));
        assert!(!map.contains_key(&7));

        // Missing key: returns false, no panic.
        assert!(!remove_if_token_matches(&mut map, 7, &token));
    }

    /// The race that motivated issue #773.
    ///
    /// Scenario (single-threaded WASM, so ordering is fully deterministic
    /// once we step through it):
    ///
    /// 1. Sender A and sender B both clone the writer from entry `e1` for
    ///    the same `stream_key`, each capturing `e1.token` (same `Rc<()>`
    ///    underneath).
    /// 2. The QUIC stream backing `e1` dies.  Sender A's write fails first.
    /// 3. Sender A re-acquires the map lock and runs the eviction check
    ///    against its captured token.  `e1` is still in the map with the
    ///    matching token, so `e1` is evicted.
    /// 4. Sender C arrives, sees the key vacant, opens a fresh stream
    ///    and inserts `e2` with a *new* identity token.
    /// 5. Sender B's write finally fails and runs the eviction check
    ///    against the token it captured from `e1`.  The map currently
    ///    holds `e2` whose token does NOT match — `e2` must survive.
    ///
    /// Without the identity-token check, step 5 would orphan a healthy
    /// stream and cause the next sender for `stream_key` to needlessly
    /// open yet another fresh stream — the precise bug #773 describes.
    #[test]
    fn fresh_entry_survives_stale_error_handler_eviction() {
        const KEY: u8 = 3;

        // Step 1: A and B both capture e1's token.
        let mut map: HashMap<u8, TestEntry> = HashMap::new();
        let e1_token = Rc::new(());
        map.insert(
            KEY,
            TestEntry {
                token: e1_token.clone(),
            },
        );
        let captured_by_a = e1_token.clone();
        let captured_by_b = e1_token.clone();

        // Step 3: A's error handler runs first, evicts e1.
        assert!(
            remove_if_token_matches(&mut map, KEY, &captured_by_a),
            "A's eviction should remove e1 because its token still matches",
        );
        assert!(
            !map.contains_key(&KEY),
            "e1 must be gone after A's eviction"
        );

        // Step 4: Fresh sender C inserts e2 with a brand-new token.
        let e2_token = Rc::new(());
        assert!(
            !Rc::ptr_eq(&e1_token, &e2_token),
            "test setup invariant: e1 and e2 must have distinct tokens",
        );
        map.insert(
            KEY,
            TestEntry {
                token: e2_token.clone(),
            },
        );

        // Step 5: B's stale error handler tries to evict.  Without the
        // token check this would remove e2 and orphan the healthy stream.
        // With the check, B sees the mismatch and leaves e2 alone.
        assert!(
            !remove_if_token_matches(&mut map, KEY, &captured_by_b),
            "B's stale eviction must NOT remove the fresh entry e2",
        );
        assert!(map.contains_key(&KEY), "e2 must survive the stale eviction",);
        assert!(
            Rc::ptr_eq(&map.get(&KEY).unwrap().token, &e2_token),
            "the entry under KEY must still be e2 (its token is unchanged)",
        );
    }

    /// Variant: two stale error handlers and one fresh insert, in the
    /// reverse interleaving order (B fires before A re-acquires the lock).
    /// Establishes that the order of failing senders does not affect the
    /// invariant — only the captured-token comparison matters.
    #[test]
    fn token_check_is_order_independent_under_multiple_stale_handlers() {
        const KEY: u8 = 9;
        let mut map: HashMap<u8, TestEntry> = HashMap::new();
        let e1_token = Rc::new(());
        map.insert(
            KEY,
            TestEntry {
                token: e1_token.clone(),
            },
        );
        let captured_by_a = e1_token.clone();
        let captured_by_b = e1_token.clone();

        // B fires first this time.
        assert!(remove_if_token_matches(&mut map, KEY, &captured_by_b));
        assert!(!map.contains_key(&KEY));

        // Fresh sender opens e2.
        let e2_token = Rc::new(());
        map.insert(
            KEY,
            TestEntry {
                token: e2_token.clone(),
            },
        );

        // A's (now stale) error handler must NOT evict e2.
        assert!(!remove_if_token_matches(&mut map, KEY, &captured_by_a));
        assert!(map.contains_key(&KEY));
        assert!(Rc::ptr_eq(&map.get(&KEY).unwrap().token, &e2_token));
    }

    // ─────────────────────────────────────────────────────────────────────
    // WebTransport uplink-saturation signal (#1219 prerequisite).
    //
    // These tests pin the INCREMENT side of the signal: the threshold→count
    // decision that the encoder AQ self-shed depends on. The counterpart
    // DECISION side (consumer window/threshold) is pinned by the existing
    // `videocall-aq` `evaluate_self_congestion` tests. Together they cover
    // both halves of the WT self-shed that replaces the relay's room-wide
    // CONGESTION cut (#1219).
    //
    // Why NATIVE `#[test]` and not `#[wasm_bindgen_test]`: the increment is
    // pure arithmetic over an elapsed-ms `f64` once the producer-side seam
    // (`is_ready_stall` / `record_ready_stall`) is extracted, so it runs on
    // the host target with no JS. This deliberately avoids the browser wasm
    // harness, which is known to silently no-op `#[wasm_bindgen_test]` on
    // this dev box (false-green). The real `writer.ready().await` and the
    // `performance.now()` reads bracketing it remain at the WASM-only call
    // site and are unchanged by this seam.
    // ─────────────────────────────────────────────────────────────────────

    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    /// Serialises the tests that mutate the process-global
    /// `UNISTREAM_READY_STALL_COUNT` so their before/after deltas are not
    /// corrupted by parallel test execution (the default for `cargo test`).
    /// The pure `is_ready_stall` boundary tests do not touch the counter and
    /// do not need this guard.
    static STALL_COUNTER_GUARD: Mutex<()> = Mutex::new(());

    /// Serialises tests that depend on the process-global dynamic threshold
    /// (`READY_STALL_THRESHOLD_MS`). Tests that assert boundary behavior must
    /// hold this guard and reset the threshold to the floor before asserting,
    /// otherwise a parallel test that raised the threshold would corrupt them.
    static THRESHOLD_GUARD: Mutex<()> = Mutex::new(());

    /// Reset the dynamic threshold to the floor (250 ms) for tests.
    fn reset_threshold_to_floor() {
        set_ready_stall_threshold_ms(READY_STALL_THRESHOLD_MS_FLOOR);
    }

    // --- Pure threshold predicate: the mutation target ---------------------
    // The single source of truth for the DEFAULT boundary is
    // `READY_STALL_THRESHOLD_MS_FLOOR` (250.0). The effective threshold is
    // dynamic (raised when dual-streaming) but the floor is pinned by tests.
    // Mutating either the `>` (to `>=`) or the floor constant must break at
    // least one of the boundary tests below.

    #[test]
    fn threshold_static_initializer_matches_floor() {
        // Pin the bit-pattern literal (READY_STALL_THRESHOLD_MS_INIT_BITS) to
        // the floor constant. If someone changes the literal without updating
        // the floor (or vice versa), this fails immediately. Does NOT call
        // reset_threshold_to_floor() — reads the actual static initializer.
        assert_eq!(
            f64::from_bits(READY_STALL_THRESHOLD_MS_INIT_BITS),
            READY_STALL_THRESHOLD_MS_FLOOR,
            "static initializer bit pattern must decode to the floor (250.0)",
        );
        assert_eq!(
            READY_STALL_THRESHOLD_MS_FLOOR, 250.0,
            "floor must be exactly 250.0 ms",
        );
    }

    #[test]
    fn ready_stall_just_below_threshold_is_not_a_stall() {
        let _guard = THRESHOLD_GUARD.lock().unwrap();
        reset_threshold_to_floor();
        // One frame interval under the threshold: an ordinary
        // bursty-but-recovering link must NOT register as saturation.
        assert!(
            !is_ready_stall(READY_STALL_THRESHOLD_MS_FLOOR - 1.0),
            "a wait below the threshold must not count as a stall",
        );
    }

    #[test]
    fn ready_stall_exactly_at_threshold_is_not_a_stall() {
        let _guard = THRESHOLD_GUARD.lock().unwrap();
        reset_threshold_to_floor();
        // Boundary pin: the comparison is strictly `>`, so a wait of EXACTLY
        // the threshold is not yet a stall. Flipping `>` to `>=` flips this.
        assert!(
            !is_ready_stall(READY_STALL_THRESHOLD_MS_FLOOR),
            "a wait of exactly the threshold must NOT count (strict `>`)",
        );
    }

    #[test]
    fn ready_stall_just_above_threshold_is_a_stall() {
        let _guard = THRESHOLD_GUARD.lock().unwrap();
        reset_threshold_to_floor();
        // One millisecond over the threshold: the smallest wait that must
        // register. Flipping the constant up (or the `>` away) flips this.
        assert!(
            is_ready_stall(READY_STALL_THRESHOLD_MS_FLOOR + 1.0),
            "a wait just above the threshold must count as a stall",
        );
    }

    #[test]
    fn ready_stall_well_above_threshold_is_a_stall() {
        let _guard = THRESHOLD_GUARD.lock().unwrap();
        reset_threshold_to_floor();
        // A multi-second cliff — the case the signal exists to catch.
        assert!(
            is_ready_stall(2_000.0),
            "a multi-second wait must count as a stall",
        );
    }

    #[test]
    fn ready_stall_boundary_is_pinned_to_250ms_absolute() {
        let _guard = THRESHOLD_GUARD.lock().unwrap();
        reset_threshold_to_floor();
        // This test pins the 250 ms FLOOR boundary in ABSOLUTE terms, matching
        // the constant's documented contract ("250 ms is ~8x a 30 fps frame
        // interval"). It is the mutation guard for the floor constant itself:
        // changing 250.0 to any other value breaks exactly one of these three
        // assertions.
        assert!(
            !is_ready_stall(249.0),
            "249 ms must NOT be a stall (just under the 250 ms boundary)",
        );
        assert!(
            !is_ready_stall(250.0),
            "exactly 250 ms must NOT be a stall (strict `>` at the boundary)",
        );
        assert!(
            is_ready_stall(251.0),
            "251 ms must be a stall (just over the 250 ms boundary)",
        );
    }

    // --- Increment side effect on the process-global counter ---------------

    #[test]
    fn record_ready_stall_increments_counter_once_above_threshold() {
        let _guard = STALL_COUNTER_GUARD.lock().unwrap();
        let _tguard = THRESHOLD_GUARD.lock().unwrap();
        reset_threshold_to_floor();
        let before = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);

        let counted = record_ready_stall(READY_STALL_THRESHOLD_MS_FLOOR + 50.0);

        let after = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);
        assert!(
            counted,
            "an above-threshold wait must report that it counted"
        );
        assert_eq!(
            after - before,
            1,
            "an above-threshold wait must increment the counter exactly once",
        );
        // The public accessor (the AQ consumer's entry point) must observe the
        // same value, proving the seam writes the counter the consumer reads.
        assert_eq!(
            unistream_ready_stall_count(),
            after,
            "public accessor must reflect the recorded stall",
        );
    }

    #[test]
    fn record_ready_stall_leaves_counter_untouched_below_threshold() {
        let _guard = STALL_COUNTER_GUARD.lock().unwrap();
        let _tguard = THRESHOLD_GUARD.lock().unwrap();
        reset_threshold_to_floor();
        let before = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);

        let counted = record_ready_stall(READY_STALL_THRESHOLD_MS_FLOOR - 50.0);

        let after = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);
        assert!(
            !counted,
            "a below-threshold wait must report that it did not count",
        );
        assert_eq!(
            after, before,
            "a below-threshold wait must NOT touch the counter",
        );
    }

    #[test]
    fn record_ready_stall_does_not_count_exactly_at_threshold() {
        // The increment must obey the same strict `>` boundary as the
        // predicate: a wait of exactly the threshold leaves the counter flat.
        let _guard = STALL_COUNTER_GUARD.lock().unwrap();
        let _tguard = THRESHOLD_GUARD.lock().unwrap();
        reset_threshold_to_floor();
        let before = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);

        let counted = record_ready_stall(READY_STALL_THRESHOLD_MS_FLOOR);

        let after = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);
        assert!(!counted, "exactly-at-threshold must not count");
        assert_eq!(after, before, "exactly-at-threshold must not increment");
    }

    #[test]
    fn record_ready_stall_accumulates_across_repeated_stalls() {
        // The consumer's window test reads "N events in the window," so K
        // distinct above-threshold waits must produce exactly K increments —
        // the property the inline-vs-extracted refactor must preserve.
        let _guard = STALL_COUNTER_GUARD.lock().unwrap();
        let _tguard = THRESHOLD_GUARD.lock().unwrap();
        reset_threshold_to_floor();
        let before = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);

        const K: u64 = 5;
        for _ in 0..K {
            assert!(record_ready_stall(READY_STALL_THRESHOLD_MS_FLOOR + 10.0));
        }

        let after = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after - before,
            K,
            "K above-threshold stalls must produce exactly K increments",
        );
    }

    // --- Dynamic threshold (frame-rate-aware, issue #1618) ---------------

    #[test]
    fn set_threshold_raises_above_floor() {
        let _guard = THRESHOLD_GUARD.lock().unwrap();
        // Simulate dual-stream: camera 30fps + screen 10fps.
        // Longest frame interval = 100ms (screen). 8 × 100 = 800ms.
        set_ready_stall_threshold_ms(800.0);
        assert_eq!(
            ready_stall_threshold_ms(),
            800.0,
            "threshold must be raised to the requested value",
        );
        // A 400ms wait that would be a stall at floor (250) is NOT a stall
        // at the raised threshold (800).
        assert!(
            !is_ready_stall(400.0),
            "400ms must NOT stall when threshold is raised to 800ms",
        );
        // A 900ms wait exceeds even the raised threshold.
        assert!(
            is_ready_stall(900.0),
            "900ms must stall even when threshold is raised to 800ms",
        );
        // Clean up for other tests.
        reset_threshold_to_floor();
    }

    #[test]
    fn set_threshold_clamps_below_floor() {
        let _guard = THRESHOLD_GUARD.lock().unwrap();
        // Attempting to set below floor must clamp to floor.
        set_ready_stall_threshold_ms(100.0);
        assert_eq!(
            ready_stall_threshold_ms(),
            READY_STALL_THRESHOLD_MS_FLOOR,
            "threshold must never go below the floor",
        );
        // Still behaves as 250ms boundary.
        assert!(!is_ready_stall(250.0));
        assert!(is_ready_stall(251.0));
        reset_threshold_to_floor();
    }

    #[test]
    fn set_threshold_reset_to_floor_restores_default() {
        let _guard = THRESHOLD_GUARD.lock().unwrap();
        set_ready_stall_threshold_ms(600.0);
        assert_eq!(ready_stall_threshold_ms(), 600.0);
        // Reset.
        set_ready_stall_threshold_ms(READY_STALL_THRESHOLD_MS_FLOOR);
        assert_eq!(ready_stall_threshold_ms(), READY_STALL_THRESHOLD_MS_FLOOR);
        // Boundary restored.
        assert!(is_ready_stall(251.0));
        assert!(!is_ready_stall(250.0));
    }

    #[test]
    fn raised_threshold_prevents_counter_increment() {
        let _guard = STALL_COUNTER_GUARD.lock().unwrap();
        let _tguard = THRESHOLD_GUARD.lock().unwrap();
        // Raise threshold to 800ms (dual-stream scenario).
        set_ready_stall_threshold_ms(800.0);
        let before = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);

        // A 400ms wait: above floor (250) but below raised threshold (800).
        let counted = record_ready_stall(400.0);

        let after = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);
        assert!(!counted, "a wait below the raised threshold must NOT count",);
        assert_eq!(
            after, before,
            "counter must not increment below the raised threshold",
        );
        reset_threshold_to_floor();
    }

    #[test]
    fn raised_threshold_still_counts_genuine_cliff() {
        let _guard = STALL_COUNTER_GUARD.lock().unwrap();
        let _tguard = THRESHOLD_GUARD.lock().unwrap();
        // Raise threshold to 800ms.
        set_ready_stall_threshold_ms(800.0);
        let before = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);

        // A 1500ms wait: a genuine bandwidth cliff exceeds even the raised threshold.
        let counted = record_ready_stall(1500.0);

        let after = UNISTREAM_READY_STALL_COUNT.load(Ordering::Relaxed);
        assert!(
            counted,
            "a wait above the raised threshold must count (genuine cliff)",
        );
        assert_eq!(
            after - before,
            1,
            "genuine cliff must still increment the counter",
        );
        reset_threshold_to_floor();
    }

    // ─────────────────────────────────────────────────────────────────────
    // WebTransport unistream-DROP signal (#1104 / #509 parity audit).
    //
    // These pin the INCREMENT side of the drop signal — the WT analogue of
    // the WS send-buffer drop that the encoder AQ self-shed (#1104) consumes
    // via `videocall_aq::evaluate_self_congestion`. Before the
    // `record_unistream_drop` extraction the drop counter's only write path
    // was inline in the wasm-only `send_on_persistent_stream` `spawn_local`,
    // so NO native test could fail if that write was deleted or pointed at the
    // wrong counter. The seam closes that regression-coverage gap on the host
    // target (deliberately NOT `#[wasm_bindgen_test]`, which is known to
    // silently no-op on this dev box — a false green).
    //
    // The DECISION side (window/threshold over this counter) is pinned by the
    // `videocall-aq` `evaluate_self_congestion` tests parameterised with
    // `WT_SELF_CONGESTION_*`, and the two halves are tied together end-to-end
    // by the `videocall-client` `wt_backpressure_wiring` integration test.
    // ─────────────────────────────────────────────────────────────────────

    /// Serialises the tests that mutate the process-global
    /// `UNISTREAM_DROP_COUNT` so their before/after deltas are not corrupted by
    /// parallel test execution. Separate from `STALL_COUNTER_GUARD`: the two
    /// counters are independent, so the drop tests and stall tests never need
    /// to exclude each other, only their own siblings.
    static DROP_COUNTER_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn record_unistream_drop_increments_counter_once() {
        let _guard = DROP_COUNTER_GUARD.lock().unwrap();
        let before = UNISTREAM_DROP_COUNT.load(Ordering::Relaxed);

        record_unistream_drop();

        let after = UNISTREAM_DROP_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after - before,
            1,
            "one recorded drop must increment the counter exactly once",
        );
        // The public accessor (the AQ consumer's entry point) must observe the
        // same value, proving the seam writes the counter the consumer reads.
        // A mutation that incremented a DIFFERENT counter (e.g. the stall
        // counter) would leave this accessor flat and fail here.
        assert_eq!(
            unistream_drop_count(),
            after,
            "public accessor must reflect the recorded drop",
        );
    }

    #[test]
    fn record_unistream_drop_accumulates_across_repeated_drops() {
        // The consumer's window test reads "N drops in the window," so K
        // recorded drops must produce exactly K increments — the property the
        // inline-vs-extracted refactor must preserve so a sustained cluster of
        // failed media-frame writes trips the self-shed.
        let _guard = DROP_COUNTER_GUARD.lock().unwrap();
        let before = UNISTREAM_DROP_COUNT.load(Ordering::Relaxed);

        const K: u64 = 7;
        for _ in 0..K {
            record_unistream_drop();
        }

        let after = UNISTREAM_DROP_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after - before,
            K,
            "K recorded drops must produce exactly K increments",
        );
        assert_eq!(
            unistream_drop_count(),
            after,
            "public accessor must reflect all recorded drops",
        );
    }
}
