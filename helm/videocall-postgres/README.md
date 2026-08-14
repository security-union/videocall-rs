# videocall-postgres

Cluster-agnostic PostgreSQL chart for the videocall application: a thin wrapper that pins
`bitnami/postgresql` and carries only the settings every deployment shares.

**This chart alone will not render.** `values.schema.json` requires `auth.database`,
`primary.persistence.storageClass`, `primary.persistence.size` and `primary.resources`, so
`helm lint`, `template`, `install` and `upgrade` all fail without a cluster values file:

```
Error: values don't meet the specifications of the schema(s) in the following chart(s):
videocall-postgres:
- at '/postgresql/auth/database': minLength: got 0, want 1
```

The schema is load-bearing, not decoration: without it Bitnami's own defaults render a perfectly
deployable StatefulSet on `registry-1.docker.io/bitnami/postgresql:latest` with an 8Gi
default-storage-class PVC and no application database — a silent wrong deployment rather than an
error. Supply the cluster values file:

```bash
helm dependency build helm/videocall-postgres/
helm upgrade --install postgres helm/videocall-postgres/ \
  -n videocall \
  -f helm/global/<cluster>/postgres/values.yaml
```

## What lives where

| In this chart | In the cluster values file |
|---|---|
| `auth.*` (secret name and keys, `postgres` user) | `auth.database` |
| `metrics.enabled: false` | `image.*` (registry, repository, tag, pull secrets) |
| `persistence.enabled` + the `resource-policy: keep` annotation | `persistence.storageClass`, `persistence.size` |
| `primary.service` (ClusterIP :5432) | `primary.resources` |
| `readReplicas.replicaCount: 0` | `primary.extendedConfiguration` (tuned to those resources) |

`extendedConfiguration` is intentionally per-cluster: the tuning is sized against that
cluster's memory limit, so a shared default would be wrong everywhere.

## Prerequisite

A `postgres-credentials` secret in the target namespace with keys `postgres-password` and
`password`. The chart consumes it via `existingSecret` and never creates or manages it.

## Upgrading

Bumping `dependencies[0].version` in `Chart.yaml` replaces the chart **templates** only. It does
not change the running PostgreSQL: the image comes from each cluster's `image.*` (pinned on
Ascend, unpinned elsewhere), and the PVC is retained by `resource-policy: keep`. So a chart bump
alone either does nothing to the database or, on an unpinned cluster, lets `latest` roll against
an older major's data directory.

A real major-version upgrade needs three things, in order:

1. the chart bump (templates),
2. an explicit `image.tag` change in each cluster's values file,
3. a supported data migration — `pg_upgrade` or dump/restore. PostgreSQL cannot read a data
   directory written by a newer major, and Bitnami's image does not migrate one for you.

Every cluster on this chart shares one `Chart.lock`, so step 1 moves them together. Steps 2 and 3
are per-cluster and are where the risk actually sits.

`helm/postgres/` is a separate, older chart (`rustlemania-postgres`, pinned to
`bitnami/postgresql` 12.5.7) referenced by `ARCHITECTURE.md`. It is not this chart and is not
affected by changes here.
