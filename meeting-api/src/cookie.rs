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

//! Session cookie helpers shared by OAuth, dev auto-login, and middleware.

/// Build a `Set-Cookie` header value for the session JWT.
///
/// Attributes match OWASP session-cookie guidance: `HttpOnly` (JavaScript
/// cannot read it), `SameSite=Lax` (CSRF mitigation), `Secure` when requested
/// (HTTPS-only transmission), and `Max-Age` for explicit browser expiry.
pub(crate) fn build_session_cookie(
    name: &str,
    jwt: &str,
    ttl_secs: i64,
    domain: Option<&str>,
    secure: bool,
) -> String {
    let mut cookie = format!("{name}={jwt}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl_secs}");
    if secure {
        cookie.push_str("; Secure");
    }
    if let Some(d) = domain {
        cookie.push_str(&format!("; Domain={d}"));
    }
    cookie
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cookie_contains_name_and_jwt() {
        let cookie = build_session_cookie("session", "my.jwt.token", 3600, None, false);
        assert!(cookie.starts_with("session=my.jwt.token;"));
    }

    #[test]
    fn session_cookie_custom_name() {
        let cookie = build_session_cookie("pr1-session", "my.jwt.token", 3600, None, false);
        assert!(cookie.starts_with("pr1-session=my.jwt.token;"));
        // Must not be mistakable for a plain "session=" cookie.
        assert!(!cookie.starts_with("session="));
    }

    #[test]
    fn session_cookie_includes_required_attributes() {
        let cookie = build_session_cookie("session", "tok", 3600, None, false);
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=3600"));
    }

    #[test]
    fn session_cookie_secure_flag_added_when_true() {
        let cookie = build_session_cookie("session", "tok", 3600, None, true);
        assert!(cookie.contains("; Secure"));
    }

    #[test]
    fn session_cookie_no_secure_flag_when_false() {
        let cookie = build_session_cookie("session", "tok", 3600, None, false);
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn session_cookie_domain_appended() {
        let cookie =
            build_session_cookie("session", "tok", 3600, Some(".sandbox.videocall.rs"), false);
        assert!(cookie.contains("Domain=.sandbox.videocall.rs"));
    }

    #[test]
    fn session_cookie_no_domain_when_none() {
        let cookie = build_session_cookie("session", "tok", 3600, None, false);
        assert!(!cookie.contains("Domain="));
    }

    #[test]
    fn dev_session_cookie_format() {
        let cookie = build_session_cookie("session", "my.jwt.tok", 3600, None, false);
        assert!(cookie.starts_with("session=my.jwt.tok;"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(!cookie.contains("Secure"));
        assert!(!cookie.contains("Domain="));
    }

    #[test]
    fn dev_session_cookie_with_secure_and_domain() {
        let cookie = build_session_cookie("session", "tok", 3600, Some(".example.com"), true);
        assert!(cookie.contains("; Secure"));
        assert!(cookie.contains("Domain=.example.com"));
    }
}
