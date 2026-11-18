// Port target: WebRTC modules/audio_coding/neteq/delay_constraints.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: delay_constraints_unittest.cc).
// Copyright (c) 2024 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Minimum, maximum and base-minimum delay bounds on the target delay.

use crate::rtc_base::safe_clamp;

/// C++ `DelayConstraints`.
#[derive(Debug, Clone)]
pub struct DelayConstraints {
    max_packets_in_buffer: i32,
    base_minimum_delay_ms: i32,
    /// Used as lower bound for target delay.
    effective_minimum_delay_ms: i32,
    /// Externally set minimum delay.
    minimum_delay_ms: i32,
    /// Externally set maximum delay. No maximum if 0.
    maximum_delay_ms: i32,
    packet_len_ms: i32,
}

const MIN_BASE_MINIMUM_DELAY_MS: i32 = 0;
const MAX_BASE_MINIMUM_DELAY_MS: i32 = 10000;

impl DelayConstraints {
    /// Constraints for a buffer of `max_packets_in_buffer` packets.
    pub fn new(max_packets_in_buffer: i32, base_minimum_delay_ms: i32) -> Self {
        Self {
            max_packets_in_buffer,
            base_minimum_delay_ms,
            effective_minimum_delay_ms: base_minimum_delay_ms,
            minimum_delay_ms: 0,
            maximum_delay_ms: 0,
            packet_len_ms: 0,
        }
    }

    /// Clamps `delay_ms` to the effective minimum and maximum.
    pub fn clamp(&self, delay_ms: i32) -> i32 {
        let mut delay_ms = std::cmp::max(delay_ms, self.effective_minimum_delay_ms);
        if self.maximum_delay_ms > 0 {
            delay_ms = std::cmp::min(delay_ms, self.maximum_delay_ms);
        }
        if self.packet_len_ms > 0 {
            // Limit to 75% of maximum buffer size.
            delay_ms = std::cmp::min(
                delay_ms,
                3 * self.max_packets_in_buffer * self.packet_len_ms / 4,
            );
        }
        delay_ms
    }

    /// Sets the packet length. Returns false if invalid.
    pub fn set_packet_audio_length(&mut self, length_ms: i32) -> bool {
        if length_ms <= 0 {
            log::error!("set_packet_audio_length: length_ms = {length_ms}");
            return false;
        }
        self.packet_len_ms = length_ms;
        true
    }

    /// Returns true if the minimum delay was applied.
    pub fn set_minimum_delay(&mut self, delay_ms: i32) -> bool {
        if !self.is_valid_minimum_delay(delay_ms) {
            return false;
        }
        self.minimum_delay_ms = delay_ms;
        self.update_effective_minimum_delay();
        true
    }

    /// Returns true if the maximum delay was applied. Zero unsets the
    /// maximum.
    pub fn set_maximum_delay(&mut self, delay_ms: i32) -> bool {
        if delay_ms != 0 && delay_ms < self.minimum_delay_ms {
            // Maximum delay shouldn't be less than minimum delay or less than
            // a packet.
            return false;
        }
        self.maximum_delay_ms = delay_ms;
        self.update_effective_minimum_delay();
        true
    }

    /// Returns true if the base minimum delay was applied.
    pub fn set_base_minimum_delay(&mut self, delay_ms: i32) -> bool {
        if !Self::is_valid_base_minimum_delay(delay_ms) {
            return false;
        }
        self.base_minimum_delay_ms = delay_ms;
        self.update_effective_minimum_delay();
        true
    }

    /// The base minimum delay.
    pub fn get_base_minimum_delay(&self) -> i32 {
        self.base_minimum_delay_ms
    }

    pub fn effective_minimum_delay_ms_for_test(&self) -> i32 {
        self.effective_minimum_delay_ms
    }

    /// Upper bound for the minimum delay, from the buffer size and
    /// `maximum_delay_ms`. Lower bound is a constant 0.
    fn minimum_delay_upper_bound(&self) -> i32 {
        // Choose the lowest possible bound discarding 0 cases which mean the
        // value is not set and unconstrained.
        let mut q75 = self.max_packets_in_buffer * self.packet_len_ms * 3 / 4;
        q75 = if q75 > 0 {
            q75
        } else {
            MAX_BASE_MINIMUM_DELAY_MS
        };
        let maximum_delay_ms = if self.maximum_delay_ms > 0 {
            self.maximum_delay_ms
        } else {
            MAX_BASE_MINIMUM_DELAY_MS
        };
        std::cmp::min(maximum_delay_ms, q75)
    }

    fn update_effective_minimum_delay(&mut self) {
        // Clamp `base_minimum_delay_ms` into the range which can be
        // effectively used.
        let base_minimum_delay_ms = safe_clamp(
            self.base_minimum_delay_ms,
            0,
            self.minimum_delay_upper_bound(),
        );
        self.effective_minimum_delay_ms =
            std::cmp::max(self.minimum_delay_ms, base_minimum_delay_ms);
    }

    fn is_valid_minimum_delay(&self, delay_ms: i32) -> bool {
        0 <= delay_ms && delay_ms <= self.minimum_delay_upper_bound()
    }

    fn is_valid_base_minimum_delay(delay_ms: i32) -> bool {
        (MIN_BASE_MINIMUM_DELAY_MS..=MAX_BASE_MINIMUM_DELAY_MS).contains(&delay_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_NUMBER_OF_PACKETS: i32 = 200;
    const FRAME_SIZE_MS: i32 = 20;
    const MAX_BUFFER_SIZE_MS: i32 = MAX_NUMBER_OF_PACKETS * FRAME_SIZE_MS;

    #[test]
    fn no_constraints() {
        let constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        assert_eq!(constraints.clamp(100), 100);
        assert_eq!(constraints.clamp(0), 0);
    }

    #[test]
    fn max_delay() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        const MAX_DELAY_MS: i32 = 60;
        assert!(constraints.set_maximum_delay(MAX_DELAY_MS));
        assert_eq!(constraints.clamp(100), MAX_DELAY_MS);
    }

    #[test]
    fn min_delay() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        const MIN_DELAY_MS: i32 = 7 * FRAME_SIZE_MS;
        constraints.set_minimum_delay(MIN_DELAY_MS);
        assert_eq!(constraints.clamp(20), MIN_DELAY_MS);
    }

    #[test]
    fn base_minimum_delay_check_valid_range() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        // Base minimum delay should be between [0, 10000] milliseconds.
        assert!(!constraints.set_base_minimum_delay(-1));
        assert!(!constraints.set_base_minimum_delay(10001));
        assert_eq!(constraints.get_base_minimum_delay(), 0);

        assert!(constraints.set_base_minimum_delay(7999));
        assert_eq!(constraints.get_base_minimum_delay(), 7999);
    }

    #[test]
    fn base_minimum_delay_lower_than_minimum_delay() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        const BASE_MINIMUM_DELAY_MS: i32 = 100;
        const MINIMUM_DELAY_MS: i32 = 200;
        assert!(constraints.set_base_minimum_delay(BASE_MINIMUM_DELAY_MS));
        assert!(constraints.set_minimum_delay(MINIMUM_DELAY_MS));
        assert_eq!(
            constraints.effective_minimum_delay_ms_for_test(),
            MINIMUM_DELAY_MS
        );
    }

    #[test]
    fn base_minimum_delay_greater_than_minimum_delay() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        const BASE_MINIMUM_DELAY_MS: i32 = 70;
        const MINIMUM_DELAY_MS: i32 = 30;
        assert!(constraints.set_base_minimum_delay(BASE_MINIMUM_DELAY_MS));
        assert!(constraints.set_minimum_delay(MINIMUM_DELAY_MS));
        assert_eq!(
            constraints.effective_minimum_delay_ms_for_test(),
            BASE_MINIMUM_DELAY_MS
        );
    }

    #[test]
    fn base_minimum_delay_greater_than_buffer_size() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        const BASE_MINIMUM_DELAY_MS: i32 = MAX_BUFFER_SIZE_MS + 1;
        const MINIMUM_DELAY_MS: i32 = 12;
        const MAXIMUM_DELAY_MS: i32 = 20;
        const MAX_BUFFER_SIZE_MS_Q75: i32 = 3 * MAX_BUFFER_SIZE_MS / 4;
        assert!(constraints.set_packet_audio_length(FRAME_SIZE_MS));

        assert!(constraints.set_maximum_delay(MAXIMUM_DELAY_MS));

        // Base minimum delay is greater than minimum delay, that is why we
        // clamp it to current the highest possible value which is maximum
        // delay.
        assert!(constraints.set_minimum_delay(MINIMUM_DELAY_MS));
        assert!(constraints.set_base_minimum_delay(BASE_MINIMUM_DELAY_MS));

        // Unset maximum value.
        assert!(constraints.set_maximum_delay(0));

        // With maximum value unset, the highest possible value now is 75% of
        // currently possible maximum buffer size.
        assert_eq!(
            constraints.effective_minimum_delay_ms_for_test(),
            MAX_BUFFER_SIZE_MS_Q75
        );
    }

    #[test]
    fn base_minimum_delay_greater_than_maximum_delay() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        const MAXIMUM_DELAY_MS: i32 = 400;
        const BASE_MINIMUM_DELAY_MS: i32 = MAXIMUM_DELAY_MS + 1;
        const MINIMUM_DELAY_MS: i32 = 20;
        assert!(constraints.set_maximum_delay(MAXIMUM_DELAY_MS));
        assert!(constraints.set_minimum_delay(MINIMUM_DELAY_MS));
        assert!(constraints.set_base_minimum_delay(BASE_MINIMUM_DELAY_MS));
        assert_eq!(
            constraints.effective_minimum_delay_ms_for_test(),
            MAXIMUM_DELAY_MS
        );
    }

    #[test]
    fn base_minimum_delay_lower_than_max_size() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        const MAXIMUM_DELAY_MS: i32 = 400;
        const BASE_MINIMUM_DELAY_MS: i32 = MAXIMUM_DELAY_MS - 1;
        const MINIMUM_DELAY_MS: i32 = 20;
        assert!(constraints.set_maximum_delay(MAXIMUM_DELAY_MS));
        assert!(constraints.set_minimum_delay(MINIMUM_DELAY_MS));
        assert!(constraints.set_base_minimum_delay(BASE_MINIMUM_DELAY_MS));
        assert_eq!(
            constraints.effective_minimum_delay_ms_for_test(),
            BASE_MINIMUM_DELAY_MS
        );
    }

    #[test]
    fn minimum_delay_memorization() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        const BASE_MINIMUM_DELAY_MS_LOW: i32 = 10;
        const MINIMUM_DELAY_MS: i32 = 20;
        const BASE_MINIMUM_DELAY_MS_HIGH: i32 = 30;

        assert!(constraints.set_base_minimum_delay(BASE_MINIMUM_DELAY_MS_LOW));
        assert!(constraints.set_minimum_delay(MINIMUM_DELAY_MS));
        // Minimum delay is used as it is higher than base minimum delay.
        assert_eq!(
            constraints.effective_minimum_delay_ms_for_test(),
            MINIMUM_DELAY_MS
        );

        assert!(constraints.set_base_minimum_delay(BASE_MINIMUM_DELAY_MS_HIGH));
        // Base minimum delay is used as it is now higher than minimum delay.
        assert_eq!(
            constraints.effective_minimum_delay_ms_for_test(),
            BASE_MINIMUM_DELAY_MS_HIGH
        );

        assert!(constraints.set_base_minimum_delay(BASE_MINIMUM_DELAY_MS_LOW));
        // Check that minimum delay is memorized and is used again.
        assert_eq!(
            constraints.effective_minimum_delay_ms_for_test(),
            MINIMUM_DELAY_MS
        );
    }

    #[test]
    fn base_minimum_delay() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        const BASE_MINIMUM_DELAY_MS: i32 = 7 * FRAME_SIZE_MS;
        assert!(constraints.set_base_minimum_delay(BASE_MINIMUM_DELAY_MS));
        assert_eq!(constraints.get_base_minimum_delay(), BASE_MINIMUM_DELAY_MS);
        assert_eq!(constraints.clamp(20), BASE_MINIMUM_DELAY_MS);
    }

    #[test]
    fn failures() {
        let mut constraints = DelayConstraints::new(MAX_NUMBER_OF_PACKETS, 0);
        // Wrong packet size.
        assert!(!constraints.set_packet_audio_length(0));
        assert!(!constraints.set_packet_audio_length(-1));

        // Minimum delay higher than a maximum delay is not accepted.
        assert!(constraints.set_maximum_delay(20));
        assert!(!constraints.set_minimum_delay(40));

        // Maximum delay less than minimum delay is not accepted.
        assert!(constraints.set_maximum_delay(100));
        assert!(constraints.set_minimum_delay(80));
        assert!(!constraints.set_maximum_delay(60));
    }
}
