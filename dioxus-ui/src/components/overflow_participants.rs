// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 3128: the participants behind the `+N more in meeting` badge, listed
//! in a popover where each one can be pinned into the grid.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::components::canvas_generator::{PinnedTile, PinnedTileKind};
use crate::components::co_hosts::{host_indicator, host_role, owner_holds_host, peer_holds_host};
use crate::components::icons::push_pin::PushPinIcon;
use crate::components::pin_order;
use crate::components::share_view;
use crate::components::signal_quality::Rect;
use crate::context::HostSetCtx;

pub const BADGE_ID: &str = "grid-overflow-badge";
pub const LIST_ID: &str = "grid-overflow-list";
const TITLE_ID: &str = "grid-overflow-list-title";
const CLOSE_ID: &str = "grid-overflow-list-close";
const FALLBACK_NAME: &str = "Participant";
const SHEET_BELOW_PX: f64 = 640.0;
const VIEWPORT_MARGIN_PX: f64 = 8.0;
const ANCHOR_GAP_PX: f64 = 8.0;
const TOUCH_HOLD_MS: u32 = 4_000;

#[derive(Clone, Debug, PartialEq)]
pub struct OverflowParticipant {
    pub session_id: String,
    pub name: Option<String>,
    /// `None` for a mock placeholder, which cannot be pinned.
    pub user_id: Option<String>,
}

impl OverflowParticipant {
    pub fn label(&self) -> &str {
        self.name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or(FALLBACK_NAME)
    }
}

/// Every peer and mock placeholder that is in none of the `rendered` groups.
pub fn hidden_participants<'a>(
    peers: &'a [String],
    mocks: &'a [String],
    rendered: &[&[String]],
) -> Vec<&'a str> {
    let shown: HashSet<&str> = rendered
        .iter()
        .flat_map(|group| group.iter().map(String::as_str))
        .collect();
    peers
        .iter()
        .chain(mocks)
        .map(String::as_str)
        .filter(|id| !shown.contains(id))
        .collect()
}

pub fn describe(
    hidden: &[&str],
    user_id_of: impl Fn(&str) -> Option<String>,
    name_of: impl Fn(&str) -> Option<String>,
) -> Vec<OverflowParticipant> {
    hidden
        .iter()
        .map(|&session_id| {
            let mock = session_id.starts_with("mock-");
            OverflowParticipant {
                session_id: session_id.to_string(),
                name: (!mock).then(|| name_of(session_id)).flatten(),
                user_id: (!mock)
                    .then(|| user_id_of(session_id).unwrap_or_else(|| session_id.to_string())),
            }
        })
        .collect()
}

/// Sort key: the shown name case-insensitively, then the session id.
pub fn display_name_key(p: &OverflowParticipant) -> (String, String) {
    (p.label().to_lowercase(), p.session_id.clone())
}

pub fn row_dom_id(session_id: &str) -> String {
    format!("grid-overflow-row-{session_id}")
}

/// Viewport `(left, top)` for the list: above the badge (below it when only
/// that fits), right edges aligned, clamped into the viewport. `None` leaves
/// it to the stylesheet.
pub(crate) fn list_placement(
    viewport: (f64, f64),
    anchor: Option<Rect>,
    size: (f64, f64),
) -> Option<(f64, f64)> {
    let (vw, vh) = viewport;
    let anchor = anchor.filter(|_| vw >= SHEET_BELOW_PX)?;
    let (w, h) = size;
    let max_left = (vw - w - VIEWPORT_MARGIN_PX).max(VIEWPORT_MARGIN_PX);
    let max_top = (vh - h - VIEWPORT_MARGIN_PX).max(VIEWPORT_MARGIN_PX);
    let above = anchor.top - ANCHOR_GAP_PX - h;
    let below = anchor.bottom + ANCHOR_GAP_PX;
    let top = if above < VIEWPORT_MARGIN_PX && below + h <= vh - VIEWPORT_MARGIN_PX {
        below
    } else {
        above.clamp(VIEWPORT_MARGIN_PX, max_top)
    };
    Some(((anchor.right - w).clamp(VIEWPORT_MARGIN_PX, max_left), top))
}

/// `order[from]`, then the other rows nearest it (next before previous), then
/// the close button.
pub(crate) fn focus_order_from(order: &[String], from: usize) -> Vec<String> {
    let mut ids: Vec<String> = order.get(from).cloned().into_iter().collect();
    for d in 1..order.len() {
        ids.extend(order.get(from + d).cloned());
        ids.extend(from.checked_sub(d).and_then(|i| order.get(i)).cloned());
    }
    ids.push(CLOSE_ID.to_string());
    ids
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Nav {
    Next,
    Prev,
    First,
    Last,
}

/// Arrow-key target: Up/Down wrap through `rows` and the close button,
/// Home/End go to the first/last row.
pub(crate) fn nav_target(rows: &[String], current: Option<&str>, nav: Nav) -> String {
    let cycle: Vec<&str> = rows.iter().map(String::as_str).chain([CLOSE_ID]).collect();
    let n = cycle.len();
    let at = current.and_then(|c| cycle.iter().position(|id| *id == c));
    let id = match (nav, at) {
        (Nav::First, _) => rows.first().map_or(CLOSE_ID, String::as_str),
        (Nav::Last, _) => rows.last().map_or(CLOSE_ID, String::as_str),
        (Nav::Next, None) => cycle[0],
        (Nav::Prev, None) => cycle[n - 1],
        (Nav::Next, Some(i)) => cycle[(i + 1) % n],
        (Nav::Prev, Some(i)) => cycle[(i + n - 1) % n],
    };
    id.to_string()
}

fn is_touch_like(e: &PointerEvent) -> bool {
    matches!(e.pointer_type().as_str(), "touch" | "pen")
}

fn arm_touch_release(
    slot: &RefCell<Option<Timeout>>,
    hold_ms: u32,
    mut held: Signal<Option<Rc<Vec<OverflowParticipant>>>>,
) {
    slot.replace(Some(Timeout::new(hold_ms, move || {
        if let Ok(mut w) = held.try_write() {
            *w = None;
        }
    })));
}

#[component]
pub fn OverflowParticipantList(
    open: Signal<bool>,
    participants: Vec<OverflowParticipant>,
    owner_user_id: Option<String>,
    pins: Signal<Vec<PinnedTile>>,
    on_toggle_pin: EventHandler<PinnedTile>,
    /// How long a touch or pen hold outlives the last pointerdown.
    #[props(default = TOUCH_HOLD_MS)]
    touch_hold_ms: u32,
) -> Element {
    let host_set = try_use_context::<HostSetCtx>();
    let mut held: Signal<Option<Rc<Vec<OverflowParticipant>>>> = use_signal(|| None);
    let rows = held().unwrap_or_else(|| {
        let mut live = participants;
        live.sort_by_cached_key(display_name_key);
        Rc::new(live)
    });
    let focusable: Rc<Vec<String>> = Rc::new(
        rows.iter()
            .filter(|r| r.user_id.is_some())
            .map(|r| row_dom_id(&r.session_id))
            .collect(),
    );
    let count = rows.len();

    let rendered_order = use_hook(|| Rc::new(RefCell::new(Vec::<String>::new())));
    keep_focus_in_list(&rendered_order.borrow(), &focusable);
    rendered_order.replace(focusable.to_vec());

    use_hook({
        let first = focusable.first().cloned();
        move || {
            focus_soon(
                first.into_iter().chain([CLOSE_ID.to_string()]).collect(),
                false,
            )
        }
    });
    use_hook(move || {
        Rc::new((
            Listener::capture_click_outside(open),
            Listener::on_window_resize(place_list),
        ))
    });
    let resize_watch = use_hook(|| Rc::new(RefCell::new(None::<ResizeWatch>)));
    let touch_release = use_hook(|| Rc::new(RefCell::new(None::<Timeout>)));
    use_drop(|| focus_soon(vec![BADGE_ID.to_string()], true));
    use_effect(use_reactive!(|count| {
        let _ = count;
        if resize_watch.borrow().is_none() {
            resize_watch.replace(ResizeWatch::new(place_list));
        }
        place_list();
    }));

    let owner = owner_user_id.as_deref();
    let owner_has_role = owner_holds_host(host_set.as_ref(), owner);
    let pins = pins.read();

    rsx! {
        div {
            id: LIST_ID,
            class: "grid-overflow-list",
            role: "dialog",
            "aria-labelledby": TITLE_ID,
            "data-testid": "grid-overflow-list",
            onclick: move |e: MouseEvent| e.stop_propagation(),
            onpointerenter: {
                let rows = rows.clone();
                let touch_release = touch_release.clone();
                move |e: PointerEvent| {
                    if is_touch_like(&e) {
                        arm_touch_release(&touch_release, touch_hold_ms, held);
                    } else {
                        touch_release.replace(None);
                    }
                    held.set(Some(rows.clone()));
                }
            },
            onpointerdown: {
                let touch_release = touch_release.clone();
                move |e: PointerEvent| {
                    if is_touch_like(&e) {
                        arm_touch_release(&touch_release, touch_hold_ms, held);
                    }
                }
            },
            onpointerleave: {
                let touch_release = touch_release.clone();
                move |e: PointerEvent| {
                    if !is_touch_like(&e) {
                        touch_release.replace(None);
                        held.set(None);
                    }
                }
            },
            onkeydown: {
                let focusable = focusable.clone();
                move |e: KeyboardEvent| {
                    let nav = match e.key() {
                        Key::Escape => {
                            e.stop_propagation();
                            e.prevent_default();
                            close_to_badge(open);
                            return;
                        }
                        Key::ArrowDown => Nav::Next,
                        Key::ArrowUp => Nav::Prev,
                        Key::Home => Nav::First,
                        Key::End => Nav::Last,
                        _ => return,
                    };
                    e.stop_propagation();
                    e.prevent_default();
                    let id = nav_target(&focusable, active_element_id().as_deref(), nav);
                    share_view::focus_first(&[id]);
                }
            },
            div { class: "grid-overflow-list-header",
                h2 { id: TITLE_ID, class: "grid-overflow-list-title", "{count} more in meeting" }
                button {
                    id: CLOSE_ID,
                    class: "close-button grid-overflow-list-close",
                    r#type: "button",
                    "aria-label": "Close",
                    onclick: move |_| close_to_badge(open),
                    "\u{00d7}"
                }
            }
            ul { class: "grid-overflow-list-rows",
                for row in rows.iter() {
                    li { key: "{row.session_id}",
                        {
                            let label = row.label().to_string();
                            match row.user_id.clone() {
                                None => rsx! {
                                    div { class: "grid-overflow-row",
                                        span { class: "grid-overflow-row-name", "{label}" }
                                    }
                                },
                                Some(user_id) => {
                                    let pinned = pin_order::pin_rank(&pins, &user_id, PinnedTileKind::Camera).is_some();
                                    let tag = host_indicator(
                                        false,
                                        host_role(
                                            peer_holds_host(host_set.as_ref(), owner, &user_id),
                                            &user_id,
                                            owner,
                                            owner_has_role,
                                        ),
                                    );
                                    let dom_id = row_dom_id(&row.session_id);
                                    let tag_id = format!("{dom_id}-role");
                                    rsx! {
                                        button {
                                            id: "{dom_id}",
                                            class: "grid-overflow-row",
                                            r#type: "button",
                                            "aria-pressed": if pinned { "true" } else { "false" },
                                            "aria-label": "Pin {label}",
                                            "aria-describedby": tag.map(|_| tag_id.clone()),
                                            title: if pinned { "Unpin {label}" } else { "Pin {label}" },
                                            onclick: move |e: MouseEvent| {
                                                e.stop_propagation();
                                                on_toggle_pin.call(PinnedTile::camera(user_id.clone()));
                                            },
                                            span { class: "grid-overflow-row-name", "{label}" }
                                            if let Some(tag) = tag {
                                                span { id: "{tag_id}", class: "peer-indicator", "{tag}" }
                                            }
                                            span { class: "grid-overflow-row-pin", "aria-hidden": "true", PushPinIcon {} }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn close_to_badge(mut open: Signal<bool>) {
    open.set(false);
    share_view::focus_first(&[BADGE_ID.to_string()]);
}

/// Runs during render, before the DOM drops rows: when the focused row is not
/// in `now`, focus its nearest surviving neighbour in `before`; when it only
/// moved, refocus it if the move blurred it.
fn keep_focus_in_list(before: &[String], now: &[String]) {
    if before == now {
        return;
    }
    let Some(active) = active_element_id() else {
        return;
    };
    if now.contains(&active) {
        focus_soon(vec![active], true);
        return;
    }
    if let Some(at) = before.iter().position(|id| *id == active) {
        focus_soon(focus_order_from(before, at), false);
    }
}

/// On the next task, [`share_view::focus_first`] over `ids`; with
/// `only_if_lost`, only when focus has fallen back to `<body>`.
fn focus_soon(ids: Vec<String>, only_if_lost: bool) {
    Timeout::new(0, move || {
        if !only_if_lost || focus_is_lost() {
            share_view::focus_first(&ids);
        }
    })
    .forget();
}

fn active_element_id() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.active_element())
        .map(|el| el.id())
}

fn focus_is_lost() -> bool {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    match (doc.active_element(), doc.body()) {
        (None, _) => true,
        (Some(active), Some(body)) => active == *body.as_ref(),
        (Some(_), None) => false,
    }
}

fn place_list() {
    let Some(win) = web_sys::window() else {
        return;
    };
    let Some(doc) = win.document() else {
        return;
    };
    let Some(list) = doc
        .get_element_by_id(LIST_ID)
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return;
    };
    let px = |v: Result<wasm_bindgen::JsValue, _>| v.ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
    let vw = px(win.inner_width());
    let placement = (vw >= SHEET_BELOW_PX)
        .then(|| {
            let anchor = doc.get_element_by_id(BADGE_ID).map(|badge| {
                let r = badge.get_bounding_client_rect();
                Rect {
                    left: r.left(),
                    top: r.top(),
                    right: r.right(),
                    bottom: r.bottom(),
                }
            });
            let size = list.get_bounding_client_rect();
            list_placement(
                (vw, px(win.inner_height())),
                anchor,
                (size.width(), size.height()),
            )
        })
        .flatten();
    let style = list.style();
    match placement {
        Some((left, top)) => {
            let _ = style.set_property("left", &format!("{left:.0}px"));
            let _ = style.set_property("top", &format!("{top:.0}px"));
            let _ = style.set_property("right", "auto");
            let _ = style.set_property("bottom", "auto");
        }
        None => {
            for prop in ["left", "top", "right", "bottom"] {
                let _ = style.remove_property(prop);
            }
        }
    }
    let _ = list.set_attribute("data-placed", "");
}

/// A DOM listener removed again when the guard drops.
struct Listener {
    target: web_sys::EventTarget,
    event: &'static str,
    capture: bool,
    cb: Closure<dyn FnMut(web_sys::Event)>,
}

impl Listener {
    fn new(
        target: Option<web_sys::EventTarget>,
        event: &'static str,
        capture: bool,
        handler: impl FnMut(web_sys::Event) + 'static,
    ) -> Option<Self> {
        let target = target?;
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(handler);
        target
            .add_event_listener_with_callback_and_bool(event, cb.as_ref().unchecked_ref(), capture)
            .ok()?;
        Some(Self {
            target,
            event,
            capture,
            cb,
        })
    }

    fn capture_click_outside(mut open: Signal<bool>) -> Option<Self> {
        let doc = web_sys::window().and_then(|w| w.document());
        Self::new(doc.map(Into::into), "click", true, move |evt| {
            let inside = evt
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .and_then(|el| {
                    el.closest(&format!("#{LIST_ID}, #{BADGE_ID}"))
                        .ok()
                        .flatten()
                })
                .is_some();
            if !inside {
                if let Ok(mut w) = open.try_write() {
                    *w = false;
                }
            }
        })
    }

    fn on_window_resize(handler: fn()) -> Option<Self> {
        Self::new(
            web_sys::window().map(Into::into),
            "resize",
            false,
            move |_| handler(),
        )
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.target.remove_event_listener_with_callback_and_bool(
            self.event,
            self.cb.as_ref().unchecked_ref(),
            self.capture,
        );
    }
}

/// Calls `handler` when the grid or the badge changes size; disconnects on drop.
struct ResizeWatch {
    observer: web_sys::ResizeObserver,
    _cb: Closure<dyn FnMut()>,
}

impl ResizeWatch {
    fn new(handler: fn()) -> Option<Self> {
        let doc = web_sys::window()?.document()?;
        let cb = Closure::<dyn FnMut()>::new(handler);
        let observer = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()).ok()?;
        for id in ["grid-container", BADGE_ID] {
            if let Some(el) = doc.get_element_by_id(id) {
                observer.observe(&el);
            }
        }
        Some(Self { observer, _cb: cb })
    }
}

impl Drop for ResizeWatch {
    fn drop(&mut self) {
        self.observer.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::CANVAS_LIMIT;

    fn ids(prefix: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{i}")).collect()
    }

    fn person(session_id: &str, name: Option<&str>) -> OverflowParticipant {
        OverflowParticipant {
            session_id: session_id.to_string(),
            name: name.map(str::to_string),
            user_id: Some(format!("{session_id}@example.com")),
        }
    }

    /// The meeting view's source with every whitespace run removed, so the
    /// call-site pins below survive reformatting.
    fn attendants_source() -> String {
        include_str!("attendants.rs").split_whitespace().collect()
    }

    #[test]
    fn hidden_is_every_peer_and_mock_not_rendered() {
        let peers = ids("", 5);
        let mocks = ids("mock-", 3);
        let shown = [peers[0].clone(), mocks[1].clone()];
        let off = [peers[3].clone()];
        let hidden = hidden_participants(&peers, &mocks, &[&shown, &[], &off]);
        assert_eq!(hidden, ["1", "2", "4", "mock-0", "mock-2"]);
    }

    #[test]
    fn hidden_keeps_peers_beyond_the_canvas_limit() {
        let peers = ids("", CANVAS_LIMIT + 5);
        let rendered = &peers[..13];
        let hidden = hidden_participants(&peers, &[], &[rendered]);
        assert_eq!(hidden.len(), CANVAS_LIMIT + 5 - 13);
        assert!(hidden.contains(&peers[CANVAS_LIMIT + 4].as_str()));
    }

    #[test]
    fn the_meeting_view_lists_uncapped_peers_and_badges_the_list_length() {
        let src = attendants_source();
        let call = concat!(
            "hidden_participants(&display_peers,&mock_ids,",
            "&[&visible_tiles,&avatar_tiles,&camera_off_tiles],)"
        );
        assert!(
            src.contains(call),
            "the hidden list must come from the uncapped `display_peers` and all three rendered groups"
        );
        assert!(
            src.contains("GridOverflowBadge{overflow_count:overflow_hidden.len(),"),
            "the badge must show the length of the list it opens"
        );
        assert!(
            src.contains("describe(&overflow_hidden,"),
            "the list must be built from the same hidden set as the badge count"
        );
    }

    #[test]
    fn the_meeting_view_escape_closes_the_list_first_and_its_click_leaves_it_alone() {
        let src = attendants_source();
        let head = concat!(
            "ifkey==Key::Escape{ifoverflow_list_open(){evt.stop_propagation();",
            "evt.prevent_default();overflow_list_open.set(false);",
            "focus_element_by_id(crate::components::overflow_participants::BADGE_ID);",
            "}elseifoverflow_menu_open(){"
        );
        assert!(
            src.contains(head),
            "Escape must close the list before any other surface"
        );
        let click_at = src
            .find(r#"id:"main-container",class:"meeting-page",onclick:"#)
            .expect("the light-dismiss handler");
        let click = &src[click_at..];
        let click = &click[..click.find("onkeydown:").expect("the Escape chain follows")];
        assert!(
            !click.contains("overflow_list_open"),
            "the background light-dismiss must leave the list to its own handlers"
        );
    }

    #[test]
    fn opening_the_list_closes_only_the_transient_popovers() {
        let raw = include_str!("attendants.rs");
        let decl = "let close_transient_popovers: EventHandler<()> = use_callback(";
        let decl_at = raw
            .find(decl)
            .expect("a stable `use_callback`, not a prop-site closure");
        assert!(
            decl_at
                < raw
                    .find("    if !meeting_joined() {")
                    .expect("pre-join early return"),
            "the hook must run before the pre-join early return"
        );
        let src = attendants_source();
        let body = concat!(
            "letclose_transient_popovers:EventHandler<()>=use_callback(move|_:()|{",
            "density_open.set(false);reactions_open.set(false);dock_menu_open.set(false);",
            "mock_peers_open.set(false);share_menu_open.set(false);overflow_menu_open.set(false);",
            "meeting_timer_open.set(false);meeting_options_open.set(false);",
            "recents_open.set(false);});"
        );
        assert!(
            src.contains(body),
            "opening closes exactly these popovers, never the peer list or diagnostics"
        );
        assert!(src.contains("on_open:close_transient_popovers,"));
    }

    #[test]
    fn describe_never_resolves_a_mock_and_falls_back_to_the_session_id() {
        let rows = describe(
            &["7", "8", "mock-0"],
            |sid| (sid == "7").then(|| "ann@example.com".to_string()),
            |sid| (sid != "mock-0").then(|| format!("name-{sid}")),
        );
        assert_eq!(rows[0].user_id.as_deref(), Some("ann@example.com"));
        assert_eq!(rows[1].user_id.as_deref(), Some("8"));
        assert_eq!(rows[2].user_id, None);
        assert_eq!(rows[2].name, None);
    }

    #[test]
    fn label_falls_back_to_participant_never_the_user_id() {
        assert_eq!(person("1", None).label(), "Participant");
        assert_eq!(person("1", Some("")).label(), "Participant");
        assert_eq!(person("1", Some("Ann")).label(), "Ann");
    }

    #[test]
    fn rows_sort_case_insensitively_with_a_session_tiebreak() {
        let mut rows = [
            person("3", Some("bob")),
            person("2", None),
            person("9", Some("Alice")),
            person("1", None),
            person("4", Some("Bob")),
        ];
        rows.sort_by_cached_key(display_name_key);
        let order: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(order, ["9", "3", "4", "1", "2"]);
    }

    fn badge_at(left: f64, top: f64) -> Rect {
        Rect {
            left,
            top,
            right: left + 200.0,
            bottom: top + 120.0,
        }
    }

    #[test]
    fn list_opens_above_the_badge_right_aligned() {
        let at = list_placement(
            (1280.0, 800.0),
            Some(badge_at(900.0, 500.0)),
            (320.0, 300.0),
        );
        assert_eq!(at, Some((1100.0 - 320.0, 500.0 - 8.0 - 300.0)));
    }

    #[test]
    fn list_opens_below_the_badge_when_only_below_fits() {
        let at = list_placement((1280.0, 800.0), Some(badge_at(0.0, 100.0)), (320.0, 300.0));
        assert_eq!(at, Some((8.0, 220.0 + 8.0)));
    }

    #[test]
    fn list_clamps_into_the_viewport() {
        let size = (320.0, 300.0);
        assert_eq!(
            list_placement((1280.0, 800.0), Some(badge_at(1200.0, 500.0)), size),
            Some((1280.0 - 320.0 - 8.0, 192.0))
        );
        assert_eq!(
            list_placement((700.0, 200.0), Some(badge_at(400.0, 50.0)), size),
            Some((600.0 - 320.0, 8.0))
        );
    }

    #[test]
    fn narrow_viewport_or_missing_badge_uses_the_sheet() {
        let size = (320.0, 300.0);
        assert_eq!(
            list_placement((639.0, 800.0), Some(badge_at(300.0, 500.0)), size),
            None
        );
        assert_eq!(list_placement((1280.0, 800.0), None, size), None);
        assert!(list_placement((640.0, 800.0), Some(badge_at(300.0, 500.0)), size).is_some());
    }

    #[test]
    fn focus_order_walks_outward_next_before_previous() {
        let rows = ids("r", 4);
        assert_eq!(
            focus_order_from(&rows, 1),
            ["r1", "r2", "r0", "r3", CLOSE_ID]
        );
        assert_eq!(
            focus_order_from(&rows, 3),
            ["r3", "r2", "r1", "r0", CLOSE_ID]
        );
        assert_eq!(focus_order_from(&rows[..1], 0), ["r0", CLOSE_ID]);
    }

    #[test]
    fn arrows_wrap_through_close_and_home_end_stay_on_the_rows() {
        let rows = ids("r", 3);
        let at = |cur: Option<&str>, nav| nav_target(&rows, cur, nav);
        assert_eq!(at(Some("r0"), Nav::Next), "r1");
        assert_eq!(at(Some("r2"), Nav::Next), CLOSE_ID);
        assert_eq!(at(Some(CLOSE_ID), Nav::Next), "r0");
        assert_eq!(at(Some("r0"), Nav::Prev), CLOSE_ID);
        assert_eq!(at(None, Nav::Next), "r0");
        assert_eq!(at(None, Nav::Prev), CLOSE_ID);
        assert_eq!(at(Some(CLOSE_ID), Nav::First), "r0");
        assert_eq!(at(Some("r0"), Nav::Last), "r2");
        assert_eq!(at(Some(CLOSE_ID), Nav::Last), "r2");
        assert_eq!(nav_target(&[], None, Nav::Last), CLOSE_ID);
        assert_eq!(nav_target(&[], Some(CLOSE_ID), Nav::Next), CLOSE_ID);
    }
}
