/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

//! Transport-layer network-condition simulator for the synthetic bot.
//!
//! A [`NetSimShim`] shapes the byte stream flowing through a single direction
//! (uplink or downlink) between the bot's application layer and its transport
//! (WebTransport uni streams, WebTransport datagrams, or WebSocket frames).
//!
//! Because the shim operates on the application byte stream, every transport
//! sees it identically: the QUIC / TLS handshake is below the shim and is
//! never impaired.
//!
//! Model:
//! - Bernoulli packet drop using `loss_pct`.
//! - Token-bucket bandwidth shaping with a 1-second burst capacity.
//! - Base latency plus uniform jitter drawn from `[0, 2 * jitter_ms]`.
//! - Bernoulli duplicate using `duplicate_pct` — caller is responsible for
//!   emitting the extra copy.
//! - `reorder_pct`: a coarse approximation. Each admission is independent,
//!   and because per-packet delays are sampled independently, jittered
//!   packets already finish in a shuffled order. When `reorder_pct > 0`
//!   we additionally draw a second small-but-noticeable delay on a fraction
//!   of packets to amplify out-of-order delivery. See [`NetworkProfile::reorder_pct`].
//!
//! Zero-cost in the passthrough case: [`NetworkProfile::passthrough`] /
//! [`NetworkProfile::is_passthrough`] let the caller skip all channel hops
//! and task plumbing so the hot path is unchanged.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(feature = "metrics")]
use std::sync::Arc;

#[cfg(feature = "metrics")]
use crate::metrics_server::BotMetrics;

/// Per-direction network shaping parameters.
///
/// Jitter convention: the shim adds a **uniform** random delay drawn from
/// `[0, 2 * jitter_ms]` on top of `latency_ms`. That is, the total one-way
/// delay falls in `[latency_ms, latency_ms + 2 * jitter_ms]`, with mean
/// `latency_ms + jitter_ms`. This is the single source of truth for the
/// jitter distribution used throughout the shim and its tests.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkProfile {
    /// Base one-way latency added to every admitted packet.
    pub latency_ms: u32,
    /// Jitter magnitude. Adds a uniform `[0, 2 * jitter_ms]` extra delay.
    /// See the struct-level doc for the full convention.
    pub jitter_ms: u32,
    /// Packet loss probability, percent (`0.0`–`100.0`).
    pub loss_pct: f32,
    /// Packet duplication probability, percent (`0.0`–`100.0`).
    pub duplicate_pct: f32,
    /// Reorder probability, percent (`0.0`–`100.0`). Approximated as an
    /// extra random delay applied to a fraction of packets — see module docs.
    pub reorder_pct: f32,
    /// Outbound bandwidth cap in kilobits/second. `None` means unlimited.
    pub uplink_kbps: Option<u32>,
    /// Inbound bandwidth cap in kilobits/second. `None` means unlimited.
    pub downlink_kbps: Option<u32>,
    /// Deterministic RNG seed (for tests). `None` draws an OS-random seed.
    pub seed: Option<u64>,
}

impl Default for NetworkProfile {
    fn default() -> Self {
        Self::passthrough()
    }
}

impl NetworkProfile {
    /// A profile that applies no impairment. `is_passthrough` returns `true`.
    pub fn passthrough() -> Self {
        Self {
            latency_ms: 0,
            jitter_ms: 0,
            loss_pct: 0.0,
            duplicate_pct: 0.0,
            reorder_pct: 0.0,
            uplink_kbps: None,
            downlink_kbps: None,
            seed: None,
        }
    }

    /// Returns `true` when the profile has no observable effect and the
    /// shim can be bypassed entirely.
    pub fn is_passthrough(&self) -> bool {
        self.latency_ms == 0
            && self.jitter_ms == 0
            && self.loss_pct <= 0.0
            && self.duplicate_pct <= 0.0
            && self.reorder_pct <= 0.0
            && self.uplink_kbps.is_none()
            && self.downlink_kbps.is_none()
    }

    /// Validate ranges. Returns a human-readable message describing the
    /// first out-of-range field, or `Ok(())`.
    pub fn validate(&self) -> Result<(), String> {
        if self.latency_ms > 5_000 {
            return Err(format!("latency_ms={} exceeds max 5000", self.latency_ms));
        }
        for (name, v) in [
            ("loss_pct", self.loss_pct),
            ("duplicate_pct", self.duplicate_pct),
            ("reorder_pct", self.reorder_pct),
        ] {
            if v.is_nan() {
                return Err(format!("{name} is NaN"));
            }
            if !(0.0..=100.0).contains(&v) {
                return Err(format!("{name}={v} out of range [0.0, 100.0]"));
            }
        }
        for (name, v) in [
            ("uplink_kbps", self.uplink_kbps),
            ("downlink_kbps", self.downlink_kbps),
        ] {
            if let Some(kbps) = v {
                if !(8..=1_000_000).contains(&kbps) {
                    return Err(format!("{name}={kbps} out of range [8, 1_000_000]"));
                }
            }
        }
        Ok(())
    }
}

/// Which direction a given shim is applied to. Only affects which
/// `uplink_kbps` / `downlink_kbps` entry is honored for bandwidth shaping.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Traffic leaving the bot (outbound, sender-side).
    Up,
    /// Traffic arriving at the bot (inbound, receiver-side).
    Down,
}

/// Result of a single [`NetSimShim::admit`] call. The caller applies the
/// decision: sleep for the given duration, emit a duplicate, or drop.
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    /// Forward immediately.
    Pass,
    /// Forward after sleeping `Duration`.
    Delay(Duration),
    /// Forward after sleeping `Duration`, then emit an additional copy.
    DelayAndDuplicate(Duration),
    /// Drop the packet entirely.
    Drop,
}

/// Per-direction network-impairment shim. One instance shapes one byte
/// stream; wrap in `Arc` for cross-task sharing.
pub struct NetSimShim {
    profile: NetworkProfile,
    /// Exposed via [`Self::direction`] for diagnostics / tests.
    direction: Direction,
    rng: Mutex<StdRng>,
    bucket: Mutex<Option<TokenBucket>>,
    /// Optional metrics hook. Set via [`Self::set_metrics`] to publish
    /// drop / delay observations to Prometheus. Absent by default to keep
    /// the shim usable from unit tests without any metrics plumbing.
    #[cfg(feature = "metrics")]
    metrics: Option<NetsimMetrics>,
}

/// Pre-computed Prometheus metric handles for a single netsim shim.
#[cfg(feature = "metrics")]
struct NetsimMetrics {
    metrics: Arc<BotMetrics>,
    bot: String,
    direction_label: &'static str,
}

impl NetSimShim {
    /// Create a shim for the given profile and direction. The per-direction
    /// rate limit is applied iff the corresponding `*_kbps` field is set.
    pub fn new(profile: NetworkProfile, direction: Direction) -> Self {
        let seed = profile
            .seed
            .unwrap_or_else(|| rand::thread_rng().gen::<u64>());
        let rng = StdRng::seed_from_u64(seed);

        let rate_kbps = match direction {
            Direction::Up => profile.uplink_kbps,
            Direction::Down => profile.downlink_kbps,
        };
        let bucket = rate_kbps.map(|kbps| {
            // 1-second burst capacity.
            let bytes_per_sec = (kbps as f64) * 1000.0 / 8.0;
            TokenBucket {
                capacity_bytes: bytes_per_sec,
                tokens: bytes_per_sec,
                refill_rate_bytes_per_sec: bytes_per_sec,
                last_refill: Instant::now(),
            }
        });

        Self {
            profile,
            direction,
            rng: Mutex::new(rng),
            bucket: Mutex::new(bucket),
            #[cfg(feature = "metrics")]
            metrics: None,
        }
    }

    /// Install a metrics hook on this shim. Returns the same shim so the
    /// caller can chain the call when building an `Arc<NetSimShim>`.
    #[cfg(feature = "metrics")]
    pub fn with_metrics(mut self, metrics: Arc<BotMetrics>, bot: String) -> Self {
        let direction_label = match self.direction {
            Direction::Up => "up",
            Direction::Down => "down",
        };
        self.metrics = Some(NetsimMetrics {
            metrics,
            bot,
            direction_label,
        });
        self
    }

    /// `true` when the underlying profile is passthrough.
    #[allow(dead_code)] // part of the public shim API; used by callers and tests
    pub fn is_passthrough(&self) -> bool {
        self.profile.is_passthrough()
    }

    /// Which direction this shim was built for.
    #[allow(dead_code)] // part of the public shim API; used by callers and tests
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Decide how a packet of `size_bytes` should be handled under the
    /// current profile and bucket state. Thread-safe.
    pub fn admit(&self, size_bytes: usize) -> Admission {
        if self.profile.is_passthrough() {
            return Admission::Pass;
        }

        let mut rng = self.rng.lock().expect("netsim RNG poisoned");

        // Step 1: loss.
        if self.profile.loss_pct > 0.0 {
            let roll: f32 = rng.gen::<f32>() * 100.0;
            if roll < self.profile.loss_pct {
                #[cfg(feature = "metrics")]
                self.record_drop("loss");
                return Admission::Drop;
            }
        }

        let mut total_delay = Duration::ZERO;

        // Step 2: bandwidth / token bucket.
        // We take the RNG lock first and the bucket lock second; no other
        // code path takes both in the opposite order, so deadlock is not
        // possible. The clamp on `extra_delay` caps pathological queueing.
        {
            let mut bucket_guard = self.bucket.lock().expect("netsim bucket poisoned");
            if let Some(bucket) = bucket_guard.as_mut() {
                let extra = bucket.consume(size_bytes);
                // Cap to 5 seconds so a single oversize packet cannot stall
                // the entire pipeline indefinitely.
                total_delay += extra.min(Duration::from_secs(5));
            }
        }

        // Step 3: base latency + jitter.
        //
        // Uniform jitter in [0, 2*jitter_ms]. See `NetworkProfile` struct doc.
        let jitter_ms = if self.profile.jitter_ms > 0 {
            rng.gen_range(0..=(2 * self.profile.jitter_ms))
        } else {
            0
        };
        let base = u64::from(self.profile.latency_ms) + u64::from(jitter_ms);
        total_delay += Duration::from_millis(base);

        // Step 4: reorder amplification.
        //
        // Reorder is already implicit via independent per-packet jitter —
        // this extra term bumps out-of-order probability when users ask for
        // noticeable reorder specifically.
        if self.profile.reorder_pct > 0.0 {
            let roll: f32 = rng.gen::<f32>() * 100.0;
            if roll < self.profile.reorder_pct {
                // Draw a second uniform delay from [0, 4*jitter_ms] or a
                // fixed 20ms floor when jitter_ms is 0.
                let bump = if self.profile.jitter_ms > 0 {
                    rng.gen_range(0..=(4 * self.profile.jitter_ms))
                } else {
                    rng.gen_range(0..=20)
                };
                total_delay += Duration::from_millis(u64::from(bump));
            }
        }

        // Step 5: duplicate check.
        if self.profile.duplicate_pct > 0.0 {
            let roll: f32 = rng.gen::<f32>() * 100.0;
            if roll < self.profile.duplicate_pct {
                return Admission::DelayAndDuplicate(total_delay);
            }
        }

        if total_delay.is_zero() {
            Admission::Pass
        } else {
            #[cfg(feature = "metrics")]
            self.record_delay(total_delay);
            Admission::Delay(total_delay)
        }
    }

    /// Increment the per-direction drop counter with the supplied reason.
    /// No-op when metrics are off or unbound.
    #[cfg(feature = "metrics")]
    fn record_drop(&self, reason: &str) {
        if let Some(m) = &self.metrics {
            m.metrics
                .netsim_dropped_total
                .with_label_values(&[m.bot.as_str(), m.direction_label, reason])
                .inc();
        }
    }

    /// Observe an injected delay (in milliseconds) on the per-direction
    /// histogram. No-op when metrics are off or unbound.
    #[cfg(feature = "metrics")]
    fn record_delay(&self, delay: Duration) {
        if let Some(m) = &self.metrics {
            m.metrics
                .netsim_delay_ms
                .with_label_values(&[m.bot.as_str(), m.direction_label])
                .observe(delay.as_secs_f64() * 1000.0);
        }
    }
}

/// Simple token bucket with floating-point token accounting. Tokens are
/// measured in bytes.
struct TokenBucket {
    capacity_bytes: f64,
    tokens: f64,
    refill_rate_bytes_per_sec: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Attempt to take `size_bytes` from the bucket. Returns the additional
    /// delay the caller must wait before the packet can be considered sent.
    ///
    /// When the bucket is short, we model the caller sleeping for the returned
    /// duration: the bucket's tokens are drained to zero AND `last_refill` is
    /// advanced forward by the wait. That way, back-to-back admits that each
    /// find the bucket empty correctly accrue successively larger delays rather
    /// than each one starting from the same "bucket full in dt" baseline.
    fn consume(&mut self, size_bytes: usize) -> Duration {
        let now = Instant::now();
        let dt = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + dt * self.refill_rate_bytes_per_sec).min(self.capacity_bytes);
        self.last_refill = now;

        let need = size_bytes as f64;
        if self.tokens >= need {
            self.tokens -= need;
            return Duration::ZERO;
        }

        let deficit = need - self.tokens;
        let wait_secs = deficit / self.refill_rate_bytes_per_sec;
        // Drain the bucket and pretend `wait_secs` of wall clock has already
        // passed — the caller is contracted to sleep that long.
        self.tokens = 0.0;
        self.last_refill = now + Duration::from_secs_f64(wait_secs);
        Duration::from_secs_f64(wait_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_admits_all() {
        let shim = NetSimShim::new(NetworkProfile::passthrough(), Direction::Up);
        assert!(shim.is_passthrough());
        for _ in 0..1000 {
            assert_eq!(shim.admit(1234), Admission::Pass);
        }
    }

    #[test]
    fn constant_latency() {
        let profile = NetworkProfile {
            latency_ms: 100,
            jitter_ms: 0,
            seed: Some(1),
            ..Default::default()
        };
        let shim = NetSimShim::new(profile, Direction::Up);
        for _ in 0..100 {
            match shim.admit(500) {
                Admission::Delay(d) => assert_eq!(d, Duration::from_millis(100)),
                other => panic!("expected Delay(100ms), got {:?}", other),
            }
        }
    }

    #[test]
    fn jitter_range() {
        // Convention: delays ∈ [latency_ms, latency_ms + 2 * jitter_ms].
        let profile = NetworkProfile {
            latency_ms: 100,
            jitter_ms: 50,
            seed: Some(7),
            ..Default::default()
        };
        let shim = NetSimShim::new(profile, Direction::Up);
        let mut mn = Duration::from_millis(u64::MAX);
        let mut mx = Duration::ZERO;
        for _ in 0..1000 {
            match shim.admit(100) {
                Admission::Delay(d) => {
                    assert!(
                        d >= Duration::from_millis(100) && d <= Duration::from_millis(200),
                        "delay {:?} outside [100ms, 200ms]",
                        d
                    );
                    if d < mn {
                        mn = d;
                    }
                    if d > mx {
                        mx = d;
                    }
                }
                other => panic!("expected Delay, got {:?}", other),
            }
        }
        eprintln!("jitter_range: min={:?}, max={:?}", mn, mx);
        // With 1000 samples across a 100ms range we expect to see near the
        // extremes.
        assert!(mn <= Duration::from_millis(110), "min too high: {:?}", mn);
        assert!(mx >= Duration::from_millis(190), "max too low: {:?}", mx);
    }

    #[test]
    fn loss_rate() {
        let profile = NetworkProfile {
            loss_pct: 50.0,
            seed: Some(42),
            ..Default::default()
        };
        let shim = NetSimShim::new(profile, Direction::Up);
        let n = 10_000;
        let mut drops = 0;
        for _ in 0..n {
            if let Admission::Drop = shim.admit(100) {
                drops += 1;
            }
        }
        let expected = n / 2;
        let tol = n / 50; // ±2%
        eprintln!(
            "loss_rate: n={}, drops={}, hit_rate={:.4}",
            n,
            drops,
            drops as f64 / n as f64
        );
        assert!(
            (drops as i64 - expected as i64).unsigned_abs() < tol as u64,
            "drops={}, expected {}±{}",
            drops,
            expected,
            tol
        );
    }

    #[test]
    fn token_bucket_rate_limit() {
        // 1000 kbps = 125_000 bytes/sec; 1-second burst capacity.
        // 10 × 50KB = 500KB total. After the 125KB burst, the remaining 375KB
        // must be drained at 125KB/s → ~3 seconds of accumulated delay.
        let profile = NetworkProfile {
            uplink_kbps: Some(1000),
            seed: Some(5),
            ..Default::default()
        };
        let shim = NetSimShim::new(profile, Direction::Up);

        let mut total = Duration::ZERO;
        for _ in 0..10 {
            match shim.admit(50_000) {
                Admission::Delay(d) => total += d,
                Admission::Pass => {}
                other => panic!("unexpected admission {:?}", other),
            }
        }
        eprintln!("token_bucket_rate_limit: total_delay={:?}", total);
        // Allow 1ms of floating-point slack; the exact theoretical total is
        // 0.2 + 0.4 × 7 = 3.0s but accumulated secs_f64 math can underflow.
        assert!(
            total >= Duration::from_millis(2_999),
            "expected ~3s cumulative delay, got {:?}",
            total
        );
    }

    #[test]
    fn bucket_cap_prevents_starvation() {
        // An oversized packet gets clamped at 5 seconds so the pipeline
        // doesn't wedge behind it.
        let profile = NetworkProfile {
            uplink_kbps: Some(8),
            seed: Some(0),
            ..Default::default()
        };
        let shim = NetSimShim::new(profile, Direction::Up);
        // 1MB at 8kbps would naïvely be ~1000s; should clamp to 5s.
        match shim.admit(1_000_000) {
            Admission::Delay(d) => {
                assert!(
                    d <= Duration::from_millis(5_100),
                    "delay not clamped: {:?}",
                    d
                );
            }
            other => panic!("unexpected admission {:?}", other),
        }
    }

    #[test]
    fn validate_ranges() {
        let mut p = NetworkProfile::passthrough();
        p.latency_ms = 10_000;
        assert!(p.validate().is_err());

        p = NetworkProfile::passthrough();
        p.loss_pct = f32::NAN;
        assert!(p.validate().is_err());

        p = NetworkProfile::passthrough();
        p.loss_pct = 101.0;
        assert!(p.validate().is_err());

        p = NetworkProfile::passthrough();
        p.uplink_kbps = Some(0);
        assert!(p.validate().is_err());

        p = NetworkProfile {
            latency_ms: 50,
            jitter_ms: 10,
            loss_pct: 0.5,
            uplink_kbps: Some(1000),
            ..Default::default()
        };
        assert!(p.validate().is_ok());
    }

    #[test]
    fn direction_selects_rate_limit() {
        // Only uplink is set — a Down shim must not rate-limit.
        let profile = NetworkProfile {
            uplink_kbps: Some(8),
            seed: Some(1),
            ..Default::default()
        };
        let down = NetSimShim::new(profile, Direction::Down);
        let up = NetSimShim::new(
            NetworkProfile {
                uplink_kbps: Some(8),
                seed: Some(1),
                ..Default::default()
            },
            Direction::Up,
        );
        // The Down shim should Pass because neither latency nor a matching
        // rate limit is configured.
        assert_eq!(down.admit(10_000), Admission::Pass);
        // The Up shim at 8 kbps should impose some delay on 10 KB.
        assert!(matches!(up.admit(10_000), Admission::Delay(_)));
    }
}
