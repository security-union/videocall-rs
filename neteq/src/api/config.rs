// Port of `NetEq::Config` (WebRTC api/neteq/neteq.h), `DelayManager::Config`
// (modules/audio_coding/neteq/delay_manager.h) and `SmartFlushingConfig`
// (modules/audio_coding/neteq/packet_buffer.h) at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! NetEq configuration.
//!
//! WebRTC reads some knobs from field trials. We do not port field-trial
//! parsing; the typed blocks below carry the same defaults as an empty
//! field-trial string (PARITY_PLAN.md section 9).

use serde::{Deserialize, Serialize};

/// Mirrors `NetEq::Config`, plus the typed field-trial blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Initial value. Will change with input data.
    pub sample_rate_hz: i32,
    pub max_packets_in_buffer: usize,
    pub max_delay_ms: i32,
    pub min_delay_ms: i32,
    pub enable_fast_accelerate: bool,
    pub enable_muted_state: bool,
    pub enable_rtx_handling: bool,
    /// Use only for testing.
    pub for_test_no_time_stretching: bool,
    /// `WebRTC-Audio-NetEqDelayManagerConfig`.
    pub delay_manager: DelayManagerConfig,
    /// `WebRTC-Audio-NetEqSmartFlushing`. `None` (the default) means a full
    /// buffer triggers a full flush.
    pub smart_flushing: Option<SmartFlushingConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48000,
            max_packets_in_buffer: 200,
            max_delay_ms: 0,
            min_delay_ms: 0,
            enable_fast_accelerate: false,
            enable_muted_state: false,
            enable_rtx_handling: false,
            for_test_no_time_stretching: false,
            delay_manager: DelayManagerConfig::default(),
            smart_flushing: None,
        }
    }
}

/// Mirrors `DelayManager::Config`. Values stay in floating point here; the
/// Q-format conversions happen in `DelayManager::new` exactly as in C++
/// (e.g. `((1 << 30) as f64 * quantile) as i32`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayManagerConfig {
    pub quantile: f64,
    pub forget_factor: f64,
    pub start_forget_weight: Option<f64>,
    pub resample_interval_ms: Option<i32>,
    pub use_reorder_optimizer: bool,
    pub reorder_forget_factor: f64,
    pub ms_per_loss_percent: i32,
}

impl Default for DelayManagerConfig {
    fn default() -> Self {
        Self {
            quantile: 0.95,
            forget_factor: 0.983,
            start_forget_weight: Some(2.0),
            resample_interval_ms: Some(500),
            use_reorder_optimizer: true,
            reorder_forget_factor: 0.9993,
            ms_per_loss_percent: 20,
        }
    }
}

/// Mirrors `SmartFlushingConfig` from `packet_buffer.h`. Only used when
/// `Config::smart_flushing` is `Some`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SmartFlushingConfig {
    /// When calculating the flushing threshold, the maximum between the target
    /// level and this value is used.
    pub target_level_threshold_ms: i32,
    /// A smart flush is triggered when the packet buffer contains a multiple of
    /// the target level.
    pub target_level_multiplier: f64,
}

impl Default for SmartFlushingConfig {
    fn default() -> Self {
        Self {
            target_level_threshold_ms: 600,
            target_level_multiplier: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_webrtc_empty_field_trials() {
        let config = Config::default();
        assert_eq!(48000, config.sample_rate_hz);
        assert_eq!(200, config.max_packets_in_buffer);
        assert_eq!(0, config.max_delay_ms);
        assert_eq!(0, config.min_delay_ms);
        assert!(!config.enable_fast_accelerate);
        assert!(!config.enable_muted_state);
        assert!(!config.enable_rtx_handling);
        assert!(!config.for_test_no_time_stretching);
        assert!(config.smart_flushing.is_none());
        let dm = config.delay_manager;
        assert_eq!(0.95, dm.quantile);
        assert_eq!(0.983, dm.forget_factor);
        assert_eq!(Some(2.0), dm.start_forget_weight);
        assert_eq!(Some(500), dm.resample_interval_ms);
        assert!(dm.use_reorder_optimizer);
        assert_eq!(0.9993, dm.reorder_forget_factor);
        assert_eq!(20, dm.ms_per_loss_percent);
        let sf = SmartFlushingConfig::default();
        assert_eq!(600, sf.target_level_threshold_ms);
        assert_eq!(1.0, sf.target_level_multiplier);
    }
}
