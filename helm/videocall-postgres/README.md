# videocall-postgres

Cluster-agnostic PostgreSQL chart for the videocall application: a thin wrapper that pins
`bitnami/postgresql` and carries only the settings every deployment shares.

**This chart alone will not render.** `values.schema.json` requires `auth.database`,
`image.tag` and `primary.resources`. When the chart creates a PVC, it also requires
`primary.persistence.storageClass` and `primary.persistence.size`. `helm lint`, `template`,
`install` and `upgrade` therefore fail without cluster values:

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
make build-videocall-postgres
helm upgrade --install postgres helm/videocall-postgres/ \
  -n videocall \
  -f helm/global/<cluster>/postgres/values.yaml
```

## What lives where

| In this chart | In the cluster values file |
|---|---|
| `auth.*` (secret name and keys, `postgres` user) | `auth.database` |
| `image.tag: ""` — empty so the subchart's `latest` default cannot satisfy the schema | `image.*` (registry, repository, **`tag` — required**, optional `digest`, pull secrets) |
| `metrics.enabled: false` | `metrics.image.*` when metrics are enabled |
| | `global.security.allowInsecureImages: true` when Bitnami reports the configured images as unrecognized, including digest-pinned and `bitnamilegacy/*` images |
| `persistence.enabled` + the `resource-policy: keep` annotation | `persistence.storageClass`, `persistence.size`, or `persistence.existingClaim` |
| `primary.service` (ClusterIP :5432) | `primary.resources` |
| `readReplicas.replicaCount: 0` | `primary.extendedConfiguration` (tuned to those resources) |

`extendedConfiguration` is intentionally per-cluster: the tuning is sized against that
cluster's memory limit, so a shared default would be wrong everywhere.

## Prerequisite

A `postgres-credentials` secret in the target namespace with keys `postgres-password` and
`password`. The chart consumes it via `existingSecret` and never creates or manages it.

## Upgrading

Bumping `dependencies[0].version` in `Chart.yaml` replaces the chart **templates** only. It does
not change the running PostgreSQL: the image comes from each cluster's `image.*`, and the PVC is
retained by `resource-policy: keep`. So a chart bump alone does nothing to the database.

**`image.tag` is schema-required**, so a chart bump can no longer *silently* move the image
underneath a retained data directory — the version is declared where it is reviewable. Two guards,
with different reach: the schema requires a tag to be *stated* and would accept the literal
`tag: "latest"`; `make test-videocall-postgres` additionally fails any cluster values file whose
rendered images include a `:latest` reference.

Where the registry publishes no versioned tags — Docker Hub's `bitnami/*`, since versioned tags
moved to paid Secure Images — pin `image.digest` as well and keep `image.tag` beside it. The digest
is what gets pulled; the tag documents which version that digest is, and turns a blanked digest
into a failed pull instead of a silent roll onto the subchart's `latest` default.

A real major-version upgrade needs three things, in order:

1. the chart bump (templates),
2. an explicit `image.tag` change — plus `image.digest`, where one is pinned — in each cluster's
   values file,
3. a supported data migration — `pg_upgrade` or dump/restore. PostgreSQL cannot read a data
   directory written by a newer major, and Bitnami's image does not migrate one for you.

Every cluster on this chart shares one `Chart.lock`, so step 1 moves them together. Steps 2 and 3
are per-cluster and are where the risk actually sits.

`helm/postgres/` is a separate, older chart (`rustlemania-postgres`, pinned to
`bitnami/postgresql` 12.5.7) referenced by `ARCHITECTURE.md`. It is not this chart and is not
affected by changes here.
