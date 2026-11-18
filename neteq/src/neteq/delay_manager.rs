// Port target: WebRTC modules/audio_coding/neteq/delay_manager.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: delay_manager_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// `DelayManager::Config` lives in `api::config::DelayManagerConfig`
// (field-trial parsing is not ported). The Q-format conversions
// (`(1 << 30) * quantile`, `(1 << 15) * forget_factor`) happen in `new`,
// in the C++ expression order.

//! Target delay from the underrun and reorder optimizers.

use crate::api::config::DelayManagerConfig;
use crate::api::delay_manager_interface::DelayManagerInterface;
use crate::api::neteq_controller::PacketArrivedInfo;
use crate::api::tick_timer::TickTimer;
use crate::neteq::reorder_optimizer::ReorderOptimizer;
use crate::neteq::underrun_optimizer::UnderrunOptimizer;

/// C++ `DelayManager::Config`.
pub type Config = DelayManagerConfig;

const START_DELAY_MS: i32 = 80;

fn maybe_create_reorder_optimizer(config: &DelayManagerConfig) -> Option<ReorderOptimizer> {
    if !config.use_reorder_optimizer {
        return None;
    }
    // C++ passes the doubles straight into `int` / `std::optional<int>`
    // parameters, so both conversions truncate.
    Some(ReorderOptimizer::new(
        ((1 << 15) as f64 * config.reorder_forget_factor) as i32,
        config.ms_per_loss_percent,
        config.start_forget_weight.map(|weight| weight as i32),
    ))
}

/// C++ `DelayManager`.
#[derive(Debug)]
pub struct DelayManager {
    underrun_optimizer: UnderrunOptimizer,
    reorder_optimizer: Option<ReorderOptimizer>,
    /// Currently preferred buffer level.
    target_level_ms: i32,
}

impl DelayManager {
    /// Builds the optimizers from `config`.
    pub fn new(config: &DelayManagerConfig, tick_timer: TickTimer) -> Self {
        let mut delay_manager = Self {
            underrun_optimizer: UnderrunOptimizer::new(
                tick_timer,
                ((1 << 30) as f64 * config.quantile) as i32,
                ((1 << 15) as f64 * config.forget_factor) as i32,
                config.start_forget_weight.map(|weight| weight as i32),
                config.resample_interval_ms,
            ),
            reorder_optimizer: maybe_create_reorder_optimizer(config),
            target_level_ms: START_DELAY_MS,
        };
        delay_manager.reset();
        delay_manager
    }
}

impl DelayManagerInterface for DelayManager {
    fn update(&mut self, arrival_delay_ms: i32, reordered: bool, _info: PacketArrivedInfo) {
        if self.reorder_optimizer.is_none() || !reordered {
            self.underrun_optimizer.update(arrival_delay_ms);
        }
        self.target_level_ms = self
            .underrun_optimizer
            .get_optimal_delay_ms()
            .unwrap_or(START_DELAY_MS);
        if let Some(reorder_optimizer) = self.reorder_optimizer.as_mut() {
            reorder_optimizer.update(arrival_delay_ms, reordered, self.target_level_ms);
            self.target_level_ms = std::cmp::max(
                self.target_level_ms,
                reorder_optimizer.get_optimal_delay_ms().unwrap_or(0),
            );
        }
    }

    fn reset(&mut self) {
        self.underrun_optimizer.reset();
        self.target_level_ms = START_DELAY_MS;
        if let Some(reorder_optimizer) = self.reorder_optimizer.as_mut() {
            reorder_optimizer.reset();
        }
    }

    fn target_delay_ms(&self) -> i32 {
        self.target_level_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // C++ DelayManagerTest.UpdateNormal
    #[test]
    fn update_normal() {
        let tick_timer = TickTimer::new();
        let mut dm = DelayManager::new(&DelayManagerConfig::default(), tick_timer.clone());
        for _ in 0..50 {
            dm.update(0, false, PacketArrivedInfo::default());
            tick_timer.increment_by(2);
        }
        assert_eq!(20, dm.target_delay_ms());
    }

    // Not in C++: the start delay holds until the first histogram update.
    #[test]
    fn start_delay_before_first_resample() {
        let tick_timer = TickTimer::new();
        let mut dm = DelayManager::new(&DelayManagerConfig::default(), tick_timer);
        assert_eq!(START_DELAY_MS, dm.target_delay_ms());
        dm.update(0, false, PacketArrivedInfo::default());
        // The reorder optimizer has seen one in-order packet: 20 ms, which is
        // below the 80 ms start delay.
        assert_eq!(START_DELAY_MS, dm.target_delay_ms());
        dm.reset();
        assert_eq!(START_DELAY_MS, dm.target_delay_ms());
    }

    // Not in C++: the Q-format conversions truncate like the C++ implicit
    // double -> int conversions.
    #[test]
    fn q_format_conversions_truncate() {
        let config = DelayManagerConfig::default();
        assert_eq!(((1 << 30) as f64 * config.quantile) as i32, 1020054732);
        assert_eq!(((1 << 15) as f64 * config.forget_factor) as i32, 32210);
        assert_eq!(
            ((1 << 15) as f64 * config.reorder_forget_factor) as i32,
            32745
        );
    }
}
