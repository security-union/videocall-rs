// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue #2170: the SEND video meter's readout must render the encoder's
// NOT-YET-PUBLISHED geometry sentinel as an em-dash, never as `0x0`.
//
// WHY THIS FILE EXISTS. The fix is one branch in `format_video_readout`, and the
// host unit tests
// (`video_readout_renders_an_em_dash_for_the_unpublished_sentinel`,
// `gauge_state_video_text_is_the_em_dash_when_nothing_is_published`) are genuinely
// mutation-sensitive on the FORMATTER and on `GaugeState`. What they cannot see is
// whether that text reaches the DOM: the readout is written by
// `write_readout_text(VIDEO_READOUT_ID, ..)` into an element resolved by
// DOCUMENT-GLOBAL id, and `PerfMeter` renders `initial_readout` as its text child.
// A regression that mapped the readout correctly but rendered the wrong field, or
// wired the wrong id, would leave every host test green.
//
// The Playwright spec `e2e/tests/performance-settings.spec.ts` does assert this
// surface with real encoders, but it is UNTAGGED — `pr-check-e2e-smoke-hcl.yaml`
// runs `--project=bvt1` (`grep: /@bvt[01]\b/`), so per-PR CI never executes it.
// Without this file, reverting the production branch leaves every per-PR CI job
// green. That is the exact seam PR #2188 was faulted for in gate round 1, applied
// here pre-emptively rather than after a reviewer found it.
//
// It runs in a browser (`wasm_bindgen_test`) because the assertion is on rendered
// DOM text, and because the panel's subtree resolves the camera top rung from
// `window.__APP_CONFIG` via the memoized `app_config()`.
//
// COVERAGE SPLIT — what this file deliberately does NOT observe, so nobody reads it
// as a replacement for the e2e spec:
//   * `CameraEncoder::live_quality_snapshot` itself, and therefore the whole
//     publish path (`set_encode_dims` → `publish_layer_dims` →
//     `top_published_layer_dims`). This file INJECTS a snapshot; the client-side
//     host tests pin the reader, and seven write call sites inside the encode-loop
//     future remain browser-only (disclosed at `shared_layer_dims`' clone in
//     `CameraEncoder::start`).
//   * `host.rs`'s wiring of the real encoder into `SnapshotReader`, and its
//     `self_metrics_overlay` consumer. Both are covered by
//     `e2e/tests/performance-settings.spec.ts::"self-tile overlay reports the FITTED
//     encode size, never the AQ tier box"` — a SOLO test, because that overlay needs
//     no remote peer — which runs green and is mutation-verified end-to-end.
//     NOT by `media-metrics-overlay.spec.ts`: an earlier version of this note said so,
//     but that spec cannot currently run green (issue #2193, its 2-peer harness times
//     out before any assertion). It adds the 2-peer case once #2193 lands.
//   * the ~4 Hz rAF driver's LIVE update path. This asserts FIRST PAINT, which runs
//     the same `gauge_state_from_snapshot` mapper — deliberately, because the rAF
//     loop's 250 ms throttle makes a live-update assertion timing-dependent for no
//     extra coverage of the branch under test.
//
// ADDING A SECOND CASE TO THIS FILE? This file has exactly ONE
// `#[wasm_bindgen_test]`, deliberately. `cleanup` only detaches the mount node; the
// panel's rAF loop keeps running afterwards and resolves its target with a
// DOCUMENT-GLOBAL `get_element_by_id`, so two mounted panels fight over the
// duplicate `perf-vu-video-readout` id and can overwrite each other's readouts —
// producing a FALSE GREEN or a confusing cross-test failure, not a clean error.
// So: a second case must either share the SAME mount as the existing one and
// assert on a distinct element from that single render, or (if it needs a
// different injected snapshot, which this one's whole premise is) live in its own
// binary with its own `--test` step.
//
// NOTE: a new test binary is only COMPILED by the build step in
// `pr-check-dioxus-ui-hcl.yaml` unless it also gets an explicit `--test` run step
// there. Without that line this file would sit in the repo looking like coverage
// while never executing — precisely the failure mode it exists to fix. The step was
// added in the same commit.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use std::rc::Rc;

use dioxus::prelude::*;
use dioxus_ui::components::performance_settings::receive::ReceivePreference;
use dioxus_ui::components::performance_settings::{
    PerformancePreference, PerformanceSettingsPanel, SnapshotReader,
};
use videocall_client::LiveQualitySnapshot;
use wasm_bindgen_test::*;

mod support;
use support::{cleanup, create_mount_point, inject_app_config, render_into, wait_for_selector};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// A snapshot in the state the encoder reports before its first frame publishes,
/// and after `stop()`: geometry is the `(0, 0)` sentinel while the AQ TIER fields
/// are live.
///
/// That split is the whole point of the assertion — `video_fps` and
/// `video_ideal_kbps` are tier targets and stay meaningful with no frame published,
/// so a correct render shows `—·30fps·1500kbps`, not a wholly-blank readout. Both
/// tier values are deliberately NON-ZERO so the test can tell "the dims segment was
/// replaced" apart from "the whole line was blanked".
fn unpublished_snapshot() -> LiveQualitySnapshot {
    LiveQualitySnapshot {
        video_tier_index: 0,
        video_width: 0,
        video_height: 0,
        video_fps: 30,
        video_ideal_kbps: 1500,
        audio_tier_index: 1,
        audio_kbps: 24,
        target_bitrate_kbps: 1234.0,
    }
}

fn panel_with_unpublished_geometry() -> Element {
    rsx! {
        PerformanceSettingsPanel {
            pref: PerformancePreference::default(),
            on_change: move |_| {},
            read_snapshot: SnapshotReader(Rc::new(|| Some(unpublished_snapshot()))),
            receive_pref: ReceivePreference::default(),
            on_receive_change: move |_| {},
        }
    }
}

/// The rendered video readout announces UNKNOWN geometry as an em-dash, keeps the
/// live tier targets, and is distinguishable from the camera-off empty state.
///
/// MUTATION RUN (not predicted): reverting `format_video_readout` to its pre-#2170
/// body —
/// `format!("{}x{}·{}fps·{}kbps", snap.video_width, snap.video_height, snap.video_fps, snap.video_ideal_kbps)`
/// — fails this test with the readout reading `0x0·30fps·1500kbps`.
#[wasm_bindgen_test]
async fn send_video_readout_renders_an_em_dash_for_unpublished_geometry() {
    inject_app_config();
    let mount = create_mount_point();
    render_into(&mount, panel_with_unpublished_geometry);

    // The panel renders its SEND cells asynchronously; poll rather than sleeping a
    // fixed duration (the harness's own guidance for non-trivial subtrees).
    let found = wait_for_selector(&mount, "#perf-vu-video-readout", 5_000).await;
    assert!(
        found,
        "SEND video readout never rendered — the panel did not mount, so the \
         assertions below would be vacuous"
    );

    let readout = mount
        .query_selector("#perf-vu-video-readout")
        .unwrap()
        .expect("video readout should render");
    let text = readout.text_content().unwrap_or_default();

    // THE ASSERTION. An em-dash (U+2014) for the dims, tier targets intact.
    assert_eq!(
        text, "—·30fps·1500kbps",
        "unpublished geometry must announce as an em-dash; `0x0` reads as a \
         MEASURED resolution of zero"
    );

    // Guard against the two ways this could pass while being wrong. Both are
    // substring checks on the SAME rendered string, so neither can be satisfied by
    // a differently-shaped readout that happens to contain the right dims.
    assert!(
        !text.contains("0x0"),
        "the pre-#2170 rendering leaked through: {text}"
    );
    assert!(
        !text.contains('-'),
        "an ASCII hyphen is not the em-dash idiom this drawer uses for \
         no-reading: {text}"
    );

    // FIXTURE-DISCRIMINATION guard: the readout is NOT simply the camera-off empty
    // state, which has its own distinct copy. Without this, a regression that
    // treated an unpublished snapshot as "no snapshot" would still render something
    // dash-like and could read as a pass.
    assert!(
        !text.contains("off"),
        "an unpublished-geometry snapshot is a LIVE encoder, not the camera-off \
         empty state: {text}"
    );

    // The bars still light, for the same reason: the AQ tier is live even with no
    // frame published. `data-level` 0 is reserved for the no-signal empty state.
    let meter = mount
        .query_selector("#perf-meter-video")
        .unwrap()
        .expect("video meter should render");
    let level = meter
        .get_attribute("data-level")
        .expect("meter carries data-level");
    assert_ne!(
        level, "0",
        "an unpublished-geometry snapshot is still a live encoder, so the meter \
         must not show the no-signal empty state"
    );

    cleanup(&mount);
}
