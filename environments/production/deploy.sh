#!/bin/bash
# Deployment script for videocall.rs production environment

set -e

# Default values
NAMESPACE="videocall-prod"
RELEASE_NAME="videocall"
VALUES_FILE="$(dirname "$0")/values.yaml"
DRY_RUN=false
UPGRADE_ONLY=false

# Parse command-line arguments
while [[ $# -gt 0 ]]; do
  case $1 in
    -n|--namespace)
      NAMESPACE="$2"
      shift 2
      ;;
    -r|--release-name)
      RELEASE_NAME="$2"
      shift 2
      ;;
    -f|--values-file)
      VALUES_FILE="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=true
      shift
      ;;
    --upgrade-only)
      UPGRADE_ONLY=true
      shift
      ;;
    -h|--help)
      echo "Usage: $0 [options]"
      echo ""
      echo "Options:"
      echo "  -n, --namespace NAMESPACE   Kubernetes namespace (default: videocall-prod)"
      echo "  -r, --release-name NAME     Helm release name (default: videocall)"
      echo "  -f, --values-file FILE      Custom values file (default: values.yaml in same dir)"
      echo "  --dry-run                   Perform a dry-run of the installation"
      echo "  --upgrade-only              Only upgrade, don't create the namespace"
      echo "  -h, --help                  Show this help message"
      exit 0
      ;;
    *)
      echo "Unknown option: $1"
      exit 1
      ;;
  esac
done

# Path to the Helm chart (relative to the repository root)
CHART_PATH="$(git rev-parse --show-toplevel)/helm/videocall"

# Check if the chart exists
if [[ ! -d "$CHART_PATH" ]]; then
  echo "Error: Helm chart not found at $CHART_PATH"
  exit 1
fi

echo "=== Deploying videocall to PRODUCTION environment ==="
echo "Namespace: $NAMESPACE"
echo "Release name: $RELEASE_NAME"
echo "Values file: $VALUES_FILE"
echo "Chart path: $CHART_PATH"
echo "Dry run: $DRY_RUN"
echo ""

# Create namespace if it doesn't exist
if [[ "$UPGRADE_ONLY" == "false" ]]; then
  if ! kubectl get namespace "$NAMESPACE" &>/dev/null; then
    echo "Creating namespace $NAMESPACE..."
    if [[ "$DRY_RUN" == "false" ]]; then
      kubectl create namespace "$NAMESPACE"
    else
      echo "DRY RUN: Would create namespace $NAMESPACE"
    fi
  else
    echo "Namespace $NAMESPACE already exists"
  fi
fi

# Construct the helm install/upgrade command
HELM_CMD="helm upgrade --install $RELEASE_NAME $CHART_PATH -n $NAMESPACE -f $VALUES_FILE"
if [[ "$DRY_RUN" == "true" ]]; then
  HELM_CMD="$HELM_CMD --dry-run"
fi

echo "Running: $HELM_CMD"

if [[ "$DRY_RUN" != "true" ]]; then
  eval "$HELM_CMD"
  echo "Deployment to PRODUCTION complete!"
else
  eval "$HELM_CMD"
  echo "Dry run complete - no changes were made"
fi

echo ""
echo "To check deployment status:"
echo "  kubectl get pods -n $NAMESPACE"
echo ""

# Print access information
echo "Once deployed, access the application at:"
echo "  UI: https://ui.videocall.rs"
echo "  API: wss://api.videocall.rs"
echo "  Transport: https://transport.videocall.rs" 