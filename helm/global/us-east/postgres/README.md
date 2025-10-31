# PostgreSQL for US East Region

This Helm chart deploys PostgreSQL with persistent storage for the videocall-rs application.

## Installation

### 1. Update Helm dependencies

```bash
cd helm/global/us-east/postgres
helm dependency update
```

### 2. Install PostgreSQL

```bash
helm install postgres . -n default
```

Or with custom values:

```bash
helm install postgres . -n default -f values.yaml
```

## Database Configuration

- **Database Name**: `actix-api-db`
- **Username**: `postgres`
- **Password**: Set in `values.yaml` (change for production!)
- **Port**: 5432
- **Service Name**: `postgres-us-east-postgresql`

## Connecting to PostgreSQL

### From within the cluster:

```
Host: postgres-us-east-postgresql
Port: 5432
Database: actix-api-db
Username: postgres
Password: <from values.yaml>
```

### Connection string for actix-api:

```
DATABASE_URL=postgres://postgres:<password>@postgres-us-east-postgresql:5432/actix-api-db?sslmode=disable
```

## Persistent Volume

- **Storage Class**: `do-block-storage` (DigitalOcean Block Storage)
- **Size**: 10Gi
- **Retention**: Volume persists after `helm uninstall` due to `helm.sh/resource-policy: keep` annotation

## Managing the Database

### Access PostgreSQL shell:

```bash
kubectl exec -it postgres-us-east-postgresql-0 -- psql -U postgres -d actix-api-db
```

### View logs:

```bash
kubectl logs postgres-us-east-postgresql-0
```

### Check PVC status:

```bash
kubectl get pvc | grep postgres
```

## Backup and Restore

### Manual backup:

```bash
kubectl exec postgres-us-east-postgresql-0 -- pg_dump -U postgres actix-api-db > backup.sql
```

### Restore from backup:

```bash
kubectl exec -i postgres-us-east-postgresql-0 -- psql -U postgres actix-api-db < backup.sql
```

## Uninstalling

```bash
helm uninstall postgres -n default
```

**Note**: The Persistent Volume Claim (PVC) will **NOT** be deleted and your data will be preserved. To completely remove everything including data:

```bash
kubectl delete pvc data-postgres-us-east-postgresql-0
```

## Monitoring

PostgreSQL metrics are enabled and can be scraped by Prometheus. The metrics endpoint is available at:

```
http://postgres-us-east-postgresql-metrics:9187/metrics
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
kubectl describe pod postgres-us-east-postgresql-0
kubectl logs postgres-us-east-postgresql-0
```

### Storage issues:

```bash
kubectl get pvc
kubectl describe pvc data-postgres-us-east-postgresql-0
```

### Connection issues:

```bash
kubectl get svc postgres-us-east-postgresql
kubectl exec -it postgres-us-east-postgresql-0 -- psql -U postgres -c "SELECT version();"
```

