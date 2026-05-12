#!/usr/bin/env bash
set -euo pipefail

# Deploy the E2E Scoreboard infrastructure to HCL K3s.
#
# Prerequisites:
#   - KUBECONFIG pointing at the K3s cluster
#   - helm, kubectl, curl, jq on PATH
#
# What this does:
#   1. Deploys pushgateway ingress (exposes pushgateway to CI runner)
#   2. Upgrades Grafana with the e2e-scoreboard dashboard
#   3. Enables Grafana public dashboard for the E2E scoreboard
#
# Usage:
#   KUBECONFIG=~/vc-k3s-config.yaml ./scripts/deploy-e2e-scoreboard.sh

NAMESPACE="${NAMESPACE:-videocall}"
GRAFANA_URL="${GRAFANA_URL:-https://grafana.videocall.fnxlabs.com}"
GRAFANA_USER="${GRAFANA_USER:-admin}"
GRAFANA_PASS="${GRAFANA_PASS:-password}"
DASHBOARD_UID="e2e-scoreboard"

echo "=== Deploying Pushgateway Ingress ==="
helm upgrade --install pushgateway-ingress \
  helm/util/hcl-daily/pushgateway-ingress/ \
  --namespace "${NAMESPACE}" \
  --wait --timeout 2m

echo ""
echo "=== Upgrading Grafana with E2E dashboard ==="
helm dependency update helm/grafana/
helm upgrade --install grafana helm/grafana/ \
  --namespace "${NAMESPACE}" \
  -f helm/global/hcl/grafana/values.yaml \
  --wait --timeout 5m

echo ""
echo "=== Waiting for Grafana to be ready ==="
for i in $(seq 1 60); do
  if curl -sf -u "${GRAFANA_USER}:${GRAFANA_PASS}" "${GRAFANA_URL}/api/health" >/dev/null 2>&1; then
    echo "Grafana ready after ${i}s"
    break
  fi
  sleep 2
done

echo ""
echo "=== Enabling public dashboard for ${DASHBOARD_UID} ==="

# Check if the dashboard exists
DASH_RESPONSE=$(curl -sf -u "${GRAFANA_USER}:${GRAFANA_PASS}" \
  "${GRAFANA_URL}/api/dashboards/uid/${DASHBOARD_UID}" 2>/dev/null || true)

if [ -z "${DASH_RESPONSE}" ] || echo "${DASH_RESPONSE}" | jq -e '.message' >/dev/null 2>&1; then
  echo "ERROR: Dashboard '${DASHBOARD_UID}' not found in Grafana."
  echo "The ConfigMap may not have been picked up yet. Wait a moment and re-run."
  exit 1
fi

DASH_ID=$(echo "${DASH_RESPONSE}" | jq '.dashboard.id')
echo "Dashboard internal ID: ${DASH_ID}"

# Check if public dashboard already exists
EXISTING_PUBLIC=$(curl -sf -u "${GRAFANA_USER}:${GRAFANA_PASS}" \
  "${GRAFANA_URL}/api/dashboards/uid/${DASHBOARD_UID}/public-dashboards" 2>/dev/null || echo "[]")

if echo "${EXISTING_PUBLIC}" | jq -e '.uid' >/dev/null 2>&1; then
  PUBLIC_UID=$(echo "${EXISTING_PUBLIC}" | jq -r '.uid')
  echo "Public dashboard already exists: ${GRAFANA_URL}/public-dashboards/${PUBLIC_UID}"
else
  # Create public dashboard
  CREATE_RESPONSE=$(curl -sf -u "${GRAFANA_USER}:${GRAFANA_PASS}" \
    -X POST "${GRAFANA_URL}/api/dashboards/uid/${DASHBOARD_UID}/public-dashboards" \
    -H 'Content-Type: application/json' \
    -d '{
      "isEnabled": true,
      "annotationsEnabled": false,
      "timeSelectionEnabled": true
    }')

  PUBLIC_UID=$(echo "${CREATE_RESPONSE}" | jq -r '.uid')
  echo "Public dashboard created!"
  echo ""
  echo "=============================================="
  echo "PUBLIC SCOREBOARD URL (no auth required):"
  echo "${GRAFANA_URL}/public-dashboards/${PUBLIC_UID}"
  echo "=============================================="
fi

echo ""
echo "=== Verifying pushgateway ingress ==="
kubectl get ingress -n "${NAMESPACE}" pushgateway

echo ""
echo "=== Done ==="
echo ""
echo "Summary:"
echo "  Pushgateway: https://pushgateway.videocall.fnxlabs.com"
echo "  Dashboard:   ${GRAFANA_URL}/d/${DASHBOARD_UID}"
echo "  Public URL:  ${GRAFANA_URL}/public-dashboards/${PUBLIC_UID:-<check above>}"
echo ""
echo "The E2E workflow will push metrics to pushgateway after each run."
echo "The scoreboard will populate once the first E2E run completes."
