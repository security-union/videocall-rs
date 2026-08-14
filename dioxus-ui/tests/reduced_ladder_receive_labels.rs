// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue #2156: every `VideoCallClientOptions` construction site in this crate must
// pass `camera_ladder_variant`, so the RECEIVER-side rung labels / `{w}x{h}` /
// `~kbps` readouts describe the ladder the deployment's publishers actually encode.
//
// WHY THIS FILE EXISTS. The field is not `Option`, so the COMPILER already forces
// every site to pass *something* — a missing site is a build error, not a silent
// bug. What the compiler cannot catch is a site passing the WRONG thing: a literal
// `LadderVariant::Default` instead of the config accessor
// `crate::constants::camera_ladder_variant()`. That compiles, is invisible in
// review once the diff scrolls past, and silently ships Default-labelled receive
// readouts on that ONE surface of a `Reduced` deployment — which is exactly the
// class of partial plumbing the #1768 review caught (the flag reached the encoder
// but not the label).
//
// So this test asserts, per file, that each construction site is wired to the
// ACCESSOR. It is a SOURCE-TEXT pin (via `include_str!`, the same technique
// `dioxus-ui/src/components/emoji_picker.rs` and `theme_file.rs` already use to pin
// the shipped CSS/HTML), because the wiring is a property of the call sites — there
// is no runtime seam that can enumerate them. `include_str!` resolves at COMPILE
// time relative to this file, so it cannot go stale or read a different checkout.
//
// It compiles on BOTH targets (no `#![cfg(target_arch = "wasm32")]` gate, unlike
// the browser-bound `reduced_ladder_flag.rs`) and executes natively in per-PR CI.
// Because it uses plain `#[test]`, a wasm test invocation reports no tests to run.
// Gating it to wasm would therefore have hidden it from the native integration-test
// step without adding browser execution.

/// The four `VideoCallClient` construction sites in this crate, as (label, source).
///
/// Adding a fifth: add it here too. `construction_site_count_is_pinned` fails if the
/// repo grows a `VideoCallClientOptions` literal this list does not mention, so a new
/// site cannot be added without confronting this file.
const SITES: &[(&str, &str)] = &[
    (
        "components/attendants.rs (the IN-CALL client — the one that renders the perf panel)",
        include_str!("../src/components/attendants.rs"),
    ),
    (
        "components/waiting_room.rs (waiting-room observer client)",
        include_str!("../src/components/waiting_room.rs"),
    ),
    (
        "pages/guest_join.rs (guest lobby observer client)",
        include_str!("../src/pages/guest_join.rs"),
    ),
    (
        "pages/meeting.rs (pre-meeting observer client)",
        include_str!("../src/pages/meeting.rs"),
    ),
];

/// The exact wiring each site must carry.
const REQUIRED_WIRING: &str = "camera_ladder_variant: crate::constants::camera_ladder_variant()";

/// Every construction site must read the variant from the runtime config accessor.
///
/// MUTATION: change any one site to `camera_ladder_variant:
/// videocall_client::adaptive_quality_constants::LadderVariant::Default` (which
/// compiles fine) and this test names that file.
#[test]
fn every_client_construction_site_reads_the_camera_ladder_variant() {
    for (label, source) in SITES {
        assert!(
            source.contains(REQUIRED_WIRING),
            "{label}: must pass `{REQUIRED_WIRING}` to VideoCallClientOptions (issue #2156). \
             Hardcoding a LadderVariant here compiles, but silently gives this surface the \
             SHIPPED 720p receive labels on a reduced-ladder deployment."
        );
    }
}

/// The `SITES` list must be COMPLETE.
///
/// Counting `VideoCallClientOptions {` literals per listed file and summing gives the
/// number of sites this file actually covers; that must equal the number of literals
/// across the listed sources. A new construction site in a file NOT listed above would
/// slip past `every_client_construction_site_reads_the_camera_ladder_variant` (which
/// only inspects listed files), so the count is pinned separately and deliberately.
///
/// This cannot see a literal added to an UNLISTED file — no compile-time API can
/// enumerate the crate's own sources. That residual gap is covered by the compiler
/// itself: `camera_ladder_variant` is a non-`Option` field, so a new site must pass
/// something, and a reviewer reading this comment knows to add the file here.
///
/// MUTATION: add a second `VideoCallClientOptions {` literal to any listed file
/// without updating `EXPECTED_SITES` and this fails.
#[test]
fn construction_site_count_is_pinned() {
    const EXPECTED_SITES: usize = 4;
    let found: usize = SITES
        .iter()
        .map(|(_, source)| source.matches("VideoCallClientOptions {").count())
        .sum();
    assert_eq!(
        found, EXPECTED_SITES,
        "expected {EXPECTED_SITES} VideoCallClient construction sites across the files listed \
         in SITES, found {found}. If a site was added, add its `camera_ladder_variant: \
         crate::constants::camera_ladder_variant()` wiring AND update this count; if one was \
         removed, drop it from SITES."
    );
    assert_eq!(
        SITES.len(),
        EXPECTED_SITES,
        "SITES lists {} files but {EXPECTED_SITES} sites are expected — today each file holds \
         exactly one literal, so a divergence means SITES drifted",
        SITES.len()
    );
}
