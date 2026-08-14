/*
 * Copyright 2026 Security Union LLC
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

//! **TEST-ONLY** serial guards for tests that read or write PROCESS-GLOBAL
//! statics (#2160 / #2016).
//!
//! A `Mutex<()>` buys mutual exclusion **among the tests that take it** — nothing
//! more. The accessors are lock-free `load(Relaxed)`, so a concurrent test that
//! does not take the guard still observes whatever a holder stored. This is a
//! CONVENTION the compiler cannot enforce; correctness depends on every test that
//! cares about one of these statics opting in.
//!
//! Two guards rather than one because the groups are independent, so a single
//! guard would needlessly serialise tests that cannot corrupt each other:
//! [`lock_screen_encoder_stall_counters`] covers `screen_encoder`'s stall
//! statics, [`lock_transport_stream_counters`] the `videocall-transport` uplink
//! counters.
//!
//! **Lock order, if a test ever needs both:** screen-encoder stall counters
//! FIRST, transport stream counters SECOND. Opposite orders deadlock.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Serialises the PARTICIPATING tests that read or write the screen encoder's
/// process-global tick-starvation stall counters
/// (`SCREEN_ENCODER_STALL_EPISODES` / `SCREEN_ENCODER_MAX_STALL_GAP_MS`),
/// reached from tests via `encode::set_screen_encoder_stall_counters_for_test`.
static SCREEN_ENCODER_STALL_COUNTER_GUARD: Mutex<()> = Mutex::new(());

/// Serialises the PARTICIPATING tests that read or write `videocall-transport`'s
/// per-stream and aggregate uplink counters (WT ready-stall, WT drop, WS drop),
/// reached from tests via the `force_*_for_stream` helpers.
static TRANSPORT_STREAM_COUNTER_GUARD: Mutex<()> = Mutex::new(());

/// Exclude OTHER GUARD-TAKERS from the screen-encoder stall counters for the
/// duration of the returned guard.
///
/// Not exclusive access to the statics themselves — the accessors are lock-free,
/// so an unguarded concurrent test can still read them.
pub(crate) fn lock_screen_encoder_stall_counters() -> MutexGuard<'static, ()> {
    lock_ignoring_poison(&SCREEN_ENCODER_STALL_COUNTER_GUARD)
}

/// Exclude OTHER GUARD-TAKERS from `videocall-transport`'s per-stream/aggregate
/// uplink counters for the duration of the returned guard.
///
/// Same caveat as the sibling above: the counters' accessors are lock-free, so
/// this bounds guard-takers only.
pub(crate) fn lock_transport_stream_counters() -> MutexGuard<'static, ()> {
    lock_ignoring_poison(&TRANSPORT_STREAM_COUNTER_GUARD)
}

/// Lock a guard mutex, treating POISONING as benign.
///
/// These mutexes carry no state — the `()` payload cannot be left inconsistent
/// by a panic, and every participating test either stores ABSOLUTE values or
/// re-reads its own before/after baseline under the guard, so it is insulated
/// from whatever a panicking predecessor left in the statics. Unwrapping instead
/// would turn the FIRST guarded test failure into a cascade of confusing
/// `PoisonError` panics in its siblings, hiding the assertion that actually
/// broke — the opposite of what a test guard should do.
fn lock_ignoring_poison(m: &'static Mutex<()>) -> MutexGuard<'static, ()> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
