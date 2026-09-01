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

//! Server-side meeting-password verification (issue #1613).
//!
//! `meeting-api` has always Argon2-hashed a meeting password at create time
//! ([`crate::routes::meetings::create_meeting`]) and stored it as
//! `meetings.password_hash`, but until this module existed no join path ever
//! read it back: `has_password` was a display-only boolean and the password was
//! not an access control at all. This module is the verifier.
//!
//! # Where the gate lives, and why there
//!
//! [`MeetingPasswordGate::verify`] is called from exactly one place:
//! `join_as_attendee` in [`crate::routes::participants`], at the top, before
//! anything else. That function owns the `MeetingRow` by value and is the sole
//! entry point into every `meeting_participants` INSERT a non-owner can reach
//! (`db_participants::join_attendee` has exactly two call sites, both inside
//! it; `admit`/`admit_all` are `UPDATE ... WHERE status = 'waiting'` and cannot
//! create a row).
//!
//! An earlier revision instead returned a `PasswordCleared` proof token that
//! `join_as_attendee` consumed. That made "you must call the verifier"
//! checkable by the compiler, but **not** "you must call it against *this*
//! meeting's hash" — `verify(None, "", None, None)` produced a valid token and
//! compiled without a warning. Verifying inside the function that owns the row
//! closes that gap by construction: a caller never gets to choose which hash is
//! checked, because it never gets to pass one.
//!
//! Throttling lives *inside* [`MeetingPasswordGate::verify`] rather than beside
//! it, so there is no way to do the verification while skipping the throttle.
//!
//! # Fail closed
//!
//! A `password_hash` that cannot be parsed as a PHC string is treated as a
//! **denial**, never as "this meeting has no password". Silently downgrading a
//! corrupt hash to "open" is exactly the failure mode this issue was filed
//! about. `verify` returns `Ok` from only two places — the `stored_hash == None`
//! early return and the success arm of the Argon2 verification — so an edit that
//! "recovers" from a parse error by falling through cannot reach a success
//! without adding a new, deliberate, and obviously wrong return.
//!
//! # Why the execution shape matters as much as the check
//!
//! Argon2id at the crate defaults is `m=19456 KiB, t=2, p=1` — ~19 MiB and tens
//! of milliseconds of pure CPU per verification, *by design*. Two deployment
//! facts turn that into a denial-of-service surface if the verification runs
//! inline on the async runtime:
//!
//! 1. `meeting-api` runs a plain `#[tokio::main]` with `worker_threads` unset
//!    (`main.rs`), so the worker count comes from `available_parallelism()`.
//!    Under a sub-1-core cgroup quota that floors to **one** worker thread. An
//!    inline verification therefore blocks the *entire* service — not one
//!    request — for its whole duration.
//! 2. `POST /join-guest` requires no authentication at all, and the
//!    `on_meeting_activated` push makes every waiting client re-join
//!    simultaneously, so one broadcast produces a burst of verifications.
//!
//! So the verification is offloaded with [`tokio::task::spawn_blocking`] and
//! bounded by a semaphore. **The semaphore is not optional.** Tokio's blocking
//! pool defaults to 512 threads; a bare `spawn_blocking` would let 512
//! concurrent verifications run at ~19 MiB each — ~9.7 GiB against a 256 MiB
//! container limit, i.e. an instant OOMKill. Offloading *without* bounding is
//! strictly worse than running inline. Hashing costs the same ~19 MiB, so it
//! takes the *same* permits (issue #2478).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::http::HeaderMap;
use tokio::sync::Semaphore;

use crate::error::AppError;

/// Upper bound on concurrent Argon2 operations, verify and hash alike.
///
/// Each in-flight operation holds ~19 MiB (`m=19456 KiB`). Four permits is
/// ~76 MiB transient, which fits under the service's 256 MiB container limit
/// alongside its steady-state footprint. Raising this without also raising the
/// memory limit re-opens the OOMKill vector described in the module docs — the
/// `const` assertion below makes that a build failure rather than an incident.
const MAX_ARGON2_PERMITS: usize = 4;

/// Peak resident memory one in-flight Argon2 operation holds, in MiB —
/// the `m=19456 KiB` cost parameter of `Argon2::default()`, rounded up.
const ARGON2_MIB_PER_OP: usize = 19;

/// The pod's memory limit, from `helm/meeting-api/values.yaml`
/// (`resources.limits.memory`). Kept here so the two cannot drift silently.
const CONTAINER_LIMIT_MIB: usize = 256;

/// Memory reserved for everything that is not a password verification: the
/// process itself, the sqlx pool, NATS, in-flight request buffers.
const STEADY_STATE_HEADROOM_MIB: usize = 128;

/// Compile-time guard on the relationship the module docs rely on.
///
/// Deliberately a `const` assertion rather than a test: raising
/// [`MAX_ARGON2_PERMITS`] past what the container can hold should fail the
/// **build**, not a test somebody might not run. If this ever fires, either
/// lower the permit count or raise `resources.limits.memory` in both
/// `helm/meeting-api/values.yaml` and the per-region overlay — and update
/// [`CONTAINER_LIMIT_MIB`] to match.
const _: () = assert!(
    MAX_ARGON2_PERMITS * ARGON2_MIB_PER_OP <= CONTAINER_LIMIT_MIB - STEADY_STATE_HEADROOM_MIB,
    "concurrent Argon2 operations could exceed the container memory limit"
);

/// How long a caller waits for an Argon2 permit before it is shed with `503`.
///
/// Sized against the legitimate worst case rather than the median: the
/// `on_meeting_activated` broadcast makes every waiting attendee re-join at
/// once, and each attendee costs two verifications over a meeting's life (once
/// for the `waiting_for_meeting` reply, once for the re-join). A 20-person herd
/// is ~40 verifications; at ~25 ms each on a single permit that is ~1 s, so 10 s
/// leaves an order of magnitude of headroom before a real meeting sheds. It
/// still bounds queue depth: sustained overload sheds rather than growing the
/// queue without limit.
const ARGON2_QUEUE_TIMEOUT: Duration = Duration::from_secs(10);

/// Failed verifications allowed per `(client IP, meeting)` per window.
const MAX_FAILED_PASSWORD_ATTEMPTS: u32 = 5;

/// Window for [`MAX_FAILED_PASSWORD_ATTEMPTS`], in seconds.
///
/// **Tumbling, not sliding.** `window_start` is stamped when an entry is
/// created and reset only once it has gone stale; charging an attempt does not
/// move it. So the budget refills in one step at the boundary rather than
/// decaying, and a client that spends its budget at the very end of one window
/// can spend a fresh one immediately after — up to
/// `2 * MAX_FAILED_PASSWORD_ATTEMPTS` guesses in a short span straddling the
/// boundary. That is immaterial here: the semaphore, not this counter, is what
/// bounds CPU, and 10 guesses buys nothing against Argon2. Written down because
/// the distinction is invisible from the constant's name.
///
/// Short on purpose: a user who fat-fingers their password five times recovers
/// in a minute rather than being locked out of a meeting they may join.
const PASSWORD_ATTEMPT_WINDOW_SECS: u64 = 60;

/// Sweep cadence for stale limiter entries (every N throttle operations).
const ATTEMPT_SWEEP_EVERY_OPS: u64 = 64;

/// Hard ceiling on tracked `(IP, meeting)` keys.
///
/// Unlike the display-name limiter — keyed on an authenticated `user_id` and so
/// bounded by the user base — this map is keyed partly on a client-supplied
/// address and is therefore attacker-growable. At roughly 100 bytes per entry
/// this cap is ~800 KiB. See [`MeetingPasswordGate::consume_attempt`] for what
/// happens at the ceiling and why that choice is safe.
const MAX_TRACKED_ATTEMPT_KEYS: usize = 8192;

/// Whether a failed verification keeps the attempt-budget slot it charged.
///
/// Only a load-shed refunds. In that case the candidate was never evaluated —
/// no permit was ever held, `verify_blocking` never ran — so billing it would
/// convert *server* overload into a *client* lockout: five sheds and a user who
/// never typed a wrong password is throttled for a full window. That also keeps
/// the code honest with two shipped claims, that
/// [`MAX_FAILED_PASSWORD_ATTEMPTS`] counts failed *verifications*, and that
/// `VerifierOverloaded` means the supplied password was not evaluated.
///
/// A rejection bills, obviously. A **panicked** verifier task also bills: we
/// cannot tell whether it died before or after doing the work, and the safe
/// assumption for an access control is that an attempt was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AttemptBilling {
    /// Keep the charged slot — the attempt counts against the budget.
    Bill,
    /// Give the slot back — nothing was verified.
    Refund,
}

/// Why a bounded Argon2 offload produced no result. `Shed` never ran the work.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Argon2Unavailable {
    Shed,
    Panicked,
}

/// Per-instance throttle and concurrency bound for meeting-password checks.
///
/// Held in [`crate::state::AppState`] behind an `Arc`, so every handler on an
/// instance shares one. See the module docs for why both halves exist.
///
/// The throttle is in-process. That is sufficient at the current
/// `replicaCount: 1` with no HPA; **if this service is ever scaled out, the
/// per-instance budget multiplies by the replica count** and the throttle needs
/// to move to a shared store (or the ingress).
pub struct MeetingPasswordGate {
    /// Bounds concurrent Argon2 operations — CPU *and* the ~19 MiB each holds.
    argon2_permits: Semaphore,
    /// How long to wait for a permit before shedding with `503`.
    queue_timeout: Duration,
    /// Failed attempts per `(client IP, meeting)`: `(window_start, count)`.
    failed_attempts: Mutex<HashMap<(IpAddr, String), (Instant, u32)>>,
    /// Operation counter driving periodic sweeps of `failed_attempts`.
    ops: AtomicU64,
    /// Highest number of Argon2 operations ever in flight at once. Monotonic
    /// (`fetch_max`), so it can be read after the fact without racing the
    /// workers. Exposed for tests and useful as an operational gauge.
    peak_in_flight: AtomicUsize,
    /// Argon2 operations currently in flight.
    in_flight: AtomicUsize,
}

impl Default for MeetingPasswordGate {
    fn default() -> Self {
        Self::new()
    }
}

impl MeetingPasswordGate {
    /// Build a gate sized to the container's CPU allocation.
    ///
    /// Permits track `available_parallelism()` clamped to
    /// `1..=MAX_ARGON2_PERMITS`: one permit on the single-core floor a
    /// sub-1-core cgroup quota produces, up to four when the pod has real CPU.
    /// More permits than cores would not add throughput for a CPU-bound hash —
    /// it would only multiply peak memory.
    pub fn new() -> Self {
        let permits = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, MAX_ARGON2_PERMITS);
        Self::with_config(permits, ARGON2_QUEUE_TIMEOUT)
    }

    /// Build a gate with explicit bounds. Tests use this to drive the
    /// load-shedding and concurrency-bound paths deterministically.
    pub fn with_config(permits: usize, queue_timeout: Duration) -> Self {
        Self {
            argon2_permits: Semaphore::new(permits),
            queue_timeout,
            failed_attempts: Mutex::new(HashMap::new()),
            ops: AtomicU64::new(0),
            peak_in_flight: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Highest concurrent Argon2 operation count observed since construction.
    pub fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::Relaxed)
    }

    /// Permits not currently held.
    pub fn available_permits(&self) -> usize {
        self.argon2_permits.available_permits()
    }

    /// Verify a join request's password against a meeting's stored Argon2 hash.
    ///
    /// - `client_ip` — throttling key. `None` disables throttling for this call;
    ///   see [`client_ip_for_throttle`] for when that happens and why it is safe.
    /// - `meeting_id` — the other half of the throttling key, so a lockout is
    ///   scoped to one meeting rather than to the whole service.
    /// - `stored_hash` — the meeting's `password_hash`. `None` means the meeting
    ///   has no password and every caller clears the gate.
    /// - `supplied` — the plaintext from the join request body, if any.
    ///
    /// # Errors
    ///
    /// - [`AppError::meeting_password_required`] (403) — meeting has a password,
    ///   request carried none.
    /// - [`AppError::invalid_meeting_password`] (403) — supplied password does
    ///   not verify, **or** the stored hash is unparseable. Deliberately
    ///   indistinguishable to the caller; the corrupt-hash case is logged at
    ///   `error!` for operators.
    /// - [`AppError::too_many_password_attempts`] (429) — this `(IP, meeting)`
    ///   pair has burned its failure budget for the window.
    /// - [`AppError::verifier_overloaded`] (503) — no verification permit became
    ///   available within [`ARGON2_QUEUE_TIMEOUT`].
    ///
    /// # Cost, and what is free
    ///
    /// Both early returns below happen **before** any throttle bookkeeping,
    /// permit acquisition or blocking-pool hop, so they cost nothing beyond the
    /// `Option` checks:
    ///
    /// - a meeting with no password (the overwhelming majority) never touches
    ///   Argon2 at all, which is why this change is not a tax on normal traffic;
    /// - a request that supplies no password is rejected without hashing, so an
    ///   empty-bodied flood cannot buy any CPU.
    ///
    /// Only a caller that supplies *something* against a meeting that *has* a
    /// password reaches the expensive path — which is exactly the attacker
    /// profile, and exactly what the throttle and the semaphore bound.
    ///
    /// # Timing
    ///
    /// A protected meeting costs a full Argon2 verification while an open one
    /// returns immediately, so response latency reveals whether a meeting is
    /// password-protected. That is not a new disclosure: `has_password` is
    /// already published verbatim on `GET /api/v1/meetings`, `/feed`, `/joined`
    /// and `/{meeting_id}`. The comparison against the hash itself is
    /// constant-time (`PasswordVerifier` compares `Output` values, whose
    /// `PartialEq` delegates to `subtle::ConstantTimeEq`), which is the property
    /// that actually matters.
    pub async fn verify(
        &self,
        client_ip: Option<IpAddr>,
        meeting_id: &str,
        stored_hash: Option<&str>,
        supplied: Option<&str>,
    ) -> Result<(), AppError> {
        let Some(stored) = stored_hash else {
            // No password on this meeting — nothing to verify. The only success
            // path that does not run Argon2.
            return Ok(());
        };

        let Some(candidate) = supplied else {
            // Costs no CPU, so deliberately not throttled: rejecting here is
            // cheaper than tracking the attempt would be.
            return Err(AppError::meeting_password_required());
        };

        // Throttle BEFORE spending a permit or a hash. Charged optimistically
        // and refunded below whenever nothing was actually verified, so a burst
        // of concurrent wrong passwords cannot all slip through a
        // check-then-act window, while neither a legitimate joiner nor a
        // load-shed victim burns budget.
        //
        // Note the `?` is deliberately NOT used on `verify_offloaded`: an early
        // return here would skip the refund and silently bill a shed request.
        let charged = self.consume_attempt(client_ip, meeting_id)?;

        match self.verify_offloaded(stored, candidate).await {
            Ok(()) => {
                if charged {
                    self.refund_attempt(client_ip, meeting_id);
                }
                Ok(())
            }
            Err((err, AttemptBilling::Refund)) => {
                if charged {
                    self.refund_attempt(client_ip, meeting_id);
                }
                Err(err)
            }
            Err((err, AttemptBilling::Bill)) => Err(err),
        }
    }

    /// Hash a meeting password on the blocking pool, bounded by the same permits
    /// as verification. The only route to [`hash_blocking`].
    pub async fn hash(&self, plaintext: &str) -> Result<String, AppError> {
        let plaintext = plaintext.to_owned();
        match self
            .run_bounded("hash", move || hash_blocking(&plaintext))
            .await
        {
            Ok(inner) => inner,
            Err(Argon2Unavailable::Shed) => Err(AppError::password_hasher_overloaded()),
            Err(Argon2Unavailable::Panicked) => {
                Err(AppError::internal("password hash task panicked"))
            }
        }
    }

    /// Turn a validated [`PasswordIntent`] into the value the column takes. Call
    /// only once the caller is known to own the meeting.
    pub async fn hash_intent(
        &self,
        intent: PasswordIntent<'_>,
    ) -> Result<PasswordUpdate, AppError> {
        Ok(match intent {
            PasswordIntent::Unchanged => PasswordUpdate::Unchanged,
            PasswordIntent::Clear => PasswordUpdate::Clear,
            PasswordIntent::Set(pw) => PasswordUpdate::Set(self.hash(pw).await?),
        })
    }

    /// Run the Argon2 verification on the blocking pool, bounded by the
    /// semaphore.
    async fn verify_offloaded(
        &self,
        stored: &str,
        candidate: &str,
    ) -> Result<(), (AppError, AttemptBilling)> {
        let stored = stored.to_owned();
        let candidate = candidate.to_owned();

        match self
            .run_bounded("verify", move || verify_blocking(&stored, &candidate))
            .await
        {
            Ok(inner) => inner.map_err(|err| (err, AttemptBilling::Bill)),
            Err(Argon2Unavailable::Shed) => {
                Err((AppError::verifier_overloaded(), AttemptBilling::Refund))
            }
            Err(Argon2Unavailable::Panicked) => {
                Err((AppError::invalid_meeting_password(), AttemptBilling::Bill))
            }
        }
    }

    /// Acquire an Argon2 permit, then run `work` on the blocking pool — the sole
    /// route to Argon2 here, which makes [`MAX_ARGON2_PERMITS`] a process bound.
    /// Acquiring the permit *inside* `spawn_blocking` would park a pool thread
    /// per waiter and reintroduce the 512-thread blow-up it exists to prevent.
    async fn run_bounded<T, F>(&self, op: &'static str, work: F) -> Result<T, Argon2Unavailable>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permit =
            match tokio::time::timeout(self.queue_timeout, self.argon2_permits.acquire()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_closed)) => {
                    tracing::error!(op, "meeting-password Argon2 semaphore closed; shedding");
                    return Err(Argon2Unavailable::Shed);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        op,
                        queue_timeout_secs = self.queue_timeout.as_secs(),
                        "no Argon2 permit available; shedding request"
                    );
                    return Err(Argon2Unavailable::Shed);
                }
            };

        let in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_in_flight.fetch_max(in_flight, Ordering::Relaxed);

        let result = tokio::task::spawn_blocking(work).await;

        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        drop(permit);

        result.map_err(|join_err| {
            tracing::error!(op, error = %join_err, "Argon2 task panicked");
            Argon2Unavailable::Panicked
        })
    }

    /// Charge one failed-attempt slot against `(client_ip, meeting_id)`.
    ///
    /// Returns `Ok(true)` when a slot was charged (and so must be refunded on
    /// success), `Ok(false)` when this call is not tracked, or `Err(429)` when
    /// the budget for this window is exhausted.
    ///
    /// Not tracked, deliberately, in two cases:
    ///
    /// - `client_ip` is `None` — see [`client_ip_for_throttle`]. Collapsing
    ///   unidentifiable callers into one shared bucket would let a single
    ///   attacker lock every legitimate joiner out of a meeting, which is a
    ///   worse failure than not throttling them.
    /// - the map is at [`MAX_TRACKED_ATTEMPT_KEYS`] and a sweep freed nothing.
    ///   Refusing to grow keeps memory bounded; the semaphore still bounds the
    ///   CPU and memory an untracked caller can consume, so what is lost is
    ///   fairness, never the concurrency bound.
    fn consume_attempt(
        &self,
        client_ip: Option<IpAddr>,
        meeting_id: &str,
    ) -> Result<bool, AppError> {
        let Some(ip) = client_ip else {
            return Ok(false);
        };

        let sweep_tick = self.ops.fetch_add(1, Ordering::Relaxed) + 1;

        let mut attempts = self
            .failed_attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if sweep_tick.is_multiple_of(ATTEMPT_SWEEP_EVERY_OPS)
            || attempts.len() >= MAX_TRACKED_ATTEMPT_KEYS
        {
            attempts.retain(|_, (window_start, _)| {
                window_start.elapsed().as_secs() < PASSWORD_ATTEMPT_WINDOW_SECS
            });
        }

        let key = (ip, meeting_id.to_owned());
        if !attempts.contains_key(&key) && attempts.len() >= MAX_TRACKED_ATTEMPT_KEYS {
            tracing::warn!(
                tracked_keys = attempts.len(),
                "meeting-password attempt limiter at capacity; this attempt is not throttled"
            );
            return Ok(false);
        }

        let entry = attempts.entry(key).or_insert_with(|| (Instant::now(), 0));

        // Reset a stale window on access, even when no periodic sweep has run.
        if entry.0.elapsed().as_secs() >= PASSWORD_ATTEMPT_WINDOW_SECS {
            *entry = (Instant::now(), 0);
        }

        if entry.1 >= MAX_FAILED_PASSWORD_ATTEMPTS {
            return Err(AppError::too_many_password_attempts());
        }
        entry.1 += 1;
        Ok(true)
    }

    /// Give back the slot charged by [`Self::consume_attempt`] after a
    /// successful verification, so a correct password never costs budget.
    fn refund_attempt(&self, client_ip: Option<IpAddr>, meeting_id: &str) {
        let Some(ip) = client_ip else {
            return;
        };
        let mut attempts = self
            .failed_attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = attempts.get_mut(&(ip, meeting_id.to_owned())) {
            entry.1 = entry.1.saturating_sub(1);
        }
    }
}

/// The Argon2 work itself, isolated so it can run on the blocking pool.
///
/// `Argon2::default()` is Argon2id v0x13 at the crate's default cost
/// (`m=19456 KiB, t=2, p=1`). Leaving that alone is a deliberate choice, not an
/// oversight: [`PasswordVerifier`] derives the cost parameters from the stored
/// PHC string, not from this instance, so weakening them here would be a
/// **no-op** on every existing row. Cheapening them at create time would affect
/// only future meetings and would still leave every stored hash at the current
/// cost absent a re-hash migration — and it would buy a 2-4x factor where the
/// throughput problem needs orders of magnitude. Bounding concurrency and
/// throttling attempts are the levers that work; cost parameters are not.
fn verify_blocking(stored: &str, candidate: &str) -> Result<(), AppError> {
    // Fail closed. `PasswordHash::new` rejects anything that is not a valid PHC
    // string — a truncated column, a hash written by a different algorithm, an
    // empty string. Every one of those means "we cannot check this password",
    // and the safe answer to "we cannot check" on an access control is no.
    let parsed = match PasswordHash::new(stored) {
        Ok(parsed) => parsed,
        Err(e) => {
            // The hash itself is never logged; only the parse failure reason,
            // which describes the PHC syntax problem and not the secret.
            tracing::error!(
                "meeting password_hash is not a parseable PHC string ({e}); denying join. \
                 The meeting is unjoinable by non-owners until the row is repaired."
            );
            return Err(AppError::invalid_meeting_password());
        }
    };

    Argon2::default()
        .verify_password(candidate.as_bytes(), &parsed)
        .map_err(|_| AppError::invalid_meeting_password())
}

/// Hash a plaintext meeting password into the PHC string stored in
/// `meetings.password_hash`. The only place that conversion happens.
fn hash_blocking(plaintext: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| AppError::internal(&format!("password hash error: {e}")))
}

/// What a `PATCH /api/v1/meetings/{meeting_id}` asks of the stored password
/// hash. [`Self::Set`] carries the hash, never the plaintext.
pub enum PasswordUpdate {
    Unchanged,
    Set(String),
    Clear,
}

/// A validated PATCH body, before any hashing has been paid for.
pub enum PasswordIntent<'a> {
    Unchanged,
    Set(&'a str),
    Clear,
}

impl PasswordIntent<'_> {
    /// Whether this intent changes `meetings.password_hash`.
    pub fn is_change(&self) -> bool {
        !matches!(self, Self::Unchanged)
    }
}

impl std::fmt::Debug for PasswordIntent<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unchanged => f.write_str("Unchanged"),
            Self::Set(_) => write!(f, "Set({REDACTED_PLAINTEXT})"),
            Self::Clear => f.write_str("Clear"),
        }
    }
}

const REDACTED_PLAINTEXT: &str = "<redacted>";

impl std::fmt::Debug for PasswordUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unchanged => f.write_str("Unchanged"),
            Self::Set(_) => write!(f, "Set({REDACTED_HASH})"),
            Self::Clear => f.write_str("Clear"),
        }
    }
}

const REDACTED_HASH: &str = "<hash>";

/// Decide what a PATCH body's `password` / `remove_password` pair means. The two
/// ambiguous bodies are refused. Deliberately free of Argon2 work, so a caller
/// who is about to get a 400 or a 403 never buys any.
pub fn parse_password_update(
    password: Option<&str>,
    remove_password: Option<bool>,
) -> Result<PasswordIntent<'_>, AppError> {
    match (password, remove_password.unwrap_or(false)) {
        (Some(_), true) => Err(AppError::bad_request(
            "`password` and `remove_password` are mutually exclusive",
        )),
        (None, true) => Ok(PasswordIntent::Clear),
        (Some(""), false) => Err(AppError::bad_request(
            "`password` must not be empty; send `remove_password: true` to clear it",
        )),
        (Some(pw), false) => Ok(PasswordIntent::Set(pw)),
        (None, false) => Ok(PasswordIntent::Unchanged),
    }
}

/// Resolve the address used to key the failed-attempt throttle.
///
/// # Why the *last* `X-Forwarded-For` entry
///
/// `meeting-api` is deployed behind an nginx ingress (`ingress.className: nginx`
/// in `helm/meeting-api/values.yaml`), which sets the header from
/// `$proxy_add_x_forwarded_for` — it **appends** the peer it actually saw.
/// Everything to the left of that final entry is client-supplied and trivially
/// forged; the final entry is written by our own proxy and is not. So with
/// exactly one trusted reverse-proxy hop, the rightmost entry is the only
/// position that means anything.
///
/// # Deployment precondition: the ingress must preserve the client address
///
/// This throttle is only as good as the address it is keyed on, and that
/// address is produced *outside* this repository. If the ingress-nginx Service
/// runs with `externalTrafficPolicy: Cluster` — the Kubernetes **default** —
/// kube-proxy SNATs incoming connections, so nginx's `$remote_addr` is the
/// node's address, and the `X-Forwarded-For` entry it appends is the node's
/// too. Every user arriving through that node then shares one throttle bucket:
/// five wrong passwords from any one of them would `429` all of them.
///
/// Nothing under `helm/` can settle this — the controller's own Service is
/// deployed separately — so it is a precondition, not an invariant. Verify with:
///
/// ```text
/// kubectl -n ingress-nginx get svc -o jsonpath='{.items[*].spec.externalTrafficPolicy}'
/// ```
///
/// `Local` preserves the client address and this function is correct as
/// written. `Cluster` requires either flipping the policy or re-keying the
/// throttle onto something the proxy cannot flatten.
///
/// **If a CDN or a second proxy is ever put in front of the ingress this becomes
/// wrong** — the rightmost entry would then be that proxy's address and every
/// user would share one throttle bucket. Such a topology change must come with a
/// trusted-hop count here.
///
/// Falls back to the transport peer address when the header is absent (local
/// development, the docker-compose stack, direct in-cluster calls), and returns
/// `None` when neither is available — the case in `oneshot`-style tests that
/// never establish a socket. See [`MeetingPasswordGate::consume_attempt`] for
/// why `None` means "do not throttle" rather than "share one bucket".
pub fn client_ip_for_throttle(headers: &HeaderMap, peer: Option<IpAddr>) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .rsplit(',')
                .next()
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .and_then(|entry| entry.parse::<IpAddr>().ok())
        })
        .or(peer)
}

/// Axum extractor resolving the throttling address for a request via
/// [`client_ip_for_throttle`].
///
/// Deliberately **infallible**: a request that cannot be attributed to an
/// address still gets served, it just is not throttled (see
/// [`MeetingPasswordGate::consume_attempt`]). Making it a rejection instead
/// would take the service down on any deployment that does not populate
/// `ConnectInfo`.
///
/// `ConnectInfo<SocketAddr>` is read out of the request extensions rather than
/// extracted directly because axum's `ConnectInfo` implements only
/// `FromRequestParts`, not `OptionalFromRequestParts` — so `Option<ConnectInfo<_>>`
/// is not a valid extractor, and a bare `ConnectInfo` would reject every request
/// in the `oneshot`-style integration tests, which never open a socket.
///
/// Bundling the header and peer lookups into one extractor also means a handler
/// cannot resolve the peer address while forgetting `X-Forwarded-For`, which
/// would silently collapse every user behind the ingress into one bucket.
pub struct ClientAddr(pub Option<IpAddr>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for ClientAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|connect_info| connect_info.0.ip());
        Ok(ClientAddr(client_ip_for_throttle(&parts.headers, peer)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    const IP: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
    const OTHER_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8));

    /// Hash a password through the production hasher, so these tests exercise
    /// the real stored-hash format rather than a hand-written fixture.
    fn hash_like_create_meeting(plaintext: &str) -> String {
        hash_blocking(plaintext).expect("hashing with default params cannot fail")
    }

    /// A gate with generous bounds, for tests about verification semantics
    /// rather than about throttling or shedding.
    fn open_gate() -> MeetingPasswordGate {
        MeetingPasswordGate::with_config(4, Duration::from_secs(10))
    }

    // ── Verification semantics ───────────────────────────────────────────

    #[tokio::test]
    async fn no_stored_password_clears_the_gate_regardless_of_input() {
        let gate = open_gate();
        assert!(gate.verify(Some(IP), "m", None, None).await.is_ok());
        assert!(gate
            .verify(Some(IP), "m", None, Some("anything"))
            .await
            .is_ok());
        assert!(gate.verify(Some(IP), "m", None, Some("")).await.is_ok());
    }

    /// The no-password path must not spend a permit or reach the blocking pool
    /// — it is the overwhelming majority of real traffic.
    #[tokio::test]
    async fn no_stored_password_never_runs_argon2() {
        let gate = open_gate();
        for _ in 0..25 {
            gate.verify(Some(IP), "m", None, Some("x"))
                .await
                .expect("open meeting");
        }
        assert_eq!(
            gate.peak_in_flight(),
            0,
            "an open meeting must never occupy a verification permit"
        );
    }

    /// Likewise a request that supplies nothing: rejecting is free, so an
    /// empty-bodied flood must not buy any Argon2 time.
    #[tokio::test]
    async fn absent_password_never_runs_argon2() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");
        for _ in 0..25 {
            let err = gate
                .verify(Some(IP), "m", Some(&stored), None)
                .await
                .expect_err("absent password must be rejected");
            assert_eq!(err.body.code, "MEETING_PASSWORD_REQUIRED");
        }
        assert_eq!(
            gate.peak_in_flight(),
            0,
            "an absent password must never occupy a verification permit"
        );
    }

    #[tokio::test]
    async fn correct_password_clears_the_gate() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("correct horse battery staple");
        gate.verify(
            Some(IP),
            "m",
            Some(&stored),
            Some("correct horse battery staple"),
        )
        .await
        .expect("the password used to build the hash must verify");
    }

    #[tokio::test]
    async fn wrong_password_is_rejected() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");
        let err = gate
            .verify(Some(IP), "m", Some(&stored), Some("s3cret "))
            .await
            .expect_err("a near-miss password must be rejected");
        assert_eq!(err.body.code, "INVALID_MEETING_PASSWORD");
        assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn absent_password_is_rejected_with_a_distinct_code() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");
        let err = gate
            .verify(Some(IP), "m", Some(&stored), None)
            .await
            .expect_err("a protected meeting must reject a join carrying no password");
        assert_eq!(err.body.code, "MEETING_PASSWORD_REQUIRED");
        assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn empty_supplied_password_is_rejected() {
        // `create_meeting` refuses to store a hash for an empty password, so an
        // empty string is never a valid credential.
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");
        let err = gate
            .verify(Some(IP), "m", Some(&stored), Some(""))
            .await
            .expect_err("an empty password must not clear a protected meeting");
        assert_eq!(err.body.code, "INVALID_MEETING_PASSWORD");
    }

    /// The fail-closed invariant. Every one of these stored values is something
    /// `PasswordHash::new` cannot parse; each must deny rather than fall through
    /// to "this meeting has no password".
    #[tokio::test]
    async fn corrupt_stored_hash_denies_instead_of_opening_the_meeting() {
        let gate = open_gate();
        let corrupt = [
            "",
            "not-a-phc-string",
            "$argon2id$v=19$m=19456,t=2,p=1$truncated",
            "$unknownalg$v=1$whatever$abc",
            "$2b$12$K3JNi5xHhK4YQ0/OaXVJ9uYy3H1zQ2WlQ0k5xkJ8Y0m6nJj8i5Xhy",
        ];
        for (i, stored) in corrupt.iter().enumerate() {
            // A distinct meeting per case so the throttle never interferes.
            let meeting = format!("corrupt-{i}");
            let err = gate
                .verify(Some(IP), &meeting, Some(stored), Some("any-guess"))
                .await
                .err()
                .unwrap_or_else(|| {
                    panic!("corrupt stored hash {stored:?} must DENY, not clear the gate")
                });
            assert_eq!(
                err.body.code, "INVALID_MEETING_PASSWORD",
                "corrupt stored hash {stored:?} must deny as an invalid password"
            );
        }
    }

    #[tokio::test]
    async fn corrupt_stored_hash_with_no_supplied_password_still_denies() {
        let gate = open_gate();
        let err = gate
            .verify(Some(IP), "m", Some("not-a-phc-string"), None)
            .await
            .expect_err("corrupt hash + no password must deny");
        assert_eq!(err.body.code, "MEETING_PASSWORD_REQUIRED");
    }

    #[tokio::test]
    async fn verification_is_exact() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("MeetingPass");
        assert!(gate
            .verify(None, "a", Some(&stored), Some("meetingpass"))
            .await
            .is_err());
        assert!(gate
            .verify(None, "b", Some(&stored), Some("MeetingPass "))
            .await
            .is_err());
        assert!(gate
            .verify(None, "c", Some(&stored), Some(" MeetingPass"))
            .await
            .is_err());
        assert!(gate
            .verify(None, "d", Some(&stored), Some("MeetingPass"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn unicode_password_round_trips() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("día-de-reunión-🔐");
        assert!(gate
            .verify(None, "a", Some(&stored), Some("día-de-reunión-🔐"))
            .await
            .is_ok());
        assert!(gate
            .verify(None, "b", Some(&stored), Some("dia-de-reunion-🔐"))
            .await
            .is_err());
    }

    // ── Concurrency bound (MUST FIX a) ───────────────────────────────────

    /// The bound that stops tokio's 512-thread blocking pool from holding
    /// ~19 MiB per thread. `peak_in_flight` is maintained with `fetch_max`, so
    /// it is monotonic and can be read after the fact without racing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_verifications_never_exceed_the_permit_count() {
        let gate = Arc::new(MeetingPasswordGate::with_config(2, Duration::from_secs(30)));
        let stored = Arc::new(hash_like_create_meeting("s3cret"));

        let mut handles = Vec::new();
        for i in 0..16u8 {
            let gate = Arc::clone(&gate);
            let stored = Arc::clone(&stored);
            handles.push(tokio::spawn(async move {
                // A distinct IP per task so the throttle cannot mask the bound
                // by rejecting requests before they reach the semaphore.
                let ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, i));
                let _ = gate
                    .verify(Some(ip), "m", Some(&stored), Some("wrong"))
                    .await;
            }));
        }
        for h in handles {
            h.await.expect("verification task must not panic");
        }

        assert!(
            gate.peak_in_flight() <= 2,
            "at most 2 verifications may run at once, observed {}",
            gate.peak_in_flight()
        );
        assert!(
            gate.peak_in_flight() >= 1,
            "the test must actually have exercised the verifier"
        );
        assert_eq!(
            gate.available_permits(),
            2,
            "every permit must be returned once the burst drains"
        );
    }

    /// The defect this whole execution shape exists to prevent: an inline
    /// Argon2 verification blocks the async runtime, and in production that
    /// runtime has exactly **one** worker thread, so the pod answers nothing
    /// else for the duration.
    ///
    /// Run on a deliberately single-threaded runtime. A co-tenant task ticks
    /// every 10 ms and records how late each tick was, while a burst of
    /// verifications runs. If the hash ran inline, the ticker could not be
    /// scheduled at all until the burst finished, so its worst lateness would
    /// approach the burst's total duration.
    ///
    /// The assertion is a *ratio* rather than an absolute millisecond bound so
    /// it is meaningful on both a fast laptop and a loaded CI box: whatever the
    /// burst costs, a runtime that stayed responsive must have kept its worst
    /// tick lateness well under half of it.
    #[tokio::test(flavor = "current_thread")]
    async fn verification_does_not_block_the_async_runtime() {
        let gate = Arc::new(MeetingPasswordGate::with_config(1, Duration::from_secs(30)));
        let stored = Arc::new(hash_like_create_meeting("s3cret"));

        let worst_lateness = Arc::new(Mutex::new(Duration::ZERO));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let ticker = tokio::spawn({
            let worst = Arc::clone(&worst_lateness);
            let stop = Arc::clone(&stop);
            async move {
                const PERIOD: Duration = Duration::from_millis(10);
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(PERIOD).await;
                    let lateness = last.elapsed().saturating_sub(PERIOD);
                    let mut w = worst.lock().expect("lateness mutex");
                    if lateness > *w {
                        *w = lateness;
                    }
                    last = Instant::now();
                }
            }
        });

        // Let the ticker establish a rhythm before the burst starts.
        tokio::time::sleep(Duration::from_millis(30)).await;

        let burst_start = Instant::now();
        let mut handles = Vec::new();
        for i in 0..12u8 {
            let gate = Arc::clone(&gate);
            let stored = Arc::clone(&stored);
            handles.push(tokio::spawn(async move {
                let ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, i));
                let _ = gate
                    .verify(Some(ip), "m", Some(&stored), Some("wrong"))
                    .await;
            }));
        }
        for h in handles {
            h.await.expect("verification task must not panic");
        }
        let burst = burst_start.elapsed();

        stop.store(true, Ordering::Relaxed);
        let _ = ticker.await;

        let worst = *worst_lateness.lock().expect("lateness mutex");

        assert!(
            gate.peak_in_flight() >= 1,
            "the burst must actually have exercised the verifier"
        );
        assert!(
            worst * 2 < burst,
            "the runtime stalled during verification: worst tick lateness {worst:?} \
             is not comfortably below the {burst:?} burst — the hash is running \
             inline on the runtime instead of on the blocking pool"
        );
    }

    /// Overload sheds with 503 instead of queueing without limit.
    #[tokio::test]
    async fn exhausted_permits_shed_with_503() {
        let gate = MeetingPasswordGate::with_config(1, Duration::from_millis(50));
        let stored = hash_like_create_meeting("s3cret");

        // Hold the only permit for longer than the queue timeout.
        let held = gate
            .argon2_permits
            .try_acquire()
            .expect("a fresh gate has its permit available");

        let err = gate
            .verify(Some(IP), "m", Some(&stored), Some("s3cret"))
            .await
            .expect_err("with no permit available the request must be shed");
        assert_eq!(err.body.code, "VERIFIER_OVERLOADED");
        assert_eq!(err.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);

        drop(held);

        // Once the permit is back the same correct password succeeds — the shed
        // is transient, not a lockout.
        gate.verify(Some(IP), "m", Some(&stored), Some("s3cret"))
            .await
            .expect("a returned permit must let the correct password through");
    }

    /// A shed request must not be billed to the client's failure budget.
    ///
    /// Without this, server overload silently becomes a client lockout: five
    /// `503`s and a user who never typed a wrong password is `429`d for a full
    /// window. It is also what keeps two shipped claims true — that the budget
    /// counts failed *verifications*, and that `VerifierOverloaded` means the
    /// password was never evaluated.
    #[tokio::test]
    async fn shed_requests_do_not_consume_the_attempt_budget() {
        let gate = MeetingPasswordGate::with_config(1, Duration::from_millis(30));
        let stored = hash_like_create_meeting("s3cret");

        // Hold the only permit so every attempt below is shed, never verified.
        let held = gate
            .argon2_permits
            .try_acquire()
            .expect("a fresh gate has its permit available");

        for _ in 0..(MAX_FAILED_PASSWORD_ATTEMPTS * 3) {
            let err = gate
                .verify(Some(IP), "m", Some(&stored), Some("s3cret"))
                .await
                .expect_err("no permit available");
            assert_eq!(err.body.code, "VERIFIER_OVERLOADED");
        }

        drop(held);

        // The budget must be untouched: a full run of real failures is still
        // available, and only the one after it is throttled.
        for _ in 0..MAX_FAILED_PASSWORD_ATTEMPTS {
            let err = gate
                .verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await
                .expect_err("wrong password");
            assert_eq!(
                err.body.code, "INVALID_MEETING_PASSWORD",
                "a shed must not have pre-spent the failure budget"
            );
        }
        assert_eq!(
            gate.verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await
                .expect_err("budget now genuinely exhausted")
                .body
                .code,
            "TOO_MANY_PASSWORD_ATTEMPTS"
        );
    }

    /// And a shed must not lock out the correct password either — the most
    /// user-visible form of the same bug.
    #[tokio::test]
    async fn a_shed_does_not_block_a_later_correct_password() {
        let gate = MeetingPasswordGate::with_config(1, Duration::from_millis(30));
        let stored = hash_like_create_meeting("s3cret");

        let held = gate
            .argon2_permits
            .try_acquire()
            .expect("a fresh gate has its permit available");
        for _ in 0..(MAX_FAILED_PASSWORD_ATTEMPTS * 2) {
            let _ = gate
                .verify(Some(IP), "m", Some(&stored), Some("s3cret"))
                .await;
        }
        drop(held);

        gate.verify(Some(IP), "m", Some(&stored), Some("s3cret"))
            .await
            .expect("the correct password must still be accepted after a shed storm");
    }

    /// #2478, hash path: same ticker-lateness shape as its verify twin above.
    #[tokio::test(flavor = "current_thread")]
    async fn hashing_does_not_block_the_async_runtime() {
        let gate = Arc::new(MeetingPasswordGate::with_config(1, Duration::from_secs(30)));

        let worst_lateness = Arc::new(Mutex::new(Duration::ZERO));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let ticker = tokio::spawn({
            let worst = Arc::clone(&worst_lateness);
            let stop = Arc::clone(&stop);
            async move {
                const PERIOD: Duration = Duration::from_millis(10);
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(PERIOD).await;
                    let lateness = last.elapsed().saturating_sub(PERIOD);
                    let mut w = worst.lock().expect("lateness mutex");
                    if lateness > *w {
                        *w = lateness;
                    }
                    last = Instant::now();
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(30)).await;

        let burst_start = Instant::now();
        let mut handles = Vec::new();
        for i in 0..12u8 {
            let gate = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                gate.hash(&format!("rotate-{i}"))
                    .await
                    .expect("hashing with default params cannot fail");
            }));
        }
        for h in handles {
            h.await.expect("hash task must not panic");
        }
        let burst = burst_start.elapsed();

        stop.store(true, Ordering::Relaxed);
        let _ = ticker.await;

        let worst = *worst_lateness.lock().expect("lateness mutex");

        assert!(
            worst * 2 < burst,
            "the runtime stalled during hashing: worst tick lateness {worst:?} is not \
             comfortably below the {burst:?} burst — the hash is running inline on the \
             runtime instead of on the blocking pool"
        );
        assert!(
            gate.peak_in_flight() >= 1,
            "the burst must actually have gone through the bounded offload"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_hashes_never_exceed_the_permit_count() {
        let gate = Arc::new(MeetingPasswordGate::with_config(2, Duration::from_secs(30)));

        let mut handles = Vec::new();
        for i in 0..16u8 {
            let gate = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                gate.hash(&format!("rotate-{i}"))
                    .await
                    .expect("hashing with default params cannot fail");
            }));
        }
        for h in handles {
            h.await.expect("hash task must not panic");
        }

        assert!(
            gate.peak_in_flight() <= 2,
            "at most 2 Argon2 operations may run at once, observed {}",
            gate.peak_in_flight()
        );
        assert!(
            gate.peak_in_flight() >= 1,
            "the test must actually have exercised the hasher"
        );
        assert_eq!(
            gate.available_permits(),
            2,
            "every permit must be returned once the burst drains"
        );
    }

    #[tokio::test]
    async fn a_held_permit_sheds_both_a_hash_and_a_verify() {
        let gate = MeetingPasswordGate::with_config(1, Duration::from_millis(30));
        let stored = hash_like_create_meeting("s3cret");

        let held = gate
            .argon2_permits
            .try_acquire()
            .expect("a fresh gate has its permit available");

        let hash_err = gate
            .hash("rotate-me")
            .await
            .expect_err("with no permit available the hash must be shed");
        assert_eq!(hash_err.body.code, "PASSWORD_HASHER_OVERLOADED");
        assert_eq!(hash_err.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);

        let verify_err = gate
            .verify(Some(IP), "m", Some(&stored), Some("s3cret"))
            .await
            .expect_err("the same held permit must shed a verify");
        assert_eq!(verify_err.body.code, "VERIFIER_OVERLOADED");

        drop(held);

        gate.hash("rotate-me")
            .await
            .expect("a returned permit must let the hash through — the shed is transient");
    }

    // ── Failed-attempt throttle (MUST FIX b) ─────────────────────────────

    #[tokio::test]
    async fn failed_attempts_are_capped_per_ip_and_meeting() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");

        for _ in 0..MAX_FAILED_PASSWORD_ATTEMPTS {
            let err = gate
                .verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await
                .expect_err("wrong password");
            assert_eq!(err.body.code, "INVALID_MEETING_PASSWORD");
        }

        let err = gate
            .verify(Some(IP), "m", Some(&stored), Some("wrong"))
            .await
            .expect_err("budget exhausted");
        assert_eq!(err.body.code, "TOO_MANY_PASSWORD_ATTEMPTS");
        assert_eq!(err.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    }

    /// The throttle must cut CPU, not merely return a different status — so an
    /// over-budget attempt must never reach Argon2.
    #[tokio::test]
    async fn throttled_attempts_do_not_run_argon2() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");

        for _ in 0..MAX_FAILED_PASSWORD_ATTEMPTS {
            let _ = gate
                .verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await;
        }
        let peak_before = gate.peak_in_flight();

        // Well past the budget: none of these may hash.
        for _ in 0..50 {
            let err = gate
                .verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await
                .expect_err("over budget");
            assert_eq!(err.body.code, "TOO_MANY_PASSWORD_ATTEMPTS");
        }
        assert_eq!(
            gate.peak_in_flight(),
            peak_before,
            "a throttled attempt must be rejected before the verifier runs"
        );
    }

    /// A lockout must be scoped to one meeting and one address, or an attacker
    /// could deny an entire meeting — or the whole service — with five guesses.
    #[tokio::test]
    async fn throttle_is_scoped_to_the_ip_and_meeting_pair() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");

        for _ in 0..MAX_FAILED_PASSWORD_ATTEMPTS {
            let _ = gate
                .verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await;
        }
        assert_eq!(
            gate.verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await
                .expect_err("exhausted for this pair")
                .body
                .code,
            "TOO_MANY_PASSWORD_ATTEMPTS"
        );

        // Same address, different meeting → unaffected.
        assert_eq!(
            gate.verify(Some(IP), "other-meeting", Some(&stored), Some("wrong"))
                .await
                .expect_err("wrong password, but not throttled")
                .body
                .code,
            "INVALID_MEETING_PASSWORD"
        );

        // Different address, same meeting → unaffected. This is the property
        // that stops one attacker from locking a meeting for everybody.
        assert_eq!(
            gate.verify(Some(OTHER_IP), "m", Some(&stored), Some("wrong"))
                .await
                .expect_err("wrong password, but not throttled")
                .body
                .code,
            "INVALID_MEETING_PASSWORD"
        );
    }

    /// A correct password must not consume budget, so a legitimate joiner is
    /// never locked out by their own successful joins.
    #[tokio::test]
    async fn successful_verification_refunds_its_slot() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");

        for _ in 0..20 {
            gate.verify(Some(IP), "m", Some(&stored), Some("s3cret"))
                .await
                .expect("a correct password must never be throttled");
        }

        // Budget untouched, so a full run of failures is still available.
        for _ in 0..MAX_FAILED_PASSWORD_ATTEMPTS {
            let err = gate
                .verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await
                .expect_err("wrong password");
            assert_eq!(err.body.code, "INVALID_MEETING_PASSWORD");
        }
    }

    /// Without a usable address there is no bucket, and crucially no *shared*
    /// bucket — collapsing anonymous callers together would let one of them lock
    /// out all the others.
    #[tokio::test]
    async fn unidentified_callers_are_not_throttled_into_a_shared_bucket() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");

        for _ in 0..(MAX_FAILED_PASSWORD_ATTEMPTS * 4) {
            let err = gate
                .verify(None, "m", Some(&stored), Some("wrong"))
                .await
                .expect_err("wrong password");
            assert_eq!(
                err.body.code, "INVALID_MEETING_PASSWORD",
                "an unidentified caller must never get a 429 from a shared bucket"
            );
        }
    }

    #[tokio::test]
    async fn attempt_window_resets() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");

        for _ in 0..MAX_FAILED_PASSWORD_ATTEMPTS {
            let _ = gate
                .verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await;
        }
        assert_eq!(
            gate.verify(Some(IP), "m", Some(&stored), Some("wrong"))
                .await
                .expect_err("exhausted")
                .body
                .code,
            "TOO_MANY_PASSWORD_ATTEMPTS"
        );

        // Back-date the window rather than sleeping 60s.
        {
            let mut attempts = gate.failed_attempts.lock().expect("limiter mutex");
            let entry = attempts
                .get_mut(&(IP, "m".to_string()))
                .expect("entry exists after the failures above");
            entry.0 = Instant::now() - Duration::from_secs(PASSWORD_ATTEMPT_WINDOW_SECS + 1);
        }

        let err = gate
            .verify(Some(IP), "m", Some(&stored), Some("wrong"))
            .await
            .expect_err("wrong password");
        assert_eq!(
            err.body.code, "INVALID_MEETING_PASSWORD",
            "a stale window must reset rather than stay locked"
        );
    }

    /// The map is keyed partly on a client-supplied address, so it must not grow
    /// without bound when an attacker rotates addresses.
    #[tokio::test]
    async fn tracked_key_count_stays_bounded() {
        let gate = open_gate();
        let stored = hash_like_create_meeting("s3cret");

        // Drive the map through `consume_attempt` directly rather than hashing
        // 8700 times — the bound under test is the map's, not the verifier's.
        for i in 0..(MAX_TRACKED_ATTEMPT_KEYS + 500) {
            let ip = IpAddr::V4(Ipv4Addr::from((i as u32).to_be_bytes()));
            let _ = gate.consume_attempt(Some(ip), "m");
        }

        let tracked = gate.failed_attempts.lock().expect("limiter mutex").len();
        assert!(
            tracked <= MAX_TRACKED_ATTEMPT_KEYS,
            "limiter map must stay bounded, found {tracked} entries"
        );

        // And the gate still works for a normal caller once at capacity.
        gate.verify(Some(IP), "m", Some(&stored), Some("s3cret"))
            .await
            .expect("a correct password must still verify at limiter capacity");
    }

    // ── Client address resolution ────────────────────────────────────────

    fn headers_with_xff(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            value.parse().expect("valid header value"),
        );
        h
    }

    #[test]
    fn xff_uses_the_rightmost_entry_because_the_ingress_appends_it() {
        // Everything left of the final entry is client-supplied; only the last
        // one was written by our own nginx ingress.
        let headers = headers_with_xff("1.1.1.1, 2.2.2.2, 203.0.113.9");
        assert_eq!(
            client_ip_for_throttle(&headers, Some(OTHER_IP)),
            Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9)))
        );
    }

    #[test]
    fn forged_leading_xff_entries_cannot_change_the_throttle_key() {
        // An attacker rotating the left-hand entries must land in the same
        // bucket every time, or the throttle is decorative.
        let a = client_ip_for_throttle(&headers_with_xff("9.9.9.9, 203.0.113.9"), None);
        let b = client_ip_for_throttle(&headers_with_xff("8.8.8.8, 203.0.113.9"), None);
        let c = client_ip_for_throttle(&headers_with_xff("203.0.113.9"), None);
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(c, Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))));
    }

    #[test]
    fn falls_back_to_the_peer_address_without_the_header() {
        assert_eq!(
            client_ip_for_throttle(&HeaderMap::new(), Some(IP)),
            Some(IP)
        );
    }

    #[test]
    fn falls_back_to_the_peer_address_when_the_header_is_garbage() {
        assert_eq!(
            client_ip_for_throttle(&headers_with_xff("not-an-ip"), Some(IP)),
            Some(IP)
        );
        assert_eq!(
            client_ip_for_throttle(&headers_with_xff(""), Some(IP)),
            Some(IP)
        );
    }

    #[test]
    fn no_header_and_no_peer_yields_none() {
        assert_eq!(client_ip_for_throttle(&HeaderMap::new(), None), None);
    }

    #[test]
    fn ipv6_forwarded_entries_parse() {
        let headers = headers_with_xff("1.1.1.1, 2001:db8::1");
        assert_eq!(
            client_ip_for_throttle(&headers, None),
            Some("2001:db8::1".parse::<IpAddr>().expect("valid v6"))
        );
    }

    // ── Sizing invariants ────────────────────────────────────────────────

    #[test]
    fn default_gate_is_sized_within_the_permit_ceiling() {
        let gate = MeetingPasswordGate::new();
        assert!(
            gate.available_permits() >= 1,
            "must allow at least one verification"
        );
        assert!(
            gate.available_permits() <= MAX_ARGON2_PERMITS,
            "must never exceed the memory-derived ceiling"
        );
    }

    // ── Setting and clearing a password (issue #2207) ────────────────────

    /// Drive both halves of the production path — validate, then hash —
    /// and return the hash a `Set` came out carrying.
    async fn expect_set(plaintext: &str) -> String {
        let intent = parse_password_update(Some(plaintext), None).expect("a plain set must parse");
        match open_gate()
            .hash_intent(intent)
            .await
            .expect("hashing a parsed set")
        {
            PasswordUpdate::Set(hash) => hash,
            other => panic!("expected a Set update, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_password_set_by_patch_verifies_at_the_join_gate() {
        let hash = expect_set("correct horse battery staple").await;

        let gate = open_gate();
        assert!(
            gate.verify(
                Some(IP),
                "m",
                Some(&hash),
                Some("correct horse battery staple")
            )
            .await
            .is_ok(),
            "the password just set must verify"
        );
        assert!(
            gate.verify(Some(OTHER_IP), "m", Some(&hash), Some("something else"))
                .await
                .is_err(),
            "a different password must still be rejected"
        );
    }

    #[tokio::test]
    async fn hashing_the_same_password_twice_yields_different_hashes() {
        let first = expect_set("same").await;
        let second = expect_set("same").await;
        assert_ne!(first, second, "the salt must be per-hash, not fixed");
    }

    #[tokio::test]
    async fn a_set_update_carries_a_hash_not_the_plaintext() {
        const SECRET: &str = "hunter2-do-not-store-me";
        let hash = expect_set(SECRET).await;
        assert!(!hash.contains(SECRET), "the plaintext reached storage");
        assert!(hash.starts_with("$argon2"), "not a PHC string: {hash}");
    }

    #[tokio::test]
    async fn debug_never_prints_the_plaintext_or_the_stored_hash() {
        const SECRET: &str = "hunter2-do-not-log-me";
        let intent = parse_password_update(Some(SECRET), None).expect("a set");
        let parsed = format!("{intent:?}");
        assert_eq!(parsed, "Set(<redacted>)");

        let hashed = format!(
            "{:?}",
            open_gate().hash_intent(intent).await.expect("hashing")
        );
        assert_eq!(hashed, "Set(<hash>)");
    }

    #[tokio::test]
    async fn remove_password_resolves_to_clear() {
        let intent = parse_password_update(None, Some(true)).expect("a clear");
        assert!(intent.is_change(), "clearing changes the column");
        assert!(matches!(
            open_gate()
                .hash_intent(intent)
                .await
                .expect("clearing needs no hash"),
            PasswordUpdate::Clear
        ));
    }

    #[tokio::test]
    async fn a_body_without_password_fields_leaves_it_unchanged() {
        let intent = parse_password_update(None, None).expect("no password fields");
        assert!(!intent.is_change(), "an untouched password writes nothing");
        assert!(matches!(
            open_gate()
                .hash_intent(intent)
                .await
                .expect("no hashing needed"),
            PasswordUpdate::Unchanged
        ));

        let explicit_no = parse_password_update(None, Some(false)).expect("remove_password false");
        assert!(!explicit_no.is_change());
    }

    #[test]
    fn an_empty_password_is_rejected_not_read_as_clear() {
        let err = parse_password_update(Some(""), None).expect_err("empty must be rejected");
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn setting_and_removing_at_once_is_rejected() {
        let err = parse_password_update(Some("pw"), Some(true))
            .expect_err("contradictory body must be rejected");
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unusual_passwords_round_trip_through_the_hasher() {
        for plaintext in ["  padded  ", "☂ unicode ☂", "a", &"x".repeat(1024)] {
            let hash = expect_set(plaintext).await;
            let parsed = PasswordHash::new(&hash).expect("a parseable PHC string");
            assert!(
                Argon2::default()
                    .verify_password(plaintext.as_bytes(), &parsed)
                    .is_ok(),
                "the stored hash must verify against exactly what was sent"
            );
        }
    }
}
