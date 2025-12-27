# Videocall Helm Charts

This directory contains public Helm charts for Videocall.rs.

## Charts

### videocall/

The unified Helm chart for deploying the complete Videocall.rs application stack.

Includes 6 components:
- UI
- WebSocket API
- WebTransport
- Website
- Metrics Client
- Metrics Server

See [videocall/README.md](videocall/README.md) for installation and configuration.

## Deployment-Specific Charts

Infrastructure and deployment-specific charts are in `helm-videocall-deployment/`:

- **infrastructure/** - NATS, PostgreSQL, Prometheus, Grafana, cert-manager, ingress, etc.
- **apps/** - Other applications (engineering-vlog, etc.)
- **us-east/** - US East region deployment values
- **singapore/** - Singapore region deployment values

## Quick Start

```bash
# Install Videocall application
cd videocall
helm install videocall . -f my-values.yaml
```

## Documentation

- **K3S_DEPLOYMENT_GUIDE.md** - Guide for deploying to K3s clusters
- **videocall/README.md** - Videocall chart documentation
- **videocall/QUICKSTART.md** - 5-minute getting started
- **videocall/MIGRATION.md** - Migrating from separate charts
