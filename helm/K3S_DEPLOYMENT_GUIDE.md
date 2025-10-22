# K3s Deployment Guide for VideoCall-RS

This guide outlines how to deploy a simple Kubernetes cluster with VideoCall-RS application on a bare K3s cluster. It includes both the infrastructure components and application-specific services with a simplified configuration.  We encourage you to read through this doc in its entirety first, ensure you understand what values need to be customized (and do it!) before you go crazy deploying with cut and paste!

## Prerequisites

- A server with sufficient resources (recommended: 4+ CPU cores, 8GB+ RAM).  This guide was done on an AWS EC2 instance using t3a.xlarge instance type.
- Domain name with DNS access, required to map dns names to ip addresses and obtain SSL certs via ACME & Lets Encrypt
- Ability to open necessary ports (80, 443, 4433/UDP for WebTransport)
- Clone the github repo https://github.com/security-union/videocall-rs locally and ensure you do your work from the root of the cloned repo (within the `videocall-rs` directory)

**Before proceeding, find all occurrences of `YOUR_DOMAIN_NAME` within the files located in the `videocall-rs/helm` directory tree.  Every occurrence should be replaced with your domain name where you will be deploying.  You must have a valid domain name, the procedure below requires you to use cert-manager and optionally external dns to set DNS entries for use with obtaining valid SSL certificates and resolving DNS names.  For example:**
```bash
find helm -type f -name "*.yaml" -exec sed -i 's/YOUR_DOMAIN_NAME/yourdomain.com/g' {} +
```
but use your actual domain name in the place of "yourdomain.com".

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
  --create-namespace \
  --namespace ingress-nginx \
  --version 4.13.0 \
  --set controller.service.type=NodePort \
  --set controller.service.externalIPs={3.83.161.171}
```

This deployment uses the following configuration for NGINX:
- Service type: NodePort
- External IP: 3.83.161.171

**Ensure you update this to your own IP address before you install.**

This configuration exposes the NGINX Ingress Controller on the specified IP address, making it accessible from outside the cluster. The NodePort service type allows the controller to be reached on specific ports on all nodes in the cluster.

### 2.2 Cert Manager

Install cert-manager for TLS certificate management:

```bash
helm repo add jetstack https://charts.jetstack.io
helm repo update

helm install cert-manager jetstack/cert-manager \
  --namespace cert-manager \
  --create-namespace \
  --version v1.13.0 \
  --set installCRDs=true \
  --values ./helm/cert-manager/values.yaml
```

Configure the LetsEncrypt Issuer in the videocall namespace.  We'll create a namespaced Issuer (va cluster issuer).  Note that in this example we're managing DNS via AWS Route 53, you could instead use CloudFlare or any other supported provider.  Refer to [Configuring DNS01 Challenge Provider](https://cert-manager.io/docs/configuration/acme/dns01/).

```bash
kubectl create namespace videocall

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
    email: your-email@yourdomain.com
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

> **Note**: This configuration uses DNS01 challenge with AWS Route53, which is required for validating wildcard certificates and is generally more reliable than HTTP01 validation. This deployment will use this method for certificate validation. You can use other DNS providers supported by cert-manager, or switch to HTTP01 validation (which doesn't work for wildcard certificates) if your deployment is available on the internet.

### 2.3 External DNS (Optional)

If you want automatic DNS management:

```bash
helm repo add external-dns https://kubernetes-sigs.github.io/external-dns/
helm repo update

# Create a secret for AWS Route53 API credentials
kubectl create namespace externaldns

# Create AWS Credentials File: Generate an AWS credentials file (e.g., ~/.aws/credentials)
# containing the Access Key ID and Secret Access Key of the IAM user.  Example content:
#  [default]
#  aws_access_key_id = AKIAEXAMPLEKEY123456
#  aws_secret_access_key = AbCdEfGhIjKlMnOpQrStUvWxYz1234567890+EXAMPLE

kubectl create secret generic external-dns -n externaldns \
  --from-file=dns-creds=~/.aws/credentials

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
    value: /etc/aws/credentials/dns-creds
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

The entire values file must be customized for your use.  See https://github.com/kubernetes-sigs/external-dns?tab=readme-ov-file for specifics on integrating with your DNS provider.

#### Alternative: Manual DNS Configuration

External DNS can be skipped if you prefer to manage DNS records manually. In this case, you would need to create the following A/CNAME records pointing to your cluster's public IP address:

| DNS Name | Purpose | Service Type |
|---------|---------|-------------|
| webtransport.yourdomain.com | WebTransport server | LoadBalancer |
| app.yourdomain.com | UI application | Ingress |
| websocket.yourdomain.com | WebSocket server | Ingress |
| grafana.yourdomain.com | Grafana dashboard | Ingress |

When using manual DNS configuration:
1. After deploying webtransport, get the IP address
   ```bash
   kubectl get service rustlemania-webtransport-lb -n videocall
   ```
2. Create an A record in your DNS provider for the appropriate hostname pointing to this IP
3. For Ingress resources, point all hostnames to the IP of your Ingress Controller:
   ```bash
   kubectl get service ingress-nginx-controller -n ingress-nginx
   ```

Manual DNS configuration requires updating records whenever your service IPs change (such as after cluster redeployment), whereas External DNS handles this automatically.

## 3. Monitoring Stack (Optional)

### 3.1 Prometheus

Install Prometheus for metrics collection:

```bash
helm install prometheus prometheus-community/prometheus \
  --namespace videocall \
  --values ./helm/global/us-east/prometheus/values.yaml
```

Key configurations to review:
- Retention period
- Storage size and class
- Scrape configurations (especially for service endpoints)

revise `./helm/global/us-east/prometheus/values.yaml` as necessary:

- **replace `do-block-storage` with "local-path" to use the default K3s storage.**
- **replace `server-metrics-api-us-east` with "server-metrics-api"**

### 3.2 Grafana

Install Grafana for metrics visualization:

```bash
./helm/global/us-east/grafana/deploy.sh
```

Key configurations to review (make changes to `helm/global/us-east/grafana/values.yaml`):
- Admin password
- Persistent storage: **replace `do-block-storage` with "local-path" to use the default K3s storage.**
- Ingress & hostname settings: **replace `grafana.videocall.rs` with your hostname
- Data source configuration (Prometheus)
- Dashboard provisioning

in `./helm/global/us-east/grafana/certificate.yaml` update the **dnsNames** to reflect your desired grafana url.

optionally, if you want to be able to access Grafana via Ingress,

**ensure the environment variable YOUR_DOMAIN_NAME is set to your domain name**

```
kubectl create -f - << EOF
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: prometheus-server
  namespace: videocall
  annotations:
    kubernetes.io/ingress.class: "nginx"
    cert-manager.io/issuer: "letsencrypt-prod"
    nginx.ingress.kubernetes.io/ssl-redirect: "true"
    nginx.ingress.kubernetes.io/proxy-buffer-size: "16k"
    nginx.ingress.kubernetes.io/proxy-busy-buffers-size: "16k"
spec:
  rules:
    - host: prometheus.${YOUR_DOMAIN_NAME}
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: prometheus-server
                port:
                  number: 80
  tls:
    - hosts:
        - prometheus.${YOUR_DOMAIN_NAME}
      secretName: prometheus-tls
EOF
```

## 4. Video Call Application Components

### 4.1 NATS Message Broker

Install NATS for application messaging:

```bash
helm install nats ./helm/nats \
  --namespace videocall \
  --values ./helm/nats/simple-values.yaml
```

This uses a very simple, non persisted NATS configuration.  Key configurations to review in `simple-values.yaml` vs the more complex configuration used with videocall.rs (`.helm/global/us-east/nats/values.yaml`):
- Authentication settings
- Resource limits
- Persistence configuration

### 4.2 Metrics API Services

Install the metrics API services:

First review `helm/global/us-east/metrics-api/values.yaml`.  Repleace `nats://nats-us-east:4222` with `nats://nats:4222`.

```bash
helm install metrics-api ./helm/global/us-east/metrics-api/metrics-api \
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

Key configurations to review in `./helm/rustlemania-websocket/custom-values.yaml`:
- Ingress hostnames -  update the **hostname** to reflect your desired url.

### 4.4 WebTransport Server

Install the WebTransport server:

```bash
helm install webtransport ./helm/rustlemania-webtransport \
  -f ./helm/rustlemania-webtransport/custom-values.yaml \
  --namespace videocall
```

> **Important**: Unlike other services, the WebTransport server does not use the NGINX Ingress Controller. Instead, it's exposed directly using a Kubernetes LoadBalancer Service that handles UDP traffic required for HTTP3/WebTransport protocol. The WebTransport server handles its own TLS termination using mounted certificates, and clients connect directly to this service rather than through NGINX. This is necessary because WebTransport requires direct UDP connectivity for the QUIC protocol.  It may be possible to configure the NGINX Ingress Controller to handle both HTTP and QUIC traffic on port 443, allowing unified ingress for WebTransport and standard HTTPS services. However, for simplicity, this guide configures the WebTransport service with a NodePort (or LoadBalancer) to directly expose UDP traffic required for QUIC/WebTransport, bypassing NGINX Ingress.

> Note:  The webtransport server may restart several times before success. Generally this is because it tries to start before cert-manager has obtained and setup the necessary SSL cert.  You can view the pod logs to confirm if your webtranport pod is failing to start.


### 4.5 UI Application

Install the UI application:

```bash
helm install ui ./helm/rustlemania-ui \
  -f ./helm/rustlemania-ui/custom-values.yaml \
  --namespace videocall
```


## 5. Post-Installation Verification

### 5.1 Verify Services

Check that all services are running:

```bash
$ kubectl get pods,services,ingress
NAME                                                     READY   STATUS    RESTARTS   AGE
pod/client-metrics-api-68889cbdb6-k7sh4                  1/1     Running   0          149m
pod/grafana-c44db467f-xkztd                              1/1     Running   0          5h25m
pod/nats-0                                               3/3     Running   0          5d1h
pod/nats-box-69b79775f4-5fxgn                            1/1     Running   0          5d1h
pod/prometheus-kube-state-metrics-686d9fd46c-jx6b9       1/1     Running   0          4h18m
pod/prometheus-prometheus-node-exporter-4zrz4            1/1     Running   0          4h18m
pod/prometheus-prometheus-pushgateway-6bf748ccc9-zlhb6   1/1     Running   0          4h18m
pod/prometheus-server-58cc4bc869-m6pnk                   2/2     Running   0          4h18m
pod/rustlemania-ui-cb7f7f5b-j8s5j                        1/1     Running   0          29h
pod/rustlemania-websocket-7d5685bf44-vs7sf               1/1     Running   0          29h
pod/rustlemania-webtransport-6db5b5678f-pthjk            1/1     Running   0          29h
pod/server-metrics-api-75447fcc86-nc4pq                  1/1     Running   0          4h44m

NAME                                          TYPE           CLUSTER-IP      EXTERNAL-IP      PORT(S)                                                 AGE
service/client-metrics-api                    ClusterIP      10.43.112.83    <none>           9091/TCP                                                4h45m
service/grafana                               ClusterIP      10.43.157.225   <none>           80/TCP                                                  5h25m
service/nats                                  ClusterIP      None            <none>           4222/TCP,6222/TCP,8222/TCP,7777/TCP,7422/TCP,7522/TCP   5d1h
service/prometheus-kube-state-metrics         ClusterIP      10.43.243.0     <none>           8080/TCP                                                5h31m
service/prometheus-prometheus-node-exporter   ClusterIP      10.43.143.191   <none>           9100/TCP                                                5h31m
service/prometheus-prometheus-pushgateway     ClusterIP      10.43.20.225    <none>           9091/TCP                                                5h31m
service/prometheus-server                     ClusterIP      10.43.32.88     <none>           80/TCP                                                  5h31m
service/rustlemania-ui                        ClusterIP      10.43.115.124   <none>           80/TCP                                                  47h
service/rustlemania-websocket                 ClusterIP      10.43.136.127   <none>           8080/TCP                                                29h
service/rustlemania-webtransport-lb           LoadBalancer   10.43.74.88     10.190.252.181   5321:32767/TCP,4433:32463/UDP                          47h
service/server-metrics-api                    ClusterIP      10.43.18.91     <none>           9092/TCP                                                4h45m

NAME                                              CLASS    HOSTS                    ADDRESS          PORTS     AGE
ingress.networking.k8s.io/grafana                 <none>   grafana.yourdomain.com      10.190.252.181   80, 443   5h25m
ingress.networking.k8s.io/prometheus-server       <none>   prometheus.yourdomain.com   10.190.252.181   80, 443   4h18m
ingress.networking.k8s.io/rustlemania-ui          nginx    app.yourdomain.com          10.190.252.181   80, 443   47h
ingress.networking.k8s.io/rustlemania-websocket   nginx    websocket.yourdomain.com    10.190.252.181   80, 443   29h
```

### 5.2 Verify TLS Certificates

Ensure certificates are properly issued, they should be ready:

```bash
$ kubectl get certificates -n videocall
NAME                           READY   SECRET             AGE
grafana-tls                    True    grafana-tls        5h18m
prometheus-tls                 True    prometheus-tls     4h16m
rustlemania-webtransport-tls   True    webtransport-tls   47h
ui-tls                         True    ui-tls             47h
websocket-tls                  True    websocket-tls      29h
```


### 5.3 Test Connectivity

Test connectivity to the various services:
- UI: https://app.yourdomain.com   **<-- this is the main url, open it in your browser!!**
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
   - Adjust retention and storage

6. **Grafana**: `./helm/global/us-east/grafana/values.yaml`
   - Configure admin credentials
   - Set up dashboards and data sources


## 7. Monitoring

Grafana is used for monitoring and visualizing metrics in this deployment. Two dashboards are provided for quick insights:

- **Server Connections Analytics**: `./helm/global/us-east/grafana/dashboards/server-connections-analytics.json`
- **Videocall Health**: `./helm/global/us-east/grafana/dashboards/videocall-health.json`

### Importing Dashboards into Grafana

1. Log in to your Grafana instance (https://grafana.yourdomain.com). The admin username and password was specified in `helm/global/us-east/grafana/values.yaml` (see `adminUser` and `adminPassword`).
2. In the left sidebar, click the **Dashboards** (four squares) icon, then select **Import**.
3. Click **Upload JSON file** and select either `server-connections-analytics.json` or `videocall-health.json` from `./helm/global/us-east/grafana/dashboards/`.
4. Choose the Prometheus data source if prompted, then click **Import**.
5. Repeat for the second dashboard.

You should now see both dashboards available in your Grafana instance for monitoring system health and analytics.

---
## 8. Troubleshooting

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


### 7.5 Common OOM Issues

If services are experiencing OOM errors:
- Check the resource limits in the custom-values.yaml files
- Ensure you've applied the changes with helm upgrade
- Verify the actual resource settings:
  ```bash
  kubectl get deployment -n videocall <deployment-name> -o yaml | grep -A10 resources
  ```
