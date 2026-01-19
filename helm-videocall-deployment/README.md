# Videocall Deployment Configurations

This directory contains deployment-specific configurations and infrastructure charts for Videocall.rs.

## Structure

```
helm-videocall-deployment/
├── infrastructure/     # Infrastructure charts (NATS, Prometheus, Postgres, etc.)
├── apps/              # Other applications (engineering-vlog, website)
├── us-east/           # US East region deployment values
└── singapore/         # Singapore region deployment values
```

## Infrastructure Charts

Shared infrastructure components:
- **nats/** - NATS messaging server
- **postgres/** - PostgreSQL database
- **prometheus/** - Metrics collection
- **grafana/** - Metrics visualization
- **cert-manager/** - TLS certificate management
- **ingress-nginx/** - Ingress controller
- **external-dns/** - DNS automation
- And more...

## Region-Specific Deployments

Each region directory contains Helm value overrides for deploying the videocall chart and infrastructure to that region.

## Usage

The public Videocall application chart is at `../helm/videocall/`

This directory is for:
1. Infrastructure dependencies
2. Region-specific configuration values
3. Other applications deployed alongside Videocall
