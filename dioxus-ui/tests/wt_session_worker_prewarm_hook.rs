// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use dioxus::prelude::*;
use dioxus_ui::context::{
    use_wt_session_worker_prewarm, TransportPreference, TransportPreferenceCtx,
};
use support::{
    create_mount_point, inject_app_config_webtransport_disabled, inject_app_config_webtransport_on,
    render_into, yield_now,
};
use videocall_client::WtSessionWorkerLease;
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[component]
fn Prewarmer(ready: bool) -> Element {
    use_wt_session_worker_prewarm(ready);
    rsx! {}
}

#[allow(non_snake_case)]
fn Harness() -> Element {
    let transport = use_signal(|| TransportPreference::WebTransport);
    use_context_provider(|| TransportPreferenceCtx(transport));
    let mut ready = use_signal(|| false);
    let mut mounted = use_signal(|| true);
    rsx! {
        button { id: "ready", onclick: move |_| ready.set(true) }
        button { id: "unmount", onclick: move |_| mounted.set(false) }
        if mounted() {
            Prewarmer { ready: ready() }
        }
    }
}

fn click(mount: &web_sys::Element, selector: &str) {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap()
        .click();
}

#[wasm_bindgen_test]
async fn the_hook_holds_a_lease_only_from_ready_until_unmount() {
    inject_app_config_webtransport_on();
    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;
    assert_eq!(WtSessionWorkerLease::held_count(), 0, "not ready yet");

    click(&mount, "#ready");
    yield_now().await;
    assert_eq!(WtSessionWorkerLease::held_count(), 1);

    click(&mount, "#unmount");
    yield_now().await;
    assert_eq!(WtSessionWorkerLease::held_count(), 0, "unmount releases");
    support::cleanup(&mount);
}

#[wasm_bindgen_test]
async fn the_hook_holds_no_lease_when_webtransport_would_not_be_attempted() {
    inject_app_config_webtransport_disabled();
    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;

    click(&mount, "#ready");
    yield_now().await;
    assert_eq!(WtSessionWorkerLease::held_count(), 0);

    click(&mount, "#unmount");
    yield_now().await;
    support::cleanup(&mount);
}
