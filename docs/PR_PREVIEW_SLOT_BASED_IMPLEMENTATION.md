# PR Preview Slot-Based Deployment - Implementation Summary

**Date:** 2026-02-19
**Status:** ✅ Implemented and ready for testing

---

## Overview

Implemented a **configurable slot-based deployment system** for PR previews to support Google OAuth, which requires pre-registered callback URLs. Each PR is assigned to one of N fixed slots with stable URLs.

**Max slots:** Configurable via `PR_PREVIEW_MAX_SLOTS` repository variable (default: 3)

---

## Key Changes

### 0. Configurable Max Slots

**Repository Variable:** `PR_PREVIEW_MAX_SLOTS`

```bash
# Set via GitHub CLI
gh variable set PR_PREVIEW_MAX_SLOTS --body "3"

# Or via GitHub UI
# Settings → Secrets and variables → Actions → Variables
```

**Benefits:**
- Change max slots without editing workflows
- Default: 3 (if not set)
- Supports 1-10 slots (higher values require more resources and OAuth callbacks)

**See:** `docs/PR_PREVIEW_CONFIGURATION.md` for detailed configuration guide

### 1. Fixed Slot URLs (OAuth-compatible)

**Slot 1:**
- UI: `https://pr1.sandbox.videocall.rs`
- API: `https://pr1-api.sandbox.videocall.rs`
- WebSocket: `wss://pr1-ws.sandbox.videocall.rs`

**Slot 2:**
- UI: `https://pr2.sandbox.videocall.rs`
- API: `https://pr2-api.sandbox.videocall.rs`
- WebSocket: `wss://pr2-ws.sandbox.videocall.rs`

**Slot 3:**
- UI: `https://pr3.sandbox.videocall.rs`
- API: `https://pr3-api.sandbox.videocall.rs`
- WebSocket: `wss://pr3-ws.sandbox.videocall.rs`

### 2. Namespace Structure

Changed from `preview-{PR_NUM}` to `preview-slot-{SLOT}`:

```bash
# Old naming
preview-123  (labels: app=preview, pr=123)

# New naming
preview-slot-1  (labels: app=preview, slot=1, pr=123)
preview-slot-2  (labels: app=preview, slot=2, pr=456)
preview-slot-3  (labels: app=preview, slot=3, pr=789)
```

**Benefits:**
- State persists in K8s labels (no external storage needed)
- Automatic slot reuse when namespace is deleted
- Idempotent deployment (redeploying same PR reuses its slot)

### 3. Database Naming

Changed from `preview_{PR_NUM}` to `preview_slot_{SLOT}`:

```sql
-- Old naming
CREATE DATABASE preview_123;

-- New naming
CREATE DATABASE preview_slot_1;
CREATE DATABASE preview_slot_2;
CREATE DATABASE preview_slot_3;
```

**Database lifecycle:**
- Created when slot is first used
- Deleted when slot is freed via `/undeploy` or PR closure
- Fresh database for each PR deployment

### 4. OAuth Configuration

**Enabled by default** with Google OAuth:

```yaml
env:
  - name: OAUTH_ISSUER
    value: "https://accounts.google.com"
  - name: OAUTH_CLIENT_ID
    valueFrom:
      secretKeyRef:
        name: google-oauth-credentials
        key: client-id
  - name: OAUTH_CLIENT_SECRET
    valueFrom:
      secretKeyRef:
        name: google-oauth-credentials
        key: client-secret
  - name: OAUTH_REDIRECT_URL
    value: "https://pr${SLOT}-api.sandbox.videocall.rs/login/callback"
  - name: AFTER_LOGIN_URL
    value: "https://pr${SLOT}.sandbox.videocall.rs"
```

**Pre-registered OAuth callbacks** (already done by Dario):
- `https://pr1-api.sandbox.videocall.rs/login/callback`
- `https://pr2-api.sandbox.videocall.rs/login/callback`
- `https://pr3-api.sandbox.videocall.rs/login/callback`

---

## Workflow Changes

### pr-deploy-reusable.yaml

**New slot assignment logic:**

```bash
# Check if PR already has a slot
EXISTING_SLOT=$(kubectl get namespaces -l app=preview,pr=${PR_NUM} \
  -o jsonpath='{.items[0].metadata.labels.slot}')

if [ -n "$EXISTING_SLOT" ]; then
  # Reuse existing slot (redeployment)
  SLOT=$EXISTING_SLOT
else
  # Find first available slot
  for SLOT in 1 2 3; do
    if ! kubectl get namespace preview-slot-${SLOT}; then
      AVAILABLE_SLOT=$SLOT
      break
    fi
  done
fi
```

**Capacity error message (when all slots full):**

```
❌ All preview slots are in use (3/3)

Active deployments:
- Slot 1: PR #123 → https://pr1.sandbox.videocall.rs (undeploy)
- Slot 2: PR #456 → https://pr2.sandbox.videocall.rs (undeploy)
- Slot 3: PR #789 → https://pr3.sandbox.videocall.rs (undeploy)

Action required: Comment /undeploy on one of the PRs above to free a slot.
```

**Success message:**

```
✅ Preview deployed to slot 2

🌐 Preview URLs:
- UI: https://pr2.sandbox.videocall.rs
- API: https://pr2-api.sandbox.videocall.rs
- WebSocket: wss://pr2-ws.sandbox.videocall.rs

🔐 Configuration:
- Authentication: Google OAuth enabled (login required)
- WebTransport: Disabled (WebSocket fallback only)

📊 Slot occupancy: 2/3 slots in use

Cleanup: Comment /undeploy to free this slot.
```

### pr-undeploy.yaml

**Find slot by PR number:**

```bash
SLOT=$(kubectl get namespaces -l app=preview,pr=${PR_NUM} \
  -o jsonpath='{.items[0].metadata.labels.slot}')
```

**Delete slot resources:**

```bash
NAMESPACE="preview-slot-${SLOT}"
DB_NAME="preview_slot_${SLOT}"

kubectl delete namespace ${NAMESPACE}
kubectl exec postgres-postgresql-0 -- psql -c "DROP DATABASE ${DB_NAME};"
```

**Success message:**

```
✅ Preview undeployed - slot 2 freed

Resources removed:
- Namespace preview-slot-2 (includes all pods and services)
- Database preview_slot_2
- GHCR images tagged pr-456

📊 Slot 2 is now available for other PRs.
```

### pr-cleanup.yaml

Same slot lookup and deletion logic as `pr-undeploy.yaml`, triggered automatically when PR is closed or merged.

---

## Resource Naming

**Kubernetes resources** (still use PR number):
- Helm release: `preview-{PR_NUM}-ws`, `preview-{PR_NUM}-api`, `preview-{PR_NUM}-ui`
- Pods: `websocket-pr-{PR_NUM}-*`, `api-pr-{PR_NUM}-*`, `ui-pr-{PR_NUM}-*`
- Services: `websocket-pr-{PR_NUM}`, `api-pr-{PR_NUM}`, `ui-pr-{PR_NUM}`
- NATS release: `nats-pr-{PR_NUM}`

**Infrastructure** (uses slot number):
- Namespace: `preview-slot-{SLOT}`
- Database: `preview_slot_{SLOT}`
- Ingress hosts: `pr{SLOT}.sandbox.videocall.rs`, etc.

**Why this hybrid approach?**
- Slot-based namespaces ensure stable URLs for OAuth
- PR-based resource names prevent conflicts when redeploying
- Labels on namespace track which PR owns which slot

---

## Prerequisites

### 0. Configuration Variable

Set the maximum number of preview slots:

```bash
gh variable set PR_PREVIEW_MAX_SLOTS --body "3"
```

**Or via GitHub UI:**
1. Go to repository Settings
2. Navigate to: Secrets and variables → Actions → Variables
3. Click "New repository variable"
4. Name: `PR_PREVIEW_MAX_SLOTS`
5. Value: `3` (or your desired number)

**Important:** Must register OAuth callbacks for ALL slots (1 to MAX_SLOTS)

### 1. DNS Configuration

Wildcard DNS record already configured: `*.sandbox.videocall.rs`

This covers:
- `pr1.sandbox.videocall.rs`, `pr2.sandbox.videocall.rs`, `pr3.sandbox.videocall.rs`
- `pr1-api.sandbox.videocall.rs`, `pr2-api.sandbox.videocall.rs`, etc.
- `pr1-ws.sandbox.videocall.rs`, `pr2-ws.sandbox.videocall.rs`, etc.

### 2. TLS Certificate

Wildcard certificate `sandbox-wildcard-tls` should exist in `default` namespace:

```bash
kubectl get secret sandbox-wildcard-tls -n default
```

Covers all slots: `*.sandbox.videocall.rs`

### 3. OAuth Secret

Create the Google OAuth credentials secret:

```bash
kubectl create secret generic google-oauth-credentials -n default \
  --from-literal=client-id="YOUR_GOOGLE_CLIENT_ID" \
  --from-literal=client-secret="YOUR_GOOGLE_CLIENT_SECRET"
```

**OAuth callbacks** must be registered in Google Cloud Console for ALL slots:

**If `PR_PREVIEW_MAX_SLOTS=3` (default):**
- `https://pr1-api.sandbox.videocall.rs/login/callback`
- `https://pr2-api.sandbox.videocall.rs/login/callback`
- `https://pr3-api.sandbox.videocall.rs/login/callback`

**If `PR_PREVIEW_MAX_SLOTS=5`:**
- Same as above, PLUS:
- `https://pr4-api.sandbox.videocall.rs/login/callback`
- `https://pr5-api.sandbox.videocall.rs/login/callback`

**⚠️ Important:** You must register callbacks for ALL slots (1 to MAX_SLOTS) in Google Cloud Console BEFORE increasing `PR_PREVIEW_MAX_SLOTS`.

### 4. Postgres Secret

Ensure `postgres-credentials` secret exists:

```bash
kubectl get secret postgres-credentials -n default
```

---

## Usage Examples

### Deploy to First Available Slot

User comments `/deploy` on PR #123:

1. Workflow finds slot 1 is available
2. Creates namespace `preview-slot-1` with labels `slot=1, pr=123`
3. Creates database `preview_slot_1`
4. Deploys services with names like `websocket-pr-123`
5. Configures ingress for `pr1.sandbox.videocall.rs`
6. Posts comment: "✅ Preview deployed to slot 1"

### Redeploy to Same Slot

User pushes new commits and comments `/build-and-deploy` on PR #123:

1. Workflow finds PR #123 already owns slot 1
2. Reuses namespace `preview-slot-1`
3. Reuses database `preview_slot_1`
4. Updates Helm releases with new images
5. Posts comment: "✅ Preview redeployed to slot 1 (slot reused)"

### Undeploy and Free Slot

User comments `/undeploy` on PR #123:

1. Workflow finds PR #123 is in slot 1
2. Deletes namespace `preview-slot-1` (cascades to all resources)
3. Drops database `preview_slot_1`
4. Deletes GHCR images tagged `pr-123`
5. Posts comment: "✅ Preview undeployed - slot 1 freed"

### Capacity Exceeded

User comments `/deploy` on PR #999 when all 3 slots are full:

1. Workflow checks slots 1, 2, 3 - all occupied
2. Posts error comment listing active PRs in each slot
3. User must comment `/undeploy` on one of the active PRs
4. Then retry `/deploy` on PR #999

---

## Testing Checklist

### Manual Testing Steps

1. **First deployment:**
   ```
   Comment /build-and-deploy on PR #1
   → Should assign slot 1
   → Verify https://pr1.sandbox.videocall.rs works
   → Verify OAuth login required
   ```

2. **Second deployment:**
   ```
   Comment /build-and-deploy on PR #2
   → Should assign slot 2
   → Verify https://pr2.sandbox.videocall.rs works
   ```

3. **Third deployment:**
   ```
   Comment /build-and-deploy on PR #3
   → Should assign slot 3
   → Verify https://pr3.sandbox.videocall.rs works
   ```

4. **Capacity test:**
   ```
   Comment /deploy on PR #4
   → Should fail with capacity error
   → Should list active slots 1, 2, 3
   ```

5. **Undeploy and reuse:**
   ```
   Comment /undeploy on PR #1
   → Should free slot 1
   Comment /deploy on PR #4
   → Should assign slot 1 (reused)
   → Verify https://pr1.sandbox.videocall.rs now shows PR #4
   ```

6. **Redeployment:**
   ```
   Push new commits to PR #4
   Comment /build-and-deploy on PR #4
   → Should reuse slot 1
   → Verify updated code deployed
   ```

7. **Auto cleanup:**
   ```
   Close PR #4
   → pr-cleanup.yaml should run automatically
   → Should free slot 1
   → Verify namespace deleted
   → Verify database dropped
   ```

### Verification Commands

```bash
# List all active slots
kubectl get namespaces -l app=preview \
  -o custom-columns=SLOT:.metadata.labels.slot,PR:.metadata.labels.pr,AGE:.metadata.creationTimestamp

# Check slot 1 resources
kubectl get all -n preview-slot-1

# Check databases
kubectl exec -n default postgres-postgresql-0 -- \
  psql -U postgres -c "\l preview_slot*"

# Check DNS resolution
dig pr1.sandbox.videocall.rs
dig pr2-api.sandbox.videocall.rs
dig pr3-ws.sandbox.videocall.rs

# Check TLS certificate
kubectl get secret sandbox-wildcard-tls -n preview-slot-1

# Check OAuth secret
kubectl get secret google-oauth-credentials -n default
```

---

## Migration from PR-Based to Slot-Based

No migration needed! Current implementation in `jboyd01/videocall-rs` hasn't been merged to `security-union/videocall-rs` yet, so there are no existing deployments to migrate.

When this is merged to upstream:
1. Any existing `preview-{NUM}` namespaces will be ignored
2. New deployments will use `preview-slot-{SLOT}` pattern
3. Old namespaces can be manually cleaned up if needed

---

## Troubleshooting

### Slot Assignment Fails

**Symptom:** Workflow fails at "Assign deployment slot" step

**Causes:**
- All 3 slots occupied
- Race condition (two deployments starting simultaneously)

**Solution:**
- Wait for undeploy workflow to complete
- Undeploy one of the active PRs
- Retry deployment

### OAuth Login Fails

**Symptom:** User sees OAuth error when accessing `pr{SLOT}.sandbox.videocall.rs`

**Causes:**
- OAuth secret not configured
- Callback URL not registered in Google Console
- Wrong `AFTER_LOGIN_URL` configuration

**Debug:**
```bash
# Check OAuth secret exists
kubectl get secret google-oauth-credentials -n default

# Check Meeting API logs
kubectl logs -n preview-slot-1 -l app.kubernetes.io/name=meeting-api

# Verify redirect URL in Meeting API env
kubectl get deployment api-pr-{PR_NUM} -n preview-slot-1 -o yaml | grep OAUTH
```

### Database Already Exists Error

**Symptom:** Deployment fails at "Create postgres database" step with error "database already exists"

**Cause:** Previous deployment in this slot didn't clean up database

**Solution:**
```bash
# Manually drop the database
SLOT=1  # or 2, 3
kubectl exec -n default postgres-postgresql-0 -- \
  env PGPASSWORD="..." psql -U postgres -c "DROP DATABASE preview_slot_${SLOT};"

# Retry deployment
```

### Slot Shows Wrong PR

**Symptom:** `https://pr1.sandbox.videocall.rs` shows content from PR #123 but namespace label says `pr=456`

**Cause:** Namespace label not updated during redeployment

**Solution:**
```bash
# Force update namespace label
kubectl label namespace preview-slot-1 pr=456 --overwrite

# Or delete and redeploy
kubectl delete namespace preview-slot-1
# Then comment /deploy again
```

---

## Performance Considerations

### Slot Reuse

When a slot is freed and immediately reused:
- Namespace deletion takes ~10-30 seconds
- Database drop is instant
- New deployment can start immediately
- Total time between undeploy and redeploy: ~1-2 minutes

### Parallel Deployments

Multiple `/deploy` commands on different PRs will:
- Assign different slots (1, 2, 3)
- Run in parallel without conflicts
- Each complete in ~3-5 minutes

### Race Conditions

If two deployments race for the last available slot:
- Both will check for available slots
- First to create namespace wins
- Second will see capacity exceeded
- Second can retry after first completes

---

## Cost Analysis

**No additional cost** compared to PR-based deployment:
- Same number of pods, services, ingresses
- Reuses shared postgres instance
- Reuses shared ingress-nginx LoadBalancer
- Only difference: fixed slot URLs instead of dynamic PR URLs

**Cost per slot:**
- NATS: ~50m CPU, 64Mi memory
- WebSocket: ~50m CPU, 128Mi memory
- Meeting API: ~50m CPU, 128Mi memory
- UI: ~10m CPU, 32Mi memory
- **Total: ~160m CPU, ~352Mi memory per slot**

**3 slots = ~480m CPU, ~1Gi memory** (well within cluster capacity)

---

## Security

### OAuth Security

- **Production Google OAuth app** used (not sandbox/test)
- **Client secret** stored in K8s secret (not in workflow)
- **HTTPS-only** communication (enforced by ingress)
- **CORS** configured to allow only `pr{SLOT}.sandbox.videocall.rs`
- **Cookie domain** set to `.sandbox.videocall.rs` (allows API → UI auth)

### Slot Isolation

- Each slot has separate namespace (network isolation)
- Each slot has separate database (data isolation)
- Each slot has separate NATS instance (message isolation)
- ResourceQuota prevents one slot from consuming all resources

### Permission Gating

- Only OWNER/MEMBER/COLLABORATOR can deploy/undeploy
- GHCR image pull requires GitHub authentication
- K8s cluster access via DIGITALOCEAN_ACCESS_TOKEN secret

---

## Future Enhancements

### Optional Improvements

1. **Slot reservation:** Allow users to request specific slot number
2. **Slot metrics:** Track usage patterns, average lifetime, etc.
3. **Auto-undeploy:** Automatically undeploy slots after N days of inactivity
4. **Slot status dashboard:** Web UI showing which PRs occupy which slots
5. **Multi-region slots:** Deploy to `us-east-slot-1`, `singapore-slot-1`, etc.
6. **E2EE support:** Enable end-to-end encryption in previews
7. **WebTransport support:** Add per-slot LoadBalancers for WebTransport

### Not Recommended

- Increasing slot count beyond 3 (cost/complexity increases)
- Sharing database across slots (data isolation compromised)
- Using production OAuth in slots (security risk)

---

## Related Documentation

- `PR_PREVIEW_SLOT_BASED_DEPLOYMENT.md` - Original design spec
- `PR_PREVIEW_DEPLOYMENT_PLAN.md` - Original PR-based design
- `PR_PREVIEW_IMPLEMENTATION_SUMMARY.md` - PR-based implementation summary
- `OAUTH_HELM_CONFIGURATION.md` - OAuth setup guide

---

## Summary

✅ **Slot-based deployment implemented and ready for testing**

**Key benefits:**
- ✅ OAuth-compatible with pre-registered callback URLs
- ✅ Stable URLs for each slot (no DNS churn)
- ✅ Automatic slot reuse (no manual cleanup needed)
- ✅ Clear capacity management (3 slots, visible occupancy)
- ✅ Idempotent deployments (redeploying reuses slot)

**Ready for testing in:** `jboyd01/videocall-rs`

**Next steps:**
1. Test first deployment (`/build-and-deploy` on test PR)
2. Verify OAuth login works
3. Test slot reuse and cleanup
4. Document any issues found
5. Merge to `security-union/videocall-rs` when validated
