# `meeting_quality_xref.py` — deep investigative meeting-analysis tool

The **deep layer** behind `scripts/parse_meeting_console_logs.sh`. Where the bash script
emits COUNTS and a Prometheus copy-paste block you run by hand, this Python tool builds
**produce / consume / relay timelines**, **auto-executes Prometheus queries anchored at the
meeting epoch**, and runs a deterministic **anomaly rule engine (R1–R14)** that reproduces
investigation findings which previously took hours of manual grepping.

- **Spec:** `~/work/notebook/videocall-rs/plans/2026-06-26-meeting-quality-xref-tool-spec.md`
- **Quick-triage front end (keep using it first):** `scripts/parse_meeting_console_logs.sh`
- **Method gate it serves:** the `meeting-analysis` skill (`SKILL.md`) references this as Phase 2.

It is an ops/dev tool — stdlib-only Python (no deps, not shipped in a crate).

---

## Usage

```bash
scripts/meeting_quality_xref.py --room <name> --date YYYY-MM-DD --env <hcl-daily|ascend> [opts]
```

| Flag | Meaning |
|------|---------|
| `--room` | meeting room name (e.g. `infra`, `meeting_sync`) — also the relay `room` Prom label |
| `--date` | `YYYY-MM-DD` |
| `--env` | `hcl-daily` (fnxlabs) or `ascend` (conceptcar7 / CC7). Selects kubeconfig + Prom endpoint/auth |
| `--log-dir` | override; default `/tmp/console-logs/<room>/<date>` |
| `--end-epoch N` | meeting end epoch (s) for Prometheus anchoring; default = last log ts |
| `--bucket N` | timeline bucket size in seconds (default 30) |
| `--produce` `--consume` `--relay` `--anomalies` | render only those sections |
| `--all` | all sections (the default if none of the above are given) |
| `--drill R7` | show only findings for one rule id |
| `--json` | machine-readable output instead of markdown |
| `--no-prom` | skip ALL Prometheus queries (log-only run) |
| `--insecure-tls` | disable Prometheus TLS cert verification (only for a self-signed internal endpoint; NOT for the auth'd Ascend endpoint — verification is ON by default) |

Logs are **auto-pulled** from the API pod (`tar` over `kubectl exec`) if the directory is
empty. Both relay pods' assignment is derived from each client's `Elected connection`
(`wt_0`/`ws_0`) line — no separate relay-pod pull is required for the pod split.

### Examples

```bash
# Full report for today's infra meeting on hcl-daily (markdown, notebook-ready)
scripts/meeting_quality_xref.py --room infra --date 2026-06-26 --env hcl-daily

# Just the anomaly engine, no Prometheus (fast, offline)
scripts/meeting_quality_xref.py --room meeting_sync --date 2026-06-26 --env ascend --no-prom --anomalies

# Drill into a single rule
scripts/meeting_quality_xref.py --room infra --date 2026-06-26 --drill R1

# JSON for piping into another tool
scripts/meeting_quality_xref.py --room infra --date 2026-06-26 --json
```

---

## Output sections

1. **Header** — window, build, **pod split** (flags rooms spanning both WT+WS), and **log
   truncation** warnings (per-user last-ts vs meeting end).
2. **Anomaly summary** — severity-sorted table of every fired rule (at the TOP).
3. **Participants** — pod / own session / cores / capability_score / network (labelled UNRELIABLE).
4. **A. Produce timelines** — per user, transitions only (active_layers, union_cap, tier,
   shed/restore, audio tier).
5. **B. Consume matrix** — receiver → sender: max highest_available, max rendered layer,
   freshness-skip count.
6. **C. Relay / pod cross-reference** — pod assignment + relay Prometheus (anchored @ epoch).
7. **D. Anomaly detail + E. drill-down** — full evidence + collapsible drill-down per finding.

---

## The anomaly rule engine (R1–R14)

Each rule is a small function over the normalized event stream + Prometheus + the pod map.

| Rule | Sev | What it catches |
|------|-----|-----------------|
| **R1** | CRIT/HIGH | **#1202 cross-pod base-pin (FLAGSHIP).** Publisher pinned to base video (union_cap≤1, active=1) with a capability ceiling >1 (so the pin is *involuntary*) while consumers on the *other* pod decode the stream at only L0. The bug that took 4h to find by hand. |
| **R2** | INFO | **Periodic tick alarm.** Machine-cadence repeats (e.g. the 5.0s LAYER_PREFERENCE chooser tick) flagged as NOT user actions. Uses a modal-gap-fraction heuristic (robust to interleaved sub-cadences) + a CV fallback. |
| **R3** | MED | **Layer oscillation.** shed↔restore flapping; sub-classifies cause (CPU watchdog drift / WS uplink saturation / WT slow-ready / capability ceiling) and reports ALL contributing signals. |
| **R4** | HIGH | **WS send-side HOL / uplink saturation.** `buffered_amount` near the 1MB cliff + backpressure drops → audio HOL-blocked behind video on the shared TCP socket (WS only). |
| **R5** | HIGH/MED | **Concealment by source.** `audio_concealment_pct` > 15%. Discriminates a **source uplink fault** (heard badly by ≥2 receivers) from a **receiver downlink fault** (one receiver hears many sources badly). |
| **R6** | MED | **ProtectiveMode thrash.** >10 ENTERED/EMERGENCY cycles; buckets the trigger (`audio_buffer` ⇒ receiver audio-jitter starvation, `fps` ⇒ decode pressure). |
| **R7** | HIGH/MED | **Keyframe-starvation freeze.** held-last-good `freshness_skip` (keyframe_seq=none) with high head_age, attributed to the SENDER. Drill-down shows head_age progression + KEYFRAME_REQUEST cadence. |
| **R9** | INFO | **navigator.connection GUARD.** Prints preamble `network=` but labels it UNRELIABLE — never a bandwidth finding on its own. |
| **R10** | HIGH/LOW | **Re-election / connection instability.** re-election triggers + connection-lost events. |
| **R12** | MED | **Camera-state contradiction.** host_render never shows video=true but peers decoded the participant's video → don't report "audio-only" (the Alena miss). |
| **R13** | LOW | **Low-core device.** cores < 4 ⇒ main-thread-stall / decode-starvation risk. |
| **R14** | MED | **High-RTT environment.** baseline RTT > 200ms; notes Cato/SASE egress-PoP possibility. |

---

## Guard-rails baked into the tool (each = a real past mistake)

1. **Prometheus is ALWAYS anchored at the meeting epoch** (`time=<end_epoch>`, range vectors
   `[Nm]`). Never queries "now". An empty instant result emits a **warning** ("did you anchor
   at the meeting epoch?"), never a conclusion of "no data" — per-peer client GaugeVecs go
   stale ~5 min after the call.
2. **navigator.connection is unreliable** (R9 is a guard, not a rule).
3. **WT and WS are two ChatServer instances always** — pod assignment is a first-class column;
   #1202 bites at replicaCount:1.
4. **LAYER_SWITCH (rendered) ≠ LAYER_PREFERENCE (requested)** — SWITCH is authoritative for
   "what a receiver got".
5. **Counters need deltas** (`increase()`); head_age is UNBOUNDED; `media_kind="camera"` for
   `encoder_active_layers` but `="video"` for `received_layer`.
6. **Negative claims are backed by a real query/scan**, not an assumption. Metric names and
   labels are verified against the cluster (e.g. `audio_concealment_pct`'s source is the
   `to_peer` *session_id*, not `from_peer`; `relay_packet_drops_total` may have zero live
   series; scheduler-lag is the histogram `videocall_relay_scheduler_lag_ms_{sum,count}`).
7. **Log truncation** is detected (per-user last-ts vs meeting end) and surfaced — don't read
   "events stopped" as "behaviour stopped"; corroborate duration with Prometheus.
8. **IBA naming** — never a country; names / "IBA team".

---

## Validated against 2026-06-26 (spec §5)

| | Expected | Reproduced |
|---|---|---|
| **infra** | R1 Jay (WT, everyone else WS sees L0, union_cap=1) | ✅ CRITICAL, ceiling=3 ⇒ involuntary |
| | R2 LAYER_PREFERENCE ~5.0s for Jay | ✅ (412 events, 93% of gaps = 5.0s) |
| | R3 Mark (CPU) | ✅ 9 shed / 11 restore, CPU drift watchdog |
| | R7 Guo universal freeze source (163) | ✅ 163 freshness_skips, max head_age 6277ms |
| | relay forwarded L0/L1/L2 + 184k filtered | ✅ 184869 filtered, all 3 layers forwarded |
| **meeting_sync** | R4 Palina + Anhelina (~1MB, thousands of drops) | ✅ both ~1.1MB buffered, 2.5k+ drops |
| | R5 SOURCE=Palina 18-30%, Anhelina 13-24% | ✅ Palina 5 rx ≤31%, Anhelina 3 rx ≤23% |
| | R6 IBA users, trigger=audio_buffer | ✅ all IBA users, audio_buffer dominant |
| | R14 IBA RTT 400-7400ms | ✅ Ilya 1940ms, others 529-581ms |
| | relay healthy (scheduler_lag ~1ms) | ✅ histogram avg 1.36ms |

---

## Maintaining the parsers

Every extractor is a **free-text phrase** in the client log `msg` field — the coupling to
client code is implicit (issue #565 tracks adding a structured `event` field). When a client
emitter changes, **update `RE` in `meeting_quality_xref.py` in the same PR** (same rule as the
bash script's PATTERN INVENTORY). The regexes here were ported from / kept in sync with
`scripts/parse_meeting_console_logs.sh`.
