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

//! Server-rendered HTML: the escape helper, the page templates and the
//! security-headers layer.
//!
//! No script, no inline style, no external resource: the only subresource is
//! `/static/auth.css`, which the CSP allows as `style-src 'self'`. Every
//! dynamic value goes through [`html_escape`].

use std::net::IpAddr;

use crate::db::cli_auth::CliAuthRequestRow;
use crate::db::devices::DeviceRow;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};

/// The stylesheet, embedded at build time.
pub const AUTH_CSS: &str = include_str!("../static/auth.css");

/// Content-Security-Policy of every response.
pub const CSP: &str = "default-src 'none'; style-src 'self'; img-src 'self'; \
     form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Fixed phishing warning shown on every consent page. It names the one
/// safe situation (you just ran the command yourself) and the attack (a
/// link someone else sent you).
pub const WARNING_COPY: &str = "Only approve if you just ran videocall-cli auth login yourself, \
     on a device you own. If someone sent you this link or asked you to open it, choose Deny: \
     approving lets that device join meetings as you.";

/// The CSP for a consent page whose form ends in a redirect to the CLI's
/// loopback listener. CSP `form-action` also governs redirects that follow
/// a form submission, so the exact callback URL is added for this response
/// only. `callback` is rebuilt from validated, stored fields.
pub fn csp_with_form_target(callback: &str) -> String {
    format!(
        "default-src 'none'; style-src 'self'; img-src 'self'; \
         form-action 'self' {callback}; frame-ancestors 'none'; base-uri 'none'"
    )
}

/// Escape `& < > " '` for HTML text and attribute values.
pub fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Referrer policy for every page, sent as the header and as a `<meta>`.
///
/// `same-origin`, not `no-referrer`: under `no-referrer` the Fetch spec
/// serializes `Origin` as `null` on non-GET requests, so every form POST
/// (consent, revoke, sign-out) would arrive with `Origin: null` and fail the
/// exact-Origin CSRF check. `same-origin` keeps the real Origin on our own
/// POSTs and still sends no Referer on any cross-origin navigation,
/// including the 303 to the CLI's `127.0.0.1` callback.
pub const REFERRER_POLICY: &str = "same-origin";

/// Add the security headers to a response. Headers a handler already set
/// (e.g. the stylesheet's `Cache-Control`) are kept.
pub async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    let fixed: [(HeaderName, &'static str); 6] = [
        (header::CONTENT_SECURITY_POLICY, CSP),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, REFERRER_POLICY),
        (
            HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (header::CACHE_CONTROL, "no-store"),
    ];
    for (name, value) in fixed {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    response
}

/// A complete HTML document around `body`.
fn document(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"color-scheme\" content=\"light dark\">\n\
         <meta name=\"referrer\" content=\"{referrer}\">\n\
         <title>{title} - videocall.rs</title>\n\
         <link rel=\"stylesheet\" href=\"/static/auth.css\">\n\
         </head>\n<body>\n<main class=\"page\">\n{body}\n</main>\n</body>\n</html>\n",
        title = html_escape(title),
        referrer = REFERRER_POLICY,
    )
}

/// An HTML response with the given status.
pub fn html(status: StatusCode, title: &str, body: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        document(title, body),
    )
        .into_response()
}

/// A one-message page: heading, paragraph, optional link.
fn message(status: StatusCode, title: &str, text: &str, link: Option<(&str, &str)>) -> Response {
    let link = link
        .map(|(href, label)| {
            format!(
                "<p><a class=\"button button-secondary\" href=\"{}\">{}</a></p>",
                html_escape(href),
                html_escape(label)
            )
        })
        .unwrap_or_default();
    let body = format!(
        "<section class=\"card\">\n<h1>{}</h1>\n<p>{}</p>\n{link}\n</section>",
        html_escape(title),
        html_escape(text)
    );
    html(status, title, &body)
}

/// Everything the consent page shows.
pub struct ConfirmView<'a> {
    pub viewer_name: &'a str,
    pub viewer_user_id: &'a str,
    pub request: &'a CliAuthRequestRow,
    pub nonce: &'a str,
    pub fingerprint: &'a str,
    pub now: DateTime<Utc>,
}

fn minutes_ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match (now - then).num_minutes() {
        m if m < 1 => "less than a minute ago".to_string(),
        1 => "1 minute ago".to_string(),
        m => format!("{m} minutes ago"),
    }
}

/// The consent page for one CLI login request.
pub fn confirm_page(view: &ConfirmView<'_>) -> Response {
    // No "request came from <IP>" line: the address is the browser that
    // opened the link, which in a phishing attempt is the victim's own, so
    // it would only reassure them.
    let request = view.request;
    let (kind_text, checkbox) = if request.is_remote() {
        (
            "Remote sign-in: after you approve, this page shows a code. You paste it into the \
             terminal where you ran videocall-cli auth login --remote.",
            "<div class=\"check\">\n\
             <input id=\"confirm\" name=\"confirm\" type=\"checkbox\" value=\"yes\">\n\
             <label for=\"confirm\">I started this sign-in myself, on a device I own.</label>\n\
             </div>\n",
        )
    } else {
        (
            "Sign-in on this computer: after you approve, your browser hands the result to \
             videocall-cli running on this same machine.",
            "",
        )
    };
    let body = format!(
        "<section class=\"card\">\n\
         <h1>Sign in videocall-cli?</h1>\n\
         {identity}\
         <dl class=\"facts\">\n\
         <dt>Device name</dt><dd>{name}</dd>\n\
         <dt>Key fingerprint</dt><dd><span class=\"mono fingerprint\">{fingerprint}</span> \
         <span class=\"hint\">Check this matches your terminal.</span></dd>\n\
         <dt>Kind</dt><dd>{kind_text}</dd>\n\
         <dt>Requested</dt><dd>{age}</dd>\n\
         </dl>\n\
         <p class=\"notice notice-danger\" role=\"note\">{warning}</p>\n\
         <form method=\"post\" action=\"/cli/consent\" class=\"stack\">\n\
         <input type=\"hidden\" name=\"request\" value=\"{request_id}\">\n\
         <input type=\"hidden\" name=\"nonce\" value=\"{nonce}\">\n\
         {checkbox}\
         <div class=\"actions\">\n\
         <button type=\"submit\" name=\"decision\" value=\"deny\" class=\"button button-secondary\">Deny</button>\n\
         <button type=\"submit\" name=\"decision\" value=\"approve\" class=\"button button-primary\">Approve</button>\n\
         </div>\n\
         </form>\n\
         </section>",
        identity = identity_bar(
            &format!(
                "<strong>{}</strong> ({}).",
                html_escape(view.viewer_name),
                html_escape(view.viewer_user_id)
            ),
            Some(&request.id),
            "Not you? Sign out"
        ),
        name = html_escape(&request.device_name),
        fingerprint = html_escape(view.fingerprint),
        kind_text = html_escape(kind_text),
        age = minutes_ago(request.created_at, view.now),
        request_id = html_escape(&request.id),
        nonce = html_escape(view.nonce),
        warning = html_escape(WARNING_COPY),
    );
    html(StatusCode::OK, "Sign in videocall-cli?", &body)
}

/// "Signed in as …" with the sign-out form beside it: the same place and the
/// same plain style on the consent page and on /devices. `who` is HTML
/// that the caller has already escaped.
fn identity_bar(who: &str, request_id: Option<&str>, sign_out_label: &str) -> String {
    format!(
        "<div class=\"identity\">\n\
         <p class=\"signed-in\">Signed in as {who}</p>\n\
         {form}\
         </div>\n",
        form = sign_out_form(request_id, sign_out_label),
    )
}

/// A POST form to `/cli/sign-out`. With `request_id` the "sign in again"
/// link afterwards leads back to that consent page, otherwise to /devices.
fn sign_out_form(request_id: Option<&str>, label: &str) -> String {
    let hidden = request_id
        .map(|id| {
            format!(
                "<input type=\"hidden\" name=\"request\" value=\"{}\">\n",
                html_escape(id)
            )
        })
        .unwrap_or_default();
    format!(
        "<form method=\"post\" action=\"/cli/sign-out\" class=\"sign-out\">\n\
         {hidden}<button type=\"submit\" class=\"button button-plain\">{label}</button>\n\
         </form>\n",
        label = html_escape(label),
    )
}

/// After `/cli/sign-out`: the session cookie is cleared. `sign_in_url` is
/// a `/login` link back to the page the user came from.
pub fn signed_out_by_user_page(sign_in_url: &str) -> Response {
    let body = format!(
        "<section class=\"card\">\n\
         <h1>Signed out</h1>\n\
         <p class=\"lead\">You are signed out on this browser.</p>\n\
         <p><a class=\"button button-primary\" href=\"{href}\">Sign in again</a></p>\n\
         </section>",
        href = html_escape(sign_in_url),
    );
    html(StatusCode::OK, "Signed out", &body)
}

/// The remote-login code, shown exactly once in the POST response.
pub fn code_page(code: &str, expires_in_secs: i64) -> Response {
    let minutes = (expires_in_secs / 60).max(1);
    let body = format!(
        "<section class=\"card\">\n\
         <h1>Your sign-in code</h1>\n\
         <label for=\"code\">Code</label>\n\
         <textarea id=\"code\" class=\"code-field\" readonly rows=\"2\" \
         autocomplete=\"off\" spellcheck=\"false\" aria-describedby=\"code-hint\">{code}</textarea>\n\
         <p class=\"hint\" id=\"code-hint\">Paste this into the terminal where you ran \
         videocall-cli auth login --remote. Never share it with anyone or paste it anywhere else. \
         It works once and expires in {minutes} minutes.</p>\n\
         </section>",
        code = html_escape(code),
    );
    html(StatusCode::OK, "Your sign-in code", &body)
}

/// `GET /cli/code`: the remote `redirect_uri` itself never carries a code.
pub fn code_info_page() -> Response {
    message(
        StatusCode::OK,
        "Sign-in code",
        "A sign-in code is shown only once, right after you approve a remote sign-in. \
         Run videocall-cli auth login --remote again to get a new one.",
        None,
    )
}

pub fn denied_page() -> Response {
    message(
        StatusCode::OK,
        "Request denied",
        "Request denied. The device was not signed in.",
        None,
    )
}

pub fn invalid_page() -> Response {
    message(
        StatusCode::BAD_REQUEST,
        "Request not valid",
        "This sign-in request is invalid, expired or already used. Run videocall-cli auth login again.",
        None,
    )
}

/// `/cli/authorize` with bad parameters. Never redirects.
pub fn authorize_error_page() -> Response {
    message(
        StatusCode::BAD_REQUEST,
        "Sign-in link not valid",
        "This sign-in link is not valid. Run videocall-cli auth login again and use the link it prints.",
        None,
    )
}

pub fn key_registered_page() -> Response {
    message(
        StatusCode::CONFLICT,
        "Key already registered",
        "This device key is already registered. Run videocall-cli auth login again to make a new one.",
        None,
    )
}

pub fn busy_page() -> Response {
    message(
        StatusCode::TOO_MANY_REQUESTS,
        "Too many sign-ins",
        "Too many sign-ins are waiting. Try again in a few minutes.",
        None,
    )
}

pub fn confirm_required_page(request_id: &str) -> Response {
    let href = format!("/cli/consent?request={request_id}");
    message(
        StatusCode::BAD_REQUEST,
        "Confirmation needed",
        "To approve, tick \"I started this sign-in myself, on a device I own.\" Nothing was changed.",
        Some((&href, "Back to the request")),
    )
}

pub fn too_many_devices_page(count: i64) -> Response {
    let text = format!("You already have {count} devices. Revoke one at /devices first.");
    message(
        StatusCode::CONFLICT,
        "Too many devices",
        &text,
        Some(("/devices", "Your devices")),
    )
}

pub fn signed_out_page() -> Response {
    message(
        StatusCode::UNAUTHORIZED,
        "You were signed out",
        "You were signed out. Run videocall-cli auth login again to start over.",
        None,
    )
}

pub fn rejected_page() -> Response {
    message(
        StatusCode::FORBIDDEN,
        "Request rejected",
        "This form was not submitted from this site, so it was ignored.",
        None,
    )
}

pub fn not_found_page() -> Response {
    message(
        StatusCode::NOT_FOUND,
        "Not found",
        "There is nothing here, or it is not yours.",
        Some(("/devices", "Your devices")),
    )
}

/// A signed-out visitor on a server with no OAuth provider configured.
pub fn sign_in_unavailable_page() -> Response {
    message(
        StatusCode::SERVICE_UNAVAILABLE,
        "Sign-in not available",
        "This server has no sign-in configured, so it cannot approve devices.",
        None,
    )
}

pub fn not_configured_page() -> Response {
    message(
        StatusCode::SERVICE_UNAVAILABLE,
        "Not available",
        "Device sign-in is not configured on this server.",
        None,
    )
}

pub fn error_page(status: StatusCode) -> Response {
    message(
        status,
        "Something went wrong",
        "We could not complete this request. Try again in a moment.",
        None,
    )
}

fn day(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d").to_string()
}

/// The address a device was approved from, if it tells the user anything.
/// Addresses that belong to infrastructure (loopback, private, CGNAT,
/// link-local, ULA; the same classification as the per-client cap) are
/// the ingress or a proxy, not the user's network, so they are not shown.
fn approved_from(ip: Option<&str>) -> Option<IpAddr> {
    let ip = ip?.parse::<IpAddr>().ok()?;
    crate::routes::device_pages::cap_key(Some(ip)).map(|_| ip)
}

/// An address as markup that may break only between IPv6 groups: each
/// `xxxx:` group is its own inline-block span (Chrome breaks at `<wbr>`
/// even under `white-space: nowrap`, so that cannot be used). An IPv4
/// address is one piece.
fn address_groups(address: &str) -> String {
    if !address.contains(':') {
        return html_escape(address);
    }
    let groups: String = address
        .split_inclusive(':')
        .map(|group| {
            format!(
                "<span class=\"address-group\">{}</span>",
                html_escape(group)
            )
        })
        .collect();
    // One wrapper, so the stacked layout's flex row sees a single value.
    format!("<span class=\"address-groups\">{groups}</span>")
}

/// The signed-in user's live devices, each with a Revoke form.
///
/// A real table (row header = device name, column headers for each value),
/// so it reads correctly with CSS off and to a screen reader. Below the wide
/// breakpoint the CSS restacks each row as a card; the explicit ARIA roles
/// keep the table semantics in browsers that drop them when a table's
/// `display` changes.
pub fn devices_page(viewer_name: &str, devices: &[DeviceRow]) -> Response {
    let addresses: Vec<Option<IpAddr>> = devices
        .iter()
        .map(|d| approved_from(d.requester_ip.as_deref()))
        .collect();
    let show_addresses = addresses.iter().any(Option::is_some);
    let rows: String = devices
        .iter()
        .zip(&addresses)
        .map(|(d, address)| {
            let address_cell = match (show_addresses, address) {
                (false, _) => String::new(),
                (true, Some(ip)) => format!(
                    "<td role=\"cell\" data-label=\"Approved from\" class=\"value mono address\">{}</td>\n",
                    address_groups(&ip.to_string())
                ),
                (true, None) => "<td role=\"cell\" data-label=\"Approved from\" class=\"value\">\
                     <span aria-hidden=\"true\">-</span>\
                     <span class=\"visually-hidden\">Not shown</span></td>\n"
                    .to_string(),
            };
            format!(
                "<tr role=\"row\">\n\
                 <th role=\"rowheader\" scope=\"row\" class=\"device-name\">{name}</th>\n\
                 <td role=\"cell\" data-label=\"Fingerprint\" class=\"value mono\">{fingerprint}</td>\n\
                 <td role=\"cell\" data-label=\"Approved\" class=\"value date\">{approved}</td>\n\
                 <td role=\"cell\" data-label=\"Last used\" class=\"value date\">{used}</td>\n\
                 <td role=\"cell\" data-label=\"Expires\" class=\"value date\">{expires}</td>\n\
                 {address_cell}\
                 <td role=\"cell\" class=\"row-action\"><form method=\"post\" action=\"/devices/{id}/revoke\">\
                 <button type=\"submit\" class=\"button button-danger\" \
                 aria-label=\"Revoke {name}\">Revoke</button></form></td>\n\
                 </tr>",
                name = html_escape(&d.name),
                fingerprint = html_escape(&d.fingerprint),
                approved = day(d.decided_at),
                used = day(d.last_used_at),
                expires = day(d.idle_expires_at.min(d.abs_expires_at)),
                id = html_escape(&d.id),
            )
        })
        .collect();
    let address_header = if show_addresses {
        "<th role=\"columnheader\" scope=\"col\">Approved from</th>"
    } else {
        ""
    };
    let content = if devices.is_empty() {
        "<p class=\"empty\">No devices are signed in.</p>".to_string()
    } else {
        format!(
            "<div class=\"table-wrap\">\n<table class=\"devices\" role=\"table\">\n\
             <caption class=\"visually-hidden\">Signed-in devices</caption>\n\
             <thead role=\"rowgroup\"><tr role=\"row\">\
             <th role=\"columnheader\" scope=\"col\">Name</th>\
             <th role=\"columnheader\" scope=\"col\">Fingerprint</th>\
             <th role=\"columnheader\" scope=\"col\">Approved</th>\
             <th role=\"columnheader\" scope=\"col\">Last used</th>\
             <th role=\"columnheader\" scope=\"col\">Expires</th>{address_header}\
             <th role=\"columnheader\" scope=\"col\"><span class=\"visually-hidden\">Actions</span></th>\
             </tr></thead>\n\
             <tbody role=\"rowgroup\">\n{rows}\n</tbody>\n</table>\n</div>"
        )
    };
    let body = format!(
        "<section class=\"card card-wide\">\n\
         <h1>Your devices</h1>\n\
         {identity}\
         <p class=\"lead\">These devices can join meetings as you. \
         Revoke any you do not recognise.</p>\n\
         {content}\n\
         </section>",
        identity = identity_bar(
            &format!("<strong>{}</strong>.", html_escape(viewer_name)),
            None,
            "Sign out"
        ),
    );
    html(StatusCode::OK, "Your devices", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_all_five_characters() {
        assert_eq!(
            html_escape("<script>alert(\"x\" & 'y')</script>"),
            "&lt;script&gt;alert(&quot;x&quot; &amp; &#x27;y&#x27;)&lt;/script&gt;"
        );
        assert_eq!(html_escape("plain"), "plain");
    }

    #[test]
    fn approved_from_shows_only_public_parsed_addresses() {
        let shown = |ip: Option<&str>| approved_from(ip).map(|ip| ip.to_string());
        assert_eq!(shown(Some("203.0.113.7")).as_deref(), Some("203.0.113.7"));
        assert_eq!(
            shown(Some("2001:db8:1:2::9")).as_deref(),
            Some("2001:db8:1:2::9")
        );
        for hidden in [
            Some("10.244.0.85"),
            Some("127.0.0.1"),
            Some("100.64.0.1"),
            Some("169.254.1.1"),
            Some("fd00::1"),
            Some("::1"),
            Some("<b>evil</b>"),
            None,
        ] {
            assert_eq!(shown(hidden), None, "{hidden:?}");
        }
    }
}
