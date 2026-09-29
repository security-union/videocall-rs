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
| `videocall-website` | Marketing / homepage — **canonical chart**. This is the one the deploy pipeline ships: `scripts/deploy-global-infrastructure.sh` deploys `global/us-east/videocall-website` (the only website overlay that exists), image tag `latest`, service `videocall-website`, with HPA support. |
| `website` | Marketing / homepage — **legacy, not deployed**. Separate chart targeting the same hosts (`videocall.rs` / `www.videocall.rs`) and image as `videocall-website`, but referenced by no deploy script or overlay; its templates still use stale `webtransport-rs.*` helpers (a copy of the WT relay chart). Use `videocall-website`; this one is a cleanup candidate. |
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
| HCL — read only for Grafana, by the hcl-daily deploy | `helm/global/hcl/{chart}/values.yaml` |
| HCL daily deploy | `helm/global/hcl-daily-deployment/{chart}/values.yaml` |

The `hcl` row is not a cluster of its own. `daily-deploy-hcl.yaml` targets hcl-daily and reads `helm/global/hcl/grafana/values.yaml` for Grafana while reading `hcl-daily-deployment/` for everything else; `helm/global/hcl/prometheus/values.yaml` has no consumer at all. See the Prometheus notes under "Notes / gotchas" before editing either.

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
`helm/videocall-ui/templates/configmap-configjs.yaml` renders the file from
`.Values.runtimeConfig`, so any key set there reaches `window.__APP_CONFIG`.

### Runtime rollback levers

Keys read at page load, changeable per environment with no rebuild. The client's
`RuntimeConfig` does not reject unknown keys, so an environment that omits one
behaves as the default says.

| Key | Default | Effect when set |
|---|---|---|
| `wtReceiveWorker` | ON | `"0"`, `"false"`, `"off"` or `"no"` runs the WebTransport session on the main thread instead of in its dedicated Worker (issue #2728). The Worker exists so a main-thread stall cannot starve the receive path; turning it off restores the pre-#2728 behaviour, including the audio-lane read starvation that stall caused. Applies to WebTransport only; WebSocket is unaffected. |
| `defaultTransport` | `"webtransport"` | `"websocket"` hands every user who has NOT chosen a protocol in Settings a WebSocket-only connection, which is the pre-#2711 behaviour. Trimmed and case-insensitive; absent, empty or unrecognised leaves the compiled default in place, and an unrecognised value logs a `warn!` so a rollback that did not take is visible. This is the cluster-wide rollback for the epic #2711 default flip. It does NOT override a user who picked a protocol: precedence is stored user preference, then this key, then the compiled default. Users already carrying a sticky or session pin keep it until they clear it in Settings. **Set this to `"websocket"` whenever you set `webTransportEnabled: "false"`** — the two are independent keys, and leaving the default on WebTransport there means every unseeded user's RESOLVED preference is WebTransport (nothing is stored) while the client runs WebSocket. The Settings panel marks WebSocket as the default and shows the WebTransport option as unavailable on a `webTransportEnabled: "false"` deployment whatever this key says, so the UI never names a protocol the cluster cannot use. |

`window.__VC_WT_RECEIVE_WORKER` is the same switch as a devtools/e2e override and
takes precedence over the config key. It is not an operator interface.

#### How to actually set one, per cluster

The clusters do not configure `runtimeConfig` the same way, so there is no single
command. Setting the key in a values file only works where a values file is used.

**us-east** — `helm/global/us-east/videocall-ui/values.yaml` carries a
`runtimeConfig:` block. Add the key there and re-run that cluster's deploy:

```yaml
runtimeConfig:
  wtReceiveWorker: "0"
  defaultTransport: "websocket"
```

**HCL and ascend** — `daily-deploy-hcl.yaml` and `daily-deploy-ascend.yaml` pass
every `runtimeConfig.*` key inline as `--set-string` with no `-f` values file, so
the durable change is a workflow edit plus a deploy run. Add to the
`helm upgrade --install videocall-dioxus-ui` step:

```
--set-string "runtimeConfig.wtReceiveWorker=0"
--set-string "runtimeConfig.defaultTransport=websocket"
```

Out of band on those two clusters, `--reuse-values` is MANDATORY:

```bash
helm upgrade --install videocall-dioxus-ui helm/videocall-ui/ -n <namespace> \
  --reuse-values --set-string "runtimeConfig.defaultTransport=websocket"
```

Without it, helm falls back to `helm/videocall-ui/values.yaml`, which resets every
other `runtimeConfig` key to the chart defaults and points the UI at
`app.videocall.rs`. The rollback lever becomes an outage.

Editing one daily-deploy workflow does not trip
`scripts/check-deploy-workflow-parity.sh`: it compares step NAMES, not step
bodies, so a single-cluster rollback is safe from that guard.

Flipping the key rolls the pods. `helm/videocall-ui/templates/deployment.yaml`
hashes the config ConfigMap into `checksum/config`, so the change is a rolling
restart plus a client reload, not a live flip.

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
| WT relay Service traffic policy / client affinity | `helm/rustlemania-webtransport/values.yaml` under `service:`, overridden per cluster in `helm/global/*/webtransport/values.yaml` and by `--set` in the deploy workflows |

---

## Notes / gotchas

- **Helm overlays are sparse.** If a cluster overlay doesn't have a `values.yaml` for a particular chart, the default at `helm/<chart>/values.yaml` is used as-is. Don't assume every cluster has every chart.
- **The `hcl` overlay had no `prometheus/` subchart** until PR #716 added one. If you're adding a new overlay for a new service, you may need to create the directory structure too.
- **`config.local.js` is the deploy-time injection point** for the Dioxus frontend. Missing or malformed `config.local.js` produces a runtime `SyntaxError: Unexpected token '<'` in the browser console because the frontend falls back to fetching the index page as JS. This is what bit E2E in #730 / was fixed in #741.
- **`WT_OUTBOUND_CHANNEL_CAPACITY`** (env var, read at startup) is resolved from `WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT` in `actix-api/src/constants.rs`, currently `1024` (#2717; lowered from `4096` for fail-fast behaviour per issue #979 — a deep queue only buffers stale, useless video for slow receivers, and the camera/screen backlog is now bounded in BYTES rather than by a shallower slot count). The env override in helm is redundant but harmless; the code default is the source of truth. Raise it only for exceptional workloads.
- **`WT_AUDIO_DOWNLINK_LANE`** (env var, read once at startup) selects which QUIC primitive carries downlink audio to a `ds=1` WebTransport receiver (#2724). `reliable` (the default) gives audio a dedicated reliable stream, so loss is recovered in about one RTT instead of concealed and the recurring sub-second hitches of #1878 stop producing gaps. A multi-second stall is still heard as a gap: NetEq caps its adaptive target at 300 ms and flushes to live above 1000 ms, so it discards the backlog the reliable stream delivered. The guarantee this buys is in-order, complete delivery at the transport. `datagram` restores the pre-#2724 route for every receiver on that relay. Unset or unrecognised falls back to `reliable`, so a typo cannot silently re-enable the lossy path. There is NO hot reload: changing it needs a relay restart. A legacy (pre-#2723, no `ds`) receiver keeps datagram audio whatever this is set to — it has one reliable stream and it carries video. Confirm which way a cluster is running with `videocall_relay_wt_audio_downlink_sessions_total{lane,mode}`; `mode` separates a legacy build from a `ds=1` receiver on a reverted cluster, which both book `lane="datagram"` (#2763).

- **`WT_SESSION_ARBITERS`** (env var, read once at startup) sets how many session arbiters the WebTransport relay shards its connections across (#2727). Leave it UNSET on every cluster: the default is `available_parallelism()`, which is what each one wants. Past one arbiter the relay builds a multi-threaded tokio runtime, so quinn's endpoint and every per-connection driver — QUIC encryption, packetization, UDP I/O — spread across the worker pool, while each session's `WtChatSession` actor, bridge readers and writers, #2723 downlink lanes, #2726 escalation state and #2717 byte meters stay together on the one arbiter that owns that session. Values are clamped to 1..=64; unset, zero and unparsable all fall back to the default. There is NO hot reload — changing it needs a relay restart. The startup log line `Session sharding: N arbiter(s) (M probed), W tokio worker(s)` reports what a running relay chose.

- **`TOKIO_WORKER_THREADS` must stay unset, because the shipped relay ignores it and a build that did not would die on a bad value.** It was inert before #2727 only because the relay ran a current-thread runtime, which never asks tokio how many cores it has. The sharded relay is multi-threaded, and there tokio reads this variable and PANICS on a value that is zero, unparsable or not valid unicode — a crash at startup, on a chart with one replica and no second pod to serve traffic. The relay passes its worker count explicitly, which is what makes tokio ignore the variable; that promise is executed by a test, not asserted. Use `WT_SESSION_ARBITERS` to change the thread budget.

- **Rolling back the sharding, helm-natively.** `WT_SESSION_ARBITERS=1` restores the pre-#2727 relay — the same current-thread runtime and the same placement — but it is in no chart env array, so setting it means a `kubectl set env` that the next `helm upgrade` silently reverts. The durable lever is the CPU limit: put `resources.limits.cpu` back to `1500m` on hcl-daily or labsworkspace and `available_parallelism()` floors to one core, which disables sharding in a line the chart owns. Expect `scripts/check_deploy_env_parity.py` to fail on the 2000m floor; that failure is the guard telling you this is a deliberate rollback rather than a typo.

- **A wedge on ANY relay runtime now takes the pod out.** `/healthz` reads the oldest heartbeat across the main runtime and every arbiter, so one stuck runtime of N+1 fails readiness and liveness and restarts the pod, dropping every session on every arbiter, not just the wedged one. That is the intended design — a wedged arbiter serves nobody and silently accepts more sessions otherwise — but sharding multiplies how reachable that state is by the number of runtimes. Placement also steers new sessions away from a shard whose heartbeat is stale, so a wedge degrades rather than black-holes while the probes decide.

- **After a deploy, watch CPU throttling, which no alert covers.** With a 2000m limit and roughly `1 + 2N` engine threads, the container can burn its quota early in each 100 ms CFS period and freeze for the remainder; a 60 ms freeze sits just under `RelaySchedulerLagHigh`'s 100 ms p99 trigger, so the lag alert can stay silent through it. Watch the ratio, not the seconds: `rate(container_cpu_cfs_throttled_periods_total[5m]) / rate(container_cpu_cfs_periods_total[5m])` for the relay containers. Under 0.05 sustained is healthy; above 0.20 the sharding gain is being eaten. This is deliberately NOT shipped as an alert rule: the series comes from cAdvisor, which the existing `container_cpu_usage_seconds_total` rules imply is scraped, but that was not confirmed against a live HCL Prometheus, and an alert on a series that does not exist is worse than none.

- **CPU requests are raised only where the node budget was checked.** hcl-daily and labsworkspace give the WT relay `requests.cpu: 500m`, because under contention the node shares by request and a sharded relay splitting a 200m share across its engine threads crawls on every arbiter at once. Node requests there total roughly 1175m of about 3800m allocatable, so a maxSurge pod still fits. Ascend also shards (it already passes `limits.cpu=2000m`) but keeps `200m`, and us-east and singapore are unchanged: their node budgets were not verified here, and the parity floor stays at 200m so it cannot force an unchecked change on them. Raising them is an ops decision with the node data in hand.

- **Set the WT relay's `resources.limits.cpu` in WHOLE cores.** `available_parallelism()` reads the container's cgroup CPU quota and FLOORS it to whole cores (`quota.min(limit / period)`, integer division, in the Rust standard library). A fractional limit like `1500m` therefore reports **one** core: the relay sizes both its tokio worker pool and its #2727 session arbiters to 1, and sharding is silently disabled — the relay runs exactly as it did before #2727 while the dashboard shows a 1.5-core limit it can never approach. `2000m` is the smallest limit that shards at all, and it is what the hcl and labsworkspace deploy steps now pass. `scripts/check_deploy_env_parity.py` enforces it as a floor. us-east's `3500m` floors to 3 cores and its trailing `500m` is unusable; rounding it to `4000m` would buy a fourth arbiter and is an ops decision, not a code one.

- **The WT relay has no CPU alert of its own any more.** `RelayWTCPUNearOneCore` fired at an absolute 0.85 cores because the relay's loop could not exceed one; #2727 removes that ceiling, so the rule was dropped and the WT container was un-excluded from the standard limit-relative `ContainerCPUHigh` warning in all three Prometheus values files. `RelaySchedulerLagHigh` (critical) is unchanged and remains the alert that matters: every arbiter's probe feeds the one unlabelled `videocall_relay_scheduler_lag_ms` histogram, so a wedge on any single arbiter shows in its p99 tail. `/healthz` reads the OLDEST runtime heartbeat, so a wedged arbiter fails the readiness and liveness probes even while the others are healthy.
- **The WT relay chart refuses to render above one replica (#2727).** `helm template` fails, naming issue #1202, when `replicaCount` is above 1 or `autoscaling.enabled` is true with `maxReplicas` above 1. The guard was added because a replica whose own copy of a split room emptied would idle the meeting, or end it on `end_on_host_leave`, while another replica still held participants (#1202). Since #2702 that cannot happen: relays never end meetings, and meeting-api idles or ends a meeting only from per-session presence leases, so a room split across WT replicas behaves like one split across the WS and WT relays. More than one WT replica has still never been run; lifting the guard in `helm/rustlemania-webtransport/templates/validate-values.yaml` is a human decision. Two things are dormant until then and must be restored with it, and the guard's failure message names both: `templates/pdb.yaml` gates on more than one replica, so the #2719 PodDisruptionBudget renders on no valid configuration today (the one surviving combination, `minReplicas: 2` with `maxReplicas: 1`, the HPA API rejects); and `service.externalTrafficPolicy` is pinned to `Cluster`. The Service also carries `sessionAffinity: ClientIP` at `timeoutSeconds: 86400` (`MaxClientIPServiceAffinitySeconds`, the API ceiling). Affinity is keyed on the client's source **address** only, so it survives a NAT rebind that changes the source port but not a move from Wi-Fi to LTE. At one replica all of it is inert; it is the precondition for a second.

- **`externalTrafficPolicy` is `Cluster` everywhere, for two different reasons.** On `us-east` and `singapore` that is **temporary**: `Local` is right above one replica (DO `REGIONAL_NETWORK` is pass-through, so it preserves the client address, and it makes the DO health check node-aware), but at one replica it only narrows serving to the node holding the pod and adds a 10-15s partial-loss window per relay reschedule, from the health-check thresholds in the same values file. Flip those two overlays to `Local` in the change that lifts the replica guard. On `hcl`, `labsworkspace` and `ascend` the `--set service.externalTrafficPolicy=Cluster` pin is **permanent**: k3s ServiceLB's klipper-lb MASQUERADEs in POSTROUTING unconditionally, and ascend proxies UDP to a pinned node port, so `Local` would preserve nothing and would blackhole a node with no relay pod. `scripts/check_wt_service_affinity.py` renders the chart, both wrappers and every relay deploy step and fails on drift; `make test-webtransport-chart` runs it with its self-test.

- **No workflow deploys `helm/global/*/webtransport`.** Both overlays are applied by hand. Only the three `daily-deploy-*.yaml` workflows ship the WT relay automatically, and they render the chart directly with their own `--set` block, so an overlay edit changes nothing until an operator runs `helm upgrade` against that cluster.

- **The DigitalOcean health-check annotations on the WT overlays are inert as written.** `do-loadbalancer-healthcheck-path`, `-port` and `-protocol` in `helm/global/{us-east,singapore}/webtransport/values.yaml` are read by the DO cloud controller only when `service.beta.kubernetes.io/do-loadbalancer-override-health-check` is also set, and it is not. Without the override the controller uses the Service's own check: kube-proxy's port 10256 under `Cluster`, or the allocated `healthCheckNodePort` under `Local`. The `Local` form is the one that marks a node with no relay pod unhealthy, so adding the override annotation would make the health check worse, not better. The interval, timeout and threshold annotations are honoured either way.

- **No workflow applies ANY `helm/global/*/prometheus/values.yaml`.** The only hits on those paths in `.github/workflows/` are the parity guard's `paths:` filter. Alert rules in these files are defined, not deployed, and each file reaches a different place — or nowhere:

  | File | Cluster | Mechanism | How it is applied |
  |---|---|---|---|
  | `helm/global/us-east/prometheus/values.yaml` | us-east (DigitalOcean) | community `prometheus` chart, wrapped locally (`Chart.yaml` + `Chart.lock` pin chart 25.0.0, Prometheus v2.47.0) | by hand, `helm upgrade` against the wrapper |
  | `helm/global/hcl-daily-deployment/prometheus/values.yaml` | hcl-daily | community `prometheus` chart, no wrapper | by hand, the command below |
  | `helm/global/hcl/prometheus/values.yaml` | **none** | — | **nothing applies it** |

  The third row is not a gap to be plumbed. The `hcl` overlay is read by exactly one deploy line anywhere in `.github/`: the "Deploy Grafana dashboards" step of `daily-deploy-hcl.yaml`, which passes `helm/global/hcl/grafana/values.yaml`. That is Grafana, not Prometheus — so hcl-daily takes its Grafana values from `hcl/` and its Prometheus values from `hcl-daily-deployment/`, and the overlay named for the cluster is not the one that holds its alert rules. Ascend runs **kube-prometheus-stack** (`docs/Monitoring_Production.md`, #2256), and labsworkspace moved its Grafana onto the same stack "mirroring Ascend" (`daily-deploy-labsworkspace.yaml`) and has no `prometheus/` overlay in this repo at all. That stack is cluster-scoped infrastructure — CRDs, ClusterRoles, an admission webhook — and `videocall-ci` is a namespace-scoped ServiceAccount, so this repo cannot install it. On a kube-prometheus-stack cluster a community-chart `serverFiles` values file does nothing whatsoever: rules there must be `PrometheusRule` CRs carrying `labels: {release: kube-prometheus-stack}`, because Ascend sets `ruleSelector` to match that label and silently ignores anything else (#2256). `helm/global/hcl/prometheus/values.yaml` therefore exists only as the third member of the parity set, so a rule added for one cluster cannot quietly skip another.

  ⚠️ **Do not run the command below against Ascend or labsworkspace.** It would install a SECOND, independent Prometheus alongside the kube-prometheus-stack one.

  For hcl-daily:

  ```bash
  helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
  helm upgrade --install prometheus prometheus-community/prometheus \
    --namespace videocall \
    -f helm/global/hcl-daily-deployment/prometheus/values.yaml
  ```

  Release name `prometheus` and namespace `videocall` are read off the running hcl-daily server, whose own targets are `prometheus-kube-state-metrics` and `prometheus-prometheus-pushgateway` (the chart prefixes these with the release name). Confirm with `helm list -n videocall` before running — nobody has cluster credentials checked in, so the release identity is inferred, not verified. The `--version 25.0.0` pin was dropped from this command, and dropping it is the fix, not a loosening: that version is us-east's `Chart.lock`, hcl-daily has no lock file, and chart 25.0.0 carries `appVersion: v2.47.0` while the running hcl-daily server is **3.11.2** (#2732). Anyone following the old command would have DOWNGRADED hcl-daily's Prometheus by a major version. Unpinned does NOT mean "what that cluster already runs": the current chart is **29.33.0 at `appVersion: v3.14.0`**, so an unpinned install UPGRADES hcl-daily from 3.11.2 to v3.14.0. Decide which you want before running it — pin `--version` to the chart whose appVersion matches the running server, or accept the upgrade deliberately and re-read `PINNED_IMAGES` in `scripts/check_prometheus_rules_parse.py` afterwards, since that guard checks every file against both 3.11.2 and 2.47.0 precisely because the fleet spans both majors. Editing these files changes nothing until someone runs that command.

- **A rule that does not parse takes the whole file down with it.** Prometheus loads a rule file atomically: one bad expression means ZERO groups, not "the rest". A fresh server refuses to start and crashloops — **exit 2 on 3.11.2, exit 1 on 2.47.0**, so do not pattern-match on the code. A server already running rejects the reload and keeps its previous rule set on both majors, logging `error loading rules, previous rule set restored`. So a bad rule can sit in the repo looking deployed while the cluster runs a rule set months older. `scripts/check_prometheus_rules_parse.py` runs `promtool check rules` in the deploy-parity job against both fleet versions, and rejects the PromQL functions 3.x gates behind `--enable-feature=promql-experimental-functions` — which 2.x does not have at all, so there is no flag that makes them portable.

- **Alert groups only load from a `rule_files` path.** A `serverFiles` key renders to `/etc/config/<key>`, and the upstream chart's default `rule_files` list is `recording_rules.yml`, `alerting_rules.yml`, `rules`, `alerts`. The HCL overlays put their groups under `alerting_rules.yml` so the default list picks them up; us-east instead keeps `alert_rules.yml` and names it in its own `serverFiles."prometheus.yml".rule_files` override. A group under any other key is silently never read — `scripts/check_prometheus_alert_parity.py` fails the build on that.

- **Alertmanager is not wired up across overlays.** `helm/global/us-east/prometheus/values.yaml` sets `alertmanager.enabled: false` explicitly; the `hcl` and `hcl-daily-deployment` overlays don't configure the key (rely on chart default), which renders an Alertmanager whose only receiver has no notifier. Either way no routes or receivers are configured, so alerts that load will evaluate and appear in Prometheus' `/alerts` UI but do NOT page anyone, and the `page: "true"` label on the critical rules is aspirational. Tracked in issue #729.

---

*Generated 2026-05-12 from a survey of the `labs-projects/videocall` repo on the `PR-staging` branch.*
