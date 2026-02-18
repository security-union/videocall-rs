# PR Preview Deployments - Dual Environment Implementation Plan

**Issue**: https://github.com/security-union/videocall-rs/issues/571
**Status**: In Progress - Phase 1 (Opensource Build Pipeline)
**Date**: 2025-02-17
**Current PR**: https://github.com/security-union/videocall-rs/pull/625
**Environments**: Opensource (security-union) + HCL Internal Fork

---

## Executive Summary

This plan implements ephemeral PR preview deployments in **two independent environments**:

1. **Opensource** (`github.com/security-union/videocall-rs`)
   - Registry: GitHub Container Registry (GHCR) - `ghcr.io/security-union/`
   - Cluster: DigitalOcean K8s (sandbox cluster)
   - Domain: `pr-<PR>.sandbox.videocall.rs`
   - Runners: GitHub-hosted (free for public repos)

2. **HCL Internal Fork** (`github01.hclpnp.com/labs-projects/videocall`)
   - Registry: Harbor - `hclcr.io/harbor/projects/89/`
   - Cluster: HCL local K8s cluster
   - Domain: TBD (e.g., `pr-<PR>.videocall-preview.hcl.internal`)
   - Runners: Self-hosted on HCL infrastructure

**Strategy**: Maximize code reuse between environments while respecting infrastructure differences. Shared components (build scripts, Helm charts, workflow logic) are parameterized for environment-specific deployment targets.

---

## Implementation Status

### Phase 1: Opensource Build Pipeline (IN PROGRESS)

**PR #625**: https://github.com/security-union/videocall-rs/pull/625

**Approach**: Modified existing `docker-build-check.yaml` workflow instead of creating a separate workflow.

**Changes Made**:
- ✅ Added GHCR authentication to existing build jobs
- ✅ Changed `push: false` to `push: true` for all 3 images
- ✅ Added image tags: `ghcr.io/security-union/videocall-*:pr-<PR>`
- ✅ Added PR comment job to show available images
- ✅ Added workflow file itself to trigger paths (self-testing)

**Current Issue**: GHCR Organization Permissions

```
ERROR: denied: installation not allowed to Create organization package
```

**Root Cause**: The `security-union` GitHub organization has not granted GitHub Actions permission to create packages in the organization namespace.

**Solution Required**: Organization admin must configure GHCR permissions (see Troubleshooting section below).

**Alternative Solutions**:
1. Push to user namespace (`ghcr.io/jboyd01/`) for testing
2. Use Personal Access Token instead of `GITHUB_TOKEN`
3. Configure organization package permissions

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                     SHARED CODEBASE (Git)                           │
│  ┌────────────────────────────────────────────────────────────┐    │
│  │ Build Scripts (cut_build_push_*.sh)                        │    │
│  │ - Parameterized: REGISTRY, TAG, PUSH                       │    │
│  │ - Used by both environments                                │    │
│  ├────────────────────────────────────────────────────────────┤    │
│  │ Helm Charts (helm/preview/)                                │    │
│  │ - Umbrella chart with image registry override              │    │
│  │ - Environment-specific values files                        │    │
│  ├────────────────────────────────────────────────────────────┤    │
│  │ Dockerfiles                                                │    │
│  │ - Dockerfile.actix, Dockerfile.yew, Dockerfile.meeting-api │    │
│  └────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────┘
                    │                           │
                    ▼                           ▼
┌──────────────────────────────────┐  ┌──────────────────────────────────┐
│   OPENSOURCE ENVIRONMENT         │  │   HCL INTERNAL ENVIRONMENT       │
├──────────────────────────────────┤  ├──────────────────────────────────┤
│ Trigger: PR opened/updated       │  │ Trigger: PR opened/updated       │
│ Runner: GitHub-hosted            │  │ Runner: Self-hosted (HCL)        │
│                                  │  │                                  │
│ Build:                           │  │ Build:                           │
│  ├─ media-server                 │  │  ├─ media-server                 │
│  ├─ meeting-api                  │  │  ├─ meeting-api                  │
│  └─ ui                           │  │  └─ ui                           │
│                                  │  │                                  │
│ Push to:                         │  │ Push to:                         │
│  ghcr.io/security-union/         │  │  hclcr.io/harbor/projects/89/    │
│  videocall-*:pr-<PR>             │  │  videocall-*:pr-<PR>             │
│                                  │  │                                  │
│ Deploy to:                       │  │ Deploy to:                       │
│  DigitalOcean K8s (NYC1)         │  │  HCL Local K8s Cluster           │
│  Namespace: preview-<PR>         │  │  Namespace: preview-<PR>         │
│                                  │  │                                  │
│ URL: pr-<PR>.sandbox.videocall.rs│  │ URL: pr-<PR>.videocall-preview   │
│                                  │  │      .hcl.internal               │
│                                  │  │                                  │
│ Shared Infra (sandbox ns):      │  │ Shared Infra (infra ns):         │
│  ├─ PostgreSQL                   │  │  ├─ PostgreSQL                   │
│  ├─ NATS                         │  │  ├─ NATS                         │
│  ├─ ingress-nginx                │  │  ├─ ingress-nginx                │
│  └─ cert-manager                 │  │  └─ cert-manager                 │
│                                  │  │                                  │
│ Commands:                        │  │ Commands:                        │
│  /deploy → create preview        │  │  /deploy → create preview        │
│  /undeploy → destroy preview     │  │  /undeploy → destroy preview     │
│  PR close → auto cleanup         │  │  PR close → auto cleanup         │
└──────────────────────────────────┘  └──────────────────────────────────┘
```

---

## Environment Comparison

| Aspect | Opensource | HCL Internal |
|--------|-----------|--------------|
| **Git Remote** | `github.com/security-union/videocall-rs` | `github01.hclpnp.com/labs-projects/videocall` |
| **Registry** | `ghcr.io/security-union/` (GHCR) | `hclcr.io/harbor/projects/89/` (Harbor) |
| **Registry Auth** | `GITHUB_TOKEN` (automatic) | Harbor credentials (secret) |
| **K8s Cluster** | DigitalOcean Managed K8s (NYC1) | HCL Local K8s Cluster |
| **K8s Auth** | `DIGITALOCEAN_ACCESS_TOKEN` + doctl | `KUBECONFIG` (direct) |
| **Domain Pattern** | `pr-<PR>.sandbox.videocall.rs` | `pr-<PR>.videocall-preview.hcl.internal` |
| **TLS** | Let's Encrypt via cert-manager | Internal CA or Let's Encrypt |
| **Runners** | GitHub-hosted (`ubuntu-latest`) | Self-hosted (`[self-hosted, linux, docker]`) |
| **Image Retention** | GHCR package settings | Harbor retention policies |
| **Capacity Limit** | Max 3 concurrent previews | Max 3 concurrent previews |

---

## Shared Components (Environment-Agnostic)

These components are identical across both environments and live in the main codebase:

### 1. Dockerfiles
- `Dockerfile.actix` - Media server (WebSocket/WebTransport)
- `Dockerfile.yew` - Web UI (WASM + nginx)
- `Dockerfile.meeting-api` - REST API

**No changes needed** - these already exist and work correctly.

### 2. Build Scripts (Refactored)
Create parameterized versions of existing build scripts:

**`build_common.sh`** (new shared library):
```bash
#!/bin/bash
# Shared functions for building and pushing Docker images

set -e

# Defaults
export REGISTRY="${REGISTRY:-securityunion}"
export REGISTRY_HOST="${REGISTRY_HOST:-docker.io}"
export TAG="${TAG:-$(git rev-parse HEAD)}"
export PUSH="${PUSH:-true}"

# Normalize registry format
# Examples:
#   ghcr.io/security-union    → ghcr.io/security-union
#   hclcr.io/harbor/projects/89 → hclcr.io/harbor/projects/89
#   securityunion             → docker.io/securityunion
normalize_registry() {
    local reg="$1"
    if [[ "$reg" =~ ^(ghcr\.io|hclcr\.io|quay\.io) ]]; then
        echo "$reg"
    else
        echo "docker.io/$reg"
    fi
}

# Build and optionally push an image
# Args: IMAGE_NAME DOCKERFILE [BUILD_ARGS...]
build_and_push() {
    local image_name="$1"
    local dockerfile="$2"
    shift 2
    local build_args=("$@")

    local full_registry=$(normalize_registry "$REGISTRY")
    local image_url="${full_registry}/${image_name}:${TAG}"

    echo "=========================================="
    echo "Building: ${image_url}"
    echo "Dockerfile: ${dockerfile}"
    echo "Push: ${PUSH}"
    echo "=========================================="

    if ! docker build -t "$image_url" -f "$dockerfile" "${build_args[@]}" .; then
        echo "ERROR: Failed to build ${image_name}"
        return 1
    fi

    if [[ "$PUSH" == "true" ]]; then
        echo "Pushing ${image_url}..."
        if ! docker push "$image_url"; then
            echo "ERROR: Failed to push ${image_name}"
            return 1
        fi
        echo "✓ Pushed ${image_url}"
    else
        echo "✓ Built ${image_url} (push disabled)"
    fi
}
```

**Updated `cut_build_push_ui.sh`**:
```bash
#!/bin/bash
set -e
source "$(dirname "$0")/build_common.sh"

TAG="${1:-$(git rev-parse HEAD)}"
build_and_push "videocall-web-ui" "Dockerfile.yew" \
    --build-arg USERS_ALLOWED_TO_STREAM="dario,griffin,hamdy"
```

**Updated `cut_build_push_backend.sh`**:
```bash
#!/bin/bash
set -e
source "$(dirname "$0")/build_common.sh"

TAG="${1:-$(git rev-parse HEAD)}"
build_and_push "videocall-media-server" "Dockerfile.actix"
```

**Updated `cut_build_push_meeting_api.sh`**:
```bash
#!/bin/bash
set -e
source "$(dirname "$0")/build_common.sh"

TAG="${1:-$(git rev-parse HEAD)}"
build_and_push "videocall-meeting-api" "Dockerfile.meeting-api"
```

### 3. Helm Preview Chart
**`helm/preview/Chart.yaml`**:
```yaml
apiVersion: v2
name: preview
description: Ephemeral PR preview environment
version: 0.1.0
appVersion: "1.0"

dependencies:
  - name: rustlemania-websocket
    version: "0.1.0"
    repository: "file://../rustlemania-websocket"
  - name: rustlemania-ui
    version: "0.1.0"
    repository: "file://../rustlemania-ui"
  - name: meeting-api
    version: "0.1.0"
    repository: "file://../meeting-api"
```

**`helm/preview/values.yaml`** (base defaults):
```yaml
# PR number (override via --set prNumber=123)
prNumber: "0"

# Global image settings (override per environment)
global:
  imageRegistry: "ghcr.io"
  imageOrg: "security-union"
  imageTag: "pr-0"

# Infrastructure connection (override per environment)
infrastructure:
  postgres:
    host: "postgres.sandbox.svc.cluster.local"
    port: 5432
    database: "actix_api_db"
    # Credentials from secrets
  nats:
    url: "nats://nats.sandbox.svc.cluster.local:4222"

# Resource limits per preview
resources:
  limits:
    cpu: "1"
    memory: "1Gi"
  requests:
    cpu: "500m"
    memory: "512Mi"

# Subdependency overrides
rustlemania-websocket:
  image:
    repository: "{{ .Values.global.imageRegistry }}/{{ .Values.global.imageOrg }}/videocall-media-server"
    tag: "{{ .Values.global.imageTag }}"
  env:
    - name: NATS_URL
      value: "{{ .Values.infrastructure.nats.url }}"
    - name: DATABASE_URL
      valueFrom:
        secretKeyRef:
          name: postgres-credentials
          key: connection-string
  resources: "{{ .Values.resources }}"

meeting-api:
  image:
    repository: "{{ .Values.global.imageRegistry }}/{{ .Values.global.imageOrg }}/videocall-meeting-api"
    tag: "{{ .Values.global.imageTag }}"
  env:
    - name: DATABASE_URL
      valueFrom:
        secretKeyRef:
          name: postgres-credentials
          key: connection-string
    - name: NATS_URL
      value: "{{ .Values.infrastructure.nats.url }}"
  resources: "{{ .Values.resources }}"

rustlemania-ui:
  image:
    repository: "{{ .Values.global.imageRegistry }}/{{ .Values.global.imageOrg }}/videocall-web-ui"
    tag: "{{ .Values.global.imageTag }}"
  runtimeConfig:
    apiBaseUrl: "https://pr-{{ .Values.prNumber }}.sandbox.videocall.rs"
    wsUrl: "wss://pr-{{ .Values.prNumber }}.sandbox.videocall.rs/ws"
    webTransportEnabled: "false"
    oauthEnabled: "false"
    e2eeEnabled: "false"
  resources: "{{ .Values.resources }}"
```

**`helm/preview/templates/ingress.yaml`**:
```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: preview-{{ .Values.prNumber }}
  namespace: {{ .Release.Namespace }}
  annotations:
    nginx.ingress.kubernetes.io/ssl-redirect: "true"
    nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
    nginx.ingress.kubernetes.io/proxy-send-timeout: "3600"
    nginx.ingress.kubernetes.io/rewrite-target: /$2
spec:
  ingressClassName: nginx
  tls:
    - secretName: {{ .Values.tls.secretName | default "preview-wildcard-tls" }}
      hosts:
        - {{ .Values.ingress.host }}
  rules:
    - host: {{ .Values.ingress.host }}
      http:
        paths:
          - path: /api(/|$)(.*)
            pathType: ImplementationSpecific
            backend:
              service:
                name: {{ include "meeting-api.fullname" .Subcharts.meetingApi }}
                port:
                  number: 80
          - path: /ws(/|$)(.*)
            pathType: ImplementationSpecific
            backend:
              service:
                name: {{ include "rustlemania-websocket.fullname" .Subcharts.rustlemaniaWebsocket }}
                port:
                  number: 80
          - path: /
            pathType: Prefix
            backend:
              service:
                name: {{ include "rustlemania-ui.fullname" .Subcharts.rustlemaniaUi }}
                port:
                  number: 80
```

**`helm/preview/templates/resource-quota.yaml`**:
```yaml
apiVersion: v1
kind: ResourceQuota
metadata:
  name: preview-quota
  namespace: {{ .Release.Namespace }}
spec:
  hard:
    requests.cpu: "1"
    requests.memory: "1Gi"
    limits.cpu: "2"
    limits.memory: "2Gi"
    pods: "10"
```

---

## Environment-Specific Components

### Opensource Environment

**`helm/preview/values-opensource.yaml`**:
```yaml
global:
  imageRegistry: "ghcr.io"
  imageOrg: "security-union"

infrastructure:
  postgres:
    host: "postgres.sandbox.svc.cluster.local"
    port: 5432
  nats:
    url: "nats://nats.sandbox.svc.cluster.local:4222"

ingress:
  host: "pr-{{ .Values.prNumber }}.sandbox.videocall.rs"

tls:
  secretName: "sandbox-wildcard-tls"

rustlemania-ui:
  runtimeConfig:
    apiBaseUrl: "https://pr-{{ .Values.prNumber }}.sandbox.videocall.rs"
    wsUrl: "wss://pr-{{ .Values.prNumber }}.sandbox.videocall.rs/ws"
```

**`.github/workflows/pr-build-images.yaml`** (Opensource):
```yaml
name: Build PR Images (Opensource)

on:
  pull_request:
    types: [opened, synchronize]
    paths:
      - 'actix-api/**'
      - 'yew-ui/**'
      - 'videocall-client/**'
      - 'videocall-types/**'
      - 'Dockerfile.*'
      - '.github/workflows/pr-build-images.yaml'

jobs:
  build-media-server:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4

      - name: Login to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Build and push media-server
        uses: docker/build-push-action@v5
        with:
          context: .
          file: Dockerfile.actix
          push: true
          tags: ghcr.io/security-union/videocall-media-server:pr-${{ github.event.pull_request.number }}
          cache-from: type=gha
          cache-to: type=gha,mode=max

  build-meeting-api:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4

      - name: Login to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Build and push meeting-api
        uses: docker/build-push-action@v5
        with:
          context: .
          file: Dockerfile.meeting-api
          push: true
          tags: ghcr.io/security-union/videocall-meeting-api:pr-${{ github.event.pull_request.number }}
          cache-from: type=gha
          cache-to: type=gha,mode=max

  build-ui:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4

      - name: Login to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Build and push UI
        uses: docker/build-push-action@v5
        with:
          context: .
          file: Dockerfile.yew
          push: true
          tags: ghcr.io/security-union/videocall-web-ui:pr-${{ github.event.pull_request.number }}
          build-args: |
            USERS_ALLOWED_TO_STREAM=dario,griffin,hamdy
          cache-from: type=gha
          cache-to: type=gha,mode=max

  comment:
    needs: [build-media-server, build-meeting-api, build-ui]
    runs-on: ubuntu-latest
    permissions:
      pull-requests: write
    steps:
      - name: Comment on PR
        uses: actions/github-script@v7
        with:
          script: |
            github.rest.issues.createComment({
              issue_number: context.issue.number,
              owner: context.repo.owner,
              repo: context.repo.repo,
              body: '✅ **PR images built successfully!**\n\nImages:\n- `ghcr.io/security-union/videocall-media-server:pr-${{ github.event.pull_request.number }}`\n- `ghcr.io/security-union/videocall-meeting-api:pr-${{ github.event.pull_request.number }}`\n- `ghcr.io/security-union/videocall-web-ui:pr-${{ github.event.pull_request.number }}`\n\nComment `/deploy` to create a preview environment.'
            })
```

**`.github/workflows/pr-deploy.yaml`** (Opensource):
```yaml
name: Deploy PR Preview (Opensource)

on:
  issue_comment:
    types: [created]

jobs:
  deploy:
    if: |
      github.event.issue.pull_request &&
      startsWith(github.event.comment.body, '/deploy') &&
      contains(fromJSON('["MEMBER", "OWNER", "COLLABORATOR"]'), github.event.comment.author_association)
    runs-on: ubuntu-latest
    permissions:
      pull-requests: write
      issues: write
    steps:
      - uses: actions/checkout@v4

      - name: Get PR number
        id: pr
        run: echo "number=${{ github.event.issue.number }}" >> $GITHUB_OUTPUT

      - name: Check capacity
        id: capacity
        run: |
          # Install doctl and configure kubectl
          cd ~
          wget https://github.com/digitalocean/doctl/releases/download/v1.98.1/doctl-1.98.1-linux-amd64.tar.gz
          tar xf doctl-*.tar.gz
          sudo mv doctl /usr/local/bin
          doctl auth init -t ${{ secrets.DIGITALOCEAN_ACCESS_TOKEN }}
          doctl kubernetes cluster kubeconfig save videocall-sandbox

          # Count existing previews
          PREVIEW_COUNT=$(kubectl get namespaces -l app=preview --no-headers 2>/dev/null | wc -l)
          echo "count=$PREVIEW_COUNT" >> $GITHUB_OUTPUT

          if [ "$PREVIEW_COUNT" -ge 3 ]; then
            echo "exceeded=true" >> $GITHUB_OUTPUT
            ACTIVE_PREVIEWS=$(kubectl get namespaces -l app=preview -o jsonpath='{.items[*].metadata.labels.pr}')
            echo "active=$ACTIVE_PREVIEWS" >> $GITHUB_OUTPUT
          else
            echo "exceeded=false" >> $GITHUB_OUTPUT
          fi

      - name: Capacity limit reached
        if: steps.capacity.outputs.exceeded == 'true'
        uses: actions/github-script@v7
        with:
          script: |
            github.rest.issues.createComment({
              issue_number: context.issue.number,
              owner: context.repo.owner,
              repo: context.repo.repo,
              body: '❌ **Preview capacity limit reached (3/3)**\n\nActive previews: ${{ steps.capacity.outputs.active }}\n\nPlease undeploy one with `/undeploy` or close a PR first.'
            })
            core.setFailed('Capacity limit reached')

      - name: Create namespace and deploy
        if: steps.capacity.outputs.exceeded == 'false'
        run: |
          PR_NUM=${{ steps.pr.outputs.number }}

          # Create namespace
          kubectl create namespace preview-${PR_NUM} --dry-run=client -o yaml | kubectl apply -f -
          kubectl label namespace preview-${PR_NUM} app=preview pr=${PR_NUM}

          # Update Helm dependencies
          helm dependency update helm/preview/

          # Deploy
          helm upgrade --install preview-${PR_NUM} helm/preview/ \
            --namespace preview-${PR_NUM} \
            --values helm/preview/values-opensource.yaml \
            --set prNumber=${PR_NUM} \
            --set global.imageTag=pr-${PR_NUM} \
            --wait --timeout 5m

      - name: Post preview URL
        if: steps.capacity.outputs.exceeded == 'false'
        uses: actions/github-script@v7
        with:
          script: |
            const prNum = ${{ steps.pr.outputs.number }};
            github.rest.issues.createComment({
              issue_number: context.issue.number,
              owner: context.repo.owner,
              repo: context.repo.repo,
              body: `🚀 **Preview deployed!**\n\n**URL**: https://pr-${prNum}.sandbox.videocall.rs\n\nComment \`/undeploy\` to remove this preview.`
            })

      - name: React to comment
        if: steps.capacity.outputs.exceeded == 'false'
        uses: actions/github-script@v7
        with:
          script: |
            github.rest.reactions.createForIssueComment({
              owner: context.repo.owner,
              repo: context.repo.repo,
              comment_id: context.payload.comment.id,
              content: 'rocket'
            })
```

---

### HCL Internal Environment

**`helm/preview/values-hcl.yaml`**:
```yaml
global:
  imageRegistry: "hclcr.io/harbor/projects/89"
  imageOrg: ""  # Harbor path already includes project

infrastructure:
  postgres:
    host: "postgres.infra.svc.cluster.local"
    port: 5432
  nats:
    url: "nats://nats.infra.svc.cluster.local:4222"

ingress:
  host: "pr-{{ .Values.prNumber }}.videocall-preview.hcl.internal"

tls:
  secretName: "preview-wildcard-tls"

rustlemania-ui:
  runtimeConfig:
    apiBaseUrl: "https://pr-{{ .Values.prNumber }}.videocall-preview.hcl.internal"
    wsUrl: "wss://pr-{{ .Values.prNumber }}.videocall-preview.hcl.internal/ws"
```

**`.github/workflows/pr-build-images.yaml`** (HCL - place in github01 repo):
```yaml
name: Build PR Images (HCL)

on:
  pull_request:
    types: [opened, synchronize]
    paths:
      - 'actix-api/**'
      - 'yew-ui/**'
      - 'videocall-client/**'
      - 'videocall-types/**'
      - 'Dockerfile.*'
      - '.github/workflows/pr-build-images.yaml'

jobs:
  build-media-server:
    runs-on: [self-hosted, linux, docker]
    steps:
      - uses: actions/checkout@v4

      - name: Login to Harbor
        uses: docker/login-action@v3
        with:
          registry: hclcr.io
          username: ${{ secrets.HARBOR_USERNAME }}
          password: ${{ secrets.HARBOR_PASSWORD }}

      - name: Build and push media-server
        run: |
          export REGISTRY="hclcr.io/harbor/projects/89"
          export TAG="pr-${{ github.event.pull_request.number }}"
          ./cut_build_push_backend.sh

  build-meeting-api:
    runs-on: [self-hosted, linux, docker]
    steps:
      - uses: actions/checkout@v4

      - name: Login to Harbor
        uses: docker/login-action@v3
        with:
          registry: hclcr.io
          username: ${{ secrets.HARBOR_USERNAME }}
          password: ${{ secrets.HARBOR_PASSWORD }}

      - name: Build and push meeting-api
        run: |
          export REGISTRY="hclcr.io/harbor/projects/89"
          export TAG="pr-${{ github.event.pull_request.number }}"
          ./cut_build_push_meeting_api.sh

  build-ui:
    runs-on: [self-hosted, linux, docker]
    steps:
      - uses: actions/checkout@v4

      - name: Login to Harbor
        uses: docker/login-action@v3
        with:
          registry: hclcr.io
          username: ${{ secrets.HARBOR_USERNAME }}
          password: ${{ secrets.HARBOR_PASSWORD }}

      - name: Build and push UI
        run: |
          export REGISTRY="hclcr.io/harbor/projects/89"
          export TAG="pr-${{ github.event.pull_request.number }}"
          ./cut_build_push_ui.sh

  comment:
    needs: [build-media-server, build-meeting-api, build-ui]
    runs-on: [self-hosted, linux, docker]
    steps:
      - name: Comment on PR
        run: |
          gh pr comment ${{ github.event.pull_request.number }} \
            --body "✅ **PR images built successfully!**

          Images:
          - \`hclcr.io/harbor/projects/89/videocall-media-server:pr-${{ github.event.pull_request.number }}\`
          - \`hclcr.io/harbor/projects/89/videocall-meeting-api:pr-${{ github.event.pull_request.number }}\`
          - \`hclcr.io/harbor/projects/89/videocall-web-ui:pr-${{ github.event.pull_request.number }}\`

          Comment \`/deploy\` to create a preview environment."
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

**`.github/workflows/pr-deploy.yaml`** (HCL - place in github01 repo):
```yaml
name: Deploy PR Preview (HCL)

on:
  issue_comment:
    types: [created]

jobs:
  deploy:
    if: |
      github.event.issue.pull_request &&
      startsWith(github.event.comment.body, '/deploy') &&
      contains(fromJSON('["MEMBER", "OWNER", "COLLABORATOR"]'), github.event.comment.author_association)
    runs-on: [self-hosted, linux, docker]
    steps:
      - uses: actions/checkout@v4

      - name: Get PR number
        id: pr
        run: echo "number=${{ github.event.issue.number }}" >> $GITHUB_OUTPUT

      - name: Configure kubectl
        run: |
          # Assumes kubeconfig is available on self-hosted runner
          # OR use secrets.KUBECONFIG if needed:
          # echo "${{ secrets.KUBECONFIG }}" > /tmp/kubeconfig
          # export KUBECONFIG=/tmp/kubeconfig
          kubectl version --client

      - name: Check capacity
        id: capacity
        run: |
          PREVIEW_COUNT=$(kubectl get namespaces -l app=preview --no-headers 2>/dev/null | wc -l)
          echo "count=$PREVIEW_COUNT" >> $GITHUB_OUTPUT

          if [ "$PREVIEW_COUNT" -ge 3 ]; then
            echo "exceeded=true" >> $GITHUB_OUTPUT
            ACTIVE_PREVIEWS=$(kubectl get namespaces -l app=preview -o jsonpath='{.items[*].metadata.labels.pr}')
            echo "active=$ACTIVE_PREVIEWS" >> $GITHUB_OUTPUT
          else
            echo "exceeded=false" >> $GITHUB_OUTPUT
          fi

      - name: Capacity limit reached
        if: steps.capacity.outputs.exceeded == 'true'
        run: |
          gh pr comment ${{ steps.pr.outputs.number }} \
            --body "❌ **Preview capacity limit reached (3/3)**

          Active previews: ${{ steps.capacity.outputs.active }}

          Please undeploy one with \`/undeploy\` or close a PR first."
          exit 1
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}

      - name: Create namespace and deploy
        if: steps.capacity.outputs.exceeded == 'false'
        run: |
          PR_NUM=${{ steps.pr.outputs.number }}

          # Create namespace
          kubectl create namespace preview-${PR_NUM} --dry-run=client -o yaml | kubectl apply -f -
          kubectl label namespace preview-${PR_NUM} app=preview pr=${PR_NUM}

          # Update Helm dependencies
          helm dependency update helm/preview/

          # Deploy
          helm upgrade --install preview-${PR_NUM} helm/preview/ \
            --namespace preview-${PR_NUM} \
            --values helm/preview/values-hcl.yaml \
            --set prNumber=${PR_NUM} \
            --set global.imageTag=pr-${PR_NUM} \
            --wait --timeout 5m

      - name: Post preview URL
        if: steps.capacity.outputs.exceeded == 'false'
        run: |
          PR_NUM=${{ steps.pr.outputs.number }}
          gh pr comment ${PR_NUM} \
            --body "🚀 **Preview deployed!**

          **URL**: https://pr-${PR_NUM}.videocall-preview.hcl.internal

          Comment \`/undeploy\` to remove this preview."
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

---

## Implementation Phases

### Phase 0: Foundation (Shared Components)

**Goal**: Create reusable build scripts and Helm charts that work in both environments.

**Tasks**:
1. ✅ Create `build_common.sh` with parameterized registry support
2. ✅ Update `cut_build_push_ui.sh` to use `build_common.sh`
3. ✅ Update `cut_build_push_backend.sh` to use `build_common.sh`
4. ✅ Update `cut_build_push_meeting_api.sh` to use `build_common.sh`
5. ✅ Test locally with different registries:
   ```bash
   # Test Docker Hub (default)
   TAG=test-local ./cut_build_push_backend.sh

   # Test GHCR
   REGISTRY="ghcr.io/security-union" TAG=test-ghcr PUSH=false ./cut_build_push_backend.sh

   # Test Harbor
   REGISTRY="hclcr.io/harbor/projects/89" TAG=test-harbor PUSH=false ./cut_build_push_backend.sh
   ```
6. ✅ Create `helm/preview/` umbrella chart with parameterized values
7. ✅ Create `helm/preview/values-opensource.yaml`
8. ✅ Create `helm/preview/values-hcl.yaml`
9. ✅ Test Helm chart dry-run:
   ```bash
   helm dependency update helm/preview/
   helm template preview-123 helm/preview/ \
     --values helm/preview/values-opensource.yaml \
     --set prNumber=123 \
     --set global.imageTag=pr-123
   ```

**Deliverables**:
- `build_common.sh` (new)
- Updated `cut_build_push_*.sh` (3 files)
- `helm/preview/` chart (new directory)
- `helm/preview/values-opensource.yaml` (new)
- `helm/preview/values-hcl.yaml` (new)

**Dependencies**: None

**Estimated Time**: 4-6 hours

---

### Phase 1: Opensource Build Pipeline

**Goal**: Implement PR image builds for opensource repo pushing to GHCR.

**Status**: ✅ Code Complete, ⏸️ Blocked on GHCR Permissions

**Approach Taken**: Modified existing `docker-build-check.yaml` instead of creating separate workflow. This is simpler and avoids duplication.

**Tasks**:
1. ✅ Modify `.github/workflows/docker-build-check.yaml`:
   - Add GHCR authentication to existing jobs
   - Change `push: false` to `push: true`
   - Add image tags for GHCR with `pr-<PR>` format
   - Add PR comment job
   - Add workflow file to trigger paths (self-test)
2. ✅ Open PR #625 to test workflow
3. ⏸️ **BLOCKED**: Configure GHCR organization package permissions
   - Error: `denied: installation not allowed to Create organization package`
   - Requires organization admin to enable Actions package creation
   - See Troubleshooting section for solutions
4. ⏳ Verify images push successfully
5. ⏳ Verify PR comment appears
6. ⏳ Verify GHA cache works for subsequent builds

**Deliverables**:
- ✅ Modified `.github/workflows/docker-build-check.yaml`
- ✅ PR #625 opened with changes
- ⏳ GHCR organization permissions configured

**Dependencies**: None (Phase 0 not needed - used existing workflow)

**Actual Time**: 1 hour (code), blocked on permissions configuration

**Next Action**: Organization admin must configure GHCR permissions (see Troubleshooting)

---

### Phase 2: Opensource Sandbox Cluster Setup

**Goal**: Prepare DigitalOcean K8s cluster for preview deployments.

**Tasks**:
1. Verify existing sandbox cluster or create new one:
   ```bash
   doctl kubernetes cluster list
   # If needed: doctl kubernetes cluster create videocall-sandbox --region nyc1 --size s-2vcpu-4gb --count 2
   ```
2. Deploy shared infrastructure in `sandbox` namespace:
   - PostgreSQL (from existing `helm/postgres/`)
   - NATS (single node, no gateway)
   - ingress-nginx (from existing `helm/ingress-nginx/`)
   - cert-manager + issuer (from existing `helm/cert-manager/`)
3. Create wildcard certificate for `*.sandbox.videocall.rs`:
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
       kind: ClusterIssuer
     dnsNames:
       - "*.sandbox.videocall.rs"
   ```
4. Configure DNS:
   - Verify `*.sandbox.videocall.rs` points to ingress IP
   - OR configure ExternalDNS if available
5. Create postgres credentials secret (for preview deployments):
   ```bash
   kubectl create secret generic postgres-credentials \
     --from-literal=connection-string="postgres://user:pass@postgres.sandbox:5432/actix_api_db" \
     --namespace=sandbox
   ```

**Deliverables**:
- Configured DigitalOcean K8s cluster
- Shared infrastructure deployed in `sandbox` namespace
- DNS configured for `*.sandbox.videocall.rs`
- Wildcard TLS certificate

**Dependencies**: Phase 1 complete (images available)

**Estimated Time**: 3-4 hours

---

### Phase 3: Opensource Deployment Pipeline

**Goal**: Implement `/deploy` command for opensource PRs.

**Tasks**:
1. Add GitHub secret `DIGITALOCEAN_ACCESS_TOKEN` to `security-union/videocall-rs`
2. Create `.github/workflows/pr-deploy.yaml` (opensource version)
3. Create `.github/workflows/pr-undeploy.yaml` (manual cleanup)
4. Create `.github/workflows/pr-cleanup.yaml` (automatic on PR close)
5. Test end-to-end:
   - Open draft PR
   - Wait for build to complete
   - Comment `/deploy`
   - Verify deployment succeeds
   - Access `https://pr-<PR>.sandbox.videocall.rs`
   - Test functionality (join room, audio/video)
   - Comment `/undeploy`
   - Verify cleanup

**Deliverables**:
- `.github/workflows/pr-deploy.yaml` (opensource)
- `.github/workflows/pr-undeploy.yaml` (opensource)
- `.github/workflows/pr-cleanup.yaml` (opensource)

**Dependencies**: Phase 2 complete (cluster ready)

**Estimated Time**: 4-5 hours

---

### Phase 4: HCL Self-Hosted Runner Setup

**Goal**: Configure self-hosted GitHub Actions runner for HCL environment.

**Tasks**:
1. Provision Linux VM or container for runner:
   - OS: Ubuntu 22.04 LTS
   - Specs: 4 vCPU, 8GB RAM, 100GB disk (for Docker layer cache)
   - Network: Access to github01.hclpnp.com and hclcr.io
2. Install prerequisites:
   ```bash
   # Docker
   curl -fsSL https://get.docker.com | sh
   sudo usermod -aG docker $USER

   # GitHub CLI (for PR comments)
   curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg
   echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" | sudo tee /etc/apt/sources.list.d/github-cli.list > /dev/null
   sudo apt update && sudo apt install gh

   # Helm
   curl https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash

   # kubectl
   curl -LO "https://dl.k8s.io/release/$(curl -L -s https://dl.k8s.io/release/stable.txt)/bin/linux/amd64/kubectl"
   sudo install -o root -g root -m 0755 kubectl /usr/local/bin/kubectl
   ```
3. Register runner with github01.hclpnp.com:
   - Navigate to `github01.hclpnp.com/labs-projects/videocall/settings/actions/runners`
   - Click "New self-hosted runner"
   - Follow instructions to download and configure runner
   - Add labels: `self-hosted`, `linux`, `docker`
4. Configure runner as a service:
   ```bash
   cd actions-runner
   sudo ./svc.sh install
   sudo ./svc.sh start
   ```
5. Test runner:
   - Create a simple test workflow on github01 repo
   - Verify it runs on the self-hosted runner
   - Check Docker access works

**Deliverables**:
- Self-hosted runner VM configured and registered
- Runner service running and healthy
- Test workflow passing

**Dependencies**: None (can be done in parallel with Phase 1-3)

**Estimated Time**: 2-3 hours

---

### Phase 5: HCL Cluster Setup

**Goal**: Prepare HCL local K8s cluster for preview deployments.

**Tasks**:
1. Verify cluster access from self-hosted runner:
   ```bash
   # On runner VM
   kubectl get nodes
   kubectl get namespaces
   ```
2. Deploy shared infrastructure in `infra` namespace (or equivalent):
   - PostgreSQL
   - NATS
   - ingress-nginx
   - cert-manager
3. Configure DNS for `*.videocall-preview.hcl.internal`:
   - Option A: Wildcard DNS entry pointing to ingress IP
   - Option B: Configure CoreDNS for internal resolution
   - Option C: Use ExternalDNS with internal DNS provider
4. Create TLS certificate:
   - For internal domain, may need internal CA
   - OR use Let's Encrypt if domain is publicly resolvable
5. Create Harbor credentials secret on runner or in cluster:
   ```bash
   # Option 1: On runner (for docker login)
   docker login hclcr.io -u <username> -p <password>

   # Option 2: In cluster (for image pull)
   kubectl create secret docker-registry harbor-registry \
     --docker-server=hclcr.io \
     --docker-username=<username> \
     --docker-password=<password> \
     --namespace=infra
   ```
6. Configure kubectl on runner:
   ```bash
   # Copy kubeconfig to runner
   mkdir -p /home/runner/.kube
   cp /path/to/kubeconfig /home/runner/.kube/config
   chmod 600 /home/runner/.kube/config
   ```

**Deliverables**:
- HCL K8s cluster prepared
- Shared infrastructure deployed
- DNS configured for `*.videocall-preview.hcl.internal`
- Runner has kubectl access
- Harbor credentials configured

**Dependencies**: Phase 4 complete (runner available)

**Estimated Time**: 3-4 hours

---

### Phase 6: HCL Build Pipeline

**Goal**: Implement PR image builds for HCL fork pushing to Harbor.

**Tasks**:
1. Add Harbor credentials to github01 repo secrets:
   - `HARBOR_USERNAME`
   - `HARBOR_PASSWORD`
2. Copy `.github/workflows/pr-build-images.yaml` to github01 repo
3. Modify workflow:
   - Change `runs-on` to `[self-hosted, linux, docker]`
   - Change registry to `hclcr.io/harbor/projects/89`
   - Use `docker/login-action` for Harbor
4. Test workflow on a draft PR:
   - Verify builds run on self-hosted runner
   - Verify images push to Harbor
   - Verify images are visible in Harbor UI: `https://hclcr.io/harbor/projects/89/repositories`
   - Verify PR comment appears
5. Configure Harbor retention policy:
   - Navigate to project settings in Harbor
   - Set retention rule (e.g., "Keep last 10 images" or "Delete after 7 days")

**Deliverables**:
- `.github/workflows/pr-build-images.yaml` (HCL version on github01)

**Dependencies**: Phase 5 complete (cluster + Harbor ready)

**Estimated Time**: 2-3 hours

---

### Phase 7: HCL Deployment Pipeline

**Goal**: Implement `/deploy` command for HCL PRs.

**Tasks**:
1. Add kubeconfig to github01 repo secrets (if not configured on runner):
   - `KUBECONFIG` (base64-encoded kubeconfig file)
2. Copy deployment workflows to github01 repo:
   - `.github/workflows/pr-deploy.yaml`
   - `.github/workflows/pr-undeploy.yaml`
   - `.github/workflows/pr-cleanup.yaml`
3. Modify workflows:
   - Change `runs-on` to `[self-hosted, linux, docker]`
   - Use `helm/preview/values-hcl.yaml`
   - Use GitHub CLI (`gh`) instead of `actions/github-script` (GHES compatibility)
4. Test end-to-end:
   - Open draft PR on github01
   - Wait for build
   - Comment `/deploy`
   - Verify deployment to HCL cluster
   - Access `https://pr-<PR>.videocall-preview.hcl.internal`
   - Test functionality
   - Comment `/undeploy`
   - Verify cleanup

**Deliverables**:
- `.github/workflows/pr-deploy.yaml` (HCL version on github01)
- `.github/workflows/pr-undeploy.yaml` (HCL version on github01)
- `.github/workflows/pr-cleanup.yaml` (HCL version on github01)

**Dependencies**: Phase 6 complete (images building)

**Estimated Time**: 3-4 hours

---

### Phase 8: Polish & Documentation

**Goal**: Add monitoring, logging, and documentation for both environments.

**Tasks**:
1. Add preview reaper workflow (stale cleanup):
   - `.github/workflows/preview-reaper.yaml` (both repos)
   - Runs daily, cleans up closed PRs
2. Add capacity monitoring:
   - Script or dashboard showing active previews
   - Slack/email notification at 80% capacity?
3. Update documentation:
   - Add PR preview usage guide to `CONTRIBUTING.md`
   - Document `/deploy` and `/undeploy` commands
   - Document capacity limits
   - Document troubleshooting steps
4. Add PR template hint:
   ```markdown
   ## Testing
   Comment `/deploy` to create a preview environment for this PR.
   ```
5. Optional: Add status badges to PR comments showing deployment status

**Deliverables**:
- `.github/workflows/preview-reaper.yaml` (both repos)
- Updated `CONTRIBUTING.md`
- PR template with deployment hint

**Dependencies**: Phases 3 and 7 complete

**Estimated Time**: 2-3 hours

---

## Secrets & Credentials Summary

### Opensource (github.com/security-union/videocall-rs)
- `GITHUB_TOKEN` - Automatic, used for GHCR push
- `DIGITALOCEAN_ACCESS_TOKEN` - Manual, used for K8s access via doctl

### HCL (github01.hclpnp.com/labs-projects/videocall)
- `HARBOR_USERNAME` - Manual, for Harbor registry auth
- `HARBOR_PASSWORD` - Manual, for Harbor registry auth
- `KUBECONFIG` - Optional, if kubectl not configured on runner

---

## Configuration Checklist

### Before Starting
- [ ] Decide on HCL preview domain pattern (e.g., `pr-<PR>.videocall-preview.hcl.internal`)
- [ ] Verify Harbor project ID is `89` or update references
- [ ] Confirm HCL K8s cluster name and access method
- [ ] Confirm DigitalOcean cluster name or plan new cluster

### Phase 0 (Foundation)
- [ ] `build_common.sh` created and tested
- [ ] All `cut_build_push_*.sh` scripts refactored
- [ ] Helm preview chart created
- [ ] Environment-specific values files created
- [ ] Helm dry-run passes

### Phase 1 (Opensource Build)
- [ ] Workflow file created
- [ ] Tested on draft PR
- [ ] Images visible in GHCR
- [ ] PR comments working

### Phase 2 (Opensource Cluster)
- [ ] Cluster created/verified
- [ ] PostgreSQL deployed
- [ ] NATS deployed
- [ ] ingress-nginx deployed
- [ ] cert-manager deployed
- [ ] Wildcard cert issued
- [ ] DNS configured
- [ ] Postgres secret created

### Phase 3 (Opensource Deploy)
- [ ] `DIGITALOCEAN_ACCESS_TOKEN` added
- [ ] Deploy workflow created
- [ ] Undeploy workflow created
- [ ] Cleanup workflow created
- [ ] End-to-end test passed
- [ ] Preview accessible and functional

### Phase 4 (HCL Runner)
- [ ] VM provisioned
- [ ] Docker installed
- [ ] GitHub CLI installed
- [ ] Helm installed
- [ ] kubectl installed
- [ ] Runner registered
- [ ] Runner service configured
- [ ] Test workflow passed

### Phase 5 (HCL Cluster)
- [ ] kubectl access verified
- [ ] PostgreSQL deployed
- [ ] NATS deployed
- [ ] ingress-nginx deployed
- [ ] cert-manager deployed
- [ ] TLS cert issued/configured
- [ ] DNS configured
- [ ] Harbor credentials configured
- [ ] kubectl configured on runner

### Phase 6 (HCL Build)
- [ ] Harbor secrets added
- [ ] Workflow file created (on github01)
- [ ] Tested on draft PR
- [ ] Images visible in Harbor
- [ ] Harbor retention configured
- [ ] PR comments working

### Phase 7 (HCL Deploy)
- [ ] kubeconfig configured
- [ ] Deploy workflow created (on github01)
- [ ] Undeploy workflow created (on github01)
- [ ] Cleanup workflow created (on github01)
- [ ] End-to-end test passed
- [ ] Preview accessible and functional

### Phase 8 (Polish)
- [ ] Reaper workflows added
- [ ] Documentation updated
- [ ] PR template updated
- [ ] Capacity monitoring added (optional)

---

## Troubleshooting

### Common Issues

**GHCR Push fails: "denied: installation not allowed to Create organization package"**

**Error Message**:
```
ERROR: failed to push ghcr.io/security-union/videocall-media-server:pr-625:
denied: installation not allowed to Create organization package
```

**Root Cause**: The GitHub organization `security-union` has not granted GitHub Actions permission to create packages in the organization namespace (`ghcr.io/security-union/`).

**Solution 1: Configure Organization Package Permissions (Recommended)**

An organization admin must enable GitHub Actions to create packages:

1. Navigate to: `https://github.com/organizations/security-union/settings/packages`
2. Under "Package creation", enable one of:
   - **"Actions can create packages in this organization"** (recommended)
   - Or configure granular permissions per repository
3. Save settings
4. Re-run the failed workflow

**Solution 2: Use Personal Namespace (Testing Only)**

For testing purposes, push to user namespace instead:

```yaml
# In .github/workflows/docker-build-check.yaml
tags: ghcr.io/jboyd01/videocall-media-server:pr-${{ github.event.pull_request.number }}
```

This works immediately but images live under user account, not organization.

**Solution 3: Use Personal Access Token**

Create a PAT with `write:packages` scope and add as repository secret:

1. Generate PAT: `https://github.com/settings/tokens/new`
   - Scope: `write:packages`
   - Optional: `read:org` for organization packages
2. Add secret to repository: `https://github.com/security-union/videocall-rs/settings/secrets/actions`
   - Name: `GHCR_TOKEN`
3. Update workflow login step:
   ```yaml
   - name: Login to GitHub Container Registry
     uses: docker/login-action@v3
     with:
       registry: ghcr.io
       username: ${{ github.actor }}
       password: ${{ secrets.GHCR_TOKEN }}  # Changed from GITHUB_TOKEN
   ```

**Verification**:

After configuration, verify permissions:
```bash
# Re-run workflow
gh run rerun <run-id> --repo security-union/videocall-rs

# Check packages page after successful push
# https://github.com/orgs/security-union/packages
```

---

**Build fails with "manifest unknown"**
- Verify registry URL format is correct
- Check Harbor project permissions (username has push access)
- Verify base images (`securityunion/actix-base`, `securityunion/yew-base`) are accessible

**Deployment fails with "ImagePullBackOff"**
- Create imagePullSecrets in preview namespace:
  ```bash
  kubectl create secret docker-registry harbor-registry \
    --docker-server=hclcr.io \
    --docker-username=$HARBOR_USER \
    --docker-password=$HARBOR_PASS \
    --namespace=preview-<PR>
  ```
- Add to Helm values:
  ```yaml
  global:
    imagePullSecrets:
      - name: harbor-registry
  ```

**DNS not resolving for preview domain**
- Verify ingress IP: `kubectl get ingress -n preview-<PR>`
- Check DNS propagation: `nslookup pr-<PR>.sandbox.videocall.rs`
- For HCL internal DNS, verify CoreDNS or internal DNS server configuration

**/deploy command doesn't trigger workflow**
- Verify comment author has correct association (MEMBER/OWNER/COLLABORATOR)
- Check GitHub Actions is enabled for the repository
- Verify workflow file is on the default branch (main/master)
- Check Actions logs for permission errors

**Capacity check always says "exceeded"**
- Verify namespace label selector: `kubectl get namespaces -l app=preview`
- Check for stale namespaces from failed deployments
- Run manual cleanup: `kubectl delete namespace preview-<PR>`

**Self-hosted runner not picking up jobs**
- Check runner status: `sudo ./svc.sh status`
- Verify runner labels match workflow `runs-on`
- Check runner logs: `sudo journalctl -u actions.runner.*`
- Verify GitHub Actions is enabled in GHES settings

---

## Cost Estimates

### Opensource (DigitalOcean)
- **K8s Cluster**: 2x s-2vcpu-4gb nodes = ~$48/month
- **GHCR Storage**: Free (public repos)
- **Ingress/LoadBalancer**: ~$12/month (DigitalOcean LB)
- **Total**: ~$60/month

### HCL Internal
- **Self-hosted runner**: VM cost (varies by HCL pricing)
- **Harbor storage**: Depends on HCL subscription (likely already available)
- **K8s cluster**: Existing cluster (no additional cost)
- **Total**: Minimal (infrastructure reuse)

---

## Success Metrics

- [ ] PRs can be previewed before merge in both environments
- [ ] Build time < 20 minutes for all 3 images
- [ ] Deploy time < 5 minutes from `/deploy` to accessible URL
- [ ] Capacity limit prevents cluster exhaustion
- [ ] Automatic cleanup keeps resource usage bounded
- [ ] Developer adoption: >50% of PRs use preview feature within first month

---

## Future Enhancements

1. **Multi-region previews** - Deploy to both NYC1 and Singapore
2. **Automatic deployment** - Deploy on every push (not just `/deploy` command)
3. **Preview links in PR description** - Bot updates PR body with preview URL
4. **Ephemeral databases** - Per-preview PostgreSQL instance (instead of shared)
5. **Preview analytics** - Track usage, performance metrics per preview
6. **Preview snapshots** - Save/restore preview state for testing
7. **A/B preview** - Deploy multiple versions (e.g., `pr-123-variant-a`)

---

## References

- Original Issue: https://github.com/security-union/videocall-rs/issues/571
- Previous Plan: `PR_PREVIEW_DEPLOYMENT_PLAN.md`
- Helm Charts: `helm/rustlemania-websocket/`, `helm/rustlemania-ui/`, `helm/meeting-api/`
- Existing Build Scripts: `cut_build_push_*.sh`
- Harbor Documentation: https://goharbor.io/docs/
- GitHub Actions Self-Hosted Runners: https://docs.github.com/en/actions/hosting-your-own-runners
