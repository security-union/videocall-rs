// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 1175: imperative glue for detaching shared content into a separate
//! window: a received share's canvas, or (issue 2792) the local capture stream.
//! wasm-only.
//!
//! ## Why this never fights Dioxus (the whole point of the v2 rewrite)
//!
//! The reverted v1 (#1634) MOVED the Dioxus-rendered canvas subtree into a
//! Document Picture-in-Picture document and mutated Dioxus-owned attributes
//! imperatively, which broke Dioxus's single-document invariant. v2 moves
//! NOTHING: the decoder's `<canvas>` stays where Dioxus rendered it in the main
//! document, mounted and painting. We MIRROR it into a plain `<video>` in a
//! window whose DOM is built ENTIRELY by the imperative code here — Dioxus never
//! sees that document, so its controls are plain-DOM buttons with plain
//! listeners that just work.
//!
//! ## Refined behaviour (issue 1175, user test round)
//!
//! While detached, the MAIN window shows the normal grid without the share
//! tile: that tile is moved off-screen (canvas kept mounted + painting so the
//! mirror stays live — an off-screen, still-composited canvas keeps delivering
//! `captureStream` frames, and the active sharer is unconditionally in
//! `active_decode_set` so decode never stops). ALL detach affordances — zoom
//! in/out/reset, pan (drag + keyboard), and reattach (button / Escape / closing
//! the window) — live in the DETACHED window, built here imperatively, reusing
//! the pure [`super::screen_share_zoom`] math for clamps/steps.
//!
//! ## Mirror mechanism
//!
//! [`HtmlCanvasElement::capture_stream`] feeds the `<video>`; the browser
//! composites the `MediaStream` on the GPU (zero per-frame JS). The video is
//! muted + `playsinline` and is EXPLICITLY `play()`-ed after `srcObject` is set —
//! autoplay of a programmatically-built srcObject video is unreliable across
//! window types / autoplay policies, and an unplayed video renders nothing;
//! explicit `play()` is the robust fix (an isolated repro confirmed the capture
//! → cross-document video path renders live frames once played).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    AddEventListenerOptions, CanvasRenderingContext2d, Document, Element, HtmlCanvasElement,
    HtmlElement, HtmlVideoElement, KeyboardEvent, MediaStream, PointerEvent, WheelEvent, Window,
};

use super::screen_share_detach_sizing::{
    detached_window_inner_dims, maximize_button_spec, DETACHED_BAR_H_PX, DETACHED_MIN_H,
    DETACHED_MIN_W,
};
use super::screen_share_zoom as zoom;
use crate::context::ScreenZoomState;

// ---------------------------------------------------------------------------
// Stable ids inside the detached document (the e2e author reaches these via the
// popup Page). Kept in sync with the published contract.
// ---------------------------------------------------------------------------

const REATTACH_BTN_ID: &str = "ss-detached-reattach";
const NAME_ID: &str = "ss-detached-name";
const WAITING_ID: &str = "ss-detached-waiting";
const ANNOUNCE_ID: &str = "ss-detached-announce";
const HANDOVER_TOAST_ID: &str = "ss-detached-handover-toast";
const REATTACH_DESC_ID: &str = "ss-detached-reattach-desc";
const WAITING_HINT_ID: &str = "ss-detached-waiting-hint";
const WAITING_NAME: &str = "Waiting for shared content";
const WAITING_HTML: &str = "\
<svg class=\"ss-detached-waiting-icon\" aria-hidden=\"true\" viewBox=\"0 0 24 24\" fill=\"none\" \
stroke=\"currentColor\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\">\
<rect x=\"2\" y=\"3\" width=\"20\" height=\"14\" rx=\"2\" ry=\"2\"/>\
<line x1=\"8\" y1=\"21\" x2=\"16\" y2=\"21\"/><line x1=\"12\" y1=\"17\" x2=\"12\" y2=\"21\"/></svg>\
<h1 class=\"ss-detached-waiting-title\">Shared content will be displayed here</h1>\
<p id=\"ss-detached-waiting-hint\" class=\"ss-detached-waiting-hint\">New shared content \
appears here automatically. To view it in the meeting window instead, select Reattach.</p>";
const WAITING_HIDES: &str = ".ss-detached-controls, .ss-detached-maximize, .ss-detached-viewport";
const VIEWPORT_ID: &str = "ss-detached-viewport";
const WRAPPER_ID: &str = "ss-detached-wrapper";
const VIDEO_ID: &str = "ss-detached-video";
const ZOOM_IN_ID: &str = "ss-detached-zoom-in";
const ZOOM_OUT_ID: &str = "ss-detached-zoom-out";
const ZOOM_RESET_ID: &str = "ss-detached-zoom-reset";
const ZOOM_LABEL_ID: &str = "ss-detached-zoom-label";
/// Issue 1821: actual-size (1:1), maximize, and stats-overlay element ids.
const ZOOM_ACTUAL_ID: &str = "ss-detached-zoom-actual";
const MAXIMIZE_BTN_ID: &str = "ss-detached-maximize";
const METRICS_ID: &str = "ss-detached-metrics";

/// Static, app-controlled title for the detached window. NEVER the peer name:
/// peer-controlled text must not reach OS window chrome (security), and a fixed
/// string also fixes the blank `about:blank` popup title.
const DETACHED_WINDOW_TITLE: &str = "Shared content";
const OWN_WINDOW_TITLE: &str = "Your shared content";

// ---------------------------------------------------------------------------
// Detached-window sizing (issue #1842): the pure aspect-fit / clamp math lives in
// `screen_share_detach_sizing` (host-testable). This module reads the live decoded
// canvas dims + the available screen and calls it.
// ---------------------------------------------------------------------------

/// Upper clamp for the undecoded-fallback client-box path (pre-#1842 behavior).
const DETACHED_FALLBACK_MAX_W: i32 = 2560;
const DETACHED_FALLBACK_MAX_H: i32 = 1600;

/// Available screen size (`screen.availWidth`/`availHeight`) for sizing the
/// detached window, with a sane 1920x1080 fallback when the API is unavailable.
fn available_screen(win: &Window) -> (i32, i32) {
    match win.screen() {
        Ok(s) => (
            s.avail_width().unwrap_or(1920),
            s.avail_height().unwrap_or(1080),
        ),
        Err(_) => (1920, 1080),
    }
}

// ---------------------------------------------------------------------------
// The single detached window's live state (one-at-a-time by design).
// ---------------------------------------------------------------------------

struct DetachState {
    peer: String,
    win: Window,
    doc: Document,
    video: HtmlVideoElement,
    mirror: Option<Mirror>,
    source: Option<DetachSource>,
    title: &'static str,
    name: String,
    follows: bool,
    /// Reattach callback (flips the Dioxus `DetachedShareCtx` to `None`).
    on_reattach: Box<dyn Fn()>,
    /// Per-detached-window zoom/pan state (independent of main-window zoom).
    zoom: Rc<RefCell<ScreenZoomState>>,
    actual: Rc<Cell<bool>>,
    /// Kept alive for the window's lifetime; dropped (detaching listeners) on
    /// teardown.
    _listeners: Vec<ListenerHandle>,
    /// `setInterval` id polling `win.closed`. Cleared on teardown.
    close_poll_id: Option<i32>,
    /// Issue 1821: `setInterval` id for the ~1 Hz detached stats-overlay fps
    /// sampler (present only when the media-metrics checkbox was on at open).
    /// Cleared on teardown alongside `close_poll_id`.
    metrics_poll_id: Option<i32>,
    /// Checked by `teardown_if_session` and the deferred Failed teardown in `follow_want`.
    session: u64,
}

/// Owns a parked event `Closure` so it outlives `finish_open` and is dropped on
/// teardown. The detached document is torn down on close, so explicit
/// `removeEventListener` is unnecessary — dropping the closure invalidates it.
/// The held closures are never READ, only kept alive (RAII), hence `dead_code`.
struct ListenerHandle {
    _closure: ClosureKind,
}

#[allow(dead_code)]
enum ClosureKind {
    Plain(Closure<dyn FnMut()>),
    Pointer(Closure<dyn FnMut(PointerEvent)>),
    Key(Closure<dyn FnMut(KeyboardEvent)>),
    Wheel(Closure<dyn FnMut(WheelEvent)>),
    Event(Closure<dyn FnMut(web_sys::Event)>),
}

type PageLeaveListener = Closure<dyn FnMut(web_sys::Event)>;

thread_local! {
    static DETACH: RefCell<Option<DetachState>> = const { RefCell::new(None) };
    static SESSION_SEQ: Cell<u64> = const { Cell::new(0) };
    /// Main-window `pagehide` listener, armed while a detached window is open.
    static PAGE_LEAVE: RefCell<Option<PageLeaveListener>> = const { RefCell::new(None) };
    /// `(share key, name)` a received-share window shows.
    static WANT: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
}

pub fn is_busy() -> bool {
    DETACH.with(|d| d.borrow().is_some())
}

/// Close every detached window without a reattach, for a page that is being
/// left (the leave paths navigate away without unmounting any tile).
pub fn close_all() {
    listen_for_page_leave(false);
    let Some(mut state) = DETACH.with(|d| d.borrow_mut().take()) else {
        return;
    };
    if let (Some(id), Some(main)) = (state.close_poll_id.take(), web_sys::window()) {
        main.clear_interval_with_handle(id);
    }
    if let Some(id) = state.metrics_poll_id.take() {
        clear_popup_interval(&state.win, id);
    }
    let _ = state.win.close();
    state.stop_mirror();
    WANT.with(|w| w.borrow_mut().take());
}

/// `persisted`: the page goes into the back/forward cache and may be shown again.
fn leave_page(event: web_sys::Event) {
    let persisted =
        js_sys::Reflect::get(&event, &JsValue::from_str("persisted")).is_ok_and(|p| p.is_truthy());
    if !persisted {
        close_all();
    } else if let Some(key) = DETACH.with(|d| d.borrow().as_ref().map(|s| s.peer.clone())) {
        super::share_view::teardown_with_cause(&key, super::share_view::TeardownCause::System);
    }
}

fn listen_for_page_leave(on: bool) {
    let Some(win) = web_sys::window() else {
        return;
    };
    PAGE_LEAVE.with(|l| {
        let mut l = l.borrow_mut();
        let cb = l.get_or_insert_with(|| PageLeaveListener::new(leave_page));
        let _ = if on {
            win.add_event_listener_with_callback("pagehide", cb.as_ref().unchecked_ref())
        } else {
            win.remove_event_listener_with_callback("pagehide", cb.as_ref().unchecked_ref())
        };
    });
}

/// Next monotonic detached-session id.
fn next_session() -> u64 {
    SESSION_SEQ.with(|s| {
        let v = s.get().wrapping_add(1);
        s.set(v);
        v
    })
}

fn cross_realm_cast<T: JsCast>(value: impl JsCast) -> T {
    value.unchecked_into::<T>()
}

/// Defer [`teardown_if_session`] to a microtask. Called from the PARKED
/// event closures (pagehide, poll, Escape, reattach button), which live inside
/// [`DetachState`]: `teardown` drops that state — and with it the running closure —
/// so it must not run while such a closure is still on the stack. Direct callers
/// that are NOT parked closures (main-window [`reattach`], own tile unmount,
/// leaving the meeting) call the session-agnostic [`teardown`] synchronously.
fn schedule_teardown(peer: String, session: u64) {
    wasm_bindgen_futures::spawn_local(async move {
        teardown_if_session(&peer, session);
    });
}

fn teardown_if_session(peer: &str, session: u64) {
    let is_current = DETACH.with(|d| {
        d.borrow()
            .as_ref()
            .map(|s| s.peer == peer && s.session == session)
            .unwrap_or(false)
    });
    if is_current {
        teardown(peer);
    }
}

// ---------------------------------------------------------------------------
// Mirror seam.
// ---------------------------------------------------------------------------

/// What the detached window shows: a received share's decoder canvas, or the
/// local capture itself (issue 2792).
#[derive(Clone)]
enum DetachSource {
    Canvas(HtmlCanvasElement),
    Stream(MediaStream),
}

struct Mirror {
    stream: MediaStream,
    canvas: Option<HtmlCanvasElement>,
}

impl Mirror {
    /// Capture `source` and play it into `video`. Returns `None` if capture
    /// fails (e.g. a tainted canvas — never expected here). Explicit `play()` is
    /// what actually makes frames appear; the `autoplay` attribute alone is
    /// unreliable for a programmatically-built srcObject video.
    fn start(source: &DetachSource, video: &HtmlVideoElement) -> Option<Mirror> {
        let canvas = match source {
            DetachSource::Canvas(c) => c,
            DetachSource::Stream(stream) => {
                video.set_src_object(Some(stream));
                play_video(video);
                return Some(Mirror {
                    stream: Clone::clone(stream),
                    canvas: None,
                });
            }
        };
        let stream = match canvas.capture_stream() {
            Ok(s) => s,
            Err(e) => {
                log::warn!("issue 1175: canvas.captureStream failed, cannot detach: {e:?}");
                return None;
            }
        };
        video.set_src_object(Some(&stream));
        play_video(video);
        // Prime the popup with the CURRENT canvas still (issue #1841). An auto-rate
        // captureStream emits a frame only when the source canvas REPAINTS, and a
        // static screen share leaves the receiver's source canvas painted but idle
        // (the decoder repaints only on new decoded frames, issue #1783) — so the
        // capture is starved and the popup mirror never receives a frame. Force a
        // no-op source-canvas repaint so captureStream emits the current bitmap.
        prime_static_source(canvas);
        videocall_client::set_mirrored_canvas(Some(canvas));
        Some(Mirror {
            stream,
            canvas: Some(canvas.clone()),
        })
    }

    fn stop(&self) {
        if self.canvas.is_none() {
            return;
        }
        videocall_client::set_mirrored_canvas(None);
        let tracks = self.stream.get_tracks();
        for i in 0..tracks.length() {
            if let Ok(track) = tracks.get(i).dyn_into::<web_sys::MediaStreamTrack>() {
                track.stop();
            }
        }
    }
}

impl DetachState {
    fn reset_zoom(&self) {
        *self.zoom.borrow_mut() = ScreenZoomState::default();
        self.actual.set(false);
        apply_detached_zoom(&self.doc, &ScreenZoomState::default(), false);
    }

    fn stop_mirror(&mut self) {
        if let Some(mirror) = self.mirror.take() {
            mirror.stop();
        }
        self.source = None;
        self.video.set_src_object(None);
    }

    /// Mirror the canvas of the share [`WANT`] names; with none, the window
    /// waits. `gone` is a canvas being unmounted.
    fn follow_want(&mut self, gone: Option<&HtmlCanvasElement>) {
        let want = WANT.with(|w| w.borrow().clone());
        let canvas = want
            .as_ref()
            .and_then(|(key, _)| share_canvas(key))
            .filter(|c| Some(c) != gone);
        let current = match &self.source {
            Some(DetachSource::Canvas(c)) => Some(c.clone()),
            _ => None,
        };
        if canvas != current {
            self.stop_mirror();
            if let Some(canvas) = canvas {
                let source = DetachSource::Canvas(canvas);
                self.mirror = Mirror::start(&source, &self.video);
                if self.mirror.is_none() {
                    let (window, session) = (self.peer.clone(), self.session);
                    wasm_bindgen_futures::spawn_local(async move {
                        use super::share_view::{teardown_with_cause, TeardownCause};
                        if DETACH
                            .with(|d| d.borrow().as_ref().is_some_and(|s| s.session == session))
                        {
                            teardown_with_cause(&window, TeardownCause::Failed);
                        }
                    });
                }
                self.source = Some(source);
            }
            self.reset_zoom();
        }
        self.name = want.clone().map(|(_, name)| name).unwrap_or_default();
        let name = want.map(|(_, name)| name);
        render_binding(&self.doc, self.title, name.as_deref());
    }
}

fn share_canvas(key: &str) -> Option<HtmlCanvasElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(&zoom::screen_canvas_id(key))?
        .dyn_into::<HtmlCanvasElement>()
        .ok()
        .filter(|c| c.is_connected())
}

fn render_binding(doc: &Document, live_title: &str, name: Option<&str>) {
    if let Some(el) = doc.get_element_by_id(NAME_ID) {
        el.set_text_content(Some(name.unwrap_or(WAITING_NAME)));
    }
    let state = if name.is_some() { "live" } else { "waiting" };
    let Some(body) = doc.body() else {
        return;
    };
    if body.get_attribute("data-ss-state").as_deref() == Some(state) {
        return;
    }
    let in_hidden_control = doc
        .active_element()
        .and_then(|a| a.closest(WAITING_HIDES).ok().flatten())
        .is_some();
    let _ = body.set_attribute("data-ss-state", state);
    let Some(btn) = doc.get_element_by_id(REATTACH_BTN_ID) else {
        return;
    };
    if name.is_some() {
        doc.set_title(live_title);
        let _ = btn.set_attribute("aria-describedby", REATTACH_DESC_ID);
        return;
    }
    doc.set_title(WAITING_NAME);
    let _ = btn.set_attribute("aria-describedby", WAITING_HINT_ID);
    if doc.fullscreen_element().is_some() {
        let _ = doc.exit_fullscreen();
    }
    if let Some(toast) = doc.get_element_by_id(HANDOVER_TOAST_ID) {
        toast.remove();
    }
    if in_hidden_control {
        let btn: HtmlElement = cross_realm_cast(btn);
        let _ = btn.focus();
    }
}

fn show_handover_toast(doc: &Document, name: &str) {
    if let Some(old) = doc.get_element_by_id(HANDOVER_TOAST_ID) {
        old.remove();
    }
    let (Some(body), Ok(toast)) = (doc.body(), doc.create_element("div")) else {
        return;
    };
    let _ = toast.set_attribute("id", HANDOVER_TOAST_ID);
    let _ = toast.set_attribute("class", "ss-detached-toast");
    let _ = toast.set_attribute("aria-hidden", "true");
    toast.set_text_content(Some(&format!("Now showing {name}'s shared content")));
    let _ = body.append_child(&toast);
    wasm_bindgen_futures::spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(4000).await;
        toast.remove();
    });
}

fn follow_want(gone: Option<&HtmlCanvasElement>) {
    DETACH.with(|d| {
        if let Some(state) = d.borrow_mut().as_mut().filter(|s| s.follows) {
            state.follow_want(gone);
        }
    });
}

/// Speak `note` in the received-share window's live region.
pub fn say(note: &str) {
    DETACH.with(|d| {
        let d = d.borrow();
        let state = d.as_ref().filter(|s| s.follows);
        if let Some(region) = state.and_then(|s| s.doc.get_element_by_id(ANNOUNCE_ID)) {
            region.set_text_content(Some(note));
        }
    });
}

pub fn received_window_focused() -> bool {
    DETACH.with(|d| {
        d.borrow()
            .as_ref()
            .filter(|s| s.follows)
            .is_some_and(|s| s.doc.has_focus().unwrap_or(false))
    })
}

fn wants(key: &str) -> bool {
    WANT.with(|w| w.borrow().as_ref().is_some_and(|(k, _)| k == key))
}

/// Point the received-share window at `(share key, name)`, or leave it
/// waiting for the next share with `None`. `note` is spoken in the window
/// when the share it shows changes.
pub fn show(binding: Option<(&str, &str)>, note: Option<&str>) {
    let before = WANT.with(|w| w.borrow().as_ref().map(|(k, _)| k.clone()));
    WANT.with(|w| *w.borrow_mut() = binding.map(|(k, n)| (k.to_string(), n.to_string())));
    let moved = before.as_deref() != binding.map(|(k, _)| k);
    DETACH.with(|d| {
        let mut d = d.borrow_mut();
        let Some(state) = d.as_mut().filter(|s| s.follows) else {
            return;
        };
        state.follow_want(None);
        if !moved {
            return;
        }
        if let Some(region) = state.doc.get_element_by_id(ANNOUNCE_ID) {
            region.set_text_content(note);
        }
        if let (Some(_), Some((_, name))) = (before, binding) {
            show_handover_toast(&state.doc, name);
        }
    });
}

/// Point the own share's window at the share's new capture, unzoomed.
pub fn replace_stream(stream: MediaStream) {
    DETACH.with(|d| {
        let mut d = d.borrow_mut();
        let Some(state) = d.as_mut().filter(|s| !s.follows) else {
            return;
        };
        state.stop_mirror();
        let source = DetachSource::Stream(stream);
        state.mirror = Mirror::start(&source, &state.video);
        state.source = Some(source);
        state.reset_zoom();
    });
}

pub fn canvas_ready(key: &str) {
    if wants(key) {
        follow_want(None);
    }
}

pub fn canvas_gone(key: &str, canvas: &HtmlCanvasElement) {
    if wants(key) {
        follow_want(Some(canvas));
    }
}

/// Explicitly play `video`, logging (not panicking on) a rejected promise. A
/// muted video is allowed to play without a user gesture, so this reliably
/// resolves (verified in the isolated repro across window types + autoplay
/// policies); the `.play()` call is what actually makes frames appear, since the
/// `autoplay` attribute alone is unreliable for a programmatically-built
/// srcObject video. A rejection would leave the browser's own play affordance on
/// the (paused) video, so no bespoke retry is wired.
fn play_video(video: &HtmlVideoElement) {
    if let Ok(promise) = video.play() {
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = wasm_bindgen_futures::JsFuture::from(promise).await {
                log::warn!("issue 1175: detached video.play() rejected: {e:?}");
            }
        });
    }
}

/// Surface the source canvas's CURRENT contents into a freshly-started mirror
/// (issue #1841).
///
/// A detached mirror of a STATIC share would otherwise stay black. `capture_stream()`
/// runs at AUTO frame rate, which emits a frame only when the source canvas next
/// REPAINTS — and a static share's source canvas does not repaint (the decoder
/// repaints on new decoded frames only, issue #1783). `CanvasCaptureMediaStreamTrack.
/// requestFrame()` does NOT help: it is defined for a 0-fps (manual) track, so on an
/// auto-rate track it is a no-op — verified empirically, the popup `<video>` never
/// reached `readyState >= 2` (an earlier `requestFrame`-based prime failed the live
/// headed receipt).
///
/// The robust fix is to REPAINT the source canvas so the auto-rate capture emits the
/// current bitmap. Reading a 1px `ImageData` and writing it straight back dirties the
/// 2D canvas WITHOUT changing any pixel, so the browser captures the WHOLE current
/// bitmap on the next compositing step. Once ONE frame reaches the paused mirror it
/// holds the still, so this is repeated a few times over ~1s to cover the case where
/// the first repaint lands before the fresh stream/video capture pipeline is ready to
/// receive it. Harmless for a LIVE source (the decoder overwrites on its next frame),
/// and every step is fail-soft (missing canvas / non-2D context / tainted read is
/// skipped) — it never panics.
fn prime_static_source(source: &HtmlCanvasElement) {
    let source = source.clone();
    wasm_bindgen_futures::spawn_local(async move {
        for delay_ms in [0u32, 80, 200, 400, 700, 1100] {
            if delay_ms > 0 {
                gloo_timers::future::TimeoutFuture::new(delay_ms).await;
            }
            let Ok(Some(obj)) = source.get_context("2d") else {
                continue;
            };
            let Ok(ctx) = obj.dyn_into::<CanvasRenderingContext2d>() else {
                continue;
            };
            // Read-and-write-back a 1px region: a genuine no-op that still marks the
            // canvas dirty so the auto-rate captureStream captures the current frame.
            if let Ok(px) = ctx.get_image_data(0.0, 0.0, 1.0, 1.0) {
                let _ = ctx.put_image_data(&px, 0.0, 0.0);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Feature detection.
// ---------------------------------------------------------------------------

pub fn detach_supported() -> bool {
    web_sys::window().is_some_and(|w| available_screen(&w).0 >= 768)
}

// ---------------------------------------------------------------------------
// Open / teardown.
// ---------------------------------------------------------------------------

/// Open the received-share window `window`, mirroring `peer`'s canvas until
/// [`show`] points it elsewhere. `on_reattach` is invoked exactly once when the
/// window closes, unless [`close_all`] closes it, and at once when no window opens:
/// one is already open, the canvas is missing, the popup is blocked, or its
/// DOM/mirror cannot be built.
pub fn open(
    window: &str,
    peer: &str,
    display_name: &str,
    on_opened: Box<dyn FnOnce()>,
    on_reattach: Box<dyn Fn()>,
) {
    if is_busy() {
        on_reattach();
        return;
    }
    let win = match web_sys::window() {
        Some(w) => w,
        None => {
            // No silent detach abort (issue 1829): these bailouts are
            // feature/environment mismatches, not failures — debug, not warn.
            log::debug!("issue 1175: no window; cannot detach");
            on_reattach();
            return;
        }
    };
    let source = match win
        .document()
        .and_then(|d| d.get_element_by_id(&zoom::screen_canvas_id(peer)))
        .and_then(|e| e.dyn_into::<HtmlCanvasElement>().ok())
    {
        Some(c) => c,
        None => {
            log::debug!("issue 1175: source screen-share canvas not found; cannot detach");
            on_reattach();
            return;
        }
    };

    // Size the detached window to the shared content's aspect (issue #1842). The
    // source canvas's width/height are the DECODED resolution (the decoder sets
    // them per frame; peer_decoder.rs). Before the first decode the canvas is the
    // HTML default 300x150 — fall back to the pre-#1842 client-box clamp then, and
    // resize-on-first-decode is out of scope (a rare race; object-fit letterboxes).
    let content_w = source.width() as i32;
    let content_h = source.height() as i32;
    let (w, h) = if content_w <= 0 || content_h <= 0 || (content_w == 300 && content_h == 150) {
        (
            source
                .client_width()
                .clamp(DETACHED_MIN_W, DETACHED_FALLBACK_MAX_W),
            source
                .client_height()
                .clamp(DETACHED_MIN_H, DETACHED_FALLBACK_MAX_H),
        )
    } else {
        let (avail_w, avail_h) = available_screen(&win);
        detached_window_inner_dims(content_w, content_h, avail_w, avail_h, DETACHED_BAR_H_PX)
    };

    WANT.with(|w| *w.borrow_mut() = Some((peer.to_string(), display_name.to_string())));
    open_popup(
        &win,
        OpenRequest {
            peer: window.to_string(),
            name: display_name.to_string(),
            title: DETACHED_WINDOW_TITLE,
            source: DetachSource::Canvas(source),
            follows: true,
            size: (w, h),
        },
        on_opened,
        on_reattach,
    );
}

/// Detach the local user's own share: the window plays `stream` directly.
pub fn open_stream(
    key: &str,
    stream: MediaStream,
    on_opened: Box<dyn FnOnce()>,
    on_reattach: Box<dyn Fn()>,
) {
    if is_busy() {
        on_reattach();
        return;
    }
    let Some(win) = web_sys::window() else {
        on_reattach();
        return;
    };
    let (content_w, content_h) = stream_dims(&stream).unwrap_or((1280, 720));
    let (avail_w, avail_h) = available_screen(&win);
    let size =
        detached_window_inner_dims(content_w, content_h, avail_w, avail_h, DETACHED_BAR_H_PX);
    open_popup(
        &win,
        OpenRequest {
            peer: key.to_string(),
            name: OWN_WINDOW_TITLE.to_string(),
            title: OWN_WINDOW_TITLE,
            source: DetachSource::Stream(stream),
            follows: false,
            size,
        },
        on_opened,
        on_reattach,
    );
}

fn stream_dims(stream: &MediaStream) -> Option<(i32, i32)> {
    let track = stream.get_video_tracks().get(0);
    let get_settings = js_sys::Reflect::get(&track, &JsValue::from_str("getSettings")).ok()?;
    let settings = get_settings
        .dyn_ref::<js_sys::Function>()?
        .call0(&track)
        .ok()?;
    let dim = |k: &str| {
        js_sys::Reflect::get(&settings, &JsValue::from_str(k))
            .ok()
            .and_then(|v| v.as_f64())
            .filter(|v| *v > 0.0)
            .map(|v| v as i32)
    };
    Some((dim("width")?, dim("height")?))
}

struct OpenRequest {
    peer: String,
    name: String,
    title: &'static str,
    source: DetachSource,
    follows: bool,
    size: (i32, i32),
}

fn open_popup(
    win: &Window,
    req: OpenRequest,
    on_opened: Box<dyn FnOnce()>,
    on_reattach: Box<dyn Fn()>,
) {
    let (w, h) = req.size;
    let features = format!("popup=yes,width={w},height={h}");
    let popup = match win.open_with_url_and_target_and_features("", "_blank", &features) {
        Ok(Some(p)) => p,
        _ => {
            log::warn!("issue 1175: window.open for detach was blocked");
            on_reattach();
            return;
        }
    };
    let opener_holder = popup.clone();
    finish_open(popup, req, on_opened, on_reattach);
    sever_opener(&opener_holder);
}

fn sever_opener(popup: &Window) {
    let _ = js_sys::Reflect::set(popup, &JsValue::from_str("opener"), &JsValue::NULL);
}

/// `clearInterval` on the popup without letting an error abort the caller.
fn clear_popup_interval(popup: &Window, id: i32) {
    let Ok(clear) = js_sys::Reflect::get(popup, &JsValue::from_str("clearInterval")) else {
        return;
    };
    if clear.is_function() {
        let clear: js_sys::Function = clear.unchecked_into();
        let _ = clear.call1(popup, &JsValue::from(id));
    }
}

/// Build the detached document (mirror video + zoom controls), start the mirror,
/// then wire every close path + the zoom/pan/maximize controls via
/// [`install_detached`]. On a build/mirror failure the fresh window is closed and
/// the share snaps back to the main window (reattach).
fn finish_open(
    detached_win: Window,
    req: OpenRequest,
    on_opened: Box<dyn FnOnce()>,
    on_reattach: Box<dyn Fn()>,
) {
    match build_and_start_mirror(&detached_win, &req.name, req.title, &req.source) {
        Some((doc, video, mirror, show_metrics)) => {
            install_detached(
                detached_win,
                doc,
                video,
                mirror,
                &req,
                show_metrics,
                on_reattach,
            );
            on_opened();
        }
        None => {
            // Silent-abort guard (issue 1829): every detach bail-out logs so a
            // field failure is diagnosable instead of a blank window + snap-back.
            log::warn!("issue 1175: could not build the detached window; aborting detach");
            let _ = detached_win.close();
            on_reattach();
        }
    }
}

fn build_and_start_mirror(
    detached_win: &Window,
    display_name: &str,
    title: &str,
    source: &DetachSource,
) -> Option<(Document, HtmlVideoElement, Mirror, bool)> {
    let doc = detached_win.document()?;
    // Issue 1821: mirror the main window's media-metrics checkbox once at open.
    // A later toggle does NOT reactively update the detached overlay (documented
    // limitation) — the detached document is plain DOM, not a Dioxus subscriber.
    let show_metrics = crate::local_storage::load_bool(
        super::media_metrics_overlay::MEDIA_METRICS_OVERLAY_KEY,
        false,
    );
    let video = build_detached_dom(&doc, display_name, title, show_metrics)?;
    let mirror = Mirror::start(source, &video)?;
    Some((doc, video, mirror, show_metrics))
}

fn install_detached(
    detached_win: Window,
    doc: Document,
    video: HtmlVideoElement,
    mirror: Mirror,
    req: &OpenRequest,
    show_metrics: bool,
    on_reattach: Box<dyn Fn()>,
) {
    let peer = req.peer.as_str();
    let session = next_session();

    let zoom_state = Rc::new(RefCell::new(ScreenZoomState::default()));
    // Issue 1821: the detached-window 1:1 INTENT (mirrors the tile's
    // ScreenActualSizeCtx). Drives the 1:1 button's aria-pressed WITHOUT a
    // per-apply layout read; set true only on a 1:1 engage, cleared by any other
    // explicit zoom, preserved across pans.
    let actual_engaged = Rc::new(Cell::new(false));

    DETACH.with(|d| {
        *d.borrow_mut() = Some(DetachState {
            peer: peer.to_string(),
            win: detached_win.clone(),
            doc: doc.clone(),
            video: video.clone(),
            mirror: Some(mirror),
            source: Some(req.source.clone()),
            title: req.title,
            name: req.name.clone(),
            follows: req.follows,
            on_reattach,
            zoom: zoom_state.clone(),
            actual: actual_engaged.clone(),
            _listeners: Vec::new(),
            close_poll_id: None,
            metrics_poll_id: None,
            session,
        });
    });
    listen_for_page_leave(true);

    let mut listeners = Vec::new();

    let peer_close = peer.to_string();
    let close_cb =
        Closure::<dyn FnMut()>::new(move || schedule_teardown(peer_close.clone(), session));
    let _ = detached_win
        .add_event_listener_with_callback("pagehide", close_cb.as_ref().unchecked_ref());
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Plain(close_cb),
    });

    // Poll `win.closed` (window.open popups don't reliably fire pagehide).
    let peer_poll = peer.to_string();
    let win_poll = detached_win.clone();
    let poll_cb = Closure::<dyn FnMut()>::new(move || {
        if win_poll.closed().unwrap_or(false) {
            schedule_teardown(peer_poll.clone(), session);
        }
    });
    let poll_id = web_sys::window().and_then(|main| {
        main.set_interval_with_callback_and_timeout_and_arguments_0(
            poll_cb.as_ref().unchecked_ref(),
            400,
        )
        .ok()
    });
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Plain(poll_cb),
    });

    // Reattach button (plain listener in the doc we own).
    let peer_btn = peer.to_string();
    let reattach_cb =
        Closure::<dyn FnMut()>::new(move || schedule_teardown(peer_btn.clone(), session));
    if let Some(btn) = doc.get_element_by_id(REATTACH_BTN_ID) {
        let _ = btn.add_event_listener_with_callback("click", reattach_cb.as_ref().unchecked_ref());
    }
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Plain(reattach_cb),
    });

    // Escape-to-reattach. Issue 1821 guard: while FULLSCREEN
    // (Maximize on a popup), Escape is the browser's native exit-fullscreen key,
    // so it must NOT tear the detach down — early-return and let the UA collapse
    // fullscreen. Teardown/reattach only happens when NOT fullscreen.
    let peer_esc = peer.to_string();
    let doc_esc = doc.clone();
    let esc_cb = Closure::<dyn FnMut(KeyboardEvent)>::new(move |e: KeyboardEvent| {
        if e.key() == "Escape" {
            if doc_esc.fullscreen_element().is_some() {
                return;
            }
            schedule_teardown(peer_esc.clone(), session);
        }
    });
    let _ = doc.add_event_listener_with_callback("keydown", esc_cb.as_ref().unchecked_ref());
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Key(esc_cb),
    });

    wire_maximize(&doc, &mut listeners);

    // Issue 1821: detached stats overlay — resolution on the video `resize` event,
    // fps sampled ~1 Hz via getVideoPlaybackQuality. Only when the checkbox was on
    // at open. Returns the interval id so teardown can clear it.
    let metrics_poll_id = if show_metrics {
        wire_detached_metrics(&doc, &detached_win, &video, &mut listeners)
    } else {
        None
    };

    // Zoom + pan controls (imperative, reusing the pure zoom math).
    wire_zoom_controls(&doc, &zoom_state, &actual_engaged, &mut listeners);

    // A dropped link or file must not navigate the window away.
    let block_drop = Closure::<dyn FnMut(web_sys::Event)>::new(|e: web_sys::Event| {
        e.prevent_default();
    });
    for kind in ["dragover", "drop"] {
        let _ = doc.add_event_listener_with_callback(kind, block_drop.as_ref().unchecked_ref());
    }
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Event(block_drop),
    });

    // Focus the reattach button so keyboard users land on a real control.
    // `cross_realm_cast` (not `dyn_into`) because the button lives in the
    // detached document's realm (issue 1829).
    if let Some(btn) = doc.get_element_by_id(REATTACH_BTN_ID) {
        let btn: HtmlElement = cross_realm_cast(btn);
        let _ = btn.focus();
    }

    DETACH.with(|d| {
        if let Some(s) = d.borrow_mut().as_mut() {
            s._listeners = listeners;
            s.close_poll_id = poll_id;
            s.metrics_poll_id = metrics_poll_id;
        }
    });
    follow_want(None);
}

fn build_detached_dom(
    doc: &Document,
    display_name: &str,
    title: &str,
    show_metrics: bool,
) -> Option<HtmlVideoElement> {
    let body = doc.body()?;
    doc.set_title(title);
    body.set_inner_html("");
    let _ = body.set_attribute("class", "ss-detached-body");
    let _ = body.set_attribute("data-ss-state", "live");

    if let Some(head) = doc.head() {
        if let Ok(style) = doc.create_element("style") {
            style.set_text_content(Some(DETACHED_CSS));
            let _ = head.append_child(&style);
        }
    }

    // --- title / control bar ---
    let bar = doc.create_element("div").ok()?;
    let _ = bar.set_attribute("class", "ss-detached-bar");

    let name_el = doc.create_element("span").ok()?;
    let _ = name_el.set_attribute("id", NAME_ID);
    let _ = name_el.set_attribute("class", "ss-detached-name");
    name_el.set_text_content(Some(display_name)); // textContent — peer-controlled
    let _ = bar.append_child(&name_el);

    let controls = doc.create_element("div").ok()?;
    let _ = controls.set_attribute("class", "ss-detached-controls");
    let _ = controls.set_attribute("role", "group");
    let _ = controls.set_attribute("aria-label", "Zoom shared content");
    let zoom_out_btn = make_btn(doc, ZOOM_OUT_ID, "Zoom out", "\u{2212}")?; // minus
    let _ = controls.append_child(&zoom_out_btn);
    let label = doc.create_element("span").ok()?;
    let _ = label.set_attribute("id", ZOOM_LABEL_ID);
    let _ = label.set_attribute("class", "ss-detached-zoom-label");
    let _ = label.set_attribute("role", "status");
    let _ = label.set_attribute("aria-live", "polite");
    label.set_text_content(Some("100%"));
    let _ = controls.append_child(&label);
    let zoom_in_btn = make_btn(doc, ZOOM_IN_ID, "Zoom in", "+")?;
    let _ = controls.append_child(&zoom_in_btn);
    let zoom_reset_btn = make_btn(doc, ZOOM_RESET_ID, "Reset zoom to 100%", "\u{21BA}")?; // U+21BA
    let _ = controls.append_child(&zoom_reset_btn);
    // Issue 1821: actual-size (1:1) toggle.
    let zoom_actual_btn = make_btn(doc, ZOOM_ACTUAL_ID, "Actual size (1:1 pixels)", "1:1")?;
    let _ = zoom_actual_btn.set_attribute("aria-pressed", "false");
    let _ = controls.append_child(&zoom_actual_btn);
    let _ = bar.append_child(&controls);

    let spec = maximize_button_spec();
    let maximize = make_btn(doc, MAXIMIZE_BTN_ID, spec.label, spec.glyph)?;
    let _ = maximize.set_attribute("aria-pressed", "false");
    let _ = maximize.set_attribute("class", "ss-detached-zoom-btn ss-detached-maximize");
    let _ = bar.append_child(&maximize);

    let reattach = doc.create_element("button").ok()?;
    let _ = reattach.set_attribute("type", "button");
    let _ = reattach.set_attribute("id", REATTACH_BTN_ID);
    let _ = reattach.set_attribute("class", "ss-detached-reattach");
    let _ = reattach.set_attribute("aria-describedby", REATTACH_DESC_ID);
    let _ = reattach.set_attribute("aria-keyshortcuts", "Escape");
    reattach.set_text_content(Some("Reattach"));
    let _ = bar.append_child(&reattach);
    let desc = doc.create_element("span").ok()?;
    let _ = desc.set_attribute("id", REATTACH_DESC_ID);
    let _ = desc.set_attribute("class", "ss-visually-hidden");
    desc.set_text_content(Some("Return shared content to the meeting window"));
    let _ = bar.append_child(&desc);
    let _ = body.append_child(&bar);

    // --- zoom viewport > wrapper > video ---
    let viewport = doc.create_element("div").ok()?;
    let _ = viewport.set_attribute("id", VIEWPORT_ID);
    let _ = viewport.set_attribute("class", "ss-detached-viewport");
    let _ = viewport.set_attribute("tabindex", "0");
    let _ = viewport.set_attribute("role", "group");
    let _ = viewport.set_attribute(
        "aria-label",
        "Shared content. Drag or use arrow keys to pan when zoomed.",
    );

    let wrapper = doc.create_element("div").ok()?;
    let _ = wrapper.set_attribute("id", WRAPPER_ID);
    let _ = wrapper.set_attribute("class", "ss-detached-wrapper");

    // `cross_realm_cast` (not `dyn_into`) because this element belongs to the
    // detached document's realm, where an instanceof check against the main
    // realm's `HTMLVideoElement` is `false` (issue 1829: this cast failing was
    // what left the detached window blank and reverted the share).
    let video: HtmlVideoElement = cross_realm_cast(doc.create_element("video").ok()?);
    let _ = video.set_attribute("id", VIDEO_ID);
    let _ = video.set_attribute("class", "ss-detached-video");
    let _ = video.set_attribute("autoplay", "");
    let _ = video.set_attribute("playsinline", "");
    video.set_muted(true);
    let _ = wrapper.append_child(&video);
    let _ = viewport.append_child(&wrapper);

    // Issue 1821: stats overlay (res·fps), passive + decorative (aria-hidden),
    // seeded with em-dashes and updated by `wire_detached_metrics`.
    if show_metrics {
        if let Ok(metrics) = doc.create_element("div") {
            let _ = metrics.set_attribute("id", METRICS_ID);
            let _ = metrics.set_attribute("class", "ss-detached-metrics");
            let _ = metrics.set_attribute("aria-hidden", "true");
            metrics.set_text_content(Some("\u{2193} \u{2014} \u{b7} \u{2014}fps"));
            let _ = viewport.append_child(&metrics);
        }
    }
    let _ = body.append_child(&viewport);

    let waiting = doc.create_element("div").ok()?;
    let _ = waiting.set_attribute("id", WAITING_ID);
    let _ = waiting.set_attribute("class", "ss-detached-waiting");
    waiting.set_inner_html(WAITING_HTML);
    let _ = body.append_child(&waiting);
    let region = doc.create_element("div").ok()?;
    let _ = region.set_attribute("id", ANNOUNCE_ID);
    let _ = region.set_attribute("class", "ss-visually-hidden");
    let _ = region.set_attribute("role", "status");
    let _ = region.set_attribute("aria-live", "polite");
    let _ = body.append_child(&region);

    Some(video)
}

fn make_btn(doc: &Document, id: &str, label: &str, glyph: &str) -> Option<Element> {
    let b = doc.create_element("button").ok()?;
    let _ = b.set_attribute("type", "button");
    let _ = b.set_attribute("id", id);
    let _ = b.set_attribute("class", "ss-detached-zoom-btn");
    let _ = b.set_attribute("aria-label", label);
    let _ = b.set_attribute("title", label);
    b.set_text_content(Some(glyph));
    Some(b)
}

// ---------------------------------------------------------------------------
// Issue 1821: detached actual-size (1:1), Maximize, wheel/pinch, stats overlay.
// ---------------------------------------------------------------------------

/// The render-clamped scale that shows the detached mirror at true 1:1, from the
/// mirror `<video>`'s decoded dims (`videoWidth`/`videoHeight`), the detached
/// viewport size, and the DETACHED window's device pixel ratio. `None` when the
/// video or viewport is not yet measurable (pre-decode `videoWidth == 0`,
/// zero-sized viewport) — the caller then does NOT engage 1:1, rather than
/// engaging at a bogus fit target. Called only on a 1:1 button ENGAGE click (a
/// discrete gesture), never on the per-frame pan/wheel/pinch apply path, so its
/// layout read is not a per-gesture reflow.
fn detached_actual_target(doc: &Document) -> Option<f64> {
    let video = doc.get_element_by_id(VIDEO_ID)?;
    // `cross_realm_cast` (not `dyn_into`): the video is a detached-realm element.
    let video: HtmlVideoElement = cross_realm_cast(video);
    let bw = video.video_width() as f64;
    let bh = video.video_height() as f64;
    if bw <= 0.0 || bh <= 0.0 {
        return None;
    }
    let (hw, hh) = detached_viewport_half(doc)?;
    let dpr = doc
        .default_view()
        .map(|w| w.device_pixel_ratio().max(1.0))
        .unwrap_or(1.0);
    Some(zoom::actual_size_target(bw, bh, hw * 2.0, hh * 2.0, dpr))
}

fn wire_maximize(doc: &Document, listeners: &mut Vec<ListenerHandle>) {
    let doc_click = doc.clone();
    let click_cb = Closure::<dyn FnMut()>::new(move || {
        if doc_click.fullscreen_element().is_some() {
            doc_click.exit_fullscreen();
        } else if let Some(root) = doc_click.document_element() {
            let _ = root.request_fullscreen();
        }
    });
    if let Some(btn) = doc.get_element_by_id(MAXIMIZE_BTN_ID) {
        let _ = btn.add_event_listener_with_callback("click", click_cb.as_ref().unchecked_ref());
    }
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Plain(click_cb),
    });

    let doc_fs = doc.clone();
    let fs_cb = Closure::<dyn FnMut()>::new(move || {
        let is_fs = doc_fs.fullscreen_element().is_some();
        if let Some(btn) = doc_fs.get_element_by_id(MAXIMIZE_BTN_ID) {
            let _ = btn.set_attribute("aria-pressed", if is_fs { "true" } else { "false" });
            let label = if is_fs {
                "Exit full screen (Escape)"
            } else {
                maximize_button_spec().label
            };
            let _ = btn.set_attribute("aria-label", label);
            let _ = btn.set_attribute("title", label);
        }
    });
    let _ =
        doc.add_event_listener_with_callback("fullscreenchange", fs_cb.as_ref().unchecked_ref());
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Plain(fs_cb),
    });
}

/// Wire the detached stats overlay: resolution from the video `resize` event
/// (fires when decoded dims change) and fps from a ~1 Hz
/// `getVideoPlaybackQuality().totalVideoFrames` delta (NO per-frame JS). Returns
/// the fps interval id so teardown can clear it. Reuses the SAME pure
/// `format_screen_metrics_line` as the in-window tile overlay.
fn wire_detached_metrics(
    doc: &Document,
    win: &Window,
    video: &HtmlVideoElement,
    listeners: &mut Vec<ListenerHandle>,
) -> Option<i32> {
    // Shared (resolution, fps) state feeding one render fn.
    let stats: Rc<RefCell<(Option<(u32, u32)>, Option<f64>)>> = Rc::new(RefCell::new((None, None)));
    let render: Rc<dyn Fn()> = {
        let doc_r = doc.clone();
        let stats_r = stats.clone();
        Rc::new(move || {
            let (res, fps) = *stats_r.borrow();
            if let Some(el) = doc_r.get_element_by_id(METRICS_ID) {
                el.set_text_content(Some(
                    &super::media_metrics_overlay::format_screen_metrics_line(res, fps),
                ));
            }
        })
    };

    // Resolution on `resize` (seed once now — metadata may already be present).
    let seed_res = |video: &HtmlVideoElement,
                    stats: &Rc<RefCell<(Option<(u32, u32)>, Option<f64>)>>| {
        let w = video.video_width();
        let h = video.video_height();
        stats.borrow_mut().0 = (w > 0 && h > 0).then_some((w, h));
    };
    seed_res(video, &stats);
    let resize_cb = {
        let video_c = video.clone();
        let stats_c = stats.clone();
        let render_c = render.clone();
        Closure::<dyn FnMut()>::new(move || {
            let w = video_c.video_width();
            let h = video_c.video_height();
            stats_c.borrow_mut().0 = (w > 0 && h > 0).then_some((w, h));
            render_c();
        })
    };
    let _ = video.add_event_listener_with_callback("resize", resize_cb.as_ref().unchecked_ref());
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Plain(resize_cb),
    });

    // fps: totalVideoFrames delta at a 1 s cadence ≈ frames/second. Seed the
    // baseline now so the FIRST interval tick already yields a 1 s delta.
    let last_frames = Rc::new(Cell::new(total_video_frames(video)));
    let interval_cb = {
        let video_i = video.clone();
        let stats_i = stats.clone();
        let render_i = render.clone();
        let last = last_frames.clone();
        Closure::<dyn FnMut()>::new(move || {
            if video_i.src_object().is_none() {
                return;
            }
            if let Some(total) = total_video_frames(&video_i) {
                if let Some(prev) = last.get() {
                    stats_i.borrow_mut().1 = Some((total - prev).max(0.0));
                    render_i();
                }
                last.set(Some(total));
            }
        })
    };
    let id = win
        .set_interval_with_callback_and_timeout_and_arguments_0(
            interval_cb.as_ref().unchecked_ref(),
            1000,
        )
        .ok();
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Plain(interval_cb),
    });
    render();
    id
}

/// Read `getVideoPlaybackQuality().totalVideoFrames` for the (detached-realm)
/// mirror video, or `None` if the API is unavailable. Uses `Reflect` (not the
/// typed web-sys shim) so the call + property read both dispatch dynamically on
/// the detached-realm object without any `instanceof` gate.
fn total_video_frames(video: &HtmlVideoElement) -> Option<f64> {
    let f = js_sys::Reflect::get(video, &JsValue::from_str("getVideoPlaybackQuality"))
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    let q = f.call0(video).ok()?;
    js_sys::Reflect::get(&q, &JsValue::from_str("totalVideoFrames"))
        .ok()?
        .as_f64()
}

// ---------------------------------------------------------------------------
// Detached-window zoom / pan (imperative, reusing the pure math).
// ---------------------------------------------------------------------------

/// Apply the current zoom state to the wrapper transform + refresh the label /
/// disabled state, resolved within the detached document `doc`. `actual_engaged`
/// is the 1:1 INTENT (mirrors the in-window tile's `ScreenActualSizeCtx`): the
/// aria-pressed state is driven straight from it, NOT by recomputing the 1:1
/// target here — recomputing would do a `client_width`/`client_height` LAYOUT
/// READ right after the transform WRITE (a forced reflow) on every pan/wheel
/// frame, and during a pan the scale is constant so the pressed state can't
/// change anyway. Every explicit zoom (button/wheel/pinch) clears the intent;
/// pan preserves it.
fn apply_detached_zoom(doc: &Document, state: &ScreenZoomState, actual_engaged: bool) {
    // `cross_realm_cast` (not `dyn_into`) because the wrapper lives in the
    // detached document's realm (issue 1829); a `dyn_into::<HtmlElement>` here
    // returns `None` cross-realm, so zoom/pan would silently never apply.
    if let Some(wrapper) = doc.get_element_by_id(WRAPPER_ID) {
        let wrapper: HtmlElement = cross_realm_cast(wrapper);
        let _ = wrapper
            .style()
            .set_property("transform", &zoom::transform_css(state));
    }
    if let Some(label) = doc.get_element_by_id(ZOOM_LABEL_ID) {
        label.set_text_content(Some(&zoom::zoom_percent_label(state.scale)));
    }
    set_aria_disabled(doc, ZOOM_OUT_ID, zoom::at_min_zoom(state.scale));
    set_aria_disabled(doc, ZOOM_IN_ID, zoom::at_max_zoom(state.scale));
    // Issue 1821: 1:1 button pressed state = the engaged intent (no layout read).
    if let Some(btn) = doc.get_element_by_id(ZOOM_ACTUAL_ID) {
        let _ = btn.set_attribute(
            "aria-pressed",
            if actual_engaged { "true" } else { "false" },
        );
    }
    if let Some(vp) = doc.get_element_by_id(VIEWPORT_ID) {
        if zoom::is_zoomed(state.scale) {
            let _ = vp.set_attribute("data-zoomed", "true");
        } else {
            let _ = vp.remove_attribute("data-zoomed");
        }
    }
}

fn set_aria_disabled(doc: &Document, id: &str, disabled: bool) {
    if let Some(el) = doc.get_element_by_id(id) {
        let _ = el.set_attribute("aria-disabled", if disabled { "true" } else { "false" });
    }
}

/// Half the detached viewport's client width/height, for pan clamping.
fn detached_viewport_half(doc: &Document) -> Option<(f64, f64)> {
    let el = doc.get_element_by_id(VIEWPORT_ID)?;
    let w = el.client_width() as f64;
    let h = el.client_height() as f64;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((w / 2.0, h / 2.0))
}

enum ZoomBtn {
    In,
    Out,
    Reset,
}

#[derive(Default)]
struct DetachedDrag {
    active: bool,
    last: Option<(f64, f64)>,
    /// Viewport half-dims cached ONCE on pointerdown (the viewport doesn't
    /// resize mid-drag), so a fast drag does no per-move layout read.
    half: Option<(f64, f64)>,
    /// Accumulated pointer delta awaiting the next rAF flush.
    pending_dx: f64,
    pending_dy: f64,
    /// A single rAF flush is pending — coalesces many moves into one transform
    /// write per frame (no read-write layout thrash at raw input rate).
    raf_scheduled: bool,
    /// Issue 1821: currently-down pointers `(pointer_id, client_x, client_y)`.
    /// Two entries → pinch mode.
    pointers: Vec<(i32, f64, f64)>,
    /// True once two pointers are down: drag is suspended and the pinch span
    /// drives zoom.
    pinching: bool,
    /// Distance between the two pointers at the previous pinch move.
    prev_dist: f64,
    /// Viewport geometry `(left, top, half_w, half_h)` cached on pinch start, so
    /// the anchor midpoint can be made viewport-local without a per-move layout
    /// read.
    pinch_geom: Option<(f64, f64, f64, f64)>,
    /// Pinch-computed next state awaiting the rAF flush (coalesced; also the BASE
    /// for the next move so the incremental ratio chain compounds).
    pending_zoom: Option<ScreenZoomState>,
}

/// Viewport geometry `(left, top, half_w, half_h)` in the detached document, for
/// making a pinch midpoint viewport-local. `None` when unmeasurable.
fn detached_viewport_geom(doc: &Document) -> Option<(f64, f64, f64, f64)> {
    let el = doc.get_element_by_id(VIEWPORT_ID)?;
    let r = el.get_bounding_client_rect();
    let (w, h) = (r.width(), r.height());
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((r.left(), r.top(), w / 2.0, h / 2.0))
}

fn wire_zoom_controls(
    doc: &Document,
    zoom_state: &Rc<RefCell<ScreenZoomState>>,
    actual_engaged: &Rc<Cell<bool>>,
    listeners: &mut Vec<ListenerHandle>,
) {
    // Zoom buttons.
    for (id, kind) in [
        (ZOOM_IN_ID, ZoomBtn::In),
        (ZOOM_OUT_ID, ZoomBtn::Out),
        (ZOOM_RESET_ID, ZoomBtn::Reset),
    ] {
        let doc_cb = doc.clone();
        let state = zoom_state.clone();
        let actual = actual_engaged.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            let (hw, hh) = detached_viewport_half(&doc_cb).unwrap_or((0.0, 0.0));
            let cur = *state.borrow();
            let next_scale = match kind {
                ZoomBtn::In => zoom::zoom_in(cur.scale),
                ZoomBtn::Out => zoom::zoom_out(cur.scale),
                ZoomBtn::Reset => zoom::RESET_ZOOM,
            };
            let next = zoom::zoom_to(cur, next_scale, hw, hh);
            *state.borrow_mut() = next;
            actual.set(false); // an explicit zoom leaves 1:1
            apply_detached_zoom(&doc_cb, &next, false);
        });
        if let Some(btn) = doc.get_element_by_id(id) {
            let _ = btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        }
        listeners.push(ListenerHandle {
            _closure: ClosureKind::Plain(cb),
        });
    }

    // Issue 1821: actual-size (1:1) toggle. Engaged (intent) → back to fit; else
    // engage 1:1 (render ceiling lets `zoom_to` exceed 4.0), computing the target
    // once here (a discrete click, not a per-frame path). Unmeasurable
    // (pre-decode) → no engage. Center-anchored, like the buttons.
    {
        let doc_cb = doc.clone();
        let state = zoom_state.clone();
        let actual = actual_engaged.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            let (hw, hh) = detached_viewport_half(&doc_cb).unwrap_or((0.0, 0.0));
            let cur = *state.borrow();
            if actual.get() {
                // Disengage → fit.
                let next = zoom::zoom_to(cur, zoom::RESET_ZOOM, hw, hh);
                *state.borrow_mut() = next;
                actual.set(false);
                apply_detached_zoom(&doc_cb, &next, false);
            } else if let Some(target) = detached_actual_target(&doc_cb) {
                let next = zoom::zoom_to(cur, target, hw, hh);
                *state.borrow_mut() = next;
                actual.set(true);
                apply_detached_zoom(&doc_cb, &next, true);
            }
            // else: video not yet measurable — leave the state untouched.
        });
        if let Some(btn) = doc.get_element_by_id(ZOOM_ACTUAL_ID) {
            let _ = btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        }
        listeners.push(ListenerHandle {
            _closure: ClosureKind::Plain(cb),
        });
    }

    // Issue 1821: Ctrl+wheel / trackpad-pinch zoom on the viewport, NON-PASSIVE so
    // `preventDefault()` suppresses the browser page zoom (same rationale as the
    // in-window tile; the detached document has no Dioxus passive-wheel issue but
    // still needs preventDefault, so it is attached imperatively here too).
    {
        let doc_cb = doc.clone();
        let state = zoom_state.clone();
        let actual = actual_engaged.clone();
        let wheel_cb = Closure::<dyn FnMut(WheelEvent)>::new(move |e: WheelEvent| {
            if !(e.ctrl_key() || e.meta_key()) {
                return;
            }
            e.prevent_default();
            let Some((left, top, hw, hh)) = detached_viewport_geom(&doc_cb) else {
                return;
            };
            let px = e.client_x() as f64 - left;
            let py = e.client_y() as f64 - top;
            let cur = *state.borrow();
            let factor = zoom::wheel_zoom_factor(e.delta_y(), e.delta_mode(), hh * 2.0);
            let next = zoom::zoom_to_anchored(cur, cur.scale * factor, px, py, hw, hh);
            *state.borrow_mut() = next;
            actual.set(false); // an explicit zoom leaves 1:1
            apply_detached_zoom(&doc_cb, &next, false);
        });
        if let Some(vp) = doc.get_element_by_id(VIEWPORT_ID) {
            let opts = AddEventListenerOptions::new();
            opts.set_passive(false);
            let _ = vp.add_event_listener_with_callback_and_add_event_listener_options(
                "wheel",
                wheel_cb.as_ref().unchecked_ref(),
                &opts,
            );
        }
        listeners.push(ListenerHandle {
            _closure: ClosureKind::Wheel(wheel_cb),
        });
    }

    // Keyboard pan on the viewport.
    let doc_key = doc.clone();
    let state_key = zoom_state.clone();
    let actual_key = actual_engaged.clone();
    let key_cb = Closure::<dyn FnMut(KeyboardEvent)>::new(move |e: KeyboardEvent| {
        let cur = *state_key.borrow();
        if !zoom::is_zoomed(cur.scale) {
            return;
        }
        let (hw, hh) = match detached_viewport_half(&doc_key) {
            Some(v) => v,
            None => return,
        };
        let next = match e.key().as_str() {
            "Home" => Some(ScreenZoomState {
                scale: cur.scale,
                off_x: zoom::max_pan_offset(cur.scale, hw),
                off_y: zoom::max_pan_offset(cur.scale, hh),
            }),
            "End" => Some(ScreenZoomState {
                scale: cur.scale,
                off_x: -zoom::max_pan_offset(cur.scale, hw),
                off_y: -zoom::max_pan_offset(cur.scale, hh),
            }),
            other => zoom::pan_key_delta(other).map(|(dx, dy)| zoom::pan_by(cur, dx, dy, hw, hh)),
        };
        if let Some(next) = next {
            e.prevent_default();
            *state_key.borrow_mut() = next;
            // Pan preserves the 1:1 intent (scale unchanged).
            apply_detached_zoom(&doc_key, &next, actual_key.get());
        }
    });
    if let Some(vp) = doc.get_element_by_id(VIEWPORT_ID) {
        let _ = vp.add_event_listener_with_callback("keydown", key_cb.as_ref().unchecked_ref());
    }
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Key(key_cb),
    });

    // Drag-to-pan on the viewport, coalesced to one transform write per animation
    // frame with the viewport half-dims cached on pointerdown — no per-move
    // layout read/write thrash (mirrors the main-window drag pattern).
    let drag = Rc::new(RefCell::new(DetachedDrag::default()));

    // The rAF flush: applies the accumulated delta once per frame. Parked in
    // `listeners` (kept alive); `move_cb` holds a JS `Function` handle to it. The
    // flush is scheduled on the DETACHED window (visible during drag), so if the
    // window closes the pending frame is cancelled with it — no dropped-closure
    // call.
    let raf_flush = {
        let doc_f = doc.clone();
        let state_f = zoom_state.clone();
        let drag_f = drag.clone();
        let actual_f = actual_engaged.clone();
        Closure::<dyn FnMut()>::new(move || {
            let (dx, dy, half, pending_zoom) = {
                let mut d = drag_f.borrow_mut();
                d.raf_scheduled = false;
                let v = (d.pending_dx, d.pending_dy, d.half, d.pending_zoom.take());
                d.pending_dx = 0.0;
                d.pending_dy = 0.0;
                v
            };
            // Issue 1821: a pending pinch state (anchored + clamped) takes
            // precedence; pinch suspends the pan accumulator so both never apply
            // in one frame. A pinch is an explicit zoom → leaves 1:1.
            if let Some(next) = pending_zoom {
                *state_f.borrow_mut() = next;
                actual_f.set(false);
                apply_detached_zoom(&doc_f, &next, false);
                return;
            }
            if dx == 0.0 && dy == 0.0 {
                return;
            }
            let (hw, hh) = match half {
                Some(v) => v,
                None => return,
            };
            let cur = *state_f.borrow();
            let next = zoom::pan_by(cur, dx, dy, hw, hh);
            *state_f.borrow_mut() = next;
            // Pan preserves the 1:1 intent (scale unchanged).
            apply_detached_zoom(&doc_f, &next, actual_f.get());
        })
    };
    let raf_fn: js_sys::Function = raf_flush
        .as_ref()
        .unchecked_ref::<js_sys::Function>()
        .clone();

    let doc_down = doc.clone();
    let state_down = zoom_state.clone();
    let drag_down = drag.clone();
    let down_cb = Closure::<dyn FnMut(PointerEvent)>::new(move |e: PointerEvent| {
        let is_zoomed = zoom::is_zoomed(state_down.borrow().scale);
        let cx = e.client_x() as f64;
        let cy = e.client_y() as f64;
        let pid = e.pointer_id();
        if let Some(vp) = doc_down.get_element_by_id(VIEWPORT_ID) {
            let _ = vp.set_pointer_capture(pid);
        }
        let geom = detached_viewport_geom(&doc_down);
        let half = detached_viewport_half(&doc_down);
        let mut d = drag_down.borrow_mut();
        d.pointers.retain(|(id, _, _)| *id != pid);
        d.pointers.push((pid, cx, cy));
        if d.pointers.len() >= 2 {
            // Pinch: allowed from FIT (pinch-out to zoom in), so NOT gated on
            // `is_zoomed`. Cache the viewport geometry for the anchor midpoint.
            let (_, x0, y0) = d.pointers[0];
            let (_, x1, y1) = d.pointers[1];
            d.pinching = true;
            d.active = false;
            d.prev_dist = zoom::pointer_distance(x0, y0, x1, y1);
            d.pinch_geom = geom;
            d.pending_zoom = None;
        } else if is_zoomed {
            // Single pointer while zoomed → drag pan (no-op at fit).
            d.active = true;
            d.last = Some((cx, cy));
            d.half = half;
        }
    });
    let win_move = doc.default_view();
    let drag_move = drag.clone();
    let state_move = zoom_state.clone();
    let move_cb = Closure::<dyn FnMut(PointerEvent)>::new(move |e: PointerEvent| {
        let cx = e.client_x() as f64;
        let cy = e.client_y() as f64;
        let pid = e.pointer_id();
        let schedule = {
            let mut d = drag_move.borrow_mut();
            if let Some(p) = d.pointers.iter_mut().find(|(id, _, _)| *id == pid) {
                p.1 = cx;
                p.2 = cy;
            }
            if d.pinching && d.pointers.len() >= 2 {
                let (_, x0, y0) = d.pointers[0];
                let (_, x1, y1) = d.pointers[1];
                let new_dist = zoom::pointer_distance(x0, y0, x1, y1);
                if new_dist > 0.0 && d.prev_dist > 0.0 {
                    if let Some((left, top, hw, hh)) = d.pinch_geom {
                        let (mx, my) = zoom::pointer_midpoint(x0, y0, x1, y1);
                        let base = d.pending_zoom.unwrap_or_else(|| *state_move.borrow());
                        let ratio = new_dist / d.prev_dist;
                        let next = zoom::zoom_to_anchored(
                            base,
                            base.scale * ratio,
                            mx - left,
                            my - top,
                            hw,
                            hh,
                        );
                        d.pending_zoom = Some(next);
                    }
                }
                d.prev_dist = new_dist;
                if d.raf_scheduled {
                    false
                } else {
                    d.raf_scheduled = true;
                    true
                }
            } else if d.active {
                let (lx, ly) = d.last.unwrap_or((cx, cy));
                d.pending_dx += cx - lx;
                d.pending_dy += cy - ly;
                d.last = Some((cx, cy));
                if d.raf_scheduled {
                    false
                } else {
                    d.raf_scheduled = true;
                    true
                }
            } else {
                false
            }
        };
        if schedule {
            if let Some(win) = win_move.as_ref() {
                let _ = win.request_animation_frame(&raf_fn);
            }
        }
    });
    let drag_up = drag.clone();
    let state_up = zoom_state.clone();
    let doc_up = doc.clone();
    let up_cb = Closure::<dyn FnMut(PointerEvent)>::new(move |e: PointerEvent| {
        let pid = e.pointer_id();
        let mut d = drag_up.borrow_mut();
        d.pointers.retain(|(id, _, _)| *id != pid);
        if d.pointers.len() < 2 {
            // Exit pinch; the pending pinch state is left for the scheduled rAF to
            // flush (final state). A single remaining pointer resumes drag when
            // zoomed.
            d.pinching = false;
            d.prev_dist = 0.0;
            d.pinch_geom = None;
            match d.pointers.first().copied() {
                Some((_, x, y)) if zoom::is_zoomed(state_up.borrow().scale) => {
                    d.active = true;
                    d.last = Some((x, y));
                    d.half = detached_viewport_half(&doc_up);
                }
                _ => {
                    d.active = false;
                    d.last = None;
                }
            }
        }
    });
    if let Some(vp) = doc.get_element_by_id(VIEWPORT_ID) {
        let _ =
            vp.add_event_listener_with_callback("pointerdown", down_cb.as_ref().unchecked_ref());
        let _ =
            vp.add_event_listener_with_callback("pointermove", move_cb.as_ref().unchecked_ref());
        let _ = vp.add_event_listener_with_callback("pointerup", up_cb.as_ref().unchecked_ref());
        let _ =
            vp.add_event_listener_with_callback("pointercancel", up_cb.as_ref().unchecked_ref());
    }
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Plain(raf_flush),
    });
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Pointer(down_cb),
    });
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Pointer(move_cb),
    });
    listeners.push(ListenerHandle {
        _closure: ClosureKind::Pointer(up_cb),
    });

    // Initial paint of the label / disabled state. Nothing is engaged at open, so
    // the 1:1 button paints aria-pressed="false" (fixes the prior false "true" at
    // open when the pre-decode target defaulted to fit and matched scale 1.0).
    apply_detached_zoom(doc, &zoom_state.borrow(), actual_engaged.get());
}

// ---------------------------------------------------------------------------
// Teardown.
// ---------------------------------------------------------------------------

/// Tear down the detached window for `peer`: close the window, stop the mirror,
/// clear the poll, drop the state (detaching all listeners), and invoke the
/// reattach callback exactly once. Idempotent; `win.close()` is a no-op on an
/// already-closed window.
pub fn teardown(peer: &str) {
    let state = DETACH.with(|d| {
        let matches = d.borrow().as_ref().map(|s| s.peer == peer).unwrap_or(false);
        if matches {
            d.borrow_mut().take()
        } else {
            None
        }
    });
    let Some(mut state) = state else {
        return;
    };
    listen_for_page_leave(false);
    if let (Some(id), Some(main)) = (state.close_poll_id.take(), web_sys::window()) {
        main.clear_interval_with_handle(id);
    }
    // Issue 1821: clear the detached stats-overlay fps sampler interval.
    if let Some(id) = state.metrics_poll_id.take() {
        clear_popup_interval(&state.win, id);
    }
    let _ = state.win.close();
    state.stop_mirror();
    if state.follows {
        WANT.with(|w| w.borrow_mut().take());
    }
    (state.on_reattach)();
    // `state` (with its listeners + zoom state) drops here.
}

pub fn reattach(peer: &str) {
    teardown(peer);
}

/// Self-contained stylesheet for the detached window (authored here, not cloned).
const DETACHED_CSS: &str = "\
html,body{margin:0;height:100%;background:#0b0d10;color:#e8eaed;\
font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;}\
.ss-detached-body{display:flex;flex-direction:column;height:100%;overflow:hidden;}\
/* .ss-detached-bar height (40px, border-box) is LOAD-BEARING: detached_window_inner_dims \
adds it as BAR_H when sizing the window to content aspect (issue #1842). Keep in sync with \
DETACHED_BAR_H_PX; do NOT revert it to content-driven. */\
/* @token-exempt: detached popup is a separate document; app :root tokens unavailable */\
.ss-detached-bar{box-sizing:border-box;height:40px;display:flex;align-items:center;\
gap:12px;padding:6px 10px;background:rgba(255,255,255,0.06);flex:0 0 auto;}\
.ss-detached-name{font-size:13px;font-weight:600;white-space:nowrap;\
overflow:hidden;text-overflow:ellipsis;flex:1 1 auto;min-width:0;}\
.ss-detached-controls{display:flex;align-items:center;gap:2px;flex:0 0 auto;}\
.ss-detached-zoom-btn{appearance:none;border:none;background:transparent;\
color:#e8eaed;width:28px;height:28px;font-size:18px;line-height:1;border-radius:6px;\
cursor:pointer;display:inline-flex;align-items:center;justify-content:center;}\
.ss-detached-zoom-btn:hover{background:rgba(255,255,255,0.16);}\
.ss-detached-zoom-btn:focus-visible{outline:2px solid #4c8bf5;outline-offset:1px;}\
.ss-detached-zoom-btn[aria-disabled=\"true\"]{opacity:0.4;cursor:default;}\
/* issue 1821: engaged (pressed) highlight for the 1:1 + Maximize toggles, \
matching the in-window tile's .ss-actual-btn[aria-pressed]. @token-exempt: \
detached popup is a separate document; app :root tokens unavailable. */\
.ss-detached-zoom-btn[aria-pressed=\"true\"]{background:rgba(76,139,245,0.35);}\
.ss-detached-zoom-label{min-width:44px;text-align:center;font-size:12px;\
font-variant-numeric:tabular-nums;user-select:none;}\
.ss-detached-reattach{appearance:none;border:1px solid rgba(255,255,255,0.25);\
background:rgba(255,255,255,0.08);color:inherit;font:inherit;font-size:12px;\
padding:5px 12px;border-radius:6px;cursor:pointer;flex:0 0 auto;}\
.ss-detached-reattach:hover{background:rgba(255,255,255,0.16);}\
.ss-detached-reattach:focus-visible{outline:2px solid #4c8bf5;outline-offset:2px;}\
.ss-detached-viewport{position:relative;flex:1 1 auto;min-height:0;overflow:hidden;\
background:#000;touch-action:none;}\
.ss-detached-viewport[data-zoomed]{cursor:grab;}\
.ss-detached-viewport[data-zoomed]:active{cursor:grabbing;}\
.ss-detached-viewport:focus-visible{outline:2px solid #4c8bf5;outline-offset:-2px;}\
.ss-detached-wrapper{position:absolute;inset:0;transform-origin:center center;}\
.ss-detached-video{width:100%;height:100%;object-fit:contain;background:#000;\
display:block;}\
/* issue 3173: waiting state. @token-exempt: detached popup is a separate document; \
app :root tokens unavailable. */\
:root{color-scheme:dark;}\
.ss-detached-body[data-ss-state=\"waiting\"] .ss-detached-controls,\
.ss-detached-body[data-ss-state=\"waiting\"] .ss-detached-maximize,\
.ss-detached-body[data-ss-state=\"waiting\"] .ss-detached-viewport{display:none;}\
.ss-detached-body[data-ss-state=\"live\"] .ss-detached-waiting{display:none;}\
.ss-detached-body[data-ss-state=\"waiting\"] .ss-detached-name{color:rgba(232,234,237,0.7);\
font-weight:500;}\
.ss-detached-waiting{flex:1 1 auto;min-height:0;margin:16px;box-sizing:border-box;\
display:flex;flex-direction:column;align-items:center;justify-content:safe center;gap:12px;\
overflow:auto;padding:24px;text-align:center;border:1.5px dashed rgba(255,255,255,0.18);border-radius:12px;}\
.ss-detached-waiting-icon{width:48px;height:48px;color:rgba(232,234,237,0.5);flex:0 0 auto;}\
.ss-detached-waiting-title{margin:0;font-size:16px;font-weight:600;line-height:1.35;color:#e8eaed;}\
.ss-detached-waiting-hint{margin:0;max-width:36ch;font-size:13px;line-height:1.45;\
color:rgba(232,234,237,0.7);}\
.ss-visually-hidden{position:absolute;width:1px;height:1px;margin:-1px;padding:0;overflow:hidden;\
clip:rect(0 0 0 0);white-space:nowrap;border:0;}\
@media (max-height:260px){.ss-detached-waiting-icon{display:none;}\
.ss-detached-waiting{margin:8px;padding:12px;}}\
@media (max-height:180px){.ss-detached-waiting-hint{display:none;}}\
/* issue 1821: passive stats overlay, bottom-left of the viewport. @token-exempt: \
detached popup is a separate document; app :root tokens unavailable. */\
.ss-detached-metrics{position:absolute;left:8px;bottom:8px;z-index:3;\
pointer-events:none;font:11px ui-monospace,Menlo,Consolas,monospace;\
font-variant-numeric:tabular-nums;color:#e8eaed;background:rgba(0,0,0,0.55);\
padding:2px 6px;border-radius:4px;}\
/* transient toast, bottom-center. @token-exempt: \
detached popup is a separate document; app :root tokens unavailable. */\
.ss-detached-toast{position:fixed;left:50%;bottom:16px;transform:translateX(-50%);\
z-index:10;max-width:90%;box-sizing:border-box;background:rgba(20,22,26,0.95);\
color:#e8eaed;border:1px solid rgba(255,255,255,0.18);border-radius:8px;\
padding:8px 14px;font-size:13px;line-height:1.3;text-align:center;\
box-shadow:0 4px 16px rgba(0,0,0,0.4);pointer-events:none;}";
