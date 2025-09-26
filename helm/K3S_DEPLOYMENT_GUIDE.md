# K3s Deployment Guide for VideoCall-RS

This guide outlines the components needed to deploy the VideoCall-RS application on a bare K3s cluster. It includes both the infrastructure components and application-specific services.

## Prerequisites

- A server with sufficient resources (recommended: 4+ CPU cores, 8GB+ RAM)
- Domain name with DNS access
- Ability to open necessary ports (80, 443, 4433/UDP for WebTransport)

## 1. K3s Base Installation

Install K3s with the following command:

```bash
curl -sfL https://get.k3s.io | sh -s - --disable traefik
```

We disable the default Traefik ingress as we'll be using NGINX Ingress Controller instead.

After installation, ensure you can access the cluster:

```bash
sudo kubectl get nodes
```

## 2. Core Infrastructure Components

### 2.1 NGINX Ingress Controller

Install the NGINX Ingress Controller:

```bash
helm repo add ingress-nginx https://kubernetes.github.io/ingress-nginx
helm repo update

kubectl create namespace ingress-nginx
# Install NGINX Ingress Controller
helm install ingress-nginx ingress-nginx/ingress-nginx \
  --namespace ingress-nginx \
  --version 4.13.0 \
  --set controller.service.type=NodePort \
  --set controller.service.externalIPs={3.83.161.171}
```

The current deployment uses the following configuration:
- Service type: NodePort
- External IP: 3.83.161.171

This configuration exposes the NGINX Ingress Controller on the specified IP address, making it accessible from outside the cluster. The NodePort service type allows the controller to be reached on specific ports on all nodes in the cluster.

### 2.2 Cert Manager

Install cert-manager for TLS certificate management:

```bash
helm repo add jetstack https://charts.jetstack.io
helm repo update

kubectl create namespace cert-manager
helm install cert-manager jetstack/cert-manager \
  --namespace cert-manager \
  --version v1.13.0 \
  --set installCRDs=true \
  --values ./helm/cert-manager/values.yaml
```

Configure the LetsEncrypt Issuer in the videocall namespace:

```bash
# Create a secret for AWS Route53 API credentials
kubectl create secret generic route53-creds -n videocall \
  --from-literal=aws_access_key_id=YOUR_AWS_ACCESS_KEY_ID \
  --from-literal=aws_secret_access_key=YOUR_AWS_SECRET_ACCESS_KEY

# Create the namespace-scoped Issuer with DNS01 challenge using Route53
kubectl apply -f - <<EOF
apiVersion: cert-manager.io/v1
kind: Issuer
metadata:
  name: letsencrypt-prod
  namespace: videocall
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: your-email@example.com
    privateKeySecretRef:
      name: letsencrypt-prod
    solvers:
    - dns01:
        route53:
          region: us-east-1
          accessKeyIDSecretRef:
            name: route53-creds
            key: aws_access_key_id
          secretAccessKeySecretRef:
            name: route53-creds
            key: aws_secret_access_key
EOF
```

> **Note**: This configuration uses DNS01 challenge with AWS Route53, which is required for validating wildcard certificates and is generally more reliable than HTTP01 validation. Your current deployment uses this method for certificate validation. If you don't have Route53 access, you can use other DNS providers supported by cert-manager, or switch to HTTP01 validation (which doesn't work for wildcard certificates).

### 2.3 External DNS (Optional)

If you want automatic DNS management:

```bash
helm repo add external-dns https://kubernetes-sigs.github.io/external-dns/
helm repo update

# Create a secret for AWS Route53 API credentials
kubectl create namespace externaldns
kubectl create secret generic external-dns -n externaldns \
  --from-file=lotus-dns-creds=/path/to/your/aws/credentials/file

# Install External DNS with AWS Route53 configuration
helm install external-dns external-dns/external-dns \
  --namespace externaldns \
  --values ./helm/external-dns/external-dns-values.yaml
```

Below is the AWS Route53 configuration used in the values file:

```yaml
# AWS Route53 Provider Configuration
provider:
  name: aws

# AWS Credentials Configuration
env:
  - name: AWS_SHARED_CREDENTIALS_FILE
    value: /etc/aws/credentials/lotus-dns-creds
  - name: AWS_DEFAULT_REGION
    value: us-east-1

# Mount AWS credentials from secret
extraVolumeMounts:
  - mountPath: /etc/aws/credentials
    name: aws-credentials
    readOnly: true
extraVolumes:
  - name: aws-credentials
    secret:
      secretName: external-dns
```

Additional key configurations to review in `external-dns-values.yaml`:
- Domain filter (to limit which DNS zones External DNS can modify)
- DNS record TTL settings
- Synchronization interval
- Policy for DNS record management (upsert-only, sync, etc.)

#### Alternative: Manual DNS Configuration

External DNS can be skipped if you prefer to manage DNS records manually. In this case, you would need to create the following A/CNAME records pointing to your cluster's public IP address:

| DNS Name | Purpose | Service Type |
|---------|---------|-------------|
| webtransport.yourdomain.com | WebTransport server | LoadBalancer |
| app.yourdomain.com | UI application | Ingress |
| websocket.yourdomain.com | WebSocket server | Ingress |
| grafana.yourdomain.com | Grafana dashboard | Ingress |

When using manual DNS configuration:
1. After deploying each service with a LoadBalancer, get the external IP:
   ```bash
   kubectl get service rustlemania-webtransport-lb -n videocall
   ```
2. Create an A record in your DNS provider for the appropriate hostname pointing to this IP
3. For Ingress resources, point all hostnames to the IP of your Ingress Controller:
   ```bash
   kubectl get service ingress-nginx-controller -n ingress-nginx
   ```

Manual DNS configuration requires updating records whenever your service IPs change (such as after cluster redeployment), whereas External DNS handles this automatically.

## 3. Monitoring Stack

### 3.1 Prometheus

Install Prometheus for metrics collection:

```bash
kubectl create namespace videocall
helm install prometheus prometheus-community/prometheus \
  --namespace videocall \
  --values ./helm/global/us-east/prometheus/values.yaml
```

Key configurations to review:
- Retention period
- Storage size and class
- Scrape configurations (especially for service endpoints)

### 3.2 Grafana

Install Grafana for metrics visualization:

```bash
helm install grafana grafana/grafana \
  --namespace videocall \
  --values ./helm/global/us-east/grafana/values.yaml
```

Key configurations to review:
- Admin password
- Persistent storage
- Ingress settings
- Data source configuration (Prometheus)
- Dashboard provisioning

## 4. Application Components

### 4.1 NATS Message Broker

Install NATS for application messaging:

```bash
helm install nats ./helm/nats \
  --namespace videocall \
  --values ./helm/nats/simple-values.yaml
```

Key configurations to review in `simple-values.yaml`:
- Authentication settings
- Resource limits
- Persistence configuration

### 4.2 Metrics API Services

Install the metrics API services:

```bash
helm install metrics-api ./helm/metrics-api \
  --namespace videocall \
  --values ./helm/global/us-east/metrics-api/values.yaml
```

### 4.3 WebSocket Server

Install the WebSocket server:

```bash
helm install websocket ./helm/rustlemania-websocket \
  -f ./helm/rustlemania-websocket/custom-values.yaml \
  --namespace videocall
```

Key configurations to review in `custom-values.yaml`:
- Image repository and tag
- Environment variables (especially NATS_URL)
- Resource limits and requests (important to prevent OOM issues)
- Ingress hostnames

### 4.4 WebTransport Server

Install the WebTransport server:

```bash
helm install webtransport ./helm/rustlemania-webtransport \
  -f ./helm/rustlemania-webtransport/custom-values.yaml \
  --namespace videocall
```

> **Important**: Unlike other services, the WebTransport server does not use the NGINX Ingress Controller. Instead, it's exposed directly using a Kubernetes LoadBalancer Service that handles UDP traffic required for HTTP3/WebTransport protocol. The WebTransport server handles its own TLS termination using mounted certificates, and clients connect directly to this service rather than through NGINX. This is necessary because WebTransport requires direct UDP connectivity for the QUIC protocol.

Key configurations to review in `custom-values.yaml`:
- Image repository and tag
- TLS certificate configuration
- Resource limits and requests
- Service type (must be LoadBalancer for UDP support)
- Environment variables (NATS_URL, LISTEN_URL, etc.)

### 4.5 UI Application

Install the UI application:

```bash
helm install ui ./helm/rustlemania-ui \
  -f ./helm/rustlemania-ui/custom-values.yaml \
  --namespace videocall
```

Key configurations to review in `custom-values.yaml`:
- Image repository and tag
- Ingress hostname
- Environment variables

## 5. Post-Installation Verification

### 5.1 Verify Services

Check that all services are running:

```bash
kubectl get pods -n videocall
kubectl get services -n videocall
kubectl get ingress -n videocall
```

### 5.2 Verify TLS Certificates

Ensure certificates are properly issued:

```bash
kubectl get certificates -n videocall
```

### 5.3 Test Connectivity

Test connectivity to the various services:
- UI: https://app.yourdomain.com
- WebSocket: wss://websocket.yourdomain.com
- WebTransport: https://webtransport.yourdomain.com
- Grafana: https://grafana.yourdomain.com

## 6. Custom Values Files to Review

The following custom values files should be reviewed and updated for your deployment:

1. **WebSocket Server**: `./helm/rustlemania-websocket/custom-values.yaml`
   - Update image repository and tag
   - Configure resource limits (min 384Mi memory request, 768Mi limit)
   - Set appropriate ingress hostname
   - Configure environment variables

2. **WebTransport Server**: `./helm/rustlemania-webtransport/custom-values.yaml`
   - Update image repository and tag
   - Configure resource limits (min 384Mi memory request, 768Mi limit)
   - Set TLS certificate name
   - Configure service type and ports
   - Configure environment variables

3. **UI Application**: `./helm/rustlemania-ui/custom-values.yaml`
   - Update image repository and tag
   - Configure ingress hostname
   - Set environment variables

4. **NATS**: `./helm/nats/simple-values.yaml`
   - Configure authentication if needed
   - Adjust resource limits

5. **Prometheus**: `./helm/global/us-east/prometheus/values.yaml`
   - Update scrape configurations for your services
   - Adjust retention and storage

6. **Grafana**: `./helm/global/us-east/grafana/values.yaml`
   - Configure admin credentials
   - Set up dashboards and data sources

## 7. Troubleshooting

### 7.1 Pod Status Issues

If pods are not reaching the Running state:

```bash
kubectl describe pod <pod-name> -n videocall
kubectl logs <pod-name> -n videocall
```

### 7.2 Certificate Issues

If certificates are not being issued:

```bash
kubectl describe certificate <cert-name> -n videocall
kubectl describe challenges -n cert-manager
```

### 7.3 Ingress Problems

For ingress troubleshooting:

```bash
kubectl logs -n ingress-nginx -l app.kubernetes.io/name=ingress-nginx
```

### 7.4 Monitoring Issues

If metrics are not appearing in Grafana:

```bash
kubectl port-forward svc/prometheus-server 9090:80 -n videocall
# Then access http://localhost:9090 to check Prometheus directly
```

### 7.5 Common OOM Issues

If services are experiencing OOM errors:
- Check the resource limits in the custom-values.yaml files
- Ensure you've applied the changes with helm upgrade
- Verify the actual resource settings:
  ```bash
  kubectl get deployment -n videocall <deployment-name> -o yaml | grep -A10 resources
  ```

## 8. Backup Considerations

- **etcd data**: K3s stores this in `/var/lib/rancher/k3s/server/db/`
- **PersistentVolumes**: Mainly used by Prometheus and Grafana
- **Configuration files**: Back up all custom values files

## 9. Upgrades

For upgrading components:

```bash
# Example for WebSocket server:
helm upgrade websocket ./helm/rustlemania-websocket \
  -f ./helm/rustlemania-websocket/custom-values.yaml \
  --namespace videocall \
  --set image.tag=<new-tag>
```