# Matomo Helm Deployment

This directory contains the working Matomo deployment configuration.

## Files

- `values.yaml` - Working Helm values configuration
- `mariadb-service.yaml` - External MariaDB service (required)
- `sqlite-matomo.yaml` - Alternative SQLite-based deployment (standalone K8s manifest)

## Deployment Process

### Option 1: Helm Deployment (Recommended)

1. **Deploy MariaDB service first:**
   ```bash
   kubectl apply -f mariadb-service.yaml
   ```

2. **Wait for MariaDB to be ready:**
   ```bash
   kubectl wait --for=condition=ready pod -l app=matomo-mariadb
   ```

3. **Deploy Matomo using Helm:**
   ```bash
   helm install matomo bitnami/matomo -f values.yaml
   ```

### Option 2: Standalone K8s Deployment (SQLite)

If you prefer SQLite instead of MariaDB:
```bash
kubectl apply -f sqlite-matomo.yaml
```

## Database Configuration

### MariaDB (Option 1)
- **Host:** `matomo-mariadb`
- **Port:** `3306`
- **Database:** `matomo`
- **Username:** `matomo`
- **Password:** `MatomoDB123`

### SQLite (Option 2)
- No external database configuration needed
- Data stored in persistent volume

## Access

- **URL:** https://matomo.videocall.rs
- **Username:** `admin`
- **Password:** `MatomoAdmin123`

## Troubleshooting

If you encounter database connection issues:
1. Check MariaDB pod status: `kubectl get pods -l app=matomo-mariadb`
2. Check MariaDB logs: `kubectl logs -l app=matomo-mariadb`
3. Test database connection: `kubectl exec matomo-mariadb-0 -- mariadb -u matomo -p'MatomoDB123' -e "SELECT 1;"` 