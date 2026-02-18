# GHCR Permissions Issue - PR #625

**Date**: 2025-02-17
**PR**: https://github.com/security-union/videocall-rs/pull/625
**Status**: Blocked on organization permissions

---

## The Problem

PR #625 successfully modified the `docker-build-check.yaml` workflow to push images to GHCR, but the build fails with:

```
ERROR: failed to push ghcr.io/security-union/videocall-media-server:pr-625:
denied: installation not allowed to Create organization package
```

## Root Cause

The `security-union` GitHub organization has not granted GitHub Actions permission to create packages in the organization namespace (`ghcr.io/security-union/`).

By default, GitHub organizations restrict package creation for security. The workflow is using `GITHUB_TOKEN` which doesn't have permission to create new packages in the org.

---

## Solution Options

### Option 1: Enable Organization Package Creation (RECOMMENDED)

**Who**: Requires `security-union` organization admin/owner

**Steps**:
1. Navigate to: https://github.com/organizations/security-union/settings/packages
2. Under "Package creation":
   - Enable: **"Actions can create packages in this organization"**
   - OR configure per-repository access for `videocall-rs`
3. Save settings
4. Re-run the failed workflow: https://github.com/security-union/videocall-rs/pull/625/checks

**Pros**:
- ✅ No code changes needed
- ✅ Works for all future PRs automatically
- ✅ Proper organization-level package namespace
- ✅ Uses automatic `GITHUB_TOKEN` (no secrets management)

**Cons**:
- ❌ Requires organization admin access
- ❌ May take time if admin is unavailable

---

### Option 2: Use Personal Access Token

**Who**: Anyone with package write permissions

**Steps**:

1. Generate PAT: https://github.com/settings/tokens/new
   - Scopes required:
     - `write:packages`
     - `read:packages`
     - `read:org` (for org packages)
   - Name: `GHCR Push Token`
   - Expiration: 90 days (or custom)

2. Add as repository secret: https://github.com/security-union/videocall-rs/settings/secrets/actions
   - Name: `GHCR_TOKEN`
   - Value: (paste PAT)

3. Update workflow (`.github/workflows/docker-build-check.yaml`):
   ```yaml
   - name: Login to GitHub Container Registry
     uses: docker/login-action@v3
     with:
       registry: ghcr.io
       username: ${{ github.actor }}
       password: ${{ secrets.GHCR_TOKEN }}  # Changed from GITHUB_TOKEN
   ```

4. Commit and push to PR #625

**Pros**:
- ✅ Doesn't require organization admin
- ✅ Can be done immediately
- ✅ Works with existing code (one-line change)

**Cons**:
- ❌ Requires secret management
- ❌ PAT expires and needs rotation
- ❌ Tied to specific user account
- ❌ Less secure than automatic token

---

### Option 3: Use Personal Namespace (TESTING ONLY)

**Who**: Anyone (good for testing)

**Steps**:

1. Update workflow to push to personal namespace:
   ```yaml
   # In all 3 build jobs, change:
   tags: ghcr.io/jboyd01/videocall-media-server:pr-${{ github.event.pull_request.number }}
   tags: ghcr.io/jboyd01/videocall-meeting-api:pr-${{ github.event.pull_request.number }}
   tags: ghcr.io/jboyd01/videocall-web-ui:pr-${{ github.event.pull_request.number }}
   ```

2. Update Helm values and deployment scripts to use `ghcr.io/jboyd01/`

3. Commit and push

**Pros**:
- ✅ Works immediately
- ✅ Good for testing workflow logic
- ✅ No organization permissions needed

**Cons**:
- ❌ Images under personal account, not organization
- ❌ Not suitable for production
- ❌ Requires changing deployment scripts
- ❌ Harder for team to discover packages

---

## Recommendation

**For immediate testing**: Use **Option 3** (personal namespace) to validate workflow logic

**For production**: Use **Option 1** (organization settings) - request admin to configure

**If admin unavailable**: Use **Option 2** (PAT) as interim solution, migrate to Option 1 later

---

## What's Already Working

✅ Workflow syntax is correct
✅ Docker builds complete successfully
✅ GHCR authentication works
✅ Image tagging is correct
✅ PR comment logic is implemented

**Only issue**: Permission to create packages in org namespace

---

## Testing Plan

Once permissions are resolved:

1. **Re-run workflow**: https://github.com/security-union/videocall-rs/pull/625/checks
2. **Verify images pushed**: https://github.com/orgs/security-union/packages
3. **Verify PR comment** appears with image tags
4. **Pull image locally**:
   ```bash
   docker pull ghcr.io/security-union/videocall-media-server:pr-625
   docker pull ghcr.io/security-union/videocall-meeting-api:pr-625
   docker pull ghcr.io/security-union/videocall-web-ui:pr-625
   ```
5. **Merge PR** if all checks pass

---

## Next Steps After Merge

Once PR #625 is merged, all future PRs will automatically:
- Build images when code changes
- Push to GHCR with `pr-<PR>` tags
- Comment on PR with available images
- Enable `/deploy` command (next phase)

---

## Contact

- **Organization admin needed**: Contact security-union org owner
- **Questions**: Comment on PR #625
- **Documentation**: See `docs/PR_PREVIEW_DUAL_ENVIRONMENT_PLAN.md`
