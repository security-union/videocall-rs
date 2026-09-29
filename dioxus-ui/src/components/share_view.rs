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
pub const CTA_TIMEOUT_MS: u32 = 12_000;
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

    pub fn parse(stored: Option<&str>) -> Self {
        match stored {
            Some("enlarged") => ShareViewMode::Enlarged,
            Some("pinned") => ShareViewMode::Pinned,
            Some("detached") => ShareViewMode::Detached,
            _ => ShareViewMode::Tile,
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
/// user activation, so a Detached pref opens directly only from a click.
pub fn arrival_plan(
    pref: ShareViewMode,
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
        ShareViewMode::Detached if detach_supported => plan.cta = CtaState::Shown,
        ShareViewMode::Detached => {}
    }
    plan
}

pub fn own_arrival_plan(pref: ShareViewMode, detach_supported: bool, guard: bool) -> ArrivalPlan {
    if guard {
        ArrivalPlan::default()
    } else {
        arrival_plan(pref, detach_supported, false)
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

pub fn load_share_view_pref(origin: ShareOrigin) -> ShareViewMode {
    let stored = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(origin.pref_key()).ok().flatten());
    ShareViewMode::parse(stored.as_deref())
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
}

thread_local! {
    static TEARDOWN_CAUSE: std::cell::Cell<TeardownCause> =
        const { std::cell::Cell::new(TeardownCause::User) };
    /// A system teardown that cancelled a still-pending open; its reattach
    /// callback fires later, outside `teardown_with_cause`.
    static PENDING_CANCEL: std::cell::RefCell<Option<(String, TeardownCause)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(target_arch = "wasm32")]
fn take_teardown_cause(key: &str) -> TeardownCause {
    let now = TEARDOWN_CAUSE.with(|c| c.replace(TeardownCause::User));
    let pending = PENDING_CANCEL.with(|p| {
        let mut p = p.borrow_mut();
        if p.as_ref().is_some_and(|(k, _)| k == key) {
            p.take().map(|(_, cause)| cause)
        } else {
            None
        }
    });
    pending.unwrap_or(now)
}

/// Close `key`'s detached window, or cancel its pending open, for a reason
/// other than the user's.
pub fn teardown_with_cause(key: &str, cause: TeardownCause) {
    TEARDOWN_CAUSE.with(|c| c.set(cause));
    #[cfg(target_arch = "wasm32")]
    {
        use crate::components::screen_share_detach as ssd;
        if cause != TeardownCause::User && ssd::is_pending(key) {
            PENDING_CANCEL.with(|p| *p.borrow_mut() = Some((key.to_string(), cause)));
        }
        ssd::teardown(key);
    }
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
    ShareSnapshot {
        base: slot.base,
        pinned: ctx.pins.try_peek().is_ok_and(|p| p.contains(&target.pin)),
        detached: detached.as_deref() == Some(target.key.as_str()),
        other_detached: detached.is_some() && detached.as_deref() != Some(target.key.as_str()),
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
    let before = snapshot(ctx, target);
    let t = next_share_view(before, action);
    apply(ctx, target, t);
    match t.detach {
        DetachOp::Keep => {}
        DetachOp::Close => {
            #[cfg(target_arch = "wasm32")]
            crate::components::screen_share_detach::reattach(&target.key);
        }
        DetachOp::Open => {
            if t.reattach_other {
                if let Some(other) = ctx.detached.peek().clone() {
                    teardown_with_cause(&other, TeardownCause::Displaced);
                }
            }
            open_detached(ctx, target);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn open_detached(ctx: ShareViewCtx, target: &ShareTarget) {
    use crate::components::screen_share_detach as ssd;
    use std::cell::Cell;
    use std::rc::Rc;

    let mut detached = ctx.detached;
    detached.set(Some(target.key.clone()));
    let opened = Rc::new(Cell::new(false));
    let on_opened: Box<dyn FnOnce()> = {
        let opened = opened.clone();
        let origin = target.origin;
        Box::new(move || {
            opened.set(true);
            save_share_view_pref(origin, ShareViewMode::Detached);
            let mut slots = ctx.slots;
            if let Ok(mut s) = slots.try_write() {
                s.get_mut(origin).cta = CtaState::Hidden;
            }
            announce(ctx, detach_announcement(origin, true));
        })
    };
    let on_reattach: Box<dyn Fn()> = {
        let target = target.clone();
        Box::new(move || {
            let cause = take_teardown_cause(&target.key);
            let mut detached = ctx.detached;
            if let Ok(mut d) = detached.try_write() {
                if d.as_deref() == Some(target.key.as_str()) {
                    *d = None;
                }
            }
            match cause {
                TeardownCause::System => {}
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
        ShareOrigin::Received => ssd::open(&target.key, &target.name, on_opened, on_reattach),
        ShareOrigin::Own => match ctx.own_stream.peek().clone() {
            Some(stream) => ssd::open_stream(&target.key, stream, on_opened, on_reattach),
            None => on_reattach(),
        },
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn open_detached(_ctx: ShareViewCtx, _target: &ShareTarget) {}

/// Apply an arrival plan to a share that just started.
pub fn begin_share(ctx: ShareViewCtx, target: &ShareTarget, plan: ArrivalPlan, guard: bool) {
    let mut slots = ctx.slots;
    slots.with_mut(|s| {
        *s.get_mut(target.origin) = ShareSlot {
            base: plan.base,
            pre_detach: None,
            cta: plan.cta,
            guard,
        };
    });
    if plan.demote_other {
        demote_other(ctx, target);
    }
    if plan.pin {
        update_pins(ctx.pins, |p| pin_order::pin_front(p, target.pin.clone()));
    }
}

/// The shares seen on the previous render.
#[derive(Default)]
pub struct ShareTracker {
    received: Option<ShareTarget>,
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
    received_user_known: bool,
    still_sharing: &dyn Fn(&str) -> bool,
    own: Option<(&ShareTarget, &web_sys::MediaStream)>,
    detach_supported: bool,
    mut zoom: Signal<HashMap<String, ScreenZoomState>>,
    mut actual: Signal<Option<String>>,
) {
    let prev_key = tracker.received.as_ref().map(|t| t.key.clone());
    if prev_key.as_deref() != received.map(|t| t.key.as_str()) {
        let prev = tracker.received.take();
        if let Some(prev) = prev.as_ref() {
            settle(ctx, prev, ShareAction::Ended);
            zoom.with_mut(|m| m.remove(&prev.key));
            if actual.peek().as_deref() == Some(prev.key.as_str()) {
                actual.set(None);
            }
            let mut slots = ctx.slots;
            slots.with_mut(|s| s.received = ShareSlot::default());
        }
        let mut cta = false;
        tracker.pin_on_resolve = None;
        if let Some(t) = received {
            let mut plan = arrival_plan(load_share_view_pref(t.origin), detach_supported, false);
            let defer_pin = plan.pin && !received_user_known;
            plan.pin &= received_user_known;
            begin_share(ctx, t, plan, false);
            if defer_pin {
                tracker.pin_on_resolve = received_slot(ctx);
            }
            cta = plan.cta == CtaState::Shown;
        }
        let message = switch_announcement(
            prev.as_ref()
                .map(|p| (p.name.as_str(), still_sharing(&p.key))),
            received.map(|t| t.name.as_str()),
            cta,
        );
        if let Some(message) = message {
            announce(ctx, message);
        }
    }
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
            let plan = own_arrival_plan(load_share_view_pref(t.origin), detach_supported, guard);
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
        load_share_view_pref(target.origin),
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
    fn pref_parse_round_trips_and_unknown_is_tile() {
        for m in [
            ShareViewMode::Tile,
            ShareViewMode::Enlarged,
            ShareViewMode::Pinned,
            ShareViewMode::Detached,
        ] {
            assert_eq!(ShareViewMode::parse(Some(m.as_str())), m);
        }
        assert_eq!(ShareViewMode::parse(None), ShareViewMode::Tile);
        assert_eq!(ShareViewMode::parse(Some("")), ShareViewMode::Tile);
        assert_eq!(ShareViewMode::parse(Some("Enlarged")), ShareViewMode::Tile);
        assert_eq!(ShareViewMode::parse(Some("split")), ShareViewMode::Tile);
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
            arrival_plan(ShareViewMode::Tile, true, false),
            ArrivalPlan::default()
        );
    }

    #[test]
    fn arrival_enlarged_pref_stages_the_share() {
        let p = arrival_plan(ShareViewMode::Enlarged, true, false);
        assert_eq!(p.base, ShareBase::Enlarged);
        assert!(p.demote_other);
        assert!(!p.pin);
    }

    #[test]
    fn arrival_pinned_pref_pins_over_a_tile_base() {
        let p = arrival_plan(ShareViewMode::Pinned, true, false);
        assert_eq!(p.base, ShareBase::Tile);
        assert!(p.pin);
        assert!(
            !p.demote_other,
            "arriving pinned leaves the other share alone"
        );
    }

    #[test]
    fn arrival_detached_pref_offers_the_cta_and_never_opens() {
        let p = arrival_plan(ShareViewMode::Detached, true, false);
        assert_eq!(p.base, ShareBase::Tile);
        assert_eq!(p.cta, CtaState::Shown);
        assert!(!p.detach_now, "C3: no popup without a user activation");
    }

    #[test]
    fn arrival_detached_pref_without_detach_support_is_plain_tile() {
        assert_eq!(
            arrival_plan(ShareViewMode::Detached, false, false),
            ArrivalPlan::default()
        );
    }

    #[test]
    fn a_click_applies_the_detached_pref_directly() {
        let p = arrival_plan(ShareViewMode::Detached, true, true);
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
        };
        assert_eq!(view(ShareOrigin::Received, None).root_style(), "order: -3;");
        assert_eq!(view(ShareOrigin::Own, None).root_style(), "order: -2;");
        assert_eq!(
            view(ShareOrigin::Own, Some(1)).root_style(),
            "order: -999;",
            "a pin moves the share ahead of every unpinned tile"
        );
    }
}
