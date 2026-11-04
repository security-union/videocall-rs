// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 2792: how a shared-content tile is shown (Tile, Enlarged, Pinned or
//! Detached) and the sticky preference that picks the mode of the next share.

use std::collections::HashMap;

use dioxus::prelude::*;

use crate::components::canvas_generator::{PinnedTile, PinnedTileKind};
use crate::components::pin_order;
use crate::context::ScreenZoomState;

/// Detach / zoom key of the local user's own share tile.
pub const OWN_SHARE_KEY: &str = "__own_share__";
/// Key of the one detached window that shows received shares.
pub const RECEIVED_WINDOW_KEY: &str = "__received_share_window__";
pub const CTA_TIMEOUT_MS: u32 = 12_000;
pub const ENLARGED_DEFAULT_MIN_WIDTH_PX: f64 = 720.0;
pub const ENLARGED_DEFAULT_MIN_HEIGHT_PX: f64 = 540.0;
pub const CTA_HINT: &str = "You can open it in a separate window.";
pub const DETACH_FAILED: &str =
    "Could not open a separate window. Allow pop-ups for this site and try again.";
pub const MIRROR_GUARD_ON: &str =
    "Preview of your shared content is hidden while you present your entire screen";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ShareViewMode {
    #[default]
    Tile,
    Enlarged,
    Pinned,
    Detached,
}

impl ShareViewMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareViewMode::Tile => "tile",
            ShareViewMode::Enlarged => "enlarged",
            ShareViewMode::Pinned => "pinned",
            ShareViewMode::Detached => "detached",
        }
    }

    pub fn parse(stored: Option<&str>) -> Option<Self> {
        match stored? {
            "tile" => Some(ShareViewMode::Tile),
            "enlarged" => Some(ShareViewMode::Enlarged),
            "pinned" => Some(ShareViewMode::Pinned),
            "detached" => Some(ShareViewMode::Detached),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareOrigin {
    Received,
    Own,
}

impl ShareOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareOrigin::Received => "received",
            ShareOrigin::Own => "own",
        }
    }

    pub fn pref_key(self) -> &'static str {
        match self {
            ShareOrigin::Received => "vc_share_view_mode",
            ShareOrigin::Own => "vc_own_share_view_mode",
        }
    }

    pub fn subject(self) -> &'static str {
        match self {
            ShareOrigin::Received => "Shared content",
            ShareOrigin::Own => "Your shared content",
        }
    }

    pub fn other(self) -> Self {
        match self {
            ShareOrigin::Received => ShareOrigin::Own,
            ShareOrigin::Own => ShareOrigin::Received,
        }
    }

    /// The mode a share opens in while no choice is stored, for a meeting area
    /// `width` CSS px wide in a window `height` CSS px tall.
    pub fn default_mode(self, (width, height): (f64, f64)) -> ShareViewMode {
        let wide = width >= ENLARGED_DEFAULT_MIN_WIDTH_PX
            && width > height
            && height >= ENLARGED_DEFAULT_MIN_HEIGHT_PX;
        match self {
            ShareOrigin::Received if wide => ShareViewMode::Enlarged,
            _ => ShareViewMode::Tile,
        }
    }

    pub fn stored_mode(self, stored: Option<&str>, meeting: (f64, f64)) -> ShareViewMode {
        ShareViewMode::parse(stored).unwrap_or(self.default_mode(meeting))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ShareBase {
    #[default]
    Tile,
    Enlarged,
}

impl ShareBase {
    pub fn mode(self) -> ShareViewMode {
        match self {
            ShareBase::Tile => ShareViewMode::Tile,
            ShareBase::Enlarged => ShareViewMode::Enlarged,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CtaState {
    #[default]
    Hidden,
    Shown,
    Suggested,
}

/// In-session state of one share origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ShareSlot {
    pub base: ShareBase,
    pub pre_detach: Option<ShareViewMode>,
    pub cta: CtaState,
    pub guard: bool,
    /// The presenter set `guard` with Hide or Show preview.
    pub guard_chosen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ShareSlots {
    pub received: ShareSlot,
    pub own: ShareSlot,
}

impl ShareSlots {
    pub fn get(&self, origin: ShareOrigin) -> ShareSlot {
        match origin {
            ShareOrigin::Received => self.received,
            ShareOrigin::Own => self.own,
        }
    }

    pub fn get_mut(&mut self, origin: ShareOrigin) -> &mut ShareSlot {
        match origin {
            ShareOrigin::Received => &mut self.received,
            ShareOrigin::Own => &mut self.own,
        }
    }
}

impl ShareOrigin {
    pub fn window_key(self) -> &'static str {
        match self {
            ShareOrigin::Received => RECEIVED_WINDOW_KEY,
            ShareOrigin::Own => OWN_SHARE_KEY,
        }
    }
}

/// The received shares on screen. `primary` holds the received slot, on the
/// stage or, while `window` is open, in the received window; `demoted` are the
/// other live shares shown as tiles.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReceivedStage {
    pub primary: Option<String>,
    pub demoted: Vec<String>,
    pub window: bool,
    pub primary_target: Option<ShareTarget>,
    pub toast: Option<(u32, String)>,
    pub toast_seq: u32,
    seen: Vec<String>,
}

impl ReceivedStage {
    /// The stage once the received sharers are `sharers`, oldest first.
    pub fn follow(&self, sharers: &[String]) -> Self {
        let live = |k: &String| sharers.contains(k);
        let mut next = Self {
            primary: self.primary.clone().filter(live),
            demoted: self.demoted.iter().filter(|k| live(k)).cloned().collect(),
            seen: sharers.to_vec(),
            ..self.clone()
        };
        if let Some(newest) = sharers.iter().rfind(|k| !self.seen.contains(k)) {
            next.primary = Some(newest.clone());
        }
        if next.primary.is_none() {
            next.primary = sharers.last().cloned();
            next.demoted.retain(|k| Some(k) != next.primary.as_ref());
        }
        if self.window {
            for key in sharers {
                if Some(key) != next.primary.as_ref() && !next.demoted.contains(key) {
                    next.demoted.push(key.clone());
                }
            }
        }
        next
    }

    /// A demoted share takes the received slot; the share holding it takes
    /// the demoted share's place. False when `key` is no longer demoted.
    pub fn take_slot(&mut self, key: &str) -> bool {
        let Some(at) = self.demoted.iter().position(|k| k == key) else {
            return false;
        };
        self.demoted.remove(at);
        if let Some(shown) = self.primary.replace(key.to_string()) {
            self.demoted.insert(at, shown);
        }
        true
    }

    /// The share the received window shows.
    pub fn bound_target(&self) -> Option<ShareTarget> {
        let key = self.primary.as_ref().filter(|_| self.window)?;
        self.primary_target.clone().filter(|t| &t.key == key)
    }
}

pub fn effective_mode(detached: bool, pinned: bool, base: ShareBase) -> ShareViewMode {
    if detached {
        ShareViewMode::Detached
    } else if pinned {
        ShareViewMode::Pinned
    } else {
        base.mode()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareLayout {
    None,
    Tile,
    Enlarged,
}

impl ShareLayout {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareLayout::None => "none",
            ShareLayout::Tile => "tile",
            ShareLayout::Enlarged => "enlarged",
        }
    }
}

/// `tiles` holds `(base, effective mode)` for every share tile on screen.
pub fn share_layout(tiles: &[(ShareBase, ShareViewMode)]) -> ShareLayout {
    if tiles.is_empty() {
        ShareLayout::None
    } else if tiles
        .iter()
        .any(|(b, m)| *b == ShareBase::Enlarged && *m != ShareViewMode::Detached)
    {
        ShareLayout::Enlarged
    } else {
        ShareLayout::Tile
    }
}

/// Normal-grid cells reserved for share tiles.
pub fn share_grid_cells(layout: ShareLayout, modes: &[ShareViewMode]) -> usize {
    if layout != ShareLayout::Tile {
        return 0;
    }
    modes
        .iter()
        .filter(|m| **m != ShareViewMode::Detached)
        .count()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub enum ShareAction {
    Enlarge,
    Pin,
    Detach,
    Reattach { opened: bool },
    Displaced,
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PinOp {
    #[default]
    Keep,
    PinThis,
    Unpin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DetachOp {
    #[default]
    Keep,
    Open,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareAnnounce {
    Enlarged,
    ReturnedToGrid,
    Pinned,
    Unpinned,
    Returned,
    DetachFailed,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ShareSnapshot {
    pub base: ShareBase,
    pub pinned: bool,
    pub detached: bool,
    pub other_detached: bool,
    pub pre_detach: Option<ShareViewMode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ShareTransition {
    pub base: ShareBase,
    pub pin: PinOp,
    pub detach: DetachOp,
    pub pre_detach: Option<ShareViewMode>,
    pub pref: Option<ShareViewMode>,
    pub demote_other: bool,
    pub reattach_other: bool,
    pub hide_cta: bool,
    pub announce: Option<ShareAnnounce>,
}

fn land(s: &ShareSnapshot, t: &mut ShareTransition) -> ShareViewMode {
    t.pre_detach = None;
    match s.pre_detach.unwrap_or(s.base.mode()) {
        ShareViewMode::Pinned => {
            t.base = ShareBase::Tile;
            t.pin = PinOp::PinThis;
            ShareViewMode::Pinned
        }
        ShareViewMode::Enlarged => {
            t.base = ShareBase::Enlarged;
            t.demote_other = true;
            ShareViewMode::Enlarged
        }
        ShareViewMode::Tile => {
            t.base = ShareBase::Tile;
            ShareViewMode::Tile
        }
        _ => s.base.mode(),
    }
}

/// The view a received share opens in after the user closes a waiting window.
#[cfg(any(test, target_arch = "wasm32"))]
pub fn closed_waiting_pref(slot: ShareSlot) -> ShareViewMode {
    let s = ShareSnapshot {
        base: slot.base,
        pre_detach: slot.pre_detach,
        ..ShareSnapshot::default()
    };
    land(&s, &mut ShareTransition::default())
}

pub fn next_share_view(s: ShareSnapshot, action: ShareAction) -> ShareTransition {
    use ShareViewMode as M;
    let mode = effective_mode(s.detached, s.pinned, s.base);
    let mut t = ShareTransition {
        base: s.base,
        pre_detach: s.pre_detach,
        ..ShareTransition::default()
    };
    match (mode, action) {
        (M::Tile | M::Pinned, ShareAction::Enlarge) => {
            t.base = ShareBase::Enlarged;
            if mode == M::Pinned {
                t.pin = PinOp::Unpin;
            }
            t.demote_other = true;
            t.hide_cta = true;
            t.pref = Some(M::Enlarged);
            t.announce = Some(ShareAnnounce::Enlarged);
        }
        (M::Enlarged, ShareAction::Enlarge) => {
            t.base = ShareBase::Tile;
            t.hide_cta = true;
            t.pref = Some(M::Tile);
            t.announce = Some(ShareAnnounce::ReturnedToGrid);
        }
        (M::Tile | M::Enlarged, ShareAction::Pin) => {
            t.base = ShareBase::Tile;
            t.pin = PinOp::PinThis;
            t.hide_cta = true;
            t.pref = Some(M::Pinned);
            t.announce = Some(ShareAnnounce::Pinned);
        }
        (M::Pinned, ShareAction::Pin) => {
            t.base = ShareBase::Tile;
            t.pin = PinOp::Unpin;
            t.hide_cta = true;
            t.pref = Some(M::Tile);
            t.announce = Some(ShareAnnounce::Unpinned);
        }
        (M::Tile | M::Enlarged | M::Pinned, ShareAction::Detach) => {
            t.detach = DetachOp::Open;
            t.pre_detach = Some(mode);
            if mode == M::Pinned {
                t.pin = PinOp::Unpin;
            }
            t.reattach_other = s.other_detached;
        }
        (M::Detached, ShareAction::Detach) => {
            t.detach = DetachOp::Close;
        }
        (_, ShareAction::Reattach { opened }) => {
            let landed = land(&s, &mut t);
            if opened {
                t.pref = Some(landed);
                t.announce = Some(ShareAnnounce::Returned);
            } else {
                t.announce = Some(ShareAnnounce::DetachFailed);
            }
        }
        (_, ShareAction::Displaced) => {
            land(&s, &mut t);
        }
        (_, ShareAction::Ended) => {
            if s.pinned {
                t.pin = PinOp::Unpin;
            }
            t.pre_detach = None;
            t.announce = Some(ShareAnnounce::Stopped);
        }
        _ => {}
    }
    t
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ArrivalPlan {
    pub base: ShareBase,
    pub pin: bool,
    pub cta: CtaState,
    pub demote_other: bool,
    pub detach_now: bool,
}

/// Section 6: the stored pref applied to a new share. A popup needs transient
/// user activation, so a Detached pref opens directly only from a click, and
/// otherwise opens in `fallback`.
pub fn arrival_plan(
    pref: ShareViewMode,
    fallback: ShareViewMode,
    detach_supported: bool,
    user_activation: bool,
) -> ArrivalPlan {
    let mut plan = ArrivalPlan::default();
    match pref {
        ShareViewMode::Tile => {}
        ShareViewMode::Enlarged => {
            plan.base = ShareBase::Enlarged;
            plan.demote_other = true;
        }
        ShareViewMode::Pinned => plan.pin = true,
        ShareViewMode::Detached if detach_supported && user_activation => plan.detach_now = true,
        ShareViewMode::Detached => {
            plan = arrival_plan(fallback, ShareViewMode::Tile, false, false);
            if detach_supported {
                plan.cta = CtaState::Shown;
            }
        }
    }
    plan
}

pub fn own_arrival_plan(pref: ShareViewMode, detach_supported: bool, guard: bool) -> ArrivalPlan {
    if guard {
        ArrivalPlan::default()
    } else {
        arrival_plan(pref, ShareViewMode::Tile, detach_supported, false)
    }
}

pub fn mirror_guard_default(display_surface: Option<&str>) -> bool {
    display_surface == Some("monitor")
}

/// The pref applied by "Show preview". The click is a user activation, but a
/// whole-screen capture would record its own detached window.
pub fn show_preview_plan(
    pref: ShareViewMode,
    detach_supported: bool,
    display_surface: Option<&str>,
) -> ArrivalPlan {
    arrival_plan(
        pref,
        ShareViewMode::Tile,
        detach_supported,
        !mirror_guard_default(display_surface),
    )
}

/// View actions on the own tile are refused while its preview is hidden.
pub fn view_action_allowed(origin: ShareOrigin, slots: &ShareSlots) -> bool {
    !(origin == ShareOrigin::Own && slots.own.guard)
}

/// Whether a share tile renders in the split's peer panel. A detached tile,
/// and a Tile or Pinned tile outside the split, keep their previous parent.
pub fn share_in_panel(split_layout: bool, mode: ShareViewMode, was_in_panel: bool) -> bool {
    match mode {
        ShareViewMode::Detached => was_in_panel,
        ShareViewMode::Tile | ShareViewMode::Pinned => split_layout || was_in_panel,
        ShareViewMode::Enlarged => false,
    }
}

/// Which share tiles sat in the peer panel on the previous render. A received
/// share only inherits the placement of the same share key.
#[derive(Default)]
pub struct PanelPlacement {
    received: Option<String>,
    own: bool,
    demoted: Vec<String>,
}

impl PanelPlacement {
    /// `(received in panel, own in panel)` for this render.
    pub fn place(
        &mut self,
        split_layout: bool,
        received: Option<(&str, ShareViewMode)>,
        own: Option<ShareViewMode>,
    ) -> (bool, bool) {
        let received_in = received.is_some_and(|(key, mode)| {
            share_in_panel(split_layout, mode, self.received.as_deref() == Some(key))
        });
        let own_in = own.is_some_and(|mode| share_in_panel(split_layout, mode, self.own));
        self.received = received
            .filter(|_| received_in)
            .map(|(key, _)| key.to_string());
        self.own = own_in;
        (received_in, own_in)
    }

    /// Which demoted tiles render in the panel; each keeps its own seat.
    pub fn place_demoted(&mut self, split_layout: bool, keys: &[String]) -> Vec<bool> {
        let placed: Vec<bool> = keys
            .iter()
            .map(|k| share_in_panel(split_layout, ShareViewMode::Tile, self.demoted.contains(k)))
            .collect();
        self.demoted = keys
            .iter()
            .zip(&placed)
            .filter(|(_, in_panel)| **in_panel)
            .map(|(k, _)| k.clone())
            .collect();
        placed
    }
}

/// The share tiles in the split's peer panel: the received and own shares
/// placed there, and the demoted tiles placed there.
pub fn panel_share_tiles((received, own): (bool, bool), demoted: &[bool]) -> usize {
    usize::from(received) + usize::from(own) + demoted.iter().filter(|p| **p).count()
}

/// The share tile's region name; the pinned badge is decorative.
pub fn region_label(label: &str, mode: ShareViewMode) -> String {
    if mode == ShareViewMode::Pinned {
        format!("{label}, pinned")
    } else {
        label.to_string()
    }
}

pub fn detach_announcement(origin: ShareOrigin, opened: bool) -> String {
    let subject = origin.subject();
    if opened {
        format!("{subject} opened in a separate window")
    } else {
        format!("{subject} returned to the meeting")
    }
}

pub fn view_announcement(a: ShareAnnounce, origin: ShareOrigin, name: &str) -> String {
    let subject = origin.subject();
    match a {
        ShareAnnounce::Enlarged => format!("{subject} enlarged"),
        ShareAnnounce::ReturnedToGrid => format!("{subject} returned to the grid"),
        ShareAnnounce::Pinned => format!("{subject} pinned"),
        ShareAnnounce::Unpinned => format!("{subject} unpinned"),
        ShareAnnounce::Returned => detach_announcement(origin, false),
        ShareAnnounce::DetachFailed => DETACH_FAILED.to_string(),
        ShareAnnounce::Stopped => format!("{name} stopped sharing"),
    }
}

pub fn arrival_announcement(name: &str, cta: bool) -> String {
    if cta {
        format!("{name} started sharing. {CTA_HINT}")
    } else {
        format!("{name} started sharing")
    }
}

/// What a change of the received top sharer announces. `prev` carries whether
/// the previous sharer is still sharing.
pub fn switch_announcement(
    prev: Option<(&str, bool)>,
    next: Option<&str>,
    cta: bool,
) -> Option<String> {
    match (prev, next) {
        (Some((gone, false)), Some(shown)) => Some(format!(
            "{gone} stopped sharing. Showing {shown}'s shared content.{}",
            if cta {
                format!(" {CTA_HINT}")
            } else {
                String::new()
            }
        )),
        (_, Some(started)) => Some(arrival_announcement(started, cta)),
        (Some((gone, false)), None) => Some(format!("{gone} stopped sharing")),
        _ => None,
    }
}

/// What a change of the share in the received window announces in the
/// meeting. `prev` carries whether the previous share is still sharing.
pub fn window_switch_announcement(
    prev: Option<(&str, bool)>,
    next: Option<&str>,
) -> Option<String> {
    match (prev, next) {
        (Some((gone, false)), Some(shown)) => Some(format!(
            "{gone} stopped sharing. Showing {shown}'s shared content in the separate window."
        )),
        (Some((moved, true)), Some(started)) => Some(format!(
            "{started} started sharing in the separate window. {moved}'s shared content moved \
             to the meeting."
        )),
        (None, Some(started)) => Some(format!(
            "{started} started sharing. Showing it in the separate window."
        )),
        (Some((gone, false)), None) => Some(format!(
            "{gone} stopped sharing. The separate window stays open for the next share."
        )),
        _ => None,
    }
}

/// The same change, spoken inside the separate window.
pub fn window_note(prev: Option<(&str, bool)>, next: Option<&str>) -> Option<String> {
    match (prev, next) {
        (Some((gone, false)), Some(shown)) => Some(format!(
            "{gone} stopped sharing. Now showing {shown}'s shared content."
        )),
        (Some((_, true)), Some(shown)) => Some(format!("Now showing {shown}'s shared content")),
        (None, Some(started)) => Some(format!("{started} started sharing")),
        (Some((gone, false)), None) => Some(format!(
            "{gone} stopped sharing. Shared content will be displayed here."
        )),
        _ => None,
    }
}

pub fn swap_announcement(shown: &str, moved: Option<&str>) -> String {
    match moved {
        Some(moved) => format!(
            "Showing {shown}'s shared content in the separate window. {moved}'s shared content \
             moved to the meeting."
        ),
        None => format!("Showing {shown}'s shared content in the separate window."),
    }
}

/// An open from a displaced tile while no window was open.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn tile_opened_announcement(shown: &str, stays: Option<&str>) -> String {
    let opened = swap_announcement(shown, None);
    match stays {
        Some(stays) => format!("{opened} {stays}'s shared content stays in the meeting."),
        None => opened,
    }
}

/// Whether a share pin points at a share tile that is no longer rendered.
/// `received_sharer` is `Some(None)` while a received share is on screen but
/// its sharer's user id could not be read this render: the pin is held.
pub fn share_pin_is_stale(
    pin: &PinnedTile,
    received_sharer: Option<Option<&str>>,
    own_user: &str,
    own_share_live: bool,
) -> bool {
    match pin.kind {
        PinnedTileKind::Camera => false,
        PinnedTileKind::Screen => match received_sharer {
            Some(None) => false,
            Some(Some(user)) => user != pin.user_id && pin.user_id != own_user,
            None => pin.user_id != own_user,
        },
        PinnedTileKind::OwnScreen => !own_share_live,
    }
}

pub fn share_dom_id(key: &str, part: &str) -> String {
    if key == OWN_SHARE_KEY {
        format!("own-screen-share-{part}")
    } else {
        format!("screen-share-{key}-{part}")
    }
}

pub fn load_share_view_pref(origin: ShareOrigin, meeting: (f64, f64)) -> ShareViewMode {
    let stored = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(origin.pref_key()).ok().flatten());
    origin.stored_mode(stored.as_deref(), meeting)
}

pub fn load_own_share_view_pref() -> ShareViewMode {
    load_share_view_pref(ShareOrigin::Own, (0.0, 0.0))
}

pub fn save_share_view_pref(origin: ShareOrigin, mode: ShareViewMode) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(origin.pref_key(), mode.as_str());
    }
}

pub fn display_surface(stream: &web_sys::MediaStream) -> Option<String> {
    let track = stream.get_video_tracks().get(0);
    let get_settings = js_sys::Reflect::get(&track, &"getSettings".into()).ok()?;
    let get_settings: &js_sys::Function = wasm_bindgen::JsCast::dyn_ref(&get_settings)?;
    let settings = get_settings.call0(&track).ok()?;
    js_sys::Reflect::get(&settings, &"displaySurface".into())
        .ok()?
        .as_string()
}

#[derive(Clone, Copy)]
pub struct ShareViewCtx {
    pub slots: Signal<ShareSlots>,
    pub pins: Signal<Vec<PinnedTile>>,
    pub detached: Signal<Option<String>>,
    pub announce: Signal<(String, u32)>,
    pub own_stream: Signal<Option<web_sys::MediaStream>>,
    pub stage: Signal<ReceivedStage>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShareTarget {
    pub origin: ShareOrigin,
    pub key: String,
    pub pin: PinnedTile,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShareTileView {
    pub target: ShareTarget,
    pub mode: ShareViewMode,
    pub cta: CtaState,
    pub guard: bool,
    pub pin_rank: Option<usize>,
    pub demoted: bool,
}

impl ShareTileView {
    /// The tile root's `order`, set on every render: Dioxus keeps a property a
    /// later `style` string omits.
    pub fn root_style(&self) -> String {
        let unpinned = match self.target.origin {
            ShareOrigin::Received => -3,
            ShareOrigin::Own => -2,
        };
        format!("order: {};", pin_order::tile_order(self.pin_rank, unpinned))
    }
}

pub fn announce(ctx: ShareViewCtx, text: String) {
    let mut sig = ctx.announce;
    let Ok(mut w) = sig.try_write() else {
        return;
    };
    w.0 = text;
    w.1 = w.1.wrapping_add(1);
}

/// Applies `op` to the pin list, writing the signal only when it changes.
fn update_pins(mut pins: Signal<Vec<PinnedTile>>, op: impl FnOnce(&mut Vec<PinnedTile>) -> bool) {
    let Ok(mut next) = pins.try_peek().map(|p| p.clone()) else {
        return;
    };
    if !op(&mut next) {
        return;
    }
    if let Ok(mut w) = pins.try_write() {
        *w = next;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeardownCause {
    User,
    System,
    Displaced,
    #[cfg(target_arch = "wasm32")]
    Failed,
}

thread_local! {
    static TEARDOWN_CAUSE: std::cell::Cell<TeardownCause> =
        const { std::cell::Cell::new(TeardownCause::User) };
}

#[cfg(target_arch = "wasm32")]
fn take_teardown_cause() -> TeardownCause {
    TEARDOWN_CAUSE.with(|c| c.replace(TeardownCause::User))
}

pub fn teardown_with_cause(key: &str, cause: TeardownCause) {
    TEARDOWN_CAUSE.with(|c| c.set(cause));
    #[cfg(target_arch = "wasm32")]
    crate::components::screen_share_detach::teardown(key);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = key;
    TEARDOWN_CAUSE.with(|c| c.set(TeardownCause::User));
}

fn snapshot(ctx: ShareViewCtx, target: &ShareTarget) -> ShareSnapshot {
    let slot = ctx
        .slots
        .try_peek()
        .map(|s| s.get(target.origin))
        .unwrap_or_default();
    let detached = ctx
        .detached
        .try_peek()
        .map(|d| d.clone())
        .unwrap_or_default();
    let other_window = match target.origin {
        ShareOrigin::Own => ctx.stage.try_peek().is_ok_and(|s| s.window),
        ShareOrigin::Received => detached.as_deref() == Some(OWN_SHARE_KEY),
    };
    ShareSnapshot {
        base: slot.base,
        pinned: ctx.pins.try_peek().is_ok_and(|p| p.contains(&target.pin)),
        detached: detached.as_deref() == Some(target.key.as_str()),
        other_detached: other_window,
        pre_detach: slot.pre_detach,
    }
}

/// One stage: the other share drops to Tile; its pin is untouched.
fn demote_other(ctx: ShareViewCtx, target: &ShareTarget) {
    let mut slots = ctx.slots;
    let Ok(mut s) = slots.try_write() else {
        return;
    };
    s.get_mut(target.origin.other()).base = ShareBase::Tile;
}

fn apply(ctx: ShareViewCtx, target: &ShareTarget, t: ShareTransition) {
    let mut slots = ctx.slots;
    if let Ok(mut s) = slots.try_write() {
        let slot = s.get_mut(target.origin);
        slot.base = t.base;
        slot.pre_detach = t.pre_detach;
        if t.hide_cta {
            slot.cta = CtaState::Hidden;
        }
    }
    if t.demote_other {
        demote_other(ctx, target);
    }
    match t.pin {
        PinOp::Keep => {}
        PinOp::PinThis => update_pins(ctx.pins, |p| pin_order::pin_front(p, target.pin.clone())),
        PinOp::Unpin => update_pins(ctx.pins, |p| pin_order::unpin(p, &target.pin)),
    }
    if let Some(mode) = t.pref {
        save_share_view_pref(target.origin, mode);
    }
    if let Some(a) = t.announce {
        announce(ctx, view_announcement(a, target.origin, &target.name));
    }
}

fn settle(ctx: ShareViewCtx, target: &ShareTarget, action: ShareAction) {
    let t = next_share_view(snapshot(ctx, target), action);
    apply(ctx, target, t);
}

/// Run a control-bar action (Enlarge, Pin, Detach) for `target`.
pub fn dispatch(ctx: ShareViewCtx, target: &ShareTarget, action: ShareAction) {
    if !ctx
        .slots
        .try_peek()
        .is_ok_and(|s| view_action_allowed(target.origin, &s))
    {
        return;
    }
    let demoted = target.origin == ShareOrigin::Received
        && ctx
            .stage
            .try_peek()
            .is_ok_and(|s| s.demoted.contains(&target.key));
    if demoted {
        if action == ShareAction::Detach {
            detach_demoted(ctx, target);
        }
        return;
    }
    let before = snapshot(ctx, target);
    let t = next_share_view(before, action);
    apply(ctx, target, t);
    match t.detach {
        DetachOp::Keep => {}
        DetachOp::Close => {
            #[cfg(target_arch = "wasm32")]
            crate::components::screen_share_detach::reattach(target.origin.window_key());
        }
        DetachOp::Open => {
            if t.reattach_other {
                teardown_with_cause(target.origin.other().window_key(), TeardownCause::Displaced);
            }
            open_detached(ctx, target, false);
        }
    }
}

/// The received window closes when the meeting ends or its view unmounts.
pub fn use_received_window_lifetime(meeting_over: impl Fn() -> bool + 'static) {
    use_effect(move || {
        if meeting_over() {
            teardown_with_cause(RECEIVED_WINDOW_KEY, TeardownCause::System);
        }
    });
    use_drop(|| teardown_with_cause(RECEIVED_WINDOW_KEY, TeardownCause::System));
}

/// Detach on a demoted tile: it takes the received slot into the window, and
/// the share it displaces becomes a demoted tile.
fn detach_demoted(ctx: ShareViewCtx, target: &ShareTarget) {
    let (window, moved) = {
        let s = ctx.stage.peek();
        (s.window, s.bound_target().map(|t| t.name))
    };
    if window {
        if take_demoted_slot(ctx, target) {
            announce(ctx, swap_announcement(&target.name, moved.as_deref()));
        }
        return;
    }
    if ctx.detached.peek().as_deref() == Some(OWN_SHARE_KEY) {
        teardown_with_cause(OWN_SHARE_KEY, TeardownCause::Displaced);
    }
    open_detached(ctx, target, true);
}

fn take_demoted_slot(ctx: ShareViewCtx, target: &ShareTarget) -> bool {
    let mut stage = ctx.stage;
    let Ok(mut s) = stage.try_write() else {
        return false;
    };
    if !s.take_slot(&target.key) {
        return false;
    }
    s.primary_target = Some(target.clone());
    s.toast = None;
    true
}

/// `parts` spoken as one announcement.
fn join_announcements(parts: &[String]) -> Option<String> {
    let (last, rest) = parts.split_last()?;
    let mut text = String::new();
    for part in rest {
        text.push_str(part.trim_end_matches('.'));
        text.push_str(". ");
    }
    text.push_str(last);
    Some(text)
}

/// Whether the focused received window, not the meeting, speaks a change.
fn speaks_in_window(window: bool) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        window && crate::components::screen_share_detach::received_window_focused()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = window;
        false
    }
}

fn bind_window(ctx: ShareViewCtx, target: Option<&ShareTarget>) {
    let key = target.map(|t| t.key.clone());
    let mut detached = ctx.detached;
    if detached.try_peek().is_ok_and(|d| *d != key) {
        if let Ok(mut d) = detached.try_write() {
            *d = key;
        }
    }
    #[cfg(target_arch = "wasm32")]
    crate::components::screen_share_detach::show(
        target.map(|t| (t.key.as_str(), t.name.as_str())),
        None,
    );
}

#[cfg(target_arch = "wasm32")]
fn received_window_closed(ctx: ShareViewCtx, cause: TeardownCause, opened: bool) {
    let mut stage = ctx.stage;
    let bound = stage.try_peek().ok().and_then(|s| s.bound_target());
    if let Ok(mut s) = stage.try_write() {
        s.window = false;
        s.toast = None;
    }
    crate::components::screen_share_detach::show(None, None);
    let mut detached = ctx.detached;
    if let Ok(mut d) = detached.try_write() {
        if d.as_deref() != Some(OWN_SHARE_KEY) {
            *d = None;
        }
    }
    match (cause, bound) {
        (TeardownCause::Displaced, Some(t)) => settle(ctx, &t, ShareAction::Displaced),
        (TeardownCause::User, Some(t)) => settle(ctx, &t, ShareAction::Reattach { opened }),
        (TeardownCause::Failed, Some(t)) => {
            settle(ctx, &t, ShareAction::Reattach { opened: false })
        }
        (TeardownCause::User, None) if opened => {
            let slot = ctx.slots.try_peek().map(|s| s.received).unwrap_or_default();
            save_share_view_pref(ShareOrigin::Received, closed_waiting_pref(slot));
            announce(ctx, "Separate window closed".to_string());
            focus_first(&[]);
        }
        (TeardownCause::User, None) => announce(ctx, DETACH_FAILED.to_string()),
        _ => {}
    }
}

#[cfg(target_arch = "wasm32")]
fn open_detached(ctx: ShareViewCtx, target: &ShareTarget, demoted: bool) {
    use crate::components::screen_share_detach as ssd;
    use std::cell::Cell;
    use std::rc::Rc;

    let mut detached = ctx.detached;
    detached.set(Some(target.key.clone()));
    if target.origin == ShareOrigin::Received {
        let mut stage = ctx.stage;
        stage.with_mut(|s| s.window = true);
    }
    let opened = Rc::new(Cell::new(false));
    let on_opened: Box<dyn FnOnce()> = {
        let opened = opened.clone();
        let origin = target.origin;
        let target = target.clone();
        Box::new(move || {
            opened.set(true);
            let mut message = detach_announcement(origin, true);
            if demoted {
                let shown = ctx
                    .stage
                    .try_peek()
                    .ok()
                    .and_then(|s| s.primary_target.clone());
                if take_demoted_slot(ctx, &target) {
                    let mut slots = ctx.slots;
                    if let Ok(mut s) = slots.try_write() {
                        s.received = ShareSlot {
                            pre_detach: Some(ShareViewMode::Tile),
                            ..ShareSlot::default()
                        };
                    };
                    let stays = shown.as_ref().map(|t| t.name.as_str());
                    message = tile_opened_announcement(&target.name, stays);
                }
            }
            save_share_view_pref(origin, ShareViewMode::Detached);
            let mut slots = ctx.slots;
            if let Ok(mut s) = slots.try_write() {
                s.get_mut(origin).cta = CtaState::Hidden;
            }
            announce(ctx, message);
        })
    };
    let on_reattach: Box<dyn Fn()> = {
        let target = target.clone();
        Box::new(move || {
            let cause = take_teardown_cause();
            if target.origin == ShareOrigin::Received {
                received_window_closed(ctx, cause, opened.get());
                return;
            }
            let mut detached = ctx.detached;
            if let Ok(mut d) = detached.try_write() {
                if d.as_deref() == Some(target.key.as_str()) {
                    *d = None;
                }
            }
            match cause {
                TeardownCause::System | TeardownCause::Failed => {}
                TeardownCause::Displaced => settle(ctx, &target, ShareAction::Displaced),
                TeardownCause::User => settle(
                    ctx,
                    &target,
                    ShareAction::Reattach {
                        opened: opened.get(),
                    },
                ),
            }
        })
    };
    match target.origin {
        ShareOrigin::Received => ssd::open(
            RECEIVED_WINDOW_KEY,
            &target.key,
            &target.name,
            on_opened,
            on_reattach,
        ),
        ShareOrigin::Own => match ctx.own_stream.peek().clone() {
            Some(stream) => ssd::open_stream(&target.key, stream, on_opened, on_reattach),
            None => on_reattach(),
        },
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn open_detached(_ctx: ShareViewCtx, _target: &ShareTarget, _demoted: bool) {}

/// Apply an arrival plan to a share that just started.
pub fn begin_share(ctx: ShareViewCtx, target: &ShareTarget, plan: ArrivalPlan, guard: bool) {
    let mut slots = ctx.slots;
    slots.with_mut(|s| {
        *s.get_mut(target.origin) = ShareSlot {
            base: plan.base,
            pre_detach: None,
            cta: plan.cta,
            guard,
            guard_chosen: false,
        };
    });
    if plan.demote_other {
        demote_other(ctx, target);
    }
    if plan.pin {
        update_pins(ctx.pins, |p| pin_order::pin_front(p, target.pin.clone()));
    }
}

/// The share tiles laid out in the meeting: received, own, then one tile per
/// demoted share.
pub fn shown_share_tiles(
    received: Option<(ShareBase, ShareViewMode)>,
    own: Option<(ShareBase, ShareViewMode)>,
    demoted: usize,
) -> Vec<(ShareBase, ShareViewMode)> {
    let demoted = std::iter::repeat_n((ShareBase::Tile, ShareViewMode::Tile), demoted);
    received.into_iter().chain(own).chain(demoted).collect()
}

/// The demoted tiles placed (`in_panel`) or not in the split's panel.
pub fn demoted_views(
    targets: &[ShareTarget],
    placed: &[bool],
    in_panel: bool,
) -> Vec<ShareTileView> {
    targets
        .iter()
        .zip(placed)
        .filter(|(_, placed)| **placed == in_panel)
        .map(|(target, _)| ShareTileView {
            target: target.clone(),
            mode: ShareViewMode::Tile,
            cta: CtaState::Hidden,
            guard: false,
            pin_rank: None,
            demoted: true,
        })
        .collect()
}

/// Follow the received sharers, oldest first, and return the stage to render.
pub fn follow_stage(ctx: ShareViewCtx, sharers: &[String]) -> ReceivedStage {
    let mut stage = ctx.stage;
    let next = stage.peek().follow(sharers);
    if *stage.peek() != next {
        stage.set(next.clone());
    }
    next
}

/// The shares seen on the previous render.
#[derive(Default)]
pub struct ShareTracker {
    received: Option<ShareTarget>,
    demoted: Vec<ShareTarget>,
    own_live: bool,
    /// The received slot left by an arrival whose pin waits for the sharer's
    /// user id; the pin applies only if the slot is unchanged.
    pin_on_resolve: Option<ShareSlot>,
}

fn received_slot(ctx: ShareViewCtx) -> Option<ShareSlot> {
    Some(ctx.slots.try_peek().ok()?.received)
}

impl ShareTracker {
    /// The last user id resolved for the received share `key`.
    pub fn known_user(&self, key: &str) -> Option<String> {
        self.received
            .as_ref()
            .filter(|t| t.key == key && t.pin.user_id != key)
            .map(|t| t.pin.user_id.clone())
    }
}

/// Section 6 / T14: apply the pref to a share that just started and clean up
/// after one that just ended. Called on every render; acts only on a change.
#[allow(clippy::too_many_arguments)]
pub fn track_shares(
    tracker: &mut ShareTracker,
    ctx: ShareViewCtx,
    received: Option<&ShareTarget>,
    demoted: &[ShareTarget],
    received_user_known: bool,
    still_sharing: &dyn Fn(&str) -> bool,
    own: Option<(&ShareTarget, &web_sys::MediaStream)>,
    detach_supported: bool,
    meeting: (f64, f64),
    mut zoom: Signal<HashMap<String, ScreenZoomState>>,
    mut actual: Signal<Option<String>>,
) {
    let window = ctx.stage.try_peek().is_ok_and(|s| s.window);
    let in_window = speaks_in_window(window);
    let bound_elsewhere = window
        && ctx
            .stage
            .try_peek()
            .is_ok_and(|s| s.primary_target.as_ref().map(|t| &t.key) != received.map(|t| &t.key));
    let target_changed = tracker.received.as_ref() != received || bound_elsewhere;
    let mut said = Vec::new();
    let mut notes = Vec::new();
    let gone: Vec<ShareTarget> = tracker
        .demoted
        .iter()
        .filter(|d| {
            !demoted.iter().any(|t| t.key == d.key)
                && received.is_none_or(|t| t.key != d.key)
                && !still_sharing(&d.key)
        })
        .cloned()
        .collect();
    for d in &gone {
        let text = format!("{} stopped sharing", d.name);
        if in_window {
            notes.push(text);
        } else {
            said.push(text);
        }
        zoom.with_mut(|m| m.remove(&d.key));
    }
    let prev_key = tracker.received.as_ref().map(|t| t.key.clone());
    if prev_key.as_deref() != received.map(|t| t.key.as_str()) {
        let prev = tracker.received.take();
        if let Some(prev) = prev.as_ref() {
            if !window {
                settle(ctx, prev, ShareAction::Ended);
            }
            zoom.with_mut(|m| m.remove(&prev.key));
            if actual.peek().as_deref() == Some(prev.key.as_str()) {
                actual.set(None);
            }
            let mut slots = ctx.slots;
            if window {
                slots.with_mut(|s| s.received.cta = CtaState::Hidden);
            } else {
                slots.with_mut(|s| s.received = ShareSlot::default());
            }
        }
        let mut cta = false;
        tracker.pin_on_resolve = None;
        if let Some(t) = received.filter(|_| !window) {
            let mut plan = arrival_plan(
                load_share_view_pref(t.origin, meeting),
                t.origin.default_mode(meeting),
                detach_supported,
                false,
            );
            let defer_pin = plan.pin && !received_user_known;
            plan.pin &= received_user_known;
            begin_share(ctx, t, plan, false);
            if defer_pin {
                tracker.pin_on_resolve = received_slot(ctx);
            }
            cta = plan.cta == CtaState::Shown;
        }
        let prev_still = prev
            .as_ref()
            .map(|p| (p.name.as_str(), still_sharing(&p.key)));
        let next = received.map(|t| t.name.as_str());
        let swapped = received.is_some_and(|t| tracker.demoted.iter().any(|d| d.key == t.key))
            && prev_still.is_some_and(|(_, sharing)| sharing);
        let message = match window {
            _ if swapped => None,
            true => window_switch_announcement(prev_still, next),
            false => switch_announcement(prev_still, next, cta),
        };
        said.extend(message.filter(|_| !in_window));
        notes.extend(window_note(prev_still, next).filter(|_| in_window && !swapped));
    }
    if window && target_changed {
        bind_window(ctx, received);
    }
    if let Some(note) = join_announcements(&notes) {
        #[cfg(target_arch = "wasm32")]
        crate::components::screen_share_detach::say(&note);
        #[cfg(not(target_arch = "wasm32"))]
        let _ = note;
    }
    if let Some(message) = join_announcements(&said) {
        announce(ctx, message);
    }
    let mut stage = ctx.stage;
    if stage
        .try_peek()
        .is_ok_and(|s| s.primary_target.as_ref() != received)
    {
        stage.with_mut(|s| {
            let moved = s.primary_target.as_ref().map(|p| &p.key) != received.map(|t| &t.key);
            if let Some(t) = received.filter(|_| s.window && moved) {
                s.toast_seq += 1;
                let text = format!("{} is sharing in the separate window", t.name);
                s.toast = Some((s.toast_seq, text));
            } else if received.is_none() {
                s.toast = None;
            }
            s.primary_target = received.cloned();
        });
    }
    tracker.demoted = demoted.to_vec();
    if let Some(t) = received.filter(|_| received_user_known) {
        if let Some(at_arrival) = tracker.pin_on_resolve.take() {
            if received_slot(ctx) == Some(at_arrival) {
                update_pins(ctx.pins, |p| pin_order::pin_front(p, t.pin.clone()));
            }
        }
    }
    tracker.received = received.cloned();

    if own.is_some() == tracker.own_live {
        return;
    }
    tracker.own_live = own.is_some();
    match own {
        Some((t, stream)) => {
            let guard = mirror_guard_default(display_surface(stream).as_deref());
            let pref = load_share_view_pref(t.origin, meeting);
            let plan = own_arrival_plan(pref, detach_supported, guard);
            begin_share(ctx, t, plan, guard);
            if guard {
                announce(ctx, MIRROR_GUARD_ON.to_string());
            }
        }
        None => {
            zoom.with_mut(|m| m.remove(OWN_SHARE_KEY));
            let mut slots = ctx.slots;
            slots.with_mut(|s| s.own = ShareSlot::default());
        }
    }
}

/// Mirror guard (own share): hiding releases every stage as a system action.
pub fn set_mirror_guard(ctx: ShareViewCtx, target: &ShareTarget, on: bool, detach_supported: bool) {
    if on {
        let mut slots = ctx.slots;
        slots.with_mut(|s| {
            *s.get_mut(target.origin) = ShareSlot {
                guard: true,
                ..ShareSlot::default()
            }
        });
        update_pins(ctx.pins, |p| pin_order::unpin(p, &target.pin));
        if ctx.detached.peek().as_deref() == Some(target.key.as_str()) {
            teardown_with_cause(&target.key, TeardownCause::System);
        }
        return;
    }
    let surface = ctx
        .own_stream
        .try_peek()
        .ok()
        .and_then(|s| s.as_ref().and_then(display_surface));
    let plan = show_preview_plan(
        load_own_share_view_pref(),
        detach_supported,
        surface.as_deref(),
    );
    begin_share(ctx, target, plan, false);
    if plan.cta == CtaState::Shown {
        announce(ctx, CTA_HINT.to_string());
    }
    if plan.detach_now {
        dispatch(ctx, target, ShareAction::Detach);
    }
}

/// The own share's guard after its capture switched to `surface`, when that
/// changes it. A guard the presenter chose only gives way to a whole screen.
pub fn replaced_guard(surface: Option<&str>, guarded: bool, chosen: bool) -> Option<bool> {
    let guard = mirror_guard_default(surface);
    (guard != guarded && (guard || !chosen)).then_some(guard)
}

/// Hide or Show preview on the own share.
pub fn choose_mirror_guard(
    ctx: ShareViewCtx,
    target: &ShareTarget,
    on: bool,
    detach_supported: bool,
) {
    set_mirror_guard(ctx, target, on, detach_supported);
    let mut slots = ctx.slots;
    let Ok(mut s) = slots.try_write() else {
        return;
    };
    s.get_mut(target.origin).guard_chosen = true;
}

/// The presenter's confirmation once "Share something else" took effect.
pub fn replaced_text(surface: Option<&str>) -> &'static str {
    match surface {
        Some("monitor") => "Switched to sharing your entire screen",
        Some("window") => "Switched to sharing a window",
        Some("browser") => "Switched to sharing a tab",
        _ => "Switched to sharing new content",
    }
}

/// The own share switched to `stream`: it becomes the own preview, its zoom
/// resets and the switch is announced. The mirror guard changes only as
/// [`replaced_guard`] decides; when it stays, an open own window plays `stream`.
/// Returns the toast text.
pub fn own_capture_replaced(
    ctx: ShareViewCtx,
    target: &ShareTarget,
    stream: &web_sys::MediaStream,
    detach_supported: bool,
    mut zoom: Signal<HashMap<String, ScreenZoomState>>,
) -> &'static str {
    let mut own_stream = ctx.own_stream;
    if let Ok(mut own) = own_stream.try_write() {
        *own = Some(Clone::clone(stream));
    }
    if zoom.try_peek().is_ok_and(|z| z.contains_key(OWN_SHARE_KEY)) {
        zoom.with_mut(|z| z.remove(OWN_SHARE_KEY));
    }
    let surface = display_surface(stream);
    let text = replaced_text(surface.as_deref());
    let own = ctx.slots.try_peek().map(|s| s.own).unwrap_or_default();
    let message = match replaced_guard(surface.as_deref(), own.guard, own.guard_chosen) {
        Some(true) => {
            set_mirror_guard(ctx, target, true, detach_supported);
            format!("{text}. {MIRROR_GUARD_ON}")
        }
        Some(false) => {
            let plan = own_arrival_plan(load_own_share_view_pref(), detach_supported, false);
            begin_share(ctx, target, plan, false);
            if plan.cta == CtaState::Shown {
                format!("{text}. {CTA_HINT}")
            } else {
                text.to_string()
            }
        }
        None => {
            #[cfg(target_arch = "wasm32")]
            crate::components::screen_share_detach::replace_stream(Clone::clone(stream));
            text.to_string()
        }
    };
    announce(ctx, message);
    text
}

/// Focus the first of `ids` that actually takes focus, else the grid.
pub fn focus_first(ids: &[String]) {
    use wasm_bindgen::JsCast;
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    for id in ids.iter().map(String::as_str).chain(["grid-container"]) {
        let Some(el) = doc
            .get_element_by_id(id)
            .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
        else {
            continue;
        };
        let _ = el.focus();
        if doc.active_element().as_ref() == Some(el.as_ref()) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(base: ShareBase, pinned: bool, detached: bool) -> ShareSnapshot {
        ShareSnapshot {
            base,
            pinned,
            detached,
            ..ShareSnapshot::default()
        }
    }

    const TILE: ShareSnapshot = ShareSnapshot {
        base: ShareBase::Tile,
        pinned: false,
        detached: false,
        other_detached: false,
        pre_detach: None,
    };

    #[test]
    fn a_stored_pref_round_trips_for_both_origins_at_any_width() {
        for origin in [ShareOrigin::Received, ShareOrigin::Own] {
            for m in [
                ShareViewMode::Tile,
                ShareViewMode::Enlarged,
                ShareViewMode::Pinned,
                ShareViewMode::Detached,
            ] {
                for at in [(390.0, 844.0), (719.0, 600.0), (1920.0, 1080.0)] {
                    assert_eq!(origin.stored_mode(Some(m.as_str()), at), m, "{at:?}");
                }
            }
        }
    }

    #[test]
    fn with_no_usable_pref_a_received_share_enlarges_only_in_a_wide_meeting_area() {
        for stored in [None, Some(""), Some("Enlarged"), Some("split")] {
            for (meeting, received) in [
                ((390.0, 844.0), ShareViewMode::Tile),
                ((719.9, 500.0), ShareViewMode::Tile),
                ((f64::NAN, 0.0), ShareViewMode::Tile),
                ((768.0, 1024.0), ShareViewMode::Tile),
                ((844.0, 390.0), ShareViewMode::Tile),
                ((1280.0, 539.0), ShareViewMode::Tile),
                ((1280.0, 540.0), ShareViewMode::Enlarged),
                (
                    (ENLARGED_DEFAULT_MIN_WIDTH_PX, 600.0),
                    ShareViewMode::Enlarged,
                ),
                ((1920.0, 1080.0), ShareViewMode::Enlarged),
            ] {
                let at = format!("{stored:?} {meeting:?}");
                assert_eq!(
                    ShareOrigin::Received.stored_mode(stored, meeting),
                    received,
                    "{at}"
                );
                assert_eq!(
                    ShareOrigin::Own.stored_mode(stored, meeting),
                    ShareViewMode::Tile,
                    "{at}"
                );
            }
        }
    }

    #[test]
    fn the_view_modes_spec_copies_the_enlarged_default_width() {
        let spec = include_str!("../../../e2e/tests/screen-share-view-modes.spec.ts");
        let copied = spec
            .lines()
            .find_map(|l| l.strip_prefix("const ENLARGED_DEFAULT_MIN_WIDTH_PX = "))
            .and_then(|v| v.strip_suffix(';'))
            .expect("premise: the spec copies the width");
        assert_eq!(copied.parse::<f64>(), Ok(ENLARGED_DEFAULT_MIN_WIDTH_PX));
    }

    #[test]
    fn an_open_peer_list_narrows_a_1024px_meeting_area_below_the_enlarged_default() {
        use crate::components::attendants_layout::{
            action_bar_band_width, drawer_reserves, DrawerState, CHAT_DRAWER_WIDTH,
        };
        let default_at_1024 = |peer_list_open| {
            let reserves = drawer_reserves(DrawerState {
                vw: 1024.0,
                peer_list_open,
                left_w: 320.0,
                diagnostics_open: false,
                right_w: 320.0,
                chat_open: false,
                chat_w: CHAT_DRAWER_WIDTH,
            });
            ShareOrigin::Received.default_mode((action_bar_band_width(1024.0, reserves), 768.0))
        };
        assert_eq!(default_at_1024(true), ShareViewMode::Tile);
        assert_eq!(default_at_1024(false), ShareViewMode::Enlarged);
    }

    #[test]
    fn the_two_origins_persist_under_separate_keys() {
        assert_eq!(ShareOrigin::Received.pref_key(), "vc_share_view_mode");
        assert_eq!(ShareOrigin::Own.pref_key(), "vc_own_share_view_mode");
    }

    #[test]
    fn effective_mode_prefers_detached_then_pinned_then_base() {
        assert_eq!(
            effective_mode(true, true, ShareBase::Enlarged),
            ShareViewMode::Detached
        );
        assert_eq!(
            effective_mode(false, true, ShareBase::Enlarged),
            ShareViewMode::Pinned
        );
        assert_eq!(
            effective_mode(false, false, ShareBase::Enlarged),
            ShareViewMode::Enlarged
        );
        assert_eq!(
            effective_mode(false, false, ShareBase::Tile),
            ShareViewMode::Tile
        );
    }

    #[test]
    fn layout_is_enlarged_only_while_an_enlarged_base_is_attached() {
        use ShareViewMode as M;
        assert_eq!(share_layout(&[]), ShareLayout::None);
        assert_eq!(
            share_layout(&[(ShareBase::Tile, M::Tile)]),
            ShareLayout::Tile
        );
        assert_eq!(
            share_layout(&[(ShareBase::Enlarged, M::Enlarged)]),
            ShareLayout::Enlarged
        );
        assert_eq!(
            share_layout(&[(ShareBase::Enlarged, M::Detached)]),
            ShareLayout::Tile,
            "detaching takes the split away"
        );
    }

    #[test]
    fn share_cells_skip_detached_tiles_and_the_split() {
        use ShareViewMode as M;
        assert_eq!(
            share_grid_cells(ShareLayout::Tile, &[M::Tile, M::Pinned]),
            2
        );
        assert_eq!(
            share_grid_cells(ShareLayout::Tile, &[M::Detached, M::Tile]),
            1
        );
        assert_eq!(share_grid_cells(ShareLayout::Enlarged, &[M::Enlarged]), 0);
        assert_eq!(share_grid_cells(ShareLayout::None, &[]), 0);
    }

    #[test]
    fn t1_tile_enlarge_stages_the_share_and_demotes_the_other() {
        let t = next_share_view(TILE, ShareAction::Enlarge);
        assert_eq!(t.base, ShareBase::Enlarged);
        assert!(t.demote_other);
        assert_eq!(t.pref, Some(ShareViewMode::Enlarged));
        assert_eq!(t.announce, Some(ShareAnnounce::Enlarged));
    }

    #[test]
    fn t2_tile_pin_pins_without_touching_the_other_share() {
        let t = next_share_view(TILE, ShareAction::Pin);
        assert_eq!(t.pin, PinOp::PinThis);
        assert_eq!(t.base, ShareBase::Tile);
        assert!(!t.demote_other, "a pin never demotes the other share");
        assert_eq!(t.pref, Some(ShareViewMode::Pinned));
        assert_eq!(t.announce, Some(ShareAnnounce::Pinned));
    }

    #[test]
    fn t3_tile_detach_opens_and_defers_the_pref_until_confirmed() {
        let t = next_share_view(TILE, ShareAction::Detach);
        assert_eq!(t.detach, DetachOp::Open);
        assert_eq!(t.pre_detach, Some(ShareViewMode::Tile));
        assert_eq!(t.pref, None, "R3: committed only once the window is open");
        assert!(!t.reattach_other);
    }

    #[test]
    fn t4_enlarged_enlarge_returns_to_the_grid() {
        let t = next_share_view(
            snap(ShareBase::Enlarged, false, false),
            ShareAction::Enlarge,
        );
        assert_eq!(t.base, ShareBase::Tile);
        assert_eq!(t.pref, Some(ShareViewMode::Tile));
        assert_eq!(t.announce, Some(ShareAnnounce::ReturnedToGrid));
    }

    #[test]
    fn t5_enlarged_pin_leaves_the_split_for_the_grid() {
        let t = next_share_view(snap(ShareBase::Enlarged, false, false), ShareAction::Pin);
        assert_eq!(t.pin, PinOp::PinThis);
        assert_eq!(t.base, ShareBase::Tile, "a pinned share sits in the grid");
        assert!(!t.demote_other);
        assert_eq!(t.pref, Some(ShareViewMode::Pinned));
        assert_eq!(t.announce, Some(ShareAnnounce::Pinned));
    }

    #[test]
    fn t6_enlarged_detach_remembers_the_split() {
        let t = next_share_view(snap(ShareBase::Enlarged, false, false), ShareAction::Detach);
        assert_eq!(t.detach, DetachOp::Open);
        assert_eq!(t.pre_detach, Some(ShareViewMode::Enlarged));
        assert_eq!(t.pref, None);
    }

    #[test]
    fn t7_pinned_pin_unpins_to_a_tile() {
        let t = next_share_view(snap(ShareBase::Tile, true, false), ShareAction::Pin);
        assert_eq!(t.pin, PinOp::Unpin);
        assert_eq!(t.base, ShareBase::Tile);
        assert_eq!(t.pref, Some(ShareViewMode::Tile));
        assert_eq!(t.announce, Some(ShareAnnounce::Unpinned));
    }

    #[test]
    fn t9_pinned_enlarge_unpins_into_the_split() {
        let t = next_share_view(snap(ShareBase::Tile, true, false), ShareAction::Enlarge);
        assert_eq!(t.pin, PinOp::Unpin);
        assert_eq!(t.base, ShareBase::Enlarged);
        assert!(t.demote_other, "one stage");
        assert_eq!(t.pref, Some(ShareViewMode::Enlarged));
        assert_eq!(t.announce, Some(ShareAnnounce::Enlarged));
    }

    #[test]
    fn t10_pinned_detach_releases_the_pin_first() {
        let t = next_share_view(snap(ShareBase::Tile, true, false), ShareAction::Detach);
        assert_eq!(t.pin, PinOp::Unpin);
        assert_eq!(t.detach, DetachOp::Open);
        assert_eq!(t.pre_detach, Some(ShareViewMode::Pinned));
        assert_eq!(t.pref, None);
        assert!(!t.demote_other);
    }

    #[test]
    fn t12_reattach_lands_on_the_pre_detach_mode() {
        let pinned_before = ShareSnapshot {
            detached: true,
            pre_detach: Some(ShareViewMode::Pinned),
            base: ShareBase::Enlarged,
            ..TILE
        };
        let repinned = next_share_view(pinned_before, ShareAction::Reattach { opened: true });
        assert_eq!(
            repinned.pin,
            PinOp::PinThis,
            "pins coexist, so there is no slot to lose"
        );
        assert_eq!(repinned.base, ShareBase::Tile);
        assert!(!repinned.demote_other, "a re-pin never demotes");
        assert_eq!(repinned.pref, Some(ShareViewMode::Pinned));
        assert_eq!(repinned.pre_detach, None);
        assert_eq!(repinned.announce, Some(ShareAnnounce::Returned));

        let enlarged = next_share_view(
            ShareSnapshot {
                detached: true,
                pre_detach: Some(ShareViewMode::Enlarged),
                ..TILE
            },
            ShareAction::Reattach { opened: true },
        );
        assert_eq!(enlarged.base, ShareBase::Enlarged);
        assert!(enlarged.demote_other);
        assert_eq!(enlarged.pref, Some(ShareViewMode::Enlarged));
    }

    #[test]
    fn t13_a_mode_change_keeps_the_scale_and_reclamps_the_pan() {
        use crate::components::screen_share_zoom::zoom_to;
        let zoomed = ScreenZoomState {
            scale: 2.0,
            off_x: 500.0,
            off_y: -500.0,
        };
        let smaller_viewport = zoom_to(zoomed, zoomed.scale, 100.0, 50.0);
        assert_eq!(smaller_viewport.scale, 2.0);
        assert_eq!(smaller_viewport.off_x, 100.0);
        assert_eq!(smaller_viewport.off_y, -50.0);
    }

    #[test]
    fn t14_share_end_clears_pin_and_window_without_a_pref() {
        let t = next_share_view(snap(ShareBase::Tile, true, false), ShareAction::Ended);
        assert_eq!(t.pin, PinOp::Unpin);
        assert_eq!(t.pref, None);
        assert_eq!(t.announce, Some(ShareAnnounce::Stopped));
        let detached = next_share_view(snap(ShareBase::Tile, false, true), ShareAction::Ended);
        assert_eq!(detached.pin, PinOp::Keep);
        assert_eq!(detached.pre_detach, None);
    }

    #[test]
    fn t15_failed_detach_reverts_silently_on_the_pref() {
        let t = next_share_view(
            ShareSnapshot {
                detached: true,
                pre_detach: Some(ShareViewMode::Pinned),
                ..TILE
            },
            ShareAction::Reattach { opened: false },
        );
        assert_eq!(t.pin, PinOp::PinThis, "back to the previous mode");
        assert!(!t.demote_other);
        assert_eq!(t.pref, None);
        assert_eq!(t.announce, Some(ShareAnnounce::DetachFailed));
    }

    #[test]
    fn t16_detach_while_the_other_is_detached_displaces_it_without_its_pref() {
        let this = next_share_view(
            ShareSnapshot {
                other_detached: true,
                ..TILE
            },
            ShareAction::Detach,
        );
        assert!(this.reattach_other);
        assert_eq!(this.detach, DetachOp::Open);
        assert_eq!(this.pref, None);

        let other = next_share_view(
            ShareSnapshot {
                detached: true,
                pre_detach: Some(ShareViewMode::Enlarged),
                ..TILE
            },
            ShareAction::Displaced,
        );
        assert_eq!(other.base, ShareBase::Enlarged);
        assert_eq!(other.pref, None);
        assert_eq!(other.announce, None);
    }

    #[test]
    fn detached_detach_button_reattaches() {
        let t = next_share_view(snap(ShareBase::Tile, false, true), ShareAction::Detach);
        assert_eq!(t.detach, DetachOp::Close);
        assert_eq!(t.pref, None, "the pref is written by the reattach itself");
    }

    #[test]
    fn arrival_tile_pref_is_the_default() {
        assert_eq!(
            arrival_plan(ShareViewMode::Tile, ShareViewMode::Enlarged, true, false),
            ArrivalPlan::default()
        );
    }

    #[test]
    fn arrival_enlarged_pref_stages_the_share() {
        let p = arrival_plan(ShareViewMode::Enlarged, ShareViewMode::Tile, true, false);
        assert_eq!(p.base, ShareBase::Enlarged);
        assert!(p.demote_other);
        assert!(!p.pin);
    }

    #[test]
    fn arrival_pinned_pref_pins_over_a_tile_base() {
        let p = arrival_plan(ShareViewMode::Pinned, ShareViewMode::Enlarged, true, false);
        assert_eq!(p.base, ShareBase::Tile);
        assert!(p.pin);
        assert!(
            !p.demote_other,
            "arriving pinned leaves the other share alone"
        );
    }

    #[test]
    fn arrival_detached_pref_offers_the_cta_over_the_default_view_and_never_opens() {
        let p = arrival_plan(ShareViewMode::Detached, ShareViewMode::Tile, true, false);
        assert_eq!(p.base, ShareBase::Tile);
        assert_eq!(p.cta, CtaState::Shown);
        assert!(!p.detach_now, "C3: no popup without a user activation");
        let wide = arrival_plan(
            ShareViewMode::Detached,
            ShareViewMode::Enlarged,
            true,
            false,
        );
        assert_eq!(wide.base, ShareBase::Enlarged);
        assert!(wide.demote_other);
        assert_eq!(wide.cta, CtaState::Shown);
        assert!(!wide.detach_now);
    }

    #[test]
    fn arrival_detached_pref_without_detach_support_opens_in_the_default_view() {
        assert_eq!(
            arrival_plan(ShareViewMode::Detached, ShareViewMode::Tile, false, false),
            ArrivalPlan::default()
        );
        assert_eq!(
            arrival_plan(
                ShareViewMode::Detached,
                ShareViewMode::Enlarged,
                false,
                false
            ),
            arrival_plan(ShareViewMode::Enlarged, ShareViewMode::Tile, false, false)
        );
    }

    #[test]
    fn a_click_applies_the_detached_pref_directly() {
        let p = arrival_plan(ShareViewMode::Detached, ShareViewMode::Tile, true, true);
        assert!(p.detach_now);
        assert_eq!(p.cta, CtaState::Hidden);
    }

    #[test]
    fn own_share_with_the_mirror_guard_ignores_the_pref() {
        for pref in [
            ShareViewMode::Enlarged,
            ShareViewMode::Pinned,
            ShareViewMode::Detached,
        ] {
            assert_eq!(own_arrival_plan(pref, true, true), ArrivalPlan::default());
        }
        assert!(own_arrival_plan(ShareViewMode::Pinned, true, false).pin);
    }

    #[test]
    fn the_mirror_guard_defaults_on_only_for_a_whole_screen() {
        assert!(mirror_guard_default(Some("monitor")));
        assert!(!mirror_guard_default(Some("window")));
        assert!(!mirror_guard_default(Some("browser")));
        assert!(!mirror_guard_default(None));
    }

    #[test]
    fn share_pins_go_stale_with_their_tile() {
        let screen = PinnedTile::screen("bob");
        assert!(!share_pin_is_stale(&screen, Some(Some("bob")), "me", false));
        assert!(share_pin_is_stale(
            &screen,
            Some(Some("carol")),
            "me",
            false
        ));
        assert!(share_pin_is_stale(&screen, None, "me", false));
        assert!(
            !share_pin_is_stale(&screen, Some(None), "me", false),
            "an unread sharer id holds the pin"
        );
        assert!(
            !share_pin_is_stale(&PinnedTile::screen("me"), None, "me", false),
            "a same-user sibling's grid share is not a received share tile"
        );
        let own = PinnedTile::own_screen("me");
        assert!(!share_pin_is_stale(&own, None, "me", true));
        assert!(share_pin_is_stale(&own, None, "me", false));
        assert!(!share_pin_is_stale(
            &PinnedTile::camera("bob"),
            None,
            "me",
            false
        ));
    }

    #[test]
    fn own_share_controls_use_the_published_ids() {
        assert_eq!(
            share_dom_id(OWN_SHARE_KEY, "detach-btn"),
            "own-screen-share-detach-btn"
        );
        assert_eq!(
            share_dom_id("42", "detach-btn"),
            "screen-share-42-detach-btn"
        );
        assert_eq!(share_dom_id("42", "viewport"), "screen-share-42-viewport");
    }

    #[test]
    fn a_detached_tile_keeps_its_parent_and_a_tile_or_pin_joins_the_panel() {
        use ShareViewMode as M;
        assert!(share_in_panel(true, M::Detached, true));
        assert!(!share_in_panel(true, M::Detached, false));
        assert!(
            share_in_panel(false, M::Detached, true),
            "the layout may change while detached"
        );
        assert!(share_in_panel(true, M::Tile, false));
        assert!(
            share_in_panel(false, M::Tile, true),
            "a tile in the panel stays there when the split turns off"
        );
        assert!(!share_in_panel(false, M::Tile, false));
        assert!(!share_in_panel(true, M::Enlarged, true));
        assert!(
            share_in_panel(true, M::Pinned, false),
            "a pinned share is placed exactly like a tile"
        );
        assert!(share_in_panel(false, M::Pinned, true));
        assert!(!share_in_panel(false, M::Pinned, false));
    }

    /// Each step lists the received share, then the own share.
    fn placements(steps: &[&[(ShareBase, ShareViewMode)]]) -> Vec<Vec<bool>> {
        let mut placement = PanelPlacement::default();
        steps
            .iter()
            .map(|tiles| {
                let split = share_layout(tiles) == ShareLayout::Enlarged;
                let (received, own) = placement.place(
                    split,
                    tiles.first().map(|(_, m)| ("a", *m)),
                    tiles.get(1).map(|(_, m)| *m),
                );
                [received, own][..tiles.len()].to_vec()
            })
            .collect()
    }

    #[test]
    fn a_single_share_never_changes_parent() {
        use ShareBase as B;
        use ShareViewMode as M;
        let steps: &[&[(B, M)]] = &[
            &[(B::Tile, M::Tile)],
            &[(B::Enlarged, M::Enlarged)],
            &[(B::Tile, M::Pinned)],
            &[(B::Tile, M::Detached)],
            &[(B::Tile, M::Pinned)],
            &[(B::Enlarged, M::Enlarged)],
            &[(B::Enlarged, M::Detached)],
            &[(B::Enlarged, M::Enlarged)],
            &[(B::Tile, M::Tile)],
        ];
        assert!(placements(steps).iter().all(|p| p == &[false]));
    }

    #[test]
    fn a_tile_in_the_panel_stays_when_the_other_share_leaves_the_stage() {
        use ShareBase as B;
        use ShareViewMode as M;
        for off in [(B::Tile, M::Tile), (B::Enlarged, M::Detached)] {
            let steps: &[&[(B, M)]] = &[
                &[(B::Enlarged, M::Enlarged), (B::Tile, M::Tile)],
                &[off, (B::Tile, M::Tile)],
                &[(B::Tile, M::Tile), (B::Tile, M::Tile)],
            ];
            let placed = placements(steps);
            assert_eq!(placed[0], vec![false, true]);
            assert!(
                placed[1..].iter().all(|p| p == &[false, true]),
                "{off:?}: neither share moves once the split turns off"
            );
        }
    }

    #[test]
    fn a_share_never_inherits_the_panel_seat_of_another_share() {
        use ShareViewMode as M;
        let mut placement = PanelPlacement::default();
        assert_eq!(
            placement.place(true, Some(("a", M::Tile)), Some(M::Enlarged)),
            (true, false),
            "premise: the split seats received share a in the panel"
        );
        assert_eq!(
            placement.place(false, Some(("b", M::Tile)), Some(M::Tile)),
            (false, false),
            "b replaced a on the same render and never sat in the panel"
        );

        let mut placement = PanelPlacement::default();
        assert_eq!(
            placement.place(true, Some(("a", M::Enlarged)), Some(M::Tile)),
            (false, true),
            "premise: the split seats the own share in the panel"
        );
        assert_eq!(
            placement.place(false, Some(("a", M::Tile)), None),
            (false, false)
        );
        assert_eq!(
            placement.place(false, Some(("a", M::Tile)), Some(M::Tile)),
            (false, false),
            "a new own share starts outside the panel"
        );
    }

    #[test]
    fn a_top_sharer_switch_announces_once() {
        assert_eq!(
            switch_announcement(Some(("Bea", false)), Some("Ann"), false).as_deref(),
            Some("Bea stopped sharing. Showing Ann's shared content.")
        );
        assert_eq!(
            switch_announcement(Some(("Ann", true)), Some("Bea"), false).as_deref(),
            Some("Bea started sharing"),
            "the previous sharer is still sharing, so nothing stopped"
        );
        assert_eq!(
            switch_announcement(None, Some("Ann"), true).as_deref(),
            Some("Ann started sharing. You can open it in a separate window.")
        );
        assert_eq!(
            switch_announcement(Some(("Ann", false)), None, false).as_deref(),
            Some("Ann stopped sharing")
        );
        assert_eq!(switch_announcement(None, None, false), None);
    }

    #[test]
    fn show_preview_never_opens_a_window_over_a_whole_screen() {
        let monitor = show_preview_plan(ShareViewMode::Detached, true, Some("monitor"));
        assert!(!monitor.detach_now);
        assert_eq!(monitor.cta, CtaState::Shown);
        assert!(show_preview_plan(ShareViewMode::Detached, true, Some("window")).detach_now);
        assert!(show_preview_plan(ShareViewMode::Detached, true, None).detach_now);
    }

    #[test]
    fn the_guard_refuses_own_view_actions_only() {
        let guarded = ShareSlots {
            own: ShareSlot {
                guard: true,
                ..ShareSlot::default()
            },
            ..ShareSlots::default()
        };
        assert!(!view_action_allowed(ShareOrigin::Own, &guarded));
        assert!(view_action_allowed(ShareOrigin::Received, &guarded));
        assert!(view_action_allowed(
            ShareOrigin::Own,
            &ShareSlots::default()
        ));
    }

    #[test]
    fn the_tracker_remembers_a_resolved_sharer_id_per_key() {
        let target = ShareTarget {
            origin: ShareOrigin::Received,
            key: "7".into(),
            pin: PinnedTile::screen("alice"),
            name: "Alice".into(),
        };
        let tracker = ShareTracker {
            received: Some(target),
            ..ShareTracker::default()
        };
        assert_eq!(tracker.known_user("7").as_deref(), Some("alice"));
        assert_eq!(tracker.known_user("8"), None);
    }

    #[test]
    fn announcements_name_the_subject_per_origin() {
        assert_eq!(
            detach_announcement(ShareOrigin::Received, true),
            "Shared content opened in a separate window"
        );
        assert_eq!(
            detach_announcement(ShareOrigin::Received, false),
            "Shared content returned to the meeting"
        );
        assert_eq!(
            detach_announcement(ShareOrigin::Own, true),
            "Your shared content opened in a separate window"
        );
        assert_eq!(
            view_announcement(ShareAnnounce::Enlarged, ShareOrigin::Own, "x"),
            "Your shared content enlarged"
        );
        assert_eq!(
            view_announcement(ShareAnnounce::ReturnedToGrid, ShareOrigin::Received, "x"),
            "Shared content returned to the grid"
        );
        assert_eq!(
            view_announcement(ShareAnnounce::Stopped, ShareOrigin::Received, "Ann"),
            "Ann stopped sharing"
        );
        assert_eq!(
            arrival_announcement("Ann", true),
            "Ann started sharing. You can open it in a separate window."
        );
    }

    #[test]
    fn a_pinned_share_region_says_so() {
        assert_eq!(
            region_label("Your shared content", ShareViewMode::Pinned),
            "Your shared content, pinned"
        );
        for mode in [
            ShareViewMode::Tile,
            ShareViewMode::Enlarged,
            ShareViewMode::Detached,
        ] {
            assert_eq!(
                region_label("Shared content from Ann", mode),
                "Shared content from Ann"
            );
        }
    }

    #[test]
    fn a_share_root_always_emits_its_order() {
        let view = |origin, pin_rank| ShareTileView {
            target: ShareTarget {
                origin,
                key: OWN_SHARE_KEY.into(),
                pin: PinnedTile::own_screen("me"),
                name: String::new(),
            },
            mode: ShareViewMode::Tile,
            cta: CtaState::Hidden,
            guard: false,
            pin_rank,
            demoted: false,
        };
        assert_eq!(view(ShareOrigin::Received, None).root_style(), "order: -3;");
        assert_eq!(view(ShareOrigin::Own, None).root_style(), "order: -2;");
        assert_eq!(
            view(ShareOrigin::Own, Some(1)).root_style(),
            "order: -999;",
            "a pin moves the share ahead of every unpinned tile"
        );
    }

    fn keys(ks: &[&str]) -> Vec<String> {
        ks.iter().map(|k| k.to_string()).collect()
    }

    fn stage(window: bool, renders: &[&[&str]]) -> ReceivedStage {
        let start = ReceivedStage {
            window,
            ..ReceivedStage::default()
        };
        renders
            .iter()
            .fold(start, |s, sharers| s.follow(&keys(sharers)))
    }

    #[test]
    fn without_a_window_the_newest_share_replaces_the_shown_one() {
        let s = stage(false, &[&["a"], &["a", "b"]]);
        assert_eq!(s.primary.as_deref(), Some("b"));
        assert!(
            s.demoted.is_empty(),
            "the older share is hidden, not demoted"
        );
        assert_eq!(
            s.follow(&keys(&["a"])).primary.as_deref(),
            Some("a"),
            "the older share returns when the newer one stops"
        );
    }

    #[test]
    fn a_new_share_takes_the_window_and_demotes_the_one_it_showed() {
        let s = stage(true, &[&["a"], &["a", "b"]]);
        assert_eq!(s.primary.as_deref(), Some("b"));
        assert_eq!(s.demoted, keys(&["a"]));
        let s = s.follow(&keys(&["a", "b", "c"]));
        assert_eq!(s.primary.as_deref(), Some("c"));
        assert_eq!(s.demoted, keys(&["a", "b"]));
        let together = stage(true, &[&["a"], &["a", "b", "c"]]);
        assert_eq!(together, s, "two arrivals in one render demote in order");
    }

    #[test]
    fn opening_the_window_turns_a_share_hidden_by_lifo_into_a_tile() {
        let mut s = stage(false, &[&["a", "b"]]);
        assert!(s.demoted.is_empty(), "premise: a is hidden under b");
        s.window = true;
        let s = s.follow(&keys(&["a", "b"]));
        assert_eq!(s.primary.as_deref(), Some("b"));
        assert_eq!(s.demoted, keys(&["a"]));
    }

    #[test]
    fn a_demoted_share_stays_until_its_sharer_stops_even_after_the_window_closes() {
        let mut s = stage(true, &[&["a"], &["a", "b"]]);
        s.window = false;
        let s = s.follow(&keys(&["a", "b", "c"]));
        assert_eq!(s.primary.as_deref(), Some("c"));
        assert_eq!(s.demoted, keys(&["a"]), "without a window b is only hidden");
        let back = s.follow(&keys(&["a", "b"]));
        assert_eq!(
            back.primary.as_deref(),
            Some("b"),
            "LIFO: b returns, a stays a tile"
        );
        assert_eq!(back.demoted, keys(&["a"]));
        assert_eq!(s.follow(&keys(&["b", "c"])).demoted, Vec::<String>::new());
    }

    #[test]
    fn every_demoted_share_is_a_tile_cell_and_view() {
        let tiles = shown_share_tiles(None, None, 2);
        assert_eq!(share_layout(&tiles), ShareLayout::Tile);
        assert_eq!(tiles.len(), 2);
        let target = |key: &str| ShareTarget {
            origin: ShareOrigin::Received,
            key: key.into(),
            pin: PinnedTile::screen(key),
            name: key.into(),
        };
        let views = demoted_views(&[target("a"), target("b")], &[true, false], false);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].target.key, "b");
        assert!(views[0].demoted && views[0].mode == ShareViewMode::Tile);
    }

    #[test]
    fn the_window_waits_when_its_share_ends_and_binds_the_next_one() {
        let s = stage(true, &[&["a"], &[]]);
        assert!(s.window);
        assert_eq!(s.primary, None, "the window waits");
        let s = s.follow(&keys(&["c"]));
        assert_eq!(s.primary.as_deref(), Some("c"));
    }

    #[test]
    fn the_window_takes_the_newest_live_share_when_its_share_ends() {
        let mut s = stage(false, &[&["a", "b"]]);
        s.window = true;
        assert_eq!(
            s.follow(&keys(&["a"])).primary.as_deref(),
            Some("a"),
            "the older stacked share becomes the top share"
        );

        let s = stage(true, &[&["a"], &["a", "b"], &["a"]]);
        assert_eq!(
            s.primary.as_deref(),
            Some("a"),
            "a demoted share moves back"
        );
        assert!(s.demoted.is_empty());

        let mut s = stage(true, &[&["a"], &["a", "b"]]);
        s.window = false;
        let s = s.follow(&keys(&["a"]));
        assert_eq!(
            s.primary.as_deref(),
            Some("a"),
            "the demoted share takes the slot"
        );
        assert!(s.demoted.is_empty());
    }

    #[test]
    fn detaching_a_demoted_share_swaps_it_with_the_shown_one_in_place() {
        let mut s = stage(true, &[&["a"], &["a", "b"], &["a", "b", "c"]]);
        s.take_slot("a");
        assert_eq!(s.primary.as_deref(), Some("a"));
        assert_eq!(s.demoted, keys(&["c", "b"]));
        assert_eq!(
            s.follow(&keys(&["a", "b", "c"])),
            s,
            "a swap survives the next render"
        );
    }

    #[test]
    fn only_a_demoted_share_can_take_the_slot() {
        let mut s = stage(true, &[&["a"], &["a", "b"]]);
        let before = s.clone();
        s.take_slot("b");
        s.take_slot("z");
        assert_eq!(s, before);
    }

    #[test]
    fn the_window_reattaches_only_the_share_it_shows() {
        let ann = ShareTarget {
            origin: ShareOrigin::Received,
            key: "a".into(),
            pin: PinnedTile::screen("ann"),
            name: "Ann".into(),
        };
        let mut s = stage(true, &[&["a"]]);
        s.primary_target = Some(ann.clone());
        assert_eq!(s.bound_target(), Some(ann));
        assert_eq!(
            s.follow(&keys(&["a", "b"])).bound_target(),
            None,
            "a target left by an earlier share is never reattached"
        );
        s.window = false;
        assert_eq!(s.bound_target(), None);
    }

    #[test]
    fn a_window_switch_is_spoken_in_the_meeting_or_in_the_window() {
        let (ann, gone, bea) = (Some(("Ann", true)), Some(("Ann", false)), Some("Bea"));
        let switches = [(ann, bea), (gone, bea), (None, bea), (gone, None)];
        let meeting = [
            "Bea started sharing in the separate window. Ann's shared content moved to the \
             meeting.",
            "Ann stopped sharing. Showing Bea's shared content in the separate window.",
            "Bea started sharing. Showing it in the separate window.",
            "Ann stopped sharing. The separate window stays open for the next share.",
        ];
        let window = [
            "Now showing Bea's shared content",
            "Ann stopped sharing. Now showing Bea's shared content.",
            "Bea started sharing",
            "Ann stopped sharing. Shared content will be displayed here.",
        ];
        for (i, (prev, next)) in switches.into_iter().enumerate() {
            let said = window_switch_announcement(prev, next);
            assert_eq!(said.as_deref(), Some(meeting[i]));
            assert_eq!(window_note(prev, next).as_deref(), Some(window[i]));
        }
        assert_eq!(window_switch_announcement(None, None), None);
        assert_eq!(
            swap_announcement("Ann", Some("Bea")),
            "Showing Ann's shared content in the separate window. Bea's shared content moved to \
             the meeting."
        );
    }

    #[test]
    fn closing_a_waiting_window_reopens_shares_in_the_view_it_was_opened_from() {
        use ShareViewMode as M;
        let slot = |base, pre_detach| ShareSlot {
            base,
            pre_detach,
            ..ShareSlot::default()
        };
        for mode in [M::Tile, M::Enlarged, M::Pinned] {
            assert_eq!(closed_waiting_pref(slot(ShareBase::Tile, Some(mode))), mode);
        }
        assert_eq!(
            closed_waiting_pref(slot(ShareBase::Enlarged, None)),
            M::Enlarged
        );
    }

    #[test]
    fn a_demoted_tile_joins_the_panel_in_the_split_and_keeps_its_seat() {
        let mut placement = PanelPlacement::default();
        let ab = keys(&["a", "b"]);
        assert_eq!(placement.place_demoted(false, &ab), vec![false, false]);
        assert_eq!(placement.place_demoted(true, &ab), vec![true, true]);
        assert_eq!(
            placement.place_demoted(false, &ab),
            vec![true, true],
            "no move when the split ends"
        );
        assert_eq!(
            placement.place_demoted(false, &keys(&["b", "c"])),
            vec![true, false],
            "a new tile starts on the stage"
        );
    }

    #[test]
    fn the_replacement_names_what_is_shared_now() {
        assert_eq!(
            replaced_text(Some("monitor")),
            "Switched to sharing your entire screen"
        );
        assert_eq!(
            replaced_text(Some("window")),
            "Switched to sharing a window"
        );
        assert_eq!(replaced_text(Some("browser")), "Switched to sharing a tab");
        assert_eq!(replaced_text(None), "Switched to sharing new content");
    }

    #[test]
    fn a_replaced_capture_rejudges_the_mirror_guard_by_its_surface() {
        assert_eq!(replaced_guard(Some("monitor"), false, false), Some(true));
        assert_eq!(replaced_guard(Some("window"), true, false), Some(false));
        assert_eq!(replaced_guard(Some("browser"), false, false), None);
        assert_eq!(replaced_guard(Some("monitor"), true, false), None);
        assert_eq!(
            replaced_guard(Some("window"), true, true),
            None,
            "hidden by hand"
        );
        assert_eq!(
            replaced_guard(Some("monitor"), false, true),
            Some(true),
            "a whole screen always hides"
        );
    }
}
