# videocall Helm Chart

This Helm chart deploys the complete videocall.rs platform, including all necessary components:

- UI (Yew-based frontend)
- WebSocket API Server (Actix-based)
- WebTransport Server
- NATS Messaging System
- PostgreSQL Database

## Prerequisites

- Kubernetes 1.19+
- Helm 3.2.0+
- PV provisioner support in the underlying infrastructure (for PostgreSQL and NATS persistence)
- Ingress controller (like nginx-ingress)

## Installation

### Add the chart repository

```bash
# This step is optional if you're installing from a local directory
helm repo add videocall https://charts.videocall.rs
helm repo update
```

### Install the chart

```bash
# From the repository
helm install videocall videocall/videocall -n videocall --create-namespace

# From local directory
helm install videocall ./helm/videocall -n videocall --create-namespace
```

### Using a custom values file

```bash
helm install videocall videocall/videocall -f values-custom.yaml -n videocall --create-namespace
```

## Upgrading

To upgrade the chart:

```bash
helm upgrade videocall videocall/videocall -n videocall
```

## Uninstalling

To uninstall/delete the deployment:

```bash
helm uninstall videocall -n videocall
```

## Configuration

The following table lists the configurable parameters of the videocall chart and their default values:

| Parameter                                 | Description                                           | Default                           |
|-------------------------------------------|-------------------------------------------------------|-----------------------------------|
| `global.environment`                      | Environment (production, staging, development)        | `production`                      |
| `global.domain`                           | Base domain for all components                        | `videocall.example.com`           |
| `global.tls.enabled`                      | Enable TLS for all components                         | `true`                            |
| `components.ui.enabled`                   | Enable UI component                                   | `true`                            |
| `components.websocket.enabled`            | Enable WebSocket component                            | `true`                            |
| `components.webtransport.enabled`         | Enable WebTransport component                         | `true`                            |
| `ui.replicaCount`                         | Number of UI replicas                                 | `1`                               |
| `ui.image.repository`                     | UI image repository                                   | `ghcr.io/security-union/rustlemania-ui` |
| `ui.image.tag`                            | UI image tag                                          | `latest`                          |
| `websocket.replicaCount`                  | Number of WebSocket API replicas                      | `2`                               |
| `websocket.image.repository`              | WebSocket API image repository                        | `ghcr.io/security-union/rustlemania-websocket` |
| `webtransport.replicaCount`               | Number of WebTransport replicas                       | `2`                               |
| `webtransport.image.repository`           | WebTransport image repository                         | `ghcr.io/security-union/rustlemania-webtransport` |
| `nats.enabled`                            | Enable NATS                                           | `true`                            |
| `postgresql.enabled`                      | Enable PostgreSQL                                     | `true`                            |
| `postgresql.auth.username`                | PostgreSQL username                                   | `videocall`                       |
| `postgresql.auth.password`                | PostgreSQL password                                   | `changeme`                        |

## Example Values Files

### Production

```yaml
global:
  environment: production
  domain: videocall.example.com

ui:
  replicaCount: 3
  resources:
    limits:
      cpu: 750m
      memory: 768Mi
    requests:
      cpu: 300m
      memory: 384Mi

websocket:
  replicaCount: 5
  resources:
    limits:
      cpu: 1500m
      memory: 1.5Gi
    requests:
      cpu: 750m
      memory: 768Mi

webtransport:
  replicaCount: 5
  resources:
    limits:
      cpu: 1500m
      memory: 1.5Gi
    requests:
      cpu: 750m
      memory: 768Mi

postgresql:
  auth:
    password: <your-secure-password>
  primary:
    persistence:
      size: 20Gi
    resources:
      limits:
        cpu: 1500m
        memory: 3Gi
```

### Development

```yaml
global:
  environment: development
  domain: dev.videocall.example.com
  tls:
    enabled: false

ui:
  replicaCount: 1
  resources:
    limits:
      cpu: 300m
      memory: 384Mi
    requests:
      cpu: 150m
      memory: 192Mi

websocket:
  replicaCount: 1
  resources:
    limits:
      cpu: 750m
      memory: 768Mi
    requests:
      cpu: 375m
      memory: 384Mi

webtransport:
  replicaCount: 1
  resources:
    limits:
      cpu: 750m
      memory: 768Mi
    requests:
      cpu: 375m
      memory: 384Mi

nats:
  cluster:
    enabled: false
    replicas: 1

postgresql:
  primary:
    persistence:
      size: 1Gi
``` 