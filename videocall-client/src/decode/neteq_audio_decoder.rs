//! Remote peer audio: NetEq in a Dedicated Worker, played by an AudioWorklet.
//!
//! Data flow for one remote peer:
//!
//! ```text
//! MediaPacket ──decode()──> RtpTimestampMapper ──Insert{seq, ts}──> NetEq worker
//!                                                                     │  ▲
//!                                     Float32Array / silence count    │  │ "n frames"
//!                                                                     ▼  │
//!                                                   AudioWorklet (pcm-player) ──> speakers
//! ```
//!
//! The worklet and the worker talk over a `MessageChannel`, so audio never
//! crosses the main thread. The worklet pulls 10 ms frames as the device
//! consumes them, which paces NetEq's `GetAudio` by the audio clock. The
//! worker reports a 50 ms audio level to the main thread for the speaking
//! indicator, and NetEq stats once a second while the diagnostics UI is open
//! ([`set_audio_diagnostics_enabled`]), every five seconds otherwise.

use crate::adaptive_quality_constants::{AUDIO_RED_FORMAT, AUDIO_RED_SEQ_HISTORY_SIZE};
use crate::audio::shared_audio_context::SharedAudioContext;
use crate::audio_constants::{
    rms_to_intensity, AUDIO_LEVEL_DELTA_THRESHOLD, DEFAULT_VAD_THRESHOLD,
};
use crate::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::decode::rtp_timestamp::{RtpMapping, RtpTimestampMapper};
use crate::decode::{AudioPeerDecoderTrait, DecodeStatus};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    AudioContext, AudioContextState, AudioWorkletNode, GainNode, MessageChannel, MessageEvent,
    MessagePort, Worker,
};

const WORKLET_CODE: &str = include_str!("../scripts/pcmPlayerWorker.js");

/// Messages to the NetEq worker (matches `neteq/src/bin/neteq_worker.rs`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
enum WorkerMsg {
    Init {
        sample_rate: u32,
        channels: u8,
    },
    /// `timestamp` is in 48 kHz samples (from [`RtpTimestampMapper`]).
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
    /// Stats every second (diagnostics UI open) or every five seconds.
    SetDiagnostics {
        enabled: bool,
    },
    /// Sent with a transferred `MessagePort` leading to the worklet.
    AttachWorklet,
}

/// Messages received from worker (matches neteq_worker.rs). Stats arrive as
/// `{type: "stats", json, ...}` and are read field by field in
/// [`NetEqAudioPeerDecoder::handle_stats_message`] instead.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum WorkerResponse {
    WorkerReady {
        mute_state: bool,
    },
    /// Peak 10 ms RMS over the last 50 ms of played audio.
    AudioLevel {
        rms: f32,
    },
}

/// Something to post to the worker.
#[derive(Debug)]
enum Outgoing {
    Msg(WorkerMsg),
    AttachWorklet(MessagePort),
}

/// Most messages held for a worker that has not reported ready. If its wasm
/// never loads, packets keep arriving at 50 per second; past this cap the
/// oldest queued packet is dropped. Queued packets are stale anyway: the
/// worker flushes NetEq on the worklet's first pull.
const MAX_PENDING_MESSAGES: usize = 256;

/// Least time between two `AudioContext.resume()` attempts. Each attempt
/// returns a promise that stays pending until the page gets a user gesture,
/// so trying on every packet would pile them up.
const RESUME_RETRY_MS: f64 = 1_000.0;

thread_local! {
    /// Whether the diagnostics UI is open, and the live workers to tell when
    /// that changes.
    static AUDIO_DIAGNOSTICS: RefCell<AudioDiagnostics> = RefCell::new(AudioDiagnostics::default());
}

#[derive(Debug, Default)]
struct AudioDiagnostics {
    enabled: bool,
    links: Vec<Weak<WorkerLink>>,
}

/// Tell every NetEq worker, current and future, whether the diagnostics UI
/// is showing. While it is, workers post stats every second for the charts;
/// otherwise every five seconds, which is enough for health reporting.
pub fn set_audio_diagnostics_enabled(enabled: bool) {
    let links = AUDIO_DIAGNOSTICS.with(|cell| {
        let mut diagnostics = cell.borrow_mut();
        diagnostics.links.retain(|link| link.strong_count() > 0);
        if diagnostics.enabled == enabled {
            return Vec::new();
        }
        diagnostics.enabled = enabled;
        diagnostics.links.clone()
    });
    // Send outside the borrow; `send` never re-enters it, but keep it simple.
    for link in links.iter().filter_map(Weak::upgrade) {
        link.send(Outgoing::Msg(WorkerMsg::SetDiagnostics { enabled }));
    }
}

/// Register a new worker and return the current diagnostics state for it.
fn register_for_diagnostics(link: &Rc<WorkerLink>) -> bool {
    AUDIO_DIAGNOSTICS.with(|cell| {
        let mut diagnostics = cell.borrow_mut();
        diagnostics.links.retain(|link| link.strong_count() > 0);
        diagnostics.links.push(Rc::downgrade(link));
        diagnostics.enabled
    })
}

/// The worker plus the queue that holds messages until it reports ready.
/// The worker's wasm loads asynchronously; messages posted before its
/// `onmessage` exists would be lost.
#[derive(Debug)]
struct WorkerLink {
    worker: Worker,
    ready: Cell<bool>,
    pending: RefCell<VecDeque<Outgoing>>,
}

impl WorkerLink {
    fn send(&self, item: Outgoing) {
        if self.ready.get() {
            self.post(item);
            return;
        }
        let mut pending = self.pending.borrow_mut();
        // A queued Mute supersedes every earlier queued Mute: only the last
        // state matters, and peers toggle mute repeatedly before the worker
        // reports ready.
        if matches!(item, Outgoing::Msg(WorkerMsg::Mute { .. })) {
            pending.retain(|queued| !matches!(queued, Outgoing::Msg(WorkerMsg::Mute { .. })));
        }
        if pending.len() >= MAX_PENDING_MESSAGES {
            // Drop the oldest packet; control messages (mute, attach) stay.
            if let Some(index) = pending
                .iter()
                .position(|queued| matches!(queued, Outgoing::Msg(WorkerMsg::Insert { .. })))
            {
                pending.remove(index);
            }
        }
        pending.push_back(item);
    }

    fn post(&self, item: Outgoing) {
        let result = match item {
            Outgoing::Msg(msg) => serde_wasm_bindgen::to_value(&msg)
                .map_err(JsValue::from)
                .and_then(|js| self.worker.post_message(&js)),
            Outgoing::AttachWorklet(port) => {
                serde_wasm_bindgen::to_value(&WorkerMsg::AttachWorklet)
                    .map_err(JsValue::from)
                    .and_then(|js| {
                        self.worker
                            .post_message_with_transfer(&js, &js_sys::Array::of1(&port))
                    })
            }
        };
        if let Err(e) = result {
            log::error!("Failed to send worker message: {e:?}");
        }
    }

    fn mark_ready(&self) {
        self.ready.set(true);
        // Drain into a local first so `post` never runs under the borrow.
        let queued: Vec<Outgoing> = self.pending.borrow_mut().drain(..).collect();
        for item in queued {
            self.post(item);
        }
    }
}

/// The peer's playback nodes. Built asynchronously after the worklet module
/// loads; `closed` tells a late build that the decoder is already gone.
#[derive(Debug, Default)]
struct Playback {
    node: Option<AudioWorkletNode>,
    gain: Option<GainNode>,
    closed: bool,
}

/// Decoder-side speaking detection state.
#[derive(Debug)]
struct Vad {
    peer_id: String,
    threshold: f32,
    speaking: Cell<bool>,
    level: Cell<f32>,
}

impl Vad {
    /// Emit a `peer_speaking` event when the speaking state flips or the
    /// level moves by more than [`AUDIO_LEVEL_DELTA_THRESHOLD`].
    ///
    /// This is the decoder-side (remote peer) VAD: it gives the UI a speaking
    /// indicator with sub-second latency instead of waiting for the 1 Hz
    /// heartbeat that carries the remote user's own `is_speaking` flag.
    fn update(&self, rms: f32) {
        let is_speaking = rms > self.threshold;
        let intensity = rms_to_intensity(rms, self.threshold);
        let level_changed = (intensity - self.level.get()).abs() > AUDIO_LEVEL_DELTA_THRESHOLD;
        if is_speaking == self.speaking.get() && !level_changed {
            return;
        }
        self.speaking.set(is_speaking);
        self.level.set(intensity);
        let peer_id = &self.peer_id;
        let _ = global_sender().try_broadcast(DiagEvent {
            subsystem: "peer_speaking",
            stream_id: Some(format!("speaking->{peer_id}")),
            ts_ms: now_ms(),
            metrics: vec![
                metric!("to_peer", peer_id.clone()),
                metric!("speaking", if is_speaking { 1u64 } else { 0u64 }),
                metric!("audio_level", intensity as f64),
            ],
        });
    }
}

/// Audio decoder that sends packets to a NetEq worker and plays its output
/// through a per-peer AudioWorklet.
#[derive(Debug)]
pub struct NetEqAudioPeerDecoder {
    link: Rc<WorkerLink>,
    audio_context: AudioContext,
    playback: Rc<RefCell<Playback>>,
    /// Owned so dropping the decoder frees it (and what it captures).
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    decoded: bool,
    peer_id: String,

    /// Ring buffer of recently received audio sequence numbers.
    /// Used to detect whether a redundant frame carried in a RED packet
    /// was already received, avoiding duplicate injection.
    received_sequences: VecDeque<u64>,

    /// Rebuilds RTP sequence numbers and 48 kHz timestamps for NetEq from
    /// the wire's 64-bit sequence and sender wall clock.
    rtp_mapper: RtpTimestampMapper,

    /// `Date.now()` of the last `AudioContext.resume()` attempt.
    last_resume_attempt_ms: f64,

    /// The mute state last handed to the worker. `set_muted` repeats often
    /// (heartbeat updates), and only a change is worth a message.
    last_mute_sent: Cell<Option<bool>>,
}

impl NetEqAudioPeerDecoder {
    /// Create a NetEq worker.
    fn create_neteq_worker() -> Result<Worker, JsValue> {
        let document = web_sys::window()
            .and_then(|window| window.document())
            .ok_or_else(|| JsValue::from_str("no document"))?;
        let worker_url = document
            .get_element_by_id("neteq-worker")
            .and_then(|link| link.get_attribute("href"))
            .ok_or_else(|| JsValue::from_str("neteq-worker link tag not found"))?;
        Worker::new(&worker_url)
    }

    /// Browsers start an AudioContext suspended until a user gesture (and
    /// Safari moves it to "interrupted" on a phone call). Try to resume while
    /// audio is flowing, at most once per [`RESUME_RETRY_MS`]; it succeeds
    /// once the page has had a gesture. A closed context cannot resume.
    fn resume_if_suspended(&mut self) {
        let state = self.audio_context.state();
        if state == AudioContextState::Running || state == AudioContextState::Closed {
            return;
        }
        let now = js_sys::Date::now();
        if now - self.last_resume_attempt_ms < RESUME_RETRY_MS {
            return;
        }
        self.last_resume_attempt_ms = now;
        if let Err(e) = self.audio_context.resume() {
            log::warn!("[neteq-audio-decoder] AudioContext resume error: {e:?}");
        }
    }

    /// Load the worklet module, create this peer's playback nodes and connect
    /// the worklet to the worker with a `MessageChannel`.
    async fn attach_playback(
        link: Rc<WorkerLink>,
        playback: Rc<RefCell<Playback>>,
        peer_id: String,
    ) -> Result<(), JsValue> {
        SharedAudioContext::ensure_pcm_worklet_ready(WORKLET_CODE).await?;
        if playback.borrow().closed {
            return Ok(());
        }
        let (node, gain) = SharedAudioContext::create_peer_playback_nodes(&peer_id)?;
        let channel = MessageChannel::new()?;
        let attach = js_sys::Object::new();
        js_sys::Reflect::set(&attach, &"command".into(), &"attach".into())?;
        node.port()?
            .post_message_with_transferable(&attach, &js_sys::Array::of1(&channel.port1()))?;
        link.send(Outgoing::AttachWorklet(channel.port2()));
        let mut playback = playback.borrow_mut();
        playback.node = Some(node);
        playback.gain = Some(gain);
        Ok(())
    }

    /// Turn a worker `{type: "stats", json, ...}` message into one `neteq`
    /// diagnostics event. The worker already serialized `json` and pulled
    /// out the numbers, so nothing here stringifies or parses JSON.
    fn handle_stats_message(data: &JsValue, peer_id: &str) {
        let field = |name: &str| js_sys::Reflect::get(data, &JsValue::from_str(name)).ok();
        let Some(json) = field("json").and_then(|json| json.as_string()) else {
            return;
        };
        // Non-negative integers below 2^53, so the casts are exact.
        let number = |name: &str| {
            field(name)
                .and_then(|value| value.as_f64())
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value as u64)
        };

        // peer_id is the target peer (whose audio we're decoding). The
        // reporting peer is a placeholder until the decoder knows the local
        // user's id.
        let current_user = "current_user"; // TODO: Get from VideoCallClient
        let mut metrics = vec![
            metric!("stats_json", json),
            metric!("reporting_peer", current_user),
            metric!("target_peer", peer_id.to_string()),
        ];
        // Same names and sources as before: `audio_buffer_ms` is
        // `network.current_buffer_size_ms`, the jitter values come from
        // `lifetime`, and `packets_awaiting_decode` is top level.
        if let Some(value) = number("buffer_ms") {
            metrics.push(metric!("audio_buffer_ms", value));
        }
        if let Some(value) = number("jitter_buffer_delay_ms") {
            metrics.push(metric!("jitter_buffer_delay_ms", value));
        }
        if let Some(value) = number("jitter_buffer_target_delay_ms") {
            metrics.push(metric!("jitter_buffer_target_delay_ms", value));
        }
        if let Some(value) = number("packets_awaiting_decode") {
            metrics.push(metric!("packets_awaiting_decode", value));
        }
        let _ = global_sender().try_broadcast(DiagEvent {
            subsystem: "neteq",
            stream_id: Some(format!("{current_user}->{peer_id}")), // reporting_peer->target_peer
            ts_ms: now_ms(),
            metrics,
        });
    }

    /// Create message handler for NetEq worker
    fn create_message_handler(link: Rc<WorkerLink>, vad: Vad) -> Closure<dyn FnMut(MessageEvent)> {
        Closure::wrap(Box::new(move |event: MessageEvent| {
            let data = event.data();
            if !data.is_object() {
                return;
            }
            let kind = js_sys::Reflect::get(&data, &JsValue::from_str("type"))
                .ok()
                .and_then(|kind| kind.as_string());
            if kind.as_deref() == Some("stats") {
                Self::handle_stats_message(&data, &vad.peer_id);
                return;
            }
            match serde_wasm_bindgen::from_value::<WorkerResponse>(data) {
                Ok(WorkerResponse::WorkerReady { mute_state }) => {
                    log::info!(
                        "Worker ready for peer {} (worker mute: {mute_state})",
                        vad.peer_id
                    );
                    link.mark_ready();
                }
                // Levels only arrive while the worklet pulls audio, so the
                // context is already running here.
                Ok(WorkerResponse::AudioLevel { rms }) => vad.update(rms),
                // Log bridge messages and anything unknown.
                Err(_) => {}
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
        let worker = Self::create_neteq_worker()?;
        let audio_context = SharedAudioContext::get_or_init(speaker_device_id)?;
        let link = Rc::new(WorkerLink {
            worker,
            ready: Cell::new(false),
            pending: RefCell::new(VecDeque::new()),
        });

        let vad = Vad {
            peer_id: peer_id.clone(),
            threshold: vad_threshold.unwrap_or(DEFAULT_VAD_THRESHOLD),
            speaking: Cell::new(false),
            level: Cell::new(0.0),
        };
        let diagnostics = register_for_diagnostics(&link);
        let on_message = Self::create_message_handler(link.clone(), vad);
        link.worker
            .set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let playback = Rc::new(RefCell::new(Playback::default()));
        {
            let link = link.clone();
            let playback = playback.clone();
            let peer_id = peer_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = Self::attach_playback(link, playback, peer_id.clone()).await {
                    log::error!("Failed to set up audio playback for peer {peer_id}: {e:?}");
                }
            });
        }
        let mut decoder = Self {
            link,
            audio_context,
            playback,
            _on_message: on_message,
            decoded: false,
            peer_id,
            received_sequences: VecDeque::with_capacity(AUDIO_RED_SEQ_HISTORY_SIZE),
            rtp_mapper: RtpTimestampMapper::new(),
            last_resume_attempt_ms: f64::NEG_INFINITY,
            last_mute_sent: Cell::new(None),
        };

        decoder.send(WorkerMsg::Init {
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS as u8,
        });
        decoder.set_muted(initial_muted);
        decoder.send(WorkerMsg::SetDiagnostics {
            enabled: diagnostics,
        });
        log::info!(
            "NetEq audio decoder initialized for peer {} (muted: {initial_muted})",
            decoder.peer_id
        );

        Ok(Box::new(decoder))
    }

    fn send(&self, msg: WorkerMsg) {
        self.link.send(Outgoing::Msg(msg));
    }

    fn insert(&self, mapping: RtpMapping, payload: Vec<u8>) {
        self.send(WorkerMsg::Insert {
            seq: mapping.sequence_number,
            timestamp: mapping.timestamp,
            payload,
        });
    }

    /// Map a packet onto NetEq's RTP timeline. `None` drops a straggler from
    /// before a sender restart.
    fn map(&mut self, sequence: u64, sender_time_ms: f64, opus: &[u8]) -> Option<RtpMapping> {
        let mapping = self.rtp_mapper.map(sequence, sender_time_ms, opus);
        match mapping {
            Some(mapping) if mapping.stream_reset => log::info!(
                "Audio sequence restarted for peer {} (epoch {})",
                self.peer_id,
                mapping.stream_epoch
            ),
            None => log::debug!(
                "Dropping audio seq {sequence} from before a sender restart (peer {})",
                self.peer_id
            ),
            _ => {}
        }
        mapping
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
}

impl Drop for NetEqAudioPeerDecoder {
    fn drop(&mut self) {
        // Peer left or was reset: stop the worker and take the worklet out of
        // the audio graph so it stops rendering and can be collected.
        self.link.worker.set_onmessage(None);
        self.link.worker.terminate();
        let mut playback = self.playback.borrow_mut();
        playback.closed = true;
        if let Some(node) = playback.node.take() {
            let close = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&close, &"command".into(), &"close".into());
            if let Ok(port) = node.port() {
                let _ = port.post_message(&close);
            }
            let _ = node.disconnect();
        }
        if let Some(gain) = playback.gain.take() {
            let _ = gain.disconnect();
        }
    }
}

impl crate::decode::AudioPeerDecoderTrait for NetEqAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        let Some(audio_meta) = packet.audio_metadata.as_ref() else {
            // Malformed/old packet that lacks metadata – skip with a warning instead of
            // propagating an error that would reset the entire peer.
            log::warn!(
                "Received audio packet with length {} without metadata – skipping",
                packet.data.len()
            );
            return Ok(DecodeStatus::SKIPPED);
        };
        // Playback is pulled by the worklet, so nothing plays (and nothing
        // else would retry) while the context is suspended. Packets arriving
        // is the cue to try again, as the old per-frame push path did.
        self.resume_if_suspended();

        let seq = audio_meta.sequence;
        // `MediaPacket.timestamp` is the sender's wall clock in ms. NetEq needs
        // 48 kHz RTP timestamps; the mapper derives them (see rtp_timestamp.rs).
        let sender_time_ms = packet.timestamp;

        // Track this sequence number so we can detect duplicates from
        // redundancy payloads later.
        self.record_sequence(seq);

        let red = (audio_meta.audio_format == AUDIO_RED_FORMAT)
            .then(|| Self::unpack_red_audio(&packet.data))
            .flatten();
        if audio_meta.audio_format == AUDIO_RED_FORMAT && red.is_none() {
            log::warn!(
                "RED unpack failed for peer {} seq {}, falling back to raw",
                self.peer_id,
                seq
            );
        }

        if let Some((primary, redundant_seq, redundant_data)) = red {
            // [4-byte primary_len LE][primary_data][4-byte redundant_seq LE][redundant_data]
            let Some(primary_mapping) = self.map(seq, sender_time_ms, &primary) else {
                return Ok(DecodeStatus::SKIPPED);
            };
            // If the redundant frame was lost (not yet received), inject it
            // at its own slot on the timeline, one frame before the primary.
            if !self.has_sequence(u64::from(redundant_seq)) {
                if let Some(recovered) = self.rtp_mapper.map_red_recovered(seq, redundant_seq) {
                    log::debug!(
                        "RED recovery: injecting lost audio seq {} for peer {}",
                        redundant_seq,
                        self.peer_id
                    );
                    self.record_sequence(u64::from(redundant_seq));
                    self.insert(recovered, redundant_data);
                }
            }
            self.insert(primary_mapping, primary);
        } else {
            let Some(mapping) = self.map(seq, sender_time_ms, &packet.data) else {
                return Ok(DecodeStatus::SKIPPED);
            };
            self.insert(mapping, packet.data.clone());
        }

        let first_frame = !self.decoded;
        self.decoded = true;
        Ok(DecodeStatus {
            rendered: true,
            first_frame,
        })
    }

    fn flush(&mut self) {
        // Drops NetEq's buffered packets and audio; the next packet starts a
        // fresh stream. The mapper keeps its timeline so the gap shows up as
        // a timestamp jump.
        self.send(WorkerMsg::Flush);
        log::debug!(
            "Sent flush message to NetEq worker for peer {}",
            self.peer_id
        );
    }

    fn set_muted(&mut self, muted: bool) {
        // Mute is idempotent and repeats at heartbeat rate; a repeated state
        // is not worth a message (and the worker would only log it as
        // redundant).
        if self.last_mute_sent.get() == Some(muted) {
            return;
        }
        self.last_mute_sent.set(Some(muted));
        log::debug!(
            "Peer {} set_muted({muted}), worker ready: {}",
            self.peer_id,
            self.link.ready.get()
        );
        self.send(WorkerMsg::Mute { muted });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

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
}
