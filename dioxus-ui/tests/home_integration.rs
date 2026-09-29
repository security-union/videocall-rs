// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Integration test for the Home (landing) page (Dioxus).
//
// Verifies that the real Home component renders without errors when
// window.__APP_CONFIG is present. Rather than
// asserting on every single DOM node, we check a handful of landmarks
// that uniquely identify the page.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{
    cleanup, create_mount_point, inject_app_config, inject_app_config_oauth_enabled,
    mock_fetch_401, mock_fetch_meetings_empty, remove_app_config, render_into,
    reset_test_browser_state, restore_fetch, wait_for_selector, yield_now,
};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
use web_sys::{Event, EventInit, HtmlElement, HtmlInputElement};

use dioxus::prelude::*;
use dioxus_ui::components::config_error::ConfigError;
use dioxus_ui::components::invalid_meeting_id::{JOIN_HINT, SETTINGS_HINT};
use dioxus_ui::components::search_modal::SearchVisibleCtx;
use dioxus_ui::constants::app_config;
use dioxus_ui::context::{
    display_name_owner_id, load_display_name_from_storage, load_transport_preference,
    validate_display_name, DisplayNameCtx, TransportPreferenceCtx, GUEST_DISPLAY_NAME_OWNER,
    MEETING_ID_ALLOWED_CHARS, MEETING_ID_MAX_LEN,
};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Create a bubbling "input" event so Dioxus event delegation picks it up.
fn bubbling_input_event() -> Event {
    let init = EventInit::new();
    init.set_bubbles(true);
    Event::new_with_event_init_dict("input", &init).unwrap()
}

/// Create a bubbling, cancelable "submit" event.
fn bubbling_submit_event() -> Event {
    let init = EventInit::new();
    init.set_bubbles(true);
    init.set_cancelable(true);
    Event::new_with_event_init_dict("submit", &init).unwrap()
}

// ---------------------------------------------------------------------------
// Wrapper component — provides the context Home needs via the full Router.
// ---------------------------------------------------------------------------

/// Push the browser URL to "/" so that Router renders the Home route, then
/// render the full app shell (DisplayNameCtx + Router) matching `main.rs`.
fn ensure_root_url() {
    let _ = gloo_utils::window().history().unwrap().push_state_with_url(
        &wasm_bindgen::JsValue::NULL,
        "",
        Some("/"),
    );
}

/// Full app wrapper: provides all three context providers that `main.rs`
/// supplies (DisplayNameCtx, TransportPreferenceCtx, SearchVisibleCtx),
/// then renders Router<Route>.  The Router picks the component based on
/// the current URL (pushed to "/").
fn home_wrapper_direct() -> Element {
    let username_signal = use_signal(|| None::<String>);
    use_context_provider(|| DisplayNameCtx(username_signal));

    let transport_pref = use_signal(load_transport_preference);
    use_context_provider(|| TransportPreferenceCtx(transport_pref));

    let search_visible = use_signal(|| false);
    use_context_provider(|| SearchVisibleCtx {
        is_visible: search_visible,
    });

    match app_config() {
        Ok(_) => rsx! {
            Router::<dioxus_ui::routing::Route> {}
        },
        Err(e) => rsx! {
            ConfigError { message: e }
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn home_page_renders() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // Wait for the Router to resolve and Home to render.
    assert!(
        wait_for_selector(&mount, "#username", 2000).await,
        "Timed out waiting for Home page to render (#username)"
    );

    // No error banner — config loaded and browser checks passed.
    assert!(
        mount.query_selector(".error-container").unwrap().is_none(),
        "BrowserCompatibility should not show an error in Chrome"
    );

    // The page text should contain landmarks that identify the home screen.
    let text = mount.text_content().unwrap_or_default();
    assert!(text.contains("Concept Car"), "title missing");
    assert!(
        text.contains("Start or Join a Meeting"),
        "form heading missing"
    );
    assert!(
        text.contains("Generate a New Meeting ID"),
        "create button missing"
    );

    // The two inputs the user fills in must be present.
    assert!(
        mount.query_selector("#username").unwrap().is_some(),
        "username input missing"
    );
    assert!(
        mount.query_selector("#meeting-id").unwrap().is_some(),
        "meeting-id input missing"
    );
    assert!(
        mount
            .query_selector("button[type='submit']")
            .unwrap()
            .is_none(),
        "join button should not render until a meeting ID is entered"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_shows_login_when_unauthenticated() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // Wait for the auth button rendered in the top-right dropdown container.
    assert!(
        wait_for_selector(&mount, ".generic-sign-in-button", 2000).await,
        "Timed out waiting for sign-in button (.generic-sign-in-button)"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("Sign in"),
        "Sign-in button text should be shown"
    );

    assert!(
        mount
            .query_selector(".generic-sign-in-button")
            .unwrap()
            .is_some(),
        "Sign-in button should be rendered"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_hides_login_when_authenticated() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_meetings_empty();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // Wait for the auth effect to resolve the mocked session/profile and for
    // the meetings fetch to render the empty authenticated state.
    assert!(
        wait_for_selector(&mount, ".meetings-empty", 2000).await,
        "Timed out waiting for empty meetings state (.meetings-empty)"
    );

    // The top-right sign-in button should NOT be visible once a profile loads.
    assert!(
        mount
            .query_selector(".generic-sign-in-button")
            .unwrap()
            .is_none(),
        "Sign-in button should NOT be visible when the user is authenticated"
    );

    // Should show the empty meetings state instead.
    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("No meetings yet"),
        "Empty meetings message should be shown when authenticated"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

// ---------------------------------------------------------------------------
// Signed-in display name (issue 1646)
// ---------------------------------------------------------------------------

fn local_storage() -> web_sys::Storage {
    gloo_utils::window().local_storage().unwrap().unwrap()
}

const NAME_KEY: &str = "vc_display_name";
const OWNER_KEY: &str = "vc_display_name_uid";
const PROFILE_USER_ID: &str = "test-user@example.com";
const NAME_ERROR: &str = "label[for='username'] .field-label__error";

fn profile_owner() -> Option<String> {
    Some(display_name_owner_id(PROFILE_USER_ID))
}

fn stored(key: &str) -> Option<String> {
    local_storage().get_item(key).unwrap()
}

fn store(key: &str, value: &str) {
    local_storage().set_item(key, value).unwrap();
}

fn username_value(mount: &web_sys::Element) -> String {
    query(mount, "#username")
        .dyn_into::<HtmlInputElement>()
        .unwrap()
        .value()
}

/// Polls `done` for up to ~5 s; returns whether it became true.
async fn poll_until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if done() {
            return true;
        }
        sleep_ms(50).await;
    }
    done()
}

/// Every request answers 200 and is recorded in `window.__fetch_urls`.
/// `/profile` returns [`PROFILE_USER_ID`] named `profile_name`, or never
/// answers when that is `None`; with `hold_session`, `/session` answers only
/// once `window.__resolve_session()` is called.
fn mock_fetch_signed_in(profile_name: Option<&str>, hold_session: bool) {
    let window = gloo_utils::window();
    let name = profile_name.map_or(wasm_bindgen::JsValue::NULL, Into::into);
    js_sys::Reflect::set(&window, &"__profile_name".into(), &name).unwrap();
    js_sys::Reflect::set(&window, &"__hold_session".into(), &hold_session.into()).unwrap();
    js_sys::Reflect::set(
        &window,
        &"__profile_user_id".into(),
        &PROFILE_USER_ID.into(),
    )
    .unwrap();
    js_sys::eval(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.__fetch_urls = [];
        window.__profile_reads = 0;
        var held = window.__hold_session
            ? new Promise(function(resolve) { window.__resolve_session = resolve; })
            : Promise.resolve();
        window.fetch = function(input) {
            var url = typeof input === 'string' ? input : input.url;
            window.__fetch_urls.push(url);
            var isProfile = /\/profile$/.test(url);
            if (isProfile && window.__profile_name === null) {
                return new Promise(function() {});
            }
            var result = isProfile
                ? { user_id: window.__profile_user_id, name: window.__profile_name }
                : { meetings: [] };
            var respond = function() {
                var resp = new Response(JSON.stringify({ success: true, result: result }), {
                    status: 200,
                    headers: { 'Content-Type': 'application/json' }
                });
                Object.defineProperty(resp, 'url', { value: url });
                if (isProfile) {
                    ['text', 'json', 'arrayBuffer'].forEach(function(read) {
                        var inner = resp[read].bind(resp);
                        resp[read] = function() {
                            return inner().then(function(body) {
                                window.__profile_reads = (window.__profile_reads || 0) + 1;
                                return body;
                            });
                        };
                    });
                }
                return resp;
            };
            return /\/session$/.test(url) ? held.then(respond) : Promise.resolve(respond());
        };
        "#,
    )
    .expect("failed to mock a signed-in fetch");
}

/// Records every fetched URL in `window.__fetch_urls` on top of the current mock.
fn record_fetch_urls() {
    js_sys::eval(
        r#"
        window.__fetch_urls = [];
        var inner = window.fetch;
        window.fetch = function(input, init) {
            window.__fetch_urls.push(typeof input === 'string' ? input : input.url);
            return inner(input, init);
        };
        "#,
    )
    .expect("failed to record fetch URLs");
}

fn requests_ending_with(suffix: &str) -> u32 {
    js_sys::eval(&format!(
        "(window.__fetch_urls || []).filter(function(u) {{ return u.endsWith('{suffix}'); }}).length"
    ))
    .unwrap()
    .as_f64()
    .unwrap_or_default() as u32
}

async fn mount_home_with_oauth(root: fn() -> Element) -> web_sys::Element {
    ensure_root_url();
    inject_app_config_oauth_enabled();
    let mount = create_mount_point();
    render_into(&mount, root);
    assert!(
        wait_for_selector(&mount, "#username", 5000).await,
        "Timed out waiting for Home page to render (#username)"
    );
    mount
}

async fn wait_for_profile(mount: &web_sys::Element) {
    assert!(
        wait_for_selector(mount, ".auth-dropdown-trigger", 5000).await,
        "Timed out waiting for the signed-in profile (.auth-dropdown-trigger)"
    );
}

/// Mounts Home signed in as "Test User" and returns `#username` once the
/// profile has loaded.
async fn signed_in_home_username() -> String {
    mock_fetch_signed_in(Some("Test User"), false);
    let mount = mount_home_with_oauth(home_wrapper_direct).await;
    wait_for_profile(&mount).await;
    let value = username_value(&mount);
    cleanup(&mount);
    value
}

#[wasm_bindgen_test]
async fn signed_in_home_shows_the_stored_display_name_over_the_profile_name() {
    reset_test_browser_state();
    store(NAME_KEY, "Tony gMail");

    assert_eq!(signed_in_home_username().await, "Tony gMail");
    assert_eq!(stored(NAME_KEY).as_deref(), Some("Tony gMail"));
    assert_eq!(stored(OWNER_KEY), profile_owner());

    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_in_home_without_a_stored_name_uses_and_saves_the_profile_name() {
    reset_test_browser_state();

    assert_eq!(signed_in_home_username().await, "Test User");
    assert_eq!(stored(NAME_KEY).as_deref(), Some("Test User"));
    assert_eq!(stored(OWNER_KEY), profile_owner());

    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_in_home_ignores_a_stored_name_saved_for_another_user() {
    reset_test_browser_state();
    store(NAME_KEY, "Tony gMail");
    store(
        OWNER_KEY,
        &display_name_owner_id("someone-else@example.com"),
    );

    assert_eq!(signed_in_home_username().await, "Test User");
    assert_eq!(stored(NAME_KEY).as_deref(), Some("Test User"));
    assert_eq!(stored(OWNER_KEY), profile_owner());

    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_in_home_forgets_another_users_name_even_when_the_profile_name_is_invalid() {
    reset_test_browser_state();
    store(NAME_KEY, "Tony gMail");
    store(
        OWNER_KEY,
        &display_name_owner_id("someone-else@example.com"),
    );
    mock_fetch_signed_in(Some("Antonio Estrada (Tony)"), true);

    let mount = mount_home_with_oauth(exposed_home_wrapper).await;
    assert_eq!(exposed_display_name(), Some("Tony gMail".to_string()));
    js_sys::eval("window.__resolve_session()").unwrap();
    wait_for_profile(&mount).await;

    assert_eq!(username_value(&mount), "Antonio Estrada (Tony)");
    assert_eq!(text_of(&mount, NAME_ERROR), "Not allowed: '(', ')'");
    assert_eq!(stored(NAME_KEY), None);
    assert_eq!(stored(OWNER_KEY), None);
    assert_eq!(exposed_display_name(), None);

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_in_home_clears_an_error_left_before_the_profile_loads() {
    reset_test_browser_state();
    mock_fetch_signed_in(Some("Test User"), true);

    let mount = mount_home_with_oauth(home_wrapper_direct).await;
    assert!(poll_until(|| requests_ending_with("/session") == 1).await);
    query(&mount, "form")
        .dispatch_event(&bubbling_submit_event())
        .unwrap();
    assert!(poll_until(|| text_of(&mount, NAME_ERROR) == "Name cannot be empty.").await);
    js_sys::eval("window.__resolve_session()").unwrap();
    wait_for_profile(&mount).await;

    assert_eq!(username_value(&mount), "Test User");
    assert_eq!(text_of(&mount, NAME_ERROR), "");

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_in_home_records_a_corrected_name_as_the_users() {
    reset_test_browser_state();
    mock_fetch_signed_in(Some("Antonio Estrada (Tony)"), false);

    let mount = mount_home_with_oauth(home_wrapper_direct).await;
    wait_for_profile(&mount).await;
    assert_eq!(stored(NAME_KEY), None);

    type_into(&mount, "#username", "Antonio Estrada");
    query(&mount, "form button[type='button']")
        .dyn_into::<HtmlElement>()
        .unwrap()
        .click();

    assert!(poll_until(|| stored(NAME_KEY).is_some()).await);
    assert_eq!(stored(NAME_KEY).as_deref(), Some("Antonio Estrada"));
    assert_eq!(stored(OWNER_KEY), profile_owner());

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_in_home_join_records_a_corrected_name_as_the_users() {
    reset_test_browser_state();
    mock_fetch_signed_in(Some("Antonio Estrada (Tony)"), false);

    let mount = mount_home_with_oauth(home_wrapper_direct).await;
    wait_for_profile(&mount).await;
    type_into(&mount, "#username", "Antonio Estrada");
    type_into(&mount, "#meeting-id", "room1");
    query(&mount, "form")
        .dispatch_event(&bubbling_submit_event())
        .unwrap();

    assert!(poll_until(|| stored(NAME_KEY).is_some()).await);
    assert_eq!(stored(NAME_KEY).as_deref(), Some("Antonio Estrada"));
    assert_eq!(stored(OWNER_KEY), profile_owner());

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn a_name_saved_before_the_profile_loads_is_kept_once_it_loads() {
    reset_test_browser_state();
    store(NAME_KEY, "Tony gMail");
    store(
        OWNER_KEY,
        &display_name_owner_id("someone-else@example.com"),
    );
    mock_fetch_signed_in(Some("Test User"), true);

    let mount = mount_home_with_oauth(home_wrapper_direct).await;
    assert!(poll_until(|| requests_ending_with("/session") == 1).await);
    type_into(&mount, "#username", "Typed Name");
    query(&mount, "form button[type='button']")
        .dyn_into::<HtmlElement>()
        .unwrap()
        .click();
    assert!(poll_until(|| stored(NAME_KEY).as_deref() == Some("Typed Name")).await);
    assert_eq!(stored(OWNER_KEY), None);

    js_sys::eval("window.__resolve_session()").unwrap();
    wait_for_profile(&mount).await;

    assert_eq!(stored(NAME_KEY).as_deref(), Some("Typed Name"));
    assert_eq!(username_value(&mount), "Typed Name");

    cleanup(&mount);
    reset_test_browser_state();
}

const MEETING_NAME_INPUT: &str = "input[placeholder='Enter your display name']";

async fn mount_signed_in_meeting_page(profile_name: &str) -> web_sys::Element {
    set_url("/meeting/abc");
    inject_app_config_oauth_enabled();
    mock_fetch_signed_in(Some(profile_name), false);
    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    assert!(poll_until(|| requests_ending_with("/profile") == 1).await);
    yield_now().await;
    mount
}

#[wasm_bindgen_test]
async fn signed_in_meeting_page_records_the_profile_name_as_the_users() {
    reset_test_browser_state();

    let mount = mount_signed_in_meeting_page("Test User").await;

    assert!(poll_until(|| stored(OWNER_KEY).is_some()).await);
    assert_eq!(stored(NAME_KEY).as_deref(), Some("Test User"));
    assert_eq!(stored(OWNER_KEY), profile_owner());

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_in_meeting_form_records_the_typed_name_as_the_users() {
    reset_test_browser_state();

    let mount = mount_signed_in_meeting_page("Antonio Estrada (Tony)").await;
    assert!(wait_for_selector(&mount, MEETING_NAME_INPUT, 5000).await);
    assert!(
        poll_until(|| js_sys::eval("window.__profile_reads")
            .ok()
            .and_then(|reads| reads.as_f64())
            .is_some_and(|reads| reads >= 1.0))
        .await,
        "the /profile body was never read"
    );
    yield_now().await;
    yield_now().await;
    assert_eq!(stored(NAME_KEY), None);
    type_into(&mount, MEETING_NAME_INPUT, "Antonio Estrada");
    query(&mount, "form")
        .dispatch_event(&bubbling_submit_event())
        .unwrap();

    assert!(poll_until(|| stored(OWNER_KEY).is_some()).await);
    assert_eq!(stored(NAME_KEY).as_deref(), Some("Antonio Estrada"));
    assert_eq!(stored(OWNER_KEY), profile_owner());

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_in_home_shows_an_invalid_profile_name_with_its_error_and_saves_nothing() {
    reset_test_browser_state();
    mock_fetch_signed_in(Some("Antonio Estrada (Tony)"), false);

    let mount = mount_home_with_oauth(home_wrapper_direct).await;
    wait_for_profile(&mount).await;

    assert_eq!(username_value(&mount), "Antonio Estrada (Tony)");
    assert_eq!(
        text_of(&mount, "label[for='username'] .field-label__error"),
        "Not allowed: '(', ')'"
    );
    assert_eq!(stored(NAME_KEY), None);
    assert_eq!(stored(OWNER_KEY), None);

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_in_home_keeps_a_name_typed_before_the_profile_loads() {
    reset_test_browser_state();
    mock_fetch_signed_in(Some("Test User"), true);

    let mount = mount_home_with_oauth(home_wrapper_direct).await;
    assert!(poll_until(|| requests_ending_with("/session") == 1).await);
    type_into(&mount, "#username", "Typed Name");
    js_sys::eval("window.__resolve_session()").unwrap();
    wait_for_profile(&mount).await;

    assert_eq!(username_value(&mount), "Typed Name");
    assert_eq!(stored(NAME_KEY), None);
    assert_eq!(stored(OWNER_KEY), None);

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn signed_out_home_leaves_the_display_name_empty() {
    reset_test_browser_state();
    mock_fetch_401();
    record_fetch_urls();
    store(NAME_KEY, "Tony gMail");

    let mount = mount_home_with_oauth(home_wrapper_direct).await;
    assert!(poll_until(|| requests_ending_with("/session") == 1).await);
    sleep_ms(300).await;
    assert_eq!(username_value(&mount), "");

    cleanup(&mount);
    reset_test_browser_state();
}

thread_local! {
    static ROUTER_SHOWN: std::cell::Cell<Option<Signal<bool>>> = const { std::cell::Cell::new(None) };
    static DISPLAY_NAME: std::cell::Cell<Option<Signal<Option<String>>>> = const { std::cell::Cell::new(None) };
}

fn exposed_display_name() -> Option<String> {
    DISPLAY_NAME.with(|slot| slot.get()).unwrap().peek().clone()
}

/// [`home_wrapper_direct`] with `DisplayNameCtx` loaded from storage and
/// exposed in `DISPLAY_NAME`; the Router (and so Home) unmounts when the
/// signal in `ROUTER_SHOWN` is set to `false`.
fn exposed_home_wrapper() -> Element {
    let shown = use_signal(|| true);
    ROUTER_SHOWN.with(|slot| slot.set(Some(shown)));

    let username_signal = use_signal(load_display_name_from_storage);
    DISPLAY_NAME.with(|slot| slot.set(Some(username_signal)));
    use_context_provider(|| DisplayNameCtx(username_signal));

    let transport_pref = use_signal(load_transport_preference);
    use_context_provider(|| TransportPreferenceCtx(transport_pref));

    let search_visible = use_signal(|| false);
    use_context_provider(|| SearchVisibleCtx {
        is_visible: search_visible,
    });

    rsx! {
        if shown() {
            Router::<dioxus_ui::routing::Route> {}
        }
    }
}

#[wasm_bindgen_test]
async fn home_session_check_stops_when_home_unmounts() {
    reset_test_browser_state();
    mock_fetch_signed_in(None, true);

    let mount = mount_home_with_oauth(exposed_home_wrapper).await;
    assert!(poll_until(|| requests_ending_with("/session") == 1).await);

    ROUTER_SHOWN.with(|slot| slot.get()).unwrap().set(false);
    assert!(
        poll_until(|| mount.query_selector("#username").unwrap().is_none()).await,
        "Home should have unmounted"
    );

    js_sys::eval("window.__resolve_session()").unwrap();
    sleep_ms(300).await;
    assert_eq!(requests_ending_with("/profile"), 0);

    cleanup(&mount);
    reset_test_browser_state();
}

/// With no `__APP_CONFIG` neither entry point can build a login URL, so they
/// return without navigating the test page away.
#[wasm_bindgen_test]
fn starting_sign_in_forgets_the_stored_display_name_and_its_owner() {
    reset_test_browser_state();

    let sign_ins: [(&str, fn()); 2] = [
        ("do_login", dioxus_ui::auth::do_login),
        ("redirect_to_login", dioxus_ui::auth::redirect_to_login),
    ];
    for (label, sign_in) in sign_ins {
        store(NAME_KEY, "Guest Name");
        store(OWNER_KEY, "someone@example.com");
        sign_in();
        assert_eq!(stored(NAME_KEY), None, "{label}");
        assert_eq!(stored(OWNER_KEY), None, "{label}");
    }

    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn guest_join_records_the_name_as_a_guests() {
    reset_test_browser_state();
    set_url("/meeting/abc/guest");
    inject_app_config();
    mock_fetch_invalid_meeting_id();

    let mount = create_mount_point();
    render_into(&mount, named_user_wrapper);
    assert!(
        wait_for_selector(&mount, "#guest-name", 5000).await,
        "Timed out waiting for the guest form"
    );
    type_into(&mount, "#guest-name", "Tester");
    yield_now().await;
    query(&mount, "form")
        .dispatch_event(&bubbling_submit_event())
        .unwrap();
    assert!(
        wait_for_selector(&mount, INVALID_ID_CARD, 5000).await,
        "Timed out waiting for the join response"
    );

    assert_eq!(stored(NAME_KEY).as_deref(), Some("Tester"));
    assert_eq!(stored(OWNER_KEY).as_deref(), Some(GUEST_DISPLAY_NAME_OWNER));

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn missing_config_shows_error_not_home() {
    reset_test_browser_state();
    remove_app_config();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    // ConfigError renders synchronously (no Router needed), but still wait
    // for at least the initial Dioxus flush.
    assert!(
        wait_for_selector(&mount, ".error-container", 2000).await,
        "Timed out waiting for ConfigError (.error-container)"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("__APP_CONFIG"),
        "Error message should mention the missing config"
    );

    // Home should NOT have rendered.
    assert!(
        mount.query_selector(".hero-container").unwrap().is_none(),
        "Home page should not render when config is missing"
    );

    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_rejects_invalid_display_name() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_meetings_empty();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    assert!(
        wait_for_selector(&mount, "#username", 2000).await,
        "Timed out waiting for Home page to render"
    );

    let username = mount
        .query_selector("#username")
        .unwrap()
        .unwrap()
        .dyn_into::<HtmlInputElement>()
        .unwrap();
    username.set_value("John&Doe");
    username.dispatch_event(&bubbling_input_event()).unwrap();

    let meeting_id = mount
        .query_selector("#meeting-id")
        .unwrap()
        .unwrap()
        .dyn_into::<HtmlInputElement>()
        .unwrap();
    meeting_id.set_value("abc_123");
    meeting_id.dispatch_event(&bubbling_input_event()).unwrap();

    // Yield so Dioxus processes the oninput state updates before submit reads them.
    yield_now().await;

    let form = mount.query_selector("form").unwrap().unwrap();
    form.dispatch_event(&bubbling_submit_event()).unwrap();

    yield_now().await;

    assert_eq!(
        text_of(&mount, "label[for='username'] .field-label__error"),
        "Not allowed: '&'"
    );
    let text = mount.text_content().unwrap_or_default();
    assert!(
        !text.contains("Invalid character"),
        "submit must keep the inline format, got page text: {text}"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_normalizes_spaces_in_display_name() {
    assert_eq!(
        validate_display_name("  John    Doe   ").unwrap(),
        "John Doe"
    );
}

#[wasm_bindgen_test]
async fn home_rejects_empty_display_name() {
    assert!(
        validate_display_name("   ")
            .unwrap_err()
            .contains("Name cannot be empty"),
        "Expected empty-name validation error"
    );
}

#[wasm_bindgen_test]
async fn home_rejects_display_name_exceeding_max_length() {
    let long_name = "A".repeat(51);
    assert!(
        validate_display_name(&long_name)
            .unwrap_err()
            .contains("too long"),
        "Expected max-length validation error"
    );
}

#[wasm_bindgen_test]
async fn home_accepts_display_name_with_special_characters() {
    assert!(validate_display_name("O'Brien-Smith").is_ok());
}

#[wasm_bindgen_test]
async fn home_shows_create_button_when_no_meeting_id() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    assert!(
        mount
            .query_selector("button[type='submit']")
            .unwrap()
            .is_none(),
        "Join button should not render until a meeting ID is entered"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("Generate a New Meeting ID"),
        "Create button should always be visible"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_join_button_enabled_when_meeting_id_entered() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);

    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    // Enter a meeting ID.
    let meeting_input = mount
        .query_selector("#meeting-id")
        .unwrap()
        .unwrap()
        .dyn_into::<HtmlInputElement>()
        .unwrap();
    meeting_input.set_value("test_meeting");
    meeting_input
        .dispatch_event(&bubbling_input_event())
        .unwrap();

    yield_now().await;

    // The submit button should now be rendered.
    assert!(
        mount
            .query_selector("button[type='submit']")
            .unwrap()
            .is_some(),
        "Join button should render when meeting ID is entered"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("Start or Join Meeting"),
        "Join button label should be shown when meeting ID is entered"
    );

    let _btn = mount
        .query_selector("button[type='submit']")
        .unwrap()
        .expect("submit button should exist");

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

// ---------------------------------------------------------------------------
// Meeting ID rule (issue 2832)
// ---------------------------------------------------------------------------

const INVALID_ID_CARD: &str = "[data-testid='meeting-invalid-id']";
const INVALID_ID_REASON: &str = "[data-testid='meeting-invalid-id-reason']";

fn meeting_id_input(mount: &web_sys::Element) -> HtmlInputElement {
    mount
        .query_selector("#meeting-id")
        .unwrap()
        .expect("#meeting-id input")
        .dyn_into::<HtmlInputElement>()
        .unwrap()
}

fn type_into(mount: &web_sys::Element, selector: &str, value: &str) {
    let input = mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} missing"))
        .dyn_into::<HtmlInputElement>()
        .unwrap();
    input.set_value(value);
    input.dispatch_event(&bubbling_input_event()).unwrap();
}

fn text_of(mount: &web_sys::Element, selector: &str) -> String {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} missing"))
        .text_content()
        .unwrap_or_default()
}

fn set_url(path: &str) {
    let _ = gloo_utils::window().history().unwrap().push_state_with_url(
        &wasm_bindgen::JsValue::NULL,
        "",
        Some(path),
    );
}

/// Every request gets the meeting API's 400 `INVALID_MEETING_ID`; each URL
/// is recorded in `window.__fetch_urls`.
fn mock_fetch_invalid_meeting_id() {
    js_sys::eval(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.__fetch_urls = [];
        window.fetch = function(input) {
            var url = typeof input === 'string' ? input : input.url;
            window.__fetch_urls.push(url);
            var body = {
                success: false,
                result: { code: 'INVALID_MEETING_ID', message: 'Invalid meeting ID: refused by server' }
            };
            var resp = new Response(JSON.stringify(body), {
                status: 400,
                headers: { 'Content-Type': 'application/json' }
            });
            Object.defineProperty(resp, 'url', { value: url });
            return Promise.resolve(resp);
        };
        "#,
    )
    .expect("failed to mock fetch with INVALID_MEETING_ID");
}

fn join_requests() -> u32 {
    js_sys::eval(
        "(window.__fetch_urls || []).filter(function(u) { return /\\/join(-guest)?$/.test(u); }).length",
    )
    .unwrap()
    .as_f64()
    .unwrap_or_default() as u32
}

fn fetch_requests() -> u32 {
    js_sys::eval("(window.__fetch_urls || []).length")
        .unwrap()
        .as_f64()
        .unwrap_or_default() as u32
}

fn query(mount: &web_sys::Element, selector: &str) -> web_sys::Element {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} missing"))
}

fn active_element_id() -> String {
    gloo_utils::document()
        .active_element()
        .map(|el| el.id())
        .unwrap_or_default()
}

/// Focus the submit button, as a click would, then submit the form.
fn submit_from_button(mount: &web_sys::Element) {
    query(mount, "button[type='submit']")
        .dyn_into::<HtmlElement>()
        .unwrap()
        .focus()
        .unwrap();
    query(mount, "form")
        .dispatch_event(&bubbling_submit_event())
        .unwrap();
}

async fn sleep_ms(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

/// Router shell with a display name already set, so `MeetingPage` auto-joins.
fn named_user_wrapper() -> Element {
    let username_signal = use_signal(|| Some("Tester".to_string()));
    use_context_provider(|| DisplayNameCtx(username_signal));

    let transport_pref = use_signal(load_transport_preference);
    use_context_provider(|| TransportPreferenceCtx(transport_pref));

    let search_visible = use_signal(|| false);
    use_context_provider(|| SearchVisibleCtx {
        is_visible: search_visible,
    });

    rsx! {
        Router::<dioxus_ui::routing::Route> {}
    }
}

#[wasm_bindgen_test]
async fn home_meeting_id_field_mirrors_the_shared_rule() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    type_into(&mount, "#meeting-id", "team-sync~2");
    yield_now().await;

    let input = meeting_id_input(&mount);
    assert_eq!(text_of(&mount, "#meeting-id-error"), "");
    assert_eq!(
        input.get_attribute("aria-invalid").as_deref(),
        Some("false")
    );
    assert!(
        input.get_attribute("pattern").is_none(),
        "a pattern attribute would block IDs the shared rule accepts"
    );
    assert_eq!(
        input.get_attribute("maxlength"),
        None,
        "maxlength silently truncates a long paste into a different meeting ID"
    );

    let tip = text_of(&mount, "#meeting-id-info-tip");
    assert!(
        tip.contains(&format!("Allowed: {MEETING_ID_ALLOWED_CHARS}.")),
        "tooltip must name the shared allowed set, got: {tip}"
    );
    assert!(
        tip.contains(&format!("Up to {MEETING_ID_MAX_LEN} characters.")),
        "tooltip must name the shared length limit, got: {tip}"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn home_meeting_id_names_the_disallowed_char_and_blocks_submit() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    type_into(&mount, "#username", "Tester");
    type_into(&mount, "#meeting-id", "room.1");
    yield_now().await;

    assert_eq!(text_of(&mount, "#meeting-id-error"), "Not allowed: '.'");
    assert_eq!(
        meeting_id_input(&mount)
            .get_attribute("aria-invalid")
            .as_deref(),
        Some("true")
    );

    let form = mount.query_selector("form").unwrap().unwrap();
    form.dispatch_event(&bubbling_submit_event()).unwrap();
    yield_now().await;
    sleep_ms(100).await;

    assert_eq!(
        gloo_utils::window().location().pathname().unwrap(),
        "/",
        "an invalid meeting ID must not navigate"
    );
    assert_eq!(text_of(&mount, "#meeting-id-error"), "Not allowed: '.'");

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn invalid_meeting_id_submit_focuses_the_field_and_reinserts_the_error() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    type_into(&mount, "#username", "Tester");
    type_into(&mount, "#meeting-id", "room.1");
    yield_now().await;
    let region = query(&mount, "#meeting-id-error");
    let typed = query(&mount, "#meeting-id-error > span");
    assert_eq!(typed.text_content().as_deref(), Some("Not allowed: '.'"));

    submit_from_button(&mount);
    yield_now().await;
    sleep_ms(50).await;

    assert_eq!(
        active_element_id(),
        "meeting-id",
        "focus must move to the invalid field so its error is read"
    );
    let submitted = query(&mount, "#meeting-id-error > span");
    assert_eq!(
        submitted.text_content(),
        typed.text_content(),
        "precondition: the submit error must repeat the typed error"
    );
    assert!(
        region.is_same_node(Some(&query(&mount, "#meeting-id-error"))),
        "the aria-live region itself must persist"
    );
    assert!(
        !typed.is_same_node(Some(&submitted)),
        "an identical message diffed in place is never announced"
    );

    query(&mount, "form")
        .dispatch_event(&bubbling_submit_event())
        .unwrap();
    yield_now().await;
    sleep_ms(50).await;
    assert_eq!(active_element_id(), "meeting-id");
    assert!(
        !submitted.is_same_node(Some(&query(&mount, "#meeting-id-error > span"))),
        "a resubmit with focus already in the field must re-insert the message again"
    );
    assert_eq!(gloo_utils::window().location().pathname().unwrap(), "/");

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn submit_with_both_fields_invalid_shows_both_errors_and_focuses_the_name() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    type_into(&mount, "#username", "Bob!");
    type_into(&mount, "#meeting-id", "a.b");
    yield_now().await;
    let name_error = "label[for='username'] .field-label__error";
    assert_eq!(text_of(&mount, name_error), "Not allowed: '!'");

    submit_from_button(&mount);
    yield_now().await;
    sleep_ms(50).await;

    assert_eq!(
        text_of(&mount, name_error),
        "Not allowed: '!'",
        "the display-name error must survive an invalid meeting ID in the inline format"
    );
    assert_eq!(text_of(&mount, "#meeting-id-error"), "Not allowed: '.'");
    for input in ["#username", "#meeting-id"] {
        assert_eq!(
            query(&mount, input)
                .get_attribute("aria-invalid")
                .as_deref(),
            Some("true"),
            "{input}"
        );
    }
    assert_eq!(
        active_element_id(),
        "username",
        "focus goes to the first invalid field"
    );
    assert_eq!(gloo_utils::window().location().pathname().unwrap(), "/");

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn invalid_name_submit_focuses_the_name_and_reinserts_its_error() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    type_into(&mount, "#username", "Bob!");
    type_into(&mount, "#meeting-id", "abc");
    yield_now().await;
    let region_selector = "label[for='username'] .field-label__error";
    let message_selector = "label[for='username'] .field-label__error > span";
    let region = query(&mount, region_selector);
    let typed = query(&mount, message_selector);
    assert_eq!(typed.text_content().as_deref(), Some("Not allowed: '!'"));

    submit_from_button(&mount);
    yield_now().await;
    sleep_ms(50).await;

    assert_eq!(active_element_id(), "username");
    let submitted = query(&mount, message_selector);
    assert_eq!(
        submitted.text_content(),
        typed.text_content(),
        "precondition: the submit error must repeat the typed error"
    );
    assert!(
        region.is_same_node(Some(&query(&mount, region_selector))),
        "the aria-live region itself must persist"
    );
    assert!(
        !typed.is_same_node(Some(&submitted)),
        "an identical message diffed in place is never announced"
    );

    query(&mount, "form")
        .dispatch_event(&bubbling_submit_event())
        .unwrap();
    yield_now().await;
    sleep_ms(50).await;
    assert_eq!(active_element_id(), "username");
    assert!(
        !submitted.is_same_node(Some(&query(&mount, message_selector))),
        "a resubmit with focus already in the field must re-insert the message again"
    );
    assert_eq!(gloo_utils::window().location().pathname().unwrap(), "/");

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn generate_with_an_invalid_name_keeps_the_inline_format() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    assert!(
        wait_for_selector(&mount, "#username", 2000).await,
        "Timed out waiting for Home page to render"
    );

    type_into(&mount, "#username", "Bob!");
    yield_now().await;
    query(&mount, "form button[type='button']")
        .dyn_into::<HtmlElement>()
        .unwrap()
        .click();
    yield_now().await;
    sleep_ms(50).await;

    assert_eq!(
        text_of(&mount, "label[for='username'] .field-label__error"),
        "Not allowed: '!'"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

fn install_app_stylesheets() -> web_sys::Element {
    let doc = gloo_utils::document();
    let style = doc.create_element("style").unwrap();
    // Same sheets, same order as index.html.
    style.set_text_content(Some(&format!(
        "{}{}{}{}",
        include_str!("../static/leptos-style.css"),
        include_str!("../static/tailwind.css"),
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    )));
    doc.head().unwrap().append_child(&style).unwrap();
    style
}

fn computed(el: &web_sys::Element, property: &str) -> String {
    gloo_utils::window()
        .get_computed_style(el)
        .unwrap()
        .unwrap()
        .get_property_value(property)
        .unwrap()
}

#[wasm_bindgen_test]
async fn empty_field_errors_stay_rendered_without_taking_space() {
    reset_test_browser_state();
    ensure_root_url();
    inject_app_config_oauth_enabled();
    mock_fetch_401();
    let style = install_app_stylesheets();
    let root = gloo_utils::document().document_element().unwrap();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    assert!(
        wait_for_selector(&mount, "#meeting-id", 2000).await,
        "Timed out waiting for Home page to render"
    );

    for theme in ["dark", "light"] {
        root.set_attribute("data-theme", theme).unwrap();
        for field in ["username", "meeting-id"] {
            let label = query(&mount, &format!("label[for='{field}']"));
            let error = query(&mount, &format!("label[for='{field}'] .field-label__error"));
            assert!(error.matches(":empty").unwrap(), "{theme} {field}");
            assert_ne!(
                computed(&error, "display"),
                "none",
                "{theme} {field}: a display:none live region is outside the \
                 accessibility tree, so its first message can go unannounced"
            );
            assert_eq!(computed(&error, "animation-name"), "none");
            assert_eq!(error.get_bounding_client_rect().height(), 0.0);

            let rendered = label.get_bounding_client_rect().height();
            let html = error.clone().dyn_into::<HtmlElement>().unwrap();
            html.style().set_property("display", "none").unwrap();
            let removed = label.get_bounding_client_rect().height();
            html.style().remove_property("display").unwrap();
            assert_eq!(rendered, removed, "{theme} {field}: label row height");
        }
    }

    let empty_height = query(&mount, "label[for='meeting-id']")
        .get_bounding_client_rect()
        .height();
    type_into(&mount, "#meeting-id", "a.b");
    yield_now().await;
    assert_eq!(
        computed(&query(&mount, "#meeting-id-error"), "animation-name"),
        "none",
        "the persistent live region must not carry the fade-in"
    );
    assert_eq!(
        computed(&query(&mount, "#meeting-id-error > span"), "animation-name"),
        "field-error-in",
        "the message node, re-inserted on each failed submit, replays the fade-in"
    );
    assert_eq!(
        query(&mount, "label[for='meeting-id']")
            .get_bounding_client_rect()
            .height(),
        empty_height,
        "showing the error must not shift the form"
    );

    root.remove_attribute("data-theme").unwrap();
    style.remove();
    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

/// A computed `rgb(...)` / `rgba(...)` colour as `[r, g, b, alpha]`.
fn parse_rgba(css: &str) -> [f64; 4] {
    let inner = css
        .strip_prefix("rgba(")
        .or_else(|| css.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unexpected computed colour {css:?}"));
    let parts: Vec<f64> = inner
        .split(',')
        .map(|p| p.trim().parse().unwrap())
        .collect();
    [
        parts[0],
        parts[1],
        parts[2],
        parts.get(3).copied().unwrap_or(1.0),
    ]
}

fn composite(top: [f64; 4], below: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|i| top[i] * top[3] + below[i] * (1.0 - top[3]))
}

fn relative_luminance(rgb: [f64; 3]) -> f64 {
    let linear = |v: f64| {
        let v = v / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
}

/// WCAG contrast of `el`'s text over its ancestors' background colours,
/// composited from a white canvas down. Background images are ignored.
fn text_contrast(el: &web_sys::Element) -> f64 {
    let mut layers = Vec::new();
    let mut node = Some(el.clone());
    while let Some(current) = node {
        layers.push(parse_rgba(&computed(&current, "background-color")));
        node = current.parent_element();
    }
    let bg = layers
        .iter()
        .rev()
        .fold([255.0; 3], |below, layer| composite(*layer, below));
    let fg = composite(parse_rgba(&computed(el, "color")), bg);
    let (a, b) = (relative_luminance(fg), relative_luminance(bg));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn assert_legible_in_both_themes(mount: &web_sys::Element, selectors: &[&str]) {
    let root = gloo_utils::document().document_element().unwrap();
    for theme in ["dark", "light"] {
        root.set_attribute("data-theme", theme).unwrap();
        for selector in selectors {
            let ratio = text_contrast(&query(mount, selector));
            assert!(
                ratio >= 4.5,
                "{theme} {selector}: contrast {ratio:.2}:1 is below 4.5:1"
            );
        }
    }
    root.remove_attribute("data-theme").unwrap();
}

#[wasm_bindgen_test]
async fn invalid_id_notice_text_is_legible_in_both_themes() {
    reset_test_browser_state();
    set_url("/meeting/a.b");
    inject_app_config();
    mock_fetch_invalid_meeting_id();
    let style = install_app_stylesheets();

    let mount = create_mount_point();
    render_into(&mount, named_user_wrapper);
    assert!(
        wait_for_selector(&mount, INVALID_ID_CARD, 2000).await,
        "Timed out waiting for the invalid meeting ID notice"
    );
    assert_legible_in_both_themes(
        &mount,
        &[
            "#meeting-invalid-id-heading",
            INVALID_ID_REASON,
            "[data-testid='meeting-invalid-id-hint']",
        ],
    );

    style.remove();
    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn config_error_text_is_legible_in_both_themes() {
    reset_test_browser_state();
    remove_app_config();
    let style = install_app_stylesheets();

    let mount = create_mount_point();
    render_into(&mount, home_wrapper_direct);
    assert!(
        wait_for_selector(&mount, ".error-container", 2000).await,
        "Timed out waiting for ConfigError (.error-container)"
    );
    assert_legible_in_both_themes(
        &mount,
        &[
            ".error-message",
            ".error-container > p:not(.error-message)",
            ".error-container a",
        ],
    );

    style.remove();
    cleanup(&mount);
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn meeting_url_with_invalid_id_shows_notice_without_calling_join() {
    reset_test_browser_state();
    set_url("/meeting/a.b");
    inject_app_config();
    mock_fetch_invalid_meeting_id();

    let mount = create_mount_point();
    render_into(&mount, named_user_wrapper);

    assert!(
        wait_for_selector(&mount, INVALID_ID_CARD, 2000).await,
        "Timed out waiting for the invalid meeting ID notice"
    );
    sleep_ms(200).await;

    let reason = text_of(&mount, INVALID_ID_REASON);
    assert!(
        reason.contains("'.'"),
        "reason must name '.', got: {reason}"
    );
    assert_eq!(
        join_requests(),
        0,
        "an invalid ID must never reach the join API"
    );
    assert_notice_is_announced(&mount, JOIN_HINT);

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

/// Focus lands on the heading, the icon is hidden from assistive tech, and
/// the card says what to do next.
fn assert_notice_is_announced(mount: &web_sys::Element, hint: &str) {
    assert_eq!(active_element_id(), "meeting-invalid-id-heading");
    assert_eq!(
        query(mount, "#meeting-invalid-id-heading")
            .text_content()
            .as_deref(),
        Some("Invalid meeting ID")
    );
    assert_eq!(
        query(mount, &format!("{INVALID_ID_CARD} svg"))
            .get_attribute("aria-hidden")
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        text_of(mount, "[data-testid='meeting-invalid-id-hint']"),
        hint
    );
}

#[wasm_bindgen_test]
async fn settings_url_with_invalid_id_shows_notice_without_calling_the_api() {
    reset_test_browser_state();
    set_url("/meeting/a.b/settings");
    inject_app_config();
    mock_fetch_invalid_meeting_id();

    let mount = create_mount_point();
    render_into(&mount, named_user_wrapper);

    assert!(
        wait_for_selector(&mount, INVALID_ID_CARD, 2000).await,
        "Timed out waiting for the invalid meeting ID notice"
    );
    sleep_ms(200).await;

    let reason = text_of(&mount, INVALID_ID_REASON);
    assert!(
        reason.contains("('.')"),
        "reason must name '.', got: {reason}"
    );
    assert_eq!(
        fetch_requests(),
        0,
        "an invalid ID must never reach the meeting API"
    );
    assert_notice_is_announced(&mount, SETTINGS_HINT);
    assert!(
        !text_of(&mount, "[data-testid='meeting-invalid-id-hint']").contains("ask the host"),
        "the settings route is usually opened by the host"
    );

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn meeting_join_refused_as_invalid_id_by_the_api_shows_the_notice() {
    reset_test_browser_state();
    set_url("/meeting/abc");
    inject_app_config();
    mock_fetch_invalid_meeting_id();

    let mount = create_mount_point();
    render_into(&mount, named_user_wrapper);

    assert!(
        wait_for_selector(&mount, INVALID_ID_CARD, 3000).await,
        "Timed out waiting for the invalid meeting ID notice"
    );
    assert_eq!(
        text_of(&mount, INVALID_ID_REASON),
        "Meeting ID refused by server."
    );
    assert!(
        join_requests() >= 1,
        "the notice must come from the join response"
    );
    sleep_ms(100).await;
    assert_notice_is_announced(&mount, JOIN_HINT);

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn guest_url_with_invalid_id_shows_notice_instead_of_the_form() {
    reset_test_browser_state();
    set_url("/meeting/a%20b/guest");
    inject_app_config();
    mock_fetch_invalid_meeting_id();

    let mount = create_mount_point();
    render_into(&mount, named_user_wrapper);

    assert!(
        wait_for_selector(&mount, INVALID_ID_CARD, 2000).await,
        "Timed out waiting for the invalid meeting ID notice"
    );
    assert!(
        mount.query_selector("#guest-name").unwrap().is_none(),
        "the guest form must not render for an invalid ID"
    );
    let reason = text_of(&mount, INVALID_ID_REASON);
    assert!(
        reason.contains("(space);"),
        "reason must name the space readably, got: {reason}"
    );
    sleep_ms(100).await;
    assert_notice_is_announced(&mount, JOIN_HINT);

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}

#[wasm_bindgen_test]
async fn guest_join_refused_as_invalid_id_by_the_api_shows_the_notice() {
    reset_test_browser_state();
    set_url("/meeting/abc/guest");
    inject_app_config();
    mock_fetch_invalid_meeting_id();

    let mount = create_mount_point();
    render_into(&mount, named_user_wrapper);
    assert!(
        wait_for_selector(&mount, "#guest-name", 2000).await,
        "Timed out waiting for the guest form"
    );

    type_into(&mount, "#guest-name", "Tester");
    yield_now().await;
    let form = mount.query_selector("form").unwrap().unwrap();
    form.dispatch_event(&bubbling_submit_event()).unwrap();

    assert!(
        wait_for_selector(&mount, INVALID_ID_CARD, 3000).await,
        "Timed out waiting for the invalid meeting ID notice"
    );
    assert_eq!(
        text_of(&mount, INVALID_ID_REASON),
        "Meeting ID refused by server."
    );
    assert!(
        join_requests() >= 1,
        "the notice must come from the join response"
    );
    sleep_ms(100).await;
    assert_notice_is_announced(&mount, JOIN_HINT);

    cleanup(&mount);
    restore_fetch();
    remove_app_config();
    reset_test_browser_state();
}
