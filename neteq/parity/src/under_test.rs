//! The common surface both NetEqs are driven through.

use serde::{Deserialize, Serialize};

use crate::cneteq::CNetEq;
use crate::error::Result;
use crate::types::{
    AudioOutput, Codec, LifetimeStatistics, NetEqConfig, NetworkStatistics, OperationsAndState,
    RtpHeader,
};

/// A NetEq implementation the diff runner can drive in lockstep.
///
/// Methods mirror `webrtc::NetEq`. Anything an implementation cannot do yet
/// returns [`crate::HarnessError::NotImplemented`] instead of panicking, so
/// the runner can report how far a partial port gets.
pub trait NetEqUnderTest {
    /// Short name used in reports (`cpp`, `rust`).
    fn name(&self) -> &'static str;

    /// Registers the decoder for `codec` (and RFC 3389 CN at the codec's
    /// rate when `with_cn`), using the harness payload types.
    fn register_codec(&mut self, codec: &Codec, with_cn: bool) -> Result<()>;

    /// `NetEq::InsertPacket`; returns NetEq's return code (0 = ok).
    fn insert_packet(
        &mut self,
        header: &RtpHeader,
        payload: &[u8],
        receive_time_ms: i64,
    ) -> Result<i32>;

    /// `NetEq::GetAudio` for one 10 ms frame.
    fn get_audio(&mut self) -> Result<AudioOutput>;

    /// Advances the implementation's clock.
    fn advance_time_ms(&mut self, ms: i64);

    /// `NetEq::TargetDelayMs`.
    fn target_delay_ms(&self) -> Result<i32>;

    /// `NetEq::FilteredCurrentDelayMs`.
    fn filtered_current_delay_ms(&self) -> Result<i32>;

    /// `NetEq::NetworkStatistics` (reset) or `CurrentNetworkStatistics`.
    fn network_statistics(&mut self, reset: bool) -> Result<NetworkStatistics>;

    /// `NetEq::GetLifetimeStatistics`.
    fn lifetime_statistics(&self) -> Result<LifetimeStatistics>;

    /// `NetEq::GetOperationsAndState`.
    fn operations_and_state(&self) -> Result<OperationsAndState>;

    /// `NetEq::SetBaseMinimumDelayMs`.
    fn set_base_minimum_delay_ms(&mut self, delay_ms: i32) -> Result<bool>;

    /// `NetEq::FlushBuffers`.
    fn flush(&mut self) -> Result<()>;
}

/// Which implementation to instantiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Implementation {
    /// Google WebRTC C++ NetEq (the reference).
    Cpp,
    /// Our Rust `neteq` crate (needs the `rust-neteq` feature). Opus decodes
    /// through the same C libopus as the reference, so a diff tests NetEq.
    Rust,
    /// Our Rust `neteq` crate with its `ropus` Opus decoder. Report only:
    /// `ropus` drifts from libopus in CNG and PLC (see README), so this is
    /// not gated.
    RustRopus,
}

impl Implementation {
    /// Builds a fresh instance.
    pub fn build(
        self,
        config: &NetEqConfig,
        start_time_ms: i64,
    ) -> Result<Box<dyn NetEqUnderTest>> {
        match self {
            Implementation::Cpp => Ok(Box::new(CNetEq::new(config, start_time_ms)?)),
            #[cfg(feature = "rust-neteq")]
            Implementation::Rust | Implementation::RustRopus => {
                let kind = if self == Implementation::Rust {
                    crate::rust_adapter::OpusDecoderKind::Libopus
                } else {
                    crate::rust_adapter::OpusDecoderKind::Ropus
                };
                Ok(Box::new(crate::rust_adapter::RustNetEq::with_opus_decoder(
                    config,
                    start_time_ms,
                    kind,
                )?))
            }
            #[cfg(not(feature = "rust-neteq"))]
            Implementation::Rust | Implementation::RustRopus => {
                Err(crate::HarnessError::NotImplemented {
                    implementation: "rust",
                    what: "build with `--features rust-neteq`",
                })
            }
        }
    }
}

impl NetEqUnderTest for CNetEq {
    fn name(&self) -> &'static str {
        "cpp"
    }

    fn register_codec(&mut self, codec: &Codec, with_cn: bool) -> Result<()> {
        match *codec {
            Codec::L16 {
                sample_rate_hz,
                channels,
            } => self.register_l16(codec.payload_type(), sample_rate_hz, channels)?,
            Codec::Opus { channels } => self.register_opus(codec.payload_type(), channels)?,
        }
        if with_cn {
            self.register_cn(codec.cn_payload_type(), codec.rtp_clock_hz())?;
        }
        Ok(())
    }

    fn insert_packet(
        &mut self,
        header: &RtpHeader,
        payload: &[u8],
        receive_time_ms: i64,
    ) -> Result<i32> {
        Ok(CNetEq::insert_packet(
            self,
            header,
            payload,
            receive_time_ms,
        ))
    }

    fn get_audio(&mut self) -> Result<AudioOutput> {
        Ok(CNetEq::get_audio(self))
    }

    fn advance_time_ms(&mut self, ms: i64) {
        CNetEq::advance_time_ms(self, ms)
    }

    fn target_delay_ms(&self) -> Result<i32> {
        Ok(CNetEq::target_delay_ms(self))
    }

    fn filtered_current_delay_ms(&self) -> Result<i32> {
        Ok(CNetEq::filtered_current_delay_ms(self))
    }

    fn network_statistics(&mut self, reset: bool) -> Result<NetworkStatistics> {
        Ok(CNetEq::network_statistics(self, reset))
    }

    fn lifetime_statistics(&self) -> Result<LifetimeStatistics> {
        Ok(CNetEq::lifetime_statistics(self))
    }

    fn operations_and_state(&self) -> Result<OperationsAndState> {
        Ok(CNetEq::operations_and_state(self))
    }

    fn set_base_minimum_delay_ms(&mut self, delay_ms: i32) -> Result<bool> {
        Ok(CNetEq::set_base_minimum_delay_ms(self, delay_ms))
    }

    fn flush(&mut self) -> Result<()> {
        CNetEq::flush(self);
        Ok(())
    }
}
