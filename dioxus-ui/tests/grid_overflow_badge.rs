// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0

// Issue #2264: `readGridOverflowCount` in `dioxus-ui/scripts/recording.js`
// reads `data-overflow-count` off this badge to draw its own "+N" cell.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{cleanup, create_mount_point, render_into, yield_now};
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::grid_overflow_badge::GridOverflowBadge;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

fn seven_more() -> Element {
    rsx! { GridOverflowBadge { overflow_count: 7usize } }
}

#[wasm_bindgen_test]
async fn badge_exposes_the_count_the_recorder_reads() {
    let mount = create_mount_point();
    render_into(&mount, seven_more);
    yield_now().await;

    let badge = mount
        .query_selector(".grid-overflow-badge[data-overflow-count]")
        .unwrap()
        .expect("recording.js selects .grid-overflow-badge[data-overflow-count]");
    assert_eq!(
        badge.get_attribute("data-overflow-count").as_deref(),
        Some("7"),
        "the attribute must carry the count, not just the printed chip text"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn badge_still_renders_the_visible_chip() {
    let mount = create_mount_point();
    render_into(&mount, seven_more);
    yield_now().await;

    let text = mount.text_content().unwrap_or_default();
    assert!(
        text.contains("+7"),
        "badge should print the count: {text:?}"
    );
    assert!(
        text.contains("more in meeting"),
        "badge should keep its caption: {text:?}"
    );

    cleanup(&mount);
}
