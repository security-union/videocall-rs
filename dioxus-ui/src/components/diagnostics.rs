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

use crate::components::media_metrics_overlay::{MediaMetricsOverlayCtx, MEDIA_METRICS_OVERLAY_KEY};
use crate::components::neteq_chart::{
    neteq_history_key, push_capped, should_push, single_peer_selected, AdvancedChartType,
    ChartType, NetEqAdvancedChart, NetEqChart, NetEqHistory, NetEqSample, NetEqStatusDisplay,
    UnifiedTimelineChart, NETEQ_SAMPLE_CAP,
};
use crate::components::performance_settings::{
    format_kbps_compact, format_mbps, format_peer_device_lines, format_peer_kind_line,
    format_send_header, format_send_layer, format_send_layer_short, format_send_total_kbps,
    format_simulcast_summary, layer_led_on, layer_quality_label, peers_for_kind,
    received_layer_led_on, DiagnosticsReader, HelpPopover, PerfControlsHandle,
    PerformanceSettingsPanel,
};
use crate::context::{confirm_transport_change, TransportPreference, TransportPreferenceCtx};
use crate::local_storage::save_bool;
use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use dioxus_core::Task;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use videocall_client::{PrefMediaKind, VideoCallClient};
use videocall_diagnostics::{subscribe, MetricValue};

/// #1482: one resolved per-peer device block for the diagnostics "Device (per
/// peer)" section: `(session_id, peer_label, [(row_label, value)])`. The rows
/// come from `format_peer_device_lines`; only present (`Some`) metric fields
/// appear, so an empty rows Vec means the peer reported nothing. Aliased to
/// keep the `SimulcastReceiveBreakdown` prop / local types readable (clippy
/// `type_complexity`).
type DeviceBlock = (u64, String, Vec<(String, String)>);

/// Merged per-(peer, media-kind) reception stats backing the Raw stats →
/// Reception dump (#1222). TWO producers emit subsystem `"video"` events with
/// DIFFERENT metric sets — the diagnostics-manager heartbeat carries
/// `fps_received`/`bitrate_kbps`, while peer_decode_manager's
/// `emit_loss_metrics` carries `video_seq_loss_per_sec`/
/// `keyframe_requests_per_sec`. Rendering each event directly made the FPS
/// line (and the section height) flap as the two shapes alternated. Instead,
/// each field holds the latest value seen for its (peer, kind) key, and the
/// dump renders a FIXED template: every label always present, `-` for fields
/// never observed.
#[derive(Default, Clone, PartialEq)]
struct ReceptionEntry {
    fps: Option<f64>,
    bitrate_kbps: Option<f64>,
    loss_per_sec: Option<f64>,
    kf_req_per_sec: Option<f64>,
    // issue 1656: content staleness (skew-cancelled ms the painted content is
    // behind real-time — #1641's `content_staleness_ms`) and TRUE painted-frame
    // fps, emitted by the worker `"video"` event (which carries `to_peer` but NO
    // `media_type`). Folded under key `(to_peer, "VIDEO")`. `None` until a worker
    // event arrives; the backend sends a literal `0.0` when unknown, but `0.0` is
    // ALSO a legitimate "perfectly on time" reading — indistinguishable from the
    // value alone — so we rely on `None` (no entry yet) for "unknown" and let a
    // genuine `0.0` render once a real worker event has been folded.
    content_staleness_ms: Option<f64>,
    fps_painted: Option<f64>,
    last_ts_ms: u64,
}

/// Fold one `"video"` [`DiagEvent`] into the merged reception map. The key is
/// `(to_peer, media_type)` — `to_peer` is the REMOTE source we receive FROM
/// (`from_peer` is the LOCAL self-id, useless as a label). Returns `false`
/// (no fold) when the event lacks either key component, so malformed events
/// can't create unkeyed entries. Pure / host-testable.
fn update_reception(
    map: &mut BTreeMap<(String, String), ReceptionEntry>,
    evt: &videocall_diagnostics::DiagEvent,
) -> bool {
    let mut peer: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut fps = None;
    let mut bitrate = None;
    let mut loss = None;
    let mut kf = None;
    let mut lag = None;
    let mut painted = None;
    for m in &evt.metrics {
        match (m.name, &m.value) {
            ("to_peer", MetricValue::Text(t)) => peer = Some(t.to_string()),
            ("media_type", MetricValue::Text(t)) => kind = Some(t.to_string()),
            ("fps_received", MetricValue::F64(v)) => fps = Some(*v),
            ("bitrate_kbps", MetricValue::F64(v)) => bitrate = Some(*v),
            ("video_seq_loss_per_sec", MetricValue::F64(v)) => loss = Some(*v),
            ("keyframe_requests_per_sec", MetricValue::F64(v)) => kf = Some(*v),
            // issue 1656: worker `"video"` event metrics (content staleness is
            // #1641's `content_staleness_ms`).
            ("content_staleness_ms", MetricValue::F64(v)) => lag = Some(*v),
            ("fps_painted", MetricValue::F64(v)) => painted = Some(*v),
            _ => {}
        }
    }
    // issue 1656: the worker `"video"` event carries `to_peer` but NO
    // `media_type`, so it would never pass the `(Some(peer), Some(kind))` gate
    // below. When such an event nonetheless carries the worker fields, fold them
    // under the synthetic `"VIDEO"` kind so they merge with the heartbeat block
    // for the same peer. We do NOT synthesize a kind for events that carry only
    // to_peer and no recognized field — those must still `return false`.
    if kind.is_none() && peer.is_some() && (lag.is_some() || painted.is_some()) {
        kind = Some("VIDEO".to_string());
    }
    let (Some(peer), Some(kind)) = (peer, kind) else {
        return false;
    };
    let entry = map.entry((peer, kind)).or_default();
    // Latest-wins per field; fields absent from THIS event keep their prior
    // value (that retention is the whole anti-flap point).
    if let Some(v) = fps {
        entry.fps = Some(v);
    }
    if let Some(v) = bitrate {
        entry.bitrate_kbps = Some(v);
    }
    if let Some(v) = loss {
        entry.loss_per_sec = Some(v);
    }
    if let Some(v) = kf {
        entry.kf_req_per_sec = Some(v);
    }
    // issue 1656: worker fields, latest-wins like the rest. Once a real worker
    // event folds even a genuine `0.0` staleness, the dump shows `0 ms` (not
    // `-`), which is how we resolve the 0.0-vs-unknown ambiguity: `-`/`None`
    // means "no worker event seen yet", any folded value (incl. 0.0) is real.
    if let Some(v) = lag {
        entry.content_staleness_ms = Some(v);
    }
    if let Some(v) = painted {
        entry.fps_painted = Some(v);
    }
    entry.last_ts_ms = evt.ts_ms;
    true
}

/// Render the merged reception map as the fixed-template dump: one block per
/// (peer, kind) in stable sorted order, every line label always present, `-`
/// where a field has never been observed. `None` only when no entry exists
/// yet (the section then shows its own "no data" fallback). Pure.
fn render_reception(map: &BTreeMap<(String, String), ReceptionEntry>) -> Option<String> {
    if map.is_empty() {
        return None;
    }
    fn fmt1(v: Option<f64>) -> String {
        v.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into())
    }
    // issue 1656: content staleness is rounded to whole ms (`{:.0}`) — like the
    // rest of the dump it must be stable sub-second so the subscribe-loop
    // change-gate (which compares rendered strings) isn't defeated by jitter
    // below the displayed precision. `-` for never-observed, matching the dump's
    // hyphen idiom.
    fn fmt0(v: Option<f64>) -> String {
        v.map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into())
    }
    let mut text = String::new();
    for ((peer, kind), e) in map {
        let fps = e
            .fps
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "-".into());
        // Timestamp renders at SECOND granularity on purpose: the change-gate
        // in the subscribe loop compares rendered strings, and a millisecond
        // timestamp changes on every 500ms heartbeat — which would make the
        // dump never byte-identical and defeat the gate entirely (each event
        // would re-render the drawer body for an invisible ms tick).
        // issue 1656: `FPS(painted)` (TRUE painted-frame fps, `{:.1}`) and
        // `Stale` (content staleness — ms the painted content is behind
        // real-time, #1641, whole ms) are NEW lines added to the fixed template
        // — the existing `FPS:` line stays as fps_received.
        text.push_str(&format!(
            "Peer: {peer} ({kind})\nFPS: {fps}\nFPS(painted): {}\nBitrate: {} kbps\nLoss: {}/s\nKeyframe requests: {}/s\nStale: {} ms\nTimestamp: {}s\n\n",
            fmt1(e.fps_painted),
            fmt1(e.bitrate_kbps),
            fmt1(e.loss_per_sec),
            fmt1(e.kf_req_per_sec),
            fmt0(e.content_staleness_ms),
            e.last_ts_ms / 1000,
        ));
    }
    Some(text)
}

/// Decide whether the diagnostics drawer should auto-select the sole peer
/// (FIX 2, #1222). Returns `Some(peer)` only when the user has NOT made a
/// manual selection, the current selection is still the `"All Peers"` default,
/// and exactly one real peer exists. Pure / host-testable.
fn auto_select_peer(current: &str, user_picked: bool, peer_keys: &[String]) -> Option<String> {
    if !user_picked && current == "All Peers" && peer_keys.len() == 1 {
        Some(peer_keys[0].clone())
    } else {
        None
    }
}

/// Quality class + reason for a Per-Peer Summary BUFFER value (Directive 5,
/// #1222). Absolute thresholds (these rows have no per-peer target handy):
/// `0` → poor "starving"; `< 40ms` → warn "low buffer"; else good (no reason).
/// Returns `(class, reason)` where `reason` is `""` for the neutral/good case.
/// Pure / host-testable.
fn peer_buffer_class(buffer_ms: u64) -> (&'static str, &'static str) {
    if buffer_ms == 0 {
        ("is-poor", "starving")
    } else if buffer_ms < 40 {
        ("is-warn", "low buffer")
    } else {
        ("is-good", "")
    }
}

/// Quality class + reason for a Per-Peer Summary JITTER value (Directive 5,
/// #1222). `<= 30ms` → good; `<= 60ms` → warn "elevated jitter"; else poor
/// "high jitter". Returns `(class, reason)` (`""` reason for good). Pure /
/// host-testable.
fn peer_jitter_class(jitter_ms: u64) -> (&'static str, &'static str) {
    if jitter_ms <= 30 {
        ("is-good", "")
    } else if jitter_ms <= 60 {
        ("is-warn", "elevated jitter")
    } else {
        ("is-poor", "high jitter")
    }
}

/// Quality class + reason for a per-peer CONTENT-STALENESS value (ms the painted
/// content is behind real-time — #1641's `content_staleness_ms`; issue 1656).
/// `< 1000ms` → good; `[1000, 1800)ms` → warn "falling behind"; `>= 1800ms` →
/// poor "severely behind real-time". Returns `(class, reason)` (`""` for good).
///
/// PLACEHOLDER thresholds, pending performance-reviewer. The poor boundary
/// (`1800ms`) is a standalone UI display cut meaning "≥1.8s behind real-time =
/// severe / slow motion" — it is the UI's own severity threshold, not derived
/// from any backend constant. The warn boundary (`1000ms`) is an interim
/// midpoint, NOT a localhost-tuned final value; real-network tuning (latency,
/// jitter, mobile) is owned by performance-reviewer. Pure / host-testable.
fn peer_lag_class(lag_ms: f64) -> (&'static str, &'static str) {
    if lag_ms < 1000.0 {
        ("is-good", "")
    } else if lag_ms < PEER_LAG_POOR_MS {
        ("is-warn", "falling behind")
    } else {
        ("is-poor", "severely behind real-time")
    }
}

/// issue 1656: poor content-staleness boundary (ms). The chip turns "severely
/// behind real-time" at this value — a standalone UI display threshold meaning
/// "≥1.8s behind real-time = severe". It is the UI's own severity cut, not
/// derived from or coupled to any backend constant. The
/// `peer_lag_poor_threshold_is_severe_display_value` test pins this value on its
/// own terms so a future edit to `PEER_LAG_POOR_MS` is visible/intentional
/// rather than a silent drift.
const PEER_LAG_POOR_MS: f64 = 1800.0;

/// issue 1656: per-peer VIDEO worker/heartbeat readout threaded from the
/// subscribe loop into the receive breakdown + Per-Peer Summary triage chip.
/// Each field is `Option` so a peer absent from the map (or a `None` field)
/// renders the em-dash "unknown" idiom, while a folded `0.0` renders a real
/// value. The three metrics arrive from TWO producers on the SAME subsystem
/// `"video"` bus: the diagnostics-manager HEARTBEAT carries `fps_received`
/// (+ `media_type`), the worker event carries `content_staleness_ms` (#1641) +
/// `fps_painted` (NO `media_type`). Both key by `to_peer`. Because a heartbeat
/// updates only `recv_fps` and a worker updates only `lag_ms` (= content
/// staleness) / `painted_fps`, the fold is PER-FIELD latest-wins (see
/// [`fold_video_readout`]).
///
/// `recv_fps` is VIDEO-only. The heartbeat fires for BOTH VIDEO and SCREEN with
/// the SAME `to_peer`, so [`fold_video_readout`] folds `fps_received` into
/// `recv_fps` ONLY when `media_type == "VIDEO"` (or is absent, as on the worker
/// event); a SCREEN heartbeat is ignored for this readout and never sets
/// `recv_fps`. `lag_ms`/`painted_fps` come from the worker event (which carries
/// no `media_type` and is structurally the video pipeline), so they are not
/// affected by the media-type filter.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct PeerVideoReadout {
    recv_fps: Option<f64>,
    lag_ms: Option<f64>,
    painted_fps: Option<f64>,
}

/// issue 1656: fold one `"video"` [`DiagEvent`]'s worker/heartbeat fields into
/// the per-peer readout map, per-field latest-wins. Returns `true` IFF a value
/// actually changed (so the caller can change-gate the signal `.set()` and not
/// wake the drawer on every ~1 Hz tick). A field is overwritten ONLY when its
/// metric is `Some` on THIS event — a heartbeat (recv only) must not clobber a
/// prior worker's staleness/painted, and vice-versa. Rounds to the displayed
/// precision (recv/painted to 1 decimal, staleness to whole ms) so jitter below
/// the shown digits can't defeat the change-gate. Returns `false` (no change)
/// for an event lacking `to_peer` or any of the three metrics. Pure /
/// host-testable.
///
/// `media_type` filter (issue 1656): `recv_fps` is the VIDEO (camera) pipeline
/// ONLY. The heartbeat producer emits this `"video"` subsystem event for BOTH
/// VIDEO and SCREEN (it excludes only AUDIO —
/// diagnostics_manager.rs:463-477) with the SAME `to_peer`, so a SCREEN
/// heartbeat's `fps_received` would otherwise last-writer-wins over the camera's
/// in the same `to_peer`-keyed entry. We therefore DROP `recv` whenever the
/// event explicitly carries a non-VIDEO `media_type`. An ABSENT `media_type`
/// (the worker event carries none) is treated as the video pipeline and KEEPS
/// recv. `lag_ms`/`painted_fps` carry no `media_type` and are emitted only by
/// the worker event, so the filter does not touch them. A SCREEN-only heartbeat
/// (whose sole relevant metric was `fps_received`) thus folds nothing and
/// returns `false` without creating or dirtying an entry.
fn fold_video_readout(
    map: &mut HashMap<String, PeerVideoReadout>,
    evt: &videocall_diagnostics::DiagEvent,
) -> bool {
    let mut peer: Option<String> = None;
    let mut recv: Option<f64> = None;
    let mut lag: Option<f64> = None;
    let mut painted: Option<f64> = None;
    let mut media_type: Option<String> = None;
    for m in &evt.metrics {
        match (m.name, &m.value) {
            ("to_peer", MetricValue::Text(t)) => peer = Some(t.to_string()),
            ("media_type", MetricValue::Text(t)) => media_type = Some(t.to_string()),
            ("fps_received", MetricValue::F64(v)) => recv = Some((*v * 10.0).round() / 10.0),
            // issue 1656: content staleness is #1641's `content_staleness_ms`.
            ("content_staleness_ms", MetricValue::F64(v)) => lag = Some((*v).round()),
            ("fps_painted", MetricValue::F64(v)) => painted = Some((*v * 10.0).round() / 10.0),
            _ => {}
        }
    }
    let Some(peer) = peer else {
        return false;
    };
    // issue 1656 (media_type filter): `recv_fps` comes from the heartbeat, which
    // emits a `"video"` subsystem event for BOTH VIDEO and SCREEN (it excludes
    // only AUDIO — see diagnostics_manager.rs:463-477) with the SAME
    // `to_peer = peer_id`. This readout is the camera/VIDEO pipeline only, so a
    // SCREEN heartbeat's `fps_received` must NOT land in `recv_fps` (last-writer
    // -wins would otherwise show the screen's recv fps on the camera row). DROP
    // recv whenever the event explicitly tags a non-VIDEO `media_type`. ABSENT
    // media_type (the worker event carries none) is treated as the video
    // pipeline → recv is kept. lag/painted are unaffected: they arrive only on
    // the worker event, which carries no media_type and no recv, so there is no
    // cross-contamination on those fields.
    if let Some(mt) = &media_type {
        if mt != "VIDEO" {
            recv = None;
        }
    }
    // Nothing of interest left to fold on this event (e.g. the loss/keyframe
    // `"video"` event, or a SCREEN heartbeat whose only relevant metric was the
    // now-dropped `fps_received`) → leave the map untouched, no change, no entry
    // created.
    if recv.is_none() && lag.is_none() && painted.is_none() {
        return false;
    }
    let entry = map.entry(peer).or_default();
    let before = *entry;
    if recv.is_some() {
        entry.recv_fps = recv;
    }
    if lag.is_some() {
        entry.lag_ms = lag;
    }
    if painted.is_some() {
        entry.painted_fps = painted;
    }
    *entry != before
}

// Serializable versions of DiagEvent structures
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SerializableDiagEvent {
    pub subsystem: String,
    pub stream_id: Option<String>,
    pub ts_ms: u64,
    pub metrics: Vec<SerializableMetric>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SerializableMetric {
    pub name: String,
    pub value: MetricValue,
}

impl From<videocall_diagnostics::DiagEvent> for SerializableDiagEvent {
    fn from(event: videocall_diagnostics::DiagEvent) -> Self {
        Self {
            subsystem: event.subsystem.to_string(),
            stream_id: event.stream_id,
            ts_ms: event.ts_ms,
            metrics: event
                .metrics
                .into_iter()
                .map(|m| SerializableMetric {
                    name: m.name.to_string(),
                    value: m.value,
                })
                .collect(),
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ConnectionManagerState {
    pub election_state: String,
    pub election_progress: Option<f64>,
    pub servers_total: Option<u64>,
    /// Total configured servers (WS + WT URLs) the manager was set up with,
    /// independent of the `ElectionState`. Emitted as `configured_servers_total`
    /// from the connection-manager bus. Phase 7 (discussion 562).
    pub configured_servers_total: Option<u64>,
    /// `true` when the manager has at most one configured server. The UI
    /// renders a "Limited connectivity" badge while this is set, since
    /// re-elections are gated on having multiple candidates. Phase 7
    /// (discussion 562).
    pub single_server_only: Option<bool>,
    pub active_connection_id: Option<String>,
    pub active_server_url: Option<String>,
    pub active_server_type: Option<String>,
    pub active_server_rtt: Option<f64>,
    pub failure_reason: Option<String>,
    pub servers: Vec<ServerInfo>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub connection_id: String,
    pub url: String,
    pub server_type: String,
    pub status: String,
    pub rtt: Option<f64>,
    pub active: bool,
    pub connected: bool,
    pub measurement_count: Option<u64>,
}

impl Default for ConnectionManagerState {
    fn default() -> Self {
        Self {
            election_state: "unknown".to_string(),
            election_progress: None,
            servers_total: None,
            configured_servers_total: None,
            single_server_only: None,
            active_connection_id: None,
            active_server_url: None,
            active_server_type: None,
            active_server_rtt: None,
            failure_reason: None,
            servers: Vec::new(),
        }
    }
}

impl ConnectionManagerState {
    pub fn from_serializable_events(events: &[SerializableDiagEvent]) -> Self {
        let mut state = Self::default();
        for event in events {
            if event.subsystem != "connection_manager" {
                continue;
            }
            if event.stream_id.is_none() {
                Self::process_main_event(event, &mut state);
            } else if let Some(connection_id) = &event.stream_id {
                if let Some(server) = Self::process_server_event(event, connection_id) {
                    if let Some(existing) = state
                        .servers
                        .iter_mut()
                        .find(|s| s.connection_id == server.connection_id)
                    {
                        *existing = server;
                    } else {
                        state.servers.push(server);
                    }
                }
            }
        }
        state
            .servers
            .sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
        state
    }

    fn process_main_event(event: &SerializableDiagEvent, state: &mut ConnectionManagerState) {
        for metric in &event.metrics {
            match metric.name.as_str() {
                "election_state" => {
                    if let MetricValue::Text(text) = &metric.value {
                        state.election_state = text.to_string();
                    }
                }
                "election_progress" => {
                    if let MetricValue::F64(progress) = &metric.value {
                        state.election_progress = Some(*progress);
                    }
                }
                "servers_total" => {
                    if let MetricValue::U64(total) = &metric.value {
                        state.servers_total = Some(*total);
                    }
                }
                "configured_servers_total" => {
                    if let MetricValue::U64(total) = &metric.value {
                        state.configured_servers_total = Some(*total);
                    }
                }
                "single_server_only" => {
                    if let MetricValue::U64(flag) = &metric.value {
                        // Encoded as u64-bool to match the
                        // `server_active`/`server_connected` convention.
                        state.single_server_only = Some(*flag != 0);
                    }
                }
                "active_connection_id" => {
                    if let MetricValue::Text(id) = &metric.value {
                        state.active_connection_id = Some(id.to_string());
                    }
                }
                "active_server_url" => {
                    if let MetricValue::Text(url) = &metric.value {
                        state.active_server_url = Some(url.to_string());
                    }
                }
                "active_server_type" => {
                    if let MetricValue::Text(server_type) = &metric.value {
                        state.active_server_type = Some(server_type.to_string());
                    }
                }
                "active_server_rtt" => {
                    if let MetricValue::F64(rtt) = &metric.value {
                        state.active_server_rtt = Some(*rtt);
                    }
                }
                "failure_reason" => {
                    if let MetricValue::Text(reason) = &metric.value {
                        state.failure_reason = Some(reason.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    fn process_server_event(
        event: &SerializableDiagEvent,
        connection_id: &str,
    ) -> Option<ServerInfo> {
        let mut server = ServerInfo {
            connection_id: connection_id.to_string(),
            url: "unknown".to_string(),
            server_type: "unknown".to_string(),
            status: "unknown".to_string(),
            rtt: None,
            active: false,
            connected: false,
            measurement_count: None,
        };
        for metric in &event.metrics {
            match metric.name.as_str() {
                "server_url" => {
                    if let MetricValue::Text(url) = &metric.value {
                        server.url = url.to_string();
                    }
                }
                "server_type" => {
                    if let MetricValue::Text(st) = &metric.value {
                        server.server_type = st.to_string();
                    }
                }
                "server_status" => {
                    if let MetricValue::Text(status) = &metric.value {
                        server.status = status.to_string();
                    }
                }
                "server_rtt" => {
                    if let MetricValue::F64(rtt) = &metric.value {
                        server.rtt = Some(*rtt);
                    }
                }
                "server_active" => {
                    if let MetricValue::U64(active) = &metric.value {
                        server.active = *active > 0;
                    }
                }
                "server_connected" => {
                    if let MetricValue::U64(connected) = &metric.value {
                        server.connected = *connected > 0;
                    }
                }
                "measurement_count" => {
                    if let MetricValue::U64(count) = &metric.value {
                        server.measurement_count = Some(*count);
                    }
                }
                _ => {}
            }
        }
        Some(server)
    }
}

#[component]
pub fn ConnectionManagerDisplay(connection_manager_state: Option<String>) -> Element {
    let parsed_state = connection_manager_state.as_ref().map(|json| {
        let events: Vec<SerializableDiagEvent> = serde_json::from_str(json).unwrap_or_default();
        ConnectionManagerState::from_serializable_events(&events)
    });

    let common_styles = include_str!("diagnostics_cm_styles.css");

    if let Some(state) = parsed_state {
        let election_upper = state.election_state.to_uppercase();
        let status_class = format!("status-value status-{}", state.election_state);
        rsx! {
            style { "{common_styles}" }
            div { class: "connection-manager-display",
                div { class: "connection-status",
                    h4 { "Connection Status" }
                    div { class: "status-grid",
                        div { class: "status-item",
                            span { class: "status-label", "State:" }
                            span { class: "{status_class}", "{election_upper}" }
                        }
                        if let Some(progress) = state.election_progress {
                            if state.election_state == "testing" {
                                {
                                    let progress_pct = (progress * 100.0).min(100.0);
                                    let progress_pct_str = format!("{progress_pct:.0}%");
                                    rsx! {
                                        div { class: "status-item",
                                            span { class: "status-label", "Progress:" }
                                            div { class: "progress-container",
                                                div { class: "progress-bar",
                                                    div { class: "progress-fill", style: "width: {progress_pct}%",
                                                    }
                                                }
                                                span { class: "progress-text", "{progress_pct_str}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(total) = state.servers_total {
                            div { class: "status-item",
                                span { class: "status-label", "Total Servers:" }
                                span { class: "status-value", "{total}" }
                            }
                        }
                        if let Some(total) = state.configured_servers_total {
                            div { class: "status-item",
                                span { class: "status-label", "Configured Servers:" }
                                span { class: "status-value", "{total}" }
                            }
                        }
                    }
                    if state.single_server_only == Some(true) {
                        // Phase 7 (discussion 562): when only one (or zero)
                        // candidates are configured, the watchdog suppresses
                        // re-election (it would re-elect onto the same host).
                        // Surface a badge so the user knows recovery is gated
                        // and isn't just a silent stall.
                        div { class: "limited-connectivity-badge",
                            role: "alert",
                            aria_live: "polite",
                            span { class: "badge-icon", "!" }
                            span { class: "badge-text",
                                "Limited connectivity \u{2014} only 1 server reachable. \
                                 Re-elections disabled."
                            }
                        }
                    }
                }
                if state.election_state == "elected" {
                    div { class: "active-connection",
                        h4 { "Active Connection" }
                        div { class: "connection-details",
                            if let Some(url) = &state.active_server_url {
                                div { class: "detail-item",
                                    span { class: "detail-label", "Server:" }
                                    span { class: "detail-value server-url", "{url}" }
                                }
                            }
                            if let Some(server_type) = &state.active_server_type {
                                {
                                    let st_upper = server_type.to_uppercase();
                                    let type_class = format!("detail-value connection-type type-{server_type}");
                                    rsx! {
                                        div { class: "detail-item",
                                            span { class: "detail-label", "Type:" }
                                            span { class: "{type_class}", "{st_upper}" }
                                        }
                                    }
                                }
                            }
                            if let Some(rtt) = state.active_server_rtt {
                                {
                                    let rtt_class = if rtt < 50.0 { "rtt-good" } else if rtt < 150.0 { "rtt-ok" } else { "rtt-poor" };
                                    let rtt_str = format!("{rtt:.1}ms");
                                    let full_class = format!("detail-value rtt-value {rtt_class}");
                                    rsx! {
                                        div { class: "detail-item",
                                            span { class: "detail-label", "RTT:" }
                                            span { class: "{full_class}", "{rtt_str}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !state.servers.is_empty() {
                    div { class: "servers-list",
                        h4 { "Servers" }
                        div { class: "servers-grid",
                            for server in state.servers.iter() {
                                {
                                    let card_class = if server.active { "server-card server-active" } else { "server-card" };
                                    let status_emoji = match server.status.as_str() {
                                        "connecting" => "\u{231b}",
                                        "connected" => "\u{1f517}",
                                        "testing" => "\u{1f50d}",
                                        "active" => "\u{2705}",
                                        _ => "\u{2753}",
                                    };
                                    let st_upper = server.server_type.to_uppercase();
                                    let type_class = format!("server-type type-{}", server.server_type);
                                    rsx! {
                                        div { class: "{card_class}",
                                            div { class: "server-header",
                                                span { class: "server-id", "{server.connection_id}" }
                                                div { class: "server-indicators",
                                                    if server.active {
                                                        span { class: "indicator active-indicator", title: "Active", "\u{25cf}" }
                                                    }
                                                    span { class: "indicator status-indicator", title: "{server.status}", "{status_emoji}" }
                                                }
                                            }
                                            div { class: "server-details",
                                                div { class: "server-url", "{server.url}" }
                                                div { class: "server-info",
                                                    span { class: "{type_class}", "{st_upper}" }
                                                    if let Some(rtt) = server.rtt {
                                                        {
                                                            let rtt_class = if rtt < 50.0 { "rtt-good" } else if rtt < 150.0 { "rtt-ok" } else { "rtt-poor" };
                                                            let rtt_str = format!("{rtt:.1}ms");
                                                            rsx! {
                                                                span { class: "server-rtt {rtt_class}", "{rtt_str}" }
                                                            }
                                                        }
                                                    } else {
                                                        span { class: "server-rtt no-rtt", "\u{2014}" }
                                                    }
                                                    if let Some(count) = server.measurement_count {
                                                        if count > 0 {
                                                            span { class: "measurement-count", title: "RTT measurements", "{count}\u{1f4ca}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if state.election_state == "failed" {
                    div { class: "connection-error",
                        h4 { "Connection Failed" }
                        if let Some(reason) = &state.failure_reason {
                            p { class: "error-reason", "{reason}" }
                        } else {
                            p { class: "error-reason", "Unknown error occurred" }
                        }
                    }
                }
            }
        }
    } else {
        rsx! {
            style { "{common_styles}" }
            div { class: "connection-manager-display",
                p { class: "no-data", "No connection manager data available" }
            }
        }
    }
}

#[component]
pub fn Diagnostics(
    is_open: bool,
    on_close: EventHandler<()>,
    video_enabled: bool,
    mic_enabled: bool,
    share_screen: bool,
    encoder_settings: Option<String>,
    /// Live SEND/RECEIVE simulcast reader for the "Simulcast layers" section,
    /// published by `Host` (which owns the encoders). `None` until Host mounts /
    /// when diagnostics aren't wired. (#1095 §6 MOVE)
    #[props(default)]
    diagnostics_reader: Option<DiagnosticsReader>,
    /// Performance controls handle (sliders/Auto/meters) published by `Host`, for
    /// the migrated Performance panel in the drawer's "Quality controls" group.
    /// `None` until Host mounts / when not wired → the controls group renders
    /// nothing. (#1131 unify)
    #[props(default)]
    perf_controls: Option<PerfControlsHandle>,
    /// Current drawer width in px, owned by the parent so it can persist the
    /// drag-resized width. (#1296)
    width: f64,
    /// Fired on resize-handle pointerdown so the parent can begin a drag. (#1296)
    on_resize_start: EventHandler<()>,
    /// Fired on each resize-handle pointermove, carrying the pointer's `client_x`.
    /// The parent owns the width signals + clamp, so the math lives there. (#1296)
    on_resize_move: EventHandler<f64>,
    /// Fired on resize-handle pointerup so the parent can persist + end the drag.
    /// (#1296)
    on_resize_end: EventHandler<()>,
) -> Element {
    let transport_pref_ctx = use_context::<TransportPreferenceCtx>();
    // Issue 1768: the shared "Show media metrics on tiles" flag. The checkbox
    // below writes it (and persists to localStorage); every PeerTile reads the
    // same signal to show/hide its overlay.
    let mut media_metrics_overlay_enabled = use_context::<MediaMetricsOverlayCtx>().0;
    let mut selected_peer = use_signal(|| "All Peers".to_string());
    // FIX 2: tracks whether the user has explicitly chosen a peer in the
    // selector. The one-shot auto-select effect (below) only fires while this
    // is false, so an automatic pick never fights a manual one.
    let mut user_picked_peer = use_signal(|| false);
    let mut diagnostics_data = use_signal(|| None::<String>);
    let mut sender_stats = use_signal(|| None::<String>);
    let mut connection_manager_state = use_signal(|| None::<String>);
    // Per-peer ring buffer of PARSED, compact NetEq samples (parse-once at
    // arrival in the subscribe loop). Replaces the old `Vec<String>` of raw JSON
    // that the render path re-parsed every event (#1223). Capped at 2 hours.
    let mut neteq_stats_per_peer = use_signal(HashMap::<String, VecDeque<NetEqSample>>::new);
    let mut neteq_buffer_per_peer = use_signal(HashMap::<String, Vec<u64>>::new);
    let mut neteq_jitter_per_peer = use_signal(HashMap::<String, Vec<u64>>::new);
    let mut peer_transport_per_peer = use_signal(HashMap::<String, String>::new);
    // issue 1656: per-peer VIDEO readout — recv fps + content staleness + painted
    // fps (see `PeerVideoReadout`), each `Option` so a peer absent from the map (or a
    // `None` field) renders the em-dash "unknown" in the Receive breakdown /
    // triage chip, while a folded `0.0` renders a real value. Keyed by
    // `Peer::sid_str` (u64-as-String) == the worker/heartbeat event's `to_peer`.
    // `.set()` is change-gated in the subscribe loop (mirrors
    // `peer_transport_per_peer`) so a 1 Hz event doesn't wake the drawer.
    let mut peer_lag_fps_per_peer = use_signal(HashMap::<String, PeerVideoReadout>::new);
    let mut diag_task = use_signal(|| None::<Task>);
    let mut backend_versions = use_signal(Vec::<serde_json::Value>::new);

    // Subscribe to diagnostics events using Dioxus `spawn`.
    // `spawn` runs within the Dioxus runtime so signal mutations properly
    // trigger re-renders.  We explicitly cancel the previous Task on each
    // re-run to prevent double-subscriptions (open → close → open).
    use_effect(move || {
        // Cancel any previous subscription task.
        if let Some(task) = *diag_task.peek() {
            task.cancel();
        }

        if !is_open {
            diagnostics_data.set(None);
            sender_stats.set(None);
            connection_manager_state.set(None);
            neteq_stats_per_peer.set(HashMap::new());
            neteq_buffer_per_peer.set(HashMap::new());
            neteq_jitter_per_peer.set(HashMap::new());
            peer_transport_per_peer.set(HashMap::new());
            peer_lag_fps_per_peer.set(HashMap::new());
            diag_task.set(None);
            return;
        }

        let task = spawn(async move {
            let mut rx = subscribe();
            let mut connection_events = Vec::<SerializableDiagEvent>::new();
            // Per-peer last-kept timestamp for the ≤1 Hz throttle. Different
            // peers throttle independently (like `peer_transport` below). This is
            // the only NetEq loop-local left: the parsed-once per-peer ring
            // buffers themselves now live in the signals and are mutated IN PLACE
            // via `with_mut` (no full-map clone per kept sample — #1223 B1).
            //
            // S3 — peer-departure retention: departed peers' deques are RETAINED
            // until the drawer closes (the `!is_open` teardown above clears every
            // map with `.set(HashMap::new())`). There is no per-event eviction
            // here because the diagnostics bus this loop consumes carries NO
            // peer-departure/disconnect event — the subsystems this loop actually
            // matches (the only arms below) are: video, sender, neteq,
            // peer_status, and connection_manager. None of them reports a peer
            // "left" event. Peer removal
            // in videocall-client fires a Rust `Callback<String>`
            // (peer_decode_manager `on_peer_removed`/`delete_peer`), NOT a
            // `DiagEvent`, and `peer_status` only reports media-enabled state +
            // transport WHILE the peer exists (no "left" field). So there is no
            // departure signal to subscribe to here; retention-until-close is
            // intentional and bounded — maps reset on close and each deque is
            // capped at 7200 samples/peer. Do NOT add a new event channel.
            let mut last_push_ms = HashMap::<String, u64>::new();
            // Per-peer transport label, locally cached. peer_status events
            // arrive on every heartbeat (~periodic), so we only push to the
            // signal when the value actually changes — heartbeat ticks must
            // not cause UI re-renders.
            let mut peer_transport = HashMap::<String, String>::new();
            // Merged per-(peer, kind) reception stats (see ReceptionEntry):
            // loop-local like `last_push_ms`, so it resets on drawer reopen
            // along with everything else.
            let mut reception = BTreeMap::<(String, String), ReceptionEntry>::new();
            // issue 1656: per-peer VIDEO readout (recv fps + lag + painted fps,
            // see `PeerVideoReadout`), locally cached. The heartbeat AND worker
            // `"video"` events each arrive ~1 Hz from DIFFERENT producers, so we
            // fold both into this map per-field latest-wins and only push to the
            // signal when a peer's readout actually changes — mirrors
            // `peer_transport` to keep the drawer body from re-rendering on every
            // tick. Keyed by `to_peer` (u64-as-String).
            let mut peer_lag_fps = HashMap::<String, PeerVideoReadout>::new();

            while let Ok(evt) = rx.recv().await {
                match evt.subsystem {
                    // The receiver feed is subsystem "video", emitted by TWO
                    // producers with different metric sets (heartbeat fps/
                    // bitrate from diagnostics_manager; ~1Hz loss/keyframe
                    // rates from peer_decode_manager). Events are folded into
                    // the merged `reception` map and re-rendered as a fixed
                    // template so line labels never appear/disappear.
                    // issue 1656: UNGATED arm. The worker `"video"` event (which
                    // carries the per-peer lag/painted-fps but does NOT change
                    // the reception dump string) must still reach the per-peer
                    // map update below; gating the whole arm on
                    // `update_reception` (as before) would skip the worker map
                    // insert whenever the dump byte-output was unchanged. So the
                    // fold + dump change-gate now live INSIDE the arm, and the
                    // per-peer map update runs regardless of the fold's verdict.
                    "video" => {
                        if update_reception(&mut reception, &evt) {
                            if let Some(text) = render_reception(&reception) {
                                // Change-gate (mirrors the peer_status arm):
                                // skip the set() when the dump is unchanged so
                                // the drawer body doesn't re-render per event.
                                // This works ONLY because render_reception emits
                                // the timestamp at second granularity — at ms
                                // granularity every heartbeat would produce a
                                // distinct string and the gate would never
                                // suppress (pinned by the gate-effectiveness
                                // test). `.peek()` reads without subscribing.
                                if diagnostics_data.peek().as_deref() != Some(text.as_str()) {
                                    diagnostics_data.set(Some(text));
                                }
                            }
                        }
                        // issue 1656: per-peer VIDEO readout. Fold `to_peer` +
                        // `fps_received` (heartbeat) / `content_staleness_ms` (#1641)
                        // + `fps_painted` (worker) per-field latest-wins via the
                        // pure helper, which rounds to the displayed precision so
                        // sub-second jitter can't defeat the change-gate, and
                        // returns whether a value actually changed. Push to the
                        // signal only on a real change (mirrors `peer_transport`).
                        if fold_video_readout(&mut peer_lag_fps, &evt) {
                            peer_lag_fps_per_peer.set(peer_lag_fps.clone());
                        }
                    }
                    "sender" => {
                        let mut text = String::new();
                        for m in &evt.metrics {
                            match m.name {
                                "sender_id" => {
                                    if let MetricValue::Text(v) = &m.value {
                                        text.push_str(&format!("Sender: {v}\n"));
                                    }
                                }
                                "target_id" => {
                                    if let MetricValue::Text(v) = &m.value {
                                        text.push_str(&format!("Target: {v}\n"));
                                    }
                                }
                                "media_type" => {
                                    if let MetricValue::Text(v) = &m.value {
                                        text.push_str(&format!("Media Type: {v}\n"));
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !text.is_empty() {
                            text.push_str(&format!("Timestamp: {}\n", evt.ts_ms));
                            sender_stats.set(Some(text));
                        }
                    }
                    "neteq" => {
                        let stream_id = evt
                            .stream_id
                            .clone()
                            .unwrap_or_else(|| "unknown->unknown".to_string());
                        let parts: Vec<&str> = stream_id.split("->").collect();
                        let target_peer = if parts.len() == 2 {
                            parts[1]
                        } else {
                            "unknown"
                        };
                        for m in &evt.metrics {
                            match m.name {
                                "stats_json" => {
                                    if let MetricValue::Text(json) = &m.value {
                                        // Throttle to ≤1 sample/sec PER PEER: skip
                                        // if <1000ms since this peer's last kept
                                        // sample. Different peers are independent.
                                        let last = last_push_ms.get(target_peer).copied();
                                        if should_push(last, evt.ts_ms) {
                                            // Parse ONCE here (not in render). A
                                            // malformed frame is dropped (None) so
                                            // it can't poison the ring buffer.
                                            if let Some(sample) =
                                                NetEqSample::from_json(json, evt.ts_ms)
                                            {
                                                // Push IN PLACE into the signal's
                                                // map (B1): `with_mut` mutates the
                                                // backing map and marks the signal
                                                // dirty itself — no full-map clone
                                                // per kept sample. The closure is
                                                // SYNCHRONOUS (no `.await` inside),
                                                // so the borrow drops before the
                                                // next `rx.recv().await`. (#1223)
                                                neteq_stats_per_peer.with_mut(|m| {
                                                    let dq = m
                                                        .entry(target_peer.to_string())
                                                        .or_default();
                                                    // pop_front-then-push_back at
                                                    // the 2-hour cap (decision 2).
                                                    push_capped(dq, sample);
                                                });
                                                last_push_ms
                                                    .insert(target_peer.to_string(), evt.ts_ms);
                                            }
                                        }
                                    }
                                }
                                // `audio_buffer_ms` / `jitter_buffer_delay_ms` are
                                // a SEPARATE per-peer feed (not derived from the
                                // NetEq `stats_json` sample), so they keep their
                                // own small 50-cap ring buffers that back the
                                // Buffer/Jitter FALLBACK charts shown before any
                                // full NetEq history exists. Pushed in place via
                                // `with_mut` (sync closure, no `.await`). (#1223)
                                "audio_buffer_ms" => {
                                    if let MetricValue::U64(v) = &m.value {
                                        neteq_buffer_per_peer.with_mut(|m| {
                                            let dq = m.entry(target_peer.to_string()).or_default();
                                            dq.push(*v);
                                            if dq.len() > 50 {
                                                dq.remove(0);
                                            }
                                        });
                                    }
                                }
                                "jitter_buffer_delay_ms" => {
                                    if let MetricValue::U64(v) = &m.value {
                                        neteq_jitter_per_peer.with_mut(|m| {
                                            let dq = m.entry(target_peer.to_string()).or_default();
                                            dq.push(*v);
                                            if dq.len() > 50 {
                                                dq.remove(0);
                                            }
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "peer_status" => {
                        let mut peer_id: Option<String> = None;
                        let mut transport: Option<String> = None;
                        for m in &evt.metrics {
                            match m.name {
                                "to_peer" => {
                                    if let MetricValue::Text(t) = &m.value {
                                        peer_id = Some(t.to_string());
                                    }
                                }
                                "peer_transport" => {
                                    if let MetricValue::Text(t) = &m.value {
                                        transport = Some(t.to_string());
                                    }
                                }
                                _ => {}
                            }
                        }
                        if let (Some(p), Some(t)) = (peer_id, transport) {
                            // Only push to the signal when the value
                            // actually changes; otherwise we'd re-render
                            // on every heartbeat tick.
                            let changed = match peer_transport.get(&p) {
                                Some(prev) => prev != &t,
                                None => true,
                            };
                            if changed {
                                peer_transport.insert(p, t);
                                peer_transport_per_peer.set(peer_transport.clone());
                            }
                        }
                    }
                    "connection_manager" => {
                        connection_events.push(SerializableDiagEvent::from(evt));
                        if connection_events.len() > 20 {
                            connection_events.remove(0);
                        }
                        let serialized =
                            serde_json::to_string(&connection_events).unwrap_or_default();
                        connection_manager_state.set(Some(serialized));
                    }
                    _ => {}
                }
            }
        });
        diag_task.set(Some(task));
    });

    // Fetch aggregated version info from meeting-api when the panel opens.
    use_effect(move || {
        if !is_open {
            backend_versions.set(Vec::new());
            return;
        }
        spawn(async move {
            if let Ok(base_url) = crate::constants::meeting_api_base_url() {
                let url = format!("{base_url}/api/v1/versions");
                if let Ok(resp) = reqwest::get(&url).await {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(components) = body["components"].as_array() {
                            backend_versions.set(components.clone());
                        }
                    }
                }
            }
        });
    });

    // FIX 2 — auto-select the sole peer in a 1:1 call so the NetEq Current
    // Status + charts render by default instead of the "All Peers" placeholder.
    // This is a guarded one-shot: it reads `neteq_stats_per_peer` INSIDE the
    // closure (so the effect re-runs when peers appear) but `.peek()`s
    // selected_peer + user_picked_peer so the `selected_peer.set(...)` below
    // does NOT retrigger this effect. `Signal::set` is synchronous, so the
    // peeks (which read current values) happen BEFORE the set. The decision is
    // delegated to the pure `auto_select_peer` helper (host-tested): it only
    // fires when the user hasn't picked, the current selection is still the
    // "All Peers" default, and exactly one peer exists.
    use_effect(move || {
        let map = neteq_stats_per_peer(); // subscribe to peer-map changes
        let keys: Vec<String> = map
            .keys()
            .filter(|k| k.as_str() != "All Peers")
            .cloned()
            .collect();
        let cur = selected_peer.peek().clone(); // peek BEFORE set
        let picked = *user_picked_peer.peek();
        if let Some(k) = auto_select_peer(&cur, picked, &keys) {
            selected_peer.set(k);
        }
    });

    // The live "Simulcast layers" section runs its OWN 250 ms (≈4 Hz) refresh
    // tick, scoped to its child component `SimulcastLayersSection` (below), so the
    // tick re-renders ONLY that small subtree — NOT this top-level `Diagnostics`
    // body. Keeping the 4 Hz tick out of here matters because the body re-renders
    // are meant to be at the throttled ≤1 Hz NetEq sample cadence (one re-render
    // per kept sample); the heavy per-event JSON PARSE was moved to the subscribe
    // loop (parse-once, #1223), so the body prelude only clones already-parsed
    // samples. The section's `is_open` gating + `use_drop` interval cleanup live
    // in that child.

    // Resolve numeric session IDs to display names via VideoCallClient context.
    let client = use_context::<VideoCallClient>();
    let peer_display_name = move |session_id: &str| -> String {
        client
            .get_peer_user_id(session_id)
            .unwrap_or_else(|| session_id.to_string())
    };

    // Get list of available peers (keys are raw session IDs).
    let available_peers: Vec<String> = {
        let mut peers = vec!["All Peers".to_string()];
        let stats = neteq_stats_per_peer();
        let mut peer_keys: Vec<String> = stats.keys().cloned().collect();
        peer_keys.sort();
        peers.extend(peer_keys);
        peers
    };

    // Build the NetEq history for the selected peer by CLONING the already-parsed
    // samples out of the ring buffer (no JSON re-parse — the heavy decode happened
    // once in the subscribe loop). The Current-Status tiles + time-series charts
    // are only meaningful for ONE peer (S2): concatenating every peer's deque into
    // one timeline mixes unrelated clocks, so for "All Peers" we build an EMPTY
    // history and render a "pick a peer" placeholder downstream instead. The
    // history is wrapped in an `Rc<Vec<_>>` (S1) so the chart props compare by
    // `Rc::ptr_eq` (O(1)) instead of a content walk over up to 7200 samples. The
    // drawer body re-renders at the throttled ≤1 Hz sample cadence (one re-render
    // per KEPT sample) — the existing accepted model. (#1223)
    let current_peer = selected_peer();
    let single_peer = single_peer_selected(&current_peer);
    let stats_map = neteq_stats_per_peer();

    // MEMOIZE the `Rc<Vec<NetEqSample>>` build so its POINTER identity is STABLE
    // across parent re-renders that did NOT append a sample to the selected peer
    // (#1451). Two layers:
    //
    // 1. `history_key` re-runs whenever EITHER signal it reads changes
    //    (`selected_peer` or `neteq_stats_per_peer`, i.e. ANY peer's sample), but
    //    only PROPAGATES a new value when the SELECTED peer's cheap identity key
    //    (`peer` + `len` + `last_ts`) actually changed — see `neteq_history_key`.
    // 2. `neteq_history_memo`'s only reactive dependency is `history_key()`, so it
    //    rebuilds the `Rc` EXACTLY when the key changes. It reads the map via
    //    `.peek()` (a NON-reactive read) so the heavy clone is NOT re-tracked on
    //    every map mutation — the key alone gates the rebuild.
    //
    // Net effect: an unrelated parent re-render (toggling a help popover, resizing
    // the drawer, another peer's sample landing) hands the SAME `Rc` to the
    // children, so the child `UnifiedTimelineChart`'s `Rc::ptr_eq` prop gate
    // engages and `unified_series_from_samples` is NOT recomputed. The `Rc` is
    // rebuilt — and the children re-clip/redraw — only on a real append to the
    // selected peer (≤1 Hz) or a peer switch. For "All Peers" the key is
    // (peer="All Peers", len=0, last_ts=None) → a stable empty history. (#1451)
    let history_key = use_memo(move || {
        let peer = selected_peer();
        let map = neteq_stats_per_peer();
        if single_peer_selected(&peer) {
            neteq_history_key(&peer, map.get(&peer))
        } else {
            // "All Peers": no single timeline → stable empty key.
            neteq_history_key(&peer, None)
        }
    });
    let neteq_history_memo = use_memo(move || {
        // Establish the ONLY reactive dependency: the cheap key.
        let _key = history_key();
        // Build via `.peek()` (non-reactive) so the clone isn't re-tracked on
        // every map mutation; the key already gates when we get here.
        let peer = selected_peer.peek().clone();
        if single_peer_selected(&peer) {
            let map = neteq_stats_per_peer.peek();
            Rc::new(
                map.get(&peer)
                    .map(|peer_stats| peer_stats.iter().cloned().collect())
                    .unwrap_or_default(),
            )
        } else {
            // "All Peers": no single timeline → empty history (placeholder shown).
            Rc::new(Vec::new())
        }
    });
    let neteq_stats_history: Rc<Vec<NetEqSample>> = neteq_history_memo();

    // Cap caption gates on len()==7200 (owner decision 2): the selected peer's
    // deque length. For "All Peers" no charts are shown, so capped stays false.
    let neteq_capped = if single_peer {
        stats_map
            .get(&current_peer)
            .map(|dq| dq.len() == NETEQ_SAMPLE_CAP)
            .unwrap_or(false)
    } else {
        false
    };

    let latest_neteq_stats = neteq_stats_history.last().cloned();

    let buffer_map = neteq_buffer_per_peer();
    let jitter_map = neteq_jitter_per_peer();
    let (buffer_history, jitter_history) = if current_peer == "All Peers" {
        let mut ab = Vec::new();
        for buf in buffer_map.values() {
            ab.extend(buf.iter().cloned());
        }
        let mut aj = Vec::new();
        for jit in jitter_map.values() {
            aj.extend(jit.iter().cloned());
        }
        (ab, aj)
    } else {
        (
            buffer_map.get(&current_peer).cloned().unwrap_or_default(),
            jitter_map.get(&current_peer).cloned().unwrap_or_default(),
        )
    };

    let conn_state = connection_manager_state();
    let diag_data = diagnostics_data();
    let send_stats = sender_stats();
    let enc_settings = encoder_settings;
    let video_str = if video_enabled { "Enabled" } else { "Disabled" };
    let audio_str = if mic_enabled { "Enabled" } else { "Disabled" };
    let screen_str = if share_screen { "Enabled" } else { "Disabled" };
    let media_status =
        format!("Video: {video_str}\nAudio: {audio_str}\nScreen Share: {screen_str}");
    let current_peer_display = if current_peer == "All Peers" {
        "All Peers".to_string()
    } else {
        peer_display_name(&current_peer)
    };
    let peer_info = format!("Showing statistics for: {current_peer_display}");

    // Group order (owner decision, iteration 4): investigation-first —
    // Connection & system → Quality controls → Live stream state.
    // Connection & system is the incident-investigation anchor; it is the first
    // thing an operator reaches for when something is wrong, so it leads the
    // body. Quality controls (the editable sliders/Auto the user actively tunes)
    // comes second; Live stream state (passive read-only telemetry) comes last.
    // Because Connection & system has NO render gate, it ALWAYS renders, so it
    // unconditionally takes the `--first` modifier (no top border / extra top
    // padding). The old `perf_controls`-dependent `--first` juggling is gone:
    // `--first` can no longer be orphaned on a sub-second window where the
    // Performance handle hasn't been published yet, because the always-present
    // Connection & system group owns it.

    rsx! {
        div {
            id: "diagnostics-sidebar",
            class: if is_open { "visible" } else { "" },
            style: format!("width: {}px", width),
            // Non-modal drawer: a labelled region (the modal-trap behaviour stays
            // off — the call UI behind it remains interactive). (#1131 §5 a11y)
            role: "region",
            "aria-label": "Performance & Diagnostics",
            div { class: "sidebar-header",
                h2 { "Performance & Diagnostics" }
                // Spacer keeps the × rightmost (the cross-nav button was removed
                // when the Performance panel merged into this drawer; #1131).
                div { style: "flex: 1 1 auto;" }
                button {
                    class: "close-button",
                    "aria-label": "Close panel",
                    onclick: move |_| on_close.call(()),
                    "\u{00d7}"
                }
            }
            div { class: "sidebar-content",
                // ── GROUP 1 — Connection & system ──
                // The incident-investigation anchor, promoted to FIRST (owner
                // decision, iteration 4). It has NO render gate, so it always
                // renders and unconditionally owns `--first`. Order: Connection
                // Manager → Transport Preference → collapsed Raw stats disclosure
                // (Reception + Sending + Encoder + Media Status merged) →
                // collapsed Build info at the very bottom.
                div { class: "diag-group-label diag-group-label--first", role: "presentation",
                    "Connection & system"
                }
                section { class: "diagnostics-section", "aria-labelledby": "diag-h-connection-manager",
                    h3 { id: "diag-h-connection-manager", "Connection Manager" }
                    ConnectionManagerDisplay { connection_manager_state: conn_state }
                }
                section { class: "diagnostics-section", "aria-labelledby": "diag-h-transport-pref",
                    h3 { id: "diag-h-transport-pref", "Transport Preference" }
                    div { class: "device-setting-group",
                        select {
                            id: "diagnostics-transport-select",
                            class: "peer-selector",
                            onchange: move |evt: Event<FormData>| {
                                // The diagnostics select has no "remember" checkbox, so it
                                // expresses an explicit, NOT-remembered choice (#1291). Passing
                                // `sticky = false` means: WebTransport clears all storage (load
                                // resolves to the default); WebSocket writes a session-scoped
                                // value AND clears any prior sticky pin, so WS wins this session
                                // and is forgotten on tab close. Reading the stored sticky flag
                                // here would re-pin against the user's intent.
                                confirm_transport_change(
                                    &evt.value(),
                                    (transport_pref_ctx.0)(),
                                    "diagnostics-transport-select",
                                    false,
                                );
                            },
                            option {
                                value: "webtransport",
                                selected: (transport_pref_ctx.0)() == TransportPreference::WebTransport,
                                "WebTransport (default)"
                            }
                            option {
                                value: "websocket",
                                selected: (transport_pref_ctx.0)() == TransportPreference::WebSocket,
                                "WebSocket"
                            }
                        }
                    }
                    p { class: "transport-preference-note",
                        "Changing protocol will reload the page."
                    }
                }
                // Issue 1768: per-tile media-metrics overlay toggle. A real
                // checkbox with an explicit `label for=id` so it is properly
                // labeled and keyboard-operable; the overlay it controls is a
                // passive (aria-hidden) readout, so the checkbox is the sole a11y
                // control surface for the feature.
                section { class: "diagnostics-section", "aria-labelledby": "diag-h-display-options",
                    h3 { id: "diag-h-display-options", "Display options" }
                    div { class: "device-setting-group diag-overlay-toggle",
                        input {
                            r#type: "checkbox",
                            id: "diag-media-metrics-overlay",
                            "data-testid": "media-metrics-overlay-toggle",
                            checked: media_metrics_overlay_enabled(),
                            onchange: move |_| {
                                let next = !media_metrics_overlay_enabled();
                                media_metrics_overlay_enabled.set(next);
                                save_bool(MEDIA_METRICS_OVERLAY_KEY, next);
                            },
                        }
                        label { r#for: "diag-media-metrics-overlay",
                            "Show media metrics on tiles"
                        }
                    }
                    p { class: "transport-preference-note",
                        "Overlays each peer's received resolution, fps and audio bitrate at the \
                         bottom of their tile, and your own sending metrics on your tile."
                    }
                }
                // Raw stats: the four low-level pre-dumps (Reception + Sending +
                // Encoder + Media Status) folded into ONE native `<details>`
                // disclosure, COLLAPSED by default (owner decisions 3 & 4) —
                // omitting the `open` attr keeps it closed. `<details>`/`<summary>`
                // is keyboard-accessible without extra ARIA.
                section { class: "diagnostics-section", "aria-labelledby": "diag-h-raw-stats",
                    details { class: "diag-disclosure",
                        summary { id: "diag-h-raw-stats", class: "diag-disclosure-summary",
                            svg {
                                class: "diag-disclosure-chev",
                                width: "12",
                                height: "12",
                                view_box: "0 0 12 12",
                                path {
                                    d: "M4 2 L8 6 L4 10",
                                    fill: "none",
                                    stroke: "currentColor",
                                    stroke_width: "1.5",
                                }
                            }
                            "Raw stats"
                        }
                        div { class: "diagnostics-data",
                            div { class: "diag-raw-block",
                                h4 { "Reception Stats" }
                                if let Some(data) = &diag_data {
                                    pre { "{data}" }
                                } else {
                                    p { "No reception data available." }
                                }
                            }
                            div { class: "diag-raw-block",
                                h4 { "Sending Stats" }
                                if let Some(data) = &send_stats {
                                    pre { "{data}" }
                                } else {
                                    p { "No sending data available." }
                                }
                            }
                            div { class: "diag-raw-block",
                                h4 { "Encoder Settings" }
                                if let Some(data) = &enc_settings {
                                    pre { "{data}" }
                                } else {
                                    p { "No encoder settings available." }
                                }
                            }
                            div { class: "diag-raw-block",
                                h4 { "Media Status" }
                                pre { "{media_status}" }
                            }
                        }
                    }
                }
                // Build info: once-per-session content, demoted to the very bottom
                // inside a collapsed `<details>` (closed by default).
                section { class: "diagnostics-section", "aria-labelledby": "diag-h-build-info",
                    details { class: "diag-disclosure",
                        summary { id: "diag-h-build-info", class: "diag-disclosure-summary",
                            svg {
                                class: "diag-disclosure-chev",
                                width: "12",
                                height: "12",
                                view_box: "0 0 12 12",
                                path {
                                    d: "M4 2 L8 6 L4 10",
                                    fill: "none",
                                    stroke: "currentColor",
                                    stroke_width: "1.5",
                                }
                            }
                            "Build info"
                        }
                        {
                            // Issue #1480: github info (Commit + Branch) is gated on
                            // showBuildGitInfo. When hidden, columns collapse to
                            // Component / Built (2). When shown: Component / Commit /
                            // Branch / Built (4). The grid column count tracks via the
                            // --git / --nogit modifier so cell count == column count.
                            let show_git = crate::constants::show_build_git_info();
                            let table_class = if show_git {
                                "build-info-table build-info-table--git"
                            } else {
                                "build-info-table build-info-table--nogit"
                            };
                            rsx! {
                                div { class: "{table_class}",
                                    div { class: "build-info-header",
                                        span { class: "build-info-cell", "Component" }
                                        if show_git {
                                            span { class: "build-info-cell", "Commit" }
                                            span { class: "build-info-cell", "Branch" }
                                        }
                                        span { class: "build-info-cell", "Built" }
                                    }
                                    div { class: "build-info-row",
                                        span { class: "build-info-cell build-info-service", "dioxus-ui (v{env!(\"CARGO_PKG_VERSION\")})" }
                                        if show_git {
                                            span { class: "build-info-cell monospace", "{crate::constants::short_sha(env!(\"GIT_SHA\"))}" }
                                            span { class: "build-info-cell", "{env!(\"GIT_BRANCH\")}" }
                                        }
                                        span { class: "build-info-cell", "{crate::constants::build_datetime(env!(\"BUILD_TIMESTAMP\")).unwrap_or_else(|| env!(\"BUILD_TIMESTAMP\").to_string())}" }
                                    }
                                    for comp in backend_versions() {
                                        {
                                            let svc = comp["service"].as_str().unwrap_or("?").to_string();
                                            let ver = comp["version"].as_str().unwrap_or("").to_string();
                                            let sha = comp["git_sha"].as_str().unwrap_or("?").to_string();
                                            let br = comp["git_branch"].as_str().unwrap_or("?").to_string();
                                            let raw_ts = comp["build_timestamp"].as_str().unwrap_or("");
                                            let built = crate::constants::build_datetime(raw_ts).unwrap_or_else(|| if raw_ts.is_empty() { "-".to_string() } else { raw_ts.to_string() });
                                            let label = if ver.is_empty() { svc } else { format!("{svc} ({ver})") };
                                            rsx! {
                                                div { class: "build-info-row",
                                                    span { class: "build-info-cell build-info-service", "{label}" }
                                                    if show_git {
                                                        span { class: "build-info-cell monospace", "{crate::constants::short_sha(&sha)}" }
                                                        span { class: "build-info-cell", "{br}" }
                                                    }
                                                    span { class: "build-info-cell", "{built}" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // ── GROUP 2 — Quality controls (the migrated Performance panel) ──
                // Second (owner decision, iteration 4: editable controls before
                // passive telemetry). The label AND the panel render together:
                // gating both on `perf_controls` avoids an orphaned group divider
                // in the sub-second window before `Host` publishes the handle
                // (#1131 review F3). This group no longer owns `--first` — the
                // always-present Connection & system group above does.
                //
                // The Performance controls (simulcast strip + per-kind cards with
                // sliders / Auto / live meters / help) render inside their own
                // child component so the panel's 250 ms refresh tick + rAF meter
                // drivers re-render ONLY that subtree — NOT this top-level body.
                // The child also reads the preference signals, keeping all
                // reactive perf state out of this body. (#1131 unify, #1128)
                if let Some(controls) = perf_controls.clone() {
                    div { class: "diag-group-label", role: "presentation",
                        "Quality controls"
                    }
                    DiagnosticsPerformancePanel { controls, audio_source_active: mic_enabled }
                }

                // ── GROUP 3 — Live stream state ──
                // Last (owner decision, iteration 4: passive read-only telemetry).
                // Order: Simulcast layers → Peer Selection (scopes the NetEq
                // sections below) → Per-Peer Summary (pick the bad peer BEFORE the
                // detail) → Current Status tiles + scrollable NetEq charts.
                div { class: "diag-group-label", role: "presentation",
                    "Live stream state"
                }
                // Simulcast layers (#1095 §6 MOVE): the per-layer SEND ladder + the
                // per-peer RECEIVE breakdown. Extracted into its own child so its
                // 4 Hz refresh tick re-renders ONLY this section, not the NetEq
                // prelude / charts in this parent (perf review #1).
                SimulcastLayersSection {
                    is_open,
                    reader: diagnostics_reader.clone(),
                    // issue 1656: read the per-peer worker signal here in the
                    // parent and thread the value down (not a signal handle).
                    lag_fps: peer_lag_fps_per_peer(),
                }
                // Peer Selection (MOVED from the Connection group): it scopes the
                // Per-Peer Summary + Current Status + charts that follow, so it
                // belongs at the top of those sections. Reads `selected_peer` /
                // `available_peers` / `current_peer` here in the parent — these
                // are value-typed and do NOT tick per NetEq event.
                if available_peers.len() > 1 {
                    section { class: "diagnostics-section", "aria-labelledby": "diag-h-peer-selection",
                        h3 { id: "diag-h-peer-selection", "Peer Selection" }
                        select {
                            class: "peer-selector",
                            onchange: move |e: Event<FormData>| {
                                user_picked_peer.set(true);
                                selected_peer.set(e.value());
                            },
                            value: "{current_peer}",
                            for peer in available_peers.iter() {
                                {
                                    let label = if peer == "All Peers" {
                                        "All Peers".to_string()
                                    } else {
                                        peer_display_name(peer)
                                    };
                                    rsx! {
                                        option {
                                            value: "{peer}",
                                            selected: peer == &current_peer,
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                        p { class: "peer-info", "{peer_info}" }
                    }
                }
                // Per-Peer Summary (MOVED up from last): a triage index — pick the
                // bad peer before drilling into the detail. Keeps its >2-peers
                // gate. Reads stats_map / buffer_map / jitter_map / peer_transport
                // here in the parent (value-typed; no NetEq-event churn).
                if available_peers.len() > 2 {
                    section { class: "diagnostics-section", "aria-labelledby": "diag-h-peer-summary",
                        h3 { id: "diag-h-peer-summary", "Per-Peer Summary" }
                        div { class: "peer-summary",
                            {
                                let transport_map = peer_transport_per_peer();
                                // BLOCKER 2: read the per-peer VIDEO readout here in
                                // the same parent scope and join on the `peer_id`
                                // key (NetEq target peer = session-id string ==
                                // `to_peer`), so the triage chip can show "which peer
                                // is in slow motion" at a glance next to Buffer/Jitter.
                                let lag_fps_map = peer_lag_fps_per_peer();
                                rsx! {
                                    for (peer_id, _) in stats_map.iter() {
                                        {
                                            let display = peer_display_name(peer_id);
                                            let latest_buffer = buffer_map.get(peer_id).and_then(|b| b.last()).unwrap_or(&0);
                                            let latest_jitter = jitter_map.get(peer_id).and_then(|j| j.last()).unwrap_or(&0);
                                            // Color-code buffer/jitter (Directive 5): each value
                                            // gets its own classified span; the title carries the
                                            // reason so meaning never depends on color alone.
                                            let (buf_class, buf_title) = peer_buffer_class(*latest_buffer);
                                            let (jit_class, jit_title) = peer_jitter_class(*latest_jitter);
                                            let (badge_label, badge_class, badge_title) = match transport_map.get(peer_id).map(String::as_str) {
                                                Some("webtransport") => ("WT", "connection-type type-webtransport", "WebTransport"),
                                                Some("websocket") => ("WS", "connection-type type-websocket", "WebSocket"),
                                                _ => ("\u{2014}", "connection-type", "Transport unknown"),
                                            };
                                            // BLOCKER 2 triage chip: lag + recv/painted fps for
                                            // this peer. Same contrast-safe treatment as the
                                            // receive row (BLOCKER 1): neutral value text + a
                                            // non-text severity dot. The chip is color-accent +
                                            // TITLE only (no aria-live) — the receive row owns the
                                            // single aria-live "poor" announcement, so we do NOT
                                            // double-announce the same peer's poor lag here (that
                                            // would spam AT). The title carries the reason text, so
                                            // the chip is never color-only. Unknown → em-dash with
                                            // a specific title/aria-label (item 4).
                                            let readout = lag_fps_map.get(peer_id).copied().unwrap_or_default();
                                            let recv_txt = readout
                                                .recv_fps
                                                .map(|v| format!("{v:.0}"))
                                                .unwrap_or_else(|| "\u{2014}".into());
                                            let painted_txt = readout
                                                .painted_fps
                                                .map(|v| format!("{v:.1}"))
                                                .unwrap_or_else(|| "\u{2014}".into());
                                            let (lag_txt, lag_class, lag_title) = match readout.lag_ms {
                                                Some(v) => {
                                                    let (c, reason) = peer_lag_class(v);
                                                    // good has no reason word; give the title a
                                                    // neutral phrase so AT never reads a bare value.
                                                    let title = if reason.is_empty() { "on time" } else { reason };
                                                    (format!("{v:.0} ms"), c, title)
                                                }
                                                None => ("\u{2014}".to_string(), "", "Content staleness unknown"),
                                            };
                                            let lag_has_dot = !lag_class.is_empty();
                                            rsx! {
                                                div { class: "peer-summary-item",
                                                    strong { "{display}" }
                                                    div {
                                                        class: "peer-summary-item__metrics",
                                                        span { class: "{badge_class}", title: "{badge_title}", "{badge_label}" }
                                                        span {
                                                            "Buffer: "
                                                            span { class: "peer-stat {buf_class}", title: "{buf_title}", "{latest_buffer}ms" }
                                                            ", Jitter: "
                                                            span { class: "peer-stat {jit_class}", title: "{jit_title}", "{latest_jitter}ms" }
                                                        }
                                                        // Triage chip: fps gap + content staleness with dot accent.
                                                        span {
                                                            class: "peer-summary-item__lag",
                                                            "data-testid": "diag-peer-summary-lag-{peer_id}",
                                                            "Stale: "
                                                            if lag_has_dot {
                                                                span {
                                                                    class: "peer-stat__dot {lag_class}",
                                                                    "aria-hidden": "true",
                                                                }
                                                            }
                                                            if lag_txt == "\u{2014}" {
                                                                span {
                                                                    class: "peer-stat__value",
                                                                    title: "{lag_title}",
                                                                    "aria-label": "{lag_title}",
                                                                    "{lag_txt}"
                                                                }
                                                            } else {
                                                                span {
                                                                    class: "peer-stat__value",
                                                                    title: "{lag_title}",
                                                                    "{lag_txt}"
                                                                }
                                                            }
                                                            ", fps "
                                                            if recv_txt == "\u{2014}" {
                                                                span {
                                                                    class: "peer-stat__value",
                                                                    title: "Received fps unknown",
                                                                    "aria-label": "Received fps unknown",
                                                                    "{recv_txt}"
                                                                }
                                                            } else {
                                                                span { class: "peer-stat__value", "{recv_txt}" }
                                                            }
                                                            "/"
                                                            if painted_txt == "\u{2014}" {
                                                                span {
                                                                    class: "peer-stat__value",
                                                                    title: "Painted fps unknown",
                                                                    "aria-label": "Painted fps unknown",
                                                                    "{painted_txt}"
                                                                }
                                                            } else {
                                                                span { class: "peer-stat__value", "{painted_txt}" }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Current Status tiles + scrollable NetEq charts, with their two
                // info-icon popovers, live in a child component so toggling a
                // popover re-renders ONLY that subtree. The already-computed,
                // value-typed history/latest/capped are passed in as props; no
                // signal reads move into the child (#1128).
                NetEqStatusAndCharts {
                    latest_stats: latest_neteq_stats,
                    // Wrap in NetEqHistory (Rc) — the `.clone()` is a refcount
                    // bump, and the child's prop memo compares by pointer (S1).
                    stats_history: NetEqHistory(neteq_stats_history.clone()),
                    buffer_history: buffer_history.clone(),
                    jitter_history: jitter_history.clone(),
                    capped: neteq_capped,
                    single_peer,
                }
            }
            div {
                class: "drawer-resize-handle",
                role: "separator",
                aria_orientation: "vertical",
                aria_label: "Resize panel",
                tabindex: "0",
                // keyboard resize is a follow-up
                // Pointer capture: capturing the pointer on pointerdown routes every
                // subsequent pointermove/up to THIS handle even when the pointer moves
                // over the drawer body or a tile — required for shrink to work. The
                // width math + persistence live in the parent (attendants.rs), which
                // owns the width signals; this handle only forwards pointer coords.
                onpointerdown: move |evt| {
                    evt.prevent_default();
                    on_resize_start.call(());
                    let native = evt.as_web_event();
                    if let Some(t) = native.target() {
                        use wasm_bindgen::JsCast;
                        if let Ok(el) = t.dyn_into::<web_sys::Element>() {
                            let _ = el.set_pointer_capture(native.pointer_id());
                        }
                    }
                },
                onpointermove: move |evt| {
                    on_resize_move.call(evt.as_web_event().client_x() as f64);
                },
                onpointerup: move |evt| {
                    evt.prevent_default();
                    on_resize_end.call(());
                },
                // Reuse on_resize_end on cancel AND lost-capture: its parent closure
                // resets resizing_drawer to None (always) and persists the (already-
                // clamped) width ONLY if a real move happened this drag. A no-move
                // cancel (OS gesture, touch interruption, lost capture) leaves
                // width and storage untouched — nothing can latch.
                onpointercancel: move |_| {
                    on_resize_end.call(());
                },
                // #1296: onpointerup only fires when the pointer is released over the
                // captured element. If capture is lost another way (release off-element,
                // OS interruption, element re-render) the browser fires
                // `lostpointercapture` on the SAME element that captured (this handle
                // div, via set_pointer_capture on evt.target() in onpointerdown).
                // Forward to on_resize_end — the parent resets resizing_drawer to None
                // (and valid-gated flush/persist), so a later hover over the handle
                // can't keep resizing.
                onlostpointercapture: move |_| {
                    on_resize_end.call(());
                },
            }
        }
    }
}

/// Plain-language explanation for the "Current Status" info icon (#1131). Every
/// claim is grounded in what the underlying NetEq field measures: BUFFER =
/// `current_buffer_size_ms`, TARGET = `target_delay_ms` (NetEq's adaptive goal),
/// PACKETS = `packets_awaiting_decode`, EXPAND/ACCEL = Q14 rates rendered per-mille
/// (‰) by `q14::to_per_mille`, REORDER = `reorder_rate_permyriad` (‱).
const HELP_NETEQ_STATUS: &str = "A live snapshot of each peer's audio jitter buffer — the queue that absorbs network timing variance before audio is played. Buffer is what's queued now; Target is the size NetEq is aiming for given recent jitter (Buffer near Target is healthy; Buffer at 0 means it ran dry → choppy audio). Packets is how many encoded packets are waiting to decode. Expand rate (‰ of output) rises when audio is stretched to cover lost or late packets; Accelerate rate (‰) rises when audio is sped up to drain an over-full buffer. Reorder rate (‱ of packets) and Max reorder distance show how often packets arrive out of order. A few ‰ of expand/accelerate is normal; sustained high expand means the network is dropping or delaying audio.";

/// Buffer vs Target chart explanation (#1222).
const HELP_CHART_BUFFER: &str = "The live jitter buffer (gauge, ms) plotted against NetEq's adaptive Target. Target rises automatically when the network gets jittery and settles back (~80 ms baseline) when it's calm. The buffer should track the target closely. Healthy: buffer hugging target; a buffer line dipping toward 0 means the queue ran dry and audio is starving.";

/// Decode Operations chart explanation (#1222).
const HELP_CHART_DECODE: &str = "How the decoder spent each second, as true per-second rates: Normal (ordinary playback), Expand (loss concealment — stretching audio over missing/late packets), Accelerate (catch-up — speeding up to drain a full buffer), plus Preemptive and Merge. Healthy: Normal dominant near ~100/s with Expand at 0; rising Expand means the network is dropping or delaying audio.";

/// Packets Awaiting Decode chart explanation (#1222).
const HELP_CHART_PACKETS: &str = "Queue depth over time — how many encoded packets are waiting to be decoded (a gauge, not a rate). Healthy: low and steady. A sustained climb means decode is falling behind the incoming stream.";

/// Packet Reordering chart explanation (#1222).
const HELP_CHART_REORDER: &str = "Two lifetime measures: the share of packets that arrived out of order (rate, ‱ of received) and the largest sequence gap seen (running max, in packets). Both are cumulative by design, so they only ever hold or rise. Healthy: flat at 0 on a clean LAN; slow, occasional growth is normal on the public internet.";

/// Unified timeline chart explanation (issue 173 / upstream 712). One shared
/// time-axis overlays several NetEq health metrics so trends line up at a glance;
/// each series has its own on/off checkbox and is scaled to its own range (the Y
/// axis reads % of each series' max), so different units (ms, count, ‰) coexist.
const HELP_CHART_UNIFIED: &str = "One shared timeline overlaying the most-watched NetEq health metrics — Buffer, Target, Packets awaiting decode, and Expand rate — so you can see how they move together at a glance. Toggle any series on or off with its checkbox. Because the metrics use different units, each is scaled to its own range and the Y axis reads as a percent of that series' max; hover the chart to read the real values for every visible series at that moment. Use the per-metric charts below for absolute values on a single dedicated axis.";

/// Current Status tiles + the scrollable NetEq charts, with one info-icon
/// popover on each cluster header (#1131 cleanup). Pulled into its own child so
/// opening a popover (a per-subtree signal toggle) re-renders ONLY this subtree —
/// not the parent [`Diagnostics`] body. The history/latest/capped are passed in
/// as value-typed props (already-parsed samples cloned once by the parent prelude
/// — the heavy JSON decode happens in the subscribe loop, not here), so no extra
/// signal reads enter the parent body (tick-scoping #1128).
///
/// All popovers (the Current Status section ⓘ plus the four per-chart ⓘ) share one
/// single-open signal keyed by the help id, so opening one closes the others —
/// identical to the Performance panel's help behaviour (same [`HelpPopover`]
/// component, same `.perf-help*` styles, same 44px hit area + aria treatment).
/// testids: `diag-status-help`, `diag-chart-buffer-help`, `diag-chart-decode-help`,
/// `diag-chart-packets-help`, `diag-chart-reorder-help` (#1222).
#[component]
fn NetEqStatusAndCharts(
    latest_stats: Option<NetEqSample>,
    /// Shared, Rc-wrapped history (S1) — pointer-compared by the prop memo.
    stats_history: NetEqHistory,
    buffer_history: Vec<u64>,
    jitter_history: Vec<u64>,
    /// `true` only when the selected peer's deque is at the 2-hour cap — gates
    /// each chart's "Showing last 2 hours" caption.
    capped: bool,
    /// `true` when a specific peer is selected (S2). The Current-Status tiles and
    /// the time-series charts are only meaningful for one peer; for "All Peers"
    /// we render a single placeholder section instead (no dangling empty heading).
    single_peer: bool,
) -> Element {
    // Single-open help signal shared by both popovers in this cluster.
    let open_help = use_signal(|| None::<&'static str>);
    let has_history = !stats_history.0.is_empty();

    // "All Peers": one placeholder section, IN PLACE of both the Current-Status
    // tiles and the charts — no Current-Status heading is rendered, so there is
    // no dangling empty section header. (S2)
    if !single_peer {
        return rsx! {
            section { class: "diagnostics-section", "aria-labelledby": "diag-h-neteq-placeholder",
                h3 { id: "diag-h-neteq-placeholder", "NetEQ" }
                p { class: "diag-neteq-placeholder",
                    "Select a specific peer to view time-series charts and current status."
                }
            }
        };
    }

    rsx! {
        section { class: "diagnostics-section", "aria-labelledby": "diag-h-current-status",
            div { class: "diag-section-head",
                h3 { id: "diag-h-current-status", "Current Status" }
                HelpPopover {
                    key_id: "diag-status",
                    help_testid: "diag-status-help",
                    help_label: "About the Current Status metrics",
                    help_body: HELP_NETEQ_STATUS,
                    open_help,
                }
            }
            NetEqStatusDisplay { latest_stats }
        }
        if has_history {
            // Unified timeline (issue 173 / upstream 712): ONE shared time-axis
            // with the most-watched NetEq metrics OVERLAID and a per-series on/off
            // checkbox legend, mounted ABOVE the per-type charts. It is seeded from
            // the SAME per-peer deque (no new signal reads) and the per-type charts
            // are kept as a drill-down behind the disclosure below.
            section { class: "diagnostics-section", "aria-labelledby": "diag-h-neteq-unified",
                div { class: "diag-section-head",
                    h3 { id: "diag-h-neteq-unified", "Timeline" }
                    HelpPopover {
                        key_id: "diag-chart-unified",
                        help_testid: "diag-chart-unified-help",
                        help_label: "About the unified timeline chart",
                        help_body: HELP_CHART_UNIFIED,
                        open_help,
                    }
                }
                div { class: "diagnostics-charts neteq-charts-stack",
                    div { class: "chart-container", "data-testid": "diag-unified-timeline",
                        // Pass the Rc-wrapped history (O(1) `Rc::ptr_eq` prop memo);
                        // the chart builds + memoizes the overlaid series INSIDE,
                        // so the per-peer deque is never deep-copied per render and
                        // the prop diff doesn't walk all points. (issue 173 perf)
                        UnifiedTimelineChart {
                            history: stats_history.clone(),
                            scroll_id: "neteq-chart-scroll-unified".to_string(),
                            capped,
                        }
                    }
                }
            }
            section { class: "diagnostics-section", "aria-labelledby": "diag-h-neteq-charts",
                // Per-metric charts kept as a DRILL-DOWN (issue 173): collapsed by
                // default in a native `<details>` so the unified timeline above is
                // the primary view, but the dedicated per-axis charts stay one
                // click away (not deleted). Each chart still carries its OWN
                // per-chart ⓘ in a `.diag-chart-head`; all popovers share the
                // existing `open_help` signal (single-open contract).
                details { class: "diag-disclosure",
                    summary { id: "diag-h-neteq-charts", class: "diag-disclosure-summary",
                        svg {
                            class: "diag-disclosure-chev",
                            width: "12",
                            height: "12",
                            view_box: "0 0 12 12",
                            path {
                                d: "M4 2 L8 6 L4 10",
                                fill: "none",
                                stroke: "currentColor",
                                stroke_width: "1.5",
                            }
                        }
                        "Per-metric charts"
                    }
                // Four scrollable charts, 1-up full drawer width, stacked. Each
                // has a UNIQUE scroll id so the shared `onscroll` scroll-sync can
                // copy scroll_left onto the other three siblings (one timeline).
                // `show_title: false` suppresses the in-SVG `.chart-title` so the
                // `.diag-chart-head__title` is the single visible heading.
                div { class: "diagnostics-charts neteq-charts-stack",
                    div { class: "chart-container",
                        div { class: "diag-chart-head",
                            span { class: "diag-chart-head__title", "Buffer Size vs Target" }
                            HelpPopover {
                                key_id: "diag-chart-buffer",
                                help_testid: "diag-chart-buffer-help",
                                help_label: "About the Buffer vs Target chart",
                                help_body: HELP_CHART_BUFFER,
                                open_help,
                            }
                        }
                        NetEqAdvancedChart { stats_history: stats_history.clone(), chart_type: AdvancedChartType::BufferVsTarget, scroll_id: "neteq-chart-scroll-buffer".to_string(), capped, show_title: false }
                    }
                    div { class: "chart-container",
                        div { class: "diag-chart-head",
                            span { class: "diag-chart-head__title", "Decode Operations" }
                            HelpPopover {
                                key_id: "diag-chart-decode",
                                help_testid: "diag-chart-decode-help",
                                help_label: "About the Decode Operations chart",
                                help_body: HELP_CHART_DECODE,
                                open_help,
                            }
                        }
                        NetEqAdvancedChart { stats_history: stats_history.clone(), chart_type: AdvancedChartType::DecodeOperations, scroll_id: "neteq-chart-scroll-decode".to_string(), capped, show_title: false }
                    }
                    div { class: "chart-container",
                        div { class: "diag-chart-head",
                            span { class: "diag-chart-head__title", "Packets Awaiting Decode" }
                            HelpPopover {
                                key_id: "diag-chart-packets",
                                help_testid: "diag-chart-packets-help",
                                help_label: "About the Packets Awaiting Decode chart",
                                help_body: HELP_CHART_PACKETS,
                                open_help,
                            }
                        }
                        NetEqAdvancedChart { stats_history: stats_history.clone(), chart_type: AdvancedChartType::QualityMetrics, scroll_id: "neteq-chart-scroll-packets".to_string(), capped, show_title: false }
                    }
                    div { class: "chart-container",
                        div { class: "diag-chart-head",
                            span { class: "diag-chart-head__title", "Packet Reordering" }
                            HelpPopover {
                                key_id: "diag-chart-reorder",
                                help_testid: "diag-chart-reorder-help",
                                help_label: "About the Packet Reordering chart",
                                help_body: HELP_CHART_REORDER,
                                open_help,
                            }
                        }
                        NetEqAdvancedChart { stats_history: stats_history.clone(), chart_type: AdvancedChartType::ReorderingAnalysis, scroll_id: "neteq-chart-scroll-reorder".to_string(), capped, show_title: false }
                    }
                }
                }
            }
        } else {
            section { class: "diagnostics-section", "aria-labelledby": "diag-h-neteq-history",
                h3 { id: "diag-h-neteq-history", "NetEQ Buffer / Jitter History" }
                div { style: "display:flex; gap:var(--space-3); align-items:center;",
                    NetEqChart { data: buffer_history.clone(), chart_type: ChartType::Buffer, width: 140, height: 80 }
                    NetEqChart { data: jitter_history.clone(), chart_type: ChartType::Jitter, width: 140, height: 80 }
                }
            }
        }
    }
}

/// Thin adapter that mounts the migrated [`PerformanceSettingsPanel`] inside the
/// Diagnostics drawer's "Quality controls" group (#1131 unify). It exists so the
/// preference-signal reads (`performance_preference()` / `receive_preference()`)
/// and the panel's own 250 ms tick + rAF meter drivers are scoped to THIS child,
/// never the top-level [`Diagnostics`] body — reading the prefs here subscribes
/// only this subtree, and the panel re-renders here, so the parent body is not
/// re-run on perf interactions (tick-scoping #1128).
///
/// All controls come from the `PerfControlsHandle` Host publishes; `audio_source_active`
/// (the live mic-capture state) is forwarded from the drawer's `mic_enabled` prop.
#[component]
fn DiagnosticsPerformancePanel(controls: PerfControlsHandle, audio_source_active: bool) -> Element {
    // Read the live preference signals here (NOT in the drawer body) so only this
    // subtree subscribes; the panel keeps its existing value-typed props.
    let pref = (controls.performance_preference)();
    let receive_pref = (controls.receive_preference)();
    let on_change = controls.on_change.clone();
    let on_receive_change = controls.on_receive_change.clone();
    rsx! {
        PerformanceSettingsPanel {
            // SEND (#961).
            pref,
            on_change: move |p| on_change(p),
            read_snapshot: controls.read_snapshot.clone(),
            read_screen_snapshot: controls.read_screen_snapshot.clone(),
            // RECEIVE (#989 simulcast).
            receive_pref,
            on_receive_change: move |c| on_receive_change(c),
            received_reader: controls.received_reader.clone(),
            // Live diagnostics (#1095) for the per-card summary lines + strip.
            diagnostics_reader: controls.diagnostics_reader.clone(),
            // SEND layer-count ceilings (real ladder depth from host).
            video_layer_max: controls.video_layer_max,
            screen_layer_max: controls.screen_layer_max,
            audio_layer_max: controls.audio_layer_max,
            // Mic capture state for the audio SEND caption.
            audio_source_active,
        }
    }
}

/// The live "Simulcast layers" section, extracted into its OWN component so its
/// 4 Hz refresh tick re-renders only this small subtree — NOT the parent
/// [`Diagnostics`] body (which should re-render at the throttled ≤1 Hz NetEq
/// sample cadence, not 4×/sec). This is the scoped-subscription pattern the perf
/// meters already use (perf review #1).
///
/// Owns the 250 ms `gloo` `Interval` (gated to `is_open`, dropped on unmount via
/// `use_drop`) and the three live reads off the `DiagnosticsReader`. The reader's
/// closures touch encoder atomics / client per-peer state, so this subtree must
/// re-render periodically; the reads are cheap (counts / min-max / a small
/// per-peer Vec). `reader` is `None` until `Host` publishes it (or when
/// diagnostics aren't wired) → the section renders nothing.
#[component]
fn SimulcastLayersSection(
    is_open: bool,
    reader: Option<DiagnosticsReader>,
    /// issue 1656: per-peer VIDEO readout (`recv_fps` + `lag_ms` +
    /// `painted_fps`, see [`PeerVideoReadout`]), keyed by `Peer::sid_str`
    /// (u64-as-String). Passed as a plain value clone (NOT a signal handle) —
    /// this section re-renders at 4 Hz via its own tick, so reading the parent
    /// signal once and threading the value down is fine.
    lag_fps: HashMap<String, PeerVideoReadout>,
) -> Element {
    // 4 Hz refresh tick scoped to THIS component. Gated to `is_open`; the handle
    // lives in a `use_hook` cell and `use_drop` cancels it on unmount.
    let mut tick = use_signal(|| 0u64);
    {
        type IntervalCell = Rc<std::cell::RefCell<Option<gloo_timers::callback::Interval>>>;
        let cell: IntervalCell = use_hook(|| Rc::new(std::cell::RefCell::new(None)));
        let cell_effect = cell.clone();
        use_effect(move || {
            if is_open {
                let interval = gloo_timers::callback::Interval::new(250, move || {
                    let next = tick.peek().wrapping_add(1);
                    tick.set(next);
                });
                *cell_effect.borrow_mut() = Some(interval);
            } else {
                *cell_effect.borrow_mut() = None;
            }
        });
        use_drop(move || {
            *cell.borrow_mut() = None;
        });
    }
    // Subscribe this subtree (only) to the throttled refresh.
    let _ = tick();

    // No reader wired → render nothing (no empty "Simulcast layers" heading).
    let Some(reader) = reader.as_ref() else {
        return rsx! {};
    };

    let summary_line = format_simulcast_summary(&reader.summary);
    let send_video_snap = (reader.send_video)();
    let send_screen_snap = (reader.send_screen)();
    let per_peer_receive = (reader.per_peer_receive)();

    // issue 1482: resolve the per-peer device blocks here (live, each tick)
    // while the reader is in scope. Iterate the ALL-PEERS device reader (NOT the
    // media-receive list) so a peer that reports device metrics via HEALTH but
    // whose media isn't currently flowing (e.g. camera off) still renders. The
    // label comes from the reader tuple, which mirrors the receive-list label
    // (display name → user id → session id), so a receiving peer's label is
    // unchanged. A peer whose formatted lines are empty contributes no block —
    // never an empty-labeled row; an empty list → no `.diag-device` container.
    let device_blocks: Vec<DeviceBlock> = {
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut blocks: Vec<DeviceBlock> = Vec::new();
        for (session_id, label, info) in (reader.per_peer_device_all)().into_iter() {
            if !seen.insert(session_id) {
                continue;
            }
            let lines = format_peer_device_lines(&info);
            if !lines.is_empty() {
                blocks.push((session_id, label, lines));
            }
        }
        blocks
    };

    rsx! {
        div { class: "diagnostics-section",
            h3 { "Simulcast layers" }
            p { class: "simulcast-effective", "{summary_line}" }
            // Both the SEND ladder and the RECEIVE breakdown now label layers by
            // quality NAME (Low/Medium/High, compact L/M/H), so they read
            // consistently under this single heading. A one-line hint states the
            // shared convention (#1222, site 10).
            p { class: "simulcast-note", "Layers are named by quality: Low, Medium, High." }
            SimulcastSendLadder {
                title: "Video (sending)",
                kind: "video",
                not_sharing_text: "Camera — off",
                snap: send_video_snap,
            }
            SimulcastSendLadder {
                title: "Screen (sending)",
                kind: "screen",
                not_sharing_text: "Screen — not sharing",
                snap: send_screen_snap,
            }
            SimulcastReceiveBreakdown {
                peers: per_peer_receive,
                device_blocks,
                lag_fps,
            }
        }
    }
}

/// One SEND stream's per-layer simulcast ladder for the Diagnostics "Simulcast
/// layers" section (#1095 §6 MOVE — relocated from the Performance panel). One
/// chip per EFFECTIVE layer (res + bitrate), styled active vs shed. `snap` is
/// `None` when the source is off (camera off / not sharing) → a static line.
#[component]
fn SimulcastSendLadder(
    title: &'static str,
    /// Media-kind slug for the per-LED testid (`video` / `screen`), so e2e can
    /// target `diag-simulcast-led-{kind}-{layer_id}` per kind (issue #1607).
    kind: &'static str,
    /// Static line shown when `snap` is `None` (source off / not sharing).
    not_sharing_text: &'static str,
    snap: Option<videocall_client::SimulcastSendSnapshot>,
) -> Element {
    let Some(snap) = snap else {
        return rsx! {
            div { class: "simulcast-send",
                span { class: "simulcast-send-title", "{title}" }
                span { class: "simulcast-send-static", "{not_sharing_text}" }
            }
        };
    };

    // Single-layer → static line (no per-layer ladder to show).
    if !snap.simulcast_active {
        let detail = snap
            .layers
            .first()
            .map(|l| {
                format!(
                    "Single layer · {} · {}",
                    format_send_layer_short(l.width, l.height),
                    format_kbps_compact(l.bitrate_kbps)
                )
            })
            .unwrap_or_else(|| "Single layer".to_string());
        return rsx! {
            div { class: "simulcast-send",
                span { class: "simulcast-send-title", "{title}" }
                span { class: "simulcast-send-static", "{detail}" }
            }
        };
    }

    let header = format_send_header(&snap);
    let total = format_send_total_kbps(&snap);
    let header_line = if total == 0 {
        header
    } else {
        format!("{header} · {} total", format_mbps(total))
    };
    let max_kbps = snap
        .layers
        .iter()
        .map(|l| l.bitrate_kbps)
        .max()
        .unwrap_or(1)
        .max(1);

    rsx! {
        div { class: "simulcast-send",
            span { class: "simulcast-send-title", "{title}" }
            span { class: "simulcast-send-header", "{header_line}" }
            div { class: "simulcast-send-ladder", "data-testid": "diag-simulcast-ladder",
                {
                    // Ladder size = number of effective layers in this send stream;
                    // the basis for the per-rung quality letter (Low/Med/High → L/M/H).
                    let ladder_count = snap.layers.len() as u32;
                    rsx! {
                        for layer in snap.layers.iter().cloned() {
                            {
                                // issue 1607: the rung LED is lit ONLY for layers
                                // currently being encoded+sent (below the shed-aware
                                // active boundary). `active_layers` is the live count
                                // off the encoder atomic (camera seeds 1 then ramps;
                                // screen seeds the optimistic baseline then sheds), so
                                // a shed top rung reads as OFF — not "all on".
                                let active = layer_led_on(layer.layer_id, snap.active_layers);
                                let grow = (layer.bitrate_kbps as f32 / max_kbps as f32).max(0.4);
                                let chip_class = if active {
                                    "simulcast-rung is-active"
                                } else {
                                    "simulcast-rung is-shed"
                                };
                                // Explicit LED dot (issue 1607): a filled green dot
                                // for SENT layers, a hollow grey dot for shed layers —
                                // an unambiguous on/off affordance distinct from the
                                // rung's subtle dimming.
                                let led_class = if active {
                                    "simulcast-led is-on"
                                } else {
                                    "simulcast-led is-off"
                                };
                                let led_label = if active { "sending" } else { "not sending" };
                                let full = format_send_layer(
                                    layer.layer_id, ladder_count, layer.width, layer.height, layer.bitrate_kbps,
                                );
                                let res_short = format_send_layer_short(layer.width, layer.height);
                                let kbps_short = format_kbps_compact(layer.bitrate_kbps);
                                // DISPLAY the quality LETTER (L/M/H); the internal
                                // `layer_id` (and the data-testid suffix) stays 0-based
                                // so e2e selectors / protobuf don't churn.
                                let letter = layer_quality_label(layer.layer_id, ladder_count, true);
                                rsx! {
                                    div {
                                        class: chip_class,
                                        "data-testid": "diag-simulcast-rung-{layer.layer_id}",
                                        title: "{full}",
                                        style: "flex-grow: {grow};",
                                        span {
                                            class: "simulcast-rung-head",
                                            span {
                                                class: led_class,
                                                "data-testid": "diag-simulcast-led-{kind}-{layer.layer_id}",
                                                "aria-label": "{letter} layer {led_label}",
                                                title: "{led_label}",
                                            }
                                            span { class: "simulcast-rung-id", "{letter}" }
                                        }
                                        span { class: "simulcast-rung-res", "{res_short}" }
                                        span { class: "simulcast-rung-kbps", "{kbps_short}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The per-peer RECEIVE simulcast breakdown for the Diagnostics "Simulcast
/// layers" section (#1095 §6 MOVE). One block per kind (video / audio / screen):
/// the top-3 peers (highest layer first) + a "+N more" tail. Fed by the live
/// per-peer snapshot list.
#[component]
fn SimulcastReceiveBreakdown(
    peers: Vec<videocall_client::PeerReceiveDiag>,
    /// #1482: pre-resolved per-peer device blocks, one entry per peer that
    /// reported something: `(session_id, peer_label, [(row_label, value)])`.
    /// Resolved in the parent (`SimulcastLayersSection`) where the live reader
    /// is in scope, so this component takes a plain `PartialEq` value prop
    /// rather than an `Rc<dyn Fn>` (which the `#[component]` macro can't derive
    /// `PartialEq` for). Empty → the Device section renders nothing.
    device_blocks: Vec<DeviceBlock>,
    /// issue 1656: per-peer VIDEO readout (`recv_fps` + `lag_ms` +
    /// `painted_fps`, see [`PeerVideoReadout`]), keyed by `Peer::sid_str`
    /// (u64-as-String) == `PeerReceiveDiag.session_id` rendered as a String. A
    /// peer absent (or a `None` field) renders the em-dash unknown idiom. Only
    /// consumed for the VIDEO kind rows.
    lag_fps: HashMap<String, PeerVideoReadout>,
) -> Element {
    rsx! {
        div { class: "simulcast-recv",
            span { class: "simulcast-recv-title", "Receiving (per peer)" }
            for (kind, kind_label) in [
                (PrefMediaKind::Video, "video"),
                (PrefMediaKind::Audio, "audio"),
                (PrefMediaKind::Screen, "screen"),
            ] {
                {
                    // `peers_for_kind` returns a fresh owned Vec; sort it IN PLACE
                    // (no extra `.clone()` per tick) and read the spread off the
                    // sorted order rather than building a second `layers` Vec.
                    let mut kind_peers = peers_for_kind(&peers, kind);
                    if kind_peers.is_empty() {
                        rsx! {}
                    } else {
                        let n = kind_peers.len();
                        // Ladder size for this kind = the shared per-peer layer_count
                        // (use the max in case peers report different ladder depths).
                        // It is the basis for the quality LETTERS in the spread/tail.
                        let count = kind_peers
                            .iter()
                            .map(|p| p.snap.layer_count)
                            .max()
                            .unwrap_or(1);
                        // Spread = lowest..highest layer INDEX, rendered as quality
                        // letters (L/M/H). Compute from explicit min/max over indices
                        // so it is independent of the display sort below.
                        let lo_idx = kind_peers
                            .iter()
                            .map(|p| p.snap.layer_index)
                            .min()
                            .unwrap_or(0);
                        let hi_idx = kind_peers
                            .iter()
                            .map(|p| p.snap.layer_index)
                            .max()
                            .unwrap_or(0);
                        let spread = if lo_idx == hi_idx {
                            layer_quality_label(lo_idx, count, true).to_string()
                        } else {
                            format!(
                                "{}\u{2013}{}",
                                layer_quality_label(lo_idx, count, true),
                                layer_quality_label(hi_idx, count, true)
                            )
                        };
                        // Full quality word for the "+N more" tail (e.g. "Low").
                        let tail_label = layer_quality_label(lo_idx, count, false);
                        // Sort by layer DESC (highest quality first) — top-3 shown.
                        kind_peers.sort_by_key(|p| std::cmp::Reverse(p.snap.layer_index));
                        let extra = n.saturating_sub(3);
                        kind_peers.truncate(3);
                        rsx! {
                            div { class: "simulcast-recv-kind",
                                "data-testid": "diag-simulcast-recv-{kind_label}",
                                span { class: "simulcast-recv-kind-head", "{kind_label} · {n} peer(s) · {spread}" }
                                for p in kind_peers.into_iter() {
                                    {
                                        // Borrow the label for the testid; the line owns its String.
                                        let session_id = p.session_id;
                                        // issue 1607: per-peer RECEIVE LED row. A receiver
                                        // decodes exactly ONE layer per kind, so the LED is
                                        // lit only for `layer_index` and dark for the rest.
                                        // Use this peer's own ladder depth (`layer_count`),
                                        // floored at the kind-wide `count` so the row width is
                                        // stable across peers reporting different depths.
                                        let recv_index = p.snap.layer_index;
                                        let recv_count = p.snap.layer_count.max(count).max(1);
                                        let line = format_peer_kind_line(kind_label, Some(&p.snap))
                                            .map(|l| format!("{}: {l}", p.label))
                                            .unwrap_or(p.label);
                                        // issue 1656: per-peer VIDEO readout. Only the video
                                        // kind shows the recv/painted fps pair and the
                                        // content-staleness chip; audio/screen render nothing
                                        // new. Look up by
                                        // session_id-as-String == the event's `to_peer`. A peer
                                        // absent (or a `None` field) renders the em-dash unknown
                                        // idiom. `0.0` (a real "on time"/"no frames" reading once
                                        // an event has arrived) renders a real value.
                                        let is_video = kind == PrefMediaKind::Video;
                                        let readout = if is_video {
                                            lag_fps.get(&session_id.to_string()).copied().unwrap_or_default()
                                        } else {
                                            PeerVideoReadout::default()
                                        };
                                        // issue 1656 (item 3): the recv-vs-painted GAP is the
                                        // slow-motion signature — both numbers must be visible
                                        // together and labelled. recv whole-number, painted .1.
                                        let recv_txt = readout
                                            .recv_fps
                                            .map(|v| format!("{v:.0}"))
                                            .unwrap_or_else(|| "\u{2014}".into());
                                        let painted_txt = readout
                                            .painted_fps
                                            .map(|v| format!("{v:.1}"))
                                            .unwrap_or_else(|| "\u{2014}".into());
                                        // BLOCKER 1: legibility is DECOUPLED from severity. The
                                        // lag VALUE text renders in a high-contrast neutral
                                        // (.peer-stat__value → --text-primary, passes WCAG AA at
                                        // --fs-2 in both themes); the severity HUE is carried only
                                        // by a non-text dot (.peer-stat__dot, filled with the
                                        // --diag-q-* token). `lag_class` styles the DOT, not the
                                        // value text. Unknown (no event yet) → em-dash, no dot, no
                                        // alarm. The poor state additionally announces an sr-only
                                        // aria-live status so the alarm is not color-only.
                                        let (lag_txt, lag_class, lag_title, lag_alarm) =
                                            match readout.lag_ms {
                                                Some(v) => {
                                                    let (c, reason) = peer_lag_class(v);
                                                    (
                                                        format!("{v:.0} ms"),
                                                        c,
                                                        reason,
                                                        c == "is-poor",
                                                    )
                                                }
                                                None => ("\u{2014}".to_string(), "", "", false),
                                            };
                                        // Show the dot only when we have a real lag reading
                                        // (a class). Unknown → no dot (em-dash carries "unknown").
                                        let lag_has_dot = !lag_class.is_empty();
                                        rsx! {
                                            div {
                                                class: "simulcast-recv-peer",
                                                "data-testid": "diag-simulcast-recv-peer-{session_id}",
                                                div {
                                                    class: "simulcast-led-row",
                                                    "data-testid": "diag-simulcast-recv-leds-{kind_label}-{session_id}",
                                                    for layer_id in 0..recv_count {
                                                        {
                                                            let on = received_layer_led_on(layer_id, recv_index);
                                                            let led_class = if on {
                                                                "simulcast-led is-on"
                                                            } else {
                                                                "simulcast-led is-off"
                                                            };
                                                            let letter = layer_quality_label(layer_id, recv_count, true);
                                                            let led_label = if on { "receiving" } else { "not receiving" };
                                                            rsx! {
                                                                span {
                                                                    class: led_class,
                                                                    "data-testid": "diag-simulcast-recv-led-{kind_label}-{session_id}-{layer_id}",
                                                                    "aria-label": "{letter} layer {led_label}",
                                                                    title: "{letter} · {led_label}",
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                span { class: "simulcast-recv-peer-line", "{line}" }
                                                if is_video {
                                                    div {
                                                        class: "simulcast-recv-peer-stats",
                                                        "data-testid": "diag-simulcast-recv-stats-{session_id}",
                                                        // issue 1656 (item 3): recv-vs-painted GAP,
                                                        // both numbers visible + labelled. Values
                                                        // render in the neutral container color
                                                        // (passes AA); the em-dash carries a title/
                                                        // aria-label for AT when unknown (item 4).
                                                        span {
                                                            class: "simulcast-recv-peer-fps",
                                                            "recv "
                                                            if recv_txt == "\u{2014}" {
                                                                span {
                                                                    class: "peer-stat__value",
                                                                    title: "Received fps unknown",
                                                                    "aria-label": "Received fps unknown",
                                                                    "{recv_txt}"
                                                                }
                                                            } else {
                                                                span { class: "peer-stat__value", "{recv_txt}" }
                                                            }
                                                            " / painted "
                                                            if painted_txt == "\u{2014}" {
                                                                span {
                                                                    class: "peer-stat__value",
                                                                    title: "Painted fps unknown",
                                                                    "aria-label": "Painted fps unknown",
                                                                    "{painted_txt}"
                                                                }
                                                            } else {
                                                                span { class: "peer-stat__value", "{painted_txt}" }
                                                            }
                                                            " fps"
                                                        }
                                                        span {
                                                            class: "simulcast-recv-peer-lag",
                                                            "stale: "
                                                            // BLOCKER 1: severity hue is a non-text
                                                            // dot; the VALUE text is neutral and
                                                            // high-contrast. The dot's class carries
                                                            // the --diag-q-* color; aria-hidden since
                                                            // the title/reason text conveys meaning.
                                                            if lag_has_dot {
                                                                span {
                                                                    class: "peer-stat__dot {lag_class}",
                                                                    "aria-hidden": "true",
                                                                }
                                                            }
                                                            if lag_txt == "\u{2014}" {
                                                                span {
                                                                    class: "peer-stat__value",
                                                                    title: "Content staleness unknown",
                                                                    "aria-label": "Content staleness unknown",
                                                                    "{lag_txt}"
                                                                }
                                                            } else {
                                                                span {
                                                                    class: "peer-stat__value",
                                                                    title: "{lag_title}",
                                                                    "{lag_txt}"
                                                                }
                                                            }
                                                        }
                                                        if lag_alarm {
                                                            // a11y: the poor staleness is NOT
                                                            // color-only — the visible chip already
                                                            // shows it via the dot + neutral "N ms"
                                                            // text + title. This element is a
                                                            // REDUNDANT, sr-only announcement for
                                                            // assistive tech, so it is
                                                            // visually-hidden (not merely a faint
                                                            // color, which would itself fail AA).
                                                            span {
                                                                class: "visually-hidden",
                                                                role: "status",
                                                                "aria-live": "polite",
                                                                "Content staleness: {lag_title}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if extra > 0 {
                                    span { class: "simulcast-recv-more",
                                        "data-testid": "diag-simulcast-recv-more-{kind_label}",
                                        "+{extra} more peer(s) on {tail_label}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // #1482: per-peer device / hardware block. Rendered only when at
            // least one peer reported something ("if available"); each block
            // lists the present `label: value` rows from `format_peer_device_lines`.
            //
            // issue 1606: with many attendees this section grew unbounded, so it
            // is wrapped in a native `<details>` disclosure that is COLLAPSED by
            // DEFAULT (no `open` attr) — the space-saving goal. It reuses the
            // drawer's existing `diag-disclosure` pattern (summary + chevron) for
            // visual consistency with the Raw-stats / Build-info / Per-metric
            // disclosures above. The summary shows the peer COUNT so the user
            // knows how many blocks are hidden without expanding.
            if !device_blocks.is_empty() {
                {
                    let device_count = device_blocks.len();
                    rsx! {
                        details { class: "diag-disclosure diag-device",
                            summary { id: "diag-h-device", class: "diag-disclosure-summary",
                                svg {
                                    class: "diag-disclosure-chev",
                                    width: "12",
                                    height: "12",
                                    view_box: "0 0 12 12",
                                    path {
                                        d: "M4 2 L8 6 L4 10",
                                        fill: "none",
                                        stroke: "currentColor",
                                        stroke_width: "1.5",
                                    }
                                }
                                "Per-peer hardware ({device_count})"
                            }
                            for (session_id , peer_label , lines) in device_blocks.into_iter() {
                                div { class: "diag-device-peer",
                                    "data-testid": "diag-device-peer-{session_id}",
                                    span { class: "diag-device-peer-label", "{peer_label}" }
                                    for (label , value) in lines.into_iter() {
                                        div { class: "diag-device-row",
                                            span { class: "diag-device-row-label", "{label}" }
                                            span { class: "diag-device-row-value", "{value}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_diagnostics::{DiagEvent, Metric, MetricValue};

    fn m(name: &'static str, value: MetricValue) -> Metric {
        Metric { name, value }
    }

    /// FIX 1: a synthetic subsystem `"video"` event must format to a Reception
    /// dump that carries the fps, the bitrate, and the `to_peer` (NOT `from_peer`)
    /// label. Mutating the metric key `"fps_received"` back to `"fps"` (the old,
    /// never-emitted name) drops the FPS line → the `"30"` assertion fails.
    #[test]
    fn reception_text_uses_to_peer_and_real_metric_keys() {
        let mut map = BTreeMap::new();
        let evt = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_234_000,
            metrics: vec![
                m("fps_received", MetricValue::F64(30.0)),
                m("bitrate_kbps", MetricValue::F64(850.0)),
                m("media_type", MetricValue::Text("VIDEO".into())),
                m("from_peer", MetricValue::Text("self-id".into())),
                m("to_peer", MetricValue::Text("peer-abc".into())),
            ],
        };
        assert!(update_reception(&mut map, &evt), "keyed event must fold");
        let text = render_reception(&map).expect("non-empty map → Some");
        assert!(text.contains("FPS: 30.00"), "FPS value present: {text}");
        assert!(text.contains("850"), "bitrate present: {text}");
        assert!(text.contains("VIDEO"), "media type present: {text}");
        // The peer label is the REMOTE source (to_peer), not the local self-id.
        assert!(text.contains("peer-abc"), "to_peer label present: {text}");
        assert!(
            !text.contains("self-id"),
            "from_peer (self-id) must NOT be the label: {text}"
        );
        // Second granularity (1_234_000 ms → 1234s) — load-bearing for the
        // change-gate; see reception_render_is_stable_within_a_second.
        assert!(
            text.contains("Timestamp: 1234s"),
            "second-granularity timestamp present: {text}"
        );
        // Fields this event never carried still render with static labels.
        assert!(text.contains("Loss: -/s"), "loss placeholder: {text}");
        assert!(
            text.contains("Keyframe requests: -/s"),
            "keyframe placeholder: {text}"
        );
    }

    /// Anti-flap regression (user-reported): the heartbeat event (fps/bitrate)
    /// and the loss event (loss/keyframe) ALTERNATE for the same (peer, kind).
    /// Folding the loss event must RETAIN the previously-seen fps/bitrate —
    /// every label stays, no line vanishes. Reverting to per-event rendering
    /// fails the `FPS: 30.00` assertion after the loss event.
    #[test]
    fn reception_merges_alternating_event_shapes_without_dropping_lines() {
        let mut map = BTreeMap::new();
        let heartbeat = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_000_000,
            metrics: vec![
                m("fps_received", MetricValue::F64(30.0)),
                m("bitrate_kbps", MetricValue::F64(850.0)),
                m("media_type", MetricValue::Text("VIDEO".into())),
                m("to_peer", MetricValue::Text("peer-abc".into())),
            ],
        };
        let loss = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1_500_000,
            metrics: vec![
                m("media_type", MetricValue::Text("VIDEO".into())),
                m("to_peer", MetricValue::Text("peer-abc".into())),
                m("video_seq_loss_per_sec", MetricValue::F64(2.5)),
                m("keyframe_requests_per_sec", MetricValue::F64(0.5)),
            ],
        };
        assert!(update_reception(&mut map, &heartbeat));
        assert!(update_reception(&mut map, &loss));
        let text = render_reception(&map).expect("non-empty map");
        assert!(
            text.contains("FPS: 30.00"),
            "fps retained across the loss event: {text}"
        );
        assert!(text.contains("Loss: 2.5/s"), "loss folded in: {text}");
        assert!(
            text.contains("Keyframe requests: 0.5/s"),
            "kf folded in: {text}"
        );
        assert!(text.contains("Timestamp: 1500s"), "ts advanced: {text}");
        // Still exactly ONE block for the single (peer, kind) key.
        assert_eq!(
            text.matches("Peer: ").count(),
            1,
            "one merged block: {text}"
        );
    }

    /// Change-gate effectiveness pin: the subscribe loop suppresses re-renders
    /// by comparing rendered strings, which only works if the render is STABLE
    /// when the data hasn't changed. Two identical-data folds within the same
    /// wall-clock second must render byte-identically (the timestamp renders
    /// at second granularity); a later-second fold may differ. Reverting the
    /// timestamp to millisecond granularity fails the equality assertion —
    /// exactly the defect that made the original gate a no-op.
    #[test]
    fn reception_render_is_stable_within_a_second() {
        let mk = |ts_ms: u64| DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms,
            metrics: vec![
                m("fps_received", MetricValue::F64(30.0)),
                m("bitrate_kbps", MetricValue::F64(850.0)),
                m("media_type", MetricValue::Text("VIDEO".into())),
                m("to_peer", MetricValue::Text("peer-abc".into())),
            ],
        };
        let mut map = BTreeMap::new();
        assert!(update_reception(&mut map, &mk(1_000_000)));
        let first = render_reception(&map).expect("non-empty");
        // Same data, 400ms later — same second → identical render → the
        // subscribe loop's gate suppresses the set().
        assert!(update_reception(&mut map, &mk(1_000_400)));
        let second = render_reception(&map).expect("non-empty");
        assert_eq!(first, second, "same-second same-data render must be stable");
        // A later second is allowed to differ (and does, via the timestamp).
        assert!(update_reception(&mut map, &mk(2_000_000)));
        let third = render_reception(&map).expect("non-empty");
        assert_ne!(first, third, "later-second render reflects the new second");
    }

    /// An event lacking the (to_peer, media_type) key must not fold (no
    /// unkeyed entries), and an empty map renders as None — no empty dump.
    #[test]
    fn reception_text_none_when_no_recognized_metrics() {
        let mut map = BTreeMap::new();
        let evt = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1,
            metrics: vec![m("decode_errors_total", MetricValue::U64(0))],
        };
        assert!(
            !update_reception(&mut map, &evt),
            "unkeyed event must not fold"
        );
        assert!(render_reception(&map).is_none());
    }

    /// FIX 2: auto-select fires only for the sole-peer default case. Mutating
    /// the `user_picked` guard flips case 3; mutating `len() == 1` flips case 2.
    #[test]
    fn auto_select_peer_only_for_sole_unpicked_default() {
        let p1 = vec!["p1".to_string()];
        let p1p2 = vec!["p1".to_string(), "p2".to_string()];
        assert_eq!(
            auto_select_peer("All Peers", false, &p1),
            Some("p1".to_string())
        );
        assert_eq!(auto_select_peer("All Peers", false, &p1p2), None);
        assert_eq!(auto_select_peer("All Peers", true, &p1), None);
        assert_eq!(auto_select_peer("p1", false, &p1), None);
    }

    /// Directive 5: per-peer BUFFER classifier boundaries. 0 → poor; 39 → warn;
    /// 40 → good. Mutating any threshold flips a boundary case.
    #[test]
    fn peer_buffer_class_boundaries() {
        assert_eq!(peer_buffer_class(0), ("is-poor", "starving"));
        assert_eq!(peer_buffer_class(39), ("is-warn", "low buffer"));
        assert_eq!(peer_buffer_class(40), ("is-good", ""));
    }

    /// Directive 5: per-peer JITTER classifier boundaries. 30 → good; 31 → warn;
    /// 60 → warn; 61 → poor.
    #[test]
    fn peer_jitter_class_boundaries() {
        assert_eq!(peer_jitter_class(30), ("is-good", ""));
        assert_eq!(peer_jitter_class(31), ("is-warn", "elevated jitter"));
        assert_eq!(peer_jitter_class(60), ("is-warn", "elevated jitter"));
        assert_eq!(peer_jitter_class(61), ("is-poor", "high jitter"));
    }

    /// issue 1656: per-peer capture-LAG classifier boundaries (PLACEHOLDER
    /// thresholds, see `peer_lag_class`). good `< 1000`; warn `[1000, 1800)`;
    /// poor `>= 1800` (the standalone UI "severely behind real-time" display
    /// cut, `PEER_LAG_POOR_MS`). Mutating either threshold flips a boundary
    /// case — e.g. raising the warn boundary from 1000.0 breaks the
    /// 1000.0→warn assertion.
    #[test]
    fn peer_lag_class_boundaries() {
        assert_eq!(peer_lag_class(999.9), ("is-good", ""));
        assert_eq!(peer_lag_class(1000.0), ("is-warn", "falling behind"));
        assert_eq!(peer_lag_class(1799.9), ("is-warn", "falling behind"));
        assert_eq!(
            peer_lag_class(1800.0),
            ("is-poor", "severely behind real-time")
        );
    }

    /// issue 1656: a worker-shaped `"video"` event (carries `to_peer` +
    /// `content_staleness_ms` + `fps_painted`, but NO `media_type`) must fold
    /// (return true) under the synthetic `"VIDEO"` kind and render the new
    /// FPS(painted) and staleness lines. Removing the `kind = Some("VIDEO")`
    /// synthesis in `update_reception` makes this event fail the `(peer, kind)`
    /// gate → the fold returns false and the assertion below breaks.
    #[test]
    fn reception_worker_event_folds_lag_and_painted() {
        let mut map = BTreeMap::new();
        let worker = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 3_000_000,
            metrics: vec![
                m("from_peer", MetricValue::Text("self-id".into())),
                m("to_peer", MetricValue::Text("42".into())),
                m("content_staleness_ms", MetricValue::F64(0.0)),
                m("fps_painted", MetricValue::F64(29.97)),
            ],
        };
        assert!(
            update_reception(&mut map, &worker),
            "worker event (no media_type) must fold under synthetic VIDEO kind"
        );
        let text = render_reception(&map).expect("non-empty map → Some");
        // Synthetic VIDEO block keyed off to_peer.
        assert!(
            text.contains("Peer: 42 (VIDEO)"),
            "VIDEO block present: {text}"
        );
        // A genuine 0.0 staleness renders as a real value (not the `-` unknown)
        // once a worker event has folded — resolves the 0.0-vs-unknown ambiguity.
        assert!(
            text.contains("Stale: 0 ms"),
            "staleness line present (0.0 → 0): {text}"
        );
        // TRUE painted fps, 1-decimal.
        assert!(
            text.contains("FPS(painted): 30.0"),
            "painted fps line present (29.97 → 30.0): {text}"
        );
    }

    /// issue 1656: a worker event and a heartbeat event for the SAME peer must
    /// MERGE into ONE `VIDEO` block — the heartbeat keys `(to_peer, "VIDEO")`
    /// directly (it carries `media_type: VIDEO`), and the worker event synthesizes
    /// the same key — so fps_received and the worker fields coexist, neither
    /// dropping the other.
    #[test]
    fn reception_worker_and_heartbeat_merge_into_one_video_block() {
        let mut map = BTreeMap::new();
        let heartbeat = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 4_000_000,
            metrics: vec![
                m("fps_received", MetricValue::F64(30.0)),
                m("media_type", MetricValue::Text("VIDEO".into())),
                m("to_peer", MetricValue::Text("7".into())),
            ],
        };
        let worker = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 4_000_500,
            metrics: vec![
                m("to_peer", MetricValue::Text("7".into())),
                m("content_staleness_ms", MetricValue::F64(1234.0)),
                m("fps_painted", MetricValue::F64(28.0)),
            ],
        };
        assert!(update_reception(&mut map, &heartbeat));
        assert!(update_reception(&mut map, &worker));
        let text = render_reception(&map).expect("non-empty");
        assert_eq!(
            text.matches("Peer: ").count(),
            1,
            "one merged block: {text}"
        );
        assert!(text.contains("FPS: 30.00"), "fps_received retained: {text}");
        assert!(
            text.contains("FPS(painted): 28.0"),
            "painted folded: {text}"
        );
        assert!(text.contains("Stale: 1234 ms"), "staleness folded: {text}");
    }

    /// issue 1656 (item 3): `fold_video_readout` must merge the HEARTBEAT
    /// (`fps_received` only) and the WORKER (`content_staleness_ms` +
    /// `fps_painted` only) events for the SAME peer PER-FIELD latest-wins — a
    /// heartbeat must NOT clobber a prior worker's staleness/painted, and a
    /// worker must NOT clobber a prior heartbeat's recv. Reverting the fold to
    /// whole-struct overwrite
    /// (dropping the `is_some()` guards) makes the heartbeat null out lag/painted
    /// (or the worker null out recv) → one of the `Some(...)` asserts fails.
    #[test]
    fn fold_video_readout_per_field_latest_wins() {
        let mut map = HashMap::<String, PeerVideoReadout>::new();
        // Heartbeat first: recv only.
        let heartbeat = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1,
            metrics: vec![
                m("media_type", MetricValue::Text("VIDEO".into())),
                m("to_peer", MetricValue::Text("7".into())),
                m("fps_received", MetricValue::F64(30.0)),
            ],
        };
        assert!(
            fold_video_readout(&mut map, &heartbeat),
            "first fold changes"
        );
        assert_eq!(
            map["7"],
            PeerVideoReadout {
                recv_fps: Some(30.0),
                lag_ms: None,
                painted_fps: None
            }
        );
        // Worker next: lag + painted only — must NOT erase recv.
        let worker = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 2,
            metrics: vec![
                m("to_peer", MetricValue::Text("7".into())),
                m("content_staleness_ms", MetricValue::F64(1234.4)),
                m("fps_painted", MetricValue::F64(12.34)),
            ],
        };
        assert!(fold_video_readout(&mut map, &worker), "worker fold changes");
        assert_eq!(
            map["7"],
            PeerVideoReadout {
                // recv retained across the worker event (the recv-vs-painted GAP
                // signature depends on both surviving): 30 recv / 12.3 painted.
                recv_fps: Some(30.0),
                // lag rounded to whole ms; painted rounded to 1 decimal.
                lag_ms: Some(1234.0),
                painted_fps: Some(12.3),
            }
        );
        // A later heartbeat (recv only) must NOT erase lag/painted either.
        let heartbeat2 = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 3,
            metrics: vec![
                m("media_type", MetricValue::Text("VIDEO".into())),
                m("to_peer", MetricValue::Text("7".into())),
                m("fps_received", MetricValue::F64(29.0)),
            ],
        };
        assert!(
            fold_video_readout(&mut map, &heartbeat2),
            "recv change folds"
        );
        assert_eq!(
            map["7"],
            PeerVideoReadout {
                recv_fps: Some(29.0),
                lag_ms: Some(1234.0),
                painted_fps: Some(12.3),
            }
        );
    }

    /// issue 1656: the fold's change-gate verdict. A re-fold of the SAME rounded
    /// values must return `false` (so the caller suppresses the signal `.set()`
    /// and the drawer doesn't re-render on every ~1 Hz tick), while an event with
    /// no `to_peer` or no recognized metric must also return `false` and leave
    /// the map untouched. Removing the `*entry != before` change-gate makes the
    /// same-value re-fold return `true` → the suppression assert fails.
    #[test]
    fn fold_video_readout_change_gate_and_noop_events() {
        let mut map = HashMap::<String, PeerVideoReadout>::new();
        let worker = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1,
            metrics: vec![
                m("to_peer", MetricValue::Text("9".into())),
                m("content_staleness_ms", MetricValue::F64(500.0)),
                m("fps_painted", MetricValue::F64(24.0)),
            ],
        };
        assert!(fold_video_readout(&mut map, &worker), "first fold changes");
        // Sub-precision jitter (below the displayed digits) re-rounds to the same
        // values → no change → no re-render.
        let worker_jitter = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 2,
            metrics: vec![
                m("to_peer", MetricValue::Text("9".into())),
                m("content_staleness_ms", MetricValue::F64(500.4)),
                m("fps_painted", MetricValue::F64(24.04)),
            ],
        };
        assert!(
            !fold_video_readout(&mut map, &worker_jitter),
            "same rounded values must not re-render"
        );
        // No `to_peer` → no fold.
        let no_peer = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 3,
            metrics: vec![m("content_staleness_ms", MetricValue::F64(900.0))],
        };
        assert!(
            !fold_video_readout(&mut map, &no_peer),
            "no to_peer → no fold"
        );
        // `to_peer` but no recognized metric (e.g. a loss/keyframe event) → no
        // fold, map untouched.
        let loss = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 4,
            metrics: vec![
                m("to_peer", MetricValue::Text("9".into())),
                m("video_seq_loss_per_sec", MetricValue::F64(2.0)),
            ],
        };
        assert!(
            !fold_video_readout(&mut map, &loss),
            "no metric of interest → no fold"
        );
        assert_eq!(map.len(), 1, "no spurious entries created");
        assert_eq!(
            map["9"],
            PeerVideoReadout {
                recv_fps: None,
                lag_ms: Some(500.0),
                painted_fps: Some(24.0),
            }
        );
    }

    /// issue 1656 (media_type filter): the heartbeat producer emits the
    /// `"video"` subsystem event for BOTH VIDEO and SCREEN with the SAME
    /// `to_peer` (diagnostics_manager.rs:463-477). A peer screen-sharing while
    /// camera-on therefore produces TWO heartbeats per tick under one key — and
    /// `recv_fps` is the VIDEO (camera) row only. This pins that a SCREEN
    /// heartbeat's `fps_received` does NOT touch `recv_fps` (it must not even
    /// create/dirty the entry), while a VIDEO heartbeat DOES, and the worker
    /// event (no `media_type`) still folds lag/painted regardless. Removing the
    /// media_type filter (folding recv from SCREEN too) makes the SCREEN
    /// heartbeat set `recv_fps = 99.0` → the `recv_fps: None` / `recv_fps:
    /// Some(30.0)` asserts fail.
    #[test]
    fn fold_video_readout_ignores_screen_heartbeat_recv() {
        let mut map = HashMap::<String, PeerVideoReadout>::new();
        // SCREEN heartbeat FIRST: carries to_peer + fps_received + media_type
        // SCREEN. Its fps_received (99) must NOT land in recv_fps, and because
        // that was its only foldable field, it must not create an entry at all.
        let screen_heartbeat = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 1,
            metrics: vec![
                m("media_type", MetricValue::Text("SCREEN".into())),
                m("to_peer", MetricValue::Text("7".into())),
                m("fps_received", MetricValue::F64(99.0)),
            ],
        };
        assert!(
            !fold_video_readout(&mut map, &screen_heartbeat),
            "SCREEN heartbeat must not fold (recv dropped, nothing left)"
        );
        assert!(
            !map.contains_key("7"),
            "SCREEN-only heartbeat must not create an entry"
        );
        // VIDEO heartbeat for the SAME peer: this one DOES set recv_fps.
        let video_heartbeat = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 2,
            metrics: vec![
                m("media_type", MetricValue::Text("VIDEO".into())),
                m("to_peer", MetricValue::Text("7".into())),
                m("fps_received", MetricValue::F64(30.0)),
            ],
        };
        assert!(
            fold_video_readout(&mut map, &video_heartbeat),
            "VIDEO heartbeat folds recv_fps"
        );
        assert_eq!(
            map["7"],
            PeerVideoReadout {
                recv_fps: Some(30.0),
                lag_ms: None,
                painted_fps: None,
            }
        );
        // A LATER SCREEN heartbeat (higher fps) for the same peer must NOT
        // overwrite the camera recv_fps that was just set — this is the exact
        // last-writer-wins bug the filter prevents.
        let screen_heartbeat2 = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 3,
            metrics: vec![
                m("media_type", MetricValue::Text("SCREEN".into())),
                m("to_peer", MetricValue::Text("7".into())),
                m("fps_received", MetricValue::F64(99.0)),
            ],
        };
        assert!(
            !fold_video_readout(&mut map, &screen_heartbeat2),
            "later SCREEN heartbeat must not change the camera readout"
        );
        assert_eq!(
            map["7"].recv_fps,
            Some(30.0),
            "camera recv_fps unchanged by the SCREEN heartbeat"
        );
        // Worker event (no media_type → video pipeline) still folds
        // staleness/painted for the same peer, regardless of the SCREEN/VIDEO
        // filtering above.
        let worker = DiagEvent {
            subsystem: "video",
            stream_id: None,
            ts_ms: 4,
            metrics: vec![
                m("to_peer", MetricValue::Text("7".into())),
                m("content_staleness_ms", MetricValue::F64(1234.0)),
                m("fps_painted", MetricValue::F64(28.0)),
            ],
        };
        assert!(
            fold_video_readout(&mut map, &worker),
            "worker (no media_type) folds staleness/painted"
        );
        assert_eq!(
            map["7"],
            PeerVideoReadout {
                recv_fps: Some(30.0),
                lag_ms: Some(1234.0),
                painted_fps: Some(28.0),
            }
        );
    }

    /// issue 1656 (drift-guard): pins the UI's "poor" content-staleness display
    /// threshold (`PEER_LAG_POOR_MS`) on its own terms — it is the UI's own
    /// "severely behind real-time" severity cut, not coupled to any backend
    /// constant. This guard makes any future edit to `PEER_LAG_POOR_MS`
    /// visible/intentional rather than a silent drift. It references the
    /// production const + classifier directly — mutating `PEER_LAG_POOR_MS`
    /// fails this test AND flips `peer_lag_class`'s poor boundary.
    #[test]
    fn peer_lag_poor_threshold_is_severe_display_value() {
        assert_eq!(
            PEER_LAG_POOR_MS, 1800.0,
            "PEER_LAG_POOR_MS is the UI 'severely behind real-time' display cut; \
             change it deliberately (the chip + alarm key off it)"
        );
        // And the classifier must actually USE the const at its boundary.
        assert_eq!(
            peer_lag_class(PEER_LAG_POOR_MS),
            ("is-poor", "severely behind real-time")
        );
        assert_eq!(
            peer_lag_class(PEER_LAG_POOR_MS - 0.1),
            ("is-warn", "falling behind")
        );
    }
}
