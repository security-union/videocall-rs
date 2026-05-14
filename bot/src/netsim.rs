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

//! Bot-side adapter around [`videocall_netsim::NetSimShim`].
//!
//! The actual loss / latency / jitter / bandwidth / duplicate / reorder
//! algorithm lives in the [`videocall_netsim`] crate so that the wasm
//! consumer (`videocall-client` running inside `dioxus-ui`) and the native
//! bot share a single, audited implementation.
//!
//! This module is a thin wrapper that:
//!
//! 1. Re-exports the public types ([`NetworkProfile`], [`Direction`],
//!    [`Admission`]) so existing call sites in `bot/src/main.rs` and
//!    `bot/src/config.rs` keep working with their current imports.
//! 2. Adds the bot's Prometheus metric hooks (gated by `feature = "metrics"`)
//!    via a [`BotNetSimShim`] wrapper that records the
//!    Drop / Delay / DelayAndDuplicate outcome of every admission.
//!
//! The metrics integration is bot-specific and was deliberately *not*
//! lifted into `videocall_netsim` because the wasm consumer has no
//! Prometheus story. Keeping the wrapper local preserves the bot's
//! existing `bot_netsim_dropped_total` / `bot_netsim_delay_ms` series
//! exactly as today.

#[cfg(feature = "metrics")]
use std::sync::Arc;

#[cfg(feature = "metrics")]
use crate::metrics_server::BotMetrics;

// Re-export the public surface so existing imports
// (`use bot::netsim::{Admission, Direction, NetSimShim, NetworkProfile}`)
// continue to resolve unchanged after the migration. `NetSimShim` is the
// bot's wrapper type — see [`BotNetSimShim`] below.
pub use videocall_netsim::{Admission, Direction, NetworkProfile};

/// Alias preserved for backward compatibility with call sites that named
/// the bot's per-direction shim `NetSimShim`. The wrapper type is
/// [`BotNetSimShim`].
pub type NetSimShim = BotNetSimShim;

/// Bot-side per-direction network-impairment shim.
///
/// Delegates the admission decision to a [`videocall_netsim::NetSimShim`]
/// (which owns the RNG, token bucket, and algorithm) and, when the
/// `metrics` feature is enabled, records the resulting decision on the
/// shared [`BotMetrics`] handles.
///
/// Wrap in `Arc` for cross-task sharing — `admit()` only needs `&self`.
pub struct BotNetSimShim {
    inner: videocall_netsim::NetSimShim,
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

impl BotNetSimShim {
    /// Build a shim for the given profile and direction. Equivalent to
    /// constructing a [`videocall_netsim::NetSimShim`] directly, plus an
    /// empty (no-op) metrics slot.
    pub fn new(profile: NetworkProfile, direction: Direction) -> Self {
        Self {
            inner: videocall_netsim::NetSimShim::new(profile, direction),
            #[cfg(feature = "metrics")]
            metrics: None,
        }
    }

    /// Install a metrics hook on this shim. Returns the same shim so the
    /// caller can chain the call when building an `Arc<BotNetSimShim>`.
    #[cfg(feature = "metrics")]
    pub fn with_metrics(mut self, metrics: Arc<BotMetrics>, bot: String) -> Self {
        let direction_label = match self.inner.direction() {
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
        self.inner.is_passthrough()
    }

    /// Which direction this shim was built for.
    #[allow(dead_code)] // part of the public shim API; used by callers and tests
    pub fn direction(&self) -> Direction {
        self.inner.direction()
    }

    /// Decide how a packet of `size_bytes` should be handled under the
    /// current profile and bucket state. Thread-safe.
    ///
    /// The decision is identical to `videocall_netsim::NetSimShim::admit`;
    /// this wrapper only adds post-hoc metrics emission for the Drop /
    /// Delay / DelayAndDuplicate cases.
    pub fn admit(&self, size_bytes: usize) -> Admission {
        let admission = self.inner.admit(size_bytes);
        #[cfg(feature = "metrics")]
        self.record_metrics(&admission);
        admission
    }

    /// Update the per-direction drop counter / delay histogram from an
    /// admission decision. The pre-migration shim only reached the drop
    /// path via the Bernoulli loss check, so the `reason` label stays
    /// `"loss"` to preserve series compatibility with deployed
    /// dashboards.
    #[cfg(feature = "metrics")]
    fn record_metrics(&self, admission: &Admission) {
        let Some(m) = self.metrics.as_ref() else {
            return;
        };
        match admission {
            Admission::Pass => {}
            Admission::Drop => {
                m.metrics
                    .netsim_dropped_total
                    .with_label_values(&[m.bot.as_str(), m.direction_label, "loss"])
                    .inc();
            }
            Admission::Delay(d) | Admission::DelayAndDuplicate(d) => {
                m.metrics
                    .netsim_delay_ms
                    .with_label_values(&[m.bot.as_str(), m.direction_label])
                    .observe(d.as_secs_f64() * 1000.0);
            }
        }
    }
}
