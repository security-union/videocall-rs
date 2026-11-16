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

//! The one source of "now" inside NetEq.
//!
//! In normal operation [`now`] is `web_time::Instant::now()`. A test can
//! freeze the clock on its own thread with [`freeze`] and move it forward
//! with [`advance`]; from then on [`now`] on that thread has no real-time
//! component at all, so a test can replay hours of network time in
//! milliseconds and get bit-identical results. The state is thread-local, so
//! tests running in parallel do not see each other's clocks.

use std::cell::Cell;
use web_time::{Duration, Instant};

thread_local! {
    /// `Some((base, offset))` once frozen on this thread.
    static FROZEN: Cell<Option<(Instant, Duration)>> = const { Cell::new(None) };
}

/// The current instant: real time, or the simulated time once [`freeze`]
/// was called on this thread.
#[inline]
pub fn now() -> Instant {
    match FROZEN.with(Cell::get) {
        Some((base, offset)) => base + offset,
        None => Instant::now(),
    }
}

/// Freeze the clock on the calling thread. The base is read from the real
/// clock once, here; only differences between instants are ever used, so
/// its value does not matter. Calling it again starts a fresh frozen clock
/// (create new NetEq instances after it).
#[doc(hidden)]
pub fn freeze() {
    let base = Instant::now();
    FROZEN.with(|frozen| frozen.set(Some((base, Duration::ZERO))));
}

/// Move the frozen clock on the calling thread forward by `step`.
///
/// # Panics
///
/// If the clock was not frozen on this thread.
#[doc(hidden)]
pub fn advance(step: Duration) {
    FROZEN.with(|frozen| {
        let (base, offset) = frozen
            .get()
            .expect("neteq::clock::advance called before neteq::clock::freeze");
        frozen.set(Some((base, offset + step)));
    });
}
