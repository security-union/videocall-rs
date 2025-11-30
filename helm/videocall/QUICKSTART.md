# Videocall Chart - Quick Start Guide

Get your videocall application running in 5 minutes!

## Prerequisites Check

```bash
# Check Kubernetes connection
kubectl cluster-info

# Verify cert-manager is installed
kubectl get pods -n cert-manager

# Verify NATS is running (or install it)
kubectl get pods -l app.kubernetes.io/name=nats
```

## Installation

### 1. Clone and Navigate

```bash
git clone https://github.com/security-union/videocall-rs.git
cd videocall-rs/helm/videocall
```

### 2. Create Your Values File

```bash
cat > my-values.yaml <<'EOF'
global:
  domain: "example.com"  # Replace with your domain
  region: "us-east"
  natsUrl: "nats:4222"

ui:
  enabled: true
  ingress:
    hosts:
      - host: app.example.com
        paths:
          - path: /
            pathType: Prefix
            service:
              name: videocall-ui
              port:
                number: 80
  runtimeConfig:
    apiBaseUrl: "https://api.example.com"
    wsUrl: "wss://websocket.example.com"
    webTransportHost: "https://webtransport.example.com:443"

websocket:
  enabled: true
  ingress:
    hosts:
      - host: websocket.example.com
        paths:
          - path: /
            pathType: Prefix
            service:
              port:
                number: 8080
      - host: api.example.com
        paths:
          - path: /
            pathType: Prefix
            service:
              port:
                number: 8080

webtransport:
  enabled: true
  certificateDomain: "webtransport.example.com"
  loadBalancerAnnotations:
    external-dns.alpha.kubernetes.io/hostname: "webtransport.example.com"

website:
  enabled: true
  ingress:
    hosts:
      - host: example.com
        paths:
          - path: /
            pathType: Prefix
            service:
              name: videocall-website
              port:
                number: 80
      - host: www.example.com
        paths:
          - path: /
            pathType: Prefix
            service:
              name: videocall-website
              port:
                number: 80
EOF
```

### 3. Install the Chart

```bash
# Install with default namespace
helm install videocall . -f my-values.yaml

# Or install in a specific namespace
kubectl create namespace videocall
helm install videocall . -f my-values.yaml -n videocall
```

### 4. Watch Deployment Progress

```bash
# Watch pods come up
kubectl get pods -l app.kubernetes.io/instance=videocall -w

# Check all resources
kubectl get all -l app.kubernetes.io/instance=videocall
```

## Verification

### Check Services and Selectors

```bash
# List all services
kubectl get svc -l app.kubernetes.io/instance=videocall

# Verify each service has correct endpoints
kubectl get endpoints | grep videocall

# Check that UI service only routes to UI pods
kubectl get endpoints videocall-ui -o yaml
```

You should see **each service routing only to its component's pods** thanks to the `app.kubernetes.io/component` label!

### Check Ingress

```bash
# Wait for ingress to be ready
kubectl get ingress -l app.kubernetes.io/instance=videocall

# Check certificate status
kubectl get certificates
```

### Test the Application

```bash
# Get the UI URL
echo "https://$(kubectl get ingress videocall-ui -o jsonpath='{.spec.rules[0].host}')"

# Visit the URL in your browser
```

## Common Scenarios

### Development: Run Only UI and WebSocket

```bash
cat > dev-values.yaml <<EOF
ui:
  enabled: true
websocket:
  enabled: true
webtransport:
  enabled: false  # Disable for local dev
website:
  enabled: false  # Disable for local dev
EOF

helm install videocall-dev . -f dev-values.yaml
```

### Production: Full Stack with Autoscaling

```bash
cat > prod-values.yaml <<EOF
ui:
  enabled: true
  replicaCount: 3
  autoscaling:
    enabled: true
    minReplicas: 2
    maxReplicas: 10

websocket:
  enabled: true
  replicaCount: 5
  autoscaling:
    enabled: true
    minReplicas: 3
    maxReplicas: 20

webtransport:
  enabled: true
  replicaCount: 3
  autoscaling:
    enabled: true
    minReplicas: 2
    maxReplicas: 15

website:
  enabled: true
  replicaCount: 2
EOF

helm install videocall . -f prod-values.yaml
```

### Multi-Region: Deploy with Region-Specific Config

```bash
cat > us-east-values.yaml <<EOF
global:
  region: "us-east"
  natsUrl: "nats-us-east:4222"

ui:
  nodeSelector:
    topology.kubernetes.io/region: "us-east-1"

websocket:
  nodeSelector:
    topology.kubernetes.io/region: "us-east-1"
  ingress:
    hosts:
      - host: websocket-us-east.example.com
      - host: api.example.com

webtransport:
  nodeSelector:
    topology.kubernetes.io/region: "us-east-1"
  certificateDomain: "webtransport-us-east.example.com"
EOF

helm install videocall-us-east . -f us-east-values.yaml
```

## Troubleshooting

### Pods Not Starting

```bash
# Check pod status
kubectl get pods -l app.kubernetes.io/instance=videocall

# Check pod logs
kubectl logs -l app.kubernetes.io/component=ui
kubectl logs -l app.kubernetes.io/component=websocket

# Describe pod to see events
kubectl describe pod <pod-name>
```

### Service Not Routing Correctly

```bash
# Verify service selectors match pod labels
kubectl get svc videocall-ui -o yaml | grep -A 5 selector
kubectl get pods -l app.kubernetes.io/component=ui --show-labels

# Check endpoints
kubectl get endpoints videocall-ui
```

**Expected:** Service selector should include `app.kubernetes.io/component: ui` and match pod labels.

### Ingress 404 Errors

```bash
# Check ingress configuration
kubectl describe ingress videocall-ui

# Verify backend service exists
kubectl get svc videocall-ui

# Check ingress controller logs
kubectl logs -n ingress-nginx -l app.kubernetes.io/component=controller
```

### Certificate Not Issued

```bash
# Check certificate status
kubectl get certificate
kubectl describe certificate videocall-ui-tls

# Check cert-manager logs
kubectl logs -n cert-manager deploy/cert-manager

# Verify issuer exists
kubectl get clusterissuer letsencrypt-prod
```

## Upgrading

```bash
# Upgrade with new values
helm upgrade videocall . -f my-values.yaml

# Upgrade specific component (disable others)
helm upgrade videocall . \
  --set ui.enabled=true \
  --set ui.replicaCount=5

# Rollback if needed
helm rollback videocall
```

## Uninstallation

```bash
# Uninstall the release
helm uninstall videocall

# Optional: Clean up PVCs and secrets
kubectl delete pvc -l app.kubernetes.io/instance=videocall
kubectl delete secret videocall-ui-tls websocket-tls api-tls webtransport-tls videocall-website-tls
```

## Next Steps

- Read the full [README.md](README.md) for detailed configuration options
- Check [MIGRATION.md](MIGRATION.md) if migrating from separate charts
- Review [values.yaml](values.yaml) for all available configuration parameters
- Visit [videocall.rs](https://videocall.rs) for documentation

## Getting Help

- **GitHub Issues**: https://github.com/security-union/videocall-rs/issues
- **Discussions**: https://github.com/security-union/videocall-rs/discussions
- **Documentation**: https://videocall.rs

---

**Happy Video Calling! 🎥**

