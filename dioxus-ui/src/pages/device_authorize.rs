// SPDX-License-Identifier: MIT OR Apache-2.0

//! The page never navigates to a URL from the query; a remote code lives in
//! component state only.

use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use videocall_meeting_client::ApiError;
use videocall_meeting_types::device::{
    decode_authorize_request, decode_public_key, fingerprint, is_valid_client_state,
    is_valid_code_challenge, normalize_device_name, parse_redirect_uri,
    CreateDeviceAuthorizationRequest, CreateDeviceAuthorizationResponse, RedirectUri,
    CLI_CLIENT_ID, CODE_CHALLENGE_METHOD_S256, KEY_ALREADY_REGISTERED, NOT_CONFIGURED,
    TOO_MANY_DEVICES,
};
use videocall_meeting_types::responses::ProfileResponse;
use videocall_theme::{
    icons, Button, ButtonVariant, Card, Field, Icon, ListFooter, ListGroup, ListHeader, ListRow,
    ListRowLabel, ListRowValue, TextField, Toolbar,
};

use crate::auth::logout;
use crate::constants::meeting_api_client;
use crate::pages::devices::{redirect_to_login, BackHome, Busy, Notice};
use crate::routing::Route;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizeRequest {
    pub redirect: RedirectUri,
    /// Computed from `public_key`, never read from the query.
    pub fingerprint: String,
    pub body: CreateDeviceAuthorizationRequest,
}

/// `None` for anything the API would reject, and for a repeated parameter.
pub fn parse_authorize_query(query: &str) -> Option<AuthorizeRequest> {
    let url = reqwest::Url::parse(&format!("http://query.invalid/?{query}")).ok()?;
    let mut pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    pairs.sort();
    if pairs.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return None;
    }
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };

    if get("response_type")? != "code"
        || get("client_id")? != CLI_CLIENT_ID
        || get("code_challenge_method")? != CODE_CHALLENGE_METHOD_S256
    {
        return None;
    }
    let code_challenge = get("code_challenge").filter(|c| is_valid_code_challenge(c))?;
    let redirect = parse_redirect_uri(get("redirect_uri")?)?;
    let state = match redirect {
        RedirectUri::Loopback { .. } => get("state").filter(|s| is_valid_client_state(s))?,
        RedirectUri::Remote => "",
    };
    let public_key = get("public_key")?;
    let raw_key = decode_public_key(public_key)?;

    Some(AuthorizeRequest {
        redirect,
        fingerprint: fingerprint(&raw_key),
        body: CreateDeviceAuthorizationRequest {
            redirect_uri: redirect.as_uri(),
            code_challenge: code_challenge.to_string(),
            code_challenge_method: CODE_CHALLENGE_METHOD_S256.to_string(),
            state: state.to_string(),
            public_key: public_key.to_string(),
            device_name: normalize_device_name(get("device_name")?)?,
            confirm: false,
        },
    })
}

/// The loopback Deny URL, built from the parsed port.
pub fn loopback_denied_url(port: u16, state: &str) -> String {
    format!(
        "http://127.0.0.1:{port}/callback?error=access_denied&state={}",
        urlencoding::encode(state)
    )
}

/// The server's `redirect_to`, only if it goes to the CLI's own listener.
pub fn checked_redirect_to(port: u16, redirect_to: &str) -> Option<&str> {
    redirect_to
        .starts_with(&format!("http://127.0.0.1:{port}/callback?"))
        .then_some(redirect_to)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    InvalidLink,
    ProfileFailed,
    Denied,
    TooManyDevices,
    KeyAlreadyRegistered,
    NotConfigured,
    SessionExpired,
    ApproveFailed,
}

impl Outcome {
    fn from_api(error: &ApiError) -> Self {
        match error {
            ApiError::NotAuthenticated => Self::SessionExpired,
            ApiError::Device { error, .. } => match error.as_str() {
                TOO_MANY_DEVICES => Self::TooManyDevices,
                KEY_ALREADY_REGISTERED => Self::KeyAlreadyRegistered,
                NOT_CONFIGURED => Self::NotConfigured,
                _ => Self::ApproveFailed,
            },
            _ => Self::ApproveFailed,
        }
    }

    fn text(self) -> (&'static str, &'static str) {
        match self {
            Self::InvalidLink => ("This sign-in link is not valid", "Nothing was approved. Run the sign-in again in your terminal and use the link it gives you."),
            Self::ProfileFailed => ("Could not load your account", "Nothing was approved. Reload the page to try again."),
            Self::Denied => ("Denied", "Nothing was approved. You can close this page."),
            Self::TooManyDevices => ("Too many devices", "Your account already has the most devices it can have. Revoke one you no longer use, then run the sign-in again."),
            Self::KeyAlreadyRegistered => ("Device already signed in", "This device key is already approved on an account. Nothing new was approved."),
            Self::NotConfigured => ("Device sign-in is not available", "This server is not set up for device sign-in. Nothing was approved."),
            Self::SessionExpired => ("Your session ended", "Sign in again, then approve the device. Nothing was approved."),
            Self::ApproveFailed => ("Could not approve the device", "Nothing was approved. Try again, or run the sign-in again in your terminal."),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Busy(&'static str),
    Confirm,
    Submitting,
    Code { code: String, expires_in: i64 },
    Done(Outcome),
}

/// `request` is base64url so the router's percent-decoding cannot change it.
#[component]
pub fn DeviceAuthorizePage(request: String) -> Element {
    let inner = match decode_authorize_request(&request)
        .as_deref()
        .and_then(parse_authorize_query)
    {
        Some(request) => rsx! { DeviceConsent { request } },
        None => rsx! { OutcomeView { outcome: Outcome::InvalidLink, on_retry: |_| {} } },
    };
    rsx! {
        div { class: "surface-grouped settings-page devices-page",
            div { class: "settings-container",
                Toolbar { header: true, class: "settings-toolbar",
                    h1 { class: "toolbar-title", "Approve a device" }
                }
                {inner}
            }
        }
    }
}

#[component]
fn DeviceConsent(request: AuthorizeRequest) -> Element {
    let mut phase = use_signal(|| Phase::Busy("Checking who is signed in..."));
    let mut profile = use_signal(|| None::<ProfileResponse>);
    let mut confirmed = use_signal(|| false);
    let is_remote = request.redirect == RedirectUri::Remote;
    let port = request.redirect.loopback_port();

    use_effect(move || {
        spawn(async move {
            let result = match meeting_api_client() {
                Ok(client) => client.get_profile().await,
                Err(e) => Err(ApiError::Config(e)),
            };
            match result {
                Ok(user) => {
                    profile.set(Some(user));
                    phase.set(Phase::Confirm);
                }
                Err(ApiError::NotAuthenticated) => redirect_to_login(),
                Err(e) => {
                    // Info, not error: warn and above go to Matomo with the page URL.
                    log::info!("device authorize: profile failed: {e}");
                    phase.set(Phase::Done(Outcome::ProfileFailed));
                }
            }
        });
    });

    let mut leave_for = move |url: &str| {
        phase.set(Phase::Busy("Returning to your terminal..."));
        if let Some(window) = web_sys::window() {
            let _ = window.location().assign(url);
        }
    };

    let body = request.body.clone();
    let on_approve = move |evt: MouseEvent| {
        // A click a script dispatched is not consent.
        let trusted = evt.try_as_web_event().is_some_and(|e| e.is_trusted());
        if !trusted || phase() != Phase::Confirm || (is_remote && !confirmed()) {
            return;
        }
        phase.set(Phase::Submitting);
        let body = CreateDeviceAuthorizationRequest {
            confirm: confirmed(),
            ..body.clone()
        };
        spawn(async move {
            let result = match meeting_api_client() {
                Ok(client) => client.authorize_device(&body).await,
                Err(e) => Err(ApiError::Config(e)),
            };
            match result {
                Ok(CreateDeviceAuthorizationResponse::Loopback { redirect_to }) => {
                    match port.and_then(|port| checked_redirect_to(port, &redirect_to)) {
                        Some(target) => leave_for(target),
                        None => phase.set(Phase::Done(Outcome::ApproveFailed)),
                    }
                }
                Ok(CreateDeviceAuthorizationResponse::Remote { code, expires_in }) if is_remote => {
                    phase.set(Phase::Code { code, expires_in });
                }
                Ok(_) => phase.set(Phase::Done(Outcome::ApproveFailed)),
                Err(e) => {
                    log::info!("device authorize: approve failed: {e}");
                    phase.set(Phase::Done(Outcome::from_api(&e)));
                }
            }
        });
    };

    let state = request.body.state.clone();
    let on_deny = move |_| match port {
        Some(port) => leave_for(&loopback_denied_url(port, &state)),
        None => phase.set(Phase::Done(Outcome::Denied)),
    };

    let submitting = match phase() {
        Phase::Busy(label) => return rsx! { Busy { label } },
        Phase::Code { code, expires_in } => return rsx! { RemoteCode { code, expires_in } },
        Phase::Done(outcome) => {
            return rsx! { OutcomeView { outcome, on_retry: move |_| phase.set(Phase::Confirm) } }
        }
        Phase::Confirm => false,
        Phase::Submitting => true,
    };
    // Approve exists only once the profile has loaded.
    let Some(user) = profile() else {
        return rsx! {};
    };
    rsx! {
        section { class: "settings-group",
            ListHeader { "Your account" }
            ListGroup {
                ListRow {
                    ListRowLabel { "data-testid": "device-authorize-account",
                        "Signed in as {user.name} ({user.user_id})"
                    }
                    ListRowValue {
                        Button {
                            variant: ButtonVariant::Plain,
                            onclick: move |_| {
                                spawn(async move {
                                    let _ = logout().await;
                                    redirect_to_login();
                                });
                            },
                            "Sign out"
                        }
                    }
                }
            }
        }

        section { class: "settings-group",
            ListHeader { "Device" }
            ListGroup {
                ListRow {
                    ListRowLabel { "Name" }
                    ListRowValue { "data-testid": "device-authorize-name", "{request.body.device_name}" }
                }
                ListRow {
                    ListRowLabel { "Fingerprint" }
                    ListRowValue {
                        span { class: "fingerprint text-mono", "data-testid": "fingerprint", "{request.fingerprint}" }
                    }
                }
                ListRow {
                    ListRowLabel { "Where" }
                    ListRowValue {
                        if is_remote { "Another device" } else { "This computer" }
                    }
                }
            }
            ListFooter { "Check this fingerprint matches your terminal." }
        }

        Card { class: "device-consent-warning", role: "note",
            Icon { glyph: &icons::ALERT, size: "20", "aria-hidden": "true" }
            p { class: "text-callout",
                "Only approve if you just ran "
                code { "videocall-cli auth login" }
                " yourself, on a device you own. If someone sent you this link or asked you to open it, choose Deny: approving lets that device join meetings as you."
            }
        }

        if is_remote {
            label { class: "device-consent-check",
                input {
                    r#type: "checkbox",
                    checked: confirmed(),
                    disabled: submitting,
                    onchange: move |evt: FormEvent| confirmed.set(evt.checked()),
                }
                span { "I started this sign-in myself, on a device I own." }
            }
        }

        div { class: "row device-consent-actions",
            Button { class: "grow", disabled: submitting, onclick: on_deny, "Deny" }
            Button {
                variant: ButtonVariant::Prominent,
                class: "grow",
                disabled: submitting || (is_remote && !confirmed()),
                "aria-busy": submitting.then_some("true"),
                onclick: on_approve,
                "Approve"
            }
        }
    }
}

#[component]
fn RemoteCode(code: String, expires_in: i64) -> Element {
    let minutes = (expires_in + 59) / 60;
    rsx! {
        Card { class: "stack gap-4", "data-testid": "device-authorize-code",
            h2 { class: "text-title-3", "Approved" }
            p { class: "text-body",
                "Paste this code only into the terminal where you ran "
                code { "videocall-cli auth login --remote" }
                ". Nobody from videocall.rs will ever ask you for it. If someone asked you to send it to them, close this page."
            }
            Field { label: "Code", label_for: "device-code",
                TextField {
                    id: "device-code",
                    class: "text-mono",
                    readonly: true,
                    value: "{code}",
                    autocomplete: "off",
                    "spellcheck": "false",
                }
            }
            p { class: "text-footnote text-secondary", "The code works once and expires in {minutes} min." }
        }
    }
}

#[component]
fn OutcomeView(outcome: Outcome, on_retry: EventHandler<()>) -> Element {
    let navigator = use_navigator();
    let (title, message) = outcome.text();
    let testid = match outcome {
        Outcome::InvalidLink => "device-authorize-invalid",
        Outcome::Denied => "device-authorize-denied",
        _ => "device-authorize-error",
    };
    rsx! {
        Notice { title, alert: outcome != Outcome::Denied, testid,
            p { "{message}" }
            match outcome {
                Outcome::TooManyDevices => rsx! {
                    Button {
                        variant: ButtonVariant::Prominent,
                        onclick: move |_| { navigator.push(Route::Devices {}); },
                        "Manage devices"
                    }
                },
                Outcome::SessionExpired => rsx! {
                    Button { variant: ButtonVariant::Prominent, onclick: |_| redirect_to_login(), "Sign in" }
                },
                Outcome::ApproveFailed => rsx! {
                    Button { onclick: move |_| on_retry.call(()), "Try again" }
                },
                Outcome::InvalidLink => rsx! { BackHome {} },
                _ => rsx! {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn query(redirect_uri: &str, extra: &str) -> String {
        format!(
            "response_type=code&client_id=videocall-cli&redirect_uri={}&code_challenge={}\
             &code_challenge_method=S256&state=abcdefghijklmnopqrstuvwxyz&public_key={ZERO_KEY}\
             &device_name=videocall-cli%20on%20pi4{extra}",
            urlencoding::encode(redirect_uri),
            "c".repeat(43)
        )
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn good_queries_parse() {
        let loopback = parse_authorize_query(&query("http://127.0.0.1:5000/callback", "")).unwrap();
        assert_eq!(loopback.redirect, RedirectUri::Loopback { port: 5000 });
        assert_eq!(loopback.body.device_name, "videocall-cli on pi4");
        assert_eq!(loopback.body.state, "abcdefghijklmnopqrstuvwxyz");
        assert_eq!(loopback.fingerprint, "6668-7AAD-F862-BD77");
        let remote = parse_authorize_query(&query("urn:ietf:wg:oauth:2.0:oob", "")).unwrap();
        assert_eq!(remote.redirect, RedirectUri::Remote);
        assert_eq!(remote.body.state, "");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn bad_queries_are_rejected() {
        let good = query("http://127.0.0.1:5000/callback", "");
        for bad in [
            query("http://localhost:5000/callback", ""),
            query("https://evil.example/callback", ""),
            query("http://127.0.0.1:5000/callback", "&device_name=other"),
            good.replace("S256", "plain"),
            good.replace("client_id=videocall-cli", "client_id=x"),
            good.replace("state=abcdefghijklmnopqrstuvwxyz", "state=short"),
            good.replace("pi4", "%E2%80%AEpi4"),
            String::new(),
        ] {
            assert_eq!(parse_authorize_query(&bad), None, "{bad}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn encoded_request_survives_the_router() {
        use videocall_meeting_types::device::encode_authorize_request;
        let raw = query("http://127.0.0.1:5000/callback", "")
            .replace(
                "state=abcdefghijklmnopqrstuvwxyz",
                "state=abcdefghijklmnop%2Bqrs%25tu",
            )
            .replace(
                "device_name=videocall-cli%20on%20pi4",
                "device_name=C%2B%2B+rig",
            );
        let encoded = encode_authorize_request(&raw);
        let route: crate::routing::Route = format!("/devices/authorize?request={encoded}")
            .parse()
            .unwrap();
        let crate::routing::Route::DeviceAuthorize { request } = route else {
            panic!("wrong route");
        };
        assert_eq!(request, encoded);
        let parsed = decode_authorize_request(&request)
            .as_deref()
            .and_then(parse_authorize_query)
            .unwrap();
        assert_eq!(parsed.body.device_name, "C++ rig");
        assert_eq!(parsed.body.state, "abcdefghijklmnop+qrs%tu");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn denied_url_is_built_from_the_port() {
        assert_eq!(
            loopback_denied_url(5000, "a&b=c d"),
            "http://127.0.0.1:5000/callback?error=access_denied&state=a%26b%3Dc%20d"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn redirect_to_must_hit_the_same_listener() {
        let good = "http://127.0.0.1:5000/callback?code=x&state=y";
        assert_eq!(checked_redirect_to(5000, good), Some(good));
        for bad in [
            "http://127.0.0.1:5001/callback?code=x",
            "http://127.0.0.1:50000/callback?code=x",
            "http://127.0.0.1:5000/callbackx?code=x",
            "https://evil.example/?",
        ] {
            assert_eq!(checked_redirect_to(5000, bad), None, "{bad}");
        }
    }
}
