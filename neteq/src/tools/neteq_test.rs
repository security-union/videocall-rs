// Port target: WebRTC modules/audio_coding/neteq/tools/neteq_test.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2016 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// `run_to_next_get_audio` is the one definition of "the same inputs"
// (PARITY_PLAN.md 7.1): advance the `SimulatedClock` to the next event,
// insert every packet whose arrival time has come (with
// `receive_time = now`), then `get_audio` on a 10 ms grid.

//! Drives a `NetEq` from a `NetEqInput` on simulated time.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::api::audio_decoder::AudioDecoderFactory;
use crate::api::audio_format::SdpAudioFormat;
use crate::api::audio_frame::AudioFrame;
use crate::api::clock::{Clock, SimulatedClock};
use crate::api::config::Config;
use crate::api::neteq::{NetEq, NetEqBuilder, Operation};
use crate::api::rtp::RtpPacketInfo;
use crate::api::stats::{NetEqLifetimeStatistics, NetEqNetworkStatistics};
use crate::tools::audio_sink::AudioSink;
use crate::tools::neteq_input::NetEqInput;

/// C++ `NetEqSimulator::SimulationStepResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimulationStepResult {
    /// True when the input has ended.
    pub is_simulation_finished: bool,
    /// Simulated time spent in this step.
    pub simulation_step_ms: i64,
}

/// C++ `NetEqTest`, without callbacks, text log and delay-analysis state.
pub struct NetEqTest {
    clock: SimulatedClock,
    neteq: NetEq,
    input: Box<dyn NetEqInput>,
    output: Box<dyn AudioSink>,
    next_action: Option<Operation>,
    out_frame: AudioFrame,
}

impl NetEqTest {
    /// Builds NetEq with `config`, registers `codecs`, and prepares to
    /// replay `input` into `output`. The simulated clock starts at the
    /// input's first event time, like C++.
    pub fn new(
        config: Config,
        decoder_factory: Arc<dyn AudioDecoderFactory>,
        codecs: BTreeMap<i32, SdpAudioFormat>,
        input: Box<dyn NetEqInput>,
        output: Box<dyn AudioSink>,
    ) -> Self {
        let start_ms = input.next_event_time().unwrap_or(0);
        let clock = SimulatedClock::new(start_ms * 1000);
        let mut neteq = NetEqBuilder::new(config)
            .decoder_factory(decoder_factory)
            .clock(Arc::new(clock.clone()))
            .build();
        for (payload_type, format) in &codecs {
            assert!(
                neteq.register_payload_type(*payload_type, format),
                "failed to register payload type {payload_type}"
            );
        }
        Self {
            clock,
            neteq,
            input,
            output,
            next_action: None,
            out_frame: AudioFrame::new(),
        }
    }

    /// Runs to the end of the input. Returns the simulation time in ms.
    pub fn run(&mut self) -> i64 {
        let mut simulation_time = 0;
        loop {
            let step_result = self.run_to_next_get_audio();
            simulation_time += step_result.simulation_step_ms;
            if step_result.is_simulation_finished {
                break;
            }
        }
        simulation_time
    }

    /// Runs until (and including) the next `get_audio`: advances the
    /// simulated clock event by event, inserts every packet whose arrival
    /// time has come (with `receive_time = now`), applies due base minimum
    /// delay changes, then calls `get_audio`.
    pub fn run_to_next_get_audio(&mut self) -> SimulationStepResult {
        let mut result = SimulationStepResult::default();
        let start_time_ms = self.input.next_event_time().unwrap_or(0);
        let mut time_now_ms = self.clock.current_time_us().div_euclid(1000);

        while !self.input.ended() {
            // Advance time to next event.
            let Some(next_event_time) = self.input.next_event_time() else {
                break;
            };
            self.clock.advance_time_ms(next_event_time - time_now_ms);
            time_now_ms = next_event_time;
            // Check if it is time to insert packet.
            if self
                .input
                .next_packet_time()
                .is_some_and(|packet_time| time_now_ms >= packet_time)
            {
                let packet = self.input.pop_packet().expect("a packet is due");
                if !packet.payload.is_empty() {
                    let info = RtpPacketInfo::new(&packet.header, Some(time_now_ms * 1000));
                    // Errors are reported through callbacks in C++; there are
                    // none here.
                    let _ = self
                        .neteq
                        .insert_packet(&packet.header, &packet.payload, info);
                }
            }

            if let Some(info) = self.input.next_set_minimum_delay_info() {
                if time_now_ms >= info.timestamp_ms {
                    self.neteq.set_base_minimum_delay_ms(info.delay_ms);
                    self.input.advance_set_minimum_delay();
                }
            }

            // Check if it is time to get output audio.
            if self
                .input
                .next_output_event_time()
                .is_some_and(|output_time| time_now_ms >= output_time)
            {
                let action = self.next_action.take();
                let get_audio_ok = self.neteq.get_audio(&mut self.out_frame, action).is_ok();
                let _ = get_audio_ok;
                assert!(
                    self.output.write_audio_frame(&self.out_frame),
                    "audio sink write failed"
                );

                self.input.advance_output_event();
                result.simulation_step_ms =
                    self.input.next_event_time().unwrap_or(time_now_ms) - start_time_ms;
                let operations_state = self.neteq.operations_and_state();
                let no_more_packets_to_decode = self.input.next_packet_time().is_none()
                    && !operations_state.next_packet_available;
                result.is_simulation_finished = no_more_packets_to_decode || self.input.ended();
                return result;
            }
        }
        result.simulation_step_ms =
            self.input.next_event_time().unwrap_or(time_now_ms) - start_time_ms;
        result.is_simulation_finished = true;
        result
    }

    /// Forces the operation of the next `get_audio` (C++ `SetNextAction`).
    pub fn set_next_action(&mut self, operation: Operation) {
        self.next_action = Some(operation);
    }

    /// The NetEq under test.
    pub fn neteq(&mut self) -> &mut NetEq {
        &mut self.neteq
    }

    /// The frame written by the last `get_audio`.
    pub fn last_frame(&self) -> &AudioFrame {
        &self.out_frame
    }

    /// Resetting network statistics (C++ `SimulationStats`).
    pub fn simulation_stats(&mut self) -> NetEqNetworkStatistics {
        self.neteq.network_statistics()
    }

    /// Lifetime statistics.
    pub fn lifetime_stats(&self) -> NetEqLifetimeStatistics {
        self.neteq.lifetime_statistics()
    }
}
