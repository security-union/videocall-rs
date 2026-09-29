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

use crate::auth::{
    check_session, clear_access_token, clear_id_token, clear_refresh_token, clear_user_profile,
    get_stored_id_token, get_user_profile, UserProfile,
};
use crate::components::about_modal::AboutModal;
use crate::components::browser_compatibility::BrowserCompatibility;
use crate::components::hero_orbs::HeroOrbs;
use crate::components::login::{do_login, ProviderButton};
use crate::components::meetings_list::MeetingsList;
use crate::constants::{logout_url, meeting_api_base_url, oauth_enabled};
use crate::context::{
    clear_display_name_from_storage, clear_display_name_owner_from_storage,
    describe_disallowed_chars, display_name_owner_id, email_to_display_name,
    is_allowed_display_name_char, is_guid_like, load_display_name_from_storage,
    load_display_name_owner_from_storage, normalize_spaces, save_display_name_owner_to_storage,
    save_display_name_to_storage, validate_display_name, validate_meeting_id, DisplayNameCtx,
    MeetingIdError, DISPLAY_NAME_MAX_LEN, MEETING_ID_ALLOWED_CHARS, MEETING_ID_MAX_LEN,
};
use crate::meeting_api::create_meeting;
use crate::routing::Route;
use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use web_sys::HtmlInputElement;

const TEXT_INPUT_CLASSES: &str = "input-apple";

/// Identifies which info-icon tooltip is currently "parked open" via an
/// explicit user action (click, Enter, or Space).  The CSS `:hover` and
/// `:focus-within` rules still drive the standard hover/keyboard-focus
/// reveal — this signal layers click-toggle and outside-tap dismissal on
/// top, so iOS Safari users can dismiss tooltips that get stuck via
/// `:focus-within` after a tap.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TooltipId {
    None,
    DisplayName,
    MeetingId,
}

/// RAII handle for the window-level `keydown` and `click` listeners that
/// dismiss any open tooltip on Escape or outside-tap.  The closures must
/// be kept alive for as long as the listeners are registered (otherwise
/// JS would reclaim them) so they live on the handle; `remove()` is
/// invoked from `use_drop` to detach the listeners on unmount.
struct TooltipDismissHandle {
    keydown_closure: Closure<dyn FnMut(web_sys::KeyboardEvent)>,
    click_closure: Closure<dyn FnMut(web_sys::Event)>,
    window: web_sys::Window,
}

impl TooltipDismissHandle {
    fn remove(&self) {
        let _ = self.window.remove_event_listener_with_callback(
            "keydown",
            self.keydown_closure.as_ref().unchecked_ref(),
        );
        let _ = self.window.remove_event_listener_with_callback(
            "click",
            self.click_closure.as_ref().unchecked_ref(),
        );
    }
}

/// Install the global Escape + outside-click dismissal listeners exactly
/// once per Home mount.  The `keydown` handler closes any open tooltip on
/// Escape; the `click` handler closes when the click target is not inside
/// any element marked with `data-tooltip-trigger` (the trigger spans set
/// this attribute, so click-on-trigger is skipped here and handled by the
/// trigger's `onclick`).
fn register_tooltip_dismiss_listeners(
    open_tooltip: Signal<TooltipId>,
) -> std::rc::Rc<TooltipDismissHandle> {
    let window = web_sys::window().expect("window is available in a browser context");

    let mut sig_for_keydown = open_tooltip;
    let keydown_closure =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |evt: web_sys::KeyboardEvent| {
            if evt.key() == "Escape" && sig_for_keydown() != TooltipId::None {
                sig_for_keydown.set(TooltipId::None);
            }
        });
    window
        .add_event_listener_with_callback("keydown", keydown_closure.as_ref().unchecked_ref())
        .expect("failed to register tooltip Escape listener");

    let mut sig_for_click = open_tooltip;
    let click_closure = Closure::<dyn FnMut(web_sys::Event)>::new(move |evt: web_sys::Event| {
        // Only act when something is open — avoid pointless ancestor walks.
        if sig_for_click() == TooltipId::None {
            return;
        }
        // If the click target (or any ancestor) is a tooltip trigger, the
        // trigger's own `onclick` is responsible for the toggle.  Skip.
        if let Some(target) = evt
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        {
            if target
                .closest("[data-tooltip-trigger]")
                .ok()
                .flatten()
                .is_some()
            {
                return;
            }
        }
        sig_for_click.set(TooltipId::None);
    });
    window
        .add_event_listener_with_callback("click", click_closure.as_ref().unchecked_ref())
        .expect("failed to register tooltip outside-click listener");

    std::rc::Rc::new(TooltipDismissHandle {
        keydown_closure,
        click_closure,
        window,
    })
}

fn not_allowed_message(chars: impl IntoIterator<Item = char>) -> String {
    format!("Not allowed: {}", describe_disallowed_chars(chars))
}

fn meeting_id_error_text(err: MeetingIdError) -> String {
    match err {
        MeetingIdError::Empty => "Enter a meeting ID".to_string(),
        MeetingIdError::TooLong => format!("Too long: max {MEETING_ID_MAX_LEN} characters"),
        MeetingIdError::InvalidChars(chars) => not_allowed_message(chars),
    }
}

/// The inline error for the display-name field while it is being edited.
/// Whitespace is judged after [`normalize_spaces`], as submit judges it.
pub fn display_name_field_error(raw: &str) -> Option<String> {
    let mut bad: Vec<char> = normalize_spaces(raw)
        .chars()
        .filter(|c| !is_allowed_display_name_char(*c))
        .collect();
    bad.sort();
    bad.dedup();
    (!bad.is_empty()).then(|| not_allowed_message(bad))
}

/// The normalised display name the form submits for `raw`, or the message
/// shown beside the field: disallowed characters in the inline format,
/// otherwise the shared empty / too-long message.
pub fn display_name_for_submit(raw: &str) -> Result<String, String> {
    validate_display_name(raw).map_err(|message| display_name_field_error(raw).unwrap_or(message))
}

/// Whether `stored_owner` records someone other than `profile`.
pub fn display_name_owned_by_another(stored_owner: Option<&str>, profile: &UserProfile) -> bool {
    stored_owner.is_some_and(|owner| owner != display_name_owner_id(&profile.user_id))
}

/// The unvalidated display name a signed-in `profile` starts with: `stored`
/// unless `stored_owner` is someone else, otherwise the name derived from the
/// profile; `None` when that is empty.
pub fn signed_in_display_name(
    stored: Option<String>,
    stored_owner: Option<&str>,
    profile: &UserProfile,
) -> Option<String> {
    stored
        .filter(|_| !display_name_owned_by_another(stored_owner, profile))
        .or_else(|| {
            let derived = if profile.name.contains('@') {
                email_to_display_name(&profile.name)
            } else if is_guid_like(&profile.name) {
                if profile.user_id.contains('@') {
                    email_to_display_name(&profile.user_id)
                } else {
                    String::new()
                }
            } else {
                profile.name.clone()
            };
            (!derived.is_empty()).then_some(derived)
        })
}

/// The meeting ID the form submits for `raw` (outer whitespace trimmed), or
/// the message shown beside the field when the shared rule rejects it.
pub fn meeting_id_for_submit(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    validate_meeting_id(id)
        .map(|()| id.to_string())
        .map_err(meeting_id_error_text)
}

/// The inline error for the field while it is being edited; blank shows none.
pub fn meeting_id_field_error(raw: &str) -> Option<String> {
    let id = raw.trim();
    if id.is_empty() {
        None
    } else {
        validate_meeting_id(id).err().map(meeting_id_error_text)
    }
}

fn focus_input(element: Option<web_sys::Element>) {
    if let Some(input) = element.and_then(|el| el.dyn_into::<HtmlInputElement>().ok()) {
        let _ = input.focus();
    }
}

#[component]
pub fn Home() -> Element {
    let navigator = use_navigator();

    let mut meeting_id_ref = use_signal(|| None::<web_sys::Element>);
    let mut meeting_id_value = use_signal(String::new);
    let mut meeting_id_error = use_signal(|| None::<String>);
    let mut display_name_ctx = use_context::<DisplayNameCtx>();

    let existing_username: String = if oauth_enabled().unwrap_or(false) {
        String::new()
    } else if let Some(name) = (display_name_ctx.0)() {
        name
    } else {
        load_display_name_from_storage().unwrap_or_default()
    };

    let mut username_ref = use_signal(|| None::<web_sys::Element>);
    let mut username_value = use_signal(|| existing_username.clone());
    let mut username_error = use_signal(|| None::<String>);
    let mut failed_submits = use_signal(|| 0u32);

    // User profile state (for displaying auth info when OAuth is enabled)
    let mut user_profile = use_signal(|| None::<UserProfile>);

    // Dropdown toggle for auth menu
    let mut show_dropdown = use_signal(|| false);

    let mut create_error = use_signal(|| None::<String>);
    let mut creating = use_signal(|| false);

    // About modal — surfaces client + server build info from a thin footer
    // link below the hero card.  Only the modal triggers the
    // `/api/v1/versions` fetch (in its own use_effect), so the homepage
    // itself doesn't pay for a network round-trip on every load.
    let mut show_about = use_signal(|| false);

    // Issue #1480: build date shows on ALL builds (not github info). Omit the
    // "· built …" suffix entirely when the timestamp is the build.rs sentinel.
    // Issue #1789: the date is the build instant's LOCAL calendar date (viewer's
    // timezone), so a near-midnight-UTC build shows the day matching the reader's
    // own calendar. Date-only, so no zone hint is needed.
    let about_built_suffix = crate::constants::build_date_local(env!("BUILD_TIMESTAMP"))
        .map(|d| format!(" · built {d}"))
        .unwrap_or_default();

    // Tracks which (if any) info-icon tooltip the user has explicitly
    // parked open via click / Enter / Space.  CSS still handles the
    // hover and keyboard-focus reveal; this signal exists so we can
    // honour Escape and outside-tap dismissal — important on iOS Safari
    // where a tap-focus on a `<span tabindex="0">` doesn't reliably
    // blur on subsequent taps to non-interactive page chrome.
    let mut open_tooltip = use_signal(|| TooltipId::None);

    // Install window-level Escape + outside-click listeners exactly once
    // per Home mount.  `use_hook` (not `use_effect`) avoids re-installing
    // on re-renders — see CmdKHandle in main.rs for the same pattern.
    let tooltip_dismiss_handle = use_hook(|| register_tooltip_dismiss_listeners(open_tooltip));
    use_drop({
        let tooltip_dismiss_handle = tooltip_dismiss_handle.clone();
        move || {
            tooltip_dismiss_handle.remove();
        }
    });

    // Fetch user profile when OAuth is enabled.
    use_effect(move || {
        if oauth_enabled().unwrap_or(false) {
            spawn(async move {
                if check_session().await.is_ok() {
                    if let Ok(profile) = get_user_profile().await {
                        // Anonymous sessions have no real identity — skip them entirely.
                        // The template also filters them so the sign-in button renders.
                        if !profile.user_id.starts_with("anon-") {
                            let owner = load_display_name_owner_from_storage();
                            if display_name_owned_by_another(owner.as_deref(), &profile) {
                                clear_display_name_from_storage();
                                display_name_ctx.0.set(None);
                            }
                            let raw = signed_in_display_name(
                                load_display_name_from_storage(),
                                owner.as_deref(),
                                &profile,
                            );
                            if let Some(raw) = raw.filter(|_| username_value.peek().is_empty()) {
                                match display_name_for_submit(&raw) {
                                    Ok(name) => {
                                        save_display_name_to_storage(&name);
                                        save_display_name_owner_to_storage(&display_name_owner_id(
                                            &profile.user_id,
                                        ));
                                        display_name_ctx.0.set(Some(name.clone()));
                                        username_error.set(None);
                                        username_value.set(name);
                                    }
                                    Err(message) => {
                                        username_error.set(Some(message));
                                        username_value.set(raw);
                                    }
                                }
                            }
                            user_profile.set(Some(profile));
                        }
                    }
                    // Session valid but profile fetch failed → leave field empty.
                }
                // Session invalid → field stays empty; signed-out state.
            });
        }
    });

    // Dev auto-login: when OAuth is disabled and no session cookie exists,
    // attempt to acquire one by fetching the dev auto-login endpoint.
    // The endpoint sets the cookie and returns a redirect to "/".
    // If the endpoint returns 404 (DEV_USER not configured), we simply
    // do nothing — the app continues to work without a session.
    use_effect(move || {
        if !oauth_enabled().unwrap_or(false) {
            // Check whether a session already exists before navigating.
            wasm_bindgen_futures::spawn_local(async move {
                if check_session().await.is_ok() {
                    return; // Already have a valid session — nothing to do.
                }
                // No valid session — try the dev auto-login endpoint.
                if let Ok(base_url) = meeting_api_base_url() {
                    let url = format!("{}/api/v1/dev/auto-login", base_url);
                    if let Ok(resp) = reqwest::Client::new().get(&url).send().await {
                        if resp.status().is_success() || resp.status().is_redirection() {
                            if let Some(window) = web_sys::window() {
                                let _ = window.location().set_href(&url);
                            }
                        }
                    }
                }
            });
        }
    });

    // Logout handler: clear all local auth state, then navigate the browser to
    // the backend /logout endpoint as a top-level navigation (not fetch).
    //
    // The top-level navigation is required because the backend 303-redirects to
    // the IdP's end_session_endpoint, and SameSite=Lax cookies are only sent on
    // navigations (sec-fetch-mode: navigate), not on fetch() requests (CORS mode).
    // Without the cookies the IdP session survives and auto-re-authenticates.
    //
    // We clear client-side state synchronously BEFORE navigating so that if the
    // post_logout_redirect_uri lands back on this SPA, it sees no tokens and
    // shows the sign-in page (avoiding the prior "can't re-sign-in" regression).
    let on_logout = move |_| {
        // Grab id_token BEFORE clearing — needed as id_token_hint for the IdP.
        let id_token_hint = get_stored_id_token();

        // Clear all client-side auth state
        clear_access_token();
        clear_refresh_token();
        clear_id_token();
        clear_user_profile();
        clear_display_name_from_storage();
        // A guest token is a bearer credential; an explicit logout should not
        // leave one behind (issue #2331).
        crate::guest_session::clear_all();

        // Defense-in-depth: drop any in-flight refresh Shared so a stale wave
        // can't linger post-logout.
        crate::meeting_api::reset_refresh_inflight();

        // Reset in-memory signals
        user_profile.set(None);
        display_name_ctx.0.set(None);
        username_value.set(String::new());

        // Navigate to backend /logout as a top-level navigation so the redirect
        // chain to the IdP carries SameSite=Lax cookies.
        if let Ok(mut url) = logout_url() {
            if let Some(hint) = id_token_hint {
                let encoded = js_sys::encode_uri_component(&hint);
                url.push_str("?id_token_hint=");
                url.push_str(&encoded.as_string().unwrap_or_default());
            }
            if let Some(window) = web_sys::window() {
                let _ = window.location().set_href(&url);
            }
        }
    };

    let get_meeting_id = move || -> String {
        meeting_id_ref()
            .and_then(|el| el.dyn_into::<HtmlInputElement>().ok())
            .map(|input| input.value())
            .unwrap_or_default()
    };

    let remember_display_name = move |name: &str| {
        save_display_name_to_storage(name);
        match user_profile.peek().as_ref() {
            Some(profile) => {
                save_display_name_owner_to_storage(&display_name_owner_id(&profile.user_id))
            }
            None => clear_display_name_owner_from_storage(),
        }
    };

    let dropdown_name = user_profile().map(|p| {
        if is_guid_like(&p.name) {
            if p.user_id.contains('@') {
                email_to_display_name(&p.user_id)
            } else {
                p.user_id.clone()
            }
        } else {
            p.name.clone()
        }
    });

    rsx! {
        div { class: "hero-container",
            BrowserCompatibility {}
            HeroOrbs {}

            // Auth dropdown — absolutely positioned in top-right of hero-container
            if oauth_enabled().unwrap_or(false) {
                if let Some(profile) = user_profile().filter(|p| !p.user_id.starts_with("anon-")) {
                    div { class: "auth-dropdown-container",
                        button {
                            r#type: "button",
                            class: "auth-dropdown-trigger",
                            onclick: move |_| {
                                show_dropdown.set(!show_dropdown());
                            },
                            span { "{dropdown_name.as_deref().unwrap_or_default()}" }
                            svg {
                                xmlns: "http://www.w3.org/2000/svg",
                                width: "16",
                                height: "16",
                                view_box: "0 0 24 24",
                                fill: "none",
                                stroke: "currentColor",
                                stroke_width: "2",
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                polyline { points: "6 9 12 15 18 9" }
                            }
                        }
                        if show_dropdown() {
                            div { class: "auth-dropdown-menu",
                                div { class: "auth-dropdown-header",
                                    p { class: "auth-dropdown-name", "{dropdown_name.as_deref().unwrap_or_default()}" }
                                    p { class: "auth-dropdown-email", "{profile.user_id}" }
                                }
                                button {
                                    r#type: "button",
                                    class: "auth-dropdown-signout",
                                    onclick: on_logout,
                                    "Sign out"
                                }
                            }
                        }
                    }
                } else {
                    div { class: "auth-dropdown-container",
                        ProviderButton { onclick: move |_| do_login() }
                    }
                }
            }

            // GitHub corner ribbon
            a {
                href: "https://github.com/security-union/videocall-rs",
                class: "github-corner",
                aria_label: "View source on GitHub",
                svg {
                    width: "80",
                    height: "80",
                    view_box: "0 0 250 250",
                    style: "fill:#7928CA; color:#0D131F; position: absolute; top: 0; border: 0; left: 0; transform: scaleX(-1);",
                    "aria-hidden": "true",
                    path { d: "M0,0 L115,115 L130,115 L142,142 L250,250 L250,0 Z" }
                    path { d: "M128.3,109.0 C113.8,99.7 119.0,89.6 119.0,89.6 C122.0,82.7 120.5,78.6 120.5,78.6 C119.2,72.0 123.4,76.3 123.4,76.3 C127.3,80.9 125.5,87.3 125.5,87.3 C122.9,97.6 130.6,101.9 134.4,103.2", fill: "currentColor", style: "transform-origin: 130px 106px;", class: "octo-arm" }
                    path { d: "M115.0,115.0 C114.9,115.1 118.7,116.5 119.8,115.4 L133.7,101.6 C136.9,99.2 139.9,98.4 142.2,98.6 C133.8,88.0 127.5,74.4 143.8,58.0 C148.5,53.4 154.0,51.2 159.7,51.0 C160.3,49.4 163.2,43.6 171.4,40.1 C171.4,40.1 176.1,42.5 178.8,56.2 C183.1,58.6 187.2,61.8 190.9,65.4 C194.5,69.0 197.7,73.2 200.1,77.6 C213.8,80.2 216.3,84.9 216.3,84.9 C212.7,93.1 206.9,96.0 205.4,96.6 C205.1,102.4 203.0,107.8 198.3,112.5 C181.9,128.9 168.3,122.5 157.7,114.1 C157.9,116.9 156.7,120.9 152.7,124.9 L141.0,136.5 C139.8,137.7 141.6,141.9 141.8,141.8 Z", fill: "currentColor", class: "octo-body" }
                }
            }
            div { class: "hero-content",
                h1 { class: "hero-title text-center", "videocall.rs" }
                p { class: "hero-tagline text-center",
                    "Built with Rust"
                    span { class: "tagline-dot", " \u{00b7} " }
                    "WebTransport"
                    span { class: "tagline-dot", " \u{00b7} " }
                    "WASM"
                }
                div { class: "content-separator" }

                // Form section
                div { class: "w-full mb-8 card-apple p-8",
                    form {
                        onsubmit: move |e| {
                            e.prevent_default();
                            let name = display_name_for_submit(&username_value());
                            let meeting_id = meeting_id_for_submit(&get_meeting_id());
                            username_error.set(name.as_ref().err().cloned());
                            meeting_id_error.set(meeting_id.as_ref().err().cloned());
                            match (name, meeting_id) {
                                (Ok(valid_name), Ok(meeting_id)) => {
                                    username_value.set(valid_name.clone());
                                    remember_display_name(&valid_name);
                                    (display_name_ctx.0).set(Some(valid_name));

                                    spawn(async move {
                                        gloo_timers::future::TimeoutFuture::new(0).await;
                                        navigator.push(Route::Meeting { id: meeting_id });
                                    });
                                }
                                (name, _) => {
                                    failed_submits += 1;
                                    focus_input(if name.is_err() {
                                        username_ref()
                                    } else {
                                        meeting_id_ref()
                                    });
                                }
                            }
                        },
                        h3 { class: "text-center text-xl font-semibold mb-6 text-white/90",
                            "Start or Join a Meeting"
                        }
                        div { class: "space-y-6",
                            div {
                                label {
                                    r#for: "username",
                                    class: "field-label",
                                    span { class: "field-label__name",
                                        "Display Name"
                                        span {
                                            class: if open_tooltip() == TooltipId::DisplayName {
                                                "field-label__info field-label__info--open"
                                            } else {
                                                "field-label__info"
                                            },
                                            tabindex: 0,
                                            role: "button",
                                            aria_label: "What's allowed in Display Name?",
                                            aria_describedby: "username-info-tip",
                                            "data-tooltip-trigger": "username",
                                            onclick: move |e| {
                                                e.stop_propagation();
                                                open_tooltip.set(if open_tooltip() == TooltipId::DisplayName {
                                                    TooltipId::None
                                                } else {
                                                    TooltipId::DisplayName
                                                });
                                            },
                                            onkeydown: move |e| {
                                                let key = e.key();
                                                if key == Key::Enter || key == Key::Character(" ".to_string()) {
                                                    e.prevent_default();
                                                    e.stop_propagation();
                                                    open_tooltip.set(if open_tooltip() == TooltipId::DisplayName {
                                                        TooltipId::None
                                                    } else {
                                                        TooltipId::DisplayName
                                                    });
                                                }
                                            },
                                            svg {
                                                class: "field-label__info-icon",
                                                xmlns: "http://www.w3.org/2000/svg",
                                                width: 14,
                                                height: 14,
                                                view_box: "0 0 16 16",
                                                fill: "none",
                                                stroke: "currentColor",
                                                stroke_width: 1.6,
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
                                                circle { cx: 8, cy: 8, r: 6.75 }
                                                line { x1: 8, y1: 7.25, x2: 8, y2: 11.25 }
                                                circle { cx: 8, cy: 5, r: 0.55, fill: "currentColor", stroke: "none" }
                                            }
                                            span {
                                                id: "username-info-tip",
                                                class: "field-label__tooltip",
                                                role: "tooltip",
                                                "Your name as shown to other participants. Allowed: letters, numbers, spaces, hyphens (-), underscores (_), and apostrophes ('). Up to 50 characters."
                                            }
                                        }
                                    }
                                    span {
                                        class: "field-label__error",
                                        aria_live: "polite",
                                        // Re-keyed per failed submit so an unchanged message is
                                        // re-inserted into the live region, not diffed in place.
                                        if let Some(message) = username_error() {
                                            span { key: "{failed_submits}", "{message}" }
                                        }
                                    }
                                }
                                input {
                                    id: "username",
                                    class: if username_error().is_some() {
                                        "input-apple input-apple--invalid"
                                    } else {
                                        TEXT_INPUT_CLASSES
                                    },
                                    r#type: "text",
                                    placeholder: "Enter your display name",
                                    required: true,
                                    autofocus: true,
                                    maxlength: DISPLAY_NAME_MAX_LEN as i64,
                                    aria_invalid: username_error().is_some(),
                                    value: "{username_value}",
                                    onmounted: move |evt| {
                                        if let Some(elem) = evt.try_as_web_event() {
                                            username_ref.set(Some(elem));
                                        }
                                    },
                                    oninput: move |e: Event<FormData>| {
                                        let v = e.value();
                                        username_error.set(display_name_field_error(&v));
                                        username_value.set(v);
                                    },
                                }
                            }
                            div {
                                label {
                                    r#for: "meeting-id",
                                    class: "field-label",
                                    span { class: "field-label__name",
                                        "Meeting ID"
                                        span {
                                            class: if open_tooltip() == TooltipId::MeetingId {
                                                "field-label__info field-label__info--open"
                                            } else {
                                                "field-label__info"
                                            },
                                            tabindex: 0,
                                            role: "button",
                                            aria_label: "What's allowed in Meeting ID?",
                                            aria_describedby: "meeting-id-info-tip",
                                            "data-tooltip-trigger": "meeting-id",
                                            onclick: move |e| {
                                                e.stop_propagation();
                                                open_tooltip.set(if open_tooltip() == TooltipId::MeetingId {
                                                    TooltipId::None
                                                } else {
                                                    TooltipId::MeetingId
                                                });
                                            },
                                            onkeydown: move |e| {
                                                let key = e.key();
                                                if key == Key::Enter || key == Key::Character(" ".to_string()) {
                                                    e.prevent_default();
                                                    e.stop_propagation();
                                                    open_tooltip.set(if open_tooltip() == TooltipId::MeetingId {
                                                        TooltipId::None
                                                    } else {
                                                        TooltipId::MeetingId
                                                    });
                                                }
                                            },
                                            svg {
                                                class: "field-label__info-icon",
                                                xmlns: "http://www.w3.org/2000/svg",
                                                width: 14,
                                                height: 14,
                                                view_box: "0 0 16 16",
                                                fill: "none",
                                                stroke: "currentColor",
                                                stroke_width: 1.6,
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
                                                circle { cx: 8, cy: 8, r: 6.75 }
                                                line { x1: 8, y1: 7.25, x2: 8, y2: 11.25 }
                                                circle { cx: 8, cy: 5, r: 0.55, fill: "currentColor", stroke: "none" }
                                            }
                                            span {
                                                id: "meeting-id-info-tip",
                                                class: "field-label__tooltip",
                                                role: "tooltip",
                                                "A unique identifier for the meeting. Click \"Generate a New Meeting ID\" to create one, paste an ID shared by a host, or enter your own. Allowed: {MEETING_ID_ALLOWED_CHARS}. Up to {MEETING_ID_MAX_LEN} characters."
                                            }
                                        }
                                    }
                                    span {
                                        id: "meeting-id-error",
                                        class: "field-label__error",
                                        aria_live: "polite",
                                        if let Some(message) = meeting_id_error() {
                                            span { key: "{failed_submits}", "{message}" }
                                        }
                                    }
                                }
                                input {
                                    id: "meeting-id",
                                    class: if meeting_id_error().is_some() {
                                        "input-apple input-apple--invalid"
                                    } else {
                                        TEXT_INPUT_CLASSES
                                    },
                                    r#type: "text",
                                    placeholder: "Enter meeting ID or generate one",
                                    required: true,
                                    aria_invalid: meeting_id_error().is_some(),
                                    oninput: move |e: Event<FormData>| {
                                        let v = e.value();
                                        meeting_id_error.set(meeting_id_field_error(&v));
                                        meeting_id_value.set(v);
                                    },
                                    onmounted: move |evt| {
                                        if let Some(elem) = evt.try_as_web_event() {
                                            meeting_id_ref.set(Some(elem));
                                        }
                                    },
                                }
                            }
                            if !meeting_id_value().is_empty() {
                                div { class: "mt-4",
                                    button {
                                        r#type: "submit",
                                        class: "btn-apple btn-primary w-full",
                                        span { class: "text-lg", "Start or Join Meeting" }
                                    }
                                }
                            } else {
                                div { class: "mt-4",
                                    if let Some(err) = create_error() {
                                        p {
                                            class: "text-sm mb-2 ml-1",
                                            style: "color: #ff6b6b;",
                                            "{err}"
                                        }
                                    }
                                    button {
                                        r#type: "button",
                                        class: "btn-apple btn-primary w-full flex items-center justify-center gap-2",
                                        disabled: creating(),
                                        onclick: move |_| {
                                            username_error.set(None);
                                            create_error.set(None);
                                            let username = username_value();
                                            match display_name_for_submit(&username) {
                                                Ok(valid_name) => {
                                                    username_value.set(valid_name.clone());
                                                    remember_display_name(&valid_name);
                                                    (display_name_ctx.0).set(Some(valid_name.clone()));
                                                    creating.set(true);

                                                    spawn(async move {
                                                        match create_meeting(None, false).await {
                                                            Ok(response) => {
                                                                creating.set(false);
                                                                let new_id = response.meeting_id;
                                                                if let Some(el) = meeting_id_ref() {
                                                                    if let Ok(input) = el.dyn_into::<HtmlInputElement>() {
                                                                        input.set_value(&new_id);
                                                                    }
                                                                }
                                                                meeting_id_value.set(new_id);
                                                                meeting_id_error.set(None);
                                                            }
                                                            Err(e) => {
                                                                creating.set(false);
                                                                create_error.set(Some(format!("Failed to create meeting: {e}")));
                                                            }
                                                        }
                                                    });
                                                }
                                                Err(message) => {
                                                    username_error.set(Some(message));
                                                }
                                            }
                                        },
                                        if creating() {
                                            span { class: "loading-spinner", style: "width: 18px; height: 18px;" }
                                            span { class: "text-lg", "Generating..." }
                                        } else {
                                            span { class: "text-lg", "Generate a New Meeting ID" }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Auth removed from here — now rendered as a fixed dropdown in the top-right

                    // Merged meetings list — owned + previously joined in
                    // a single section, server-ordered by `last_active_at`.
                    if !oauth_enabled().unwrap_or(false) || user_profile().is_some() {
                        MeetingsList {
                            on_select_meeting: move |meeting_id: String| {
                                if let Some(el) = meeting_id_ref() {
                                    if let Ok(input) = el.dyn_into::<HtmlInputElement>() {
                                        input.set_value(&meeting_id);
                                    }
                                }
                                meeting_id_error.set(meeting_id_field_error(&meeting_id));
                                meeting_id_value.set(meeting_id);
                            },
                        }
                    }
                }

                div { class: "content-separator" }

                div { class: "about-footer",
                    button {
                        r#type: "button",
                        class: "about-footer-link",
                        "data-testid": "about-footer-link",
                        "aria-label": "Show app version and About details",
                        onclick: move |_| show_about.set(true),
                        "About videocall-ui v"
                        "{env!(\"CARGO_PKG_VERSION\")}"
                        "{about_built_suffix}"
                    }
                }

                div { class: "grid grid-cols-1 md:grid-cols-2 gap-8", style: "margin-top:1em",
                    div {
                        button {
                            onclick: move |_| {
                                let window = web_sys::window().expect("no global window exists");
                                let _ = window.open_with_url("https://github.com/security-union/videocall-rs");
                            },
                            class: "secondary-button flex items-center justify-center mx-auto gap-2",
                            style: "margin-top:1em",
                            svg { xmlns: "http://www.w3.org/2000/svg", width: "18", height: "18", view_box: "0 0 24 24", fill: "currentColor",
                                path { d: "M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.333-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z" }
                            }
                            span { "Contribute on GitHub" }
                        }
                    }
                }
            }

            AboutModal { open: show_about }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_the_shared_rule_accepts_submit_unchanged() {
        for id in ["abc_123", "my-meeting", "a~b", "-", "~"] {
            assert_eq!(meeting_id_for_submit(id), Ok(id.to_string()), "{id:?}");
            assert_eq!(meeting_id_field_error(id), None, "{id:?}");
        }
    }

    #[test]
    fn disallowed_characters_are_named_after_the_verdict() {
        assert_eq!(
            meeting_id_field_error("a.b").as_deref(),
            Some("Not allowed: '.'")
        );
        assert_eq!(
            meeting_id_field_error("a/b").as_deref(),
            Some("Not allowed: '/'")
        );
        assert_eq!(
            meeting_id_for_submit("x/y.z/w"),
            Err("Not allowed: '/', '.'".to_string())
        );
    }

    #[test]
    fn hard_to_read_characters_are_named_readably() {
        assert_eq!(
            meeting_id_field_error("a b").as_deref(),
            Some("Not allowed: space")
        );
        assert_eq!(
            meeting_id_field_error("o'brien").as_deref(),
            Some("Not allowed: apostrophe")
        );
        assert_eq!(
            meeting_id_field_error("a\\b").as_deref(),
            Some("Not allowed: '\\'")
        );
        assert_eq!(
            meeting_id_field_error("a\tb\u{202e}c\u{a0}d").as_deref(),
            Some("Not allowed: '\\t', '\\u{202e}', '\\u{a0}'")
        );
    }

    #[test]
    fn pasted_outer_whitespace_is_trimmed_but_inner_whitespace_is_not() {
        assert_eq!(meeting_id_for_submit("  abc  "), Ok("abc".to_string()));
        assert_eq!(meeting_id_field_error("  abc  "), None);
        assert_eq!(
            meeting_id_for_submit(" a b "),
            Err("Not allowed: space".to_string())
        );
    }

    #[test]
    fn blank_input_shows_no_field_error_but_cannot_be_submitted() {
        assert_eq!(meeting_id_field_error(""), None);
        assert_eq!(meeting_id_field_error("   "), None);
        assert_eq!(
            meeting_id_for_submit("   "),
            Err("Enter a meeting ID".to_string())
        );
    }

    #[test]
    fn ids_longer_than_the_shared_limit_are_refused() {
        let at_limit = "a".repeat(MEETING_ID_MAX_LEN);
        assert_eq!(meeting_id_for_submit(&at_limit), Ok(at_limit.clone()));
        assert_eq!(
            meeting_id_field_error(&format!("{at_limit}a")),
            Some(format!("Too long: max {MEETING_ID_MAX_LEN} characters"))
        );
    }

    #[test]
    fn display_name_errors_use_the_meeting_id_format() {
        assert_eq!(display_name_field_error("O'Brien Smith-Jones_2"), None);
        assert_eq!(display_name_field_error(""), None);
        assert_eq!(
            display_name_field_error("alice@").as_deref(),
            Some("Not allowed: '@'")
        );
        assert_eq!(
            display_name_field_error("Bob!\t@!").as_deref(),
            Some("Not allowed: '!', '@'")
        );
        assert_eq!(
            display_name_field_error("a\\b\u{202e}").as_deref(),
            Some("Not allowed: '\\', '\\u{202e}'")
        );
    }

    #[test]
    fn display_name_whitespace_that_submit_accepts_is_not_flagged() {
        for raw in ["Ann\tLee", "Ann\u{a0}Lee", "Ann\u{3000}Lee", " Ann  Lee "] {
            assert_eq!(display_name_field_error(raw), None, "{raw:?}");
            assert_eq!(
                display_name_for_submit(raw),
                Ok("Ann Lee".to_string()),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn submitted_display_name_errors_use_the_inline_format() {
        assert_eq!(
            display_name_for_submit("Bob!"),
            Err("Not allowed: '!'".to_string())
        );
        assert_eq!(
            display_name_for_submit("user@name.com"),
            Err("Not allowed: '.', '@'".to_string())
        );
        let too_long = "a".repeat(DISPLAY_NAME_MAX_LEN + 1);
        for raw in ["", "   ", too_long.as_str()] {
            assert_eq!(
                display_name_for_submit(raw),
                validate_display_name(raw),
                "{raw:?}"
            );
        }
        assert_eq!(
            display_name_for_submit(&format!("{too_long}!")),
            Err("Not allowed: '!'".to_string())
        );
    }

    fn profile(user_id: &str, name: &str) -> UserProfile {
        UserProfile {
            user_id: user_id.to_string(),
            name: name.to_string(),
        }
    }

    fn tony_gmail() -> Option<String> {
        Some("Tony gMail".to_string())
    }

    #[test]
    fn signed_in_display_name_keeps_a_stored_name_saved_for_this_user() {
        assert_eq!(
            signed_in_display_name(
                tony_gmail(),
                Some(&display_name_owner_id("antonio@example.com")),
                &profile("antonio@example.com", "Antonio Estrada"),
            ),
            tony_gmail()
        );
    }

    #[test]
    fn signed_in_display_name_keeps_a_stored_name_with_no_recorded_owner() {
        assert_eq!(
            signed_in_display_name(
                tony_gmail(),
                None,
                &profile("antonio@example.com", "Antonio Estrada"),
            ),
            tony_gmail()
        );
    }

    #[test]
    fn signed_in_display_name_ignores_a_stored_name_saved_for_another_user() {
        let me = profile("antonio@example.com", "Antonio Estrada");
        for owner in [
            display_name_owner_id("someone-else@example.com"),
            "antonio@example.com".to_string(),
            crate::context::GUEST_DISPLAY_NAME_OWNER.to_string(),
        ] {
            assert!(display_name_owned_by_another(Some(&owner), &me), "{owner}");
            assert_eq!(
                signed_in_display_name(tony_gmail(), Some(&owner), &me),
                Some("Antonio Estrada".to_string()),
                "{owner}"
            );
        }
    }

    #[test]
    fn signed_in_display_name_derives_from_the_profile_when_nothing_is_stored() {
        const GUID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let cases = [
            (
                profile("a@example.com", "Antonio Estrada"),
                Some("Antonio Estrada"),
            ),
            (
                profile("jane.doe@example.com", "jane.doe@example.com"),
                Some("Jane Doe"),
            ),
            (profile("jane.doe@example.com", GUID), Some("Jane Doe")),
            (profile(GUID, GUID), None),
            (
                profile("a@example.com", "Antonio Estrada (Tony)"),
                Some("Antonio Estrada (Tony)"),
            ),
        ];
        for (p, expected) in cases {
            assert_eq!(
                signed_in_display_name(None, None, &p),
                expected.map(str::to_string),
                "{p:?}"
            );
        }
    }

    #[test]
    fn overlong_ids_with_disallowed_characters_name_the_characters() {
        assert_eq!(
            meeting_id_field_error(&"\u{e9}".repeat(130)).as_deref(),
            Some("Not allowed: '\u{e9}'")
        );
    }
}
