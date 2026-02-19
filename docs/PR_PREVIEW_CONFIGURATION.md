# PR Preview Deployment Configuration

**Last Updated:** 2026-02-19

---

## Overview

PR preview deployments use **GitHub Repository Variables** for configuration. This allows changing settings without modifying workflow files.

---

## Repository Variables

### `PR_PREVIEW_MAX_SLOTS`

**Purpose:** Maximum number of concurrent PR preview deployments allowed

**Type:** Integer (1-10 recommended)

**Default:** `3` (if not set)

**Location:** GitHub Repository → Settings → Secrets and variables → Actions → Variables

**Usage:**
- Controls how many PRs can have active preview deployments simultaneously
- Each slot has a fixed URL: `pr1.sandbox.videocall.rs`, `pr2.sandbox.videocall.rs`, etc.
- Required for OAuth compatibility (callback URLs must be pre-registered)

---

## Setting Up Configuration

### Via GitHub UI

1. Go to your repository on GitHub
2. Navigate to: **Settings** → **Secrets and variables** → **Actions**
3. Click the **Variables** tab
4. Click **New repository variable**
5. Set:
   - **Name:** `PR_PREVIEW_MAX_SLOTS`
   - **Value:** `3` (or your desired number)
6. Click **Add variable**

### Via GitHub CLI

```bash
# Set max slots to 3
gh variable set PR_PREVIEW_MAX_SLOTS --body "3"

# Verify
gh variable list
```

### Via API

```bash
# Set variable
curl -X POST \
  -H "Authorization: token YOUR_GITHUB_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/repos/OWNER/REPO/actions/variables \
  -d '{"name":"PR_PREVIEW_MAX_SLOTS","value":"3"}'

# Get variable
curl -H "Authorization: token YOUR_GITHUB_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/repos/OWNER/REPO/actions/variables/PR_PREVIEW_MAX_SLOTS
```

---

## Choosing Max Slots Value

### Considerations

**OAuth Callback URLs:**
- Each slot needs a pre-registered OAuth callback URL
- Google OAuth: `https://pr{N}-api.sandbox.videocall.rs/login/callback`
- You must register callback URLs for ALL slots (1 to MAX_SLOTS)

**Cluster Resources:**
- Each slot consumes: ~160m CPU, ~350Mi memory
- 3 slots = ~480m CPU, ~1Gi memory
- 5 slots = ~800m CPU, ~1.7Gi memory
- 10 slots = ~1.6 CPU, ~3.5Gi memory

**DNS Configuration:**
- Wildcard DNS `*.sandbox.videocall.rs` covers unlimited slots
- TLS wildcard cert `*.sandbox.videocall.rs` covers unlimited slots
- No changes needed when increasing slots

**Team Size:**
- Small team (1-5 developers): `3` slots sufficient
- Medium team (5-10 developers): `5` slots recommended
- Large team (10+ developers): `7-10` slots

**Development Velocity:**
- Frequent PRs: Higher slot count prevents capacity issues
- Infrequent PRs: Lower slot count reduces resource usage

### Recommended Values

| Team Size | Concurrent PRs | Recommended Slots |
|-----------|---------------|-------------------|
| 1-3 devs  | 1-2 PRs/day   | 2-3 slots         |
| 4-7 devs  | 3-5 PRs/day   | 4-5 slots         |
| 8-15 devs | 6-10 PRs/day  | 6-8 slots         |
| 16+ devs  | 10+ PRs/day   | 8-10 slots        |

**Default of 3** is conservative and suitable for most teams.

---

## Changing Max Slots

### Increasing Slots (e.g., 3 → 5)

**Steps:**

1. **Register OAuth callbacks:**
   ```
   https://pr4-api.sandbox.videocall.rs/login/callback
   https://pr5-api.sandbox.videocall.rs/login/callback
   ```
   Add to Google Cloud Console OAuth credentials.

2. **Update repository variable:**
   ```bash
   gh variable set PR_PREVIEW_MAX_SLOTS --body "5"
   ```

3. **Test new slots:**
   - Deploy PRs until slots 4 and 5 are used
   - Verify OAuth works on new slots
   - Verify DNS resolves for `pr4.*` and `pr5.*`

**No downtime:** Existing deployments (slots 1-3) continue working.

### Decreasing Slots (e.g., 5 → 3)

**Steps:**

1. **Check active deployments:**
   ```bash
   kubectl get namespaces -l app=preview
   ```

2. **Undeploy slots beyond new limit:**
   ```bash
   # If slots 4 and 5 are occupied, undeploy them
   kubectl delete namespace preview-slot-4
   kubectl delete namespace preview-slot-5
   ```

3. **Update repository variable:**
   ```bash
   gh variable set PR_PREVIEW_MAX_SLOTS --body "3"
   ```

4. **Verify:**
   - Next deployment should fail if trying to use slot 4+
   - Capacity errors should show "3/3" instead of "5/5"

**Impact:** PRs currently in slots 4-5 will be undeployed.

---

## Validation and Defaults

### Workflow Behavior

**Variable not set:**
```bash
MAX_SLOTS=${{ vars.PR_PREVIEW_MAX_SLOTS }}
MAX_SLOTS=${MAX_SLOTS:-3}  # Defaults to 3
```

**Variable set to empty string:**
```bash
# Treated as not set, defaults to 3
```

**Variable set to invalid value (e.g., "abc"):**
```bash
# Bash treats as 0, loop fails
# Recommendation: Validate via GitHub Actions input validation
```

**Variable set to 0:**
```bash
# Loop `seq 1 0` produces no output
# No slots available, all deployments fail
```

### Best Practices

**Always set explicitly:**
```bash
gh variable set PR_PREVIEW_MAX_SLOTS --body "3"
```

**Validate value:**
```bash
# Check value is positive integer
if [[ ! "$MAX_SLOTS" =~ ^[1-9][0-9]*$ ]]; then
  echo "Invalid MAX_SLOTS: $MAX_SLOTS"
  exit 1
fi
```

**Document current value:**
```bash
# Add to repository README or docs
echo "Current max slots: $(gh variable get PR_PREVIEW_MAX_SLOTS)"
```

---

## OAuth Callback URL Management

### Workflow for Adding Slots

When increasing `PR_PREVIEW_MAX_SLOTS` from `N` to `M`:

1. **Calculate new callbacks needed:**
   - New slots: `N+1` to `M`
   - Callbacks: `https://pr{X}-api.sandbox.videocall.rs/login/callback` for X in [N+1, M]

2. **Register in Google Cloud Console:**
   - Go to: APIs & Services → Credentials
   - Select OAuth 2.0 Client ID
   - Add new Authorized redirect URIs
   - Save

3. **Update repository variable:**
   ```bash
   gh variable set PR_PREVIEW_MAX_SLOTS --body "$M"
   ```

4. **Test immediately:**
   - Deploy a PR to verify new slots work
   - Test OAuth login on new slot

### OAuth Callback List (Reference)

For `PR_PREVIEW_MAX_SLOTS=5`:

```
https://pr1-api.sandbox.videocall.rs/login/callback
https://pr2-api.sandbox.videocall.rs/login/callback
https://pr3-api.sandbox.videocall.rs/login/callback
https://pr4-api.sandbox.videocall.rs/login/callback
https://pr5-api.sandbox.videocall.rs/login/callback
```

For `PR_PREVIEW_MAX_SLOTS=10`:

```
https://pr1-api.sandbox.videocall.rs/login/callback
https://pr2-api.sandbox.videocall.rs/login/callback
https://pr3-api.sandbox.videocall.rs/login/callback
https://pr4-api.sandbox.videocall.rs/login/callback
https://pr5-api.sandbox.videocall.rs/login/callback
https://pr6-api.sandbox.videocall.rs/login/callback
https://pr7-api.sandbox.videocall.rs/login/callback
https://pr8-api.sandbox.videocall.rs/login/callback
https://pr9-api.sandbox.videocall.rs/login/callback
https://pr10-api.sandbox.videocall.rs/login/callback
```

---

## Troubleshooting

### Variable Not Taking Effect

**Symptom:** Workflows still use old value (e.g., 3 instead of 5)

**Causes:**
1. Variable not saved properly
2. Workflow using cached value
3. Variable name typo

**Debug:**
```bash
# Check variable value
gh variable get PR_PREVIEW_MAX_SLOTS

# Check in workflow
# Add debug step in workflow:
- name: Debug max slots
  run: |
    echo "Variable value: ${{ vars.PR_PREVIEW_MAX_SLOTS }}"
    MAX_SLOTS=${{ vars.PR_PREVIEW_MAX_SLOTS }}
    echo "After assignment: ${MAX_SLOTS}"
    echo "With default: ${MAX_SLOTS:-3}"
```

**Solution:**
1. Verify variable set correctly: `gh variable list`
2. Re-run workflow (don't just re-trigger)
3. Check workflow logs for debug output

### OAuth Fails on New Slots

**Symptom:** Slots 1-3 work, slots 4-5 show OAuth error

**Cause:** Callback URLs not registered for slots 4-5

**Solution:**
1. Go to Google Cloud Console
2. Add missing callback URLs
3. Wait 1-2 minutes for propagation
4. Retry OAuth login

### Capacity Error Shows Wrong Number

**Symptom:** Error says "3/3" but MAX_SLOTS is set to 5

**Cause:** Workflow not reading variable correctly

**Debug:**
```bash
# Check assign-slot step output
kubectl get namespaces -l app=preview
# Should show up to 5 namespaces if MAX_SLOTS=5
```

**Solution:**
1. Check variable name is exactly `PR_PREVIEW_MAX_SLOTS`
2. Verify variable scope is "Repository" not "Environment"
3. Re-run workflow from GitHub Actions UI

---

## Migration Path

### From Hardcoded to Variable

**Current state:** Workflows have hardcoded `3`

**Migration steps:**

1. **Set variable to current value:**
   ```bash
   gh variable set PR_PREVIEW_MAX_SLOTS --body "3"
   ```

2. **Update workflows to use variable:**
   - Replace `for SLOT in 1 2 3; do` with `for SLOT in $(seq 1 $MAX_SLOTS); do`
   - Replace hardcoded "3" in messages with `${MAX_SLOTS}`

3. **Test with same value (3):**
   - Deploy PRs to verify nothing broke
   - Check capacity errors show "3/3"

4. **Increase to new value:**
   ```bash
   gh variable set PR_PREVIEW_MAX_SLOTS --body "5"
   ```

5. **Validate:**
   - Deploy to slots 1-5
   - Verify capacity error at 6th deployment

---

## Security Considerations

### Variable Access

**Who can read:**
- Anyone with read access to repository (variables are NOT secrets)
- Visible in workflow logs
- Visible in GitHub Actions UI

**Who can modify:**
- Repository admins
- Users with "Actions" write permissions

### Recommended Access Control

**Use GitHub Actions RBAC:**
1. Limit who can modify variables
2. Audit changes via GitHub audit log
3. Require approval for variable changes

**Do NOT store sensitive data:**
- MAX_SLOTS is not sensitive (just a number)
- Never use variables for secrets (use Secrets instead)

---

## Future Enhancements

### Potential Improvements

1. **Auto-scaling slots:**
   - Automatically increase MAX_SLOTS based on queue length
   - Decrease when slots are idle

2. **Per-environment slots:**
   - Different MAX_SLOTS for staging vs production
   - Use GitHub Environments instead of Variables

3. **Slot priority:**
   - Reserve slots for high-priority PRs
   - Queue system when capacity reached

4. **Cost tracking:**
   - Calculate cost per slot
   - Alert when approaching budget limit

---

## Related Documentation

- `PR_PREVIEW_SLOT_BASED_DEPLOYMENT.md` - Original slot-based design
- `PR_PREVIEW_SLOT_BASED_IMPLEMENTATION.md` - Implementation details
- `PR_PREVIEW_QUICK_START.md` - User guide

---

## Summary

✅ **Configuration is flexible:** Change MAX_SLOTS without editing code

✅ **Default is safe:** Falls back to 3 if not set

✅ **OAuth-aware:** Must register callbacks for all slots

✅ **Resource-conscious:** Consider cluster capacity when increasing

**Recommended default:** `3` slots for most teams

**To configure:**
```bash
gh variable set PR_PREVIEW_MAX_SLOTS --body "3"
```
