# Configurable Max Slots - Update Summary

**Date:** 2026-02-19
**Change:** Made maximum preview slots configurable via repository variable

---

## What Changed

Replaced hardcoded `3` with configurable `PR_PREVIEW_MAX_SLOTS` repository variable throughout all workflows and documentation.

---

## Configuration Variable

### `PR_PREVIEW_MAX_SLOTS`

**Type:** GitHub Repository Variable (not Secret)

**Location:** Repository Settings → Secrets and variables → Actions → Variables

**Default:** `3` (if not set)

**Purpose:** Control maximum number of concurrent PR preview deployments

**Set via GitHub CLI:**
```bash
gh variable set PR_PREVIEW_MAX_SLOTS --body "3"
```

**Set via GitHub UI:**
1. Navigate to repository **Settings**
2. Go to **Secrets and variables** → **Actions** → **Variables** tab
3. Click **New repository variable**
4. Name: `PR_PREVIEW_MAX_SLOTS`
5. Value: `3` (or desired number)

---

## Changes Made

### Workflow Files

#### 1. `.github/workflows/pr-deploy-reusable.yaml`

**Slot assignment loop:**
```bash
# Before (hardcoded)
for SLOT in 1 2 3; do

# After (configurable)
MAX_SLOTS=${{ vars.PR_PREVIEW_MAX_SLOTS }}
MAX_SLOTS=${MAX_SLOTS:-3}  # Default to 3
for SLOT in $(seq 1 $MAX_SLOTS); do
```

**Capacity error message:**
```bash
# Before
echo "❌ All 3 preview slots are occupied"

# After
echo "❌ All ${MAX_SLOTS} preview slots are occupied"
```

**Success comment:**
```javascript
// Before
body: `📊 **Slot occupancy:** ${slot}/3 slots in use`

// After
const maxSlots = ${{ steps.assign-slot.outputs.max_slots }};
body: `📊 **Slot occupancy:** Slot ${slot} of ${maxSlots} maximum slots`
```

**Outputs added:**
- `max_slots` - Passed to subsequent steps and comments

#### 2. `.github/workflows/pr-deploy.yaml`

**Deployment started message:**
```javascript
// Before
body: `- Assigning deployment slot (1, 2, or 3)`

// After
const maxSlots = '${{ vars.PR_PREVIEW_MAX_SLOTS }}' || '3';
body: `- Assigning deployment slot (1-${maxSlots} available)`
```

### Documentation Files

#### 3. `docs/PR_PREVIEW_CONFIGURATION.md` (NEW)

Complete configuration guide including:
- How to set the variable
- Choosing appropriate max slots value
- OAuth callback management
- Troubleshooting
- Migration path

#### 4. `docs/PR_PREVIEW_SLOT_BASED_IMPLEMENTATION.md`

**Updated sections:**
- Overview: Mentions configurable slots
- Key Changes: Added section 0 about configuration
- Prerequisites: Added step to set variable
- OAuth: Notes callbacks needed for ALL slots

#### 5. `docs/PR_PREVIEW_QUICK_START.md`

**Updated sections:**
- Slot-Based System: Notes configurable maximum
- Capacity Management: Reflects dynamic slot count
- FAQ: Added question about increasing slots

#### 6. `SLOT_BASED_DEPLOYMENT_CHANGES.md`

**Updated sections:**
- Added Configuration Variable section
- Updated test plan to include variable setup

---

## Behavior Comparison

### Before (Hardcoded 3 Slots)

```bash
# Workflow code
for SLOT in 1 2 3; do
  # Check slot availability
done

# Error message
"All preview slots are in use (3/3)"

# Success message
"Slot occupancy: 2/3 slots in use"
```

**Changing slots required:**
- Editing workflow files
- Updating all hardcoded references
- Committing and pushing changes
- Waiting for workflow file updates to deploy

### After (Configurable Slots)

```bash
# Workflow code
MAX_SLOTS=${PR_PREVIEW_MAX_SLOTS:-3}
for SLOT in $(seq 1 $MAX_SLOTS); do
  # Check slot availability
done

# Error message
"All preview slots are in use (5/5)"  # If MAX_SLOTS=5

# Success message
"Slot occupancy: Slot 2 of 5 maximum slots"  # If MAX_SLOTS=5
```

**Changing slots requires:**
- Setting repository variable (1 command or UI click)
- No code changes
- No commits
- Immediate effect on next workflow run

---

## How to Use

### Initial Setup (One-Time)

Set the variable to your desired maximum:

```bash
# Default: 3 slots
gh variable set PR_PREVIEW_MAX_SLOTS --body "3"

# Or for larger team: 5 slots
gh variable set PR_PREVIEW_MAX_SLOTS --body "5"
```

### Increasing Slots Later

**Example: 3 → 5 slots**

1. **Register OAuth callbacks:**
   - Add `https://pr4-api.sandbox.videocall.rs/login/callback`
   - Add `https://pr5-api.sandbox.videocall.rs/login/callback`
   - In Google Cloud Console OAuth credentials

2. **Update variable:**
   ```bash
   gh variable set PR_PREVIEW_MAX_SLOTS --body "5"
   ```

3. **Test:**
   - Deploy PRs to fill all 5 slots
   - Verify OAuth works on slots 4-5
   - Check capacity error shows "5/5"

### Decreasing Slots

**Example: 5 → 3 slots**

1. **Undeploy excess slots:**
   ```bash
   kubectl delete namespace preview-slot-4
   kubectl delete namespace preview-slot-5
   ```

2. **Update variable:**
   ```bash
   gh variable set PR_PREVIEW_MAX_SLOTS --body "3"
   ```

3. **Verify:**
   - Next deployment should only use slots 1-3
   - Capacity error should show "3/3"

---

## Default Behavior

**If variable NOT set:**
```bash
MAX_SLOTS=${{ vars.PR_PREVIEW_MAX_SLOTS }}  # Empty string
MAX_SLOTS=${MAX_SLOTS:-3}                   # Defaults to 3
```

**Result:** Works exactly as before (3 slots)

**No breaking changes** - existing deployments continue to work without any configuration.

---

## Validation

### Check Current Value

```bash
# Via GitHub CLI
gh variable get PR_PREVIEW_MAX_SLOTS

# Via API
curl -H "Authorization: token TOKEN" \
  https://api.github.com/repos/OWNER/REPO/actions/variables/PR_PREVIEW_MAX_SLOTS
```

### Test in Workflow

Add debug step to workflow:

```yaml
- name: Debug max slots
  run: |
    echo "Variable: ${{ vars.PR_PREVIEW_MAX_SLOTS }}"
    MAX_SLOTS=${{ vars.PR_PREVIEW_MAX_SLOTS }}
    echo "After assignment: ${MAX_SLOTS}"
    echo "With default: ${MAX_SLOTS:-3}"
```

### Verify Capacity Error

Deploy PRs until capacity reached:

```bash
# Deploy MAX_SLOTS + 1 PRs
# Last one should show:
# "❌ All preview slots are in use (N/N)"
# Where N = PR_PREVIEW_MAX_SLOTS value
```

---

## Resource Implications

### Per Slot Resources

- CPU: ~160m
- Memory: ~350Mi
- Pods: 4 (NATS, WebSocket, API, UI)

### Total by Max Slots

| MAX_SLOTS | CPU Request | Memory Request | Cost Impact |
|-----------|-------------|----------------|-------------|
| 3         | ~480m       | ~1Gi           | Baseline    |
| 5         | ~800m       | ~1.7Gi         | +67%        |
| 8         | ~1280m      | ~2.7Gi         | +167%       |
| 10        | ~1600m      | ~3.5Gi         | +233%       |

**Recommendation:** Start with 3, increase only if capacity issues occur frequently.

---

## OAuth Callback Management

### Callbacks Needed per Max Slots

**MAX_SLOTS=3 (default):**
```
https://pr1-api.sandbox.videocall.rs/login/callback
https://pr2-api.sandbox.videocall.rs/login/callback
https://pr3-api.sandbox.videocall.rs/login/callback
```

**MAX_SLOTS=5:**
```
(same as above) +
https://pr4-api.sandbox.videocall.rs/login/callback
https://pr5-api.sandbox.videocall.rs/login/callback
```

**MAX_SLOTS=10:**
```
(same as above) +
https://pr6-api.sandbox.videocall.rs/login/callback
https://pr7-api.sandbox.videocall.rs/login/callback
https://pr8-api.sandbox.videocall.rs/login/callback
https://pr9-api.sandbox.videocall.rs/login/callback
https://pr10-api.sandbox.videocall.rs/login/callback
```

**⚠️ Critical:** MUST register all callbacks BEFORE increasing MAX_SLOTS, otherwise OAuth will fail on new slots.

---

## Troubleshooting

### Variable Not Working

**Symptom:** Workflow still uses 3 slots despite setting variable to 5

**Debug:**
```bash
# Check variable exists
gh variable list | grep PR_PREVIEW_MAX_SLOTS

# Check workflow logs
# Look for "Maximum preview slots configured: X"
```

**Solutions:**
1. Verify exact variable name: `PR_PREVIEW_MAX_SLOTS`
2. Check scope is "Repository" not "Environment"
3. Re-run workflow (don't just re-trigger)

### OAuth Fails on Higher Slots

**Symptom:** Slots 1-3 work, slot 4 shows OAuth error

**Cause:** Callback URL not registered for slot 4

**Solution:**
1. Add `https://pr4-api.sandbox.videocall.rs/login/callback` to Google OAuth
2. Wait 1-2 minutes
3. Retry login

---

## Migration Checklist

✅ Set `PR_PREVIEW_MAX_SLOTS` variable (default: 3)
✅ Test deployment to verify variable is read correctly
✅ Check capacity error shows correct slot count
✅ Verify success message shows correct slot occupancy
✅ Test increasing slots (if needed)
✅ Test decreasing slots (if needed)

---

## Why This Change?

**Benefits:**

1. **Flexibility:** Change capacity without editing code
2. **No commits:** Adjust slots via GitHub UI or CLI
3. **Safe default:** Falls back to 3 if not set
4. **Team-specific:** Each fork can configure independently
5. **Future-proof:** Easy to scale as team grows

**Before:** Hardcoded `3` meant editing workflows for any change

**After:** One-line command to adjust capacity: `gh variable set PR_PREVIEW_MAX_SLOTS --body "5"`

---

## Related Documentation

- **`docs/PR_PREVIEW_CONFIGURATION.md`** - Complete configuration guide
- **`docs/PR_PREVIEW_SLOT_BASED_IMPLEMENTATION.md`** - Technical implementation
- **`docs/PR_PREVIEW_QUICK_START.md`** - User guide
- **`SLOT_BASED_DEPLOYMENT_CHANGES.md`** - Original changes summary

---

## Summary

✅ **Configurable:** Max slots controlled by repository variable

✅ **Backward compatible:** Defaults to 3 if not set

✅ **No code changes required:** Set once, works everywhere

✅ **Dynamic messaging:** Error and success messages reflect actual max slots

✅ **Well documented:** Complete configuration guide provided

**To set:**
```bash
gh variable set PR_PREVIEW_MAX_SLOTS --body "3"
```

**Documentation:**
- Technical: `docs/PR_PREVIEW_CONFIGURATION.md`
- User guide: `docs/PR_PREVIEW_QUICK_START.md`
