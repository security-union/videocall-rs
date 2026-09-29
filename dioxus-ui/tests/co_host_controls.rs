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

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::PinnedTile;
use dioxus_ui::components::co_hosts::{
    post_co_host_notice, run_co_host_request, CoHostMenuAction, CoHostNotice, CoHostNoticeCtx,
    CoHostNoticeLayer, CoHostRequest, CoHostsSection, HostChangeNotice, MeetingOwnership,
};
use dioxus_ui::components::meeting_options_controls::MeetingOptionsPanel;
use dioxus_ui::components::peer_list_item::PeerListItem;
use dioxus_ui::components::peer_tile::PeerTile;
use dioxus_ui::components::pre_join_settings_card::PreJoinSettingsCard;
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, HostSetCtx, MeetingTime, PeerAudioLivenessMap,
    PeerSignalHistoryMap, SignalPopupStateMap,
};
use support::{
    cleanup, create_mount_point, inject_app_config, render_into, reset_test_browser_state,
    restore_fetch, wait_for_selector, yield_now,
};
use videocall_client::VideoCallClient;
use videocall_diagnostics::{global_sender, metric, DiagEvent};
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
        window.__coHostGrants = [];
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
                    window.__coHostGrants.push(JSON.parse(text));
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
    let client = use_hook(|| VideoCallClient::new_for_test(&s.viewer));
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
    // No config: a menu action's API call fails fast instead of reaching out.
    reset_test_browser_state();
    set_scenario(s);
    let mount = create_mount_point();
    render_into(&mount, TileParent);
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
    let peer = s.peer.clone();
    let mount = mount_tile(s).await;
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
    settle().await;
    mount
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

#[allow(non_snake_case)]
fn HostChangeParent() -> Element {
    let mut toast = use_signal(|| None::<String>);
    rsx! {
        button {
            "data-testid": "grant",
            onclick: move |_| toast.set(Some("You now have host controls".to_string())),
            "grant"
        }
        button { "data-testid": "clear", onclick: move |_| toast.set(None), "clear" }
        HostChangeNotice { toast }
    }
}

#[wasm_bindgen_test]
async fn the_host_change_toast_is_announced_by_a_persistent_region() {
    reset_test_browser_state();
    let mount = create_mount_point();
    render_into(&mount, HostChangeParent);
    settle().await;
    let region = query(&mount, "[data-testid='host-change-status']")
        .expect("mounted before any host change");
    assert_eq!(region.get_attribute("role").as_deref(), Some("status"));
    assert_eq!(region.text_content().as_deref(), Some(""));

    click(&mount, "[data-testid='grant']");
    settle().await;
    let first = region.text_content().unwrap_or_default();
    assert!(first.starts_with("You now have host controls"), "{first}");
    assert!(query(&mount, "[data-testid='host-change-toast']").is_some());
    assert!(query(&mount, "[data-testid='host-change-toast'][role]").is_none());

    click(&mount, "[data-testid='clear']");
    settle().await;
    click(&mount, "[data-testid='grant']");
    settle().await;
    let same = query(&mount, "[data-testid='host-change-status']").unwrap();
    assert!(same.is_same_node(Some(&region)));
    assert_ne!(region.text_content().unwrap_or_default(), first);
    cleanup(&mount);
}
