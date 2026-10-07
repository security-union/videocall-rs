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
 */

//! Run lifetime: every client stops on Ctrl-C, SIGTERM, or when `--duration` elapses.

use anyhow::anyhow;
use std::future::Future;
use std::time::Duration;
use tokio::sync::watch;

use crate::run_manifest::{epoch_secs, ParticipantRegistry, Presence};
use crate::transport::{stop_after, Stop};
use std::sync::atomic::AtomicBool;
use tokio::task::JoinHandle;
use tracing::info;

/// Parse a run duration: `90`, `90s`, `500ms`, `10m` or `2h`. Zero is rejected.
pub fn parse_duration(raw: &str) -> anyhow::Result<Duration> {
    let s = raw.trim();
    let (digits, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(idx) => s.split_at(idx),
        None => (s, "s"),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| anyhow!("invalid duration '{raw}': expected e.g. 90s, 10m, 2h"))?;
    let d = match unit {
        "ms" => Duration::from_millis(value),
        "s" => Duration::from_secs(value),
        "m" => Duration::from_secs(value.saturating_mul(60)),
        "h" => Duration::from_secs(value.saturating_mul(3600)),
        _ => {
            return Err(anyhow!(
                "invalid duration unit in '{raw}': use ms, s, m or h"
            ))
        }
    };
    if d.is_zero() {
        return Err(anyhow!("duration must be greater than zero; got '{raw}'"));
    }
    Ok(d)
}

/// A shutdown flag every client can wait on; `true` means stop.
pub fn shutdown_channel() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

/// Flip the shutdown flag on Ctrl-C, SIGTERM, or after `duration` (if set).
pub fn spawn_shutdown_trigger(
    tx: watch::Sender<bool>,
    duration: Option<Duration>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let timer = async {
            match duration {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        let reason = tokio::select! {
            _ = tokio::signal::ctrl_c() => "Ctrl-C",
            _ = terminate_signal() => "SIGTERM",
            _ = timer => "run duration elapsed",
        };
        info!("Shutting down all clients: {reason}");
        let _ = tx.send(true);
    })
}

#[cfg(unix)]
async fn terminate_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
async fn terminate_signal() {
    std::future::pending::<()>().await
}

/// Resolve once the shutdown flag is `true` (or its sender is gone).
pub async fn wait_for_shutdown(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Hold a connected client until the run shuts down (`None`) or the relay
/// connection closes on its own (`Some(reason)`).
pub async fn until_shutdown_or_closed(
    shutdown: &mut watch::Receiver<bool>,
    closed: &mut watch::Receiver<Option<String>>,
) -> Option<String> {
    tokio::select! {
        biased;
        _ = wait_for_shutdown(shutdown) => None,
        reason = crate::transport::wait_closed(closed) => Some(reason),
    }
}

/// Await a client task; once the run is shutting down, give it `grace` from
/// the first such wait (`deadline`, shared across tasks) and then abort it.
pub async fn join_or_abort<T>(
    mut handle: JoinHandle<T>,
    shutdown: &mut watch::Receiver<bool>,
    grace: Duration,
    deadline: &mut Option<tokio::time::Instant>,
) -> Option<T> {
    let give_up = async {
        if deadline.is_none() {
            wait_for_shutdown(shutdown).await;
            *deadline = Some(tokio::time::Instant::now() + grace);
        }
        if let Some(at) = *deadline {
            tokio::time::sleep_until(at).await;
        }
    };
    tokio::select! {
        biased;
        joined = &mut handle => joined.ok(),
        _ = give_up => {
            handle.abort();
            None
        }
    }
}

/// Release media to every client and stamp `media_started_at`.
pub fn release_media(
    cell: &tokio::sync::OnceCell<std::time::Instant>,
    registry: &ParticipantRegistry,
) -> std::time::Instant {
    let now = std::time::Instant::now();
    let _ = cell.set(now);
    registry.media_started(epoch_secs());
    now
}

/// Poll for the shared media start; `None` if the run shuts down or the relay
/// connection closes first.
pub async fn wait_for_media_start(
    cell: &tokio::sync::OnceCell<std::time::Instant>,
    shutdown: &watch::Receiver<bool>,
    closed: &watch::Receiver<Option<String>>,
) -> Option<std::time::Instant> {
    loop {
        if let Some(t) = cell.get() {
            return Some(*t);
        }
        if *shutdown.borrow() || closed.borrow().is_some() {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Sleep for `d` unless shutdown comes first. Returns `true` if shutdown won.
pub async fn sleep_or_shutdown(d: Duration, rx: &mut watch::Receiver<bool>) -> bool {
    if *rx.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(d) => *rx.borrow(),
        _ = wait_for_shutdown(rx) => true,
    }
}

/// Why a connected client stopped.
#[derive(Debug, PartialEq)]
pub enum Ended {
    Shutdown,
    Dropped { reason: String, at: f64 },
}

/// Run `setup` (it starts the client's tasks), then hold until the run shuts
/// down or the relay connection closes on its own.
pub async fn connected_phase<T>(
    setup: impl Future<Output = anyhow::Result<T>>,
    shutdown: &mut watch::Receiver<bool>,
    closed: &mut watch::Receiver<Option<String>>,
) -> anyhow::Result<(T, Ended)> {
    let started = setup.await?;
    let ended = match until_shutdown_or_closed(shutdown, closed).await {
        None => Ended::Shutdown,
        Some(reason) => Ended::Dropped {
            reason,
            at: epoch_secs(),
        },
    };
    Ok((started, ended))
}

/// Hold the connected client, record how it left, then stop it.
pub async fn hold_then_stop<T>(
    client: &mut impl Stop,
    quit: &AtomicBool,
    setup: impl Future<Output = anyhow::Result<T>>,
    shutdown: &mut watch::Receiver<bool>,
    closed: &mut watch::Receiver<Option<String>>,
    presence: Presence,
    user_id: &str,
) -> (Option<T>, anyhow::Result<()>) {
    stop_after(client, quit, async {
        match connected_phase(setup, shutdown, closed).await {
            Ok((started, ended)) => (Some(started), settle(presence, user_id, Ok(ended))),
            Err(e) => (None, settle(presence, user_id, Err(e))),
        }
    })
    .await
}

/// Record how the client left; a drop or an error fails the client.
pub fn settle(
    presence: Presence,
    user_id: &str,
    ended: anyhow::Result<Ended>,
) -> anyhow::Result<()> {
    match ended {
        Ok(Ended::Shutdown) => {
            presence.leave(epoch_secs());
            Ok(())
        }
        Ok(Ended::Dropped { reason, at }) => {
            presence.drop_out(at, &reason);
            Err(anyhow::anyhow!("{user_id}: {reason}"))
        }
        Err(e) => {
            presence.fail(epoch_secs(), &e.to_string());
            Err(e)
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::{connected_phase, settle, shutdown_channel};
    use crate::netsim::NetworkProfile;
    use crate::run_manifest::{epoch_secs, ParticipantRegistry};
    use crate::transport::ClosedSignal;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    fn registry() -> Arc<ParticipantRegistry> {
        Arc::new(ParticipantRegistry::new(
            "m".into(),
            None,
            1.0,
            vec![crate::run_manifest::test_record(
                "u",
                &NetworkProfile::passthrough(),
            )],
        ))
    }

    async fn run(close_first: bool) -> (anyhow::Result<()>, serde_json::Value, f64) {
        let reg = registry();
        let presence = reg.join("u", 10.0);
        let (tx, mut shutdown) = shutdown_channel();
        let closed = ClosedSignal::default();
        let mut closed_rx = closed.subscribe();
        let before = epoch_secs();
        if close_first {
            closed.report(&AtomicBool::new(false), "relay closed the WebSocket");
        } else {
            let _ = tx.send(true);
        }
        let ended = tokio::time::timeout(
            Duration::from_secs(2),
            connected_phase(async { Ok(()) }, &mut shutdown, &mut closed_rx),
        )
        .await
        .expect("the client must stop on a relay close as well as on shutdown")
        .map(|((), ended)| ended);
        let result = settle(presence, "u", ended);
        reg.finish(before + 1_000.0);
        let json = serde_json::to_value(reg.snapshot()).unwrap();
        (result, json["participants"][0].clone(), before)
    }

    #[tokio::test]
    async fn a_relay_close_fails_the_client_and_records_the_drop() {
        let (result, p, before) = run(true).await;
        assert!(result.is_err(), "a dropped client must fail the run");
        assert_eq!(p["outcome"], "dropped: relay closed the WebSocket");
        let left = p["leave_ts"].as_f64().unwrap();
        assert!(
            left >= before && left < before + 1_000.0,
            "leave at the close"
        );
    }

    #[tokio::test]
    async fn what_setup_returns_stays_alive_through_the_hold() {
        struct Guard(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let (tx, mut shutdown) = shutdown_channel();
        let mut closed = ClosedSignal::default().subscribe();
        let flag = Arc::clone(&dropped);
        let during_hold = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let alive = !flag.load(std::sync::atomic::Ordering::Relaxed);
            let _ = tx.send(true);
            alive
        });
        let guard = Arc::clone(&dropped);
        let (kept, ended) =
            connected_phase(async move { Ok(Guard(guard)) }, &mut shutdown, &mut closed)
                .await
                .unwrap();
        assert!(
            during_hold.await.unwrap(),
            "setup's producers must live through the hold"
        );
        assert_eq!(ended, super::Ended::Shutdown);
        drop(kept);
    }

    #[tokio::test]
    async fn a_setup_error_is_recorded_as_a_failure_not_a_relay_drop() {
        let reg = registry();
        let presence = reg.join("u", 10.0);
        let (_tx, mut shutdown) = shutdown_channel();
        let mut closed = ClosedSignal::default().subscribe();
        let ended = connected_phase(
            async { Err::<(), _>(anyhow::anyhow!("no costume")) },
            &mut shutdown,
            &mut closed,
        )
        .await
        .map(|((), ended)| ended);
        assert!(settle(presence, "u", ended).is_err());
        let json = serde_json::to_value(reg.snapshot()).unwrap();
        assert_eq!(json["participants"][0]["outcome"], "failed: no costume");
    }

    #[tokio::test]
    async fn a_shutdown_is_a_clean_leave() {
        let (result, p, _) = run(false).await;
        assert!(result.is_ok());
        assert!(p.get("outcome").is_none());
        assert!(p["leave_ts"].is_number());
    }

    #[tokio::test]
    async fn a_setup_error_through_hold_then_stop_fails_and_still_stops() {
        struct Stub(bool);
        impl crate::transport::Stop for Stub {
            async fn stop(&mut self) {
                self.0 = true;
            }
        }
        let reg = registry();
        let presence = reg.join("u", 10.0);
        let (_tx, mut shutdown) = shutdown_channel();
        let mut closed = ClosedSignal::default().subscribe();
        let mut client = Stub(false);
        let (started, result) = super::hold_then_stop(
            &mut client,
            &AtomicBool::new(false),
            async { Err::<(), _>(anyhow::anyhow!("x")) },
            &mut shutdown,
            &mut closed,
            presence,
            "u",
        )
        .await;
        assert!(result.is_err());
        assert!(started.is_none());
        let outcome = reg.snapshot().participants[0].outcome.clone();
        assert!(
            outcome.as_deref().is_some_and(|o| o.starts_with("failed:")),
            "{:?}",
            outcome
        );
        assert!(client.0, "the client is stopped after a setup error");
    }

    #[tokio::test(start_paused = true)]
    async fn a_relay_drop_reaches_the_file_before_a_slow_stop_ends() {
        use crate::run_manifest::{ParticipantsWriter, WRITE_INTERVAL};
        use std::sync::atomic::Ordering;
        struct SlowStop(Arc<AtomicBool>);
        impl crate::transport::Stop for SlowStop {
            async fn stop(&mut self) {
                tokio::time::sleep(Duration::from_secs(5)).await;
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let reg = registry();
        let path = std::env::temp_dir().join(format!(
            "bot-participants-slow-stop-{}-{}.json",
            std::process::id(),
            epoch_secs()
        ));
        let writer = ParticipantsWriter::spawn(Arc::clone(&reg), path.clone(), WRITE_INTERVAL);
        let presence = reg.join("u", 10.0);
        let (_tx, mut shutdown) = shutdown_channel();
        let closed = ClosedSignal::default();
        let mut closed_rx = closed.subscribe();
        closed.report(&AtomicBool::new(false), "relay closed the WebSocket");
        let stopped = Arc::new(AtomicBool::new(false));
        let mut client = SlowStop(Arc::clone(&stopped));
        let quit = AtomicBool::new(false);
        let start = tokio::time::Instant::now();
        let run = super::hold_then_stop(
            &mut client,
            &quit,
            async { Ok(()) },
            &mut shutdown,
            &mut closed_rx,
            presence,
            "u",
        );
        tokio::pin!(run);
        let on_disk = async {
            loop {
                let v: Option<serde_json::Value> = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok());
                if v.is_some_and(|v| v["participants"][0]["leave_ts"].is_number()) {
                    return start.elapsed();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let landed = tokio::select! {
            at = on_disk => at,
            _ = &mut run => panic!("the stop ended before the drop reached the file"),
        };
        assert!(!stopped.load(Ordering::Relaxed));
        assert!(landed <= WRITE_INTERVAL + Duration::from_millis(100));
        let (_, result) = run.await;
        assert!(result.is_err(), "a dropped client still fails the run");
        assert!(stopped.load(Ordering::Relaxed));
        writer.stop().await;
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_relay_close_ends_the_hold_with_its_reason() {
        let (_tx, mut shutdown) = shutdown_channel();
        let closed = crate::transport::ClosedSignal::default();
        let mut rx = closed.subscribe();
        closed.report(&std::sync::atomic::AtomicBool::new(false), "relay closed");
        assert_eq!(
            until_shutdown_or_closed(&mut shutdown, &mut rx)
                .await
                .as_deref(),
            Some("relay closed")
        );
        let (tx, mut shutdown) = shutdown_channel();
        let mut quiet = crate::transport::ClosedSignal::default().subscribe();
        let _ = tx.send(true);
        assert_eq!(
            until_shutdown_or_closed(&mut shutdown, &mut quiet).await,
            None
        );
    }

    #[tokio::test]
    async fn a_client_stuck_after_shutdown_is_aborted_after_the_grace() {
        let (tx, mut shutdown) = shutdown_channel();
        let _ = tx.send(true);
        let stuck = tokio::spawn(std::future::pending::<bool>());
        let mut deadline = None;
        let joined = tokio::time::timeout(
            Duration::from_secs(5),
            join_or_abort(
                stuck,
                &mut shutdown,
                Duration::from_millis(50),
                &mut deadline,
            ),
        )
        .await
        .expect("must not hang past the grace");
        assert_eq!(joined, None);
        let done = tokio::spawn(async { true });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            join_or_abort(
                done,
                &mut shutdown,
                Duration::from_millis(50),
                &mut deadline
            )
            .await,
            Some(true)
        );
    }

    #[test]
    fn parses_units_and_bare_seconds() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn rejects_bad_durations() {
        for bad in ["", "0", "0s", "10x", "m", "-5s", "1.5m"] {
            assert!(parse_duration(bad).is_err(), "'{}' must be rejected", bad);
        }
    }

    #[tokio::test]
    async fn the_duration_timer_flips_the_shutdown_flag() {
        let (tx, mut rx) = shutdown_channel();
        let _h = spawn_shutdown_trigger(tx, Some(Duration::from_millis(20)));
        tokio::time::timeout(Duration::from_secs(5), wait_for_shutdown(&mut rx))
            .await
            .expect("shutdown must fire when the run duration elapses");
        assert!(*rx.borrow());
    }

    #[tokio::test]
    async fn without_a_duration_the_flag_stays_down() {
        let (tx, mut rx) = shutdown_channel();
        let _h = spawn_shutdown_trigger(tx, None);
        let fired =
            tokio::time::timeout(Duration::from_millis(100), wait_for_shutdown(&mut rx)).await;
        assert!(fired.is_err(), "no duration, no signal: must keep running");
    }

    #[tokio::test]
    async fn the_media_start_wait_ends_on_shutdown_or_close() {
        let cell = &tokio::sync::OnceCell::new();
        let wait = |s, c| async move {
            tokio::time::timeout(Duration::from_secs(5), wait_for_media_start(cell, &s, &c)).await
        };
        let (tx, shutdown) = shutdown_channel();
        let _ = tx.send(true);
        let quiet = crate::transport::ClosedSignal::default().subscribe();
        assert_eq!(wait(shutdown, quiet).await, Ok(None));

        let (_tx, shutdown) = shutdown_channel();
        let closed = crate::transport::ClosedSignal::default();
        let rx = closed.subscribe();
        closed.report(&std::sync::atomic::AtomicBool::new(false), "relay closed");
        assert_eq!(wait(shutdown, rx).await, Ok(None));
    }

    #[tokio::test]
    async fn sleep_or_shutdown_is_cut_short_by_shutdown() {
        let (tx, mut rx) = shutdown_channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(true);
        });
        let started = std::time::Instant::now();
        assert!(sleep_or_shutdown(Duration::from_secs(30), &mut rx).await);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
