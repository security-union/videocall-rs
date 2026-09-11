// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// The WT/WS pill took ONE generic token for both transports, in the signal
// popup and in the diagnostics drawer, so the two rendered identically while
// the tile badge painted WT blue and WS amber.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use dioxus::prelude::*;
use dioxus_ui::components::diagnostics::ConnectionManagerDisplay;
use dioxus_ui::components::signal_quality::SignalQualityPopup;
use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const GLOBAL_CSS: &str = include_str!("../static/global.css");
const STYLE_CSS: &str = include_str!("../static/style.css");
const DIAGNOSTICS_RS: &str = include_str!("../src/components/diagnostics.rs");

fn cm_events(transport: &str) -> String {
    format!(
        r#"[{{"subsystem":"connection_manager","stream_id":null,"ts_ms":0,"metrics":[
             {{"name":"election_state","value":{{"t":"Text","v":"elected"}}}},
             {{"name":"active_server_type","value":{{"t":"Text","v":"{transport}"}}}}]}},
           {{"subsystem":"connection_manager","stream_id":"c-{transport}","ts_ms":0,"metrics":[
             {{"name":"server_type","value":{{"t":"Text","v":"{transport}"}}}}]}}]"#
    )
}

/// Every surface the pill reaches. Popup and drawer pills come from real
/// components; the `.peer-summary-item` row is pinned against the source below.
fn pills() -> Element {
    rsx! {
        SignalQualityPopup {
            peer_id: "ws".to_string(),
            peer_name: "Ada".to_string(),
            history: Vec::new(),
            meeting_start_ms: 0.0,
            transport: Some("websocket".to_string()),
            anchor_id: "absent".to_string(),
            on_close: move |_| {},
        }
        SignalQualityPopup {
            peer_id: "wt".to_string(),
            peer_name: "Ada".to_string(),
            history: Vec::new(),
            meeting_start_ms: 0.0,
            transport: Some("webtransport".to_string()),
            anchor_id: "absent".to_string(),
            on_close: move |_| {},
        }
        span { id: "badge-ws", class: "transport-badge transport-badge--ws", "WS" }
        span { id: "badge-wt", class: "transport-badge transport-badge--wt", "WT" }
        div { id: "cm-ws", ConnectionManagerDisplay { connection_manager_state: cm_events("websocket") } }
        div { id: "cm-wt", ConnectionManagerDisplay { connection_manager_state: cm_events("webtransport") } }
        div { class: "peer-summary-item",
            div { class: "peer-summary-item__metrics",
                span { id: "row-ws", class: "connection-type type-websocket", "WS" }
                span { id: "row-wt", class: "connection-type type-webtransport", "WT" }
            }
        }
    }
}

fn document() -> web_sys::Document {
    web_sys::window().unwrap().document().unwrap()
}

fn add_sheet(css: &str) -> web_sys::Element {
    let style = document().create_element("style").unwrap();
    style.set_text_content(Some(css));
    document().head().unwrap().append_child(&style).unwrap();
    style
}

/// Selector of the shipped rule declaring the WS pill fill, read out of
/// `style.css` so a broadened one is caught rather than restated as a literal.
fn shipped_ws_selector(css: &str) -> &str {
    let decl = "var(--transport-badge-ws-bg)";
    assert_eq!(
        css.matches(decl).count(),
        1,
        "expected exactly one rule to set {decl}"
    );
    let head = &css[..css.find(decl).unwrap()];
    let open = head.rfind('{').expect("a rule body");
    let start = head[..open].rfind('}').map_or(0, |i| i + 1);
    head[start..open].trim()
}

fn paint(el: &web_sys::Element) -> (String, String, String) {
    let cs = web_sys::window()
        .unwrap()
        .get_computed_style(el)
        .unwrap()
        .unwrap();
    let get = |p: &str| cs.get_property_value(p).unwrap();
    (
        get("background-color"),
        get("color"),
        get("border-top-color"),
    )
}

#[wasm_bindgen_test]
async fn every_transport_pill_paints_the_tile_badge_colours() {
    for class in [
        "peer-summary-item__metrics",
        "connection-type type-websocket",
        "connection-type type-webtransport",
    ] {
        assert!(
            DIAGNOSTICS_RS.contains(class),
            "the drawer no longer renders `{class}`, so this test guards nothing"
        );
    }

    let global = add_sheet(GLOBAL_CSS);
    let style = add_sheet(STYLE_CSS);

    let mount = create_mount_point();
    render_into(&mount, pills);
    yield_now().await;

    let one = |sel: &str| {
        mount
            .query_selector(sel)
            .unwrap()
            .unwrap_or_else(|| panic!("no {sel}"))
    };
    let badge_ws = one("#badge-ws.transport-badge--ws");
    let badge_wt = one("#badge-wt.transport-badge--wt");
    let surfaces = [
        (
            "popup",
            one(".signal-quality-popup .type-websocket"),
            one(".signal-quality-popup .type-webtransport"),
        ),
        (
            "active-connection",
            one("#cm-ws .detail-value.type-websocket"),
            one("#cm-wt .detail-value.type-webtransport"),
        ),
        ("peer-summary row", one("#row-ws"), one("#row-wt")),
    ];
    let root = document().document_element().unwrap();
    for theme in ["dark", "light"] {
        root.set_attribute("data-theme", theme).unwrap();

        for (where_, ws, wt) in &surfaces {
            assert_ne!(
                paint(ws),
                paint(wt),
                "{theme}/{where_}: WS and WT paint the same, so the pill carries no transport"
            );
            assert_eq!(
                paint(ws),
                paint(&badge_ws),
                "{theme}/{where_}: the WS pill drifted off the tile badge's WS palette"
            );
            assert_eq!(
                paint(wt),
                paint(&badge_wt),
                "{theme}/{where_}: the WT pill drifted off the tile badge's WT palette"
            );
        }
    }

    root.remove_attribute("data-theme").unwrap();
    style.remove();
    global.remove();
    cleanup(&mount);
}

/// Its own test so a broadened selector and the `color` regression each report,
/// instead of whichever assertion the run reaches first.
#[wasm_bindgen_test]
async fn the_pill_rules_stay_off_the_servers_list_chip() {
    let mount = create_mount_point();
    render_into(&mount, pills);
    yield_now().await;

    // Computed style cannot see this: `.server-type` sets all three properties
    // and wins on document order either way, masking a broadened selector.
    let ws_rule = shipped_ws_selector(STYLE_CSS);
    let one = |sel: &str| {
        mount
            .query_selector(sel)
            .unwrap()
            .unwrap_or_else(|| panic!("no {sel}"))
    };
    assert!(
        one("#row-ws").matches(ws_rule).unwrap(),
        "the drawer pill fell outside `{ws_rule}`"
    );
    assert!(
        !one("#cm-ws .server-type.type-websocket")
            .matches(ws_rule)
            .unwrap(),
        "`{ws_rule}` reaches the servers-list chip, which owns its chrome"
    );

    cleanup(&mount);
}
