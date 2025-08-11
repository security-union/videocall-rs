+++
title = "You Can’t Scale What You Can’t See: Telemetry‑first Video Calls with Prometheus, Grafana, and NATS"
date = 2025-08-11
description = "Two weeks instrumenting videocall.rs: client‑measured latency, Grafana that tells the truth, and NATS as the spine."
authors = ["Dario Lencina Talarico"]
slug = "telemetry-first-video-calls-prometheus-grafana-nats"
tags = ["rust", "prometheus", "grafana", "nats", "observability", "webrtc", "webtransport"]
categories = ["Observability", "Distributed Systems", "Rust"]
keywords = ["prometheus", "grafana", "nats", "rust", "metrics", "slo", "videocall", "neteq", "video call latency"]
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
    <img src="/images/grafana-neteq-buffer-size.jpg" alt="Grafana NetEQ buffer size per peer (receive‑side lag)" style="max-width:900px; width:100%; height:auto; border-radius:4px;" />
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

- Primary signals (in order)
  - Client RTT to elected server (p50/p95/p99).
  - NetEQ buffer depth and concealment/acceleration ops.
  - Population: sessions, participants, peer connections.

- Architecture slice
  - Client diagnostics → health reporter → NATS fan‑in → `metrics-api` → Prometheus → Grafana.

- Label discipline
  - Only bounded labels: `meeting_id`, `session_id`, `peer_id`, `server_url`, `server_type`.
  - No IPs, UAs, or free‑form text in labels.

- 30‑second triage
  - Check RTT SLO; if p95 spikes, pivot by server_type/server_url.
  - If RTT is flat but audio is bad, scan NetEQ buffer/ops (loss/jitter).
  - Cross‑check population to size blast radius.

- Minimal dashboards
  - RTT SLOs, NetEQ Health, Participation/Connections.

## The Role of NATS (the nervous system)

- Subjects
  - Room: `room.{room}.*` (queue groups per session)
  - Health fan‑in: `health.diagnostics.{region}.{service_type}.{server_id}`
- Why it works here
  - Pub/sub + queue groups → load distribution and backpressure without bespoke brokers.
  - Gateways bridge regions; add JetStream if durability is needed later.

## Hard‑won lessons

- Delete stale series or your p95 is fiction.
- Client RTT is the truth source for UX.
- Bound labels; put unbounded data in logs.
- Default for your smallest cluster (scrape interval, retention, resources); scale up later.



