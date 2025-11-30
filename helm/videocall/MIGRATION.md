# Migration Guide: From Separate Charts to Unified Videocall Chart

This guide helps you migrate from the four separate Helm charts to the unified `videocall` chart.

## The Problem We Solved

Previously, deploying all four components from separate charts with similar naming could cause **selector label collisions**, where:

- ❌ UI service might route to WebSocket pods
- ❌ WebSocket service might route to WebTransport pods
- ❌ WebTransport load balancer might route to Website pods
- ❌ All services shared the same `app.kubernetes.io/name` label

## The Solution: Component-Based Labeling

The unified chart uses a **component-specific label** to ensure each service only routes to its own pods:

```yaml
# Each component gets a unique label
app.kubernetes.io/component: ui|websocket|webtransport|website
```

### Label Structure Example

**UI Component:**
```yaml
# Pod labels (from Deployment)
labels:
  app.kubernetes.io/name: videocall
  app.kubernetes.io/instance: my-release
  app.kubernetes.io/component: ui  # <-- Unique identifier

# Service selector (from Service)
selector:
  app.kubernetes.io/name: videocall
  app.kubernetes.io/instance: my-release
  app.kubernetes.io/component: ui  # <-- Matches only UI pods
```

**WebSocket Component:**
```yaml
# Pod labels
labels:
  app.kubernetes.io/name: videocall
  app.kubernetes.io/instance: my-release
  app.kubernetes.io/component: websocket  # <-- Different from UI

# Service selector
selector:
  app.kubernetes.io/name: videocall
  app.kubernetes.io/instance: my-release
  app.kubernetes.io/component: websocket  # <-- Matches only WebSocket pods
```

This pattern is repeated for all four components, preventing any cross-component routing.

## Migration Steps

### Option 1: Clean Deployment (Recommended for Non-Production)

1. **Uninstall old charts:**
   ```bash
   helm uninstall rustlemania-ui
   helm uninstall rustlemania-websocket
   helm uninstall rustlemania-webtransport
   helm uninstall videocall-website
   ```

2. **Create unified values file:**
   ```bash
   cat > my-values.yaml <<EOF
   global:
     domain: "yourdomain.com"
     region: "us-east"
     natsUrl: "nats:4222"
   
   ui:
     enabled: true
     replicaCount: 1
     ingress:
       hosts:
         - host: app.yourdomain.com
   
   websocket:
     enabled: true
     replicaCount: 1
     ingress:
       hosts:
         - host: websocket.yourdomain.com
         - host: api.yourdomain.com
   
   webtransport:
     enabled: true
     certificateDomain: "webtransport.yourdomain.com"
   
   website:
     enabled: true
     ingress:
       hosts:
         - host: yourdomain.com
         - host: www.yourdomain.com
   EOF
   ```

3. **Install unified chart:**
   ```bash
   helm install videocall ./helm/videocall -f my-values.yaml
   ```

### Option 2: Rolling Migration (For Production)

Deploy components one at a time to minimize downtime:

1. **Deploy UI component only:**
   ```bash
   helm install videocall ./helm/videocall \
     --set ui.enabled=true \
     --set websocket.enabled=false \
     --set webtransport.enabled=false \
     --set website.enabled=false \
     -f my-values.yaml
   ```

2. **Verify UI is working, then uninstall old UI:**
   ```bash
   helm uninstall rustlemania-ui
   ```

3. **Enable WebSocket component:**
   ```bash
   helm upgrade videocall ./helm/videocall \
     --set ui.enabled=true \
     --set websocket.enabled=true \
     --set webtransport.enabled=false \
     --set website.enabled=false \
     -f my-values.yaml
   ```

4. **Verify, then uninstall old WebSocket:**
   ```bash
   helm uninstall rustlemania-websocket
   ```

5. **Repeat for WebTransport and Website**

### Option 3: Gradual Migration (Keep Both Running)

If you need zero downtime:

1. Deploy the unified chart with different release name:
   ```bash
   helm install videocall-v2 ./helm/videocall -f my-values.yaml
   ```

2. Update DNS/ingress to point to new services gradually

3. Monitor traffic shift

4. Once stable, uninstall old charts

## Verification

After migration, verify each service is routing correctly:

```bash
# Check that services exist with correct selectors
kubectl get svc -l app.kubernetes.io/instance=videocall

# Verify endpoints show only correct pods
kubectl get endpoints videocall-ui
kubectl get endpoints videocall-websocket
kubectl get endpoints videocall-webtransport-lb
kubectl get endpoints videocall-website

# Check pod labels
kubectl get pods -l app.kubernetes.io/instance=videocall --show-labels
```

You should see:
- UI service endpoints → only UI pods (with `component=ui` label)
- WebSocket service endpoints → only WebSocket pods (with `component=websocket` label)
- WebTransport LB endpoints → only WebTransport pods (with `component=webtransport` label)
- Website service endpoints → only Website pods (with `component=website` label)

## Compatibility with Existing Deployments

### Using the Chart as a Wrapper

Your existing deployment in `helm-videocall-deployment/us-east/` can continue to work by wrapping the unified chart:

**Example: `helm-videocall-deployment/us-east/videocall/Chart.yaml`**
```yaml
apiVersion: v2
name: videocall-us-east
version: 0.1.0
dependencies:
  - name: videocall
    version: 0.1.0
    repository: "file://../../../../helm/videocall"
```

**Example: `helm-videocall-deployment/us-east/videocall/values.yaml`**
```yaml
videocall:
  global:
    domain: "videocall.rs"
    region: "us-east"
    natsUrl: "nats-us-east:4222"
  
  ui:
    enabled: true
    nameOverride: "videocall-ui-us-east"
    fullnameOverride: "videocall-ui-us-east"
    replicaCount: 1
    # ... rest of UI config
  
  websocket:
    enabled: true
    nameOverride: "websocket-us-east"
    fullnameOverride: "websocket-us-east"
    # ... rest of WebSocket config
```

This approach:
- ✅ Keeps your region-specific values separate
- ✅ Uses the public chart as a library
- ✅ Maintains your existing deployment structure
- ✅ Easy to update when the public chart is updated

## Troubleshooting

### Service Routing to Wrong Pods

**Symptom:** Requests to UI are hitting WebSocket pods

**Diagnosis:**
```bash
# Check service selector
kubectl get svc videocall-ui -o yaml | grep -A 5 selector

# Should show:
#   selector:
#     app.kubernetes.io/name: videocall
#     app.kubernetes.io/instance: my-release
#     app.kubernetes.io/component: ui
```

**Fix:** Ensure you're using the unified chart, not mixing old and new charts

### Ingress Not Working

**Symptom:** 404 errors or wrong backend

**Diagnosis:**
```bash
# Check ingress backend
kubectl get ingress videocall-ui -o yaml

# Verify service name matches
kubectl get svc | grep videocall
```

### Certificate Issues

**Symptom:** WebTransport certificate not issued

**Diagnosis:**
```bash
# Check certificate status
kubectl get certificate
kubectl describe certificate videocall-webtransport-cert

# Check cert-manager logs
kubectl logs -n cert-manager deploy/cert-manager
```

## Benefits of the Unified Chart

1. **Single Source of Truth**: One chart to maintain
2. **Consistent Configuration**: Shared global values
3. **Proper Label Isolation**: Component-specific routing
4. **Simplified Deployment**: One command to deploy everything
5. **Easy Selective Deployment**: Enable/disable components with flags
6. **Better Documentation**: Comprehensive README and comments

## Rollback

If you need to rollback to the old charts:

```bash
# Uninstall unified chart
helm uninstall videocall

# Reinstall individual charts
helm install rustlemania-ui ./helm/rustlemania-ui -f old-ui-values.yaml
helm install rustlemania-websocket ./helm/rustlemania-websocket -f old-ws-values.yaml
helm install rustlemania-webtransport ./helm/rustlemania-webtransport -f old-wt-values.yaml
helm install videocall-website ./helm/videocall-website -f old-site-values.yaml
```

## Questions?

- GitHub Issues: https://github.com/security-union/videocall-rs/issues
- Discussions: https://github.com/security-union/videocall-rs/discussions

