# PR Preview Slot-Based Deployment Plan

**Date:** 2026-02-19
**Context:** OAuth callback URL requirements for PR preview environments

---

## Problem

Google OAuth does NOT support wildcard callback URLs. Each callback URL must be explicitly registered:
- ❌ Cannot use: `https://pr-*.sandbox.videocall.rs/login/callback`
- ❌ Cannot use: `https://pr-123.sandbox.videocall.rs/login/callback` (dynamic PR numbers)
- ✅ Must use: Pre-registered exact URLs

Current `pr-{NUM}.sandbox.videocall.rs` approach breaks OAuth because each PR creates a new callback URL.


---

## Solution: Slot-Based Deployment

Use **3 fixed preview URLs** mapped to deployment "slots":
- `pr1.sandbox.videocall.rs` → Slot 1
- `pr2.sandbox.videocall.rs` → Slot 2
- `pr3.sandbox.videocall.rs` → Slot 3

Register OAuth callbacks **once** with Google:
```
https://pr1-api.sandbox.videocall.rs/login/callback
https://pr2-api.sandbox.videocall.rs/login/callback
https://pr3-api.sandbox.videocall.rs/login/callback
```
Done!  Dario has added the OAuth callback URLs to the Google OAuth console.


When a maintainer runs `/deploy`:
1. Find available slot (1, 2, or 3)
2. Assign PR to that slot
3. Deploy to `pr{slot}.sandbox.videocall.rs`
4. Remember PR → slot mapping

When `/undeploy` runs:
1. Find which slot the PR is using
2. Delete deployment
3. Free the slot for reuse

---

## State Management: Kubernetes Namespace Labels

Store PR → slot mapping directly in Kubernetes using namespace labels.

**Namespace naming:**
```
preview-slot-1  (labels: app=preview, slot=1, pr=123)
preview-slot-2  (labels: app=preview, slot=2, pr=456)
preview-slot-3  (labels: app=preview, slot=3, pr=789)
```

**Why this works:**
- ✅ State persists in cluster (survives workflow runs)
- ✅ Atomic operations (namespace exists or doesn't)
- ✅ Easy queries: `kubectl get ns -l slot=1 -o jsonpath='{.items[0].metadata.labels.pr}'`
- ✅ Automatic cleanup (delete namespace = free slot)
- ✅ No external state storage needed
- ✅ Idempotent (redeploying same PR reuses its slot)

---

## Implementation

### Deploy Workflow Changes

**1. Find available slot:**
```bash
# Check if PR is already deployed
EXISTING_SLOT=$(kubectl get namespaces -l app=preview,pr=${PR_NUM} \
  -o jsonpath='{.items[0].metadata.labels.slot}' 2>/dev/null)

if [ -n "$EXISTING_SLOT" ]; then
  echo "PR #${PR_NUM} is already deployed in slot ${EXISTING_SLOT}"
  SLOT=$EXISTING_SLOT
else
  # Find first available slot
  for SLOT in 1 2 3; do
    if ! kubectl get namespace preview-slot-${SLOT} >/dev/null 2>&1; then
      AVAILABLE_SLOT=$SLOT
      break
    fi
  done

  if [ -z "$AVAILABLE_SLOT" ]; then
    echo "❌ All slots full"
    exit 1
  fi

  SLOT=$AVAILABLE_SLOT
fi
```

**2. Create namespace with labels:**
```bash
NAMESPACE="preview-slot-${SLOT}"
kubectl create namespace ${NAMESPACE}
kubectl label namespace ${NAMESPACE} app=preview slot=${SLOT} pr=${PR_NUM}
```

**3. Deploy with slot-based URLs:**
```bash
UI_HOST="pr${SLOT}.sandbox.videocall.rs"
API_HOST="pr${SLOT}-api.sandbox.videocall.rs"
WS_HOST="pr${SLOT}-ws.sandbox.videocall.rs"

# Use these in helm --set ingress.hosts[0].host=${UI_HOST}
```

### Undeploy Workflow Changes

**Find and delete slot:**
```bash
# Find which slot this PR is using
SLOT=$(kubectl get namespaces -l app=preview,pr=${PR_NUM} \
  -o jsonpath='{.items[0].metadata.labels.slot}')

if [ -z "$SLOT" ]; then
  echo "PR #${PR_NUM} is not deployed"
  exit 0
fi

# Delete namespace (automatically frees slot)
kubectl delete namespace preview-slot-${SLOT}
```

### Capacity Check Changes

**List active slots:**
```bash
# Get all active slots with their PR numbers
kubectl get namespaces -l app=preview \
  -o custom-columns=SLOT:.metadata.labels.slot,PR:.metadata.labels.pr,AGE:.metadata.creationTimestamp
```

**Error message when full:**
```
❌ All preview slots are in use (3/3)

Active deployments:
- Slot 1: PR #123 → https://pr1.sandbox.videocall.rs (undeploy)
- Slot 2: PR #456 → https://pr2.sandbox.videocall.rs (undeploy)
- Slot 3: PR #789 → https://pr3.sandbox.videocall.rs (undeploy)

Action required: Comment /undeploy on one of the PRs above.
```

---

## User Experience

### Successful Deployment

```
✅ Preview deployed to slot 2

🌐 Preview URLs:
- UI: https://pr2.sandbox.videocall.rs
- API: https://pr2-api.sandbox.videocall.rs
- WebSocket: wss://pr2-ws.sandbox.videocall.rs

📊 Slot 2/3 occupied by PR #456
```

### Redeployment (Updates)

```
✅ Redeploying to slot 2 (was already assigned)

🌐 Preview URLs:
- UI: https://pr2.sandbox.videocall.rs
- API: https://pr2-api.sandbox.videocall.rs
- WebSocket: wss://pr2-ws.sandbox.videocall.rs

📊 Updated PR #456 in slot 2/3
```

---

## DNS Configuration

**One-time setup:**
Create DNS records for all 3 slots:
```
pr1.sandbox.videocall.rs      → ingress-nginx LB IP
pr1-api.sandbox.videocall.rs  → ingress-nginx LB IP
pr1-ws.sandbox.videocall.rs   → ingress-nginx LB IP

pr2.sandbox.videocall.rs      → ingress-nginx LB IP
pr2-api.sandbox.videocall.rs  → ingress-nginx LB IP
pr2-ws.sandbox.videocall.rs   → ingress-nginx LB IP

pr3.sandbox.videocall.rs      → ingress-nginx LB IP
pr3-api.sandbox.videocall.rs  → ingress-nginx LB IP
pr3-ws.sandbox.videocall.rs   → ingress-nginx LB IP
```

**Or use wildcard** (covers all slots):
```
*.sandbox.videocall.rs → ingress-nginx LB IP
```

---

## OAuth Configuration

**Google Cloud Console → APIs & Services → Credentials:**

Add authorized redirect URIs:
```
https://pr1-api.sandbox.videocall.rs/login/callback
https://pr2-api.sandbox.videocall.rs/login/callback
https://pr3-api.sandbox.videocall.rs/login/callback
```

**Meeting API environment variables:**
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
```

**UI runtime config:**
```yaml
runtimeConfig:
  oauthEnabled: "true"  # Enable OAuth for testing
  apiBaseUrl: "https://pr${SLOT}-api.sandbox.videocall.rs"
  meetingApiBaseUrl: "https://pr${SLOT}-api.sandbox.videocall.rs"
```

---

## Benefits

1. **Works with Google OAuth** - Pre-registered callback URLs
2. **Simple state management** - Kubernetes labels, no external storage
3. **Idempotent** - Redeploying same PR reuses its slot
4. **Automatic cleanup** - Delete namespace = free slot
5. **Clear to users** - Slot number in URLs and comments
6. **Industry standard** - Same pattern as Heroku Review Apps

---

## Alternatives Considered

### Option 1: Dynamic OAuth URI Registration via API
**Verdict:** ❌ Not possible - Google doesn't provide API for OAuth client configuration

### Option 2: Use Auth0 or Okta
**Verdict:** ✅ Possible - Auth0 supports regex patterns, Okta supports wildcards
**Downside:** Requires changing OAuth provider

### Option 3: Single shared preview URL
**Verdict:** ❌ Not practical - Only one PR can test OAuth at a time

---

## Implementation Checklist

- [ ] Update pr-deploy.yaml slot assignment logic
- [ ] Update pr-undeploy.yaml slot lookup logic
- [ ] Update capacity check to show slots
- [ ] Create DNS records for pr1, pr2, pr3 (all subdomains)
- [ ] Register OAuth callback URLs with Google
- [ ] Test deployment to slot 1
- [ ] Test redeployment to same slot
- [ ] Test undeploy and slot freeing
- [ ] Test capacity exceeded error
- [ ] Update documentation with slot-based URLs

---

## Next Steps

1. Implement slot assignment in `.github/workflows/pr-deploy.yaml`
2. Update DNS records in DigitalOcean
3. Register OAuth callbacks in Google Cloud Console
4. Test end-to-end deployment with OAuth enabled
