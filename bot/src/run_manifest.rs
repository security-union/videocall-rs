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

//! The bot's participant list for the call-quality run manifest.
//!
//! The manifest schema (`call-quality-run-manifest/v1`) is owned by Discussion
//! #2913 (design doc §5.2). This module writes only the per-participant fields
//! the bot knows, under the schema's field names; the scenario runner merges
//! them into the full manifest and adds run-level fields and step membership.

use crate::config::Role;
use crate::netsim::NetworkProfile;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;
use tracing::warn;

pub const MANIFEST_SCHEMA: &str = "call-quality-run-manifest/v1";
pub const FRAGMENT_KIND: &str = "rust-bot-participants";
/// Shortest gap between two interim writes of `--participants-out`.
pub const WRITE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Publishes {
    pub camera: bool,
    pub mic: bool,
    pub screen: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct NetworkRecord {
    pub profile: String,
    pub shaped: bool,
    pub direction: &'static str,
    pub shaper: &'static str,
    pub params: BTreeMap<&'static str, serde_json::Value>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Placement {
    pub node: String,
}

/// One entry of the manifest's `participants` array, as far as the bot knows it.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ParticipantRecord {
    pub user_id: String,
    pub fleet: &'static str,
    pub role: &'static str,
    pub observer: bool,
    pub talker: bool,
    pub publishes: Publishes,
    pub network: NetworkRecord,
    pub transport_intended: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    pub join_ts: Option<f64>,
    pub leave_ts: Option<f64>,
    /// Not in the schema; the scorer ignores unknown fields.
    pub instance_id: Option<String>,
    /// Why the participant left early (`dropped: <reason>`), as the browser
    /// record's `outcome`; absent for a clean leave at shutdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

/// The file written by `--participants-out`.
#[derive(Serialize, Clone, Debug)]
pub struct BotParticipantsFragment {
    pub kind: &'static str,
    pub manifest_schema: &'static str,
    pub meeting_id: String,
    pub id_prefix: Option<String>,
    pub started_at: f64,
    /// When this process released media to its clients.
    pub media_started_at: Option<f64>,
    pub ended_at: Option<f64>,
    /// Participants this process runs.
    pub planned: usize,
    pub participants: Vec<ParticipantRecord>,
}

/// UTC epoch seconds, the manifest's timestamp unit.
pub fn epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Scenario role: `talker` (continuous audio), `speaker` (speech pattern) or
/// `viewer` (mic muted; `publishes.camera` says whether the camera is on).
pub fn role_for(role: Role) -> &'static str {
    match role {
        Role::Presenter => "talker",
        Role::Speaker => "speaker",
        Role::Camera | Role::Viewer => "viewer",
    }
}

/// The network block for a participant: the netsim profile as applied.
pub fn network_record(label: &str, profile: &NetworkProfile) -> NetworkRecord {
    let shaped = !profile.is_passthrough();
    let mut params = BTreeMap::new();
    if shaped {
        params.insert("latency_ms", serde_json::json!(profile.latency_ms));
        params.insert("jitter_ms", serde_json::json!(profile.jitter_ms));
        params.insert("loss_pct", serde_json::json!(profile.loss_pct));
        params.insert("duplicate_pct", serde_json::json!(profile.duplicate_pct));
        params.insert("reorder_pct", serde_json::json!(profile.reorder_pct));
        params.insert("uplink_kbps", serde_json::json!(profile.uplink_kbps));
        params.insert("downlink_kbps", serde_json::json!(profile.downlink_kbps));
    }
    NetworkRecord {
        profile: if shaped {
            label.to_string()
        } else {
            "none".to_string()
        },
        shaped,
        direction: if shaped { "both" } else { "none" },
        shaper: if shaped { "netsim" } else { "none" },
        params,
    }
}

/// Thread-safe holder of the fragment, updated as bots join and leave.
pub struct ParticipantRegistry {
    inner: Mutex<BotParticipantsFragment>,
    changed: Notify,
}

impl ParticipantRegistry {
    pub fn new(
        meeting_id: String,
        id_prefix: Option<String>,
        started_at: f64,
        participants: Vec<ParticipantRecord>,
    ) -> Self {
        Self {
            inner: Mutex::new(BotParticipantsFragment {
                kind: FRAGMENT_KIND,
                manifest_schema: MANIFEST_SCHEMA,
                meeting_id,
                id_prefix,
                started_at,
                media_started_at: None,
                ended_at: None,
                planned: participants.len(),
                participants,
            }),
            changed: Notify::new(),
        }
    }

    fn update(&self, user_id: &str, f: impl FnOnce(&mut ParticipantRecord)) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = inner.participants.iter_mut().find(|p| p.user_id == user_id) {
            f(p);
        }
        drop(inner);
        self.changed.notify_one();
    }

    /// Stamp `media_started_at`; only the first call counts.
    pub fn media_started(&self, ts: f64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.media_started_at.get_or_insert(ts);
        drop(inner);
        self.changed.notify_one();
    }

    pub fn set_instance_id(&self, user_id: &str, instance_id: &str) {
        self.update(user_id, |p| p.instance_id = Some(instance_id.to_string()));
    }

    /// Stamp `join_ts`. The returned [`Presence`] stamps `leave_ts` when it is
    /// left or dropped, so a client that errors out after joining still records
    /// when it went away instead of inheriting the run end.
    pub fn join(self: &Arc<Self>, user_id: &str, ts: f64) -> Presence {
        self.update(user_id, |p| p.join_ts = Some(ts));
        Presence {
            registry: Some(Arc::clone(self)),
            user_id: user_id.to_string(),
        }
    }

    fn set_leave(&self, user_id: &str, ts: f64) {
        self.update(user_id, |p| {
            if p.join_ts.is_some() && p.leave_ts.is_none() {
                p.leave_ts = Some(ts);
            }
        });
    }

    /// Close the run: stamp `ended_at` and a leave time on anyone still joined.
    pub fn finish(&self, ended_at: f64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.ended_at = Some(ended_at);
        for p in &mut inner.participants {
            if p.join_ts.is_some() && p.leave_ts.is_none() {
                p.leave_ts = Some(ended_at);
            }
        }
    }

    /// Stop `writer`, then [`Self::finish`] and write the final file to `path`.
    /// Returns the writer's interim write count.
    pub async fn close(
        &self,
        writer: Option<ParticipantsWriter>,
        path: Option<&Path>,
        ended_at: f64,
    ) -> anyhow::Result<usize> {
        let interim = match writer {
            Some(writer) => writer.stop().await,
            None => 0,
        };
        self.finish(ended_at);
        if let Some(path) = path {
            self.write(path)?;
        }
        Ok(interim)
    }

    pub fn snapshot(&self) -> BotParticipantsFragment {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Write the fragment as pretty JSON, atomically (temp file + rename).
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(&self.snapshot())?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Rewrites `--participants-out` after each registry change, at most once per
/// interval, so an unclean exit still leaves the join times on disk.
pub struct ParticipantsWriter {
    task: JoinHandle<()>,
    stop: oneshot::Sender<()>,
    writes: Arc<AtomicUsize>,
}

impl ParticipantsWriter {
    pub fn spawn(registry: Arc<ParticipantRegistry>, path: PathBuf, interval: Duration) -> Self {
        let (stop, mut stopped) = oneshot::channel();
        let writes = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&writes);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = registry.changed.notified() => {}
                    _ = &mut stopped => return,
                }
                let (reg, out) = (Arc::clone(&registry), path.clone());
                match tokio::task::spawn_blocking(move || reg.write(&out)).await {
                    Ok(Ok(())) => {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Err(e)) => warn!("participant list write to {} failed: {e}", path.display()),
                    Err(e) => warn!("participant list write to {} failed: {e}", path.display()),
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = &mut stopped => return,
                }
            }
        });
        Self { task, stop, writes }
    }

    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::Relaxed)
    }

    /// Stop and wait out any write in flight, so a later write is the last.
    pub async fn stop(self) -> usize {
        let _ = self.stop.send(());
        let _ = self.task.await;
        self.writes.load(Ordering::Relaxed)
    }
}

/// One joined participant; see [`ParticipantRegistry::join`].
pub struct Presence {
    registry: Option<Arc<ParticipantRegistry>>,
    user_id: String,
}

impl Presence {
    pub fn leave(mut self, ts: f64) {
        if let Some(registry) = self.registry.take() {
            registry.set_leave(&self.user_id, ts);
        }
    }

    /// Leave at `ts` because of `reason`, not a run shutdown.
    pub fn drop_out(mut self, ts: f64, reason: &str) {
        self.end(ts, format!("dropped: {reason}"));
    }

    /// Leave at `ts` because the bot itself failed (`failed: <reason>`).
    pub fn fail(mut self, ts: f64, reason: &str) {
        self.end(ts, format!("failed: {reason}"));
    }

    fn end(&mut self, ts: f64, outcome: String) {
        if let Some(registry) = self.registry.take() {
            registry.update(&self.user_id, |p| {
                if p.join_ts.is_some() && p.leave_ts.is_none() {
                    p.leave_ts = Some(ts);
                    p.outcome = Some(outcome);
                }
            });
        }
    }
}

/// Dropped without a leave: the client task panicked or was aborted.
impl Drop for Presence {
    fn drop(&mut self) {
        self.end(epoch_secs(), "aborted".to_string());
    }
}

#[cfg(test)]
pub(crate) fn test_record(user_id: &str, profile: &NetworkProfile) -> ParticipantRecord {
    tests::record(user_id, profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn record(user_id: &str, profile: &NetworkProfile) -> ParticipantRecord {
        ParticipantRecord {
            user_id: user_id.to_string(),
            fleet: "rust",
            role: role_for(Role::Presenter),
            observer: false,
            talker: true,
            publishes: Publishes {
                camera: true,
                mic: true,
                screen: false,
            },
            network: network_record("lossy_mobile", profile),
            transport_intended: "webtransport",
            placement: None,
            join_ts: None,
            leave_ts: None,
            instance_id: None,
            outcome: None,
        }
    }

    #[test]
    fn roles_follow_broadcaster_and_talker_flags() {
        assert_eq!(role_for(Role::Presenter), "talker");
        assert_eq!(role_for(Role::Speaker), "speaker");
        assert_eq!(role_for(Role::Camera), "viewer");
        assert_eq!(role_for(Role::Viewer), "viewer");
    }

    #[test]
    fn an_unshaped_participant_reports_no_shaping() {
        let n = network_record("none", &NetworkProfile::passthrough());
        assert!(!n.shaped);
        assert_eq!(
            (n.direction, n.shaper, n.profile.as_str()),
            ("none", "none", "none")
        );
        assert!(n.params.is_empty());
    }

    #[test]
    fn a_shaped_participant_reports_netsim_both_ways_with_params() {
        let profile = videocall_netsim::resolve_profile("lossy_mobile").unwrap();
        let n = network_record("lossy_mobile", &profile);
        assert!(n.shaped);
        assert_eq!((n.direction, n.shaper), ("both", "netsim"));
        assert_eq!(
            n.params["latency_ms"],
            serde_json::json!(profile.latency_ms)
        );
    }

    #[test]
    fn serialized_fields_use_the_manifest_schema_names() {
        let reg = ParticipantRegistry::new(
            "scale-r1".into(),
            Some("r1".into()),
            1.0,
            vec![record("r1-alice", &NetworkProfile::passthrough())],
        );
        let v: serde_json::Value = serde_json::to_value(reg.snapshot()).unwrap();
        assert_eq!(v["manifest_schema"], MANIFEST_SCHEMA);
        let p = &v["participants"][0];
        for key in [
            "user_id",
            "fleet",
            "role",
            "observer",
            "talker",
            "publishes",
            "network",
            "transport_intended",
            "join_ts",
            "leave_ts",
        ] {
            assert!(p.get(key).is_some(), "missing manifest field {}", key);
        }
        for key in ["camera", "mic", "screen"] {
            assert!(
                p["publishes"].get(key).is_some(),
                "missing publishes.{}",
                key
            );
        }
        for key in ["profile", "shaped", "direction", "shaper", "params"] {
            assert!(p["network"].get(key).is_some(), "missing network.{}", key);
        }
        assert!(
            p.get("placement").is_none(),
            "optional placement omitted when unknown"
        );
    }

    #[test]
    fn join_leave_and_finish_stamp_actual_times() {
        let none = NetworkProfile::passthrough();
        let reg = Arc::new(ParticipantRegistry::new(
            "m".into(),
            None,
            1.0,
            vec![
                record("a", &none),
                record("b", &none),
                record("c", &none),
                record("d", &none),
            ],
        ));
        let a = reg.join("a", 10.0);
        reg.join("b", 11.0).leave(20.0);
        reg.set_leave("c", 21.0); // never joined: no leave time
        let failed_at = epoch_secs();
        drop(reg.join("d", 12.0)); // a client that errored after joining
        reg.finish(failed_at + 1_000.0);
        let snap = reg.snapshot();
        let by = |id: &str| snap.participants.iter().find(|p| p.user_id == id).unwrap();
        let end = failed_at + 1_000.0;
        assert_eq!((by("a").join_ts, by("a").leave_ts), (Some(10.0), Some(end)));
        assert_eq!(
            (by("b").join_ts, by("b").leave_ts),
            (Some(11.0), Some(20.0))
        );
        assert_eq!((by("c").join_ts, by("c").leave_ts), (None, None));
        let d_left = by("d")
            .leave_ts
            .expect("an errored client records its leave");
        assert!(
            d_left < end,
            "leave {} must be the drop time, not the run end",
            d_left
        );
        assert_eq!(by("d").outcome.as_deref(), Some("aborted"));
        assert_eq!(snap.ended_at, Some(end));
        drop(a);
        assert_eq!(reg.snapshot().participants[0].leave_ts, Some(end));
        assert_eq!(reg.snapshot().participants[0].outcome, None);
    }

    #[test]
    fn a_dropped_client_records_the_close_time_and_why() {
        let reg = Arc::new(ParticipantRegistry::new(
            "m".into(),
            None,
            1.0,
            vec![record("a", &NetworkProfile::passthrough())],
        ));
        reg.join("a", 10.0)
            .drop_out(70.0, "relay closed the WebSocket");
        reg.finish(1_800.0);
        let json = serde_json::to_value(reg.snapshot()).unwrap();
        let p = &json["participants"][0];
        assert_eq!(p["leave_ts"], 70.0);
        assert_eq!(p["outcome"], "dropped: relay closed the WebSocket");
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bot-participants-{tag}-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Poll the file until `pred` holds, for up to 10 s of (paused) test time.
    async fn on_disk(path: &Path, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        for _ in 0..1_000 {
            let parsed = std::fs::read_to_string(path)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
            if let Some(v) = parsed.filter(|v| pred(v)) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} never matched", path.display());
    }

    fn registry_of(ids: &[&str]) -> Arc<ParticipantRegistry> {
        let none = NetworkProfile::passthrough();
        let records = ids.iter().map(|id| record(id, &none)).collect();
        Arc::new(ParticipantRegistry::new("m".into(), None, 1.0, records))
    }

    #[tokio::test(start_paused = true)]
    async fn a_join_reaches_the_file_before_the_run_finishes() {
        let reg = registry_of(&["a"]);
        let path = temp_path("join");
        let writer = ParticipantsWriter::spawn(Arc::clone(&reg), path.clone(), WRITE_INTERVAL);
        let _a = reg.join("a", 10.0);
        let v = on_disk(&path, |v| !v["participants"][0]["join_ts"].is_null()).await;
        assert_eq!(v["participants"][0]["join_ts"], 10.0);
        assert!(v["ended_at"].is_null());
        writer.stop().await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(start_paused = true)]
    async fn release_media_stamps_media_started_at_once() {
        let reg = registry_of(&["a"]);
        let path = temp_path("media");
        reg.write(&path).unwrap();
        assert!(on_disk(&path, |_| true).await["media_started_at"].is_null());
        let writer = ParticipantsWriter::spawn(Arc::clone(&reg), path.clone(), WRITE_INTERVAL);
        let cell = tokio::sync::OnceCell::new();
        let before = epoch_secs();
        crate::shutdown::release_media(&cell, &reg);
        assert!(cell.get().is_some());
        let v = on_disk(&path, |v| v["media_started_at"].is_number()).await;
        let first = reg.snapshot().media_started_at.unwrap();
        assert!(first >= before);
        assert!((v["media_started_at"].as_f64().unwrap() - first).abs() < 1e-3);
        reg.media_started(first + 100.0);
        assert_eq!(reg.snapshot().media_started_at, Some(first));
        writer.stop().await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(start_paused = true)]
    async fn writes_are_at_most_one_per_interval_and_the_last_change_lands() {
        let ids: Vec<String> = (0..60).map(|i| format!("b{i:03}")).collect();
        let reg = registry_of(&ids.iter().map(String::as_str).collect::<Vec<_>>());
        let path = temp_path("debounce");
        let start = tokio::time::Instant::now();
        let writer = ParticipantsWriter::spawn(Arc::clone(&reg), path.clone(), WRITE_INTERVAL);
        let mut joined = Vec::new();
        for id in &ids {
            joined.push(reg.join(id, epoch_secs()));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(WRITE_INTERVAL * 2).await;
        for p in joined {
            p.leave(epoch_secs());
        }
        let v = on_disk(&path, |v| {
            v["participants"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["leave_ts"].is_number())
        })
        .await;
        assert_eq!(v["planned"], 60);
        let elapsed = start.elapsed().as_secs_f64();
        let writes = writer.stop().await;
        let bound = (elapsed / WRITE_INTERVAL.as_secs_f64()).floor() as usize + 1;
        assert!(
            (2..=bound).contains(&writes),
            "{} writes in {:.2}s (bound {}) for 120 changes",
            writes,
            elapsed,
            bound
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_write_does_not_stop_later_writes() {
        let reg = registry_of(&["a", "b"]);
        let path = temp_path("write-error");
        let blocker = path.with_extension("tmp");
        std::fs::create_dir(&blocker).unwrap();
        let writer = ParticipantsWriter::spawn(Arc::clone(&reg), path.clone(), WRITE_INTERVAL);
        let _a = reg.join("a", 10.0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!path.exists());
        assert_eq!(writer.writes(), 0);
        std::fs::remove_dir(&blocker).unwrap();
        let _b = reg.join("b", 11.0);
        let v = on_disk(&path, |v| v["participants"][1]["join_ts"] == 11.0).await;
        assert_eq!(v["participants"][0]["join_ts"], 10.0);
        writer.stop().await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(start_paused = true)]
    async fn close_stops_the_writer_then_writes_the_final_file() {
        let reg = registry_of(&["a", "b"]);
        let path = temp_path("close");
        let writer = ParticipantsWriter::spawn(Arc::clone(&reg), path.clone(), WRITE_INTERVAL);
        let _a = reg.join("a", 10.0);
        reg.join("b", 11.0).leave(12.0);
        on_disk(&path, |v| v["participants"][1]["leave_ts"] == 12.0).await;
        let interim = writer.writes();
        assert!(interim >= 1);
        let closed = reg.close(Some(writer), Some(&path), 99.0).await.unwrap();
        assert_eq!(closed, interim);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["ended_at"], 99.0);
        assert_eq!(v["participants"][0]["leave_ts"], 99.0);
        assert_eq!(v["participants"][1]["leave_ts"], 12.0);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_waits_out_a_write_in_flight() {
        let reg = registry_of(&["a"]);
        let path = temp_path("stop");
        let writer = ParticipantsWriter::spawn(Arc::clone(&reg), path.clone(), WRITE_INTERVAL);
        let _a = reg.join("a", 10.0);
        tokio::task::yield_now().await;
        assert_eq!(writer.stop().await, 1);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["participants"][0]["join_ts"], 10.0);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(start_paused = true)]
    async fn an_abort_mid_run_leaves_the_earlier_join_ts_on_disk() {
        let reg = registry_of(&["a", "b"]);
        let path = temp_path("abort");
        let writer = ParticipantsWriter::spawn(Arc::clone(&reg), path.clone(), WRITE_INTERVAL);
        let a = reg.join("a", 10.0);
        let client = tokio::spawn({
            let reg = Arc::clone(&reg);
            async move {
                let _b = reg.join("b", 11.0);
                std::future::pending::<()>().await
            }
        });
        on_disk(&path, |v| v["participants"][1]["join_ts"] == 11.0).await;
        client.abort();
        let _ = client.await;
        on_disk(&path, |v| v["participants"][1]["outcome"] == "aborted").await;
        // The process dies here: `a` never leaves, no finish, no final write.
        std::mem::forget(a);
        tokio::time::sleep(WRITE_INTERVAL * 2).await;
        let v = on_disk(&path, |_| true).await;
        drop(writer);
        let (a, b) = (&v["participants"][0], &v["participants"][1]);
        assert_eq!(
            (a["join_ts"].as_f64(), a["leave_ts"].as_f64()),
            (Some(10.0), None)
        );
        assert_eq!(b["join_ts"], 11.0);
        assert!(b["leave_ts"].is_number());
        assert!(v["ended_at"].is_null());
        assert_eq!(v["planned"], 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn write_produces_parseable_json_atomically() {
        let reg = ParticipantRegistry::new("m".into(), None, 1.0, vec![]);
        let path = std::env::temp_dir().join(format!(
            "bot-participants-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        reg.write(&path).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["kind"], FRAGMENT_KIND);
        assert!(!path.with_extension("tmp").exists());
        let _ = std::fs::remove_file(path);
    }
}
