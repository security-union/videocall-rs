use crate::adaptive_quality_constants::{
    AUDIO_RED_FORMAT, AUDIO_RED_SEQ_HISTORY_SIZE, OPUS_FRAME_DURATION_MS,
};
use crate::audio::shared_audio_context::SharedAudioContext;
use crate::audio_constants::{
    rms_to_intensity, AUDIO_LEVEL_DELTA_THRESHOLD, DEFAULT_VAD_THRESHOLD,
};
use crate::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::decode::{AudioPeerDecoderTrait, DecodeStatus};
use js_sys::Float32Array;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_wasm_bindgen;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent, Metric, MetricValue};
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{AudioContext, AudioWorkletNode, MessageEvent, Worker};

const WORKLET_CODE: &str = include_str!("../scripts/pcmPlayerWorker.js");

thread_local! {
    /// Scratch buffer for [`NetEqAudioPeerDecoder::calculate_rms`] (issue 2225).
    ///
    /// The RMS used to read the worker's `Float32Array` one sample at a time
    /// (`get_index`), i.e. one wasm↔JS boundary crossing per sample. `copy_to`
    /// moves the whole frame in one bulk memcpy instead, and this buffer is what
    /// keeps that copy from allocating on every frame.
    ///
    /// SIZE — note this is NOT the Opus packet frame. What arrives here is
    /// NetEQ's **playout** frame: `neteq/src/neteq.rs` sets
    /// `output_frame_size_samples = sample_rate / 100 * channels`, so at 48 kHz
    /// mono that is 480 f32 (1.9 KiB), posted at 100 Hz — ~48 000 crossings/s
    /// for a single unmuted peer, ~960 k/s across 20. [`SAMPLES_PER_AUDIO_FRAME`]
    /// (960) is a different quantity: the 20 ms Opus *packet*, used only to
    /// derive RTP timestamps on the way IN. Conflating the two is what produced
    /// the wrong figure that used to sit in this comment.
    ///
    /// The buffer grows to the largest frame ever seen and stays there, so the
    /// high-water mark is ~1.9 KiB. One buffer serves every peer's decoder
    /// because the browser main thread (where the worker's `onmessage` closure
    /// runs) is single-threaded, and the borrow is released before
    /// `calculate_rms` returns, so no two frames can hold it at once.
    static RMS_SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

/// Root-mean-square of one decoded PCM frame, in pure wasm.
///
/// Split out of [`NetEqAudioPeerDecoder::calculate_rms`] (issue 2225) so the
/// arithmetic is reachable from the host test target, where `js_sys` types
/// cannot be constructed. `calculate_rms` is now just "bulk-copy the frame, then
/// call this".
///
/// The accumulation is deliberately a plain left-to-right `f32` loop, matching
/// the pre-2225 per-sample implementation operation for operation, so the result
/// is bit-identical. `f32` addition is neither associative nor width-agnostic:
/// reversing the order, or widening the accumulator to `f64`, changes the
/// returned bits. `bulk_copy_rms_is_bit_identical_to_the_per_sample_oracle`
/// pins that against the superseded implementation, and both of those mutations
/// were run and do fail it.
fn rms_of_samples(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let mut sum_squares: f32 = 0.0;
    for &sample in samples {
        sum_squares += sample * sample;
    }

    (sum_squares / samples.len() as f32).sqrt()
}

/// Number of audio samples in one Opus frame at the negotiated sample rate.
/// 48000 Hz / 1000 ms * 20 ms = 960 samples per 20 ms frame. NetEQ's
/// delay manager treats the packet `timestamp` field as a sample counter, so
/// consecutive frames must advance by exactly this many samples.
const SAMPLES_PER_AUDIO_FRAME: u32 = AUDIO_SAMPLE_RATE / 1000 * OPUS_FRAME_DURATION_MS;

/// Derive a NetEQ sample-domain RTP timestamp from the monotonic packet
/// sequence number. Using the sequence (not the wall-clock `packet.timestamp`)
/// makes the timestamp immune to the browser-ms vs CLI-micros encoder
/// divergence: each sequence step is exactly one Opus frame = +960 samples.
/// Wraps in the u32 domain like a real RTP timestamp.
fn seq_to_sample_timestamp(seq: u64) -> u32 {
    (seq as u32).wrapping_mul(SAMPLES_PER_AUDIO_FRAME)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
enum WorkerMsg {
    Init {
        sample_rate: u32,
        channels: u8,
    },
    Insert {
        seq: u16,
        timestamp: u32,
        #[serde(with = "serde_bytes")]
        payload: Vec<u8>,
    },
    Flush,
    Clear,
    Close,
    Mute {
        muted: bool,
    },
    SetDiagnostics {
        enabled: bool,
    },
}

/// Messages received from worker (matches neteq_worker.rs)
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum WorkerResponse {
    WorkerReady {
        mute_state: bool,
    },
    Stats {
        #[serde(skip)]
        stats: JsValue, // Will be processed manually
    },
}

/// Decoder-side voice-activity state for one peer.
///
/// Lives in a single `Rc<RefCell<..>>` shared between the decoder and the NetEQ
/// worker's `onmessage` closure. The three fields are deliberately kept in one
/// cell rather than side-by-side `Rc`s: [`Self::observe`] consults `suppressed`
/// and updates `speaking`/`audio_level` under a single borrow, so there is no
/// interleaving in which a PCM frame is judged against a gate that no longer
/// applies.
#[derive(Debug)]
struct VadState {
    /// Whether this peer is currently reported to the UI as speaking.
    speaking: bool,
    /// Last reported audio level, normalised to 0.0–1.0.
    audio_level: f32,
    /// True once the NetEQ worker has been told to stop producing PCM
    /// (`set_muted(true)`), has been terminated (`Drop`), or has been retired
    /// in favour of a replacement decoder.
    ///
    /// The invariant is: `suppressed == true` **iff** we have already
    /// instructed the worker to go quiet. Any PCM that arrives in that state
    /// was produced *before* the instruction — it is stale by construction and
    /// must not move the VAD. See [`Self::observe`] and issue 2174.
    ///
    /// The gate therefore cannot wedge the indicator off on a live stream: the
    /// single `set_muted` call that closes it is the same one that tells the
    /// worker to stop producing (`should_produce` is false on the muted branch
    /// of `neteq/src/bin/neteq_worker.rs`), and the matching `set_muted(false)`
    /// reopens it. A state in which the gate is shut but PCM keeps flowing
    /// would mean the worker ignored its own mute — the peer would be audible
    /// while shown as muted, a louder failure than a dark glow.
    suppressed: bool,
}

impl VadState {
    fn new() -> Self {
        Self {
            speaking: false,
            audio_level: 0.0,
            suppressed: false,
        }
    }

    /// Fold one decoded PCM frame into the VAD, and report whether the caller
    /// must broadcast a `peer_speaking` event for it.
    ///
    /// Returns `false` — leaving the state untouched — while `suppressed`.
    /// That is what closes the mute race (issue 2174 follow-up): `set_muted`
    /// runs synchronously, but the worker may already have posted PCM that the
    /// browser has not dispatched yet. Without this gate that frame lands
    /// *after* the terminal `speaking: 0`, sees `speaking == false` against
    /// real speech, and broadcasts a fresh `speaking: 1` that nothing will ever
    /// close — because the worker is muted by then and produces no further PCM.
    /// Since the gate is read at message-dispatch time (not at post time), a
    /// frame posted arbitrarily earlier is still caught.
    ///
    /// Otherwise this is the original edge-trigger: emit when the speaking
    /// boolean toggles OR the level moves by more than
    /// `AUDIO_LEVEL_DELTA_THRESHOLD`, which keeps the event rate reasonable
    /// while giving the UI smooth level updates.
    fn observe(&mut self, is_speaking: bool, intensity: f32) -> bool {
        if self.suppressed {
            return false;
        }

        let level_changed = (intensity - self.audio_level).abs() > AUDIO_LEVEL_DELTA_THRESHOLD;
        if is_speaking == self.speaking && !level_changed {
            return false;
        }

        self.speaking = is_speaking;
        self.audio_level = intensity;
        true
    }

    /// Clear the VAD and report whether a terminal `peer_speaking` event still
    /// has to be broadcast for this peer.
    ///
    /// Returns `true` at most once per speaking episode: the first call made
    /// while the peer is recorded as speaking (or still carrying a non-zero
    /// level) clears the state and asks for the zero event; every later call
    /// finds the state already idle and returns `false`.
    ///
    /// That idempotence is what makes the teardown paths in
    /// `peer_decode_manager.rs` emit **exactly one** zero event even though
    /// they call `set_muted(true)` and `flush()` back to back.
    ///
    /// Clearing the state (rather than latching a "done" flag) is also what
    /// lets a later unmute emit cleanly: [`Self::observe`] only reports a
    /// change, so it needs to see `false` here to treat the next speech frame
    /// as a fresh `false -> true` transition.
    fn take_terminal(&mut self) -> bool {
        if !self.speaking && self.audio_level <= 0.0 {
            return false;
        }

        self.speaking = false;
        self.audio_level = 0.0;
        true
    }

    /// As [`Self::take_terminal`], but also closes the VAD to PCM the worker
    /// has already posted.
    ///
    /// Used by the paths that stop PCM for good — `set_muted(true)` and `Drop`
    /// — and deliberately **not** by `flush()`: a flush only discards the
    /// worker's buffered packets, the worker keeps producing, and the live VAD
    /// must stay open to close the episode on its own.
    fn suppress_and_take_terminal(&mut self) -> bool {
        self.suppressed = true;
        self.take_terminal()
    }

    /// Reopen the VAD after an unmute. The next speech frame is then a fresh
    /// `false -> true` transition and emits on its own; the unmute itself needs
    /// no event.
    fn reopen(&mut self) {
        self.suppressed = false;
    }

    /// Retire this VAD silently: suppress it and drop any in-flight speaking
    /// episode on the floor **without** broadcasting a terminal zero.
    ///
    /// Used when a fresh decoder is taking over the same peer
    /// (`Peer::reset_for_decode_error` after an AUDIO decode error). Clearing
    /// the state here is what makes the
    /// subsequent `Drop` a no-op: [`Self::take_terminal`] finds the VAD already
    /// idle and stays silent, so the UI never sees the peer stop speaking
    /// mid-word. Suppressing is what stops the outgoing worker's already-posted
    /// PCM from broadcasting for a peer the new decoder now owns.
    fn retire(&mut self) {
        self.suppressed = true;
        self.speaking = false;
        self.audio_level = 0.0;
    }
}

/// Audio decoder that sends packets to a NetEq worker and plays the returned PCM via WebAudio.
#[derive(Debug)]
pub struct NetEqAudioPeerDecoder {
    worker: Worker,
    _audio_context: AudioContext,
    decoded: bool,
    /// Which peer this decoder belongs to.
    ///
    /// `Rc<str>` rather than `String` (issue 2225) because this field and the
    /// worker's `onmessage` closure each need an independent owned handle, and
    /// sharing one allocation beats cloning the `String` twice at construction.
    ///
    /// It is NOT what removed the per-frame allocation — that was
    /// [`Self::handle_pcm_data`] taking `&str` instead of an owned `String`, so
    /// the closure now passes a borrow per PCM message rather than cloning. The
    /// one `String` the diagnostics event needs is built only on the
    /// (edge-triggered, rare) frames that actually broadcast.
    peer_id: Rc<str>,
    _pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>, // AudioWorklet PCM player

    // Message queueing system
    pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
    worker_ready: Rc<RefCell<bool>>,

    /// Voice activity detection state, shared with the worker's `onmessage`
    /// closure (see `_on_message`).
    vad: Rc<RefCell<VadState>>,

    /// The worker's `onmessage` handler.
    ///
    /// Issue 2225: this used to be `.forget()`-ed, leaking the closure, every
    /// `Rc` it captures (`vad`, `pcm_player`, `worker_ready`,
    /// `pending_messages`), and the `Worker`'s own JS wrapper — which the
    /// closure captures, so worker → `onmessage` → closure → worker was a JS
    /// reference cycle no GC could collect. Holding it here frees all of that
    /// when the decoder is dropped.
    ///
    /// Correct only because [`Drop`] detaches it (`set_onmessage(None)`)
    /// *before* this field is freed: field drops run after the `Drop::drop`
    /// body, so by the time wasm-bindgen frees the closure the browser no longer
    /// holds a reference that could invoke it. Freeing a still-installed closure
    /// would make a late worker message throw a JS `Error` ("closure invoked
    /// recursively or after being dropped"). That is a contained exception in an
    /// event handler, not memory unsafety and not a page abort — wasm-bindgen
    /// replaces the freed entry with a throwing stub precisely so nothing can
    /// reach the freed box — but it is still an avoidable error, so we avoid
    /// it.
    ///
    /// The same requirement applies at the other end of the lifetime: see the
    /// ORDERING INVARIANT on the `set_onmessage` call in [`Self::build`], which
    /// keeps the install below every fallible `?` so no early return can drop
    /// the closure while the worker still points at it.
    _on_message: Closure<dyn FnMut(MessageEvent)>,

    /// The one-shot `setTimeout` callback that posts `Init` to the worker.
    ///
    /// Issue 2225: also `.forget()`-ed before. Held here and cancelled by
    /// [`Drop`] via `_init_timer_id`, for the same reason as `_on_message` — a
    /// timer that fires after the closure has been freed would panic.
    _init_timer_cb: Closure<dyn FnMut()>,

    /// Handle of the pending `Init` timer, or `0` once it has fired.
    ///
    /// The callback zeroes it as its first action, so [`Drop`] only calls
    /// `clear_timeout_with_handle` while the timer is genuinely outstanding.
    /// That matters because the browser may reuse a completed timer's handle
    /// id: clearing a stale id could cancel an unrelated timer.
    _init_timer_id: Rc<std::cell::Cell<i32>>,

    /// Ring buffer of recently received audio sequence numbers.
    /// Used to detect whether a redundant frame carried in a RED packet
    /// was already received, avoiding duplicate injection.
    received_sequences: VecDeque<u64>,
}

impl NetEqAudioPeerDecoder {
    /// Send message through queue (immediate if worker ready, otherwise queued)
    fn send_worker_message(&self, msg: WorkerMsg) {
        let is_ready = *self.worker_ready.borrow();

        if is_ready {
            // Worker ready - send immediately
            self.send_message_immediate(msg);
        } else {
            // Worker not ready - queue the message
            log::debug!(
                "🔄 Queueing message for peer {} (worker not ready)",
                self.peer_id
            );
            self.pending_messages.borrow_mut().push_back(msg);
        }
    }

    /// Send message immediately to worker
    fn send_message_immediate(&self, msg: WorkerMsg) {
        if let Err(e) =
            serde_wasm_bindgen::to_value(&msg).map(|js_msg| self.worker.post_message(&js_msg))
        {
            log::error!("Failed to send worker message: {e:?}");
            web_sys::console::error_1(&format!("Failed to send worker message: {e:?}").into());
        }
    }

    /// Create a NetEq worker.
    fn create_neteq_worker() -> Result<Worker, JsValue> {
        let window = web_sys::window().expect("no window");
        let document = window.document().expect("no document");
        let worker_url = document
            .get_element_by_id("neteq-worker")
            .expect("neteq-worker link tag not found")
            .get_attribute("href")
            .expect("link tag has no href");
        Worker::new(&worker_url)
    }

    /// Send PCM data to Safari AudioWorklet (simple and efficient)
    fn send_pcm_to_safari_worklet(pcm_player: &AudioWorkletNode, pcm: &Float32Array) {
        // Create message object for the worklet
        let message = js_sys::Object::new();
        js_sys::Reflect::set(&message, &"command".into(), &"play".into()).unwrap();
        js_sys::Reflect::set(&message, &"pcm".into(), pcm).unwrap();

        // Send PCM data to the worklet - it handles all timing internally
        if let Err(e) = pcm_player.port().unwrap().post_message(&message) {
            web_sys::console::warn_1(
                &format!("Safari: Failed to send PCM to worklet: {e:?}").into(),
            );
        }
    }

    /// Create Safari-optimized AudioContext with PCM player worklet
    async fn create_safari_audio_context(
        speaker_device_id: Option<Rc<str>>,
    ) -> Result<(AudioContext, AudioWorkletNode), JsValue> {
        // Use shared context and ensure worklet is registered before creating node
        // `SharedAudioContext` owns its device id, so the `Rc<str>` the PCM path
        // carries is materialised into a `String` here — on first-init only, not
        // per frame (see `ensure_worklet_initialized`'s early return).
        let audio_context =
            SharedAudioContext::get_or_init(speaker_device_id.as_deref().map(str::to_owned))?;
        SharedAudioContext::ensure_pcm_worklet_ready(WORKLET_CODE).await?;

        // Create per-peer nodes after registration completes
        let (pcm_player, _peer_gain) = SharedAudioContext::create_peer_playback_nodes("safari")?;

        Ok((audio_context, pcm_player))
    }

    /// Calculate RMS (Root Mean Square) of audio samples for voice activity detection.
    ///
    /// This is part of the **decoder-side (remote peer) VAD**.  We run a
    /// fast-path RMS check on every decoded PCM frame so the UI can show a
    /// speaking indicator for remote peers with sub-second latency, rather
    /// than waiting for the heartbeat update that carries the remote user's
    /// own (encoder-side) `is_speaking` flag — that heartbeat is edge-triggered
    /// on state changes over a 5s keepalive floor
    /// (`HEARTBEAT_KEEPALIVE_INTERVAL_MS`), not a 1 Hz tick.
    /// Issue 2225: copy the frame across the wasm↔JS boundary ONCE (a bulk
    /// `copy_to` memcpy) and do the arithmetic in pure wasm, instead of one
    /// `get_index` crossing per sample. See [`RMS_SCRATCH`] for the cost this
    /// removes.
    ///
    /// The arithmetic itself is unchanged — [`rms_of_samples`] performs the same
    /// left-to-right `f32` accumulation over the same values in the same order —
    /// so the result is bit-identical and the VAD threshold comparisons
    /// downstream see exactly what they saw before.
    fn calculate_rms(pcm: &Float32Array) -> f32 {
        let length = pcm.length() as usize;
        if length == 0 {
            return 0.0;
        }

        RMS_SCRATCH.with(|scratch| {
            let mut buf = scratch.borrow_mut();
            if buf.len() < length {
                buf.resize(length, 0.0);
            }
            // `Float32Array::copy_to` PANICS unless the destination slice length
            // matches the array exactly, so the (possibly larger) scratch buffer
            // must be truncated to this frame. The copy overwrites every one of
            // those elements, so a shorter frame can never read samples left
            // behind by a longer one.
            let samples = &mut buf[..length];
            pcm.copy_to(samples);
            rms_of_samples(samples)
        })
    }

    /// Handle PCM audio data from NetEq worker.
    ///
    /// Includes decoder-side VAD: computes RMS on the decoded PCM and emits
    /// a `peer_speaking` diagnostics event when the speaking state changes.
    /// This gives the UI a faster speaking indicator for remote peers than the
    /// heartbeat, which only reflects the remote user's own encoder-side VAD
    /// result and arrives on state changes over a 5s keepalive floor.
    ///
    /// PCM that arrives while the VAD is suppressed (the peer has been muted,
    /// or the decoder has been torn down / retired) is still played out — it is
    /// audio the listener was already part-way through hearing — but it is not
    /// allowed to move the speaking indicator. See [`VadState::observe`].
    ///
    /// Issue 2225: neither `peer_id` nor `speaker_device_id` is cloned into an
    /// owned `String` any more. The worker calls this ~100 times a second per
    /// unmuted peer, and neither value is needed as a `String` on the common
    /// path — the diagnostics event allocates one only when the VAD actually
    /// emits, and the speaker id only on the first frame, when the worklet is
    /// built.
    ///
    /// `peer_id` is a plain `&str` (the caller's `Rc<str>` deref-coerces): this
    /// function only formats and `to_string()`s it, so requiring an `Rc` would
    /// constrain callers for nothing. `speaker_device_id` stays `Option<Rc<str>>`
    /// because it is *moved* into the `spawn_local` future below, which a
    /// borrow cannot be.
    fn handle_pcm_data(
        pcm: Float32Array,
        pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>,
        audio_context: &AudioContext,
        speaker_device_id: Option<Rc<str>>,
        peer_id: &str,
        vad: Rc<RefCell<VadState>>,
        vad_threshold: f32,
    ) {
        // Calculate RMS for voice activity detection
        let rms = Self::calculate_rms(&pcm);
        let is_speaking = rms > vad_threshold;

        // Normalize RMS to a 0.0–1.0 intensity range using the shared
        // perceptual curve (sqrt for human hearing).
        let intensity = rms_to_intensity(rms, vad_threshold);

        // Borrow ends with the statement, so the broadcast below never runs
        // while the shared VAD state is borrowed.
        let changed = vad.borrow_mut().observe(is_speaking, intensity);

        if changed {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "peer_speaking",
                stream_id: Some(format!("speaking->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("to_peer", peer_id.to_string()),
                    metric!("speaking", if is_speaking { 1u64 } else { 0u64 }),
                    metric!("audio_level", intensity as f64),
                ],
            });
        }

        // Ensure AudioContext is running
        if let Err(e) = audio_context.resume() {
            web_sys::console::warn_1(
                &format!("[neteq-audio-decoder] AudioContext resume error: {e:?}").into(),
            );
        }

        let pcm_player_clone = pcm_player.clone();
        wasm_bindgen_futures::spawn_local(async move {
            Self::ensure_worklet_initialized(&pcm_player_clone, speaker_device_id).await;

            if let Some(ref worklet) = *pcm_player_clone.borrow() {
                Self::send_pcm_to_safari_worklet(worklet, &pcm);
            }
        });
    }

    /// Build the terminal `speaking: 0` event for `peer_id`.
    ///
    /// Deliberately the same subsystem / `stream_id` / metric shape that
    /// `handle_pcm_data` emits, so every existing consumer (the tile glow, the
    /// peer list, the health reporter) handles it with no changes — it simply
    /// reads as "this peer went quiet".
    fn terminal_speaking_event(peer_id: &str, ts_ms: u64) -> DiagEvent {
        DiagEvent {
            subsystem: "peer_speaking",
            stream_id: Some(format!("speaking->{peer_id}")),
            ts_ms,
            metrics: vec![
                metric!("to_peer", peer_id.to_string()),
                metric!("speaking", 0u64),
                metric!("audio_level", 0.0f64),
            ],
        }
    }

    /// Broadcast the terminal `speaking: 0` for `peer_id` when `owed`.
    ///
    /// # Why (issue 2174)
    ///
    /// The decoder-side VAD in [`Self::handle_pcm_data`] is edge-triggered: it
    /// only emits when the speaking flag flips or the level moves. It is driven
    /// purely by decoded PCM arriving from the NetEQ worker — and the worker
    /// stops producing PCM entirely while muted (`should_produce` is false on
    /// the muted branch of `neteq/src/bin/neteq_worker.rs`). So when a peer
    /// mutes mid-word, the last event ever emitted is `speaking: 1` and the
    /// glow stays lit. The same holds for `flush()` (buffer dropped) and for
    /// `Drop` (worker terminated).
    ///
    /// This closes the fast path with an explicit terminal edge. Without it the
    /// only remaining off-switch is the peer's own heartbeat `is_speaking`
    /// flag, which `PeerDecodeManager` applies on receipt — correct, but up to
    /// `HEARTBEAT_KEEPALIVE_INTERVAL_MS` (5s) late when the mute itself is what
    /// silenced the stream.
    fn broadcast_terminal_zero(peer_id: &str, owed: bool) {
        if owed {
            let _ = global_sender().try_broadcast(Self::terminal_speaking_event(peer_id, now_ms()));
        }
    }

    /// Close out an in-flight speaking episode on the `flush()` path: emit the
    /// terminal zero if one is owed, but leave the VAD open — the worker keeps
    /// producing PCM after a flush.
    fn emit_terminal_speaking_zero(peer_id: &str, vad: &Rc<RefCell<VadState>>) {
        let owed = vad.borrow_mut().take_terminal();
        Self::broadcast_terminal_zero(peer_id, owed);
    }

    /// Close out an in-flight speaking episode on a path that stops PCM for
    /// good (`set_muted(true)`, `Drop`): suppress the VAD *first*, so PCM the
    /// worker has already posted cannot re-arm it after the zero goes out, then
    /// emit the terminal zero if one is owed.
    fn suppress_and_emit_terminal_speaking_zero(peer_id: &str, vad: &Rc<RefCell<VadState>>) {
        let owed = vad.borrow_mut().suppress_and_take_terminal();
        Self::broadcast_terminal_zero(peer_id, owed);
    }

    /// Instance-level wrapper around [`Self::emit_terminal_speaking_zero`],
    /// used only by the `flush()` path — the one close-out that must leave the
    /// VAD open.
    fn emit_speaking_off(&self) {
        Self::emit_terminal_speaking_zero(&self.peer_id, &self.vad);
    }

    /// The whole VAD half of [`AudioPeerDecoderTrait::set_muted`], in one
    /// place so it can be driven directly by tests.
    ///
    /// Muting closes the gate **before** broadcasting the terminal zero, in the
    /// same synchronous turn: PCM the worker posted moments ago may still be
    /// sitting in the browser's event queue, and JavaScript runs this call to
    /// completion before dispatching it, so that frame is guaranteed to find
    /// the gate already shut. Unmuting reopens it — the next speech frame is a
    /// fresh rising edge and emits on its own, so the unmute itself is silent.
    fn apply_mute_to_vad(peer_id: &str, vad: &Rc<RefCell<VadState>>, muted: bool) {
        if muted {
            Self::suppress_and_emit_terminal_speaking_zero(peer_id, vad);
        } else {
            vad.borrow_mut().reopen();
        }
    }

    /// Ensure AudioWorklet is initialized (lazy initialization)
    async fn ensure_worklet_initialized(
        pcm_player: &Rc<RefCell<Option<AudioWorkletNode>>>,
        speaker_device_id: Option<Rc<str>>,
    ) {
        if pcm_player.borrow().is_some() {
            return;
        }

        log::info!("Initializing AudioWorklet for PCM playback");

        match Self::create_safari_audio_context(speaker_device_id).await {
            Ok((_, worklet)) => {
                *pcm_player.borrow_mut() = Some(worklet);
                log::info!("AudioWorklet initialized successfully");
            }
            Err(e) => {
                web_sys::console::error_2(&"Failed to initialize worklet:".into(), &e);
            }
        }
    }

    /// Handle statistics messages from NetEq worker
    fn handle_stats_message(data: &JsValue, peer_id: &str) {
        let obj = match data.dyn_ref::<js_sys::Object>() {
            Some(obj) => obj,
            None => return,
        };

        let cmd =
            js_sys::Reflect::get(obj, &JsValue::from_str("cmd")).unwrap_or(JsValue::UNDEFINED);

        if cmd.as_string().as_deref() != Some("stats") {
            return;
        }

        let stats_js = match js_sys::Reflect::get(obj, &JsValue::from_str("stats")) {
            Ok(stats) => stats,
            Err(_) => return,
        };

        let stats_json = match js_sys::JSON::stringify(&stats_js) {
            Ok(json) => json,
            Err(_) => return,
        };

        let json_str = match stats_json.as_string() {
            Some(s) => s,
            None => return,
        };

        Self::emit_stats_diagnostics(&json_str, peer_id);
        Self::emit_parsed_metrics(&json_str, peer_id);
    }

    /// Emit raw stats JSON for debugging
    fn emit_stats_diagnostics(json_str: &str, peer_id: &str) {
        // peer_id here is the target peer (whose audio we're decoding)
        // We need to get the current user's ID for the reporting peer
        // For now, we'll use a placeholder and enhance this later
        let current_user = "current_user"; // TODO: Get from VideoCallClient

        let _ = global_sender().try_broadcast(DiagEvent {
            subsystem: "neteq",
            stream_id: Some(format!("{current_user}->{peer_id}")), // reporting_peer->target_peer
            ts_ms: now_ms(),
            metrics: vec![
                metric!("stats_json", json_str.to_string()),
                // `current_user` is a `&'static str`; borrow it (zero-alloc, #1421).
                Metric {
                    name: "reporting_peer",
                    value: MetricValue::text_static(current_user),
                },
                metric!("target_peer", peer_id.to_string()),
            ],
        });
    }

    /// Parse and emit specific metrics
    fn emit_parsed_metrics(json_str: &str, peer_id: &str) {
        let parsed: Value = match serde_json::from_str(json_str) {
            Ok(p) => p,
            Err(_) => return,
        };

        Self::emit_jitter_metrics(&parsed, peer_id);
        Self::emit_buffer_metrics(&parsed, peer_id);
    }

    /// Emit jitter buffer metrics
    fn emit_jitter_metrics(parsed: &Value, peer_id: &str) {
        let lifetime = match parsed.get("lifetime") {
            Some(l) => l,
            None => return,
        };

        if let Some(jitter) = lifetime
            .get("jitter_buffer_delay_ms")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(format!("current_user->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("jitter_buffer_delay_ms", jitter),
                    Metric {
                        name: "reporting_peer",
                        value: MetricValue::text_static("current_user"),
                    },
                    metric!("target_peer", peer_id.to_string()),
                ],
            });
        }

        if let Some(target) = lifetime
            .get("jitter_buffer_target_delay_ms")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(format!("current_user->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("jitter_buffer_target_delay_ms", target),
                    Metric {
                        name: "reporting_peer",
                        value: MetricValue::text_static("current_user"),
                    },
                    metric!("target_peer", peer_id.to_string()),
                ],
            });
        }
    }

    /// Emit buffer size metrics
    fn emit_buffer_metrics(parsed: &Value, peer_id: &str) {
        let network = match parsed.get("network") {
            Some(n) => n,
            None => return,
        };

        // Audio data buffered for playback
        if let Some(buf) = network
            .get("current_buffer_size_ms")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(format!("current_user->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("audio_buffer_ms", buf),
                    Metric {
                        name: "reporting_peer",
                        value: MetricValue::text_static("current_user"),
                    },
                    metric!("target_peer", peer_id.to_string()),
                ],
            });
        }

        // Encoded packets awaiting decode
        if let Some(packets) = network
            .get("packets_awaiting_decode")
            .and_then(|v| v.as_u64())
        {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(format!("current_user->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("packets_awaiting_decode", packets),
                    Metric {
                        name: "reporting_peer",
                        value: MetricValue::text_static("current_user"),
                    },
                    metric!("target_peer", peer_id.to_string()),
                ],
            });
        }

        // Expand rate: ratio of concealed vs real audio (Q14 format).
        // Broadcast as a parsed metric so consumers can match directly
        // without re-parsing the full stats JSON.
        if let Some(er) = network.get("expand_rate").and_then(|v| v.as_f64()) {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "neteq",
                stream_id: Some(format!("current_user->{peer_id}")),
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("expand_rate", er),
                    Metric {
                        name: "reporting_peer",
                        value: MetricValue::text_static("current_user"),
                    },
                    metric!("target_peer", peer_id.to_string()),
                ],
            });
        }
    }

    /// Create message handler for NetEq worker
    #[allow(clippy::too_many_arguments)]
    fn create_message_handler(
        pcm_player: Rc<RefCell<Option<AudioWorkletNode>>>,
        audio_context: AudioContext,
        peer_id: Rc<str>,
        speaker_device_id: Option<Rc<str>>,
        worker_ready: Rc<RefCell<bool>>,
        pending_messages: Rc<RefCell<VecDeque<WorkerMsg>>>,
        worker: Worker,
        vad: Rc<RefCell<VadState>>,
        vad_threshold: f32,
    ) -> Closure<dyn FnMut(MessageEvent)> {
        Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();

            if data.is_instance_of::<Float32Array>() {
                // High-performance PCM path with voice activity detection
                let pcm = Float32Array::from(data);
                Self::handle_pcm_data(
                    pcm,
                    pcm_player.clone(),
                    &audio_context,
                    speaker_device_id.clone(),
                    &peer_id,
                    vad.clone(),
                    vad_threshold,
                );
            } else if data.is_object() {
                // Try to parse as WorkerResponse first
                if let Ok(response) = serde_wasm_bindgen::from_value::<WorkerResponse>(data.clone())
                {
                    match response {
                        WorkerResponse::WorkerReady { mute_state } => {
                            // Handle worker ready - flush queue
                            log::info!(
                                "✅ Worker ready for peer {peer_id} (worker mute: {mute_state})"
                            );

                            *worker_ready.borrow_mut() = true;

                            // Flush queued messages in FIFO order
                            let mut queue = pending_messages.borrow_mut();
                            let queue_length = queue.len();

                            if queue_length > 0 {
                                log::info!(
                                    "📤 Flushing {queue_length} queued messages for peer {peer_id}"
                                );

                                // Send each queued message immediately
                                while let Some(msg) = queue.pop_front() {
                                    if let Err(e) = serde_wasm_bindgen::to_value(&msg)
                                        .map(|js_msg| worker.post_message(&js_msg))
                                    {
                                        log::error!("Failed to send queued message: {e:?}");
                                    } else {
                                        log::debug!("📤 Sent queued message: {msg:?}");
                                    }
                                }
                            }
                        }
                        WorkerResponse::Stats { .. } => {
                            // Handle stats message (fallback to old method for now)
                            Self::handle_stats_message(&data, &peer_id);
                        }
                    }
                } else {
                    // Fallback to old stats message handling
                    Self::handle_stats_message(&data, &peer_id);
                }
            }
        }) as Box<dyn FnMut(_)>)
    }

    /// Create audio decoder that uses NetEq worker for buffering and timing
    pub fn new_with_muted_state(
        speaker_device_id: Option<String>,
        peer_id: String,
        vad_threshold: Option<f32>,
    ) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
        Self::new_with_mute_state(speaker_device_id, peer_id, true, vad_threshold)
        // Default to muted
    }

    /// Create audio decoder with explicit initial mute state
    pub fn new_with_mute_state(
        speaker_device_id: Option<String>,
        peer_id: String,
        initial_muted: bool,
        vad_threshold: Option<f32>,
    ) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
        Ok(Box::new(Self::build(
            speaker_device_id,
            peer_id,
            initial_muted,
            vad_threshold,
        )?))
    }

    /// Build the concrete decoder. [`Self::new_with_mute_state`] boxes this
    /// behind the trait; the browser tests call it directly so they can reach
    /// the shared VAD state and exercise `Drop`, which is otherwise
    /// unobservable from behind `Box<dyn AudioPeerDecoderTrait>`.
    fn build(
        speaker_device_id: Option<String>,
        peer_id: String,
        initial_muted: bool,
        vad_threshold: Option<f32>,
    ) -> Result<Self, JsValue> {
        // Create worker
        let worker = Self::create_neteq_worker()?;

        // Use shared AudioContext and ensure worklet registered once
        let audio_context = SharedAudioContext::get_or_init(speaker_device_id.clone())?;
        SharedAudioContext::ensure_pcm_worklet(WORKLET_CODE);

        // Issue 2225: both ids become `Rc` here, at construction, so the
        // closure's capture costs one refcount bump rather than a second heap
        // copy. `speaker_device_id` additionally NEEDS the `Rc` on the hot path:
        // `handle_pcm_data` moves it into a `spawn_local` future once per PCM
        // message, which as `Option<String>` was an allocation per frame.
        // `peer_id` is merely borrowed per frame (`&str`), so for it this is a
        // construction-time tidy-up, not the hot-path fix.
        let peer_id: Rc<str> = Rc::from(peer_id);
        let speaker_device_id: Option<Rc<str>> = speaker_device_id.map(Rc::from);

        let pcm_player_ref = Rc::new(RefCell::new(None::<AudioWorkletNode>));

        let threshold = vad_threshold.unwrap_or(DEFAULT_VAD_THRESHOLD);

        let worker_ready = Rc::new(RefCell::new(false));
        let pending_messages = Rc::new(RefCell::new(VecDeque::new()));
        let vad = Rc::new(RefCell::new(VadState::new()));

        // Set up worker message handling with decoder's queue references
        let on_message_closure = Self::create_message_handler(
            pcm_player_ref.clone(),
            audio_context.clone(),
            peer_id.clone(),
            speaker_device_id.clone(),
            worker_ready.clone(),
            pending_messages.clone(),
            worker.clone(),
            vad.clone(),
            threshold,
        );

        // Initialize worker
        let init_msg = WorkerMsg::Init {
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS as u8,
        };

        let init_js = serde_wasm_bindgen::to_value(&init_msg)?;
        let worker_clone = worker.clone();
        // Zeroed by the callback itself so `Drop` can tell "still pending" from
        // "already fired" — clearing an already-completed handle id risks
        // cancelling an unrelated timer that inherited it.
        let init_timer_id = Rc::new(std::cell::Cell::new(0i32));
        let init_timer_id_for_cb = init_timer_id.clone();
        let send_cb = Closure::wrap(Box::new(move || {
            init_timer_id_for_cb.set(0);
            if let Err(e) = worker_clone.post_message(&init_js) {
                web_sys::console::error_2(&"[neteq-audio-decoder] failed to post Init:".into(), &e);
            }
        }) as Box<dyn FnMut()>);

        // A `?` here leaves NO timer registered, so `send_cb` is dropped without
        // the browser ever having held a reference to it.
        let timer_id = web_sys::window()
            .expect("no window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                send_cb.as_ref().unchecked_ref(),
                10,
            )?;
        // `set_timeout` returns synchronously, before any callback can run, so
        // this store cannot race the callback's own zeroing.
        init_timer_id.set(timer_id);

        // Issue 2225: install the handler but KEEP the closure (stored on the
        // struct below) instead of `.forget()`-ing it. `Drop` detaches the
        // handler before the field is freed, so nothing can invoke a freed
        // closure, and every `Rc` the closure captures — plus the `Worker` JS
        // wrapper it holds, which closed a worker→closure→worker reference cycle
        // — is released with the decoder.
        //
        // ORDERING INVARIANT: this must stay BELOW every `?` in this function
        // and directly above the struct literal that takes ownership. An early
        // return between the install and that literal would drop the closure
        // while the worker still referenced it, arming exactly the
        // freed-closure panic this arrangement exists to avoid. Nothing is lost
        // by installing late: `build` is synchronous, so the browser cannot
        // dispatch a worker message until it returns.
        worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        // Create decoder with explicit mute state
        let mut decoder = Self {
            worker: worker.clone(),
            _audio_context: audio_context.clone(),
            decoded: false,
            peer_id: peer_id.clone(),
            _pcm_player: pcm_player_ref.clone(),

            // Message queueing system
            pending_messages,
            worker_ready,

            // Voice activity detection state
            vad,

            // Issue 2225: owned, not leaked — see the field docs.
            _on_message: on_message_closure,
            _init_timer_cb: send_cb,
            _init_timer_id: init_timer_id,

            // RED redundancy: track recently received sequence numbers
            received_sequences: VecDeque::with_capacity(AUDIO_RED_SEQ_HISTORY_SIZE),
        };

        log::info!("NetEq audio decoder initialized for peer {peer_id} (muted: {initial_muted})");

        // Set the initial mute state explicitly
        decoder.set_muted(initial_muted);
        log::info!(
            "✅ NetEq decoder initialized for peer {} with muted: {}",
            decoder.peer_id,
            initial_muted
        );

        // Enable diagnostics in the NetEQ worker
        decoder.send_worker_message(WorkerMsg::SetDiagnostics { enabled: true });
        log::info!(
            "🔧 Enabled diagnostics for NetEq worker for peer {}",
            decoder.peer_id
        );

        Ok(decoder)
    }

    /// Record a sequence number as received for RED deduplication.
    fn record_sequence(&mut self, seq: u64) {
        if self.received_sequences.len() >= AUDIO_RED_SEQ_HISTORY_SIZE {
            self.received_sequences.pop_front();
        }
        self.received_sequences.push_back(seq);
    }

    /// Check whether a sequence number was already received.
    fn has_sequence(&self, seq: u64) -> bool {
        self.received_sequences.contains(&seq)
    }

    /// Unpack a RED-encoded audio data buffer.
    ///
    /// Expected format:
    /// `[4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]`
    ///
    /// Returns `(primary_data, redundant_sequence, redundant_data)` or `None` if
    /// the buffer is too short or malformed.
    fn unpack_red_audio(data: &[u8]) -> Option<(Vec<u8>, u32, Vec<u8>)> {
        // Minimum: 4 (primary_len) + 0 (primary) + 4 (redundant_seq) + 0 (redundant)
        if data.len() < 8 {
            return None;
        }

        let primary_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;

        // Sanity check: an individual Opus audio frame should never exceed 10KB.
        // Reject clearly malformed or corrupt packets early.
        if primary_len > 10_000 {
            return None;
        }

        // Validate: primary_len + 4 (itself) + 4 (redundant_seq) must not exceed total
        let redundant_seq_offset = 4 + primary_len;
        if redundant_seq_offset + 4 > data.len() {
            return None;
        }

        let primary_data = data[4..4 + primary_len].to_vec();

        let redundant_seq = u32::from_le_bytes([
            data[redundant_seq_offset],
            data[redundant_seq_offset + 1],
            data[redundant_seq_offset + 2],
            data[redundant_seq_offset + 3],
        ]);

        let redundant_data = data[redundant_seq_offset + 4..].to_vec();

        Some((primary_data, redundant_seq, redundant_data))
    }

    /// Public wrapper around `unpack_red_audio` for cross-module tests.
    #[cfg(test)]
    pub fn unpack_red_audio_public(data: &[u8]) -> Option<(Vec<u8>, u32, Vec<u8>)> {
        Self::unpack_red_audio(data)
    }

    /// Build a `WorkerMsg::Insert` from a full-width u64 sequence number.
    ///
    /// The `seq` field in `WorkerMsg::Insert` is u16 by design: NetEQ's
    /// `RtpHeader.sequence_number` is a u16, and packet ordering/flush/reject
    /// decisions inside the NetEQ worker are driven solely by the sample-domain
    /// `timestamp` (derived here via `seq_to_sample_timestamp`), never by the
    /// sequence number itself.  Ordering is RTP wrap-aware (0x8000 half-window
    /// comparison in `neteq/src/packet.rs::is_sequence_newer`), so the
    /// truncation from u64 → u16 is wrap-safe: the u16 seq wraps at 65536
    /// frames (~21.8 min at 20 ms/frame) exactly as a real RTP sequence number
    /// would.  The truncation is intentional and must NOT be widened to u32/u64.
    ///
    /// Cross-references:
    ///  - `neteq/src/neteq.rs::test_seq_wrap_no_buffer_flush` — regression test
    ///    proving that a u16 seq wrap does not flush or reject packets.
    ///  - `tests::test_insert_msg_truncates_seq_to_u16_but_red_tracks_full_u64`
    ///    in this file — receiver-boundary test proving this seam truncates to
    ///    the expected u16 value while RED dedup tracks the full u64.
    fn build_insert_msg(seq: u64, payload: Vec<u8>) -> WorkerMsg {
        WorkerMsg::Insert {
            // DELIBERATE u64 → u16 truncation: wrap-safe by RTP design.
            // See doc-comment above for the full rationale.
            seq: seq as u16,
            timestamp: seq_to_sample_timestamp(seq),
            payload,
        }
    }
}

impl Drop for NetEqAudioPeerDecoder {
    fn drop(&mut self) {
        // Issue 2174: terminating the worker stops PCM for good, so this is the
        // last chance to clear a glow left lit by the edge-triggered VAD.
        //
        // Suppressing as part of the same call also covers PCM the worker posted
        // before this teardown: the `onmessage` closure holds the shared VAD
        // state, so a frame dispatched between here and the `set_onmessage(None)`
        // below would otherwise broadcast `speaking: 1` for a peer whose decoder
        // is gone.
        //
        // Drop fires on decoder REPLACEMENT as well as on genuine teardown
        // (`Peer::replace_audio_decoder` assigns over `self.audio`, which is
        // how `Peer::reset_for_decode_error` swaps this decoder out on an AUDIO
        // decode error). A replacement must NOT announce that the peer stopped
        // speaking — see [`Self::retire_for_replacement`], which clears the VAD
        // beforehand so this call finds it idle and stays silent.
        Self::suppress_and_emit_terminal_speaking_zero(&self.peer_id, &self.vad);

        // Issue 2225: detach the handler and cancel the pending `Init` timer
        // BEFORE their closures are freed. Struct fields (`_on_message`,
        // `_init_timer_cb`) are dropped after this body returns, so this is the
        // last point at which the browser can still hold a live reference to
        // either. Invoking a freed wasm-bindgen closure THROWS a JS `Error`
        // ("closure invoked recursively or after being dropped" — the
        // `throw_str` in wasm-bindgen's `convert/closures.rs`), i.e. an uncaught
        // exception in an event handler. It is NOT a Rust panic, NOT a
        // use-after-free, and does not abort the page; nothing reads freed
        // memory either way. Detaching first is what keeps owning the closures
        // (rather than leaking them via `.forget()`) free of even that
        // controlled throw.
        //
        // Detaching also means no late worker message is dispatched at all,
        // which is a strictly stronger guarantee than the pre-2225 arrangement
        // (where the leaked closure stayed installed and the `suppress` above
        // was what stopped an in-flight PCM frame from broadcasting for a
        // decoder that no longer exists). The suppression stays regardless: it
        // is also what `retire_for_replacement` relies on, and it still gates
        // any frame dispatched between the `suppress` and this line.
        self.worker.set_onmessage(None);
        if self._init_timer_id.get() != 0 {
            // A decoder cannot exist without a Window: `build` `.expect()`s one
            // to arm this very timer. So this lookup is unreachable-`None`, and
            // the skip below cannot silently strand a live timer.
            //
            // Deliberately NOT `.expect()`-ed to match `build`: a panic in
            // `Drop` while an unwind is already in flight aborts the process,
            // which is strictly worse than the unreachable branch it would be
            // guarding. `debug_assert!` surfaces it in tests instead.
            let window = web_sys::window();
            debug_assert!(
                window.is_some(),
                "a decoder built with a Window must still see one at Drop"
            );
            if let Some(window) = window {
                window.clear_timeout_with_handle(self._init_timer_id.get());
            }
        }
        self.worker.terminate();
    }
}

impl crate::decode::AudioPeerDecoderTrait for NetEqAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        match packet.audio_metadata.as_ref() {
            Some(audio_meta) => {
                let seq = audio_meta.sequence;

                // Track this sequence number so we can detect duplicates from
                // redundancy payloads later.  RED dedup intentionally tracks
                // the FULL u64 protobuf sequence so that two sequences that
                // differ only above bit 15 (e.g. 5 and 65541) are never
                // collapsed — unlike the u16 worker seq, which is truncated by
                // design.  See `build_insert_msg` and the boundary test
                // `test_insert_msg_truncates_seq_to_u16_but_red_tracks_full_u64`.
                self.record_sequence(seq);

                // Check whether the packet carries RED-style redundancy.
                let is_red = audio_meta.audio_format == AUDIO_RED_FORMAT;

                if is_red {
                    // Unpack the RED payload:
                    // [4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]
                    if let Some((primary, redundant_seq, redundant_data)) =
                        Self::unpack_red_audio(&packet.data)
                    {
                        // First, check if the redundant frame was lost (not yet received).
                        if !self.has_sequence(redundant_seq as u64) {
                            log::debug!(
                                "RED recovery: injecting lost audio seq {} for peer {}",
                                redundant_seq,
                                self.peer_id
                            );
                            self.record_sequence(redundant_seq as u64);
                            // Inject the recovered frame with its original sequence and a
                            // sample-domain timestamp derived from the recovered frame's own
                            // sequence number (which is one Opus frame, +960 samples, before
                            // the primary's).
                            let recovered_insert =
                                Self::build_insert_msg(redundant_seq as u64, redundant_data);
                            self.send_worker_message(recovered_insert);
                        }

                        // Now send the primary frame.
                        let insert = Self::build_insert_msg(seq, primary);
                        self.send_worker_message(insert);
                    } else {
                        // RED unpack failed -- fall back to treating the whole
                        // data blob as a single frame.
                        log::warn!(
                            "RED unpack failed for peer {} seq {}, falling back to raw",
                            self.peer_id,
                            seq
                        );
                        let insert = Self::build_insert_msg(seq, packet.data.clone());
                        self.send_worker_message(insert);
                    }
                } else {
                    // Standard (non-RED) audio packet.
                    let insert = Self::build_insert_msg(seq, packet.data.clone());
                    self.send_worker_message(insert);
                }

                let first_frame = !self.decoded;
                self.decoded = true;
                Ok(DecodeStatus {
                    rendered: true,
                    first_frame,
                })
            }
            None => {
                // Malformed/old packet that lacks metadata – skip with a warning instead of
                // propagating an error. Since issue 2225 that error would rebuild only this
                // peer's AUDIO decoder (`AudioDecodeError` → `DecoderResetScope::Audio`),
                // not all three of its decoders — but respawning the NetEQ worker over a
                // packet we can simply ignore is still the wrong trade.
                log::warn!(
                    "Received audio packet with length {} without metadata – skipping",
                    packet.data.len()
                );
                Ok(DecodeStatus {
                    rendered: false,
                    first_frame: false,
                })
            }
        }
    }

    fn flush(&mut self) {
        // Issue 2174: the buffered PCM is being dropped, so the frame that
        // would have closed out an in-flight speaking episode may never arrive.
        //
        // The VAD is deliberately left OPEN here (unlike the mute path): a
        // flush only discards the worker's buffered packets, the worker keeps
        // producing, and a live VAD is what closes the next episode. Every
        // current caller in `peer_decode_manager.rs` flushes as part of an
        // audio-off transition and calls `set_muted(true)` in the same
        // synchronous turn, so the gate that the mute path installs is already
        // in place before the browser can dispatch any stale PCM.
        self.emit_speaking_off();

        // Send flush message to NetEq worker through queue
        self.send_worker_message(WorkerMsg::Flush);
        log::debug!(
            "Sent flush message to NetEq worker for peer {}",
            self.peer_id
        );
    }

    fn set_muted(&mut self, muted: bool) {
        // Issue 2174: a muted NetEQ worker produces no PCM at all, so the
        // edge-triggered VAD can never emit the closing `speaking: 0` on its
        // own. [`Self::apply_mute_to_vad`] emits it here, and closes the VAD to
        // PCM the worker has already posted before doing so. Ordering the
        // worker `Mute` message first would not have helped — the stale frame
        // is already posted either way; only the gate closes the race.
        Self::apply_mute_to_vad(&self.peer_id, &self.vad, muted);

        // Send mute message to NetEq worker through queue
        let mute_msg = WorkerMsg::Mute { muted };
        let now = js_sys::Date::now();
        let is_ready = *self.worker_ready.borrow();
        let queue_length = self.pending_messages.borrow().len();

        // Enhanced logging for mute state tracking
        log::info!(
            "🔇 [MUTE DEBUG] Peer {} set_muted({}) at {:.0}ms - worker_ready: {}, queue_length: {}",
            self.peer_id,
            muted,
            now,
            is_ready,
            queue_length
        );

        self.send_worker_message(mute_msg);

        log::debug!(
            "Sent mute message to NetEq worker for peer {} (muted: {})",
            self.peer_id,
            muted
        );
        log::debug!(
            "✅ Mute message {} for peer {} (muted: {}) at {:.0}ms",
            if is_ready {
                "sent immediately"
            } else {
                "queued"
            },
            self.peer_id,
            muted,
            now
        );
    }

    /// Issue 2174 follow-up: hand this peer's speaking indicator over to the
    /// replacement decoder instead of tearing it down.
    ///
    /// [`VadState::retire`] clears the in-flight episode without broadcasting,
    /// so the `Drop` that fires one line later in `Peer::replace_audio_decoder`
    /// finds the VAD idle and stays silent — the UI keeps the glow the peer has
    /// earned, and the incoming decoder simply carries on emitting from its
    /// first speech frame. It also suppresses the outgoing worker's VAD for
    /// good, so PCM it posted before `terminate()` cannot broadcast for a peer
    /// the new decoder now owns.
    fn retire_for_replacement(&mut self) {
        self.vad.borrow_mut().retire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    // === Issue 2174: terminal `speaking: 0` on teardown ===
    //
    // These run on the native host target (`cargo test -p videocall-client
    // --lib`) because the seam is pure: `Rc<RefCell<..>>` state plus a
    // `DiagEvent` constructor, with no browser API in reach. The one part that
    // genuinely needs a browser — the broadcast actually landing on the global
    // bus — is the `#[wasm_bindgen_test]` at the end of this block, since the
    // bus keeps no live receiver on native and closes itself on first use.

    /// A VAD mid-episode: the peer is registered as speaking at `level`, and
    /// the gate is open (the worker is producing PCM).
    fn vad_state(speaking: bool, level: f32) -> VadState {
        VadState {
            speaking,
            audio_level: level,
            suppressed: false,
        }
    }

    fn metric_of<'a>(evt: &'a DiagEvent, name: &str) -> &'a MetricValue {
        &evt.metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("event is missing the `{name}` metric"))
            .value
    }

    /// A peer that was speaking must yield exactly one terminal event, and the
    /// state must be cleared so a later unmute re-emits cleanly.
    ///
    /// Reverting the reset in `VadState::take_terminal` makes the second-call
    /// assertion fail.
    #[test]
    fn speaking_peer_yields_one_terminal_event_and_resets() {
        let mut vad = vad_state(true, 0.73);

        assert!(
            vad.take_terminal(),
            "a speaking peer must request a terminal speaking:0 event"
        );
        assert!(!vad.speaking, "speaking flag must be cleared");
        assert_eq!(vad.audio_level, 0.0, "audio level must be cleared");

        assert!(
            !vad.take_terminal(),
            "a second teardown call must not emit a duplicate event"
        );
    }

    /// `set_muted(true)` immediately followed by `flush()` is exactly what
    /// `Peer::force_media_off` and the heartbeat audio-off path do
    /// (peer_decode_manager.rs). The pair must produce ONE event, not two.
    ///
    /// The two calls mirror production: the mute path suppresses as it closes,
    /// the flush path does not.
    #[test]
    fn mute_then_flush_emits_exactly_once() {
        let mut vad = vad_state(true, 0.5);

        let emissions = [vad.suppress_and_take_terminal(), vad.take_terminal()];

        assert_eq!(
            emissions,
            [true, false],
            "mute+flush must emit the terminal zero exactly once"
        );
    }

    /// A residual level with the speaking flag already false still has to be
    /// flushed to zero — otherwise the tile keeps rendering a partial glow.
    ///
    /// Dropping the `audio_level > 0.0` half of the guard fails this.
    #[test]
    fn residual_level_alone_still_emits() {
        let mut vad = vad_state(false, 0.31);

        assert!(
            vad.take_terminal(),
            "a non-zero residual level must still emit a terminal zero"
        );
        assert_eq!(vad.audio_level, 0.0);
    }

    /// An already-idle decoder must stay silent. This is what keeps the
    /// constructor's `set_muted(initial_muted)` from broadcasting a spurious
    /// zero for a peer that has never spoken.
    ///
    /// Making the guard unconditional fails this.
    #[test]
    fn idle_peer_emits_nothing() {
        let mut vad = vad_state(false, 0.0);

        assert!(
            !vad.take_terminal(),
            "an idle decoder must not emit a terminal event"
        );
    }

    // === Issue 2174 follow-up: the mute path must not race in-flight PCM ===

    /// THE finding-1 regression. `set_muted(true)` runs synchronously, but the
    /// worker may already have posted a PCM frame that the browser has not
    /// dispatched yet. That frame lands *after* the terminal zero, carries real
    /// speech, and — before this fix — saw `speaking == false` and broadcast a
    /// fresh `speaking: 1`. Nothing could ever close it: the worker is muted by
    /// then and produces no more PCM, so the tile glow and the green mic stayed
    /// lit next to a muted-mic glyph until the ≤5s keepalive heartbeat or the
    /// 12.5s deadman.
    ///
    /// Mutation: delete the `suppressed` early-return in `VadState::observe`,
    /// or the `self.suppressed = true` in `suppress_and_take_terminal`, and the
    /// third assertion fails.
    #[test]
    fn pcm_posted_before_a_mute_cannot_re_arm_the_speaking_indicator() {
        let peer_id = "issue-2174-mute-race";
        let vad = Rc::new(RefCell::new(VadState::new()));

        // The peer is mid-sentence.
        assert!(
            vad.borrow_mut().observe(true, 0.8),
            "a loud PCM frame must arm the speaking indicator"
        );
        assert!(vad.borrow().speaking);

        // The peer mutes, through the production entry point. (On the host
        // target the diagnostics bus has no live receiver and swallows the
        // broadcast; `terminal_zero_reaches_the_diagnostics_bus` covers the
        // emit itself in a browser.)
        NetEqAudioPeerDecoder::apply_mute_to_vad(peer_id, &vad, true);
        assert!(
            !vad.borrow().speaking,
            "muting mid-word must clear the speaking state (issue 2174)"
        );

        // A frame the worker had already posted is dispatched now. It is real
        // speech, recorded before the mute — and it must change nothing.
        assert!(
            !vad.borrow_mut().observe(true, 0.8),
            "PCM posted before the mute must not broadcast a new speaking event"
        );
        assert!(
            !vad.borrow().speaking,
            "stale PCM must not leave the peer recorded as speaking"
        );
        assert_eq!(
            vad.borrow().audio_level,
            0.0,
            "stale PCM must not restore the level"
        );
    }

    /// Unmuting must fully re-arm the fast path: the gate reopens and the next
    /// speech frame is a fresh rising edge. Without this the mute fix would
    /// trade a stuck-on glow for a permanently dead one.
    ///
    /// Mutation: empty out `VadState::reopen`, or drop the unmute arm of
    /// `apply_mute_to_vad`, and the final assertion fails.
    #[test]
    fn unmute_reopens_the_speaking_indicator() {
        let peer_id = "issue-2174-unmute";
        let vad = Rc::new(RefCell::new(VadState::new()));
        vad.borrow_mut().observe(true, 0.8);
        NetEqAudioPeerDecoder::apply_mute_to_vad(peer_id, &vad, true);

        NetEqAudioPeerDecoder::apply_mute_to_vad(peer_id, &vad, false);

        assert!(
            vad.borrow_mut().observe(true, 0.8),
            "the first speech frame after an unmute must emit a fresh rising edge"
        );
        assert!(vad.borrow().speaking);
    }

    /// The edge-trigger that moved out of `handle_pcm_data` into
    /// `VadState::observe` must still behave the same: emit on a speaking flip
    /// OR on a level move larger than `AUDIO_LEVEL_DELTA_THRESHOLD`, and stay
    /// quiet otherwise. Without the level half the tile glow would not track
    /// loudness; without the threshold every 20 ms frame would broadcast.
    #[test]
    fn observe_emits_on_a_level_move_but_not_on_jitter() {
        let mut vad = VadState::new();
        assert!(vad.observe(true, 0.50), "the rising edge must emit");

        assert!(
            !vad.observe(true, 0.50 + AUDIO_LEVEL_DELTA_THRESHOLD / 2.0),
            "a sub-threshold level move must not emit"
        );
        assert!(
            vad.observe(true, 0.50 + AUDIO_LEVEL_DELTA_THRESHOLD * 2.0),
            "a level move past the threshold must emit without a speaking flip"
        );
    }

    // === Issue 2225: bulk-copy RMS ===============================================
    //
    // `calculate_rms` used to cross the wasm↔JS boundary once per sample. It now
    // bulk-copies the frame and calls `rms_of_samples`. These pin the arithmetic
    // (host) and the copy (browser).

    /// Hand-computed reference values. None of them is derived by re-running the
    /// production formula: a constant-amplitude frame has RMS exactly equal to
    /// that amplitude, and the two-sample case is `sqrt((0.09 + 0.16) / 2)`.
    ///
    /// Mutation: widen the accumulator, change `sum / len` to `sum`, or drop the
    /// `sqrt`, and these fail.
    #[test]
    fn rms_of_hand_computed_frames() {
        assert_eq!(rms_of_samples(&[]), 0.0, "an empty frame has no energy");
        assert_eq!(rms_of_samples(&[0.0; 960]), 0.0, "silence is RMS 0");
        assert_eq!(
            rms_of_samples(&[0.5]),
            0.5,
            "a single sample's RMS is its own magnitude"
        );
        assert_eq!(
            rms_of_samples(&[-0.5, 0.5, -0.5, 0.5]),
            0.5,
            "RMS must square away the sign, not cancel"
        );

        let two = rms_of_samples(&[0.3, -0.4]);
        assert!(
            (two - 0.125_f32.sqrt()).abs() < 1e-6,
            "sqrt((0.09 + 0.16) / 2) expected, got {two}"
        );
    }

    /// The VAD compares `rms > vad_threshold`, so the values that matter are the
    /// ones sitting either side of it. A constant-amplitude frame reads back its
    /// own amplitude, which makes the expected side of the threshold exact.
    ///
    /// Mutation: any change that scales the result (dropping the `/ len`, or
    /// summing magnitudes instead of squares) moves both frames to the same side
    /// and this fails.
    #[test]
    fn rms_straddles_the_vad_threshold_on_the_right_side() {
        let quiet = rms_of_samples(&[0.0019; 960]);
        let loud = rms_of_samples(&[0.0021; 960]);

        assert!(
            quiet <= DEFAULT_VAD_THRESHOLD,
            "a 0.0019 frame must read as silence against the {DEFAULT_VAD_THRESHOLD} threshold, \
             got {quiet}"
        );
        assert!(
            loud > DEFAULT_VAD_THRESHOLD,
            "a 0.0021 frame must read as speech against the {DEFAULT_VAD_THRESHOLD} threshold, \
             got {loud}"
        );
    }

    /// Browser-only: the bulk `copy_to` path must produce EXACTLY what the
    /// pre-2225 per-sample `get_index` loop produced.
    ///
    /// The oracle here is the superseded implementation, reproduced verbatim —
    /// it is the thing the change claims to preserve, so comparing against it is
    /// the point, not a re-implementation of current logic. `assert_eq!` on f32
    /// is deliberate: bit-identical, not approximately equal.
    ///
    /// Mutation: sum with `f64` accumulation, or `iter().rev()`, and the
    /// asymmetric fixtures below diverge in the last bits. Both mutations were
    /// run against this test and both fail it.
    #[wasm_bindgen_test]
    fn bulk_copy_rms_is_bit_identical_to_the_per_sample_oracle() {
        /// The pre-2225 implementation: one boundary crossing per sample.
        fn per_sample_oracle(pcm: &Float32Array) -> f32 {
            let length = pcm.length() as usize;
            if length == 0 {
                return 0.0;
            }
            let mut sum_squares: f32 = 0.0;
            for i in 0..length {
                let sample = pcm.get_index(i as u32);
                sum_squares += sample * sample;
            }
            (sum_squares / length as f32).sqrt()
        }

        fn from_slice(values: &[f32]) -> Float32Array {
            let arr = Float32Array::new_with_length(values.len() as u32);
            for (i, v) in values.iter().enumerate() {
                arr.set_index(i as u32, *v);
            }
            arr
        }

        // A ramp of irrational-ish magnitudes, so every partial sum carries
        // rounding error that a reordered accumulation would expose.
        let ramp: Vec<f32> = (0..SAMPLES_PER_AUDIO_FRAME)
            .map(|i| ((i as f32) * 0.000_713).sin() * 0.37 - 0.11)
            .collect();

        // Absorption fixture: a full-scale click followed by a frame of very
        // quiet samples. Each quiet square (~3e-8) is below half an ULP of the
        // running sum (~1.0), so an `f32` accumulator drops every one of them
        // while a WIDER accumulator keeps them — the 960 of them then add up to
        // a difference far above `f32::EPSILON`. Without this fixture, widening
        // the accumulator is invisible and the bit-identity claim on
        // `rms_of_samples` would be unguarded.
        let mut absorbed = vec![1.0f32];
        absorbed.extend(std::iter::repeat_n(0.000_172_6f32, 960));

        for fixture in [
            Vec::new(),
            vec![0.0; 480],
            vec![0.5],
            vec![-0.5, 0.5, -0.5, 0.5],
            vec![f32::MIN_POSITIVE, 0.9, f32::MIN_POSITIVE],
            ramp,
            absorbed,
        ] {
            let arr = from_slice(&fixture);
            assert_eq!(
                NetEqAudioPeerDecoder::calculate_rms(&arr),
                per_sample_oracle(&arr),
                "bulk copy diverged from the per-sample oracle for a {}-sample frame",
                fixture.len()
            );
        }
    }

    /// Browser-only: the scratch buffer is shared and only ever grows, so a
    /// short frame following a long one must not read the tail the long frame
    /// left behind.
    ///
    /// Mutation: pass the whole `buf` to `copy_to` instead of `&mut buf[..length]`
    /// and this panics (length mismatch); divide by `buf.len()` instead of
    /// `length` and the second assertion fails.
    #[wasm_bindgen_test]
    fn a_short_frame_after_a_long_one_ignores_the_stale_tail() {
        let loud = Float32Array::new_with_length(960);
        loud.fill(0.5, 0, 960);
        assert_eq!(NetEqAudioPeerDecoder::calculate_rms(&loud), 0.5);

        // Same amplitude, quarter the length: RMS is amplitude-only, so a stale
        // tail (or the wrong divisor) would show up immediately.
        let short = Float32Array::new_with_length(240);
        short.fill(0.25, 0, 240);
        assert_eq!(
            NetEqAudioPeerDecoder::calculate_rms(&short),
            0.25,
            "a 240-sample frame must not be diluted or inflated by the 960-sample scratch"
        );
    }

    // === Issue 2174 follow-up: Drop fires on REPLACEMENT, not just teardown ==

    /// THE finding-3 regression. An AUDIO decode error calls
    /// `Peer::reset_for_decode_error`, which swaps this decoder out via
    /// `Peer::replace_audio_decoder` — whose assignment over `self.audio` drops
    /// the old decoder while the peer is still talking. `Drop`'s terminal
    /// `speaking: 0` then blanks the AUDIO indicator (tile level forced to 0.0,
    /// roster dot darkened, deadman cancelled) until the replacement worker
    /// finishes worklet init; repeated errors turn that into a blinking glow.
    ///
    /// (Issue 2225 removed the VIDEO/SCREEN trigger for this replacement — those
    /// errors no longer touch the audio decoder — but the AUDIO trigger remains,
    /// so both halves below still describe live paths.)
    ///
    /// Both halves matter: a replacement must stay silent, and a genuine
    /// teardown must still emit — that terminal zero IS issue 2174's fix.
    ///
    /// Mutation: empty out `VadState::retire` and the second half fails; make
    /// `suppress_and_take_terminal` unconditionally silent and the first fails.
    #[test]
    fn teardown_still_emits_the_terminal_zero_but_replacement_stays_silent() {
        // Genuine teardown (peer removal): the peer is still marked speaking,
        // so Drop owes the zero that clears the glow.
        let mut torn_down = vad_state(true, 0.9);
        assert!(
            torn_down.suppress_and_take_terminal(),
            "a real teardown must still emit the terminal speaking:0 (issue 2174)"
        );

        // Replacement: `retire_for_replacement` runs first, so the Drop that
        // fires one line later in `Peer::replace_audio_decoder` finds the VAD
        // already idle and says nothing.
        let mut replaced = vad_state(true, 0.9);
        replaced.retire();
        assert!(
            !replaced.suppress_and_take_terminal(),
            "a decoder being replaced must not announce that the peer stopped speaking"
        );
    }

    /// Retiring also has to close the gate: the outgoing worker's `onmessage`
    /// closure shares the VAD state, so PCM it posted before the handler is
    /// detached would otherwise broadcast for a peer the replacement decoder
    /// already owns.
    ///
    /// Mutation: drop `self.suppressed = true` from `VadState::retire` and this
    /// fails.
    #[test]
    fn a_retired_vad_ignores_the_outgoing_workers_in_flight_pcm() {
        let mut vad = vad_state(true, 0.9);
        vad.retire();

        assert!(
            !vad.observe(true, 0.95),
            "PCM from the retired worker must not broadcast for the replacement's peer"
        );
        assert!(!vad.speaking);
    }

    /// The terminal event must be shaped exactly like the ones
    /// `handle_pcm_data` emits, but reporting silence — otherwise existing
    /// consumers (tile glow, peer list, health reporter) would ignore it.
    #[test]
    fn terminal_event_reports_silence_in_the_normal_shape() {
        let evt = NetEqAudioPeerDecoder::terminal_speaking_event("peer-42", 1_700_000_000_123);

        assert_eq!(evt.subsystem, "peer_speaking");
        assert_eq!(evt.stream_id.as_deref(), Some("speaking->peer-42"));
        assert_eq!(evt.ts_ms, 1_700_000_000_123);

        match metric_of(&evt, "to_peer") {
            MetricValue::Text(v) => assert_eq!(v, "peer-42"),
            other => panic!("to_peer must be Text, got {other:?}"),
        }
        assert!(
            matches!(metric_of(&evt, "speaking"), MetricValue::U64(0)),
            "speaking must be 0, got {:?}",
            metric_of(&evt, "speaking")
        );
        match metric_of(&evt, "audio_level") {
            MetricValue::F64(v) => assert_eq!(*v, 0.0),
            other => panic!("audio_level must be F64(0.0), got {other:?}"),
        }
    }

    /// Drain the diagnostics bus up to the next `peer_speaking` event for
    /// `peer_id`.
    ///
    /// `Overflowed` is explicitly recoverable (see
    /// `videocall_diagnostics::recv_loop_action`, issue 2174): the bus runs in
    /// overflow mode, so a `while let Ok(..)` drain would stop at the first
    /// overflow and report "no event" for a peer that did in fact emit one.
    fn next_for_peer(
        rx: &mut async_broadcast::Receiver<DiagEvent>,
        peer_id: &str,
    ) -> Option<DiagEvent> {
        let want = format!("speaking->{peer_id}");
        loop {
            match rx.try_recv() {
                Ok(evt) => {
                    if evt.subsystem == "peer_speaking" && evt.stream_id.as_deref() == Some(&want) {
                        return Some(evt);
                    }
                }
                Err(async_broadcast::TryRecvError::Overflowed(_)) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Browser-only: prove the terminal event actually reaches the global
    /// diagnostics bus (the native bus closes itself, so this cannot be
    /// asserted on the host target), and that a second teardown call is silent.
    #[wasm_bindgen_test]
    fn terminal_zero_reaches_the_diagnostics_bus() {
        let peer_id = "issue-2174-terminal-zero";
        let vad = Rc::new(RefCell::new(vad_state(true, 0.9)));

        // Subscribe first: a receiver only sees events broadcast after it.
        let mut rx = videocall_diagnostics::subscribe();

        NetEqAudioPeerDecoder::emit_terminal_speaking_zero(peer_id, &vad);

        let evt = next_for_peer(&mut rx, peer_id)
            .expect("teardown must broadcast a terminal speaking:0 on the diagnostics bus");
        assert!(matches!(metric_of(&evt, "speaking"), MetricValue::U64(0)));
        assert!(!vad.borrow().speaking);

        NetEqAudioPeerDecoder::emit_terminal_speaking_zero(peer_id, &vad);
        assert!(
            next_for_peer(&mut rx, peer_id).is_none(),
            "an already-idle decoder must not broadcast a duplicate terminal event"
        );
    }

    /// Install the `<link id="neteq-worker">` that
    /// [`NetEqAudioPeerDecoder::create_neteq_worker`] reads, pointing at a stub
    /// worker built from a blob URL. Idempotent, so several tests can call it.
    ///
    /// The stub does nothing: these tests drive the decoder's VAD directly and
    /// only need the `Worker` to construct (and to be terminated by `Drop`).
    fn install_stub_neteq_worker_link() {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .expect("browser test needs a document");
        if document.get_element_by_id("neteq-worker").is_some() {
            return;
        }

        let parts = js_sys::Array::of1(&JsValue::from_str("self.onmessage = () => {};"));
        let options = web_sys::BlobPropertyBag::new();
        options.set_type("text/javascript");
        let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options)
            .expect("stub worker blob");
        let url = web_sys::Url::create_object_url_with_blob(&blob).expect("stub worker url");

        let link = document.create_element("link").expect("create link");
        link.set_id("neteq-worker");
        link.set_attribute("href", &url).expect("set href");
        document
            .body()
            .expect("browser test needs a body")
            .append_child(&link)
            .expect("append link");
    }

    /// Browser-only: `Drop` is the path issue 2174 added the terminal
    /// `speaking: 0` to, and the path finding 3 says fires on REPLACEMENT too.
    /// It cannot be reached from behind `Box<dyn AudioPeerDecoderTrait>`, so
    /// this builds real decoders via `NetEqAudioPeerDecoder::build` and drops
    /// them.
    ///
    /// Mutation: delete the emit from `drop` and the teardown half fails;
    /// empty out `retire_for_replacement` and the replacement half fails.
    #[wasm_bindgen_test]
    fn dropping_a_decoder_announces_silence_unless_it_was_retired() {
        install_stub_neteq_worker_link();
        let mut rx = videocall_diagnostics::subscribe();

        // Genuine teardown (the peer left): the decoder is mid-episode, so its
        // Drop owes the terminal zero that clears the glow.
        let torn_down = "issue-2174-drop-teardown";
        let decoder = NetEqAudioPeerDecoder::build(None, torn_down.to_string(), false, None)
            .expect("build teardown decoder");
        decoder.vad.borrow_mut().observe(true, 0.8);
        drop(decoder);

        let evt = next_for_peer(&mut rx, torn_down)
            .expect("dropping a live decoder must broadcast the terminal speaking:0");
        assert!(
            matches!(metric_of(&evt, "speaking"), MetricValue::U64(0)),
            "teardown must report speaking: 0"
        );

        // Replacement (`Peer::reset_for_decode_error` on AUDIO): the same Drop
        // runs, but the peer has not stopped speaking, so it must say nothing.
        let replaced = "issue-2174-drop-replacement";
        let mut decoder = NetEqAudioPeerDecoder::build(None, replaced.to_string(), false, None)
            .expect("build replaced decoder");
        decoder.vad.borrow_mut().observe(true, 0.8);
        decoder.retire_for_replacement();
        drop(decoder);

        assert!(
            next_for_peer(&mut rx, replaced).is_none(),
            "a decoder retired for replacement must not announce that the peer went quiet"
        );
    }

    /// Issue 2225: the `onmessage` closure is now OWNED by the decoder instead of
    /// `.forget()`-ed, so `Drop` frees it. That is only safe if `Drop` detaches
    /// the handler first — a worker message dispatched into a freed
    /// wasm-bindgen closure panics, and a panic in wasm takes the page with it.
    ///
    /// The `Worker` handle is cloned before the drop (a `Worker` is a JS object
    /// reference, so the clone observes the same object the decoder held) and
    /// the handler slot is read after it.
    ///
    /// Mutation: delete `self.worker.set_onmessage(None)` from `Drop` and the
    /// assertion fails — which is precisely the state in which a late message
    /// would reach freed memory.
    #[wasm_bindgen_test]
    fn dropping_a_decoder_detaches_the_worker_handler_before_freeing_it() {
        install_stub_neteq_worker_link();

        let decoder =
            NetEqAudioPeerDecoder::build(None, "issue-2225-detach".to_string(), true, None)
                .expect("build decoder");
        let worker = decoder.worker.clone();
        assert!(
            worker.onmessage().is_some(),
            "a live decoder must have its handler installed"
        );

        drop(decoder);

        assert!(
            worker.onmessage().is_none(),
            "Drop must detach the worker handler before the closure field is freed"
        );
    }

    /// Browser-only end-to-end pin for the mute race (issue 2174 follow-up,
    /// finding 1). The host tests above pin [`VadState`]; this one drives the
    /// REAL [`NetEqAudioPeerDecoder::handle_pcm_data`] — the function the NetEQ
    /// worker's `onmessage` closure calls for every `Float32Array` — and
    /// asserts against the real diagnostics bus that a frame dispatched after
    /// the mute broadcasts nothing.
    ///
    /// Step 3 is the frame the worker posted just before the mute took effect;
    /// the browser dispatches it after `set_muted(true)` has already run to
    /// completion. Reverting the gate in `VadState::observe` makes step 3
    /// broadcast a `speaking: 1` that nothing ever closes, and this test fails.
    #[wasm_bindgen_test]
    fn pcm_dispatched_after_a_mute_puts_nothing_on_the_bus() {
        let peer_id = "issue-2174-late-pcm";
        let vad = Rc::new(RefCell::new(VadState::new()));
        let audio_context = AudioContext::new().expect("test needs a WebAudio context");
        let pcm_player: Rc<RefCell<Option<AudioWorkletNode>>> = Rc::new(RefCell::new(None));

        // One 20 ms frame of steady, clearly-audible speech: RMS 0.05 sits well
        // above DEFAULT_VAD_THRESHOLD (0.002).
        let speech = Float32Array::new_with_length(SAMPLES_PER_AUDIO_FRAME);
        speech.fill(0.05, 0, SAMPLES_PER_AUDIO_FRAME);

        let mut rx = videocall_diagnostics::subscribe();

        // 1. The peer is mid-sentence: the fast path arms the indicator.
        NetEqAudioPeerDecoder::handle_pcm_data(
            speech.clone(),
            pcm_player.clone(),
            &audio_context,
            None,
            peer_id,
            vad.clone(),
            DEFAULT_VAD_THRESHOLD,
        );
        let armed = next_for_peer(&mut rx, peer_id)
            .expect("a speech frame must broadcast peer_speaking on the bus");
        assert!(
            matches!(metric_of(&armed, "speaking"), MetricValue::U64(1)),
            "the first speech frame must report speaking: 1"
        );

        // 2. The peer mutes mid-word — the whole VAD side of `set_muted(true)`.
        NetEqAudioPeerDecoder::apply_mute_to_vad(peer_id, &vad, true);
        let closed = next_for_peer(&mut rx, peer_id)
            .expect("muting mid-word must broadcast the terminal zero");
        assert!(
            matches!(metric_of(&closed, "speaking"), MetricValue::U64(0)),
            "the mute must report speaking: 0"
        );

        // 3. The frame the worker had already posted is dispatched now.
        NetEqAudioPeerDecoder::handle_pcm_data(
            speech,
            pcm_player,
            &audio_context,
            None,
            peer_id,
            vad,
            DEFAULT_VAD_THRESHOLD,
        );
        assert!(
            next_for_peer(&mut rx, peer_id).is_none(),
            "PCM posted before the mute must not re-light the speaking indicator"
        );
    }

    #[wasm_bindgen_test]
    fn unpack_valid_red_data() {
        // Manually build a RED buffer:
        // [4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]
        let primary = b"primary_frame";
        let redundant = b"redundant_frame";
        let primary_len = (primary.len() as u32).to_le_bytes();
        let redundant_seq = 42u32.to_le_bytes();

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(primary);
        data.extend_from_slice(&redundant_seq);
        data.extend_from_slice(redundant);

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (p, seq, r) = result.unwrap();
        assert_eq!(p, primary);
        assert_eq!(seq, 42);
        assert_eq!(r, redundant);
    }

    #[wasm_bindgen_test]
    fn unpack_empty_input() {
        let result = NetEqAudioPeerDecoder::unpack_red_audio(&[]);
        assert!(result.is_none(), "empty input should return None");
    }

    #[wasm_bindgen_test]
    fn unpack_too_short_input() {
        // Less than 8 bytes (minimum: 4 primary_len + 0 primary + 4 redundant_seq)
        let result = NetEqAudioPeerDecoder::unpack_red_audio(&[0, 0, 0]);
        assert!(result.is_none(), "3 bytes should return None");

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&[0, 0, 0, 0, 0, 0, 0]);
        assert!(result.is_none(), "7 bytes should return None");
    }

    #[wasm_bindgen_test]
    fn unpack_exactly_8_bytes_zero_length_frames() {
        // primary_len=0, redundant_seq=0, no primary data, no redundant data
        let data = [0u8; 8];
        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (p, seq, r) = result.unwrap();
        assert!(p.is_empty());
        assert_eq!(seq, 0);
        assert!(r.is_empty());
    }

    #[wasm_bindgen_test]
    fn unpack_primary_len_exceeds_sanity_limit() {
        // primary_len > 10,000 should return None
        let primary_len = 10_001u32.to_le_bytes();
        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&[0u8; 8]); // some filler

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_none(), "primary_len > 10000 should be rejected");
    }

    #[wasm_bindgen_test]
    fn unpack_primary_len_at_sanity_limit() {
        // primary_len == 10,000 should be accepted (boundary)
        let primary_len = 10_000u32.to_le_bytes();
        let primary_data = vec![0xAA; 10_000];
        let redundant_seq = 5u32.to_le_bytes();
        let redundant_data = b"red";

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&primary_data);
        data.extend_from_slice(&redundant_seq);
        data.extend_from_slice(redundant_data);

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some(), "primary_len == 10000 should be accepted");

        let (p, seq, r) = result.unwrap();
        assert_eq!(p.len(), 10_000);
        assert_eq!(seq, 5);
        assert_eq!(r, redundant_data);
    }

    #[wasm_bindgen_test]
    fn unpack_primary_len_exceeds_data_length() {
        // primary_len claims 100 bytes but total data is only 20 bytes
        let primary_len = 100u32.to_le_bytes();
        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&[0u8; 16]); // only 16 bytes after primary_len header

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(
            result.is_none(),
            "malformed packet with primary_len > remaining data should return None"
        );
    }

    #[wasm_bindgen_test]
    fn unpack_no_room_for_redundant_seq() {
        // primary_len = 10, but data only has 4 (header) + 10 (primary) + 3 (not enough for seq)
        let primary_len = 10u32.to_le_bytes();
        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&[0xBB; 10]); // primary data
        data.extend_from_slice(&[0, 0, 0]); // only 3 bytes, need 4 for seq

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(
            result.is_none(),
            "not enough room for redundant_seq should return None"
        );
    }

    #[wasm_bindgen_test]
    fn unpack_no_redundant_data_after_seq() {
        // Valid format but zero-length redundant data
        let primary_len = 5u32.to_le_bytes();
        let redundant_seq = 99u32.to_le_bytes();

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(b"AUDIO");
        data.extend_from_slice(&redundant_seq);
        // No redundant data after seq

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (p, seq, r) = result.unwrap();
        assert_eq!(p, b"AUDIO");
        assert_eq!(seq, 99);
        assert!(r.is_empty());
    }

    #[wasm_bindgen_test]
    fn unpack_preserves_binary_data() {
        // Ensure all byte values 0x00-0xFF are preserved correctly
        let primary: Vec<u8> = (0..=255).collect();
        let redundant: Vec<u8> = (0..=255).rev().collect();
        let primary_len = (primary.len() as u32).to_le_bytes();
        let redundant_seq = 1000u32.to_le_bytes();

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.extend_from_slice(&primary);
        data.extend_from_slice(&redundant_seq);
        data.extend_from_slice(&redundant);

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (p, seq, r) = result.unwrap();
        assert_eq!(p, primary);
        assert_eq!(seq, 1000);
        assert_eq!(r, redundant);
    }

    #[wasm_bindgen_test]
    fn unpack_max_valid_sequence_number() {
        let primary_len = 1u32.to_le_bytes();
        let redundant_seq = u32::MAX.to_le_bytes();

        let mut data = Vec::new();
        data.extend_from_slice(&primary_len);
        data.push(0xFF); // 1 byte primary
        data.extend_from_slice(&redundant_seq);
        data.push(0xAA); // 1 byte redundant

        let result = NetEqAudioPeerDecoder::unpack_red_audio(&data);
        assert!(result.is_some());

        let (_, seq, _) = result.unwrap();
        assert_eq!(seq, u32::MAX);
    }

    #[wasm_bindgen_test]
    fn record_and_has_sequence() {
        // Test the sequence tracking ring buffer used for RED deduplication.
        // We can't construct a full NetEqAudioPeerDecoder without browser APIs,
        // so we test the VecDeque logic directly.
        use std::collections::VecDeque;

        let capacity = crate::adaptive_quality_constants::AUDIO_RED_SEQ_HISTORY_SIZE;
        let mut received: VecDeque<u64> = VecDeque::with_capacity(capacity);

        // Helper: mirrors record_sequence logic
        let record = |buf: &mut VecDeque<u64>, seq: u64| {
            if buf.len() >= capacity {
                buf.pop_front();
            }
            buf.push_back(seq);
        };

        // Record some sequences
        record(&mut received, 10);
        record(&mut received, 11);
        record(&mut received, 12);

        assert!(received.contains(&10));
        assert!(received.contains(&11));
        assert!(received.contains(&12));
        assert!(!received.contains(&13));

        // Fill to capacity and verify eviction
        for i in 13..(13 + capacity as u64) {
            record(&mut received, i);
        }

        // Sequence 10 should have been evicted
        assert!(!received.contains(&10));
        assert_eq!(received.len(), capacity);
    }

    // -----------------------------------------------------------------------
    // Receiver-boundary tests for issue #623
    // -----------------------------------------------------------------------

    /// Verify that `build_insert_msg` (the production seam used by all four
    /// `WorkerMsg::Insert` call sites in `decode()`) truncates the u64 sequence
    /// number to u16, and that the truncation produces the expected wrap-reduced
    /// value for a seq that has crossed the u16 boundary (~21.8 min at 20 ms/frame).
    ///
    /// Mutation sensitivity:
    ///   - The `seq` assertion confirms the wrap-reduced value (4), but because
    ///     `WorkerMsg::Insert.seq` is typed `u16`, the compiler enforces truncation
    ///     at the field boundary regardless of whether `build_insert_msg` uses
    ///     `as u16` explicitly. The seq assertion is therefore NOT independently
    ///     mutation-resistant against removal of the explicit cast.
    ///   - THE PRIMARY MUTATION-RESISTANT PIN is the `timestamp` assertion.
    ///     `seq_to_sample_timestamp` is called with the full u64 (65540) and
    ///     produces 62_918_400. If `build_insert_msg` sourced the timestamp from
    ///     the truncated u16 (4) instead, it would produce 3_840 and the assertion
    ///     would fail -- proving the test pins the full-u64 timestamp path.
    ///
    /// Cross-reference: `neteq/src/neteq.rs::test_seq_wrap_no_buffer_flush`
    /// proves that this u16 seq wrap does not flush or reject packets in NetEQ.
    #[wasm_bindgen_test]
    fn test_insert_msg_truncates_seq_to_u16_but_red_tracks_full_u64() {
        // 65540 = 65536 + 4, so as u16 == 4. This simulates a seq that has
        // crossed the u16 wrap boundary (~21.8 min at 20 ms/frame).
        let over_wrap_seq: u64 = 65540;
        let payload = b"opus_frame".to_vec();

        let msg = NetEqAudioPeerDecoder::build_insert_msg(over_wrap_seq, payload.clone());

        match msg {
            WorkerMsg::Insert {
                seq,
                timestamp,
                payload: returned_payload,
            } => {
                // Confirms the wrap-reduced value. Because the field is u16-typed
                // the compiler enforces truncation at the boundary regardless; this
                // assertion documents the expected post-wrap value (65540 mod 65536 == 4)
                // but is NOT the primary mutation pin. See the timestamp assertion below.
                assert_eq!(seq, 4u16, "seq must be 65540 mod 65536 == 4 after u16 wrap");

                // PRIMARY MUTATION-RESISTANT PIN: the timestamp must be derived
                // from the FULL u64 seq via the production function, not from the
                // truncated u16. If build_insert_msg sourced the timestamp from
                // `over_wrap_seq as u16 as u64` (== 4) instead of 65540, the result
                // would be 4*960 = 3_840, not 65540*960 = 62_918_400, and this
                // assertion would fail.
                let expected_ts = seq_to_sample_timestamp(over_wrap_seq);
                assert_eq!(
                    timestamp, expected_ts,
                    "timestamp must be derived from the full u64 seq, not the truncated u16"
                );
                // Sanity-check: 65540 * 960 = 62_918_400 (fits in u32, no wrap here).
                assert_eq!(expected_ts, 65540u32.wrapping_mul(960));

                assert_eq!(returned_payload, payload);
            }
            other => panic!("Expected WorkerMsg::Insert, got {:?}", other),
        }
    }

    /// Verify that the RED deduplication ring buffer tracks FULL u64 sequence
    /// numbers, so two sequences that share the same u16 low bits (e.g. 5 and
    /// 65541 = 5 + 65536) are tracked as DISTINCT entries and are never collapsed.
    ///
    /// Why this matters: if `record_sequence`/`has_sequence` were truncated
    /// to u16 internally, `has_sequence(65541)` would return true after only
    /// `record_sequence(5)` was called, causing RED recovery to wrongly suppress
    /// a frame at sequence 65541 as a duplicate.
    ///
    /// Mutation check: if the VecDeque element type or the insert/lookup were
    /// changed to u16 (truncating before storage), then
    /// `received.contains(&65541u64)` would return false (the stored value
    /// would be 5, not 65541), causing `assert!(received.contains(&65541))` to
    /// fail -- proving the test pins the full-width tracking invariant.
    #[wasm_bindgen_test]
    fn test_red_dedup_tracks_full_u64_no_u16_collision() {
        use std::collections::VecDeque;

        let capacity = crate::adaptive_quality_constants::AUDIO_RED_SEQ_HISTORY_SIZE;
        // Mirror record_sequence / has_sequence using the same VecDeque<u64> type.
        // This follows the precedent established by `record_and_has_sequence`.
        let mut received: VecDeque<u64> = VecDeque::with_capacity(capacity);

        let record = |buf: &mut VecDeque<u64>, seq: u64| {
            if buf.len() >= capacity {
                buf.pop_front();
            }
            buf.push_back(seq);
        };

        // Record sequence 5 (u16 low bits: 5).
        record(&mut received, 5);

        // Record sequence 65541 = 5 + 65536; as u16 this is also 5.
        // If tracking were truncated to u16, 65541 and 5 would collide.
        record(&mut received, 65541);

        // Both full-width u64 values must be present as distinct entries.
        assert!(
            received.contains(&5u64),
            "sequence 5 must be tracked as u64=5"
        );
        assert!(
            received.contains(&65541u64),
            "sequence 65541 must be tracked as u64=65541, \
             not collapsed with seq 5 via u16 truncation"
        );

        // The buffer has two distinct entries, not one collapsed entry.
        assert_eq!(
            received.len(),
            2,
            "5 and 65541 must be two distinct entries in the RED ring buffer; \
             u16 truncation would collapse them into one"
        );
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod host_tests {
    use super::*;

    #[test]
    fn seq_maps_to_sample_domain_timestamp() {
        // The constant must resolve to exactly one Opus frame at 48 kHz.
        assert_eq!(SAMPLES_PER_AUDIO_FRAME, 960);

        // Each sequence step advances by exactly one Opus frame (+960 samples),
        // which is what NetEQ's delay manager expects from the timestamp field.
        assert_eq!(seq_to_sample_timestamp(0), 0);
        assert_eq!(seq_to_sample_timestamp(1), 960);
        assert_eq!(seq_to_sample_timestamp(2), 1920);

        // The timestamp wraps in the u32 domain like a real RTP timestamp:
        // 4_500_000 * 960 = 4_320_000_000, which exceeds u32::MAX. After wrapping
        // (minus 4_294_967_296) the result is 25_032_704.
        assert_eq!(seq_to_sample_timestamp(4_500_000), 25_032_704);
    }
}
