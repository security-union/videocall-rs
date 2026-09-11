// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 2661: the signal disc's 1 Hz imperative repaint. It writes the DOM
// directly, so no host test can reach it — disabling every repaint in the app
// left 1249/1249 host tests passing.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;

use dioxus::prelude::*;
use dioxus_ui::components::icons::signal_spark::SignalSparkIcon;
use dioxus_ui::components::peer_tile::refresh_peer_disc;
use dioxus_ui::components::signal_quality::{
    PeerSignalHistory, SampleData, SignalLevel, SparkPaint,
};
use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const PEER: &str = "peer-session-1";

/// A disc in the shape `canvas_generator` mounts one.
fn disc() -> Element {
    rsx! {
        button {
            class: "signal-indicator",
            "data-testid": "peer-signal-indicator",
            "data-signal-state": "measuring",
            "data-signal-level": "0",
            "data-signal-lost": "false",
            "data-signal-samples": "0",
            "aria-label": "mounted",
            title: "mounted",
            SignalSparkIcon {
                paint: SparkPaint::default(),
                spark_id: PEER.to_string(),
            }
        }
    }
}

fn history_of(qualities: &[f64]) -> PeerSignalHistory {
    let mut history = PeerSignalHistory::new();
    for (i, q) in qualities.iter().enumerate() {
        let data = SampleData {
            audio_expand_rate: (1.0 - q) * 1000.0,
            audio_buffer_ms: 100.0,
            audio_enabled: true,
            latency_ms: 212.0,
            ..SampleData::default()
        };
        history.push_sample_at(&data, 1_000.0 + i as f64 * 1_000.0);
    }
    history
}

fn button_of(mount: &web_sys::Element) -> web_sys::Element {
    mount
        .query_selector("button.signal-indicator")
        .unwrap()
        .expect("the disc mounted")
}

#[wasm_bindgen_test]
async fn refresh_writes_every_hook_onto_the_mounted_disc() {
    let mount = create_mount_point();
    render_into(&mount, disc);
    yield_now().await;

    let history = history_of(&[0.2, 0.2, 0.2, 0.2, 0.2]);
    let paint = history.spark_paint(SignalLevel::Bad, true, false, false, true, false);
    let last = RefCell::new(None);
    refresh_peer_disc(PEER, "Ada", false, &paint, &last);

    let btn = button_of(&mount);
    let attr = |name: &str| btn.get_attribute(name).unwrap_or_default();

    assert_eq!(attr("data-signal-state"), "measured");
    assert_eq!(attr("data-signal-level"), "1");
    assert_eq!(attr("data-signal-lost"), "false");
    assert_eq!(attr("data-signal-samples"), "5");
    assert_eq!(
        attr("aria-label"),
        "Ada connection: bad. Show signal quality details."
    );
    assert!(
        attr("title").contains("212 ms"),
        "the reading lives in the title: {}",
        attr("title")
    );

    let spark = btn
        .query_selector(".signal-spark polyline:not(.spark-halo)")
        .unwrap()
        .expect("the trend was plotted");
    assert!(!spark.get_attribute("points").unwrap_or_default().is_empty());
    assert_eq!(
        spark.get_attribute("stroke").as_deref(),
        Some(SignalLevel::Bad.level_color()),
        "the TREND carries the level colour now, not a fixed white"
    );
    let halo = btn
        .query_selector(".signal-spark polyline.spark-halo")
        .unwrap()
        .expect("the trend rides on a halo");
    assert_eq!(
        halo.get_attribute("points"),
        spark.get_attribute("points"),
        "the halo must trace the same run, or it drifts off the trend"
    );

    assert_eq!(
        btn.query_selector_all(".signal-spark line.spark-grid")
            .unwrap()
            .length(),
        0,
        "the background grid is gone"
    );
    assert_eq!(
        btn.query_selector_all(".signal-spark line")
            .unwrap()
            .length(),
        0,
        "a measured disc draws no <line> at all now"
    );

    // Generic: `circle` is what a re-added dot OR ring must use.
    assert!(
        btn.query_selector("circle").unwrap().is_none(),
        "no circle may be rendered: the head dot and the ring arc are both gone"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn an_unchanged_paint_touches_nothing() {
    let mount = create_mount_point();
    render_into(&mount, disc);
    yield_now().await;

    let history = history_of(&[0.9, 0.9, 0.9, 0.9, 0.9]);
    let paint = history.spark_paint(SignalLevel::Good, true, false, false, true, false);
    let last = RefCell::new(None);
    refresh_peer_disc(PEER, "Ada", false, &paint, &last);

    // Tampering must survive a second refresh with the SAME paint — proving it
    // did not write, rather than that it wrote the same value.
    let btn = button_of(&mount);
    btn.set_attribute("data-signal-samples", "sentinel")
        .unwrap();
    refresh_peer_disc(PEER, "Ada", false, &paint, &last);
    assert_eq!(
        btn.get_attribute("data-signal-samples").unwrap_or_default(),
        "sentinel",
        "an unchanged paint must skip the write entirely"
    );

    let worse = history.spark_paint(SignalLevel::Bad, true, false, false, true, false);
    refresh_peer_disc(PEER, "Ada", false, &worse, &last);
    assert_eq!(
        btn.get_attribute("data-signal-samples").unwrap_or_default(),
        "5"
    );

    cleanup(&mount);
}

/// Off-budget tiles dominate a large meeting and their markup is byte-identical
/// every tick, so this is a correctness property, not a timing one.
#[wasm_bindgen_test]
async fn an_unmeasured_peer_never_repaints() {
    let mount = create_mount_point();
    render_into(&mount, disc);
    yield_now().await;

    let history = history_of(&[0.9, 0.9, 0.9, 0.9, 0.9]);
    let last = RefCell::new(None);
    let paint = || history.spark_paint(SignalLevel::Unmeasured, true, true, false, false, false);

    refresh_peer_disc(PEER, "Ada", false, &paint(), &last);
    let btn = button_of(&mount);
    assert_eq!(
        btn.get_attribute("data-signal-state").unwrap_or_default(),
        "unmeasured"
    );

    btn.set_attribute("data-signal-state", "sentinel").unwrap();
    for _ in 0..5 {
        refresh_peer_disc(PEER, "Ada", false, &paint(), &last);
    }
    assert_eq!(
        btn.get_attribute("data-signal-state").unwrap_or_default(),
        "sentinel",
        "an unmeasured peer must cost zero DOM writes per tick"
    );

    cleanup(&mount);
}

/// The two discs open different popups, so one refresh must not touch the other.
#[wasm_bindgen_test]
async fn the_screen_disc_is_refreshed_apart_from_the_camera_disc() {
    let mount = create_mount_point();
    render_into(&mount, || {
        rsx! {
            button {
                class: "signal-indicator",
                id: "camera",
                "aria-label": "mounted",
                SignalSparkIcon {
                    paint: SparkPaint::default(),
                    spark_id: PEER.to_string(),
                }
            }
            button {
                class: "signal-indicator",
                id: "screen",
                "aria-label": "mounted",
                SignalSparkIcon {
                    paint: SparkPaint::default(),
                    spark_id: format!("{PEER}:screen"),
                }
            }
        }
    });
    yield_now().await;

    let history = history_of(&[0.9, 0.9, 0.9, 0.9, 0.9]);
    let paint = history.spark_paint(SignalLevel::Good, true, false, false, true, false);

    let camera_last = RefCell::new(None);
    refresh_peer_disc(PEER, "Ada", false, &paint, &camera_last);

    let camera = mount.query_selector("#camera").unwrap().unwrap();
    let screen = mount.query_selector("#screen").unwrap().unwrap();
    assert_eq!(
        camera.get_attribute("aria-label").unwrap_or_default(),
        "Ada connection: good. Show signal quality details."
    );
    assert_eq!(
        screen.get_attribute("aria-label").unwrap_or_default(),
        "mounted",
        "the camera refresh must not reach the screen disc"
    );

    let screen_last = RefCell::new(None);
    refresh_peer_disc(&format!("{PEER}:screen"), "Ada", true, &paint, &screen_last);
    assert_eq!(
        screen.get_attribute("aria-label").unwrap_or_default(),
        "Ada screen share connection: good. Show screen-share signal quality details."
    );

    cleanup(&mount);
}

const SHIPPED_CSS: &str = include_str!("../static/style.css");

/// The shipped forced-colors rules that reach a stroked mark. The line rule
/// went with the reference line: nothing but halos and the slash are lines now.
const FORCED_COLORS_RULES: [&str; 2] = [
    ".signal-spark svg polyline",
    ".signal-spark svg .spark-halo",
];

fn css_rule_body<'a>(css: &'a str, selector: &str) -> &'a str {
    let needle = format!("{selector} {{");
    let at = css
        .find(&needle)
        .unwrap_or_else(|| panic!("style.css no longer declares `{selector}`"));
    let rest = &css[at + needle.len()..];
    &rest[..rest.find('}').expect("a closed rule body")]
}

/// `forced-colors` cannot be turned on from a test, so this re-declares the
/// SHIPPED rules outside the query and reads back what the cascade resolved.
#[wasm_bindgen_test]
async fn the_forced_colors_repaint_skips_every_halo() {
    let sheet: String = FORCED_COLORS_RULES
        .iter()
        .map(|sel| format!("{sel} {{{}}} ", css_rule_body(SHIPPED_CSS, sel)))
        .collect();

    let mount = create_mount_point();
    render_into(&mount, disc);
    yield_now().await;

    let history = history_of(&[0.05, 0.05, 0.05, 0.05, 0.05]);
    let paint = history.spark_paint(SignalLevel::Lost, true, false, false, true, false);
    refresh_peer_disc(PEER, "Ada", false, &paint, &RefCell::new(None));

    let document = web_sys::window().unwrap().document().unwrap();
    let style = document.create_element("style").unwrap();
    style.set_text_content(Some(&sheet));
    document.head().unwrap().append_child(&style).unwrap();

    let one = |sel: &str| {
        mount
            .query_selector(sel)
            .unwrap()
            .unwrap_or_else(|| panic!("no {sel}"))
    };
    let trend = one(".signal-spark polyline:not(.spark-halo)");
    let trend_halo = one(".signal-spark polyline.spark-halo");
    // Positional: `nth-of-type` counts <line> only, so it pins the paint order.
    let slash_halo = one(".signal-spark line.spark-halo:nth-of-type(1)");
    let slash = one(".signal-spark line:not(.spark-halo):nth-of-type(2)");

    assert!(trend_halo.matches(FORCED_COLORS_RULES[0]).unwrap());
    assert!(slash_halo.matches(FORCED_COLORS_RULES[1]).unwrap());
    assert!(slash_halo.matches(".signal-spark svg line").unwrap());

    let stroke = |el: &web_sys::Element| {
        web_sys::window()
            .unwrap()
            .get_computed_style(el)
            .unwrap()
            .unwrap()
            .get_property_value("stroke")
            .unwrap()
    };
    assert_ne!(
        stroke(&trend_halo),
        stroke(&trend),
        "the trend's halo resolved to the trend's own paint"
    );
    assert_ne!(
        stroke(&slash_halo),
        stroke(&slash),
        "the slash's keyline resolved to the slash's own paint"
    );
    // The slash keeps its red, so `assert_ne!` alone misses a lost exclusion.
    assert_eq!(
        stroke(&trend_halo),
        stroke(&slash_halo),
        "a halo took a different rule, so it lost the `.spark-halo` opt-out"
    );

    style.remove();
    cleanup(&mount);
}
