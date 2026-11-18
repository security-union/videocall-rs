//! Seeded, deterministic scenario generators.
//!
//! A scenario is two independent models fed from forked RNG streams:
//! media (speech synth, encoder, DTX) decides which packets are sent and
//! when; the network (delay, jitter, loss, spikes, reordering, duplicates,
//! sender clock drift) decides when, and whether, each one arrives.

use serde::{Deserialize, Serialize};

use crate::cneteq::{CngEncoder, OpusEncoder};
use crate::error::{HarnessError, Result};
use crate::rng::Rng;
use crate::signal::{SegmentKind, SpeechSynth, SynthConfig};
use crate::trace::{Trace, TracePacket, TRACE_FORMAT_VERSION};
use crate::types::{Codec, NetEqConfig};

/// Packet duration used by every scenario.
pub const PTIME_MS: u32 = 20;

/// Named scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    /// Constant 20 ms one-way delay, nothing lost.
    Clean,
    /// Uniform jitter in [0, 60) ms, in-order delivery.
    UniformJitter,
    /// Gaussian jitter plus occasional 150-500 ms network stalls that release
    /// packets in a burst.
    GaussianSpikes,
    /// Gilbert-Elliott burst loss.
    BurstLoss,
    /// ~6 % of packets overtaken by the next 1-3 packets.
    Reordering,
    /// Sender clock runs 2000 ppm fast (packets arrive early over time).
    ClockDriftFast,
    /// Sender clock runs 2000 ppm slow (buffer starves over time).
    ClockDriftSlow,
    /// Long silences with DTX: RFC 3389 SID for L16, Opus DTX for Opus.
    Dtx,
    /// ~8 % of packets delivered twice.
    Duplicates,
    /// 60 s cycling through all of the above, with DTX and +300 ppm drift.
    LongMixed,
}

impl Scenario {
    /// Every scenario.
    pub const ALL: [Scenario; 10] = [
        Scenario::Clean,
        Scenario::UniformJitter,
        Scenario::GaussianSpikes,
        Scenario::BurstLoss,
        Scenario::Reordering,
        Scenario::ClockDriftFast,
        Scenario::ClockDriftSlow,
        Scenario::Dtx,
        Scenario::Duplicates,
        Scenario::LongMixed,
    ];

    /// Stable snake_case name.
    pub fn name(&self) -> &'static str {
        match self {
            Scenario::Clean => "clean",
            Scenario::UniformJitter => "uniform_jitter",
            Scenario::GaussianSpikes => "gaussian_spikes",
            Scenario::BurstLoss => "burst_loss",
            Scenario::Reordering => "reordering",
            Scenario::ClockDriftFast => "clock_drift_fast",
            Scenario::ClockDriftSlow => "clock_drift_slow",
            Scenario::Dtx => "dtx",
            Scenario::Duplicates => "duplicates",
            Scenario::LongMixed => "long_mixed",
        }
    }

    /// Parses [`Scenario::name`].
    pub fn from_name(name: &str) -> Result<Scenario> {
        Scenario::ALL
            .into_iter()
            .find(|s| s.name() == name)
            .ok_or_else(|| HarnessError::UnknownScenario(name.to_string()))
    }

    /// Default media duration.
    pub fn default_duration_ms(&self) -> u32 {
        match self {
            Scenario::LongMixed => 60_000,
            _ => 20_000,
        }
    }

    fn uses_dtx(&self) -> bool {
        matches!(self, Scenario::Dtx | Scenario::LongMixed)
    }

    fn drift_ppm(&self) -> f64 {
        match self {
            Scenario::ClockDriftFast => 2000.0,
            Scenario::ClockDriftSlow => -2000.0,
            Scenario::LongMixed => 300.0,
            _ => 0.0,
        }
    }

    /// Network phases: (`until_ms`, model). The last phase covers the rest.
    fn network(&self) -> Vec<(f64, NetModel)> {
        let base = NetModel::default();
        let jitter = |max: f64| NetModel {
            jitter: Jitter::Uniform(max),
            ..base
        };
        let spikes = NetModel {
            base_delay_ms: 40.0,
            jitter: Jitter::HalfNormal(12.0),
            stalls: Some(Stalls {
                probability: 0.004,
                min_ms: 150.0,
                max_ms: 500.0,
            }),
            ..base
        };
        let loss = NetModel {
            jitter: Jitter::Uniform(10.0),
            loss: Some(GilbertElliott {
                p_good_to_bad: 0.02,
                p_bad_to_good: 0.25,
                loss_good: 0.002,
                loss_bad: 0.75,
            }),
            ..base
        };
        let reorder = NetModel {
            jitter: Jitter::Uniform(10.0),
            reorder_probability: 0.06,
            fifo: false,
            ..base
        };
        let dups = NetModel {
            duplicate_probability: 0.08,
            ..jitter(20.0)
        };
        let forever = f64::INFINITY;
        match self {
            Scenario::Clean => vec![(forever, base)],
            Scenario::UniformJitter => vec![(forever, jitter(60.0))],
            Scenario::GaussianSpikes => vec![(forever, spikes)],
            Scenario::BurstLoss => vec![(forever, loss)],
            Scenario::Reordering => vec![(forever, reorder)],
            Scenario::ClockDriftFast | Scenario::ClockDriftSlow | Scenario::Dtx => {
                vec![(forever, jitter(20.0))]
            }
            Scenario::Duplicates => vec![(forever, dups)],
            Scenario::LongMixed => {
                let mut heavy = jitter(80.0);
                heavy.loss = loss.loss;
                let mut reorder_dups = reorder;
                reorder_dups.duplicate_probability = 0.05;
                vec![
                    (10_000.0, base),
                    (20_000.0, jitter(80.0)),
                    (30_000.0, loss),
                    (40_000.0, spikes),
                    (50_000.0, reorder_dups),
                    (forever, heavy),
                ]
            }
        }
    }

    /// Generates the trace for `codec` with `seed`, over `duration_ms` of
    /// media. Identical arguments give byte-identical traces.
    pub fn generate(&self, codec: Codec, seed: u64, duration_ms: u32) -> Result<Trace> {
        let mut root = Rng::new(seed ^ fnv1a(self.name()));
        let mut media_rng = root.fork(1);
        let mut net_rng = root.fork(2);

        let media = generate_media(codec, &mut media_rng, duration_ms, self.uses_dtx())?;
        let packets = apply_network(
            &media.packets,
            &self.network(),
            self.drift_ppm(),
            &mut net_rng,
        );

        Ok(Trace {
            version: TRACE_FORMAT_VERSION,
            scenario: self.name().to_string(),
            seed,
            codec,
            ptime_ms: PTIME_MS,
            duration_ms,
            ssrc: media.ssrc,
            with_cn: media.with_cn,
            config: NetEqConfig::default(),
            payload_generator: media.generator,
            packets,
        })
    }
}

fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xCBF2_9CE4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// A packet leaving the sender.
#[derive(Debug, Clone)]
struct SentPacket {
    frame_index: u64,
    seq: u16,
    rtp_timestamp: u32,
    payload_type: u8,
    marker: bool,
    payload: Vec<u8>,
}

struct Media {
    packets: Vec<SentPacket>,
    ssrc: u32,
    with_cn: bool,
    generator: String,
}

fn generate_media(codec: Codec, rng: &mut Rng, duration_ms: u32, dtx: bool) -> Result<Media> {
    let synth_seed = rng.next_u64();
    let ssrc = rng.next_u64() as u32;
    let mut seq = rng.next_u64() as u16;
    let mut rtp_ts = rng.next_u64() as u32;

    let channels = codec.channels();
    let synth_rate = codec.rtp_clock_hz();
    let synth_config = if dtx {
        SynthConfig::with_long_silences()
    } else {
        SynthConfig::default()
    };
    let mut synth = SpeechSynth::new(synth_seed, synth_rate, channels, synth_config);
    let spf = (synth_rate * PTIME_MS / 1000) as usize;
    let frames = u64::from(duration_ms / PTIME_MS);

    let mut opus = match codec {
        Codec::Opus { channels } => Some(OpusEncoder::new(
            channels,
            32_000 * u32::from(channels) as i32,
            5,
            dtx,
        )?),
        Codec::L16 { .. } => None,
    };
    // NetEq's RFC 3389 comfort noise is mono-only (ComfortNoise::Generate
    // returns kMultiChannelNotSupported), so stereo L16 DTX just pauses.
    let with_cn = dtx && matches!(codec, Codec::L16 { channels: 1, .. });
    let silent_gaps = dtx && matches!(codec, Codec::L16 { channels: 2, .. });
    let cng_rate = if synth_rate == 8000 { 8000 } else { 16000 };
    let mut cng = if with_cn {
        Some(CngEncoder::new(cng_rate, 100, 8)?)
    } else {
        None
    };

    let mut packets = Vec::new();
    let mut in_silence = false;
    let mut next_marker = true;
    let mut last_sent_frame: Option<u64> = None;

    for frame in 0..frames {
        let kind_before = synth.current_kind();
        let pcm = synth.generate(spf);
        let silent =
            kind_before == SegmentKind::Silence && synth.current_kind() == SegmentKind::Silence;

        let mut push = |pt: u8, payload: Vec<u8>, marker: bool, packets: &mut Vec<SentPacket>| {
            packets.push(SentPacket {
                frame_index: frame,
                seq,
                rtp_timestamp: rtp_ts,
                payload_type: pt,
                marker,
                payload,
            });
            seq = seq.wrapping_add(1);
        };

        match (&mut opus, &mut cng) {
            (Some(enc), _) => {
                let payload = enc.encode(&pcm)?;
                let is_dtx_frame = payload.len() <= 2;
                // First DTX frame of a pause goes out, then one every 400 ms.
                let due = !in_silence || last_sent_frame.is_none_or(|f| frame - f >= 20);
                if !is_dtx_frame || !dtx || due {
                    let marker = next_marker && !is_dtx_frame;
                    push(codec.payload_type(), payload, marker, &mut packets);
                    last_sent_frame = Some(frame);
                    if marker {
                        next_marker = false;
                    }
                }
                in_silence = is_dtx_frame && dtx;
                if in_silence {
                    next_marker = true;
                }
            }
            (None, Some(cng)) if silent => {
                let step = (synth_rate / cng_rate) as usize * usize::from(channels);
                let mono: Vec<i16> = pcm.iter().step_by(step).copied().collect();
                if let Some(sid) = cng.encode(&mono, !in_silence) {
                    push(codec.cn_payload_type(), sid, false, &mut packets);
                }
                in_silence = true;
                next_marker = true;
            }
            (None, None) if silent && silent_gaps => {
                next_marker = true;
            }
            _ => {
                let payload: Vec<u8> = pcm.iter().flat_map(|s| s.to_be_bytes()).collect();
                push(codec.payload_type(), payload, next_marker, &mut packets);
                next_marker = false;
                in_silence = false;
            }
        }
        rtp_ts = rtp_ts.wrapping_add(spf as u32);
    }

    let generator = match codec {
        Codec::L16 { .. } if with_cn => {
            "speech_synth_v1 -> L16 big-endian, WebRTC ComfortNoiseEncoder SID in silences"
        }
        Codec::L16 { .. } if silent_gaps => "speech_synth_v1 -> L16 big-endian, silences not sent",
        Codec::L16 { .. } => "speech_synth_v1 -> L16 big-endian",
        Codec::Opus { .. } if dtx => {
            "speech_synth_v1 -> libopus VOIP 32 kbps/ch c5 DTX (SID every 400 ms)"
        }
        Codec::Opus { .. } => "speech_synth_v1 -> libopus VOIP 32 kbps/ch c5",
    };
    Ok(Media {
        packets,
        ssrc,
        with_cn,
        generator: generator.to_string(),
    })
}

#[derive(Debug, Clone, Copy)]
enum Jitter {
    None,
    /// Uniform in [0, max).
    Uniform(f64),
    /// |N(0, sigma)|.
    HalfNormal(f64),
}

#[derive(Debug, Clone, Copy)]
struct GilbertElliott {
    p_good_to_bad: f64,
    p_bad_to_good: f64,
    loss_good: f64,
    loss_bad: f64,
}

#[derive(Debug, Clone, Copy)]
struct Stalls {
    probability: f64,
    min_ms: f64,
    max_ms: f64,
}

#[derive(Debug, Clone, Copy)]
struct NetModel {
    base_delay_ms: f64,
    jitter: Jitter,
    loss: Option<GilbertElliott>,
    stalls: Option<Stalls>,
    reorder_probability: f64,
    duplicate_probability: f64,
    /// Never deliver a packet before one sent earlier (except reordering).
    fifo: bool,
}

impl Default for NetModel {
    fn default() -> Self {
        Self {
            base_delay_ms: 20.0,
            jitter: Jitter::None,
            loss: None,
            stalls: None,
            reorder_probability: 0.0,
            duplicate_probability: 0.0,
            fifo: true,
        }
    }
}

fn apply_network(
    sent: &[SentPacket],
    phases: &[(f64, NetModel)],
    drift_ppm: f64,
    rng: &mut Rng,
) -> Vec<TracePacket> {
    let mut out = Vec::with_capacity(sent.len());
    let mut bad_state = false;
    let mut stall_end = f64::NEG_INFINITY;
    let mut prev_arrival = f64::NEG_INFINITY;
    let scale = 1.0 + drift_ppm * 1e-6;

    for p in sent {
        let send_ms = p.frame_index as f64 * f64::from(PTIME_MS) * scale;
        let model = phases
            .iter()
            .find(|(until, _)| send_ms < *until)
            .map_or(phases[phases.len() - 1].1, |(_, m)| *m);

        if let Some(ge) = model.loss {
            bad_state = if bad_state {
                !rng.chance(ge.p_bad_to_good)
            } else {
                rng.chance(ge.p_good_to_bad)
            };
            let p_loss = if bad_state { ge.loss_bad } else { ge.loss_good };
            if rng.chance(p_loss) {
                continue;
            }
        }

        let jitter = match model.jitter {
            Jitter::None => 0.0,
            Jitter::Uniform(max) => rng.range(0.0, max),
            Jitter::HalfNormal(sigma) => (rng.normal() * sigma).abs(),
        };
        let mut arrival = send_ms + model.base_delay_ms + jitter;

        if let Some(st) = model.stalls {
            if send_ms >= stall_end && rng.chance(st.probability) {
                stall_end = send_ms + rng.range(st.min_ms, st.max_ms);
            }
            if send_ms < stall_end {
                arrival = arrival.max(stall_end + model.base_delay_ms);
            }
        }

        let reordered = model.reorder_probability > 0.0 && rng.chance(model.reorder_probability);
        if reordered {
            arrival += f64::from(PTIME_MS) * rng.range(1.0, 3.5);
        } else if model.fifo {
            arrival = arrival.max(prev_arrival);
        }
        if !reordered {
            prev_arrival = arrival;
        }

        let packet = TracePacket {
            seq: p.seq,
            rtp_timestamp: p.rtp_timestamp,
            arrival_ms: arrival.floor() as i64,
            payload_type: p.payload_type,
            marker: p.marker,
            payload_len: p.payload.len(),
            payload: p.payload.clone(),
        };
        if model.duplicate_probability > 0.0 && rng.chance(model.duplicate_probability) {
            let mut dup = packet.clone();
            dup.arrival_ms = (arrival + rng.range(0.0, 40.0)).floor() as i64;
            out.push(packet);
            out.push(dup);
        } else {
            out.push(packet);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for s in Scenario::ALL {
            assert_eq!(Scenario::from_name(s.name()).unwrap(), s);
        }
        assert!(Scenario::from_name("nope").is_err());
    }
}
