# Videocall Helm Chart

Unified chart for deploying the complete Videocall.rs stack.

## Components

- **UI** (port 80) - Web application
- **WebSocket** (port 8080) - Real-time signaling
- **WebTransport** (port 443 UDP) - High-performance media transport
- **Website** (port 80) - Marketing site
- **Metrics Client** (port 9091) - Client metrics collection
- **Metrics Server** (port 9092) - Server stats tracking

All components can be independently enabled/disabled.

## Quick Start

```bash
# Clone repo
git clone https://github.com/security-union/videocall-rs.git
cd videocall-rs/helm/videocall

# Install with defaults
helm install videocall .

# Or with custom values
helm install videocall . -f my-values.yaml
```

## Configuration

See [values.yaml](values.yaml) - every option is documented with comments.

**Key settings:**

```yaml
global:
  domain: "yourdomain.com"
  region: "us-east"
  natsUrl: "nats:4222"

ui:
  enabled: true
  replicaCount: 1

websocket:
  enabled: true
  
webtransport:
  enabled: true
  certificateDomain: "webtransport.yourdomain.com"

website:
  enabled: true

metricsClient:
  enabled: true

metricsServer:
  enabled: true
```

## Prerequisites

- Kubernetes 1.19+
- Helm 3.2.0+
- NATS server
- cert-manager (for TLS)
- Ingress controller (nginx recommended)

## Component Labels

Each component uses unique labels to prevent selector collisions:

```yaml
app.kubernetes.io/component: ui|websocket|webtransport|website|metrics-client|metrics-server
```

Services only route to their own component's pods.

## Secrets

**PostgreSQL** (if websocket database enabled):
```bash
kubectl create secret generic postgres-credentials --from-literal=password=YOUR_PASSWORD
```

**OAuth** (if enabled):
```bash
kubectl create secret generic google-oauth-credentials \
  --from-literal=client-id=YOUR_CLIENT_ID \
  --from-literal=client-secret=YOUR_CLIENT_SECRET
```

## Monitoring

Metrics exposed on ports 9091 (client) and 9092 (server) with Prometheus annotations.

```bash
# Access metrics
kubectl port-forward svc/videocall-metrics-client 9091:9091
curl http://localhost:9091/metrics
```

## Troubleshooting

```bash
# Check pods
kubectl get pods -l app.kubernetes.io/instance=videocall

# Check logs by component
kubectl logs -l app.kubernetes.io/component=ui
kubectl logs -l app.kubernetes.io/component=websocket

# Verify service endpoints
kubectl get endpoints
```

## More Info

- **QUICKSTART.md** - 5-minute getting started guide
- **MIGRATION.md** - Migrating from separate charts
- **CHART_SUMMARY.md** - Technical deep-dive
- **METRICS_COMPONENTS.md** - Metrics setup guide

## License

Dual-licensed under Apache 2.0 and MIT.
