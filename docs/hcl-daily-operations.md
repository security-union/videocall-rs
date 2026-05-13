# HCL Daily — Cluster & CI Operations Guide

This guide covers physical infrastructure, cluster access, application architecture, deployment, E2E testing, monitoring, and meeting analysis workflows for the HCL daily videocall environment.

## 1. Physical Infrastructure

### VMs (QuickStart — qs.hcllabs.net/servers)

| Hostname | Role | Specs | Runner labels | SSH |
|----------|------|-------|---------------|-----|
| `videocall.videocall.fnxlabs.com` | K3s cluster + daily builds/deploys | ec2 VM |  | `ssh videocall.videocall.fnxlabs.com` |
| `videocallci.fnxlabs.com` | CI checks + E2E tests | AWS c7a.2xlarge (8 vCPU, 16 GB RAM) | `[self-hosted, linux, x64, hcl-ci]` | `ssh videocallci.fnxlabs.com` |
| `videocall-qs-jenkins-agent-ubuntu` | QuickStart pod that runs github hosted | `[self-hosted, linux, docker]`| use QuickStart UI to exec into the pod|

Your SSH public key has been added to both videocall.videocall.fnxlabs.com and videocallci.fnxlabs.com.

### What runs where

**videocall.videocall.fnxlabs.com** (K3s VM):
- K3s cluster hosting all videocall services (the `videocall` namespace)

**videocall-qs-jenkins-agent-ubuntu** (QS github action runner)
- Daily image builds (Docker builds, Harbor push)
- Daily deployments (Helm upgrades to K3s)
- PR image builds and PR preview deploys
- GitHub Actions runner with label `[self-hosted, linux, docker]`
- Has `kubectl` access to the K3s cluster and `docker` for builds

**videocallci.fnxlabs.com** (AWS — dedicated CI runner):
- PR checks only: Rust `cargo check`/`clippy`/`fmt`, Dioxus UI WASM check, E2E lint, style token checks
- E2E tests: spins up a Docker Compose stack, runs Playwright against it
- 4 runner instances (concurrent job capacity): `videocallci`, `videocallci-2`, `videocallci-3`, `videocallci-4`
- Does NOT deploy to the cluster or build production images
- OS: RHEL9, toolchain: Rust stable + wasm32 target, Node.js 22, Chromium (EPEL)

### Accessing the VMs

```bash
# Daily build / K3s host
ssh testrada@videocall.videocall.fnxlabs.com

# CI runner (E2E tests, PR checks)
ssh testrada@videocallci.fnxlabs.com
# sudo is available (NOPASSWD) and required to interact with docker
```

On `videocallci`, useful paths:
- Runner work dirs: `/var/lib/ghrunner/_work`, `/var/lib/ghrunner/_work-2`, etc.
- Rust toolchain: `/var/lib/ci/cargo/bin/`, `/var/lib/ci/rustup/`
- Runner services: `systemctl status actions.runner._services.videocallci.service`

## 2. Cluster Access

### Prerequisites

| Tool | Install | Purpose |
|------|---------|---------|
| `kubectl` | `brew install kubectl` | Cluster management |
| `helm` | `brew install helm` | Deploy/upgrade charts |
| `jq` | `brew install jq` | Parse JSON output |
| `k9s` | `brew install derailed/k9s/k9s` | Terminal UI for K8s (highly recommended) |
| `stern` | `brew install stern` | Multi-pod log tailing |

### Setup

1. Save `hcl-daily-tony-kubeconfig.yaml` to `~/.kube/hcl-daily.yaml`

2. Set the environment variable:

```bash
export KUBECONFIG=~/.kube/hcl-daily.yaml
```

Add to `~/.zshrc` to make permanent.

3. Test connectivity:

```bash
kubectl get pods -n videocall
```

### Namespaces you have access to

| Namespace | What's there |
|-----------|-------------|
| `videocall` | Main app, Prometheus, Grafana, NATS, Postgres |
| `preview-infra` | Shared infra for PR preview environments |
| `preview-slot-1` through `preview-slot-6` | PR sandbox deployments |
| `speedtest` | Speed testing tools |

You can manage workloads (pods, deployments, services, ingresses, configmaps, secrets, etc.), view logs, exec into pods, port-forward, and manage Helm releases within these namespaces.

You do **not** have access to cluster-level resources (nodes, namespaces, CRDs, RBAC) or system namespaces (kube-system, cert-manager).

### Common kubectl commands

```bash
# Switch namespace context
kubectl config set-context --current --namespace=videocall

# List all pods
kubectl get pods -n videocall

# Describe a pod (events, status, restarts)
kubectl describe pod <pod-name> -n videocall

# List deployments, services, ingresses
kubectl get deployments,svc,ingress -n videocall

# Restart a deployment (rolling restart)
kubectl rollout restart deployment/<name> -n videocall

# Exec into a pod
kubectl exec -it -n videocall deploy/<name> -- /bin/sh
```

## 3. Application Architecture

The videocall application runs as several microservices:

```
┌─────────────────────────────────────────────────────────────────────┐
│                        videocall namespace                           │
│                                                                     │
│  ┌──────────────┐   ┌──────────────┐   ┌───────────────────────┐  │
│  │  dioxus-ui   │   │ meeting-api  │   │  websocket-server     │  │
│  │  (frontend)  │   │  (REST API)  │   │  (media relay - WS)   │  │
│  └──────────────┘   └──────────────┘   └───────────────────────┘  │
│                            │                       │                │
│                            │              ┌────────────────────┐   │
│                            │              │ webtransport-server │   │
│                            │              │ (media relay - WT)  │   │
│                            │              └────────────────────┘   │
│                            │                       │                │
│  ┌──────────────┐   ┌─────┴──────┐   ┌───────────┴───────────┐  │
│  │  metrics-api │   │  postgres   │   │        NATS           │  │
│  │  (telemetry) │   │  (database) │   │  (pub/sub messaging)  │  │
│  └──────────────┘   └────────────┘   └───────────────────────┘  │
│                                                                     │
│  ┌──────────────┐   ┌──────────────┐                               │
│  │  prometheus  │   │   grafana    │                               │
│  │  (metrics)   │   │ (dashboards) │                               │
│  └──────────────┘   └──────────────┘                               │
└─────────────────────────────────────────────────────────────────────┘
```

### Service endpoints (production)

| Service | External URL | Internal service |
|---------|-------------|-----------------|
| Dioxus UI | https://app.videocall.fnxlabs.com | `videocall-dioxus-ui:80` |
| Meeting API | https://api.videocall.fnxlabs.com | `videocall-api:8081` |
| WebSocket relay | wss://websocket.videocall.fnxlabs.com | `videocall-websocket:8080` |
| WebTransport relay | https://webtransport.videocall.fnxlabs.com:4433 | `videocall-webtransport:4433` |
| Prometheus | https://prometheus.videocall.fnxlabs.com | `prometheus-server:80` |
| Grafana | https://grafana.videocall.fnxlabs.com | `grafana:80` |

### Key environment variables (set per-service via Helm)

- `JWT_SECRET` — shared across meeting-api, websocket, and webtransport (K8s secret `jwt-secret`)
- `NATS_URL` — inter-service messaging (typically `nats://nats:4222`). Required by **all** services: meeting-api (broadcast features: mute, rename, waiting room, host-leave), websocket, webtransport, and metrics-api. Without it, meeting-api silently degrades — no error, just disabled push notifications.
- `DATABASE_URL` — Postgres connection string (meeting-api only)
- `RUST_LOG` — log verbosity (`warn` for relays, `info` for meeting-api)
- `TOKEN_TTL_SECS` — JWT token lifetime (currently `86400` = 24h)

## 4. Helm Charts & Deployment

### Chart structure

The Helm charts live in `helm/` at the repo root:

| Chart | Path | Deploys |
|-------|------|---------|
| `meeting-api` | `helm/meeting-api/` | REST API (auth, sessions, OAuth) |
| `rustlemania-websocket` | `helm/rustlemania-websocket/` | WebSocket media relay |
| `rustlemania-webtransport` | `helm/rustlemania-webtransport/` | WebTransport media relay (QUIC/HTTP3) |
| `videocall-ui` | `helm/videocall-ui/` | Dioxus frontend (static WASM app) |
| `metrics-api` | `helm/metrics-api/` | Client/server metrics collector |
| `grafana` | `helm/grafana/` | Grafana with dashboard configmaps |

Each chart has a `values.yaml` with generic defaults (open-source defaults like `securityunion/*` images, `.videocall.rs` domains). **These are not what runs in production.**

### How deployment works (preferred method)

The preferred way to deploy to HCL daily is via GitHub Actions:

1. **Go to**: github01 → Actions → **"Daily Build Images (HCL)"**
2. **Run workflow** on `hcl-main` branch
3. This builds all Docker images, pushes them to Harbor with a tag like `daily-2026-05-12-0954`
4. On success, it automatically triggers **"Daily Deploy to Production (HCL)"**
5. The deploy workflow runs `helm upgrade --install` for each service on the K3s cluster

This is a two-stage pipeline: **Build → Deploy**. The deploy only fires if the build succeeds. Both run on the `videocall-qs-jenkins-agent-ubuntu` QuickStart runner (label `[self-hosted, linux, docker]`).

> **Note**: Despite the name "Production (HCL)", this deploys to the **hcl-daily** environment (`app.videocall.fnxlabs.com`), not a customer-facing production system.

### What the deploy workflow does

The deploy workflow (`.github/workflows/daily-deploy-hcl.yaml`) uses `helm upgrade --install` with extensive `--set` overrides. **The workflow file is the source of truth for what's actually deployed** — not the chart `values.yaml` files.

Key overrides include:

- **Image registry/tag**: `--set image.repository=hclcr.io/hcllabs/<service> --set image.tag=daily-YYYY-MM-DD-HHMM`
- **Domains**: `--set ingress.hosts[0].host=<service>.videocall.fnxlabs.com`
- **Secrets**: `--set env[N].valueFrom.secretKeyRef.name=...`
- **Feature flags**: `--set runtimeConfig.consoleLogUploadEnabled=true`

### Inspecting what's deployed

```bash
# List all Helm releases
helm list -n videocall

# See the values used for a specific release (shows the --set overrides)
helm get values videocall-dioxus-ui -n videocall

# See the full rendered manifest
helm get manifest videocall-api -n videocall

# Check deployment history (rollback targets)
helm history videocall-websocket -n videocall
```

### Image registry

Images are built daily from `hcl-main` and pushed to Harbor:
- Registry: `hclcr.io/hcllabs/`
- Tag format: `daily-YYYY-MM-DD-HHMM`
- Relevant images: `videocall-media-server`, `videocall-meeting-api`, `videocall-dioxus-ui`

### Deploying a specific tag (manual trigger)

If you need to deploy a specific image tag without rebuilding:

```bash
# Go to: github01 → Actions → "Daily Deploy to Production (HCL)" → Run workflow
# Enter the tag (e.g., daily-2026-05-12-0954) and run
```

Direct Helm commands are possible but discouraged — the workflow has many `--set` flags that are easy to miss. Only use direct Helm in emergencies and copy the exact flags from the workflow file.

## 5. E2E Tests (Playwright)

End-to-end tests run browser-based scenarios against the Dioxus UI. They live in `e2e/` and use Playwright with Chromium.

### How E2E runs in CI

The workflow `.github/workflows/e2e-hcl.yaml` runs on `videocallci.fnxlabs.com` (label: `[self-hosted, linux, x64, hcl-ci]`). It:

1. Checks out the branch
2. Installs Node.js 22 dependencies + Playwright Chromium
3. Builds and starts a Docker Compose stack (`docker/docker-compose.e2e.yaml`)
4. Waits for services to be healthy (dioxus-ui on :3001, meeting-api on :8081, websocket on :8080)
5. Runs Playwright tests (2 workers, headless Chromium)
6. Pushes pass/fail metrics to Pushgateway → Prometheus → Grafana scoreboard
7. Posts results as a PR comment (if triggered via `/run-e2e`)
8. Notifies Google Chat on failures (push to `hcl-main` or `PR-staging`)

### Triggers

- **Automatic**: push to `hcl-main` or `PR-staging`
- **Manual**: comment `/run-e2e` on any PR (requires write access)
- **Workflow dispatch**: Actions UI → select branch, optional `--grep` filter

### Running E2E locally

```bash
# 1. Install dependencies (first time only)
make e2e-install

# 2. Build the Docker stack images
make e2e-build

# 3. Start the stack (postgres, nats, meeting-api, websocket-api, dioxus-ui)
make e2e-up

# 4. Wait ~60s for dioxus-ui to compile and become ready
#    Check with: curl -s http://localhost:3001 | head -5

# 5. Run all tests
make e2e

# 6. Run a single spec file
make e2e SPEC=two-users-meeting

# 7. Run in headed mode (see the browser)
cd e2e && npx playwright test --headed

# 8. Run with Playwright UI (interactive debugging)
cd e2e && npx playwright test --ui

# 9. Tear down
make e2e-down
```

### E2E stack architecture

The Docker Compose stack (`docker/docker-compose.e2e.yaml`) spins up a minimal self-contained environment:

| Service | Port | Notes |
|---------|------|-------|
| `postgres` | 5432 | Fresh DB, auto-migrated by meeting-api on startup |
| `nats` | 4222 | JetStream enabled |
| `meeting-api` | 8081 | Auth bypassed via JWT cookie injection (no OAuth) |
| `websocket-api` | 8080 | Media relay (WebSocket mode) |
| `dioxus-ui` | 3001 | WASM frontend (built from source in the container) |

Auth is bypassed in E2E by injecting a JWT session cookie directly (see `e2e/helpers/auth.ts`). No Google OAuth flow needed.

### Configuration

`e2e/playwright.config.ts`:
- `workers: 2` — capped to avoid CPU saturation on CI (8 vCPU shared with Docker stack)
- `timeout: 60_000` — per-test timeout (60s)
- `expect.timeout: 10_000` — assertion timeout (10s)
- Chrome launched with fake media devices (no real camera/mic needed)

### Viewing results

- **CI**: check the workflow run in GitHub Actions, or the PR comment
- **Scoreboard**: https://grafana.videocall.fnxlabs.com/public-dashboards/2dd04ba37cf144f1862941620daa6349?orgId=1&from=now-24h&to=now
- **Local**: `npx playwright show-report` (opens HTML report in browser)

### Debugging a failing test on CI

```bash
# SSH to the CI runner
ssh videocallci.fnxlabs.com

# Check if a Docker stack is still running (from a failed job)
docker ps | grep videocall-e2e

# Clean up leftover stacks
docker compose -p videocall-e2e -f docker/docker-compose.e2e.yaml down -v

# Check runner logs
journalctl -u actions.runner._services.videocallci.service --since "1 hour ago" | tail -50
```

### Writing new E2E tests

Key patterns:
- Tests use `e2e/helpers/auth.ts` to inject a session cookie (auto-join, no login page)
- Use `page.getByRole()` and `page.getByTestId()` for selectors (resilient to style changes)
- Multi-user tests create multiple browser contexts within one test
- After writing tests, run lint: `cd e2e && npx prettier --write tests/<file> && npx eslint tests/<file>`

## 6. Viewing Logs

### Pod logs (application output)

```bash
# Follow logs for a deployment (latest pod)
kubectl logs -n videocall deploy/videocall-websocket -f --tail=100

# All pods for a service (if multiple replicas)
kubectl logs -n videocall -l app.kubernetes.io/name=videocall-websocket --tail=50

# Logs from a crashed/restarted pod (previous container)
kubectl logs -n videocall deploy/videocall-api --previous

# stern: tail multiple pods at once with color coding
stern -n videocall "videocall-websocket.*" --tail=50
stern -n videocall "videocall-api.*" --tail=50
```

### What to look for in relay logs

- `RUST_LOG=info` on websocket server → connection events, room joins/leaves
- `RUST_LOG=warn` on webtransport → only errors/warnings (verbose at info)
- Look for: `new_connection`, `disconnected`, `room_created`, `session_expired`

### Console logs (client-side browser logs uploaded to meeting-api)

The Dioxus UI uploads structured JSON console logs to the meeting-api when `consoleLogUploadEnabled=true`. These are stored in a PVC mounted on the meeting-api pod.

```bash
# List console log files
kubectl exec -n videocall deploy/videocall-api -- ls /data/console-logs/

# Copy logs locally for analysis
kubectl cp videocall/$(kubectl get pod -n videocall -l app.kubernetes.io/name=videocall-api -o jsonpath='{.items[0].metadata.name}'):/data/console-logs/ ./console-logs/

# Or view a specific log directly
kubectl exec -n videocall deploy/videocall-api -- cat /data/console-logs/<filename>.json | jq .
```

Console log filenames contain the meeting ID and peer session. They include:
- Transport type (WebSocket vs WebTransport)
- RTT measurements
- Server elections and re-elections
- Codec events and errors
- Client preamble (hardware specs: CPU cores, memory, OS, screen resolution)

### Quick log analysis

```bash
# Check for recent errors across all services
stern -n videocall "" --tail=200 | grep -iE "error|panic|fatal"

# Check meeting-api for auth failures
kubectl logs -n videocall deploy/videocall-api --tail=500 | grep -i "unauthorized\|forbidden\|expired"

# Check WebSocket relay for connection issues
kubectl logs -n videocall deploy/videocall-websocket --tail=500 | grep -i "disconnect\|timeout\|error"
```

## 7. Prometheus

### Access

https://prometheus.videocall.fnxlabs.com (no login required)

### Key metrics

| Metric | What it measures |
|--------|-----------------|
| `videocall_active_sessions` | Current number of active meeting sessions |
| `videocall_outbound_channel_drops_total` | Packets dropped by relay (saturation signal) |
| `videocall_datagram_drops_total` | Client-side upstream datagram drops |
| `videocall_client_memory_used_bytes` | Client heap size (leak detection) |
| `e2e_tests_passed` | E2E test pass count per branch |
| `e2e_tests_failed` | E2E test failure count per branch |
| `e2e_run_duration_seconds` | E2E suite wall-clock time |

### Useful queries

```promql
# Active sessions right now
videocall_active_sessions

# Relay packet drop rate (per transport, per kind)
sum by (transport, kind) (rate(videocall_outbound_channel_drops_total[5m]))

# Client memory growth ratio (leak detection)
videocall_client_memory_used_bytes / on(meeting_id, session_id) first_over_time(videocall_client_memory_used_bytes[5m])

# Server stats reporting (confirms relay is alive)
rate(videocall_server_stats_reports_total[1m])
```

### Scrape interval

Prometheus scrapes every **15 seconds**. When building Grafana panels, set `$__interval` / minStep to `15s` to avoid visual gaps.

### Alerts

Alerts are defined in `helm/global/hcl-daily-deployment/prometheus/values.yaml`. Current active alerts:
- `ClientHeapGrowthHigh` — client memory >2x initial value
- `ClientUpstreamDropsActive` — client dropping upstream datagrams
- `RelayOutboundSaturating` — relay dropping >5 packets/sec

## 8. Grafana

### Access

https://grafana.videocall.fnxlabs.com (ask Jay for credentials)

### Key dashboards

| Dashboard | Purpose |
|-----------|---------|
| E2E Test Scoreboard | CI pass/fail rates, test counts, duration trends |
| Videocall Overview | Active sessions, relay health, connection stats |

The E2E scoreboard is also available as a public dashboard (no login required):
https://grafana.videocall.fnxlabs.com/public-dashboards/2dd04ba37cf144f1862941620daa6349?orgId=1&from=now-24h&to=now

### Dashboard provisioning

Dashboards are defined as JSON files in `helm/grafana/dashboards/` and loaded via a ConfigMap (`helm/grafana/templates/dashboards-configmap.yaml`). To add or modify dashboards, edit the JSON and redeploy the Grafana Helm chart.

## 9. Meeting Analysis Workflow

When investigating meeting quality issues (audio drops, video freezes, disconnections), follow this process:

### Step 1: Collect console logs

```bash
# Copy all console logs locally
kubectl cp videocall/$(kubectl get pod -n videocall -l app.kubernetes.io/name=videocall-api -o jsonpath='{.items[0].metadata.name}'):/data/console-logs/ ./meeting-logs/

# Filter to a specific meeting/date
ls ./meeting-logs/ | grep "2026-05-12"
```

### Step 2: Run the analysis script

The repo includes a parser that produces a 9-second summary:

```bash
scripts/parse_meeting_console_logs.sh ./meeting-logs/<meeting-dir>/
```

This outputs:
- Transport type per participant (WS vs WT)
- RTT baseline and anomalies
- Server election/re-election timeline
- Implausible RTT discards
- Peer-ID to email/display-name mapping
- Copy-paste Prometheus queries for that time range

### Step 3: Check the preamble

Every client's first log chunk has a `"level":"preamble"` entry with full hardware specs:

```bash
grep -l '"level":"preamble"' ./meeting-logs/<meeting-dir>/*.json | head -5
cat <file> | jq 'select(.level == "preamble")'
```

This reveals CPU cores, memory, OS, screen resolution. Always check this BEFORE theorizing about issues — a 2-core machine will behave very differently than an 8-core one.

### Step 4: Cross-reference with Prometheus

Using the time range from the console logs, query Prometheus at https://prometheus.videocall.fnxlabs.com:

Query for the meeting time window:
- `videocall_outbound_channel_drops_total` — was the relay dropping packets?
- `videocall_active_sessions` — how many sessions were active (load)?
- Container CPU/memory — was the relay saturated?

### Step 5: Check relay pod logs for the time window

```bash
# Get logs from a specific time range (stern supports --since)
stern -n videocall "videocall-websocket.*" --since=1h | grep -i "error\|disconnect"

# Or kubectl with timestamps
kubectl logs -n videocall deploy/videocall-websocket --since=2h --timestamps | grep "2026-05-12T16:"
```

### Common patterns to look for

| Symptom | Likely cause | Where to check |
|---------|-------------|----------------|
| "56 years ago" timestamps | Epoch 0 / unset timestamp | Console log preamble |
| Frequent re-elections | High RTT / packet loss | Console logs RTT section |
| Audio cuts out | Relay saturation or client drops | `outbound_channel_drops_total` |
| Video freeze | Keyframe request storm (WT bug #814) | Relay logs for PLI flood |
| Complete disconnection | JWT expiry or transport failure | Meeting-api auth logs |
| One participant bad, others fine | Client hardware (check preamble) | Preamble cores/memory |

## 10. Troubleshooting

### Pod not starting

```bash
kubectl describe pod <pod-name> -n videocall
# Check Events section at the bottom for: ImagePullBackOff, CrashLoopBackOff, OOMKilled
```

### ImagePullBackOff

The Harbor pull secret may be expired or the image tag doesn't exist:

```bash
kubectl get secret harbor-pull-secret -n videocall -o jsonpath='{.data.\.dockerconfigjson}' | base64 -d | jq .
```

### Service unreachable

```bash
# Check ingress
kubectl get ingress -n videocall

# Check endpoints (are pods actually backing the service?)
kubectl get endpoints -n videocall

# Test internal connectivity
kubectl run test-curl --rm -it --image=curlimages/curl -- curl http://videocall-api:8081/health
```

### Connection errors

- **"Forbidden" from kubectl** — you're accessing a namespace or resource outside your permissions
- **Connection timeout** — check VPN connectivity to `videocall.videocall.fnxlabs.com:8443`
- **Certificate errors** — the CA is embedded in your kubeconfig; don't modify the file

### Checking what version is deployed

```bash
# Check the image tag on a deployment
kubectl get deploy videocall-dioxus-ui -n videocall -o jsonpath='{.spec.template.spec.containers[0].image}'

# Or hit the version endpoint
curl -s https://app.videocall.fnxlabs.com/version.json | jq .
curl -s https://api.videocall.fnxlabs.com/version | jq .
```
