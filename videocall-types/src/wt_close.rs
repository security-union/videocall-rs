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

//! WebTransport application close codes, shared so the relay that sends one
//! and the client that matches on it cannot drift apart silently.

/// Contract E5 (#2726). Sent by the relay's stage-2 shed escalation when it
/// cannot deliver this receiver's downlink; the client re-elects on it.
pub const WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE: u32 = 1001;

/// Trace and log text for [`WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE`]; the client matches on the CODE.
pub const WT_CLOSE_REASON_DOWNLINK_UNRECOVERABLE: &[u8] = b"downlink-shed-escalation";

const _: () = assert!(
    WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE >= 1000,
    "code 0 is the clean close every pre-existing relay path uses, so a \
     diagnostic code below 1000 reads as an ordinary hang-up"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_downlink_unrecoverable_close_code_is_1001() {
        assert_eq!(WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE, 1001);
        assert_eq!(
            WT_CLOSE_REASON_DOWNLINK_UNRECOVERABLE,
            b"downlink-shed-escalation"
        );
    }

    #[test]
    fn the_close_reason_is_printable_ascii() {
        assert_eq!(WT_CLOSE_REASON_DOWNLINK_UNRECOVERABLE.len(), 24);
        assert!(
            WT_CLOSE_REASON_DOWNLINK_UNRECOVERABLE
                .iter()
                .all(|byte| byte.is_ascii_graphic()),
            "a multi-byte or control byte reaches a QUIC trace as mojibake"
        );
        assert_eq!(
            std::str::from_utf8(WT_CLOSE_REASON_DOWNLINK_UNRECOVERABLE).unwrap(),
            "downlink-shed-escalation"
        );
    }
}
