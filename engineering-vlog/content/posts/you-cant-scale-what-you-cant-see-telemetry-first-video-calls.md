+++
title = "You Can’t Scale What You Can’t See: Telemetry‑first Video Calls with Prometheus, Grafana, and NATS"
date = 2025-08-11
description = "The last two weeks of videocall.rs: shipping Prometheus metrics, Grafana dashboards, client instrumentation, and a NATS‑backed metrics API that doesn’t lie."
authors = ["Dario Lencina Talarico"]
slug = "telemetry-first-video-calls-prometheus-grafana-nats"
tags = ["rust", "prometheus", "grafana", "nats", "observability", "webrtc", "webtransport"]
categories = ["Observability", "Distributed Systems", "Rust"]
keywords = ["prometheus", "grafana", "nats", "rust", "metrics", "slo", "videocall", "neteq"]
[taxonomies]
tags = ["rust", "prometheus", "grafana", "nats", "observability", "webrtc", "webtransport"]
authors = ["Dario Lencina Talarico"]
+++

# You Can’t Scale What You Can’t See: Telemetry‑first Video Calls with Prometheus, Grafana, and NATS

Real‑time systems don’t fail gently. They fail at the edges, in the jitter, in the microscopic places dashboards gloss over. Over the last two weeks I wired videocall.rs end‑to‑end with a monitoring path that survives reality: client instrumentation → NATS → metrics API → Prometheus → Grafana. No wishful thinking, no vanity graphs.

## Executive Summary

- First‑class Prometheus coverage end‑to‑end—client and server, not just backend counters.
- A compact `metrics-api` that fans in client health over NATS and exposes `/metrics` for Prometheus.
- Grafana panels for health/RTT/session quality—with labels kept on a leash to avoid cardinality blowups.
- Stale client series reaped within ~30s so p95 reflects reality, not ghosts.

If you care about the details, here they are.

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/grafana-neteq buffer size.jpg" alt="Grafana NetEQ dashboard" style="max-width:900px; width:100%; height:auto; border-radius:4px;" />
</p>
<p style="text-align:center; margin-top:-0.5em; color:#666; font-size:0.95em;">
  <em>NetEQ buffer size per peer — effectively receive‑side lag: how many audio packets are currently buffered before playback.</em>
</p>

## The moment guessing stopped

It clicked on a late‑night call with Seth Reid and the Fame team in the Philippines. Two continents, three networks, five time zones. We were pushing a new build, trying to understand why the experience felt “a beat behind” on their side. My notebook had quotes like “a little laggy here” and “fine on my end now,” which is to say: nothing you can chart.

Seth did what good CEOs do in hard mode: stayed calm, precise, and relentlessly constructive. He kept the room focused—“Show me what matters and we’ll push again.” We needed objective truth, not adjectives.

That night turned the monitoring plan from nice‑to‑have into ship‑it‑now. Client‑measured RTT to the elected server gave us the real latency story. NetEQ buffer depth showed when audio was on the edge of concealment. Once those numbers landed on a Grafana panel, the conversation switched from feels to facts:

> p95 RTT sits at 280–320 ms in APAC; NetEQ expands 15–25 ops/sec during spikes. Pin APAC, retry.

We tightened the loop, re‑tested, and the graphs moved the right way. Seth made it easy to do the right engineering thing—ask for the truth and keep shipping until you get it.

## What matters to engineers

- Outcomes first
  - Detect client‑perceived degradation in under 30 seconds, isolate by region/server, quantify blast radius.
  - Keep time‑series cardinality bounded so the system stays fast and cheap.

- Primary signals (read in this order)
  - Client RTT to elected server (p50/p95/p99) — truth source for user experience.
  - Audio continuity risk — NetEQ buffer depth and concealment/acceleration rates.
  - Population — active sessions, participants, peer connections to size impact.

- Architecture slice
  - Client emits diagnostics → health reporter snapshots → NATS fan‑in → `metrics-api` → Prometheus → Grafana.
  - Client series are actively deleted when a session goes quiet (~30s) to avoid lying dashboards.

- Label and cardinality strategy
  - Only bounded labels: `meeting_id`, `session_id`, `peer_id`, `server_url`, `server_type`.
  - No IPs, user‑agents, or free‑form text in labels. Those live in logs, not metrics.

- 30‑second triage workflow
  1) Check RTT SLO panel: if p95 spikes, pivot by `server_type`/`server_url` to localize.
  2) If RTT is stable but audio is bad, scan NetEQ buffer/operations — packet loss or jitter.
  3) Correlate with participant/connection counts to understand blast radius before action.

- Dashboards you actually need
  - RTT SLOs (client‑measured), NetEQ Health, and Participation/Connections. Everything else is optional.

## The Role of NATS (the nervous system)

NATS connects everything:

- Room data plane: subjects like `room.{room}.*` with queue groups per session to avoid echo and enforce delivery to the right websocket/webtransport connection
- Health fan‑in: metrics published under `health.diagnostics.{region}.{service_type}.{server_id}`; the metrics API subscribes and translates to Prometheus series
- Gateways bridge regions so cross‑region rooms still function; exporter and ServiceMonitor integration provide broker‑level observability when deployed via Helm

Why NATS here? Because pub/sub semantics plus queue groups give you load distribution and backpressure without inventing one‑off brokers. And JetStream is there if/when we need durability.

## Hard‑won lessons (so you don’t have to learn them twice)

- If you don’t delete stale series, your p95 is fiction. Clean up aggressively when clients disappear.
- Client‑measured RTT is the truth. Prefer it over server‑side timings for user experience panels.
- Label discipline matters more than “more metrics.” Keep labels bounded and meaningful; everything else belongs in logs or traces.
- Ship defaults that fit your smallest cluster first (scrape interval, retention, resources), then scale up. You can add cardinality later; you can’t un‑explode Prometheus.



