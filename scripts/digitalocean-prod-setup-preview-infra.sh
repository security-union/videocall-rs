#!/bin/bash
set -e

# Setup Preview Infrastructure for PR Previews (DigitalOcean Production)
#
# This script creates the shared infrastructure used by all PR preview
# deployments in the preview-infra namespace:
#
#   - preview-infra namespace
#   - PostgreSQL (isolated from production, via the same internal helm chart)
#   - postgres-credentials secret
#   - google-oauth-credentials secret
#
# TLS is NOT managed here.  Each preview slot copies the existing
# sandbox-wildcard-tls from the default namespace at deploy time.
#
# Prerequisites:
#   - kubectl configured for the target cluster
#   - helm available
#   - helm/global/us-east/postgres/charts/postgresql-18.1.3.tgz present (committed to repo)
#
# Usage:
#   export KUBECONFIG=~/videocall-us-east-kubeconfig.yaml
#   bash scripts/digitalocean-prod-setup-preview-infra.sh

NAMESPACE="preview-infra"

echo "========================================"
echo " PR Preview Infrastructure Setup"
echo " Cluster: $(kubectl config current-context 2>/dev/null || echo 'unknown')"
echo " Namespace: ${NAMESPACE}"
echo "========================================"
echo ""

# ── Preflight checks ────────────────────────────────────────────────────────

for cmd in kubectl helm; do
  if ! command -v "$cmd" &>/dev/null; then
    echo "❌ $cmd not found in PATH"
    exit 1
  fi
done

if ! kubectl version --client &>/dev/null; then
  echo "❌ kubectl is not properly configured"
  exit 1
fi

echo "✅ kubectl and helm are available"
echo ""

# ── Collect credentials ─────────────────────────────────────────────────────

echo "You will be prompted for the PostgreSQL password."
echo "The Google OAuth credentials will be copied from the default namespace."
echo ""

read -rsp "PostgreSQL password for preview-infra (will be set as postgres superuser password): " PG_PASSWORD
echo ""
if [ -z "${PG_PASSWORD}" ]; then
  echo "❌ Postgres password cannot be empty"
  exit 1
fi

echo ""

# Verify the source OAuth secret exists before proceeding
if ! kubectl get secret google-oauth-credentials -n default &>/dev/null; then
  echo "❌ Secret google-oauth-credentials not found in default namespace"
  echo "   Cannot copy OAuth credentials — ensure production is configured first"
  exit 1
fi

# ── 1. Namespace ─────────────────────────────────────────────────────────────

echo "1. Namespace"
if kubectl get namespace "${NAMESPACE}" &>/dev/null; then
  echo "   Namespace ${NAMESPACE} already exists, skipping"
else
  kubectl create namespace "${NAMESPACE}"
  echo "   ✅ Namespace created"
fi
echo ""

# ── 2. postgres-credentials secret ──────────────────────────────────────────

echo "2. Creating postgres-credentials secret"
if kubectl get secret postgres-credentials -n "${NAMESPACE}" &>/dev/null; then
  echo "   Secret already exists — deleting and recreating"
  kubectl delete secret postgres-credentials -n "${NAMESPACE}"
fi

kubectl create secret generic postgres-credentials \
  --from-literal=password="${PG_PASSWORD}" \
  --from-literal=postgres-password="${PG_PASSWORD}" \
  --namespace "${NAMESPACE}"

echo "   ✅ postgres-credentials secret created"
echo ""

# ── 3. PostgreSQL ─────────────────────────────────────────────────────────────

echo "3. Deploying PostgreSQL into ${NAMESPACE}"

CHART_DIR="helm/global/us-east/postgres"

# The chart dependency tgz is committed to the repo; fail early if somehow missing
if ! ls "${CHART_DIR}/charts/postgresql-"*.tgz &>/dev/null 2>&1; then
  echo "❌ Helm chart dependency missing: ${CHART_DIR}/charts/postgresql-*.tgz"
  echo "   Re-run: git submodule update --init (or restore charts/ from git)"
  exit 1
fi
echo "   Chart dependency present: $(ls ${CHART_DIR}/charts/postgresql-*.tgz)"

if helm list -n "${NAMESPACE}" | grep -q "^postgres"; then
  echo "   PostgreSQL is already deployed — skipping"
  echo "   To upgrade: helm upgrade postgres helm/global/us-east/postgres/ -n ${NAMESPACE} -f <values>"
else
  echo "   Installing PostgreSQL..."
  helm install postgres "${CHART_DIR}" \
    --namespace "${NAMESPACE}" \
    --set postgresql.auth.existingSecret=postgres-credentials \
    --set postgresql.auth.secretKeys.adminPasswordKey=postgres-password \
    --set postgresql.auth.secretKeys.userPasswordKey=password \
    --set postgresql.auth.database=postgres \
    --set postgresql.auth.username=postgres \
    --set postgresql.auth.enablePostgresUser=true \
    --set postgresql.primary.persistence.enabled=true \
    --set postgresql.primary.persistence.storageClass=do-block-storage \
    --set postgresql.primary.persistence.size=5Gi \
    --set 'postgresql.primary.persistence.annotations.helm\.sh/resource-policy=keep' \
    --set postgresql.primary.resources.requests.cpu=50m \
    --set postgresql.primary.resources.requests.memory=128Mi \
    --set postgresql.primary.resources.limits.cpu=200m \
    --set postgresql.primary.resources.limits.memory=256Mi \
    --set postgresql.primary.extendedConfiguration="max_connections=50
shared_buffers=32MB
effective_cache_size=128MB
maintenance_work_mem=16MB
work_mem=512kB" \
    --set postgresql.metrics.enabled=false \
    --set postgresql.readReplicas.replicaCount=0 \
    --set postgresql.backup.enabled=false \
    --set global.security.allowInsecureImages=true \
    --wait --timeout=5m

  echo "   ✅ PostgreSQL deployed"
fi
echo ""

# ── 4. Verify PostgreSQL connectivity ────────────────────────────────────────

echo "4. Verifying PostgreSQL connectivity"
if kubectl wait --for=condition=ready pod \
    -l app.kubernetes.io/name=postgresql \
    -n "${NAMESPACE}" \
    --timeout=120s 2>/dev/null; then
  if kubectl exec -n "${NAMESPACE}" postgres-postgresql-0 -- \
      env PGPASSWORD="${PG_PASSWORD}" psql -U postgres -c "SELECT version();" &>/dev/null; then
    echo "   ✅ PostgreSQL is reachable"
  else
    echo "   ⚠️  PostgreSQL pod is ready but connection test failed — check credentials"
    echo "   Test manually: kubectl exec -n ${NAMESPACE} postgres-postgresql-0 -- psql -U postgres"
  fi
else
  echo "   ⚠️  PostgreSQL pod not ready within 2 minutes"
  echo "   Check: kubectl get pod -n ${NAMESPACE} -l app.kubernetes.io/name=postgresql"
fi
echo ""

# ── 5. google-oauth-credentials secret ──────────────────────────────────────

echo "5. Copying google-oauth-credentials from default namespace"
if kubectl get secret google-oauth-credentials -n "${NAMESPACE}" &>/dev/null; then
  echo "   Secret already exists — deleting and recreating"
  kubectl delete secret google-oauth-credentials -n "${NAMESPACE}"
fi

kubectl get secret google-oauth-credentials -n default -o json \
  | jq 'del(.metadata.namespace, .metadata.resourceVersion, .metadata.uid,
             .metadata.creationTimestamp, .metadata.selfLink,
             .metadata.managedFields)
        | .metadata.namespace = "'${NAMESPACE}'"' \
  | kubectl apply -f -

echo "   ✅ google-oauth-credentials copied from default"
echo ""

# ── Summary ──────────────────────────────────────────────────────────────────

echo "========================================"
echo " ✅ Preview Infrastructure Ready"
echo "========================================"
echo ""
echo "Namespace: ${NAMESPACE}"
echo ""
echo "PostgreSQL:"
echo "  Pod:      postgres-postgresql-0"
echo "  Service:  postgres-postgresql.${NAMESPACE}.svc.cluster.local:5432"
echo "  User:     postgres"
echo "  Secret:   postgres-credentials"
echo ""
echo "Secrets:"
echo "  postgres-credentials        (keys: password, postgres-password)"
echo "  google-oauth-credentials    (copied from default namespace)"
echo ""
echo "Next steps:"
echo "  - PR previews will now create databases named preview_slot_{N} here"
echo "  - Databases are dropped automatically on undeploy/PR close"
echo "  - TLS certs are copied per-deploy from default/sandbox-wildcard-tls"
echo ""
echo "Useful commands:"
echo "  # List databases"
echo "  kubectl exec -n ${NAMESPACE} postgres-postgresql-0 -- \\"
echo "    env PGPASSWORD=<pass> psql -U postgres -c '\\l'"
echo ""
echo "  # Drop a stale preview database manually"
echo "  kubectl exec -n ${NAMESPACE} postgres-postgresql-0 -- \\"
echo "    env PGPASSWORD=<pass> psql -U postgres -c 'DROP DATABASE IF EXISTS preview_slot_1;'"
