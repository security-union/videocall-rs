// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

// Issue 2792: the shared-content tile's view modes, mounted for real (the own
// share tile, its control bar, the dispatcher, the detach glue and the
// announcer) plus the stylesheet rules that place each mode.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use dioxus::prelude::*;
use dioxus_ui::components::canvas_generator::{
    OwnShareTile, PinnedTile, ScreenDetachAnnouncer, TileMode,
};
use dioxus_ui::components::decode_budget::TileRenderMode;
use dioxus_ui::components::peer_tile::{camera_tiles, CameraTiles, PeerTile};
use dioxus_ui::components::pin_order;
use dioxus_ui::components::screen_share_detach as ssd;
use dioxus_ui::components::share_view::{
    self, effective_mode, CtaState, ShareAction, ShareBase, ShareOrigin, ShareSlot, ShareSlots,
    ShareTarget, ShareTileView, ShareTracker, ShareViewCtx, TeardownCause, CTA_HINT, DETACH_FAILED,
    OWN_SHARE_KEY,
};
use dioxus_ui::context::{
    AppearanceSettings, AppearanceSettingsCtx, DetachedShareCtx, MeetingTime, PeerAudioLivenessMap,
    PeerSignalHistoryMap, ScreenActualSizeCtx, ScreenZoomCtx, ScreenZoomState, SignalPopupStateMap,
};
use support::{create_mount_point, inject_app_config, render_into, yield_now};
use videocall_client::VideoCallClient;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const PREF_KEY: &str = "vc_own_share_view_mode";
const TILE: &str = "[data-testid='own-share-tile']";
const DETACH_BTN: &str = "own-screen-share-detach-btn";

thread_local! {
    static SEED_GUARD: Cell<bool> = const { Cell::new(false) };
    static SEED_CTA: Cell<CtaState> = const { Cell::new(CtaState::Hidden) };
    static SEED_RECEIVED_ENLARGED: Cell<bool> = const { Cell::new(false) };
    static SEED_PINS: RefCell<Vec<PinnedTile>> = const { RefCell::new(Vec::new()) };
    static SEED_BOX: Cell<Option<u32>> = const { Cell::new(None) };
    static STREAM: RefCell<Option<web_sys::MediaStream>> = const { RefCell::new(None) };
}

fn storage() -> web_sys::Storage {
    web_sys::window().unwrap().local_storage().unwrap().unwrap()
}

fn js(src: &str) -> JsValue {
    js_sys::Function::new_no_args(src)
        .call0(&JsValue::NULL)
        .unwrap()
}

struct Page {
    mount: web_sys::Element,
}

/// Clears what an earlier test left in the page.
fn reset_page() {
    let marked = gloo_utils::document()
        .query_selector_all("[data-share-tile-test]")
        .unwrap();
    for i in 0..marked.length() {
        if let Some(el) = marked
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
        {
            el.remove();
        }
    }
    ssd::close_all();
    js("if (window.__pipResolve) window.__pipResolve(null); \
        delete window.documentPictureInPicture; delete window.__pipResolve;");
    let _ = storage().remove_item(PREF_KEY);
    let _ = storage().remove_item(ShareOrigin::Received.pref_key());
    SEED_GUARD.with(|g| g.set(false));
    SEED_CTA.with(|c| c.set(CtaState::Hidden));
    SEED_RECEIVED_ENLARGED.with(|r| r.set(false));
    SEED_PINS.with(|p| p.borrow_mut().clear());
    SEED_BOX.with(|b| b.set(None));
    STREAM.with(|s| *s.borrow_mut() = None);
}

async fn fresh() {
    reset_page();
    yield_now().await;
}

fn mark(el: &web_sys::Element) {
    el.set_attribute("data-share-tile-test", "").unwrap();
}

fn marked_mount() -> web_sys::Element {
    let mount = create_mount_point();
    mark(&mount);
    mount
}

/// A Document PiP whose `requestWindow` resolves only when the test says so.
fn install_pip_stub() {
    js(
        "Object.defineProperty(window, 'documentPictureInPicture', { configurable: true, \
         value: { requestWindow: () => new Promise(r => { window.__pipResolve = r; }) } });",
    );
}

/// Resolves the pending `requestWindow` with a same-origin frame's window and
/// returns that frame's document.
fn resolve_pip_with_frame() -> web_sys::Document {
    let frame = gloo_utils::document().create_element("iframe").unwrap();
    mark(&frame);
    gloo_utils::document()
        .body()
        .unwrap()
        .append_child(&frame)
        .unwrap();
    let resolve: js_sys::Function =
        js_sys::Reflect::get(&gloo_utils::window(), &"__pipResolve".into())
            .unwrap()
            .unchecked_into();
    let frame_window = js_sys::Reflect::get(&frame, &"contentWindow".into()).unwrap();
    js_sys::Reflect::set(
        &frame_window,
        &"close".into(),
        &js("return function () { this.__closedByApp = true; };"),
    )
    .unwrap();
    resolve.call1(&JsValue::NULL, &frame_window).unwrap();
    js_sys::Reflect::get(&frame, &"contentDocument".into())
        .unwrap()
        .unchecked_into()
}

fn frame_closed(doc: &web_sys::Document) -> bool {
    let win = js_sys::Reflect::get(doc, &"defaultView".into()).unwrap();
    js_sys::Reflect::get(&win, &"__closedByApp".into())
        .unwrap()
        .is_truthy()
}

fn install_stylesheets(rewrite: &[(&str, &str)]) {
    let mut css = format!(
        "{}{}",
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    );
    for (from, to) in rewrite {
        css = css.replace(from, to);
    }
    let style = gloo_utils::document().create_element("style").unwrap();
    mark(&style);
    style.set_text_content(Some(&css));
    gloo_utils::document()
        .head()
        .unwrap()
        .append_child(&style)
        .unwrap();
}

#[allow(non_snake_case)]
fn Harness() -> Element {
    let mut zoom = use_signal(HashMap::new);
    let mut detached = use_signal(|| None::<String>);
    let actual = use_signal(|| None::<String>);
    let slots = use_signal(|| ShareSlots {
        own: ShareSlot {
            guard: SEED_GUARD.with(|g| g.get()),
            cta: SEED_CTA.with(|c| c.get()),
            ..ShareSlot::default()
        },
        received: ShareSlot {
            base: if SEED_RECEIVED_ENLARGED.with(|r| r.get()) {
                ShareBase::Enlarged
            } else {
                ShareBase::Tile
            },
            ..ShareSlot::default()
        },
    });
    let pins = use_signal(|| SEED_PINS.with(|p| p.borrow().clone()));
    let announce = use_signal(|| (String::new(), 0u32));
    let own_stream = use_signal(|| STREAM.with(|s| s.borrow().as_ref().map(Clone::clone)));
    let mut show = use_signal(|| true);
    let mut twin = use_signal(|| false);
    use_context_provider(|| ScreenZoomCtx(zoom));
    use_context_provider(|| DetachedShareCtx(detached));
    use_context_provider(|| ScreenActualSizeCtx(actual));
    use_context_provider(|| ShareViewCtx {
        slots,
        pins,
        detached,
        announce,
        own_stream,
    });
    let target = ShareTarget {
        origin: ShareOrigin::Own,
        key: OWN_SHARE_KEY.to_string(),
        pin: PinnedTile::own_screen("me"),
        name: ShareOrigin::Own.subject().to_string(),
    };
    let slot = slots.read().own;
    let mode = effective_mode(
        detached.read().as_deref() == Some(OWN_SHARE_KEY),
        pins.read().contains(&target.pin),
        slot.base,
    );
    let view = ShareTileView {
        pin_rank: pin_order::pin_rank(&pins.read(), &target.pin.user_id, target.pin.kind),
        target,
        mode,
        cta: slot.cta,
        guard: slot.guard,
    };
    let pin_probe = pin_list_probe(&pins.read());
    let zoom_probe = zoom
        .read()
        .get(OWN_SHARE_KEY)
        .map(|z| format!("{:.0}", z.off_x))
        .unwrap_or_default();
    let box_style = SEED_BOX
        .with(|b| b.get())
        .map(|w| format!("width: {w}px; height: {}px;", w * 2 / 3))
        .unwrap_or_default();
    rsx! {
        div { id: "grid-container", tabindex: "-1",
            div { style: "{box_style}",
                if twin() {
                    OwnShareTile { view: view.clone() }
                }
                if show() {
                    OwnShareTile { view }
                }
            }
            ScreenDetachAnnouncer {}
            button { id: "h-unmount", onclick: move |_| show.set(false) }
            button { id: "h-twin", onclick: move |_| twin.set(true) }
            button { id: "h-detach-on", onclick: move |_| detached.set(Some(OWN_SHARE_KEY.to_string())) }
            button { id: "h-detach-off", onclick: move |_| detached.set(None) }
            button {
                id: "h-seed-zoom",
                onclick: move |_| {
                    zoom.write()
                        .insert(
                            OWN_SHARE_KEY.to_string(),
                            ScreenZoomState {
                                scale: 2.0,
                                off_x: 5000.0,
                                off_y: -5000.0,
                            },
                        );
                },
            }
            span { id: "h-received-base", "{slots.read().received.base:?}" }
            span { id: "h-pin", "{pin_probe}" }
            span { id: "h-zoom", "{zoom_probe}" }
            span { id: "h-detached", "{detached.read().clone().unwrap_or_default()}" }
        }
    }
}

fn pin_list_probe(pins: &[PinnedTile]) -> String {
    pins.iter()
        .map(|p| format!("{:?}:{}", p.kind, p.user_id))
        .collect::<Vec<_>>()
        .join(",")
}

async fn mount_with(seed: impl FnOnce()) -> Page {
    fresh().await;
    seed();
    let mount = marked_mount();
    render_into(&mount, Harness);
    yield_now().await;
    Page { mount }
}

async fn mount() -> Page {
    mount_with(|| {}).await
}

fn el(page: &Page, selector: &str) -> web_sys::HtmlElement {
    page.mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
        .dyn_into()
        .unwrap()
}

fn attr(page: &Page, selector: &str, name: &str) -> Option<String> {
    el(page, selector).get_attribute(name)
}

fn text(page: &Page, selector: &str) -> String {
    el(page, selector).text_content().unwrap_or_default()
}

fn mode(page: &Page) -> String {
    attr(page, TILE, "data-share-mode").unwrap_or_default()
}

fn active_id() -> String {
    gloo_utils::document()
        .active_element()
        .map(|e| e.id())
        .unwrap_or_default()
}

fn announced(page: &Page) -> String {
    text(page, "[data-testid='ss-detach-announce']")
        .trim_end_matches('\u{00A0}')
        .to_string()
}

async fn settle() {
    yield_now().await;
    gloo_timers::future::TimeoutFuture::new(20).await;
}

#[wasm_bindgen_test]
async fn the_own_tile_is_a_region_with_the_view_controls_and_no_one_to_one() {
    let page = mount().await;

    assert_eq!(attr(&page, TILE, "role").as_deref(), Some("region"));
    assert_eq!(
        attr(&page, TILE, "aria-label").as_deref(),
        Some("Your shared content")
    );
    assert_eq!(
        attr(&page, TILE, "data-share-origin").as_deref(),
        Some("own")
    );
    assert_eq!(mode(&page), "tile");
    assert_eq!(
        attr(&page, "[data-testid='ss-zoom-controls']", "aria-label").as_deref(),
        Some("Your shared content controls")
    );
    for (testid, label) in [
        ("ss-enlarge", "Enlarge shared content"),
        ("ss-pin", "Pin shared content"),
    ] {
        let sel = format!("[data-testid='{testid}']");
        assert_eq!(attr(&page, &sel, "aria-label").as_deref(), Some(label));
        assert_eq!(attr(&page, &sel, "aria-pressed").as_deref(), Some("false"));
    }
    assert!(
        page.mount
            .query_selector("[data-testid='ss-zoom-actual']")
            .unwrap()
            .is_none(),
        "native pixels of your own screen add nothing"
    );
    assert!(page
        .mount
        .query_selector("#own-screen-share-video")
        .unwrap()
        .is_some());
    assert!(page
        .mount
        .query_selector("#screen-share-preview")
        .unwrap()
        .is_none());
}

#[wasm_bindgen_test]
async fn the_own_video_plays_the_capture_stream() {
    let stream = web_sys::MediaStream::new().unwrap();
    let seeded = Clone::clone(&stream);
    let page = mount_with(move || STREAM.with(|s| *s.borrow_mut() = Some(seeded))).await;
    yield_now().await;
    let video: web_sys::HtmlVideoElement = el(&page, "#own-screen-share-video").unchecked_into();
    let src = video.src_object().expect("srcObject is attached");
    assert!(
        js_sys::Object::is(src.as_ref(), stream.as_ref()),
        "the tile shows the stream ScreenShareEvent::Started carried"
    );
    assert!(video.muted());
}

fn live_canvas_stream() -> (web_sys::MediaStream, web_sys::CanvasRenderingContext2d) {
    let canvas: web_sys::HtmlCanvasElement = gloo_utils::document()
        .create_element("canvas")
        .unwrap()
        .unchecked_into();
    canvas.set_width(64);
    canvas.set_height(36);
    let paint: web_sys::CanvasRenderingContext2d =
        canvas.get_context("2d").unwrap().unwrap().unchecked_into();
    let stream = canvas.capture_stream_with_frame_request_rate(30.0).unwrap();
    (stream, paint)
}

fn assert_capture_live(stream: &web_sys::MediaStream, step: &str) {
    let tracks = stream.get_tracks();
    assert!(tracks.length() > 0, "premise: the capture has tracks");
    for track in tracks.iter() {
        let prop = |name: &str| js_sys::Reflect::get(&track, &name.into()).unwrap();
        assert_eq!(
            prop("readyState").as_string().as_deref(),
            Some("live"),
            "{step}: the UI must never stop the encoder's capture track"
        );
        assert_eq!(
            prop("enabled").as_bool(),
            Some(true),
            "{step}: the UI must never disable the encoder's capture track"
        );
    }
}

#[wasm_bindgen_test]
async fn the_own_preview_pauses_while_detached_and_releases_the_stream_on_unmount() {
    let (stream, paint) = live_canvas_stream();
    let seeded = Clone::clone(&stream);
    let page = mount_with(move || STREAM.with(|s| *s.borrow_mut() = Some(seeded))).await;
    let video: web_sys::HtmlVideoElement = el(&page, "#own-screen-share-video").unchecked_into();
    for i in 0..40 {
        paint.set_fill_style_str(if i % 2 == 0 { "#123456" } else { "#654321" });
        paint.fill_rect(0.0, 0.0, 64.0, 36.0);
        if !video.paused() {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(50).await;
    }
    assert!(!video.paused(), "premise: the attached preview plays");
    assert_capture_live(&stream, "attached");

    el(&page, "#h-detach-on").click();
    settle().await;
    assert!(
        video.paused(),
        "the detached window plays the stream; the off-screen copy must not"
    );
    assert_capture_live(&stream, "detached");

    el(&page, "#h-detach-off").click();
    settle().await;
    assert_capture_live(&stream, "reattached");

    el(&page, "[data-testid='ss-hide-preview']").click();
    settle().await;
    assert_eq!(attr(&page, TILE, "data-guard").as_deref(), Some("true"));
    assert_capture_live(&stream, "guard on");

    el(&page, "[data-testid='ss-show-preview']").click();
    settle().await;
    assert_capture_live(&stream, "guard off");

    el(&page, "#h-unmount").click();
    settle().await;
    assert!(
        video.src_object().is_none(),
        "an unmounted preview releases its sink"
    );
    assert_capture_live(&stream, "unmounted");
}

#[wasm_bindgen_test]
async fn unmounting_an_old_preview_leaves_the_new_one_its_stream() {
    let stream = web_sys::MediaStream::new().unwrap();
    let seeded = Clone::clone(&stream);
    let page = mount_with(move || STREAM.with(|s| *s.borrow_mut() = Some(seeded))).await;
    el(&page, "#h-twin").click();
    settle().await;
    el(&page, "#h-unmount").click();
    settle().await;
    let videos = page
        .mount
        .query_selector_all("#own-screen-share-video")
        .unwrap();
    assert_eq!(videos.length(), 1, "premise: only the new preview is left");
    let video: web_sys::HtmlVideoElement = videos.item(0).unwrap().unchecked_into();
    let src = video
        .src_object()
        .expect("the old preview's unmount must not clear the new one");
    assert!(js_sys::Object::is(src.as_ref(), stream.as_ref()));
}

#[wasm_bindgen_test]
async fn view_buttons_change_the_mode_on_the_same_nodes_and_escape_leaves_the_pin() {
    let page = mount().await;
    let root_before = el(&page, TILE);
    let video_before = el(&page, "#own-screen-share-video");
    let badge = || {
        page.mount
            .query_selector("[data-testid='tile-pin-badge']")
            .unwrap()
            .is_some()
    };

    el(&page, "[data-testid='ss-enlarge']").click();
    settle().await;
    assert_eq!(mode(&page), "enlarged");
    assert_eq!(
        attr(&page, "[data-testid='ss-enlarge']", "aria-pressed").as_deref(),
        Some("true")
    );
    assert_eq!(announced(&page), "Your shared content enlarged");
    assert_eq!(
        storage().get_item(PREF_KEY).unwrap().as_deref(),
        Some("enlarged")
    );
    assert_eq!(
        active_id(),
        "own-screen-share-enlarge-btn",
        "the pressed button keeps focus"
    );
    assert!(!badge());

    el(&page, "[data-testid='ss-pin']").click();
    settle().await;
    assert_eq!(mode(&page), "pinned");
    let root = el(&page, TILE);
    assert!(root.class_list().contains("tile-pinned"));
    assert_eq!(attr(&page, TILE, "data-pinned").as_deref(), Some("true"));
    assert_eq!(attr(&page, TILE, "data-pin-rank").as_deref(), Some("0"));
    assert_eq!(
        attr(&page, TILE, "aria-label").as_deref(),
        Some("Your shared content, pinned")
    );
    assert_eq!(root.style().get_property_value("order").unwrap(), "-1000");
    assert!(badge(), "a pinned share shows the passive badge");
    assert_eq!(
        attr(&page, "[data-testid='ss-enlarge']", "aria-pressed").as_deref(),
        Some("false"),
        "R2: Enlarge reads unpressed while pinned"
    );
    assert_eq!(announced(&page), "Your shared content pinned");
    assert_eq!(active_id(), "own-screen-share-pin-btn");
    assert!(
        js_sys::Object::is(el(&page, TILE).as_ref(), root_before.as_ref())
            && js_sys::Object::is(
                el(&page, "#own-screen-share-video").as_ref(),
                video_before.as_ref()
            ),
        "C1: a mode change must not remount the tile or its media element"
    );

    let viewport = el(&page, "[data-testid='ss-zoom-viewport']");
    viewport.focus().unwrap();
    let esc = js("return new KeyboardEvent('keydown', { key: 'Escape', bubbles: true });");
    viewport.dispatch_event(esc.unchecked_ref()).unwrap();
    settle().await;
    assert_eq!(mode(&page), "pinned", "Escape no longer unpins");
    assert_eq!(text(&page, "#h-pin"), "OwnScreen:me");

    el(&page, "[data-testid='ss-pin']").click();
    settle().await;
    assert_eq!(mode(&page), "tile", "an unpin returns to the grid");
    assert_eq!(announced(&page), "Your shared content unpinned");
    assert_eq!(
        storage().get_item(PREF_KEY).unwrap().as_deref(),
        Some("tile")
    );
    assert_eq!(active_id(), "own-screen-share-pin-btn");
    assert_eq!(attr(&page, TILE, "data-pinned"), None);
    assert_eq!(
        attr(&page, TILE, "aria-label").as_deref(),
        Some("Your shared content")
    );
    assert_eq!(
        el(&page, TILE).style().get_property_value("order").unwrap(),
        "-2",
        "the unpinned order replaces the pinned one"
    );
    assert!(!badge());
}

#[wasm_bindgen_test]
async fn the_mirror_guard_hides_the_preview_until_show_applies_the_pref() {
    let page = mount_with(|| {
        storage().set_item(PREF_KEY, "pinned").unwrap();
        SEED_GUARD.with(|g| g.set(true));
    })
    .await;

    assert_eq!(attr(&page, TILE, "data-guard").as_deref(), Some("true"));
    assert!(el(&page, "[data-testid='ss-zoom-viewport']").hidden());
    assert!(page
        .mount
        .query_selector("[data-testid='ss-hide-preview']")
        .unwrap()
        .is_none());

    el(&page, "[data-testid='ss-show-preview']").click();
    settle().await;
    assert_eq!(attr(&page, TILE, "data-guard"), None);
    assert!(!el(&page, "[data-testid='ss-zoom-viewport']").hidden());
    assert_eq!(
        mode(&page),
        "pinned",
        "Show preview applies the sticky pref"
    );
    assert_eq!(active_id(), "own-screen-share-hide-preview");

    el(&page, "[data-testid='ss-hide-preview']").click();
    settle().await;
    assert_eq!(
        mode(&page),
        "tile",
        "hiding releases the pin as a system action"
    );
    assert_eq!(active_id(), "own-screen-share-show-preview");
    assert_eq!(
        storage().get_item(PREF_KEY).unwrap().as_deref(),
        Some("pinned"),
        "R1: the guard never writes the pref"
    );
}

#[wasm_bindgen_test]
async fn the_guard_refuses_view_actions_on_the_own_tile() {
    let page = mount_with(|| SEED_GUARD.with(|g| g.set(true))).await;
    el(&page, "[data-testid='ss-enlarge']").click();
    el(&page, "[data-testid='ss-pin']").click();
    settle().await;
    assert_eq!(mode(&page), "tile");
    assert_eq!(text(&page, "#h-pin"), "");
    assert_eq!(storage().get_item(PREF_KEY).unwrap(), None);
}

#[wasm_bindgen_test]
async fn enlarge_on_one_share_demotes_the_other_and_keeps_its_pin() {
    let page = mount_with(|| {
        SEED_RECEIVED_ENLARGED.with(|r| r.set(true));
        SEED_PINS.with(|p| p.borrow_mut().push(PinnedTile::screen("bob")));
    })
    .await;
    el(&page, "[data-testid='ss-enlarge']").click();
    settle().await;
    assert_eq!(text(&page, "#h-received-base"), "Tile", "C2: one stage");
    assert_eq!(
        text(&page, "#h-pin"),
        "Screen:bob",
        "the other share's pin is untouched"
    );
    assert_eq!(mode(&page), "enlarged");
}

#[wasm_bindgen_test]
async fn pin_on_one_share_leaves_the_other_alone() {
    let page = mount_with(|| {
        SEED_RECEIVED_ENLARGED.with(|r| r.set(true));
        SEED_PINS.with(|p| p.borrow_mut().push(PinnedTile::screen("bob")));
    })
    .await;
    el(&page, "[data-testid='ss-pin']").click();
    settle().await;
    assert_eq!(
        text(&page, "#h-received-base"),
        "Enlarged",
        "a pin never demotes the other share"
    );
    assert_eq!(
        text(&page, "#h-pin"),
        "OwnScreen:me,Screen:bob",
        "both shares stay pinned, most recent first"
    );
}

#[wasm_bindgen_test]
async fn a_synchronous_detach_failure_reverts_and_keeps_focus() {
    let page = mount_with(install_pip_stub).await;
    let detach = el(&page, "[data-testid='ss-detach']");
    detach.focus().unwrap();
    detach.click();
    settle().await;
    assert_eq!(mode(&page), "tile", "T15: back to the previous mode");
    assert_eq!(announced(&page), DETACH_FAILED);
    assert_eq!(storage().get_item(PREF_KEY).unwrap(), None, "R1");
    assert_eq!(
        active_id(),
        DETACH_BTN,
        "T15: focus stays on the pressed button"
    );
}

fn seed_live_capture() -> web_sys::MediaStream {
    let (stream, _paint) = live_canvas_stream();
    let seeded = Clone::clone(&stream);
    install_pip_stub();
    STREAM.with(|s| *s.borrow_mut() = Some(seeded));
    stream
}

#[wasm_bindgen_test]
async fn the_detached_pref_is_committed_only_once_the_window_opens() {
    let mut stream = None;
    let page = mount_with(|| stream = Some(seed_live_capture())).await;
    let stream = stream.unwrap();
    el(&page, "[data-testid='ss-detach']").click();
    settle().await;
    assert_eq!(mode(&page), "detached", "the layout switch is optimistic");
    assert_eq!(
        storage().get_item(PREF_KEY).unwrap(),
        None,
        "R3: nothing is written while the window is pending"
    );

    let frame = resolve_pip_with_frame();
    settle().await;
    assert_eq!(
        storage().get_item(PREF_KEY).unwrap().as_deref(),
        Some("detached")
    );
    assert_eq!(
        announced(&page),
        "Your shared content opened in a separate window"
    );
    let built = frame.get_element_by_id("ss-detached-reattach");
    assert!(built.is_some(), "premise: the window was built");

    ssd::reattach(OWN_SHARE_KEY);
    settle().await;
    assert_capture_live(&stream, "reattached");
    assert_eq!(mode(&page), "tile");
    assert_eq!(
        announced(&page),
        "Your shared content returned to the meeting"
    );
    assert_eq!(
        active_id(),
        DETACH_BTN,
        "T12: focus returns to the Detach button"
    );
}

#[wasm_bindgen_test]
async fn a_system_teardown_cancels_a_pending_open() {
    let page = mount_with(|| {
        install_pip_stub();
        STREAM.with(|s| *s.borrow_mut() = Some(web_sys::MediaStream::new().unwrap()));
    })
    .await;
    el(&page, "[data-testid='ss-detach']").click();
    settle().await;
    assert!(
        ssd::is_pending(OWN_SHARE_KEY),
        "premise: the open is in flight"
    );

    share_view::teardown_with_cause(OWN_SHARE_KEY, TeardownCause::System);
    let frame = resolve_pip_with_frame();
    settle().await;
    assert!(!ssd::is_busy(), "the late window must not be installed");
    assert!(
        frame.get_element_by_id("ss-detached-reattach").is_none(),
        "the late window must not be built"
    );
    assert_eq!(text(&page, "#h-detached"), "");
    assert_ne!(
        announced(&page),
        DETACH_FAILED,
        "a system cancel is not a failed open"
    );
}

#[wasm_bindgen_test]
async fn close_all_closes_the_window_without_a_reattach() {
    let mut stream = None;
    let page = mount_with(|| stream = Some(seed_live_capture())).await;
    let stream = stream.unwrap();
    detach_own_share(&page).await;
    ssd::close_all();
    assert!(!ssd::is_busy());
    assert_capture_live(&stream, "close_all");
    assert_eq!(
        storage().get_item(PREF_KEY).unwrap().as_deref(),
        Some("detached"),
        "leaving writes no pref"
    );
}

fn page_hide(persisted: bool) {
    let event = js(&format!(
        "return new PageTransitionEvent('pagehide', {{ persisted: {persisted} }});"
    ));
    gloo_utils::window()
        .dispatch_event(event.unchecked_ref())
        .unwrap();
}

fn page_leave() {
    page_hide(false);
}

async fn detach_own_share(page: &Page) -> web_sys::Document {
    el(page, "[data-testid='ss-detach']").click();
    settle().await;
    let frame = resolve_pip_with_frame();
    settle().await;
    assert!(ssd::is_busy(), "premise: a window is open");
    frame
}

#[wasm_bindgen_test]
async fn leaving_the_page_closes_the_detached_window() {
    let mut stream = None;
    let page = mount_with(|| stream = Some(seed_live_capture())).await;
    let stream = stream.unwrap();
    let frame = detach_own_share(&page).await;
    page_leave();
    assert!(!ssd::is_busy());
    assert!(frame_closed(&frame), "the window must not outlive the page");
    assert_capture_live(&stream, "page left");
    settle().await;
    assert_eq!(
        text(&page, "#h-detached"),
        OWN_SHARE_KEY,
        "a page being left writes no signal"
    );
    assert_eq!(
        storage().get_item(PREF_KEY).unwrap().as_deref(),
        Some("detached")
    );
}

#[wasm_bindgen_test]
async fn a_page_kept_in_the_back_forward_cache_gets_its_share_tile_back() {
    let mut stream = None;
    let page = mount_with(|| stream = Some(seed_live_capture())).await;
    let stream = stream.unwrap();
    let frame = detach_own_share(&page).await;
    page_hide(true);
    assert!(!ssd::is_busy());
    assert!(frame_closed(&frame));
    assert_capture_live(&stream, "page cached");
    settle().await;
    assert_eq!(text(&page, "#h-detached"), "");
    assert_eq!(mode(&page), "tile", "the tile is back on the page");
    assert_eq!(
        storage().get_item(PREF_KEY).unwrap().as_deref(),
        Some("detached"),
        "a system reattach writes no pref"
    );
}

async fn open_detached(key: &str) -> (web_sys::Document, web_sys::MediaStream) {
    let (stream, _paint) = live_canvas_stream();
    ssd::open_stream(key, Clone::clone(&stream), Box::new(|| {}), Box::new(|| {}));
    assert!(ssd::is_pending(key), "premise: the open is in flight");
    page_leave();
    let frame = resolve_pip_with_frame();
    settle().await;
    (frame, stream)
}

#[wasm_bindgen_test]
async fn the_page_leave_listener_goes_with_the_window() {
    fresh().await;
    install_pip_stub();
    let (_, stream) = open_detached("a").await;
    ssd::close_all();
    assert_capture_live(&stream, "close_all");

    let (frame, stream) = open_detached("b").await;
    assert!(
        ssd::is_busy() && !frame_closed(&frame),
        "close_all must remove the listener"
    );
    ssd::reattach("b");
    assert_capture_live(&stream, "reattach");

    let (frame, _) = open_detached("c").await;
    assert!(
        ssd::is_busy() && !frame_closed(&frame),
        "a teardown must remove the listener"
    );
}

#[wasm_bindgen_test]
async fn show_preview_over_a_whole_screen_announces_the_detach_hint() {
    let (stream, _paint) = live_canvas_stream();
    let track = stream.get_video_tracks().get(0);
    js_sys::Reflect::set(
        &track,
        &"getSettings".into(),
        &js("return () => ({ displaySurface: 'monitor' });"),
    )
    .unwrap();
    let page = mount_with(move || {
        install_pip_stub();
        storage().set_item(PREF_KEY, "detached").unwrap();
        SEED_GUARD.with(|g| g.set(true));
        STREAM.with(|s| *s.borrow_mut() = Some(stream));
    })
    .await;
    el(&page, "[data-testid='ss-show-preview']").click();
    settle().await;
    assert!(
        page.mount
            .query_selector("[data-testid='ss-detach-cta']")
            .unwrap()
            .is_some(),
        "premise: Show preview offers the CTA instead of opening a window"
    );
    assert_eq!(announced(&page), CTA_HINT);
}

#[allow(non_snake_case)]
fn ArrivalHarness() -> Element {
    let zoom = use_signal(HashMap::new);
    let actual = use_signal(|| None::<String>);
    let ctx = ShareViewCtx {
        slots: use_signal(ShareSlots::default),
        pins: use_signal(Vec::<PinnedTile>::new),
        detached: use_signal(|| None::<String>),
        announce: use_signal(|| (String::new(), 0u32)),
        own_stream: use_signal(|| None::<web_sys::MediaStream>),
    };
    let mut resolved = use_signal(|| false);
    let tracker = use_hook(|| Rc::new(RefCell::new(ShareTracker::default())));
    let known = resolved();
    let target = ShareTarget {
        origin: ShareOrigin::Received,
        key: "7".to_string(),
        pin: PinnedTile::screen(if known { "carol" } else { "7" }),
        name: "Carol".to_string(),
    };
    share_view::track_shares(
        &mut tracker.borrow_mut(),
        ctx,
        Some(&target),
        known,
        &|_| true,
        None,
        false,
        zoom,
        actual,
    );
    let pin_probe = pin_list_probe(&ctx.pins.read());
    let mut pins = ctx.pins;
    rsx! {
        button { id: "h-resolve", onclick: move |_| resolved.set(true) }
        button {
            id: "h-pin-camera",
            onclick: move |_| {
                pin_order::toggle_pin(&mut pins.write(), PinnedTile::camera("bob"));
            },
        }
        button {
            id: "h-enlarge",
            onclick: move |_| share_view::dispatch(ctx, &target, ShareAction::Enlarge),
        }
        span { id: "h-pin", "{pin_probe}" }
    }
}

async fn mount_arrival() -> Page {
    fresh().await;
    storage()
        .set_item(ShareOrigin::Received.pref_key(), "pinned")
        .unwrap();
    let mount = marked_mount();
    render_into(&mount, ArrivalHarness);
    yield_now().await;
    Page { mount }
}

#[wasm_bindgen_test]
async fn a_pinned_pref_waits_for_the_sharer_id_before_pinning() {
    let page = mount_arrival().await;
    assert_eq!(
        text(&page, "#h-pin"),
        "",
        "a pin by session id would be cleared as stale once the id resolves"
    );
    el(&page, "#h-resolve").click();
    settle().await;
    assert_eq!(text(&page, "#h-pin"), "Screen:carol");
}

#[wasm_bindgen_test]
async fn a_choice_made_before_the_sharer_id_resolves_settles_the_arrival_pin() {
    for (choice, before, after) in [
        ("#h-pin-camera", "Camera:bob", "Screen:carol,Camera:bob"),
        ("#h-enlarge", "", ""),
    ] {
        let page = mount_arrival().await;
        el(&page, choice).click();
        settle().await;
        assert_eq!(
            text(&page, "#h-pin"),
            before,
            "premise: {choice} took effect"
        );
        el(&page, "#h-resolve").click();
        settle().await;
        assert_eq!(
            text(&page, "#h-pin"),
            after,
            "{choice}: pins coexist, so only a view change cancels the arrival pin"
        );
    }
}

#[wasm_bindgen_test]
async fn reattach_focus_returns_to_the_detach_button() {
    let page = mount_with(install_pip_stub).await;
    el(&page, "#h-detach-on").click();
    settle().await;
    assert_eq!(active_id(), "grid-container", "premise: ENTER moved focus");
    el(&page, "#h-detach-off").click();
    settle().await;
    assert_eq!(active_id(), DETACH_BTN);
}

#[wasm_bindgen_test]
async fn a_mode_change_reclamps_the_pan_to_the_new_viewport() {
    let page = mount().await;
    el(&page, "#h-seed-zoom").click();
    settle().await;
    assert_eq!(text(&page, "#h-zoom"), "5000", "premise: an unclamped pan");
    el(&page, "[data-testid='ss-enlarge']").click();
    settle().await;
    let half = el(&page, "[data-testid='ss-zoom-viewport']").client_width() as f64 / 2.0;
    assert_eq!(
        text(&page, "#h-zoom"),
        format!("{half:.0}"),
        "T13: scale kept, pan clamped to (scale - 1) x half the viewport"
    );
}

#[wasm_bindgen_test]
async fn unmounting_the_tile_leaves_focus_that_moved_elsewhere() {
    let page = mount().await;
    let viewport = el(&page, "[data-testid='ss-zoom-viewport']");
    viewport.focus().unwrap();
    viewport.blur().unwrap();
    let input: web_sys::HtmlElement = gloo_utils::document()
        .create_element("input")
        .unwrap()
        .unchecked_into();
    mark(&input);
    gloo_utils::document()
        .body()
        .unwrap()
        .append_child(&input)
        .unwrap();
    input.focus().unwrap();
    el(&page, "#h-unmount").click();
    settle().await;
    assert!(
        gloo_utils::document()
            .active_element()
            .is_some_and(|a| a.is_same_node(Some(&input))),
        "the unmount rescue must not steal focus from outside the tile"
    );
}

#[wasm_bindgen_test]
async fn unmounting_the_tile_rescues_focus_left_inside_it() {
    let page = mount().await;
    el(&page, "[data-testid='ss-zoom-viewport']")
        .focus()
        .unwrap();
    el(&page, "#h-unmount").click();
    settle().await;
    assert_eq!(active_id(), "grid-container");
}

#[wasm_bindgen_test]
async fn recording_draws_the_own_share_even_while_its_preview_is_paused() {
    js(include_str!("../scripts/recording.js"));
    let (stream, paint) = live_canvas_stream();
    let page = mount_with(move || {
        STREAM.with(|s| *s.borrow_mut() = Some(stream));
        SEED_GUARD.with(|g| g.set(true));
    })
    .await;
    let video: web_sys::HtmlVideoElement = el(&page, "#own-screen-share-video").unchecked_into();
    assert!(
        video.paused(),
        "premise: the mirror guard paused the preview"
    );

    let recording = js_sys::Reflect::get(&gloo_utils::window(), &"__vcRecording".into()).unwrap();
    let resolve: js_sys::Function =
        js_sys::Reflect::get(&recording, &"_resolveScreenSource".into())
            .unwrap()
            .unchecked_into();
    let grid = gloo_utils::document()
        .get_element_by_id("grid-container")
        .unwrap();
    let mut source = JsValue::NULL;
    for i in 0..60 {
        paint.set_fill_style_str(if i % 2 == 0 { "#123456" } else { "#654321" });
        paint.fill_rect(0.0, 0.0, 64.0, 36.0);
        let resolved = resolve.call1(&recording, &grid).unwrap();
        source = js_sys::Reflect::get(&resolved, &"source".into()).unwrap();
        if !source.is_null() {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(50).await;
    }
    assert!(
        js_sys::Object::is(&source, video.as_ref()),
        "the recording composites the own share video"
    );
    let local: js_sys::Function = js_sys::Reflect::get(&recording, &"_localShareVideo".into())
        .unwrap()
        .unchecked_into();
    let audio_source = local.call0(&recording).unwrap();
    assert!(
        js_sys::Object::is(&audio_source, video.as_ref()),
        "shared tab audio is mixed from the same element"
    );
}

fn recording_call(name: &str, args: &[&JsValue]) -> JsValue {
    let recording = js_sys::Reflect::get(&gloo_utils::window(), &"__vcRecording".into()).unwrap();
    let f: js_sys::Function = js_sys::Reflect::get(&recording, &name.into())
        .unwrap()
        .unchecked_into();
    let args: js_sys::Array = args.iter().copied().collect();
    f.apply(&recording, &args).unwrap()
}

async fn kick_until_playing(
    video: &web_sys::HtmlVideoElement,
    paint: &web_sys::CanvasRenderingContext2d,
) {
    let grid: JsValue = gloo_utils::document()
        .get_element_by_id("grid-container")
        .unwrap()
        .into();
    for i in 0..60 {
        paint.set_fill_style_str(if i % 2 == 0 { "#123456" } else { "#654321" });
        paint.fill_rect(0.0, 0.0, 64.0, 36.0);
        recording_call("_resolveScreenSource", &[&grid]);
        if !video.paused() {
            return;
        }
        gloo_timers::future::TimeoutFuture::new(50).await;
    }
    panic!("premise: the recording kick plays the paused preview");
}

#[wasm_bindgen_test]
async fn stopping_the_recording_re_pauses_only_a_preview_that_stays_hidden() {
    js(include_str!("../scripts/recording.js"));
    let (stream, paint) = live_canvas_stream();
    let page = mount_with(move || {
        STREAM.with(|s| *s.borrow_mut() = Some(stream));
        SEED_GUARD.with(|g| g.set(true));
    })
    .await;
    let video: web_sys::HtmlVideoElement = el(&page, "#own-screen-share-video").unchecked_into();
    kick_until_playing(&video, &paint).await;
    recording_call("_stopFrameLoop", &[]);
    assert!(video.paused(), "the hidden preview goes back to sleep");

    kick_until_playing(&video, &paint).await;
    el(&page, "[data-testid='ss-show-preview']").click();
    settle().await;
    recording_call("_stopFrameLoop", &[]);
    assert!(
        !video.paused(),
        "a preview the user showed meanwhile keeps playing"
    );
}

// ---- one peer template across the grid, the Tile layout and the split ----

thread_local! {
    static PEER_MODE: RefCell<Option<Signal<TileMode>>> = const { RefCell::new(None) };
    static PEER_PIN_RANK: RefCell<Option<Signal<Option<usize>>>> = const { RefCell::new(None) };
}

#[allow(non_snake_case)]
fn PeerModes() -> Element {
    let client = use_hook(|| VideoCallClient::new_for_test("local-user"));
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
    let render_mode = use_signal(|| TileMode::Full);
    PEER_MODE.with(|m| *m.borrow_mut() = Some(render_mode));
    let pin_rank = use_signal(|| None::<usize>);
    PEER_PIN_RANK.with(|r| *r.borrow_mut() = Some(pin_rank));
    let pin: EventHandler<PinnedTile> = use_callback(|_: PinnedTile| {});
    let decode: EventHandler<String> = use_callback(|_: String| {});
    rsx! {
        div { class: "ss-peer-panel",
            PeerTile {
                key: "tile-alice",
                peer_id: "alice".to_string(),
                render_mode: render_mode(),
                pin_rank: pin_rank(),
                on_toggle_pin: pin,
                on_request_decode: decode,
            }
        }
    }
}

#[wasm_bindgen_test]
async fn a_peer_tile_keeps_its_nodes_across_every_share_layout() {
    fresh().await;
    inject_app_config();
    let mount = marked_mount();
    render_into(&mount, PeerModes);
    yield_now().await;
    let page = Page { mount };
    let root = el(&page, "#peer-video-alice-div");
    let inner = el(&page, "#peer-video-alice-div .canvas-container");
    for (next, class) in [
        (TileMode::GridVideoOnly, "grid-item"),
        (TileMode::VideoOnly, "split-peer-tile"),
        (TileMode::GridVideoOnly, "grid-item"),
        (TileMode::Full, "grid-item"),
    ] {
        let mut signal = PEER_MODE.with(|m| m.borrow().unwrap());
        signal.set(next.clone());
        settle().await;
        let now = el(&page, "#peer-video-alice-div");
        assert!(
            js_sys::Object::is(now.as_ref(), root.as_ref())
                && root.is_connected()
                && js_sys::Object::is(
                    el(&page, "#peer-video-alice-div .canvas-container").as_ref(),
                    inner.as_ref()
                ),
            "{next:?}: the tile must not remount (a camera canvas remount asks the publisher for a keyframe)"
        );
        assert!(
            root.class_list().contains(class),
            "{next:?}: root class should include {class}, got {}",
            root.class_name()
        );
    }
}

async fn mount_peer_modes() -> Page {
    fresh().await;
    inject_app_config();
    let mount = marked_mount();
    render_into(&mount, PeerModes);
    yield_now().await;
    Page { mount }
}

/// `focus({ focusVisible })`: the browser's keyboard-or-pointer heuristic, set
/// explicitly.
fn focus_visibly(el: &web_sys::HtmlElement, visible: bool) {
    el.blur().unwrap();
    let focus: js_sys::Function = js_sys::Reflect::get(el, &"focus".into())
        .unwrap()
        .unchecked_into();
    let options = js(&format!("return {{ focusVisible: {visible} }};"));
    focus.call1(el, &options).unwrap();
    assert_eq!(
        el.matches(":focus-visible").unwrap(),
        visible,
        "premise: the browser honours focusVisible"
    );
}

fn set_peer_pin_rank(rank: Option<usize>) {
    let mut signal = PEER_PIN_RANK.with(|r| r.borrow().unwrap());
    signal.set(rank);
}

const ALICE: &str = "#peer-video-alice-div";
const ALICE_PIN: &str = "#peer-video-alice-div-pin-btn";

#[wasm_bindgen_test]
async fn a_pinned_camera_tile_leads_by_order_at_its_size_and_drops_it_on_unpin() {
    let page = mount_peer_modes().await;
    assert_eq!(
        attr(&page, ALICE_PIN, "aria-label").as_deref(),
        Some("Pin alice"),
        "the name is constant, aria-pressed carries the state"
    );
    set_peer_pin_rank(Some(0));
    settle().await;
    let root = el(&page, ALICE);
    assert!(root.class_list().contains("tile-pinned"));
    assert!(!root.class_list().contains("grid-item-pinned"));
    assert_eq!(attr(&page, ALICE, "data-pinned").as_deref(), Some("true"));
    assert_eq!(attr(&page, ALICE, "data-pin-rank").as_deref(), Some("0"));
    assert_eq!(root.style().get_property_value("order").unwrap(), "-1000");
    assert_eq!(
        attr(&page, ALICE_PIN, "aria-pressed").as_deref(),
        Some("true")
    );
    assert_eq!(attr(&page, ALICE_PIN, "title").as_deref(), Some("Unpin"));

    set_peer_pin_rank(None);
    settle().await;
    let root = el(&page, ALICE);
    assert!(!root.class_list().contains("tile-pinned"));
    assert_eq!(attr(&page, ALICE, "data-pinned"), None);
    assert_eq!(attr(&page, ALICE, "data-pin-rank"), None);
    assert_eq!(
        root.style().get_property_value("order").unwrap(),
        "0",
        "Dioxus restores a longhand a later style omits, so unpin must write it"
    );
    assert_eq!(
        attr(&page, ALICE_PIN, "aria-pressed").as_deref(),
        Some("false")
    );
    assert_eq!(attr(&page, ALICE_PIN, "title").as_deref(), Some("Pin"));
}

#[wasm_bindgen_test]
async fn the_camera_pin_keeps_focus_and_is_reachable_while_hidden() {
    let page = mount_peer_modes().await;
    install_stylesheets(&[("@media (prefers-reduced-motion: reduce)", "@media all")]);
    settle().await;
    assert_eq!(
        computed(&page, ALICE_PIN, "opacity"),
        "0",
        "premise: hidden until revealed"
    );
    assert_eq!(computed(&page, ALICE_PIN, "visibility"), "visible");

    el(&page, ALICE_PIN).click();
    settle().await;
    assert_eq!(
        active_id(),
        "peer-video-alice-div-pin-btn",
        "focus follows the pinned tile's button"
    );
    let pin = el(&page, ALICE_PIN);
    focus_visibly(&pin, false);
    assert_eq!(active_id(), "peer-video-alice-div-pin-btn");
    assert_eq!(
        computed(&page, ALICE_PIN, "opacity"),
        "0",
        "a mouse-focused, unpressed pin hides once the pointer leaves"
    );
    focus_visibly(&pin, true);
    assert_eq!(
        computed(&page, ALICE_PIN, "opacity"),
        "1",
        "keyboard focus reveals the pin"
    );

    set_peer_pin_rank(Some(0));
    settle().await;
    el(&page, ALICE_PIN).blur().unwrap();
    assert_eq!(
        computed(&page, ALICE_PIN, "opacity"),
        "1",
        "a pressed pin shows without hover or focus"
    );
    assert_eq!(
        computed(&page, ALICE_PIN, "background-color"),
        "rgb(0, 122, 255)"
    );
}

thread_local! {
    static CAMERA_TILES: RefCell<Option<Signal<Vec<(String, TileRenderMode)>>>> =
        const { RefCell::new(None) };
    static CAMERA_SEED: Cell<CameraSeed> = const { Cell::new(CameraSeed::GRID) };
    static DECODE_REQUESTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static NOOP_CALLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy)]
struct CameraSeed {
    split: bool,
    tiles: &'static [&'static str],
    pinned: &'static [&'static str],
    allowed_to_stream: &'static str,
    css: bool,
}

impl CameraSeed {
    const GRID: Self = Self {
        split: false,
        tiles: &["11", "12", "mock-0", "13"],
        pinned: &[],
        allowed_to_stream: "",
        css: false,
    };
}

/// Three camera-on peers and a mock tile, rendered by the production list.
#[allow(non_snake_case)]
fn CameraTileList() -> Element {
    let seed = use_hook(|| CAMERA_SEED.with(Cell::get));
    let client = use_hook(|| {
        let client = VideoCallClient::new_for_test("local-user");
        for (session, user) in [(11, "ann"), (12, "bea"), (13, "cy")] {
            client.insert_peer_for_test(session, user);
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
    let tiles = use_signal(|| camera_tile_order(seed.tiles, &[]));
    CAMERA_TILES.with(|t| *t.borrow_mut() = Some(tiles));
    let pin: EventHandler<PinnedTile> = use_callback(|_: PinnedTile| {});
    // Declared in attendants.rs's order: the noop first.
    let noop: EventHandler<String> =
        use_callback(|sid: String| NOOP_CALLS.with(|n| n.borrow_mut().push(sid)));
    let request: EventHandler<String> =
        use_callback(|sid: String| DECODE_REQUESTS.with(|r| r.borrow_mut().push(sid)));
    let ranks: HashMap<String, usize> = seed
        .pinned
        .iter()
        .enumerate()
        .map(|(rank, id)| (id.to_string(), rank))
        .collect();
    let no_bleed = |_: &str| false;
    let (grid_class, flow, mode) = if seed.split {
        ("has-screen-share", None, TileMode::VideoOnly)
    } else {
        ("", Some("wrap"), TileMode::Full)
    };
    let order = tiles
        .read()
        .iter()
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    rsx! {
        div {
            id: "grid-container",
            class: grid_class,
            "data-tile-flow": flow,
            style: "display: flex; flex-wrap: wrap; width: 1200px; height: 600px; \
                    --tile-w: 210px; --tile-h: 140px;",
            div { class: "ss-left-pane" }
            div { class: "ss-peer-panel",
                {camera_tiles(CameraTiles {
                    tiles: &tiles.read(),
                    pin_rank: &ranks,
                    full_bleed: &no_bleed,
                    host_user_id: &None,
                    render_mode: &mode,
                    my_session_id: &None,
                    room_id: "room",
                    is_host: false,
                    on_toggle_pin: pin,
                    on_request_decode: request,
                    mock_on_toggle_pin: pin,
                    mock_on_request_decode: noop,
                })}
            }
        }
        button { id: "h-noop", onclick: move |_| noop.call("probe".to_string()) }
        // After the list, so it only updates when every list edit applied.
        span { id: "h-order", "{order}" }
    }
}

fn camera_tile_order(ids: &[&str], paused: &[&str]) -> Vec<(String, TileRenderMode)> {
    ids.iter()
        .map(|id| {
            let mode = if paused.contains(id) {
                TileRenderMode::Avatar
            } else {
                TileRenderMode::Decoded
            };
            (id.to_string(), mode)
        })
        .collect()
}

fn set_camera_tiles(ids: &[&str], paused: &[&str]) {
    let mut signal = CAMERA_TILES.with(|t| t.borrow().unwrap());
    signal.set(camera_tile_order(ids, paused));
}

fn allow_to_stream(users: &str) {
    let window = gloo_utils::window();
    let config = js_sys::Reflect::get(&window, &"__APP_CONFIG".into()).unwrap();
    let next = js_sys::Object::assign(&js_sys::Object::new(), &config.into());
    js_sys::Reflect::set(&next, &"usersAllowedToStream".into(), &users.into()).unwrap();
    js_sys::Reflect::set(
        &window,
        &"__APP_CONFIG".into(),
        &js_sys::Object::freeze(&next),
    )
    .unwrap();
    dioxus_ui::constants::reset_config_cache_for_test();
}

async fn mount_camera_tiles(seed: CameraSeed) -> Page {
    fresh().await;
    inject_app_config();
    if !seed.allowed_to_stream.is_empty() {
        allow_to_stream(seed.allowed_to_stream);
    }
    if seed.css {
        install_stylesheets(&[]);
    }
    CAMERA_SEED.with(|s| s.set(seed));
    DECODE_REQUESTS.with(|r| r.borrow_mut().clear());
    NOOP_CALLS.with(|n| n.borrow_mut().clear());
    let mount = marked_mount();
    render_into(&mount, CameraTileList);
    settle().await;
    Page { mount }
}

type TileNodes = (web_sys::HtmlElement, Option<web_sys::Element>);

fn tile_nodes(page: &Page, id: &str) -> TileNodes {
    let root = el(page, &format!("#peer-video-{id}-div"));
    let canvas = root.query_selector(".canvas-container > canvas").unwrap();
    (root, canvas)
}

fn assert_moved_not_rebuilt(page: &Page, id: &str, (root, canvas): &TileNodes) {
    let (root_now, canvas_now) = tile_nodes(page, id);
    assert!(
        root.is_connected() && js_sys::Object::is(root.as_ref(), root_now.as_ref()),
        "{id}: the tile root must be moved, not rebuilt"
    );
    if let Some(canvas) = canvas {
        assert!(
            canvas.is_connected()
                && canvas_now.is_some_and(|c| js_sys::Object::is(c.as_ref(), canvas.as_ref())),
            "{id}: a rebuilt canvas asks the publisher for a keyframe"
        );
    }
}

fn rendered_order(page: &Page) -> String {
    el(page, "#h-order").text_content().unwrap_or_default()
}

fn tile_root_ids(page: &Page) -> Vec<String> {
    page.mount
        .query_selector_all("[data-tile-root]")
        .unwrap()
        .values()
        .into_iter()
        .map(|n| n.unwrap().unchecked_into::<web_sys::Element>().id())
        .collect()
}

#[wasm_bindgen_test]
async fn a_reorder_moves_camera_tiles_instead_of_remounting_them() {
    let page = mount_camera_tiles(CameraSeed::GRID).await;
    let ids = ["11", "12", "mock-0", "13"];
    let before: Vec<_> = ids.iter().map(|id| tile_nodes(&page, id)).collect();
    for (id, (_, canvas)) in ids.iter().zip(&before) {
        assert_eq!(
            canvas.is_some(),
            !id.starts_with("mock-"),
            "premise: {id}'s canvas"
        );
    }

    set_camera_tiles(&["13", "11", "12", "mock-0"], &[]);
    settle().await;
    let moved = &before[3].0;
    assert!(
        moved.is_connected() && page.mount.contains(Some(moved)),
        "13: the moved tile must still be in the page"
    );
    assert_eq!(
        rendered_order(&page),
        "13,11,12,mock-0",
        "every list edit applied"
    );
    assert_eq!(
        tile_root_ids(&page),
        ["13", "11", "12", "mock-0"].map(|id| format!("peer-video-{id}-div")),
        "premise: the list reordered"
    );
    for (id, nodes) in ids.iter().zip(&before) {
        assert_moved_not_rebuilt(&page, id, nodes);
    }
}

#[wasm_bindgen_test]
async fn a_reorder_moves_a_tile_that_renders_nothing() {
    // bea (12) may not stream, so her tile renders only a placeholder.
    let page = mount_camera_tiles(CameraSeed {
        tiles: &["12", "11", "13"],
        allowed_to_stream: "ann,cy",
        ..CameraSeed::GRID
    })
    .await;
    assert!(
        page.mount
            .query_selector("#peer-video-12-div")
            .unwrap()
            .is_none(),
        "premise: 12 renders nothing"
    );
    let before = [tile_nodes(&page, "11"), tile_nodes(&page, "13")];

    set_camera_tiles(&["11", "13", "12"], &[]);
    settle().await;
    assert_eq!(rendered_order(&page), "11,13,12", "every list edit applied");
    for (id, nodes) in ["11", "13"].iter().zip(&before) {
        assert_moved_not_rebuilt(&page, id, nodes);
    }
}

#[wasm_bindgen_test]
async fn a_pinned_camera_tile_leads_by_order_through_its_slot() {
    for split in [false, true] {
        // 13 is last in the DOM, so only its `order` can lead it.
        let page = mount_camera_tiles(CameraSeed {
            split,
            pinned: &["13"],
            css: true,
            ..CameraSeed::GRID
        })
        .await;
        let slot = el(&page, "#peer-video-13-div").parent_element().unwrap();
        let slot_attrs: Vec<_> = slot
            .get_attribute_names()
            .iter()
            .filter_map(|a| a.as_string())
            .collect();
        assert_eq!(
            slot_attrs,
            ["class"],
            "no role, aria, tabindex or style on the slot"
        );
        assert_eq!(
            web_sys::window()
                .unwrap()
                .get_computed_style(&slot)
                .unwrap()
                .unwrap()
                .get_property_value("display")
                .unwrap(),
            "contents",
            "split={split}: the slot generates no box"
        );
        let mut placed: Vec<(i64, f64, String)> = page
            .mount
            .query_selector_all("[data-tile-root]")
            .unwrap()
            .values()
            .into_iter()
            .map(|n| {
                let tile = n.unwrap().unchecked_into::<web_sys::Element>();
                let rect = tile.get_bounding_client_rect();
                (rect.top().round() as i64, rect.left(), tile.id())
            })
            .collect();
        placed.sort_by(|a, b| (a.0, a.1).partial_cmp(&(b.0, b.1)).unwrap());
        let reading: Vec<_> = placed.into_iter().map(|(_, _, id)| id).collect();
        assert_eq!(
            reading,
            ["13", "11", "12", "mock-0"].map(|id| format!("peer-video-{id}-div")),
            "split={split}: the pin leads through the slot"
        );
        if !split {
            assert_eq!(
                computed(&page, "#peer-video-11-div", "width"),
                "210px",
                "the wrap arm sizes a slotted tile"
            );
        }
    }
}

#[wasm_bindgen_test]
async fn a_decode_mode_flip_leaves_the_shared_handlers_alone() {
    let page = mount_camera_tiles(CameraSeed::GRID).await;
    let ids = ["11", "12", "mock-0", "13"];
    set_camera_tiles(&ids, &["12"]);
    settle().await;
    set_camera_tiles(&ids, &[]);
    settle().await;

    el(&page, "#h-noop").click();
    settle().await;
    assert_eq!(
        NOOP_CALLS.with(|n| n.borrow().clone()),
        ["probe"],
        "issue 2125: a flip must not re-point the shared no-op at another handler"
    );

    set_camera_tiles(&ids, &["11"]);
    settle().await;
    el(&page, "#peer-video-11-div [data-testid='decode-play-btn']").click();
    settle().await;
    assert_eq!(DECODE_REQUESTS.with(|r| r.borrow().clone()), ["11"]);
    assert_eq!(NOOP_CALLS.with(|n| n.borrow().clone()), ["probe"]);
}

thread_local! {
    static SHARING_MODES: RefCell<Vec<TileMode>> = const { RefCell::new(Vec::new()) };
}

/// Peer 7 publishes a camera and a screen share, rendered in `SHARING_MODES`.
#[allow(non_snake_case)]
fn SharingPeer() -> Element {
    let client = use_hook(|| {
        let client = VideoCallClient::new_for_test("local-user");
        client.insert_peer_for_test(7, "carol");
        client.set_peer_media_for_test(7, true, true);
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
    let zoom = use_signal(HashMap::new);
    let detached = use_signal(|| None::<String>);
    let actual = use_signal(|| None::<String>);
    use_context_provider(|| ScreenZoomCtx(zoom));
    use_context_provider(|| DetachedShareCtx(detached));
    use_context_provider(|| ScreenActualSizeCtx(actual));
    let ctx = ShareViewCtx {
        slots: use_signal(ShareSlots::default),
        pins: use_signal(Vec::<PinnedTile>::new),
        detached,
        announce: use_signal(|| (String::new(), 0u32)),
        own_stream: use_signal(|| None::<web_sys::MediaStream>),
    };
    use_context_provider(|| ctx);
    let pin: EventHandler<PinnedTile> = use_callback(|_: PinnedTile| {});
    let decode: EventHandler<String> = use_callback(|_: String| {});
    let modes = SHARING_MODES.with(|m| m.borrow().clone());
    rsx! {
        for mode in modes {
            PeerTile {
                key: "{mode:?}",
                peer_id: "7".to_string(),
                render_mode: mode,
                on_toggle_pin: pin,
                on_request_decode: decode,
            }
        }
    }
}

#[wasm_bindgen_test]
async fn only_the_camera_tile_offers_the_crop_toggle() {
    for (modes, share) in [
        (
            vec![TileMode::ScreenOnly, TileMode::VideoOnly],
            ".share-tile[data-share-origin='received']",
        ),
        (vec![TileMode::Full], "#screen-share-7-div"),
    ] {
        fresh().await;
        inject_app_config();
        SHARING_MODES.with(|m| *m.borrow_mut() = modes.clone());
        let mount = marked_mount();
        render_into(&mount, SharingPeer);
        yield_now().await;
        let page = Page { mount };
        let crop_icons = |tile: &str| {
            el(&page, tile)
                .query_selector_all(".crop-icon")
                .unwrap()
                .length()
        };
        assert_eq!(
            crop_icons(share),
            0,
            "{modes:?}: shared content is always letterboxed, so a crop toggle does nothing"
        );
        assert_eq!(
            crop_icons("#peer-video-7-div"),
            1,
            "{modes:?}: the camera tile keeps its crop toggle"
        );
    }
}

// ---- stylesheet placement ------------------------------------------------

fn static_page(markup: &str) -> Page {
    reset_page();
    install_stylesheets(&[]);
    let mount = marked_mount();
    mount.set_inner_html(markup);
    Page { mount }
}

fn computed(page: &Page, selector: &str, property: &str) -> String {
    let el = page.mount.query_selector(selector).unwrap().unwrap();
    web_sys::window()
        .unwrap()
        .get_computed_style(&el)
        .unwrap()
        .unwrap()
        .get_property_value(property)
        .unwrap()
}

#[wasm_bindgen_test]
fn the_share_wrappers_only_lay_out_in_the_split() {
    let page = static_page(
        "<div id='grid-container' data-testid='g'>\
           <div class='ss-left-pane' style='--ss-ratio: 40%;'><span class='probe'></span></div>\
           <div class='screen-share-resize-handle'></div>\
           <div class='ss-peer-panel'></div>\
         </div>",
    );
    assert_eq!(computed(&page, ".ss-left-pane", "display"), "contents");
    assert_eq!(computed(&page, ".ss-peer-panel", "display"), "contents");
    assert_eq!(
        computed(&page, ".screen-share-resize-handle", "display"),
        "none"
    );
    assert_eq!(
        computed(&page, ".probe", "--ss-ratio").trim(),
        "66.7%",
        "a resize drag must not restyle the panel's whole subtree"
    );

    page.mount
        .query_selector("#grid-container")
        .unwrap()
        .unwrap()
        .set_class_name("has-screen-share");
    assert_eq!(computed(&page, ".ss-left-pane", "display"), "flex");
    assert_eq!(computed(&page, ".ss-peer-panel", "display"), "grid");
}

/// The production inline style of a share root.
fn share_root_style(origin: ShareOrigin, pin_rank: Option<usize>) -> String {
    ShareTileView {
        target: ShareTarget {
            origin,
            key: "k".into(),
            pin: PinnedTile::screen("k"),
            name: String::new(),
        },
        mode: share_view::ShareViewMode::Tile,
        cta: CtaState::Hidden,
        guard: false,
        pin_rank,
    }
    .root_style()
}

#[wasm_bindgen_test]
fn a_tile_share_in_the_peer_panel_lays_out_like_one_on_the_stage() {
    let page = static_page(&format!(
        "<div id='grid-container' data-tile-flow='wrap' \
              style='display: flex; flex-wrap: wrap; width: 900px; height: 600px; \
                     --tile-w: 210px; --tile-h: 140px;'>\
           <div class='ss-left-pane'>\
             <div class='split-screen-tile share-tile' data-share-origin='own' \
                  data-share-mode='tile' style='{}'></div>\
           </div>\
           <div class='screen-share-resize-handle'></div>\
           <div class='ss-peer-panel'>\
             <div class='split-screen-tile share-tile' data-share-origin='received' \
                  data-share-mode='tile' style='{}'></div>\
           </div>\
         </div>",
        share_root_style(ShareOrigin::Own, None),
        share_root_style(ShareOrigin::Received, None),
    ));
    for origin in ["own", "received"] {
        let sel = format!(".share-tile[data-share-origin='{origin}']");
        assert_eq!(computed(&page, &sel, "width"), "210px", "{origin} width");
        assert_eq!(computed(&page, &sel, "height"), "140px", "{origin} height");
    }
    let left = |origin: &str| {
        page.mount
            .query_selector(&format!(".share-tile[data-share-origin='{origin}']"))
            .unwrap()
            .unwrap()
            .get_bounding_client_rect()
            .left()
    };
    assert!(
        left("received") < left("own"),
        "the received share leads whichever wrapper holds it"
    );
}

#[wasm_bindgen_test]
fn a_detached_tile_stays_rendered_but_off_screen() {
    let page = static_page(
        "<div class='split-screen-tile share-tile' data-share-mode='detached'>\
           <div class='canvas-container'></div>\
         </div>",
    );
    assert_eq!(computed(&page, ".share-tile", "position"), "absolute");
    assert_eq!(computed(&page, ".share-tile", "left"), "-99999px");
    assert_ne!(
        computed(&page, ".share-tile", "display"),
        "none",
        "C5: display:none stalls captureStream for the detached mirror"
    );
}

#[wasm_bindgen_test]
fn a_pinned_share_keeps_the_tile_geometry_and_leads_by_rank() {
    let page = static_page(&format!(
        "<div id='grid-container' data-tile-flow='wrap' \
              style='display: flex; flex-wrap: wrap; width: 1200px; height: 600px; \
                     --tile-w: 210px; --tile-h: 140px;'>\
           <div class='ss-left-pane'>\
             <div class='split-screen-tile share-tile' data-share-origin='received' \
                  data-share-mode='tile' style='{}'></div>\
             <div class='split-screen-tile share-tile tile-pinned' data-share-origin='own' \
                  data-share-mode='pinned' style='{}'></div>\
           </div>\
           <div class='screen-share-resize-handle'></div>\
           <div class='ss-peer-panel'>\
             <div class='tile-slot'><div class='grid-item' id='cam-late' style='order: 0;'></div></div>\
             <div class='tile-slot'>\
               <div class='grid-item tile-pinned' id='cam-pinned' style='order: -1000;'></div>\
             </div>\
           </div>\
         </div>",
        share_root_style(ShareOrigin::Received, None),
        share_root_style(ShareOrigin::Own, Some(1)),
    ));
    let pinned = ".share-tile[data-share-mode='pinned']";
    assert_eq!(computed(&page, pinned, "width"), "210px");
    assert_eq!(computed(&page, pinned, "height"), "140px");
    assert_ne!(
        computed(&page, pinned, "position"),
        "fixed",
        "a pin no longer lays the tile over the others"
    );
    let left = |selector: &str| {
        page.mount
            .query_selector(selector)
            .unwrap()
            .unwrap()
            .get_bounding_client_rect()
            .left()
    };
    let lefts = [
        left("#cam-pinned"),
        left(pinned),
        left(".share-tile[data-share-origin='received']"),
        left("#cam-late"),
    ];
    assert!(
        lefts.windows(2).all(|w| w[0] < w[1]),
        "pins by rank, then the unpinned share, then the camera tiles: {lefts:?}"
    );
}

/// A tile whose icon row holds the mic, `extra`, then the pin.
fn phone_tile(id: &str, extra: &str) -> String {
    format!(
        "<div class='grid-item' id='{id}'><div class='canvas-container'>\
           <h4 class='floating-name'><span class='floating-name-text'>\
             A participant with a very long display name</span></h4>\
           <div class='tile-top-icons'>\
             <div class='audio-indicator'><svg></svg></div>{extra}\
             <button class='pin-icon'><svg></svg></button>\
           </div>\
         </div></div>"
    )
}

#[wasm_bindgen_test]
fn on_a_phone_each_name_ends_before_its_own_icon_row() {
    reset_page();
    install_stylesheets(&[("@media (pointer: coarse) {", "@media all {")]);
    let menu = "<div class='tile-mute-menu-wrapper'><button class='tile-mute-btn'></button></div>";
    let signal = "<button class='signal-indicator'></button>";
    let mount = marked_mount();
    // The tailwind preflight the page loads.
    mount.set_inner_html(&format!(
        "<style>*, ::before, ::after {{ box-sizing: border-box; }}</style>\
         <div id='grid-container' data-tile-flow='wrap' \
              style='display: flex; width: 600px; --tile-w: 159px; --tile-h: 106px;'>\
           {}{}{}\
         </div>",
        phone_tile("viewer", ""),
        phone_tile("host", menu),
        phone_tile("host-diag", &format!("{signal}{menu}")),
    ));
    let page = Page { mount };
    let rect = |selector: &str| {
        page.mount
            .query_selector(selector)
            .unwrap()
            .unwrap()
            .get_bounding_client_rect()
    };
    assert_eq!(
        computed(&page, "#host .pin-icon", "width"),
        "32px",
        "premise: phone pin"
    );
    for tile in ["viewer", "host", "host-diag"] {
        let name = rect(&format!("#{tile} .floating-name"));
        let icons = rect(&format!("#{tile} .tile-top-icons"));
        assert!(
            name.right() <= icons.left(),
            "{tile}: the name ({:.1}) must end before its icon row ({:.1})",
            name.right(),
            icons.left()
        );
    }
    let width = |tile: &str| rect(&format!("#{tile} .floating-name")).width();
    assert!(
        width("viewer") > width("host") + 20.0,
        "a non-host name keeps the room no host menu takes: {:.1} vs {:.1}",
        width("viewer"),
        width("host")
    );
    assert!(
        width("host") > width("host-diag") + 30.0,
        "only the tile showing the signal disc reserves it: {:.1} vs {:.1}",
        width("host"),
        width("host-diag")
    );
}

/// A touch-screen document `width` CSS px wide: every `(pointer: coarse)`
/// query is live and every width query is real.
fn touch_frame(width: u32, markup: &str) -> web_sys::Document {
    let frame = gloo_utils::document().create_element("iframe").unwrap();
    mark(&frame);
    frame
        .set_attribute(
            "style",
            &format!("width: {width}px; height: 400px; border: 0;"),
        )
        .unwrap();
    gloo_utils::document()
        .body()
        .unwrap()
        .append_child(&frame)
        .unwrap();
    let doc: web_sys::Document = js_sys::Reflect::get(&frame, &"contentDocument".into())
        .unwrap()
        .unchecked_into();
    let css = format!(
        "*, ::before, ::after {{ box-sizing: border-box; }}{}{}",
        include_str!("../static/style.css"),
        include_str!("../static/global.css"),
    )
    .replace("(pointer: coarse)", "(min-width: 0px)");
    let style = doc.create_element("style").unwrap();
    style.set_text_content(Some(&css));
    doc.head().unwrap().append_child(&style).unwrap();
    let body = doc.body().unwrap();
    body.set_attribute("style", "margin: 0; overflow: hidden;")
        .unwrap();
    body.set_inner_html(markup);
    doc
}

#[wasm_bindgen_test]
fn on_a_landscape_phone_or_a_tablet_each_name_ends_before_its_own_icon_row() {
    reset_page();
    let crop = "<button class='crop-icon'><svg></svg></button>";
    let menu = "<div class='tile-mute-menu-wrapper'><button class='tile-mute-btn'></button></div>";
    // A landscape phone at four and at six tiles, then a tablet.
    for (width, tile_w, tile_h) in [(844, 189, 126), (844, 151, 101), (1024, 240, 160)] {
        let doc = touch_frame(
            width,
            &format!(
                "<div id='grid-container' data-tile-flow='wrap' style='display: flex; \
                      flex-wrap: wrap; --tile-w: {tile_w}px; --tile-h: {tile_h}px;'>{}{}</div>",
                phone_tile("viewer", crop),
                phone_tile("host", &format!("{crop}{menu}")),
            ),
        );
        let rect = |selector: &str| {
            doc.query_selector(selector)
                .unwrap()
                .unwrap()
                .get_bounding_client_rect()
        };
        let at = format!("{width}px wide, {tile_w}x{tile_h}");
        assert_eq!(
            rect("#host").width().round() as u32,
            tile_w,
            "premise: {at} tile"
        );
        assert!(
            rect("#host .crop-icon").width() > 0.0,
            "premise: {at}: the hidden crop button keeps its slot"
        );
        for tile in ["viewer", "host"] {
            let name = rect(&format!("#{tile} .floating-name"));
            let icons = rect(&format!("#{tile} .tile-top-icons"));
            assert!(
                name.right() <= icons.left(),
                "{at} {tile}: the name ({:.1}) must end before its icon row ({:.1})",
                name.right(),
                icons.left()
            );
        }
    }
}

#[wasm_bindgen_test]
fn the_split_pin_pad_stops_short_of_the_host_menu() {
    let page = static_page(
        "<div class='split-peer-tile'><button class='pin-icon'></button></div>\
         <div class='grid-item'><button class='pin-icon'></button></div>",
    );
    let pad = |selector: &str| {
        let el = page.mount.query_selector(selector).unwrap().unwrap();
        web_sys::window()
            .unwrap()
            .get_computed_style_with_pseudo_elt(&el, "::after")
            .unwrap()
            .unwrap()
            .get_property_value("top")
            .unwrap()
    };
    assert_eq!(pad(".split-peer-tile .pin-icon"), "-3px");
    assert_eq!(pad(".grid-item .pin-icon"), "-6px");
}

#[wasm_bindgen_test]
fn forced_colors_keep_the_pressed_pin_distinct() {
    reset_page();
    install_stylesheets(&[("@media (forced-colors: active)", "@media all")]);
    let mount = marked_mount();
    mount.set_inner_html(
        "<button class='pin-icon' aria-pressed='true'><svg></svg></button>\
         <span class='tile-pin-badge'><svg></svg></span>\
         <button class='pin-icon' id='unpressed' aria-pressed='false'><svg></svg></button>\
         <span id='probe' style='background-color: Highlight; color: HighlightText;'></span>",
    );
    let page = Page { mount };
    let highlight = computed(&page, "#probe", "background-color");
    let highlight_text = computed(&page, "#probe", "color");
    assert_ne!(
        highlight, "rgb(0, 122, 255)",
        "premise: Highlight is not the accent"
    );
    for pressed in [".pin-icon[aria-pressed='true']", ".tile-pin-badge"] {
        assert_eq!(
            computed(&page, pressed, "background-color"),
            highlight,
            "{pressed}"
        );
        assert_eq!(
            computed(&page, &format!("{pressed} svg"), "fill"),
            highlight_text,
            "{pressed}"
        );
    }
    assert_ne!(
        computed(&page, "#unpressed", "background-color"),
        highlight,
        "an unpressed pin keeps its own disc"
    );
}

async fn boxed_tile(width: u32, rewrite: &[(&str, &str)]) -> Page {
    let page = mount_with(|| {
        install_pip_stub();
        install_stylesheets(rewrite);
        SEED_BOX.with(|b| b.set(Some(width)));
        SEED_CTA.with(|c| c.set(CtaState::Shown));
    })
    .await;
    settle().await;
    page
}

fn shown(page: &Page, testid: &str) -> bool {
    computed(page, &format!("[data-testid='{testid}']"), "display") != "none"
}

#[wasm_bindgen_test]
async fn the_real_bar_sheds_controls_by_the_tile_width() {
    for (width, zoom, reset, detach) in [
        (180, false, false, false),
        (260, true, false, true),
        (400, true, false, true),
        (600, true, true, true),
    ] {
        let page = boxed_tile(width, &[]).await;
        assert_eq!(shown(&page, "ss-zoom-in"), zoom, "{width}px zoom in");
        assert_eq!(shown(&page, "ss-zoom-out"), zoom, "{width}px zoom out");
        assert_eq!(shown(&page, "ss-zoom-reset"), reset, "{width}px reset");
        assert_eq!(shown(&page, "ss-detach"), detach, "{width}px detach");
        assert!(shown(&page, "ss-enlarge"), "{width}px enlarge");
        assert!(shown(&page, "ss-pin"), "{width}px pin");
        assert!(
            shown(&page, "ss-zoom-label"),
            "{width}px: the % label stays in the DOM, it is the zoom live region"
        );
    }
}

#[wasm_bindgen_test]
async fn a_narrow_cta_is_a_round_icon_button_with_a_ring() {
    let page = boxed_tile(180, &[]).await;
    let cta = "[data-testid='ss-detach-cta']";
    assert_eq!(computed(&page, cta, "border-top-left-radius"), "50%");
    assert_eq!(computed(&page, cta, "border-top-width"), "2px");
    assert_eq!(computed(&page, cta, "width"), "32px");
    assert_eq!(computed(&page, cta, "height"), "32px");
    assert_eq!(computed(&page, ".ss-detach-cta-text", "display"), "none");
    assert_eq!(
        attr(&page, cta, "aria-label").as_deref(),
        Some("Open in separate window"),
        "WCAG 2.5.3: the name contains the visible text"
    );
}

#[wasm_bindgen_test]
async fn the_preview_toggle_is_reachable_on_touch() {
    let page = boxed_tile(
        400,
        &[
            ("@media (hover: none)", "@media all"),
            ("@media (pointer: coarse)", "@media all"),
        ],
    )
    .await;
    let toggle = "[data-testid='ss-hide-preview']";
    assert_eq!(computed(&page, toggle, "opacity"), "0.9");
    assert_eq!(computed(&page, toggle, "pointer-events"), "auto");
    assert_eq!(computed(&page, toggle, "width"), "44px");
}
