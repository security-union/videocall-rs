# Slot-Based Deployment - Changes Summary

**Date:** 2026-02-19
**Status:** ✅ Ready for testing

---

## What Changed

Converted PR preview deployment from dynamic `pr-{NUM}` URLs to fixed slot-based URLs (`pr1`, `pr2`, `pr3`) to support Google OAuth callback URL requirements.

---

## Modified Files

### Workflows

1. **`.github/workflows/pr-deploy-reusable.yaml`** (Core deployment logic)
   - Added slot assignment algorithm (finds first available slot 1-3)
   - Changed namespace: `preview-{PR}` → `preview-slot-{SLOT}`
   - Changed database: `preview_{PR}` → `preview_slot_{SLOT}`
   - Changed ingress hosts: `pr-{PR}.sandbox...` → `pr{SLOT}.sandbox...`
   - Enabled OAuth by default with slot-based callback URLs
   - Updated success messages to show slot + PR number
   - Added slot reuse logic for redeployments

2. **`.github/workflows/pr-deploy.yaml`** (Deploy command trigger)
   - Updated deployment started message to mention slot assignment

3. **`.github/workflows/pr-undeploy.yaml`** (Undeploy command trigger)
   - Changed to find slot by PR number label
   - Updated to delete `preview-slot-{SLOT}` namespace
   - Updated to drop `preview_slot_{SLOT}` database
   - Updated success message to show freed slot

4. **`.github/workflows/pr-cleanup.yaml`** (Auto-cleanup on PR close)
   - Same slot lookup and deletion logic as undeploy
   - Updated messages to show slot information

### Documentation

5. **`docs/PR_PREVIEW_SLOT_BASED_IMPLEMENTATION.md`** (NEW)
   - Comprehensive technical implementation guide
   - Troubleshooting section
   - Testing checklist
   - Architecture details

6. **`docs/PR_PREVIEW_QUICK_START.md`** (NEW)
   - User-friendly quick reference guide
   - Common commands and workflows
   - FAQ section

7. **`SLOT_BASED_DEPLOYMENT_CHANGES.md`** (THIS FILE)
   - Summary of all changes
   - Testing instructions

---

## Key Behavioral Changes

### Before (PR-Based)
```
PR #123 → preview-123 namespace
       → preview_123 database
       → pr-123.sandbox.videocall.rs
       → pr-123-api.sandbox.videocall.rs
       → pr-123-ws.sandbox.videocall.rs
```

### After (Slot-Based)
```
PR #123 → preview-slot-1 namespace (labels: slot=1, pr=123)
       → preview_slot_1 database
       → pr1.sandbox.videocall.rs
       → pr1-api.sandbox.videocall.rs
       → pr1-ws.sandbox.videocall.rs
```

### Slot Reuse
```
PR #456 deploys → slot 1 (if available)
PR #456 undeploys → slot 1 freed
PR #789 deploys → slot 1 (reused)
```

---

## New Features

### 1. Fixed Slot URLs
- 3 stable URLs that don't change between PRs
- OAuth callback URLs pre-registered in Google Console
- URLs: `pr1`, `pr2`, `pr3` (no hyphens, just slot number)

### 2. Automatic Slot Assignment
- Workflow finds first available slot automatically
- Reuses same slot if PR is redeployed
- Shows clear error when all slots occupied

### 3. OAuth Enabled by Default
- Google OAuth configured automatically
- Uses `google-oauth-credentials` K8s secret
- Callback URLs: `https://pr{SLOT}-api.sandbox.videocall.rs/login/callback`
- After login redirects to: `https://pr{SLOT}.sandbox.videocall.rs`

### 4. Slot Occupancy Tracking
- Namespace labels track which PR owns which slot
- Capacity error shows active PRs and their slots
- Success message shows slot occupancy (e.g., "2/3 slots in use")

### 5. Improved User Messages
- Deployment: "✅ Preview deployed to slot 2"
- Redeployment: "✅ Preview redeployed to slot 2 (slot reused)"
- Undeploy: "✅ Preview undeployed - slot 2 freed"
- Capacity: "❌ All preview slots are in use (3/3)"

---

## Configuration Variable

### `PR_PREVIEW_MAX_SLOTS` (Repository Variable)

Set the maximum number of preview slots:

```bash
# Via GitHub CLI
gh variable set PR_PREVIEW_MAX_SLOTS --body "3"

# Or via GitHub UI:
# Settings → Secrets and variables → Actions → Variables → New repository variable
# Name: PR_PREVIEW_MAX_SLOTS
# Value: 3
```

**Default:** 3 (if not set)

**Recommended values:**
- Small team (1-5 devs): 2-3 slots
- Medium team (5-10 devs): 4-5 slots
- Large team (10+ devs): 6-8 slots

See `docs/PR_PREVIEW_CONFIGURATION.md` for detailed configuration guide.

---

## Prerequisites Checklist

Before testing, verify these are configured:

### DNS ✅ (Already Done)
```bash
# Wildcard DNS should resolve
dig pr1.sandbox.videocall.rs
dig pr2-api.sandbox.videocall.rs
dig pr3-ws.sandbox.videocall.rs

# All should return ingress LoadBalancer IP
```

### TLS Certificate ✅ (Should Exist)
```bash
# Wildcard cert should exist
kubectl get secret sandbox-wildcard-tls -n default
```

### OAuth Secret ❓ (Needs Creation)
```bash
# Create Google OAuth credentials secret
kubectl create secret generic google-oauth-credentials -n default \
  --from-literal=client-id="YOUR_GOOGLE_CLIENT_ID" \
  --from-literal=client-secret="YOUR_GOOGLE_CLIENT_SECRET"

# Verify
kubectl get secret google-oauth-credentials -n default
```

**OAuth callbacks** already registered (Dario confirmed):
- `https://pr1-api.sandbox.videocall.rs/login/callback`
- `https://pr2-api.sandbox.videocall.rs/login/callback`
- `https://pr3-api.sandbox.videocall.rs/login/callback`

### Postgres Secret ✅ (Should Exist)
```bash
# Postgres credentials should exist
kubectl get secret postgres-credentials -n default
```

---

## Testing Plan

### Phase 0: Configuration Setup
```
1. Set max slots variable:
   gh variable set PR_PREVIEW_MAX_SLOTS --body "3"

2. Verify variable set:
   gh variable get PR_PREVIEW_MAX_SLOTS
   # Should output: 3
```

### Phase 1: Single Deployment
```
1. Create test PR or use existing PR #1
2. Comment: /build-and-deploy
3. Wait ~15-20 minutes
4. Verify bot comments: "✅ Preview deployed to slot 1"
5. Visit: https://pr1.sandbox.videocall.rs
6. Test OAuth login works
7. Verify app functionality
```

**Expected:**
- Namespace `preview-slot-1` created
- Database `preview_slot_1` created
- Resources named `websocket-pr-1`, `api-pr-1`, `ui-pr-1`
- URLs work: `pr1.sandbox.videocall.rs`
- OAuth login required and working

### Phase 2: Multiple Deployments
```
1. On PR #2, comment: /build-and-deploy
2. Verify assigned to slot 2
3. On PR #3, comment: /build-and-deploy
4. Verify assigned to slot 3
5. Visit all three:
   - https://pr1.sandbox.videocall.rs (PR #1)
   - https://pr2.sandbox.videocall.rs (PR #2)
   - https://pr3.sandbox.videocall.rs (PR #3)
6. Verify each shows correct PR content
```

**Expected:**
- 3 namespaces: `preview-slot-1`, `preview-slot-2`, `preview-slot-3`
- 3 databases: `preview_slot_1`, `preview_slot_2`, `preview_slot_3`
- Each URL shows different PR content
- OAuth works on all 3 slots

### Phase 3: Capacity Test
```
1. With all 3 slots full, on PR #4 comment: /deploy
2. Verify error message lists active PRs
3. On PR #1, comment: /undeploy
4. Verify slot 1 freed
5. On PR #4, comment: /deploy
6. Verify assigned to slot 1 (reused)
7. Visit: https://pr1.sandbox.videocall.rs
8. Verify shows PR #4 content (not PR #1)
```

**Expected:**
- Capacity error shows all 3 occupied slots
- Undeploy frees slot immediately
- Slot 1 reused for PR #4
- URL `pr1.sandbox...` now serves PR #4 content

### Phase 4: Redeployment
```
1. Push new commits to PR #4
2. Comment: /build-and-deploy
3. Verify message: "redeployed to slot 1 (slot reused)"
4. Visit: https://pr1.sandbox.videocall.rs
5. Verify updated code deployed
```

**Expected:**
- Same slot reused (slot 1)
- No new namespace created
- Database persists
- Updated code visible

### Phase 5: Auto-Cleanup
```
1. Close PR #4 (without merging)
2. Wait ~1 minute for pr-cleanup.yaml to run
3. Verify namespace deleted:
   kubectl get namespace preview-slot-1
4. Verify slot 1 available again
```

**Expected:**
- Namespace `preview-slot-1` deleted automatically
- Database `preview_slot_1` dropped
- GHCR images deleted
- Slot 1 available for reuse

---

## Verification Commands

### Check Active Slots
```bash
kubectl get namespaces -l app=preview \
  -o custom-columns=SLOT:.metadata.labels.slot,PR:.metadata.labels.pr,AGE:.metadata.creationTimestamp
```

Expected output:
```
SLOT   PR    AGE
1      123   5m
2      456   3m
3      789   1m
```

### Check Slot Resources
```bash
# Pods in slot 1
kubectl get pods -n preview-slot-1

# Services in slot 1
kubectl get services -n preview-slot-1

# Ingresses in slot 1
kubectl get ingress -n preview-slot-1
```

### Check Databases
```bash
kubectl exec -n default postgres-postgresql-0 -- \
  psql -U postgres -c "\l" | grep preview_slot
```

Expected output:
```
preview_slot_1
preview_slot_2
preview_slot_3
```

### Check OAuth Configuration
```bash
# Get OAuth secret
kubectl get secret google-oauth-credentials -n default -o yaml

# Check Meeting API OAuth env vars
kubectl get deployment api-pr-123 -n preview-slot-1 -o yaml | grep -A 10 "OAUTH"
```

Expected output should include:
```yaml
- name: OAUTH_ISSUER
  value: https://accounts.google.com
- name: OAUTH_CLIENT_ID
  value: ...
- name: OAUTH_CLIENT_SECRET
  value: ...
- name: OAUTH_REDIRECT_URL
  value: https://pr1-api.sandbox.videocall.rs/login/callback
- name: AFTER_LOGIN_URL
  value: https://pr1.sandbox.videocall.rs
```

### Check DNS Resolution
```bash
for slot in 1 2 3; do
  echo "Checking slot ${slot}:"
  dig +short pr${slot}.sandbox.videocall.rs
  dig +short pr${slot}-api.sandbox.videocall.rs
  dig +short pr${slot}-ws.sandbox.videocall.rs
done
```

All should return the same LoadBalancer IP.

---

## Rollback Plan

If slot-based deployment doesn't work, rollback by reverting these commits:

```bash
git revert <commit-hash-of-slot-changes>
git push origin main
```

This will restore the PR-based `preview-{NUM}` deployment pattern.

**Note:** Since this is in `jboyd01/videocall-rs` (not upstream yet), there's no production impact.

---

## Known Issues / Limitations

### Expected Limitations
1. **Max 3 concurrent previews** - By design for OAuth compatibility
2. **No WebTransport** - Disabled in previews (WebSocket fallback)
3. **No E2EE** - Disabled for simplicity in testing
4. **Requires maintainer** - Only collaborators can deploy

### Potential Issues
1. **Race condition:** Two simultaneous deployments might race for last slot
   - Mitigation: 30-second wait for in-progress undeploys
   - One deployment will fail with capacity error

2. **Stale namespace labels:** If manual cleanup leaves labels inconsistent
   - Detection: `kubectl get ns -l app=preview` shows unexpected results
   - Fix: Manually update labels or delete namespace

3. **Database not dropped:** If undeploy fails partway through
   - Detection: Database exists but namespace doesn't
   - Fix: Manually drop database: `DROP DATABASE preview_slot_N;`

---

## Success Criteria

Deployment is successful if:

✅ First deployment assigns slot 1
✅ Second deployment assigns slot 2
✅ Third deployment assigns slot 3
✅ Fourth deployment fails with capacity error
✅ Undeploy frees slot immediately
✅ Redeployment reuses same slot
✅ OAuth login works on all slots
✅ Each slot shows correct PR content
✅ Auto-cleanup works on PR close
✅ DNS resolves for all slot URLs
✅ TLS certificate works for all slots

---

## Next Steps

1. **Create OAuth secret** (if not exists):
   ```bash
   kubectl create secret generic google-oauth-credentials -n default \
     --from-literal=client-id="..." \
     --from-literal=client-secret="..."
   ```

2. **Commit and push changes**:
   ```bash
   git add .github/workflows/ docs/
   git commit -m "feat: implement slot-based PR preview deployment with OAuth

   - Use fixed 3-slot system (pr1, pr2, pr3) for OAuth compatibility
   - Enable Google OAuth by default in all previews
   - Automatic slot assignment and reuse
   - Improved capacity management and user messaging
   - Slot-based database and namespace naming

   Closes #571"
   git push origin main
   ```

3. **Test with real PR**:
   - Create or use existing PR
   - Comment `/build-and-deploy`
   - Follow Phase 1 testing steps

4. **Validate OAuth**:
   - Visit preview URL
   - Test Google login flow
   - Verify redirect works correctly

5. **Test full workflow**:
   - Deploy 3 PRs (fill all slots)
   - Test capacity error
   - Test undeploy and slot reuse
   - Test auto-cleanup

6. **Document findings**:
   - Note any issues in PR comments
   - Update documentation if needed
   - Fix issues and retest

7. **Merge to upstream** (after validation):
   - Create PR to `security-union/videocall-rs`
   - Include all workflow and documentation changes
   - Reference issue #571

---

## Questions or Issues?

If you encounter problems:

1. Check workflow logs in GitHub Actions
2. Review `docs/PR_PREVIEW_SLOT_BASED_IMPLEMENTATION.md` for details
3. Check `docs/PR_PREVIEW_QUICK_START.md` for common issues
4. Verify prerequisites are configured
5. Test with single deployment first before multiple

**Ready to shake'n bake! 🚀**
