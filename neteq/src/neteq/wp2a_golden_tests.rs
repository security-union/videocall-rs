// Included from expand.rs as `expand::golden_tests`.
//
// Golden tests for background_noise.rs, expand.rs, normal.rs and merge.rs.
// The expected hashes come from the real WebRTC C++ (BackgroundNoise, Expand,
// Normal, Merge) at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd, built by
// `spl/golden/build_wp2a.sh` from `spl/golden/golden_gen_wp2a.cc`, which this
// file mirrors line by line: same xorshift32 draws in the same order, same
// values folded into one FNV-1a hash per group. A mismatch means the port is
// not bit-exact.
//
// `speech_like` is also used by the ported unit tests in place of WebRTC's
// `testfile32kHz.pcm` resource, which the sparse clone does not have.

use std::sync::Arc;

use crate::api::neteq::Mode;
use crate::codecs::BuiltinAudioDecoderFactory;
use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::audio_vector::AudioVector;
use crate::neteq::background_noise::BackgroundNoise;
use crate::neteq::decoder_database::DecoderDatabase;
use crate::neteq::expand::{Expand, ExpandContext};
use crate::neteq::merge::Merge;
use crate::neteq::normal::Normal;
use crate::neteq::random_vector::RandomVector;
use crate::neteq::statistics_calculator::StatisticsCalculator;
use crate::neteq::sync_buffer::SyncBuffer;
use crate::spl::test_util::{Hash, Rng};

const K_RATES: [i32; 4] = [8000, 16000, 32000, 48000];

// ---------------------------------------------------------------------------
// Deterministic test signals (integer only, mirrored from C++).

/// Pulse train of `period` samples through a two-pole resonator, plus noise.
fn gen_voiced(r: &mut Rng, out: &mut [i16], period: i32, amp: i32, noise: i32) {
    let (mut y1, mut y2): (i64, i64) = (0, 0);
    for (n, sample) in out.iter_mut().enumerate() {
        let mut x: i64 = if n % period as usize == 0 {
            amp as i64
        } else {
            0
        };
        x += r.range(-noise, noise) as i64;
        let y = (x + ((29000 * y1 - 15000 * y2) >> 14)).clamp(-32768, 32767);
        y2 = y1;
        y1 = y;
        *sample = y as i16;
    }
}

fn gen_noise(r: &mut Rng, out: &mut [i16], amp: i32) {
    for sample in out.iter_mut() {
        *sample = r.range(-amp, amp) as i16;
    }
}

/// kind: 0 voiced, 1 noise, 2 low noise, 3 loud voiced (saturating),
/// 4 silence, 5 onset (quiet, then voiced at the end), 6 DC (singular LPC).
fn gen_signal(r: &mut Rng, kind: usize, out: &mut [i16], fs_mult: i32) {
    match kind {
        0 => {
            let period = r.range(20 * fs_mult, 115 * fs_mult);
            let amp = r.range(200, 1500);
            gen_voiced(r, out, period, amp, 30);
        }
        1 => {
            let amp = r.range(500, 4000);
            gen_noise(r, out, amp);
        }
        2 => {
            let amp = r.range(1, 40);
            gen_noise(r, out, amp);
        }
        3 => {
            let period = r.range(20 * fs_mult, 115 * fs_mult);
            gen_voiced(r, out, period, 9000, 3000);
        }
        4 => out.fill(0),
        6 => {
            let dc = r.range(-20000, 20000) as i16;
            out.fill(dc);
        }
        _ => {
            let len = out.len();
            let tail = len.min((r.range(10, 150) * fs_mult) as usize);
            gen_noise(r, &mut out[..len - tail], 10);
            let period = r.range(20 * fs_mult, 115 * fs_mult);
            gen_voiced(r, &mut out[len - tail..], period, 1200, 30);
        }
    }
}

/// A voiced test signal for the ported unit tests (stands in for
/// `testfile32kHz.pcm`). `variant` shifts the pitch and the noise draws.
pub(crate) fn speech_like(len: usize, fs: i32, variant: u32) -> Vec<i16> {
    let mut r = Rng::new();
    for _ in 0..variant {
        r.next();
    }
    let fs_mult = fs / 8000;
    let mut out = vec![0i16; len];
    gen_voiced(&mut r, &mut out, 57 * fs_mult + variant as i32, 1000, 30);
    out
}

fn fill_sync(r: &mut Rng, sb: &mut SyncBuffer, kind: usize, fs_mult: i32) {
    let mut tmp = vec![0i16; sb.size()];
    for ch in 0..sb.channels() {
        gen_signal(r, kind, &mut tmp, fs_mult);
        sb.channel_mut(ch).overwrite_at_slice(&tmp, 0);
    }
}

/// Low-level noise into a separate vector, then `BackgroundNoise::update` a
/// few times so the estimate is initialized.
fn prime_bgn(r: &mut Rng, bgn: &mut BackgroundNoise, channels: usize) {
    let mut v = AudioMultiVector::with_size(channels, 256);
    let mut tmp = [0i16; 256];
    let rounds = r.range(1, 3);
    for _ in 0..rounds {
        for ch in 0..channels {
            let amp = r.range(1, 40);
            gen_noise(r, &mut tmp, amp);
            v[ch].overwrite_at_slice(&tmp, 0);
        }
        bgn.update(&v);
    }
}

fn feed_vec(h: &mut Hash, v: &AudioVector) {
    h.feed(v.size() as i64);
    for i in 0..v.size() {
        h.feed(v[i] as i64);
    }
}

fn feed_multi(h: &mut Hash, v: &AudioMultiVector) {
    h.feed(v.channels() as i64);
    for ch in 0..v.channels() {
        feed_vec(h, &v[ch]);
    }
}

fn feed_tail(h: &mut Hash, sb: &SyncBuffer, n: usize) {
    h.feed(sb.size() as i64);
    h.feed(sb.next_index() as i64);
    let n = n.min(sb.size());
    for ch in 0..sb.channels() {
        for i in sb.size() - n..sb.size() {
            h.feed(sb[ch][i] as i64);
        }
    }
}

fn feed_bgn(h: &mut Hash, bgn: &BackgroundNoise, channels: usize) {
    h.feed(bgn.initialized() as i64);
    for ch in 0..channels {
        h.feed(bgn.energy(ch) as i64);
        h.feed(bgn.mute_factor(ch) as i64);
        h.feed(bgn.scale(ch) as i64);
        h.feed(bgn.scale_shift(ch) as i64);
        h.feed16(bgn.filter(ch));
        h.feed16(bgn.filter_state(ch));
    }
}

fn feed_stats(h: &mut Hash, stats: &StatisticsCalculator) {
    let s = stats.get_lifetime_statistics();
    h.feed(s.delayed_packet_outage_samples as i64);
    h.feed(s.delayed_packet_outage_events as i64);
    h.feed(s.concealed_samples as i64);
    h.feed(s.concealment_events as i64);
    h.feed(s.interruption_count as i64);
    h.feed(s.total_interruption_duration_ms as i64);
}

fn feed_expand(h: &mut Hash, e: &Expand, channels: usize) {
    for ch in 0..channels {
        h.feed(e.mute_factor(ch) as i64);
    }
    h.feed(e.muted() as i64);
    h.feed(e.max_lag() as i64);
    h.feed(e.overlap_length() as i64);
}

/// Bundles what `NetEqImpl` owns around `Expand`.
struct Rig {
    fs_mult: i32,
    channels: usize,
    bgn: BackgroundNoise,
    sb: SyncBuffer,
    rv: RandomVector,
    stats: StatisticsCalculator,
    expand: Expand,
}

impl Rig {
    fn new(fs: i32, channels: usize, sb_len: usize) -> Self {
        Self {
            fs_mult: fs / 8000,
            channels,
            bgn: BackgroundNoise::new(channels),
            sb: SyncBuffer::new(channels, sb_len),
            rv: RandomVector::new(),
            stats: StatisticsCalculator::new(),
            expand: Expand::new(fs, channels),
        }
    }

    /// `Expand` plus the context it borrows, split so both can be used.
    fn parts(&mut self) -> (&mut Expand, ExpandContext<'_>) {
        (
            &mut self.expand,
            ExpandContext {
                sync_buffer: &mut self.sb,
                background_noise: &mut self.bgn,
                random_vector: &mut self.rv,
                statistics: &mut self.stats,
            },
        )
    }
}

/// Common setup: fill the sync buffer, maybe prime BGN and the random vector.
fn setup_rig(r: &mut Rng, rig: &mut Rig, kind: usize) {
    fill_sync(r, &mut rig.sb, kind, rig.fs_mult);
    if r.below(2) == 0 {
        prime_bgn(r, &mut rig.bgn, rig.channels);
    }
    if r.below(3) == 0 {
        rig.rv.set_seed_increment(r.range(-300, 300) as i16);
    }
    rig.stats.decoded_output_played();
}

/// One `Expand::process` call with the NetEqImpl-style bookkeeping around it.
fn expand_once(r: &mut Rng, h: &mut Hash, rig: &mut Rig, out: &mut AudioMultiVector, first: bool) {
    if r.below(3) == 0 {
        out.clear();
    }
    let (expand, mut ctx) = rig.parts();
    h.feed(expand.process(&mut ctx, out) as i64);
    rig.stats.expanded_voice_samples(out.size(), first);
    feed_multi(h, out);
    feed_expand(h, &rig.expand, rig.channels);
    feed_tail(h, &rig.sb, 2 * rig.expand.overlap_length());
    h.feed(rig.bgn.mute_factor(0) as i64);
    h.feed16(rig.bgn.filter_state(0));
    h.feed(rig.rv.seed_increment() as i64);
    if r.below(2) == 0 {
        rig.sb.push_back(out);
    }
}

// ---------------------------------------------------------------------------

#[test]
fn golden_wp2a_background_noise() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    let mut bgn1 = BackgroundNoise::new(1);
    let mut bgn2 = BackgroundNoise::new(2);
    for _ in 0..400 {
        let channels = 1 + r.below(2);
        let bgn = if channels == 1 { &mut bgn1 } else { &mut bgn2 };
        let fs_mult = 1 << r.below(3);
        if r.below(40) == 0 {
            bgn.reset();
        }
        let len = 256 + r.below(300);
        let mut v = AudioMultiVector::with_size(channels, len);
        let mut tmp = vec![0i16; len];
        for ch in 0..channels {
            // Bias towards quiet signals so the estimate actually updates.
            let kind = if r.below(3) == 0 { r.below(7) } else { 2 };
            gen_signal(&mut r, kind, &mut tmp, fs_mult);
            v[ch].overwrite_at_slice(&tmp, 0);
        }
        h.feed(bgn.update(&v) as i64);
        feed_bgn(&mut h, bgn, channels);
        if r.below(2) == 0 {
            let ch = r.below(channels);
            let num = r.urange(8, 750);
            let random: Vec<i16> = (0..750).map(|_| r.range(-8000, 8000) as i16).collect();
            bgn.set_mute_factor(ch, r.range(0, 16384) as i16);
            let mut buffer: Vec<i16> = (0..8 + num).map(|_| r.i16()).collect();
            let mute_slope = r.range(0, 5000);
            let too_many = r.below(2) == 1;
            bgn.generate_background_noise(&random, ch, mute_slope, too_many, num, &mut buffer);
            h.feed16(&buffer);
            feed_bgn(&mut h, bgn, channels);
        }
    }
    assert_eq!(0xcfd02f1cee2f1f5b, h.value());
}

#[test]
fn golden_wp2a_expand() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for fs in K_RATES {
        for channels in 1..=2usize {
            for kind in 0..7usize {
                let fs_mult = fs / 8000;
                let mut rig = Rig::new(fs, channels, (300 + r.below(100)) * fs_mult as usize);
                setup_rig(&mut r, &mut rig, kind);
                let mut out = AudioMultiVector::new(channels);
                // One long run per rate goes past kMaxConsecutiveExpands.
                let calls = if kind == 0 && channels == 1 { 210 } else { 14 };
                let mut first = true;
                for _ in 0..calls {
                    // The long run takes no detours so it reaches the cap.
                    let action = if calls > 100 { 99 } else { r.below(16) };
                    match action {
                        0 => {
                            rig.expand.reset();
                            first = true;
                        }
                        1 => rig
                            .expand
                            .set_parameters_for_merge_after_expand(&mut rig.stats),
                        2 => rig
                            .expand
                            .set_parameters_for_normal_after_expand(&mut rig.stats),
                        _ => {}
                    }
                    expand_once(&mut r, &mut h, &mut rig, &mut out, first);
                    first = false;
                }
                feed_stats(&mut h, &rig.stats);
                feed_bgn(&mut h, &rig.bgn, channels);
            }
        }
    }
    assert_eq!(0xeabfc25bbe23d210, h.value());
}

#[test]
fn golden_wp2a_normal() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    let mut db = DecoderDatabase::new(Arc::new(BuiltinAudioDecoderFactory::new()));
    for fs in K_RATES {
        for channels in 1..=2usize {
            for _ in 0..12 {
                let fs_mult = fs / 8000;
                let mut rig = Rig::new(fs, channels, (300 + r.below(100)) * fs_mult as usize);
                let kind = r.below(7);
                setup_rig(&mut r, &mut rig, kind);
                let mut normal = Normal::new(fs);
                let mut out = AudioMultiVector::new(channels);
                let expands = r.below(4);
                for k in 0..expands {
                    expand_once(&mut r, &mut h, &mut rig, &mut out, k == 0);
                }

                let mode = match r.below(8) {
                    0 => Mode::Normal,
                    1 => Mode::CodecPlc,
                    2 => Mode::Merge,
                    _ => Mode::Expand,
                };
                let per_channel = match r.below(4) {
                    0 => (fs / 100) as usize,
                    1 => (fs / 50) as usize,
                    2 => 63,
                    _ => r.range(1, 3 * fs / 100) as usize,
                };
                let mut length = per_channel * channels;
                if r.below(10) == 0 {
                    length += 1;
                }
                let mut input = vec![0i16; length + 1];
                let kind = r.below(4);
                gen_signal(&mut r, kind, &mut input, fs_mult);
                out.clear();
                let (expand, mut ctx) = rig.parts();
                let ret = normal.process(&input, length, mode, &mut out, expand, &mut ctx, &mut db);
                h.feed(ret as i64);
                feed_multi(&mut h, &out);
                feed_expand(&mut h, &rig.expand, channels);
                feed_stats(&mut h, &rig.stats);
                feed_tail(&mut h, &rig.sb, 2 * rig.expand.overlap_length());
            }
        }
    }
    assert_eq!(0xfaed5b9e8df805b5, h.value());
}

#[test]
fn golden_wp2a_merge() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for fs in K_RATES {
        for channels in 1..=2usize {
            for _ in 0..12 {
                let fs_mult = fs / 8000;
                let overlap = 5 * fs_mult as usize;
                let tpc = (fs / 100) as usize;
                let mut rig = Rig::new(fs, channels, (720 * fs / 1000) as usize);
                let kind = r.below(7);
                setup_rig(&mut r, &mut rig, kind);
                let mut merge = Merge::new(fs, channels);
                let mut out = AudioMultiVector::new(channels);
                let expands = 1 + r.below(3);
                for k in 0..expands {
                    expand_once(&mut r, &mut h, &mut rig, &mut out, k == 0);
                }

                // How much of the sync buffer is still unplayed (Merge's old_length).
                let mut future = match r.below(4) {
                    0 => overlap,
                    1 => tpc / 2,
                    2 => tpc + overlap,
                    _ => r.urange(overlap, tpc + overlap),
                };
                let big = fs == 48000 && r.below(4) == 0;
                if big {
                    future = 1260 + r.below(200); // The >= 210 * 6 branch.
                }
                let next_index = rig.sb.size() - future;
                rig.sb.set_next_index(next_index);

                let mut per_channel = match r.below(7) {
                    0 => 1,
                    1 => 2,
                    2 => 3,
                    3 => tpc,
                    4 => tpc + 1,
                    5 => 2 * tpc,
                    _ => r.urange(1, 4 * tpc),
                };
                // Keep the peak search inside its buffer (C++ over-reads otherwise).
                let start = future.min(1260).max(tpc + overlap);
                let pad = overlap - 1;
                if per_channel < start && (start - per_channel) / (2 * fs_mult as usize) > 60 + pad
                {
                    per_channel = start;
                }

                let mut input = vec![0i16; per_channel * channels];
                let kind = r.below(4);
                gen_signal(&mut r, kind, &mut input, fs_mult);
                out.clear();
                let input_len = input.len();
                let (expand, mut ctx) = rig.parts();
                let ret = merge.process(&mut input, input_len, &mut out, expand, &mut ctx);
                h.feed(ret as i64);
                feed_multi(&mut h, &out);
                feed_expand(&mut h, &rig.expand, channels);
                feed_stats(&mut h, &rig.stats);
                h.feed(rig.sb.future_length() as i64);
                feed_tail(&mut h, &rig.sb, future + 2 * overlap);
                h.feed(merge.required_future_samples() as i64);
            }
        }
    }
    assert_eq!(0x0c4f3f456c08f312, h.value());
}
