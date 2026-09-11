/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! The runtime half of issue 2645. Its own binary because `prometheus::gather()` reads a
//! process-global registry, and inside `--lib` other tests register these same families.

use sec_api::metrics::{
    init_health_ingest_series, init_server_stats_series, init_websocket_relay_series,
    init_webtransport_relay_series,
};

fn gathered_names() -> Vec<String> {
    prometheus::gather()
        .iter()
        .map(|f| f.get_name().to_string())
        .collect()
}

fn assert_absent(names: &[&str], init: &str) {
    let before = gathered_names();
    for name in names {
        assert!(
            !before.contains(&name.to_string()),
            "{name} is already registered before {init} ran — this binary is no longer \
             isolated, so the transition below would be vacuous"
        );
    }
}

fn assert_present(names: &[&str], init: &str) {
    let after = gathered_names();
    for name in names {
        assert!(
            after.contains(&name.to_string()),
            "{name} absent from a scrape taken after {init}"
        );
    }
}

/// Both relay inits in ONE test on purpose: they share `init_relay_common_series`, so a
/// separate test asserting the shared families absent would race this one's registration.
#[test]
fn each_relay_init_moves_its_families_from_absent_to_zero() {
    let ws = [
        "relay_ws_fragmented_inbound_total",
        "relay_ws_fragment_discarded_total",
    ];
    let shared = [
        "videocall_legacy_token_type_accepted_total",
        "relay_nats_publish_latency_ms",
    ];
    let wt = ["videocall_relay_scheduler_lag_ms"];

    assert_absent(&ws, "init_websocket_relay_series");
    assert_absent(&shared, "init_websocket_relay_series");
    init_websocket_relay_series();
    assert_present(&ws, "init_websocket_relay_series");
    assert_present(&shared, "init_websocket_relay_series");

    // The WT init must still publish its own transport-specific family. `shared` is already
    // registered by now, so only `wt` can be asserted absent here.
    assert_absent(&wt, "init_webtransport_relay_series");
    init_webtransport_relay_series();
    assert_present(&wt, "init_webtransport_relay_series");
}

#[test]
fn the_health_ingest_init_moves_its_families_from_absent_to_zero() {
    let names = [
        "videocall_health_reports_total",
        "videocall_client_non_finite_samples_dropped_total",
        "videocall_tier_transitions_dropped_total",
        "videocall_encoder_layer_geometry_dropped_total",
    ];
    assert_absent(&names, "init_health_ingest_series");
    init_health_ingest_series();
    assert_present(&names, "init_health_ingest_series");
}

#[test]
fn the_server_stats_init_publishes_its_family() {
    // No absence pre-assert: `metrics_server_snapshot`'s `main` calls `.reset()` on this
    // counter, which registers it, so the init is defensive and absence is not its contract.
    init_server_stats_series();
    assert_present(
        &["videocall_server_connection_events_total"],
        "init_server_stats_series",
    );
}
