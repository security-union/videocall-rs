# PR Preview Deployment - Quick Start Guide

## What are PR Previews?

PR previews are temporary live environments where you can test changes from a pull request before merging. Each preview includes:
- Full stack deployment (UI, API, WebSocket server, database)
- Google OAuth authentication (login required)
- Isolated environment per PR
- Automatic cleanup when PR closes

## Slot-Based System

We have **multiple deployment slots** available (default: 3):
- **Slot 1**: `https://pr1.sandbox.videocall.rs`
- **Slot 2**: `https://pr2.sandbox.videocall.rs`
- **Slot 3**: `https://pr3.sandbox.videocall.rs`
- ... (up to configured maximum)

When you deploy your PR, it gets assigned to the first available slot.

**Note:** The maximum number of slots is configured by repository maintainers via the `PR_PREVIEW_MAX_SLOTS` variable (see `docs/PR_PREVIEW_CONFIGURATION.md`).

---

## Quick Commands

### Deploy a Preview

**Option 1: Build and Deploy (one command)**
```
/build-and-deploy
```
Builds Docker images and deploys in one step (~15-20 minutes)

**Option 2: Build then Deploy (two commands)**
```
/build-images
```
Wait for images to build (~10-15 minutes), then:
```
/deploy
```
Deploy to available slot (~3-5 minutes)

### Update a Preview

Push new commits to your PR, then:
```
/build-and-deploy
```
This rebuilds images and redeploys to the same slot.

### Remove a Preview

```
/undeploy
```
Frees up the slot for other PRs (~1 minute)

---

## What Happens When You Deploy

1. **Slot Assignment**
   - Finds first available slot (1, 2, or 3)
   - Or reuses your existing slot if redeploying

2. **Infrastructure Setup**
   - Creates Kubernetes namespace `preview-slot-{N}`
   - Creates database `preview_slot_{N}`
   - Deploys NATS messaging server

3. **Service Deployment**
   - WebSocket server
   - Meeting API with OAuth
   - Web UI

4. **URL Generation**
   - UI: `https://pr{N}.sandbox.videocall.rs`
   - API: `https://pr{N}-api.sandbox.videocall.rs`
   - WebSocket: `wss://pr{N}-ws.sandbox.videocall.rs`

---

## Example Workflow

**Scenario:** You're working on PR #123

1. **Create PR and wait for CI**
   - Push commits
   - Wait for build checks to pass

2. **Deploy preview**
   - Comment `/build-and-deploy` on PR #123
   - Wait ~15-20 minutes
   - Bot comments: "✅ Preview deployed to slot 1"

3. **Test your changes**
   - Open `https://pr1.sandbox.videocall.rs`
   - Log in with Google OAuth
   - Test functionality

4. **Make updates**
   - Push new commits
   - Comment `/build-and-deploy` again
   - Bot redeploys to slot 1

5. **Cleanup**
   - Comment `/undeploy` when done testing
   - Or close/merge PR (auto-cleanup)

---

## Capacity Management

### What if all slots are full?

You'll see an error message like:

```
❌ All preview slots are in use (3/3)

Active deployments:
- Slot 1: PR #120 → https://pr1.sandbox.videocall.rs (undeploy)
- Slot 2: PR #121 → https://pr2.sandbox.videocall.rs (undeploy)
- Slot 3: PR #122 → https://pr3.sandbox.videocall.rs (undeploy)

Action required: Comment /undeploy on one of the PRs above.
```

**Note:** The number shown (e.g., "3/3") reflects the configured maximum slots for your repository.

**Solutions:**
1. Ask if any of the active PRs can be undeployed
2. Wait for one to be automatically cleaned up (when PR closes)
3. Prioritize: Undeploy older/less critical PRs
4. Ask a maintainer to increase max slots (see `docs/PR_PREVIEW_CONFIGURATION.md`)

### Slot Reuse

When you undeploy:
- Slot becomes available immediately
- Another PR can claim it
- Database and resources are fully cleaned up

---

## Authentication

All PR previews require **Google OAuth login**:

1. Visit `https://pr{N}.sandbox.videocall.rs`
2. Click "Sign in with Google"
3. Authorize with your Google account
4. Redirected to the app

**Note:** Only the sandbox OAuth app is used (not production credentials)

---

## Features & Limitations

### Enabled Features
✅ WebSocket communication
✅ Google OAuth authentication
✅ Full media server functionality
✅ Database persistence (per deployment)
✅ HTTPS/TLS encryption

### Disabled Features
❌ WebTransport (uses WebSocket fallback)
❌ End-to-end encryption
❌ Multi-region deployment

### Limitations
- Maximum 3 concurrent previews
- No persistent storage (data cleared on undeploy)
- Single region (US East only)
- Requires maintainer permissions to deploy

---

## Permissions

Only repository **maintainers** can deploy previews:
- OWNER
- MEMBER
- COLLABORATOR

First-time contributors cannot deploy previews (security measure).

---

## Troubleshooting

### "Images not found" error

**Problem:** Docker images haven't been built yet

**Solution:**
```
/build-images
```
Wait for build to complete, then:
```
/deploy
```

### "Capacity exceeded" error

**Problem:** All 3 slots are occupied

**Solution:** Comment `/undeploy` on one of the active PRs, then retry

### OAuth login fails

**Problem:** OAuth redirect fails or shows error

**Possible causes:**
- OAuth secret not configured (ask maintainer)
- Callback URL not registered (already done)
- Wrong redirect URL in configuration

**Debug:** Check workflow logs for OAuth configuration

### Deployment fails

**Problem:** Workflow fails during deployment

**Common causes:**
1. GHCR images not found → Run `/build-images` first
2. TLS certificate missing → Ask maintainer to configure
3. Database connection fails → Ask maintainer to check postgres

**Next steps:**
1. Check workflow logs: Click "View deployment logs" in error comment
2. Ask in PR comments for help
3. Retry with `/deploy` after issue is fixed

### Preview shows old code

**Problem:** Deployed but still shows old version

**Solution:**
1. Force browser refresh (Ctrl+Shift+R)
2. Clear browser cache
3. Verify correct images were built:
   ```
   /build-images
   /undeploy
   /deploy
   ```

---

## Best Practices

### When to Deploy
✅ Testing major features
✅ Testing OAuth flows
✅ Testing UI changes
✅ Sharing live demo with reviewers

### When NOT to Deploy
❌ Small typo fixes
❌ Documentation-only changes
❌ Backend-only changes (can test locally)

### Cleanup Etiquette
- Undeploy when done testing (frees slot for others)
- Don't leave previews running overnight
- Close PRs when abandoned (triggers auto-cleanup)

### Security
- Don't use production data in previews
- Don't commit secrets (use K8s secrets)
- Always use HTTPS URLs
- OAuth is required (can't disable)

---

## FAQ

**Q: How long does deployment take?**
A: 15-20 minutes for build+deploy, 3-5 minutes for deploy-only

**Q: Can I deploy to a specific slot?**
A: No, slots are auto-assigned to first available

**Q: Can I have multiple PRs deployed at once?**
A: Yes, up to the configured maximum slots (default: 3) can be deployed simultaneously

**Q: Can we increase the number of available slots?**
A: Yes, maintainers can increase `PR_PREVIEW_MAX_SLOTS` repository variable. See `docs/PR_PREVIEW_CONFIGURATION.md` for details. Note: OAuth callbacks must be registered for new slots.

**Q: What happens if my PR is closed?**
A: Preview is automatically undeployed within a few minutes

**Q: Can I deploy to production from a preview?**
A: No, previews are for testing only. Merge to main to deploy to production.

**Q: Are preview databases persistent?**
A: Database persists during the preview lifetime but is deleted on undeploy

**Q: Can I access the preview from mobile?**
A: Yes, `https://pr{N}.sandbox.videocall.rs` works on any device

**Q: Is the preview URL public?**
A: URL is public, but requires Google OAuth login

**Q: How do I check which slot my PR is using?**
A: Look for "✅ Preview deployed to slot X" in PR comments

**Q: Can I deploy someone else's PR?**
A: Yes, if you're a maintainer you can deploy any PR

---

## Getting Help

**Deployment issues:**
1. Check workflow logs (link in error comment)
2. Ask in PR comments: `@maintainer can you help with deployment?`
3. Check `#engineering` Slack channel

**OAuth issues:**
1. Verify you're using correct Google account
2. Try incognito mode
3. Clear cookies for `.sandbox.videocall.rs`

**General questions:**
- Read `docs/PR_PREVIEW_SLOT_BASED_IMPLEMENTATION.md` (technical details)
- Ask in PR comments
- Ping in Slack

---

## Reference

### Commands
- `/build-images` - Build Docker images only
- `/deploy` - Deploy to available slot
- `/build-and-deploy` - Build images and deploy
- `/undeploy` - Remove preview and free slot

### URLs (after deployment)
- **UI:** `https://pr{SLOT}.sandbox.videocall.rs`
- **API:** `https://pr{SLOT}-api.sandbox.videocall.rs`
- **WebSocket:** `wss://pr{SLOT}-ws.sandbox.videocall.rs`

### Kubectl Commands (maintainers only)
```bash
# List active slots
kubectl get namespaces -l app=preview

# Check slot resources
kubectl get all -n preview-slot-1

# Check logs
kubectl logs -n preview-slot-1 -l app.kubernetes.io/name=meeting-api

# Force cleanup
kubectl delete namespace preview-slot-1
```

---

**Happy testing! 🚀**
