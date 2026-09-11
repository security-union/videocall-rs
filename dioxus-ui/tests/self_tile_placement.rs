// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 66 / issue 2693. `Host` and the attendants tree are not mountable here
// (see `handler_cell_teardown.rs`), so the DOM cases mount the Preferences panel
// and the self-view hidden pill directly, each with seeded contexts.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use dioxus::prelude::*;
use dioxus_ui::components::preferences_settings_panel::PreferencesSettingsPanel;
use dioxus_ui::components::self_view_hidden_pill::SelfViewHiddenPill;
use dioxus_ui::context::{
    load_self_view_placement, load_self_view_visible, save_self_view_visible, AppearanceSettings,
    AppearanceSettingsCtx, DockPosition, DockPositionCtx, SelfViewPlacement, SelfViewPlacementCtx,
    SelfViewVisibleCtx,
};
use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const CORNER_RADIO: &str = "[data-testid='self-view-placement-corner']";
const GRID_RADIO: &str = "[data-testid='self-view-placement-grid']";
const VISIBLE_BOX: &str = "[data-testid='self-view-visible-checkbox']";
const ICON_CLASS: &str = ".self-view-hidden-icon";
// The production selector, so a testid rename in the component fails these
// cases instead of leaving the toast's focus handoff quietly pointing at air.
use dioxus_ui::components::self_view::SELF_VIEW_SHOW_BUTTON_SELECTOR as SHOW_BUTTON;
const PROBE: &str = "[data-testid='pill-probe']";
const SCOPE_SENTENCE: &str = "Only affects your view — others still see you.";

thread_local! {
    static SEED_PLACEMENT: std::cell::RefCell<SelfViewPlacement> =
        const { std::cell::RefCell::new(SelfViewPlacement::Corner) };
    static SEED_VISIBLE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static SEED_CAN_STREAM: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static SEED_DOCK: std::cell::Cell<DockPosition> =
        const { std::cell::Cell::new(DockPosition::Bottom) };
    static SEED_TOAST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SEED_SETTINGS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ON_SHOW_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn clear_storage() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item("vc_self_view_placement");
        let _ = storage.remove_item("vc_self_view_visible");
    }
}

#[allow(non_snake_case)]
fn SeededPrefsPanel() -> Element {
    let placement = use_signal(|| SEED_PLACEMENT.with(|p| *p.borrow()));
    let visible = use_signal(|| SEED_VISIBLE.with(|v| v.get()));
    use_context_provider(|| SelfViewPlacementCtx(placement));
    use_context_provider(|| SelfViewVisibleCtx(visible));
    let appearance = use_signal(AppearanceSettings::default);
    use_context_provider(|| AppearanceSettingsCtx(appearance));
    rsx! { PreferencesSettingsPanel {} }
}

fn checked_state(mount: &web_sys::Element, selector: &str) -> Option<String> {
    mount
        .query_selector(selector)
        .ok()
        .flatten()
        .and_then(|el| el.get_attribute("aria-checked"))
}

#[wasm_bindgen_test]
async fn preferences_shows_corner_selected_by_default() {
    clear_storage();
    SEED_PLACEMENT.with(|p| *p.borrow_mut() = SelfViewPlacement::Corner);
    SEED_VISIBLE.with(|v| v.set(true));

    let mount = create_mount_point();
    render_into(&mount, SeededPrefsPanel);
    yield_now().await;

    // Positive control before the negative: the section rendered at all.
    assert!(
        mount.query_selector(CORNER_RADIO).unwrap().is_some(),
        "the Self view section must render its Corner option"
    );
    assert_eq!(checked_state(&mount, CORNER_RADIO).as_deref(), Some("true"));
    assert_eq!(checked_state(&mount, GRID_RADIO).as_deref(), Some("false"));

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn preferences_reflects_a_seeded_grid_preference() {
    clear_storage();
    SEED_PLACEMENT.with(|p| *p.borrow_mut() = SelfViewPlacement::Grid);
    SEED_VISIBLE.with(|v| v.set(true));

    let mount = create_mount_point();
    render_into(&mount, SeededPrefsPanel);
    yield_now().await;

    assert_eq!(checked_state(&mount, GRID_RADIO).as_deref(), Some("true"));
    assert_eq!(
        checked_state(&mount, CORNER_RADIO).as_deref(),
        Some("false")
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn choosing_grid_persists_the_preference() {
    clear_storage();
    SEED_PLACEMENT.with(|p| *p.borrow_mut() = SelfViewPlacement::Corner);
    SEED_VISIBLE.with(|v| v.set(true));

    let mount = create_mount_point();
    render_into(&mount, SeededPrefsPanel);
    yield_now().await;

    assert_eq!(
        load_self_view_placement(),
        SelfViewPlacement::Corner,
        "nothing is stored before the click"
    );

    let grid = mount
        .query_selector(GRID_RADIO)
        .unwrap()
        .expect("Grid option must be present");
    grid.dyn_ref::<web_sys::HtmlElement>().unwrap().click();
    yield_now().await;

    assert_eq!(load_self_view_placement(), SelfViewPlacement::Grid);
    assert_eq!(checked_state(&mount, GRID_RADIO).as_deref(), Some("true"));

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn the_visibility_checkbox_reflects_and_persists() {
    clear_storage();
    SEED_PLACEMENT.with(|p| *p.borrow_mut() = SelfViewPlacement::Corner);
    SEED_VISIBLE.with(|v| v.set(true));

    let mount = create_mount_point();
    render_into(&mount, SeededPrefsPanel);
    yield_now().await;

    let checkbox = mount
        .query_selector(VISIBLE_BOX)
        .unwrap()
        .expect("Show self view checkbox must be present")
        .dyn_into::<web_sys::HtmlInputElement>()
        .unwrap();
    assert!(checkbox.checked(), "self view is visible by default");
    assert!(load_self_view_visible());

    checkbox.click();
    yield_now().await;

    assert!(
        !load_self_view_visible(),
        "unchecking must persist the hidden preference"
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn a_hidden_seeded_self_view_shows_the_checkbox_unchecked() {
    clear_storage();
    SEED_PLACEMENT.with(|p| *p.borrow_mut() = SelfViewPlacement::Corner);
    SEED_VISIBLE.with(|v| v.set(false));

    let mount = create_mount_point();
    render_into(&mount, SeededPrefsPanel);
    yield_now().await;

    let checkbox = mount
        .query_selector(VISIBLE_BOX)
        .unwrap()
        .expect("Show self view checkbox must be present")
        .dyn_into::<web_sys::HtmlInputElement>()
        .unwrap();
    assert!(
        !checkbox.checked(),
        "the Preferences switch must show the hidden state"
    );

    cleanup(&mount);
    clear_storage();
}

// Issue 2693: the persistent hidden-self-view icon. The probe node mirrors
// `SelfViewVisibleCtx` so the context flip is observable in the DOM.
#[allow(non_snake_case)]
fn SeededPill() -> Element {
    let visible = use_signal(|| SEED_VISIBLE.with(|v| v.get()));
    use_context_provider(|| SelfViewVisibleCtx(visible));
    let dock = use_signal(|| SEED_DOCK.with(|d| d.get()));
    use_context_provider(|| DockPositionCtx(dock));
    let can_stream = SEED_CAN_STREAM.with(|c| c.get());
    let mut settings = use_signal(|| SEED_SETTINGS.with(|s| s.get()));
    rsx! {
        div {
            "data-testid": "pill-probe",
            "data-visible": if visible() { "true" } else { "false" },
            // Clicking the probe closes the seeded settings overlay.
            onclick: move |_| settings.set(false),
        }
        SelfViewHiddenPill {
            can_stream,
            toast_present: SEED_TOAST.with(|t| t.get()),
            settings_open: settings(),
            on_show: move |_| ON_SHOW_CALLS.with(|c| c.set(c.get() + 1)),
        }
    }
}

fn seed_pill(visible: bool, can_stream: bool, dock: DockPosition) {
    clear_storage();
    SEED_VISIBLE.with(|v| v.set(visible));
    SEED_CAN_STREAM.with(|c| c.set(can_stream));
    SEED_DOCK.with(|d| d.set(dock));
    SEED_TOAST.with(|t| t.set(false));
    SEED_SETTINGS.with(|s| s.set(false));
    ON_SHOW_CALLS.with(|c| c.set(0));
}

fn attr(mount: &web_sys::Element, selector: &str, name: &str) -> Option<String> {
    mount
        .query_selector(selector)
        .ok()
        .flatten()
        .and_then(|el| el.get_attribute(name))
}

#[wasm_bindgen_test]
async fn a_hidden_self_view_renders_one_action_bar_button() {
    seed_pill(false, true, DockPosition::Bottom);

    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    assert!(
        mount.query_selector(PROBE).unwrap().is_some(),
        "positive control: the seeded tree rendered at all"
    );

    assert_eq!(
        mount.query_selector_all(ICON_CLASS).unwrap().length(),
        1,
        "the corner control is ONE element; a wrapper would land in \
         #grid-container, which the screen-share specs index positionally"
    );

    let button = mount
        .query_selector(SHOW_BUTTON)
        .unwrap()
        .expect("a hidden self view must render the corner Show control");
    assert_eq!(button.tag_name(), "BUTTON");
    assert_eq!(button.get_attribute("type").as_deref(), Some("button"));
    assert!(
        button.class_list().contains("video-control-button"),
        "the corner control borrows the action bar's button styling"
    );
    assert!(button.class_list().contains("self-view-hidden-icon"));
    assert!(
        !button
            .class_list()
            .contains("self-view-hidden-icon--dock-right"),
        "a bottom dock leaves the icon in the vacated corner"
    );
    assert_eq!(
        button.get_attribute("role"),
        None,
        "a control, not a status: the toast is what announces the hide"
    );
    assert_eq!(button.get_attribute("aria-live"), None);

    assert_eq!(
        button.get_attribute("aria-label").as_deref(),
        Some("Show self view"),
        "the accessible name must match the Preferences switch exactly"
    );
    assert_eq!(
        button.get_attribute("aria-describedby").as_deref(),
        Some("self-view-hidden-hint"),
        "landing on the icon by Tab must read out the scope"
    );
    let hint = mount
        .query_selector("#self-view-hidden-hint")
        .unwrap()
        .expect("the describedby id must resolve to a node");
    assert_eq!(
        hint.text_content().as_deref(),
        Some(SCOPE_SENTENCE),
        "the scope sentence must match the toast and the Preferences switch"
    );
    assert!(
        hint.class_list().contains("visually-hidden"),
        "the hint is for AT only; the icon has no room for it"
    );

    let title = mount
        .query_selector(".self-view-hidden-icon .tooltip .tooltip-title")
        .unwrap()
        .expect("hovering the icon must say what pressing it does");
    assert_eq!(title.text_content().as_deref(), Some("Show self view"));
    let desc = mount
        .query_selector(".self-view-hidden-icon .tooltip .tooltip-desc")
        .unwrap()
        .expect("the hover text carries the reassurance too");
    assert_eq!(desc.text_content().as_deref(), Some(SCOPE_SENTENCE));

    assert_eq!(
        attr(&mount, ".self-view-hidden-icon svg", "aria-hidden").as_deref(),
        Some("true"),
        "the glyph must not be announced"
    );
    assert_eq!(
        attr(&mount, ".self-view-hidden-icon .tooltip", "aria-hidden").as_deref(),
        Some("true"),
        "the description AT reads is the hint span; the tooltip would double it"
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn arriving_with_no_toast_reveals_the_tooltip_by_attribute() {
    seed_pill(false, true, DockPosition::Bottom);

    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    assert_eq!(
        attr(&mount, SHOW_BUTTON, "data-tooltip-open").as_deref(),
        Some("true"),
        "the icon must explain itself on arrival when no toast does it first"
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn arriving_while_the_toast_is_up_stays_quiet() {
    seed_pill(false, true, DockPosition::Bottom);
    SEED_TOAST.with(|t| t.set(true));

    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    assert!(
        mount.query_selector(SHOW_BUTTON).unwrap().is_some(),
        "positive control: the icon is on screen"
    );
    assert_eq!(
        attr(&mount, SHOW_BUTTON, "data-tooltip-open").as_deref(),
        Some("false"),
        "the toast already carries the explanation; two at once is noise"
    );

    cleanup(&mount);
    clear_storage();
}

// A Preferences hide leaves the settings overlay (fixed, z 9500) covering the
// corner, so an immediate reveal would burn its whole window unseen.
#[wasm_bindgen_test]
async fn a_reveal_behind_the_settings_overlay_waits_for_it_to_close() {
    seed_pill(false, true, DockPosition::Bottom);
    SEED_SETTINGS.with(|s| s.set(true));

    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    assert!(
        mount.query_selector(SHOW_BUTTON).unwrap().is_some(),
        "positive control: the icon is on screen behind the overlay"
    );
    assert_eq!(
        attr(&mount, SHOW_BUTTON, "data-tooltip-open").as_deref(),
        Some("false"),
        "the overlay covers the corner, so the reveal must wait"
    );

    mount
        .query_selector(PROBE)
        .unwrap()
        .unwrap()
        .dyn_ref::<web_sys::HtmlElement>()
        .unwrap()
        .click();
    yield_now().await;
    yield_now().await;

    assert_eq!(
        attr(&mount, SHOW_BUTTON, "data-tooltip-open").as_deref(),
        Some("true"),
        "closing settings is the first wake the corner is visible on"
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn the_reveal_attribute_actually_shows_the_tooltip() {
    seed_pill(false, true, DockPosition::Bottom);
    let style = install_stylesheets();
    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    let win = web_sys::window().unwrap();
    let tooltip = mount
        .query_selector(".self-view-hidden-icon .tooltip")
        .unwrap()
        .expect("positive control: the tooltip node exists");
    let computed = win.get_computed_style(&tooltip).unwrap().unwrap();
    assert_eq!(
        attr(&mount, SHOW_BUTTON, "data-tooltip-open").as_deref(),
        Some("true"),
        "positive control: the reveal window is open"
    );
    assert_eq!(
        computed.get_property_value("visibility").unwrap(),
        "visible",
        "the attribute must actually reveal the tooltip, not just annotate it"
    );
    assert_eq!(computed.get_property_value("opacity").unwrap(), "1");
    // The phone `display` opt-out cannot be exercised here: the test browser's
    // viewport is fixed above 640px. Its rule is pinned in `self_view.rs` and
    // measured at 390px by the Playwright spec.

    cleanup(&mount);
    style.remove();
    clear_storage();
}

#[wasm_bindgen_test]
async fn pressing_show_restores_the_tile_persists_it_and_retires_the_icon() {
    seed_pill(false, true, DockPosition::Bottom);
    save_self_view_visible(false);

    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    assert!(
        mount.query_selector(SHOW_BUTTON).unwrap().is_some(),
        "positive control: the icon is on screen before the press"
    );
    assert!(
        !load_self_view_visible(),
        "the stored preference starts hidden"
    );
    assert_eq!(
        attr(&mount, PROBE, "data-visible").as_deref(),
        Some("false")
    );

    mount
        .query_selector(SHOW_BUTTON)
        .unwrap()
        .expect("Show button must be present")
        .dyn_ref::<web_sys::HtmlElement>()
        .unwrap()
        .click();
    yield_now().await;

    assert_eq!(
        attr(&mount, PROBE, "data-visible").as_deref(),
        Some("true"),
        "Show must flip the shared visibility context"
    );
    assert!(load_self_view_visible(), "and persist it");
    assert_eq!(
        web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("vc_self_view_visible").ok().flatten())
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        ON_SHOW_CALLS.with(|c| c.get()),
        1,
        "the parent's announce-and-refocus handler must fire exactly once"
    );
    assert!(
        mount.query_selector(ICON_CLASS).unwrap().is_none(),
        "a restored self view leaves no icon node behind"
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn a_visible_self_view_emits_no_icon_nodes() {
    seed_pill(true, true, DockPosition::Bottom);

    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    assert!(
        mount.query_selector(PROBE).unwrap().is_some(),
        "positive control: the seeded tree rendered at all"
    );
    assert!(
        mount.query_selector(ICON_CLASS).unwrap().is_none(),
        "the screen-share specs index #grid-container > div positionally"
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn a_participant_who_cannot_stream_never_sees_the_icon() {
    seed_pill(false, false, DockPosition::Bottom);

    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    assert!(
        mount.query_selector(PROBE).unwrap().is_some(),
        "positive control: the seeded tree rendered at all"
    );
    assert!(
        mount.query_selector(ICON_CLASS).unwrap().is_none(),
        "no self tile exists to restore, so there is nothing to offer"
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn a_right_dock_shifts_the_icon_clear_of_the_bar() {
    seed_pill(false, true, DockPosition::Right);

    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    let icon = mount
        .query_selector(SHOW_BUTTON)
        .unwrap()
        .expect("positive control: the icon is on screen");
    assert!(
        icon.class_list()
            .contains("self-view-hidden-icon--dock-right"),
        "a right dock would otherwise bury the only recovery affordance"
    );

    cleanup(&mount);
    clear_storage();
}

#[wasm_bindgen_test]
async fn a_left_dock_leaves_the_icon_in_the_vacated_corner() {
    seed_pill(false, true, DockPosition::Left);

    let mount = create_mount_point();
    render_into(&mount, SeededPill);
    yield_now().await;

    let icon = mount
        .query_selector(SHOW_BUTTON)
        .unwrap()
        .expect("positive control: the icon is on screen");
    assert!(
        !icon
            .class_list()
            .contains("self-view-hidden-icon--dock-right"),
        "a left dock does not touch the bottom-right lane"
    );

    cleanup(&mount);
    clear_storage();
}

/// Attach the two production stylesheets so a case can measure real geometry.
/// They are global to the shared test document, hence the paired remover.
fn install_stylesheets() -> web_sys::Element {
    let doc = gloo_utils::document();
    let style = doc.create_element("style").unwrap();
    style.set_text_content(Some(concat!(
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    )));
    doc.head().unwrap().append_child(&style).unwrap();
    style
}

#[allow(non_snake_case)]
fn SeededIconInBox() -> Element {
    let visible = use_signal(|| false);
    use_context_provider(|| SelfViewVisibleCtx(visible));
    let dock = use_signal(|| DockPosition::Bottom);
    use_context_provider(|| DockPositionCtx(dock));
    rsx! {
        div {
            "data-testid": "icon-box",
            // 390px wide: the tightest box the tooltip has to fit inside.
            style: "position: relative; width: 390px; height: 300px;",
            SelfViewHiddenPill {
                can_stream: true,
                toast_present: SEED_TOAST.with(|t| t.get()),
                settings_open: false,
                on_show: move |_| {},
            }
        }
    }
}

// Geometry in real Chrome against the real stylesheets. The box is a 390px
// DIV, not a viewport, so no `@media` applies: this exercises the desktop
// right-anchor inside a narrow container. The phone rules are the Playwright
// spec's job.
#[wasm_bindgen_test]
async fn the_corner_icon_right_anchors_its_tooltip_inside_a_narrow_box() {
    let style = install_stylesheets();
    let mount = create_mount_point();
    render_into(&mount, SeededIconInBox);
    yield_now().await;

    let win = web_sys::window().unwrap();
    let button = mount
        .query_selector(SHOW_BUTTON)
        .unwrap()
        .expect("positive control: the icon is on screen");
    let position = win
        .get_computed_style(&button)
        .unwrap()
        .unwrap()
        .get_property_value("position")
        .unwrap();
    assert_eq!(
        position, "absolute",
        "the corner anchor must out-specify `.video-control-button {{ position: \
         relative }}`; `relative` here means the button is back in normal flow"
    );

    let boxed = mount
        .query_selector("[data-testid='icon-box']")
        .unwrap()
        .unwrap()
        .get_bounding_client_rect();
    let tip = mount
        .query_selector(".self-view-hidden-icon .tooltip")
        .unwrap()
        .expect("the hover text must exist to be measured")
        .get_bounding_client_rect();
    assert!(
        tip.width() > 0.0,
        "positive control: a laid-out tooltip, not a display:none one"
    );
    assert!(
        tip.right() <= boxed.right() + 1.0,
        "the tooltip must not hang off the right edge: tooltip right {} vs \
         container right {}",
        tip.right(),
        boxed.right()
    );
    assert!(
        tip.left() >= boxed.left() - 1.0,
        "and must not run off the left either: tooltip left {} vs container \
         left {}",
        tip.left(),
        boxed.left()
    );

    cleanup(&mount);
    style.remove();
    clear_storage();
}

// The production `on_show` is a `use_callback` that spawns a task to move focus.
// Nothing else in `attendants.rs` spawns from inside a `use_callback`, so this
// mounts the pill in that exact shape and proves the spawned continuation runs.
#[wasm_bindgen_test]
async fn on_show_as_a_use_callback_runs_its_spawned_continuation() {
    seed_pill(false, true, DockPosition::Bottom);

    #[allow(non_snake_case)]
    fn SpawnProbe() -> Element {
        let visible = use_signal(|| false);
        use_context_provider(|| SelfViewVisibleCtx(visible));
        let mut spawned = use_signal(|| false);
        let show: EventHandler<()> = use_callback(move |_| {
            spawn(async move {
                gloo_timers::future::TimeoutFuture::new(0).await;
                spawned.set(true);
            });
        });
        rsx! {
            div {
                "data-testid": "spawn-probe",
                "data-spawned": if spawned() { "true" } else { "false" },
            }
            SelfViewHiddenPill {
                can_stream: true,
                toast_present: false,
                settings_open: false,
                on_show: show,
            }
        }
    }

    let mount = create_mount_point();
    render_into(&mount, SpawnProbe);
    yield_now().await;

    assert_eq!(
        attr(&mount, "[data-testid='spawn-probe']", "data-spawned").as_deref(),
        Some("false"),
        "positive control: nothing has spawned before the press"
    );

    mount
        .query_selector(SHOW_BUTTON)
        .unwrap()
        .expect("Show button must be present")
        .dyn_ref::<web_sys::HtmlElement>()
        .unwrap()
        .click();
    yield_now().await;
    yield_now().await;

    assert_eq!(
        attr(&mount, "[data-testid='spawn-probe']", "data-spawned").as_deref(),
        Some("true"),
        "a `use_callback` handler must be able to spawn, and its awaited \
         continuation must run — the focus move depends on both"
    );

    cleanup(&mount);
    clear_storage();
}
