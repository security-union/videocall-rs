# Videocall-rs Server Sizing Guide

Single-node (non-HA) Kubernetes deployment sizing for videocall-rs.

Based on load testing with 20-participant bot meetings using realistic VP9 video
(costume recordings at 30fps/1000kbps) and Opus audio with DTX. Measurements
taken April 2026 on HCL K3s daily build cluster with 2-meeting and 3-meeting
simultaneous load tests.

## Sizing Formula

### CPU (cores)

```
Total cores = K8s base + Monitoring + App infra + Meeting load

K8s base:       0.5 cores   (kubelet, kube-proxy, containerd, node-exporter)
Monitoring:     0.2 cores   (Prometheus @ 15s scrape + Grafana)
App infra:      0.1 cores   (meeting-api + postgres + metrics-api + NATS idle + UI)
Meeting load:   0.2 cores × N   (WS relay + NATS fan-out per 20-person meeting)

Total = 0.8 + (0.2 × N)
```

### Memory (GiB)

```
Total RAM = K8s base + Monitoring + App infra + Meeting load

K8s base:       1.5 GiB     (kubelet, containerd, system)
Prometheus:     0.5 GiB     (grows ~50 MiB per active meeting from metric series)
Grafana:        0.1 GiB
App infra:      0.2 GiB     (postgres + meeting-api + metrics-api + UI)
NATS:           0.1 GiB
Relay per mtg:  0.025 GiB × N   (~25 MiB per 20-person meeting, both relays)

Total = 2.4 + (0.075 × N)
```

## Quick Reference Table

| Simultaneous meetings (20 users each) | CPU (cores) | RAM (GiB) | Recommended node |
|---|---|---|---|
| 1 | 1.0 | 2.5 | 2 core / 4 GiB |
| 5 | 1.8 | 2.8 | 2 core / 4 GiB |
| 10 | 2.8 | 3.2 | 4 core / 4 GiB |
| 15 | 3.8 | 3.5 | 4 core / 8 GiB |
| 20 | 4.8 | 3.9 | 6 core / 8 GiB |
| 50 | 10.8 | 6.2 | 12 core / 8 GiB |

## Measurement Basis

| Metric | Value | Source |
|---|---|---|
| Per-meeting relay CPU (WS) | ~170m steady state | 3-meeting tests (avg of 2 runs) |
| Per-meeting NATS CPU | ~30m steady state | 3-meeting tests |
| Per-meeting relay memory | ~25 MiB | cAdvisor during 20-bot meeting |
| Prometheus memory growth | ~50 MiB per meeting | 420 health metric series per 20-person meeting |
| Scaling behavior | Sub-linear | 3 meetings cost less per-meeting than 2 |

## Caveats

**CPU is the binding constraint.** Memory is almost irrelevant for the relay — it
forwards packets without buffering. Even at 50 meetings the relay memory is under
2 GiB.

**Conservative estimates.** Bot participants send at maximum bitrate (1000 kbps VP9)
and never adapt quality downward. Real users trigger adaptive quality within
minutes of a large meeting, reducing their bitrate to 150-600 kbps. Real meetings
will use less CPU than these estimates.

**WebSocket only.** These numbers are validated for the WS relay path. The WT relay
has similar per-meeting cost but bot-side WT inbound reception has a known bug
(accept_uni issue) that prevented full bidirectional WT load testing. Verify with
real WT clients before sizing WT-heavy deployments.

**Network bandwidth.** Not measured in these tests but can be estimated:
each 20-person meeting generates ~20 × 19 × 200 kbps ≈ 76 Mbps of relay egress
at typical AQ-adapted bitrates. At 50 meetings that's ~3.8 Gbps — factor NIC
capacity for large deployments.

**Client-side limit.** The server can handle many more meetings than any single
client can handle streams. Browser VP9 decode overload triggers AQ death spiral
at ~15 simultaneous video streams regardless of server capacity. Large meetings
(20+ participants) need selective forwarding or simulcast to work well.

**Single-node only.** For production HA deployments, plan for at least 2 nodes and
account for one node failing (N+1 capacity).

## How These Numbers Were Obtained

Load testing used the videocall-rs bot (`bot/` crate) with costume video
(pre-recorded Google Meet costume filter clips normalized to 1280x720 I420 frames).
Each bot sends VP9 at 30fps/1000kbps + Opus audio at 48kHz with DTX silence
suppression, and sends health packets every 1s with quality scores.

Tests were run on the HCL K3s daily build cluster (app.videocall.fnxlabs.com)
with Prometheus at 15s scrape intervals. CPU was measured from cAdvisor
`container_cpu_usage_seconds_total` counters using instant queries at known
timestamps with manual rate calculation.

See `bot/README.md` for bot usage and the bot improvement plan in project memory
for planned enhancements.
