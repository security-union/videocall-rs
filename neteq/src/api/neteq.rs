// Port of WebRTC api/neteq/neteq.h (`NetEq`, `Operation`, `Mode`,
// `DecoderFormat`) at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! The public NetEq facade.
//!
//! C++ return codes become `Result<_, NetEqError>` here and only here;
//! `NetEqImpl` keeps the integer codes so control flow matches C++.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::api::audio_decoder::AudioDecoderFactory;
use crate::api::audio_format::SdpAudioFormat;
use crate::api::audio_frame::AudioFrame;
use crate::api::clock::{Clock, SystemClock};
use crate::api::config::Config;
use crate::api::rtp::{RtpHeader, RtpPacketInfo};
use crate::api::stats::{NetEqLifetimeStatistics, NetEqNetworkStatistics, NetEqOperationsAndState};
use crate::codecs::BuiltinAudioDecoderFactory;
use crate::error::{NetEqError, NetEqImplError, Result};
use crate::neteq::neteq_impl::{ControllerFactory, Dependencies, NetEqImpl};

/// C++ `NetEq::Operation`: what NetEq does in one `GetAudio`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Operation {
    Normal,
    Merge,
    Expand,
    Accelerate,
    FastAccelerate,
    PreemptiveExpand,
    Rfc3389Cng,
    Rfc3389CngNoPacket,
    CodecInternalCng,
    Dtmf,
    #[default]
    Undefined,
}

/// C++ `NetEq::Mode`: the outcome of the last `GetAudio`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Mode {
    Normal,
    Expand,
    Merge,
    AccelerateSuccess,
    AccelerateLowEnergy,
    AccelerateFail,
    PreemptiveExpandSuccess,
    PreemptiveExpandLowEnergy,
    PreemptiveExpandFail,
    Rfc3389Cng,
    CodecInternalCng,
    CodecPlc,
    Dtmf,
    Error,
    #[default]
    Undefined,
}

/// C++ `NetEq::DecoderFormat`: info about the most recently used decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderFormat {
    pub payload_type: i32,
    pub sample_rate_hz: i32,
    pub num_channels: i32,
    pub sdp_format: SdpAudioFormat,
}

/// The C++ `GetAudio` out-parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetAudioResult {
    /// True if NetEq is in muted state; the frame data reads as zeros.
    pub muted: bool,
    /// Same as `last_output_sample_rate_hz()` after this call.
    pub current_sample_rate_hz: i32,
}

/// The NetEq jitter buffer. Thin wrapper over `NetEqImpl`.
pub struct NetEq {
    inner: NetEqImpl,
}

/// Maps a C++ `kOK` / `kFail` return code to `Result`. `cause` is the
/// internal error behind a `kFail`, when NetEq knows it.
fn check(return_code: i32, cause: NetEqImplError) -> Result<()> {
    if return_code == 0 {
        Ok(())
    } else {
        Err(NetEqError::Fail(cause))
    }
}

impl NetEq {
    /// Builds NetEq from explicit dependencies (tests, harness).
    pub fn new(config: Config, deps: Dependencies) -> Self {
        Self {
            inner: NetEqImpl::new(&config, deps, true),
        }
    }

    /// Starts a builder with the built-in decoder factory, `SystemClock` and
    /// the default `DecisionLogic` controller.
    pub fn builder(config: Config) -> NetEqBuilder {
        NetEqBuilder::new(config)
    }

    /// C++ `InsertPacket(header, payload, packet_info)`.
    pub fn insert_packet(
        &mut self,
        header: &RtpHeader,
        payload: &[u8],
        info: RtpPacketInfo,
    ) -> Result<()> {
        check(
            self.inner.insert_packet(header, payload, info),
            NetEqImplError::OtherError,
        )
    }

    /// C++ `GetAudio`. Writes 10 ms into `frame`. `action_override` forces the
    /// next operation (testing only).
    pub fn get_audio(
        &mut self,
        frame: &mut AudioFrame,
        action_override: Option<Operation>,
    ) -> Result<GetAudioResult> {
        let mut muted = false;
        let mut current_sample_rate_hz = 0;
        let return_code = self.inner.get_audio(
            frame,
            Some(&mut muted),
            Some(&mut current_sample_rate_hz),
            action_override,
        );
        check(return_code, self.inner.last_get_audio_error())?;
        Ok(GetAudioResult {
            muted,
            current_sample_rate_hz,
        })
    }

    /// Replaces the current set of decoders.
    pub fn set_codecs(&mut self, codecs: &BTreeMap<i32, SdpAudioFormat>) {
        self.inner.set_codecs(codecs)
    }

    /// Associates `rtp_payload_type` with `audio_format`. Returns true on success.
    pub fn register_payload_type(
        &mut self,
        rtp_payload_type: i32,
        audio_format: &SdpAudioFormat,
    ) -> bool {
        self.inner
            .register_payload_type(rtp_payload_type, audio_format)
    }

    /// Instantiates the decoder for `rtp_payload_type` ahead of time.
    pub fn create_decoder(&mut self, rtp_payload_type: i32) -> bool {
        self.inner.create_decoder(rtp_payload_type)
    }

    /// Removes `rtp_payload_type`. Removing an unknown type is not an error.
    pub fn remove_payload_type(&mut self, rtp_payload_type: u8) -> Result<()> {
        check(
            self.inner.remove_payload_type(rtp_payload_type),
            NetEqImplError::OtherError,
        )
    }

    /// Removes all payload types.
    pub fn remove_all_payload_types(&mut self) {
        self.inner.remove_all_payload_types()
    }

    /// Sets the maximum number of packets in the packet buffer.
    pub fn set_maximum_buffer_packets(&mut self, max_packets: usize) {
        self.inner.set_maximum_buffer_packets(max_packets)
    }

    /// Enables or disables fast accelerate.
    pub fn set_fast_accelerate(&mut self, enable: bool) {
        self.inner.set_fast_accelerate(enable)
    }

    /// C++ `SetMinimumDelay`. Returns true if applied.
    pub fn set_minimum_delay(&mut self, delay_ms: i32) -> bool {
        self.inner.set_minimum_delay(delay_ms)
    }

    /// C++ `SetMaximumDelay`. Returns true if applied.
    pub fn set_maximum_delay(&mut self, delay_ms: i32) -> bool {
        self.inner.set_maximum_delay(delay_ms)
    }

    /// C++ `SetBaseMinimumDelayMs`. Returns true if applied.
    pub fn set_base_minimum_delay_ms(&mut self, delay_ms: i32) -> bool {
        self.inner.set_base_minimum_delay_ms(delay_ms)
    }

    /// C++ `GetBaseMinimumDelayMs`.
    pub fn get_base_minimum_delay_ms(&self) -> i32 {
        self.inner.get_base_minimum_delay_ms()
    }

    /// C++ `TargetDelayMs`.
    pub fn target_delay_ms(&self) -> i32 {
        self.inner.target_delay_ms()
    }

    /// C++ `FilteredCurrentDelayMs`.
    pub fn filtered_current_delay_ms(&self) -> i32 {
        self.inner.filtered_current_delay_ms()
    }

    /// C++ `NetworkStatistics`. Resets the rate counters, like C++.
    pub fn network_statistics(&mut self) -> NetEqNetworkStatistics {
        let mut stats = NetEqNetworkStatistics::default();
        self.inner.network_statistics(&mut stats);
        stats
    }

    /// C++ `CurrentNetworkStatistics`. Does not reset anything.
    pub fn current_network_statistics(&self) -> NetEqNetworkStatistics {
        self.inner.current_network_statistics()
    }

    /// C++ `GetLifetimeStatistics`.
    pub fn lifetime_statistics(&self) -> NetEqLifetimeStatistics {
        self.inner.get_lifetime_statistics()
    }

    /// C++ `GetOperationsAndState`.
    pub fn operations_and_state(&self) -> NetEqOperationsAndState {
        self.inner.get_operations_and_state()
    }

    /// C++ `GetPlayoutTimestamp`.
    pub fn playout_timestamp(&self) -> Option<u32> {
        self.inner.get_playout_timestamp()
    }

    /// C++ `last_output_sample_rate_hz`.
    pub fn last_output_sample_rate_hz(&self) -> i32 {
        self.inner.last_output_sample_rate_hz()
    }

    /// C++ `GetCurrentDecoderFormat`.
    pub fn current_decoder_format(&self) -> Option<DecoderFormat> {
        self.inner.get_current_decoder_format()
    }

    /// Flushes both the packet buffer and the sync buffer.
    pub fn flush_buffers(&mut self) {
        self.inner.flush_buffers()
    }

    /// Length of the audio yet to play in the sync buffer, in ms.
    pub fn sync_buffer_size_ms(&self) -> i32 {
        self.inner.sync_buffer_size_ms()
    }

    /// Final operation of the last `get_audio` (C++
    /// `last_operation_for_test`); this name is for production telemetry
    /// (`telemetry::TelemetryCollector::on_get_audio`).
    pub fn last_operation(&self) -> Operation {
        self.inner.last_operation_for_test()
    }

    /// Final operation of the last `get_audio` (C++ `last_operation_for_test`).
    #[cfg(any(test, feature = "parity"))]
    pub fn last_operation_for_test(&self) -> Operation {
        self.inner.last_operation_for_test()
    }

    /// Mode after the last `get_audio` (C++ `last_mode_`).
    #[cfg(any(test, feature = "parity"))]
    pub fn last_mode_for_test(&self) -> Mode {
        self.inner.last_mode_for_test()
    }

    /// The sync buffer (C++ `sync_buffer_for_test`).
    #[cfg(any(test, feature = "parity"))]
    pub fn sync_buffer_for_test(&self) -> &crate::neteq::sync_buffer::SyncBuffer {
        self.inner.sync_buffer_for_test()
    }

    /// Installs a per-tick observer for the parity harness.
    #[cfg(feature = "parity")]
    pub fn set_observer(&mut self, observer: Box<dyn crate::parity::Observer>) {
        self.inner.set_observer(observer)
    }
}

/// Builder for [`NetEq`] with production defaults. Replaces C++
/// `NetEqFactory` / `NetEqControllerFactory`.
pub struct NetEqBuilder {
    config: Config,
    decoder_factory: Option<Arc<dyn AudioDecoderFactory>>,
    clock: Option<Arc<dyn Clock>>,
    controller_factory: Option<ControllerFactory>,
}

impl NetEqBuilder {
    /// Starts a builder for `config`.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            decoder_factory: None,
            clock: None,
            controller_factory: None,
        }
    }

    /// Overrides the decoder factory (default: `codecs::BuiltinAudioDecoderFactory`).
    pub fn decoder_factory(mut self, factory: Arc<dyn AudioDecoderFactory>) -> Self {
        self.decoder_factory = Some(factory);
        self
    }

    /// Overrides the clock (default: `SystemClock`).
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Overrides the controller (default: `DecisionLogic` + `DelayManager`).
    pub fn controller_factory(mut self, factory: ControllerFactory) -> Self {
        self.controller_factory = Some(factory);
        self
    }

    /// Builds NetEq.
    pub fn build(self) -> NetEq {
        let decoder_factory = self
            .decoder_factory
            .unwrap_or_else(|| Arc::new(BuiltinAudioDecoderFactory::new()));
        let clock = self.clock.unwrap_or_else(|| Arc::new(SystemClock));
        let deps = Dependencies::new(
            &self.config,
            clock,
            decoder_factory,
            self.controller_factory,
        );
        NetEq::new(self.config, deps)
    }
}
