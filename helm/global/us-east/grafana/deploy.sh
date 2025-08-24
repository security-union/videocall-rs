#!/bin/bash

# Set Grafana credentials from environment variables
export GRAFANA_ADMIN_USER=${GRAFANA_ADMIN_USER:-admin}
export GRAFANA_ADMIN_PASSWORD=${GRAFANA_ADMIN_PASSWORD:-videocall-monitoring-2024}

# Deploy Grafana with credentials from environment
helm upgrade --install grafana grafana/grafana \
  --namespace default \
  -f values.yaml \
  --set grafana.adminUser=$GRAFANA_ADMIN_USER \
  --set grafana.adminPassword=$GRAFANA_ADMIN_PASSWORD \
  --set grafana.grafana.ini.security.admin_user=$GRAFANA_ADMIN_USER \
  --set grafana.grafana.ini.security.admin_password=$GRAFANA_ADMIN_PASSWORD 