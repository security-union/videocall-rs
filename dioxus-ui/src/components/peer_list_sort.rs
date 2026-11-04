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

//! Participant-panel (roster) ordering, issue 3123. Only the roster uses this;
//! the video grid keeps its own order.

use crate::components::co_hosts::HostRole;
use crate::local_storage;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{Element, Event, EventTarget, KeyboardEvent};

/// `localStorage` key for the chosen [`RosterSort`].
pub const SORT_PREF_KEY: &str = "vc_peer_list_sort";

/// How the roster orders the rows below the pinned self row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RosterSort {
    #[default]
    Name,
    Joined,
    Recent,
    Role,
    Hands,
    Unmuted,
    Camera,
}

impl RosterSort {
    pub const ALL: [RosterSort; 7] = [
        RosterSort::Name,
        RosterSort::Joined,
        RosterSort::Recent,
        RosterSort::Role,
        RosterSort::Hands,
        RosterSort::Unmuted,
        RosterSort::Camera,
    ];

    pub fn value(self) -> &'static str {
        match self {
            RosterSort::Name => "name",
            RosterSort::Joined => "joined",
            RosterSort::Recent => "recent",
            RosterSort::Role => "role",
            RosterSort::Hands => "hands",
            RosterSort::Unmuted => "unmuted",
            RosterSort::Camera => "camera",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RosterSort::Name => "Name",
            RosterSort::Joined => "Join order",
            RosterSort::Recent => "Recently joined",
            RosterSort::Role => "Role",
            RosterSort::Hands => "Raised hands",
            RosterSort::Unmuted => "Unmuted",
            RosterSort::Camera => "Camera on",
        }
    }

    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.value() == value)
    }
}

/// The persisted mode; [`RosterSort::Name`] when absent or unreadable.
pub fn load_sort_pref() -> RosterSort {
    local_storage::load_json(SORT_PREF_KEY, RosterSort::default())
}

pub fn save_sort_pref(mode: RosterSort) {
    local_storage::save_json(SORT_PREF_KEY, &mode);
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum NameChunk {
    Number(usize, String, usize),
    Text(String),
}

/// Case-insensitive, natural-number ordering key ("Guest 2" < "Guest 10").
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NameKey(Vec<NameChunk>);

impl NameKey {
    pub fn new(name: &str) -> Self {
        let mut chunks = Vec::new();
        let mut rest = name;
        while let Some(first) = rest.chars().next() {
            let numeric = first.is_ascii_digit();
            let end = rest
                .find(|c: char| c.is_ascii_digit() != numeric)
                .unwrap_or(rest.len());
            let (run, tail) = rest.split_at(end);
            chunks.push(if numeric {
                let value = run.trim_start_matches('0');
                NameChunk::Number(value.len(), value.to_owned(), run.len())
            } else {
                NameChunk::Text(run.to_lowercase())
            });
            rest = tail;
        }
        Self(chunks)
    }
}

/// Role rank for [`RosterSort::Role`], in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RosterRole {
    Host,
    CoHost,
    Participant,
    Guest,
}

impl RosterRole {
    pub fn of(role: Option<HostRole>, is_guest: bool) -> Self {
        match role {
            Some(HostRole::Host) => RosterRole::Host,
            Some(HostRole::CoHost) => RosterRole::CoHost,
            None if is_guest => RosterRole::Guest,
            None => RosterRole::Participant,
        }
    }
}

/// Each row's [`NameKey`], rebuilt only when that row's display name changes.
#[derive(Default)]
pub struct NameKeyCache(HashMap<String, (String, Rc<NameKey>)>);

impl NameKeyCache {
    pub fn key(&mut self, session_id: &str, name: &str) -> Rc<NameKey> {
        if let Some((_, key)) = self.0.get(session_id).filter(|(cached, _)| cached == name) {
            return key.clone();
        }
        let key = Rc::new(NameKey::new(name));
        self.0
            .insert(session_id.to_owned(), (name.to_owned(), key.clone()));
        key
    }

    /// Forget rows missing from `present`, once the cache has outgrown it.
    pub fn prune<'a>(&mut self, present: impl ExactSizeIterator<Item = &'a str>) {
        if self.0.len() > present.len() {
            let present: HashSet<&str> = present.collect();
            self.0.retain(|sid, _| present.contains(sid.as_str()));
        }
    }
}

/// Everything one row sorts by, resolved once per render.
#[derive(Clone, Debug, PartialEq)]
pub struct RosterSortKey {
    pub session_id: String,
    pub name: Rc<NameKey>,
    pub joined_at: f64,
    pub role: RosterRole,
    pub hand_position: Option<usize>,
    pub muted: bool,
    pub camera_on: bool,
}

/// First-seen time for `session_id`; a peer with no entry sorts as `0.0`, the
/// same fallback the video grid uses.
pub fn join_time_of(join_times: &HashMap<String, f64>, session_id: &str) -> f64 {
    join_times.get(session_id).copied().unwrap_or(0.0)
}

/// Total order for `mode`; ties fall to the name, then the session id.
pub fn compare_roster(mode: RosterSort, a: &RosterSortKey, b: &RosterSortKey) -> Ordering {
    let primary = match mode {
        RosterSort::Name => Ordering::Equal,
        RosterSort::Joined => a.joined_at.total_cmp(&b.joined_at),
        RosterSort::Recent => b.joined_at.total_cmp(&a.joined_at),
        RosterSort::Role => a.role.cmp(&b.role),
        RosterSort::Hands => (a.hand_position.is_none(), a.hand_position)
            .cmp(&(b.hand_position.is_none(), b.hand_position)),
        RosterSort::Unmuted => a.muted.cmp(&b.muted),
        RosterSort::Camera => b.camera_on.cmp(&a.camera_on),
    };
    primary
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.session_id.cmp(&b.session_id))
}

/// Sort `items` by their precomputed keys.
pub fn sort_roster<T>(mode: RosterSort, items: &mut [T], key: impl Fn(&T) -> &RosterSortKey) {
    items.sort_by(|a, b| compare_roster(mode, key(a), key(b)));
}

/// The last order the roster displayed, and the mode it was sorted by.
#[derive(Debug, Default)]
pub struct RosterOrderMemo {
    mode: RosterSort,
    session_ids: Vec<String>,
}

impl RosterOrderMemo {
    /// Display order for already-sorted rows. While `frozen` in one mode, rows
    /// still present keep their shown order and new rows follow in `sorted` order.
    pub fn arrange<T>(
        &mut self,
        mode: RosterSort,
        frozen: bool,
        mut sorted: Vec<T>,
        session_id: impl Fn(&T) -> &str,
    ) -> Vec<T> {
        if frozen && mode == self.mode {
            let shown = self.session_ids.iter().enumerate();
            let rank: HashMap<&str, usize> = shown.map(|(i, sid)| (sid.as_str(), i)).collect();
            sorted.sort_by_key(|item| rank.get(session_id(item)).copied().unwrap_or(usize::MAX));
        }
        self.mode = mode;
        let shown = sorted.iter().map(&session_id);
        if !self.session_ids.iter().map(String::as_str).eq(shown) {
            self.session_ids = sorted.iter().map(&session_id).map(str::to_owned).collect();
        }
        sorted
    }
}

type ListRef = Rc<RefCell<Option<Element>>>;

/// Whether the page's last input was a key press (Tab, or a key without
/// Meta/Ctrl/Alt), from capture-phase `document` listeners removed on drop.
pub struct KeyboardModality(Rc<[Cell<bool>; 2]>, Closure<dyn FnMut(Event)>, ListRef);

impl KeyboardModality {
    /// `release` runs when a press begun with keyboard focus in `list` lifts or cancels.
    pub fn listen<R>(list: ListRef, mut release: impl FnMut() -> R + 'static) -> Self {
        let state = Rc::new([true, false].map(Cell::new));
        let (shared, roster) = (state.clone(), list.clone());
        let listener = Closure::new(move |e: Event| match e.dyn_ref::<KeyboardEvent>() {
            Some(key)
                if key.key() != "Tab" && (key.meta_key() || key.ctrl_key() || key.alt_key()) => {}
            Some(_) => shared[0].set(true),
            None if e.type_() == "pointerdown" => shared[1].set(
                shared[0].replace(false)
                    && inside(&roster, gloo_utils::document().active_element()).is_some(),
            ),
            None if shared[1].replace(false) => drop(release()),
            None => {}
        });
        let modality = Self(state, listener, list);
        modality.register(true);
        modality
    }

    fn register(&self, add: bool) {
        let (doc, callback) = (gloo_utils::document(), self.1.as_ref().unchecked_ref());
        for event in ["keydown", "pointerdown", "pointerup", "pointercancel"] {
            let _ = match add {
                true => doc.add_event_listener_with_callback_and_bool(event, callback, true),
                false => doc.remove_event_listener_with_callback_and_bool(event, callback, true),
            };
        }
    }

    /// Keyboard input came last and focus is in the list, outside any row whose
    /// `data-sid` fails `present`; or a press that began in such a state is down.
    pub fn focus_within(&self, present: impl Fn(&str) -> bool) -> bool {
        let [kb, pressing] = &*self.0;
        let focused = kb.get().then(|| gloo_utils::document().active_element());
        let row = |el: &Element| el.closest("li[data-sid]").ok()??.get_attribute("data-sid");
        let held = inside(&self.2, focused.flatten())
            .is_some_and(|el| row(&el).is_none_or(|sid| present(&sid)));
        held || (pressing.get() && !kb.get())
    }

    /// Whether a `focusout` toward `next` leaves the list; going nowhere counts.
    pub fn focus_leaves(&self, next: Option<EventTarget>) -> bool {
        inside(&self.2, next.and_then(|t| t.dyn_into().ok())).is_none()
    }
}

impl Drop for KeyboardModality {
    fn drop(&mut self) {
        self.register(false);
    }
}

fn inside(list: &ListRef, node: Option<Element>) -> Option<Element> {
    let list = list.borrow();
    node.filter(|node| list.as_ref().is_some_and(|list| list.contains(Some(node))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sids(ids: &str) -> Vec<String> {
        ids.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn name_keys_ignore_case_and_order_numbers_naturally() {
        use Ordering::{Equal, Greater, Less};
        for (a, b, want) in [
            ("alice", "ALICE", Equal),
            ("émile", "ÉMILE", Equal),
            ("Guest 2", "Guest 10", Less),
            ("Guest 007", "Guest 8", Less),
            ("Guest 1", "Guest 01", Less),
            ("12345678901234567890123", "99", Greater),
        ] {
            assert_eq!(NameKey::new(a).cmp(&NameKey::new(b)), want, "{a} vs {b}");
        }
    }

    #[test]
    fn every_mode_sorts_by_its_key_then_name_then_session_id() {
        use HostRole::{CoHost, Host};
        let joined: HashMap<String, f64> = [("s1", 50.0), ("s3", 300.0), ("s4", 300.0)]
            .into_iter()
            .chain([("s5", 200.0), ("s6", 200.0)])
            .map(|(sid, at)| (sid.to_owned(), at))
            .collect();
        let mut names = NameKeyCache::default();
        let roster: Vec<RosterSortKey> = [
            ("s1", "Zara", Some(CoHost), false, Some(1), false, false),
            ("s2", "alice", None, true, None, true, false),
            ("s4", "bob", None, false, Some(2), true, true),
            ("s3", "Bob", None, false, None, true, false),
            ("s5", "Guest 10", Some(Host), false, None, false, false),
            ("s6", "Guest 2", None, false, None, true, true),
        ]
        .into_iter()
        .map(|(sid, name, role, guest, hand, mute, cam)| RosterSortKey {
            session_id: sid.to_owned(),
            name: names.key(sid, name),
            joined_at: join_time_of(&joined, sid),
            role: RosterRole::of(role, guest),
            hand_position: hand,
            muted: mute,
            camera_on: cam,
        })
        .collect();
        for (mode, want) in [
            (RosterSort::Name, "s2 s3 s4 s6 s5 s1"),
            (RosterSort::Joined, "s2 s1 s6 s5 s3 s4"),
            (RosterSort::Recent, "s3 s4 s6 s5 s1 s2"),
            (RosterSort::Role, "s5 s1 s3 s4 s6 s2"),
            (RosterSort::Hands, "s1 s4 s2 s3 s6 s5"),
            (RosterSort::Unmuted, "s5 s1 s2 s3 s4 s6"),
            (RosterSort::Camera, "s4 s6 s2 s3 s5 s1"),
        ] {
            let mut keys = roster.clone();
            sort_roster(mode, &mut keys, |k| k);
            let got: Vec<&str> = keys.iter().map(|k| k.session_id.as_str()).collect();
            assert_eq!(got.join(" "), want, "{mode:?}");
        }
    }

    #[test]
    fn name_keys_are_reused_until_the_name_changes_or_the_row_leaves() {
        let mut cache = NameKeyCache::default();
        let ann = cache.key("s1", "Ann");
        assert!(Rc::ptr_eq(&ann, &cache.key("s1", "Ann")));
        let anne = cache.key("s1", "Anne");
        assert!(!Rc::ptr_eq(&ann, &anne));
        cache.key("s2", "Bob");
        cache.prune(["s2"].into_iter());
        assert!(!Rc::ptr_eq(&anne, &cache.key("s1", "Anne")));
    }

    #[test]
    fn a_frozen_list_keeps_its_order_appends_joiners_and_drops_leavers_until_released() {
        let mut memo = RosterOrderMemo::default();
        for (mode, frozen, sorted, shown) in [
            (RosterSort::Name, false, "a b c", "a b c"),
            (RosterSort::Name, true, "c b a", "a b c"),
            (RosterSort::Name, true, "n2 c a n1", "a c n2 n1"),
            (RosterSort::Name, false, "c b a", "c b a"),
            (RosterSort::Joined, true, "b a c", "b a c"),
        ] {
            let arranged = memo.arrange(mode, frozen, sids(sorted), |s| s.as_str());
            assert_eq!(arranged, sids(shown), "{mode:?} frozen={frozen}");
        }
    }

    #[test]
    fn the_stored_form_round_trips_and_rejects_unknown_values() {
        assert_eq!(RosterSort::default(), RosterSort::Name);
        for mode in RosterSort::ALL {
            let stored = serde_json::to_string(&mode).unwrap();
            assert_eq!(stored, format!("\"{}\"", mode.value()));
            assert_eq!(RosterSort::from_value(mode.value()), Some(mode));
        }
        assert!(serde_json::from_str::<RosterSort>("\"bogus\"").is_err());
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use crate::components::peer_list::{PeerList, PeerListEntry, RosterLiveness};
    use crate::context::{AppearanceSettings, AppearanceSettingsCtx};
    use dioxus::prelude::{Element, *};
    use videocall_client::VideoCallClient;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::wasm_bindgen_test;

    const LOCAL: &str = "local-user";
    const BY_NAME: [&str; 3] = ["alice", "Bob", "Zara"];
    const BY_JOIN: [&str; 3] = ["Zara", "Bob", "alice"];
    const ALICE_FIRST: [&str; 3] = ["alice", "Zara", "Bob"];

    #[component]
    fn SortHarness(host: bool) -> Element {
        let client = use_hook(|| VideoCallClient::new_for_test(LOCAL));
        use_context_provider(|| client.clone());
        use_context_provider(|| Rc::new(RosterLiveness::default()));
        let appearance = use_signal(AppearanceSettings::default);
        use_context_provider(|| AppearanceSettingsCtx(appearance));
        let peers = [
            ("s1", "Zara", 100.0),
            ("s2", "alice", 300.0),
            ("s3", "Bob", 200.0),
        ];
        let mut join_times =
            use_signal(|| HashMap::from(peers.map(|(sid, _, at)| (sid.to_owned(), at))));
        let (mut zara_left, mut shown) = (use_signal(|| false), use_signal(|| true));
        let present = peers.into_iter().filter(|p| !(zara_left() && p.0 == "s1"));
        rsx! {
            button {
                "data-testid": "flip-alice-join",
                onclick: move |_| {
                    join_times.write().entry("s2".to_owned()).and_modify(|at| *at = -*at);
                },
            }
            button { "data-testid": "zara-leaves", onclick: move |_| zara_left.set(true) }
            button { "data-testid": "unmount", onclick: move |_| shown.set(false) }
            if shown() { PeerList {
                peers: present.map(|(sid, uid, _)| PeerListEntry { session_id: sid.into(), user_id: uid.into() }).collect::<Vec<_>>(),
                onclose: move |_| {},
                room_id: "room-3123".to_string(),
                host_user_id: host.then(|| LOCAL.to_string()),
                peer_join_time: join_times,
            } }
        }
    }

    async fn settle() {
        for _ in 0..4 {
            let frame = js_sys::Promise::new(&mut |resolve, _| {
                let _ = gloo_utils::window().request_animation_frame(&resolve);
            });
            let _ = JsFuture::from(frame).await;
        }
    }

    fn storage() -> web_sys::Storage {
        gloo_utils::window().local_storage().unwrap().unwrap()
    }

    async fn mount_roster(stored_pref: Option<&str>, host: bool) -> web_sys::Element {
        match stored_pref {
            Some(raw) => storage().set_item(SORT_PREF_KEY, raw).unwrap(),
            None => local_storage::remove_item(SORT_PREF_KEY),
        }
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new_with_props(SortHarness, SortHarnessProps { host }),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        settle().await;
        mount
    }

    async fn unmount(mount: web_sys::Element) {
        click(&mount, "[data-testid='unmount']").await;
        mount.remove();
        local_storage::remove_item(SORT_PREF_KEY);
    }

    fn find(root: &web_sys::Element, selector: &str) -> web_sys::HtmlElement {
        root.query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("{selector} must render"))
            .unchecked_into()
    }

    const CHANGE: &str = "el.dispatchEvent(new Event('change', { bubbles: true }))";
    const POINTER_DOWN: &str =
        "el.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }))";
    const TAB_DOWN: &str =
        "el.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', altKey: true, shiftKey: true }))";
    const META_DOWN: &str =
        "el.dispatchEvent(new KeyboardEvent('keydown', { key: 'Meta', metaKey: true }))";
    const SWALLOW_FOCUSOUT: &str = "window.stop ??= (e) => e.stopImmediatePropagation();
        document.addEventListener('focusout', window.stop, true);";
    const ALLOW_FOCUSOUT: &str = "document.removeEventListener('focusout', window.stop, true);";

    fn run(el: &web_sys::Element, script: &str) {
        let script = js_sys::Function::new_with_args("el", script);
        script.call1(&JsValue::NULL, el).unwrap();
    }

    fn rows(mount: &web_sys::Element) -> Vec<web_sys::Element> {
        let list = mount.query_selector_all(".peer-list ul > li").unwrap();
        (0..list.length())
            .filter_map(|i| list.item(i)?.dyn_into().ok())
            .collect()
    }

    fn peer_names(mount: &web_sys::Element) -> Vec<String> {
        let name = |li: &web_sys::Element| find(li, ".peer_item_name_container").inner_text();
        rows(mount)[1..]
            .iter()
            .map(|li| name(li).trim().to_owned())
            .collect()
    }

    fn sort_control(mount: &web_sys::Element) -> web_sys::HtmlSelectElement {
        find(mount, "#peer-list-sort").unchecked_into()
    }

    async fn choose(mount: &web_sys::Element, value: &str) {
        sort_control(mount).set_value(value);
        run(&sort_control(mount), CHANGE);
        settle().await;
    }

    #[wasm_bindgen_test]
    async fn rows_default_to_name_order_and_a_chosen_sort_is_applied_and_saved() {
        let mount = mount_roster(None, false).await;
        assert_eq!(sort_control(&mount).value(), "name");
        assert_eq!(peer_names(&mount), BY_NAME);
        find(&rows(&mount)[0], ".peer_item_edit_btn");
        choose(&mount, "joined").await;
        assert_eq!(peer_names(&mount), BY_JOIN);
        let stored = storage().get_item(SORT_PREF_KEY).unwrap();
        assert_eq!(stored.as_deref(), Some("\"joined\""));
        unmount(mount).await;
    }

    #[wasm_bindgen_test]
    async fn a_stored_sort_is_restored_and_a_corrupt_one_falls_back_to_name() {
        for (stored, value, names) in [
            ("\"joined\"", "joined", BY_JOIN),
            ("not json", "name", BY_NAME),
        ] {
            let mount = mount_roster(Some(stored), false).await;
            assert_eq!(sort_control(&mount).value(), value);
            assert_eq!(peer_names(&mount), names);
            unmount(mount).await;
        }
    }

    #[wasm_bindgen_test]
    async fn a_moved_row_keeps_its_dom_node() {
        let mount = mount_roster(None, false).await;
        let alice_row = rows(&mount)[1].clone();
        choose(&mount, "joined").await;
        assert!(rows(&mount)[3].is_same_node(Some(&alice_row)));
        unmount(mount).await;
    }

    async fn click(mount: &web_sys::Element, selector: &str) {
        find(mount, selector).click();
        settle().await;
    }

    async fn flip_alice(mount: &web_sys::Element) {
        click(mount, "[data-testid='flip-alice-join']").await;
    }

    fn keyboard_focus_on_the_self_row(mount: &web_sys::Element) -> web_sys::HtmlElement {
        run(&gloo_utils::body(), TAB_DOWN);
        let edit = find(mount, ".peer-list ul .peer_item_edit_btn");
        edit.focus().unwrap();
        edit
    }

    #[wasm_bindgen_test]
    async fn option_tab_focus_holds_the_order_even_after_a_pointerdown_on_a_row() {
        let mount = mount_roster(Some("\"joined\""), false).await;
        run(&find(&mount, ".peer_item_name_container"), POINTER_DOWN);
        let edit = keyboard_focus_on_the_self_row(&mount);
        flip_alice(&mount).await;
        assert_eq!(peer_names(&mount), BY_JOIN, "held");
        edit.blur().unwrap();
        settle().await;
        assert_eq!(peer_names(&mount), ALICE_FIRST, "released");
        unmount(mount).await;
    }

    #[wasm_bindgen_test]
    async fn a_pointer_over_the_list_does_not_hold_the_order_but_an_open_row_menu_does() {
        let mount = mount_roster(Some("\"joined\""), true).await;
        let enter = "el.dispatchEvent(new PointerEvent('pointerenter'))";
        run(&find(&mount, ".peer-list ul"), enter);
        flip_alice(&mount).await;
        assert_eq!(peer_names(&mount), ALICE_FIRST, "pointer over the list");
        let kebab = ".peer-list li:nth-child(2) .peer_item_menu_btn";
        click(&mount, kebab).await;
        flip_alice(&mount).await;
        assert_eq!(peer_names(&mount), ALICE_FIRST, "held");
        click(&mount, kebab).await;
        assert_eq!(peer_names(&mount), BY_JOIN, "released");
        unmount(mount).await;
    }

    #[wasm_bindgen_test]
    async fn a_pointer_menu_action_then_a_modifier_key_does_not_hold() {
        let mount = mount_roster(Some("\"joined\""), true).await;
        let kebab = find(&mount, ".peer-list li:nth-child(4) .peer_item_menu_btn");
        run(&kebab, POINTER_DOWN);
        kebab.click();
        settle().await;
        let item = find(&mount, ".peer_item_context_menu .context-menu-item");
        run(&item, POINTER_DOWN);
        item.click();
        settle().await;
        let focused = gloo_utils::document().active_element();
        assert!(focused.is_some_and(|el| el.is_same_node(Some(&kebab))));
        run(&gloo_utils::body(), META_DOWN);
        flip_alice(&mount).await;
        assert_eq!(peer_names(&mount), ALICE_FIRST);
        unmount(mount).await;
    }

    #[wasm_bindgen_test]
    async fn the_render_that_removes_the_focused_row_is_not_held() {
        let mount = mount_roster(Some("\"joined\""), true).await;
        run(&gloo_utils::body(), TAB_DOWN);
        let zara_kebab = find(&mount, ".peer-list li:nth-child(2) .peer_item_menu_btn");
        zara_kebab.focus().unwrap();
        flip_alice(&mount).await;
        assert_eq!(peer_names(&mount), BY_JOIN, "held");
        run(&gloo_utils::body(), SWALLOW_FOCUSOUT);
        click(&mount, "[data-testid='zara-leaves']").await;
        run(&gloo_utils::body(), ALLOW_FOCUSOUT);
        assert_eq!(peer_names(&mount), ["alice", "Bob"]);
        unmount(mount).await;
    }

    #[wasm_bindgen_test]
    async fn a_render_during_a_press_keeps_the_hold_until_the_press_lifts_or_cancels() {
        for end in ["pointerup", "pointercancel"] {
            let mount = mount_roster(Some("\"joined\""), true).await;
            let _edit = keyboard_focus_on_the_self_row(&mount);
            let kebab = find(&mount, ".peer-list li:nth-child(4) .peer_item_menu_btn");
            run(&kebab, POINTER_DOWN);
            flip_alice(&mount).await;
            assert_eq!(
                peer_names(&mount),
                BY_JOIN,
                "held through a mid-press render"
            );
            let lift = format!("el.dispatchEvent(new PointerEvent('{end}', {{ bubbles: true }}))");
            run(&kebab, &lift);
            settle().await;
            assert_eq!(peer_names(&mount), ALICE_FIRST, "released on {end}");
            unmount(mount).await;
        }
    }
}
