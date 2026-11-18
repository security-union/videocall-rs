//! Deterministic speech-like test signal. No external audio files.
//!
//! The synth alternates voiced segments (a gliding glottal pitch with
//! harmonics shaped by three formant bumps and a syllable-rate envelope),
//! short unvoiced fricative bursts (differentiated noise), and silences with
//! a faint noise floor, so NetEq sees realistic pitch for expand / merge /
//! time-stretch and real gaps for CNG and DTX.

use std::f64::consts::TAU;

use crate::rng::Rng;

/// Segment the synth is currently producing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Harmonic, pitched speech.
    Voiced,
    /// Fricative noise burst.
    Unvoiced,
    /// Near-silence (noise floor only).
    Silence,
}

/// Segment-length knobs, in ms.
#[derive(Debug, Clone, Copy)]
pub struct SynthConfig {
    /// Voiced segment length range.
    pub voiced_ms: (u32, u32),
    /// Unvoiced burst length range.
    pub unvoiced_ms: (u32, u32),
    /// Silence length range.
    pub silence_ms: (u32, u32),
    /// Probability that a voiced segment is followed by silence (otherwise
    /// an unvoiced burst or another voiced segment).
    pub silence_probability: f64,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            voiced_ms: (250, 1200),
            unvoiced_ms: (40, 140),
            silence_ms: (80, 600),
            silence_probability: 0.35,
        }
    }
}

impl SynthConfig {
    /// Longer pauses, for DTX / comfort-noise scenarios.
    pub fn with_long_silences() -> Self {
        Self {
            silence_ms: (400, 2500),
            silence_probability: 0.6,
            ..Self::default()
        }
    }
}

/// Speech-like signal generator at a fixed sample rate and channel count.
#[derive(Debug, Clone)]
pub struct SpeechSynth {
    rng: Rng,
    config: SynthConfig,
    sample_rate_hz: u32,
    channels: u8,
    kind: SegmentKind,
    remaining: u64,
    seg_len: u64,
    seg_pos: u64,
    phase: f64,
    f0_start: f64,
    f0_end: f64,
    formants: [f64; 3],
    level: f64,
    prev_noise: f64,
    // Small delay line for a decorrelated-but-related second channel.
    history: Vec<f64>,
    history_pos: usize,
}

impl SpeechSynth {
    /// Creates a synth; identical arguments give identical output.
    pub fn new(seed: u64, sample_rate_hz: u32, channels: u8, config: SynthConfig) -> Self {
        let mut s = Self {
            rng: Rng::new(seed ^ 0x5EED_5917_A11C_0DE5),
            config,
            sample_rate_hz,
            channels,
            kind: SegmentKind::Silence,
            remaining: 0,
            seg_len: 0,
            seg_pos: 0,
            phase: 0.0,
            f0_start: 140.0,
            f0_end: 140.0,
            formants: [600.0, 1500.0, 2500.0],
            level: 0.0,
            prev_noise: 0.0,
            history: vec![0.0; 64],
            history_pos: 0,
        };
        // Start with a short lead-in silence so the first packets are quiet.
        s.start_segment(SegmentKind::Silence, 60);
        s
    }

    /// Kind of the segment the next sample belongs to.
    pub fn current_kind(&self) -> SegmentKind {
        self.kind
    }

    fn ms_to_samples(&self, ms: u32) -> u64 {
        u64::from(ms) * u64::from(self.sample_rate_hz) / 1000
    }

    fn start_segment(&mut self, kind: SegmentKind, ms: u32) {
        self.kind = kind;
        self.seg_len = self.ms_to_samples(ms).max(1);
        self.remaining = self.seg_len;
        self.seg_pos = 0;
        if kind == SegmentKind::Voiced {
            self.f0_start = self.rng.range(95.0, 230.0);
            self.f0_end = (self.f0_start * self.rng.range(0.75, 1.25)).clamp(80.0, 280.0);
            self.formants = [
                self.rng.range(350.0, 850.0),
                self.rng.range(900.0, 2200.0),
                self.rng.range(2300.0, 3200.0),
            ];
            self.level = self.rng.range(4000.0, 11000.0);
        } else if kind == SegmentKind::Unvoiced {
            self.level = self.rng.range(800.0, 2500.0);
        }
    }

    fn next_segment(&mut self) {
        let c = self.config;
        let (kind, range) = match self.kind {
            SegmentKind::Voiced => {
                if self.rng.chance(c.silence_probability) {
                    (SegmentKind::Silence, c.silence_ms)
                } else if self.rng.chance(0.5) {
                    (SegmentKind::Unvoiced, c.unvoiced_ms)
                } else {
                    (SegmentKind::Voiced, c.voiced_ms)
                }
            }
            SegmentKind::Unvoiced | SegmentKind::Silence => (SegmentKind::Voiced, c.voiced_ms),
        };
        let ms = self.rng.range_i64(i64::from(range.0), i64::from(range.1)) as u32;
        self.start_segment(kind, ms);
    }

    fn formant_gain(&self, freq: f64) -> f64 {
        let widths = [90.0, 140.0, 220.0];
        let weights = [1.0, 0.6, 0.25];
        let mut g = 0.02;
        for i in 0..3 {
            let d = (freq - self.formants[i]) / widths[i];
            g += weights[i] * (-0.5 * d * d).exp();
        }
        g
    }

    fn next_mono(&mut self) -> f64 {
        if self.remaining == 0 {
            self.next_segment();
        }
        let rate = f64::from(self.sample_rate_hz);
        let t = self.seg_pos as f64 / self.seg_len as f64;
        let floor = self.rng.normal() * 6.0;
        let value = match self.kind {
            SegmentKind::Silence => floor,
            SegmentKind::Unvoiced => {
                let white = self.rng.normal();
                let hp = white - 0.85 * self.prev_noise;
                self.prev_noise = white;
                let env = (t * std::f64::consts::PI).sin();
                self.level * 0.5 * hp * env + floor
            }
            SegmentKind::Voiced => {
                let f0 = self.f0_start + (self.f0_end - self.f0_start) * t;
                self.phase = (self.phase + TAU * f0 / rate) % TAU;
                let nyquist = (rate * 0.45).min(3800.0);
                let mut acc = 0.0;
                let mut k = 1.0;
                while k * f0 < nyquist {
                    acc += self.formant_gain(k * f0) * (k * self.phase).sin() / k.sqrt();
                    k += 1.0;
                }
                // 20 ms attack, 40 ms release, ~4 Hz syllabic modulation.
                let pos_ms = self.seg_pos as f64 * 1000.0 / rate;
                let left_ms = self.remaining as f64 * 1000.0 / rate;
                let edge = (pos_ms / 20.0).min(1.0) * (left_ms / 40.0).min(1.0);
                let syllable = 0.65 + 0.35 * (TAU * 4.0 * pos_ms / 1000.0).cos();
                self.level * 0.35 * acc * edge * syllable + floor
            }
        };
        self.seg_pos += 1;
        self.remaining -= 1;
        value
    }

    /// Produces `samples_per_channel` interleaved samples.
    pub fn generate(&mut self, samples_per_channel: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(samples_per_channel * usize::from(self.channels));
        let delay = (self.sample_rate_hz as usize / 2000).clamp(1, self.history.len() - 1);
        for _ in 0..samples_per_channel {
            let left = self.next_mono();
            out.push(to_i16(left));
            if self.channels == 2 {
                let n = self.history.len();
                self.history[self.history_pos] = left;
                let delayed = self.history[(self.history_pos + n - delay) % n];
                self.history_pos = (self.history_pos + 1) % n;
                out.push(to_i16(0.8 * delayed + 0.2 * left));
            }
        }
        out
    }
}

fn to_i16(v: f64) -> i16 {
    v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let a = SpeechSynth::new(3, 16000, 1, SynthConfig::default()).generate(16000);
        let b = SpeechSynth::new(3, 16000, 1, SynthConfig::default()).generate(16000);
        assert_eq!(a, b);
        let c = SpeechSynth::new(4, 16000, 1, SynthConfig::default()).generate(16000);
        assert_ne!(a, c);
    }

    #[test]
    fn has_speech_and_silence() {
        let mut s = SpeechSynth::new(9, 16000, 2, SynthConfig::default());
        let mut loud = 0;
        let mut quiet = 0;
        for _ in 0..1000 {
            let frame = s.generate(160);
            assert_eq!(frame.len(), 320);
            let peak = frame.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
            if peak > 2000 {
                loud += 1;
            } else if peak < 100 {
                quiet += 1;
            }
        }
        assert!(loud > 200, "loud frames {loud}");
        assert!(quiet > 50, "quiet frames {quiet}");
    }
}
