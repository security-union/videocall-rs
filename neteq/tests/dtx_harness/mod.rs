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

//! Deterministic end-to-end harness for NetEq with and without Opus DTX.
//!
//! One pipeline, all of it seeded and run in virtual time:
//!
//! 1. Source: the fixture clip (`tests/fixtures/jfk_rice_moon_speech_20s.wav`)
//!    played unedited, or a script of speech and quiet room-noise segments.
//!    Ground truth per 20 ms frame: voiced or silent.
//! 2. Sender: the pure-Rust Opus encoder (`ropus`), 20 ms frames, VoIP, DTX
//!    on or off, and a transmit policy (`SenderMode`) and timestamp
//!    convention (`Timestamps`).
//! 3. Network: per-packet delay (`Delay`) and loss, from a hash of the seed
//!    and the source frame index, so runs that send different packets still
//!    see the same network for the same frame.
//! 4. Receiver: NetEq with the simulated clock (`neteq::clock`). The clock is
//!    advanced to each packet's arrival before `insert_packet`, and one 10 ms
//!    frame is pulled every 10 ms of virtual time. No sleeping, no wall clock.
//!    An ideal receiver (the transmitted packets decoded in order by a
//!    standalone decoder) gives the reference signal.
//! 5. Report: measured metrics, one printed line per scenario, and WAV files
//!    under `<target dir>/neteq-dtx-harness/<scenario>/`, written before any
//!    assertion runs.
//!
//! Only public API that exists on both `main` and the DTX integration branch
//! is used, so this file compiles unchanged on both.

#![allow(dead_code)]

use neteq::codec::{AudioDecoder, NativeOpusDecoder};
use neteq::neteq::SpeechType;
use neteq::{AudioPacket, Mode, NetEq, NetEqConfig, Operation, RtpHeader};
use ropus::{Application, Bitrate, Channels, Encoder};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const RATE: u32 = 48_000;
pub const PT: u8 = 111;
pub const FRAME_MS: u64 = 20;
pub const FRAME: usize = 960;
pub const TICK_MS: u64 = 10;
pub const TICK: usize = 480;
/// One-way propagation delay every packet sees on top of the delay model.
pub const BASE_NET_DELAY_MS: u64 = 20;
/// Output before this is "startup" and reported on its own.
pub const STARTUP_MS: u64 = 1000;
/// Deep speech/silence metrics start here: the delay manager's history
/// window (`DelayConfig::max_history_ms`, 2000 ms) has filled once.
pub const CONVERGED_MS: u64 = 2000;
/// Quiet room noise for scripted silences: -60 dBFS RMS.
pub const ROOM_NOISE_RMS: f32 = 0.001;
/// A frame whose RMS is below -80 dBFS is digital silence: no microphone
/// room noise is that quiet.
pub const DIGITAL_SILENCE_RMS: f64 = 1e-4;

const SSRC: u32 = 0x5EED_0001;
const SEQ_BASE: u16 = 20_000;
const TS_BASE: u32 = 100_000_000;

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Segment {
    /// Continue through the clip for this many ms.
    Speech(u32),
    /// Quiet room noise (about -60 dBFS) for this many ms.
    Silence(u32),
    /// Microphone muted for this many ms: the sender encodes and sends
    /// nothing, and its RTP timestamps skip the gap. The source is digital
    /// silence there.
    Mute(u32),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Script {
    /// The clip, unedited.
    Natural,
    Edited(Vec<Segment>),
}

pub struct Source {
    pub pcm: Vec<f32>,
    /// Ground truth per 20 ms frame.
    pub voiced: Vec<bool>,
    /// Voiced runs of at least 300 ms, as frame ranges `[start, end)`.
    pub talkspurts: Vec<(usize, usize)>,
    /// Per 20 ms frame: the sender was muted and sent nothing.
    pub muted: Vec<bool>,
    pub threshold_dbfs: f64,
}

impl Source {
    pub fn frames(&self) -> usize {
        self.voiced.len()
    }
    pub fn duration_ms(&self) -> u64 {
        self.frames() as u64 * FRAME_MS
    }
}

fn clip() -> &'static [f32] {
    static CLIP: OnceLock<Vec<f32>> = OnceLock::new();
    CLIP.get_or_init(|| {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/jfk_rice_moon_speech_20s.wav"
        );
        let mut reader = hound::WavReader::open(path).expect("fixture clip");
        let spec = reader.spec();
        assert_eq!(
            (spec.sample_rate, spec.channels, spec.bits_per_sample),
            (RATE, 1, 16),
            "fixture must be 48 kHz mono s16"
        );
        let mut pcm: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| f32::from(s.expect("fixture sample")) / 32_768.0)
            .collect();
        pcm.truncate(pcm.len() / FRAME * FRAME);
        pcm
    })
}

fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt()
}

pub fn dbfs(rms: f64) -> f64 {
    20.0 * rms.max(1e-10).log10()
}

fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Voice threshold: 6 dB above the clip's own noise floor (its 10th
/// percentile 20 ms level), i.e. at least twice the floor's amplitude.
fn voice_threshold_dbfs() -> f64 {
    let levels: Vec<f64> = clip().chunks(FRAME).map(|f| dbfs(rms(f))).collect();
    percentile(&levels, 0.10) + 6.0
}

fn build_source(script: &Script) -> Source {
    let clip = clip();
    let mut pcm = Vec::new();
    let mut muted = Vec::new();
    match script {
        Script::Natural => pcm.extend_from_slice(clip),
        Script::Edited(segments) => {
            let mut cursor = 0usize;
            let mut noise = XorShift(0x2545_F491);
            let amplitude = ROOM_NOISE_RMS * 3f32.sqrt(); // uniform noise of that RMS
            for segment in segments {
                match *segment {
                    Segment::Speech(ms) => {
                        for _ in 0..(ms as usize * RATE as usize / 1000) {
                            pcm.push(clip[cursor % clip.len()]);
                            cursor += 1;
                        }
                    }
                    Segment::Silence(ms) => {
                        for _ in 0..(ms as usize * RATE as usize / 1000) {
                            pcm.push(noise.symmetric() * amplitude);
                        }
                    }
                    Segment::Mute(ms) => {
                        let at = pcm.len() / FRAME;
                        pcm.resize(pcm.len() + ms as usize * RATE as usize / 1000, 0.0);
                        muted.resize(at, false);
                        muted.resize(pcm.len() / FRAME, true);
                    }
                }
            }
            pcm.truncate(pcm.len() / FRAME * FRAME);
        }
    }
    let threshold_dbfs = voice_threshold_dbfs();
    let mut voiced: Vec<bool> = pcm
        .chunks(FRAME)
        .map(|f| dbfs(rms(f)) > threshold_dbfs)
        .collect();
    // Bridge gaps shorter than 200 ms between words, then drop voiced blips
    // shorter than 60 ms: a talkspurt is a phrase, not a syllable.
    fill_runs(&mut voiced, false, 10);
    fill_runs(&mut voiced, true, 3);
    let talkspurts = runs(&voiced, true)
        .into_iter()
        .filter(|(s, e)| e - s >= 15)
        .collect();
    muted.resize(voiced.len(), false);
    Source {
        pcm,
        voiced,
        talkspurts,
        muted,
        threshold_dbfs,
    }
}

/// Runs of `value` as `[start, end)`.
fn runs(flags: &[bool], value: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < flags.len() {
        if flags[i] == value {
            let start = i;
            while i < flags.len() && flags[i] == value {
                i += 1;
            }
            out.push((start, i));
        } else {
            i += 1;
        }
    }
    out
}

/// Flip interior runs of `value` shorter than `min_len` frames.
fn fill_runs(flags: &mut [bool], value: bool, min_len: usize) {
    for (start, end) in runs(flags, value) {
        let interior = start > 0 && end < flags.len();
        if interior && end - start < min_len {
            flags[start..end].iter_mut().for_each(|f| *f = !value);
        }
    }
}

struct XorShift(u32);

impl XorShift {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
    /// Uniform in [-1, 1).
    fn symmetric(&mut self) -> f32 {
        (self.next() >> 8) as f32 / 8_388_608.0 - 1.0
    }
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Per-frame random value: same seed and frame give the same value in every
/// run, whatever else was sent.
fn frame_random(seed: u64, frame: usize, salt: u64) -> u64 {
    splitmix64(seed ^ splitmix64(frame as u64 ^ salt.rotate_left(32)))
}

// ---------------------------------------------------------------------------
// Caches (encoding 20 s of audio is the slow part of a run)
// ---------------------------------------------------------------------------

type Memo<K, V> = Mutex<HashMap<K, Arc<OnceLock<Arc<V>>>>>;

fn memo<K: Eq + Hash + Clone, V>(cache: &Memo<K, V>, key: &K, make: impl FnOnce() -> V) -> Arc<V> {
    let slot = cache
        .lock()
        .unwrap()
        .entry(key.clone())
        .or_default()
        .clone();
    slot.get_or_init(|| Arc::new(make())).clone()
}

pub fn source(script: &Script) -> Arc<Source> {
    static CACHE: OnceLock<Memo<Script, Source>> = OnceLock::new();
    memo(CACHE.get_or_init(Default::default), script, || {
        build_source(script)
    })
}

type EncodingCache = Memo<(Script, bool), Vec<Vec<u8>>>;

/// One Opus payload per 20 ms source frame.
pub fn encoded(script: &Script, dtx: bool) -> Arc<Vec<Vec<u8>>> {
    static CACHE: OnceLock<EncodingCache> = OnceLock::new();
    memo(
        CACHE.get_or_init(Default::default),
        &(script.clone(), dtx),
        || {
            let source = source(script);
            let mut encoder = Encoder::builder(RATE, Channels::Mono, Application::Voip)
                .dtx(dtx)
                .bitrate(Bitrate::Bits(32_000))
                .build()
                .expect("opus encoder");
            source
                .pcm
                .chunks(FRAME)
                .zip(&source.muted)
                .map(|(frame, &muted)| {
                    if muted {
                        return Vec::new(); // never encoded, never sent
                    }
                    let pcm: Vec<i16> = frame
                        .iter()
                        .map(|&x| (x.clamp(-1.0, 1.0) * 32_767.0).round() as i16)
                        .collect();
                    let mut buf = [0u8; 1500];
                    let len = encoder.encode(&pcm, &mut buf).expect("opus encode");
                    buf[..len].to_vec()
                })
                .collect()
        },
    )
}

/// An Opus DTX frame: a payload of at most 2 bytes carries no audio.
pub fn is_dtx_payload(payload: &[u8]) -> bool {
    payload.len() <= 2
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderMode {
    /// DTX off: every frame is sent.
    Continuous,
    /// DTX on; every non-DTX frame plus the first DTX frame after one (what
    /// libwebrtc transmits).
    DtxWebrtc,
    /// DTX on; DTX frames are never sent (allowed by RFC 7587).
    DtxDropAll,
}

impl SenderMode {
    pub fn dtx(self) -> bool {
        self != SenderMode::Continuous
    }
    pub fn label(self) -> &'static str {
        match self {
            SenderMode::Continuous => "continuous",
            SenderMode::DtxWebrtc => "dtx_webrtc",
            SenderMode::DtxDropAll => "dtx_dropall",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timestamps {
    /// 48 kHz sample units.
    Rtp,
    /// Every packet `u32::MAX` (the browser client today: ms since 1970
    /// cast `as u32`).
    Saturated,
}

impl Timestamps {
    pub fn label(self) -> &'static str {
        match self {
            Timestamps::Rtp => "rtp",
            Timestamps::Saturated => "saturated",
        }
    }
}

/// The sender restarts at `at_frame`: from there its sequence numbers and
/// RTP timestamps continue from new bases (RFC 3550 picks both at random),
/// with no gap in the audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restart {
    pub at_frame: usize,
    pub seq_base: u16,
    pub ts_base: u32,
}

#[derive(Clone, Debug)]
pub struct SentPacket {
    pub frame: usize,
    pub send_ms: u64,
    pub seq: u16,
    pub ts: u32,
    pub payload: Vec<u8>,
}

fn send(
    encoded: &[Vec<u8>],
    muted: &[bool],
    mode: SenderMode,
    timestamps: Timestamps,
    restart: Option<Restart>,
) -> Vec<SentPacket> {
    let mut out = Vec::new();
    let mut seq = SEQ_BASE;
    let mut ts_base = TS_BASE;
    let mut ts_frame0 = 0usize;
    let mut previous_dtx = false;
    for (frame, payload) in encoded.iter().enumerate() {
        if let Some(r) = restart.filter(|r| r.at_frame == frame) {
            seq = r.seq_base;
            ts_base = r.ts_base;
            ts_frame0 = frame;
        }
        if muted[frame] {
            continue; // muted: nothing is sent and no sequence number is used
        }
        let dtx = is_dtx_payload(payload);
        let transmit = match mode {
            SenderMode::Continuous => true,
            SenderMode::DtxWebrtc => !dtx || !previous_dtx,
            SenderMode::DtxDropAll => !dtx,
        };
        previous_dtx = dtx;
        if !transmit {
            continue;
        }
        let ts = match timestamps {
            Timestamps::Rtp => ts_base.wrapping_add(((frame - ts_frame0) * FRAME) as u32),
            Timestamps::Saturated => u32::MAX,
        };
        out.push(SentPacket {
            frame,
            send_ms: frame as u64 * FRAME_MS,
            seq,
            ts,
            payload: payload.clone(),
        });
        seq = seq.wrapping_add(1); // only transmitted packets use a number
    }
    out
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delay {
    Clean,
    /// Uniform extra delay in `[0, max_ms]` (which also reorders).
    Jitter {
        max_ms: u64,
    },
    /// Packets are held and released together at the end of each
    /// `every_n` × 20 ms window plus the rest of `hold_ms`.
    Bursty {
        every_n: u64,
        hold_ms: u64,
    },
    /// One packet in `every_n` (by source frame) is `extra_ms` late; the
    /// rest are on time, so the late one arrives after its successors.
    Straggler {
        every_n: u64,
        extra_ms: u64,
    },
}

/// A duplicated packet's copy arrives this long after the original: late
/// enough that the original has usually been played already.
pub const DUPLICATE_LAG_MS: u64 = 40;

#[derive(Clone, Copy, Debug)]
pub struct Network {
    pub delay: Delay,
    pub loss_percent: u64,
    /// Share of delivered packets that arrive twice.
    pub duplicate_percent: u64,
    pub seed: u64,
}

impl Network {
    pub fn clean() -> Self {
        Self {
            delay: Delay::Clean,
            loss_percent: 0,
            duplicate_percent: 0,
            seed: 0xC0FF_EE00,
        }
    }
    pub fn jitter(max_ms: u64) -> Self {
        Self {
            delay: Delay::Jitter { max_ms },
            ..Self::clean()
        }
    }
    pub fn bursty(every_n: u64, hold_ms: u64) -> Self {
        assert!(
            hold_ms + FRAME_MS >= every_n * FRAME_MS,
            "hold covers window"
        );
        Self {
            delay: Delay::Bursty { every_n, hold_ms },
            ..Self::clean()
        }
    }
    pub fn straggler(every_n: u64, extra_ms: u64) -> Self {
        Self {
            delay: Delay::Straggler { every_n, extra_ms },
            ..Self::clean()
        }
    }
    pub fn with_duplicates(self, duplicate_percent: u64) -> Self {
        Self {
            duplicate_percent,
            ..self
        }
    }
    pub fn with_loss(self, loss_percent: u64) -> Self {
        Self {
            loss_percent,
            ..self
        }
    }
    /// Smallest and largest one-way delay a packet can see.
    pub fn delay_range_ms(&self) -> (u64, u64) {
        let extra = match self.delay {
            Delay::Clean => 0,
            Delay::Jitter { max_ms } => max_ms,
            Delay::Bursty { hold_ms, .. } => hold_ms,
            Delay::Straggler { extra_ms, .. } => extra_ms,
        };
        let dup = if self.duplicate_percent > 0 {
            DUPLICATE_LAG_MS
        } else {
            0
        };
        (BASE_NET_DELAY_MS, BASE_NET_DELAY_MS + extra + dup)
    }
    pub fn label(&self) -> String {
        let delay = match self.delay {
            Delay::Clean => "clean".to_string(),
            Delay::Jitter { max_ms } => format!("jitter{max_ms}"),
            Delay::Bursty { every_n, hold_ms } => format!("bursty{every_n}x{hold_ms}"),
            Delay::Straggler { every_n, extra_ms } => format!("straggler{every_n}x{extra_ms}"),
        };
        let mut label = delay;
        if self.loss_percent > 0 {
            label = format!("{label}_loss{}", self.loss_percent);
        }
        if self.duplicate_percent > 0 {
            label = format!("{label}_dup{}", self.duplicate_percent);
        }
        label
    }

    /// Arrival time per delivered packet (index into `sent`), sorted by
    /// arrival, and the frames lost.
    fn deliver(&self, sent: &[SentPacket]) -> (Vec<(u64, usize)>, Vec<usize>) {
        let mut arrivals = Vec::new();
        let mut lost = Vec::new();
        for (index, packet) in sent.iter().enumerate() {
            if frame_random(self.seed, packet.frame, 2) % 100 < self.loss_percent {
                lost.push(packet.frame);
                continue;
            }
            let extra = match self.delay {
                Delay::Clean => 0,
                Delay::Jitter { max_ms } => frame_random(self.seed, packet.frame, 1) % (max_ms + 1),
                Delay::Bursty { every_n, hold_ms } => {
                    let window = every_n * FRAME_MS;
                    (packet.send_ms / window) * window + hold_ms - packet.send_ms
                }
                Delay::Straggler { every_n, extra_ms } => {
                    if (packet.frame as u64).checked_rem(every_n) == Some(0) {
                        extra_ms
                    } else {
                        0
                    }
                }
            };
            let at = packet.send_ms + BASE_NET_DELAY_MS + extra;
            arrivals.push((at, index));
            if frame_random(self.seed, packet.frame, 3) % 100 < self.duplicate_percent {
                arrivals.push((at + DUPLICATE_LAG_MS, index));
            }
        }
        arrivals.sort();
        (arrivals, lost)
    }
}

// ---------------------------------------------------------------------------
// Scenario and receiver
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    pub script: Script,
    pub sender: SenderMode,
    pub timestamps: Timestamps,
    pub network: Network,
    /// Virtual time played after the last source frame.
    pub tail_ms: u64,
    pub restart: Option<Restart>,
    pub receiver_mute: Option<ReceiverMute>,
}

/// The browser worker's mute path (`neteq_worker.rs`): at `from_ms` the
/// peer's heartbeat says audio off, so the client sends Flush and Mute;
/// from then on the worker pulls every 10 ms and discards the audio
/// (`get_audio_muted`). The un-mute message arrives `unmute_lag_ms` after
/// the first audio packet that follows; the worker un-mutes earlier, on
/// that first insert, because a packet reaching it means the peer's mic
/// is on.
///
/// `stale_mute_at_ms`: a second Mute{true} on its own (no Flush) that
/// reaches the worker after speech has resumed; the worker discards pulls
/// again until the next insert.
#[derive(Clone, Copy, Debug)]
pub struct ReceiverMute {
    pub from_ms: u64,
    pub unmute_lag_ms: u64,
    pub stale_mute_at_ms: Option<u64>,
}

impl Scenario {
    /// Name derived from the parameters, so it is the same on every branch.
    pub fn natural(sender: SenderMode, timestamps: Timestamps, network: Network) -> Self {
        Self {
            name: format!(
                "natural_{}_{}_{}",
                sender.label(),
                timestamps.label(),
                network.label()
            ),
            script: Script::Natural,
            sender,
            timestamps,
            network,
            tail_ms: 1000,
            restart: None,
            receiver_mute: None,
        }
    }
    /// A scripted source; `name` must say what the script is.
    pub fn scripted(name: &str, script: Script, sender: SenderMode, network: Network) -> Self {
        Self {
            name: name.to_string(),
            script,
            sender,
            timestamps: Timestamps::Rtp,
            network,
            tail_ms: 1000,
            restart: None,
            receiver_mute: None,
        }
    }
    pub fn named(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }
    pub fn with_restart(mut self, restart: Restart) -> Self {
        self.restart = Some(restart);
        self
    }
    pub fn with_receiver_mute(mut self, mute: ReceiverMute) -> Self {
        self.receiver_mute = Some(mute);
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tick {
    pub t_ms: u64,
    pub speech: SpeechType,
    pub buffer_ms: u32,
    pub target_ms: u32,
    pub operation: Operation,
    pub mode: Mode,
    /// The worker was muted and discarded this pull (`ReceiverMute`);
    /// marked Expand, nothing was played.
    pub paused: bool,
}

pub struct Run {
    pub scenario: Scenario,
    pub source: Arc<Source>,
    pub encoded_dtx_frames: usize,
    pub sent: Vec<SentPacket>,
    pub sent_dtx: usize,
    pub lost_frames: Vec<usize>,
    pub last_arrival_ms: u64,
    pub out: Vec<f32>,
    pub ticks: Vec<Tick>,
    pub reference: Vec<f32>,
    pub insert_errors: Vec<String>,
    pub get_audio_errors: Vec<String>,
    pub frame_len_errors: usize,
    pub concealed_samples: u64,
    pub out_dir: PathBuf,
    pub report: Report,
    /// Lifetime samples removed by accelerate and inserted by preemptive
    /// expand.
    pub stretch_samples: (u64, u64),
    /// Concealed samples counted while the worker was muted (all mutes).
    pub muted_concealment: u64,
    /// When the un-mute message arrived (`ReceiverMute`).
    pub unmute_signal_ms: Option<u64>,
}

/// Run a scenario end to end, write its audio and metrics, print its line.
pub fn run(scenario: &Scenario) -> Run {
    let source = source(&scenario.script);
    let encoded = encoded(&scenario.script, scenario.sender.dtx());
    let sent = send(
        &encoded,
        &source.muted,
        scenario.sender,
        scenario.timestamps,
        scenario.restart,
    );
    let (arrivals, lost_frames) = scenario.network.deliver(&sent);

    neteq::clock::freeze();
    let mut neteq = NetEq::new(NetEqConfig {
        sample_rate: RATE,
        channels: 1,
        ..NetEqConfig::default()
    })
    .expect("neteq");
    neteq.register_decoder(
        PT,
        Box::new(NativeOpusDecoder::new(RATE, 1).expect("opus decoder")),
    );

    let total_ms = source.duration_ms() + scenario.tail_ms;
    let mut now_ms = 0u64;
    fn advance_to(t: u64, now_ms: &mut u64) {
        neteq::clock::advance(Duration::from_millis(t - *now_ms));
        *now_ms = t;
    }
    let mut next = 0;
    let mut out = Vec::with_capacity(total_ms as usize * 48);
    let mut ticks = Vec::new();
    let mut insert_errors = Vec::new();
    let mut get_audio_errors = Vec::new();
    let mut frame_len_errors = 0;
    let mut muted = false;
    let mut mute_done = false;
    let mut stale_done = false;
    let mut unmute_signal_ms: Option<u64> = None;
    let mut concealed_at_mute = 0;
    let mut muted_concealment = 0;
    for tick in 0..total_ms / TICK_MS {
        let t = tick * TICK_MS;
        if let Some(mute) = scenario.receiver_mute {
            if !mute_done && t >= mute.from_ms {
                // Flush + Mute, in the client's order.
                mute_done = true;
                muted = true;
                neteq.flush();
                concealed_at_mute = neteq.get_statistics().lifetime.concealed_samples;
            }
            if let Some(at) = mute.stale_mute_at_ms {
                if mute_done && !muted && !stale_done && t >= at {
                    stale_done = true;
                    muted = true;
                    concealed_at_mute = neteq.get_statistics().lifetime.concealed_samples;
                }
            }
        }
        while next < arrivals.len() && arrivals[next].0 <= t {
            let (at, index) = arrivals[next];
            advance_to(at, &mut now_ms);
            if let (Some(mute), true) = (scenario.receiver_mute, muted) {
                // The first insert after the mute un-mutes the worker; the
                // un-mute message itself only arrives later.
                muted = false;
                muted_concealment +=
                    neteq.get_statistics().lifetime.concealed_samples - concealed_at_mute;
                if unmute_signal_ms.is_none() {
                    unmute_signal_ms = Some(at + mute.unmute_lag_ms);
                }
            }
            let packet = &sent[index];
            let header = RtpHeader::new(packet.seq, packet.ts, SSRC, PT, false);
            let audio_packet = AudioPacket::new(header, packet.payload.clone(), RATE, 1, 20);
            if let Err(e) = neteq.insert_packet(audio_packet) {
                insert_errors.push(format!("t={at} seq={}: {e}", packet.seq));
            }
            next += 1;
        }
        advance_to(t, &mut now_ms);
        if muted {
            if let Err(e) = neteq.get_audio_muted() {
                get_audio_errors.push(format!("t={t} (muted): {e}"));
            }
            {
                out.extend_from_slice(&[0.0; TICK]);
                ticks.push(Tick {
                    t_ms: t,
                    speech: SpeechType::Expand,
                    buffer_ms: neteq.current_buffer_size_ms(),
                    target_ms: neteq.target_delay_ms(),
                    operation: neteq.last_operation(),
                    mode: neteq.last_mode(),
                    paused: true,
                });
                continue;
            }
        }
        let (samples, speech) = match neteq.get_audio() {
            Ok(frame) => (frame.samples, frame.speech_type),
            Err(e) => {
                get_audio_errors.push(format!("t={t}: {e}"));
                (vec![0.0; TICK], SpeechType::Expand)
            }
        };
        if samples.len() != TICK {
            frame_len_errors += 1;
        }
        let mut samples = samples;
        samples.resize(TICK, 0.0);
        out.extend_from_slice(&samples);
        ticks.push(Tick {
            t_ms: t,
            speech,
            buffer_ms: neteq.current_buffer_size_ms(),
            target_ms: neteq.target_delay_ms(),
            operation: neteq.last_operation(),
            mode: neteq.last_mode(),
            paused: false,
        });
    }
    let lifetime = neteq.get_statistics().lifetime;
    let concealed_samples = lifetime.concealed_samples;
    let stretch_samples = (
        lifetime.removed_samples_for_acceleration,
        lifetime.inserted_samples_for_deceleration,
    );

    // Ideal receiver: every transmitted packet, decoded in order, no jitter
    // buffer, placed at its source frame.
    let mut reference = vec![0.0f32; source.pcm.len()];
    let mut decoder = NativeOpusDecoder::new(RATE, 1).expect("opus decoder");
    for packet in &sent {
        if let Ok(pcm) = decoder.decode(&packet.payload) {
            let n = pcm.len().min(FRAME);
            let at = packet.frame * FRAME;
            reference[at..at + n].copy_from_slice(&pcm[..n]);
        }
    }

    let mut run = Run {
        scenario: scenario.clone(),
        encoded_dtx_frames: encoded
            .iter()
            .zip(&source.muted)
            .filter(|(p, &m)| !m && is_dtx_payload(p))
            .count(),
        sent_dtx: sent.iter().filter(|p| is_dtx_payload(&p.payload)).count(),
        last_arrival_ms: arrivals.last().map_or(0, |a| a.0),
        source,
        sent,
        lost_frames,
        out,
        ticks,
        reference,
        insert_errors,
        get_audio_errors,
        frame_len_errors,
        concealed_samples,
        stretch_samples,
        muted_concealment,
        unmute_signal_ms,
        out_dir: output_root().join(&scenario.name),
        report: Report::default(),
    };
    run.report = analyze(&run);
    let line = run.report.line(&scenario.name);
    println!("{line}");
    write_outputs(&run, &line).expect("writing scenario audio");
    run
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deep {
    Speech,
    Silence,
}

#[derive(Clone, Debug, Default)]
pub struct Talkspurt {
    pub start_ms: u64,
    pub end_ms: u64,
    /// End-to-end delay from envelope alignment; `None` if not found.
    pub delay_ms: Option<u64>,
    pub correlation: f64,
    /// Output energy over reference energy, whole talkspurt.
    pub energy_ratio: f64,
    /// Same, first 200 ms.
    pub onset_ratio: f64,
    /// Concealment while this talkspurt plays, in deep speech.
    pub conceal_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub ticks: usize,
    pub expand_ms: u64,
    pub cng_ms: u64,
    pub normal_ms: u64,
    pub conceal_startup_ms: u64,
    /// Concealment from startup to the end of the source.
    pub conceal_after_startup_ms: u64,
    pub deep_speech_ms: u64,
    pub conceal_speech_ms: u64,
    pub deep_silence_ms: u64,
    pub conceal_silence_ms: u64,
    pub cng_silence_ms: u64,
    pub silence_out_dbfs: f64,
    pub silence_src_dbfs: f64,
    /// 5th to 95th percentile of the 100 ms output level relative to the
    /// source over deep silence (pumping).
    pub silence_spread_db: f64,
    pub silence_windows: usize,
    pub digital_silence_frames: usize,
    pub talkspurts: Vec<Talkspurt>,
    pub target_min: u32,
    pub target_median: u32,
    pub target_max: u32,
    pub final_buffer_ms: u32,
    pub non_finite: usize,
    pub max_abs: f32,
    pub reference_max_abs: f32,
    pub sent: usize,
    pub sent_dtx: usize,
    pub encoded_dtx: usize,
    pub lost: usize,
    /// Ticks whose operation was accelerate, by result.
    pub accelerate: StretchCount,
    /// Ticks whose operation was preemptive expand, by result.
    pub preemptive: StretchCount,
    /// Lifetime samples removed by accelerate.
    pub removed_samples: u64,
    /// Lifetime samples inserted by preemptive expand.
    pub inserted_samples: u64,
}

/// Results of the ticks that ran one time-stretch operation.
#[derive(Clone, Copy, Debug, Default)]
pub struct StretchCount {
    pub success: usize,
    pub low_energy: usize,
    pub fail: usize,
}

impl Report {
    pub fn line(&self, name: &str) -> String {
        let delays: Vec<String> = self
            .talkspurts
            .iter()
            .map(|t| t.delay_ms.map_or("-".into(), |d| d.to_string()))
            .collect();
        let min_of = |f: fn(&Talkspurt) -> f64| {
            self.talkspurts
                .iter()
                .filter(|t| t.start_ms >= STARTUP_MS)
                .map(f)
                .fold(f64::INFINITY, f64::min)
        };
        format!(
            "HARNESS {name} | conceal startup/after/speech/silence {}/{}/{}/{} ms of deep {}/{} | cng {} (silence {}) \
             | silence out {:.1} src {:.1} dBFS spread {:.1} dB digsil {} \
             | spurts {} delay [{}] onset_min {:.2} energy_min {:.2} \
             | target {}/{}/{} buf_end {} | sent {} dtx enc/sent {}/{} lost {} \
             | accel ok/low/fail {}/{}/{} removed {} | pe ok/low/fail {}/{}/{} inserted {}",
            self.conceal_startup_ms,
            self.conceal_after_startup_ms,
            self.conceal_speech_ms,
            self.conceal_silence_ms,
            self.deep_speech_ms,
            self.deep_silence_ms,
            self.cng_ms,
            self.cng_silence_ms,
            self.silence_out_dbfs,
            self.silence_src_dbfs,
            self.silence_spread_db,
            self.digital_silence_frames,
            self.talkspurts.len(),
            delays.join(","),
            min_of(|t| t.onset_ratio),
            min_of(|t| t.energy_ratio),
            self.target_min,
            self.target_median,
            self.target_max,
            self.final_buffer_ms,
            self.sent,
            self.encoded_dtx,
            self.sent_dtx,
            self.lost,
            self.accelerate.success,
            self.accelerate.low_energy,
            self.accelerate.fail,
            self.removed_samples,
            self.preemptive.success,
            self.preemptive.low_energy,
            self.preemptive.fail,
            self.inserted_samples,
        )
    }

    pub fn median_talkspurt_delay(&self) -> Option<u64> {
        let mut d: Vec<u64> = self.talkspurts.iter().filter_map(|t| t.delay_ms).collect();
        d.sort_unstable();
        d.get(d.len() / 2).copied()
    }
}

/// Which deep region (if any) the output tick at `t` plays. The source it
/// plays lies in `[t - d_hi, t + 10 - d_lo]`, where `d_lo` is the smallest
/// network delay and `d_hi` the largest network delay plus what is buffered
/// (with 25% for time-stretching and 50 ms for a packet, a tick and the
/// decode leftover). Frames whose window crosses a speech/silence boundary
/// are neither.
fn deep_kind(run: &Run, tick: &Tick) -> Option<Deep> {
    let (d_lo, d_hi_net) = run.scenario.network.delay_range_ms();
    let d_hi = d_hi_net + u64::from(tick.buffer_ms) * 5 / 4 + 50;
    let end = (tick.t_ms + TICK_MS).checked_sub(d_lo)?;
    let start = tick.t_ms.checked_sub(d_hi)?;
    if end > run.source.duration_ms() || end <= start {
        return None;
    }
    let first = (start / FRAME_MS) as usize;
    let last = ((end - 1) / FRAME_MS) as usize;
    let truth = &run.source.voiced[first..=last];
    if truth.iter().all(|&v| v) {
        Some(Deep::Speech)
    } else if truth.iter().all(|&v| !v) {
        Some(Deep::Silence)
    } else {
        None
    }
}

fn source_window(run: &Run, tick: &Tick) -> (usize, usize) {
    let (d_lo, d_hi_net) = run.scenario.network.delay_range_ms();
    let d_hi = d_hi_net + u64::from(tick.buffer_ms) * 5 / 4 + 50;
    let start = tick.t_ms.saturating_sub(d_hi) as usize * 48;
    let end = ((tick.t_ms + TICK_MS).saturating_sub(d_lo) as usize * 48).min(run.source.pcm.len());
    (start, end.max(start))
}

fn energy(samples: &[f32], from: i64, to: i64) -> f64 {
    let from = from.clamp(0, samples.len() as i64) as usize;
    let to = to.clamp(0, samples.len() as i64) as usize;
    samples[from..to.max(from)]
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum()
}

fn envelope(samples: &[f32]) -> Vec<f64> {
    samples.chunks(TICK).map(rms).collect()
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    if n < 2 {
        return 0.0;
    }
    let (a, b) = (&a[..n], &b[..n]);
    let ma = a.iter().sum::<f64>() / n as f64;
    let mb = b.iter().sum::<f64>() / n as f64;
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        ab += (a[i] - ma) * (b[i] - mb);
        aa += (a[i] - ma) * (a[i] - ma);
        bb += (b[i] - mb) * (b[i] - mb);
    }
    if aa <= 0.0 || bb <= 0.0 {
        0.0
    } else {
        ab / (aa * bb).sqrt()
    }
}

fn analyze(run: &Run) -> Report {
    let mut r = Report {
        ticks: run.ticks.len(),
        sent: run.sent.len(),
        sent_dtx: run.sent_dtx,
        encoded_dtx: run.encoded_dtx_frames,
        lost: run.lost_frames.len(),
        removed_samples: run.stretch_samples.0,
        inserted_samples: run.stretch_samples.1,
        ..Report::default()
    };
    for tick in &run.ticks {
        let count = match tick.operation {
            Operation::Accelerate | Operation::FastAccelerate => &mut r.accelerate,
            Operation::PreemptiveExpand => &mut r.preemptive,
            _ => continue,
        };
        match tick.mode {
            Mode::AccelerateSuccess | Mode::PreemptiveExpandSuccess => count.success += 1,
            Mode::AccelerateLowEnergy | Mode::PreemptiveExpandLowEnergy => count.low_energy += 1,
            _ => count.fail += 1,
        }
    }
    let source_ms = run.source.duration_ms();
    let deep: Vec<Option<Deep>> = run.ticks.iter().map(|t| deep_kind(run, t)).collect();
    let mut silence_out = 0.0;
    let mut silence_src = 0.0;
    let mut silence_n = 0usize;
    let mut window = (0.0, 0.0, 0usize);
    let mut window_levels = Vec::new();
    for (i, tick) in run.ticks.iter().enumerate() {
        let frame = &run.out[i * TICK..(i + 1) * TICK];
        let expand = tick.speech == SpeechType::Expand;
        let cng = tick.speech == SpeechType::Cng;
        r.expand_ms += if expand { TICK_MS } else { 0 };
        r.cng_ms += if cng { TICK_MS } else { 0 };
        r.normal_ms += if !cng && !expand { TICK_MS } else { 0 };
        if tick.t_ms < STARTUP_MS {
            r.conceal_startup_ms += if expand { TICK_MS } else { 0 };
            continue;
        }
        if tick.t_ms < source_ms && expand {
            r.conceal_after_startup_ms += TICK_MS;
        }
        if tick.t_ms < CONVERGED_MS {
            continue;
        }
        match deep[i] {
            Some(Deep::Speech) => {
                r.deep_speech_ms += TICK_MS;
                r.conceal_speech_ms += if expand { TICK_MS } else { 0 };
            }
            Some(Deep::Silence) => {
                r.deep_silence_ms += TICK_MS;
                r.conceal_silence_ms += if expand { TICK_MS } else { 0 };
                r.cng_silence_ms += if cng { TICK_MS } else { 0 };
                let out_ms = rms(frame).powi(2);
                let (s, e) = source_window(run, tick);
                let src_ms = rms(&run.source.pcm[s..e]).powi(2);
                silence_out += out_ms;
                silence_src += src_ms;
                silence_n += 1;
                if out_ms.sqrt() < DIGITAL_SILENCE_RMS {
                    r.digital_silence_frames += 1;
                }
                window = (window.0 + out_ms, window.1 + src_ms, window.2 + 1);
                if window.2 == 10 {
                    window_levels.push(dbfs(window.0.sqrt()) - dbfs(window.1.sqrt()));
                    window = (0.0, 0.0, 0);
                }
                continue;
            }
            None => {}
        }
        window = (0.0, 0.0, 0); // windows are consecutive deep-silence ticks
    }
    if silence_n > 0 {
        r.silence_out_dbfs = dbfs((silence_out / silence_n as f64).sqrt());
        r.silence_src_dbfs = dbfs((silence_src / silence_n as f64).sqrt());
    } else {
        r.silence_out_dbfs = f64::NAN;
        r.silence_src_dbfs = f64::NAN;
    }
    r.silence_windows = window_levels.len();
    r.silence_spread_db = if window_levels.len() >= 2 {
        percentile(&window_levels, 0.95) - percentile(&window_levels, 0.05)
    } else {
        0.0
    };

    // Talkspurts: align the 10 ms envelopes. The lag can be at most what is
    // physically possible: the largest network delay plus the most that was
    // buffered while the talkspurt played (same bound as `deep_kind`), so a
    // short talkspurt cannot lock onto a similar-looking later one.
    let ref_env = envelope(&run.reference);
    let out_env = envelope(&run.out);
    let (_, net_max) = run.scenario.network.delay_range_ms();
    for &(fs, fe) in &run.source.talkspurts {
        let (s, e) = (fs * 2, fe * 2); // in ticks
        let reference = &ref_env[s..e];
        let buffered = run.ticks[s.min(run.ticks.len())..(e + 150).min(run.ticks.len())]
            .iter()
            .map(|t| u64::from(t.buffer_ms))
            .max()
            .unwrap_or(0);
        let max_lag = ((net_max + buffered * 5 / 4 + 50) / TICK_MS) as usize;
        let mut best = (0usize, f64::MIN);
        for lag in 0..=max_lag {
            if s + lag >= out_env.len() {
                break;
            }
            let end = (e + lag).min(out_env.len());
            let c = pearson(reference, &out_env[s + lag..end]);
            if c > best.1 + 1e-12 {
                best = (lag, c);
            }
        }
        let (lag, correlation) = best;
        let delay_ms = (correlation >= 0.5).then_some(lag as u64 * TICK_MS);
        let sample = |tick: usize| (tick * TICK) as i64;
        let l = sample(lag);
        let margin = sample(5); // 50 ms: alignment drift from time-stretching
        let energy_ratio = energy(&run.out, sample(s) + l - margin, sample(e) + l + margin)
            / energy(&run.reference, sample(s) - margin, sample(e) + margin).max(1e-12);
        let onset_ratio = energy(&run.out, sample(s) + l - sample(2), sample(s + 20) + l)
            / energy(&run.reference, sample(s) - sample(2), sample(s + 20)).max(1e-12);
        let conceal_ms = (s + lag..(e + lag).min(run.ticks.len()))
            .filter(|&i| {
                // A discarded (muted) pull is not concealment.
                run.ticks[i].speech == SpeechType::Expand
                    && !run.ticks[i].paused
                    && deep[i] == Some(Deep::Speech)
                    && run.ticks[i].t_ms >= STARTUP_MS
            })
            .count() as u64
            * TICK_MS;
        r.talkspurts.push(Talkspurt {
            start_ms: fs as u64 * FRAME_MS,
            end_ms: fe as u64 * FRAME_MS,
            delay_ms,
            correlation,
            energy_ratio,
            onset_ratio,
            conceal_ms,
        });
    }

    let mut targets: Vec<u32> = run
        .ticks
        .iter()
        .filter(|t| t.t_ms >= STARTUP_MS && t.t_ms < source_ms)
        .map(|t| t.target_ms)
        .collect();
    targets.sort_unstable();
    if !targets.is_empty() {
        r.target_min = targets[0];
        r.target_median = targets[targets.len() / 2];
        r.target_max = targets[targets.len() - 1];
    }
    r.final_buffer_ms = run.ticks.last().map_or(0, |t| t.buffer_ms);
    r.non_finite = run.out.iter().filter(|x| !x.is_finite()).count();
    r.max_abs = run.out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    r.reference_max_abs = run.reference.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    r
}

/// Median target delay over `[from_ms, to_ms)`.
pub fn median_target(run: &Run, from_ms: u64, to_ms: u64) -> u32 {
    let mut t: Vec<u32> = run
        .ticks
        .iter()
        .filter(|t| t.t_ms >= from_ms && t.t_ms < to_ms)
        .map(|t| t.target_ms)
        .collect();
    t.sort_unstable();
    t.get(t.len() / 2).copied().unwrap_or(0)
}

/// Median buffer level over `[from_ms, to_ms)`.
pub fn median_buffer(run: &Run, from_ms: u64, to_ms: u64) -> u32 {
    let mut b: Vec<u32> = run
        .ticks
        .iter()
        .filter(|t| t.t_ms >= from_ms && t.t_ms < to_ms)
        .map(|t| t.buffer_ms)
        .collect();
    b.sort_unstable();
    b.get(b.len() / 2).copied().unwrap_or(0)
}

/// Concealment (Expand frames) over `[from_ms, to_ms)`.
pub fn conceal_ms_between(run: &Run, from_ms: u64, to_ms: u64) -> u64 {
    run.ticks
        .iter()
        .filter(|t| t.t_ms >= from_ms && t.t_ms < to_ms && t.speech == SpeechType::Expand)
        .count() as u64
        * TICK_MS
}

/// Talkspurts that start after startup (earlier ones are startup's).
pub fn scored_talkspurts(run: &Run) -> Vec<&Talkspurt> {
    run.report
        .talkspurts
        .iter()
        .filter(|t| t.start_ms >= STARTUP_MS)
        .collect()
}

/// Checks that hold for every run: one full frame per tick, finite samples,
/// no louder than the decoded reference, no API errors, and the lifetime
/// concealment counter agrees with the frames marked as concealment.
pub fn assert_sane(run: &Run) {
    let name = &run.scenario.name;
    let r = &run.report;
    let expected_ticks = (run.source.duration_ms() + run.scenario.tail_ms) / TICK_MS;
    assert_eq!(
        r.ticks as u64, expected_ticks,
        "{name}: frames out != ticks"
    );
    assert_eq!(run.out.len(), r.ticks * TICK, "{name}: output length");
    assert_eq!(
        run.frame_len_errors, 0,
        "{name}: frames of the wrong length"
    );
    assert!(
        run.insert_errors.is_empty(),
        "{name}: insert_packet errors {:?}",
        run.insert_errors
    );
    assert!(
        run.get_audio_errors.is_empty(),
        "{name}: get_audio errors {:?}",
        run.get_audio_errors
    );
    assert_eq!(r.non_finite, 0, "{name}: {} NaN/inf samples", r.non_finite);
    assert!(
        r.max_abs <= r.reference_max_abs.max(1.0),
        "{name}: |sample| {} exceeds 1.0 and the reference peak {}",
        r.max_abs,
        r.reference_max_abs
    );
    let paused_ticks = run.ticks.iter().filter(|t| t.paused).count() as u64;
    let expand_samples = (r.expand_ms / TICK_MS - paused_ticks) * TICK as u64;
    let total = r.ticks as u64 * TICK as u64;
    // Concealed samples come only from concealment frames; a frame may also
    // be marked Expand when a decode came up short (no counter), allowed for
    // 1% of the output.
    assert!(
        run.concealed_samples <= expand_samples
            && expand_samples - run.concealed_samples <= total / 100 + TICK as u64,
        "{name}: lifetime concealed_samples {} vs {} samples in Expand frames",
        run.concealed_samples,
        expand_samples
    );
}

// ---------------------------------------------------------------------------
// Audio output
// ---------------------------------------------------------------------------

/// `<target dir>/neteq-dtx-harness`, from cargo's per-target temp dir
/// (`<target dir>/tmp`). Never inside the source tree.
pub fn output_root() -> PathBuf {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    tmp.parent().unwrap_or(tmp).join("neteq-dtx-harness")
}

fn to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

/// Write via a temporary file and rename, so tests that run the same
/// scenario concurrently never leave a torn file.
fn atomic_write(
    path: &Path,
    write: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let tmp = path
        .with_extension(format!("tmp-{:?}", std::thread::current().id()).replace(['(', ')'], ""));
    write(&tmp)?;
    std::fs::rename(&tmp, path)
}

fn write_wav(path: &Path, channels: &[&[f32]]) -> std::io::Result<()> {
    let len = channels.iter().map(|c| c.len()).max().unwrap_or(0);
    atomic_write(path, |tmp| {
        let spec = hound::WavSpec {
            channels: channels.len() as u16,
            sample_rate: RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(tmp, spec).map_err(std::io::Error::other)?;
        for i in 0..len {
            for channel in channels {
                let x = channel.get(i).copied().unwrap_or(0.0);
                writer
                    .write_sample(to_i16(x))
                    .map_err(std::io::Error::other)?;
            }
        }
        writer.finalize().map_err(std::io::Error::other)
    })
}

/// Marker track: silence for normal frames, a 440 Hz tone for comfort
/// noise, 880 Hz for concealment, both -30 dBFS RMS.
fn marker_track(ticks: &[Tick]) -> Vec<f32> {
    let amplitude = 10f32.powf(-30.0 / 20.0) * 2f32.sqrt();
    let mut out = Vec::with_capacity(ticks.len() * TICK);
    for (i, tick) in ticks.iter().enumerate() {
        let freq = match tick.speech {
            SpeechType::Cng => 440.0,
            SpeechType::Expand => 880.0,
            _ => 0.0,
        };
        for k in 0..TICK {
            let n = (i * TICK + k) as f32;
            out.push(if freq > 0.0 {
                amplitude * (2.0 * std::f32::consts::PI * freq * n / RATE as f32).sin()
            } else {
                0.0
            });
        }
    }
    out
}

fn write_outputs(run: &Run, line: &str) -> std::io::Result<()> {
    let dir = &run.out_dir;
    std::fs::create_dir_all(dir)?;
    write_wav(&dir.join("source.wav"), &[&run.source.pcm])?;
    write_wav(&dir.join("reference.wav"), &[&run.reference])?;
    write_wav(&dir.join("neteq.wav"), &[&run.out])?;
    write_wav(&dir.join("ab.wav"), &[&run.reference, &run.out])?;
    write_wav(
        &dir.join("marked.wav"),
        &[&run.out, &marker_track(&run.ticks)],
    )?;
    let mut text = String::new();
    let _ = writeln!(text, "{line}");
    for t in &run.report.talkspurts {
        let _ = writeln!(text, "{t:?}");
    }
    atomic_write(&dir.join("metrics.txt"), |tmp| std::fs::write(tmp, &text))?;
    let mut csv = String::from("t_ms,speech,buffer_ms,target_ms,deep,out_dbfs\n");
    for (i, tick) in run.ticks.iter().enumerate() {
        let level = dbfs(rms(&run.out[i * TICK..(i + 1) * TICK]));
        let _ = writeln!(
            csv,
            "{},{:?},{},{},{:?},{level:.1}",
            tick.t_ms,
            tick.speech,
            tick.buffer_ms,
            tick.target_ms,
            deep_kind(run, tick)
        );
    }
    atomic_write(&dir.join("ticks.csv"), |tmp| std::fs::write(tmp, &csv))
}
