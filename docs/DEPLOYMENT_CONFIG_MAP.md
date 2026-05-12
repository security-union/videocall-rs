# Deployment Configuration Map — videocall-rs

> Where every kind of deployment configuration lives in the `labs-projects/videocall` repo, and which file to edit for which job.

Two layers — Kubernetes/Helm for cluster deployments, Docker Compose for local/dev — plus a runtime config injection layer for the Dioxus frontend.

---

## 1. Helm (production / per-cluster) — `helm/`

### Default values (shared, per-chart)

`helm/{chart-name}/values.yaml` — one per chart.

Charts in the repo:

| Chart | Purpose |
|---|---|
| `meeting-api` | Meeting REST API (auth, room mgmt) |
| `rustlemania-webtransport` | WT relay |
| `rustlemania-websocket` | WS relay |
| `videocall-ui` | Dioxus frontend |
| `videocall-website` / `website` | Marketing / homepage |
| `metrics-api` | Metrics ingestion endpoint |
| `postgres` | Database |
| `grafana` | Dashboards |
| `cert-manager` / `cert-manager-issuer` | TLS cert lifecycle |
| `external-dns` | DNS provisioning |
| `ingress-nginx` | Ingress controller |
| `digital-ocean-service-account` | DO RBAC |
| `engineering-vlog` | Static-site blog |

### Per-cluster overlays

Naming pattern: `helm/global/<region>/<chart>/values.yaml` overrides the default `helm/<chart>/values.yaml`.

| Cluster overlay | Path |
|---|---|
| US-East (production) | `helm/global/us-east/{chart}/values.yaml` |
| Singapore (production) | `helm/global/singapore/{chart}/values.yaml` |
| HCL (production) | `helm/global/hcl/{chart}/values.yaml` |
| HCL daily deploy | `helm/global/hcl-daily-deployment/{chart}/values.yaml` |

Not every chart has every overlay — only the ones that need cluster-specific overrides (different DNS, secrets, scaling, etc.).

---

## 2. Docker Compose — `docker/`

Local dev and CI stacks.

| File | Purpose |
|---|---|
| `docker/docker-compose.yaml` | Main local dev stack |
| `docker/docker-compose.e2e.yaml` | Playwright E2E stack (Dioxus UI on port 3001 + shared backend) |
| `docker/docker-compose.integration.yaml` | Integration test stack |
| `docker/.env-sample` | Env var template (copy to `.env` at repo root) |
| `docker/bot-config.yaml` | Synthetic-bot configuration |
| `docker/monitoring/prometheus/prometheus.yml` | Local Prometheus config |
| `docker/monitoring/prometheus/alert_rules.yml` | Local Prometheus alert rules |
| `docker/monitoring/grafana/dashboards/*.json` | Local Grafana dashboards |
| `docker/monitoring/grafana/provisioning/` | Local Grafana datasource + dashboard provisioning |
| `docker/Dockerfile.actix.dev` | Backend (actix-api) dev image |
| `docker/Dockerfile.dioxus.dev` | Frontend (Dioxus UI) dev image |
| `docker/Dockerfile.website` / `Dockerfile.website.dev` | Marketing site |
| `docker/Dockerfile.engineering-vlog` | Engineering vlog |
| `docker/Dockerfile.video-daemon` | Video daemon |
| `docker/start-dioxus.sh` | Dioxus container entrypoint |

---

## 3. Frontend runtime config — `dioxus-ui/scripts/`

Browser-side configuration injected at deploy time.

| File | Role |
|---|---|
| `dioxus-ui/scripts/config.js` | Committed default; baseline shape, e.g. `window.VIDEOCALL_CONFIG = { ... }` |
| `dioxus-ui/scripts/config.local.js.example` | Committed template — copy to `config.local.js` for local overrides |
| `dioxus-ui/scripts/config.local.js` | Local override (gitignored); injects env-specific URLs |
| `dioxus-ui/dist/config.js` | Trunk build artifact (output of `cp` step in `index.html` / Trunk config) |

Helm injects production URLs into one of these at deploy time per cluster.

---

## 4. Build-time config

| File | Purpose |
|---|---|
| `.cargo/config.toml` (root + per-crate) | Cargo config (target dirs, lints) |
| `.env` (root, gitignored) | Local dev env vars (created from `docker/.env-sample`) |
| `engineering-vlog/config.toml` | Zola static-site config |
| `leptos-website/.envrc` | direnv config for Leptos site |

---

## 5. CI / GitHub Actions

| Path | Status |
|---|---|
| `.github/workflows/` | Active CI workflows (PR checks, deploys) |
| `.github/oss-workflows/` | Active OSS-mirror workflows (e.g. Dioxus UI Docker Hub upload) |
| `.github/workflows-opensource/` | Archived (not in current use) |

---

## Quick map: "I need to change X — where do I edit?"

| Need to change | Edit |
|---|---|
| Production env var on US-East WT relay | `helm/global/us-east/webtransport/values.yaml` (overrides `helm/rustlemania-webtransport/values.yaml`) |
| Prometheus alert rule | `helm/global/{us-east,hcl,hcl-daily-deployment}/prometheus/values.yaml` |
| Grafana dashboard | `helm/grafana/dashboards/*.json` + `helm/grafana/templates/dashboards-configmap.yaml` |
| Local dev stack composition | `docker/docker-compose.yaml` + `docker/.env-sample` → `.env` |
| Frontend runtime URL injection | `dioxus-ui/scripts/config.js` (committed default) or `config.local.js` (local override) |
| Add a new per-service default | `helm/{chart}/values.yaml` |
| Local Grafana dashboard for dev | `docker/monitoring/grafana/dashboards/*.json` |
| Bot configuration | `docker/bot-config.yaml` (local) / helm equivalent for production |
| TLS / cert issuer | `helm/cert-manager-issuer/values.yaml` |
| Ingress rules (per cluster) | `helm/global/{cluster}/ingress-nginx/values.yaml` |
| Synthetic-bot deployment | `helm/{...}/values.yaml` (bot has no top-level helm chart yet) + `docker/bot-config.yaml` |

---

## Notes / gotchas

- **Helm overlays are sparse.** If a cluster overlay doesn't have a `values.yaml` for a particular chart, the default at `helm/<chart>/values.yaml` is used as-is. Don't assume every cluster has every chart.
- **The `hcl` overlay had no `prometheus/` subchart** until PR #716 added one. If you're adding a new overlay for a new service, you may need to create the directory structure too.
- **`config.local.js` is the deploy-time injection point** for the Dioxus frontend. Missing or malformed `config.local.js` produces a runtime `SyntaxError: Unexpected token '<'` in the browser console because the frontend falls back to fetching the index page as JS. This is what bit E2E in #730 / was fixed in #741.
- **`WT_OUTBOUND_CHANNEL_CAPACITY`** (env var, read at startup) is resolved from `WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT` in `actix-api/src/constants.rs:85`, currently `4096`. After PR #706 landed, the env override in helm is redundant but harmless; the code default is the source of truth.
- **Alertmanager is not wired up across overlays.** `helm/global/us-east/prometheus/values.yaml` sets `alertmanager.enabled: false` explicitly; the `hcl` and `hcl-daily-deployment` overlays don't configure the key (rely on chart default). Either way, no Alertmanager routes or receivers are configured, so alerts evaluate and appear in Prometheus' `/alerts` UI but do NOT page anyone. Tracked in issue #729.

---

*Generated 2026-05-12 from a survey of the `labs-projects/videocall` repo on the `PR-staging` branch.*
