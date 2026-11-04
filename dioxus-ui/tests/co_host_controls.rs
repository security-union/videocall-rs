// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Owner-only co-host controls, the Host / Co-host labels, and the
// host menu gates. A co-host holds the host role but must not get the owner's
// surfaces.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use dioxus::prelude::*;
use dioxus_ui::components::announcement_overrides::use_override_editor;
use dioxus_ui::components::canvas_generator::PinnedTile;
use dioxus_ui::components::captions::MAX_CAPTION_LINES;
use dioxus_ui::components::co_hosts::{
    post_co_host_notice, run_co_host_request, show_host_change_toast, CoHostMenuAction,
    CoHostNotice, CoHostNoticeCtx, CoHostNoticeLayer, CoHostRequest, CoHostsSection, HostChange,
    HostChangeNotice, HostChangeToast, MeetingOwnership,
};
use dioxus_ui::components::host_action_notice::{HostActionKind, HostActionNotice};
use dioxus_ui::components::host_controls::{publish_panel_bottom, HostControls};
use dioxus_ui::components::live_tile_menu_dismiss_listeners_for_test;
use dioxus_ui::components::media_metrics_overlay::MediaMetricsOverlayCtx;
use dioxus_ui::components::meeting_options_controls::MeetingOptionsPanel;
use dioxus_ui::components::peer_list::{PeerList, PeerListEntry, RosterLiveness};
use dioxus_ui::components::peer_list_item::PeerListItem;
use dioxus_ui::components::peer_tile::PeerTile;
use dioxus_ui::components::pre_join_settings_card::PreJoinSettingsCard;
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, HostSetCtx, MeetingTime, PeerAudioLivenessMap,
    PeerSignalHistoryMap, SignalPopupStateMap,
};
use support::{
    cleanup, create_mount_point, inject_app_config, inject_app_config_pkce, render_into,
    reset_test_browser_state, restore_fetch, wait_for_selector, yield_now,
};
use videocall_client::VideoCallClient;
use videocall_diagnostics::{global_sender, metric, DiagEvent};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const OWNER: &str = "owner@example.com";
const COHOST: &str = "cohost@example.com";
const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";

#[derive(Clone, Default)]
struct Scenario {
    viewer: String,
    viewer_is_host: bool,
    hosts: Vec<String>,
    peer: String,
    meeting_active: bool,
    with_host_set: bool,
    card_title: bool,
    read_only: bool,
}

thread_local! {
    static SCENARIO: RefCell<Scenario> = RefCell::new(Scenario::default());
    static REFRESH: RefCell<Option<Signal<u64>>> = const { RefCell::new(None) };
}

fn scenario() -> Scenario {
    SCENARIO.with(|s| s.borrow().clone())
}

fn set_scenario(s: Scenario) {
    SCENARIO.with(|slot| *slot.borrow_mut() = s);
}

fn ownership(viewer: &str) -> MeetingOwnership {
    MeetingOwnership::of(Some(OWNER), Some(viewer))
}

async fn settle() {
    for _ in 0..6 {
        yield_now().await;
    }
}

async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        gloo_utils::window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

fn query(mount: &web_sys::Element, selector: &str) -> Option<web_sys::Element> {
    mount.query_selector(selector).unwrap()
}

fn text_of(mount: &web_sys::Element, selector: &str) -> Option<String> {
    query(mount, selector).map(|el| el.text_content().unwrap_or_default())
}

fn html(el: &web_sys::Element) -> web_sys::HtmlElement {
    el.clone().dyn_into::<web_sys::HtmlElement>().unwrap()
}

fn click(mount: &web_sys::Element, selector: &str) {
    html(
        &query(mount, selector)
            .unwrap_or_else(|| panic!("{selector} must be rendered to be clicked")),
    )
    .click();
}

fn is_focused(el: &web_sys::Element) -> bool {
    gloo_utils::document()
        .active_element()
        .is_some_and(|active| active.is_same_node(Some(el)))
}

/// Through Dioxus's delegated `oninput`, which needs a bubbling event.
async fn type_into(input: &web_sys::Element, text: &str) {
    let field: web_sys::HtmlInputElement = input.clone().unchecked_into();
    field.set_value(text);
    let init = web_sys::EventInit::new();
    init.set_bubbles(true);
    let event = web_sys::Event::new_with_event_init_dict("input", &init).unwrap();
    field.dispatch_event(&event).unwrap();
    settle().await;
}

fn press_escape(el: &web_sys::Element) {
    let dispatch = js_sys::Function::new_with_args(
        "el",
        "el.dispatchEvent(new KeyboardEvent('keydown', \
         { key: 'Escape', bubbles: true, cancelable: true }));",
    );
    dispatch.call1(&wasm_bindgen::JsValue::NULL, el).unwrap();
}

fn press_tab(el: &web_sys::Element, shift: bool) {
    let dispatch = js_sys::Function::new_with_args(
        "el, shift",
        "el.dispatchEvent(new KeyboardEvent('keydown', \
         { key: 'Tab', shiftKey: shift, bubbles: true, cancelable: true }));",
    );
    dispatch
        .call2(
            &wasm_bindgen::JsValue::NULL,
            el,
            &wasm_bindgen::JsValue::from_bool(shift),
        )
        .unwrap();
}

fn menu_items(mount: &web_sys::Element, selector: &str) -> Vec<String> {
    let nodes = mount.query_selector_all(selector).unwrap();
    (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .map(|n| n.text_content().unwrap_or_default().trim().to_string())
        .collect()
}

/// `window.__coHostMode` picks the canned outcomes; `window.__coHostGets`
/// counts list reads.
fn mock_co_host_api(mode: &str) {
    let script = format!(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.__coHostMode = {mode};
        window.__coHostGets = 0;
        window.__coHostSuggestGets = 0;
        window.__coHostGrants = [];
        window.__coHostSuggestions = [
            {{ user_id: 'Zoe@Example.com', display_name: 'Zoe Park' }},
            {{ user_id: 'joanna@example.com', display_name: 'Joanna Diaz' }},
            {{ user_id: 'ann.lee@example.com', display_name: 'Ann Lee' }},
            {{ user_id: 'alice@example.com', display_name: 'Alice' }},
            {{ user_id: 'sam.r@example.com', display_name: 'Sam Rivera' }},
            {{ user_id: 'srivera@example.com', display_name: 'Sam Rivera' }},
            {{ user_id: 'pat@example.com' }}
        ];
        window.__coHostRows = [
            {{ user_id: 'alice@example.com', persistent: true, is_present_host: true,
               display_name: 'Alice', designated: true, suspended: false }},
            {{ user_id: 'bob@example.com', persistent: false, is_present_host: false,
               designated: true, suspended: false }},
            {{ user_id: 'terry@example.com', persistent: false, is_present_host: true,
               designated: false, suspended: false }},
            {{ user_id: 'sam@example.com', persistent: true, is_present_host: false,
               designated: true, suspended: true }}
        ];
        if (window.__coHostMode.rows) {{
            window.__coHostRows = window.__coHostMode.rows;
        }}
        window.fetch = function(input, init) {{
            var req = typeof input === 'string' ? null : input;
            var url = req ? req.url : input;
            var method = ((init && init.method) || (req && req.method) || 'GET').toUpperCase();
            var bodyText = req ? req.clone().text() : Promise.resolve((init && init.body) || '');
            var mode = window.__coHostMode;
            var list = function() {{
                return {{ success: true, result: {{ co_hosts: window.__coHostRows }} }};
            }};
            var respond = function(status, body) {{
                var resp = new Response(JSON.stringify(body), {{
                    status: status,
                    headers: {{ 'Content-Type': 'application/json' }}
                }});
                Object.defineProperty(resp, 'url', {{ value: url }});
                return resp;
            }};
            if (url.match(/\/co-hosts\/revoke$/)) {{
                if (mode.revoke !== 'ok') {{
                    return Promise.resolve(respond(409, {{ success: false, result: {{
                        code: 'LAST_PRESENT_HOST',
                        message: 'Cannot remove the only host present in the meeting.'
                    }} }}));
                }}
                return bodyText.then(function(text) {{
                    var who = JSON.parse(text).user_id;
                    window.__coHostRows = window.__coHostRows.filter(function(r) {{
                        return r.user_id !== who;
                    }});
                    return respond(200, list());
                }});
            }}
            if (url.match(/\/co-hosts$/) && method === 'POST') {{
                if (mode.grant === 'pending') {{
                    return new Promise(function() {{}});
                }}
                if (mode.grant === 'inactive') {{
                    return Promise.resolve(respond(400, {{ success: false, result: {{
                        code: 'BAD_REQUEST',
                        message: 'an instance-only co-host requires an active meeting; set persist to save it for future instances'
                    }} }}));
                }}
                return bodyText.then(function(text) {{
                    var granted = JSON.parse(text);
                    window.__coHostGrants.push(granted);
                    var stored = granted.user_id.toLowerCase();
                    if (!window.__coHostRows.some(function(r) {{ return r.user_id === stored; }})) {{
                        window.__coHostRows = window.__coHostRows.concat([{{
                            user_id: stored, persistent: true, is_present_host: false,
                            designated: true, suspended: false
                        }}]);
                    }}
                    return respond(200, list());
                }});
            }}
            if (url.match(/\/co-hosts$/)) {{
                window.__coHostGets += 1;
                if (mode.list === 'forbidden') {{
                    return Promise.resolve(respond(403, {{ success: false, result: {{
                        code: 'NOT_OWNER', message: 'not the owner'
                    }} }}));
                }}
                var step = (mode.plan && mode.plan.shift()) || 'ok';
                if (step === 'fail') {{
                    return Promise.resolve(respond(500, {{ success: false, result: {{
                        code: 'INTERNAL_ERROR', message: 'boom'
                    }} }}));
                }}
                if (step === 'slow') {{
                    return new Promise(function(resolve) {{
                        setTimeout(function() {{ resolve(respond(200, list())); }}, 400);
                    }});
                }}
                if (typeof step === 'number') {{
                    return new Promise(function(resolve) {{
                        setTimeout(function() {{ resolve(respond(200, list())); }}, step);
                    }});
                }}
                return Promise.resolve(respond(200, list()));
            }}
            if (url.match(/\/co-hosts\/suggestions$/)) {{
                window.__coHostSuggestGets += 1;
                if (mode.suggest === 'fail') {{
                    return Promise.resolve(respond(500, {{ success: false, result: {{
                        code: 'INTERNAL_ERROR', message: 'boom'
                    }} }}));
                }}
                return Promise.resolve(respond(200, {{ success: true, result: {{
                    suggestions: window.__coHostSuggestions
                }} }}));
            }}
            return Promise.resolve(respond(200, {{ success: true, result: {{}} }}));
        }};
        "#
    );
    js_sys::eval(&script).expect("failed to mock the co-host API");
}

/// The JSON bodies of the grant requests sent so far.
fn grant_bodies() -> Vec<String> {
    let bodies =
        js_sys::eval("window.__coHostGrants.map(function(b) { return JSON.stringify(b); })")
            .expect("grant log");
    js_sys::Array::from(&bodies)
        .iter()
        .filter_map(|v| v.as_string())
        .collect()
}

fn list_reads() -> u32 {
    js_sys::eval("window.__coHostGets")
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as u32
}

// ---------------------------------------------------------------------------
// Video tile: menu items, gates and labels
// ---------------------------------------------------------------------------

#[allow(non_snake_case)]
fn TileParent() -> Element {
    let s = scenario();
    let client = use_hook(|| {
        let client = VideoCallClient::new_for_test(&s.viewer);
        // A numeric peer is a session with its camera on.
        if let Ok(sid) = s.peer.parse::<u64>() {
            client.insert_peer_for_test(sid, "carol@example.com");
            client.set_peer_media_for_test(sid, true, false);
        }
        client
    });
    use_context_provider(|| client.clone());
    let history_map: PeerSignalHistoryMap = use_signal(HashMap::new);
    use_context_provider(|| history_map);
    let liveness_map: PeerAudioLivenessMap = use_signal(HashMap::new);
    use_context_provider(|| liveness_map);
    let popup_map: SignalPopupStateMap = use_signal(HashMap::new);
    use_context_provider(|| popup_map);
    let appearance = use_signal(AppearanceSettings::default);
    use_context_provider(|| AppearanceSettingsCtx(appearance));
    let meeting_time = use_signal(MeetingTime::default);
    use_context_provider(|| meeting_time);
    let hosts = use_signal(|| s.hosts.iter().cloned().collect::<HashSet<String>>());
    use_context_provider(|| HostSetCtx(hosts));

    let pin: EventHandler<PinnedTile> = use_callback(|_: PinnedTile| {});
    let decode: EventHandler<String> = use_callback(|_: String| {});
    rsx! {
        PeerTile {
            peer_id: s.peer.clone(),
            host_user_id: Some(OWNER.to_string()),
            room_id: Some("m1".to_string()),
            is_current_user_host: s.viewer_is_host,
            on_toggle_pin: pin,
            on_request_decode: decode,
        }
    }
}

async fn mount_tile(s: Scenario) -> web_sys::Element {
    mount_tile_root(s, TileParent).await
}

async fn mount_tile_root(s: Scenario, root: fn() -> Element) -> web_sys::Element {
    // No config: a menu action's API call fails fast instead of reaching out.
    reset_test_browser_state();
    // A failed small-tile test aborts before removing its sheet.
    let stale = gloo_utils::document()
        .query_selector_all("style[data-tile-menu-sheet]")
        .unwrap();
    for i in 0..stale.length() {
        if let Some(sheet) = stale
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
        {
            sheet.remove();
        }
    }
    set_scenario(s);
    let mount = create_mount_point();
    render_into(&mount, root);
    settle().await;
    assert!(
        query(&mount, ".grid-item").is_some(),
        "positive control: the tile must mount"
    );
    mount
}

async fn open_tile(s: Scenario) -> web_sys::Element {
    let mount = mount_tile(s).await;
    click(&mount, ".tile-mute-btn");
    settle().await;
    assert!(
        query(&mount, ".tile-context-menu").is_some(),
        "positive control: the host-actions menu must open"
    );
    mount
}

/// Mount a tile whose peer has mic and camera on, so mute and disable-video
/// are only withheld by the host gate.
async fn mount_tile_with_media(s: Scenario) -> web_sys::Element {
    mount_tile_with_media_in(s, TileParent).await
}

async fn mount_tile_with_media_in(s: Scenario, root: fn() -> Element) -> web_sys::Element {
    let peer = s.peer.clone();
    let mount = mount_tile_root(s, root).await;
    announce_media_on(&peer);
    settle().await;
    mount
}

fn announce_media_on(peer: &str) {
    let _ = global_sender().try_broadcast(DiagEvent {
        subsystem: "peer_status",
        stream_id: None,
        ts_ms: 0,
        metrics: vec![
            metric!("to_peer", peer),
            metric!("audio_enabled", 1u64),
            metric!("video_enabled", 1u64),
        ],
    });
}

async fn tile_menu(mount: &web_sys::Element) -> Vec<String> {
    if query(mount, ".tile-mute-btn").is_none() {
        return Vec::new();
    }
    click(mount, ".tile-mute-btn");
    settle().await;
    menu_items(mount, ".tile-context-menu-item")
}

#[wasm_bindgen_test]
async fn mute_and_disable_video_are_offered_only_on_non_hosts() {
    let mount = mount_tile_with_media(Scenario {
        viewer: OWNER.into(),
        viewer_is_host: true,
        hosts: vec![OWNER.into()],
        peer: "dana@example.com".into(),
        ..Default::default()
    })
    .await;
    assert_eq!(
        tile_menu(&mount).await,
        vec![
            "Mute",
            "Disable video",
            "Make co-host",
            "Transfer host",
            "Remove from meeting"
        ],
        "positive control: media is on, so a non-host gets mute and disable-video"
    );
    cleanup(&mount);

    let mount = mount_tile_with_media(Scenario {
        viewer: OWNER.into(),
        viewer_is_host: true,
        hosts: vec![OWNER.into(), "erin@example.com".into()],
        peer: "erin@example.com".into(),
        ..Default::default()
    })
    .await;
    assert_eq!(
        tile_menu(&mount).await,
        vec!["Remove co-host", "Remove from meeting"],
        "a co-host's client ignores host mute and disable-video"
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_host_shown_as_host_offers_remove_host_role() {
    let mount = mount_tile(Scenario {
        viewer: OWNER.into(),
        viewer_is_host: false,
        hosts: vec![ALICE.into()],
        peer: ALICE.into(),
        ..Default::default()
    })
    .await;
    assert_eq!(
        text_of(&mount, ".floating-name .host-indicator").as_deref(),
        Some("(Host)")
    );
    assert_eq!(tile_menu(&mount).await, vec!["Remove host role"]);
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_co_host_has_no_menu_on_the_owner() {
    let mount = mount_tile_with_media(Scenario {
        viewer: COHOST.into(),
        viewer_is_host: true,
        hosts: vec![OWNER.into(), COHOST.into()],
        peer: OWNER.into(),
        ..Default::default()
    })
    .await;
    assert!(
        query(&mount, ".tile-mute-btn").is_none(),
        "no trigger for an empty menu"
    );
    assert!(query(&mount, "[aria-expanded]").is_none());
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_owner_can_make_a_participant_a_co_host_from_the_tile() {
    let mount = open_tile(Scenario {
        viewer: OWNER.into(),
        viewer_is_host: true,
        hosts: vec![OWNER.into()],
        peer: "carol@example.com".into(),
        ..Default::default()
    })
    .await;
    assert_eq!(
        menu_items(&mount, ".tile-context-menu-item"),
        vec!["Make co-host", "Transfer host", "Remove from meeting"],
        "the destructive item is last"
    );
    let trigger = query(&mount, ".tile-mute-btn").unwrap();
    assert_eq!(
        trigger.get_attribute("aria-expanded").as_deref(),
        Some("true")
    );
    assert!(!trigger.has_attribute("aria-haspopup"));
    assert!(query(&mount, ".floating-name .host-indicator").is_none());

    click(&mount, "[data-testid='tile-co-host-action']");
    settle().await;
    assert!(
        is_focused(&trigger),
        "focus must return to the menu trigger when the chosen item unmounts"
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_owner_sees_co_host_label_and_no_transfer_on_a_host() {
    let mount = open_tile(Scenario {
        viewer: OWNER.into(),
        viewer_is_host: true,
        hosts: vec![OWNER.into(), ALICE.into()],
        peer: ALICE.into(),
        ..Default::default()
    })
    .await;
    assert_eq!(
        menu_items(&mount, ".tile-context-menu-item"),
        vec!["Remove co-host", "Remove from meeting"]
    );
    assert_eq!(
        text_of(&mount, ".floating-name .host-indicator").as_deref(),
        Some("(Co-host)")
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn every_role_holder_reads_host_while_the_owner_lacks_the_role() {
    let mount = mount_tile(Scenario {
        viewer: COHOST.into(),
        viewer_is_host: true,
        hosts: vec![COHOST.into(), ALICE.into()],
        peer: ALICE.into(),
        ..Default::default()
    })
    .await;
    assert_eq!(
        text_of(&mount, ".floating-name .host-indicator").as_deref(),
        Some("(Host)")
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_co_host_can_kick_a_participant_but_not_manage_co_hosts() {
    let mount = open_tile(Scenario {
        viewer: COHOST.into(),
        viewer_is_host: true,
        hosts: vec![OWNER.into(), COHOST.into()],
        peer: ALICE.into(),
        ..Default::default()
    })
    .await;
    assert_eq!(
        menu_items(&mount, ".tile-context-menu-item"),
        vec!["Transfer host", "Remove from meeting"]
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_co_host_gets_no_kick_on_the_owner_or_another_host() {
    let cases: [(&str, Vec<&str>, Vec<&str>); 4] = [
        (OWNER, vec![OWNER, COHOST], vec![]),
        (ALICE, vec![OWNER, COHOST, ALICE], vec![]),
        (OWNER, vec![COHOST], vec!["Transfer host"]),
        (
            BOB,
            vec![OWNER, COHOST],
            vec!["Transfer host", "Remove from meeting"],
        ),
    ];
    for (peer, hosts, want) in cases {
        let mount = mount_tile(Scenario {
            viewer: COHOST.into(),
            viewer_is_host: true,
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            peer: peer.into(),
            ..Default::default()
        })
        .await;
        let items = tile_menu(&mount).await;
        assert_eq!(
            query(&mount, ".tile-context-menu").is_some(),
            !want.is_empty(),
            "the menu opens exactly when it has items ({peer}, hosts {hosts:?})"
        );
        assert_eq!(items, want, "{peer} with hosts {hosts:?}");
        cleanup(&mount);
    }
}

thread_local! {
    static TILE_BOX: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
    static TILE_ALIVE: std::cell::Cell<Option<Signal<bool>>> = const { std::cell::Cell::new(None) };
    static FOOTER: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
}

/// A `tabindex="-1"` stage like `#grid-container` holding the tile placed by
/// `TILE_BOX`, a tile to the left of the default spot and a later tile under
/// it whose icon row covers its whole area; then a meeting footer styled by
/// `FOOTER` unless it is empty.
#[allow(non_snake_case)]
fn SmallTileGrid() -> Element {
    let tile_box = TILE_BOX.with(|b| b.get());
    let footer = FOOTER.with(|f| f.get());
    let alive = use_signal(|| true);
    use_hook(|| TILE_ALIVE.with(|a| a.set(Some(alive))));
    rsx! {
        div { tabindex: "-1", style: "position: fixed; inset: 0; z-index: 0;",
            div {
                class: "grid-item",
                style: "position: absolute; left: 40px; top: 60px; width: 150px; height: 100px;",
            }
            div { style: "position: absolute; display: flex; width: 150px; height: 100px; {tile_box}",
                if alive() {
                    TileParent {}
                }
            }
            div {
                class: "grid-item",
                style: "position: absolute; left: 40px; top: 170px; width: 310px; height: 100px;",
                div { class: "canvas-container",
                    div { class: "tile-top-icons", style: "inset: 0;" }
                }
            }
        }
        if !footer.is_empty() {
            footer { class: "meeting-footer", style: "{footer}" }
        }
    }
}

/// Opens the host menu of a 150x100 tile under the shipped stylesheets.
async fn open_small_tile(
    tile_box: &'static str,
    peer: &str,
    footer: &'static str,
) -> (web_sys::Element, web_sys::Element) {
    TILE_BOX.with(|b| b.set(tile_box));
    FOOTER.with(|f| f.set(footer));
    let mount = mount_tile_root(
        Scenario {
            peer: peer.into(),
            ..tile_scenario()
        },
        SmallTileGrid,
    )
    .await;
    let doc = gloo_utils::document();
    let sheet = doc.create_element("style").unwrap();
    sheet.set_attribute("data-tile-menu-sheet", "").unwrap();
    sheet.set_text_content(Some(&format!("{STYLE_CSS}{GLOBAL_CSS}")));
    doc.head().unwrap().append_child(&sheet).unwrap();
    click(&mount, ".tile-mute-btn");
    settle().await;
    (mount, sheet)
}

/// Asserts the menu lies in the viewport and is the topmost hit at three edges.
fn assert_menu_fully_shown(mount: &web_sys::Element) -> web_sys::DomRect {
    let doc = gloo_utils::document();
    let menu = query(mount, ".tile-context-menu").expect("the host-actions menu must open");
    let r = menu.get_bounding_client_rect();
    let win = gloo_utils::window();
    let vw = win.inner_width().unwrap().as_f64().unwrap();
    let vh = win.inner_height().unwrap().as_f64().unwrap();
    let edges = (r.left(), r.top(), r.right(), r.bottom());
    assert!(
        r.left() >= 0.0 && r.top() >= 0.0 && r.right() <= vw && r.bottom() <= vh,
        "menu {edges:?} must lie inside the {vw}x{vh} viewport"
    );
    let (mid_x, mid_y) = (r.left() + r.width() / 2.0, r.top() + r.height() / 2.0);
    let covered: Vec<&str> = [
        ("left edge", r.left() + 3.0, mid_y),
        ("right edge", r.right() - 3.0, mid_y),
        ("bottom edge", mid_x, r.bottom() - 3.0),
    ]
    .into_iter()
    .filter(|&(_, x, y)| !lands_in(&doc, &menu, x, y))
    .map(|(edge, ..)| edge)
    .collect();
    assert!(
        covered.is_empty(),
        "menu {edges:?} is clipped or covered at its {covered:?}"
    );
    r
}

#[wasm_bindgen_test]
async fn a_small_tiles_host_menu_paints_over_its_neighbours_unclipped() {
    let (mount, sheet) = open_small_tile("left: 200px; top: 60px;", "carol@example.com", "").await;
    let tile = query(&mount, "[data-tile-root]").unwrap();
    assert_eq!(
        computed(
            &tile.query_selector(".canvas-container").unwrap().unwrap(),
            "overflow"
        ),
        "hidden",
        "positive control: the shipped stylesheet clips the tile"
    );
    let menu = assert_menu_fully_shown(&mount);
    let tile = tile.get_bounding_client_rect();
    assert!(
        menu.left() < tile.left() && menu.bottom() > tile.bottom(),
        "positive control: the menu must reach past the tile's left and bottom edges"
    );
    let trigger = query(&mount, ".tile-mute-btn")
        .unwrap()
        .get_bounding_client_rect();
    assert!(
        (menu.right() - trigger.right()).abs() < 1.0 && menu.top() > trigger.bottom(),
        "with room below, the menu hangs under the trigger, right edges aligned"
    );

    js_sys::eval(
        "window.__tileMenuErrors = []; \
         window.addEventListener('error', e => window.__tileMenuErrors.push(String(e.message)));",
    )
    .unwrap();
    if let Some(mut alive) = TILE_ALIVE.with(|a| a.take()) {
        alive.set(false);
    }
    settle().await;
    assert!(
        query(&mount, ".tile-context-menu").is_none(),
        "positive control: the tile unmounted"
    );
    let errors = js_sys::eval("window.__tileMenuErrors.join('\\n')").unwrap();
    assert_eq!(
        errors.as_string().as_deref(),
        Some(""),
        "unmounting the tile with its menu open must not throw"
    );
    cleanup(&mount);
    sheet.remove();
}

#[wasm_bindgen_test]
async fn a_host_menu_in_the_bottom_left_corner_flips_above_and_stays_on_screen() {
    let (mount, sheet) = open_small_tile("left: 0; bottom: 0;", "dave@example.com", "").await;
    let menu = assert_menu_fully_shown(&mount);
    let trigger = query(&mount, ".tile-mute-btn")
        .unwrap()
        .get_bounding_client_rect();
    assert!(
        menu.bottom() < trigger.top(),
        "with no room below, the menu opens above its trigger"
    );
    cleanup(&mount);
    sheet.remove();
}

#[wasm_bindgen_test]
async fn a_host_menu_stays_clear_of_the_meeting_footer() {
    let (mount, sheet) = open_small_tile(
        "left: 200px; bottom: 110px;",
        "frank@example.com",
        "--meeting-footer-h: 72px;",
    )
    .await;
    let footer = query(&mount, ".meeting-footer")
        .unwrap()
        .get_bounding_client_rect();
    assert!(footer.height() > 0.0, "positive control: the footer shows");
    let menu = assert_menu_fully_shown(&mount);
    assert!(
        menu.bottom() <= footer.top(),
        "menu bottom {} must end above the footer top {}",
        menu.bottom(),
        footer.top()
    );
    cleanup(&mount);
    sheet.remove();
}

#[wasm_bindgen_test]
async fn a_host_menu_with_no_room_on_either_side_never_covers_its_trigger() {
    let (mount, sheet) = open_small_tile(
        "left: 200px; top: 72px;",
        "gina@example.com",
        "--meeting-footer-h: calc(100vh - 175px);",
    )
    .await;
    let menu = assert_menu_fully_shown(&mount);
    let trigger = query(&mount, ".tile-mute-btn").unwrap();
    let t = trigger.get_bounding_client_rect();
    assert!(
        menu.bottom() <= t.top() || menu.top() >= t.bottom(),
        "menu {:?} must not cover trigger {:?}",
        (menu.top(), menu.bottom()),
        (t.top(), t.bottom())
    );
    assert!(
        lands_in(
            &gloo_utils::document(),
            &trigger,
            t.left() + t.width() / 2.0,
            t.top() + t.height() / 2.0
        ),
        "a second click on the trigger must reach the trigger"
    );
    cleanup(&mount);
    sheet.remove();
}

fn tile_scenario() -> Scenario {
    Scenario {
        viewer: OWNER.into(),
        viewer_is_host: true,
        hosts: vec![OWNER.into()],
        peer: "carol@example.com".into(),
        ..Default::default()
    }
}

#[wasm_bindgen_test]
async fn escape_closes_the_tile_menu_and_returns_focus_to_its_trigger() {
    let mount = open_tile(tile_scenario()).await;
    let trigger = query(&mount, ".tile-mute-btn").unwrap();
    let menu_id = query(&mount, ".tile-context-menu").unwrap().id();
    assert_eq!(trigger.get_attribute("aria-controls"), Some(menu_id));
    let item = query(&mount, ".tile-context-menu-item").unwrap();
    html(&item).focus().unwrap();
    assert!(
        is_focused(&item),
        "positive control: a menu item holds focus"
    );

    press_escape(&item);
    settle().await;
    assert!(
        query(&mount, ".tile-context-menu").is_none(),
        "Escape closes the menu"
    );
    assert!(is_focused(&trigger), "focus returns to the menu trigger");
    assert_eq!(trigger.get_attribute("aria-controls"), None);
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_mousedown_on_the_menus_own_padding_keeps_it_open() {
    let (mount, sheet) = open_small_tile("left: 200px; top: 60px;", "kim@example.com", "").await;
    html(&query(&mount, ".tile-context-menu-item").unwrap())
        .focus()
        .unwrap();
    let menu = query(&mount, ".tile-context-menu").unwrap();
    let focus_target = menu
        .closest("[tabindex]")
        .unwrap()
        .expect("the menu has a focusable ancestor");
    html(&focus_target).focus().unwrap();
    settle().await;
    assert!(
        query(&mount, ".tile-context-menu").is_some(),
        "focus landing on the menu's nearest focusable ancestor ({:?}) must keep it open",
        focus_target.class_name()
    );
    cleanup(&mount);
    sheet.remove();
}

#[wasm_bindgen_test]
async fn focus_leaving_the_open_menu_closes_it_but_a_plain_blur_does_not() {
    let mount = open_tile(tile_scenario()).await;
    let item = html(&query(&mount, ".tile-context-menu-item").unwrap());
    item.focus().unwrap();
    item.blur().unwrap();
    settle().await;
    assert!(
        query(&mount, ".tile-context-menu").is_some(),
        "a blur to nowhere keeps the menu"
    );

    item.focus().unwrap();
    html(&query(&mount, ".pin-icon").unwrap()).focus().unwrap();
    settle().await;
    assert!(
        query(&mount, ".tile-context-menu").is_none(),
        "focus moving to another control closes the menu"
    );
    cleanup(&mount);
}

#[allow(non_snake_case)]
fn MeteredTileParent() -> Element {
    let overlay_on = use_signal(|| true);
    use_context_provider(|| MediaMetricsOverlayCtx(overlay_on));
    rsx! {
        TileParent {}
    }
}

async fn assert_clicking_closes_the_menu(peer: &str, root: fn() -> Element, control: &str) {
    let scenario = Scenario {
        peer: peer.into(),
        ..tile_scenario()
    };
    let mount = mount_tile_with_media_in(scenario, root).await;
    click(&mount, ".tile-mute-btn");
    settle().await;
    assert!(
        query(&mount, ".tile-context-menu").is_some(),
        "positive control: the menu opens"
    );
    click(&mount, control);
    settle().await;
    assert!(
        query(&mount, ".tile-context-menu").is_none(),
        "clicking {control} must close the menu"
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_tiles_pin_button_closes_its_open_menu() {
    assert_clicking_closes_the_menu("hank@example.com", TileParent, ".pin-icon").await;
}

#[wasm_bindgen_test]
async fn the_tiles_crop_button_closes_its_open_menu() {
    assert_clicking_closes_the_menu("4242", TileParent, ".crop-icon").await;
}

#[wasm_bindgen_test]
async fn the_tiles_signal_button_closes_its_open_menu() {
    assert_clicking_closes_the_menu("ivy@example.com", MeteredTileParent, ".signal-indicator")
        .await;
}

#[derive(Clone, Copy)]
enum Reply {
    Status(u16),
    Held(u16),
    NotAdmitted,
    Waiting(usize),
}

const RETRY: &[Reply] = &[Reply::Status(401), Reply::Status(200)];
const HELD_403: &[Reply] = &[Reply::Held(403)];
const TOKEN_URL: &str = "https://id.test.invalid/oauth2/v1/token";
const STALE_TOKEN: &str = "stale-access-token";
const ALERT: &str = "[data-testid='co-host-notice'][role='alert']";
const DANA: &str = "dana@example.com";
const ONLY_HOSTS: &str = "Only hosts can do that.";

impl Reply {
    fn to_json(self) -> serde_json::Value {
        match self {
            Reply::Status(status) => serde_json::json!({ "status": status }),
            Reply::Held(status) => serde_json::json!({ "status": status, "held": true }),
            Reply::NotAdmitted => serde_json::json!({
                "status": 400,
                "code": "BAD_REQUEST",
                "message": "target is not an admitted participant",
            }),
            Reply::Waiting(count) => {
                let rows: Vec<serde_json::Value> = ["dana", "erin", "fay", "gus", "hal", "ivy"]
                    [..count]
                    .iter()
                    .map(|id| {
                        serde_json::json!({ "user_id": id, "display_name": id, "status": "waiting",
                                             "is_host": false, "joined_at": 0 })
                    })
                    .collect();
                serde_json::json!({
                    "status": 200,
                    "result": { "meeting_id": "m1", "waiting": rows },
                })
            }
        }
    }
}

/// `plan` maps a request path to the replies its successive requests get; an
/// exhausted or absent plan answers 200, and a held reply waits for
/// `release_held_reply()`. Each token refresh mints `refreshed-<n>`.
fn install_host_api(plan: &[(String, &[Reply])]) {
    let plan: serde_json::Map<String, serde_json::Value> = plan
        .iter()
        .map(|(path, replies)| {
            let replies = replies.iter().map(|r| r.to_json()).collect();
            (path.clone(), serde_json::Value::Array(replies))
        })
        .collect();
    let install = js_sys::eval(include_str!("support/host_api_stub.js")).expect("the stub");
    let install: js_sys::Function = install.unchecked_into();
    let plan = js_sys::JSON::parse(&serde_json::Value::Object(plan).to_string()).unwrap();
    install
        .call2(&wasm_bindgen::JsValue::NULL, &plan, &TOKEN_URL.into())
        .expect("install the host-api stub");
}

fn path(endpoint: &str) -> String {
    format!("/api/v1/meetings/m1/{endpoint}")
}

fn inject_pkce() {
    inject_app_config_pkce(TOKEN_URL);
    dioxus_ui::auth::store_access_token(STALE_TOKEN);
    dioxus_ui::auth::store_refresh_token("refresh-token-fixture");
}

fn release_held_reply() {
    js_sys::eval("window.__hostApi.release()").expect("a held reply to release");
}

fn strings(value: wasm_bindgen::JsValue) -> Vec<String> {
    js_sys::Array::from(&value)
        .iter()
        .map(|v| v.as_string().unwrap_or_default())
        .collect()
}

/// The `Authorization` header of every POST to `path`, in order.
fn posts_to(path: &str) -> Vec<String> {
    let posts = js_sys::eval("window.__hostApi.posts").unwrap();
    js_sys::Array::from(&posts)
        .iter()
        .map(strings)
        .filter(|pair| pair.first().map(String::as_str) == Some(path))
        .map(|pair| pair.get(1).cloned().unwrap_or_default())
        .collect()
}

fn page_errors() -> Vec<String> {
    strings(js_sys::eval("window.__hostApi.errors").unwrap())
}

fn refreshes() -> usize {
    js_sys::eval("window.__hostApi.refreshes")
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as usize
}

fn bearer(refresh: usize) -> String {
    match refresh {
        0 => format!("Bearer {STALE_TOKEN}"),
        n => format!("Bearer refreshed-{n}"),
    }
}

async fn poll_until(mut done: impl FnMut() -> bool) {
    for _ in 0..150 {
        if done() {
            return;
        }
        sleep_ms(20).await;
    }
}

async fn alert_once_shown(root: &web_sys::Element, want: &str) -> Option<String> {
    poll_until(|| text_of(root, ALERT).as_deref() == Some(want)).await;
    text_of(root, ALERT)
}

async fn click_menu_item(root: &web_sys::Element, trigger: &str, items: &str, label: &str) {
    click(root, trigger);
    yield_now().await;
    let nodes = root.query_selector_all(items).unwrap();
    (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .find(|n| n.text_content().unwrap_or_default().trim() == label)
        .unwrap_or_else(|| panic!("the menu must offer {label}"))
        .unchecked_into::<web_sys::HtmlElement>()
        .click();
    yield_now().await;
}

#[allow(non_snake_case)]
fn TileNoticeParent() -> Element {
    let notice = use_signal(|| None::<CoHostNotice>);
    use_context_provider(|| CoHostNoticeCtx(notice));
    let mut tile_mounted = use_signal(|| true);
    rsx! {
        if tile_mounted() {
            TileParent {}
        }
        button { "data-testid": "unmount-tile", onclick: move |_| tile_mounted.set(false) }
        CoHostNoticeLayer { notice }
    }
}

fn roster_contexts() {
    let client = use_hook(|| VideoCallClient::new_for_test(OWNER));
    use_context_provider(|| client.clone());
    use_context_provider(|| Rc::new(RosterLiveness::default()));
    let appearance = use_signal(AppearanceSettings::default);
    use_context_provider(|| AppearanceSettingsCtx(appearance));
    let hosts = use_signal(|| HashSet::from([OWNER.to_string()]));
    use_context_provider(|| HostSetCtx(hosts));
}

fn dana() -> Vec<PeerListEntry> {
    vec![PeerListEntry {
        session_id: DANA.to_string(),
        user_id: DANA.to_string(),
    }]
}

#[allow(non_snake_case)]
fn RosterNoticeParent() -> Element {
    roster_contexts();
    let notice = use_signal(|| None::<CoHostNotice>);
    use_context_provider(|| CoHostNoticeCtx(notice));
    let mut drawer_open = use_signal(|| true);
    rsx! {
        if drawer_open() {
            PeerList {
                peers: dana(),
                onclose: move |_| drawer_open.set(false),
                room_id: "m1".to_string(),
                host_user_id: Some(OWNER.to_string()),
            }
        }
        CoHostNoticeLayer { notice }
    }
}

struct Action {
    label: &'static str,
    endpoint: &'static str,
    failure: &'static str,
    refusal: Reply,
    reason: &'static str,
}

const ACTIONS: [Action; 6] = [
    Action {
        label: "Mute all",
        endpoint: "mute-all",
        failure: "Couldn't mute everyone.",
        refusal: Reply::Status(403),
        reason: ONLY_HOSTS,
    },
    Action {
        label: "Disable video for all",
        endpoint: "disable-video-all",
        failure: "Couldn't disable video for everyone.",
        refusal: Reply::Status(500),
        reason: "Something went wrong. Try again.",
    },
    Action {
        label: "Mute",
        endpoint: "mute",
        failure: "Couldn't mute dana@example.com.",
        refusal: Reply::Status(403),
        reason: ONLY_HOSTS,
    },
    Action {
        label: "Disable video",
        endpoint: "disable-video",
        failure: "Couldn't disable video for dana@example.com.",
        refusal: Reply::Status(401),
        reason: "Your session has expired. Sign in again.",
    },
    Action {
        label: "Transfer host",
        endpoint: "transfer-host",
        failure: "Couldn't transfer host to dana@example.com.",
        refusal: Reply::NotAdmitted,
        reason: "They may have left, or you're no longer a host.",
    },
    Action {
        label: "Remove from meeting",
        endpoint: "kick",
        failure: "Couldn't remove dana@example.com from the meeting.",
        refusal: Reply::Status(403),
        reason: ONLY_HOSTS,
    },
];

fn meeting_wide(label: &str) -> bool {
    label.ends_with(" all")
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Surface {
    Roster,
    Tile,
}

impl Surface {
    fn actions(self) -> Vec<&'static Action> {
        let offered = |a: &&Action| self == Surface::Roster || !meeting_wide(a.label);
        ACTIONS.iter().filter(offered).collect()
    }

    async fn mount(self) -> web_sys::Element {
        match self {
            Surface::Roster => {
                reset_test_browser_state();
                let mount = create_mount_point();
                render_into(&mount, RosterNoticeParent);
                settle().await;
                announce_media_on(DANA);
                settle().await;
                mount
            }
            Surface::Tile => {
                let dana = Scenario {
                    viewer: OWNER.into(),
                    viewer_is_host: true,
                    hosts: vec![OWNER.into()],
                    peer: DANA.into(),
                    ..Default::default()
                };
                mount_tile_with_media_in(dana, TileNoticeParent).await
            }
        }
    }

    async fn act(self, mount: &web_sys::Element, label: &str) {
        let (trigger, items) = match self {
            Surface::Tile => (".tile-mute-btn", ".tile-context-menu-item"),
            Surface::Roster if meeting_wide(label) => (
                "button[aria-label='Host actions']",
                ".in-call-menu-wrapper .context-menu-item",
            ),
            Surface::Roster => (
                "[data-testid='peer-item-menu-button']",
                ".peer_item_context_menu .context-menu-item",
            ),
        };
        click_menu_item(mount, trigger, items, label).await;
    }

    async fn close(self, mount: &web_sys::Element) {
        let (button, surface) = match self {
            Surface::Roster => (".sidebar-header .close-button", ".sidebar-header"),
            Surface::Tile => ("[data-testid='unmount-tile']", ".grid-item"),
        };
        click(mount, button);
        settle().await;
        assert!(query(mount, surface).is_none(), "{self:?} closed");
    }
}

#[wasm_bindgen_test]
async fn every_host_action_retries_once_after_a_pkce_refresh() {
    let (mut seen, mut want) = (Vec::new(), Vec::new());
    for surface in [Surface::Roster, Surface::Tile] {
        let mount = surface.mount().await;
        inject_pkce();
        let actions = surface.actions();
        install_host_api(
            &actions
                .iter()
                .map(|a| (path(a.endpoint), RETRY))
                .collect::<Vec<_>>(),
        );
        for a in &actions {
            surface.act(&mount, a.label).await;
            poll_until(|| posts_to(&path(a.endpoint)).len() >= 2).await;
        }
        sleep_ms(200).await;
        let posts: Vec<_> = actions
            .iter()
            .map(|a| (a.label, posts_to(&path(a.endpoint))))
            .collect();
        seen.push((surface, posts, refreshes(), text_of(&mount, ALERT)));
        let bearers = actions.iter().enumerate();
        let bearers = bearers.map(|(n, a)| (a.label, vec![bearer(n), bearer(n + 1)]));
        want.push((surface, bearers.collect(), actions.len(), None));
        cleanup(&mount);
    }
    reset_test_browser_state();
    assert_eq!(seen, want);
}

#[wasm_bindgen_test]
async fn host_action_refusals_name_the_action_and_the_reason() {
    let (mut seen, mut want) = (Vec::new(), Vec::new());
    for surface in [Surface::Roster, Surface::Tile] {
        let mount = surface.mount().await;
        inject_app_config();
        let actions = surface.actions();
        let plan = actions
            .iter()
            .map(|a| (path(a.endpoint), std::slice::from_ref(&a.refusal)));
        install_host_api(&plan.collect::<Vec<_>>());
        for a in &actions {
            surface.act(&mount, a.label).await;
            let text = format!("{} {}", a.failure, a.reason);
            seen.push((surface, a.label, alert_once_shown(&mount, &text).await));
            want.push((surface, a.label, Some(text)));
        }
        cleanup(&mount);
    }
    reset_test_browser_state();
    assert_eq!(seen, want);
}

#[wasm_bindgen_test]
async fn every_host_action_outlives_its_surface() {
    let (mut seen, mut want) = (Vec::new(), Vec::new());
    for surface in [Surface::Roster, Surface::Tile] {
        for a in surface.actions() {
            let mount = surface.mount().await;
            inject_app_config();
            install_host_api(&[(path(a.endpoint), HELD_403)]);
            surface.act(&mount, a.label).await;
            poll_until(|| posts_to(&path(a.endpoint)).len() == 1).await;
            surface.close(&mount).await;
            release_held_reply();
            let text = format!("{} {ONLY_HOSTS}", a.failure);
            let shown = alert_once_shown(&mount, &text).await;
            sleep_ms(200).await;
            seen.push((surface, a.label, shown, page_errors()));
            want.push((surface, a.label, Some(text), Vec::<String>::new()));
            cleanup(&mount);
        }
    }
    reset_test_browser_state();
    assert_eq!(seen, want);
}

#[wasm_bindgen_test]
async fn a_pkce_retry_still_goes_out_after_the_drawer_closes() {
    let mount = Surface::Roster.mount().await;
    inject_pkce();
    let mute_all = path("mute-all");
    install_host_api(&[(mute_all.clone(), &[Reply::Held(401), Reply::Status(200)])]);
    Surface::Roster.act(&mount, "Mute all").await;
    poll_until(|| posts_to(&mute_all).len() == 1).await;
    Surface::Roster.close(&mount).await;
    release_held_reply();
    poll_until(|| posts_to(&mute_all).len() >= 2).await;
    sleep_ms(200).await;
    assert_eq!(
        (posts_to(&mute_all), page_errors(), text_of(&mount, ALERT)),
        (vec![bearer(0), bearer(1)], Vec::new(), None)
    );
    cleanup(&mount);
    reset_test_browser_state();
}

const LEPTOS_CSS: &str = include_str!("../static/leptos-style.css");
const TAILWIND_CSS: &str = include_str!("../static/tailwind.css");
const STYLE_CSS: &str = include_str!("../static/style.css");
const GLOBAL_CSS: &str = include_str!("../static/global.css");
const FAILURE: &str = "Couldn't mute everyone. Only hosts can do that.";
const NETWORK_FAILURE: &str =
    "Couldn't disable video for everyone. Network error. Check your connection and try again.";
const SUCCESS: &str = "Alice is now a co-host and saved for future meetings.";
const LINUX_METRICS: &str = "* { font-family: Verdana, 'DejaVu Sans', sans-serif !important } \
                             .peer-toasts .toast-text { line-height: 1.5 }";
const LONG_FAILURE: &str =
    "Couldn't remove maximiliana.wolfeschlegelsteinhausen@example-university.edu \
                            from the meeting. They're no longer in the meeting.";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drawer {
    Closed,
    Roster,
    Diagnostics,
}

#[derive(Clone, Copy)]
struct Layout {
    drawer: Drawer,
    waiting: usize,
    notice: &'static str,
    is_error: bool,
    captions: bool,
}

const PHONE_ROSTER: Layout = Layout {
    drawer: Drawer::Roster,
    waiting: 0,
    notice: FAILURE,
    is_error: true,
    captions: false,
};

const CAPTION: &str = "so the plan for next week is to finish the rollout and then check in with \
                       everyone on Thursday";

thread_local! {
    static LAYOUT: std::cell::Cell<Layout> = const { std::cell::Cell::new(PHONE_ROSTER) };
    static ALIVE: std::cell::Cell<Option<Signal<bool>>> = const { std::cell::Cell::new(None) };
}

#[allow(non_snake_case)]
fn MeetingHarness() -> Element {
    roster_contexts();
    let layout = LAYOUT.with(|l| l.get());
    let notice = use_signal(|| {
        Some(CoHostNotice {
            seq: 1,
            text: layout.notice.to_string(),
            is_error: layout.is_error,
        })
    });
    use_context_provider(|| CoHostNoticeCtx(notice));
    let host_mute = use_signal(|| Some(1u32));
    let version = use_signal(|| 0u64);
    let alive = use_signal(|| true);
    use_hook(|| ALIVE.with(|a| a.set(Some(alive))));
    if !alive() {
        return rsx! {};
    }
    let open = |drawer: Drawer| {
        if layout.drawer == drawer {
            "visible"
        } else {
            ""
        }
    };
    rsx! {
        if layout.captions {
            div { class: "captions-overlay", "data-testid": "captions-overlay",
                for line in 0..MAX_CAPTION_LINES {
                    div { key: "{line}", class: "caption-line caption-line--final",
                        div { class: "caption-line__body",
                            bdi { class: "caption-line__speaker", "Bob" }
                            bdi { class: "caption-line__text", "{CAPTION}" }
                        }
                    }
                }
            }
        }
        div { class: "peer-toasts",
            HostActionNotice { kind: HostActionKind::Mic, notice: host_mute, device_on: false }
            div {
                class: "peer-toast recording-status-banner",
                "data-testid": "recording-banner",
                role: "status",
                span { class: "rec-dot" }
                span { class: "toast-text",
                    span { class: "toast-name", "Recording" }
                }
            }
            CoHostNoticeLayer { notice }
            div { class: "peer-toast toast-joined", "data-testid": "join-toast",
                span { class: "toast-text",
                    span { class: "toast-name", "Bob" }
                    span { class: "toast-action", "joined the meeting" }
                }
            }
            div { class: "peer-toast self-view-hidden-toast", "data-testid": "self-view-toast",
                span { class: "toast-text",
                    span { class: "toast-name", "Self view hidden." }
                }
                button { class: "toast-undo-btn", "Undo" }
            }
        }
        div { id: "grid-container",
            div { class: "video-controls-container dock-bottom",
                for slot in 0..6 {
                    button { key: "{slot}", class: "video-control-button" }
                }
            }
        }
        div { id: "peer-list-container", class: open(Drawer::Roster),
            PeerList {
                peers: dana(),
                onclose: move |_| {},
                room_id: "m1".to_string(),
                host_user_id: Some(OWNER.to_string()),
            }
        }
        if layout.waiting > 0 {
            HostControls {
                meeting_id: "m1".to_string(),
                is_admitted: true,
                waiting_room_version: version,
            }
        }
        div { id: "diagnostics-sidebar", class: open(Drawer::Diagnostics) }
    }
}

/// The harness's real DOM, copied into a frame under the shipped stylesheets.
async fn meeting_frame(layout: Layout) -> (web_sys::Element, web_sys::Document) {
    reset_test_browser_state();
    inject_app_config();
    install_host_api(&[(path("waiting"), &[Reply::Waiting(layout.waiting); 3])]);
    LAYOUT.with(|l| l.set(layout));
    let source = create_mount_point();
    render_into(&source, MeetingHarness);
    settle().await;
    let rows = || source.query_selector_all(".waiting-participant").unwrap();
    poll_until(|| rows().length() as usize == layout.waiting).await;
    assert_eq!(rows().length() as usize, layout.waiting, "waiting rows");
    let markup = source.inner_html();
    if let Some(mut alive) = ALIVE.with(|a| a.take()) {
        alive.set(false);
    }
    yield_now().await;
    source.remove();
    reset_test_browser_state();

    let doc = gloo_utils::document();
    let frame = doc.create_element("iframe").unwrap();
    doc.body().unwrap().append_child(&frame).unwrap();
    let inner: web_sys::Document = js_sys::Reflect::get(&frame, &"contentDocument".into())
        .unwrap()
        .unchecked_into();
    for css in [LEPTOS_CSS, TAILWIND_CSS, STYLE_CSS, GLOBAL_CSS] {
        let style = inner.create_element("style").unwrap();
        style.set_text_content(Some(css));
        inner.head().unwrap().append_child(&style).unwrap();
    }
    inner.body().unwrap().set_inner_html(&format!(
        "<div id='reserves'><div id='main-container' class='meeting-page'>{markup}</div></div>"
    ));
    (frame, inner)
}

async fn resize(
    frame: &web_sys::Element,
    doc: &web_sys::Document,
    (width, height): (f64, f64),
    reserve: f64,
) {
    let size = format!("width: {width}px; height: {height}px; border: 0");
    frame.set_attribute("style", &size).unwrap();
    let reserves =
        format!("display: flex; --drawer-left-reserve: {reserve}px; --drawer-right-reserve: 0px");
    el(doc, "#reserves")
        .set_attribute("style", &reserves)
        .unwrap();
    sleep_ms(600).await;
    if let Some(panel) = doc.query_selector(".host-controls-container").unwrap() {
        publish_panel_bottom(&panel, &el(doc, ".peer-toasts").unchecked_into());
        sleep_ms(50).await;
    }
}

fn el(doc: &web_sys::Document, selector: &str) -> web_sys::Element {
    doc.query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} must render"))
}

fn lands_in(doc: &web_sys::Document, target: &web_sys::Element, x: f64, y: f64) -> bool {
    doc.element_from_point(x as f32, y as f32)
        .is_some_and(|hit| target.contains(Some(hit.as_ref())))
}

fn hits_at_center(doc: &web_sys::Document, selector: &str) -> bool {
    let target = el(doc, selector);
    let r = target.get_bounding_client_rect();
    lands_in(
        doc,
        &target,
        r.left() + r.width() / 2.0,
        r.top() + r.height() / 2.0,
    )
}

fn overlaps(a: &web_sys::DomRect, b: &web_sys::DomRect) -> bool {
    a.left() < b.right() && b.left() < a.right() && a.top() < b.bottom() && b.top() < a.bottom()
}

fn notice_text_readable(doc: &web_sys::Document) -> bool {
    let notice = el(doc, ALERT);
    let text = notice.query_selector(".toast-text").unwrap().unwrap();
    let r = text.get_bounding_client_rect();
    notice
        .set_attribute("style", "pointer-events: auto")
        .unwrap();
    let probes = [(0.02, 0.5), (0.5, 0.5), (0.98, 0.5), (0.5, 0.1), (0.5, 0.9)];
    let hit = probes.iter().all(|(fx, fy)| {
        lands_in(
            doc,
            &notice,
            r.left() + r.width() * fx,
            r.top() + r.height() * fy,
        )
    });
    notice.remove_attribute("style").unwrap();
    let view = doc.default_view().unwrap();
    let width = view.inner_width().unwrap().as_f64().unwrap();
    let height = view.inner_height().unwrap().as_f64().unwrap();
    let inside = r.left() >= 0.0 && r.top() >= 0.0 && r.right() <= width && r.bottom() <= height;
    hit && inside && text.scroll_width() <= text.client_width()
}

fn computed_px(doc: &web_sys::Document, selector: &str, prop: &str) -> f64 {
    let value = doc
        .default_view()
        .unwrap()
        .get_computed_style(&el(doc, selector))
        .unwrap()
        .unwrap()
        .get_property_value(prop)
        .unwrap();
    value.trim_end_matches("px").parse().unwrap_or(0.0)
}

#[wasm_bindgen_test]
async fn the_failure_notice_stays_readable_beside_the_waiting_room_panel() {
    let buttons = [
        "button.host-controls-toggle",
        "button.btn-admit-all",
        "button[aria-label='Admit dana']",
        "button[aria-label='Reject erin']",
    ];
    let tall = [(360.0, 780.0), (1280.0, 780.0)];
    let short = [(568.0, 320.0), (667.0, 375.0), (844.0, 390.0)];
    let mut failures = Vec::new();
    for (drawer, waiting, notice, sizes, taps) in [
        (Drawer::Closed, 2, Some(FAILURE), &tall[..], &buttons[..]),
        (Drawer::Roster, 2, Some(FAILURE), &tall[..], &buttons[..]),
        (
            Drawer::Closed,
            6,
            Some(LONG_FAILURE),
            &short[..],
            &buttons[..2],
        ),
        (
            Drawer::Closed,
            6,
            None,
            &[(568.0, 230.0)][..],
            &buttons[..2],
        ),
    ] {
        let layout = Layout {
            drawer,
            waiting,
            notice: notice.unwrap_or(FAILURE),
            ..PHONE_ROSTER
        };
        let (frame, doc) = meeting_frame(layout).await;
        for &(width, height) in sizes {
            let docked = drawer == Drawer::Roster && width >= 568.0;
            resize(
                &frame,
                &doc,
                (width, height),
                if docked { 400.0 } else { 0.0 },
            )
            .await;
            let mut wrong: Vec<String> = Vec::new();
            if notice.is_some() && !notice_text_readable(&doc) {
                wrong.push("notice text hidden".into());
            }
            if drawer == Drawer::Closed || docked {
                let missed = taps.iter().filter(|b| !hits_at_center(&doc, b));
                wrong.extend(missed.map(|b| format!("taps miss {b}")));
            }
            if !wrong.is_empty() {
                failures.push((format!("{drawer:?} {waiting} {width}x{height}"), wrong));
            }
        }
        frame.remove();
    }
    assert_eq!(failures, Vec::<(String, Vec<String>)>::new());
}

#[wasm_bindgen_test]
async fn without_a_failure_notice_the_waiting_panel_leaves_the_toast_stack_in_place() {
    let mut tops = Vec::new();
    for waiting in [0, 2] {
        let (frame, doc) = meeting_frame(Layout {
            drawer: Drawer::Closed,
            waiting,
            notice: SUCCESS,
            is_error: false,
            ..PHONE_ROSTER
        })
        .await;
        for width in [360.0, 1280.0] {
            resize(&frame, &doc, (width, 780.0), 0.0).await;
            let top = el(&doc, ".peer-toasts").get_bounding_client_rect().top();
            tops.push((waiting, width, top));
        }
        frame.remove();
    }
    let at = |waiting: usize| -> Vec<(f64, f64)> {
        tops.iter()
            .filter(|t| t.0 == waiting)
            .map(|t| (t.1, t.2))
            .collect()
    };
    assert_eq!(at(2), at(0));
}

#[wasm_bindgen_test]
async fn with_captions_on_the_failure_notice_clears_the_caption_lane() {
    let (frame, doc) = meeting_frame(Layout {
        drawer: Drawer::Closed,
        waiting: 6,
        notice: NETWORK_FAILURE,
        captions: true,
        ..PHONE_ROSTER
    })
    .await;
    let metrics = doc.create_element("style").unwrap();
    doc.head().unwrap().append_child(&metrics).unwrap();
    let lane = Some(".captions-overlay");
    let newest = Some(".captions-overlay > .caption-line:last-child");
    let mut failures = Vec::new();
    for (profile, css) in [("", ""), (" linux", LINUX_METRICS)] {
        metrics.set_text_content(Some(css));
        for (width, height, kept_clear) in [
            (320.0, 480.0, newest),
            (320.0, 568.0, lane),
            (360.0, 640.0, lane),
            (1280.0, 540.0, lane),
            (1280.0, 720.0, lane),
            (568.0, 320.0, None),
            (844.0, 390.0, None),
        ] {
            resize(&frame, &doc, (width, height), 0.0).await;
            let mut wrong: Vec<String> = Vec::new();
            if !notice_text_readable(&doc) {
                wrong.push("notice text hidden".into());
            }
            let notice = el(&doc, ALERT).get_bounding_client_rect();
            if let Some(captions) = kept_clear {
                let r = el(&doc, captions).get_bounding_client_rect();
                if overlaps(&notice, &r) {
                    let by = notice.bottom() - r.top();
                    wrong.push(format!("notice covers {captions} by {by:.0}px"));
                }
            }
            for b in ["button.host-controls-toggle", "button.btn-admit-all"] {
                if !hits_at_center(&doc, b) {
                    wrong.push(format!("taps miss {b}"));
                }
            }
            if !wrong.is_empty() {
                failures.push((format!("{width}x{height}{profile}"), wrong));
            }
        }
    }
    frame.remove();
    assert_eq!(failures, Vec::<(String, Vec<String>)>::new());
}

#[wasm_bindgen_test]
fn every_dvh_waiting_list_cap_keeps_a_vh_fallback() {
    let collect = js_sys::Function::new_with_args(
        "css",
        "const sheet = new CSSStyleSheet(); sheet.replaceSync(css); const caps = [];
         const walk = (rules, cond) => { for (const r of rules) {
           if (r instanceof CSSStyleRule) {
             const value = r.style.getPropertyValue('max-height');
             if (r.selectorText.endsWith('.host-controls-list') && value)
               caps.push([cond, r.selectorText, value].join('|'));
           } else if (r.cssRules) {
             walk(r.cssRules, r instanceof CSSSupportsRule ? r.conditionText : cond);
           } } };
         walk(sheet.cssRules, ''); return caps;",
    );
    let caps = strings(
        collect
            .call1(&wasm_bindgen::JsValue::NULL, &STYLE_CSS.into())
            .unwrap(),
    );
    let caps: Vec<Vec<&str>> = caps.iter().map(|c| c.splitn(3, '|').collect()).collect();
    let unguarded: Vec<&str> = caps
        .iter()
        .filter(|c| c[2].contains("dvh") && !c[0].contains("dvh"))
        .map(|c| c[1])
        .collect();
    let no_fallback: Vec<&str> = caps
        .iter()
        .filter(|c| {
            !caps
                .iter()
                .any(|f| f[0].is_empty() && f[1] == c[1] && !f[2].contains("dvh"))
        })
        .map(|c| c[1])
        .collect();
    assert!(!caps.is_empty(), "the waiting list has a max-height cap");
    assert_eq!((unguarded, no_fallback), (vec![], vec![]));
}

#[wasm_bindgen_test]
async fn over_an_open_phone_drawer_only_the_failure_notice_shows_and_takes_taps() {
    let controls = [
        ".sidebar-header .close-button",
        ".search-input",
        "button[aria-label='Host actions']",
    ];
    let others = [
        "[data-testid='host-mute-notice']",
        "[data-testid='recording-banner']",
        "[data-testid='join-toast']",
        "[data-testid='self-view-toast']",
    ];
    let mut failures = Vec::new();
    for drawer in [Drawer::Roster, Drawer::Diagnostics] {
        let (frame, doc) = meeting_frame(Layout {
            drawer,
            ..PHONE_ROSTER
        })
        .await;
        let drawer_sel = match drawer {
            Drawer::Diagnostics => "#diagnostics-sidebar",
            _ => "#peer-list-container",
        };
        for width in [320.0, 360.0, 414.0, 567.0] {
            resize(&frame, &doc, (width, 780.0), 0.0).await;
            let rect = el(&doc, ALERT).get_bounding_client_rect();
            let mut wrong: Vec<String> = Vec::new();
            if !hits_at_center(&doc, ALERT) || rect.top() < 0.0 || rect.bottom() > 780.0 {
                wrong.push("failure notice not on top".into());
            }
            if drawer == Drawer::Roster {
                for c in controls.iter().filter(|c| !hits_at_center(&doc, c)) {
                    wrong.push(format!("taps miss {c}"));
                }
                for c in controls.iter().chain(&[".sidebar-header"]) {
                    if overlaps(&rect, &el(&doc, c).get_bounding_client_rect()) {
                        wrong.push(format!("notice covers {c}"));
                    }
                }
            }
            for t in others {
                let r = el(&doc, t).get_bounding_client_rect();
                if r.width() > 1.0 || r.height() > 1.0 {
                    wrong.push(format!("painted over the drawer: {t}"));
                }
            }
            let clearance = 780.0 - rect.top();
            for prop in ["scroll-padding-bottom", "padding-bottom"] {
                if computed_px(&doc, drawer_sel, prop) < clearance {
                    wrong.push(format!("{drawer_sel} {prop} under {clearance}px"));
                }
            }
            if !wrong.is_empty() {
                failures.push((format!("{drawer:?} {width}px"), wrong));
            }
        }
        frame.remove();
    }
    assert_eq!(failures, Vec::<(String, Vec<String>)>::new());
}

#[wasm_bindgen_test]
async fn a_success_notice_stays_under_an_open_phone_drawer() {
    let (frame, doc) = meeting_frame(Layout {
        is_error: false,
        notice: SUCCESS,
        ..PHONE_ROSTER
    })
    .await;
    resize(&frame, &doc, (360.0, 780.0), 0.0).await;
    let notice = el(&doc, "[data-testid='co-host-notice']");
    let r = notice.get_bounding_client_rect();
    notice
        .set_attribute("style", "pointer-events: auto")
        .unwrap();
    let drawer = el(&doc, "#peer-list-container");
    let under = lands_in(
        &doc,
        &drawer,
        r.left() + r.width() / 2.0,
        r.top() + r.height() / 2.0,
    );
    frame.remove();
    assert!(under, "the drawer paints over a success notice");
}

#[wasm_bindgen_test]
async fn a_long_failure_notice_fits_beside_a_docked_drawer_and_on_a_phone() {
    let long =
        "Couldn't transfer host to averyveryverylongunbrokendisplayname_without_any_spaces. \
                They may have left, or you're no longer a host.";
    let mut failures = Vec::new();
    for (drawer, width, reserve) in [(Drawer::Roster, 580.0, 320.0), (Drawer::Closed, 320.0, 0.0)] {
        let (frame, doc) = meeting_frame(Layout {
            drawer,
            notice: long,
            ..PHONE_ROSTER
        })
        .await;
        if reserve > 0.0 {
            let width_style = format!("width: {reserve}px");
            el(&doc, "#peer-list-container")
                .set_attribute("style", &width_style)
                .unwrap();
        }
        resize(&frame, &doc, (width, 780.0), reserve).await;
        let r = el(&doc, ALERT).get_bounding_client_rect();
        if !notice_text_readable(&doc) || r.left() < reserve || r.right() > width {
            failures.push((width, (r.left(), r.right())));
        }
        frame.remove();
    }
    assert_eq!(failures, vec![]);
}

thread_local! {
    static PAGE_CLICKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[allow(non_snake_case)]
fn NoticeOverPageParent() -> Element {
    let notice = use_signal(|| {
        Some(CoHostNotice {
            seq: 1,
            text: "Couldn't mute everyone. Only hosts can do that.".to_string(),
            is_error: true,
        })
    });
    rsx! {
        div { onclick: move |_| PAGE_CLICKS.with(|c| c.set(c.get() + 1)),
            CoHostNoticeLayer { notice }
        }
    }
}

#[wasm_bindgen_test]
async fn a_tap_on_a_failure_notice_stops_at_the_notice() {
    reset_test_browser_state();
    PAGE_CLICKS.with(|c| c.set(0));
    let mount = create_mount_point();
    render_into(&mount, NoticeOverPageParent);
    settle().await;
    click(&mount, "[data-testid='co-host-notice']");
    settle().await;
    let after_notice = PAGE_CLICKS.with(|c| c.get());
    click(&mount, "[data-testid='co-host-status']");
    settle().await;
    let after_page = PAGE_CLICKS.with(|c| c.get());
    assert_eq!(
        (after_notice, after_page),
        (0, 1),
        "the notice absorbs its tap; a tap beside it still reaches the page"
    );
    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// Roster row and the notice live region
// ---------------------------------------------------------------------------

#[allow(non_snake_case)]
fn RosterRowParent() -> Element {
    let appearance = use_signal(AppearanceSettings::default);
    use_context_provider(|| AppearanceSettingsCtx(appearance));
    let notice = use_signal(|| None::<CoHostNotice>);
    use_context_provider(|| CoHostNoticeCtx(notice));
    let kick: EventHandler<()> = use_callback(|_: ()| {});
    let transfer: EventHandler<()> = use_callback(|_: ()| {});
    rsx! {
        PeerListItem {
            name: "Alice".to_string(),
            tooltip: ALICE.to_string(),
            is_host: true,
            is_co_host: true,
            on_kick: kick,
            on_transfer_host: transfer,
            co_host_request: CoHostRequest {
                action: CoHostMenuAction::Remove,
                meeting_id: "m1".to_string(),
                user_id: ALICE.to_string(),
                display_name: "Alice".to_string(),
            },
        }
        CoHostNoticeLayer { notice }
    }
}

#[wasm_bindgen_test]
async fn the_roster_row_orders_items_labels_and_announces_a_refusal() {
    reset_test_browser_state();
    inject_app_config();
    mock_co_host_api("{}");
    let mount = create_mount_point();
    render_into(&mount, RosterRowParent);
    settle().await;

    assert_eq!(
        text_of(&mount, ".peer-indicator").as_deref(),
        Some("(Co-host)")
    );
    let row = query(&mount, ".peer_item").expect("row renders");
    assert_eq!(
        row.get_attribute("title").as_deref(),
        Some("Co-host: alice@example.com")
    );

    let trigger = query(&mount, "[data-testid='peer-item-menu-button']").expect("trigger");
    assert!(!trigger.has_attribute("aria-haspopup"));
    assert_eq!(
        trigger.get_attribute("aria-expanded").as_deref(),
        Some("false")
    );
    click(&mount, "[data-testid='peer-item-menu-button']");
    settle().await;
    assert_eq!(
        trigger.get_attribute("aria-expanded").as_deref(),
        Some("true")
    );
    assert_eq!(
        menu_items(&mount, ".peer_item_context_menu .context-menu-item"),
        vec!["Remove co-host", "Transfer host", "Remove from meeting"]
    );

    click(&mount, "[data-testid='peer-item-co-host-action']");
    settle().await;
    assert!(
        is_focused(&trigger),
        "focus must return to the row's menu trigger"
    );
    assert!(
        wait_for_selector(
            &mount,
            "[data-testid='co-host-notice'][role='alert']",
            3_000
        )
        .await,
        "the refusal must reach an alert"
    );
    assert_eq!(
        text_of(&mount, "[data-testid='co-host-notice']").as_deref(),
        Some("Couldn't remove Alice as co-host. Can't remove the only host in the meeting.")
    );
    cleanup(&mount);
    restore_fetch();
}

#[allow(non_snake_case)]
fn NoticeParent() -> Element {
    let notice = use_signal(|| None::<CoHostNotice>);
    let ctx = CoHostNoticeCtx(notice);
    let make = |user_id: &str, name: &str| CoHostRequest {
        action: CoHostMenuAction::Make,
        meeting_id: "m1".to_string(),
        user_id: user_id.to_string(),
        display_name: name.to_string(),
    };
    let make_alice = make(ALICE, "Alice");
    let make_bob = make(BOB, "Bob");
    rsx! {
        button {
            "data-testid": "run-make",
            onclick: move |_| run_co_host_request(make_alice.clone(), Some(ctx)),
            "make"
        }
        button {
            "data-testid": "run-make-bob",
            onclick: move |_| run_co_host_request(make_bob.clone(), Some(ctx)),
            "make bob"
        }
        CoHostNoticeLayer { notice }
    }
}

#[wasm_bindgen_test]
async fn success_is_announced_in_one_persistent_status_region() {
    reset_test_browser_state();
    inject_app_config();
    mock_co_host_api("{}");
    let mount = create_mount_point();
    render_into(&mount, NoticeParent);
    settle().await;

    let region = query(&mount, "[data-testid='co-host-status']")
        .expect("the status region is mounted before anything is announced");
    assert_eq!(region.get_attribute("role").as_deref(), Some("status"));
    assert_eq!(region.text_content().as_deref(), Some(""));

    click(&mount, "[data-testid='run-make']");
    assert!(wait_for_selector(&mount, "[data-testid='co-host-notice']", 3_000).await);
    let first = region.text_content().unwrap_or_default();
    assert!(
        first.starts_with("Alice is now a co-host and saved for future meetings."),
        "{first}"
    );
    assert!(
        !first.contains("Meeting Options"),
        "no more how-to-save instructions: {first}"
    );
    assert_eq!(
        grant_bodies(),
        vec![r#"{"user_id":"alice@example.com"}"#.to_string()],
        "the menu grant leaves persist unset, so a saved co-host stays saved"
    );
    assert!(
        query(&mount, "[data-testid='co-host-notice'][role]").is_none(),
        "the visible success toast must not be a second live region"
    );

    click(&mount, "[data-testid='run-make']");
    for _ in 0..50 {
        if region.text_content().unwrap_or_default() != first {
            break;
        }
        sleep_ms(20).await;
    }
    let same_node = query(&mount, "[data-testid='co-host-status']").unwrap();
    assert!(
        same_node.is_same_node(Some(&region)),
        "the region stays mounted"
    );
    assert_ne!(
        region.text_content().unwrap_or_default(),
        first,
        "a repeated message must still change the region's text"
    );

    click(&mount, "[data-testid='run-make-bob']");
    for _ in 0..50 {
        if region.text_content().unwrap_or_default().starts_with("Bob") {
            break;
        }
        sleep_ms(20).await;
    }
    let bob = region.text_content().unwrap_or_default();
    assert!(
        bob.starts_with("Bob is now a co-host and saved for future meetings."),
        "the toast confirms the save unconditionally now — no more how-to-save copy: {bob}"
    );
    cleanup(&mount);
    restore_fetch();
}

// ---------------------------------------------------------------------------
// Owner-only surfaces
// ---------------------------------------------------------------------------

#[allow(non_snake_case)]
fn PreJoinParent() -> Element {
    let s = scenario();
    let waiting_room = use_signal(|| true);
    let admitted_can_admit = use_signal(|| false);
    let end_on_host_leave = use_signal(|| true);
    let allow_guests = use_signal(|| false);
    let recording = use_signal(|| false);
    let chat = use_signal(|| true);
    let saving = use_signal(|| false);
    let toggle_error = use_signal(|| None::<String>);
    let connection_error = use_signal(|| None::<String>);
    let join: EventHandler<()> = use_callback(|_: ()| {});
    rsx! {
        PreJoinSettingsCard {
            ownership: ownership(&s.viewer),
            is_host: s.viewer_is_host,
            meeting_id: "m1".to_string(),
            owner_user_id: Some(OWNER.to_string()),
            meeting_active: true,
            waiting_room_toggle: waiting_room,
            admitted_can_admit_toggle: admitted_can_admit,
            end_on_host_leave_toggle: end_on_host_leave,
            allow_guests_toggle: allow_guests,
            recording_allowed_for_all_toggle: recording,
            chat_allowed_for_all_toggle: chat,
            announcement_overrides: use_override_editor(use_signal(Default::default)),
            saving,
            toggle_error,
            connection_error,
            on_join: join,
        }
    }
}

#[allow(non_snake_case)]
fn PanelParent() -> Element {
    let s = scenario();
    let mut open = use_signal(|| false);
    let waiting_room = use_signal(|| true);
    let admitted_can_admit = use_signal(|| false);
    let end_on_host_leave = use_signal(|| true);
    let allow_guests = use_signal(|| false);
    let recording = use_signal(|| false);
    let chat = use_signal(|| true);
    let saving = use_signal(|| false);
    let toggle_error = use_signal(|| None::<String>);
    rsx! {
        button {
            "data-testid": "opener",
            onclick: move |_| open.set(true),
            "Meeting options"
        }
        MeetingOptionsPanel {
            ownership: ownership(&s.viewer),
            is_host: s.viewer_is_host,
            open,
            meeting_id: "m1".to_string(),
            owner_user_id: Some(OWNER.to_string()),
            waiting_room_toggle: waiting_room,
            admitted_can_admit_toggle: admitted_can_admit,
            end_on_host_leave_toggle: end_on_host_leave,
            allow_guests_toggle: allow_guests,
            recording_allowed_for_all_toggle: recording,
            chat_allowed_for_all_toggle: chat,
            announcement_overrides: use_override_editor(use_signal(Default::default)),
            saving,
            toggle_error,
        }
    }
}

async fn render_as(viewer: &str, root: fn() -> Element) -> web_sys::Element {
    render_as_host(viewer, false, root).await
}

/// Like `render_as`, but also controls `viewer_is_host`.
async fn render_as_host(
    viewer: &str,
    viewer_is_host: bool,
    root: fn() -> Element,
) -> web_sys::Element {
    reset_test_browser_state();
    inject_app_config();
    mock_co_host_api("{}");
    set_scenario(Scenario {
        viewer: viewer.into(),
        viewer_is_host,
        ..Default::default()
    });
    let mount = create_mount_point();
    render_into(&mount, root);
    settle().await;
    mount
}

/// A plain, non-host participant gets neither the meeting-option toggles nor
/// the co-hosts section on the pre-join card.
#[wasm_bindgen_test]
async fn a_plain_participant_gets_no_options_or_co_hosts_on_pre_join() {
    let mount = render_as(COHOST, PreJoinParent).await;
    assert!(
        query(&mount, ".settings-card").is_some(),
        "positive control"
    );
    assert!(
        query(&mount, ".settings-option-row").is_none(),
        "no meeting-option toggles for a plain participant"
    );
    assert!(query(&mount, "[data-testid='co-hosts-section']").is_none());
    assert_eq!(
        text_of(&mount, ".settings-action-btn").as_deref(),
        Some("Join Meeting")
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_co_host_who_is_host_gets_options_but_not_co_hosts_on_pre_join() {
    let mount = render_as_host(COHOST, true, PreJoinParent).await;
    assert!(
        query(&mount, ".settings-option-row").is_some(),
        "a co-host currently holding the host role may edit meeting options"
    );
    assert!(
        query(&mount, "[data-testid='co-hosts-section']").is_none(),
        "co-host management stays owner-only"
    );
    assert_eq!(
        text_of(&mount, ".settings-action-btn").as_deref(),
        Some("Join Meeting"),
        "the button stays Join Meeting for a non-owner host"
    );
    assert_eq!(
        list_reads(),
        0,
        "no co-host-list GET may fire for a non-owner, even a host"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn the_owner_gets_options_and_co_hosts_on_pre_join() {
    let mount = render_as(OWNER, PreJoinParent).await;
    assert!(query(&mount, ".settings-option-row").is_some());
    let details = query(&mount, "[data-testid='co-hosts-section'] details")
        .expect("the pre-join co-hosts section is collapsible");
    assert!(!details.has_attribute("open"), "collapsed by default");
    assert!(
        wait_for_selector(&mount, "[data-testid='co-host-row']", 3_000).await,
        "the list still loads while collapsed"
    );
    assert_eq!(
        text_of(&mount, ".co-hosts-summary").as_deref(),
        Some("Co-hosts (4)")
    );
    assert_eq!(
        text_of(&mount, ".settings-action-btn").as_deref(),
        Some("Start Meeting")
    );
    cleanup(&mount);
    restore_fetch();
}

fn computed(el: &web_sys::Element, property: &str) -> String {
    gloo_utils::window()
        .get_computed_style(el)
        .unwrap()
        .unwrap()
        .get_property_value(property)
        .unwrap()
}

#[allow(non_snake_case)]
fn HeroPreJoinParent() -> Element {
    rsx! {
        div { class: "hero-content", PreJoinParent {} }
    }
}

#[wasm_bindgen_test]
async fn the_pre_join_co_hosts_disclosure_reads_like_an_option_row_clear_of_start_meeting() {
    let doc = gloo_utils::document();
    let style = doc.create_element("style").unwrap();
    style.set_text_content(Some(&format!(
        "{}{}.co-hosts-chevron{{transition:none}}",
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    )));
    doc.head().unwrap().append_child(&style).unwrap();
    let mount = render_as(OWNER, HeroPreJoinParent).await;
    let loaded = wait_for_selector(&mount, "[data-testid='co-host-row']", 3_000).await;
    let rect = |selector: &str| query(&mount, selector).map(|e| e.get_bounding_client_rect());
    let css =
        |selector: &str, property: &str| query(&mount, selector).map(|e| computed(&e, property));
    let gap = rect(".settings-action-btn")
        .zip(rect(".co-hosts-summary"))
        .map(|(btn, summary)| btn.top() - summary.bottom());
    let summary_display = css(".co-hosts-summary", "display");
    let section_border_top = css("[data-testid='co-hosts-section']", "border-top-style");
    let font = |selector: &str| css(selector, "font-weight").zip(css(selector, "font-size"));
    let heading_font = font(".co-hosts-summary .co-hosts-heading");
    let label_font = font(".settings-option-label");
    let closed_transform = css(".co-hosts-chevron", "transform");
    if let Some(summary) = query(&mount, ".co-hosts-summary") {
        html(&summary).click();
    }
    settle().await;
    let opened = query(&mount, ".co-hosts-details").map(|d| d.has_attribute("open"));
    let open_transform = css(".co-hosts-chevron", "transform");
    let body_display = css(".co-hosts-body", "display");
    let details = rect(".co-hosts-details");
    let above = details
        .clone()
        .zip(rect(".co-hosts-add"))
        .map(|(d, add)| d.bottom() - add.bottom());
    let below = rect(".settings-action-btn")
        .zip(details)
        .map(|(btn, d)| btn.top() - d.bottom());
    style.remove();
    cleanup(&mount);
    restore_fetch();

    assert!(loaded, "positive control: the co-host list loaded");
    assert!(
        gap.is_some_and(|g| g >= 16.0),
        "Start Meeting must sit clear of the collapsed Co-hosts row; gap was {gap:?}px"
    );
    assert!(
        summary_display.as_deref().is_some_and(|d| d != "list-item"),
        "the summary must not render the UA disclosure marker, got {summary_display:?}"
    );
    assert_eq!(
        section_border_top.as_deref(),
        Some("none"),
        "the collapsible section drops the section divider"
    );
    assert!(
        heading_font.is_some() && heading_font == label_font,
        "the Co-hosts label must match the sibling option-row labels: \
         {heading_font:?} vs {label_font:?}"
    );
    assert_eq!(
        closed_transform.as_deref(),
        Some("none"),
        "chevron rendered"
    );
    assert_eq!(
        opened,
        Some(true),
        "clicking the summary opens the disclosure"
    );
    assert!(
        open_transform
            .as_deref()
            .is_some_and(|t| t.starts_with("matrix(-1,")),
        "the chevron flips when open, got {open_transform:?}"
    );
    assert_eq!(
        body_display.as_deref(),
        Some("flex"),
        "the expanded body spaces its children"
    );
    assert!(
        above
            .zip(below)
            .is_some_and(|(above, below)| above <= below + 8.0),
        "the expanded divider must not sit nearer Start Meeting than the add form: \
         above {above:?}px, below {below:?}px"
    );
}

#[wasm_bindgen_test]
async fn a_plain_participant_does_not_get_the_in_call_meeting_options_panel() {
    let mount = render_as(COHOST, PanelParent).await;
    click(&mount, "[data-testid='opener']");
    settle().await;
    assert!(query(&mount, "[data-testid='meeting-options-panel']").is_none());
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_co_host_who_is_host_gets_the_panel_but_not_co_hosts() {
    let mount = render_as_host(COHOST, true, PanelParent).await;
    click(&mount, "[data-testid='opener']");
    settle().await;
    assert!(
        query(&mount, "[data-testid='meeting-options-panel']").is_some(),
        "a co-host currently holding the host role gets the dialog"
    );
    assert!(query(&mount, ".settings-option-row").is_some());
    assert!(
        query(&mount, "[data-testid='co-hosts-section']").is_none(),
        "co-host management stays owner-only, even inside the dialog"
    );
    assert_eq!(
        list_reads(),
        0,
        "no co-host-list GET may fire for a non-owner, even a host"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn the_meeting_options_dialog_takes_and_returns_focus() {
    let mount = render_as(OWNER, PanelParent).await;
    let opener = query(&mount, "[data-testid='opener']").unwrap();

    for close_with_escape in [false, true] {
        html(&opener).focus().unwrap();
        html(&opener).click();
        settle().await;
        let dialog = query(&mount, "[data-testid='meeting-options-panel']").expect("opens");
        assert_eq!(dialog.get_attribute("role").as_deref(), Some("dialog"));
        assert_eq!(dialog.get_attribute("aria-modal").as_deref(), Some("true"));
        let title_id = dialog.get_attribute("aria-labelledby").expect("named");
        let title = gloo_utils::document().get_element_by_id(&title_id).unwrap();
        assert_eq!(title.text_content().as_deref(), Some("Meeting Options"));
        assert!(is_focused(&title), "initial focus lands inside the dialog");
        assert!(query(&mount, "[data-testid='co-hosts-section']").is_some());

        if !close_with_escape {
            assert!(wait_for_selector(&mount, "[data-testid='co-host-row']", 3_000).await);
            let nodes = dialog
                .query_selector_all("button:not([disabled]), input:not([disabled]), summary")
                .unwrap();
            let first = nodes
                .item(0)
                .unwrap()
                .dyn_into::<web_sys::Element>()
                .unwrap();
            let last = nodes
                .item(nodes.length() - 1)
                .unwrap()
                .dyn_into::<web_sys::Element>()
                .unwrap();
            html(&last).focus().unwrap();
            press_tab(&last, false);
            settle().await;
            assert!(
                is_focused(&first),
                "Tab from the last control wraps to the first"
            );
            press_tab(&first, true);
            settle().await;
            assert!(
                is_focused(&last),
                "Shift+Tab from the first wraps to the last"
            );
            html(&title).focus().unwrap();
            press_tab(&title, true);
            settle().await;
            assert!(
                is_focused(&last),
                "Shift+Tab from the title wraps to the last"
            );
        }

        if close_with_escape {
            press_escape(&title);
        } else {
            click(&mount, "[aria-label='Close meeting options']");
        }
        settle().await;
        assert!(query(&mount, "[data-testid='meeting-options-panel']").is_none());
        assert!(
            is_focused(&opener),
            "focus returns to the opener (escape: {close_with_escape})"
        );
    }
    cleanup(&mount);
    restore_fetch();
}

// ---------------------------------------------------------------------------
// Co-hosts section
// ---------------------------------------------------------------------------

#[allow(non_snake_case)]
fn SectionParent() -> Element {
    let s = scenario();
    let hosts = use_signal(|| s.hosts.iter().cloned().collect::<HashSet<String>>());
    let with_host_set = s.with_host_set;
    use_hook(move || {
        if with_host_set {
            provide_context(HostSetCtx(hosts));
        }
    });
    let refresh = use_signal(|| 0u64);
    use_hook(move || REFRESH.with(|r| *r.borrow_mut() = Some(refresh)));
    let mut active = use_signal(|| s.meeting_active);
    let notice = use_signal(|| None::<CoHostNotice>);
    rsx! {
        button { "data-testid": "set-active", onclick: move |_| active.set(true), "active" }
        button { "data-testid": "set-idle", onclick: move |_| active.set(false), "idle" }
        button {
            "data-testid": "post-toast",
            onclick: move |_| post_co_host_notice(CoHostNoticeCtx(notice), "toast".into(), false),
            "toast"
        }
        CoHostsSection {
            meeting_id: "m1".to_string(),
            owner_user_id: Some(OWNER.to_string()),
            meeting_active: active(),
            refresh,
            card_title: s.card_title,
            read_only: s.read_only,
        }
        CoHostNoticeLayer { notice }
    }
}

fn mount_section(s: Scenario, mode: &str) -> web_sys::Element {
    reset_test_browser_state();
    inject_app_config();
    mock_co_host_api(mode);
    set_scenario(Scenario {
        viewer: OWNER.into(),
        ..s
    });
    let mount = create_mount_point();
    render_into(&mount, SectionParent);
    mount
}

async fn render_section(s: Scenario, mode: &str) -> web_sys::Element {
    let mount = mount_section(s, mode);
    assert!(
        wait_for_selector(&mount, "[data-testid='co-host-row']", 3_000).await,
        "the listed co-hosts must render"
    );
    mount
}

fn row(mount: &web_sys::Element, user_id: &str) -> web_sys::Element {
    query(
        mount,
        &format!("[data-testid='co-host-row'][data-user-id='{user_id}']"),
    )
    .unwrap_or_else(|| panic!("row for {user_id}"))
}

fn has(el: &web_sys::Element, selector: &str) -> bool {
    el.query_selector(selector).unwrap().is_some()
}

#[wasm_bindgen_test]
async fn a_non_owner_sees_no_co_host_data_or_form() {
    reset_test_browser_state();
    inject_app_config();
    mock_co_host_api("{ list: 'forbidden' }");
    set_scenario(Scenario {
        viewer: COHOST.into(),
        meeting_active: true,
        ..Default::default()
    });
    let mount = create_mount_point();
    render_into(&mount, SectionParent);
    assert!(query(&mount, "[data-testid='co-host-input']").is_none());
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-forbidden']", 3_000).await);
    assert!(query(&mount, "[data-testid='co-host-row']").is_none());
    assert!(query(&mount, "[data-testid='co-host-input']").is_none());
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn rows_show_presence_saved_state_suspension_and_designation() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    let alice = row(&mount, ALICE);
    assert!(has(&alice, "[data-testid='co-host-present']"));
    let alice_toggle = alice.query_selector("[role='switch']").unwrap().unwrap();
    assert_eq!(
        alice_toggle.get_attribute("aria-checked").as_deref(),
        Some("true")
    );
    assert_eq!(
        alice_toggle.get_attribute("aria-label").as_deref(),
        Some("Save for future meetings (Alice)")
    );
    let bob = row(&mount, BOB);
    assert!(!has(&bob, "[data-testid='co-host-present']"));
    let bob_toggle = bob.query_selector("[role='switch']").unwrap().unwrap();
    assert_eq!(
        bob_toggle.get_attribute("aria-checked").as_deref(),
        Some("false")
    );

    let terry = row(&mount, "terry@example.com");
    assert_eq!(
        text_of(&terry, "[data-testid='co-host-undesignated']").as_deref(),
        Some("Host by transfer")
    );
    assert!(
        !has(&terry, "[role='switch']"),
        "no save switch without an entry"
    );
    assert!(has(&terry, "[data-testid='co-host-remove']"));

    let sam = row(&mount, "sam@example.com");
    assert_eq!(
        text_of(&sam, "[data-testid='co-host-suspended']").as_deref(),
        Some("Paused for this meeting. Add them again to restore.")
    );
    assert!(
        !has(&sam, "[role='switch']"),
        "re-granting would lift the pause"
    );
    assert!(has(&sam, "[data-testid='co-host-saved']"));
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn presence_follows_the_live_host_set_not_the_fetched_flag() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            with_host_set: true,
            hosts: vec![BOB.into()],
            ..Default::default()
        },
        "{}",
    )
    .await;
    assert!(!has(&row(&mount, ALICE), "[data-testid='co-host-present']"));
    assert!(has(&row(&mount, BOB), "[data-testid='co-host-present']"));
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn host_events_refetch_once_after_a_debounce() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    assert_eq!(list_reads(), 1, "fetched once on open");
    let mut refresh = REFRESH.with(|r| r.borrow().expect("refresh signal"));
    for _ in 0..3 {
        refresh.set(refresh() + 1);
        settle().await;
    }
    sleep_ms(500).await;
    assert_eq!(list_reads(), 1, "no per-event refetch");
    sleep_ms(1_500).await;
    assert_eq!(list_reads(), 2, "one trailing refetch for the burst");
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn an_idle_meeting_shows_a_saved_tag_instead_of_a_switch() {
    let mount = render_section(Scenario::default(), "{}").await;
    let alice = row(&mount, ALICE);
    assert!(!has(&alice, "[role='switch']"));
    assert_eq!(
        text_of(&alice, "[data-testid='co-host-saved']").as_deref(),
        Some("Saved")
    );
    assert!(has(&row(&mount, BOB), "[role='switch']"));
    cleanup(&mount);
    restore_fetch();
}

/// The add form no longer offers a persist choice: it always sends no
/// `persist` field, which the server treats as "save for future meetings".
/// Fails on the old markup, which sent an explicit `"persist":true` here.
#[wasm_bindgen_test]
async fn the_section_add_defaults_to_saved_by_omitting_persist() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    assert!(
        query(&mount, "[data-testid='co-host-save-for-future']").is_none(),
        "the checkbox is gone — co-hosts are saved by default now"
    );
    let input = query(&mount, "[data-testid='co-host-input']").unwrap();
    type_into(&input, "new@example.com").await;
    click(&mount, "[data-testid='co-host-add']");
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-status']:not(:empty)", 3_000).await);
    assert_eq!(
        grant_bodies(),
        vec![r#"{"user_id":"new@example.com"}"#.to_string()],
        "no persist field: the server default (saved) applies"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn busy_controls_keep_focus() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{ grant: 'pending' }",
    )
    .await;
    let input = query(&mount, "[data-testid='co-host-input']").unwrap();
    type_into(&input, "new@example.com").await;
    let add = query(&mount, "[data-testid='co-host-add']").unwrap();
    html(&add).focus().unwrap();
    html(&add).click();
    settle().await;

    assert_eq!(add.get_attribute("aria-disabled").as_deref(), Some("true"));
    let section = query(&mount, "[data-testid='co-hosts-section']").unwrap();
    assert!(
        !section.has_attribute("aria-busy"),
        "the section also holds the status region"
    );
    assert_eq!(
        query(&mount, ".co-hosts-list")
            .unwrap()
            .get_attribute("aria-busy")
            .as_deref(),
        Some("true")
    );
    assert!(
        is_focused(&add),
        "focus stays on Add while the grant is in flight"
    );
    assert!(
        !add.has_attribute("disabled"),
        "`disabled` would drop focus"
    );
    let remove = row(&mount, ALICE)
        .query_selector("[data-testid='co-host-remove']")
        .unwrap()
        .unwrap();
    assert_eq!(
        remove.get_attribute("aria-disabled").as_deref(),
        Some("true")
    );
    assert!(!remove.has_attribute("disabled"));
    let toggle = row(&mount, ALICE)
        .query_selector("[role='switch']")
        .unwrap()
        .unwrap();
    assert_eq!(
        toggle.get_attribute("aria-disabled").as_deref(),
        Some("true")
    );
    assert!(!toggle.has_attribute("disabled"));
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn removing_a_row_moves_focus_to_the_next_remove() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{ revoke: 'ok' }",
    )
    .await;
    let remove = row(&mount, ALICE)
        .query_selector("[data-testid='co-host-remove']")
        .unwrap()
        .unwrap();
    html(&remove).focus().unwrap();
    html(&remove).click();
    for _ in 0..50 {
        if query(&mount, "[data-user-id='alice@example.com']").is_none() {
            break;
        }
        sleep_ms(20).await;
    }
    settle().await;
    let next = row(&mount, BOB)
        .query_selector("[data-testid='co-host-remove']")
        .unwrap()
        .unwrap();
    let active = gloo_utils::document()
        .active_element()
        .map(|a| format!("{} #{}", a.tag_name(), a.id()));
    assert!(
        is_focused(&next),
        "focus moves to the next row's Remove; active: {active:?}, next: {}",
        next.id()
    );
    let status = text_of(&mount, "[data-testid='co-hosts-status']").unwrap_or_default();
    assert!(
        status.starts_with("Alice is no longer a co-host."),
        "status: {status:?}"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn removing_the_only_host_is_announced() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    click(&row(&mount, ALICE), "[data-testid='co-host-remove']");
    assert!(
        wait_for_selector(
            &mount,
            "[data-testid='co-hosts-error'][role='alert']",
            3_000
        )
        .await,
        "the refusal must reach an alert region"
    );
    assert_eq!(
        text_of(&mount, "[data-testid='co-hosts-error']").as_deref(),
        Some("Couldn't remove Alice as co-host. Can't remove the only host in the meeting.")
    );
    assert!(query(&mount, "[data-user-id='alice@example.com']").is_some());
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn an_invalid_add_is_flagged_and_focuses_the_input() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    let add = query(&mount, "[data-testid='co-host-add']").unwrap();
    html(&add).focus().unwrap();
    html(&add).click();
    settle().await;
    let input = query(&mount, "[data-testid='co-host-input']").unwrap();
    assert!(
        is_focused(&input),
        "a submit error moves focus to the input"
    );
    assert_eq!(input.get_attribute("aria-invalid").as_deref(), Some("true"));
    let described = input
        .get_attribute("aria-describedby")
        .expect("describedby");
    let error = gloo_utils::document()
        .get_element_by_id(&described)
        .expect("the error the input points at");
    assert_eq!(error.get_attribute("role").as_deref(), Some("alert"));
    assert_eq!(
        error.text_content().as_deref(),
        Some("Enter an email or user ID.")
    );

    type_into(&input, OWNER).await;
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    let error = gloo_utils::document()
        .get_element_by_id(&described)
        .expect("the error is shown again");
    assert_eq!(
        error.text_content().as_deref(),
        Some("That's you, the meeting owner.")
    );
    cleanup(&mount);
    restore_fetch();
}

async fn add_co_host(mount: &web_sys::Element, user_id: &str) {
    let input = query(mount, "[data-testid='co-host-input']").unwrap();
    type_into(&input, user_id).await;
    click(mount, "[data-testid='co-host-add']");
}

async fn wait_for_grants(count: usize) {
    for _ in 0..100 {
        if grant_bodies().len() >= count {
            break;
        }
        sleep_ms(20).await;
    }
    settle().await;
}

#[wasm_bindgen_test]
async fn a_toast_between_two_identical_statuses_does_not_silence_the_second() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    let status = || text_of(&mount, "[data-testid='co-hosts-status']").unwrap_or_default();
    add_co_host(&mount, "new@example.com").await;
    wait_for_grants(1).await;
    let first = status();
    assert!(first.starts_with("new@example.com was added"), "{first}");

    click(&mount, "[data-testid='post-toast']");
    settle().await;
    add_co_host(&mount, "new@example.com").await;
    wait_for_grants(2).await;
    assert_ne!(status(), first, "the repeat must change the region's text");
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_superseded_retry_does_not_leave_retrying_stuck() {
    let mount = mount_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{ plan: ['fail', 'slow', 'fail'] }",
    );
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-load-error']", 3_000).await);
    click(&mount, "[data-testid='co-hosts-retry']");
    settle().await;
    assert_eq!(
        text_of(&mount, "[data-testid='co-hosts-retry']").as_deref(),
        Some("Retrying…")
    );

    add_co_host(&mount, "new@example.com").await;
    wait_for_grants(1).await;
    assert!(wait_for_selector(&mount, "[data-testid='co-host-row']", 3_000).await);
    sleep_ms(600).await;

    let mut refresh = REFRESH.with(|r| r.borrow().expect("refresh signal"));
    refresh.set(refresh() + 1);
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-load-error']", 4_000).await);
    let retry = query(&mount, "[data-testid='co-hosts-retry']").unwrap();
    assert_eq!(retry.text_content().as_deref(), Some("Try again"));
    assert!(!retry.has_attribute("aria-disabled"));
    cleanup(&mount);
    restore_fetch();
}

/// A superseded retry's response must clear `retrying` on its own, even though
/// its list data is discarded — checked via `data-retrying` (mirrors
/// `retrying` on every render, independent of which list-state branch is
/// showing) so the assertion holds even while a later mutation has already
/// moved the section to `Ready` and hidden the retry button that would
/// otherwise surface a stuck signal.
#[wasm_bindgen_test]
async fn a_superseded_retrys_response_clears_retrying_on_its_own() {
    let mount = mount_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{ plan: ['fail', 'slow'] }",
    );
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-load-error']", 3_000).await);
    click(&mount, "[data-testid='co-hosts-retry']");
    settle().await;
    let section = query(&mount, "[data-testid='co-hosts-section']").unwrap();
    assert_eq!(
        section.get_attribute("data-retrying").as_deref(),
        Some("true"),
        "retrying must be true right after the retry click"
    );

    // Supersede the in-flight retry: a co-host grant succeeds and advances
    // the fetch generation directly, without itself re-running the list
    // fetch, so nothing else will ever clear `retrying` for us.
    add_co_host(&mount, "new@example.com").await;
    wait_for_grants(1).await;
    assert!(wait_for_selector(&mount, "[data-testid='co-host-row']", 3_000).await);

    // Let the now-superseded retry's slow response land.
    sleep_ms(600).await;
    assert_eq!(
        section.get_attribute("data-retrying").as_deref(),
        None,
        "the stale response must still clear retrying, even though its data is discarded"
    );
    cleanup(&mount);
    restore_fetch();
}

/// An older fetch (the original retry click) resolving after a newer one has
/// already started must not clear `retrying` while that newer fetch — the one
/// actually entitled to — is still in flight.
#[wasm_bindgen_test]
async fn an_older_fetch_does_not_clear_retrying_while_a_newer_one_is_in_flight() {
    let mount = mount_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        // [0] the initial load, [1] the retry click (2000ms), [2] the
        // refresh-triggered reload that starts while [1] is still pending
        // (2000ms, starting ~1500ms after [1]).
        "{ plan: ['fail', 2000, 2000] }",
    );
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-load-error']", 3_000).await);
    click(&mount, "[data-testid='co-hosts-retry']");
    settle().await;
    let section = query(&mount, "[data-testid='co-hosts-section']").unwrap();
    assert_eq!(
        section.get_attribute("data-retrying").as_deref(),
        Some("true"),
        "retrying must be true right after the retry click"
    );

    // Debounced refresh starts a second, independent reload ~1500ms later,
    // while the retry's own fetch (2000ms) is still pending.
    let mut refresh = REFRESH.with(|r| r.borrow().expect("refresh signal"));
    refresh.set(refresh() + 1);

    // ~2200ms: the OLDER fetch (the retry click, 2000ms) has resolved, but
    // the NEWER one (started ~1500ms, 2000ms of its own) has not (~3500ms).
    sleep_ms(2200).await;
    assert_eq!(
        section.get_attribute("data-retrying").as_deref(),
        Some("true"),
        "the older, superseded fetch landing must not clear retrying while a newer retry is still in flight"
    );

    // ~3700ms: the newer fetch has now resolved and is entitled to clear it.
    sleep_ms(1500).await;
    assert_eq!(
        section.get_attribute("data-retrying").as_deref(),
        None,
        "the newer, current fetch clears retrying on its own completion"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_repeated_load_failure_is_announced_again() {
    let mount = mount_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{ plan: ['fail', 'fail'] }",
    );
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-load-error']", 3_000).await);
    let first = query(&mount, "[data-testid='co-hosts-load-error']").unwrap();
    click(&mount, "[data-testid='co-hosts-retry']");
    let mut replaced = None;
    for _ in 0..100 {
        sleep_ms(20).await;
        if let Some(now) = query(&mount, "[data-testid='co-hosts-load-error']") {
            if !now.is_same_node(Some(&first)) {
                replaced = Some(now);
                break;
            }
        }
    }
    let second = replaced.expect("the second failure mounts a fresh alert");
    assert_eq!(second.text_content(), first.text_content());
    assert_eq!(second.get_attribute("role").as_deref(), Some("alert"));
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn known_inactive_from_a_switch_failure_clears_when_the_meeting_is_active_again() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{ grant: 'inactive' }",
    )
    .await;
    let switch = row(&mount, ALICE)
        .query_selector("[role='switch']")
        .unwrap()
        .unwrap();
    html(&switch).click();
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-error']", 3_000).await);
    settle().await;
    assert!(
        !has(&row(&mount, ALICE), "[role='switch']"),
        "the server said the meeting isn't running; the row falls back to a static tag"
    );

    click(&mount, "[data-testid='set-idle']");
    settle().await;
    click(&mount, "[data-testid='set-active']");
    settle().await;
    assert!(
        has(&row(&mount, ALICE), "[role='switch']"),
        "an active meeting restores the interactive switch"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_switch_that_becomes_a_tag_hands_focus_to_remove() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{ grant: 'inactive' }",
    )
    .await;
    let switch = row(&mount, ALICE)
        .query_selector("[role='switch']")
        .unwrap()
        .unwrap();
    html(&switch).focus().unwrap();
    html(&switch).click();
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-error']", 3_000).await);
    for _ in 0..50 {
        if row(&mount, ALICE)
            .query_selector("[role='switch']")
            .unwrap()
            .is_none()
        {
            break;
        }
        sleep_ms(20).await;
    }
    settle().await;
    assert!(has(&row(&mount, ALICE), "[data-testid='co-host-saved']"));
    let remove = row(&mount, ALICE)
        .query_selector("[data-testid='co-host-remove']")
        .unwrap()
        .unwrap();
    assert!(is_focused(&remove), "focus moves to the row's Remove");
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn the_settings_card_titles_the_section_like_its_siblings() {
    let mount = render_section(
        Scenario {
            card_title: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    assert_eq!(
        text_of(
            &mount,
            "[data-testid='co-hosts-section'] h3.settings-card-title"
        )
        .as_deref(),
        Some("Co-hosts")
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn read_only_renders_the_roster_with_no_add_or_remove_or_switch() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            read_only: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    assert!(
        query(&mount, "[data-testid='co-host-input']").is_none(),
        "no add input"
    );
    assert!(
        query(&mount, "[data-testid='co-host-add']").is_none(),
        "no add button"
    );
    assert!(
        query(&mount, "[data-testid='co-host-remove']").is_none(),
        "no remove button on any row"
    );
    assert!(
        query(&mount, "[role='switch']").is_none(),
        "no interactive save-for-future switch"
    );
    // The roster and its state tags are still shown: Alice is present +
    // saved, Bob is a plain designated co-host degraded from Switch to a
    // static "This meeting only" tag, Terry is the undesignated transfer
    // target, Sam is suspended.
    let alice = row(&mount, ALICE);
    assert!(has(&alice, "[data-testid='co-host-present']"));
    assert_eq!(
        text_of(&alice, "[data-testid='co-host-saved']").as_deref(),
        Some("Saved")
    );
    let bob = row(&mount, BOB);
    assert!(
        !has(&bob, "[data-testid='co-host-remove']"),
        "Bob's row has no Remove either"
    );
    assert!(
        text_of(&bob, ".co-hosts-tag")
            .as_deref()
            .is_some_and(|t| t.contains("This meeting only")),
        "Bob's would-be switch degrades to a static tag"
    );
    assert_eq!(
        text_of(&mount, "[data-testid='co-hosts-section'] .co-hosts-hint").as_deref(),
        Some("Managed by the meeting owner."),
        "explains why there are no controls, instead of the owner's hint"
    );
    cleanup(&mount);
    restore_fetch();
}

/// The list GET must still fire exactly once for a read-only viewer — the
/// widened server-side read permission (owner or anyone who can edit
/// options) is exercised the same way as the owner's fetch.
#[wasm_bindgen_test]
async fn read_only_still_fetches_the_list_exactly_once() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            read_only: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    assert_eq!(list_reads(), 1, "fetched once for a read-only viewer");
    cleanup(&mount);
    restore_fetch();
}

/// The default (`read_only: false`) path — what the owner gets — keeps its
/// add input and every row's Remove button. A false pass here would mean the
/// read-only gate above proves nothing (mutation coverage for the two tests
/// above).
#[wasm_bindgen_test]
async fn non_read_only_keeps_the_full_management_ui() {
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        "{}",
    )
    .await;
    assert!(query(&mount, "[data-testid='co-host-input']").is_some());
    assert!(query(&mount, "[data-testid='co-host-add']").is_some());
    assert!(query(&mount, "[data-testid='co-host-remove']").is_some());
    assert_eq!(
        text_of(&mount, "[data-testid='co-hosts-section'] .co-hosts-hint").as_deref(),
        Some("Co-hosts share your host controls and can change meeting options. Only you can manage co-hosts.")
    );
    cleanup(&mount);
    restore_fetch();
}

// ---------------------------------------------------------------------------
// Co-host picker (issue 3124)
// ---------------------------------------------------------------------------

fn suggestion_reads() -> u32 {
    js_sys::eval("window.__coHostSuggestGets")
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as u32
}

/// `false` when a handler called `preventDefault`.
fn fire(el: &web_sys::Element, constructor: &str, kind: &str) -> bool {
    js_sys::Function::new_with_args(
        "el, ctor, kind",
        "return el.dispatchEvent(new window[ctor](kind, { bubbles: true, cancelable: true }));",
    )
    .call3(
        &wasm_bindgen::JsValue::NULL,
        el,
        &wasm_bindgen::JsValue::from_str(constructor),
        &wasm_bindgen::JsValue::from_str(kind),
    )
    .unwrap()
    .as_bool()
    .unwrap()
}

fn track_picker_listeners() {
    js_sys::eval(
        "(function () {
            var watched = ['pointerdown', 'pointercancel', 'click', 'auxclick', 'contextmenu'];
            var add = window.addEventListener, remove = window.removeEventListener;
            window.__pickerListeners = 0;
            window.addEventListener = function (type, fn, capture) {
                if (capture === true && watched.indexOf(type) >= 0) window.__pickerListeners += 1;
                return add.call(this, type, fn, capture);
            };
            window.removeEventListener = function (type, fn, capture) {
                if (capture === true && watched.indexOf(type) >= 0) window.__pickerListeners -= 1;
                return remove.call(this, type, fn, capture);
            };
        })()",
    )
    .unwrap();
}

fn picker_listeners() -> i32 {
    js_sys::eval("window.__pickerListeners")
        .ok()
        .and_then(|v| v.as_f64())
        .expect("track_picker_listeners ran") as i32
}

fn untrack_picker_listeners() -> i32 {
    let count = picker_listeners();
    js_sys::eval("delete window.addEventListener; delete window.removeEventListener;").unwrap();
    count
}

fn press_key(el: &web_sys::Element, key: &str) -> bool {
    key_down(el, key, false)
}

fn key_down(el: &web_sys::Element, key: &str, repeat: bool) -> bool {
    let dispatch = js_sys::Function::new_with_args(
        "el, key, repeat",
        "return el.dispatchEvent(new KeyboardEvent('keydown', \
         { key: key, repeat: repeat, bubbles: true, cancelable: true }));",
    );
    dispatch
        .call3(
            &wasm_bindgen::JsValue::NULL,
            el,
            &wasm_bindgen::JsValue::from_str(key),
            &wasm_bindgen::JsValue::from_bool(repeat),
        )
        .unwrap()
        .as_bool()
        .unwrap()
}

async fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..150 {
        if ready() {
            return true;
        }
        sleep_ms(20).await;
    }
    ready()
}

fn input_value(input: &web_sys::Element) -> String {
    input
        .clone()
        .unchecked_into::<web_sys::HtmlInputElement>()
        .value()
}

fn suggested_ids(mount: &web_sys::Element) -> Vec<String> {
    let nodes = mount
        .query_selector_all("[data-testid='co-host-suggestion']")
        .unwrap();
    (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .filter_map(|n| n.dyn_into::<web_sys::Element>().ok())
        .filter_map(|el| el.get_attribute("data-user-id"))
        .collect()
}

/// Focuses the field, which is what starts the one suggestion fetch.
async fn focus_picker(mount: &web_sys::Element) -> web_sys::Element {
    let input = query(mount, "[data-testid='co-host-input']").expect("the add field");
    html(&input).focus().unwrap();
    assert!(
        wait_until(|| input.get_attribute("placeholder").as_deref() == Some("Name or email")).await,
        "the suggestions load on focus"
    );
    settle().await;
    input
}

async fn render_active_section(mode: &str) -> web_sys::Element {
    render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        mode,
    )
    .await
}

#[wasm_bindgen_test]
async fn the_picker_suggests_known_hosts_by_name_or_email_after_focus() {
    let mount = render_active_section("{}").await;
    let list = query(&mount, "[data-testid='co-host-suggestions']").unwrap();
    let input = query(&mount, "[data-testid='co-host-input']").unwrap();
    assert_eq!(suggestion_reads(), 0, "no suggestion fetch before focus");
    assert!(list.has_attribute("hidden"), "the closed list is hidden");
    assert_eq!(input.get_attribute("role").as_deref(), Some("combobox"));
    assert_eq!(input.get_attribute("aria-controls"), Some(list.id()));
    assert_eq!(list.get_attribute("role").as_deref(), Some("listbox"));
    assert_eq!(
        input.get_attribute("placeholder").as_deref(),
        Some("Email or user ID")
    );

    let input = focus_picker(&mount).await;
    assert_eq!(suggestion_reads(), 1);
    assert!(list.has_attribute("hidden"), "focus alone never opens it");

    type_into(&input, "Diaz").await;
    assert_eq!(
        suggested_ids(&mount),
        vec!["joanna@example.com"],
        "matches the name, not only the id"
    );
    type_into(&input, "ANN").await;
    assert!(!list.has_attribute("hidden"));
    assert_eq!(
        input.get_attribute("aria-expanded").as_deref(),
        Some("true")
    );
    assert_eq!(
        suggested_ids(&mount),
        vec!["ann.lee@example.com", "joanna@example.com"],
        "a name match at a word start first, then server order"
    );
    let first = query(&mount, "[data-testid='co-host-suggestion']").unwrap();
    assert_eq!(
        text_of(&first, ".co-hosts-option-name").as_deref(),
        Some("Ann Lee")
    );
    assert_eq!(
        text_of(&first, ".co-hosts-option-id").as_deref(),
        Some("ann.lee@example.com")
    );
    let options = mount.query_selector_all("[role='option']").unwrap();
    assert_eq!(options.length(), 2);
    for i in 0..options.length() {
        let option: web_sys::Element = options.item(i).unwrap().dyn_into().unwrap();
        assert!(
            !option.has_attribute("tabindex"),
            "options stay out of the dialog's Tab order"
        );
    }
    assert!(
        wait_until(|| text_of(&mount, "[data-testid='co-host-suggest-status']")
            .is_some_and(|t| t.starts_with("2 suggestions")))
        .await,
        "the count is announced after typing pauses"
    );

    type_into(&input, "").await;
    assert_eq!(
        suggested_ids(&mount),
        vec![
            "Zoe@Example.com",
            "joanna@example.com",
            "ann.lee@example.com",
            "sam.r@example.com",
            "srivera@example.com",
            "pat@example.com"
        ],
        "an empty field lists everyone except the current co-hosts"
    );

    type_into(&input, "zoe@example.com").await;
    assert!(!list.has_attribute("hidden"));
    click(&mount, "[data-testid='co-host-add']");
    wait_for_grants(1).await;
    assert_eq!(
        grant_bodies(),
        vec![r#"{"user_id":"Zoe@Example.com"}"#.to_string()]
    );
    assert!(list.has_attribute("hidden"), "Add closes the list");
    html(&input).click();
    settle().await;
    assert!(!suggested_ids(&mount).is_empty(), "positive control");
    assert!(
        !suggested_ids(&mount).contains(&"Zoe@Example.com".to_string()),
        "stored lowercase, still not suggested"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_pick_fills_the_field_and_only_add_grants() {
    let mount = render_active_section("{}").await;
    let input = focus_picker(&mount).await;
    type_into(&input, "ann").await;
    assert!(
        !press_key(&input, "ArrowDown"),
        "arrows move the active option, not the caret"
    );
    settle().await;
    let first = query(&mount, "[data-testid='co-host-suggestion']").unwrap();
    assert_eq!(
        first.get_attribute("aria-selected").as_deref(),
        Some("true")
    );
    assert_eq!(
        input.get_attribute("aria-activedescendant"),
        Some(first.id())
    );
    type_into(&input, "ann ").await;
    assert!(
        !input.has_attribute("aria-activedescendant"),
        "typing clears the active option"
    );
    press_key(&input, "ArrowDown");
    settle().await;
    assert!(input.has_attribute("aria-activedescendant"));
    assert!(press_key(&input, "Home"), "Home still moves the caret");
    settle().await;
    assert!(
        !input.has_attribute("aria-activedescendant"),
        "and clears the active option"
    );
    press_key(&input, "ArrowDown");
    settle().await;

    assert!(
        !press_key(&input, "Enter"),
        "Enter on an active option must not submit the form"
    );
    settle().await;
    assert_eq!(input_value(&input), "ann.lee@example.com");
    let list = query(&mount, "[data-testid='co-host-suggestions']").unwrap();
    assert!(list.has_attribute("hidden"));
    assert!(!input.has_attribute("aria-activedescendant"));
    assert!(is_focused(&input), "focus stays in the field");
    sleep_ms(100).await;
    assert!(grant_bodies().is_empty(), "a pick never grants");
    let selected = "Ann Lee (ann.lee@example.com) selected. Choose Add to confirm.";
    assert!(text_of(&mount, "[data-testid='co-host-suggest-status']")
        .is_some_and(|t| t.starts_with(selected)));
    assert_eq!(
        text_of(&mount, "[data-testid='co-host-suggest-note']").as_deref(),
        Some(selected),
        "the id is on screen before Add"
    );

    click(&mount, "[data-testid='co-host-add']");
    wait_for_grants(1).await;
    assert_eq!(
        grant_bodies(),
        vec![r#"{"user_id":"ann.lee@example.com"}"#.to_string()]
    );
    assert!(
        wait_until(|| text_of(&mount, "[data-testid='co-hosts-status']").is_some_and(|t| t
            .starts_with(
                "Ann Lee (ann.lee@example.com) was added as a co-host and saved for future meetings."
            )))
        .await,
        "the notice names the person and their id"
    );
    assert!(query(&mount, "[data-testid='co-host-suggest-note']").is_none());
    assert_eq!(
        text_of(&mount, "[data-testid='co-host-suggest-status']").as_deref(),
        Some(""),
        "the selection is no longer announced after Add"
    );

    html(&input).click();
    settle().await;
    assert!(!suggested_ids(&mount).is_empty(), "positive control");
    assert!(
        !suggested_ids(&mount).contains(&"ann.lee@example.com".to_string()),
        "a new co-host is no longer suggested"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_typed_name_only_fills_and_a_shared_or_partial_name_asks_for_a_pick() {
    let mount = render_active_section("{}").await;
    let input = focus_picker(&mount).await;
    let error_text = || {
        input
            .get_attribute("aria-describedby")
            .and_then(|id| gloo_utils::document().get_element_by_id(&id))
            .and_then(|e| e.text_content())
    };
    type_into(&input, "sam rivera").await;
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    assert_eq!(
        error_text().as_deref(),
        Some("More than one person has that name. Pick one from the list.")
    );
    assert_eq!(input.get_attribute("aria-invalid").as_deref(), Some("true"));
    assert!(input.class_list().contains("input-apple--invalid"));
    assert_eq!(
        suggested_ids(&mount),
        vec!["sam.r@example.com", "srivera@example.com"],
        "the list opens on the people who share the name"
    );
    click(&mount, "[data-user-id='sam.r@example.com'].co-hosts-option");
    settle().await;
    assert_eq!(error_text(), None, "a pick clears the error");
    assert!(!input.has_attribute("aria-invalid"));
    assert!(!input.class_list().contains("input-apple--invalid"));
    assert_eq!(
        text_of(&mount, "[data-testid='co-host-suggest-note']").as_deref(),
        Some("Sam Rivera (sam.r@example.com) selected. Choose Add to confirm.")
    );

    type_into(&input, "Ann").await;
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    assert_eq!(
        error_text().as_deref(),
        Some(
            "No one by that name. Pick from the list, or press Add again to add “Ann” as a user ID."
        )
    );
    sleep_ms(100).await;
    assert!(
        grant_bodies().is_empty(),
        "a partial name is never sent as an id"
    );
    type_into(&input, "alice").await;
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    assert_eq!(
        error_text().as_deref(),
        Some("A co-host, Alice (alice@example.com), already has that name. Press Add again to add “alice” as a user ID.")
    );
    sleep_ms(100).await;
    assert!(
        grant_bodies().is_empty(),
        "a co-host's name is never sent as an id"
    );

    type_into(&input, "Ann Lee").await;
    assert!(
        !key_down(&input, "Enter", true),
        "a held Enter never submits"
    );
    click(&mount, "[data-testid='co-host-add']");
    click(&mount, "[data-testid='co-host-add']");
    js_sys::eval("window.__realDateNow = Date.now; Date.now = function () { return 0; };").unwrap();
    click(&mount, "[data-testid='co-host-add']");
    js_sys::eval("Date.now = window.__realDateNow;").unwrap();
    settle().await;
    assert_eq!(input_value(&input), "ann.lee@example.com");
    assert_eq!(
        text_of(&mount, "[data-testid='co-host-suggest-note']").as_deref(),
        Some("Ann Lee (ann.lee@example.com) selected. Choose Add to confirm.")
    );
    sleep_ms(100).await;
    assert!(
        grant_bodies().is_empty(),
        "a typed name never grants on a double press, even if the wall clock steps back"
    );

    sleep_ms(500).await;
    click(&mount, "[data-testid='co-host-add']");
    wait_for_grants(1).await;
    assert_eq!(
        grant_bodies(),
        vec![r#"{"user_id":"ann.lee@example.com"}"#.to_string()]
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_short_id_inside_a_name_needs_a_deliberate_second_add() {
    let mount = render_active_section("{}").await;
    let input = focus_picker(&mount).await;
    type_into(&input, "ann").await;
    click(&mount, "[data-testid='co-host-add']");
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    assert_eq!(
        input
            .get_attribute("aria-describedby")
            .and_then(|id| gloo_utils::document().get_element_by_id(&id))
            .and_then(|e| e.text_content())
            .as_deref(),
        Some(
            "No one by that name. Pick from the list, or press Add again to add “ann” as a user ID."
        )
    );
    sleep_ms(100).await;
    assert!(grant_bodies().is_empty(), "a double click never confirms");

    sleep_ms(500).await;
    type_into(&input, "ann").await;
    click(&mount, "[data-testid='co-host-add']");
    sleep_ms(100).await;
    assert!(grant_bodies().is_empty(), "an edit starts over");

    sleep_ms(500).await;
    click(&mount, "[data-testid='co-host-add']");
    wait_for_grants(1).await;
    assert_eq!(
        grant_bodies(),
        vec![r#"{"user_id":"ann"}"#.to_string()],
        "a deliberate second Add grants exactly what was typed"
    );
    cleanup(&mount);
    restore_fetch();
}

fn add_error(input: &web_sys::Element) -> Option<web_sys::Element> {
    input
        .get_attribute("aria-describedby")
        .and_then(|id| gloo_utils::document().get_element_by_id(&id))
}

#[wasm_bindgen_test]
async fn a_repeated_add_error_is_announced_again() {
    let mount = render_active_section("{}").await;
    let input = focus_picker(&mount).await;
    type_into(&input, "Bob Smith").await;
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    let first = add_error(&input).expect("the error");
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    let again = add_error(&input).expect("the error again");
    assert_eq!(again.text_content(), first.text_content());
    assert!(
        !again.is_same_node(Some(&first)),
        "a fresh alert re-announces the same text"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_plain_id_that_is_a_co_hosts_name_can_be_confirmed() {
    let mount = render_active_section("{}").await;
    let input = focus_picker(&mount).await;
    type_into(&input, "alice").await;
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    assert!(add_error(&input).is_some());
    sleep_ms(600).await;
    click(&mount, "[data-testid='co-host-add']");
    wait_for_grants(1).await;
    assert_eq!(grant_bodies(), vec![r#"{"user_id":"alice"}"#.to_string()]);
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_burst_of_adds_after_a_typed_name_grants_nothing() {
    let mount = render_active_section("{}").await;
    let input = focus_picker(&mount).await;
    type_into(&input, "Ann Lee").await;
    click(&mount, "[data-testid='co-host-add']");
    for _ in 0..3 {
        sleep_ms(200).await;
        click(&mount, "[data-testid='co-host-add']");
    }
    sleep_ms(100).await;
    assert!(grant_bodies().is_empty(), "each press restarts the wait");
    sleep_ms(600).await;
    click(&mount, "[data-testid='co-host-add']");
    wait_for_grants(1).await;
    assert_eq!(
        grant_bodies(),
        vec![r#"{"user_id":"ann.lee@example.com"}"#.to_string()]
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_long_unbroken_id_wraps_in_the_error_and_the_notice() {
    let long = "averyveryveryveryveryverylongaddresswithouthyphens@example.com";
    let doc = gloo_utils::document();
    let style = doc.create_element("style").unwrap();
    style.set_text_content(Some(&format!(
        "{}{}*{{transition:none !important}}",
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    )));
    doc.head().unwrap().append_child(&style).unwrap();
    let mount = render_section(
        Scenario {
            meeting_active: true,
            ..Default::default()
        },
        &format!(
            "{{ rows: [{{ user_id: '{long}', display_name: 'Long Name', persistent: true, \
             is_present_host: false, designated: true, suspended: false }}] }}"
        ),
    )
    .await;
    mount.set_attribute("style", "width:320px").unwrap();
    settle().await;
    let section = query(&mount, "[data-testid='co-hosts-section']").unwrap();
    let fits = || section.scroll_width() <= section.client_width();
    let input = query(&mount, "[data-testid='co-host-input']").unwrap();
    let fits_before = fits();
    type_into(&input, "Long Name").await;
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    let error = add_error(&input).and_then(|e| e.text_content());
    let error_fits = fits();
    type_into(&input, &format!("x{long}")).await;
    click(&mount, "[data-testid='co-host-add']");
    wait_for_grants(1).await;
    let notice = text_of(&mount, "[data-testid='co-hosts-status']");
    let notice_fits = fits();
    style.remove();
    cleanup(&mount);
    restore_fetch();

    assert!(fits_before, "positive control: the roster row wraps");
    assert_eq!(
        error,
        Some(format!("Long Name ({long}) is already a co-host."))
    );
    assert!(error_fits, "the error wraps instead of scrolling sideways");
    assert!(notice.is_some_and(|t| t.starts_with(&format!("x{long} was added"))));
    assert!(notice_fits, "so does the notice");
}

#[wasm_bindgen_test]
async fn a_failed_add_names_the_id_and_drops_the_selection_note() {
    let mount = render_active_section("{ grant: 'inactive' }").await;
    let input = focus_picker(&mount).await;
    type_into(&input, "Ann Lee").await;
    click(&mount, "[data-testid='co-host-add']");
    settle().await;
    assert!(query(&mount, "[data-testid='co-host-suggest-note']").is_some());
    sleep_ms(500).await;
    click(&mount, "[data-testid='co-host-add']");
    assert!(wait_for_selector(&mount, "[data-testid='co-hosts-error']", 3_000).await);
    assert!(
        text_of(&mount, "[data-testid='co-hosts-error']").is_some_and(
            |t| t.starts_with("Couldn't add Ann Lee (ann.lee@example.com) as a co-host.")
        )
    );
    assert_eq!(
        input_value(&input),
        "ann.lee@example.com",
        "the draft is kept"
    );
    assert!(query(&mount, "[data-testid='co-host-suggest-note']").is_none());
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_press_below_the_open_list_lands_before_the_list_closes() {
    let mount = render_active_section("{}").await;
    let input = focus_picker(&mount).await;
    html(&input).click();
    settle().await;
    let list = query(&mount, "[data-testid='co-host-suggestions']").unwrap();
    assert!(!list.has_attribute("hidden"));
    let add = query(&mount, "[data-testid='co-host-add']").unwrap();
    assert!(
        !fire(&add, "MouseEvent", "mousedown"),
        "pressing Add keeps focus in the field"
    );

    let hint = query(&mount, ".co-hosts-hint").unwrap();
    let top = hint.get_bounding_client_rect().top();
    fire(&hint, "PointerEvent", "pointerdown");
    html(&input).blur().unwrap();
    settle().await;
    assert!(
        !list.has_attribute("hidden"),
        "the list holds while the press is down"
    );
    assert_eq!(hint.get_bounding_client_rect().top(), top);
    fire(&hint, "MouseEvent", "click");
    settle().await;
    assert!(
        list.has_attribute("hidden"),
        "and closes once the click lands"
    );

    html(&input).focus().unwrap();
    html(&input).click();
    settle().await;
    assert!(!list.has_attribute("hidden"));
    html(&input).blur().unwrap();
    settle().await;
    assert!(
        list.has_attribute("hidden"),
        "a blur with no press down closes at once"
    );

    html(&input).focus().unwrap();
    html(&input).click();
    settle().await;
    assert!(!list.has_attribute("hidden"));
    fire(&input, "PointerEvent", "pointerdown");
    fire(&input, "MouseEvent", "contextmenu");
    html(&input).blur().unwrap();
    settle().await;
    assert!(
        list.has_attribute("hidden"),
        "a right-click ends the press like a click does"
    );

    html(&input).focus().unwrap();
    html(&input).click();
    settle().await;
    assert!(!list.has_attribute("hidden"));
    fire(&input, "PointerEvent", "pointerdown");
    fire(&input, "MouseEvent", "auxclick");
    html(&input).blur().unwrap();
    settle().await;
    assert!(
        list.has_attribute("hidden"),
        "so does a middle or back-button press"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn the_list_scrolls_into_view_when_it_opens() {
    let mount = render_active_section("{}").await;
    let input = focus_picker(&mount).await;
    let window = gloo_utils::window();
    let viewport = window.inner_height().unwrap().as_f64().unwrap();
    mount
        .set_attribute("style", &format!("padding-top:{viewport}px"))
        .unwrap();
    settle().await;
    let input_bottom = input.get_bounding_client_rect().bottom() + window.scroll_y().unwrap();
    window.scroll_to_with_x_and_y(0.0, input_bottom - viewport);
    settle().await;
    let before = window.scroll_y().unwrap();
    html(&input).click();
    settle().await;
    sleep_ms(50).await;
    let list = query(&mount, "[data-testid='co-host-suggestions']").unwrap();
    let list_bottom = list.get_bounding_client_rect().bottom();
    let after = window.scroll_y().unwrap();
    window.scroll_to_with_x_and_y(0.0, 0.0);
    cleanup(&mount);
    restore_fetch();
    assert!(!list.has_attribute("hidden"));
    assert!(
        after > before,
        "the page scrolls to the list: {before} -> {after}"
    );
    assert!(
        list_bottom <= viewport + 1.0,
        "the list ends on screen: {list_bottom} of {viewport}"
    );
}

#[wasm_bindgen_test]
async fn a_removed_co_host_returns_to_the_top_and_a_click_only_fills() {
    let mount = render_active_section("{ revoke: 'ok' }").await;
    let input = focus_picker(&mount).await;
    html(&input).click();
    settle().await;
    assert!(
        !suggested_ids(&mount).contains(&ALICE.to_string()),
        "a current co-host is not suggested"
    );

    click(&row(&mount, ALICE), "[data-testid='co-host-remove']");
    assert!(
        wait_until(|| query(&mount, "[data-user-id='alice@example.com'].co-hosts-row").is_none())
            .await
    );
    html(&input).focus().unwrap();
    html(&input).click();
    settle().await;
    assert_eq!(
        suggested_ids(&mount).first().map(String::as_str),
        Some(ALICE)
    );

    let alice = query(&mount, "[data-testid='co-host-suggestion']").unwrap();
    assert!(
        !fire(&alice, "MouseEvent", "mousedown"),
        "pressing an option must not blur the field"
    );
    html(&alice).click();
    settle().await;
    assert_eq!(input_value(&input), ALICE);
    assert!(query(&mount, "[data-testid='co-host-suggestions']")
        .unwrap()
        .has_attribute("hidden"));
    sleep_ms(100).await;
    assert!(grant_bodies().is_empty(), "a click pick never grants");
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn escape_closes_the_suggestions_but_not_the_meeting_options_dialog() {
    let mount = render_as(OWNER, PanelParent).await;
    track_picker_listeners();
    click(&mount, "[data-testid='opener']");
    settle().await;
    assert!(wait_for_selector(&mount, "[data-testid='co-host-row']", 3_000).await);
    assert_eq!(picker_listeners(), 5, "the open picker listens for presses");
    let input = focus_picker(&mount).await;
    html(&input).click();
    settle().await;
    let list = query(&mount, "[data-testid='co-host-suggestions']").unwrap();
    assert!(!list.has_attribute("hidden"), "a click opens the list");

    assert!(!press_key(&input, "Escape"));
    settle().await;
    assert!(list.has_attribute("hidden"), "Escape closes the list");
    assert!(
        query(&mount, "[data-testid='meeting-options-panel']").is_some(),
        "and only the list"
    );

    type_into(&input, "zzz").await;
    assert!(query(&mount, "[data-testid='co-host-suggest-note']").is_some());
    assert!(list.has_attribute("hidden"));
    assert!(press_key(&input, "Escape"));
    settle().await;
    let left = untrack_picker_listeners();
    assert!(
        query(&mount, "[data-testid='meeting-options-panel']").is_none(),
        "with the list closed, Escape closes the dialog as before"
    );
    assert_eq!(left, 0, "closing the dialog removes the picker's listeners");
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_failed_suggestion_fetch_leaves_free_text_add_working() {
    let mount = render_active_section("{ suggest: 'fail' }").await;
    let input = query(&mount, "[data-testid='co-host-input']").unwrap();
    html(&input).focus().unwrap();
    assert!(wait_until(|| suggestion_reads() == 1).await);
    sleep_ms(100).await;
    html(&input).click();
    type_into(&input, "new@example.com").await;
    assert!(query(&mount, "[data-testid='co-host-suggestions']")
        .unwrap()
        .has_attribute("hidden"));
    assert!(query(&mount, "[data-testid='co-host-suggest-note']").is_none());
    assert_eq!(
        input.get_attribute("placeholder").as_deref(),
        Some("Email or user ID")
    );
    click(&mount, "[data-testid='co-host-add']");
    wait_for_grants(1).await;
    assert_eq!(
        grant_bodies(),
        vec![r#"{"user_id":"new@example.com"}"#.to_string()]
    );

    html(&input).blur().unwrap();
    html(&input).focus().unwrap();
    assert!(
        wait_until(|| suggestion_reads() == 2).await,
        "one retry on the next focus"
    );
    sleep_ms(100).await;
    html(&input).blur().unwrap();
    html(&input).focus().unwrap();
    sleep_ms(200).await;
    assert_eq!(suggestion_reads(), 2, "and no more");
    cleanup(&mount);
    restore_fetch();
}

struct ErrorLook {
    text: Option<String>,
    color: String,
    size: String,
    hint: String,
    border: String,
    red: String,
}

async fn unknown_name_error_look(mount: &web_sys::Element) -> Option<ErrorLook> {
    if let Some(summary) = query(mount, ".co-hosts-summary") {
        html(&summary).click();
        settle().await;
    }
    let input = query(mount, "[data-testid='co-host-input']")?;
    type_into(&input, "Nobody Here").await;
    click(mount, "[data-testid='co-host-add']");
    settle().await;
    let doc = gloo_utils::document();
    let error = input
        .get_attribute("aria-describedby")
        .and_then(|id| doc.get_element_by_id(&id))?;
    let probe = doc.create_element("span").ok()?;
    probe.set_attribute("style", "color: var(--error)").ok()?;
    mount.append_child(&probe).ok()?;
    Some(ErrorLook {
        text: error.text_content(),
        color: computed(&error, "color"),
        size: computed(&error, "font-size"),
        hint: computed(&query(mount, ".co-hosts-hint")?, "color"),
        border: computed(&input, "border-top-color"),
        red: computed(&probe, "color"),
    })
}

#[wasm_bindgen_test]
async fn co_host_form_errors_read_red_on_pre_join_and_elsewhere() {
    let doc = gloo_utils::document();
    let style = doc.create_element("style").unwrap();
    style.set_text_content(Some(&format!(
        "{}{}*{{transition:none !important}}",
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    )));
    doc.head().unwrap().append_child(&style).unwrap();
    let mount = render_as(OWNER, HeroPreJoinParent).await;
    let loaded = wait_for_selector(&mount, "[data-testid='co-host-row']", 3_000).await;
    let pre_join = unknown_name_error_look(&mount).await;
    cleanup(&mount);
    restore_fetch();
    let mount = render_active_section("{}").await;
    let elsewhere = unknown_name_error_look(&mount).await;
    cleanup(&mount);
    restore_fetch();
    style.remove();

    assert!(loaded, "positive control: the pre-join list loaded");
    let pre_join = pre_join.expect("the pre-join card shows the error");
    let elsewhere = elsewhere.expect("the section shows the error");
    for (place, look) in [("pre-join", &pre_join), ("elsewhere", &elsewhere)] {
        assert_eq!(
            look.text.as_deref(),
            Some("No one by that name. Pick from the list, or enter their email or user ID."),
            "{place}"
        );
        assert_eq!(look.color, look.red, "{place}: the error is red");
        assert_ne!(look.hint, look.red, "{place}: unlike the hint");
        assert_eq!(look.border, look.red, "{place}: the field is flagged too");
    }
    assert_eq!(
        pre_join.size, elsewhere.size,
        "the error is one size everywhere"
    );
}

const HOST_TOAST: &str = "[data-testid='host-change-toast']";
const HOST_STATUS: &str = "[data-testid='host-change-status']";

#[derive(Clone, Copy)]
struct HostChangeHandles {
    toast: Signal<Option<HostChangeToast>>,
    hosts: Signal<HashSet<String>>,
    timer: Signal<Option<gloo_timers::callback::Timeout>>,
}

thread_local! {
    static HOST_CHANGE: RefCell<Option<HostChangeHandles>> = const { RefCell::new(None) };
}

/// The local user is COHOST; the owner and COHOST both hold the role.
#[allow(non_snake_case)]
fn HostChangeParent() -> Element {
    let toast = use_signal(|| None::<HostChangeToast>);
    let hosts = use_signal(|| HashSet::from([OWNER.to_string(), COHOST.to_string()]));
    let timer = use_signal(|| None::<gloo_timers::callback::Timeout>);
    use_context_provider(|| HostSetCtx(hosts));
    use_hook(move || {
        HOST_CHANGE.with(|h| {
            *h.borrow_mut() = Some(HostChangeHandles {
                toast,
                hosts,
                timer,
            })
        })
    });
    rsx! {
        HostChangeNotice {
            toast,
            owner_user_id: Some(OWNER.to_string()),
            local_user_id: COHOST.to_string(),
        }
    }
}

async fn render_host_change() -> (web_sys::Element, HostChangeHandles) {
    reset_test_browser_state();
    let mount = create_mount_point();
    render_into(&mount, HostChangeParent);
    settle().await;
    let handles = HOST_CHANGE.with(|h| h.borrow().expect("host change handles"));
    (mount, handles)
}

fn granted(seq: u64) -> Option<HostChangeToast> {
    Some(HostChangeToast {
        seq,
        change: HostChange::Granted,
    })
}

#[wasm_bindgen_test]
async fn the_host_change_toast_is_announced_by_a_persistent_region() {
    let (mount, h) = render_host_change().await;
    let mut toast = h.toast;
    let region = query(&mount, HOST_STATUS).expect("mounted before any host change");
    assert_eq!(region.get_attribute("role").as_deref(), Some("status"));
    assert_eq!(region.text_content().as_deref(), Some(""));

    toast.set(granted(1));
    settle().await;
    let first = region.text_content().unwrap_or_default();
    assert!(first.starts_with("You're now a co-host"), "{first}");
    assert!(query(&mount, HOST_TOAST).is_some());
    assert!(query(&mount, "[data-testid='host-change-toast'][role]").is_none());

    toast.set(None);
    settle().await;
    toast.set(granted(2));
    settle().await;
    let same = query(&mount, HOST_STATUS).unwrap();
    assert!(same.is_same_node(Some(&region)));
    assert_ne!(region.text_content().unwrap_or_default(), first);
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_granted_toast_follows_the_live_role_through_a_transfer() {
    let (mount, h) = render_host_change().await;
    let (mut toast, mut hosts) = (h.toast, h.hosts);
    toast.set(granted(1));
    settle().await;
    assert_eq!(
        text_of(&mount, HOST_TOAST).as_deref(),
        Some("You're now a co-host")
    );
    let status = text_of(&mount, HOST_STATUS).unwrap_or_default();
    assert!(status.starts_with("You're now a co-host"), "{status}");

    hosts.write().remove(OWNER);
    settle().await;
    assert_eq!(
        text_of(&mount, HOST_TOAST).as_deref(),
        Some("You're now a host")
    );
    let status = text_of(&mount, HOST_STATUS).unwrap_or_default();
    assert!(status.starts_with("You're now a host"), "{status}");
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn an_unrelated_host_set_change_does_not_re_announce_the_toast() {
    let (mount, h) = render_host_change().await;
    let (mut toast, mut hosts) = (h.toast, h.hosts);
    toast.set(granted(1));
    settle().await;
    let before = text_of(&mount, HOST_STATUS).unwrap_or_default();
    assert!(before.starts_with("You're now a co-host"), "{before}");

    hosts.write().insert(ALICE.to_string());
    settle().await;
    assert_eq!(
        text_of(&mount, HOST_TOAST).as_deref(),
        Some("You're now a co-host")
    );
    assert_eq!(text_of(&mount, HOST_STATUS).unwrap_or_default(), before);
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn losing_the_role_under_a_grant_toast_withdraws_it() {
    let (mount, h) = render_host_change().await;
    let (mut toast, mut hosts) = (h.toast, h.hosts);
    let region = query(&mount, HOST_STATUS).expect("status region");
    toast.set(granted(1));
    settle().await;
    assert_eq!(
        text_of(&mount, HOST_TOAST).as_deref(),
        Some("You're now a co-host")
    );

    hosts.write().remove(COHOST);
    settle().await;
    assert!(
        query(&mount, HOST_TOAST).is_none(),
        "{:?}",
        text_of(&mount, HOST_TOAST)
    );
    let status = region.text_content().unwrap_or_default();
    assert!(!status.contains("host"), "{status}");
    assert!(query(&mount, HOST_STATUS)
        .unwrap()
        .is_same_node(Some(&region)));
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_self_host_change_toast_waits_out_the_transfer() {
    let (mount, h) = render_host_change().await;
    let mut hosts = h.hosts;
    show_host_change_toast(HostChange::Granted, h.toast, h.timer);
    yield_now().await;
    assert!(
        query(&mount, HOST_TOAST).is_none(),
        "shown only after the defer window"
    );
    hosts.write().remove(OWNER);
    sleep_ms(600).await;
    settle().await;
    assert_eq!(
        text_of(&mount, HOST_TOAST).as_deref(),
        Some("You're now a host")
    );

    show_host_change_toast(HostChange::Revoked, h.toast, h.timer);
    sleep_ms(600).await;
    settle().await;
    assert_eq!(
        text_of(&mount, HOST_TOAST).as_deref(),
        Some("You no longer have host controls")
    );
    show_host_change_toast(HostChange::Revoked, h.toast, h.timer);
    let repeated = text_of(&mount, HOST_STATUS).unwrap_or_default();
    sleep_ms(600).await;
    settle().await;
    assert_ne!(
        text_of(&mount, HOST_STATUS).unwrap_or_default(),
        repeated,
        "a fresh toast with the same text is announced again"
    );
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_self_host_change_toast_clears_after_its_visible_window() {
    let (mount, h) = render_host_change().await;
    show_host_change_toast(HostChange::Granted, h.toast, h.timer);
    sleep_ms(600).await;
    settle().await;
    assert!(query(&mount, HOST_TOAST).is_some(), "shown after the delay");
    sleep_ms(4_400).await;
    settle().await;
    assert!(query(&mount, HOST_TOAST).is_some(), "still shown near 5 s");
    sleep_ms(1_800).await;
    settle().await;
    assert!(query(&mount, HOST_TOAST).is_none(), "dismissed");
    assert_eq!(text_of(&mount, HOST_STATUS).as_deref(), Some(""));
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_replacement_host_change_toast_is_a_new_node() {
    let (mount, h) = render_host_change().await;
    let mut toast = h.toast;
    toast.set(granted(1));
    settle().await;
    let first = query(&mount, HOST_TOAST).expect("first toast");
    toast.set(granted(2));
    settle().await;
    let second = query(&mount, HOST_TOAST).expect("replacement toast");
    assert_eq!(second.text_content(), first.text_content());
    assert!(!second.is_same_node(Some(&first)));
    cleanup(&mount);
}

fn pointer_down(target: &web_sys::Element) {
    let init = web_sys::EventInit::new();
    init.set_bubbles(true);
    let event = web_sys::Event::new_with_event_init_dict("pointerdown", &init).unwrap();
    target.dispatch_event(&event).unwrap();
}

#[wasm_bindgen_test]
async fn a_pointerdown_outside_the_tile_menu_closes_it() {
    let mount = open_tile(Scenario {
        viewer: OWNER.into(),
        viewer_is_host: true,
        hosts: vec![OWNER.into()],
        peer: "carol@example.com".into(),
        ..Default::default()
    })
    .await;
    let is_open = |m: &web_sys::Element| query(m, ".tile-context-menu").is_some();
    for inside in [
        ".tile-context-menu-item",
        ".tile-mute-btn",
        ".tile-menu-backdrop",
    ] {
        pointer_down(&query(&mount, inside).unwrap());
        settle().await;
        assert!(
            is_open(&mount),
            "a pointerdown on {inside} keeps the menu open"
        );
    }

    assert_eq!(
        live_tile_menu_dismiss_listeners_for_test(),
        1,
        "one listener while open"
    );

    let outside = gloo_utils::document().create_element("div").unwrap();
    let stop = Closure::<dyn FnMut(web_sys::Event)>::new(|e: web_sys::Event| e.stop_propagation());
    outside
        .add_event_listener_with_callback("pointerdown", stop.as_ref().unchecked_ref())
        .unwrap();
    gloo_utils::document()
        .body()
        .unwrap()
        .append_child(&outside)
        .unwrap();
    pointer_down(&outside);
    settle().await;
    outside.remove();
    assert!(
        !is_open(&mount),
        "a pointerdown outside closes the menu even when its target stops propagation"
    );
    assert_eq!(
        query(&mount, ".tile-mute-btn")
            .unwrap()
            .get_attribute("aria-expanded")
            .as_deref(),
        Some("false")
    );
    assert_eq!(
        live_tile_menu_dismiss_listeners_for_test(),
        0,
        "removed on close"
    );

    let errors = Rc::new(RefCell::new(Vec::<String>::new()));
    let sink = errors.clone();
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
        e.prevent_default();
        let message = js_sys::Reflect::get(&e, &"message".into()).ok();
        sink.borrow_mut()
            .push(message.and_then(|m| m.as_string()).unwrap_or_default());
    });
    let window = gloo_utils::window();
    window
        .add_event_listener_with_callback("error", on_error.as_ref().unchecked_ref())
        .unwrap();
    let later = gloo_utils::document().create_element("div").unwrap();
    gloo_utils::document()
        .body()
        .unwrap()
        .append_child(&later)
        .unwrap();
    pointer_down(&later);
    settle().await;
    later.remove();
    window
        .remove_event_listener_with_callback("error", on_error.as_ref().unchecked_ref())
        .unwrap();
    assert!(
        errors.borrow().is_empty(),
        "a pointerdown after close reached the dropped listener: {:?}",
        errors.borrow()
    );

    click(&mount, ".tile-mute-btn");
    settle().await;
    assert!(is_open(&mount), "the menu reopens");
    assert_eq!(
        live_tile_menu_dismiss_listeners_for_test(),
        1,
        "one listener after reopening"
    );
    cleanup(&mount);
}
