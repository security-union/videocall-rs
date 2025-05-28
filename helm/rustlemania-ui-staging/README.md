# Rustlemania UI Staging Deployment

This Helm chart deploys a staging version of the rustlemania-ui application.

## Overview

This chart is a wrapper around the base `rustlemania-ui` chart with staging-specific overrides:

- **Domain**: `staging-app.videocall.rs`
- **Service Name**: `rustlemania-ui-staging`
- **TLS Secret**: `rustlemania-ui-staging-tls`

## Prerequisites

- Kubernetes cluster with ingress-nginx controller
- cert-manager for TLS certificate management
- DNS configured to point `staging-app.videocall.rs` to your cluster

## Deployment

### Install/Upgrade the staging deployment:

```bash
# From the helm/rustlemania-ui-staging directory
helm upgrade --install rustlemania-ui-staging . --namespace default
```

### With custom values:

```bash
helm upgrade --install rustlemania-ui-staging . \
  --namespace default \
  --set rustlemania-ui.image.tag=your-staging-tag
```

## Configuration

The chart inherits all configuration from the base `rustlemania-ui` chart. Key overrides are defined in `values.yaml`:

- `rustlemania-ui.image.tag`: Set to `staging`
- `rustlemania-ui.ingress.hosts[0].host`: Set to `staging-app.videocall.rs`
- `rustlemania-ui.nameOverride`: Set to `rustlemania-ui-staging`

## Verification

After deployment, verify the staging application is accessible:

```bash
# Check deployment status
kubectl get deployment rustlemania-ui-staging

# Check ingress
kubectl get ingress rustlemania-ui-staging

# Check certificate
kubectl get certificate rustlemania-ui-staging-tls
```

The application should be accessible at: https://staging-app.videocall.rs 