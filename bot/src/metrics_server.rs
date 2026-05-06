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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Prometheus metrics for the synthetic-bot fleet.
//!
//! This module is gated behind the `metrics` Cargo feature. When the feature
//! is disabled the module compiles to an empty stub so the rest of the crate
//! can reference a single `Option<Arc<BotMetrics>>` handle without any
//! `cfg` at the call site.
//!
//! When enabled, [`BotMetrics`] exposes counters/gauges/histograms that mirror
//! the naming conventions used by `actix-api`'s `videocall_*` / `relay_*`
//! metrics so existing Grafana dashboards can be extended to cover bots
//! without a new data model:
//!
//! - `bot_aq_video_tier_index`, `bot_aq_audio_tier_index`
//! - `bot_aq_target_bitrate_kbps`, `bot_aq_worst_peer_fps`
//! - `bot_aq_fps_ratio`, `bot_aq_bitrate_ratio`
//! - `bot_netsim_dropped_total`, `bot_netsim_delay_ms`
//! - `bot_packets_sent_total`, `bot_packets_received_total`
//! - `bot_packets_parsed_error_total`
//!
//! All series are labeled `bot=<user_id>` and `meeting=<meeting_id>`; some
//! additionally carry `direction` (up/down), `media_type`
//! (audio/video/health/diagnostics/other), or `reason` (loss/rate_limit).

// The bulk of this module compiles only when the `metrics` feature is on.
// A tiny stub below keeps a single `Option<Arc<BotMetrics>>` type available
// in the rest of the crate regardless of feature state, so call sites don't
// need `#[cfg(…)]` on every line.

#[cfg(not(feature = "metrics"))]
mod stub {
    //! No-op metrics facade used when the `metrics` feature is off.
    //!
    //! The public surface is intentionally empty: code that holds
    //! `Option<Arc<BotMetrics>>` will only ever see `None` because the
    //! constructor doesn't exist and no call site can build one without the
    //! feature enabled.

    /// Stand-in for the full [`BotMetrics`](super::BotMetrics) when metrics
    /// are disabled. The type is uninhabited to guarantee no instance can
    /// ever be constructed — an `Option<Arc<BotMetrics>>` therefore always
    /// compiles to `None`.
    #[allow(dead_code)]
    pub enum BotMetrics {}
}

#[cfg(not(feature = "metrics"))]
#[allow(unused_imports)]
pub use stub::BotMetrics;

#[cfg(feature = "metrics")]
use std::convert::Infallible;
#[cfg(feature = "metrics")]
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(feature = "metrics")]
use std::sync::Arc;

#[cfg(feature = "metrics")]
use hyper::service::{make_service_fn, service_fn};
#[cfg(feature = "metrics")]
use hyper::{Body, Request, Response, Server, StatusCode};
#[cfg(feature = "metrics")]
use prometheus::{
    register_gauge_vec_with_registry, register_histogram_vec_with_registry,
    register_int_counter_vec_with_registry, register_int_gauge_vec_with_registry, Encoder,
    GaugeVec, HistogramVec, IntCounterVec, IntGaugeVec, Registry, TextEncoder,
};
#[cfg(feature = "metrics")]
use tracing::{error, info, warn};

/// Bot-side Prometheus metrics handle.
///
/// One instance is constructed per bot process and cloned (via `Arc`) into the
/// AQ controller, netsim shim tasks, the inbound-stats recorder, and the
/// outbound packet-sender path. Each hot path either calls a method on
/// [`BotMetrics`] directly or bails early when the handle is `None`.
#[cfg(feature = "metrics")]
pub struct BotMetrics {
    /// Prometheus registry backing all of the metrics below. Kept so the HTTP
    /// server can encode it on scrape.
    registry: Arc<Registry>,

    // ---------------------------------------------------------------------
    // Adaptive-quality gauges.
    //
    // Labels: [bot, meeting]. These mirror `videocall_adaptive_video_tier`
    // and friends from actix-api so dashboards can `or` them together.
    // ---------------------------------------------------------------------
    pub aq_video_tier_index: IntGaugeVec,
    pub aq_audio_tier_index: IntGaugeVec,
    pub aq_target_bitrate_kbps: GaugeVec,
    pub aq_worst_peer_fps: GaugeVec,
    pub aq_fps_ratio: GaugeVec,
    pub aq_bitrate_ratio: GaugeVec,

    // ---------------------------------------------------------------------
    // Network-impairment shim counters + histogram.
    //
    // `direction` ∈ {up, down}; `reason` ∈ {loss, rate_limit}.
    // Histogram buckets are tuned for the range we actually simulate
    // (10ms LAN to a few seconds of heavy congestion).
    // ---------------------------------------------------------------------
    pub netsim_dropped_total: IntCounterVec,
    pub netsim_delay_ms: HistogramVec,

    // ---------------------------------------------------------------------
    // Packet counters.
    //
    // `media_type` uses the label set `{audio, video, health, diagnostics,
    // other}`. The label space is bounded so cardinality is predictable
    // even with many bots/meetings on one scrape target.
    // ---------------------------------------------------------------------
    pub packets_sent_total: IntCounterVec,
    pub packets_received_total: IntCounterVec,
    pub packets_parsed_error_total: IntCounterVec,
}

#[cfg(feature = "metrics")]
impl BotMetrics {
    /// Build + register every metric against the supplied registry.
    ///
    /// Call sites should keep the returned `Arc<BotMetrics>` around and pass
    /// it (wrapped in `Option`) into components that emit metrics.
    pub fn new(registry: Arc<Registry>) -> prometheus::Result<Arc<Self>> {
        let aq_video_tier_index = register_int_gauge_vec_with_registry!(
            "bot_aq_video_tier_index",
            "Current bot adaptive-quality video tier index (0 = best, higher = more degraded)",
            &["bot", "meeting"],
            registry
        )?;

        let aq_audio_tier_index = register_int_gauge_vec_with_registry!(
            "bot_aq_audio_tier_index",
            "Current bot adaptive-quality audio tier index (0 = high, higher = more degraded)",
            &["bot", "meeting"],
            registry
        )?;

        let aq_target_bitrate_kbps = register_gauge_vec_with_registry!(
            "bot_aq_target_bitrate_kbps",
            "Bot PID-adjusted target video bitrate in kbps",
            &["bot", "meeting"],
            registry
        )?;

        let aq_worst_peer_fps = register_gauge_vec_with_registry!(
            "bot_aq_worst_peer_fps",
            "Bot p75 received FPS across reporting peers (historically 'worst peer')",
            &["bot", "meeting"],
            registry
        )?;

        let aq_fps_ratio = register_gauge_vec_with_registry!(
            "bot_aq_fps_ratio",
            "Bot fps_ratio = received / target (driving AQ step-down)",
            &["bot", "meeting"],
            registry
        )?;

        let aq_bitrate_ratio = register_gauge_vec_with_registry!(
            "bot_aq_bitrate_ratio",
            "Bot bitrate_ratio = PID-clamped / tier-ideal (driving AQ step-down)",
            &["bot", "meeting"],
            registry
        )?;

        let netsim_dropped_total = register_int_counter_vec_with_registry!(
            "bot_netsim_dropped_total",
            "Total packets dropped by the bot network-impairment shim",
            &["bot", "direction", "reason"],
            registry
        )?;

        let netsim_delay_ms = register_histogram_vec_with_registry!(
            "bot_netsim_delay_ms",
            "Per-packet delay injected by the bot netsim shim, in milliseconds",
            &["bot", "direction"],
            // Buckets cover the range we realistically simulate: good LAN
            // (~10ms), fair WAN (~50-100ms), congested cellular (~500ms),
            // and pathological cases that hit the 5-second clamp.
            vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_000.0, 5_000.0],
            registry
        )?;

        let packets_sent_total = register_int_counter_vec_with_registry!(
            "bot_packets_sent_total",
            "Total packets sent by the bot's outbound pipeline",
            &["bot", "meeting", "media_type"],
            registry
        )?;

        let packets_received_total = register_int_counter_vec_with_registry!(
            "bot_packets_received_total",
            "Total packets received by the bot's inbound pipeline",
            &["bot", "meeting", "media_type"],
            registry
        )?;

        let packets_parsed_error_total = register_int_counter_vec_with_registry!(
            "bot_packets_parsed_error_total",
            "Total inbound packets that failed to parse (wrapper or inner payload)",
            &["bot", "meeting", "stage"],
            registry
        )?;

        Ok(Arc::new(Self {
            registry,
            aq_video_tier_index,
            aq_audio_tier_index,
            aq_target_bitrate_kbps,
            aq_worst_peer_fps,
            aq_fps_ratio,
            aq_bitrate_ratio,
            netsim_dropped_total,
            netsim_delay_ms,
            packets_sent_total,
            packets_received_total,
            packets_parsed_error_total,
        }))
    }

    /// Access the underlying registry (useful when mounting extra metrics
    /// from a caller-owned module, e.g. a future NATS probe).
    #[allow(dead_code)]
    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }
}

/// Default metrics bind address. Loopback-only so the `/metrics` endpoint,
/// which exposes meeting IDs / user IDs as label values, is not reachable
/// over the network unless the operator explicitly opts in via
/// `--metrics-bind`.
#[cfg(feature = "metrics")]
pub const DEFAULT_METRICS_BIND: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Spawn a minimal hyper server that exposes `/metrics` (Prometheus text
/// format) on the supplied address and port.
///
/// The server runs until the surrounding tokio runtime shuts down. We
/// deliberately do not expose a graceful-shutdown handle because the bot is
/// typically terminated with `SIGINT` and `SIGKILL` cleanup is sufficient
/// for a load-test tool — no state is persisted across the server.
///
/// `bind` defaults to [`DEFAULT_METRICS_BIND`] (127.0.0.1). Operators who
/// need fleet-wide scraping can pass `0.0.0.0` (or a specific NIC IP) via
/// the `--metrics-bind` CLI flag — this is an explicit opt-in because the
/// endpoint exposes meeting/user identifiers as Prometheus labels.
#[cfg(feature = "metrics")]
pub fn start_server(registry: Arc<Registry>, bind: IpAddr, port: u16) {
    let addr = SocketAddr::new(bind, port);
    let registry_for_service = Arc::clone(&registry);

    let make_svc = make_service_fn(move |_| {
        let registry = Arc::clone(&registry_for_service);
        async move {
            let registry = Arc::clone(&registry);
            Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                let registry = Arc::clone(&registry);
                async move {
                    let resp = match req.uri().path() {
                        "/metrics" => encode_metrics(&registry),
                        // Anything else returns 404 so load balancers don't
                        // conflate "alive" with "has data".
                        _ => Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Body::from("not found"))
                            .unwrap_or_else(|e| {
                                error!("failed to build 404 response: {e}");
                                Response::new(Body::empty())
                            }),
                    };
                    Ok::<_, Infallible>(resp)
                }
            }))
        }
    });

    tokio::spawn(async move {
        info!("Bot metrics server listening on {addr}");
        match Server::try_bind(&addr) {
            Ok(builder) => {
                if let Err(e) = builder.serve(make_svc).await {
                    error!("Bot metrics server error: {e}");
                }
            }
            Err(e) => {
                // We warn-and-continue rather than aborting the bot: metrics
                // are observability, not correctness, and a port clash
                // should not take the whole fleet down.
                warn!("Bot metrics server failed to bind {addr}: {e}");
            }
        }
    });
}

/// Encode the registry into a Prometheus text body. Kept small so it can be
/// re-used from tests.
#[cfg(feature = "metrics")]
fn encode_metrics(registry: &Registry) -> Response<Body> {
    let encoder = TextEncoder::new();
    let families = registry.gather();
    let mut buf = Vec::with_capacity(4096);
    match encoder.encode(&families, &mut buf) {
        Ok(()) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain; version=0.0.4")
            .body(Body::from(buf))
            .unwrap_or_else(|e| {
                error!("failed to build metrics response: {e}");
                Response::new(Body::empty())
            }),
        Err(e) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(format!("failed to encode metrics: {e}")))
            .unwrap_or_else(|e| {
                error!("failed to build metrics-error response: {e}");
                Response::new(Body::empty())
            }),
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use super::*;

    #[test]
    fn bot_metrics_register_without_panic() {
        let registry = Arc::new(Registry::new());
        let metrics = BotMetrics::new(registry.clone()).expect("metrics register");
        // Touch every metric with a single sample so the registry actually
        // has data when we gather.
        metrics
            .aq_video_tier_index
            .with_label_values(&["b", "m"])
            .set(4);
        metrics
            .aq_audio_tier_index
            .with_label_values(&["b", "m"])
            .set(0);
        metrics
            .aq_target_bitrate_kbps
            .with_label_values(&["b", "m"])
            .set(500.0);
        metrics
            .aq_worst_peer_fps
            .with_label_values(&["b", "m"])
            .set(25.0);
        metrics
            .aq_fps_ratio
            .with_label_values(&["b", "m"])
            .set(0.83);
        metrics
            .aq_bitrate_ratio
            .with_label_values(&["b", "m"])
            .set(0.75);
        metrics
            .netsim_dropped_total
            .with_label_values(&["b", "up", "loss"])
            .inc();
        metrics
            .netsim_delay_ms
            .with_label_values(&["b", "up"])
            .observe(42.0);
        metrics
            .packets_sent_total
            .with_label_values(&["b", "m", "audio"])
            .inc();
        metrics
            .packets_received_total
            .with_label_values(&["b", "m", "video"])
            .inc();
        metrics
            .packets_parsed_error_total
            .with_label_values(&["b", "m", "wrapper"])
            .inc();

        // Encoding must succeed and include at least one of our families.
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        encoder
            .encode(&registry.gather(), &mut buf)
            .expect("encode");
        let body = String::from_utf8(buf).expect("utf8");
        assert!(
            body.contains("bot_aq_video_tier_index"),
            "metrics body: {body}"
        );
        assert!(body.contains("bot_netsim_dropped_total"));
    }
}
