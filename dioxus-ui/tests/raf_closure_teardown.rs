// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Freeing a `Closure` while a `requestAnimationFrame` registered with it is still
// pending makes the browser call the freed function, which throws "closure invoked
// recursively or after being dropped" as an uncaught window error.
// MUTATION: make `AnimationFrame`'s `Drop` skip `cancel_animation_frame` and every
// test here fails on the recorded window error.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use dioxus::prelude::*;
use dioxus_ui::components::animation_frame::AnimationFrame;
use dioxus_ui::components::canvas_generator::ScreenShareZoomable;
use dioxus_ui::components::performance_settings::receive::ReceivePreference;
use dioxus_ui::components::performance_settings::{
    PerformancePreference, PerformanceSettingsPanel,
};
use dioxus_ui::context::{ScreenActualSizeCtx, ScreenZoomCtx, ScreenZoomState};
use videocall_client::VideoCallClient;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

mod support;
use support::{cleanup, create_mount_point, inject_app_config, render_into, yield_now};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const DROPPED_CLOSURE: &str = "closure invoked recursively or after being dropped";

/// Longer than both drivers' 250 ms DOM-write throttle.
const SETTLE_MS: f64 = 600.0;

fn record_window_errors() {
    js_sys::eval(
        r#"
        window.__rafTeardownErrors = [];
        if (!window.__rafTeardownListener) {
            window.__rafTeardownListener = function (e) {
                window.__rafTeardownErrors.push(String(e.message));
            };
            window.addEventListener('error', window.__rafTeardownListener);
        }
        "#,
    )
    .expect("failed to install the window error recorder");
}

fn dropped_closure_errors() -> Vec<String> {
    let errors = js_sys::eval("window.__rafTeardownErrors").unwrap();
    js_sys::Array::from(&errors)
        .iter()
        .filter_map(|e| e.as_string())
        .filter(|e| e.contains(DROPPED_CLOSURE))
        .collect()
}

fn now_ms() -> f64 {
    gloo_utils::window().performance().unwrap().now()
}

async fn settle() {
    let start = now_ms();
    while now_ms() - start < SETTLE_MS {
        yield_now().await;
    }
}

#[wasm_bindgen_test]
async fn dropping_a_pending_one_shot_frame_never_invokes_the_freed_closure() {
    record_window_errors();
    let runs = Rc::new(Cell::new(0u32));

    let frame = AnimationFrame::new({
        let runs = runs.clone();
        move || runs.set(runs.get() + 1)
    });
    frame.request();
    frame.request();
    yield_now().await;
    assert_eq!(
        runs.get(),
        1,
        "a requested frame must run exactly once, even when requested twice"
    );

    frame.request();
    frame.request();
    drop(frame);
    settle().await;

    assert_eq!(dropped_closure_errors(), Vec::<String>::new());
    assert_eq!(runs.get(), 1, "a cancelled frame must not run");
}

#[wasm_bindgen_test]
async fn dropping_a_running_loop_cancels_its_rescheduled_frame() {
    record_window_errors();
    let ticks = Rc::new(Cell::new(0u32));

    let frame = AnimationFrame::new_loop({
        let ticks = ticks.clone();
        move || ticks.set(ticks.get() + 1)
    });
    frame.request();
    yield_now().await;
    yield_now().await;
    assert!(
        ticks.get() >= 2,
        "positive control: the loop must reschedule itself, ticked {}",
        ticks.get()
    );

    drop(frame);
    let ticks_at_drop = ticks.get();
    settle().await;

    assert_eq!(dropped_closure_errors(), Vec::<String>::new());
    assert_eq!(
        ticks.get(),
        ticks_at_drop,
        "the loop must stop once dropped"
    );
}

const TOGGLE: &str = "[data-testid='perf-panel-unmount']";
const SEND_METER_ID: &str = "perf-meter-audio";
const RECV_METER_ID: &str = "perf-meter-recv-audio";

#[allow(non_snake_case)]
fn PanelHost() -> Element {
    let mut mounted = use_signal(|| true);
    rsx! {
        button {
            "data-testid": "perf-panel-unmount",
            onclick: move |_| mounted.set(false),
            "unmount"
        }
        if mounted() {
            PerformanceSettingsPanel {
                pref: PerformancePreference::default(),
                on_change: move |_| {},
                receive_pref: ReceivePreference::default(),
                on_receive_change: move |_| {},
            }
        }
    }
}

fn by_id(id: &str) -> Option<web_sys::Element> {
    gloo_utils::document().get_element_by_id(id)
}

/// A bare node carrying a driver's meter id: a loop still running writes
/// `data-level` onto it.
fn sentinel(id: &str) -> web_sys::Element {
    let el = gloo_utils::document().create_element("div").unwrap();
    el.set_id(id);
    gloo_utils::document()
        .body()
        .unwrap()
        .append_child(&el)
        .unwrap();
    el
}

#[wasm_bindgen_test]
async fn unmounting_the_performance_panel_stops_both_meter_drivers_cleanly() {
    inject_app_config();
    record_window_errors();
    let mount = create_mount_point();
    render_into(&mount, PanelHost);

    for _ in 0..40 {
        if by_id(SEND_METER_ID).is_some() && by_id(RECV_METER_ID).is_some() {
            break;
        }
        yield_now().await;
    }
    assert!(
        by_id(SEND_METER_ID).is_some() && by_id(RECV_METER_ID).is_some(),
        "positive control: both drivers' meters must have mounted"
    );
    settle().await;

    mount
        .query_selector(TOGGLE)
        .unwrap()
        .expect("the harness toggle must be rendered")
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap()
        .click();
    for _ in 0..40 {
        if by_id(SEND_METER_ID).is_none() {
            break;
        }
        yield_now().await;
    }
    assert!(
        by_id(SEND_METER_ID).is_none() && by_id(RECV_METER_ID).is_none(),
        "positive control: the panel must actually unmount"
    );

    let sentinels = [sentinel(SEND_METER_ID), sentinel(RECV_METER_ID)];
    settle().await;

    assert_eq!(dropped_closure_errors(), Vec::<String>::new());
    for s in &sentinels {
        assert_eq!(
            s.get_attribute("data-level"),
            None,
            "a driver loop for #{} kept running after unmount",
            s.id()
        );
        s.remove();
    }
    cleanup(&mount);
}

const PRESENTER: &str = "presenter";
const VIEWPORT: &str = "[data-testid='ss-zoom-viewport']";

#[allow(non_snake_case)]
fn ZoomableHost() -> Element {
    let client = use_hook(|| VideoCallClient::new_for_test("local-user"));
    use_context_provider(|| client.clone());
    let zoomed = ScreenZoomState {
        scale: 2.0,
        off_x: 0.0,
        off_y: 0.0,
    };
    let zoom = use_signal(|| HashMap::from([(PRESENTER.to_string(), zoomed)]));
    use_context_provider(|| ScreenZoomCtx(zoom));
    let actual = use_signal(|| None::<String>);
    use_context_provider(|| ScreenActualSizeCtx(actual));

    let mut mounted = use_signal(|| true);
    rsx! {
        button {
            "data-testid": "zoomable-unmount",
            onclick: move |_| mounted.set(false),
            "unmount"
        }
        if mounted() {
            ScreenShareZoomable { peer_id: PRESENTER.to_string() }
        }
    }
}

fn dispatch_pointer(kind: &str, client_x: i32) {
    js_sys::eval(&format!(
        "document.querySelector(\"{VIEWPORT}\").dispatchEvent(new PointerEvent('{kind}', \
         {{ bubbles: true, pointerId: 1, isPrimary: true, clientX: {client_x}, clientY: 40 }}))"
    ))
    .expect("failed to dispatch a pointer event");
}

fn wrapper_style(mount: &web_sys::Element) -> Option<String> {
    mount
        .query_selector(".ss-zoom-wrapper")
        .unwrap()
        .and_then(|w| w.get_attribute("style"))
}

async fn microtask() {
    JsFuture::from(js_sys::Promise::resolve(&JsValue::UNDEFINED))
        .await
        .unwrap();
}

#[wasm_bindgen_test]
async fn unmounting_a_zoomed_share_tile_mid_pan_never_invokes_the_freed_closure() {
    inject_app_config();
    record_window_errors();
    let mount = create_mount_point();
    render_into(&mount, ZoomableHost);
    for _ in 0..40 {
        if mount.query_selector(VIEWPORT).unwrap().is_some() {
            break;
        }
        yield_now().await;
    }
    let before = wrapper_style(&mount);
    assert!(before.is_some(), "positive control: the tile must mount");

    dispatch_pointer("pointerdown", 100);
    dispatch_pointer("pointermove", 130);
    yield_now().await;
    yield_now().await;
    assert_ne!(
        wrapper_style(&mount),
        before,
        "positive control: a pan must schedule a frame that moves the content"
    );

    js_sys::eval("window.__zoomFrameFired = false; requestAnimationFrame(() => { window.__zoomFrameFired = true; })")
        .unwrap();
    dispatch_pointer("pointermove", 160);
    mount
        .query_selector("[data-testid='zoomable-unmount']")
        .unwrap()
        .expect("the harness toggle must be rendered")
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap()
        .click();
    for _ in 0..200 {
        if mount.query_selector(VIEWPORT).unwrap().is_none() {
            break;
        }
        microtask().await;
    }
    assert!(
        mount.query_selector(VIEWPORT).unwrap().is_none(),
        "positive control: the tile must unmount"
    );
    assert_eq!(
        js_sys::eval("window.__zoomFrameFired").unwrap(),
        JsValue::FALSE,
        "positive control: the tile must unmount before the pan frame is due"
    );

    settle().await;
    assert_eq!(dropped_closure_errors(), Vec::<String>::new());
    cleanup(&mount);
}
