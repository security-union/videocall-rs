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

//! Downlink unistream timing, shared so the relay that sheds a parked stream
//! and the client that must not provoke that shed cannot drift apart.

/// How long the relay tolerates a parked downlink unistream before it sheds it
/// (#1638). The client's inbound reader hand-over grace is HALF this, so a
/// queued stream can spend its whole grace without provoking the shed.
pub const WT_UNISTREAM_WRITE_DEADLINE_MS: u64 = 1000;

const _: () = assert!(
    WT_UNISTREAM_WRITE_DEADLINE_MS.is_multiple_of(2),
    "the client halves this; an odd value rounds its grace down and the two \
     stop being exactly half of each other"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unistream_write_deadline_is_1000ms() {
        assert_eq!(WT_UNISTREAM_WRITE_DEADLINE_MS, 1000);
    }
}
