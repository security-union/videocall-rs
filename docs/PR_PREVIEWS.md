# PR Previews

PR previews give contributors and maintainers a live staging environment for each pull request — a full stack (UI, WebSocket server, Meeting API, database) deployed to the shared sandbox cluster. WebRTC and OAuth flows require a real browser against real infrastructure; this system makes that possible without any manual setup per PR.

## Design

### Slot-based URLs

Google OAuth requires pre-registered exact callback URLs — wildcards and per-PR dynamic URLs are not supported. The system uses a fixed pool of numbered **slots** with pre-registered URLs rather than one URL per PR:

| Slot | UI | API |
|------|----|-----|
| 1 | `pr1.sandbox.videocall.rs` | `pr1-api.sandbox.videocall.rs` |
| 2 | `pr2.sandbox.videocall.rs` | `pr2-api.sandbox.videocall.rs` |
| 3 | `pr3.sandbox.videocall.rs` | `pr3-api.sandbox.videocall.rs` |

The number of active slots is controlled by the `PR_PREVIEW_MAX_SLOTS` repository variable. **Adding slots requires registering the new callback URLs in the Google OAuth console** (`https://prN-api.sandbox.videocall.rs/login/callback`) before they will work.

### Infrastructure layout

Each active slot has its own Kubernetes namespace (`preview-slot-{N}`) containing NATS, the WebSocket server, the Meeting API, and the web UI.

A shared `preview-infra` namespace holds the PostgreSQL instance (one database per slot: `preview_slot_{N}`) and OAuth credentials. This is provisioned once by a cluster maintainer and outlives individual deployments.

### State management

Slot → PR assignments are stored as labels on the `preview-slot-{N}` namespace. No external state store is needed. Deleting the namespace frees the slot atomically and is idempotent.

### Images

PR images are pushed to **GHCR** (`ghcr.io/<org>/...`) — this is intentional and separate from production images on Docker Hub. GHCR is free for public repositories, which keeps preview costs low.

A weekly scheduled job purges GHCR images tagged `pr-*` that are older than 7 days, along with any untagged (ghost) images. Production images (non-`pr-*` tags) are never touched (they are in a different repo!). If you need to redeploy a PR whose images have been purged, run `/build-and-deploy` to rebuild them.

### WebTransport

WebTransport is not enabled in preview environments. It requires a dedicated DigitalOcean load balancer (additional cost), so previews fall back to WebSocket.  We'll revisit this later.

### Workflows

| Workflow | Trigger | Purpose |
|----------|---------|---------|
| `pr-deploy-reusable.yaml` | `workflow_call` | Core deployment logic — single source of truth |
| `pr-deploy.yaml` | `/deploy` comment | Deploy pre-built images to a slot |
| `pr-build-and-deploy.yaml` | `/build-and-deploy` comment | Build images then deploy in one step |
| `pr-build-images-command.yaml` | `/build-images` comment | Build images only |
| `pr-undeploy.yaml` | `/undeploy` comment | Tear down a preview and free its slot |
| `pr-cleanup.yaml` | PR closed | Auto-cleanup on merge or close |
| `pr-welcome.yaml` | PR opened | Post available commands to the PR |

## Developer / Maintainer Guide

Deployment commands are posted as PR comments. Only OWNER, MEMBER, and COLLABORATOR roles can trigger them (security measure — external contributors cannot deploy).

### Commands

**Build and deploy (most common):**
```
/build-and-deploy
```
Builds all Docker images and deploys them (~15–20 min). The bot replies with the preview URL when ready.

**Deploy only**
```
/deploy
```
Use this when images are already built from a previous run, only takes ~3–5 min to deploy.

**Build only:**
```
/build-images
```
Build all images for deployment in parallel, this takes 7-10 minutes.

**Remove a preview:**
```
/undeploy
```
Frees the slot immediately deleting the pr namespace and database. Previews are also removed automatically when the PR is closed or merged.  Takes 1-3 minutes.

**Redeploying:** Push new commits and re-comment `/build-and-deploy`. The same slot is preserved.

**Capacity:** If all slots are occupied, the bot lists which PRs hold them. Ask one to `/undeploy` by commenting in the specific PR or wait for a PR to close.

## Cluster Setup (one-time, maintainers only)

Required GitHub Actions secrets:
- `DIGITALOCEAN_ACCESS_TOKEN`
- `GH_TOKEN` (with `read:packages` scope)

The `preview-infra` namespace must be provisioned once before any previews can deploy:

```bash
export KUBECONFIG=<path-to-cluster-kubeconfig>
bash scripts/digitalocean-prod-setup-preview-infra.sh
```

The script creates the PR namespace, creates a database in the running postgres instance in the preview-infra namespace, and copies OAuth credentials from the `default` namespace.

When adding new slots beyond the current maximum (we started at only 3 slots):
1. Update `PR_PREVIEW_MAX_SLOTS` in repository variables.
2. Register `https://prN-api.sandbox.videocall.rs/login/callback` in the Google OAuth console for each new slot.

## Kubernetes Internals

### Namespace labels

Every `preview-slot-{N}` namespace carries three labels that serve as the system's state store:

```
app=preview   # identifies all preview namespaces
slot=N        # the slot number
pr=PR_NUM     # the PR currently occupying this slot
```

Slot lookup at deploy time:
```bash
# Find if a PR already has a slot
kubectl get namespaces -l app=preview,pr=<PR_NUM> \
  -o jsonpath='{.items[0].metadata.labels.slot}'

# List all occupied slots
kubectl get namespaces -l app=preview \
  -o custom-columns=SLOT:.metadata.labels.slot,PR:.metadata.labels.pr
```

Labels are written with `--overwrite`, so redeploying the same PR to the same slot is a no-op on the namespace itself.

### Secrets per slot namespace

Each `preview-slot-{N}` namespace contains:

| Secret | Source | Notes |
|--------|--------|-------|
| `sandbox-wildcard-tls` | Copied from `default` at deploy time | TLS cert for `*.sandbox.videocall.rs` |
| `jwt-secret` | Generated once per slot | Persisted across redeployments — preserves user sessions |

The `jwt-secret` is intentionally **not regenerated on redeployment**. If it were, any logged-in user's session would be invalidated every time a PR is updated.

### Namespace lifecycle

```
/deploy         → namespace created, labels written, secrets populated, helm releases installed
/undeploy       → namespace deleted (takes all helm releases, secrets, and the slot label with it)
PR closed/merged → same as /undeploy (pr-cleanup.yaml)
```

Deleting the namespace is the single operation that frees a slot — there is no separate state to clean up.

## Troubleshooting

**"Images not found"** — Images haven't been built. Use `/build-and-deploy` instead of `/deploy`.

**"All slots are in use"** — The bot lists active slots. Ask a PR author to `/undeploy`, or wait for a PR to close.

**Pod in CrashLoopBackOff:**
```bash
kubectl logs <pod> -n preview-slot-<N>
```
Common causes: invalid `DATABASE_URL` (password with special characters), missing secrets, image pull failure.

**OAuth redirect mismatch** — The callback URL for this slot must be registered in the Google OAuth console. Only pre-registered slot URLs work; per-PR dynamic URLs will fail.

**Namespace stuck terminating:**
```bash
kubectl get namespace preview-slot-<N> -o json \
  | jq '.spec.finalizers = []' \
  | kubectl replace --raw /api/v1/namespaces/preview-slot-<N>/finalize -f -
```
