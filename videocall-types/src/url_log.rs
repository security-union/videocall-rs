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

//! URL scrubbing helper for log statements.
//!
//! Lobby URLs carry the room JWT in `?token=<JWT>`. The client, the relay and
//! the headless tools all call [`strip_query_for_log`] from here, so they
//! cannot drift into subtly different redactions.

/// Strip the query string from a URL before logging it.
///
/// - URLs containing `?` are truncated at the first `?`.
/// - URLs without `?` are returned unchanged.
/// - Inputs that don't look like URLs (no `://`) collapse to an empty string,
///   so a malformed value can never accidentally print a token-bearing
///   fragment.
pub fn strip_query_for_log(url: &str) -> String {
    if !url.contains("://") {
        return String::new();
    }
    match url.find('?') {
        Some(i) => url[..i].to_string(),
        None => url.to_string(),
    }
}

/// `message` with every echo of `url`'s query string removed.
pub fn strip_url_query_from_message(message: &str, url: &str) -> String {
    match url.find('?') {
        Some(i) if i + 1 < url.len() => message.replace(&url[i..], ""),
        _ => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::strip_url_query_from_message;

    #[test]
    fn strips_the_echoed_query_from_a_constructor_error() {
        let url = "https://relay:99999/lobby?token=SECRETJWT";
        let message = "SyntaxError: Failed to construct 'WebTransport': \
                       The URL 'https://relay:99999/lobby?token=SECRETJWT' is invalid.";
        assert_eq!(
            strip_url_query_from_message(message, url),
            "SyntaxError: Failed to construct 'WebTransport': \
             The URL 'https://relay:99999/lobby' is invalid."
        );
    }

    #[test]
    fn leaves_the_message_alone_when_the_url_has_no_query() {
        let message = "The URL's scheme must be 'https'. 'http' is not allowed.";
        assert_eq!(
            strip_url_query_from_message(message, "http://relay/lobby"),
            message
        );
        let with_mark = "Is 'http://relay/lobby?' valid?";
        assert_eq!(
            strip_url_query_from_message(with_mark, "http://relay/lobby?"),
            with_mark
        );
    }
}
