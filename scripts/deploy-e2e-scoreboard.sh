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
#   2. Imports the E2E scoreboard dashboard via Grafana API
#   3. Enables Grafana public dashboard for the E2E scoreboard
#
# Usage:
#   KUBECONFIG=~/vc-k3s-config.yaml ./scripts/deploy-e2e-scoreboard.sh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

NAMESPACE="${NAMESPACE:-videocall}"
GRAFANA_URL="${GRAFANA_URL:-https://grafana.videocall.fnxlabs.com}"
GRAFANA_USER="${GRAFANA_USER:-admin}"
GRAFANA_PASS="${GRAFANA_PASS:-password}"
DASHBOARD_UID="e2e-scoreboard"
DASHBOARD_JSON="${REPO_ROOT}/helm/grafana/dashboards/e2e-scoreboard.json"

echo "=== Deploying Pushgateway Ingress ==="
helm upgrade --install pushgateway-ingress \
  "${REPO_ROOT}/helm/util/hcl-daily/pushgateway-ingress/" \
  --namespace "${NAMESPACE}" \
  --wait --timeout 2m

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
echo "=== Importing E2E Scoreboard dashboard via API ==="

# Get or create the Videocall folder
FOLDER_ID=$(curl -sf -u "${GRAFANA_USER}:${GRAFANA_PASS}" \
  "${GRAFANA_URL}/api/folders" | jq -r '.[] | select(.title == "Videocall") | .id')

if [ -z "${FOLDER_ID}" ] || [ "${FOLDER_ID}" = "null" ]; then
  FOLDER_RESPONSE=$(curl -sf -u "${GRAFANA_USER}:${GRAFANA_PASS}" \
    -X POST "${GRAFANA_URL}/api/folders" \
    -H 'Content-Type: application/json' \
    -d '{"title": "Videocall"}')
  FOLDER_ID=$(echo "${FOLDER_RESPONSE}" | jq '.id')
  echo "Created Videocall folder (ID: ${FOLDER_ID})"
else
  echo "Using existing Videocall folder (ID: ${FOLDER_ID})"
fi

# Delete existing provisioned version if present (provisioned dashboards block API import)
EXISTING=$(curl -s -u "${GRAFANA_USER}:${GRAFANA_PASS}" \
  "${GRAFANA_URL}/api/dashboards/uid/${DASHBOARD_UID}" 2>/dev/null || true)

if echo "${EXISTING}" | jq -e '.meta.provisioned == true' >/dev/null 2>&1; then
  echo "Removing provisioned version of dashboard (will re-import as editable)..."
  # Cannot delete provisioned dashboards via API; remove from ConfigMap instead
  echo "WARNING: Dashboard is provisioned via ConfigMap. It will be overwritten on next import."
  echo "Ensure e2e-scoreboard.json is NOT in the dashboards-configmap.yaml template."
fi

# Import dashboard via API (overwrite if exists)
IMPORT_PAYLOAD=$(jq -n --argjson dashboard "$(cat "${DASHBOARD_JSON}")" \
  --argjson folderId "${FOLDER_ID}" \
  '{dashboard: $dashboard, overwrite: true, folderId: $folderId}')

IMPORT_RESPONSE=$(curl -s -u "${GRAFANA_USER}:${GRAFANA_PASS}" \
  -X POST "${GRAFANA_URL}/api/dashboards/db" \
  -H 'Content-Type: application/json' \
  -d "${IMPORT_PAYLOAD}")

if echo "${IMPORT_RESPONSE}" | jq -e '.status == "success"' >/dev/null 2>&1; then
  echo "Dashboard imported successfully"
  echo "  URL: ${GRAFANA_URL}$(echo "${IMPORT_RESPONSE}" | jq -r '.url')"
else
  echo "Import response: ${IMPORT_RESPONSE}"
  if echo "${IMPORT_RESPONSE}" | jq -e '.message' >/dev/null 2>&1; then
    echo "ERROR: $(echo "${IMPORT_RESPONSE}" | jq -r '.message')"
    echo ""
    echo "If the dashboard is provisioned, remove it from the ConfigMap template,"
    echo "run 'helm upgrade grafana', restart the grafana pod, then re-run this script."
    exit 1
  fi
fi

echo ""
echo "=== Enabling public dashboard for ${DASHBOARD_UID} ==="

# Check if public dashboard already exists
EXISTING_PUBLIC=$(curl -s -u "${GRAFANA_USER}:${GRAFANA_PASS}" \
  "${GRAFANA_URL}/api/dashboards/uid/${DASHBOARD_UID}/public-dashboards" 2>/dev/null || echo "{}")

if echo "${EXISTING_PUBLIC}" | jq -e '.uid' >/dev/null 2>&1; then
  PUBLIC_UID=$(echo "${EXISTING_PUBLIC}" | jq -r '.uid')
  echo "Public dashboard already exists: ${GRAFANA_URL}/public-dashboards/${PUBLIC_UID}"
else
  # Create public dashboard
  CREATE_RESPONSE=$(curl -s -u "${GRAFANA_USER}:${GRAFANA_PASS}" \
    -X POST "${GRAFANA_URL}/api/dashboards/uid/${DASHBOARD_UID}/public-dashboards" \
    -H 'Content-Type: application/json' \
    -d '{
      "isEnabled": true,
      "annotationsEnabled": false,
      "timeSelectionEnabled": true
    }')

  if echo "${CREATE_RESPONSE}" | jq -e '.uid' >/dev/null 2>&1; then
    PUBLIC_UID=$(echo "${CREATE_RESPONSE}" | jq -r '.uid')
    echo "Public dashboard created!"
  else
    echo "ERROR creating public dashboard: ${CREATE_RESPONSE}"
    echo "You may need to enable the public dashboard feature in grafana.ini:"
    echo "  [feature_toggles]"
    echo "  publicDashboards = true"
    exit 1
  fi
fi

echo ""
echo "=== Verifying pushgateway ingress ==="
kubectl get ingress -n "${NAMESPACE}" pushgateway

echo ""
echo "=============================================="
echo "DEPLOYMENT COMPLETE"
echo "=============================================="
echo ""
echo "  Pushgateway:  https://pushgateway.videocall.fnxlabs.com"
echo "  Dashboard:    ${GRAFANA_URL}/d/${DASHBOARD_UID}"
echo "  Public URL:   ${GRAFANA_URL}/public-dashboards/${PUBLIC_UID:-<see above>}"
echo ""
echo "The E2E workflow will push metrics to pushgateway after each run."
echo "The scoreboard will populate once the first E2E run completes."
