// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue #2170 (a11y): the PINNED FLOOR of every SEND quality slider must announce
// the BASE rung of its ladder in `aria-valuetext`, not the TOP.
//
// WHY THIS FILE EXISTS (pre-submit gate finding). The fix lives at ONE line in
// `DualRangeSlider` — `send_min_valuetext(layer_mode, sel.min_pos, &labels)`. The
// host unit test `send_min_valuetext_speaks_the_base_rung_in_layer_mode` pins the
// HELPER, and it is genuinely mutation-sensitive there. But it CANNOT see the call
// site: reverting that one line to `position_label(sel.min_pos, &labels)` — the
// original defect — leaves the ENTIRE `cargo test -p videocall-ui --lib` suite GREEN.
// Verified by running exactly that mutation. (No test count stated on purpose: the
// behavioural claim is stable, but any absolute drifts on every unrelated merge to the
// base branch, and CI's figure differs from a branch-local one because CI builds the
// merge ref.)
//
// The Playwright assertions in `e2e/tests/performance-settings.spec.ts` (`no SEND
// 'Fixed' badge…` and `SEND ceiling thumb is grabbable…`) do cover it, but that spec is
// UNTAGGED — `pr-check-e2e-smoke-hcl.yaml` runs `--project=bvt1`
// (`grep: /@bvt[01]\b/`), so per-PR CI never executes it. Net effect before this file:
// revert the production line and every CI job stays green. That is the same
// untagged-spec drift that let issue #2158 sit for ~6 weeks. Those specs stay
// as the fuller-context check (real config.js chain, host.rs prop plumbing, CSS, real
// pointer drag) — none of which this file can see; see the coverage split below.
//
// This test closes it at the CALL SITE, in a real browser, with no docker stack and
// no live encoders: it mounts the real `PerformanceSettingsPanel` and reads the
// rendered `aria-valuetext` attribute off the DOM.
//
// It runs in a browser (`wasm_bindgen_test`) because the assertion is on rendered
// DOM output.
//
// COVERAGE SPLIT — what this file deliberately does NOT observe, so nobody reads it as
// a replacement for the Playwright specs:
//   * the real `config.js` → `experimental_simulcast_max_layers` →
//     `min(flag, capability_max_simulcast_layers())` chain, and the
//     `testCapabilityMaxLayersOverride` path. This file forces depth via the props.
//   * the `host.rs::send_layer_max` / `audio_layer_max` → `PerfControlsHandle` →
//     `diagnostics.rs` → panel prop plumbing. A regression that stopped FORWARDING the
//     depth stays green here.
//   * anything stylesheet-dependent — `.is-pinned { pointer-events: none; z-index: 0 }`,
//     i.e. the actual WebKit pinned-floor fix — and a real pointer drag on the ceiling.
//     No CSS is loaded in this harness.
//   * the panel being mounted inside `#diagnostics-sidebar` (the #1131 relocation), and
//     the localStorage round-trip across a reload.
// Forcing the props is nonetheless the STRONGER premise for THIS defect: the only thing
// the config path would additionally catch is a ladder-DEPTH regression, and at depth 1
// the buggy and fixed lookups agree, so a depth regression cannot resurrect this bug.
// The props path drops the CPU-clamp dependence, the config-interception ordering
// hazard, and every skip path.
//
// NOTE: a new test binary is only COMPILED by the build step in
// `pr-check-dioxus-ui-hcl.yaml` unless it also gets an explicit `--test` run step
// there. Without that line this file would sit in the repo looking like coverage
// while never executing — precisely the failure mode it exists to fix. The step was
// added in the same commit.
//
// ADDING A SECOND CASE TO THIS FILE? `cleanup` only detaches the mount node. The
// panel's 250 ms `Interval` and the VU-meter rAF loop keep running afterwards, and
// they resolve their targets with a DOCUMENT-GLOBAL `get_element_by_id` — so two
// mounted panels fight over duplicate ids and can write each other's readouts. That
// produces a FALSE GREEN rather than an error, so it is not self-announcing.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use dioxus::prelude::*;
use dioxus_ui::components::performance_settings::receive::ReceivePreference;
use dioxus_ui::components::performance_settings::{
    send_layer_labels_with_top, PerformancePreference, PerformanceSettingsPanel,
};
use dioxus_ui::constants::audio_published_layer_count;
use videocall_client::PrefMediaKind;
use wasm_bindgen_test::*;

mod support;
use support::{cleanup, create_mount_point, inject_app_config, render_into, wait_for_selector};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Mount the panel with a THREE-rung ladder for VIDEO, the only SEND kind that
/// still has one. SCREEN is single-rung since issue #2343 and AUDIO since #2279;
/// their `*_layer_max` props are the production depths.
///
/// The `*_layer_max` props are plain `usize` and feed `send_layer_labels(kind,
/// layer_max)` directly, so 3 rungs are forced with NO config flag, NO capability
/// sniff, and NO encoders — unlike the e2e path, which needs
/// `enableSimulcastFlag(ctx, 3, {capabilityMaxLayersOverride: 3})` to defeat the
/// CPU clamp on video.
///
/// THREE is load-bearing, not incidental: at depth 1 a ladder is single-element and
/// `position_to_tier_index(0, 1) == 0`, so the buggy tier lookup and the fixed layer
/// lookup return the SAME string and the assertions below would pass on broken code.
/// At depth 3 they diverge (base `180p` vs top `720p`). The depth is therefore
/// asserted before any valuetext assertion relies on it.
///
/// SCREEN AND AUDIO CANNOT PARTICIPATE IN THAT DIVERGENCE. Both ladders are one
/// rung, so base == top at every input and including them in the divergence loop
/// would be a tautology — each is asserted separately, for the narrower property it
/// can still carry.
///
/// VIDEO's ceiling is seeded to 2-of-3 deliberately. At a full ceiling `active_count`
/// and the ladder length are BOTH 3, so transposing
/// `send_layer_ceiling_valuetext(active_count, ladder_len)` at its call site renders a
/// byte-identical `"3 of 3 layers"` and no assertion can see it. A lowered video
/// ceiling makes the two arguments differ, so argument binding is pinned and not just
/// the callee's body. It also exercises the non-full-ceiling render path (`show_reset`).
fn three_rung_panel() -> Element {
    rsx! {
        PerformanceSettingsPanel {
            pref: PerformancePreference::default().with_video_layers(Some(2)),
            on_change: move |_| {},
            receive_pref: ReceivePreference::default(),
            on_receive_change: move |_| {},
            video_layer_max: 3,
            screen_layer_max: 3,
            audio_layer_max: audio_published_layer_count() as usize,
        }
    }
}

/// Both thumbs of every SEND slider announce the right thing: the pinned FLOOR names
/// the BASE rung, and the CEILING names the published layer COUNT.
///
/// MUTATION RUN (not predicted): reverting the `DualRangeSlider` call site to
/// `let min_valuetext = position_label(sel.min_pos, &labels).to_string();` — leaving
/// the `send_min_valuetext` helper and its passing unit test fully intact — fails
/// this test with the video floor reading `720p` instead of `180p`. That is the
/// exact call-site revert the host suite cannot see.
#[wasm_bindgen_test]
async fn send_slider_thumbs_announce_base_rung_and_layer_count() {
    inject_app_config();
    let mount = create_mount_point();
    render_into(&mount, three_rung_panel);

    // The panel renders its SEND cells asynchronously; poll rather than sleeping a
    // fixed duration (the harness's own guidance for non-trivial subtrees).
    let found = wait_for_selector(&mount, "[data-testid='perf-video-range-min']", 5_000).await;
    assert!(
        found,
        "SEND video slider never rendered — the panel did not mount, so the \
         assertions below would be vacuous"
    );

    // SCREEN (#2343) and AUDIO (#2279) are deliberately ABSENT from this loop: each
    // ladder is one rung, so base == top and the buggy top-announcing lookup agrees
    // with the fixed base-announcing one. Both are asserted after the loop, for what
    // they can still prove.
    //
    // The floor of video's lowest-first ladder. Unfixed, `position_label`'s tier
    // inversion made it announce the TOP: 720p.
    //
    // The fixture seeds video's ceiling to 2-of-3, which is what makes the ceiling
    // assertion sensitive to ARGUMENT ORDER at the
    // `send_layer_ceiling_valuetext(active_count, ladder_len)` call site — at a full
    // ceiling both arguments are 3 and a transposition is invisible.
    for (kind, expected_base, expected_top, expected_ceiling) in
        [("video", "180p", "720p", "2 of 3 layers")]
    {
        let el = mount
            .query_selector(&format!("[data-testid='perf-{kind}-range-min']"))
            .unwrap()
            .unwrap_or_else(|| panic!("{kind} min thumb should render"));

        // LADDER-DEPTH PREMISE, asserted PER KIND before anything depends on it.
        // `max` is `labels.len() - 1`, so "2" == a 3-rung ladder. Every assertion
        // below is vacuous at depth 1 (buggy == fixed), so if the panel ever stopped
        // honouring this kind's `*_layer_max` prop it must fail LOUDLY here.
        assert_eq!(
            el.get_attribute("max")
                .unwrap_or_else(|| panic!("{kind} min thumb carries a max attribute")),
            "2",
            "expected a 3-rung {kind} ladder; at depth 1 the buggy and fixed lookups \
             agree and this assertion proves nothing"
        );
        let spoken = el
            .get_attribute("aria-valuetext")
            .unwrap_or_else(|| panic!("{kind} min thumb should carry aria-valuetext"));
        assert_eq!(
            spoken, expected_base,
            "the pinned {kind} floor must ANNOUNCE the base rung ({expected_base}), \
             not the top of the ladder ({expected_top})"
        );

        // THE CEILING THUMB, same rendered composite. A layer-mode ceiling announces
        // the published COUNT, never a rung label. This assertion is the ONLY guard on
        // that whole path — no e2e spec asserts a SEND ceiling `aria-valuetext`, and the
        // host unit test pins only `send_layer_ceiling_valuetext`'s body. (The word SEND
        // is load-bearing: `performance-settings.spec.ts` DOES assert a
        // `perf-recv-*-range-max` valuetext, but that is the receive module's own
        // `DualRangeSlider`, which uses direct-indexed `index_label` and never
        // `position_label` — a different component with no share of this hazard.)
        //
        // THREE mutations it catches, all RUN against THIS fixture (video ceiling at
        // 2-of-3, so `sel.max_pos == 1` — the expected values below depend on that):
        //  1. dropping the `layer_mode` branch on `max_valuetext` (always deriving from
        //     `labels`) → video reads `360p`, i.e. `position_label(1, …)`. NOT `180p`:
        //     that was the value at the old FULL ceiling (`max_pos == 2` → index 0), and
        //     the difference is itself evidence the fixture changed rendered state.
        //     Compiles clean with NO warning at all.
        //  2. transposing the call site to
        //     `send_layer_ceiling_valuetext(labels.len(), active_count)` → video reads
        //     `3 of 2 layers`. Both args are `usize`, so it compiles with zero warnings,
        //     and this was INVISIBLE until video's ceiling was lowered (at a full
        //     ceiling both args are 3 and the render is byte-identical).
        //  3. rendering an EMPTY valuetext (`String::new()`) → reads "". Dioxus types a
        //     literal-`String` prop as `impl Display`, so neither empty nor a bare
        //     integer is rejected at compile time; only OMITTING the prop is. Confirmed
        //     by running it that Dioxus EMITS the empty attribute rather than dropping
        //     it, so the `assert_eq!` below is what fails (`left: ""`) — the attribute
        //     lookup above does not panic first.
        let max_el = mount
            .query_selector(&format!("[data-testid='perf-{kind}-range-max']"))
            .unwrap()
            .unwrap_or_else(|| panic!("{kind} max thumb should render"));
        let ceiling_spoken = max_el
            .get_attribute("aria-valuetext")
            .unwrap_or_else(|| panic!("{kind} max thumb should carry aria-valuetext"));
        assert_eq!(
            ceiling_spoken, expected_ceiling,
            "the {kind} ceiling must ANNOUNCE the published layer COUNT \
             ({expected_ceiling}) — a resolution means it fell back onto the tier \
             lookup, a transposed count means the call-site args are swapped, and an \
             empty string means AT falls back to the bare slider position"
        );
    }

    // AUDIO: single-rung (#2279), so a NARROWER property. What it still proves is the
    // one thing the panel must never render again — the floor announcing a LADDER
    // BITRATE. `AUDIO_LAYER_KBPS[0]` is 12 kbps while a single-layer publisher runs at
    // the AQ audio tier, so any digit in this attribute is a wrong number on screen.
    let audio_rungs = send_layer_labels_with_top(
        PrefMediaKind::Audio,
        audio_published_layer_count() as usize,
        "720p",
    );
    assert_eq!(
        audio_rungs.len(),
        1,
        "issue 2279: audio publishes ONE layer, so its ladder is one label"
    );

    let audio_min = mount
        .query_selector("[data-testid='perf-audio-range-min']")
        .unwrap()
        .expect("audio min thumb should render");
    assert_eq!(
        audio_min
            .get_attribute("max")
            .expect("audio min thumb carries a max attribute"),
        "0",
        "a single-layer audio ladder must render max=0; a higher value means the \
         publisher's layer count and this control have drifted apart"
    );
    let audio_spoken = audio_min
        .get_attribute("aria-valuetext")
        .expect("audio min thumb should carry aria-valuetext");
    assert_eq!(audio_spoken, audio_rungs[0]);
    assert!(
        !audio_spoken.chars().any(|c| c.is_ascii_digit()),
        "the audio floor must announce no bitrate; got {audio_spoken:?}"
    );

    let audio_max = mount
        .query_selector("[data-testid='perf-audio-range-max']")
        .unwrap()
        .expect("audio max thumb should render");
    assert_eq!(
        audio_max
            .get_attribute("aria-valuetext")
            .expect("audio max thumb should carry aria-valuetext"),
        "1 layer",
        "the audio ceiling must ANNOUNCE the published layer COUNT"
    );

    // SCREEN: single-rung, so a NARROWER property (issue #2343). It cannot carry the
    // base-vs-top inversion guard above — that would be a tautology. It still proves
    // the ladder is one rung (`max` == "0", failing if screen simulcast returns), that
    // the label comes from `screen_display_label` (drop its `native` arm and the
    // helper returns `"?"`), and that the ceiling announces a COUNT not a resolution.
    // The label expectation is read from the production helper so a relabel moves both
    // together; `max` and `"1 layer"` are literals on purpose — they pin the DEPTH.
    let screen_rungs = send_layer_labels_with_top(PrefMediaKind::Screen, 3, "720p");
    assert_eq!(
        screen_rungs.len(),
        1,
        "issue 2343: screen publishes ONE rung; `send_layer_labels_with_top` must \
         ignore its layer_max argument for Screen"
    );

    let screen_min = mount
        .query_selector("[data-testid='perf-screen-range-min']")
        .unwrap()
        .expect("screen min thumb should render");
    assert_eq!(
        screen_min
            .get_attribute("max")
            .expect("screen min thumb carries a max attribute"),
        "0",
        "a single-rung screen ladder must render max=0; a higher value means the \
         simulcast ladder came back"
    );
    assert_eq!(
        screen_min
            .get_attribute("aria-valuetext")
            .expect("screen min thumb should carry aria-valuetext"),
        screen_rungs[0],
        "the screen floor must announce the rung `screen_display_label` produces \
         (\"Native\"); \"?\" means the label helper lost its `native` arm"
    );

    let screen_max = mount
        .query_selector("[data-testid='perf-screen-range-max']")
        .unwrap()
        .expect("screen max thumb should render");
    assert_eq!(
        screen_max
            .get_attribute("aria-valuetext")
            .expect("screen max thumb should carry aria-valuetext"),
        // `send_layer_ceiling_valuetext` special-cases `ladder_len <= 1` to the
        // ungrammatical-free "1 layer" rather than "1 of 1 layers".
        "1 layer",
        "the screen ceiling must ANNOUNCE the published layer COUNT — a resolution \
         means it fell back onto the tier lookup"
    );

    cleanup(&mount);
}
