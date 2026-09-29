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

//! Heartbeat-backed liveness surface for the WebTransport relay (#2719).
//!
//! `/healthz` answers on an actix-server worker thread, while the QUIC accept
//! loop and the session actors run on relay runtimes it cannot observe. Each
//! relay runtime stamps its own slot via [`register_heartbeat_slot`], and
//! [`relay_health_responder`] fails the check once the OLDEST stamp stops
//! advancing, so a wedge on any single arbiter fails it. With nothing
//! registered the surface reads slot 0 alone.

use actix_web::HttpResponse;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Held until the first stamp lands; [`heartbeat_is_stale`] treats it as stale.
const UNSTAMPED_MS: u64 = 0;

/// Cadence at which the relay stamps the heartbeat.
pub const HEARTBEAT_PERIOD: Duration = Duration::from_millis(HEARTBEAT_PERIOD_MS);

/// [`HEARTBEAT_PERIOD`] in milliseconds.
pub const HEARTBEAT_PERIOD_MS: u64 = 500;

/// Consecutive missed heartbeats tolerated before `/healthz` fails.
pub const HEARTBEAT_STALE_PERIODS: u64 = 4;

/// Threshold installed before [`configure_stale_threshold`] runs.
pub const DEFAULT_STALE_THRESHOLD_MS: u64 = HEARTBEAT_PERIOD_MS * HEARTBEAT_STALE_PERIODS;

/// Environment variable overriding the derived staleness threshold, in
/// milliseconds. Read once, at probe-spawn time.
pub const STALE_THRESHOLD_ENV: &str = "RELAY_HEALTH_STALE_MS";

/// The slot the relay's main runtime stamps, and the only slot read when no
/// runtime has registered.
pub const MAIN_HEARTBEAT_SLOT: usize = 0;

/// Slot capacity: the main runtime plus every session arbiter (#2727 clamps the
/// arbiter count to [`crate::relay_shards::MAX_SESSION_ARBITERS`]).
pub const MAX_HEARTBEAT_SLOTS: usize = crate::relay_shards::MAX_SESSION_ARBITERS + 1;

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
static HEARTBEATS: [AtomicU64; MAX_HEARTBEAT_SLOTS] =
    [const { AtomicU64::new(UNSTAMPED_MS) }; MAX_HEARTBEAT_SLOTS];
static REGISTERED_SLOTS: AtomicUsize = AtomicUsize::new(0);
static STALE_THRESHOLD_MS: AtomicU64 = AtomicU64::new(DEFAULT_STALE_THRESHOLD_MS);

fn monotonic_now_ms() -> u64 {
    u64::try_from(EPOCH.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Claim a heartbeat slot for one relay runtime, called before its probe starts.
///
/// Slots are handed out in order from [`MAIN_HEARTBEAT_SLOT`]. Past capacity the
/// last slot is shared rather than the relay refusing to start: an over-shared
/// slot under-reports one runtime's wedge, which is strictly better than a
/// panic, and the #2727 clamp makes it unreachable in practice.
pub fn register_heartbeat_slot() -> usize {
    REGISTERED_SLOTS
        .fetch_add(1, Ordering::Relaxed)
        .min(MAX_HEARTBEAT_SLOTS - 1)
}

/// The slot [`register_heartbeat_slot`] would hand out next, WITHOUT claiming it.
///
/// Lets a caller name a slot in work it is about to hand to another runtime and
/// claim it only if that hand-off succeeded, so a rejected hand-off leaves no
/// registered, never-stamped slot. Safe because slots are claimed from one
/// thread.
pub fn peek_next_heartbeat_slot() -> usize {
    REGISTERED_SLOTS
        .load(Ordering::Relaxed)
        .min(MAX_HEARTBEAT_SLOTS - 1)
}

/// Slots [`oldest_heartbeat_ms`] reads: the registered count, else slot 0 alone.
fn observed_slots() -> usize {
    REGISTERED_SLOTS
        .load(Ordering::Relaxed)
        .clamp(1, MAX_HEARTBEAT_SLOTS)
}

/// Record that the runtime owning `slot` is still scheduling work. Clamped away
/// from [`UNSTAMPED_MS`] so a first-millisecond stamp is not read back as
/// "never".
pub fn stamp_relay_heartbeat_slot(slot: usize) {
    if let Some(cell) = HEARTBEATS.get(slot) {
        cell.store(monotonic_now_ms().max(UNSTAMPED_MS + 1), Ordering::Relaxed);
    }
}

/// Stamp [`MAIN_HEARTBEAT_SLOT`].
pub fn stamp_relay_heartbeat() {
    stamp_relay_heartbeat_slot(MAIN_HEARTBEAT_SLOT);
}

/// The most recent stamp on [`MAIN_HEARTBEAT_SLOT`], or [`UNSTAMPED_MS`].
pub fn last_heartbeat_ms() -> u64 {
    HEARTBEATS[MAIN_HEARTBEAT_SLOT].load(Ordering::Relaxed)
}

/// Whether the runtime owning `slot` has stamped within the staleness threshold.
///
/// Used by #2727's placement to steer new sessions away from a WEDGED arbiter:
/// `ArbiterHandle::spawn` reports failure only once the thread has EXITED, so
/// the heartbeat is what distinguishes a wedged thread from a live one. A
/// never-stamped slot reads as not fresh.
pub fn heartbeat_slot_is_fresh(slot: usize) -> bool {
    let Some(cell) = HEARTBEATS.get(slot) else {
        return false;
    };
    !heartbeat_is_stale(
        cell.load(Ordering::Relaxed),
        monotonic_now_ms(),
        stale_threshold_ms(),
    )
}

/// The OLDEST stamp across every registered runtime — what `/healthz` reads.
///
/// A minimum, not a maximum: one healthy runtime must not mask a wedged one. An
/// unstamped registered slot is [`UNSTAMPED_MS`] and therefore reads as stale,
/// which is correct — a runtime that has never ticked is not yet live.
pub fn oldest_heartbeat_ms() -> u64 {
    HEARTBEATS[..observed_slots()]
        .iter()
        .map(|cell| cell.load(Ordering::Relaxed))
        .min()
        .unwrap_or(UNSTAMPED_MS)
}

/// Test-only: release every registered slot and clear its stamp.
///
/// The slot table is process-global, so a test that registers slots must hand
/// them back or every later reader sees a permanently stale runtime.
#[cfg(test)]
pub fn reset_heartbeat_slots_for_test() {
    REGISTERED_SLOTS.store(0, Ordering::Relaxed);
    for cell in HEARTBEATS.iter() {
        cell.store(UNSTAMPED_MS, Ordering::Relaxed);
    }
}

/// The staleness threshold `/healthz` is currently comparing against.
pub fn stale_threshold_ms() -> u64 {
    STALE_THRESHOLD_MS.load(Ordering::Relaxed)
}

/// Parse a [`STALE_THRESHOLD_ENV`] value. Absent, unparsable and zero all yield
/// `None`, so a typo falls back to the derived threshold instead of wedging.
pub fn parse_stale_threshold_override(raw: Option<&str>) -> Option<u64> {
    raw?.trim().parse::<u64>().ok().filter(|ms| *ms > 0)
}

/// The threshold for `period`: the override when present, else
/// [`HEARTBEAT_STALE_PERIODS`] times the period.
pub fn derive_stale_threshold_ms(period: Duration, override_ms: Option<u64>) -> u64 {
    override_ms.unwrap_or_else(|| {
        u64::try_from(period.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_mul(HEARTBEAT_STALE_PERIODS)
    })
}

/// Install the staleness threshold for `period` and return it.
pub fn configure_stale_threshold(period: Duration) -> u64 {
    let raw = std::env::var(STALE_THRESHOLD_ENV).ok();
    let threshold =
        derive_stale_threshold_ms(period, parse_stale_threshold_override(raw.as_deref()));
    STALE_THRESHOLD_MS.store(threshold, Ordering::Relaxed);
    threshold
}

/// Whether the heartbeat has never landed, or its age exceeds `threshold_ms`. A
/// `stamp_ms` greater than `now_ms` reads as age 0, not as a wrapped maximum.
pub fn heartbeat_is_stale(stamp_ms: u64, now_ms: u64, threshold_ms: u64) -> bool {
    stamp_ms == UNSTAMPED_MS || now_ms.saturating_sub(stamp_ms) > threshold_ms
}

/// Build the `/healthz` response for one observation of the heartbeat.
pub fn health_response(stamp_ms: u64, now_ms: u64, threshold_ms: u64) -> HttpResponse {
    if !heartbeat_is_stale(stamp_ms, now_ms, threshold_ms) {
        return HttpResponse::Ok().body("Ok");
    }
    let age = if stamp_ms == UNSTAMPED_MS {
        "never".to_owned()
    } else {
        format!("{}ms", now_ms.saturating_sub(stamp_ms))
    };
    HttpResponse::ServiceUnavailable().body(format!(
        "relay heartbeat stale: age={age} limit={threshold_ms}ms"
    ))
}

/// Which way `/healthz` just moved, or `None` if it did not move (#2750).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthFlip {
    Degraded,
    Recovered,
}

static HEALTH_IS_STALE: AtomicBool = AtomicBool::new(false);

/// Claim the 200 <-> 503 EDGE, once, for whichever worker thread observes it.
/// `compare_exchange` is what bounds the volume to one line per episode.
pub fn note_health_state(stale: bool) -> Option<HealthFlip> {
    HEALTH_IS_STALE
        .compare_exchange(!stale, stale, Ordering::Relaxed, Ordering::Relaxed)
        .ok()
        .map(|_| {
            if stale {
                HealthFlip::Degraded
            } else {
                HealthFlip::Recovered
            }
        })
}

#[cfg(test)]
pub fn reset_health_state_for_test() {
    HEALTH_IS_STALE.store(false, Ordering::Relaxed);
}

/// `/healthz` handler for the WebTransport relay.
///
/// Reads [`oldest_heartbeat_ms`], so a wedge on ANY registered runtime — the
/// main one or any #2727 session arbiter — fails the check.
///
/// Logged from THIS worker thread, which keeps scheduling while the wedged
/// runtime does not (#2750).
pub async fn relay_health_responder() -> HttpResponse {
    let stamp_ms = oldest_heartbeat_ms();
    let now_ms = monotonic_now_ms();
    let threshold_ms = stale_threshold_ms();
    let response = health_response(stamp_ms, now_ms, threshold_ms);
    match note_health_state(heartbeat_is_stale(stamp_ms, now_ms, threshold_ms)) {
        Some(HealthFlip::Degraded) => warn!(
            "/healthz is now FAILING: oldest relay heartbeat age={}ms limit={}ms — \
             a relay runtime has stopped scheduling; readiness will pull this pod \
             out of the Service",
            now_ms.saturating_sub(stamp_ms),
            threshold_ms,
        ),
        Some(HealthFlip::Recovered) => {
            info!("/healthz recovered: every relay runtime is stamping again")
        }
        None => {}
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::spawn_scheduler_lag_probe_on_slot;
    use actix_web::http::StatusCode;
    use actix_web::{web, App, HttpServer};
    use serial_test::serial;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::mpsc;

    const PERIOD: Duration = Duration::from_millis(50);
    const WARMUP: Duration = Duration::from_millis(150);
    const BLOCK: Duration = Duration::from_millis(1_200);
    const READ_AFTER_BLOCK_STARTS: Duration = Duration::from_millis(500);
    const RECOVERY_BUDGET: Duration = Duration::from_secs(5);
    const HANDSHAKE_BUDGET: Duration = Duration::from_secs(10);

    #[test]
    fn heartbeat_is_stale_only_past_the_threshold() {
        assert!(
            !heartbeat_is_stale(1_000, 1_200, 200),
            "age exactly at the threshold must still be healthy"
        );
        assert!(
            heartbeat_is_stale(1_000, 1_201, 200),
            "one millisecond past the threshold must be stale"
        );
        assert!(!heartbeat_is_stale(1_000, 1_000, 200));
        assert!(heartbeat_is_stale(1_000, 9_999, 200));
    }

    #[test]
    fn an_unstamped_heartbeat_is_stale_even_at_age_zero() {
        assert!(heartbeat_is_stale(UNSTAMPED_MS, 0, 200));
        assert!(heartbeat_is_stale(UNSTAMPED_MS, u64::MAX, u64::MAX));
    }

    #[test]
    fn a_stamp_ahead_of_now_does_not_wrap_into_staleness() {
        assert!(!heartbeat_is_stale(5_000, 4_000, 0));
    }

    #[test]
    fn health_response_maps_staleness_to_503() {
        assert_eq!(health_response(1_000, 1_200, 200).status(), StatusCode::OK);
        assert_eq!(
            health_response(1_000, 1_201, 200).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            health_response(UNSTAMPED_MS, 10, 200).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    /// #2750 item 2. BITES: swap the `compare_exchange` for a store.
    #[test]
    #[serial(relay_scheduler_lag_probe)]
    fn a_health_flip_is_claimed_once_per_edge() {
        reset_health_state_for_test();

        assert_eq!(note_health_state(false), None, "healthy is the start state");
        assert_eq!(note_health_state(true), Some(HealthFlip::Degraded));
        assert_eq!(
            note_health_state(true),
            None,
            "a 2s-period readiness probe must not re-log for the whole episode",
        );
        assert_eq!(note_health_state(false), Some(HealthFlip::Recovered));
        assert_eq!(note_health_state(false), None);

        reset_health_state_for_test();
    }

    /// BITES: delete the `note_health_state` call in the responder.
    #[actix_rt::test]
    #[serial(relay_scheduler_lag_probe)]
    async fn the_responder_claims_the_degraded_edge() {
        reset_heartbeat_slots_for_test();
        reset_health_state_for_test();

        assert_eq!(
            relay_health_responder().await.status(),
            StatusCode::SERVICE_UNAVAILABLE,
        );
        assert_eq!(
            note_health_state(true),
            None,
            "the responder must have claimed the degraded edge itself",
        );

        reset_health_state_for_test();
        reset_heartbeat_slots_for_test();
    }

    #[test]
    fn stale_threshold_derives_from_the_probe_period() {
        assert_eq!(
            derive_stale_threshold_ms(HEARTBEAT_PERIOD, None),
            DEFAULT_STALE_THRESHOLD_MS
        );
        assert_eq!(
            derive_stale_threshold_ms(Duration::from_millis(50), None),
            50 * HEARTBEAT_STALE_PERIODS
        );
        assert_eq!(
            derive_stale_threshold_ms(Duration::from_millis(50), Some(7_000)),
            7_000
        );
    }

    #[test]
    fn a_bad_threshold_override_falls_back_to_the_derived_value() {
        assert_eq!(parse_stale_threshold_override(Some(" 1234 ")), Some(1_234));
        assert_eq!(parse_stale_threshold_override(None), None);
        assert_eq!(parse_stale_threshold_override(Some("")), None);
        assert_eq!(parse_stale_threshold_override(Some("soon")), None);
        assert_eq!(parse_stale_threshold_override(Some("-1")), None);
        assert_eq!(parse_stale_threshold_override(Some("0")), None);
    }

    struct StalledRelay {
        health_addr: Option<SocketAddr>,
        threshold_ms: u64,
        unblocked: mpsc::Receiver<()>,
        done: mpsc::Sender<()>,
        thread: std::thread::JoinHandle<()>,
    }

    impl StalledRelay {
        fn assert_recovers(self, status: impl Fn() -> StatusCode) {
            self.unblocked
                .recv_timeout(BLOCK + HANDSHAKE_BUDGET)
                .expect("relay thread must leave its blocking span");

            let deadline = Instant::now() + RECOVERY_BUDGET;
            while status() != StatusCode::OK {
                assert!(
                    Instant::now() < deadline,
                    "/healthz must return 200 once the relay runtime resumes stamping"
                );
                std::thread::sleep(PERIOD);
            }

            let _ = self.done.send(());
            self.thread.join().expect("relay thread must not panic");
        }
    }

    /// Stand a relay runtime up on its own thread and stall it with
    /// `std::thread::sleep` — the #1636 wedge. Returns once the block has STARTED,
    /// so the caller owns the whole stalled window.
    fn stall_a_relay_runtime(with_health_server: bool) -> StalledRelay {
        let (blocking_tx, blocking_rx) = mpsc::channel::<(Option<SocketAddr>, u64)>();
        let (unblocked_tx, unblocked_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let thread = std::thread::spawn(move || {
            actix_rt::System::new().block_on(async move {
                let probe = spawn_scheduler_lag_probe_on_slot(PERIOD, MAIN_HEARTBEAT_SLOT);

                let health_addr = with_health_server.then(|| {
                    let server = HttpServer::new(|| {
                        App::new().route("/healthz", web::get().to(relay_health_responder))
                    })
                    .workers(1)
                    .bind(("127.0.0.1", 0))
                    .expect("health server must bind an ephemeral port");
                    let addr = server.addrs()[0];
                    actix_rt::spawn(server.run());
                    addr
                });

                actix_rt::time::sleep(WARMUP).await;
                blocking_tx
                    .send((health_addr, stale_threshold_ms()))
                    .expect("test thread must still be listening");

                std::thread::sleep(BLOCK);

                unblocked_tx
                    .send(())
                    .expect("test thread must be listening");
                let deadline = Instant::now() + RECOVERY_BUDGET;
                while done_rx.try_recv().is_err() && Instant::now() < deadline {
                    actix_rt::time::sleep(PERIOD).await;
                }
                probe.abort();
            });
        });

        let (health_addr, threshold_ms) = blocking_rx
            .recv_timeout(HANDSHAKE_BUDGET)
            .expect("relay thread must reach its blocking span");
        assert_eq!(
            threshold_ms,
            PERIOD.as_millis() as u64 * HEARTBEAT_STALE_PERIODS,
            "the probe spawn must install the threshold derived from its period"
        );
        assert!(
            READ_AFTER_BLOCK_STARTS.as_millis() as u64 > threshold_ms,
            "the stale read must land after the threshold has elapsed"
        );
        assert!(
            READ_AFTER_BLOCK_STARTS < BLOCK,
            "the stale read must land while the relay thread is still blocked"
        );
        std::thread::sleep(READ_AFTER_BLOCK_STARTS);

        StalledRelay {
            health_addr,
            threshold_ms,
            unblocked: unblocked_rx,
            done: done_tx,
            thread,
        }
    }

    fn healthz_status() -> StatusCode {
        futures::executor::block_on(relay_health_responder()).status()
    }

    fn healthz_status_over_tcp(addr: SocketAddr) -> StatusCode {
        let mut stream = TcpStream::connect(addr).expect("health server must accept");
        stream
            .set_read_timeout(Some(HANDSHAKE_BUDGET))
            .expect("read timeout must be settable");
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("request must be writable");
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .expect("response must be readable");
        let response = String::from_utf8_lossy(&raw);
        let code = response
            .split_whitespace()
            .nth(1)
            .unwrap_or_else(|| panic!("no status line in {response:?}"));
        StatusCode::from_bytes(code.as_bytes()).expect("status line must carry a status code")
    }

    /// #2727: `/healthz` reads the OLDEST runtime's stamp, so one healthy runtime
    /// cannot mask a wedged one. FAILS on the pre-#2727 single global, which the
    /// main runtime's stamps keep fresh. Deterministic: no probe threads.
    #[test]
    #[serial(relay_scheduler_lag_probe)]
    fn healthz_reads_the_oldest_runtime_stamp_not_the_newest() {
        reset_heartbeat_slots_for_test();
        let threshold = configure_stale_threshold(PERIOD);
        assert_eq!(
            threshold,
            PERIOD.as_millis() as u64 * HEARTBEAT_STALE_PERIODS
        );

        let main = register_heartbeat_slot();
        let arbiter = register_heartbeat_slot();
        assert_eq!(
            (main, arbiter),
            (MAIN_HEARTBEAT_SLOT, MAIN_HEARTBEAT_SLOT + 1),
            "slots must be handed out in order from the main runtime's"
        );

        stamp_relay_heartbeat_slot(main);
        stamp_relay_heartbeat_slot(arbiter);
        assert_eq!(
            healthz_status(),
            StatusCode::OK,
            "both runtimes freshly stamped must be healthy"
        );

        // The arbiter wedges. Only the main runtime keeps stamping.
        std::thread::sleep(Duration::from_millis(threshold + PERIOD.as_millis() as u64));
        stamp_relay_heartbeat_slot(main);
        assert_eq!(
            healthz_status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a wedged arbiter must fail /healthz even while the main runtime stamps on cadence"
        );

        stamp_relay_heartbeat_slot(arbiter);
        assert_eq!(
            healthz_status(),
            StatusCode::OK,
            "/healthz must recover once the wedged runtime resumes stamping"
        );
        reset_heartbeat_slots_for_test();
    }

    /// A relay is not healthy until every registered arbiter has ticked once.
    #[test]
    #[serial(relay_scheduler_lag_probe)]
    fn a_registered_but_silent_runtime_is_not_healthy() {
        reset_heartbeat_slots_for_test();
        configure_stale_threshold(PERIOD);
        let main = register_heartbeat_slot();
        let _silent_arbiter = register_heartbeat_slot();
        stamp_relay_heartbeat_slot(main);
        assert_eq!(healthz_status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(oldest_heartbeat_ms(), UNSTAMPED_MS);
        reset_heartbeat_slots_for_test();
    }

    /// With nothing registered the surface is the single-runtime one: slot 0.
    #[test]
    #[serial(relay_scheduler_lag_probe)]
    fn an_unregistered_relay_reads_the_main_slot_alone() {
        reset_heartbeat_slots_for_test();
        configure_stale_threshold(PERIOD);
        stamp_relay_heartbeat();
        assert_eq!(oldest_heartbeat_ms(), last_heartbeat_ms());
        assert_eq!(healthz_status(), StatusCode::OK);
        reset_heartbeat_slots_for_test();
    }

    /// Only the heartbeat's thread is stalled; the reader is on another. On the
    /// pre-#2719 constant-200 handler the 503 assertion fails.
    #[test]
    #[serial(relay_scheduler_lag_probe)]
    fn healthz_fails_while_the_relay_runtime_is_blocked_and_recovers_after() {
        let relay = stall_a_relay_runtime(false);
        assert_eq!(
            healthz_status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "/healthz must fail while the relay runtime is blocked past {}ms",
            relay.threshold_ms
        );
        relay.assert_recovers(healthz_status);
    }

    /// The same wedge over TCP against a real bound `.workers(1)` health server,
    /// covering the route wiring and actix-server's off-runtime worker.
    #[test]
    #[serial(relay_scheduler_lag_probe)]
    fn a_bound_health_server_reports_503_over_tcp_while_the_relay_runtime_is_blocked() {
        let relay = stall_a_relay_runtime(true);
        let addr = relay
            .health_addr
            .expect("scenario asked for a health server");
        assert_eq!(
            healthz_status_over_tcp(addr),
            StatusCode::SERVICE_UNAVAILABLE,
            "a blocked relay runtime must not serve 200 on /healthz"
        );
        relay.assert_recovers(move || healthz_status_over_tcp(addr));
    }
}
