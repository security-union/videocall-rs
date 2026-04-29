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
| `RelayQueueNearFull` | Queue depth > 200/256 for 30s | warning |
| `MeetingQualityDegraded` | Avg call quality < 50 for 2m | warning |
| `LowAudioConnectivity` | Peer can't hear for 1m | critical |
| `ContainerCPUHigh` | CPU > 85% of limit for 3m | warning |
| `ContainerMemoryHigh` | Memory > 85% of limit for 3m | warning |

## Key Metrics Reference

### Relay server metrics (scraped directly from relay pods)
| Metric | Type | Labels | Description |
|---|---|---|---|
| `relay_packet_drops_total` | Counter | room, transport, drop_reason | Packets dropped due to full queue/mailbox |
| `relay_nats_publish_latency_ms` | Histogram | — | Time to publish media packet to NATS |
| `relay_outbound_queue_depth` | Gauge | room | WT channel occupancy (capacity 256) |
| `relay_active_sessions_per_room` | Gauge | room, transport | Connections per meeting |
| `relay_room_bytes_total` | Counter | room, direction | Bytes forwarded (use `rate()` for bps) |

### Client quality metrics (via metrics_server)
| Metric | Description |
|---|---|
| `videocall_call_quality_score` | 0-100, min(audio, video) — **primary alerting metric** |
| `videocall_audio_quality_score` | 0-100, concealment + packet loss penalty |
| `videocall_video_quality_score` | 0-100, FPS health + decode error penalty |
| `videocall_neteq_expand_ops_per_sec` | Audio concealment rate (key audio health signal) |
| `videocall_neteq_target_delay_ms` | Jitter estimate |
| `videocall_audio_concealment_pct` | Audio concealment percentage |

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
