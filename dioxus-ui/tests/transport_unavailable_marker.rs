// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use dioxus::prelude::*;
use dioxus_ui::components::device_settings_modal::DeviceSettingsModal;
use dioxus_ui::components::diagnostics::Diagnostics;
use dioxus_ui::components::media_metrics_overlay::MediaMetricsOverlayCtx;
use dioxus_ui::context::{TransportPreference, TransportPreferenceCtx};
use support::{cleanup, create_mount_point, render_into, yield_now};
use videocall_client::VideoCallClient;
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

fn text_of(mount: &web_sys::Element, selector: &str) -> String {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
        .text_content()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn attr_of(mount: &web_sys::Element, selector: &str, name: &str) -> Option<String> {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
        .get_attribute(name)
}

fn checked_of(mount: &web_sys::Element, selector: &str) -> bool {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
        .dyn_into::<web_sys::HtmlInputElement>()
        .unwrap()
        .checked()
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

#[allow(non_snake_case)]
fn ModalParent() -> Element {
    rsx! {
        DeviceSettingsModal {
            microphones: Vec::<web_sys::MediaDeviceInfo>::new(),
            cameras: Vec::<web_sys::MediaDeviceInfo>::new(),
            speakers: Vec::<web_sys::MediaDeviceInfo>::new(),
            selected_microphone_id: None::<String>,
            selected_camera_id: None::<String>,
            selected_speaker_id: None::<String>,
            on_microphone_select: move |_| {},
            on_camera_select: move |_| {},
            on_speaker_select: move |_| {},
            visible: true,
            transport_preference: TransportPreference::WebTransport,
            on_close: move |_| {},
        }
    }
}

#[allow(non_snake_case)]
fn DiagnosticsParent() -> Element {
    let client = use_hook(|| VideoCallClient::new_for_test("local-user"));
    use_context_provider(|| client.clone());
    let transport = use_signal(|| TransportPreference::WebTransport);
    use_context_provider(|| TransportPreferenceCtx(transport));
    let overlay_on = use_signal(|| false);
    use_context_provider(|| MediaMetricsOverlayCtx(overlay_on));

    let noop: EventHandler<()> = use_callback(|_| {});
    let noop_f64: EventHandler<f64> = use_callback(|_: f64| {});
    rsx! {
        Diagnostics {
            is_open: true,
            on_close: noop,
            video_enabled: true,
            mic_enabled: true,
            share_screen: false,
            encoder_settings: None,
            width: 420.0,
            on_resize_start: noop,
            on_resize_move: noop_f64,
            on_resize_end: noop,
        }
    }
}

/// Fails on the UNCLAMPED default: the labels would read "WebTransport
/// (default)" / "WebSocket" on a client that cannot use WebTransport.
#[wasm_bindgen_test]
async fn modal_marks_websocket_and_flags_webtransport_unavailable() {
    support::inject_app_config_webtransport_disabled();
    let mount = create_mount_point();
    render_into(&mount, ModalParent);
    yield_now().await;

    click(&mount, "[data-testid='settings-nav-network']");
    yield_now().await;

    let wt = "[data-testid='transport-radio-webtransport']";
    let ws = "[data-testid='transport-radio-websocket']";
    let wt_label = text_of(&mount, wt);
    let ws_label = text_of(&mount, ws);
    let wt_aria_disabled = attr_of(&mount, wt, "aria-disabled");
    let wt_native_disabled = attr_of(&mount, wt, "disabled");
    let wt_checked = attr_of(&mount, wt, "aria-checked");
    let desc = text_of(&mount, "#transport-webtransport-desc");
    support::remove_app_config();
    cleanup(&mount);

    assert_eq!(wt_label, "WebTransport (unavailable)");
    assert_eq!(ws_label, "WebSocket (default)");
    assert_eq!(wt_aria_disabled.as_deref(), Some("true"));
    assert_eq!(
        wt_native_disabled, None,
        "the radio carries the group's checked state, so it must stay in the tab \
         order: aria-disabled only, never the native attribute"
    );
    assert_eq!(
        wt_checked.as_deref(),
        Some("true"),
        "an unclamped stored preference still checks this radio — which is why it \
         has to remain focusable"
    );
    assert_eq!(desc, "Unavailable on this deployment.");
}

/// Fails on the UNCLAMPED default: the advisory would offer WebTransport,
/// which the same panel marks unavailable.
#[wasm_bindgen_test]
async fn pinned_advisory_absent_when_the_pin_is_the_marked_default() {
    support::reset_test_browser_state();
    support::inject_app_config_webtransport_disabled();
    let mount = create_mount_point();
    render_into(&mount, ModalParent);
    yield_now().await;

    click(&mount, "[data-testid='settings-nav-network']");
    yield_now().await;
    click(&mount, "[data-testid='transport-radio-websocket']");
    yield_now().await;
    click(&mount, "#sticky-transport-checkbox");
    yield_now().await;

    let advisory_sel = "[data-testid='transport-pinned-advisory']";
    let rendered = mount.query_selector(advisory_sel).unwrap().is_some();
    let advisory = if rendered {
        text_of(&mount, advisory_sel)
    } else {
        String::new()
    };
    let checkbox_on = checked_of(&mount, "#sticky-transport-checkbox");
    support::remove_app_config();
    cleanup(&mount);

    assert!(
        checkbox_on,
        "Remember must actually be on, or the gate is unreached and this asserts nothing"
    );
    assert!(
        !rendered,
        "WebSocket IS the marked default here, so there is nothing to advise: {advisory}"
    );
}

/// Fails on the UNCLAMPED default: a WebTransport pin equals it, so no
/// advisory would render at all.
#[wasm_bindgen_test]
async fn pinned_advisory_renders_against_the_marked_default_when_wt_is_pinned() {
    support::reset_test_browser_state();
    support::inject_app_config_webtransport_disabled();
    let mount = create_mount_point();
    render_into(&mount, ModalParent);
    yield_now().await;

    click(&mount, "[data-testid='settings-nav-network']");
    yield_now().await;
    click(&mount, "#sticky-transport-checkbox");
    yield_now().await;

    let advisory_sel = "[data-testid='transport-pinned-advisory']";
    let rendered = mount.query_selector(advisory_sel).unwrap().is_some();
    let advisory = if rendered {
        text_of(&mount, advisory_sel)
    } else {
        String::new()
    };
    support::remove_app_config();
    cleanup(&mount);

    assert!(
        rendered,
        "the pin differs from the marked default, so the advisory must render"
    );
    assert!(
        advisory.contains("WebTransport will be used"),
        "advisory should name the pinned protocol: {advisory}"
    );
    assert!(
        advisory.contains("switch back to WebSocket"),
        "advisory should offer the marked default: {advisory}"
    );
}

/// The call site the modal test cannot reach.
#[wasm_bindgen_test]
async fn diagnostics_select_marks_websocket_and_flags_webtransport_unavailable() {
    support::inject_app_config_webtransport_disabled();
    let mount = create_mount_point();
    render_into(&mount, DiagnosticsParent);
    yield_now().await;

    let wt_label = text_of(
        &mount,
        "#diagnostics-transport-select option[value='webtransport']",
    );
    let ws_label = text_of(
        &mount,
        "#diagnostics-transport-select option[value='websocket']",
    );
    support::remove_app_config();
    cleanup(&mount);

    assert_eq!(wt_label, "WebTransport (unavailable)");
    assert_eq!(ws_label, "WebSocket (default)");
}
