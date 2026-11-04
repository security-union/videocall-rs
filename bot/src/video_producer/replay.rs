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

//! Replay video source (#3132): EKG GOPs encoded once per process by the live
//! pipeline, then replayed by every publisher with fresh sequence numbers and
//! timestamps.

use super::{
    build_and_send_layer, ekg_rms_index_for_render_frame, rgb_to_i420_into,
    simulcast_layer_directives, VideoProducer,
};
use crate::aq_controller::BotAq;
use crate::ekg_renderer::{ekg_frame_is_speaking, EkgRenderer};
use crate::i420_scale::scale_i420;
use crate::transport::OutboundFrameSender;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Most library build threads, split between the two variants.
const BUILD_THREADS_MAX: usize = 4;

/// The browser's `FORCED_KEYFRAME_COOLDOWN_MS` (videocall-client camera_encoder.rs).
pub const KEYFRAME_REQUEST_COOLDOWN_US: u64 = 250_000;

/// The cooldown the browser uses while its WebSocket send queue is over the camera gate.
pub const CONGESTED_KEYFRAME_REQUEST_COOLDOWN_US: u64 =
    (videocall_aq::constants::CAMERA_WS_CONGESTED_KEYFRAME_COOLDOWN_MS * 1000.0) as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Silent = 0,
    Speaking = 1,
}

/// A closed GOP: `frames[0]` is the keyframe, the rest are its deltas.
pub struct ReplayGop {
    frames: Vec<Box<[u8]>>,
}

pub struct ReplayLayer {
    pub layer_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub gop_frames: u32,
    variants: [Vec<ReplayGop>; 2],
}

impl ReplayLayer {
    pub fn gops(&self, variant: Variant) -> usize {
        self.variants[variant as usize].len()
    }

    pub fn bytes(&self, variant: Variant) -> usize {
        self.variants[variant as usize]
            .iter()
            .flat_map(|g| g.frames.iter())
            .map(|f| f.len())
            .sum()
    }

    pub fn frames(&self, variant: Variant, gop: usize) -> Vec<&[u8]> {
        self.variants[variant as usize][gop]
            .frames
            .iter()
            .map(|f| &f[..])
            .collect()
    }
}

/// The per-process replay library, shared by every replay publisher.
pub struct ReplayLibrary {
    layers: Vec<ReplayLayer>,
    build_threads: usize,
}

impl ReplayLibrary {
    pub fn layers(&self) -> &[ReplayLayer] {
        &self.layers
    }

    pub fn build_threads(&self) -> usize {
        self.build_threads
    }

    pub fn total_bytes(&self) -> usize {
        self.layers
            .iter()
            .map(|l| l.bytes(Variant::Silent) + l.bytes(Variant::Speaking))
            .sum()
    }
}

/// One talker's EKG timeline: `rms` at `rms_fps`, normalised by `max_rms`.
pub struct SpeechTrack {
    pub rms: Vec<f32>,
    pub max_rms: f32,
}

/// Render ticks (at `render_fps`) of every talker where the live EKG draws the
/// speaking waveform, in timeline order.
fn speaking_ticks(talkers: &[SpeechTrack], rms_fps: u32, render_fps: u32) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    for t in talkers {
        let ticks = t.rms.len() as u64 * render_fps as u64 / rms_fps.max(1) as u64;
        for tick in 0..ticks as usize {
            let idx = ekg_rms_index_for_render_frame(tick, rms_fps, render_fps);
            if let Some(&rms) = t.rms.get(idx) {
                if ekg_frame_is_speaking(rms, t.max_rms) {
                    out.push((rms, t.max_rms));
                }
            }
        }
    }
    out
}

/// Encode `loop_gops` GOPs per video layer and variant with the live simulcast
/// encoders, renderer and pacing, on a virtual clock. Each variant is split into
/// `threads / 2` chunks, one thread each, and every chunk first encodes a
/// warm-up GOP that it drops.
pub fn build_library(
    n_layers: u32,
    color: [u8; 3],
    talkers: &[SpeechTrack],
    rms_fps: u32,
    loop_gops: usize,
    quit: &AtomicBool,
) -> anyhow::Result<ReplayLibrary> {
    let built = build_library_with(
        n_layers,
        color,
        talkers,
        rms_fps,
        loop_gops,
        build_threads(),
        quit,
    );
    release_build_memory();
    built
}

fn build_library_with(
    n_layers: u32,
    color: [u8; 3],
    talkers: &[SpeechTrack],
    rms_fps: u32,
    loop_gops: usize,
    threads: usize,
    quit: &AtomicBool,
) -> anyhow::Result<ReplayLibrary> {
    let ladder = videocall_aq::constants::simulcast_layers(n_layers as usize);
    let top = ladder
        .last()
        .ok_or_else(|| anyhow::anyhow!("empty simulcast ladder for n={n_layers}"))?;
    let (native_w, native_h) = (top.max_width & !1, top.max_height & !1);
    let render_fps = ladder
        .iter()
        .map(|t| t.target_fps)
        .max()
        .unwrap_or(1)
        .max(1);
    let speech = speaking_ticks(talkers, rms_fps, render_fps);
    let loop_gops = loop_gops.max(1);

    let chunks = chunk_plan(loop_gops, threads);
    let gop_ticks = top.keyframe_interval_frames as usize * render_fps as usize
        / top.target_fps.max(1) as usize;
    let speech = &speech;
    let variants: Vec<Vec<Vec<EncodedLayer>>> = thread::scope(|s| {
        let spawn = |speaking: bool| -> Vec<_> {
            chunks
                .iter()
                .map(|&(first, gops)| {
                    s.spawn(move || {
                        encode_variant(
                            n_layers,
                            color,
                            native_w,
                            native_h,
                            gops,
                            first * gop_ticks,
                            quit,
                            |k| {
                                if speaking {
                                    speech[k % speech.len()]
                                } else {
                                    (0.0, 0.01)
                                }
                            },
                        )
                    })
                })
                .collect()
        };
        let silent = spawn(false);
        let spoken = if speech.is_empty() {
            Vec::new()
        } else {
            spawn(true)
        };
        vec![silent, spoken]
            .into_iter()
            .map(|handles| {
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .map_err(|_| anyhow::anyhow!("replay encode thread panicked"))?
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })?;

    let mut variants = variants.into_iter();
    let silent = variants.next().unwrap_or_default();
    let spoken = variants.next().unwrap_or_default();
    let merge = |chunks: &mut Vec<Vec<EncodedLayer>>, layer: usize| -> Vec<ReplayGop> {
        chunks
            .iter_mut()
            .flat_map(|c| std::mem::take(&mut c[layer].gops))
            .collect()
    };
    let (mut silent, mut spoken) = (silent, spoken);
    let first = silent
        .first()
        .ok_or_else(|| anyhow::anyhow!("replay build produced no chunk"))?;
    let shape: Vec<(u32, u32, u32, u32, u32)> = first
        .iter()
        .map(|l| (l.layer_id, l.width, l.height, l.fps, l.gop_frames))
        .collect();
    let layers = shape
        .into_iter()
        .enumerate()
        .map(
            |(idx, (layer_id, width, height, fps, gop_frames))| ReplayLayer {
                layer_id,
                width,
                height,
                fps,
                gop_frames,
                variants: [merge(&mut silent, idx), merge(&mut spoken, idx)],
            },
        )
        .collect();
    Ok(ReplayLibrary {
        layers,
        build_threads: threads,
    })
}

struct EncodedLayer {
    layer_id: u32,
    width: u32,
    height: u32,
    fps: u32,
    gop_frames: u32,
    gops: Vec<ReplayGop>,
}

#[allow(clippy::too_many_arguments)]
fn encode_variant(
    n_layers: u32,
    color: [u8; 3],
    native_w: u32,
    native_h: u32,
    loop_gops: usize,
    start_tick: usize,
    quit: &AtomicBool,
    source: impl Fn(usize) -> (f32, f32),
) -> anyhow::Result<Vec<EncodedLayer>> {
    let mut layers = VideoProducer::build_simulcast_layers(n_layers, native_w, native_h)?;
    let renderer = EkgRenderer::new(color, native_w, native_h);
    let render_fps = layers.iter().map(|l| l.framerate).max().unwrap_or(1).max(1);
    let frame_interval_us = 1_000_000 / render_fps as u64;
    let mut frame_buf = renderer.create_frame_buffer();
    let mut i420_buf = vec![0u8; (native_w * native_h * 3 / 2) as usize];
    let wanted: Vec<u64> = layers
        .iter()
        .map(|l| (loop_gops as u64 + 1) * l.kf.frames_per_keyframe as u64)
        .collect();
    let mut out: Vec<Vec<(Vec<u8>, bool)>> = layers.iter().map(|_| Vec::new()).collect();

    let mut tick: usize = 0;
    while layers
        .iter()
        .zip(&wanted)
        .any(|(l, &want)| l.kf.sequence < want)
    {
        if quit.load(Ordering::Relaxed) {
            anyhow::bail!("replay library build interrupted by shutdown");
        }
        let elapsed_us = tick as u64 * frame_interval_us;
        let first_tick = tick == 0;
        let (rms, max_rms) = source(start_tick + tick);
        renderer.render_frame_rgb_into(&mut frame_buf, rms, max_rms, start_tick + tick);
        rgb_to_i420_into(&frame_buf, &mut i420_buf);

        for (idx, layer) in layers.iter_mut().enumerate() {
            layer.kf.latch_wrap(first_tick);
            if layer.kf.sequence >= wanted[idx] || elapsed_us < layer.next_due_us {
                continue;
            }
            if elapsed_us > layer.next_due_us + layer.frame_interval_us {
                layer.next_due_us = elapsed_us + layer.frame_interval_us;
            } else {
                layer.next_due_us += layer.frame_interval_us;
            }
            let force = layer.kf.force_keyframe(first_tick);
            let input: &[u8] = if layer.needs_scale {
                scale_i420(
                    &i420_buf,
                    native_w,
                    native_h,
                    &mut layer.scale_buf,
                    layer.enc_w,
                    layer.enc_h,
                );
                &layer.scale_buf[..]
            } else {
                &i420_buf[..]
            };
            let pts = layer.kf.sequence as i64;
            let frames = if force {
                layer.encoder.encode_keyframe(pts, input)?
            } else {
                layer.encoder.encode(pts, input)?
            };
            let mut saw_key = false;
            for frame in frames {
                saw_key |= frame.key;
                out[idx].push((frame.data.to_vec(), frame.key));
            }
            layer.kf.on_encoded(saw_key);
        }
        tick += 1;
    }

    layers
        .iter()
        .zip(out)
        .map(|(layer, packets)| {
            let mut gops = split_gops(packets)?;
            if gops.len() > 1 {
                gops.remove(0);
            }
            Ok(EncodedLayer {
                layer_id: layer.layer_id,
                width: layer.enc_w,
                height: layer.enc_h,
                fps: layer.framerate,
                gop_frames: layer.kf.frames_per_keyframe,
                gops,
            })
        })
        .collect()
}

fn split_gops(packets: Vec<(Vec<u8>, bool)>) -> anyhow::Result<Vec<ReplayGop>> {
    let mut gops: Vec<ReplayGop> = Vec::new();
    for (data, key) in packets {
        if key {
            gops.push(ReplayGop { frames: Vec::new() });
        }
        match gops.last_mut() {
            Some(g) => g.frames.push(data.into_boxed_slice()),
            None => anyhow::bail!("encoder output did not start with a keyframe"),
        }
    }
    if gops.is_empty() {
        anyhow::bail!("encoder produced no frames");
    }
    Ok(gops)
}

/// One frame to put on the wire.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Emit<'a> {
    pub layer_id: u32,
    pub data: &'a [u8],
    pub key: bool,
    pub seq: u64,
    pub fps: u32,
    /// A keyframe that only a KEYFRAME_REQUEST caused.
    pub forced: bool,
    /// A keyframe that only an AQ tier change caused.
    pub tier_change: bool,
}

struct LayerCursor {
    fps: u32,
    interval_us: u64,
    next_due_us: u64,
    seq: u64,
    playing: Option<(Variant, usize, usize)>,
    next_gop: [usize; 2],
}

/// Per-publisher replay state machine, driven once per render tick. Like the
/// browser camera encoder, one decision per tick keys every active video layer
/// together: on each periodic boundary of the publisher's own keyframe clock,
/// and on a KEYFRAME_REQUEST once the publisher's cooldown allows.
pub(crate) struct ReplaySchedule {
    layers: Vec<LayerCursor>,
    phase_us: u64,
    gop_us: u64,
    boundary: Option<u64>,
    pending: bool,
    pending_tier: bool,
    last_key_us: Option<u64>,
}

impl ReplaySchedule {
    /// `phase_us` sets where in the keyframe cycle this publisher starts;
    /// `start_gop` which stored GOP it plays first.
    pub fn new(library: &ReplayLibrary, phase_us: u64, start_gop: usize) -> Self {
        let gop_us = library
            .layers
            .last()
            .map(|l| l.gop_frames as u64 * 1_000_000 / l.fps.max(1) as u64)
            .unwrap_or(1)
            .max(1);
        let layers = library
            .layers
            .iter()
            .map(|l| LayerCursor {
                fps: l.fps,
                interval_us: 1_000_000 / l.fps.max(1) as u64,
                next_due_us: 0,
                seq: 0,
                playing: None,
                next_gop: [start_gop; 2],
            })
            .collect();
        Self {
            layers,
            phase_us: phase_us % gop_us,
            gop_us,
            boundary: None,
            pending: false,
            pending_tier: false,
            last_key_us: None,
        }
    }

    pub fn request_keyframe(&mut self) {
        self.pending = true;
    }

    /// The AQ changed tier (the `take_tier_changed` edge, which includes every
    /// active video layer count change). As in the browser, this keys every
    /// active video layer, skipping the cooldown unless `reset_cooldown` is
    /// false (a congested WebSocket uplink).
    pub fn request_tier_keyframe(&mut self, reset_cooldown: bool) {
        self.pending_tier = true;
        if reset_cooldown {
            self.last_key_us = None;
        }
    }

    /// Emit the frames due at `elapsed_us`, at most one per video layer. A
    /// keyframe bypasses video layer pacing. `speaking(window_us)` says whether
    /// the publisher speaks for at least half of the next `window_us`.
    pub fn tick<'a>(
        &mut self,
        library: &'a ReplayLibrary,
        elapsed_us: u64,
        active: usize,
        cooldown_us: u64,
        speaking: &dyn Fn(u64) -> bool,
        emit: &mut dyn FnMut(Emit<'a>),
    ) {
        let boundary = (elapsed_us + self.phase_us) / self.gop_us;
        let periodic = self.boundary != Some(boundary);
        self.boundary = Some(boundary);
        let cooled = self
            .last_key_us
            .is_none_or(|last| elapsed_us.saturating_sub(last) >= cooldown_us);
        let forced = (self.pending || self.pending_tier) && cooled && !periodic;
        let (by_request, by_tier) = (forced && self.pending, forced && self.pending_tier);
        let key_all = periodic || forced;
        if key_all {
            self.pending = false;
            self.pending_tier = false;
            self.last_key_us = Some(elapsed_us);
        }
        let window_us = self.gop_us - (elapsed_us + self.phase_us) % self.gop_us;
        let mut variant = None;

        for (layer, cursor) in library.layers.iter().zip(self.layers.iter_mut()) {
            if layer.layer_id as usize >= active {
                continue;
            }
            let due = elapsed_us >= cursor.next_due_us;
            if !due && !key_all {
                continue;
            }
            if !due || elapsed_us > cursor.next_due_us + cursor.interval_us {
                cursor.next_due_us = elapsed_us + cursor.interval_us;
            } else {
                cursor.next_due_us += cursor.interval_us;
            }

            let exhausted = cursor
                .playing
                .is_none_or(|(v, g, pos)| pos >= layer.variants[v as usize][g].frames.len());
            if exhausted && !key_all && cursor.playing.is_some() && window_us <= cursor.interval_us
            {
                continue;
            }
            if key_all || exhausted {
                let v = *variant.get_or_insert_with(|| {
                    if layer.gops(Variant::Speaking) > 0 && speaking(window_us) {
                        Variant::Speaking
                    } else {
                        Variant::Silent
                    }
                });
                let v = if layer.gops(v) > 0 {
                    v
                } else {
                    Variant::Silent
                };
                let count = layer.gops(v);
                if count == 0 {
                    continue;
                }
                let gop = cursor.next_gop[v as usize] % count;
                cursor.next_gop[v as usize] = gop + 1;
                cursor.playing = Some((v, gop, 0));
            }

            let Some((v, gop, pos)) = cursor.playing.as_mut() else {
                continue;
            };
            let data = &layer.variants[*v as usize][*gop].frames[*pos];
            emit(Emit {
                layer_id: layer.layer_id,
                data,
                key: *pos == 0,
                seq: cursor.seq,
                fps: cursor.fps,
                forced: *pos == 0 && by_request,
                tier_change: *pos == 0 && by_tier,
            });
            *pos += 1;
            cursor.seq += 1;
        }
    }
}

/// Where in the keyframe cycle and the render tick one publisher starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PublisherDraw {
    pub phase_us: u64,
    pub tick_offset_us: u64,
    pub start_gop: usize,
}

impl PublisherDraw {
    pub fn new(rng: &mut impl Rng, gop_us: u64, frame_interval_us: u64) -> Self {
        Self {
            phase_us: rng.gen_range(0..gop_us.max(1)),
            tick_offset_us: rng.gen_range(0..frame_interval_us.max(1)),
            start_gop: rng.gen_range(0..64),
        }
    }
}

/// The publisher's random source: seeded from `seed` and the user id when a
/// replay seed is configured, from entropy otherwise.
pub(crate) fn publisher_rng(seed: Option<u64>, user_id: &str) -> StdRng {
    match seed {
        Some(seed) => {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in user_id.bytes() {
                h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
            }
            StdRng::seed_from_u64(seed ^ h)
        }
        None => StdRng::from_entropy(),
    }
}

/// Per-render-tick speaking flags of one publisher's media loop, with prefix
/// sums so a window's majority is O(1).
pub(crate) struct SpeechTimeline {
    prefix: Vec<u32>,
    render_interval_us: u64,
}

impl SpeechTimeline {
    pub fn new(
        rms: &[f32],
        max_rms: f32,
        rms_fps: u32,
        render_fps: u32,
        loop_duration: Duration,
    ) -> Self {
        let render_interval_us = 1_000_000 / render_fps.max(1) as u64;
        let ticks = (loop_duration.as_micros() as u64)
            .div_ceil(render_interval_us)
            .max(1) as usize;
        let mut prefix = Vec::with_capacity(ticks + 1);
        prefix.push(0);
        for t in 0..ticks {
            let idx = ekg_rms_index_for_render_frame(t, rms_fps, render_fps);
            let on = rms
                .get(idx)
                .is_some_and(|&r| ekg_frame_is_speaking(r, max_rms));
            let last = *prefix.last().unwrap_or(&0);
            prefix.push(last + on as u32);
        }
        Self {
            prefix,
            render_interval_us,
        }
    }

    fn ticks(&self) -> usize {
        self.prefix.len() - 1
    }

    fn count(&self, start: usize, len: usize) -> u64 {
        let n = self.ticks();
        let total = self.prefix[n] as u64;
        let start = start % n;
        let mut count = (len / n) as u64 * total;
        let rem = len % n;
        let end = start + rem;
        count += if end <= n {
            (self.prefix[end] - self.prefix[start]) as u64
        } else {
            (self.prefix[n] - self.prefix[start] + self.prefix[end - n]) as u64
        };
        count
    }

    /// Whether at least half the render ticks in `[frame_in_loop, +window_us)` speak.
    pub fn speaking_majority(&self, frame_in_loop: usize, window_us: u64) -> bool {
        let len = (window_us / self.render_interval_us).max(1) as usize;
        self.count(frame_in_loop, len) * 2 >= len as u64
    }
}

/// Per-video-layer counts of the keyframes one publisher sent only because a
/// KEYFRAME_REQUEST or an AQ tier change asked for one.
pub struct ForcedKeyframeCounts {
    request: Vec<u64>,
    tier_change: Vec<u64>,
    #[cfg(feature = "metrics")]
    metrics: Vec<[prometheus::IntCounter; 2]>,
}

impl ForcedKeyframeCounts {
    pub fn new(n_layers: usize) -> Self {
        Self {
            request: vec![0; n_layers],
            tier_change: vec![0; n_layers],
            #[cfg(feature = "metrics")]
            metrics: Vec::new(),
        }
    }

    /// Also count into `bot_replay_forced_keyframes_total{cause}`.
    #[cfg(feature = "metrics")]
    pub fn with_metrics(
        mut self,
        metrics: &crate::metrics_server::BotMetrics,
        bot: &str,
        meeting: &str,
    ) -> Self {
        self.metrics = (0..self.request.len())
            .map(|layer| {
                let layer = layer.to_string();
                ["request", "tier_change"].map(|cause| {
                    metrics
                        .replay_forced_keyframes_total
                        .with_label_values(&[bot, meeting, &layer, cause])
                })
            })
            .collect();
        self
    }

    pub fn record(&mut self, layer: u32, by_request: bool, by_tier: bool) {
        let layer = layer as usize;
        if by_request {
            if let Some(n) = self.request.get_mut(layer) {
                *n += 1;
            }
            #[cfg(feature = "metrics")]
            if let Some(counters) = self.metrics.get(layer) {
                counters[0].inc();
            }
        }
        if by_tier {
            if let Some(n) = self.tier_change.get_mut(layer) {
                *n += 1;
            }
            #[cfg(feature = "metrics")]
            if let Some(counters) = self.metrics.get(layer) {
                counters[1].inc();
            }
        }
    }

    pub fn request(&self) -> &[u64] {
        &self.request
    }

    pub fn tier_change(&self) -> &[u64] {
        &self.tier_change
    }
}

/// Called for every keyframe a KEYFRAME_REQUEST (`by_request`) or an AQ tier
/// change (`by_tier`) forced, with its video layer id.
pub type ForcedKeyframeHook = Box<dyn Fn(u32, bool, bool) + Send + 'static>;

/// The uplink saturation proxy for the congested keyframe cooldown: the
/// netsim shim's cumulative `bandwidth_wait_us`, and whether the transport is
/// WebSocket (the browser only widens its cooldown there).
pub struct UplinkProbe {
    pub websocket: bool,
    pub wait_us: Box<dyn Fn() -> u64 + Send>,
}

/// A publisher's KEYFRAME_REQUEST cooldown: `KEYFRAME_REQUEST_COOLDOWN_US`, or
/// the congested one while a WebSocket uplink's shim waited within the last AQ tick.
pub(crate) struct RequestCooldown {
    last_wait_us: u64,
    saturated_at_us: Option<u64>,
}

impl RequestCooldown {
    pub fn new(uplink: Option<&UplinkProbe>) -> Self {
        Self {
            last_wait_us: uplink.map_or(0, |u| (u.wait_us)()),
            saturated_at_us: None,
        }
    }

    pub fn cooldown_us(&mut self, uplink: Option<&UplinkProbe>, elapsed_us: u64) -> u64 {
        let Some(uplink) = uplink else {
            return KEYFRAME_REQUEST_COOLDOWN_US;
        };
        let wait = (uplink.wait_us)();
        if wait > self.last_wait_us {
            self.saturated_at_us = Some(elapsed_us);
        }
        self.last_wait_us = wait;
        let window_us = videocall_aq::constants::AQ_TICK_INTERVAL_MS * 1000;
        let saturated = self
            .saturated_at_us
            .is_some_and(|at| elapsed_us.saturating_sub(at) <= window_us);
        if uplink.websocket && saturated {
            CONGESTED_KEYFRAME_REQUEST_COOLDOWN_US
        } else {
            KEYFRAME_REQUEST_COOLDOWN_US
        }
    }
}

/// The probe for a publisher on `transport` behind `shim`; `None` without a shim.
pub fn uplink_probe(
    shim: Option<Arc<crate::netsim::NetSimShim>>,
    transport: &crate::config::Transport,
) -> Option<UplinkProbe> {
    shim.map(|shim| UplinkProbe {
        websocket: matches!(transport, crate::config::Transport::WebSocket),
        wait_us: Box::new(move || shim.bandwidth_wait_us()),
    })
}

/// What a replay publisher reads from the rest of the bot each tick.
pub(crate) struct ReplayControls<'a> {
    aq: &'a BotAq,
    keyframe_requests: Option<&'a AtomicU64>,
    uplink: Option<&'a UplinkProbe>,
    n_layers: usize,
    last_epoch: u64,
    last_tier_epoch: u64,
    seen_requests: u64,
    active: usize,
    cooldown: RequestCooldown,
}

impl<'a> ReplayControls<'a> {
    /// `keyframe_requests` is `None` when the publisher ignores KEYFRAME_REQUESTs.
    pub fn new(
        aq: &'a BotAq,
        keyframe_requests: Option<&'a AtomicU64>,
        uplink: Option<&'a UplinkProbe>,
        n_layers: usize,
    ) -> Self {
        Self {
            aq,
            keyframe_requests,
            uplink,
            n_layers,
            last_epoch: aq.simulcast_epoch(),
            last_tier_epoch: aq.tier_epoch(),
            seen_requests: keyframe_requests.map_or(0, |r| r.load(Ordering::Relaxed)),
            active: Self::read_active(aq, n_layers),
            cooldown: RequestCooldown::new(uplink),
        }
    }

    fn read_active(aq: &BotAq, n_layers: usize) -> usize {
        simulcast_layer_directives(&aq.simulcast_snapshot(), n_layers, &vec![0; n_layers]).0
    }

    /// Pass new keyframe requests and AQ tier changes to `schedule` (both only
    /// when the publisher answers requests); return the active video layer
    /// count and the keyframe-request cooldown for this tick.
    pub fn poll(
        &mut self,
        schedule: &mut ReplaySchedule,
        user_id: &str,
        elapsed_us: u64,
    ) -> (usize, u64) {
        let tier_epoch = self.aq.tier_epoch();
        let epoch = self.aq.simulcast_epoch();
        if epoch != self.last_epoch {
            self.last_epoch = epoch;
            self.active = Self::read_active(self.aq, self.n_layers);
        }
        if let Some(requests) = self.keyframe_requests {
            let n = requests.load(Ordering::Relaxed);
            if n != self.seen_requests {
                self.seen_requests = n;
                schedule.request_keyframe();
                debug!("[{}] replay: KEYFRAME_REQUEST #{}", user_id, n);
            }
        }
        let cooldown_us = self.cooldown.cooldown_us(self.uplink, elapsed_us);
        if tier_epoch != self.last_tier_epoch {
            self.last_tier_epoch = tier_epoch;
            if self.keyframe_requests.is_some() {
                schedule
                    .request_tier_keyframe(cooldown_us != CONGESTED_KEYFRAME_REQUEST_COOLDOWN_US);
            }
        }
        (self.active, cooldown_us)
    }
}

/// Everything one replay publisher needs, besides the shared library.
pub struct ReplayPublisher {
    pub user_id: String,
    pub rms: Vec<f32>,
    pub max_rms: f32,
    pub rms_fps: u32,
    pub packet_sender: OutboundFrameSender,
    pub media_start: Instant,
    pub loop_duration: Duration,
    pub aq: Arc<BotAq>,
    pub encoder_output_fps: Arc<AtomicU32>,
    pub encoder_frames_ok: Arc<AtomicU64>,
    pub transport_drops_counter: Arc<AtomicU64>,
    /// `None`: KEYFRAME_REQUESTs are ignored.
    pub keyframe_requests: Option<Arc<AtomicU64>>,
    /// `None` without an uplink shim: the cooldown stays at 250 ms.
    pub uplink: Option<UplinkProbe>,
    pub seed: Option<u64>,
    pub on_forced_keyframe: ForcedKeyframeHook,
}

impl VideoProducer {
    /// Replay `library` for one publisher (`video_mode: replay`).
    pub fn from_replay(library: Arc<ReplayLibrary>, publisher: ReplayPublisher) -> Self {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let user_id = publisher.user_id.clone();
        let handle = thread::spawn(move || {
            if let Err(e) = replay_loop(&library, &publisher, &quit_clone) {
                error!(
                    "Replay video producer error for {}: {}",
                    publisher.user_id, e
                );
            }
        });
        VideoProducer {
            user_id,
            quit,
            handle: Some(handle),
        }
    }
}

fn replay_loop(
    library: &ReplayLibrary,
    p: &ReplayPublisher,
    quit: &AtomicBool,
) -> anyhow::Result<()> {
    let user_id = p.user_id.as_str();
    let loop_duration_us = p.loop_duration.as_micros() as u64;
    let top = library
        .layers
        .last()
        .ok_or_else(|| anyhow::anyhow!("empty replay library"))?;
    if loop_duration_us == 0 {
        warn!(
            "Replay producer for {} has zero loop duration, exiting",
            user_id
        );
        return Ok(());
    }
    let render_fps = library
        .layers
        .iter()
        .map(|l| l.fps)
        .max()
        .unwrap_or(1)
        .max(1);
    let frame_interval_us = 1_000_000 / render_fps as u64;
    let gop_us = top.gop_frames as u64 * 1_000_000 / top.fps.max(1) as u64;
    let draw = PublisherDraw::new(
        &mut publisher_rng(p.seed, user_id),
        gop_us,
        frame_interval_us,
    );
    let start = p.media_start + Duration::from_micros(draw.tick_offset_us);
    let mut schedule = ReplaySchedule::new(library, draw.phase_us, draw.start_gop);
    let timeline = SpeechTimeline::new(&p.rms, p.max_rms, p.rms_fps, render_fps, p.loop_duration);
    let user_id_bytes = user_id.as_bytes().to_vec();
    let mut controls = ReplayControls::new(
        &p.aq,
        p.keyframe_requests.as_deref(),
        p.uplink.as_ref(),
        library.layers.len(),
    );
    let mut render_count: u64 = 0;
    let mut drops: u64 = 0;
    p.encoder_output_fps.store(render_fps, Ordering::Relaxed);
    info!(
        "Replay video producer started for {}: {} layers, phase={}ms, tick offset={}us, library={} bytes",
        user_id,
        library.layers.len(),
        draw.phase_us / 1000,
        draw.tick_offset_us,
        library.total_bytes()
    );
    let now = Instant::now();
    if now < start {
        thread::sleep(start - now);
    }

    while !quit.load(Ordering::Relaxed) {
        let elapsed_us = start.elapsed().as_micros() as u64;
        let (active, cooldown_us) = controls.poll(&mut schedule, user_id, elapsed_us);
        let position_in_loop_us = elapsed_us % loop_duration_us;
        let frame_in_loop = (position_in_loop_us / frame_interval_us) as usize;
        let speaking = |window_us: u64| timeline.speaking_majority(frame_in_loop, window_us);
        let mut result = Ok(());
        schedule.tick(
            library,
            elapsed_us,
            active,
            cooldown_us,
            &speaking,
            &mut |e: Emit<'_>| {
                if result.is_err() {
                    return;
                }
                if e.forced || e.tier_change {
                    (p.on_forced_keyframe)(e.layer_id, e.forced, e.tier_change);
                }
                match build_and_send_layer(
                    &p.packet_sender,
                    &user_id_bytes,
                    e.data,
                    e.key,
                    e.seq,
                    e.fps,
                    e.layer_id,
                ) {
                    Ok(true) => {}
                    Ok(false) => {
                        drops += 1;
                        p.transport_drops_counter.fetch_add(1, Ordering::Relaxed);
                        if drops % 100 == 1 {
                            warn!(
                                "[{}] Dropped simulcast video packets due to full send channel (total: {})",
                                user_id, drops,
                            );
                        }
                    }
                    Err(err) => result = Err(err),
                }
                p.encoder_frames_ok.fetch_add(1, Ordering::Relaxed);
            },
        );
        result?;

        if render_count.is_multiple_of(render_fps as u64 * 5) {
            debug!(
                "[{}] ekg-replay render={}, frame={}, loop={}, pos={:.1}s/{:.1}s, layers={}",
                user_id,
                render_count,
                frame_in_loop,
                elapsed_us / loop_duration_us,
                position_in_loop_us as f64 / 1_000_000.0,
                loop_duration_us as f64 / 1_000_000.0,
                active,
            );
        }
        render_count += 1;

        let next_frame_us = ((frame_in_loop as u64 + 1) * frame_interval_us).min(loop_duration_us);
        let target =
            start + Duration::from_micros(elapsed_us - position_in_loop_us + next_frame_us);
        let now = Instant::now();
        if now < target {
            thread::sleep(target - now);
        }
    }
    info!("Replay video producer stopping for {}", user_id);
    Ok(())
}

/// Library build threads for this host: `min(4, available_parallelism / 2)`, at least 2.
fn build_threads() -> usize {
    threads_for(thread::available_parallelism().map_or(2, |n| n.get()))
}

fn threads_for(cores: usize) -> usize {
    (cores / 2).clamp(2, BUILD_THREADS_MAX)
}

/// `(first GOP, GOPs)` per build chunk of one variant, `threads / 2` chunks.
fn chunk_plan(loop_gops: usize, threads: usize) -> Vec<(usize, usize)> {
    let chunk_gops = loop_gops.max(1).div_ceil((threads / 2).max(1));
    (0..loop_gops.max(1))
        .step_by(chunk_gops)
        .map(|first| (first, chunk_gops.min(loop_gops.max(1) - first)))
        .collect()
}

/// Return freed heap pages to the OS after the library build (glibc only).
fn release_build_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: malloc_trim only inspects and releases free glibc heap pages.
        unsafe {
            malloc_trim(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SILENT: u8 = 0x51;
    const SPEAKING: u8 = 0x53;

    /// Frames are `[variant tag, gop, pos]` so a test can tell exactly which
    /// stored frame was emitted.
    fn library(gops: usize, with_speaking: bool) -> ReplayLibrary {
        let ladder = videocall_aq::constants::simulcast_layers(3);
        let layers = ladder
            .iter()
            .enumerate()
            .map(|(id, t)| {
                let make = |tag: u8| {
                    (0..gops)
                        .map(|g| ReplayGop {
                            frames: (0..t.keyframe_interval_frames)
                                .map(|p| vec![tag, g as u8, (p % 251) as u8].into_boxed_slice())
                                .collect(),
                        })
                        .collect::<Vec<_>>()
                };
                ReplayLayer {
                    layer_id: id as u32,
                    width: t.max_width,
                    height: t.max_height,
                    fps: t.target_fps,
                    gop_frames: t.keyframe_interval_frames,
                    variants: [
                        make(SILENT),
                        if with_speaking {
                            make(SPEAKING)
                        } else {
                            Vec::new()
                        },
                    ],
                }
            })
            .collect();
        ReplayLibrary {
            layers,
            build_threads: 4,
        }
    }

    #[derive(Debug, Clone)]
    struct Sent {
        t_us: u64,
        layer: u32,
        key: bool,
        seq: u64,
        forced: bool,
        frame: Vec<u8>,
    }

    struct Drive {
        active: Box<dyn Fn(u64) -> usize>,
        speaking: Box<dyn Fn(u64) -> bool>,
        pli_at_us: Vec<u64>,
        cooldown_us: u64,
    }

    impl Default for Drive {
        fn default() -> Self {
            Self {
                active: Box::new(|_| 3),
                speaking: Box::new(|_| false),
                pli_at_us: Vec::new(),
                cooldown_us: KEYFRAME_REQUEST_COOLDOWN_US,
            }
        }
    }

    fn run(lib: &ReplayLibrary, phase_us: u64, secs: u64, drive: &Drive) -> Vec<Sent> {
        let mut sched = ReplaySchedule::new(lib, phase_us, 0);
        let mut sent = Vec::new();
        let interval = 1_000_000 / 30;
        let mut plis = drive.pli_at_us.clone();
        for tick in 0..secs * 30 {
            let t_us = tick * interval;
            while plis.first().is_some_and(|&p| p <= t_us) {
                plis.remove(0);
                sched.request_keyframe();
            }
            let speaking = |_w: u64| (drive.speaking)(t_us);
            sched.tick(
                lib,
                t_us,
                (drive.active)(t_us),
                drive.cooldown_us,
                &speaking,
                &mut |e| {
                    sent.push(Sent {
                        t_us,
                        layer: e.layer_id,
                        key: e.key,
                        seq: e.seq,
                        forced: e.forced,
                        frame: e.data.to_vec(),
                    })
                },
            );
        }
        sent
    }

    fn of_layer(sent: &[Sent], layer: u32) -> Vec<Sent> {
        sent.iter().filter(|s| s.layer == layer).cloned().collect()
    }

    fn key_times_s(sent: &[Sent], layer: u32) -> Vec<f64> {
        of_layer(sent, layer)
            .iter()
            .filter(|s| s.key)
            .map(|s| s.t_us as f64 / 1e6)
            .collect()
    }

    /// Every delta continues the GOP of the last keyframe, in stored order.
    fn assert_decodable(sent: &[Sent]) {
        for layer in 0..3 {
            let mut current: Option<(u8, u8, u8)> = None;
            for s in of_layer(sent, layer) {
                let (tag, gop, pos) = (s.frame[0], s.frame[1], s.frame[2]);
                if s.key {
                    assert_eq!(pos, 0, "a keyframe must be a GOP's first frame: {s:?}");
                } else {
                    let (ct, cg, cp) = current.expect("a delta before any keyframe");
                    assert_eq!((tag, gop, pos), (ct, cg, cp + 1), "layer {layer}: {s:?}");
                }
                current = Some((tag, gop, pos));
            }
        }
    }

    #[test]
    fn per_layer_rates_match_the_ladder() {
        let sent = run(&library(4, false), 0, 60, &Drive::default());
        for (layer, fps) in [(0u32, 7usize), (1, 15), (2, 30)] {
            let n = of_layer(&sent, layer).len();
            assert!(
                n.abs_diff(fps * 60) <= 1,
                "{}",
                format!("layer {layer}: {n} frames in 60 s")
            );
        }
    }

    #[test]
    fn sequences_are_contiguous_across_library_wraps() {
        let sent = run(&library(2, false), 1_234_000, 40, &Drive::default());
        for layer in 0..3 {
            let seqs: Vec<u64> = of_layer(&sent, layer).iter().map(|s| s.seq).collect();
            assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
        }
        assert_decodable(&sent);
        let gops_seen: std::collections::HashSet<u8> =
            of_layer(&sent, 2).iter().map(|s| s.frame[1]).collect();
        assert_eq!(gops_seen.len(), 2, "the library wrapped");
    }

    #[test]
    fn the_first_frame_is_a_keyframe_and_the_phase_sets_the_second() {
        let sent = run(&library(4, false), 2_000_000, 20, &Drive::default());
        for layer in 0..3 {
            let keys = key_times_s(&sent, layer);
            assert_eq!(keys[0], 0.0);
            assert!(
                (keys[1] - 3.0).abs() < 0.2,
                "{}",
                format!("layer {layer}: {keys:?}")
            );
            for w in keys[1..].windows(2) {
                assert!(
                    (w[1] - w[0] - 5.0).abs() < 0.2,
                    "{}",
                    format!("layer {layer}: {keys:?}")
                );
            }
        }
    }

    #[test]
    fn random_phases_spread_over_the_keyframe_cycle() {
        let lib = library(4, false);
        let mut second_key: Vec<f64> = Vec::new();
        for i in 0..100u64 {
            let phase_us = i * 50_000;
            second_key.push(key_times_s(&run(&lib, phase_us, 7, &Drive::default()), 2)[1]);
        }
        let min = second_key.iter().cloned().fold(f64::MAX, f64::min);
        let max = second_key.iter().cloned().fold(0.0, f64::max);
        assert!(
            min < 0.3 && max > 4.7,
            "{}",
            format!("second keyframes in [{min}, {max}] s")
        );
    }

    #[test]
    fn the_variant_follows_the_speech_timeline_at_keyframes_only() {
        let drive = Drive {
            speaking: Box::new(|t| (9_900_000..19_900_000).contains(&t)),
            ..Drive::default()
        };
        let sent = run(&library(4, true), 0, 30, &drive);
        assert_decodable(&sent);
        for s in of_layer(&sent, 2) {
            let want = if (2..4).contains(&(s.seq / 150)) {
                SPEAKING
            } else {
                SILENT
            };
            assert_eq!(s.frame[0], want, "{s:?}");
        }
    }

    #[test]
    fn a_publisher_with_no_speaking_variant_stays_silent() {
        let drive = Drive {
            speaking: Box::new(|_| true),
            ..Drive::default()
        };
        let sent = run(&library(2, false), 0, 12, &drive);
        assert!(sent.iter().all(|s| s.frame[0] == SILENT));
    }

    #[test]
    fn a_shed_layer_sends_nothing_and_resumes_its_gop() {
        let drive = Drive {
            active: Box::new(|t| {
                if (4_000_000..6_000_000).contains(&t) {
                    1
                } else {
                    3
                }
            }),
            ..Drive::default()
        };
        let sent = run(&library(4, false), 0, 12, &drive);
        assert_decodable(&sent);
        for layer in [1u32, 2] {
            let l = of_layer(&sent, layer);
            assert!(!l.iter().any(|s| (4_000_000..6_000_000).contains(&s.t_us)));
            let seqs: Vec<u64> = l.iter().map(|s| s.seq).collect();
            assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
            let resumed = l.iter().find(|s| s.t_us >= 6_000_000).unwrap();
            assert!(
                !resumed.key,
                "{}",
                format!("layer {layer} resumes with a delta: {resumed:?}")
            );
        }
        let base = of_layer(&sent, 0);
        assert!(base
            .iter()
            .any(|s| (4_000_000..6_000_000).contains(&s.t_us)));
    }

    /// Every active video layer keys within the cooldown plus one render tick
    /// of each request, forced keys respect the cooldown, and every video layer
    /// keys on each boundary of the publisher's keyframe clock.
    fn assert_requests_answered(sent: &[Sent], phase_us: u64, requests_us: &[u64], secs: u64) {
        let tick_us = 1_000_000 / 30 + 1;
        for layer in 0..3u32 {
            let l = of_layer(sent, layer);
            let keys: Vec<&Sent> = l.iter().filter(|s| s.key).collect();
            for &r in requests_us {
                let bound = r + KEYFRAME_REQUEST_COOLDOWN_US + tick_us;
                assert!(
                    keys.iter().any(|k| k.t_us >= r && k.t_us <= bound),
                    "{}",
                    format!("phase {phase_us} request {r} layer {layer}: no key by {bound}")
                );
            }
            for w in keys.windows(2) {
                if w[1].forced {
                    assert!(
                        w[1].t_us - w[0].t_us + 1 >= KEYFRAME_REQUEST_COOLDOWN_US,
                        "{}",
                        format!("phase {phase_us} layer {layer}: cooldown broken {w:?}")
                    );
                }
            }
            for b in boundaries(phase_us, secs) {
                assert!(
                    keys.iter()
                        .any(|k| !k.forced && k.t_us >= b && k.t_us < b + tick_us),
                    "{}",
                    format!("phase {phase_us} layer {layer}: no periodic key at {b}")
                );
            }
        }
    }

    /// Periodic keyframe times of a publisher with `phase_us`, after the first.
    fn boundaries(phase_us: u64, secs: u64) -> Vec<u64> {
        (1..)
            .map(|n| n * 5_000_000 - phase_us % 5_000_000)
            .take_while(|&b| b + 40_000 < secs * 1_000_000)
            .collect()
    }

    #[test]
    fn keyframe_requests_are_answered_on_every_layer_at_any_phase() {
        let requests = [1_300_000, 2_050_000, 2_100_000, 4_986_600, 7_400_000];
        for phase_ms in (0..5_000).step_by(173) {
            let phase_us = phase_ms * 1000;
            let drive = Drive {
                pli_at_us: requests.to_vec(),
                ..Drive::default()
            };
            let sent = run(&library(4, false), phase_us, 10, &drive);
            assert_decodable(&sent);
            assert_requests_answered(&sent, phase_us, &requests, 10);
        }
    }

    #[test]
    fn a_request_held_by_the_cooldown_is_still_answered_on_the_top_layer() {
        let drive = Drive {
            pli_at_us: vec![2_050_000, 2_100_000],
            ..Drive::default()
        };
        let sent = run(&library(4, false), 0, 4, &drive);
        for layer in 0..3 {
            let keys: Vec<f64> = key_times_s(&sent, layer)
                .into_iter()
                .filter(|k| (2.05..2.45).contains(k))
                .collect();
            assert_eq!(keys.len(), 2, "layer {}: {:?}", layer, keys);
        }
        let drive = Drive {
            pli_at_us: vec![4_986_600],
            ..Drive::default()
        };
        let sent = run(&library(4, false), 25_000, 7, &drive);
        assert!(
            key_times_s(&sent, 2)
                .iter()
                .any(|&k| (4.9866..5.3).contains(&k)),
            "{}",
            format!("{:?}", key_times_s(&sent, 2))
        );
    }

    #[test]
    fn only_request_keyframes_are_marked_forced() {
        let sent = run(&library(4, false), 1_000_000, 12, &Drive::default());
        assert!(sent.iter().all(|s| !s.forced));
        let drive = Drive {
            pli_at_us: vec![2_000_000],
            ..Drive::default()
        };
        let sent = run(&library(4, false), 1_000_000, 12, &drive);
        let forced: Vec<u32> = sent.iter().filter(|s| s.forced).map(|s| s.layer).collect();
        assert_eq!(forced, vec![0, 1, 2]);
    }

    #[test]
    fn steady_requests_do_not_move_the_periodic_keyframes() {
        let drive = Drive {
            pli_at_us: (0..200).map(|i| 1_000_000 + i * 300_000).collect(),
            ..Drive::default()
        };
        let phase_us = 1_234_000;
        let sent = run(&library(4, false), phase_us, 62, &drive);
        assert_decodable(&sent);
        for layer in 0..3 {
            let periodic: Vec<u64> = of_layer(&sent, layer)
                .iter()
                .filter(|s| s.key && !s.forced)
                .map(|s| s.t_us)
                .collect();
            let want: Vec<u64> = boundaries(phase_us, 62);
            assert_eq!(
                periodic.len(),
                want.len() + 1,
                "layer {}: {:?}",
                layer,
                periodic
            );
            for (got, b) in periodic[1..].iter().zip(&want) {
                assert!(
                    *got >= *b && *got < b + 34_000,
                    "{}",
                    format!("layer {layer}: {periodic:?}")
                );
            }
            assert!(of_layer(&sent, layer).iter().filter(|s| s.forced).count() > 150);
        }
    }

    #[test]
    fn a_periodic_keyframe_satisfies_a_held_request() {
        let drive = Drive {
            pli_at_us: vec![4_800_000, 4_900_000],
            ..Drive::default()
        };
        let sent = run(&library(4, false), 0, 7, &drive);
        for layer in 0..3 {
            let forced: Vec<u64> = of_layer(&sent, layer)
                .iter()
                .filter(|s| s.forced)
                .map(|s| s.t_us)
                .collect();
            assert_eq!(forced.len(), 1, "layer {}: {:?}", layer, forced);
            assert!(forced[0] < 5_000_000, "layer {}: {:?}", layer, forced);
        }
    }

    #[test]
    fn a_wider_cooldown_holds_requests_longer() {
        let drive = Drive {
            pli_at_us: vec![1_000_000, 2_500_000],
            cooldown_us: CONGESTED_KEYFRAME_REQUEST_COOLDOWN_US,
            ..Drive::default()
        };
        let sent = run(&library(4, false), 0, 5, &drive);
        for layer in 0..3 {
            let forced: Vec<u64> = of_layer(&sent, layer)
                .iter()
                .filter(|s| s.forced)
                .map(|s| s.t_us)
                .collect();
            assert_eq!(forced.len(), 2, "layer {}: {:?}", layer, forced);
            assert!(forced[1] - forced[0] >= 2_000_000 && forced[1] - forced[0] < 2_040_000);
        }
    }

    #[test]
    fn the_build_pool_is_bounded_and_split_between_the_variants() {
        assert_eq!(threads_for(1), 2);
        assert_eq!(threads_for(4), 2);
        assert_eq!(threads_for(6), 3);
        assert_eq!(threads_for(8), 4);
        assert_eq!(threads_for(64), 4);
        assert_eq!(chunk_plan(4, 4), vec![(0, 2), (2, 2)]);
        assert_eq!(chunk_plan(8, 4), vec![(0, 4), (4, 4)]);
        assert_eq!(chunk_plan(3, 4), vec![(0, 2), (2, 1)]);
        assert_eq!(chunk_plan(4, 2), vec![(0, 4)]);
    }

    #[test]
    fn publisher_draws_cover_the_keyframe_cycle_and_the_render_tick() {
        let mut rng = StdRng::seed_from_u64(7);
        let draws: Vec<PublisherDraw> = (0..200)
            .map(|_| PublisherDraw::new(&mut rng, 5_000_000, 33_333))
            .collect();
        let span = |f: &dyn Fn(&PublisherDraw) -> u64| {
            draws.iter().map(f).max().unwrap() - draws.iter().map(f).min().unwrap()
        };
        assert!(span(&|d| d.phase_us) > 4_500_000);
        assert!(span(&|d| d.tick_offset_us) > 30_000);
        assert!(draws
            .iter()
            .all(|d| d.phase_us < 5_000_000 && d.tick_offset_us < 33_333));
        let seeded =
            |user: &str| PublisherDraw::new(&mut publisher_rng(Some(9), user), 5_000_000, 33_333);
        assert_eq!(seeded("bot-002"), seeded("bot-002"), "a seed repeats a run");
        assert_ne!(
            seeded("bot-002"),
            seeded("bot-003"),
            "publishers draw apart"
        );
    }

    #[test]
    fn controls_follow_the_aq_active_count_and_pass_on_requests() {
        let lib = library(4, false);
        let aq = BotAq::with_default_clock();
        aq.set_simulcast_layers(3);
        let requests = AtomicU64::new(0);
        let mut controls = ReplayControls::new(&aq, Some(&requests), None, 3);
        let mut sched = ReplaySchedule::new(&lib, 0, 0);
        assert_eq!(controls.poll(&mut sched, "b", 0).0, 3);
        aq.observe_layer_hint(0);
        aq.tick();
        assert_eq!(controls.poll(&mut sched, "b", 0).0, 1);

        let mut keys = Vec::new();
        let mut tick_at = |sched: &mut ReplaySchedule, t_us: u64| {
            sched.tick(
                &lib,
                t_us,
                1,
                KEYFRAME_REQUEST_COOLDOWN_US,
                &|_| false,
                &mut |e| keys.push((t_us, e.layer_id, e.key, e.forced)),
            )
        };
        tick_at(&mut sched, 0);
        tick_at(&mut sched, 1_000_000);
        requests.fetch_add(1, Ordering::Relaxed);
        controls.poll(&mut sched, "b", 1_033_333);
        tick_at(&mut sched, 1_033_333);
        assert_eq!(keys.last(), Some(&(1_033_333, 0, true, true)));

        let mut ignoring = ReplayControls::new(&aq, None, None, 3);
        let mut sched = ReplaySchedule::new(&lib, 0, 0);
        requests.fetch_add(1, Ordering::Relaxed);
        ignoring.poll(&mut sched, "b", 0);
        let mut forced = false;
        for t_us in [0, 400_000, 433_333] {
            sched.tick(
                &lib,
                t_us,
                1,
                KEYFRAME_REQUEST_COOLDOWN_US,
                &|_| false,
                &mut |e| forced |= e.forced,
            );
        }
        assert!(!forced, "no requests without a signal");
    }

    #[test]
    fn an_active_layer_count_change_keys_every_active_layer_inside_the_cooldown() {
        let lib = library(4, false);
        let ticks = |reset: bool| {
            let mut sched = ReplaySchedule::new(&lib, 0, 0);
            let mut keys = Vec::new();
            for tick in 0..30u64 {
                let t_us = tick * 33_333;
                if tick == 3 {
                    sched.request_tier_keyframe(reset);
                }
                sched.tick(
                    &lib,
                    t_us,
                    3,
                    KEYFRAME_REQUEST_COOLDOWN_US,
                    &|_| false,
                    &mut |e| {
                        if e.key && t_us > 0 {
                            keys.push((t_us, e.layer_id, e.forced, e.tier_change))
                        }
                    },
                );
            }
            keys
        };
        assert_eq!(
            ticks(true),
            vec![
                (99_999, 0, false, true),
                (99_999, 1, false, true),
                (99_999, 2, false, true)
            ],
            "no cooldown, as the browser's tier change"
        );
        let held = ticks(false);
        assert_eq!(held.len(), 3, "{:?}", held);
        assert!(held.iter().all(|k| k.0 >= 250_000 && k.3), "{:?}", held);
    }

    #[test]
    fn controls_turn_an_aq_tier_change_into_an_all_layer_keyframe() {
        let lib = library(4, false);
        let requests = AtomicU64::new(0);
        for (answering, want) in [(true, true), (false, false)] {
            let aq = BotAq::with_default_clock();
            aq.set_simulcast_layers(3);
            let mut controls = ReplayControls::new(&aq, answering.then_some(&requests), None, 3);
            let mut sched = ReplaySchedule::new(&lib, 0, 0);
            sched.tick(
                &lib,
                0,
                3,
                KEYFRAME_REQUEST_COOLDOWN_US,
                &|_| false,
                &mut |_| {},
            );
            let epoch = aq.tier_epoch();
            aq.observe_layer_hint(0);
            aq.tick();
            assert_ne!(
                aq.tier_epoch(),
                epoch,
                "the active layer count change is a tier change"
            );
            let (active, cooldown) = controls.poll(&mut sched, "b", 33_333);
            let mut tier = false;
            sched.tick(&lib, 33_333, active, cooldown, &|_| false, &mut |e| {
                tier |= e.tier_change
            });
            assert_eq!(tier, want, "answering = {}", answering);
            let mut again = 0;
            for tick in 2..30u64 {
                let t_us = tick * 33_333;
                let (active, cooldown) = controls.poll(&mut sched, "b", t_us);
                sched.tick(&lib, t_us, active, cooldown, &|_| false, &mut |e| {
                    again += e.tier_change as u32
                });
            }
            assert_eq!(again, 0, "one tier change keys once");
        }
    }

    #[test]
    fn a_congested_websocket_keeps_the_cooldown_on_a_tier_change() {
        let lib = library(4, false);
        let aq = BotAq::with_default_clock();
        aq.set_simulcast_layers(3);
        let requests = AtomicU64::new(0);
        let wait = Arc::new(AtomicU64::new(0));
        let w = Arc::clone(&wait);
        let probe = UplinkProbe {
            websocket: true,
            wait_us: Box::new(move || w.load(Ordering::Relaxed)),
        };
        let mut controls = ReplayControls::new(&aq, Some(&requests), Some(&probe), 3);
        let mut sched = ReplaySchedule::new(&lib, 0, 0);
        sched.tick(
            &lib,
            0,
            3,
            KEYFRAME_REQUEST_COOLDOWN_US,
            &|_| false,
            &mut |_| {},
        );
        wait.fetch_add(10_000, Ordering::Relaxed);
        aq.observe_layer_hint(0);
        aq.tick();
        let (active, cooldown) = controls.poll(&mut sched, "b", 33_333);
        assert_eq!(cooldown, CONGESTED_KEYFRAME_REQUEST_COOLDOWN_US);
        let mut tier = false;
        sched.tick(&lib, 33_333, active, cooldown, &|_| false, &mut |e| {
            tier |= e.tier_change
        });
        assert!(
            !tier,
            "the congested cooldown holds the tier-change keyframe"
        );
    }

    #[test]
    fn forced_keyframe_counts_split_by_cause() {
        let mut counts = ForcedKeyframeCounts::new(3);
        counts.record(0, true, false);
        counts.record(2, false, true);
        counts.record(1, true, true);
        assert_eq!(counts.request(), &[1, 1, 0]);
        assert_eq!(counts.tier_change(), &[0, 1, 1]);
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn forced_keyframe_counts_reach_the_metric() {
        let registry = Arc::new(prometheus::Registry::new());
        let metrics = crate::metrics_server::BotMetrics::new(registry).expect("metrics");
        let mut counts = ForcedKeyframeCounts::new(3).with_metrics(&metrics, "b", "m");
        counts.record(2, true, false);
        counts.record(2, false, true);
        counts.record(2, false, true);
        let value = |cause: &str| {
            metrics
                .replay_forced_keyframes_total
                .with_label_values(&["b", "m", "2", cause])
                .get()
        };
        assert_eq!((value("request"), value("tier_change")), (1, 2));
    }

    #[test]
    fn the_uplink_probe_follows_the_transport_and_the_shim() {
        use crate::config::Transport;
        use crate::netsim::{Direction, NetSimShim, NetworkProfile};
        let shim = || {
            Some(Arc::new(NetSimShim::new(
                NetworkProfile::passthrough(),
                Direction::Up,
            )))
        };
        let ws = uplink_probe(shim(), &Transport::WebSocket).expect("probe");
        assert!(ws.websocket);
        assert_eq!((ws.wait_us)(), 0);
        assert!(
            !uplink_probe(shim(), &Transport::WebTransport)
                .expect("probe")
                .websocket
        );
        assert!(uplink_probe(None, &Transport::WebSocket).is_none());
    }

    #[test]
    fn the_cooldown_widens_only_on_a_saturated_websocket_uplink() {
        let aq = BotAq::with_default_clock();
        aq.set_simulcast_layers(3);
        let lib = library(1, false);
        let cooldown = |websocket: bool, advancing: bool| {
            let wait = Arc::new(AtomicU64::new(0));
            let w = Arc::clone(&wait);
            let probe = UplinkProbe {
                websocket,
                wait_us: Box::new(move || w.load(Ordering::Relaxed)),
            };
            let mut controls = ReplayControls::new(&aq, None, Some(&probe), 3);
            let mut sched = ReplaySchedule::new(&lib, 0, 0);
            if advancing {
                wait.fetch_add(5_000, Ordering::Relaxed);
            }
            let now = controls.poll(&mut sched, "b", 10_000_000).1;
            let later = controls.poll(&mut sched, "b", 11_500_000).1;
            (now, later)
        };
        let congested = CONGESTED_KEYFRAME_REQUEST_COOLDOWN_US;
        assert_eq!(
            cooldown(true, true),
            (congested, KEYFRAME_REQUEST_COOLDOWN_US)
        );
        assert_eq!(
            cooldown(true, false),
            (KEYFRAME_REQUEST_COOLDOWN_US, KEYFRAME_REQUEST_COOLDOWN_US)
        );
        assert_eq!(
            cooldown(false, true),
            (KEYFRAME_REQUEST_COOLDOWN_US, KEYFRAME_REQUEST_COOLDOWN_US)
        );
        assert_eq!(
            cooldown(false, false),
            (KEYFRAME_REQUEST_COOLDOWN_US, KEYFRAME_REQUEST_COOLDOWN_US)
        );
        let mut controls = ReplayControls::new(&aq, None, None, 3);
        let mut sched = ReplaySchedule::new(&lib, 0, 0);
        assert_eq!(
            controls.poll(&mut sched, "b", 0).1,
            KEYFRAME_REQUEST_COOLDOWN_US,
            "no shim"
        );
        assert_eq!(congested, 2_000_000);
    }

    #[test]
    fn a_keyframe_request_only_hits_active_layers() {
        let drive = Drive {
            active: Box::new(|t| {
                if (1_000_000..3_000_000).contains(&t) {
                    1
                } else {
                    3
                }
            }),
            pli_at_us: vec![2_000_000],
            ..Drive::default()
        };
        let sent = run(&library(4, false), 0, 4, &drive);
        assert_decodable(&sent);
        assert!(key_times_s(&sent, 0)
            .iter()
            .any(|&k| (2.0..2.2).contains(&k)));
        let top = of_layer(&sent, 2);
        let resumed = top.iter().find(|s| s.t_us >= 3_000_000).unwrap();
        assert!(!resumed.key, "{}", format!("{resumed:?}"));
    }

    #[test]
    fn speech_majority_uses_the_renderer_predicate() {
        let rms: Vec<f32> = (0..300).map(|i| if i < 150 { 0.5 } else { 0.0 }).collect();
        let tl = SpeechTimeline::new(&rms, 0.5, 15, 30, Duration::from_secs(20));
        assert!(tl.speaking_majority(0, 5_000_000));
        assert!(!tl.speaking_majority(300, 5_000_000));
        assert!(tl.speaking_majority(150, 10_000_000));
        assert!(!tl.speaking_majority(200, 10_000_000));
        assert!(tl.speaking_majority(500, 10_000_000), "wraps the loop");
        let quiet = SpeechTimeline::new(&[0.005; 300], 0.5, 15, 30, Duration::from_secs(20));
        assert!(
            !quiet.speaking_majority(0, 5_000_000),
            "rms <= 0.01 is not speech"
        );
    }

    #[test]
    fn a_window_longer_than_the_loop_counts_whole_loops() {
        let rms: Vec<f32> = (0..30).map(|i| if i < 20 { 0.5 } else { 0.0 }).collect();
        let tl = SpeechTimeline::new(&rms, 0.5, 15, 30, Duration::from_secs(2));
        assert!(tl.speaking_majority(50, 5_000_000));
    }

    #[test]
    fn the_library_holds_closed_gops_and_speech_costs_more_bytes() {
        let speech = SpeechTrack {
            rms: (0..150)
                .map(|i| {
                    if i < 15 {
                        0.2 + (i % 10) as f32 * 0.05
                    } else {
                        0.0
                    }
                })
                .collect(),
            max_rms: 0.7,
        };
        let lib = build_library_with(
            3,
            [0, 220, 120],
            &[speech],
            15,
            1,
            4,
            &AtomicBool::new(false),
        )
        .expect("build");
        let ladder = videocall_aq::constants::simulcast_layers(3);
        assert_eq!(lib.layers().len(), 3);
        for (layer, tier) in lib.layers().iter().zip(ladder) {
            assert_eq!(
                (layer.width, layer.height),
                (tier.max_width, tier.max_height)
            );
            for v in [Variant::Silent, Variant::Speaking] {
                assert_eq!(layer.gops(v), 1, "the warm-up GOP is dropped");
                let frames = layer.frames(v, 0);
                assert_eq!(frames.len() as u32, tier.keyframe_interval_frames);
            }
        }
        let top = &lib.layers()[2];
        assert!(
            top.bytes(Variant::Speaking) >= 3 * top.bytes(Variant::Silent),
            "speaking {} vs silent {}",
            top.bytes(Variant::Speaking),
            top.bytes(Variant::Silent)
        );
        for layer in lib.layers() {
            for v in [Variant::Silent, Variant::Speaking] {
                let frames = layer.frames(v, 0);
                assert!(is_vp9_keyframe(frames[0]), "layer {} {v:?}", layer.layer_id);
                assert!(frames[1..].iter().all(|f| !is_vp9_keyframe(f)));
            }
        }
    }

    #[test]
    fn a_process_without_talkers_builds_no_speaking_variant() {
        let lib =
            build_library(2, [255, 255, 255], &[], 15, 3, &AtomicBool::new(false)).expect("build");
        assert_eq!(lib.build_threads(), build_threads(), "the host's pool");
        assert_eq!(lib.layers().len(), 2);
        for layer in lib.layers() {
            assert_eq!(layer.gops(Variant::Speaking), 0);
            assert_eq!(layer.gops(Variant::Silent), 3, "two build chunks, merged");
            for g in 0..3 {
                let frames = layer.frames(Variant::Silent, g);
                assert_eq!(frames.len() as u32, layer.gop_frames);
                assert!(is_vp9_keyframe(frames[0]));
                assert!(frames[1..].iter().all(|f| !is_vp9_keyframe(f)));
            }
        }
        assert!(
            lib.layers()
                .iter()
                .any(|l| l.frames(Variant::Silent, 0) != l.frames(Variant::Silent, 2)),
            "each chunk renders its own part of the source"
        );
    }

    /// VP9 uncompressed header: frame_marker 0b10, then profile bits, then
    /// show_existing_frame, then frame_type (0 = key). Profile 0 only.
    fn is_vp9_keyframe(frame: &[u8]) -> bool {
        frame
            .first()
            .is_some_and(|&b| b >> 6 == 0b10 && (b >> 3) & 1 == 0 && (b >> 2) & 1 == 0)
    }
}
