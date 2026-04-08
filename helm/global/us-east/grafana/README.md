# Grafana — us-east

Environment-specific values and resources for the us-east Grafana deployment.

- **URL**: https://grafana.videocall.rs
- **Namespace**: `default`
- **Helm release name**: `grafana-us-east`

## Chart structure

The base Grafana chart lives at `helm/grafana/` (shared across all environments).
This directory contains only us-east-specific files:

```
helm/global/us-east/grafana/
├── values.yaml           # Environment-specific values (domain, datasource, creds, storage)
├── certificate.yaml      # cert-manager Certificate for grafana.videocall.rs
└── external-secret.yaml  # ExternalSecret pulling admin credentials from cluster SecretStore
```

All values in `values.yaml` are nested under the `grafana:` key because the base
chart wraps upstream `grafana/grafana` v7.0.0 as a subchart dependency.

## Deploying

```bash
helm upgrade --install grafana-us-east helm/grafana/ \
  -f helm/global/us-east/grafana/values.yaml \
  --namespace default
```

If the upstream subchart dependency needs to be refreshed first:

```bash
helm dependency update helm/grafana/
```

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

## TLS

Managed by `certificate.yaml` — a cert-manager `Certificate` resource that issues
a Let's Encrypt certificate for `grafana.videocall.rs` via the `letsencrypt-prod`
cluster issuer. The secret is named `grafana-tls`.

## Datasources

| Name       | Type       | URL                                  |
|------------|------------|--------------------------------------|
| Prometheus | prometheus | `http://prometheus-us-east-server:80` |

## Dashboards

### Custom dashboards

Three videocall dashboards are provisioned from JSON files in the base chart
(`helm/grafana/dashboards/`). They are loaded via a parent-chart ConfigMap
(`videocall-dashboards`) and mounted into the Grafana pod.

To add or update a custom dashboard: edit the JSON in `helm/grafana/dashboards/`
and add a reference in `helm/grafana/templates/dashboards-configmap.yaml`.

### Community dashboards (downloaded from grafana.com at pod startup)

| Key                        | grafana.com ID | Revision | Covers                              |
|----------------------------|----------------|----------|-------------------------------------|
| `node-exporter-full`       | [1860](https://grafana.com/grafana/dashboards/1860) | 37 | CPU, memory, disk, network per node |
| `kubernetes-cluster`       | [315](https://grafana.com/grafana/dashboards/315) | 3 | Overall cluster health |
| `kubernetes-deployments`   | [8588](https://grafana.com/grafana/dashboards/8588) | 1 | Deployment/StatefulSet/DaemonSet |
| `kubernetes-pod-resources` | [6417](https://grafana.com/grafana/dashboards/6417) | 1 | Per-pod CPU and memory usage |

To add more, add an entry in the environment values.yaml:

```yaml
grafana:
  dashboards:
    kubernetes:
      my-dashboard:
        gnetId: <ID from grafana.com>
        revision: <revision number>
        datasource: Prometheus
```

## Persistence

A 1Gi PVC on `do-block-storage` (DigitalOcean). Community dashboards are
re-downloaded on each pod restart.
