# Grafana — us-east

Deploys Grafana monitoring for the `videocall.rs` us-east cluster.

- **URL**: https://grafana.videocall.rs
- **Namespace**: `default`
- **Helm release name**: `grafana-us-east`

## Chart structure

This is a local parent chart (`grafana-0.1.0`) that wraps the upstream
[`grafana/grafana` v7.0.0](https://grafana.github.io/helm-charts) as a subchart.
Because of this, **all values in `values.yaml` are nested under the `grafana:` key**.

```
grafana/
├── Chart.yaml                  # Parent chart definition; declares grafana/grafana 7.0.0 as dependency
├── Chart.lock                  # Locked dependency versions
├── charts/
│   └── grafana-7.0.0.tgz      # Vendored upstream chart
├── values.yaml                 # All chart configuration (nested under grafana:)
├── certificate.yaml            # cert-manager Certificate for grafana.videocall.rs
├── external-secret.yaml        # ExternalSecret pulling admin credentials from cluster SecretStore
└── dashboards/
    ├── videocall-health.json
    └── server-connections-analytics.json
```

## Deploying

Run from the repo root (or any directory — the chart path is explicit):

```bash
helm upgrade --install grafana-us-east helm/global/us-east/grafana/ \
  --namespace default \
  -f helm/global/us-east/grafana/values.yaml
```

**`-f values.yaml` is required.** Omitting it (or using `--reuse-values` alone) causes
Helm to fall back to its stored values, which will not pick up any local changes.

If the upstream subchart dependency needs to be refreshed first:

```bash
helm dependency update helm/global/us-east/grafana/
```

> **Do not use `grafana/grafana` from the public Helm repo as the chart argument.**
> That will deploy a bare chart without the local config and will create a second,
> conflicting release.

## Credentials

Admin credentials are managed by the `ExternalSecret` in `external-secret.yaml`.
It pulls from the cluster `SecretStore` and creates the Kubernetes secret
`grafana-admin-credentials` with keys `admin-user` and `admin-password`.

Grafana's `grafana.ini` references them via its native `${VAR}` interpolation:

```yaml
grafana.ini:
  security:
    admin_user: ${GRAFANA_ADMIN_USER}
    admin_password: ${GRAFANA_ADMIN_PASSWORD}
```

Upstream SecretStore keys: `grafana/admin-user`, `grafana/admin-password`.

## TLS

Managed by `certificate.yaml` — a cert-manager `Certificate` resource that issues
a Let's Encrypt certificate for `grafana.videocall.rs` via the `letsencrypt-prod`
cluster issuer. The secret is named `grafana-tls`.

## Datasources

A single Prometheus datasource is provisioned automatically:

| Name       | Type       | URL                                  |
|------------|------------|--------------------------------------|
| Prometheus | prometheus | `http://prometheus-us-east-server:80` |

The target (`prometheus-us-east`) runs in the same `default` namespace.

## Dashboards

### Custom dashboards (local JSON files)

Stored in `dashboards/` and provisioned as ConfigMaps at deploy time.

| Key                          | File                                   |
|------------------------------|----------------------------------------|
| `videocall-health`           | `dashboards/videocall-health.json`     |
| `server-connections-analytics` | `dashboards/server-connections-analytics.json` |

To add or update a custom dashboard: edit or add a JSON file in `dashboards/`,
reference it in `values.yaml` under `grafana.dashboards.default`, and redeploy.

### Community dashboards (downloaded from grafana.com at pod startup)

The `download-dashboards` init container fetches these automatically using `curl`
and writes them to `/var/lib/grafana/dashboards/default/`. They are not stored in
the repo.

For Grafana to load these files, a `dashboardProviders` entry **must** be present in
`values.yaml` pointing at that directory. Without it, the files are downloaded but
Grafana never scans the directory and the dashboards will not appear in the UI.

| Key                        | grafana.com ID | Revision | Covers                              |
|----------------------------|----------------|----------|-------------------------------------|
| `node-exporter-full`       | [1860](https://grafana.com/grafana/dashboards/1860)           | 37       | CPU, memory, disk, network per node |
| `kubernetes-cluster`       | [315](https://grafana.com/grafana/dashboards/315)            | 3        | Overall cluster health              |
| `kubernetes-deployments`   | [8588](https://grafana.com/grafana/dashboards/8588)           | 1        | Deployment/StatefulSet/DaemonSet    |
| `kubernetes-pod-resources` | [6417](https://grafana.com/grafana/dashboards/6417)           | 1        | Per-pod CPU and memory usage        |

To add more community dashboards, add an entry to `values.yaml`:

```yaml
grafana:
  dashboards:
    default:
      my-dashboard:
        gnetId: <ID from grafana.com>
        revision: <revision number>
        datasource: Prometheus
```

## Resources

Sized for cost efficiency; adjust if Grafana becomes slow under load.

| | CPU | Memory |
|---|---|---|
| Request | 50m | 64Mi |
| Limit | 50m | 128Mi |

## Persistence

A 1Gi PVC on `do-block-storage` (DigitalOcean block storage) persists Grafana's
internal state (user preferences, annotations, alert state, etc.). Community
dashboards are re-downloaded from grafana.com on each pod restart.
