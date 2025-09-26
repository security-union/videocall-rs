
#!/bin/bash


# Save current working directory and ensure we return to it on script exit
ORIG_CWD="$(pwd)"
trap 'cd "$ORIG_CWD"' EXIT

# Change to the directory where the script resides
cd "$(dirname "$0")"

# Set Grafana credentials from environment variables
export GRAFANA_ADMIN_USER=${GRAFANA_ADMIN_USER:-admin}
export GRAFANA_ADMIN_PASSWORD=${GRAFANA_ADMIN_PASSWORD:-videocall-monitoring-2024}

# Update Helm dependencies
echo "Updating Helm dependencies..."
helm dependency update .

# Deploy Grafana with credentials from environment
echo "Deploying Grafana..."
helm upgrade --install grafana . \
  --namespace videocall \
  --debug \
  --set grafana.adminUser=$GRAFANA_ADMIN_USER \
  --set grafana.adminPassword=$GRAFANA_ADMIN_PASSWORD \
  --set grafana.grafana.ini.security.admin_user=$GRAFANA_ADMIN_USER \
  --set grafana.grafana.ini.security.admin_password=$GRAFANA_ADMIN_PASSWORD

# Apply the certificate resource
echo "Applying certificate..."
kubectl apply -f certificate.yaml

echo "Deployment complete!"
echo "Check status with: kubectl get pods,ingress,certificate -n videocall"
