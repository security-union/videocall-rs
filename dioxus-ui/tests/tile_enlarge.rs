// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

// Issue 3367: an enlarged camera tile, mounted through the production tile list
// and painted by the production plan, flow style and stylesheets.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use dioxus::prelude::*;
use dioxus_ui::components::attendants_layout::{
    plan_tile_grid, tile_flow_style, tile_size_vars, TilePlanInput,
};
use dioxus_ui::components::canvas_generator::{PinnedTile, TileMode};
use dioxus_ui::components::decode_budget::build_unified_render_list;
use dioxus_ui::components::grid_overflow_badge::GridOverflowBadge;
use dioxus_ui::components::peer_tile::{camera_tiles, CameraTiles};
use dioxus_ui::components::pin_order::{activate_enlarge, camera_pin_sessions, grid_lead};
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, MeetingTime, PeerAudioLivenessMap,
    PeerSignalHistoryMap, SignalPopupStateMap,
};
use support::{cleanup, create_mount_point, inject_app_config, render_into, yield_now};
use videocall_client::VideoCallClient;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

const GAP: f64 = 16.0;
const PAD: f64 = 20.0;
const PEERS: [&str; 6] = ["11", "12", "13", "14", "15", "16"];

#[derive(Clone, Copy)]
struct Seed {
    area: (f64, f64),
    split: bool,
    full_bleed: bool,
    enlarged_users: &'static [&'static str],
    paused: &'static [&'static str],
    self_and_badge: bool,
    rewrite: &'static [(&'static str, &'static str)],
}

impl Seed {
    const DESKTOP: Seed = Seed {
        area: (1240.0, 548.0),
        split: false,
        full_bleed: false,
        enlarged_users: &[],
        paused: &[],
        self_and_badge: false,
        rewrite: &[],
    };
}

thread_local! {
    static SEED: Cell<Seed> = const { Cell::new(Seed::DESKTOP) };
    static PLAN_ENLARGED: Cell<usize> = const { Cell::new(0) };
}

#[allow(non_snake_case)]
fn EnlargeGrid() -> Element {
    let client = use_hook(|| {
        let client = VideoCallClient::new_for_test("local-user");
        for (i, session) in PEERS.iter().enumerate() {
            let session: u64 = session.parse().unwrap();
            client.insert_peer_for_test(session, &format!("user-{i}"));
            client.set_peer_media_for_test(session, true, false);
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

    let seed = use_hook(|| SEED.with(Cell::get));
    let mut enlarged: Signal<Vec<PinnedTile>> = use_signal(|| {
        seed.enlarged_users
            .iter()
            .map(|user| PinnedTile::camera(*user))
            .collect()
    });
    let last = use_hook(|| Rc::new(RefCell::new(None)));
    let toggle: EventHandler<PinnedTile> = use_callback(move |tile: PinnedTile| {
        let mut list = enlarged.peek().clone();
        let now = js_sys::Date::now();
        if activate_enlarge(&mut list, &mut last.borrow_mut(), now, tile).is_some() {
            enlarged.set(list);
        }
    });
    let pin: EventHandler<PinnedTile> = use_callback(|_: PinnedTile| {});
    let decode: EventHandler<String> = use_callback(|_: String| {});
    let badge_open = use_signal(|| false);
    let badge_opened: EventHandler<()> = use_callback(|_: ()| {});

    let (avail_w, avail_h) = seed.area;
    let ids: Vec<String> = PEERS.iter().map(|id| id.to_string()).collect();
    let (paused, decoded): (Vec<String>, Vec<String>) = ids
        .iter()
        .cloned()
        .partition(|id| seed.paused.contains(&id.as_str()));
    let sessions = camera_pin_sessions(&enlarged.read(), &ids, |sid| {
        client.get_peer_user_id(sid).unwrap_or_else(|| sid.into())
    });
    let lead = grid_lead(&sessions, &[]);
    let unified = build_unified_render_list(&decoded, &paused, &[], &lead.rank, &HashMap::new());
    let tile_count = unified.len() + if seed.self_and_badge { 2 } else { 0 };
    let (plan, view) = plan_tile_grid(
        TilePlanInput {
            tile_count,
            split_layout: seed.split,
            full_bleed: seed.full_bleed,
            avail_w,
            avail_h,
            gap: GAP,
            vw: avail_w + 2.0 * PAD,
        },
        &unified,
        &sessions,
        false,
    );
    PLAN_ENLARGED.with(|p| p.set(plan.enlarged));
    let style = format!(
        "position: absolute; top: 0; left: 0; width: {w}px; height: {h}px; gap: {GAP}px; \
         padding: {PAD}px; box-sizing: border-box; overflow: hidden; {flow} {sizes}",
        w = avail_w + 2.0 * PAD,
        h = avail_h + 2.0 * PAD,
        flow = tile_flow_style(tile_count, plan),
        sizes = tile_size_vars(plan.tile_w, plan.tile_h(), GAP),
    );
    let no_bleed = |_: &str| false;
    let (grid_class, mode) = if seed.split {
        ("has-screen-share", TileMode::VideoOnly)
    } else {
        ("participants-6", TileMode::Full)
    };
    rsx! {
        div {
            id: "grid-container",
            class: grid_class,
            "data-tile-flow": "wrap",
            style: "{style}",
            if seed.self_and_badge {
                nav {
                    id: "self-tile",
                    class: "host",
                    "data-self-placement": "grid",
                    "data-self-hidden": "false",
                }
            }
            div { class: "ss-peer-panel",
                {camera_tiles(CameraTiles {
                    tiles: &unified,
                    pin_rank: &HashMap::new(),
                    enlarged: &view,
                    on_toggle_enlarge: toggle,
                    full_bleed: &no_bleed,
                    host_user_id: &None,
                    render_mode: &mode,
                    my_session_id: &None,
                    room_id: "room",
                    is_host: false,
                    on_toggle_pin: pin,
                    on_request_decode: decode,
                    mock_on_toggle_pin: pin,
                    mock_on_request_decode: decode,
                })}
                if seed.self_and_badge {
                    GridOverflowBadge {
                        overflow_count: 4,
                        open: badge_open,
                        on_open: badge_opened,
                    }
                }
            }
        }
    }
}

struct Page {
    mount: web_sys::Element,
    style: web_sys::Element,
}

impl Page {
    fn close(self) {
        cleanup(&self.mount);
        self.style.remove();
    }

    fn el(&self, selector: &str) -> web_sys::HtmlElement {
        self.mount
            .query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("{selector} must be rendered"))
            .unchecked_into()
    }

    fn has(&self, selector: &str) -> bool {
        self.mount.query_selector(selector).unwrap().is_some()
    }

    fn tile(&self, id: &str) -> web_sys::HtmlElement {
        self.el(&tile_id(id))
    }

    fn button(&self, id: &str) -> web_sys::HtmlElement {
        self.el(&format!(
            "{} [data-testid='tile-enlarge-button']",
            tile_id(id)
        ))
    }

    fn computed(&self, selector: &str, property: &str) -> String {
        web_sys::window()
            .unwrap()
            .get_computed_style(&self.el(selector))
            .unwrap()
            .unwrap()
            .get_property_value(property)
            .unwrap()
    }

    fn rects(&self) -> Vec<(String, web_sys::DomRect)> {
        let mut rects: Vec<(String, web_sys::DomRect)> = PEERS
            .iter()
            .map(|id| (id.to_string(), self.tile(id).get_bounding_client_rect()))
            .collect();
        rects.sort_by(|(_, a), (_, b)| {
            (a.top().round(), a.left().round())
                .partial_cmp(&(b.top().round(), b.left().round()))
                .unwrap()
        });
        rects
    }

    fn assert_grid_holds_every_tile_without_overlap(&self) {
        let grid = self.el("#grid-container").get_bounding_client_rect();
        let rects = self.rects();
        for (id, r) in &rects {
            assert!(
                r.left() >= grid.left() + PAD - 0.5
                    && r.top() >= grid.top() + PAD - 0.5
                    && r.right() <= grid.right() - PAD + 0.5
                    && r.bottom() <= grid.bottom() - PAD + 0.5,
                "tile {id} [{},{} {}x{}] leaves the {}x{} grid's content box",
                r.left(),
                r.top(),
                r.width(),
                r.height(),
                grid.width(),
                grid.height(),
            );
        }
        for (i, (a_id, a)) in rects.iter().enumerate() {
            for (b_id, b) in &rects[i + 1..] {
                let apart = a.right() <= b.left() + 0.5
                    || b.right() <= a.left() + 0.5
                    || a.bottom() <= b.top() + 0.5
                    || b.bottom() <= a.top() + 0.5;
                assert!(apart, "tiles {a_id} and {b_id} overlap");
            }
        }
    }
}

fn tile_id(id: &str) -> String {
    format!("#peer-video-{id}-div")
}

fn install_stylesheets(rewrite: &[(&str, &str)]) -> web_sys::Element {
    let mut css = format!(
        "{}{}{}",
        include_str!("../static/tailwind.css"),
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    );
    for (from, to) in rewrite {
        css = css.replace(from, to);
    }
    let style = gloo_utils::document().create_element("style").unwrap();
    style.set_text_content(Some(&css));
    gloo_utils::document()
        .head()
        .unwrap()
        .append_child(&style)
        .unwrap();
    style
}

async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

async fn settle() {
    for _ in 0..4 {
        yield_now().await;
    }
}

/// The double-activation guard drops a click on another tile inside its window.
async fn past_the_guard() {
    sleep(400).await;
}

async fn mount_grid(seed: Seed) -> Page {
    inject_app_config();
    SEED.with(|s| s.set(seed));
    let style = install_stylesheets(seed.rewrite);
    let mount = create_mount_point();
    render_into(&mount, EnlargeGrid);
    settle().await;
    Page { mount, style }
}

fn px(value: &str) -> f64 {
    value
        .trim()
        .trim_end_matches("px")
        .parse()
        .unwrap_or_else(|_| panic!("{value:?} is not a px length"))
}

#[wasm_bindgen_test]
async fn enlarging_spans_two_cells_and_restoring_returns_to_the_flex_flow() {
    let page = mount_grid(Seed::DESKTOP).await;
    assert_eq!(page.computed("#grid-container", "display"), "flex");
    let std_w = page.tile("13").get_bounding_client_rect().width();
    let std_h = page.tile("13").get_bounding_client_rect().height();
    assert_eq!(
        page.button("13").get_attribute("aria-pressed").unwrap(),
        "false"
    );

    page.button("13").click();
    settle().await;
    assert_eq!(
        PLAN_ENLARGED.with(Cell::get),
        1,
        "premise: the plan paints it"
    );
    assert_eq!(page.computed("#grid-container", "display"), "grid");
    assert_eq!(
        page.computed("#grid-container", "grid-auto-flow"),
        "dense",
        "`row dense` serializes without its default"
    );
    let grid_w = px(&page.computed("#grid-container", "--tile-w"));
    let grid_h = px(&page.computed("#grid-container", "--tile-h"));
    assert!(grid_w < std_w, "the grid re-fits: {grid_w} vs {std_w}");

    let enlarged = page.tile("13");
    assert_eq!(enlarged.get_attribute("data-enlarged").unwrap(), "true");
    assert!(enlarged.class_name().contains("tile-enlarged"));
    let rect = enlarged.get_bounding_client_rect();
    assert_eq!(rect.width(), 2.0 * grid_w + GAP, "two columns and a gap");
    assert_eq!(rect.height(), 2.0 * grid_h + GAP, "two rows and a gap");
    assert_eq!(
        page.rects()[0].0,
        "13",
        "the enlarged tile leads by `order`, through the slot"
    );
    assert_eq!(
        page.tile("11").get_bounding_client_rect().width(),
        grid_w,
        "a standard tile keeps the standard track"
    );
    page.assert_grid_holds_every_tile_without_overlap();

    assert_eq!(
        page.button("13").get_attribute("aria-pressed").unwrap(),
        "true"
    );
    assert_eq!(
        page.button("13").get_attribute("title").unwrap(),
        "Restore size"
    );
    assert_eq!(
        page.button("13").get_attribute("aria-label").unwrap(),
        page.button("11")
            .get_attribute("aria-label")
            .unwrap()
            .replace("user-0", "user-2"),
        "the name is the only difference: the label does not flip with the state"
    );

    page.button("13").click();
    settle().await;
    assert_eq!(page.computed("#grid-container", "display"), "flex");
    assert_eq!(
        page.computed("#grid-container", "grid-auto-flow"),
        "row",
        "the style restore loop must not keep the dense flow"
    );
    assert_eq!(page.computed("#grid-container", "grid-auto-rows"), "auto");
    assert!(!page.has("[data-enlarged]"));
    assert!(!page.has(".tile-enlarged"));
    let restored = page.tile("13").get_bounding_client_rect();
    assert_eq!(
        (restored.width(), restored.height().round()),
        (std_w, std_h.round())
    );
    page.close();
}

#[wasm_bindgen_test]
async fn a_toggle_keeps_every_canvas_node_and_the_focus() {
    let page = mount_grid(Seed::DESKTOP).await;
    let canvases: Vec<_> = PEERS
        .iter()
        .map(|id| page.tile(id).query_selector("canvas").unwrap().unwrap())
        .collect();

    page.button("14").focus().unwrap();
    page.button("14").click();
    settle().await;
    sleep(20).await;
    let enlarge_id = page.button("14").id();
    assert_eq!(enlarge_id, "peer-video-14-div-enlarge-btn");
    assert_eq!(
        gloo_utils::document().active_element().unwrap().id(),
        enlarge_id,
        "the keyed reorder blurs the moved button, so focus must be put back"
    );
    let same_nodes = |page: &Page| {
        for (id, canvas) in PEERS.iter().zip(&canvases) {
            let now = page.tile(id).query_selector("canvas").unwrap().unwrap();
            assert!(
                js_sys::Object::is(now.as_ref(), canvas.as_ref()),
                "{id}: a rebuilt canvas asks the publisher for a keyframe"
            );
        }
    };
    same_nodes(&page);

    page.button("14").click();
    settle().await;
    same_nodes(&page);
    page.close();
}

#[wasm_bindgen_test]
async fn the_enlarged_tile_chrome_reads_the_doubled_vars() {
    let page = mount_grid(Seed::DESKTOP).await;
    page.button("12").click();
    settle().await;
    let grid_h = px(&page.computed("#grid-container", "--tile-h"));
    let grid_w = px(&page.computed("#grid-container", "--tile-w"));
    assert_eq!(
        px(&page.computed(&tile_id("12"), "--tile-h")),
        2.0 * grid_h + GAP
    );
    assert_eq!(
        px(&page.computed(&tile_id("12"), "--tile-w")),
        2.0 * grid_w + GAP
    );
    assert_eq!(px(&page.computed(&tile_id("11"), "--tile-h")), grid_h);
    let disc = |id: &str| px(&page.computed(&format!("{} .pin-icon", tile_id(id)), "width"));
    assert!(
        disc("12") > disc("11"),
        "the tile's chrome scales with it: {} vs {}",
        disc("12"),
        disc("11")
    );
    page.close();
}

#[wasm_bindgen_test]
async fn a_second_enlarge_goes_behind_the_first_and_pressed_stays_visible() {
    let page = mount_grid(Seed::DESKTOP).await;
    page.button("15").click();
    settle().await;
    past_the_guard().await;
    page.button("12").click();
    settle().await;
    sleep(250).await;
    assert_eq!(
        PLAN_ENLARGED.with(Cell::get),
        2,
        "premise: both are painted"
    );
    let reading: Vec<String> = page.rects().into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        &reading[..2],
        ["15", "12"],
        "the first enlarge does not move"
    );
    page.assert_grid_holds_every_tile_without_overlap();
    assert_eq!(
        page.computed(&format!("{} .enlarge-icon", tile_id("15")), "opacity"),
        "1",
        "the way back must not hide"
    );
    assert_eq!(
        page.computed(&format!("{} .enlarge-icon", tile_id("11")), "opacity"),
        "0",
        "a resting control waits for hover, like the pin"
    );
    page.close();
}

#[wasm_bindgen_test]
async fn the_control_is_absent_where_enlarging_is_not_offered() {
    for seed in [
        Seed {
            split: true,
            ..Seed::DESKTOP
        },
        Seed {
            full_bleed: true,
            ..Seed::DESKTOP
        },
        Seed {
            area: (359.0, 547.0),
            ..Seed::DESKTOP
        },
    ] {
        let area = seed.area;
        let page = mount_grid(seed).await;
        assert!(
            !page.has("[data-testid='tile-enlarge-button']"),
            "{area:?}: an invisible control still takes taps"
        );
        assert!(page.has("[data-testid='tile-pin-button']"), "premise");
        page.close();
    }
}

#[wasm_bindgen_test]
async fn the_control_is_the_last_of_the_row_so_tab_order_reaches_the_pin_first() {
    let page = mount_grid(Seed::DESKTOP).await;
    let row = page.el(&format!("{} .tile-top-icons", tile_id("11")));
    let last = row.last_element_child().unwrap();
    assert_eq!(
        last.get_attribute("data-testid").unwrap(),
        "tile-enlarge-button"
    );
    let previous = last.previous_element_sibling().unwrap();
    assert_eq!(
        previous.get_attribute("data-testid").unwrap(),
        "tile-pin-button"
    );
    let (enlarge, pin) = (
        last.get_bounding_client_rect(),
        previous.get_bounding_client_rect(),
    );
    assert!(
        enlarge.right() <= pin.left(),
        "row-reverse puts it leftmost"
    );
    page.close();
}

#[wasm_bindgen_test]
async fn touch_reserves_name_room_only_for_a_tile_with_the_control() {
    let reserved = |page: &Page| -> f64 {
        let max_width = page.computed(&format!("{} .floating-name", tile_id("11")), "max-width");
        let (_, after) = max_width
            .split_once("100% - ")
            .unwrap_or_else(|| panic!("{max_width:?} has no reserve"));
        px(after.trim_end_matches(')'))
    };
    let page = mount_grid(Seed {
        rewrite: &[("(pointer: coarse)", "all")],
        ..Seed::DESKTOP
    })
    .await;
    assert_eq!(
        page.computed(&tile_id("11"), "--name-row-enlarge").trim(),
        "44px"
    );
    assert_eq!(
        page.computed(&format!("{} .enlarge-icon", tile_id("11")), "opacity"),
        "1"
    );
    let with_control = reserved(&page);
    page.close();

    let page = mount_grid(Seed {
        rewrite: &[("(pointer: coarse)", "all")],
        full_bleed: true,
        ..Seed::DESKTOP
    })
    .await;
    assert_eq!(
        page.computed(&tile_id("11"), "--name-row-enlarge").trim(),
        ""
    );
    let without_control = reserved(&page);
    page.close();
    assert_eq!(with_control, without_control + 44.0);
}

#[wasm_bindgen_test]
async fn an_odd_column_count_fills_the_holes_beside_the_enlarged_tiles() {
    let page = mount_grid(Seed {
        area: (800.0, 900.0),
        enlarged_users: &["user-0", "user-1"],
        ..Seed::DESKTOP
    })
    .await;
    assert_eq!(PLAN_ENLARGED.with(Cell::get), 2, "premise");
    let tracks = page.computed("#grid-container", "grid-template-columns");
    assert_eq!(tracks.split_whitespace().count(), 3, "premise: {tracks}");
    page.assert_grid_holds_every_tile_without_overlap();
    page.close();
}

#[wasm_bindgen_test]
async fn a_paused_enlarged_tile_keeps_its_two_by_two_cell() {
    let page = mount_grid(Seed {
        enlarged_users: &["user-3"],
        paused: &["14"],
        ..Seed::DESKTOP
    })
    .await;
    let tile = page.tile("14");
    assert_eq!(tile.get_attribute("data-off-budget").unwrap(), "true");
    assert!(tile.class_name().contains("tile-enlarged"));
    let grid_w = px(&page.computed("#grid-container", "--tile-w"));
    assert_eq!(tile.get_bounding_client_rect().width(), 2.0 * grid_w + GAP);
    assert_eq!(page.rects()[0].0, "14");
    page.assert_grid_holds_every_tile_without_overlap();
    page.close();
}

#[wasm_bindgen_test]
async fn neither_the_split_nor_full_bleed_enlarges_a_requested_tile() {
    for seed in [
        Seed {
            enlarged_users: &["user-3"],
            split: true,
            ..Seed::DESKTOP
        },
        Seed {
            enlarged_users: &["user-3"],
            full_bleed: true,
            ..Seed::DESKTOP
        },
    ] {
        let page = mount_grid(seed).await;
        assert_eq!(PLAN_ENLARGED.with(Cell::get), 0);
        let tile = page.tile("14");
        assert_eq!(
            tile.class_name().contains("split-peer-tile"),
            seed.split,
            "premise"
        );
        assert!(!tile.class_name().contains("tile-enlarged"));
        assert!(tile.get_attribute("data-enlarged").is_none());
        assert_eq!(page.computed(&tile_id("14"), "order"), "0");
        assert!(!page.has("[data-testid='tile-enlarge-button']"));
        page.close();
    }
}

#[wasm_bindgen_test]
async fn a_requested_tile_the_floor_drops_is_standard_size_at_order_zero() {
    let page = mount_grid(Seed {
        area: (700.0, 330.0),
        enlarged_users: &["user-0", "user-1", "user-5"],
        ..Seed::DESKTOP
    })
    .await;
    assert_eq!(PLAN_ENLARGED.with(Cell::get), 2, "premise: the floor binds");
    for id in ["11", "12"] {
        assert_eq!(
            page.tile(id).get_attribute("data-enlarged").unwrap(),
            "true"
        );
        assert!(page.has(&format!(
            "{} [data-testid='tile-enlarge-button']",
            tile_id(id)
        )));
    }
    let dropped = page.tile("16");
    assert!(!dropped.class_name().contains("tile-enlarged"));
    assert!(dropped.get_attribute("data-enlarged").is_none());
    assert_eq!(
        page.computed(&tile_id("16"), "order"),
        "0",
        "behind every pin's `order`"
    );
    assert_eq!(
        dropped.get_bounding_client_rect().width(),
        px(&page.computed("#grid-container", "--tile-w"))
    );
    assert!(!page.has(&format!(
        "{} [data-testid='tile-enlarge-button']",
        tile_id("16")
    )));
    let reading: Vec<String> = page.rects().into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        reading,
        ["11", "12", "16", "13", "14", "15"],
        "it keeps its window priority, so it still leads the other `order: 0` tiles"
    );
    page.assert_grid_holds_every_tile_without_overlap();
    page.close();
}

#[wasm_bindgen_test]
async fn a_restore_that_removes_the_control_moves_focus_to_the_pin() {
    let page = mount_grid(Seed {
        area: (700.0, 330.0),
        enlarged_users: &["user-0"],
        ..Seed::DESKTOP
    })
    .await;
    assert_eq!(PLAN_ENLARGED.with(Cell::get), 1, "premise");

    page.button("11").focus().unwrap();
    page.button("11").click();
    settle().await;
    sleep(20).await;
    assert_eq!(PLAN_ENLARGED.with(Cell::get), 0, "premise");
    assert!(
        !page.has("[data-testid='tile-enlarge-button']"),
        "premise: the restored tile is too small to offer the control"
    );
    assert_eq!(
        gloo_utils::document().active_element().unwrap().id(),
        "peer-video-11-div-pin-btn"
    );
    page.close();
}

#[wasm_bindgen_test]
async fn the_self_tile_and_the_badge_each_take_one_cell_beside_an_enlarged_tile() {
    let page = mount_grid(Seed {
        enlarged_users: &["user-3"],
        self_and_badge: true,
        ..Seed::DESKTOP
    })
    .await;
    assert_eq!(PLAN_ENLARGED.with(Cell::get), 1, "premise");
    page.assert_grid_holds_every_tile_without_overlap();
    assert_eq!(
        page.rects()[0].0,
        "14",
        "ahead of the self tile's `order: -1`"
    );
    let (w, h) = (
        px(&page.computed("#grid-container", "--tile-w")),
        px(&page.computed("#grid-container", "--tile-h")),
    );
    let tile = page.tile("14").get_bounding_client_rect();
    let grid = page.el("#grid-container").get_bounding_client_rect();
    for cell in ["#self-tile", ".grid-overflow-badge"] {
        let rect = page.el(cell).get_bounding_client_rect();
        assert_eq!((rect.width(), rect.height()), (w, h), "{cell}");
        assert!(
            tile.right() <= rect.left() + 0.5 || tile.bottom() <= rect.top() + 0.5,
            "{cell} overlaps the enlarged tile"
        );
        assert!(
            rect.bottom() <= grid.bottom() - PAD + 0.5,
            "{cell} is clipped"
        );
    }
    page.close();
}
