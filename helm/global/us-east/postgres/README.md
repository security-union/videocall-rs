# PostgreSQL values for US East

This directory contains the US East values for the shared `helm/videocall-postgres` chart.

## Installation

### 1. Build Helm dependencies

```bash
make build-videocall-postgres
```

### 2. Install PostgreSQL

```bash
helm upgrade --install postgres helm/videocall-postgres \
  -n default \
  -f helm/global/us-east/postgres/values.yaml
```

## Database Configuration

- **Database Name**: `actix-api-db`
- **Username**: `postgres`
- **Password**: Set in `values.yaml` (change for production!)
- **Port**: 5432
- **Service Name**: `postgres-postgresql`

## Connecting to PostgreSQL

### From within the cluster:

```
Host: postgres-postgresql
Port: 5432
Database: actix-api-db
Username: postgres
Password: <from values.yaml>
```

### Connection string for actix-api:

```
DATABASE_URL=postgres://postgres:<password>@postgres-postgresql:5432/actix-api-db?sslmode=disable
```

## Persistent Volume

- **Storage Class**: `do-block-storage` (DigitalOcean Block Storage)
- **Size**: 10Gi
- **Retention**: Volume persists after `helm uninstall` due to `helm.sh/resource-policy: keep` annotation

## Managing the Database

### Access PostgreSQL shell:

```bash
kubectl exec -it postgres-postgresql-0 -- psql -U postgres -d actix-api-db
```

### View logs:

```bash
kubectl logs postgres-postgresql-0
```

### Check PVC status:

```bash
kubectl get pvc | grep postgres
```

## Backup and Restore

### Manual backup:

```bash
kubectl exec postgres-postgresql-0 -- pg_dump -U postgres actix-api-db > backup.sql
```

### Restore from backup:

```bash
kubectl exec -i postgres-postgresql-0 -- psql -U postgres actix-api-db < backup.sql
```

## Uninstalling

```bash
helm uninstall postgres -n default
```

**Note**: The Persistent Volume Claim (PVC) will **NOT** be deleted and your data will be preserved. To completely remove everything including data:

```bash
kubectl delete pvc data-postgres-postgresql-0
```

## Monitoring

PostgreSQL metrics are enabled and can be scraped by Prometheus. The metrics endpoint is available at:

```
http://postgres-postgresql-metrics:9187/metrics
```

## Security Recommendations

1. **Change default passwords** in production
2. Use **Kubernetes Secrets** instead of plain text passwords in values.yaml
3. Enable **SSL/TLS** for database connections
4. Set up **regular backups**
5. Consider enabling **read replicas** for high availability

## Troubleshooting

### Pod not starting:

```bash
kubectl describe pod postgres-postgresql-0
kubectl logs postgres-postgresql-0
```

### Storage issues:

```bash
kubectl get pvc
kubectl describe pvc data-postgres-postgresql-0
```

### Connection issues:

```bash
kubectl get svc postgres-postgresql
kubectl exec -it postgres-postgresql-0 -- psql -U postgres -c "SELECT version();"
```
