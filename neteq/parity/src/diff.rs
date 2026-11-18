//! Compares a candidate recording against the C++ reference.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runner::{FrameRecord, Recording};
use crate::trace::Trace;
use crate::types::{Operation, SpeechType};

/// Where the two outputs first differ.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Divergence {
    /// Frame index.
    pub frame: usize,
    /// Output time of that frame.
    pub time_ms: i64,
    /// Reference operation.
    pub reference_operation: Operation,
    /// Candidate operation.
    pub candidate_operation: Operation,
    /// Reference speech type.
    pub reference_speech_type: SpeechType,
    /// Candidate speech type.
    pub candidate_speech_type: SpeechType,
    /// First differing sample index within the frame (interleaved), if the
    /// lengths match.
    pub first_sample: Option<usize>,
    /// Max |ref - cand| in that frame.
    pub max_abs_diff: i32,
    /// Reference / candidate sample counts in that frame.
    pub lengths: (usize, usize),
}

/// Sample-level agreement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleDiff {
    /// Samples compared (frames with equal length).
    pub compared: u64,
    /// Bit-identical samples.
    pub bitexact: u64,
    /// `bitexact / compared` (1.0 when nothing was compared and no frame
    /// had a length mismatch).
    pub bitexact_ratio: f64,
    /// Frames identical in every sample, length, rate and channel count.
    pub frames_bitexact: usize,
    /// Frames whose length / rate / channels differ.
    pub frames_shape_mismatch: usize,
    /// Largest |ref - cand| over compared samples.
    pub max_abs_diff: i32,
    /// 10·log10(Σref² / Σ(ref−cand)²) in dB; `None` means no error at all.
    pub snr_db: Option<f64>,
    /// First differing frame.
    pub first_divergence: Option<Divergence>,
}

/// Agreement of per-frame decisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionDiff {
    /// Fraction of frames with the same `Operation`.
    pub operation_agreement: f64,
    /// First frame whose operation differs.
    pub first_operation_divergence: Option<usize>,
    /// Fraction of frames with the same speech type.
    pub speech_type_agreement: f64,
    /// Fraction of frames with the same muted flag.
    pub muted_agreement: f64,
    /// Reference operation histogram.
    pub reference_operations: BTreeMap<Operation, u64>,
    /// Candidate operation histogram.
    pub candidate_operations: BTreeMap<Operation, u64>,
    /// (reference op, candidate op, count) for disagreeing frames.
    pub confusion: Vec<(Operation, Operation, u64)>,
}

/// Delay estimator agreement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayDiff {
    /// Mean |Δ target delay| in ms.
    pub target_delay_mean_abs_diff_ms: f64,
    /// Max |Δ target delay| in ms.
    pub target_delay_max_abs_diff_ms: i32,
    /// First frame whose target delay differs.
    pub target_delay_first_divergence: Option<usize>,
    /// Mean |Δ filtered current delay| in ms.
    pub filtered_delay_mean_abs_diff_ms: f64,
    /// Max |Δ filtered current delay| in ms.
    pub filtered_delay_max_abs_diff_ms: i32,
    /// Reference mean target delay.
    pub reference_target_delay_mean_ms: f64,
    /// Candidate mean target delay.
    pub candidate_target_delay_mean_ms: f64,
}

/// One numeric stat field compared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDelta {
    /// Field name.
    pub field: String,
    /// Reference value.
    pub reference: f64,
    /// Candidate value.
    pub candidate: f64,
    /// candidate − reference.
    pub delta: f64,
}

/// Per-field mean / max |Δ| over the network-stat intervals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntervalFieldDelta {
    /// Field name (Q14 rates are converted to fractions, suffix `_frac`).
    pub field: String,
    /// Mean reference value.
    pub reference_mean: f64,
    /// Mean candidate value.
    pub candidate_mean: f64,
    /// Mean |Δ| per interval.
    pub mean_abs_diff: f64,
    /// Max |Δ| over intervals.
    pub max_abs_diff: f64,
}

/// Full comparison report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffReport {
    /// Scenario name.
    pub scenario: String,
    /// Codec label.
    pub codec: String,
    /// Seed.
    pub seed: u64,
    /// Reference implementation name.
    pub reference: String,
    /// Candidate implementation name.
    pub candidate: String,
    /// Packets in the trace.
    pub packets: usize,
    /// Frames each side produced.
    pub frames: (usize, usize),
    /// Frames compared (min of the two).
    pub frames_compared: usize,
    /// Insert return codes that differ.
    pub insert_code_mismatches: usize,
    /// Sample comparison.
    pub samples: SampleDiff,
    /// Decision comparison.
    pub decisions: DecisionDiff,
    /// Delay comparison.
    pub delay: DelayDiff,
    /// Network statistics over intervals.
    pub network_stats: Vec<IntervalFieldDelta>,
    /// Final lifetime statistics, fields that differ first.
    pub lifetime_stats: Vec<FieldDelta>,
    /// Final operations-and-state, fields that differ first.
    pub operations_and_state: Vec<FieldDelta>,
    /// Reference error, if it stopped early.
    pub reference_error: Option<String>,
    /// Candidate error, if it stopped early.
    pub candidate_error: Option<String>,
    /// True when every frame, decision, delay and final stat matches exactly.
    pub identical: bool,
}

fn samples_equal_shape(a: &FrameRecord, b: &FrameRecord) -> bool {
    a.samples.len() == b.samples.len()
        && a.sample_rate_hz == b.sample_rate_hz
        && a.num_channels == b.num_channels
}

/// Builds the report. `reference` is the C++ recording.
pub fn compare(trace: &Trace, reference: &Recording, candidate: &Recording) -> DiffReport {
    let n = reference.frames.len().min(candidate.frames.len());
    let pairs = || reference.frames[..n].iter().zip(&candidate.frames[..n]);

    // Samples.
    let mut compared = 0u64;
    let mut bitexact = 0u64;
    let mut frames_bitexact = 0usize;
    let mut shape_mismatch = 0usize;
    let mut max_abs = 0i32;
    let mut signal = 0f64;
    let mut noise = 0f64;
    let mut first_div: Option<Divergence> = None;
    for (i, (r, c)) in pairs().enumerate() {
        let mut frame_max = 0i32;
        let mut first_sample = None;
        let same_shape = samples_equal_shape(r, c);
        if same_shape {
            for (k, (&a, &b)) in r.samples.iter().zip(&c.samples).enumerate() {
                let d = (i32::from(a) - i32::from(b)).abs();
                compared += 1;
                signal += f64::from(a) * f64::from(a);
                noise += f64::from(d) * f64::from(d);
                if d == 0 {
                    bitexact += 1;
                } else if first_sample.is_none() {
                    first_sample = Some(k);
                }
                frame_max = frame_max.max(d);
            }
            max_abs = max_abs.max(frame_max);
        } else {
            shape_mismatch += 1;
        }
        let frame_equal = same_shape && first_sample.is_none();
        if frame_equal {
            frames_bitexact += 1;
        } else if first_div.is_none() {
            first_div = Some(Divergence {
                frame: i,
                time_ms: r.time_ms,
                reference_operation: r.operation,
                candidate_operation: c.operation,
                reference_speech_type: r.speech_type,
                candidate_speech_type: c.speech_type,
                first_sample,
                max_abs_diff: frame_max,
                lengths: (r.samples.len(), c.samples.len()),
            });
        }
    }
    let bitexact_ratio = if compared == 0 {
        if shape_mismatch == 0 && n > 0 {
            1.0
        } else {
            0.0
        }
    } else {
        bitexact as f64 / compared as f64
    };
    let snr_db = (noise > 0.0).then(|| 10.0 * (signal.max(1.0) / noise).log10());

    // Decisions.
    let mut ref_ops = BTreeMap::new();
    let mut cand_ops = BTreeMap::new();
    let mut confusion: BTreeMap<(Operation, Operation), u64> = BTreeMap::new();
    let mut op_agree = 0usize;
    let mut st_agree = 0usize;
    let mut muted_agree = 0usize;
    let mut first_op_div = None;
    for (i, (r, c)) in pairs().enumerate() {
        *ref_ops.entry(r.operation).or_insert(0) += 1;
        *cand_ops.entry(c.operation).or_insert(0) += 1;
        if r.operation == c.operation {
            op_agree += 1;
        } else {
            *confusion.entry((r.operation, c.operation)).or_insert(0) += 1;
            first_op_div.get_or_insert(i);
        }
        st_agree += usize::from(r.speech_type == c.speech_type);
        muted_agree += usize::from(r.muted == c.muted);
    }
    let ratio = |k: usize| if n == 0 { 0.0 } else { k as f64 / n as f64 };

    // Delay.
    let mut td_sum = 0f64;
    let mut td_max = 0i32;
    let mut td_first = None;
    let mut fd_sum = 0f64;
    let mut fd_max = 0i32;
    let mut ref_td = 0f64;
    let mut cand_td = 0f64;
    for (i, (r, c)) in pairs().enumerate() {
        let td = (r.target_delay_ms - c.target_delay_ms).abs();
        let fd = (r.filtered_current_delay_ms - c.filtered_current_delay_ms).abs();
        if td != 0 {
            td_first.get_or_insert(i);
        }
        td_sum += f64::from(td);
        td_max = td_max.max(td);
        fd_sum += f64::from(fd);
        fd_max = fd_max.max(fd);
        ref_td += f64::from(r.target_delay_ms);
        cand_td += f64::from(c.target_delay_ms);
    }
    let mean = |s: f64| if n == 0 { 0.0 } else { s / n as f64 };

    let insert_code_mismatches = reference
        .insert_return_codes
        .iter()
        .zip(&candidate.insert_return_codes)
        .filter(|(a, b)| a != b)
        .count()
        + reference
            .insert_return_codes
            .len()
            .abs_diff(candidate.insert_return_codes.len());

    let network_stats = interval_deltas(&reference.network_intervals, &candidate.network_intervals);
    let lifetime_stats = field_deltas(
        reference.lifetime.as_ref().map(to_value),
        candidate.lifetime.as_ref().map(to_value),
    );
    let operations_and_state = field_deltas(
        reference.operations_and_state.as_ref().map(to_value),
        candidate.operations_and_state.as_ref().map(to_value),
    );

    // Processing delay is wall-clock based; it never matches across
    // implementations and is excluded from the verdict.
    let stats_equal = lifetime_stats
        .iter()
        .filter(|d| d.field != "total_processing_delay_us")
        .all(|d| d.delta == 0.0)
        && operations_and_state.iter().all(|d| d.delta == 0.0)
        && network_stats.iter().all(|d| d.max_abs_diff == 0.0);
    let identical = reference.error.is_none()
        && candidate.error.is_none()
        && reference.frames.len() == candidate.frames.len()
        && frames_bitexact == n
        && op_agree == n
        && st_agree == n
        && td_max == 0
        && fd_max == 0
        && insert_code_mismatches == 0
        && stats_equal;

    DiffReport {
        scenario: trace.scenario.clone(),
        codec: trace.codec.label(),
        seed: trace.seed,
        reference: reference.implementation.clone(),
        candidate: candidate.implementation.clone(),
        packets: trace.packets.len(),
        frames: (reference.frames.len(), candidate.frames.len()),
        frames_compared: n,
        insert_code_mismatches,
        samples: SampleDiff {
            compared,
            bitexact,
            bitexact_ratio,
            frames_bitexact,
            frames_shape_mismatch: shape_mismatch,
            max_abs_diff: max_abs,
            snr_db,
            first_divergence: first_div,
        },
        decisions: DecisionDiff {
            operation_agreement: ratio(op_agree),
            first_operation_divergence: first_op_div,
            speech_type_agreement: ratio(st_agree),
            muted_agreement: ratio(muted_agree),
            reference_operations: ref_ops,
            candidate_operations: cand_ops,
            confusion: confusion.into_iter().map(|((a, b), k)| (a, b, k)).collect(),
        },
        delay: DelayDiff {
            target_delay_mean_abs_diff_ms: mean(td_sum),
            target_delay_max_abs_diff_ms: td_max,
            target_delay_first_divergence: td_first,
            filtered_delay_mean_abs_diff_ms: mean(fd_sum),
            filtered_delay_max_abs_diff_ms: fd_max,
            reference_target_delay_mean_ms: mean(ref_td),
            candidate_target_delay_mean_ms: mean(cand_td),
        },
        network_stats,
        lifetime_stats,
        operations_and_state,
        reference_error: reference.error.clone(),
        candidate_error: candidate.error.clone(),
        identical,
    }
}

fn to_value<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn numeric_fields(v: &Value) -> Vec<(String, f64)> {
    match v {
        Value::Object(map) => map
            .iter()
            .filter_map(|(k, v)| {
                let x = match v {
                    Value::Number(n) => n.as_f64()?,
                    Value::Bool(b) => f64::from(u8::from(*b)),
                    _ => return None,
                };
                Some((k.clone(), x))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn field_deltas(reference: Option<Value>, candidate: Option<Value>) -> Vec<FieldDelta> {
    let (Some(r), Some(c)) = (reference, candidate) else {
        return Vec::new();
    };
    let cand: BTreeMap<String, f64> = numeric_fields(&c).into_iter().collect();
    let mut out: Vec<FieldDelta> = numeric_fields(&r)
        .into_iter()
        .map(|(field, reference)| {
            let candidate = cand.get(&field).copied().unwrap_or(f64::NAN);
            FieldDelta {
                delta: candidate - reference,
                field,
                reference,
                candidate,
            }
        })
        .collect();
    // Differences first, then by name.
    out.sort_by(|a, b| {
        (a.delta == 0.0)
            .cmp(&(b.delta == 0.0))
            .then_with(|| a.field.cmp(&b.field))
    });
    out
}

/// Numeric fields of one stats struct, sorted by field name.
type Fields = Vec<(String, f64)>;

const Q14_FIELDS: [&str; 6] = [
    "expand_rate",
    "speech_expand_rate",
    "preemptive_rate",
    "accelerate_rate",
    "secondary_decoded_rate",
    "secondary_discarded_rate",
];

fn interval_deltas<T: Serialize>(reference: &[T], candidate: &[T]) -> Vec<IntervalFieldDelta> {
    let n = reference.len().min(candidate.len());
    if n == 0 {
        return Vec::new();
    }
    let rows: Vec<(Fields, Fields)> = reference[..n]
        .iter()
        .zip(&candidate[..n])
        .map(|(r, c)| (numeric_fields(&to_value(r)), numeric_fields(&to_value(c))))
        .collect();
    let fields: Vec<String> = rows[0].0.iter().map(|(k, _)| k.clone()).collect();
    fields
        .into_iter()
        .enumerate()
        .map(|(idx, field)| {
            let scale = if Q14_FIELDS.contains(&field.as_str()) {
                1.0 / 16384.0
            } else {
                1.0
            };
            let (mut rs, mut cs, mut sum, mut max) = (0.0, 0.0, 0.0, 0.0f64);
            for (r, c) in &rows {
                let a = r[idx].1 * scale;
                let b = c.get(idx).map_or(f64::NAN, |x| x.1) * scale;
                rs += a;
                cs += b;
                sum += (a - b).abs();
                max = max.max((a - b).abs());
            }
            let k = n as f64;
            IntervalFieldDelta {
                field: if scale != 1.0 {
                    format!("{field}_frac")
                } else {
                    field
                },
                reference_mean: rs / k,
                candidate_mean: cs / k,
                mean_abs_diff: sum / k,
                max_abs_diff: max,
            }
        })
        .collect()
}

impl DiffReport {
    /// Human-readable multi-line summary.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        let verdict = if self.identical {
            "IDENTICAL"
        } else {
            "DIFFERS"
        };
        let _ = writeln!(
            s,
            "== {} / {} (seed {}) : {} vs {} : {verdict}",
            self.scenario, self.codec, self.seed, self.reference, self.candidate
        );
        if let Some(e) = &self.reference_error {
            let _ = writeln!(s, "   reference error: {e}");
        }
        if let Some(e) = &self.candidate_error {
            let _ = writeln!(s, "   candidate error: {e}");
        }
        let sm = &self.samples;
        let snr = sm
            .snr_db
            .map_or("inf".to_string(), |v| format!("{v:.1} dB"));
        let _ = writeln!(
            s,
            "   samples   bit-exact {:.4} ({} / {})  frames bit-exact {} / {}  max|d| {}  SNR {snr}",
            sm.bitexact_ratio, sm.bitexact, sm.compared, sm.frames_bitexact, self.frames_compared, sm.max_abs_diff
        );
        if let Some(d) = &sm.first_divergence {
            let _ = writeln!(
                s,
                "   first divergence: frame {} (t={} ms) ops {:?}/{:?} speech {:?}/{:?} max|d| {} len {:?}",
                d.frame,
                d.time_ms,
                d.reference_operation,
                d.candidate_operation,
                d.reference_speech_type,
                d.candidate_speech_type,
                d.max_abs_diff,
                d.lengths
            );
        }
        let dec = &self.decisions;
        let _ = writeln!(
            s,
            "   decisions op agree {:.4} (first diff {:?})  speech-type agree {:.4}",
            dec.operation_agreement, dec.first_operation_divergence, dec.speech_type_agreement
        );
        let _ = writeln!(s, "   ref ops  {}", fmt_hist(&dec.reference_operations));
        let _ = writeln!(s, "   cand ops {}", fmt_hist(&dec.candidate_operations));
        let dl = &self.delay;
        let _ = writeln!(
            s,
            "   delay     target mean {:.1}/{:.1} ms  mean|d| {:.2}  max|d| {}  filtered mean|d| {:.2}",
            dl.reference_target_delay_mean_ms,
            dl.candidate_target_delay_mean_ms,
            dl.target_delay_mean_abs_diff_ms,
            dl.target_delay_max_abs_diff_ms,
            dl.filtered_delay_mean_abs_diff_ms
        );
        for f in self
            .network_stats
            .iter()
            .filter(|f| f.max_abs_diff != 0.0)
            .take(6)
        {
            let _ = writeln!(
                s,
                "   net {:<28} ref {:>9.4} cand {:>9.4} mean|d| {:.4}",
                f.field, f.reference_mean, f.candidate_mean, f.mean_abs_diff
            );
        }
        for f in self
            .lifetime_stats
            .iter()
            .filter(|f| f.delta != 0.0 && f.field != "total_processing_delay_us")
            .take(8)
        {
            let _ = writeln!(
                s,
                "   life {:<34} ref {:>10} cand {:>10} d {:+}",
                f.field, f.reference, f.candidate, f.delta
            );
        }
        s
    }
}

/// Formats an operation histogram as `normal=812 expand=12 ...`.
pub fn fmt_hist(h: &BTreeMap<Operation, u64>) -> String {
    h.iter()
        .map(|(op, n)| {
            format!(
                "{}={n}",
                serde_json::to_value(op)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reference-only summary for runs without a candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// Scenario name.
    pub scenario: String,
    /// Codec label.
    pub codec: String,
    /// Seed.
    pub seed: u64,
    /// Implementation name.
    pub implementation: String,
    /// Packets in the trace.
    pub packets: usize,
    /// Frames produced.
    pub frames: usize,
    /// Operation histogram.
    pub operations: BTreeMap<Operation, u64>,
    /// Mean / min / max target delay.
    pub target_delay_ms: (f64, i32, i32),
    /// Output sample rate of the last frame.
    pub last_sample_rate_hz: i32,
    /// Non-zero `InsertPacket` return codes.
    pub insert_failures: usize,
    /// Non-zero `GetAudio` return codes.
    pub get_audio_failures: usize,
    /// Final lifetime stats.
    pub lifetime: Option<crate::types::LifetimeStatistics>,
    /// Final operations and state.
    pub operations_and_state: Option<crate::types::OperationsAndState>,
    /// Network statistics intervals.
    pub network_intervals: Vec<crate::types::NetworkStatistics>,
    /// Error, if the run stopped early.
    pub error: Option<String>,
}

impl RunSummary {
    /// Summarizes one recording.
    pub fn new(trace: &Trace, rec: &Recording) -> Self {
        let mut operations = BTreeMap::new();
        for f in &rec.frames {
            *operations.entry(f.operation).or_insert(0) += 1;
        }
        let td: Vec<i32> = rec.frames.iter().map(|f| f.target_delay_ms).collect();
        let mean = if td.is_empty() {
            0.0
        } else {
            td.iter().map(|&v| f64::from(v)).sum::<f64>() / td.len() as f64
        };
        Self {
            scenario: trace.scenario.clone(),
            codec: trace.codec.label(),
            seed: trace.seed,
            implementation: rec.implementation.clone(),
            packets: trace.packets.len(),
            frames: rec.frames.len(),
            operations,
            target_delay_ms: (
                mean,
                td.iter().copied().min().unwrap_or(0),
                td.iter().copied().max().unwrap_or(0),
            ),
            last_sample_rate_hz: rec.frames.last().map_or(0, |f| f.sample_rate_hz),
            insert_failures: rec.insert_return_codes.iter().filter(|&&c| c != 0).count(),
            get_audio_failures: rec.frames.iter().filter(|f| f.return_code != 0).count(),
            lifetime: rec.lifetime,
            operations_and_state: rec.operations_and_state,
            network_intervals: rec.network_intervals.clone(),
            error: rec.error.clone(),
        }
    }

    /// Human-readable summary.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "== {} / {} (seed {}) : {} only : {} packets, {} frames, rate {} Hz",
            self.scenario,
            self.codec,
            self.seed,
            self.implementation,
            self.packets,
            self.frames,
            self.last_sample_rate_hz
        );
        let _ = writeln!(s, "   ops {}", fmt_hist(&self.operations));
        let _ = writeln!(
            s,
            "   target delay mean {:.1} ms (min {}, max {})  insert failures {}  get_audio failures {}",
            self.target_delay_ms.0, self.target_delay_ms.1, self.target_delay_ms.2, self.insert_failures, self.get_audio_failures
        );
        if let Some(l) = &self.lifetime {
            let _ = writeln!(
                s,
                "   concealed {} / {} samples, events {}, accel removed {}, decel inserted {}, discarded pkts {}, jb delay {} ms over {}",
                l.concealed_samples,
                l.total_samples_received,
                l.concealment_events,
                l.removed_samples_for_acceleration,
                l.inserted_samples_for_deceleration,
                l.packets_discarded,
                l.jitter_buffer_delay_ms,
                l.jitter_buffer_emitted_count
            );
        }
        if let Some(e) = &self.error {
            let _ = writeln!(s, "   error: {e}");
        }
        s
    }
}
