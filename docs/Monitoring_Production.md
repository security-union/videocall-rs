# Production Monitoring Stack

## Overview

The videocall-rs monitoring stack provides end-to-end visibility from browser client to media relay server. It answers two key questions:

1. **Why did a meeting fail for everyone?** → Relay metrics (packet drops, NATS latency, queue depth)
2. **Why can't a specific person hear/see?** → Client metrics (quality scores, concealment, jitter, packet loss)

## Architecture

```
Browser Client (WASM)
  ├── Health Reporter (every 5s) → NATS health.diagnostics.>
  │                                       ↓
  │                              metrics_server (client-metrics-api:9091)
  │                                       ↓
  │                                  Prometheus ← scrapes /metrics
  │                                       ↓
  │                                    Grafana
  │
  └── Media packets → Relay Server (WS:8080 / WT:443)
                            ├── relay_* Prometheus metrics → scraped directly
                            └── NATS publish → other peers
```

## Prerequisites

### metrics-server (enables kubectl top + HPA)
```bash
kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
```

### Namespace
All videocall services run in the `videocall` namespace.

## Deployment Checklist

Deploy in this order (Prometheus must be up before relay metrics can be scraped):

```bash
export KUBECONFIG=~/do-kubeconfig
export NS=videocall

# 1. Prometheus (includes cAdvisor scraping, metric filters, alert rules)
helm upgrade --install prometheus ./helm/global/us-east/prometheus -n $NS

# 2. Grafana (auto-provisions all dashboards from JSON files)
helm upgrade --install grafana ./helm/grafana/ -f helm/global/us-east/grafana/values.yaml -n $NS

# 3. Metrics API (NATS → Prometheus bridge for client health packets)
helm upgrade --install metrics-api ./helm/global/us-east/metrics-api -n $NS

# 4. Relay servers (pod annotations enable Prometheus auto-discovery)
helm upgrade --install websocket ./helm/rustlemania-websocket \
  -f helm/global/us-east/websocket/values.yaml -n $NS
helm upgrade --install webtransport ./helm/rustlemania-webtransport \
  -f helm/global/us-east/webtransport/values.yaml -n $NS
```

### Verify deployment
```bash
# All pods running
kubectl get pods -n $NS

# Prometheus targets healthy
kubectl exec -n $NS deploy/nats-box -- \
  wget -qO- http://prometheus-server:80/api/v1/targets | python3 -c "
import json,sys
for t in json.load(sys.stdin)['data']['activeTargets']:
    print(f\"{t['labels']['job']:40s} {t['health']}\")"

# kubectl top working
kubectl top pods -n $NS

# Relay /metrics responding
kubectl exec -n $NS deploy/nats-box -- wget -qO- http://rustlemania-websocket:8080/metrics | head -5
```

## Grafana Dashboards

| Dashboard | UID | Panels | Purpose |
|---|---|---|---|
| **Meeting Investigation** | `videocall-investigation` | 36 | Primary investigative dashboard. Relay health, quality scores, audio/video deep dive, client health, server resources. |
| **Client Monitoring** | `videocall-health` | 30 | Detailed per-peer client metrics. |
| **Server Connections** | `dc5539f9-...` | 4 | Basic server connection analytics. |

Dashboards are provisioned from JSON files in `helm/grafana/dashboards/`. To update a dashboard: edit in Grafana UI → export JSON → save to the dashboards directory → commit.

### Template variables
- **Meeting Investigation**: `$meeting` — filter by meeting_id
- **Client Monitoring**: `meeting_id`, `session_id`, `from_peer`, `to_peer`

## Prometheus Configuration

### Scrape jobs

| Job | Target | Interval | What it scrapes |
|---|---|---|---|
| `videocall-client-metrics` | `client-metrics-api:9091` | 5s | Client health metrics from NATS |
| `videocall-server-stats` | `server-metrics-api:9092` | 5s | Server connection stats |
| `kubernetes-pods` | Auto-discovered | 15s | Relay server `/metrics` (via pod annotations) |
| `kubernetes-nodes-cadvisor` | Kubelet | 15s | Container CPU/memory (filtered) |
| `kubernetes-service-endpoints` | Auto-discovered | 15s | kube-state-metrics (filtered) |

### Scrape interval and Grafana alignment

The default Prometheus scrape interval for videocall pods is **15 seconds**, set via pod annotations (`prometheus.io/scrape-interval: "15s"`) or the global scrape config. The Grafana Prometheus datasource has a **minimum step** (`timeInterval`) that must match.

If `timeInterval` is larger than the scrape interval (e.g., 30s vs 15s), Grafana ignores every other data point, creating visual gaps in graphs even though the data exists in Prometheus. If Grafana's dashboard refresh rate is faster than the scrape interval (e.g., 5s refresh vs 15s scrape), it re-queries the same stale data repeatedly.

**Correct settings:**
| Setting | Value | Where |
|---|---|---|
| Prometheus scrape interval | 15s | Pod annotation or `additionalScrapeConfigs` in Prometheus config |
| Grafana datasource `timeInterval` | 15s | Datasource settings → Scrape interval (or provisioning ConfigMap `jsonData.timeInterval`) |
| Grafana dashboard refresh | 15s | Dashboard time picker (top right) |

**Standalone Prometheus** (us-east): set `scrape_interval: 15s` in `helm/global/us-east/prometheus/values.yaml` under the relevant scrape job.

**kube-prometheus-stack** (Ascend): the datasource is provisioned as a read-only ConfigMap (`kube-prometheus-stack-grafana-datasource`). To change `timeInterval`, either patch the ConfigMap and restart Grafana, or set `prometheus.prometheusSpec.scrapeInterval` in the helm values (this controls both the global scrape interval and the Grafana `timeInterval`).

### Metric filtering
All non-application scrape jobs use `metric_relabel_configs` to drop unused metrics (~96% reduction). Only container CPU/memory, resource limits, and essential kubelet metrics are kept.

Config: `helm/global/us-east/prometheus/values.yaml`

### Alert rules

| Alert | Condition | Severity |
|---|---|---|
| `RelayPacketDrops` | `rate(relay_packet_drops_total[1m]) > 0` for 1m | critical |
| `RelayNATSLatencyHigh` | NATS publish p99 > 50ms for 2m | warning |
| `RelayQueueNearFullWS` | One receiver's WS `channel="ws"` depth > 819/1024 across a 1m window | warning |
| `RelayQueueNearFullWT` | One receiver's WT per-primitive depth > 410/512 across a 1m window | warning |
| `RelayQueueNearFullWSVideoBytes` | One receiver's WS `kind="video"` bytes > 307,200 across a 1m window | warning |
| `RelayQueueNearFullWSScreenBytes` | One receiver's WS `kind="screen"` bytes > 6,369,062 across a 1m window | warning |
| `MeetingQualityDegraded` | Avg call quality < 50 for 2m | warning |
| `LowAudioConnectivity` | Peer can't hear for 1m | critical |
| `ContainerCPUHigh` | CPU > 85% of limit for 3m | warning |
| `ContainerMemoryHigh` | Memory > 85% of limit for 3m | warning |

All four `RelayQueueNearFull*` rules are `max by (room, transport, pod, <kind|channel>) (min_over_time(<series>[1m])) > <threshold>` with `for: 15s`. Three properties matter:

- **`*_by_session`, not the room-level gauge.** The room-level `relay_outbound_queue_{depth,bytes}` are written by every session's heartbeat under one label set, so the last writer per scrape wins and an idle peer erases a stalled one.
- **`max by` drops only `session_id`.** That keeps the per-receiver property — an idle peer's `0` cannot mask a stalled peer — while surviving reconnect churn, since `session_id` is regenerated per *connection* and a bare per-session series would restart the `for` timer on the very reconnect a downlink stall provokes. `pod` is retained for on-call attribution and Alertmanager routing. Per-`session_id` attribution comes from the dashboard panels, not the alert label.
- **`min_over_time` puts persistence on one receiver.** `for:` binds to the aggregated output, whose identity is constant per room, so `for:` alone asks only that *some* session be over threshold at each evaluation — possibly a different one each time. At `scrape_interval: 15s` a 1m window is 4 samples. Caveat: `min_over_time` mins over the samples that *exist* in the window, so a session that is over threshold for one scrape and then disconnects still yields a high min until its samples age out.

Time-to-page is therefore ~60s (window) + 15s (`for`) ≈ 75s, against ~30s before. That is the deliberate cost of requiring one receiver to actually sustain the condition rather than paging on a room-wide flap.

## Key Metrics Reference

### Relay server metrics (scraped directly from relay pods)
| Metric | Type | Labels | Description |
|---|---|---|---|
| `relay_packet_drops_total` | Counter | room, transport, drop_reason | Packets dropped due to full queue/mailbox |
| `relay_nats_publish_latency_ms` | Histogram | — | Time to publish media packet to NATS |
| `relay_outbound_queue_depth` | Gauge | room, transport | Outbound channel occupancy in SLOTS (WS 1024; WT default 512, env `WT_OUTBOUND_CHANNEL_CAPACITY`). Room-level: every session in the room writes this same series, so each scrape reports one arbitrary session and it does not reliably detect a single backed-up receiver — use `videocall_relay_outbound_queue_depth_by_session` for that. On WT it is the uni+datagram SUM |
| `videocall_relay_outbound_queue_depth_by_session` | Gauge | room, transport, session_id, channel | Per-receiver outbound occupancy in SLOTS, attributable (#1737). `channel` is `ws` on WebSocket, `unistream` or `datagram` on WebTransport — per primitive, not summed. The four `RelayQueueNearFull*` alerts key off this and its byte sibling, never the room-level pair |
| `relay_outbound_queue_bytes` | Gauge | room, transport, kind | Outbound channel occupancy in BYTES by kind (`video`\|`screen`\|`other`), WS only. `video` and `screen` are the dimensions the #2261 policy sheds on (80% of 384,000 B; 90% of 7,076,736 B). Audio and control are COUNTED, under `kind="other"` — they are simply never *shed* on bytes, only on slots. Room-level: each scrape reports one arbitrary session, so it does not reliably detect a single backed-up receiver — use `relay_outbound_queue_bytes_by_session` for that |
| `relay_outbound_queue_bytes_by_session` | Gauge | room, transport, session_id, kind | Per-receiver outbound occupancy in BYTES by kind, WS only (#2593). Same `kind` taxonomy and same byte budgets as `relay_outbound_queue_bytes`, but attributable: one series per receiver, so a receiver sitting at its shed point is not overwritten by an idle peer in the same room. Cardinality is sessions x 3 kinds per live room, swept on session teardown |
| `relay_active_sessions_per_room` | Gauge | room, transport | Connections per meeting |
| `relay_room_bytes_total` | Counter | room, direction | Bytes forwarded (use `rate()` for bps) |
| `relay_viewport_filtered_total` | Counter | room | VIDEO packets dropped by viewport-aware filtering (off-screen source not in receiver's viewport, HCL #988) |
| `relay_viewport_forwarded_total` | Counter | room | VIDEO packets forwarded after passing the viewport filter — denominator for "% filtered" = `filtered / (filtered + forwarded)` (HCL #988) |
| `relay_viewport_set_size` | Gauge | room | Most recently accepted viewport (desired-streams) set size per room. A collapse toward 0/1 while peers still publish is the wrongly-dropping / "froze my video" signature (HCL #988) |
| `relay_viewport_updates_total` | Counter | room, outcome | VIEWPORT control-packet update outcomes (`accepted` \| `rate_limited` \| `truncated` \| `ignored_other_subject`) — makes the DoS-guard caps fire visibly without labeling normal fan-out ignores as ownership failures (HCL #988) |

> **Viewport metrics (HCL #988) are Category B (dashboard, not alert page).** They back the "Viewport" panels on the *Relay Health (Server-Side)* row of the meeting-investigation dashboard; investigate "% filtered" spikes and set-size collapse there. A filtered-VIDEO drop is intentional bandwidth saving, not a fault; the only rules that fire on this family are `ViewportBlackout` (drops climbing while forwarded is ~zero) and the `ViewportNonVideoInvariantBreach` tripwire, both in `docker/monitoring/prometheus/alert_rules.yml`. Per-source forensics ("who dropped what from whom") are deliberately NOT labels (session IDs are unbounded); enable a scoped `RUST_LOG=...chat_server=debug` on the relay to reconstruct per-room drop detail from the VIDEO-drop debug log.

### Client quality metrics (via metrics_server)
| Metric | Description |
|---|---|
| `videocall_call_quality_score` | 0-100, min(audio, video) — **primary alerting metric** |
| `videocall_audio_quality_score` | 0-100, concealment + packet loss penalty |
| `videocall_video_quality_score` | 0-100, FPS health + decode error penalty |
| `videocall_neteq_expand_ops_per_sec` | Audio concealment rate (key audio health signal) |
| `videocall_neteq_target_delay_ms` | Jitter estimate |
| `videocall_audio_concealment_pct` | Audio concealment percentage |

### Decode-budget metrics (client-side, via metrics_server)
Receiver-side decode-budget state reported by each client in its health packet. All are `meeting_id`-keyed per-session gauges (not relay `room`-keyed), and surface in the **Decode-Budget (Client-Side)** dashboard row.

> **Note (#1580):** per-client metrics no longer carry a `display_name` label (it was user-supplied PII). To label a series by name, join the `videocall_peer_info{meeting_id,session_id,peer_id,display_name}` info-metric, e.g. `<metric> * on(meeting_id,session_id,peer_id) group_left(display_name) videocall_peer_info`.

| Metric | Type | Labels | Description |
|---|---|---|---|
| `videocall_decode_budget_effective_cap` | Gauge | meeting_id, session_id, peer_id | Current effective visible-tile cap the client is decoding. When `pressured==1`, `natural - effective_cap` is the shed magnitude (tiles dropped to avatars); a collapse here = that client is shedding video under decode pressure (HCL #987) |
| `videocall_decode_budget_natural` | Gauge | meeting_id, session_id, peer_id | Natural/unconstrained tile count — what the client would decode unthrottled. The gap above `effective_cap` is the shed magnitude (HCL #987) |
| `videocall_decode_budget_pressured` | Gauge | meeting_id, session_id, peer_id | 0/1 pressured latch; 1 = the budget loop is actively shedding tiles. A fleet-wide rise in the `sum()` = widespread weak-hardware distress (HCL #987) |
| `videocall_decode_budget_override_mode` | Gauge | meeting_id, session_id, peer_id | User override mode: 0=unspecified, 1=auto, 2=fixed. mode=2 = user manually capped tiles (user-chosen) vs auto-shed by the system — distinguishes user choice from system-forced shedding for triage (HCL #987) |
| `videocall_decode_budget_override_fixed_n` | Gauge | meeting_id, session_id, peer_id | The user's fixed tile cap when `override_mode==2` (fixed) (HCL #987) |

### Container resource metrics (via cAdvisor)
| Metric | Description |
|---|---|
| `container_cpu_usage_seconds_total` | CPU usage (use `rate()` for cores) |
| `container_memory_working_set_bytes` | Memory usage |
| `kube_pod_container_resource_limits` | Configured limits (by resource type) |

## Helm Chart Locations

| Chart | Path | Purpose |
|---|---|---|
| Prometheus | `helm/global/us-east/prometheus/` | Server config, scrape jobs, alerts |
| Grafana | `helm/grafana/` (base) + `helm/global/us-east/grafana/` (env values) | Dashboards, datasource, provisioning |
| Metrics API | `helm/global/us-east/metrics-api/` | NATS→Prometheus bridge (client + server) |
| WS relay | `helm/rustlemania-websocket/` + `helm/global/us-east/websocket/` | WebSocket relay server |
| WT relay | `helm/rustlemania-webtransport/` + `helm/global/us-east/webtransport/` | WebTransport relay server |

## Troubleshooting

### No relay metrics in Grafana
1. Check pod annotations: `kubectl get pod <relay-pod> -o yaml | grep prometheus`
2. Check Prometheus targets: `http://prometheus-server:80/api/v1/targets`
3. Verify relay `/metrics` responds: `curl http://<pod-ip>:8080/metrics`

### No client quality metrics
1. Check client-metrics-api logs: `kubectl logs deploy/client-metrics-api`
2. Verify NATS subscription: look for "Subscribed to health.diagnostics.>" in logs
3. Check health packet flow: run `vcprobe --nats nats://nats:4222 <meeting-id>`

### High Prometheus memory
Check series count: `http://prometheus-server:80/api/v1/status/tsdb`
If >10K series, verify `metric_relabel_configs` are applied (check running config at `/api/v1/status/config`).

### Stale display names (session IDs in legends)
Display names resolve within 5 seconds of a peer sending their first health packet. If session IDs persist, check that the peer's client is actually sending health packets.

---

## Screen Share Egress: Operator Callout

> **⚠️ Capacity alert:** Screen sharing at the high tier (1920×1080, 2500 kbps steady /
> 4000 kbps VBR peak) generates **approximately 4× the relay egress of a single
> camera-only participant slot.** A 20-person meeting where one participant shares their
> screen adds ~47.5 Mbps (steady) to ~76 Mbps (VBR peak) of relay egress. At high tier,
> each active screen-share slot accounts for a **~67% fan-out increase** relative to a
> camera-only participant at typical AQ-adapted bitrates.

### Why this matters

The relay is a stateless forwarder — it receives one inbound stream and copies it to
every other participant's outbound queue. Screen share egress scales as:

```
relay_egress = (N - 1) × tier_bitrate
```

At N=20, high tier: 19 × 2500 kbps = **47.5 Mbps** steady, **76 Mbps** peak. A NIC
sized for camera-only meetings may saturate under simultaneous screen share in large
calls. See [server-sizing-guide.md](server-sizing-guide.md#screen-share-bandwidth) for
the full per-tier, per-N table.

### Prometheus query to monitor screen-share egress

The relay does not currently label outbound bytes by media type. Use
`relay_room_bytes_total` to monitor total room egress and compare against the
camera-only baseline for the same N:

```promql
# Total relay egress rate for a specific room (bits/s)
rate(relay_room_bytes_total{direction="outbound", room="<meeting-id>"}[1m]) * 8

# Per-room egress across all meetings (top-10 by egress)
topk(10,
  sum by (room) (
    rate(relay_room_bytes_total{direction="outbound"}[1m])
  )
) * 8
```

An unusually high rate for a single room (>10× the median) is a strong signal that
at least one participant is screen-sharing at high tier.

### Alert recommendation

Add a room-level egress alert to catch NIC saturation before it affects other meetings:

```yaml
# In helm/global/us-east/prometheus/values.yaml alerting rules
- alert: RoomEgressHigh
  expr: |
    sum by (room) (
      rate(relay_room_bytes_total{direction="outbound"}[2m])
    ) * 8 > 100e6
  for: 1m
  labels:
    severity: warning
  annotations:
    summary: "Room {{ $labels.room }} relay egress > 100 Mbps"
    description: >
      One or more participants may be screen-sharing at high tier.
      At 100 Mbps per room, a NIC sized for camera-only meetings is near capacity.
      Check the room participant count and reduce concurrent meetings or lower the
      screen-share tier cap (SCREEN_QUALITY_TIERS in adaptive_quality_constants.rs).
```

Tune the threshold (100 Mbps) to your NIC capacity. A 1 GbE NIC is safe up to ~5
simultaneous high-tier screen-share rooms (5 × 76 Mbps peak ≈ 380 Mbps, giving 60%
headroom for camera + audio traffic).

### Measurement status

The per-tier model is derived from `SCREEN_QUALITY_TIERS` constants. Bot-based empirical
validation (one screen-share producer, N−1 observer bots) has not yet been run. Update
[server-sizing-guide.md](server-sizing-guide.md#measurement-status) with the measured
`rate(relay_room_bytes_total...)` value once the bot screen-share producer is implemented.
