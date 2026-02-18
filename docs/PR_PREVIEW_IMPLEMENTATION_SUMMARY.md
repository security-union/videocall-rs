# PR Preview Deployment - Implementation Summary

**Date:** 2026-02-18
**Status:** ✅ Workflows implemented, awaiting manual setup

---

## What Was Implemented

### Three GitHub Actions Workflows

#### 1. `.github/workflows/pr-deploy.yaml`
Deploys a complete preview environment when maintainers comment `/deploy` on a PR.

**Trigger:** Issue comment matching `/deploy`

**What it does:**
1. ✅ Permission check (OWNER/MEMBER/COLLABORATOR only)
2. ✅ Capacity check (max 3 concurrent previews)
3. ✅ Creates namespace `preview-{PR_NUM}` with labels
4. ✅ Applies ResourceQuota (500m CPU, 1Gi memory limits)
5. ✅ Creates postgres database `preview_{PR_NUM}`
6. ✅ Deploys NATS instance (isolated, 50m CPU, 64Mi memory)
7. ✅ Deploys WebSocket server with PR-specific images
8. ✅ Deploys Meeting API with PR-specific images
9. ✅ Deploys UI with PR-specific images
10. ✅ Posts success comment with preview URLs
11. ✅ Cleans up on failure

**Preview URLs created:**
- `https://pr-{NUM}.sandbox.videocall.rs` (UI)
- `https://pr-{NUM}-api.sandbox.videocall.rs` (Meeting API)
- `wss://pr-{NUM}-ws.sandbox.videocall.rs` (WebSocket)

#### 2. `.github/workflows/pr-undeploy.yaml`
Tears down a preview environment when maintainers comment `/undeploy` on a PR.

**Trigger:** Issue comment matching `/undeploy`

**What it does:**
1. ✅ Permission check
2. ✅ Deletes namespace (cascades to all pods/services)
3. ✅ Drops postgres database
4. ✅ Deletes GHCR images tagged `pr-{NUM}`
5. ✅ Posts confirmation comment

#### 3. `.github/workflows/pr-cleanup.yaml`
Automatically cleans up preview environments when PRs are closed or merged.

**Trigger:** PR closed (merged or not)

**What it does:**
1. ✅ Checks if preview exists
2. ✅ Deletes namespace if found
3. ✅ Drops postgres database if found
4. ✅ Deletes GHCR images

---

## Manual Setup Required

Before these workflows can be used, you must complete these one-time setup tasks:

### 1. Create Wildcard TLS Certificate

Create a certificate for `*.sandbox.videocall.rs` to cover all preview subdomains.

```yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: sandbox-wildcard-tls
  namespace: default
spec:
  secretName: sandbox-wildcard-tls
  issuerRef:
    name: letsencrypt-prod
    kind: Issuer
  dnsNames:
    - "*.sandbox.videocall.rs"
```

**Apply with:**
```bash
kubectl apply -f - <<EOF
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: sandbox-wildcard-tls
  namespace: default
spec:
  secretName: sandbox-wildcard-tls
  issuerRef:
    name: letsencrypt-prod
    kind: Issuer
  dnsNames:
    - "*.sandbox.videocall.rs"
EOF
```

**Verify:**
```bash
kubectl get certificate sandbox-wildcard-tls -n default
kubectl get secret sandbox-wildcard-tls -n default
```

### 2. Create Wildcard DNS Record

Add a single wildcard A record in DigitalOcean DNS that points to your ingress-nginx LoadBalancer IP.

**Get the LoadBalancer IP:**
```bash
kubectl get service ingress-nginx-us-east-controller -n default -o jsonpath='{.status.loadBalancer.ingress[0].ip}'
```

**In DigitalOcean DNS console:**
- Domain: `videocall.rs`
- Type: `A`
- Hostname: `*.sandbox`
- Value: `{YOUR_LB_IP}`
- TTL: `3600`

This creates: `*.sandbox.videocall.rs → {LB_IP}`

**Verify:**
```bash
dig pr-123.sandbox.videocall.rs
# Should return your LoadBalancer IP
```

### 3. Add NATS Helm Repository

This is needed by the deploy workflow to install per-preview NATS instances.

```bash
helm repo add nats https://nats-io.github.io/k8s/helm/charts/
helm repo update
```

**Note:** This only needs to be done once on your local machine for testing. The workflow adds the repo automatically in CI.

### 4. Verify Postgres Secret Exists

The workflows need to read the postgres password from a secret.

**Check:**
```bash
kubectl get secret postgres-credentials -n default
```

**If missing, create it:**
```bash
# Get password from your postgres helm values or wherever it's stored
PG_PASSWORD="your-postgres-password"

kubectl create secret generic postgres-credentials -n default \
  --from-literal=password="${PG_PASSWORD}"
```

### 5. Test Postgres Connection

Verify the workflow can create databases:

```bash
# Get password
PG_PASSWORD=$(kubectl get secret postgres-credentials -n default -o jsonpath='{.data.password}' | base64 -d)

# Test creating/dropping a database
kubectl exec -n default postgres-postgresql-0 -- \
  env PGPASSWORD="${PG_PASSWORD}" psql -U postgres -c "CREATE DATABASE test_preview;"

kubectl exec -n default postgres-postgresql-0 -- \
  env PGPASSWORD="${PG_PASSWORD}" psql -U postgres -c "DROP DATABASE test_preview;"
```

---

## Testing the Workflows

### Testing in Your Fork (Recommended)

Before submitting to upstream, test in `jboyd01/videocall-rs`:

1. **Push workflows to fork's main branch:**
   ```bash
   git checkout main
   git add .github/workflows/pr-deploy.yaml
   git add .github/workflows/pr-undeploy.yaml
   git add .github/workflows/pr-cleanup.yaml
   git commit -m "feat: add PR preview deployment workflows"
   git push origin main
   ```

2. **Update GHCR image repositories in workflows:**
   - Change `ghcr.io/security-union/` → `ghcr.io/jboyd01/`
   - Update `doctl kubernetes cluster kubeconfig save` cluster name if different

3. **Create/use test PR:**
   - Use your existing PR #1 in the fork
   - Comment `/build-images` first (if not already built)
   - Comment `/deploy` to test deployment
   - Comment `/undeploy` to test cleanup

4. **Verify:**
   - Check namespace created: `kubectl get namespace preview-1`
   - Check pods running: `kubectl get pods -n preview-1`
   - Test URLs: `https://pr-1.sandbox.videocall.rs`

### Testing in Production (After Fork Testing)

Once validated in your fork:

1. **Revert GHCR image paths:**
   - Change `ghcr.io/jboyd01/` → `ghcr.io/security-union/`

2. **Create clean PR to upstream:**
   - Include all three workflows
   - Include updated `PR_PREVIEW_DEPLOYMENT_REVIEW.md`
   - Reference issue #571

3. **Test on a real PR:**
   - Use a small test PR (e.g., documentation change)
   - Comment `/build-images` to build images
   - Comment `/deploy` to deploy preview
   - Verify all services work
   - Comment `/undeploy` to cleanup

---

## Architecture Summary

### Per-PR Resources Created

```
preview-{PR_NUM} namespace:
├── ResourceQuota (500m CPU, 1Gi memory)
├── nats-pr-{PR_NUM} (NATS pod + service)
├── websocket-pr-{PR_NUM} (deployment + service + ingress)
├── api-pr-{PR_NUM} (deployment + service + ingress)
└── ui-pr-{PR_NUM} (deployment + service + ingress)

default namespace:
└── postgres-postgresql:
    └── preview_{PR_NUM} (database)

GHCR:
├── videocall-media-server:pr-{PR_NUM}
├── videocall-meeting-api:pr-{PR_NUM}
└── videocall-web-ui:pr-{PR_NUM}
```

### Resource Usage Per Preview

| Resource | Per Preview | 3 Previews |
|----------|-------------|------------|
| CPU Request | 190m | 570m |
| Memory Request | 212Mi | 636Mi |
| Pods | 4 | 12 |
| Databases | 1 (~10MB) | 3 (~30MB) |
| Ingresses | 3 | 9 |

**Cost:** Minimal - reuses existing cluster and postgres, only adds pods.

---

## Configuration Details

### Services Connect To

**NATS:** `nats://nats-pr-{PR}.preview-{PR}.svc.cluster.local:4222` (isolated per-PR)

**Postgres:** `postgres://postgres:PASSWORD@postgres-postgresql.default.svc.cluster.local:5432/preview_{PR}` (shared instance, isolated DB)

### UI Runtime Config

```javascript
window.__APP_CONFIG = {
  apiBaseUrl: "https://pr-123-api.sandbox.videocall.rs",
  wsUrl: "wss://pr-123-ws.sandbox.videocall.rs",
  webTransportHost: "",
  webTransportEnabled: "false",  // ← WebTransport disabled for previews
  oauthEnabled: "false",         // ← OAuth disabled for previews
  e2eeEnabled: "false",          // ← Can enable if desired
  firefoxEnabled: "false",
  usersAllowedToStream: "",
  serverElectionPeriodMs: 2000,
  audioBitrateKbps: 65,
  videoBitrateKbps: 1000,
  screenBitrateKbps: 1000
}
```

### Ingress Configuration

All ingresses use:
- **TLS secret:** `sandbox-wildcard-tls` (shared wildcard cert)
- **Ingress class:** `nginx`
- **WebSocket annotations:** `proxy-read-timeout: 3600`, `proxy-send-timeout: 3600`

---

## Usage

### Deploy a Preview

1. Ensure PR images are built: comment `/build-images`
2. Wait for images to push to GHCR (~10-15 min)
3. Comment `/deploy` on the PR
4. Wait for deployment (~3-5 min)
5. Open preview URL from comment

### Update a Preview

1. Push new commits to PR
2. Comment `/build-images` to rebuild images
3. Comment `/undeploy` to tear down old preview
4. Comment `/deploy` to deploy new preview

**Note:** In-place updates are not supported. Must undeploy then redeploy.

### Undeploy a Preview

- **Manual:** Comment `/undeploy` on the PR
- **Automatic:** Close or merge the PR (auto-cleanup runs)

### Check Active Previews

```bash
kubectl get namespaces -l app=preview
```

---

## Troubleshooting

### Preview Deployment Fails

**Common causes:**
1. **Images not found:** Run `/build-images` first
2. **Capacity exceeded:** Max 3 concurrent previews, undeploy one first
3. **TLS cert missing:** Create wildcard cert (see Manual Setup #1)
4. **DNS not configured:** Add wildcard DNS record (see Manual Setup #2)
5. **Postgres secret missing:** Create postgres-credentials secret (see Manual Setup #4)

**Check logs:**
```bash
# View workflow run logs in GitHub Actions UI
# Or check pod logs:
kubectl logs -n preview-{PR} -l app.kubernetes.io/name=rustlemania-websocket
kubectl logs -n preview-{PR} -l app.kubernetes.io/name=meeting-api
kubectl logs -n preview-{PR} -l app.kubernetes.io/name=rustlemania-ui
```

### Preview Not Accessible

**DNS issues:**
```bash
# Verify DNS resolves
dig pr-123.sandbox.videocall.rs

# Should return ingress LoadBalancer IP
```

**Ingress issues:**
```bash
# Check ingress created
kubectl get ingress -n preview-123

# Check certificate
kubectl get certificate -n default sandbox-wildcard-tls
kubectl describe certificate -n default sandbox-wildcard-tls
```

**Pod issues:**
```bash
# Check pod status
kubectl get pods -n preview-123

# If ImagePullBackOff, GHCR images not accessible
# If CrashLoopBackOff, check logs: kubectl logs -n preview-123 <pod-name>
```

### Database Cleanup Failed

**Manual cleanup:**
```bash
PG_PASSWORD=$(kubectl get secret postgres-credentials -n default -o jsonpath='{.data.password}' | base64 -d)

kubectl exec -n default postgres-postgresql-0 -- \
  env PGPASSWORD="${PG_PASSWORD}" psql -U postgres -c "DROP DATABASE preview_{PR_NUM};"
```

---

## Limitations

1. **Max 3 concurrent previews** - Prevents cluster resource exhaustion
2. **No WebTransport** - Uses WebSocket fallback only (simplicity)
3. **No OAuth** - Authentication disabled for testing
4. **No in-place updates** - Must undeploy/redeploy to update
5. **Single region** - Deploys to US East only (no multi-region testing)
6. **No persistent storage** - Data is ephemeral, cleared on undeploy

---

## Security Considerations

1. ✅ **Permission gating:** Only OWNER/MEMBER/COLLABORATOR can deploy/undeploy
2. ✅ **Namespace isolation:** Each PR has isolated namespace
3. ✅ **Database isolation:** Each PR has isolated database
4. ✅ **NATS isolation:** Each PR has isolated NATS instance
5. ✅ **ResourceQuota:** Prevents DoS via resource exhaustion
6. ✅ **Capacity limit:** Max 3 concurrent previews
7. ✅ **No production secrets:** Preview JWTs are dummy values
8. ✅ **Automatic cleanup:** Previews cleaned up on PR close

---

## Next Steps

1. ✅ Complete manual setup (TLS cert, DNS, NATS repo)
2. ✅ Test in fork (`jboyd01/videocall-rs`)
3. ✅ Submit PR to upstream (`security-union/videocall-rs`)
4. ✅ Document in README or CONTRIBUTING.md
5. 📋 **Future:** Add `/deploy` to PR template suggestions
6. 📋 **Future:** Add Slack/Discord notifications on deploy/undeploy
7. 📋 **Future:** Add metrics/monitoring for preview environments
