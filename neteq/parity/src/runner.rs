//! Drives one or more NetEqs through a trace in lockstep, 10 ms at a time.
//!
//! The event loop mirrors WebRTC's `NetEqTest`: before each output event at
//! time `t`, every packet with `arrival_ms <= t` is inserted (clock advanced
//! to its arrival), then the clock is advanced to `t` and `GetAudio` runs.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::trace::Trace;
use crate::types::{
    LifetimeStatistics, NetworkStatistics, Operation, OperationsAndState, SpeechType, VadActivity,
};
use crate::under_test::NetEqUnderTest;

/// Output frame period.
pub const FRAME_MS: i64 = 10;
/// How often `NetworkStatistics` (resetting) is sampled.
pub const NETWORK_STATS_INTERVAL_FRAMES: usize = 100;
/// Extra playout after the last arrival so the buffer drains.
pub const TAIL_MS: i64 = 500;

/// Everything recorded for one 10 ms frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameRecord {
    /// Output event time.
    pub time_ms: i64,
    /// Operation NetEq performed.
    pub operation: Operation,
    /// Speech type of the frame.
    pub speech_type: SpeechType,
    /// VAD activity.
    pub vad_activity: VadActivity,
    /// Muted output.
    pub muted: bool,
    /// Output rate.
    pub sample_rate_hz: i32,
    /// Output channels.
    pub num_channels: usize,
    /// `TargetDelayMs` after this frame.
    pub target_delay_ms: i32,
    /// `FilteredCurrentDelayMs` after this frame.
    pub filtered_current_delay_ms: i32,
    /// Playout timestamp after this frame.
    pub playout_timestamp: Option<u32>,
    /// `GetAudio` return code.
    pub return_code: i32,
    /// Interleaved samples (not serialized; large).
    #[serde(skip)]
    pub samples: Vec<i16>,
}

/// Full recording of one implementation over one trace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recording {
    /// Implementation name.
    pub implementation: String,
    /// Per-frame records.
    pub frames: Vec<FrameRecord>,
    /// Return codes of `InsertPacket`, in arrival order.
    pub insert_return_codes: Vec<i32>,
    /// `NetworkStatistics` sampled (and reset) every
    /// [`NETWORK_STATS_INTERVAL_FRAMES`] frames.
    pub network_intervals: Vec<NetworkStatistics>,
    /// Lifetime statistics at the end.
    pub lifetime: Option<LifetimeStatistics>,
    /// Operations and state at the end.
    pub operations_and_state: Option<OperationsAndState>,
    /// First harness error; recording stops there for this implementation.
    pub error: Option<String>,
}

/// Number of output frames simulated for `trace`.
pub fn frame_count(trace: &Trace) -> usize {
    let end = trace.last_arrival_ms().max(i64::from(trace.duration_ms)) + TAIL_MS;
    (end / FRAME_MS) as usize + 1
}

/// Registers the trace's codec on every implementation, then drives all of
/// them through `trace` in lockstep. Implementations that error stop
/// recording; the others continue.
pub fn run_lockstep(trace: &Trace, impls: &mut [Box<dyn NetEqUnderTest>]) -> Vec<Recording> {
    let mut recordings: Vec<Recording> = impls
        .iter()
        .map(|n| Recording {
            implementation: n.name().to_string(),
            ..Recording::default()
        })
        .collect();

    for (neteq, rec) in impls.iter_mut().zip(recordings.iter_mut()) {
        if let Err(e) = neteq.register_codec(&trace.codec, trace.with_cn) {
            rec.error = Some(e.to_string());
        }
    }

    let order = trace.arrival_order();
    let mut next = 0;
    let mut now_ms = 0i64;
    let frames = frame_count(trace);

    for frame in 0..frames {
        let t = frame as i64 * FRAME_MS;
        while next < order.len() && trace.packets[order[next]].arrival_ms <= t {
            let packet = &trace.packets[order[next]];
            let arrival = packet.arrival_ms.max(now_ms);
            let header = packet.header(trace.ssrc);
            for (neteq, rec) in impls.iter_mut().zip(recordings.iter_mut()) {
                if rec.error.is_some() {
                    continue;
                }
                neteq.advance_time_ms(arrival - now_ms);
                match neteq.insert_packet(&header, &packet.payload, packet.arrival_ms) {
                    Ok(code) => rec.insert_return_codes.push(code),
                    Err(e) => rec.error = Some(e.to_string()),
                }
            }
            now_ms = arrival;
            next += 1;
        }

        for (neteq, rec) in impls.iter_mut().zip(recordings.iter_mut()) {
            if rec.error.is_some() {
                continue;
            }
            neteq.advance_time_ms(t - now_ms);
            if let Err(e) = step(neteq.as_mut(), rec, t, frame) {
                rec.error = Some(e.to_string());
            }
        }
        now_ms = t;
    }

    for (neteq, rec) in impls.iter_mut().zip(recordings.iter_mut()) {
        if rec.error.is_some() {
            continue;
        }
        let finish = || -> Result<(LifetimeStatistics, OperationsAndState)> {
            Ok((neteq.lifetime_statistics()?, neteq.operations_and_state()?))
        };
        match finish() {
            Ok((lifetime, ops)) => {
                rec.lifetime = Some(lifetime);
                rec.operations_and_state = Some(ops);
            }
            Err(e) => rec.error = Some(e.to_string()),
        }
    }
    recordings
}

fn step(neteq: &mut dyn NetEqUnderTest, rec: &mut Recording, t: i64, frame: usize) -> Result<()> {
    let out = neteq.get_audio()?;
    rec.frames.push(FrameRecord {
        time_ms: t,
        operation: out.operation,
        speech_type: out.speech_type,
        vad_activity: out.vad_activity,
        muted: out.muted,
        sample_rate_hz: out.sample_rate_hz,
        num_channels: out.num_channels,
        target_delay_ms: neteq.target_delay_ms()?,
        filtered_current_delay_ms: neteq.filtered_current_delay_ms()?,
        playout_timestamp: out.playout_timestamp,
        return_code: out.return_code,
        samples: out.samples,
    });
    if (frame + 1).is_multiple_of(NETWORK_STATS_INTERVAL_FRAMES) {
        rec.network_intervals.push(neteq.network_statistics(true)?);
    }
    Ok(())
}
