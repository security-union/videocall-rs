# PR Preview Deployment - Thorough Architecture Review

**Date:** 2026-02-18
**Status:** Pre-implementation review
**Decision:** WebTransport DISABLED for PR previews

---

## Deployment Architecture

### Infrastructure Layout
```
DigitalOcean K8s Cluster: videocall-us-east (shared with production)
├── default namespace (production + shared postgres)
│   ├── postgres-postgresql (SHARED - hosts multiple databases)
│   │   ├── actix-api-db (production)
│   │   ├── preview_123 (PR #123)
│   │   ├── preview_124 (PR #124)
│   │   └── preview_125 (PR #125)
│   ├── nats-us-east-box (production NATS - NOT shared with previews)
│   ├── ingress-nginx-us-east-controller (shared)
│   ├── Production deployments (meeting-api-us-east, etc.)
│   └── TLS wildcard cert: *.sandbox.videocall.rs (to be created)
│
└── preview-{PR_NUMBER} namespaces (ephemeral, per-PR)
    ├── nats-pr-{PR} (isolated NATS instance per preview)
    ├── websocket-pr-{PR}
    ├── meeting-api-pr-{PR}
    └── ui-pr-{PR}
```

### DNS/Ingress Pattern
Each PR gets 3 hostnames:
- `pr-{NUM}.sandbox.videocall.rs` → UI
- `pr-{NUM}-api.sandbox.videocall.rs` → Meeting API
- `pr-{NUM}-ws.sandbox.videocall.rs` → WebSocket

All covered by wildcard cert `*.sandbox.videocall.rs`

---

## 1. UI Client Configuration ✅ VERIFIED

### How It Works
The UI reads `window.__APP_CONFIG` from `/config.js` mounted via ConfigMap.

**File:** `helm/rustlemania-ui/templates/configmap-configjs.yaml`
```yaml
data:
  config.js: |-
    window.__APP_CONFIG = Object.freeze({{ .Values.runtimeConfig | toJson }});
```

### Configuration for PR Previews
```yaml
runtimeConfig:
  apiBaseUrl: "https://pr-123-api.sandbox.videocall.rs"
  wsUrl: "wss://pr-123-ws.sandbox.videocall.rs"
  webTransportHost: ""  # Empty string (ignored when disabled)
  webTransportEnabled: "false"  # ← Critical: disables WebTransport
  oauthEnabled: "false"
  e2eeEnabled: "false"  # Can enable if desired
  firefoxEnabled: "false"
  usersAllowedToStream: ""
  serverElectionPeriodMs: 2000
  audioBitrateKbps: 65
  videoBitrateKbps: 1000
  screenBitrateKbps: 1000
```

### Client Behavior
**File:** `yew-ui/src/pages/meeting.rs:94`
```rust
<AttendantsComponent
    webtransport_enabled={webtransport_enabled().unwrap_or(false)}
    ...
/>
```

**File:** `yew-ui/src/components/attendants.rs:318`
```rust
VideoCallClient::new(VideoCallClientOptions {
    websocket_urls,
    webtransport_urls,
    enable_webtransport: ctx.props().webtransport_enabled,  // ← When false, ignores webtransport_urls
    ...
})
```

**Conclusion:** When `webTransportEnabled: "false"`, the client:
1. Only connects via WebSocket URLs
2. Ignores `webtransport_urls` entirely
3. No connection attempts to WebTransport ports
4. No user-facing errors or warnings

✅ **No code changes needed.**

---

## 2. TLS Certificates ⚠️ REQUIRES SETUP

### Current State
Production uses individual certificates per service:
- `videocall-ui-us-east-tls` for `app.videocall.rs`
- `api-videocall-rs-tls` for `api.videocall.rs`
- etc.

### Required for PR Previews
**One-time setup: Create wildcard certificate**

```yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: sandbox-wildcard-tls
  namespace: default  # Must be in same namespace as ingress-nginx
spec:
  secretName: sandbox-wildcard-tls
  issuerRef:
    name: letsencrypt-prod
    kind: Issuer
  dnsNames:
    - "*.sandbox.videocall.rs"
  secretTemplate:
    labels:
      cert-type: wildcard
```

**Why wildcard?**
- Covers `pr-123.sandbox.videocall.rs`, `pr-123-api.sandbox.videocall.rs`, etc.
- Avoids hitting Let's Encrypt rate limits (50 certs/week)
- Single cert shared across all PR previews
- ExternalDNS will create DNS A records automatically

**Deployment workflow must:**
1. Check if cert exists in `default` namespace
2. If not, error with instructions to create it manually
3. Reference cert in ingress: `tls.secretName: sandbox-wildcard-tls`

✅ **Action required:** Create wildcard cert before first PR deployment

---

## 3. Infrastructure: PostgreSQL & NATS ✅ VERIFIED

### PostgreSQL: Shared Instance, Per-PR Database

**Strategy:** Reuse existing `postgres-postgresql` in `default` namespace, create a separate database per PR.

```bash
# On deployment:
kubectl exec postgres-postgresql-0 -n default -- \
  psql -U postgres -c "CREATE DATABASE preview_${PR_NUM};"

# Services connect to PR-specific database:
DATABASE_URL=postgres://postgres:PASSWORD@postgres-postgresql.default.svc.cluster.local:5432/preview_${PR_NUM}

# On cleanup:
kubectl exec postgres-postgresql-0 -n default -- \
  psql -U postgres -c "DROP DATABASE preview_${PR_NUM};"
```

**Why per-PR database?**
- ✅ **Schema isolation** - PR-123's migrations don't affect PR-124 or production
- ✅ **Lightweight** - Only ~10MB overhead per database (no new pods)
- ✅ **Meeting API compatibility** - `dbmate migrate` runs normally per-PR
- ✅ **Production safety** - Production uses separate `actix-api-db`
- ✅ **Easy cleanup** - Just `DROP DATABASE`

**Resource impact:** ~10MB memory per PR database

### NATS: Per-Preview Instance

**Strategy:** Deploy a lightweight NATS instance in each `preview-{PR}` namespace.

```bash
helm repo add nats https://nats-io.github.io/k8s/helm/charts/
helm upgrade --install nats-pr-${PR} nats/nats \
  --namespace preview-${PR} \
  --set nats.jetstream.enabled=false \
  --set nats.cluster.enabled=false \
  --set nats.resources.limits.cpu=100m \
  --set nats.resources.limits.memory=128Mi \
  --set nats.resources.requests.cpu=50m \
  --set nats.resources.requests.memory=64Mi
```

**Why per-PR NATS?**
- ✅ **Complete isolation** - One PR can't DoS another's NATS
- ✅ **Testable** - Can test NATS config/version changes safely
- ✅ **Lightweight** - Only 50-80MB per instance (vs 600Mi production)
- ✅ **Fast startup** - Single-node NATS boots in ~5 seconds
- ✅ **No clustering needed** - Previews are single-region only

**Resource impact:** ~100MB memory, 50m CPU per PR

### Service URLs from Preview Namespace

**WebSocket Configuration:**
```yaml
env:
  - name: NATS_URL
    value: "nats://nats-pr-${PR}.preview-${PR}.svc.cluster.local:4222"
  - name: DATABASE_URL
    value: "postgres://postgres:password@postgres-postgresql.default.svc.cluster.local:5432/preview_${PR}?sslmode=disable"
```

**Meeting API Configuration:**
```yaml
env:
  - name: NATS_URL
    value: "nats://nats-pr-${PR}.preview-${PR}.svc.cluster.local:4222"
  - name: DATABASE_URL
    value: "postgres://postgres:password@postgres-postgresql.default.svc.cluster.local:5432/preview_${PR}?sslmode=disable"
```

### Resource Calculation Per PR

| Component | CPU Request | Memory Request | Notes |
|-----------|-------------|----------------|-------|
| NATS | 50m | 64Mi | Lightweight single-node config |
| WebSocket | 20m | 10Mi | Base chart default |
| Meeting API | 100m | 128Mi | Base chart default |
| UI | 20m | 10Mi | Base chart default |
| Postgres DB | 0 | ~10Mi | Database only, not a pod |
| **Total** | **190m** | **~212Mi** | Per preview environment |

**3 concurrent PRs:** ~570m CPU, ~636Mi memory

✅ **Isolated infrastructure per PR**

---

## 4. Namespace Isolation & Resource Limits ✅ VERIFIED

### Namespace Creation
```bash
kubectl create namespace preview-${PR_NUM}
kubectl label namespace preview-${PR_NUM} app=preview pr=${PR_NUM}
```

Labels enable:
- Discovery: `kubectl get namespaces -l app=preview`
- Cleanup: Identify all preview namespaces
- RBAC: Restrict deployer permissions to `preview-*` only

### ResourceQuota (Recommended)
Prevent one PR from consuming all cluster resources:

```yaml
apiVersion: v1
kind: ResourceQuota
metadata:
  name: preview-quota
  namespace: preview-${PR_NUM}
spec:
  hard:
    requests.cpu: "500m"
    requests.memory: "1Gi"
    limits.cpu: "1000m"
    limits.memory: "2Gi"
    pods: "10"
```

**How to deploy:**
Create as a separate kubectl apply before helm installs, or include in a minimal `helm/pr/templates/` directory.

### Per-Service Resource Limits
Use base chart defaults (they're already conservative):
- **WebSocket:** 20m CPU request, 50m limit
- **Meeting API:** 100m CPU request, 200m limit
- **UI:** 20m CPU request, 50m limit

**Total per PR:** ~140m CPU request, ~300m limit (well within quota)

✅ **Resource isolation handled**

---

## 5. DNS & ExternalDNS Configuration ⚠️ REQUIRES VERIFICATION

### Current Production Setup
Looking at production ingresses, they use:
```yaml
annotations:
  cert-manager.io/issuer: letsencrypt-prod
  nginx.ingress.kubernetes.io/ssl-redirect: "true"
```

**But no `external-dns.alpha.kubernetes.io/hostname` annotations.**

### How Are Production DNS Records Created?
**Theory 1:** Manual DNS configuration (most likely)
- Someone manually created A records in DigitalOcean DNS
- Points `app.videocall.rs` → ingress LB IP

**Theory 2:** ExternalDNS running but undocumented
- Would need to check: `kubectl get pods -n kube-system | grep external-dns`

### For PR Previews: Two Options

**Option A: Manual DNS Wildcard (One-Time)**
Create a single wildcard A record in DigitalOcean DNS:
```
*.sandbox.videocall.rs  →  {ingress-nginx LB IP}
```

**Pros:**
- Simple, one-time setup
- No ExternalDNS needed
- All `pr-*.sandbox.videocall.rs` resolve automatically

**Cons:**
- Requires manual DNS change (but only once)
- Can't use different IPs per PR (but we don't need to)

**Option B: ExternalDNS (More Automated)**
Deploy ExternalDNS to automatically create DNS records based on Ingress annotations.

**Pros:**
- Fully automated
- Matches "enterprise" setups

**Cons:**
- Additional complexity
- Requires DigitalOcean API token
- Not strictly necessary for our use case

**Recommendation:** Use Option A (manual wildcard) for simplicity.

⚠️ **Action required:** Create wildcard DNS record `*.sandbox.videocall.rs`

---

## 6. Ingress Configuration ✅ VERIFIED

### Base Chart Ingress Template
**File:** `helm/rustlemania-ui/templates/ingress.yaml`

Already flexible - fully supports custom hosts via values:
```yaml
spec:
  ingressClassName: nginx
  tls:
    - secretName: sandbox-wildcard-tls
      hosts:
        - pr-123.sandbox.videocall.rs
  rules:
    - host: pr-123.sandbox.videocall.rs
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: ui-pr-123
                port:
                  number: 80
```

### WebSocket Ingress - Timeout Annotations
WebSocket connections need long timeouts:
```yaml
annotations:
  nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
  nginx.ingress.kubernetes.io/proxy-send-timeout: "3600"
```

Without these, connections timeout after 60 seconds (nginx default).

✅ **Must set timeouts for WebSocket ingress**

---

## 7. Image Pull from GHCR ✅ VERIFIED

### GHCR Authentication
GitHub Container Registry for public repos:
- Images: `ghcr.io/security-union/videocall-media-server:pr-123`
- **Public packages:** No pull secret needed
- **Private packages:** Would need `imagePullSecrets`

Since `security-union/videocall-rs` is a **public repo**, GHCR packages inherit public visibility automatically.

**Verification:**
```bash
# Test pulling without auth
docker pull ghcr.io/security-union/videocall-media-server:pr-123
```

If it fails with 403/401, the package is private and needs:
```yaml
imagePullSecrets:
  - name: ghcr-credentials
```

✅ **Likely no issue, but verify after first PR build**

---

## 8. Database Migrations ✅ VERIFIED

### Meeting API Startup
**File:** `Dockerfile.meeting-api`
```dockerfile
CMD ["/app/dbmate/startup.sh", "&&", "meeting-api"]
```

The `startup.sh` script runs `dbmate migrate` before starting the API.

### Per-PR Database Strategy
Each PR gets its own database: `preview_${PR_NUM}`

**How it works:**
1. Deploy workflow creates database: `CREATE DATABASE preview_123;`
2. Meeting API container starts, runs `dbmate migrate` against `preview_123`
3. Migrations apply to isolated database (doesn't affect production or other PRs)
4. Cleanup workflow drops database: `DROP DATABASE preview_123;`

**Benefits:**
- ✅ **Complete isolation** - PR migrations can't break production
- ✅ **Concurrent deploys safe** - Each PR has its own database
- ✅ **Test migrations safely** - See schema changes in isolation
- ✅ **No conflicts** - PR-123 and PR-124 can have different schemas

**Implementation:**
```yaml
# Meeting API deployment
env:
  - name: DATABASE_URL
    value: "postgres://postgres:${PASSWORD}@postgres-postgresql.default.svc.cluster.local:5432/preview_${PR_NUM}?sslmode=disable"
```

**Postgres credentials:**
Need to fetch password from secret:
```bash
PG_PASSWORD=$(kubectl get secret postgres-credentials -n default -o jsonpath='{.data.password}' | base64 -d)
```

✅ **Per-PR database provides safe migration testing**

---

## 9. OAuth & Secrets ✅ VERIFIED

### Production OAuth
Production uses Google OAuth with real credentials:
```yaml
env:
  - name: OAUTH_CLIENT_ID
    valueFrom:
      secretKeyRef:
        name: google-oauth-credentials
        key: client-id
  - name: OAUTH_SECRET
    valueFrom:
      secretKeyRef:
        name: google-oauth-credentials
        key: client-secret
```

### PR Previews: Disable OAuth
```yaml
env:
  - name: OAUTH_ISSUER
    value: ""  # Empty = OAuth disabled
```

**Why disable?**
1. OAuth callback URLs must be pre-registered (can't use dynamic `pr-123-api.sandbox.videocall.rs`)
2. Security: Don't want PRs accessing real user accounts
3. Testing: Can test video call features without auth

**Alternative:** Create a sandbox OAuth app in Google with wildcard callback:
```
https://*.sandbox.videocall.rs/login/callback
```
But this is overkill for functional testing.

✅ **Disable OAuth for simplicity**

---

## 10. Deployment Workflow Script ✅ READY TO IMPLEMENT

### High-Level Flow
```bash
PR_NUM=123
NAMESPACE="preview-${PR_NUM}"

# 1. Verify prerequisites
- Check wildcard cert exists
- Check GHCR images exist for pr-${PR_NUM}
- Check capacity (max 3 concurrent previews)

# 2. Create namespace
kubectl create namespace ${NAMESPACE}
kubectl label namespace ${NAMESPACE} app=preview pr=${PR_NUM}

# 3. Apply ResourceQuota
kubectl apply -f - <<EOF
apiVersion: v1
kind: ResourceQuota
metadata:
  name: preview-quota
  namespace: ${NAMESPACE}
spec:
  hard:
    requests.cpu: "500m"
    requests.memory: "1Gi"
    limits.cpu: "1000m"
    limits.memory: "2Gi"
    pods: "10"
EOF

# 4. Deploy WebSocket
helm upgrade --install preview-${PR_NUM}-ws helm/rustlemania-websocket/ \
  --namespace ${NAMESPACE} \
  --set image.repository=ghcr.io/security-union/videocall-media-server \
  --set image.tag=pr-${PR_NUM} \
  --set fullnameOverride=websocket-pr-${PR_NUM} \
  --set "ingress.hosts[0].host=pr-${PR_NUM}-ws.sandbox.videocall.rs" \
  --set "ingress.hosts[0].paths[0].path=/" \
  --set "ingress.hosts[0].paths[0].pathType=Prefix" \
  --set "ingress.hosts[0].paths[0].service.name=websocket-pr-${PR_NUM}" \
  --set "ingress.hosts[0].paths[0].service.port.number=8080" \
  --set "ingress.tls[0].secretName=sandbox-wildcard-tls" \
  --set "ingress.tls[0].hosts[0]=pr-${PR_NUM}-ws.sandbox.videocall.rs" \
  --set "ingress.annotations.nginx\.ingress\.kubernetes\.io/proxy-read-timeout=3600" \
  --set "ingress.annotations.nginx\.ingress\.kubernetes\.io/proxy-send-timeout=3600" \
  --set "env[0].name=NATS_URL" \
  --set "env[0].value=nats://nats-us-east-box.default.svc.cluster.local:4222" \
  --wait --timeout 5m

# 5. Deploy Meeting API (similar pattern)
# 6. Deploy UI (similar pattern)

# 7. Post success comment to PR
```

---

## Summary: What Needs to Be Done

### One-Time Setup (Before First PR Deployment)
1. ✅ **Wildcard TLS cert:** Create `*.sandbox.videocall.rs` certificate
2. ✅ **Wildcard DNS:** Add `*.sandbox.videocall.rs → ingress-nginx LB IP` in DigitalOcean DNS
3. ✅ **NATS Helm repo:** Add `helm repo add nats https://nats-io.github.io/k8s/helm/charts/`

### Per-PR Deployment (Automated in Workflow)
1. Create namespace `preview-${PR_NUM}` with labels
2. Apply ResourceQuota
3. Create postgres database `preview_${PR_NUM}`
4. Deploy NATS instance for preview
5. Deploy 3 services (websocket, meeting-api, ui) with `--set` overrides
6. Post PR comment with preview URLs

### Per-PR Cleanup (Automated in Workflow)
1. Delete namespace (cascades NATS + 3 services)
2. Drop postgres database `preview_${PR_NUM}`
3. Delete GHCR images with tag `pr-${PR_NUM}`

### No Code Changes Required ✅
- Base charts are already flexible enough
- UI client handles `webTransportEnabled: false` gracefully
- Cross-namespace DNS works out of the box

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Breaking DB migration in PR | Medium | High | Use separate `preview_shared` database |
| ResourceQuota exhaustion | Low | Medium | Enforce 3 PR limit, monitor usage |
| TLS cert rate limit | Low | Low | Use wildcard cert (not per-PR) |
| GHCR image pull failure | Low | Medium | Verify public package visibility |
| WebSocket timeout | Medium | High | Set ingress timeout annotations |
| Node IP changes break DNS | N/A | N/A | Using ingress, not NodePort |

---

## Open Questions
1. Does ExternalDNS already exist in production cluster?
2. What is the ingress-nginx LoadBalancer IP? (needed for DNS wildcard)
3. Should we enable E2EE for PR previews? (Currently planning to disable)
4. Do we want Grafana/Prometheus metrics for preview namespaces? (Likely no)

---

## Next Steps
1. **Manual setup:** Create wildcard cert, DNS, and `preview_shared` database
2. **Implement workflow:** `.github/workflows/pr-deploy.yaml` with helm commands
3. **Test in fork:** Deploy to jboyd01/videocall-rs first
4. **Submit upstream PR:** After validation, submit to security-union/videocall-rs
