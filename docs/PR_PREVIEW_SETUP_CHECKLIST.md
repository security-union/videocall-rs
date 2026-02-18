# PR Preview Setup Checklist

Quick reference for setting up PR preview infrastructure. Complete these steps before using `/deploy`.

---

## Prerequisites

- [ ] Access to DigitalOcean K8s cluster `videocall-us-east`
- [ ] Access to DigitalOcean DNS for `videocall.rs`
- [ ] `kubectl` configured with cluster access
- [ ] Repository has `DIGITALOCEAN_ACCESS_TOKEN` secret configured

---

## Setup Steps

### 1. Get Ingress LoadBalancer IP

```bash
export KUBECONFIG=~/videocall-us-east-kubeconfig.yaml
LB_IP=$(kubectl get service ingress-nginx-us-east-controller -n default -o jsonpath='{.status.loadBalancer.ingress[0].ip}')
echo "Ingress LoadBalancer IP: ${LB_IP}"
```

**Record this IP:** `____________________`

---

### 2. Create Wildcard DNS Record

**In DigitalOcean DNS Console:**

1. Go to: https://cloud.digitalocean.com/networking/domains/videocall.rs
2. Click "Add Record"
3. Fill in:
   - **Type:** `A`
   - **Hostname:** `*.sandbox`
   - **Will Direct To:** `{LB_IP from step 1}`
   - **TTL:** `3600`
4. Click "Create Record"

**Verify after 1-2 minutes:**
```bash
dig pr-test.sandbox.videocall.rs
# Should return your LoadBalancer IP
```

- [ ] DNS record created
- [ ] DNS resolution verified

---

### 3. Create Wildcard TLS Certificate

```bash
kubectl apply -f - <<EOF
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: sandbox-wildcard-tls
  namespace: default
spec:
  secretName: sandbox-wildcard-tls
  issuerRef:
    name: letsencrypt-prod
    kind: Issuer
  dnsNames:
    - "*.sandbox.videocall.rs"
EOF
```

**Wait for certificate to be issued (~2-3 minutes):**
```bash
kubectl get certificate sandbox-wildcard-tls -n default -w
# Wait for READY=True
```

**Verify:**
```bash
kubectl get secret sandbox-wildcard-tls -n default
# Should show the secret exists
```

- [ ] Certificate created
- [ ] Certificate issued (READY=True)
- [ ] Secret exists

---

### 4. Verify Postgres Secret

```bash
kubectl get secret postgres-credentials -n default
```

**If exists:** ✅ Skip to step 5

**If missing, create it:**
```bash
# Get password from your postgres deployment
# Look in helm values or secrets manager

# Replace with actual password
PG_PASSWORD="your-actual-postgres-password"

kubectl create secret generic postgres-credentials -n default \
  --from-literal=password="${PG_PASSWORD}"
```

**Verify:**
```bash
kubectl get secret postgres-credentials -n default -o jsonpath='{.data.password}' | base64 -d
# Should print the password
```

- [ ] Secret exists
- [ ] Password verified

---

### 5. Test Postgres Database Creation

```bash
PG_PASSWORD=$(kubectl get secret postgres-credentials -n default -o jsonpath='{.data.password}' | base64 -d)

# Test create
kubectl exec -n default postgres-postgresql-0 -- \
  env PGPASSWORD="${PG_PASSWORD}" psql -U postgres -c "CREATE DATABASE test_preview;"

# Test drop
kubectl exec -n default postgres-postgresql-0 -- \
  env PGPASSWORD="${PG_PASSWORD}" psql -U postgres -c "DROP DATABASE test_preview;"
```

**Expected output:**
```
CREATE DATABASE
DROP DATABASE
```

- [ ] Database creation successful
- [ ] Database deletion successful

---

### 6. Add NATS Helm Repository (Local Testing Only)

This is automatically done in CI, but needed for local testing:

```bash
helm repo add nats https://nats-io.github.io/k8s/helm/charts/
helm repo update
```

- [ ] NATS repo added

---

## Verification

Run this script to verify all prerequisites:

```bash
#!/bin/bash

echo "🔍 Verifying PR Preview Prerequisites..."
echo ""

# Check kubectl access
echo -n "✓ Kubectl access: "
if kubectl cluster-info >/dev/null 2>&1; then
  echo "✅"
else
  echo "❌ Cannot connect to cluster"
  exit 1
fi

# Check ingress LB
echo -n "✓ Ingress LoadBalancer: "
LB_IP=$(kubectl get service ingress-nginx-us-east-controller -n default -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null)
if [ -n "$LB_IP" ]; then
  echo "✅ ${LB_IP}"
else
  echo "❌ Not found"
  exit 1
fi

# Check DNS
echo -n "✓ Wildcard DNS (*.sandbox.videocall.rs): "
DNS_IP=$(dig +short pr-test.sandbox.videocall.rs | head -1)
if [ "$DNS_IP" = "$LB_IP" ]; then
  echo "✅ Resolves to ${DNS_IP}"
elif [ -n "$DNS_IP" ]; then
  echo "⚠️  Resolves to ${DNS_IP} (expected ${LB_IP})"
else
  echo "❌ Does not resolve"
  exit 1
fi

# Check wildcard cert
echo -n "✓ Wildcard TLS certificate: "
if kubectl get secret sandbox-wildcard-tls -n default >/dev/null 2>&1; then
  CERT_READY=$(kubectl get certificate sandbox-wildcard-tls -n default -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')
  if [ "$CERT_READY" = "True" ]; then
    echo "✅ Ready"
  else
    echo "⚠️  Exists but not ready yet"
  fi
else
  echo "❌ Not found"
  exit 1
fi

# Check postgres secret
echo -n "✓ Postgres credentials secret: "
if kubectl get secret postgres-credentials -n default >/dev/null 2>&1; then
  echo "✅"
else
  echo "❌ Not found"
  exit 1
fi

# Check postgres connectivity
echo -n "✓ Postgres connectivity: "
PG_PASSWORD=$(kubectl get secret postgres-credentials -n default -o jsonpath='{.data.password}' | base64 -d)
if kubectl exec -n default postgres-postgresql-0 -- env PGPASSWORD="${PG_PASSWORD}" psql -U postgres -c "SELECT 1;" >/dev/null 2>&1; then
  echo "✅"
else
  echo "❌ Cannot connect"
  exit 1
fi

echo ""
echo "✅ All prerequisites verified!"
echo ""
echo "You can now use /deploy on PRs."
```

Save as `scripts/verify-pr-preview-setup.sh` and run:
```bash
chmod +x scripts/verify-pr-preview-setup.sh
./scripts/verify-pr-preview-setup.sh
```

---

## Ready to Deploy

Once all steps are complete:

1. **Push workflows to repository:**
   ```bash
   git add .github/workflows/pr-deploy.yaml
   git add .github/workflows/pr-undeploy.yaml
   git add .github/workflows/pr-cleanup.yaml
   git commit -m "feat: add PR preview deployment workflows"
   git push
   ```

2. **Test on a PR:**
   - Open or create a test PR
   - Comment `/build-images` (wait ~10-15 min)
   - Comment `/deploy` (wait ~3-5 min)
   - Test the preview URLs
   - Comment `/undeploy` to cleanup

3. **Monitor:**
   - Watch GitHub Actions for workflow runs
   - Check cluster: `kubectl get namespaces -l app=preview`
   - Check pods: `kubectl get pods -n preview-{PR}`

---

## Troubleshooting

### Certificate Not Issuing

**Check cert-manager logs:**
```bash
kubectl logs -n cert-manager -l app=cert-manager
```

**Check certificate status:**
```bash
kubectl describe certificate sandbox-wildcard-tls -n default
```

**Common issues:**
- DNS not propagated yet (wait 5-10 minutes)
- cert-manager not installed
- letsencrypt-prod issuer not configured

### DNS Not Resolving

**Check DNS propagation:**
```bash
dig @8.8.8.8 pr-test.sandbox.videocall.rs
```

**Common issues:**
- DNS record not saved
- Wrong hostname (should be `*.sandbox`, not `*.sandbox.videocall.rs`)
- TTL cache (wait for TTL to expire)

### Postgres Connection Failed

**Check postgres pod:**
```bash
kubectl get pods -n default | grep postgres
kubectl logs -n default postgres-postgresql-0
```

**Test connection manually:**
```bash
kubectl exec -it -n default postgres-postgresql-0 -- psql -U postgres
```

---

## Summary

**Setup Time:** ~15-20 minutes (mostly waiting for DNS/certs)

**One-Time Tasks:**
- ✅ DNS wildcard record
- ✅ TLS wildcard certificate
- ✅ Postgres credentials secret

**Per-Deployment (Automated):**
- Namespace creation
- NATS deployment
- Service deployments
- Database creation

---

**Questions?** Check `docs/PR_PREVIEW_IMPLEMENTATION_SUMMARY.md` for detailed architecture and troubleshooting.
