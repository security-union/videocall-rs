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

//! Session sharding for the WebTransport relay (#2727).
//!
//! Each accepted WebTransport connection is placed on one of N actix arbiters,
//! and every object that connection owns stays on that arbiter for the
//! session's whole life. `ChatServer` stays a single-owner actor elsewhere and
//! reaches sessions by `Recipient::try_send`.
//!
//! One shard is the rollback posture: no arbiter thread is created and the
//! session future is spawned on the caller's `LocalSet`.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use actix_rt::{Arbiter, ArbiterHandle};
use tracing::warn;

/// Environment variable overriding the session arbiter count.
pub const SESSION_ARBITERS_ENV: &str = "WT_SESSION_ARBITERS";

/// The tokio variable a multi-threaded runtime consults when the caller leaves
/// the pool size unset; [`resolve_worker_thread_count`] neutralises it.
pub const TOKIO_WORKER_THREADS_ENV: &str = "TOKIO_WORKER_THREADS";

/// Lower clamp: one shard is the un-sharded posture, never zero shards.
pub const MIN_SESSION_ARBITERS: usize = 1;

/// Upper clamp. Each shard is an OS thread with its own runtime; past this the
/// threads cost more than the parallelism buys on any node we deploy to.
pub const MAX_SESSION_ARBITERS: usize = 64;

/// Resolve the session arbiter count from a raw environment value.
///
/// `parallelism` is the caller's [`std::thread::available_parallelism`] reading.
/// Absent, empty, unparsable and zero values all fall back to it, so a typo
/// degrades to the derived default instead of wedging the relay at one shard or
/// panicking. The result is clamped to
/// [`MIN_SESSION_ARBITERS`]..=[`MAX_SESSION_ARBITERS`].
///
/// Pure, so the resolution table is testable without racing the `OnceLock` in
/// [`session_arbiter_count`] or mutating the process environment.
pub fn resolve_session_arbiter_count(raw: Option<&str>, parallelism: usize) -> usize {
    let requested = match raw.map(str::trim) {
        None | Some("") => parallelism,
        Some(value) => match value.parse::<usize>() {
            Ok(0) => {
                warn!("{SESSION_ARBITERS_ENV}=0 is invalid; using {parallelism}");
                parallelism
            }
            Ok(n) => n,
            Err(e) => {
                warn!("{SESSION_ARBITERS_ENV}={value:?} is not a count ({e}); using {parallelism}");
                parallelism
            }
        },
    };
    requested.clamp(MIN_SESSION_ARBITERS, MAX_SESSION_ARBITERS)
}

/// The default arbiter count for this process.
///
/// [`std::thread::available_parallelism`] FLOORS a cgroup quota to whole cores,
/// so the relay's CPU limits must be set in whole cores; see
/// `docs/DEPLOYMENT_CONFIG_MAP.md`.
pub fn default_session_arbiter_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(MIN_SESSION_ARBITERS)
}

/// Memoised [`SESSION_ARBITERS_ENV`] reading. The env is read once, so changing
/// it needs a relay restart.
pub fn session_arbiter_count() -> usize {
    use std::sync::OnceLock;
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        resolve_session_arbiter_count(
            std::env::var(SESSION_ARBITERS_ENV).ok().as_deref(),
            default_session_arbiter_count(),
        )
    })
}

/// Whether `count` shards need a multi-threaded tokio runtime.
///
/// One shard keeps the current-thread runtime `#[actix_rt::main]` builds, so a
/// single-shard relay has the pre-#2727 runtime flavour and placement.
pub fn needs_multi_thread_runtime(count: usize) -> bool {
    count > MIN_SESSION_ARBITERS
}

/// Worker threads for the relay's tokio pool, given its arbiter count.
///
/// The caller MUST pass this to `Builder::worker_threads`. Leaving it unset is
/// not equivalent: tokio would then call `num_cpus`, which reads
/// `TOKIO_WORKER_THREADS` and PANICS on a zero, unparsable or non-unicode value.
/// Passing a count short-circuits that call, which is the only reason the deploy
/// configs can say the variable is ignored.
pub fn resolve_worker_thread_count(arbiters: usize) -> usize {
    arbiters.max(MIN_SESSION_ARBITERS)
}

/// Live-session counter, shared by a shard and every [`ShardLease`] on it.
type ShardLoad = Arc<AtomicUsize>;

/// Decrements its shard's live-session count when dropped. Moved INTO the
/// session future, so it is released when that future ends.
#[derive(Debug)]
pub struct ShardLease {
    shard: usize,
    load: ShardLoad,
}

impl ShardLease {
    /// The shard this session was placed on.
    pub fn shard(&self) -> usize {
        self.shard
    }
}

impl Drop for ShardLease {
    fn drop(&mut self) {
        self.load.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct Shard {
    /// `None` on the single-shard posture: no arbiter thread exists.
    arbiter: Option<Arbiter>,
    handle: Option<ArbiterHandle>,
    load: ShardLoad,
    /// Heartbeat slot of the probe running ON this arbiter, once one has been
    /// started. `pick_shard` reads its freshness to avoid placing a session on
    /// a wedged arbiter.
    heartbeat_slot: Option<usize>,
}

/// The relay's session arbiters (#2727).
///
/// Built once in `main` BEFORE the QUIC listener accepts, so no connection can
/// race an unregistered shard. [`Arbiter::new`] does not return until its
/// thread has registered with the actix `System`.
#[derive(Debug)]
pub struct SessionShards {
    shards: Vec<Shard>,
}

impl SessionShards {
    /// Create `count` shards, clamped to at least [`MIN_SESSION_ARBITERS`].
    ///
    /// # Panics
    /// [`Arbiter::new`] panics if no actix `System` is registered on the
    /// calling thread, so call this from inside `System::block_on`.
    pub fn new(count: usize) -> Self {
        let count = count.max(MIN_SESSION_ARBITERS);
        let shards = (0..count)
            .map(|_| {
                let (arbiter, handle) = if needs_multi_thread_runtime(count) {
                    let arbiter = Arbiter::new();
                    let handle = arbiter.handle();
                    (Some(arbiter), Some(handle))
                } else {
                    (None, None)
                };
                Shard {
                    arbiter,
                    handle,
                    load: Arc::new(AtomicUsize::new(0)),
                    heartbeat_slot: None,
                }
            })
            .collect();
        Self { shards }
    }

    /// Number of shards.
    pub fn len(&self) -> usize {
        self.shards.len()
    }

    /// Always false — [`SessionShards::new`] clamps to at least one shard.
    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    /// Live SESSIONS per shard, indexed by shard — a count, not a measure of
    /// work. Test-only: `pick_shard` reads each shard's counter directly.
    #[cfg(test)]
    pub fn live_counts(&self) -> Vec<usize> {
        self.shards
            .iter()
            .map(|s| s.load.load(Ordering::Relaxed))
            .collect()
    }

    /// The least-loaded LIVE shard, lowest index breaking ties.
    ///
    /// Least-loaded rather than round-robin or an id hash: sessions are
    /// long-lived and churn unevenly, and nothing rebalances a placed session.
    ///
    /// Liveness is checked because load alone points STRAIGHT AT a broken shard:
    /// a wedged arbiter finishes nothing, so its count falls to zero and it
    /// becomes the permanent argmin. With no fresh shard this is least-loaded.
    pub fn pick_shard(&self) -> usize {
        let fresh = |shard: &Shard| {
            shard
                .heartbeat_slot
                .is_some_and(crate::relay_health::heartbeat_slot_is_fresh)
        };
        self.shards
            .iter()
            .enumerate()
            .filter(|(_, shard)| fresh(shard))
            .min_by_key(|(idx, shard)| (shard.load.load(Ordering::Relaxed), *idx))
            .or_else(|| {
                self.shards
                    .iter()
                    .enumerate()
                    .min_by_key(|(idx, shard)| (shard.load.load(Ordering::Relaxed), *idx))
            })
            .map(|(idx, _)| idx)
            .unwrap_or(0)
    }

    /// Place one session on the least-loaded shard and run `make`'s future
    /// there for the session's whole life.
    ///
    /// The [`ShardLease`] is passed to `make` so it can be moved into the
    /// returned future and dropped with it. Returns false only if the chosen
    /// arbiter thread has died; the rejected send drops the future, and with it
    /// the lease, so the shard's count still settles.
    pub fn spawn_session<F, Fut>(&self, make: F) -> bool
    where
        F: FnOnce(ShardLease) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        // Not atomic as a pair, and need not be: the QUIC accept loop is the only
        // caller and it is one sequential task.
        let shard = self.pick_shard();
        let load = self.shards[shard].load.clone();
        load.fetch_add(1, Ordering::Relaxed);
        let lease = ShardLease { shard, load };

        match &self.shards[shard].handle {
            Some(handle) => handle.spawn(async move { make(lease).await }),
            None => {
                actix_rt::spawn(async move { make(lease).await });
                true
            }
        }
    }

    /// Start one #2713 scheduler-lag probe on each arbiter and return how many
    /// were started.
    ///
    /// Each arbiter claims its own heartbeat slot, so a wedge on any one of them
    /// fails `/healthz` (#2719) instead of being masked by the healthy ones.
    ///
    /// The slot is claimed only once the arbiter has ACCEPTED the probe: claiming
    /// first would leave a registered, never-stamped slot on a rejected spawn.
    ///
    /// Returns 0 on the single-shard posture, whose runtime already stamps
    /// [`crate::relay_health::MAIN_HEARTBEAT_SLOT`].
    pub fn spawn_scheduler_lag_probes(&mut self, period: std::time::Duration) -> usize {
        // Synchronously, not only from inside each probe: the spawn is a channel
        // send, so a `/healthz` read could race startup.
        crate::relay_health::configure_stale_threshold(period);
        let mut started = 0;
        for shard in &mut self.shards {
            let Some(handle) = &shard.handle else {
                continue;
            };
            let slot = crate::relay_health::peek_next_heartbeat_slot();
            if handle.spawn(async move {
                crate::metrics::spawn_scheduler_lag_probe_on_slot(period, slot);
            }) {
                // The arbiter took the probe, so claim the slot it was handed.
                crate::relay_health::register_heartbeat_slot();
                shard.heartbeat_slot = Some(slot);
                started += 1;
            } else {
                warn!("session arbiter rejected its scheduler-lag probe; it will take no sessions");
            }
        }
        started
    }

    /// Stop every arbiter thread and JOIN it.
    ///
    /// No production caller, by design: dropping the shards already stops the
    /// arbiters un-joined. This JOINS, which tests need.
    pub fn stop(self) {
        for shard in self.shards {
            if let Some(arbiter) = shard.arbiter {
                arbiter.stop();
                let _ = arbiter.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::collections::{HashMap, HashSet};
    use std::thread::ThreadId;
    use std::time::Duration;
    use tokio::sync::{mpsc as async_mpsc, oneshot};

    const BUDGET: Duration = Duration::from_secs(10);

    #[test]
    fn arbiter_count_resolves_from_env_or_parallelism() {
        assert_eq!(resolve_session_arbiter_count(None, 4), 4);
        assert_eq!(resolve_session_arbiter_count(Some(""), 4), 4);
        assert_eq!(resolve_session_arbiter_count(Some("   "), 4), 4);
        assert_eq!(resolve_session_arbiter_count(Some("0"), 4), 4);
        assert_eq!(resolve_session_arbiter_count(Some("nope"), 4), 4);
        assert_eq!(resolve_session_arbiter_count(Some("-1"), 4), 4);
        assert_eq!(resolve_session_arbiter_count(Some(" 3 "), 4), 3);
        assert_eq!(resolve_session_arbiter_count(Some("1"), 8), 1);
    }

    /// Brackets both clamp bounds, so moving either is red here.
    #[test]
    fn arbiter_count_clamps_at_both_bounds() {
        assert_eq!(
            resolve_session_arbiter_count(Some("999"), 4),
            MAX_SESSION_ARBITERS
        );
        assert_eq!(
            resolve_session_arbiter_count(
                Some(&MAX_SESSION_ARBITERS.to_string()),
                MIN_SESSION_ARBITERS
            ),
            MAX_SESSION_ARBITERS,
            "the upper bound itself must pass through, not clamp down"
        );
        assert_eq!(
            resolve_session_arbiter_count(None, MAX_SESSION_ARBITERS + 1),
            MAX_SESSION_ARBITERS,
            "the derived default is clamped too, not only the env override"
        );
        assert_eq!(
            resolve_session_arbiter_count(Some("1"), 0),
            MIN_SESSION_ARBITERS
        );
    }

    #[test]
    fn the_worker_pool_is_sized_from_the_arbiter_count() {
        assert_eq!(resolve_worker_thread_count(2), 2);
        assert_eq!(resolve_worker_thread_count(8), 8);
        assert_eq!(
            resolve_worker_thread_count(0),
            MIN_SESSION_ARBITERS,
            "tokio rejects a zero worker count, so the floor must hold"
        );
    }

    /// Passing a worker count makes `TOKIO_WORKER_THREADS` ignored. Both halves
    /// are EXECUTED, not asserted.
    ///
    /// Mutates the process environment AND installs a panic hook. Safe only
    /// because CI runs this binary with `--test-threads=1` and no other test here
    /// builds a multi-threaded runtime without an explicit worker count; both
    /// globals are restored before the assertions run.
    #[test]
    #[serial(tokio_worker_threads_env)]
    fn an_explicit_worker_count_makes_the_tokio_env_ignored() {
        let previous = std::env::var(TOKIO_WORKER_THREADS_ENV).ok();
        std::env::set_var(TOKIO_WORKER_THREADS_ENV, "0");
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        // Negative control: with the count left to tokio, the env is read and a
        // zero is fatal.
        let inherited = std::panic::catch_unwind(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map(|_| ())
        });

        // The relay's construction: the count short-circuits `unwrap_or_else`, so
        // `num_cpus` never runs and the same hostile value is ignored.
        let explicit = std::panic::catch_unwind(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(resolve_worker_thread_count(2))
                .enable_all()
                .build()
                .map(|_| ())
        });

        std::panic::set_hook(hook);
        match previous {
            Some(value) => std::env::set_var(TOKIO_WORKER_THREADS_ENV, value),
            None => std::env::remove_var(TOKIO_WORKER_THREADS_ENV),
        }

        assert!(
            inherited.is_err(),
            "tokio must still read {TOKIO_WORKER_THREADS_ENV} when the count is left unset; if \
             this stops panicking the deploy configs' claim needs re-checking against tokio"
        );
        assert!(
            explicit.is_ok(),
            "passing an explicit worker count must make {TOKIO_WORKER_THREADS_ENV} ignored"
        );
    }

    #[test]
    fn a_multi_thread_runtime_is_needed_only_past_one_shard() {
        assert!(!needs_multi_thread_runtime(1));
        assert!(needs_multi_thread_runtime(2));
        assert!(needs_multi_thread_runtime(MAX_SESSION_ARBITERS));
    }

    /// Run an async body inside an actix `System` on its own thread: `Arbiter::new`
    /// needs one, and a single-shard `actix_rt::spawn` needs the `LocalSet` driven.
    fn in_a_system<T, F, Fut>(body: F) -> T
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T>,
    {
        std::thread::spawn(move || actix_rt::System::new().block_on(body()))
            .join()
            .expect("system thread must not panic")
    }

    /// Place sessions through the production `spawn_session` and report each one's
    /// shard and thread. Leases are held by `await`, never a blocking recv, until
    /// all have reported, so the spread is deterministic.
    fn place_sessions(shard_count: usize, sessions: usize) -> (ThreadId, Vec<(usize, ThreadId)>) {
        in_a_system(move || async move {
            let shards = SessionShards::new(shard_count);
            let (report_tx, mut report_rx) = async_mpsc::unbounded_channel::<(usize, ThreadId)>();
            let mut releases = Vec::with_capacity(sessions);

            for _ in 0..sessions {
                let report_tx = report_tx.clone();
                let (release_tx, release_rx) = oneshot::channel::<()>();
                releases.push(release_tx);
                assert!(
                    shards.spawn_session(move |lease| async move {
                        report_tx
                            .send((lease.shard(), std::thread::current().id()))
                            .expect("collector must still be listening");
                        let _ = release_rx.await;
                        drop(lease);
                    }),
                    "every session must be accepted by a live shard"
                );
            }
            drop(report_tx);

            let mut placed = Vec::with_capacity(sessions);
            for _ in 0..sessions {
                let report = actix_rt::time::timeout(BUDGET, report_rx.recv())
                    .await
                    .expect("every placed session must report within the budget")
                    .expect("the report channel must stay open until all sessions report");
                placed.push(report);
            }
            for release in releases {
                let _ = release.send(());
            }
            shards.stop();
            (std::thread::current().id(), placed)
        })
    }

    /// #2727 invariant 1, through the production `spawn_session` the accept loop
    /// calls: a session runs on the arbiter that owns it, and shards are threads.
    #[test]
    fn each_shard_runs_its_sessions_on_its_own_thread() {
        let (system_thread, placed) = place_sessions(3, 6);

        let mut per_shard: HashMap<usize, Vec<ThreadId>> = HashMap::new();
        for (shard, thread) in &placed {
            per_shard.entry(*shard).or_default().push(*thread);
        }
        assert_eq!(per_shard.len(), 3, "all three shards must receive sessions");

        let mut shard_threads = HashSet::new();
        for (shard, threads) in &per_shard {
            let first = threads[0];
            assert!(
                threads.iter().all(|t| *t == first),
                "shard {shard} ran its sessions on more than one thread: {threads:?}"
            );
            assert_ne!(
                first, system_thread,
                "shard {shard} ran on the system thread, so it was not placed on an arbiter"
            );
            shard_threads.insert(first);
        }
        assert_eq!(
            shard_threads.len(),
            3,
            "three shards must be three distinct threads"
        );
    }

    /// Least-loaded placement deals a burst evenly instead of stacking it.
    #[test]
    fn placement_spreads_sessions_across_shards() {
        let (_, placed) = place_sessions(4, 8);
        let mut counts = [0usize; 4];
        for (shard, _) in &placed {
            counts[*shard] += 1;
        }
        assert_eq!(
            counts,
            [2, 2, 2, 2],
            "least-loaded placement must deal a burst evenly across shards"
        );
    }

    /// #2727 invariant 3: one shard keeps today's placement, on the caller's thread.
    #[test]
    fn a_single_shard_runs_sessions_on_the_caller_thread() {
        let (system_thread, session_thread) = in_a_system(|| async {
            let shards = SessionShards::new(1);
            assert_eq!(shards.len(), 1);
            let (tx, rx) = oneshot::channel::<ThreadId>();
            assert!(shards.spawn_session(move |lease| async move {
                let _ = tx.send(std::thread::current().id());
                drop(lease);
            }));
            let session_thread = actix_rt::time::timeout(BUDGET, rx)
                .await
                .expect("the single-shard session must run within the budget")
                .expect("the single-shard session must report its thread");
            shards.stop();
            (std::thread::current().id(), session_thread)
        });
        assert_eq!(
            system_thread, session_thread,
            "a single shard must not move sessions off the relay's own runtime thread"
        );
    }

    /// #2727 invariant 5: teardown on any shard releases its slot.
    #[test]
    fn a_finished_session_releases_its_shard_slot() {
        in_a_system(|| async {
            let shards = SessionShards::new(2);
            let (done_tx, done_rx) = oneshot::channel::<()>();
            assert!(shards.spawn_session(move |lease| async move {
                drop(lease);
                let _ = done_tx.send(());
            }));
            actix_rt::time::timeout(BUDGET, done_rx)
                .await
                .expect("the session must run within the budget")
                .expect("the session must signal completion");

            let deadline = std::time::Instant::now() + BUDGET;
            while shards.live_counts().iter().sum::<usize>() != 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "a finished session must release its shard slot; counts were {:?}",
                    shards.live_counts()
                );
                actix_rt::time::sleep(Duration::from_millis(10)).await;
            }
            shards.stop();
        });
    }

    /// #2727 invariant 2 end to end: a session wedging one arbiter fails `/healthz`
    /// and does not stop the other arbiter's probe. The wedge goes on the SECOND
    /// shard so a main-slot-only read would answer 200 rather than pass by luck.
    #[test]
    #[serial(relay_scheduler_lag_probe)]
    fn a_wedged_arbiter_fails_healthz_while_the_other_keeps_ticking() {
        use crate::metrics::RELAY_SCHEDULER_LAG_MS;
        use crate::relay_health::{reset_heartbeat_slots_for_test, stale_threshold_ms};

        const PROBE_PERIOD: Duration = Duration::from_millis(50);
        const WARMUP: Duration = Duration::from_millis(200);
        const BLOCK: Duration = Duration::from_millis(1_500);
        const READ_AFTER_BLOCK_STARTS: Duration = Duration::from_millis(600);
        const RECOVERY_BUDGET: Duration = Duration::from_secs(5);

        fn healthz() -> actix_web::http::StatusCode {
            futures::executor::block_on(crate::relay_health::relay_health_responder()).status()
        }

        reset_heartbeat_slots_for_test();
        in_a_system(move || async move {
            let mut shards = SessionShards::new(2);
            assert_eq!(
                shards.spawn_scheduler_lag_probes(PROBE_PERIOD),
                2,
                "both arbiters must take a probe"
            );
            let threshold = stale_threshold_ms();
            assert!(
                READ_AFTER_BLOCK_STARTS.as_millis() as u64 > threshold,
                "the stale read must land after the threshold has elapsed"
            );
            assert!(
                READ_AFTER_BLOCK_STARTS < BLOCK,
                "the stale read must land while the arbiter is still wedged"
            );
            actix_rt::time::sleep(WARMUP).await;
            assert_eq!(
                healthz(),
                actix_web::http::StatusCode::OK,
                "both arbiters stamping on cadence must be healthy"
            );

            // Hold shard 0 so least-loaded placement puts the wedge on shard 1.
            let (held_tx, held_rx) = oneshot::channel::<usize>();
            let (release_tx, release_rx) = oneshot::channel::<()>();
            assert!(shards.spawn_session(move |lease| async move {
                let _ = held_tx.send(lease.shard());
                let _ = release_rx.await;
                drop(lease);
            }));
            assert_eq!(
                held_rx.await.expect("the holder must report its shard"),
                0,
                "the first session must land on shard 0"
            );

            let (wedged_tx, wedged_rx) = oneshot::channel::<usize>();
            assert!(shards.spawn_session(move |lease| async move {
                let _ = wedged_tx.send(lease.shard());
                std::thread::sleep(BLOCK);
                drop(lease);
            }));
            assert_eq!(
                wedged_rx.await.expect("the wedge must report its shard"),
                1,
                "the wedge must land on the shard the main slot does NOT cover"
            );

            let samples_at_wedge = RELAY_SCHEDULER_LAG_MS.get_sample_count();
            actix_rt::time::sleep(READ_AFTER_BLOCK_STARTS).await;
            assert_eq!(
                healthz(),
                actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
                "a session wedging one arbiter must fail /healthz past {threshold}ms"
            );
            assert!(
                RELAY_SCHEDULER_LAG_MS.get_sample_count() > samples_at_wedge,
                "the other arbiter's probe must keep ticking while one arbiter is wedged"
            );

            let deadline = std::time::Instant::now() + BLOCK + RECOVERY_BUDGET;
            while healthz() != actix_web::http::StatusCode::OK {
                assert!(
                    std::time::Instant::now() < deadline,
                    "/healthz must recover once the wedged arbiter resumes"
                );
                actix_rt::time::sleep(PROBE_PERIOD).await;
            }
            let _ = release_tx.send(());
            shards.stop();
        });
        reset_heartbeat_slots_for_test();
    }

    /// #2727 B5: a REJECTED probe must not leave a registered, never-stamped slot
    /// behind, which would pin `/healthz` at 503 for the life of the process.
    /// BITES: claim the slot before `handle.spawn`.
    #[test]
    #[serial(relay_scheduler_lag_probe)]
    fn a_rejected_probe_leaves_no_unstamped_slot_behind() {
        use crate::relay_health::reset_heartbeat_slots_for_test;

        const PROBE_PERIOD: Duration = Duration::from_millis(50);
        const WARMUP: Duration = Duration::from_millis(250);

        fn healthz() -> actix_web::http::StatusCode {
            futures::executor::block_on(crate::relay_health::relay_health_responder()).status()
        }

        reset_heartbeat_slots_for_test();
        in_a_system(move || async move {
            let mut shards = SessionShards::new(2);

            // Kill shard 1's arbiter and JOIN it, so its command channel is
            // certainly closed and `handle.spawn` cannot succeed.
            let dead = shards.shards[1]
                .arbiter
                .take()
                .expect("a two-shard set owns an arbiter per shard");
            dead.stop();
            dead.join().expect("the arbiter thread must not panic");

            assert_eq!(
                shards.spawn_scheduler_lag_probes(PROBE_PERIOD),
                1,
                "only the live arbiter can take a probe"
            );
            assert_eq!(
                shards.shards[1].heartbeat_slot, None,
                "a shard that rejected its probe must hold no slot"
            );

            actix_rt::time::sleep(WARMUP).await;
            assert_eq!(
                healthz(),
                actix_web::http::StatusCode::OK,
                "the dead arbiter must not have registered a slot it will never stamp"
            );

            shards.stop();
        });
        reset_heartbeat_slots_for_test();
    }

    /// #2727 S2: a WEDGED arbiter stops receiving sessions. Built so load cannot
    /// be what moves the answer: the wedged shard is the lower index and holds
    /// the same one session.
    #[test]
    #[serial(relay_scheduler_lag_probe)]
    fn a_wedged_shard_stops_receiving_sessions() {
        use crate::relay_health::{reset_heartbeat_slots_for_test, stale_threshold_ms};

        const PROBE_PERIOD: Duration = Duration::from_millis(50);
        const WARMUP: Duration = Duration::from_millis(200);
        const BLOCK: Duration = Duration::from_millis(2_000);

        reset_heartbeat_slots_for_test();
        in_a_system(move || async move {
            let mut shards = SessionShards::new(2);
            assert_eq!(shards.spawn_scheduler_lag_probes(PROBE_PERIOD), 2);
            actix_rt::time::sleep(WARMUP).await;
            assert_eq!(
                shards.pick_shard(),
                0,
                "with both shards fresh and idle, the lowest index wins"
            );

            let (wedged_tx, wedged_rx) = oneshot::channel::<usize>();
            assert!(shards.spawn_session(move |lease| async move {
                let _ = wedged_tx.send(lease.shard());
                std::thread::sleep(BLOCK);
                drop(lease);
            }));
            assert_eq!(wedged_rx.await.expect("the wedge must report"), 0);

            let (held_tx, held_rx) = oneshot::channel::<usize>();
            let (release_tx, release_rx) = oneshot::channel::<()>();
            assert!(shards.spawn_session(move |lease| async move {
                let _ = held_tx.send(lease.shard());
                let _ = release_rx.await;
                drop(lease);
            }));
            assert_eq!(held_rx.await.expect("the holder must report"), 1);

            actix_rt::time::sleep(Duration::from_millis(stale_threshold_ms() * 2)).await;
            assert_eq!(
                shards.live_counts(),
                vec![1, 1],
                "both shards hold one session, so load cannot be what moves placement"
            );
            assert_eq!(
                shards.pick_shard(),
                1,
                "a wedged shard must not be chosen while its heartbeat is stale"
            );

            let _ = release_tx.send(());
            shards.stop();
        });
        reset_heartbeat_slots_for_test();
    }

    /// A shard whose count never drops strands every later session elsewhere.
    #[test]
    fn a_live_session_keeps_its_shard_loaded() {
        in_a_system(|| async {
            let shards = SessionShards::new(2);
            let (placed_tx, placed_rx) = oneshot::channel::<()>();
            let (release_tx, release_rx) = oneshot::channel::<()>();
            assert!(shards.spawn_session(move |lease| async move {
                let _ = placed_tx.send(());
                let _ = release_rx.await;
                drop(lease);
            }));
            actix_rt::time::timeout(BUDGET, placed_rx)
                .await
                .expect("the session must run within the budget")
                .expect("the session must signal placement");
            assert_eq!(
                shards.live_counts().iter().sum::<usize>(),
                1,
                "a live session must be counted against its shard"
            );
            assert_eq!(
                shards.pick_shard(),
                1,
                "the next session must go to the shard that is not carrying one"
            );
            let _ = release_tx.send(());
            shards.stop();
        });
    }
}
