// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::{Cell, RefCell};

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::TILE_MENU_OPEN_CLASS;
use dioxus_ui::components::captions::{
    remember_room_token, CaptionBoard, CAPTIONS_OFF_HINT, CAPTIONS_SHOW_PREF_KEY,
    CAPTION_FINAL_LINGER_MS, MAX_CAPTION_LINES,
};
use dioxus_ui::components::captions_overlay::{
    ingest_caption, now_ms, use_captions, CaptionsHandle, CaptionsLanguagePicker, CaptionsLayer,
    CaptionsLiveIndicator, CaptionsOverlay,
};
use dioxus_ui::components::co_hosts::{CoHostNotice, CoHostNoticeCtx, CoHostNoticeLayer};
use dioxus_ui::components::video_control_buttons::CaptionsButton;
use dioxus_ui::console_log_collector::set_console_log_context;
use support::{
    cleanup, create_mount_point, inject_app_config, render_into, reset_test_browser_state,
    restore_fetch, yield_now,
};
use videocall_client::{CaptionEvent, CaptionStatus, CaptionTranslation, CaptionUpdate};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const RENEW_MS: u32 = 200;
const STATUS_MS: u32 = 300;

thread_local! {
    static BOARD: RefCell<Option<Signal<CaptionBoard>>> = const { RefCell::new(None) };
    static LANGUAGE: RefCell<Option<Signal<String>>> = const { RefCell::new(None) };
    static HANDLE: RefCell<Option<CaptionsHandle>> = const { RefCell::new(None) };
    static IS_HOST: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    static MOUNTED: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    static START_AS_HOST: Cell<bool> = const { Cell::new(false) };
    static LANE_HIDDEN: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    static FLAG: Cell<bool> = const { Cell::new(false) };
    static E2EE: Cell<bool> = const { Cell::new(false) };
    static PROBE_RENDERS: Cell<u32> = const { Cell::new(0) };
    static MEETING_RENDERS: Cell<u32> = const { Cell::new(0) };
    static RENEW_INTERVAL: Cell<u32> = const { Cell::new(RENEW_MS) };
}

async fn sleep_ms(ms: u32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        gloo_utils::window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms as i32)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

fn event(session: u64, utterance: u64, revision: u32, is_final: bool, text: &str) -> CaptionEvent {
    CaptionEvent {
        speaker_session_id: session,
        speaker_name: format!("Speaker {session}"),
        is_local: false,
        utterance_id: utterance,
        revision,
        is_final,
        text: text.to_string(),
        text_language: if is_final { "en" } else { "it" }.to_string(),
        source_text: String::new(),
        source_language: String::new(),
        timestamp_ms: 0,
        translations: vec![],
    }
}

fn translated(mut e: CaptionEvent, pairs: &[(&str, &str)]) -> CaptionEvent {
    e.translations = pairs
        .iter()
        .map(|(language, text)| CaptionTranslation {
            language: language.to_string(),
            text: text.to_string(),
        })
        .collect();
    e
}

fn set_language(code: &str) {
    let mut l = LANGUAGE.with(|c| c.borrow().expect("language signal"));
    l.set(code.to_string());
}

fn active(name: &str) -> CaptionUpdate {
    CaptionUpdate::Status(CaptionStatus {
        active: true,
        started_by_user_id: "host@example.com".to_string(),
        started_by_name: name.to_string(),
        started_by_local: false,
    })
}

fn active_by_me() -> CaptionUpdate {
    CaptionUpdate::Status(CaptionStatus {
        active: true,
        started_by_user_id: "me@example.com".to_string(),
        started_by_name: "Me".to_string(),
        started_by_local: true,
    })
}

fn stopped() -> CaptionUpdate {
    CaptionUpdate::Status(CaptionStatus {
        active: false,
        started_by_user_id: String::new(),
        started_by_name: String::new(),
        started_by_local: false,
    })
}

fn push(e: CaptionEvent) {
    let board = BOARD.with(|b| b.borrow().expect("board signal"));
    ingest_caption(board, e, now_ms());
}

fn handle() -> CaptionsHandle {
    HANDLE.with(|h| h.borrow().clone().expect("captions handle"))
}

fn deliver(update: CaptionUpdate) {
    handle().on_update(update);
}

fn set(cell: &'static std::thread::LocalKey<RefCell<Option<Signal<bool>>>>, v: bool) {
    let mut s = cell.with(|c| c.borrow().expect("signal"));
    s.set(v);
}

#[component]
fn BoardProbe(board: Signal<CaptionBoard>) -> Element {
    let n = board.read().lines().len();
    PROBE_RENDERS.with(|c| c.set(c.get() + 1));
    rsx! { span { "{n}" } }
}

#[allow(non_snake_case)]
fn OverlayHarness() -> Element {
    let board = use_signal(CaptionBoard::default);
    let language = use_signal(|| "en".to_string());
    use_hook(move || {
        BOARD.with(|b| *b.borrow_mut() = Some(board));
        LANGUAGE.with(|l| *l.borrow_mut() = Some(language));
    });
    rsx! {
        CaptionsOverlay { board, language }
        BoardProbe { board }
    }
}

/// The production pieces wired the way `AttendantsComponent` wires them.
#[component]
fn MeetingCaptions() -> Element {
    MEETING_RENDERS.with(|c| c.set(c.get() + 1));
    let is_host = use_signal(|| START_AS_HOST.with(Cell::get));
    let is_host_read: ReadSignal<bool> = use_hook(|| is_host.into());
    let captions = use_captions(
        FLAG.with(Cell::get),
        E2EE.with(Cell::get),
        is_host_read,
        STATUS_MS,
    );
    let hidden = use_signal(|| false);
    let lane_hidden = use_memo(move || hidden());
    use_hook(move || LANE_HIDDEN.with(|h| *h.borrow_mut() = Some(hidden)));
    {
        let captions = captions.clone();
        use_hook(move || {
            IS_HOST.with(|c| *c.borrow_mut() = Some(is_host));
            BOARD.with(|b| *b.borrow_mut() = Some(captions.board));
            HANDLE.with(|h| *h.borrow_mut() = Some(captions));
        });
    }
    let button = captions.available().then(|| captions.button_state());
    let toggle = captions.clone();
    rsx! {
        if let Some(starter) = (captions.notice)() {
            CaptionsLiveIndicator { starter }
            if captions.available() {
                CaptionsLanguagePicker { captions: captions.clone() }
            }
        }
        if let Some(state) = button {
            CaptionsButton { state, onclick: move |_| toggle.toggle("m1") }
        }
        CaptionsLayer {
            meeting_id: "m1".to_string(),
            captions: captions.clone(),
            lane_hidden,
            renew_interval_ms: Some(RENEW_INTERVAL.with(Cell::get)),
        }
    }
}

#[allow(non_snake_case)]
fn MeetingHarness() -> Element {
    let mounted = use_signal(|| true);
    use_hook(move || MOUNTED.with(|b| *b.borrow_mut() = Some(mounted)));
    let notice: Signal<Option<CoHostNotice>> = use_signal(|| None);
    use_context_provider(|| CoHostNoticeCtx(notice));
    rsx! {
        if mounted() {
            MeetingCaptions {}
        }
        CoHostNoticeLayer { notice }
    }
}

/// Records every `/captions` POST as `"<Authorization> <body>"`; answers the
/// n-th with `replies[n]` (the last one repeats), a status plus an error
/// envelope carrying the code. A `/status` GET (the room token refresh) is
/// counted and answered `window.__statusReply` (a status, or one per GET), by
/// default 200 with `room-tok-2`. Tokens handed to the console-log collector
/// land in `window.__logTokens`.
fn mock_captions_api(replies: &[(u16, &str)]) {
    let replies = replies
        .iter()
        .map(|(status, code)| format!("[{status},'{code}']"))
        .collect::<Vec<_>>()
        .join(",");
    js_sys::eval(&format!(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.__captionsPosts = [];
        window.__captionsDelayMs = 0;
        window.__statusGets = 0;
        window.__statusReply = 200;
        window.__statusDelayMs = 0;
        window.__logTokens = [];
        window.__consoleLogCollector = {{
            setContext: function() {{}},
            setAuthToken: function(t) {{ window.__logTokens.push(t); }}
        }};
        var replies = [{replies}];
        window.fetch = function(input, init) {{
            var req = typeof input === 'string' ? new Request(input, init) : input;
            if (/\/meetings\/[^/]+\/status$/.test(req.url)) {{
                window.__statusGets += 1;
                var code = Array.isArray(window.__statusReply)
                    ? window.__statusReply[Math.min(window.__statusGets, window.__statusReply.length) - 1]
                    : window.__statusReply;
                var ok = code === 200;
                var status = JSON.stringify(ok ? {{
                    success: true,
                    result: {{
                        user_id: 'u', status: 'admitted', is_host: true, joined_at: 0,
                        room_token: 'room-tok-2'
                    }}
                }} : {{ success: false, result: {{ code: 'E', message: 'm' }} }});
                var sresp = new Response(status, {{
                    status: code,
                    headers: {{ 'Content-Type': 'application/json' }}
                }});
                Object.defineProperty(sresp, 'url', {{ value: req.url }});
                return new Promise(function(resolve) {{
                    setTimeout(function() {{ resolve(sresp); }}, window.__statusDelayMs);
                }});
            }}
            if (req.url.indexOf('/captions') === -1) {{
                return window.__original_fetch(input, init);
            }}
            var auth = req.headers.get('Authorization');
            return req.text().then(function(body) {{
                var n = window.__captionsPosts.push(auth + ' ' + body);
                var reply = replies[Math.min(n, replies.length) - 1];
                var status = reply[0];
                var payload = status < 300 ? '' : JSON.stringify({{
                    success: false, result: {{ code: reply[1], message: 'm' }}
                }});
                var resp = new Response(status === 204 ? null : payload, {{
                    status: status, headers: {{ 'Content-Type': 'application/json' }}
                }});
                Object.defineProperty(resp, 'url', {{ value: req.url }});
                return new Promise(function(resolve) {{
                    setTimeout(function() {{ resolve(resp); }}, window.__captionsDelayMs);
                }});
            }});
        }};
        "#
    ))
    .expect("failed to mock the captions API");
}

fn posts() -> Vec<String> {
    let value = js_sys::Reflect::get(&gloo_utils::window(), &"__captionsPosts".into()).unwrap();
    js_sys::Array::from(&value)
        .iter()
        .filter_map(|v| v.as_string())
        .collect()
}

fn log_tokens() -> Vec<String> {
    let value = js_sys::Reflect::get(&gloo_utils::window(), &"__logTokens".into()).unwrap();
    js_sys::Array::from(&value)
        .iter()
        .filter_map(|v| v.as_string())
        .collect()
}

fn status_gets() -> u32 {
    js_sys::Reflect::get(&gloo_utils::window(), &"__statusGets".into())
        .unwrap()
        .as_f64()
        .unwrap_or(0.0) as u32
}

async fn wait_for_posts(n: usize, timeout_ms: u32) -> bool {
    let mut waited = 0;
    while posts().len() < n && waited < timeout_ms {
        sleep_ms(10).await;
        waited += 10;
    }
    posts().len() >= n
}

const ENABLE: &str = r#"Bearer room-tok {"enabled":true,"renew":false}"#;
const RENEW: &str = r#"Bearer room-tok {"enabled":true,"renew":true}"#;
const OFF: &str = r#"Bearer room-tok {"enabled":false,"renew":false}"#;
const ENABLE_2: &str = r#"Bearer room-tok-2 {"enabled":true,"renew":false}"#;
const RENEW_2: &str = r#"Bearer room-tok-2 {"enabled":true,"renew":true}"#;
const OFF_2: &str = r#"Bearer room-tok-2 {"enabled":false,"renew":false}"#;

struct Setup {
    flag: bool,
    e2ee: bool,
    host: bool,
    replies: &'static [(u16, &'static str)],
}

const VIEWER: Setup = Setup {
    flag: true,
    e2ee: false,
    host: false,
    replies: &[(204, "")],
};

const HOST: Setup = Setup {
    host: true,
    ..VIEWER
};

async fn mount_meeting(setup: Setup) -> web_sys::Element {
    reset_test_browser_state();
    inject_app_config();
    mock_captions_api(setup.replies);
    remember_room_token("m1", "room-tok");
    FLAG.with(|c| c.set(setup.flag));
    E2EE.with(|c| c.set(setup.e2ee));
    START_AS_HOST.with(|c| c.set(setup.host));
    RENEW_INTERVAL.with(|c| c.set(RENEW_MS));
    let mount = create_mount_point();
    render_into(&mount, MeetingHarness);
    yield_now().await;
    mount
}

async fn unmount_meeting(mount: &web_sys::Element) {
    set(&MOUNTED, false);
    sleep_ms(50).await;
    cleanup(mount);
    restore_fetch();
}

fn all(mount: &web_sys::Element, selector: &str) -> Vec<web_sys::Element> {
    let list = mount.query_selector_all(selector).unwrap();
    (0..list.length())
        .filter_map(|i| list.item(i))
        .filter_map(|n| n.dyn_into::<web_sys::Element>().ok())
        .collect()
}

fn texts(mount: &web_sys::Element, selector: &str) -> Vec<String> {
    all(mount, selector)
        .iter()
        .map(|e| e.text_content().unwrap_or_default())
        .collect()
}

fn indicator(mount: &web_sys::Element) -> Option<String> {
    texts(mount, "[data-testid='captions-live-indicator']")
        .into_iter()
        .next()
}

fn button(mount: &web_sys::Element) -> Option<web_sys::HtmlElement> {
    mount
        .query_selector("[data-testid='captions-button']")
        .unwrap()
        .map(|e| e.dyn_into::<web_sys::HtmlElement>().unwrap())
}

/// A CC button attribute, empty when absent.
fn cc(mount: &web_sys::Element, name: &str) -> String {
    button(mount)
        .and_then(|b| b.get_attribute(name))
        .unwrap_or_default()
}

async fn assert_no_more_posts() {
    let n = posts().len();
    sleep_ms(RENEW_MS * 3).await;
    assert_eq!(posts().len(), n, "posted {:?}", posts());
}

fn overlay_shown(mount: &web_sys::Element) -> bool {
    !all(mount, "[data-testid='captions-overlay']").is_empty()
}

async fn mount_overlay() -> web_sys::Element {
    let mount = create_mount_point();
    render_into(&mount, OverlayHarness);
    yield_now().await;
    mount
}

// --- caption lane -----------------------------------------------------------

#[wasm_bindgen_test]
async fn a_higher_revision_replaces_the_line_and_a_stale_one_is_ignored() {
    let mount = mount_overlay().await;
    push(event(1, 1, 1, false, "buon"));
    push(event(1, 1, 2, false, "buongiorno"));
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        ["buongiorno"]
    );

    push(event(1, 1, 3, true, "good morning"));
    push(event(1, 1, 2, false, "late interim"));
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        ["good morning"]
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_caption_that_changes_nothing_does_not_write_the_board() {
    let mount = mount_overlay().await;
    push(event(1, 1, 2, false, "ciao"));
    yield_now().await;
    let renders = PROBE_RENDERS.with(Cell::get);
    push(event(1, 1, 1, false, "stale"));
    push(event(1, 1, 2, false, "duplicate"));
    push(event(1, 1, 3, false, "ciao"));
    yield_now().await;
    assert_eq!(PROBE_RENDERS.with(Cell::get), renders);
    push(event(1, 1, 4, false, "ciao a tutti"));
    yield_now().await;
    assert_eq!(PROBE_RENDERS.with(Cell::get), renders + 1);
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn lines_render_in_the_viewers_language_and_follow_a_change() {
    let mount = mount_overlay().await;
    push(translated(
        event(1, 1, 1, true, "Good morning"),
        &[
            ("en", "Good morning"),
            ("es", "Buenos días"),
            ("it", "Buongiorno"),
        ],
    ));
    yield_now().await;
    let line_text = |m: &web_sys::Element| texts(m, "[data-testid='caption-line-text']");
    assert_eq!(line_text(&mount), ["Good morning"]);
    set_language("it");
    yield_now().await;
    assert_eq!(line_text(&mount), ["Buongiorno"]);
    let text = &all(&mount, "[data-testid='caption-line-text']")[0];
    assert_eq!(text.get_attribute("lang").as_deref(), Some("it"));
    let region = &all(&mount, "[data-testid='captions-live-region']")[0];
    assert_eq!(
        region.text_content().unwrap_or_default(),
        "Speaker 1: Buongiorno"
    );
    let entry = &all(&mount, "[data-testid='captions-live-region'] p")[0];
    assert_eq!(
        entry.get_attribute("lang").as_deref(),
        Some("it"),
        "the screen reader is told the announcement's language"
    );
    set_language("fr");
    yield_now().await;
    assert_eq!(
        line_text(&mount),
        ["Good morning"],
        "no French: the caption as sent"
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_badge_marks_only_lines_not_in_the_viewers_language() {
    let mount = mount_overlay().await;
    set_language("en");
    push(translated(
        event(1, 1, 1, false, "Hola"),
        &[("es", "Hola"), ("en", "Hello")],
    ));
    push(event(2, 1, 1, true, "Ciao a tutti"));
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        ["Hello", "Ciao a tutti"]
    );
    let mut e = event(2, 1, 2, true, "Ciao a tutti");
    e.text_language = "it".into();
    push(e);
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-lang']"),
        ["IT"],
        "the translated interim has no badge; the untranslated final does"
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn interims_are_greyed_and_hidden_from_screen_readers_finals_are_announced() {
    let mount = mount_overlay().await;
    push(event(1, 1, 1, false, "ciao"));
    push(event(2, 1, 1, true, "hello"));
    yield_now().await;

    let lines = all(&mount, "[data-testid='caption-line']");
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0].get_attribute("data-final").as_deref(),
        Some("false")
    );
    assert!(lines[0].class_list().contains("caption-line--interim"));
    assert_eq!(
        lines[1].get_attribute("data-final").as_deref(),
        Some("true")
    );
    assert!(lines[1].class_list().contains("caption-line--final"));
    assert_eq!(texts(&mount, "[data-testid='caption-line-lang']"), ["IT"]);

    let lane = all(&mount, "[data-testid='captions-overlay']");
    assert_eq!(
        lane[0].get_attribute("aria-hidden").as_deref(),
        Some("true")
    );
    let region = &all(&mount, "[data-testid='captions-live-region']")[0];
    assert_eq!(region.get_attribute("aria-live").as_deref(), Some("polite"));
    assert_eq!(
        region.text_content().unwrap_or_default(),
        "Speaker 2: hello"
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn speaker_and_text_are_bidi_isolated_and_a_colon_name_cannot_spoof() {
    let mount = mount_overlay().await;
    let mut e = event(1, 1, 1, true, "hello");
    e.speaker_name = "Bob: I resign".to_string();
    push(e);
    yield_now().await;
    for id in ["caption-line-speaker", "caption-line-text"] {
        let el = &all(&mount, &format!("[data-testid='{id}']"))[0];
        assert_eq!(el.tag_name().to_ascii_lowercase(), "bdi", "{id}");
    }
    assert_eq!(
        texts(&mount, "[data-testid='captions-live-region'] p"),
        ["Bob I resign: hello"]
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn caption_text_and_speaker_render_as_inert_text() {
    let mount = mount_overlay().await;
    let hostile = "<img src=x onerror=\"window.__pwned=1\"><b>bold</b>";
    let mut e = event(1, 1, 1, true, hostile);
    e.speaker_name = "<script>alert(1)</script>".to_string();
    push(e);
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        [hostile]
    );
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-speaker']"),
        ["<script>alert(1)</script>"]
    );
    assert!(all(&mount, "img, b, script").is_empty());
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_repeated_speaker_is_collapsed_into_the_line_above() {
    let mount = mount_overlay().await;
    let continued = |m: &web_sys::Element| -> Vec<bool> {
        all(m, "[data-testid='caption-line']")
            .iter()
            .map(|l| l.class_list().contains("caption-line--continued"))
            .collect()
    };
    push(event(1, 1, 1, true, "one"));
    push(event(1, 2, 1, true, "two"));
    yield_now().await;
    assert_eq!(continued(&mount), [false, true]);
    assert_eq!(
        texts(&mount, "[data-testid='captions-live-region'] p"),
        ["Speaker 1: one", "Speaker 1: two"],
        "screen readers still hear the speaker"
    );
    cleanup(&mount);

    let mount = mount_overlay().await;
    push(event(1, 1, 1, true, "one"));
    push(event(2, 1, 1, true, "other"));
    push(event(1, 2, 1, true, "two"));
    yield_now().await;
    assert_eq!(continued(&mount), [false, false, false]);
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn at_most_three_lines_are_rendered() {
    let mount = mount_overlay().await;
    for u in 1..=5 {
        push(event(1, u, 1, true, &format!("line {u}")));
    }
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        ["line 3", "line 4", "line 5"]
    );
    assert_eq!(
        all(&mount, "[data-testid='caption-line']").len(),
        MAX_CAPTION_LINES
    );
    cleanup(&mount);
}

// --- transcription notice ---------------------------------------------------

#[wasm_bindgen_test]
async fn the_notice_renders_with_the_flag_off_and_nothing_else_does() {
    let mount = mount_meeting(Setup {
        flag: false,
        ..VIEWER
    })
    .await;
    assert_eq!(indicator(&mount), None);
    deliver(active("Hana <b>"));
    yield_now().await;
    assert_eq!(
        indicator(&mount).as_deref(),
        Some("Live captions on \u{b7} started by Hana <b>")
    );
    assert_eq!(
        texts(&mount, "[data-testid='captions-live-starter']"),
        ["Hana <b>"]
    );
    assert!(all(&mount, "b").is_empty());
    assert!(button(&mount).is_none());

    deliver(CaptionUpdate::Text(event(1, 1, 1, true, "hello")));
    sleep_ms(RENEW_MS * 2).await;
    assert!(!overlay_shown(&mount));
    assert!(handle().board.peek().is_empty());
    assert!(posts().is_empty(), "posted {:?}", posts());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_host_with_the_flag_off_gets_the_notice_but_no_text_or_control() {
    let mount = mount_meeting(Setup {
        flag: false,
        ..HOST
    })
    .await;
    deliver(active("Hana"));
    deliver(CaptionUpdate::Text(event(1, 1, 1, true, "hello")));
    yield_now().await;
    assert!(indicator(&mount).is_some());
    assert!(button(&mount).is_none());
    assert!(!overlay_shown(&mount));
    assert!(handle().board.peek().is_empty());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn with_e2ee_the_notice_shows_but_text_and_the_control_do_not() {
    let mount = mount_meeting(Setup {
        e2ee: true,
        ..VIEWER
    })
    .await;
    deliver(active("Hana"));
    deliver(CaptionUpdate::Text(event(1, 1, 1, true, "hello")));
    yield_now().await;
    assert!(indicator(&mount).is_some());
    assert!(button(&mount).is_none());
    assert!(!overlay_shown(&mount));
    assert!(handle().board.peek().is_empty());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn heartbeats_keep_the_notice_on_until_they_stop() {
    let mount = mount_meeting(VIEWER).await;
    deliver(active("Hana"));
    yield_now().await;
    assert!(indicator(&mount).is_some());
    for beat in 1..=3 {
        sleep_ms(STATUS_MS * 2 / 3).await;
        assert!(
            indicator(&mount).is_some(),
            "lapsed before heartbeat {beat}"
        );
        deliver(active("Hana"));
    }
    yield_now().await;
    assert!(indicator(&mount).is_some(), "heartbeats must re-arm");
    sleep_ms(STATUS_MS + 100).await;
    assert_eq!(indicator(&mount), None, "no status for the timeout");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_persistent_status_region_announces_captions_on_and_off() {
    let mount = mount_meeting(Setup {
        flag: false,
        ..VIEWER
    })
    .await;
    let region = || all(&mount, "[data-testid='captions-status-region']");
    let before = region();
    assert_eq!(before.len(), 1, "the region exists before any status");
    assert_eq!(before[0].get_attribute("role").as_deref(), Some("status"));
    assert_eq!(
        before[0].get_attribute("aria-live").as_deref(),
        Some("polite")
    );
    assert_eq!(before[0].text_content().unwrap_or_default(), "");

    deliver(active("Hana"));
    yield_now().await;
    let on = region();
    assert!(on[0].is_same_node(Some(&before[0])), "the same node");
    assert_eq!(
        on[0].text_content().unwrap_or_default(),
        "Live captions started by Hana"
    );
    let visible = &all(&mount, "[data-testid='captions-live-indicator']")[0];
    assert_eq!(visible.get_attribute("role"), None);
    assert_eq!(visible.get_attribute("aria-live"), None);

    deliver(stopped());
    yield_now().await;
    let off = region();
    assert!(off[0].is_same_node(Some(&before[0])), "the same node");
    assert_eq!(
        off[0].text_content().unwrap_or_default(),
        "Live captions stopped"
    );
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn the_status_region_says_when_the_viewer_started_captions() {
    let mount = mount_meeting(VIEWER).await;
    deliver(CaptionUpdate::Status(CaptionStatus {
        active: true,
        started_by_user_id: "me@example.com".to_string(),
        started_by_name: "Me".to_string(),
        started_by_local: true,
    }));
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='captions-status-region']"),
        ["You started live captions"]
    );
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_hidden_lane_keeps_its_lines_its_log_and_the_status_region() {
    let mount = mount_meeting(VIEWER).await;
    deliver(active("Hana"));
    deliver(CaptionUpdate::Text(event(1, 1, 1, true, "hello")));
    yield_now().await;
    assert!(overlay_shown(&mount));

    set(&LANE_HIDDEN, true);
    yield_now().await;
    assert!(!overlay_shown(&mount), "a hidden lane must not render");
    assert_eq!(
        texts(&mount, "[data-testid='captions-live-region'] p"),
        ["Speaker 1: hello"]
    );
    assert_eq!(
        texts(&mount, "[data-testid='captions-status-region']"),
        ["Live captions started by Hana"]
    );
    assert!(indicator(&mount).is_some());

    set(&LANE_HIDDEN, false);
    yield_now().await;
    assert!(overlay_shown(&mount));
    assert_eq!(texts(&mount, ".caption-line__text"), ["hello"]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_remote_starter_named_you_is_announced_by_name() {
    let mount = mount_meeting(VIEWER).await;
    deliver(active("you"));
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='captions-status-region']"),
        ["Live captions started by you"]
    );
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_self_start_and_a_remote_you_render_different_indicators() {
    let mount = mount_meeting(VIEWER).await;
    deliver(CaptionUpdate::Status(CaptionStatus {
        active: true,
        started_by_user_id: "me@example.com".to_string(),
        started_by_name: "Me".to_string(),
        started_by_local: true,
    }));
    yield_now().await;
    let mine = indicator(&mount);
    let my_starter = texts(&mount, "[data-testid='captions-live-starter']");
    deliver(stopped());
    deliver(active("you"));
    yield_now().await;
    let theirs = indicator(&mount);
    assert_eq!(
        mine.as_deref(),
        Some("Live captions on \u{b7} you started them")
    );
    assert_eq!(
        theirs.as_deref(),
        Some("Live captions on \u{b7} started by you")
    );
    assert_eq!(my_starter, ["you"]);
    assert_eq!(
        texts(&mount, "[data-testid='captions-live-starter']"),
        ["you"]
    );
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn the_viewers_own_finals_show_in_the_lane_but_are_not_read_back() {
    let mount = mount_meeting(VIEWER).await;
    deliver(active("Hana"));
    let mut mine = event(1, 1, 1, true, "my words");
    mine.is_local = true;
    deliver(CaptionUpdate::Text(mine));
    deliver(CaptionUpdate::Text(event(2, 1, 1, true, "their words")));
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        ["my words", "their words"]
    );
    assert_eq!(
        texts(&mount, "[data-testid='captions-live-region'] p"),
        ["Speaker 2: their words"]
    );
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn only_the_viewers_own_lines_are_marked_local() {
    let mount = mount_meeting(VIEWER).await;
    deliver(active("Hana"));
    let mut mine = event(1, 1, 1, true, "my words");
    mine.is_local = true;
    deliver(CaptionUpdate::Text(mine));
    let mut you = event(2, 1, 1, true, "their words");
    you.speaker_name = "You".to_string();
    deliver(CaptionUpdate::Text(you));
    yield_now().await;
    let local: Vec<bool> = all(&mount, "[data-testid='caption-line']")
        .iter()
        .map(|l| l.class_list().contains("caption-line--local"))
        .collect();
    assert_eq!(local, [true, false]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn status_stopped_clears_the_notice_and_the_lane_at_once() {
    let mount = mount_meeting(VIEWER).await;
    deliver(active("Hana"));
    deliver(CaptionUpdate::Text(event(1, 1, 1, true, "hello")));
    yield_now().await;
    assert!(overlay_shown(&mount));
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        ["hello"]
    );

    deliver(stopped());
    yield_now().await;
    assert_eq!(indicator(&mount), None);
    assert!(!overlay_shown(&mount));
    assert!(handle().board.peek().is_empty());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn text_before_any_status_is_not_shown() {
    let mount = mount_meeting(VIEWER).await;
    deliver(CaptionUpdate::Text(event(1, 1, 1, true, "hello")));
    yield_now().await;
    assert!(!overlay_shown(&mount));
    assert!(handle().board.peek().is_empty());
    unmount_meeting(&mount).await;
}

// --- participant control ----------------------------------------------------

#[wasm_bindgen_test]
async fn a_participant_button_is_disabled_until_captions_are_on_and_never_posts() {
    let mount = mount_meeting(VIEWER).await;
    assert_eq!(cc(&mount, "data-captions-role"), "viewer");
    assert_eq!(cc(&mount, "aria-disabled"), "true");
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    assert_eq!(
        texts(&mount, "[data-testid='captions-button'] .tooltip-desc"),
        [CAPTIONS_OFF_HINT]
    );
    button(&mount).expect("CC button").click();
    sleep_ms(RENEW_MS * 2).await;
    assert!(posts().is_empty(), "posted {:?}", posts());

    deliver(active("Hana"));
    deliver(CaptionUpdate::Text(event(1, 1, 1, true, "hello")));
    yield_now().await;
    assert_eq!(cc(&mount, "aria-disabled"), "");
    assert_eq!(cc(&mount, "aria-pressed"), "true");
    assert!(overlay_shown(&mount));

    button(&mount).expect("CC button").click();
    yield_now().await;
    assert!(!overlay_shown(&mount));
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    assert!(!dioxus_ui::local_storage::load_bool(
        CAPTIONS_SHOW_PREF_KEY,
        true
    ));
    sleep_ms(RENEW_MS * 2).await;
    assert!(posts().is_empty(), "posted {:?}", posts());
    unmount_meeting(&mount).await;
}

// --- host control -----------------------------------------------------------

#[wasm_bindgen_test]
async fn a_host_starts_renews_and_revokes_room_captions() {
    let mount = mount_meeting(HOST).await;
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    sleep_ms(RENEW_MS * 2).await;
    assert!(posts().is_empty(), "no auto-start on mount: {:?}", posts());

    button(&mount).expect("CC button").click();
    assert!(
        wait_for_posts(3, RENEW_MS * 6).await,
        "posted {:?}",
        posts()
    );
    assert_eq!(&posts()[..3], [ENABLE, RENEW, RENEW]);
    assert_eq!(cc(&mount, "aria-pressed"), "true");

    button(&mount).unwrap().click();
    let n = posts().len() + 1;
    assert!(wait_for_posts(n, 1_000).await);
    assert_eq!(posts().last().map(String::as_str), Some(OFF));
    sleep_ms(RENEW_MS * 3).await;
    assert_eq!(posts().len(), n, "no renewal after revoking: {:?}", posts());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn hiding_the_lane_does_not_stop_renewal() {
    let mount = mount_meeting(HOST).await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    deliver(active_by_me());
    yield_now().await;
    assert!(overlay_shown(&mount));
    set(&LANE_HIDDEN, true);
    yield_now().await;
    assert!(!overlay_shown(&mount), "the lane is hidden");
    let n = posts().len();
    assert!(
        wait_for_posts(n + 2, RENEW_MS * 4).await,
        "posted {:?}",
        posts()
    );
    assert!(posts()[n..].iter().all(|p| p == RENEW), "{:?}", posts());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_token_for_another_meeting_is_not_sent_with_this_meetings_post() {
    let mount = mount_meeting(HOST).await;
    remember_room_token("other-meeting", "stale-tok");
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    assert_eq!(posts(), [ENABLE]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_revoke_waits_for_the_enable_already_in_flight() {
    let mount = mount_meeting(HOST).await;
    js_sys::eval("window.__captionsDelayMs = 150;").unwrap();
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    button(&mount).unwrap().click();
    sleep_ms(60).await;
    assert_eq!(posts(), [ENABLE]);
    assert!(wait_for_posts(2, 1_000).await);
    assert_eq!(posts(), [ENABLE, OFF]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_revoke_that_fails_transiently_is_sent_again() {
    let mount = mount_meeting(Setup {
        replies: &[
            (503, "CAPTIONS_UNAVAILABLE"),
            (429, "RATE_LIMIT_EXCEEDED"),
            (204, ""),
        ],
        ..HOST
    })
    .await;
    deliver(active("Other host"));
    yield_now().await;
    button(&mount).expect("CC button").click();
    for _ in 0..65 {
        deliver(active("Other host"));
        sleep_ms(100).await;
    }
    assert_eq!(posts(), [OFF, OFF, OFF], "no resend once accepted");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn turning_captions_back_on_ends_a_failing_revoke() {
    let mount = mount_meeting(Setup {
        replies: &[(204, ""), (503, "CAPTIONS_UNAVAILABLE"), (204, "")],
        ..HOST
    })
    .await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, 1_000).await);
    assert_eq!(posts()[1], OFF);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(3, 1_000).await);
    assert_eq!(posts()[2], ENABLE);
    sleep_ms(1_500).await;
    assert!(
        posts()[2..].iter().all(|p| p != OFF),
        "a superseded revoke was resent: {:?}",
        posts()
    );
    assert_eq!(cc(&mount, "aria-pressed"), "true");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_newer_click_ends_an_older_revokes_retries() {
    let mount = mount_meeting(Setup {
        replies: &[
            (204, ""),
            (503, "CAPTIONS_UNAVAILABLE"),
            (204, ""),
            (503, "CAPTIONS_UNAVAILABLE"),
            (204, ""),
        ],
        ..HOST
    })
    .await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, 1_000).await);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(3, 1_000).await);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(4, 1_000).await);
    assert_eq!(posts(), [ENABLE, OFF, ENABLE, OFF]);
    sleep_ms(1_500).await;
    assert_eq!(
        posts(),
        [ENABLE, OFF, ENABLE, OFF, OFF],
        "only the latest revoke is retried"
    );
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_failing_revoke_stops_once_the_room_is_reported_stopped() {
    let mount = mount_meeting(Setup {
        replies: &[(204, ""), (503, "CAPTIONS_UNAVAILABLE"), (204, "")],
        ..HOST
    })
    .await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, 1_000).await);
    deliver(stopped());
    deliver(active("Other host"));
    sleep_ms(1_500).await;
    assert_eq!(posts(), [ENABLE, OFF], "the revoke was resent");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_rejoining_host_renews_its_own_lease_without_re_enabling() {
    let mount = mount_meeting(HOST).await;
    deliver(active_by_me());
    yield_now().await;
    assert_eq!(cc(&mount, "aria-pressed"), "true");
    assert_eq!(cc(&mount, "data-captions-pending"), "false");
    assert!(
        wait_for_posts(2, RENEW_MS * 6).await,
        "posted {:?}",
        posts()
    );
    assert_eq!(&posts()[..2], [RENEW, RENEW]);
    assert!(*handle().room_on.peek() && *handle().enable_accepted.peek());
    deliver(stopped());
    yield_now().await;
    assert_no_more_posts().await;
    assert!(posts().iter().all(|p| p != ENABLE), "posted {:?}", posts());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_revoking_host_does_not_adopt_its_lease_until_the_room_stops() {
    let mount = mount_meeting(HOST).await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, 1_000).await);
    assert_eq!(posts(), [ENABLE, OFF]);
    deliver(active_by_me());
    yield_now().await;
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE, OFF]);

    deliver(stopped());
    deliver(active_by_me());
    assert!(
        wait_for_posts(3, RENEW_MS * 2).await,
        "posted {:?}",
        posts()
    );
    assert_eq!(posts()[2], RENEW);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_viewer_never_posts_for_its_own_lease() {
    let mount = mount_meeting(VIEWER).await;
    deliver(active_by_me());
    yield_now().await;
    assert!(!*handle().room_on.peek());
    assert_no_more_posts().await;
    assert!(posts().is_empty(), "posted {:?}", posts());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn accepted_renewals_do_not_re_render_the_control() {
    let mount = mount_meeting(HOST).await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, RENEW_MS * 4).await);
    sleep_ms(50).await;
    let renders = MEETING_RENDERS.with(Cell::get);
    assert!(wait_for_posts(4, RENEW_MS * 4).await);
    sleep_ms(50).await;
    assert_eq!(MEETING_RENDERS.with(Cell::get), renders);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_superseded_queued_enable_is_never_sent() {
    let mount = mount_meeting(HOST).await;
    RENEW_INTERVAL.with(|c| c.set(4_000));
    js_sys::eval("window.__captionsDelayMs = 1000;").unwrap();
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    for _ in 0..3 {
        yield_now().await;
        button(&mount).unwrap().click();
    }
    assert!(wait_for_posts(3, 5_000).await, "posted {:?}", posts());
    sleep_ms(1_500).await;
    assert_eq!(posts(), [ENABLE, OFF, OFF]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_stop_status_before_the_enable_answers_does_not_cancel_it() {
    let mount = mount_meeting(HOST).await;
    js_sys::eval("window.__captionsDelayMs = 100;").unwrap();
    button(&mount).unwrap().click();
    yield_now().await;
    deliver(stopped());
    assert!(
        wait_for_posts(3, RENEW_MS * 6).await,
        "posted {:?}",
        posts()
    );
    assert_eq!(&posts()[..3], [ENABLE, RENEW, RENEW]);
    assert_eq!(cc(&mount, "aria-pressed"), "true");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_self_started_status_confirms_a_start_the_server_did_not_answer() {
    let mount = mount_meeting(Setup {
        replies: &[(503, "CAPTIONS_UNAVAILABLE")],
        ..HOST
    })
    .await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    assert!(!*handle().enable_accepted.peek());
    deliver(active_by_me());
    yield_now().await;
    assert!(*handle().enable_accepted.peek());
    deliver(stopped());
    yield_now().await;
    assert_no_more_posts().await;
    assert!(!*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_host_revokes_captions_another_host_started() {
    let mount = mount_meeting(HOST).await;
    deliver(active("Other host"));
    yield_now().await;
    assert_eq!(cc(&mount, "aria-pressed"), "true");
    button(&mount).expect("CC button").click();
    assert!(wait_for_posts(1, 1_000).await);
    assert_eq!(posts(), [OFF]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_host_leaving_does_not_revoke() {
    let mount = mount_meeting(HOST).await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    set(&MOUNTED, false);
    sleep_ms(RENEW_MS * 3).await;
    assert!(posts().iter().all(|p| p != OFF), "posted {:?}", posts());
    let n = posts().len();
    sleep_ms(RENEW_MS * 2).await;
    assert_eq!(posts().len(), n, "no renewal after unmount");
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn losing_host_mid_call_stops_renewing_and_turns_the_button_into_a_viewer() {
    let mount = mount_meeting(HOST).await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    set(&IS_HOST, false);
    yield_now().await;
    assert_no_more_posts().await;
    assert!(posts().iter().all(|p| p != OFF));
    assert_eq!(cc(&mount, "data-captions-role"), "viewer");

    set(&IS_HOST, true);
    yield_now().await;
    assert_no_more_posts().await;
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_stop_status_ends_this_hosts_renewal() {
    let mount = mount_meeting(HOST).await;
    button(&mount).unwrap().click();
    deliver(active_by_me());
    assert!(wait_for_posts(1, 1_000).await);
    deliver(stopped());
    yield_now().await;
    assert_no_more_posts().await;
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn the_button_presses_only_once_the_server_accepts() {
    let mount = mount_meeting(HOST).await;
    // Inside the request deadline (3/4 of RENEW_MS), but after the first render.
    js_sys::eval("window.__captionsDelayMs = 100;").unwrap();
    button(&mount).unwrap().click();
    yield_now().await;
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    assert_eq!(cc(&mount, "data-captions-pending"), "true");
    assert_eq!(cc(&mount, "aria-busy"), "true");
    assert!(wait_for_posts(1, 1_000).await);
    sleep_ms(200).await;
    assert_eq!(cc(&mount, "aria-pressed"), "true");
    assert_eq!(cc(&mount, "data-captions-pending"), "false");
    assert_eq!(cc(&mount, "aria-busy"), "");
    unmount_meeting(&mount).await;
}

// --- error mapping ----------------------------------------------------------

async fn host_clicks_with(replies: &'static [(u16, &'static str)]) -> web_sys::Element {
    let mount = mount_meeting(Setup { replies, ..HOST }).await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    sleep_ms(50).await;
    mount
}

fn start_failure(mount: &web_sys::Element) -> Option<(String, String)> {
    let notice = mount
        .query_selector("[data-testid='co-host-notice']")
        .unwrap()?;
    Some((
        notice.text_content().unwrap_or_default(),
        notice.get_attribute("role").unwrap_or_default(),
    ))
}

#[wasm_bindgen_test]
async fn a_rejected_start_says_why_captions_did_not_start() {
    let cases: [(&'static [(u16, &'static str)], &str); 6] = [
        (
            &[(403, "NOT_HOST")],
            "Couldn't start captions. Only hosts can do that.",
        ),
        (
            &[(404, "NOT_FOUND")],
            "Couldn't start captions. They aren't available in this meeting.",
        ),
        (
            &[(409, "CAPTIONS_OFF")],
            "Couldn't start captions. They aren't available in this meeting.",
        ),
        (
            &[(409, "MEETING_NOT_ACTIVE")],
            "Couldn't start captions. The meeting isn't active.",
        ),
        (
            &[(404, "MEETING_NOT_FOUND")],
            "Couldn't start captions. This meeting is no longer available.",
        ),
        (
            &[(401, "UNAUTHORIZED")],
            "Couldn't start captions. Try again.",
        ),
    ];
    for (replies, want) in cases {
        let mount = host_clicks_with(replies).await;
        assert_no_more_posts().await;
        assert_eq!(
            start_failure(&mount),
            Some((want.to_string(), "alert".to_string())),
            "{replies:?}"
        );
        assert_ne!(cc(&mount, "aria-pressed"), "true", "{replies:?}");
        unmount_meeting(&mount).await;
    }
}

#[wasm_bindgen_test]
async fn an_accepted_start_or_a_rejected_renewal_says_nothing() {
    for replies in [
        &[(204, "")][..],
        &[(204, ""), (403, "NOT_HOST")][..],
        &[(204, ""), (409, "CAPTIONS_OFF")][..],
    ] {
        let mount = host_clicks_with(replies).await;
        assert!(wait_for_posts(2, RENEW_MS * 4).await, "{replies:?}");
        sleep_ms(50).await;
        assert_eq!(start_failure(&mount), None, "{replies:?}");
        unmount_meeting(&mount).await;
    }
}

#[wasm_bindgen_test]
async fn a_start_confirmed_by_a_self_started_status_says_nothing_when_a_retry_is_rejected() {
    let mount = mount_meeting(Setup {
        replies: &[(503, "CAPTIONS_UNAVAILABLE"), (404, "MEETING_NOT_FOUND")],
        ..HOST
    })
    .await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    deliver(active_by_me());
    yield_now().await;
    assert_eq!(cc(&mount, "aria-pressed"), "true");
    assert!(
        wait_for_posts(2, RENEW_MS * 4).await,
        "posted {:?}",
        posts()
    );
    sleep_ms(50).await;
    assert_eq!(posts(), [ENABLE, ENABLE]);
    assert_eq!(start_failure(&mount), None);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_start_rejected_after_the_host_turned_it_off_says_nothing() {
    let mount = mount_meeting(Setup {
        replies: &[(403, "NOT_HOST")],
        ..HOST
    })
    .await;
    js_sys::eval("window.__captionsDelayMs = 100;").unwrap();
    button(&mount).unwrap().click();
    yield_now().await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, 1_000).await, "posted {:?}", posts());
    sleep_ms(200).await;
    assert_eq!(&posts()[..1], [ENABLE]);
    assert_eq!(start_failure(&mount), None);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_room_token_rejected_again_after_a_refresh_stops() {
    for replies in [&[(401, "UNAUTHORIZED")], &[(403, "FORBIDDEN")]] {
        let mount = host_clicks_with(replies).await;
        assert_no_more_posts().await;
        assert_eq!(posts(), [ENABLE, ENABLE_2], "{replies:?}");
        assert_eq!(status_gets(), 1, "{replies:?}");
        assert_eq!(cc(&mount, "aria-pressed"), "false", "{replies:?}");
        assert_eq!(cc(&mount, "data-captions-pending"), "false", "{replies:?}");
        assert!(!*handle().room_on.peek(), "{replies:?}");
        unmount_meeting(&mount).await;
    }
}

#[wasm_bindgen_test]
async fn a_rejected_room_token_is_refreshed_and_the_enable_retried() {
    for replies in [
        &[(401, "UNAUTHORIZED"), (204, "")],
        &[(403, "FORBIDDEN"), (204, "")],
    ] {
        let mount = host_clicks_with(replies).await;
        assert!(wait_for_posts(3, RENEW_MS * 2).await, "{replies:?}");
        assert_eq!(&posts()[..3], [ENABLE, ENABLE_2, RENEW_2], "{replies:?}");
        assert_eq!(status_gets(), 1, "{replies:?}");
        assert_eq!(log_tokens(), ["room-tok-2"], "{replies:?}");
        assert_eq!(cc(&mount, "aria-pressed"), "true", "{replies:?}");
        assert!(*handle().room_on.peek(), "{replies:?}");
        unmount_meeting(&mount).await;
    }
}

#[wasm_bindgen_test]
async fn a_refresh_does_not_hand_the_log_collector_a_token_for_another_meeting() {
    let mount = mount_meeting(Setup {
        replies: &[(401, "UNAUTHORIZED"), (204, "")],
        ..HOST
    })
    .await;
    set_console_log_context("other-meeting", "u", "n");
    button(&mount).unwrap().click();
    let refreshed = wait_for_posts(2, 1_000).await;
    let tokens = log_tokens();
    set_console_log_context("m1", "u", "n");
    unmount_meeting(&mount).await;
    assert!(refreshed);
    assert_eq!(&posts()[..2], [ENABLE, ENABLE_2]);
    assert!(tokens.is_empty(), "{tokens:?}");
}

#[wasm_bindgen_test]
async fn a_room_token_that_cannot_be_refreshed_stops() {
    let mount = mount_meeting(Setup {
        replies: &[(401, "UNAUTHORIZED")],
        ..HOST
    })
    .await;
    js_sys::eval("window.__statusReply = 404;").unwrap();
    button(&mount).unwrap().click();
    assert!(wait_for_posts(1, 1_000).await);
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE]);
    assert_eq!(status_gets(), 1);
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    assert!(!*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_transient_refresh_failure_retries_with_the_old_token() {
    let mount = mount_meeting(Setup {
        replies: &[(401, "UNAUTHORIZED"), (401, "UNAUTHORIZED"), (204, "")],
        ..HOST
    })
    .await;
    js_sys::eval("window.__statusReply = 503;").unwrap();
    button(&mount).unwrap().click();
    let mut waited = 0;
    while status_gets() < 1 && waited < 1_000 {
        sleep_ms(5).await;
        waited += 5;
    }
    js_sys::eval("window.__statusReply = 200;").unwrap();
    assert!(
        wait_for_posts(4, RENEW_MS * 3).await,
        "posted {:?}",
        posts()
    );
    assert_eq!(&posts()[..4], [ENABLE, ENABLE, ENABLE_2, RENEW_2]);
    assert_eq!(status_gets(), 2);
    assert_eq!(cc(&mount, "aria-pressed"), "true");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn transient_refresh_failures_stop_after_three_in_a_row() {
    let mount = mount_meeting(Setup {
        replies: &[(401, "UNAUTHORIZED")],
        ..HOST
    })
    .await;
    js_sys::eval("window.__statusReply = 503;").unwrap();
    button(&mount).unwrap().click();
    assert!(
        wait_for_posts(3, RENEW_MS * 3).await,
        "posted {:?}",
        posts()
    );
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE, ENABLE, ENABLE]);
    assert_eq!(status_gets(), 3);
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    assert_eq!(cc(&mount, "data-captions-pending"), "false");
    assert!(!*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_successful_refresh_resets_the_refresh_failure_count() {
    let mount = mount_meeting(Setup {
        replies: &[
            (401, "UNAUTHORIZED"),
            (401, "UNAUTHORIZED"),
            (401, "UNAUTHORIZED"),
            (503, "CAPTIONS_UNAVAILABLE"),
            (401, "UNAUTHORIZED"),
        ],
        ..HOST
    })
    .await;
    js_sys::eval("window.__statusReply = [503, 503, 200, 503];").unwrap();
    button(&mount).unwrap().click();
    assert!(
        wait_for_posts(7, RENEW_MS * 4).await,
        "posted {:?}",
        posts()
    );
    assert_no_more_posts().await;
    assert_eq!(
        posts(),
        [ENABLE, ENABLE, ENABLE, ENABLE_2, ENABLE_2, ENABLE_2, ENABLE_2]
    );
    assert_eq!(status_gets(), 6);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn an_accepted_post_resets_the_refresh_failure_count() {
    let mount = mount_meeting(Setup {
        replies: &[
            (401, "UNAUTHORIZED"),
            (401, "UNAUTHORIZED"),
            (204, ""),
            (401, "UNAUTHORIZED"),
        ],
        ..HOST
    })
    .await;
    js_sys::eval("window.__statusReply = 503;").unwrap();
    button(&mount).unwrap().click();
    assert!(
        wait_for_posts(6, RENEW_MS * 4).await,
        "posted {:?}",
        posts()
    );
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE, ENABLE, ENABLE, RENEW, RENEW, RENEW]);
    assert_eq!(status_gets(), 5);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_hung_refresh_counts_toward_the_refresh_failure_cap() {
    let mount = mount_meeting(Setup {
        replies: &[(401, "UNAUTHORIZED")],
        ..HOST
    })
    .await;
    js_sys::eval("window.__statusDelayMs = 2000;").unwrap();
    button(&mount).unwrap().click();
    assert!(
        wait_for_posts(3, RENEW_MS * 4).await,
        "posted {:?}",
        posts()
    );
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE, ENABLE, ENABLE]);
    assert_eq!(status_gets(), 3);
    assert!(!*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_revoke_with_a_rejected_room_token_is_resent_with_a_refreshed_one() {
    let mount = mount_meeting(Setup {
        replies: &[(401, "UNAUTHORIZED"), (204, "")],
        ..HOST
    })
    .await;
    deliver(active("Other host"));
    yield_now().await;
    button(&mount).expect("CC button").click();
    assert!(wait_for_posts(2, 1_000).await, "posted {:?}", posts());
    assert_no_more_posts().await;
    assert_eq!(posts(), [OFF, OFF_2]);
    assert_eq!(status_gets(), 1);
    unmount_meeting(&mount).await;
}

async fn takeover_rejected_with(
    replies: &'static [(u16, &'static str)],
    sent: &'static [&'static str],
) {
    let mount = mount_meeting(Setup { replies, ..HOST }).await;
    deliver(active_by_me());
    assert!(wait_for_posts(sent.len(), 1_000).await);
    sleep_ms(50).await;
    assert_eq!(posts(), sent);
    deliver(active_by_me());
    yield_now().await;
    assert_no_more_posts().await;
    assert_eq!(posts(), sent, "the lease was re-adopted");
    assert!(!*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn captions_off_on_a_takeover_is_not_retried_on_the_next_heartbeat() {
    takeover_rejected_with(&[(409, "CAPTIONS_OFF")], &[RENEW]).await;
}

#[wasm_bindgen_test]
async fn a_rejected_token_on_a_takeover_is_not_retried_on_the_next_heartbeat() {
    takeover_rejected_with(&[(401, "UNAUTHORIZED")], &[RENEW, RENEW_2]).await;
}

#[wasm_bindgen_test]
async fn meeting_not_found_on_a_takeover_is_not_retried_on_the_next_heartbeat() {
    takeover_rejected_with(&[(404, "MEETING_NOT_FOUND")], &[RENEW]).await;
}

#[wasm_bindgen_test]
async fn a_revoke_whose_stop_status_was_lost_lets_a_later_start_be_adopted() {
    let mount = mount_meeting(HOST).await;
    deliver(active_by_me());
    assert!(wait_for_posts(1, 1_000).await);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, 1_000).await);
    assert_eq!(posts()[1], OFF);
    let n = posts().len();
    sleep_ms(STATUS_MS + 100).await;
    assert_eq!(indicator(&mount), None);
    deliver(active_by_me());
    assert!(
        wait_for_posts(n + 1, RENEW_MS * 2).await,
        "not adopted: {:?}",
        posts()
    );
    assert_eq!(posts()[n], RENEW);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_revoke_still_retrying_across_the_status_timeout_blocks_adoption_until_it_ends() {
    let mount = mount_meeting(Setup {
        replies: &[(204, ""), (503, "CAPTIONS_UNAVAILABLE"), (204, "")],
        ..HOST
    })
    .await;
    deliver(active_by_me());
    assert!(wait_for_posts(1, 1_000).await);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, 1_000).await);
    assert_eq!(posts(), [RENEW, OFF]);
    sleep_ms(STATUS_MS + 100).await;
    assert_eq!(indicator(&mount), None);
    deliver(active_by_me());
    sleep_ms(150).await;
    assert_eq!(posts(), [RENEW, OFF], "adopted while the revoke retries");
    assert!(wait_for_posts(3, 2_000).await, "posted {:?}", posts());
    assert_eq!(posts()[2], OFF);
    sleep_ms(50).await;
    deliver(active_by_me());
    assert!(
        wait_for_posts(4, RENEW_MS * 2).await,
        "not adopted after the revoke ended: {:?}",
        posts()
    );
    assert_eq!(posts()[3], RENEW);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_superseded_revoke_ending_does_not_clear_a_later_rejection() {
    let mount = mount_meeting(Setup {
        replies: &[
            (204, ""),
            (503, "CAPTIONS_UNAVAILABLE"),
            (409, "CAPTIONS_OFF"),
        ],
        ..HOST
    })
    .await;
    deliver(active_by_me());
    assert!(wait_for_posts(1, 1_000).await);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, 1_000).await);
    sleep_ms(STATUS_MS + 100).await;
    assert_eq!(indicator(&mount), None);
    button(&mount).unwrap().click();
    assert!(wait_for_posts(3, 1_000).await);
    assert_eq!(posts(), [RENEW, OFF, ENABLE]);
    sleep_ms(800).await;
    deliver(active_by_me());
    assert_no_more_posts().await;
    assert_eq!(posts(), [RENEW, OFF, ENABLE], "the lease was re-adopted");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_timed_out_renewal_is_sent_again_at_once() {
    let mount = host_clicks_with(&[(204, "")]).await;
    assert!(wait_for_posts(2, RENEW_MS * 2).await);
    // Every later request outlives its deadline (3/4 of RENEW_MS).
    js_sys::eval("window.__captionsDelayMs = 1000;").unwrap();
    assert!(wait_for_posts(3, RENEW_MS * 2).await);
    assert!(
        wait_for_posts(4, RENEW_MS * 5 / 4).await,
        "the retry waited a full interval: {:?}",
        posts()
    );
    assert_eq!(&posts()[1..4], [RENEW, RENEW, RENEW]);
    assert!(
        wait_for_posts(7, RENEW_MS * 4).await,
        "hung renewals stopped: {:?}",
        posts()
    );
    assert!(*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn feature_not_found_hides_the_control_until_the_room_is_captioned() {
    let mount = host_clicks_with(&[(404, "NOT_FOUND")]).await;
    assert!(*handle().server_disabled.peek());
    assert!(button(&mount).is_none());
    assert_no_more_posts().await;
    deliver(active("Other host"));
    yield_now().await;
    assert!(!*handle().server_disabled.peek());
    assert!(button(&mount).is_some());
    assert!(!*handle().room_on.peek());
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE]);
    unmount_meeting(&mount).await;
}

async fn not_host_on_a_renewal(replies: &'static [(u16, &'static str)]) -> web_sys::Element {
    let mount = host_clicks_with(replies).await;
    assert!(
        wait_for_posts(2, RENEW_MS * 3).await,
        "posted {:?}",
        posts()
    );
    sleep_ms(50).await;
    assert_eq!(posts(), [ENABLE, RENEW]);
    assert!(!*handle().room_on.peek());
    mount
}

#[wasm_bindgen_test]
async fn an_active_status_lifts_a_not_host_and_the_host_adopts_its_lease() {
    let mount = not_host_on_a_renewal(&[(204, ""), (403, "NOT_HOST"), (204, "")]).await;
    deliver(active_by_me());
    yield_now().await;
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    assert!(
        wait_for_posts(4, RENEW_MS * 3).await,
        "posted {:?}",
        posts()
    );
    assert_eq!(posts(), [ENABLE, RENEW, RENEW, RENEW]);
    assert!(*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_second_not_host_after_a_lift_stays_latched() {
    let mount = not_host_on_a_renewal(&[(204, ""), (403, "NOT_HOST")]).await;
    deliver(active_by_me());
    assert!(wait_for_posts(3, 1_000).await, "posted {:?}", posts());
    sleep_ms(50).await;
    for _ in 0..4 {
        deliver(active_by_me());
        sleep_ms(RENEW_MS).await;
    }
    assert_eq!(posts(), [ENABLE, RENEW, RENEW]);
    assert_eq!(cc(&mount, "data-captions-role"), "viewer");
    assert!(!*handle().room_on.peek());

    deliver(active("Other host"));
    set(&IS_HOST, false);
    yield_now().await;
    set(&IS_HOST, true);
    yield_now().await;
    assert!(indicator(&mount).is_some());
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    unmount_meeting(&mount).await;
}

async fn latched_then_timed_out(replies: &'static [(u16, &'static str)]) -> web_sys::Element {
    let mount = not_host_on_a_renewal(replies).await;
    deliver(active_by_me());
    assert!(wait_for_posts(3, 1_000).await, "posted {:?}", posts());
    sleep_ms(STATUS_MS + 100).await;
    assert_eq!(indicator(&mount), None);
    assert_eq!(posts(), [ENABLE, RENEW, RENEW]);
    mount
}

#[wasm_bindgen_test]
async fn an_accepted_start_clears_a_latched_not_host() {
    let mount =
        latched_then_timed_out(&[(204, ""), (403, "NOT_HOST"), (403, "NOT_HOST"), (204, "")]).await;
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    button(&mount).unwrap().click();
    assert!(wait_for_posts(4, 1_000).await, "posted {:?}", posts());
    sleep_ms(20).await;
    deliver(active_by_me());
    yield_now().await;
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    assert_eq!(cc(&mount, "aria-pressed"), "true");
    assert!(
        wait_for_posts(5, RENEW_MS * 2).await,
        "posted {:?}",
        posts()
    );
    assert_eq!(posts(), [ENABLE, RENEW, RENEW, ENABLE, RENEW]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_latched_hosts_start_reported_active_before_its_204_keeps_renewing() {
    let mount =
        latched_then_timed_out(&[(204, ""), (403, "NOT_HOST"), (403, "NOT_HOST"), (204, "")]).await;
    js_sys::eval("window.__captionsDelayMs = 100;").unwrap();
    button(&mount).unwrap().click();
    assert!(wait_for_posts(4, 1_000).await, "posted {:?}", posts());
    deliver(active_by_me());
    yield_now().await;
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    assert!(
        wait_for_posts(5, RENEW_MS * 3).await,
        "posted {:?}",
        posts()
    );
    assert_eq!(posts(), [ENABLE, RENEW, RENEW, ENABLE, RENEW]);
    assert!(*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_latched_hosts_start_refused_after_its_own_active_status_latches_again() {
    const NOT_HOST: (u16, &str) = (403, "NOT_HOST");
    for replies in [
        &[(204, ""), NOT_HOST, NOT_HOST, (409, "CAPTIONS_OFF")],
        &[(204, ""), NOT_HOST, NOT_HOST, (401, "UNAUTHORIZED")],
        &[(204, ""), NOT_HOST, NOT_HOST, (404, "MEETING_NOT_FOUND")],
        &[(204, ""), NOT_HOST, NOT_HOST, (404, "NOT_FOUND")],
        &[(204, ""), NOT_HOST, NOT_HOST, (409, "MEETING_NOT_ACTIVE")],
    ] {
        let mount = latched_then_timed_out(replies).await;
        js_sys::eval("window.__captionsDelayMs = 100; window.__statusReply = 404;").unwrap();
        button(&mount).unwrap().click();
        assert!(
            wait_for_posts(4, 1_000).await,
            "{replies:?}: posted {:?}",
            posts()
        );
        deliver(active_by_me());
        yield_now().await;
        assert_eq!(cc(&mount, "data-captions-role"), "host", "{replies:?}");
        let mut waited = 0;
        while *handle().room_on.peek() && waited < 1_000 {
            sleep_ms(10).await;
            waited += 10;
        }
        assert!(!*handle().room_on.peek(), "{replies:?}");
        deliver(active_by_me());
        yield_now().await;
        assert_eq!(cc(&mount, "data-captions-role"), "viewer", "{replies:?}");
        unmount_meeting(&mount).await;
    }
}

#[wasm_bindgen_test]
async fn a_latched_hosts_pending_start_demoted_by_another_hosts_status_is_not_resent_when_it_stops()
{
    let mount = latched_then_timed_out(&[
        (204, ""),
        (403, "NOT_HOST"),
        (403, "NOT_HOST"),
        (503, "CAPTIONS_UNAVAILABLE"),
    ])
    .await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(4, 1_000).await, "posted {:?}", posts());
    deliver(active("Other host"));
    yield_now().await;
    assert_eq!(cc(&mount, "data-captions-role"), "viewer");
    sleep_ms(50).await;
    deliver(stopped());
    yield_now().await;
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    assert_no_more_posts().await;
    assert!(!*handle().room_on.peek());
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_latched_hosts_start_still_pending_at_the_status_timeout_is_not_resent_after_another_host(
) {
    let mount = latched_then_timed_out(&[
        (204, ""),
        (403, "NOT_HOST"),
        (403, "NOT_HOST"),
        (503, "CAPTIONS_UNAVAILABLE"),
    ])
    .await;
    button(&mount).unwrap().click();
    assert!(wait_for_posts(4, 1_000).await, "posted {:?}", posts());
    deliver(active_by_me());
    sleep_ms(STATUS_MS + 100).await;
    assert_eq!(indicator(&mount), None);
    deliver(active("Other host"));
    yield_now().await;
    assert_eq!(cc(&mount, "data-captions-role"), "viewer");
    sleep_ms(50).await;
    let sent = posts();
    sleep_ms(STATUS_MS + 100).await;
    assert_eq!(indicator(&mount), None);
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    assert_no_more_posts().await;
    assert_eq!(posts(), sent);
    assert!(!*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_latched_host_does_not_adopt_on_a_status_that_captions_the_room() {
    let mount = latched_then_timed_out(&[(204, ""), (403, "NOT_HOST")]).await;
    deliver(active_by_me());
    yield_now().await;
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE, RENEW, RENEW]);
    assert!(!*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn an_accepted_renewal_lets_a_later_active_status_lift_again() {
    let mount = not_host_on_a_renewal(&[
        (204, ""),
        (403, "NOT_HOST"),
        (204, ""),
        (403, "NOT_HOST"),
        (204, ""),
    ])
    .await;
    deliver(active_by_me());
    assert!(
        wait_for_posts(4, RENEW_MS * 3).await,
        "posted {:?}",
        posts()
    );
    sleep_ms(20).await;
    assert!(!*handle().room_on.peek());
    deliver(active_by_me());
    assert!(wait_for_posts(5, RENEW_MS).await, "posted {:?}", posts());
    assert_eq!(posts(), [ENABLE, RENEW, RENEW, RENEW, RENEW]);
    assert!(*handle().room_on.peek());
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_stopped_status_unlatches_a_not_host_for_the_next_active_status() {
    let mount =
        not_host_on_a_renewal(&[(204, ""), (403, "NOT_HOST"), (403, "NOT_HOST"), (204, "")]).await;
    deliver(active_by_me());
    assert!(wait_for_posts(3, 1_000).await, "posted {:?}", posts());
    sleep_ms(20).await;
    deliver(stopped());
    yield_now().await;
    deliver(active_by_me());
    assert!(wait_for_posts(4, RENEW_MS).await, "posted {:?}", posts());
    assert_eq!(posts(), [ENABLE, RENEW, RENEW, RENEW]);
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_feature_off_on_an_adopted_lease_is_not_adopted_again() {
    let mount = mount_meeting(Setup {
        replies: &[(404, "NOT_FOUND")],
        ..HOST
    })
    .await;
    deliver(active_by_me());
    assert!(wait_for_posts(1, 1_000).await);
    sleep_ms(20).await;
    assert!(*handle().server_disabled.peek());
    for _ in 0..2 {
        deliver(active_by_me());
        sleep_ms(RENEW_MS).await;
    }
    assert!(button(&mount).is_some());
    assert_eq!(posts(), [RENEW]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn meeting_not_found_stops_renewing_without_hiding_the_control() {
    let mount = host_clicks_with(&[(404, "MEETING_NOT_FOUND")]).await;
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE]);
    assert!(!*handle().room_on.peek());
    assert_eq!(cc(&mount, "data-captions-pending"), "false");
    assert!(!*handle().server_disabled.peek());
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn transient_failures_retry_as_a_first_enable() {
    for replies in [
        &[(404, "NOT_IN_MEETING")],
        &[(403, "MEETING_NOT_FOUND")],
        &[(429, "RATE_LIMIT_EXCEEDED")],
        &[(503, "CAPTIONS_UNAVAILABLE")],
    ] {
        let mount = host_clicks_with(replies).await;
        assert!(
            wait_for_posts(3, RENEW_MS * 6).await,
            "{replies:?}: posted {:?}",
            posts()
        );
        assert_eq!(&posts()[..3], [ENABLE, ENABLE, ENABLE], "{replies:?}");
        assert!(!*handle().server_disabled.peek(), "{replies:?}");
        assert_eq!(cc(&mount, "data-captions-role"), "host", "{replies:?}");
        assert_eq!(cc(&mount, "aria-pressed"), "false", "{replies:?}");
        assert_eq!(cc(&mount, "data-captions-pending"), "true", "{replies:?}");
        assert_eq!(start_failure(&mount), None, "{replies:?}");
        unmount_meeting(&mount).await;
    }
}

#[wasm_bindgen_test]
async fn renewals_start_only_after_an_accepted_enable() {
    let mount = host_clicks_with(&[(503, "CAPTIONS_UNAVAILABLE"), (204, "")]).await;
    assert!(
        wait_for_posts(3, RENEW_MS * 6).await,
        "posted {:?}",
        posts()
    );
    assert_eq!(&posts()[..3], [ENABLE, ENABLE, RENEW]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn not_host_with_the_room_uncaptioned_leaves_the_control_clickable() {
    let mount = host_clicks_with(&[(403, "NOT_HOST")]).await;
    assert_no_more_posts().await;
    assert!(!*handle().server_disabled.peek());
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    assert_ne!(cc(&mount, "aria-disabled"), "true");
    button(&mount).unwrap().click();
    assert!(wait_for_posts(2, 1_000).await);
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE, ENABLE]);
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn captions_off_on_a_renewal_stops_and_unpresses_the_button() {
    let mount = host_clicks_with(&[(204, ""), (409, "CAPTIONS_OFF")]).await;
    assert!(
        wait_for_posts(2, RENEW_MS * 4).await,
        "posted {:?}",
        posts()
    );
    sleep_ms(50).await;
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE, RENEW]);
    assert!(!*handle().room_on.peek());
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn meeting_not_active_on_enable_stops_and_unpresses_the_button() {
    let mount = host_clicks_with(&[(409, "MEETING_NOT_ACTIVE")]).await;
    assert_no_more_posts().await;
    assert_eq!(posts(), [ENABLE]);
    assert!(!*handle().room_on.peek());
    assert!(!*handle().server_disabled.peek());
    assert_eq!(cc(&mount, "aria-pressed"), "false");
    assert_eq!(cc(&mount, "data-captions-role"), "host");
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn a_viewer_picks_the_caption_language_and_the_choice_is_saved() {
    let mount = mount_meeting(VIEWER).await;
    let picker = |m: &web_sys::Element| {
        m.query_selector("[data-testid='captions-language-select']")
            .unwrap()
            .map(|e| e.dyn_into::<web_sys::HtmlSelectElement>().unwrap())
    };
    assert!(
        picker(&mount).is_none(),
        "no picker until the room is captioned"
    );
    deliver(active("Host"));
    yield_now().await;
    let select = picker(&mount).expect("language picker");
    let options: Vec<String> = (0..select.length())
        .filter_map(|i| select.item(i))
        .map(|o| o.text_content().unwrap_or_default())
        .collect();
    assert_eq!(options, ["English", "Español", "Français", "Italiano"]);
    deliver(CaptionUpdate::Text(translated(
        event(1, 1, 1, true, "Good morning"),
        &[("en", "Good morning"), ("fr", "Bonjour")],
    )));
    yield_now().await;
    select.set_value("fr");
    let init = web_sys::EventInit::new();
    init.set_bubbles(true);
    let change = web_sys::Event::new_with_event_init_dict("change", &init).unwrap();
    select.dispatch_event(&change).unwrap();
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        ["Bonjour"]
    );
    assert_eq!(*handle().language.peek(), "fr");
    let saved = gloo_utils::window()
        .local_storage()
        .unwrap()
        .unwrap()
        .get_item("vc_captions_language")
        .unwrap();
    assert_eq!(saved.as_deref(), Some("\"fr\""));

    handle().set_language("xx");
    yield_now().await;
    assert_eq!(
        *handle().language.peek(),
        "fr",
        "an unknown code is ignored"
    );
    let saved = gloo_utils::window()
        .local_storage()
        .unwrap()
        .unwrap()
        .get_item("vc_captions_language")
        .unwrap();
    assert_eq!(saved.as_deref(), Some("\"fr\""));
    unmount_meeting(&mount).await;
}

#[wasm_bindgen_test]
async fn packets_after_unmount_are_ignored() {
    let mount = mount_meeting(VIEWER).await;
    let late = handle();
    deliver(active("Hana"));
    yield_now().await;
    set(&MOUNTED, false);
    sleep_ms(50).await;
    late.on_update(active("Hana"));
    late.on_update(CaptionUpdate::Text(event(1, 1, 1, true, "late")));
    late.on_update(stopped());
    sleep_ms(STATUS_MS + 100).await;
    assert!(late.notice.try_peek().is_err());
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_earliest_expiring_line_is_pruned_on_time() {
    let mount = mount_overlay().await;
    push(event(1, 1, 1, false, "interim"));
    let board = BOARD.with(|b| b.borrow().expect("board signal"));
    let soon = now_ms() - CAPTION_FINAL_LINGER_MS + 100.0;
    ingest_caption(board, event(2, 1, 1, true, "final"), soon);
    yield_now().await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        ["interim", "final"]
    );
    sleep_ms(300).await;
    assert_eq!(
        texts(&mount, "[data-testid='caption-line-text']"),
        ["interim"]
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_refreshed_final_is_not_announced_again() {
    let mount = mount_overlay().await;
    push(event(1, 1, 1, true, "hello"));
    yield_now().await;
    let region = "[data-testid='captions-live-region'] p";
    let first = mount.query_selector(region).unwrap().expect("announced");
    push(event(1, 1, 2, true, "hello"));
    push(event(2, 1, 1, false, "ciao"));
    yield_now().await;
    let again = mount.query_selector(region).unwrap().expect("still listed");
    assert!(first.is_same_node(Some(&again)));
    assert_eq!(all(&mount, region).len(), 1);
    cleanup(&mount);
}

fn lane_frame(markup: &str) -> (web_sys::Element, web_sys::Document) {
    let doc = gloo_utils::document();
    let frame = doc.create_element("iframe").unwrap();
    doc.body().unwrap().append_child(&frame).unwrap();
    let inner: web_sys::Document = js_sys::Reflect::get(&frame, &"contentDocument".into())
        .unwrap()
        .unchecked_into();
    for css in [
        include_str!("../static/leptos-style.css"),
        include_str!("../static/tailwind.css"),
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
        "* { transition: none !important; }",
    ] {
        let style = inner.create_element("style").unwrap();
        style.set_text_content(Some(css));
        inner.head().unwrap().append_child(&style).unwrap();
    }
    let buttons =
        "<div class='action-bar-slot-wrapper'><button class='video-control-button'></button></div>"
            .repeat(8);
    inner.body().unwrap().set_inner_html(&format!(
        "<div id='reserves'><div id='main-container' class='meeting-page'>{markup}\
         <div id='grid-container'><nav id='host-controls-nav' class='host' \
         data-self-placement='corner' data-self-hidden='false'><div class='controls'>\
         <div class='video-controls-container'>{buttons}</div></div>\
         <div class='host-video-wrapper'></div></nav>\
         <div class='host-controls-icon-slot'><button></button></div>\
         </div></div></div>"
    ));
    (frame, inner)
}

#[derive(Clone, Copy, PartialEq)]
enum Lane {
    Clear,
    OverSelf,
    Narrow,
    Centred(f64),
    Spans(f64, f64),
    /// Clear, with at least this gap to the self view or show-self icon.
    Gap(f64),
}

fn lay_out(
    frame: &web_sys::Element,
    doc: &web_sys::Document,
    case: (f64, &str, f64, f64, bool, bool),
) {
    let (width, dock, left, right, self_hidden, icon) = case;
    let q = |s: &str| doc.query_selector(s).unwrap().unwrap();
    let set = |s: &str, attr: &str, v: String| q(s).set_attribute(attr, &v).unwrap();
    frame
        .set_attribute(
            "style",
            &format!("width: {width}px; height: 800px; border: 0"),
        )
        .unwrap();
    set(
        "#reserves",
        "style",
        format!(
            "display: flex; --drawer-left-reserve: {left}px; --drawer-right-reserve: {right}px"
        ),
    );
    set("#reserves", "data-dock", dock.to_string());
    set(
        "#grid-container",
        "style",
        format!("position: absolute; top: 0; bottom: 0; left: {left}px; right: {right}px"),
    );
    set(
        ".video-controls-container",
        "class",
        format!("video-controls-container {dock}"),
    );
    set(
        "#host-controls-nav",
        "data-self-hidden",
        self_hidden.to_string(),
    );
    let modifier = if dock == "dock-right" {
        " self-view-hidden-icon--dock-right"
    } else {
        ""
    };
    // The real icon is unmounted for a participant who cannot stream, so a
    // hidden one must not leave a node that `:has()` can match.
    let class = if icon {
        format!("video-control-button self-view-hidden-icon{modifier}")
    } else {
        "unmounted".to_string()
    };
    set(".host-controls-icon-slot > *", "class", class);
}

fn rect_of(doc: &web_sys::Document, selector: &str) -> web_sys::DomRect {
    doc.query_selector(selector)
        .unwrap()
        .unwrap()
        .get_bounding_client_rect()
}

fn span(r: &web_sys::DomRect) -> String {
    format!("{}..{} x {}..{}", r.left(), r.right(), r.top(), r.bottom())
}

fn overlap(a: &web_sys::DomRect, b: &web_sys::DomRect) -> bool {
    a.left() < b.right() && b.left() < a.right() && a.top() < b.bottom() && b.top() < a.bottom()
}

#[wasm_bindgen_test]
async fn the_lane_clears_docks_drawers_and_the_corner_self_view_but_stays_readable() {
    let mount = mount_overlay().await;
    push(event(1, 1, 1, true, &"a long caption line ".repeat(12)));
    yield_now().await;
    let markup = mount
        .query_selector(".captions-overlay")
        .unwrap()
        .expect("the lane renders")
        .outer_html();
    cleanup(&mount);
    let (frame, doc) = lane_frame(&markup);

    use Lane::*;
    for (case, want) in [
        ((700.0, "dock-bottom", 0.0, 0.0, false, false), Clear),
        ((700.0, "dock-left", 0.0, 0.0, false, false), Clear),
        ((700.0, "dock-right", 0.0, 0.0, false, false), Clear),
        ((900.0, "dock-bottom", 300.0, 0.0, false, false), Clear),
        ((900.0, "dock-left", 300.0, 0.0, false, false), Clear),
        ((900.0, "dock-right", 300.0, 0.0, false, false), Clear),
        ((900.0, "dock-bottom", 0.0, 360.0, false, false), Clear),
        ((900.0, "dock-right", 0.0, 360.0, false, false), Clear),
        ((1100.0, "dock-left", 0.0, 360.0, false, false), Clear),
        ((700.0, "dock-left", 300.0, 0.0, false, false), OverSelf),
        ((800.0, "dock-bottom", 0.0, 360.0, false, false), OverSelf),
        ((600.0, "dock-left", 300.0, 0.0, false, false), Narrow),
        ((1440.0, "dock-bottom", 0.0, 0.0, false, false), Clear),
        (
            (1440.0, "dock-bottom", 0.0, 0.0, false, false),
            Spans(36.0, 1120.0),
        ),
        ((1280.0, "dock-bottom", 0.0, 0.0, false, false), Clear),
        (
            (1280.0, "dock-bottom", 0.0, 0.0, false, false),
            Spans(16.0, 990.0),
        ),
        ((1920.0, "dock-bottom", 0.0, 0.0, false, false), Clear),
        (
            (1920.0, "dock-bottom", 0.0, 0.0, false, false),
            Centred(1120.0),
        ),
        (
            (1920.0, "dock-bottom", 0.0, 0.0, true, false),
            Centred(1120.0),
        ),
        (
            (1440.0, "dock-bottom", 0.0, 0.0, true, false),
            Centred(1120.0),
        ),
        (
            (1000.0, "dock-bottom", 0.0, 0.0, true, false),
            Centred(820.0),
        ),
        (
            (900.0, "dock-bottom", 0.0, 0.0, true, false),
            Centred(738.0),
        ),
        (
            (800.0, "dock-bottom", 0.0, 0.0, true, false),
            Centred(720.0),
        ),
        ((800.0, "dock-right", 0.0, 0.0, true, false), Clear),
        ((800.0, "dock-bottom", 0.0, 0.0, true, true), Clear),
        ((900.0, "dock-right", 0.0, 0.0, true, true), Clear),
        ((900.0, "dock-bottom", 0.0, 360.0, true, true), Clear),
        ((900.0, "dock-bottom", 0.0, 0.0, false, false), Gap(20.0)),
        ((800.0, "dock-bottom", 0.0, 0.0, true, true), Gap(20.0)),
        ((900.0, "dock-right", 0.0, 0.0, true, true), Gap(20.0)),
        ((567.0, "dock-right", 0.0, 0.0, true, true), Clear),
        ((500.0, "dock-right", 0.0, 0.0, true, true), Gap(20.0)),
        ((460.0, "dock-right", 0.0, 0.0, true, true), Clear),
        ((500.0, "dock-left", 0.0, 0.0, true, true), Clear),
        ((500.0, "dock-bottom", 0.0, 0.0, true, true), Clear),
        ((375.0, "dock-bottom", 0.0, 0.0, true, true), Clear),
        ((375.0, "dock-right", 0.0, 0.0, true, true), Narrow),
        ((375.0, "dock-left", 0.0, 0.0, true, true), Narrow),
        (
            (567.0, "dock-bottom", 0.0, 0.0, false, false),
            Spans(16.0, 535.0),
        ),
        (
            (500.0, "dock-bottom", 0.0, 0.0, false, false),
            Spans(16.0, 468.0),
        ),
    ] {
        lay_out(&frame, &doc, case);
        let (width, dock, left, right, self_hidden, icon) = case;
        let at = format!("{width}px {dock} reserves {left}/{right}");
        let lane = rect_of(&doc, ".captions-overlay");
        let nav = rect_of(&doc, "#host-controls-nav");
        let bar = rect_of(&doc, ".video-controls-container");
        let icon_rect = rect_of(&doc, ".host-controls-icon-slot > *");
        let floor = 280.0_f64.min(width - left - right - 32.0);
        assert!(lane.height() > 0.0, "{at}: lane box");
        assert!(
            lane.width() >= floor - 0.5,
            "{at}: lane {} under {floor}px",
            span(&lane)
        );
        assert!(
            lane.left() >= left && lane.right() <= width - right,
            "{at}: drawers"
        );
        let clear = matches!(want, Clear | Gap(_));
        if clear || want == OverSelf {
            assert!(
                dock == "dock-bottom" || !overlap(&lane, &bar),
                "{at}: dock {} under lane {}",
                span(&bar),
                span(&lane)
            );
        }
        if clear {
            assert!(
                self_hidden || !overlap(&lane, &nav),
                "{at}: self view {} under lane {}",
                span(&nav),
                span(&lane)
            );
            assert!(
                !icon || (icon_rect.width() > 0.0 && !overlap(&lane, &icon_rect)),
                "{at}: show-self icon {} under lane {}",
                span(&icon_rect),
                span(&lane)
            );
        }
        if let Gap(g) = want {
            let obstacle = if self_hidden { &icon_rect } else { &nav };
            assert!(
                obstacle.left() - lane.right() >= g - 0.5,
                "{at}: lane {} within {g}px of {}",
                span(&lane),
                span(obstacle)
            );
        }
        if let Spans(x, w) = want {
            assert_eq!(
                (lane.left().round(), lane.width().round()),
                (x, w),
                "{at}: spans"
            );
        }
        if let Centred(w) = want {
            assert_eq!(
                (lane.left().round(), lane.width().round()),
                (((width - w) / 2.0).round(), w),
                "{at}: centred"
            );
        }
    }
    frame.remove();
}

#[wasm_bindgen_test]
async fn a_wide_lane_gives_a_caption_a_fourth_row_and_a_larger_font() {
    let mount = mount_overlay().await;
    push(event(1, 1, 1, true, &"a long caption line ".repeat(60)));
    yield_now().await;
    let markup = mount
        .query_selector(".captions-overlay")
        .unwrap()
        .expect("the lane renders")
        .outer_html();
    cleanup(&mount);
    let (frame, doc) = lane_frame(&markup);
    let q = |s: &str| doc.query_selector(s).unwrap().unwrap();
    q("#main-container")
        .insert_adjacent_html(
            "beforeend",
            "<div class='host-controls-container'><div class='host-controls-list'></div></div>",
        )
        .unwrap();
    let view = doc.default_view().unwrap();
    let style = |s: &str, prop: &str| {
        let value = view
            .get_computed_style(&q(s))
            .unwrap()
            .unwrap()
            .get_property_value(prop)
            .unwrap();
        value.trim_end_matches("px").parse::<f64>().unwrap()
    };

    for ((width, height, left, right, self_hidden), (lane_w, rows, font), (lane_max, list_max)) in [
        (
            (1097.0, 800.0, 0.0, 0.0, true),
            (899.5, 3.0, 16.0),
            (248.0, 248.0),
        ),
        (
            (1100.0, 800.0, 0.0, 0.0, true),
            (902.0, 4.0, 16.04),
            (248.0, 248.0),
        ),
        (
            (1279.0, 800.0, 0.0, 0.0, true),
            (1048.8, 4.0, 18.0),
            (248.0, 248.0),
        ),
        (
            (1280.0, 800.0, 0.0, 0.0, false),
            (989.6, 4.0, 17.79),
            (280.0, 216.0),
        ),
        (
            (1440.0, 900.0, 300.0, 208.0, true),
            (900.0, 4.0, 16.0),
            (312.0, 284.0),
        ),
        (
            (1440.0, 900.0, 300.0, 209.0, true),
            (899.0, 3.0, 16.0),
            (312.0, 284.0),
        ),
        (
            (1440.0, 900.0, 300.0, 360.0, true),
            (748.0, 3.0, 16.0),
            (312.0, 284.0),
        ),
        (
            (1920.0, 1080.0, 0.0, 0.0, true),
            (1120.0, 4.0, 18.0),
            (312.0, 300.0),
        ),
        (
            (2560.0, 1440.0, 0.0, 0.0, true),
            (1120.0, 4.0, 18.0),
            (312.0, 300.0),
        ),
    ] {
        lay_out(
            &frame,
            &doc,
            (width, "dock-bottom", left, right, self_hidden, false),
        );
        frame
            .set_attribute(
                "style",
                &format!("width: {width}px; height: {height}px; border: 0"),
            )
            .unwrap();
        let at = format!("{width}x{height} reserves {left}/{right}");
        let lane = rect_of(&doc, ".captions-overlay").width();
        assert!((lane - lane_w).abs() < 0.1, "{at}: lane {lane}");
        let px = style(".caption-line", "font-size");
        assert!((px - font).abs() < 0.01, "{at}: font {px}");
        let text = rect_of(&doc, ".caption-line__text").height();
        assert_eq!((text / (1.4 * px)).round(), rows, "{at}: rows");
        assert_eq!(
            style(".captions-overlay", "max-height"),
            lane_max,
            "{at}: lane"
        );
        assert_eq!(
            style(".host-controls-list", "max-height"),
            list_max,
            "{at}: host controls"
        );
    }
    frame.remove();
}

async fn lane_markup(captions: &[&str]) -> String {
    let mount = mount_overlay().await;
    for (session, text) in (1..).zip(captions) {
        push(event(session, 1, 1, true, text));
    }
    yield_now().await;
    let markup = mount
        .query_selector(".captions-overlay")
        .unwrap()
        .expect("the lane renders")
        .outer_html();
    cleanup(&mount);
    markup
}

#[wasm_bindgen_test]
async fn the_lane_fades_only_a_caption_that_overflows_its_top() {
    let long = "a long caption line ".repeat(60);
    let mut lanes = vec![
        ("short", lane_markup(&["short"]).await),
        ("one long", lane_markup(&[&long]).await),
        ("two long", lane_markup(&[&long, &long]).await),
    ];
    for middle in [3, 5, 7, 9, 11, 60] {
        let text = "a long caption line ".repeat(middle);
        let markup = lane_markup(&[&long, &text, &long]).await;
        lanes.push((if middle == 60 { "three long" } else { "three" }, markup));
    }
    let expected = |lane: &str, width: f64, height: f64| match (lane, width, height) {
        ("short", _, _) => Some(false),
        ("one long", 1100.0, 800.0) | ("one long", 800.0, 700.0) => Some(false),
        ("two long", 1920.0, 1080.0) | ("two long", 1440.0, 900.0) => Some(false),
        ("three long", 1280.0, 800.0) => Some(true),
        _ => None,
    };

    let mut cut_at_1280 = 0;
    for (name, markup) in lanes {
        let (frame, doc) = lane_frame(&markup);
        let view = doc.default_view().unwrap();
        let px = |e: &web_sys::Element, prop: &str| -> f64 {
            let v = view
                .get_computed_style(e)
                .unwrap()
                .unwrap()
                .get_property_value(prop)
                .unwrap();
            v.trim_end_matches("px").parse().unwrap()
        };
        for (width, height) in [
            (1280.0, 800.0),
            (1440.0, 900.0),
            (1536.0, 864.0),
            (1920.0, 1080.0),
            (1100.0, 800.0),
            (900.0, 720.0),
            (800.0, 700.0),
            (500.0, 800.0),
        ] {
            lay_out(&frame, &doc, (width, "dock-bottom", 0.0, 0.0, false, false));
            frame
                .set_attribute(
                    "style",
                    &format!("width: {width}px; height: {height}px; border: 0"),
                )
                .unwrap();
            let at = format!("{width}x{height} {name}");
            let lane_el = doc.query_selector(".captions-overlay").unwrap().unwrap();
            let mask = view
                .get_computed_style(&lane_el)
                .unwrap()
                .unwrap()
                .get_property_value("mask-image")
                .unwrap();
            let stops: Vec<f64> = mask
                .split("px")
                .filter_map(|p| p.rsplit(' ').next()?.parse().ok())
                .collect();
            let [0.0, fade] = stops[..] else {
                panic!("{at}: mask {mask}");
            };
            let lane = lane_el.get_bounding_client_rect();
            let lines = lane_el.query_selector_all(".caption-line").unwrap();
            let rect = |i: u32| {
                lines
                    .item(i)
                    .unwrap()
                    .unchecked_into::<web_sys::Element>()
                    .get_bounding_client_rect()
            };
            let (first, last) = (rect(0), rect(lines.length() - 1));
            let room = px(&lane_el, "max-height") - px(&lane_el, "padding-top");
            let overflows = last.bottom() - first.top() > room + 0.5;
            if let Some(want) = expected(name, width, height) {
                assert_eq!(overflows, want, "{at}: overflow");
            }
            if !overflows {
                assert!(
                    first.top() - lane.top() >= fade - 0.5,
                    "{at}: caption top {} inside the {fade}px fade below {}",
                    first.top(),
                    lane.top()
                );
                continue;
            }
            assert!(
                (lane.height() - room - fade).abs() < 0.5,
                "{at}: lane {} not full",
                lane.height()
            );
            let parts = lane_el
                .query_selector_all(".caption-line__speaker, .caption-line__text")
                .unwrap();
            let cut: Vec<f64> = (0..parts.length())
                .filter_map(|i| parts.item(i))
                .map(|n| n.unchecked_into::<web_sys::Element>())
                .filter_map(|e| {
                    let r = e.get_bounding_client_rect();
                    let row = px(&e, "line-height");
                    (r.top() < lane.top() && r.bottom() > lane.top())
                        .then(|| (r.bottom() - lane.top()) % row)
                })
                .filter(|&sliver| sliver > 0.5)
                .collect();
            assert!(
                cut.iter().all(|&sliver| sliver <= fade),
                "{at}: slivers {cut:?} past a {fade}px fade"
            );
            if width == 1280.0 {
                cut_at_1280 += cut.len();
            }
        }
        frame.remove();
    }
    assert!(cut_at_1280 > 0, "no lane at 1280x800 cut a caption");
}

#[wasm_bindgen_test]
async fn an_open_tile_menu_hides_the_lane() {
    let (frame, doc) = lane_frame(&lane_markup(&["over the tile menu"]).await);
    lay_out(
        &frame,
        &doc,
        (1920.0, "dock-bottom", 0.0, 0.0, false, false),
    );
    let view = doc.default_view().unwrap();
    let lane = doc.query_selector(".captions-overlay").unwrap().unwrap();
    let visibility = || {
        view.get_computed_style(&lane)
            .unwrap()
            .unwrap()
            .get_property_value("visibility")
            .unwrap()
    };
    let icons = doc.create_element("div").unwrap();
    icons.set_class_name("tile-top-icons");
    doc.query_selector("#grid-container")
        .unwrap()
        .unwrap()
        .append_child(&icons)
        .unwrap();
    assert_eq!(visibility(), "visible", "menu closed");
    icons.set_class_name(&format!("tile-top-icons {TILE_MENU_OPEN_CLASS}"));
    assert_eq!(visibility(), "hidden", "menu open");
    icons.set_class_name("tile-top-icons");
    assert_eq!(visibility(), "visible", "menu closed again");
    frame.remove();
}
