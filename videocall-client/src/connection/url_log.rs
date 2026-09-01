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

//! URL scrubbing helpers for log statements.
//!
//! In WASM the `info!`/`debug!`/`error!` macros write to the browser console
//! at the corresponding console levels. Lobby URLs in this client carry the
//! room JWT in `?token=<JWT>` — printing those URLs verbatim leaks the token
//! to anyone with DevTools, anyone watching a screen-share, and any
//! error-capture library that scoops up console output (Sentry, Datadog RUM,
//! etc.).
//!
//! The implementation lives in `videocall_types::url_log` because the relay
//! logs connection URLs too and must redact them identically. Use it at every
//! site that logs a connection URL — see PR #570 (Phase 1) for the
//! diagnostic-bus fix and security-audit follow-up F2 for the log-scrubbing
//! context.

pub(crate) use videocall_types::url_log::strip_query_for_log;

#[cfg(test)]
mod tests {
    use super::strip_query_for_log;

    #[test]
    fn strips_token_query_string() {
        let input = "https://relay.example.com/lobby/room-1?token=eyJhbGciOiJIUzI1NiJ9.payload.sig";
        let out = strip_query_for_log(input);
        assert_eq!(out, "https://relay.example.com/lobby/room-1");
        assert!(!out.contains("eyJ"));
        assert!(!out.contains("token"));
        assert!(!out.contains('?'));
    }

    #[test]
    fn strips_query_with_multiple_params() {
        let input = "wss://relay.example.com/lobby/room-1?token=secret&foo=bar&baz=qux";
        let out = strip_query_for_log(input);
        assert_eq!(out, "wss://relay.example.com/lobby/room-1");
        assert!(!out.contains("secret"));
    }

    #[test]
    fn passthrough_when_no_query() {
        let input = "https://relay.example.com/lobby/room-1";
        assert_eq!(strip_query_for_log(input), input);
    }

    #[test]
    fn passthrough_websocket_scheme_no_query() {
        let input = "wss://relay.example.com/lobby/room-1";
        assert_eq!(strip_query_for_log(input), input);
    }

    #[test]
    fn passthrough_webtransport_scheme_no_query() {
        let input = "https://relay.example.com:4433/lobby/room-1";
        assert_eq!(strip_query_for_log(input), input);
    }

    #[test]
    fn empty_string_returns_empty() {
        assert_eq!(strip_query_for_log(""), "");
    }

    #[test]
    fn malformed_input_returns_empty() {
        // No `://` — anything could be in here, refuse to print.
        assert_eq!(strip_query_for_log("not-a-url"), "");
        assert_eq!(strip_query_for_log("relay.example.com/lobby?token=x"), "");
        assert_eq!(strip_query_for_log("?token=eyJhbGc"), "");
    }

    #[test]
    fn only_first_question_mark_terminates() {
        // Defensive: ensure we use `find('?')` (first match), so a `?` inside
        // the truncated query string can't smuggle the rest back.
        let input = "https://r.example.com/path?a=1?b=2";
        assert_eq!(strip_query_for_log(input), "https://r.example.com/path");
    }

    #[test]
    fn empty_query_string_is_dropped() {
        // `?` with nothing after it is still stripped.
        let input = "https://relay.example.com/lobby/room-1?";
        assert_eq!(
            strip_query_for_log(input),
            "https://relay.example.com/lobby/room-1"
        );
    }
}
