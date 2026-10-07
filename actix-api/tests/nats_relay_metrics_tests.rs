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

//! Issue 2925 against a live NATS (`NATS_URL`). Its own binary: the statistics collector
//! registers once per process registry.

use std::time::Duration;

use sec_api::metrics::{register_nats_statistics, with_nats_event_metrics};

fn nats_url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string())
}

fn scraped(name: &str, event: Option<&str>) -> f64 {
    prometheus::gather()
        .iter()
        .filter(|mf| mf.get_name() == name)
        .flat_map(|mf| mf.get_metric().iter())
        .filter(|m| {
            event.is_none_or(|e| {
                m.get_label()
                    .iter()
                    .any(|l| l.get_name() == "event" && l.get_value() == e)
            })
        })
        .map(|m| m.get_counter().get_value())
        .sum()
}

async fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn relay_nats_client_exports_its_statistics_and_events() {
    const N: u64 = 50;
    let subject = format!("test.relay-nats-metrics.{}", std::process::id());

    let publisher = with_nats_event_metrics(async_nats::ConnectOptions::new())
        .connect(nats_url())
        .await
        .expect("NATS must be reachable at NATS_URL");
    register_nats_statistics(&publisher).expect("first registration in this process");

    // A one-slot subscription that is never read: past the first message async-nats
    // drops each delivery and emits SlowConsumer.
    let slow = with_nats_event_metrics(async_nats::ConnectOptions::new().subscription_capacity(1))
        .connect(nats_url())
        .await
        .expect("NATS must be reachable at NATS_URL");
    let _unread = slow.subscribe(subject.clone()).await.expect("subscribe");
    slow.flush()
        .await
        .expect("subscription must reach the server");

    assert!(scraped("relay_nats_connects_total", None) >= 1.0);
    let out_before = scraped("relay_nats_out_messages_total", None);
    for i in 0..N {
        publisher
            .publish(subject.clone(), i.to_string().into())
            .await
            .expect("publish");
    }
    publisher.flush().await.expect("flush");

    assert!(
        scraped("relay_nats_out_messages_total", None) - out_before >= N as f64,
        "out_messages must advance by at least the {N} publishes"
    );
    eventually("both connects must book event=connected", || {
        scraped("relay_nats_events_total", Some("connected")) >= 2.0
    })
    .await;
    eventually(
        "an unread one-slot subscription must book slow_consumer",
        || scraped("relay_nats_events_total", Some("slow_consumer")) >= 1.0,
    )
    .await;
}
