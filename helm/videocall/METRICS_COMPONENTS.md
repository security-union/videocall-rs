# Metrics Components

The unified Videocall chart includes two dedicated metrics collection pods:

## Component Overview

### 1. Metrics Client (`metricsClient`)
- **Purpose**: Collects client-side metrics from NATS events
- **Port**: 9091
- **Binary**: `/usr/bin/metrics_server`
- **Service Type**: ClusterIP
- **Resources**: Minimal (50m CPU, 128Mi memory)
- **Component Label**: `app.kubernetes.io/component: metrics-client`

**What it collects**:
- Client connection events
- Media quality metrics
- User engagement statistics
- Client-side performance data

### 2. Metrics Server (`metricsServer`)
- **Purpose**: Tracks server connection statistics and aggregates server stats
- **Port**: 9092
- **Binary**: `/usr/bin/metrics_server_snapshot`
- **Service Type**: ClusterIP
- **Resources**: Minimal (100m CPU, 128Mi memory)
- **Component Label**: `app.kubernetes.io/component: metrics-server`

**What it collects**:
- Active server connections per WebSocket/WebTransport server
- Server load distribution
- Connection state snapshots
- Server health metrics

## Configuration

### Enable/Disable Metrics

```yaml
# Both metrics components enabled by default
metricsClient:
  enabled: true

metricsServer:
  enabled: true
```

### Prometheus Annotations

Both components include automatic Prometheus scraping annotations:

```yaml
metricsClient:
  podAnnotations:
    prometheus.io/scrape: "true"
    prometheus.io/port: "9091"
    prometheus.io/path: "/metrics"

metricsServer:
  podAnnotations:
    prometheus.io/scrape: "true"
    prometheus.io/port: "9092"
    prometheus.io/path: "/metrics"
```

### Resource Optimization

Both components use minimal resources for cost optimization:

```yaml
metricsClient:
  resources:
    limits:
      cpu: "50m"      # 0.05 CPU cores
      memory: "128Mi"
    requests:
      cpu: "25m"
      memory: "64Mi"

metricsServer:
  resources:
    limits:
      cpu: "100m"     # Slightly higher for aggregation
      memory: "128Mi"
    requests:
      cpu: "50m"
      memory: "64Mi"
```

## Accessing Metrics

### Via Port-Forward

```bash
# Client metrics
kubectl port-forward svc/videocall-metrics-client 9091:9091
curl http://localhost:9091/metrics

# Server stats
kubectl port-forward svc/videocall-metrics-server 9092:9092
curl http://localhost:9092/metrics
```

### Via Service Discovery

```yaml
# Prometheus scrape config
scrape_configs:
  - job_name: 'videocall-metrics'
    kubernetes_sd_configs:
      - role: service
    relabel_configs:
      - source_labels: [__meta_kubernetes_service_label_app_kubernetes_io_component]
        regex: metrics-(client|server)
        action: keep
```

## Health Checks

Both components include health endpoints:

- **Liveness Probe**: `/health` (checks if pod is alive)
- **Readiness Probe**: `/health` (checks if ready to serve traffic)

## Security Context

Both run as non-root user 1000 for security:

```yaml
podSecurityContext:
  runAsUser: 1000
  runAsGroup: 1000
  fsGroup: 1000

securityContext:
  runAsUser: 1000
  runAsGroup: 1000
```

## Service Selectors

Each component has unique selectors to prevent routing collisions:

```yaml
# Metrics Client Service
selector:
  app.kubernetes.io/name: videocall
  app.kubernetes.io/instance: my-release
  app.kubernetes.io/component: metrics-client

# Metrics Server Service
selector:
  app.kubernetes.io/name: videocall
  app.kubernetes.io/instance: my-release
  app.kubernetes.io/component: metrics-server
```

## Deployment Example

```yaml
# values.yaml
global:
  domain: "videocall.rs"
  region: "us-east"
  natsUrl: "nats:4222"

metricsClient:
  enabled: true
  replicaCount: 1
  env:
    - name: METRICS_PORT
      value: "9091"
    - name: NATS_URL
      value: nats:4222
    - name: REGION
      value: "us-east"

metricsServer:
  enabled: true
  replicaCount: 1
  env:
    - name: METRICS_PORT
      value: "9092"
    - name: NATS_URL
      value: nats:4222
    - name: REGION
      value: "us-east"
```

## Verification

After deployment, verify both metrics pods are running:

```bash
# Check pods
kubectl get pods -l app.kubernetes.io/component=metrics-client
kubectl get pods -l app.kubernetes.io/component=metrics-server

# Check services
kubectl get svc -l app.kubernetes.io/component=metrics-client
kubectl get svc -l app.kubernetes.io/component=metrics-server

# Check endpoints
kubectl get endpoints | grep metrics
```

## Integration with Grafana

Create a Grafana dashboard to visualize metrics from both endpoints:

```yaml
# datasource.yaml
apiVersion: 1
datasources:
  - name: Videocall Metrics
    type: prometheus
    url: http://prometheus:9090
    access: proxy
    isDefault: true
```

Query examples:
- Client metrics: `{job="videocall-metrics", component="metrics-client"}`
- Server stats: `{job="videocall-metrics", component="metrics-server"}`

## Troubleshooting

### Metrics not appearing in Prometheus

1. Check pod annotations:
   ```bash
   kubectl get pods -l app.kubernetes.io/component=metrics-client -o jsonpath='{.items[0].metadata.annotations}'
   ```

2. Verify service is exposing the right port:
   ```bash
   kubectl get svc videocall-metrics-client -o yaml
   ```

3. Check Prometheus target status at `/targets`

### Pod not starting

1. Check logs:
   ```bash
   kubectl logs -l app.kubernetes.io/component=metrics-client
   kubectl logs -l app.kubernetes.io/component=metrics-server
   ```

2. Verify NATS connectivity:
   ```bash
   kubectl exec -it deployment/videocall-metrics-client -- sh
   nc -zv nats 4222
   ```

### Health check failures

Check health endpoint directly:
```bash
kubectl port-forward svc/videocall-metrics-client 9091:9091
curl http://localhost:9091/health
```
