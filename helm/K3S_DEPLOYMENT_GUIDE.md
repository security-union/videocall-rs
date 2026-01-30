# K3s Deployment Guide for VideoCall-RS

This guide outlines how to deploy a simple Kubernetes cluster with VideoCall-RS application on a bare K3s cluster. It includes both the infrastructure components and application-specific services with a simplified configuration.

**Updated for Unified Chart**: This guide now uses the unified `videocall` Helm chart which deploys all application components (UI, WebSocket, WebTransport, Website, Metrics) in a single install. Infrastructure components (NATS, Prometheus, etc.) are installed separately from `helm-videocall-deployment/infrastructure/`.

We encourage you to read through this doc in its entirety first, ensure you understand what values need to be customized (and customize them!) before you go crazy deploying with cut and paste!

## Prerequisites

- A server with sufficient resources (recommended: 4+ CPU cores, 8GB+ RAM).  This guide was done on an AWS EC2 instance using t3a.xlarge instance type.
- A domain name that you can modify DNS A records.  This guide assumes you can establish programmatic interaction with DNS so that Cert Manager can obtain SSL certs via ACME & Lets Encrypt.  You will also need to add A records, either dynamically with External DNS or manually prior to your install.
- Ability to open necessary ports (80, 443, 443/UDP for WebTransport)
- Clone the github repo https://github.com/security-union/videocall-rs locally and ensure you do your work from the root of the cloned repo (within the `videocall-rs` directory)

**Before proceeding, you'll need to create a values file for your domain.** You must have a valid domain name - the procedure below requires cert-manager and optionally external-dns to set DNS entries for use with obtaining valid SSL certificates and resolving DNS names.

Create a custom values file for your deployment:
```bash
cat > my-values.yaml <<EOF
global:
  domain: "yourdomain.com"  # Replace with your actual domain
  region: "your-region"
  natsUrl: "nats:4222"

ui:
  enabled: true
  runtimeConfig:
    apiBaseUrl: "https://api.yourdomain.com"
    wsUrl: "wss://websocket.yourdomain.com"
    webTransportHost: "https://webtransport.yourdomain.com:443"

websocket:
  enabled: true
  ingress:
    hosts:
      - host: websocket.yourdomain.com
      - host: api.yourdomain.com

webtransport:
  enabled: true
  certificateDomain: "webtransport.yourdomain.com"

website:
  enabled: false  # Optional

metricsClient:
  enabled: true

metricsServer:
  enabled: true
EOF
```

Replace `yourdomain.com` with your actual domain (e.g., `example.com`).

## 1. K3s Base Installation

Install K3s with the following command as a normal user with sudo privileges:

```bash
curl -sfL https://get.k3s.io | sh -s - --disable traefik --write-kubeconfig-mode=644
```

We disable the default Traefik ingress as we'll be using NGINX Ingress Controller instead.

After installation, ensure you can access the cluster:

```bash
kubectl get nodes
```
Install Helm:

```bash
curl -fsSL -o get_helm.sh https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3
chmod +x get_helm.sh
./get_helm.sh
```

The version of `kubectl` installed with K3s will locate and use the k3s config file, but Helm will not, and by default it is readonly. Copy the k32 kubeconfig file so it's standard location so Helm can access the cluster:

```bash
mkdir ~/.kube
cp /etc/rancher/k3s/k3s.yaml  ~/.kube/config
```

Update your shell startup (e.g. `~/.bashrc`) and export `KUBECONFIG=~/.kube/config`
```bash
# update your shell startup script:
echo "export KUBECONFIG=~/.kube/config" >> ~/.bashrc
# Set it for this interactive shell sessions
export KUBECONFIG=~/.kube/config
```

We're going to install the application components into the `videocall` namespace.  Let's create that now and set it as the default context:
```bash
kubectl create namespace videocall
kubectl config set-context --current --namespace=videocall
```

## 2. Core Infrastructure Components

### 2.1 NGINX Ingress Controller

Install the NGINX Ingress Controller.

Setup the required Helm repository:
```bash
helm repo add ingress-nginx https://kubernetes.github.io/ingress-nginx
helm repo update
```
You need to specify the Ingress external IP address:
```bash
IPADDR=`hostname -i`
echo "Ingress IP Address: $IPADDR"
```
Ensure you validate this IP address before proceeding, reset the environment variable if necessary.  Then install NGINX:
```bash
# Install NGINX Ingress Controller
helm install ingress-nginx helm-videocall-deployment/infrastructure/ingress-nginx \
  --create-namespace \
  --namespace ingress-nginx \
  --version 4.13.0 \
  --set controller.service.type=NodePort \
  --set controller.service.externalIPs={$IPADDR}
```

This configuration exposes the NGINX Ingress Controller on the specified IP address, making it accessible from outside the cluster. The NodePort service type allows the controller to be reached on specific ports on all nodes in the cluster (in our example here, single node).

### 2.2 Cert Manager

Install cert-manager for TLS certificate management.

Add the necessary Helm repo:
```bash
helm repo add jetstack https://charts.jetstack.io
helm repo update
```
Install Cert Manager:
```bash
helm install cert-manager helm-videocall-deployment/infrastructure/cert-manager \
  --namespace cert-manager \
  --create-namespace
```

You only need to install one Issuer.  Below are examples of a AWS Route 53 Issuer and a CloudFlare Issuer.  If you are using different DNS management, consult the Cert Manager documentation: https://cert-manager.io/docs/configuration/acme/dns01

#### 2.2.1 AWS Route 53
##### AWS IAM Permissions for Route 53 DNS01 Challenge

The AWS identity (user or role) whose credentials are used for DNS01 challenge must have permissions to manage Route 53 records for your domain. At minimum, the following IAM policy should be attached:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "route53:ListHostedZones",
        "route53:GetChange",
        "route53:ChangeResourceRecordSets"
      ],
      "Resource": "*"
    }
  ]
}
```

This allows cert-manager to list hosted zones, submit DNS record changes, and check their status. For production, you may wish to restrict the `Resource` to only the hosted zone(s) you use.

For more details, see the cert-manager documentation: https://cert-manager.io/docs/configuration/acme/dns01/route53/

Configure the LetsEncrypt Issuer in the videocall namespace using AWS Route 53 for DNS01 challenge. We'll create a namespaced Issuer (vs a cluster issuer).

Refer to [Configuring DNS01 Challenge Provider](https://cert-manager.io/docs/configuration/acme/dns01/).


In the steps below, you must use your own AWS Access Key details.  Additionally, locate and edit `helm-videocall-deployment/infrastructure/cert-manager-issuer/route53-issuer.yaml` and update the `email` attribute with your own email.   Do that now before continuing.
```bash
# Create a secret for AWS Route53 API credentials
kubectl create secret generic route53-creds -n videocall \
  --from-literal=aws_access_key_id=YOUR_AWS_ACCESS_KEY_ID \
  --from-literal=aws_secret_access_key=YOUR_AWS_SECRET_ACCESS_KEY

# Create the namespace-scoped Issuer with DNS01 challenge using Route53
kubectl apply -f helm-videocall-deployment/infrastructure/cert-manager-issuer/route53-issuer.yaml
```

> **Note**: This configuration uses DNS01 challenge with AWS Route53, which is required for validating wildcard certificates and is generally more reliable than HTTP01 validation. This deployment will use this method for certificate validation. You can use other DNS providers supported by cert-manager, or switch to HTTP01 validation (which doesn't work for wildcard certificates) if your deployment is available on the internet.

#### 2.2.2 Cloudflare

Alternatively, you can use Cloudflare for DNS01 challenge. The process is similar, but uses Cloudflare API credentials. See [cert-manager Cloudflare DNS01 documentation](https://cert-manager.io/docs/configuration/acme/dns01/cloudflare/).

1. Create a Cloudflare API token with permissions for "Zone:DNS:Edit" and "Zone:Read" for your domain.
2. Store the token in a Kubernetes secret:

```bash
kubectl create secret generic cloudflare-api-token-secret -n videocall \
  --from-literal=api-token=YOUR_CLOUDFLARE_API_TOKEN
```

3. Edit `helm-videocall-deployment/infrastructure/cert-manager-issuer/cloudflare-issuer.yaml` and update the two `email` attributes with your own email.   Do that now before continuing.

4. Create the Issuer:
```bash
kubectl apply -f helm-videocall-deployment/infrastructure/cert-manager-issuer/cloudflare-issuer.yaml
```
> **Note**: This configuration uses DNS01 challenge with Cloudflare. You must use an API token (not your global API key) with the correct permissions. For more details, see [cert-manager Cloudflare DNS01 documentation](https://cert-manager.io/docs/configuration/acme/dns01/cloudflare/).

### 2.3 DNS A Records

A DNS A record, or Address record, maps a domain name (like www.example.com) to a specific IPv4 address (e.g., 192.0.2.1).  You can manually create these or you can use External DNS to do it for you.  Follow one of these steps below.

#### 2.3.1 External DNS
The Videocall charts are setup to utilize External DNS (https://github.com/kubernetes-sigs/external-dns) to automatically create the necessary DNS records.  The procedure below installs and configured External DNS.

Add the necessary Helm repo:
```bash
helm repo add external-dns https://kubernetes-sigs.github.io/external-dns/
helm repo update
```

Create a new namespace for External DNS:
```bash
# Create a secret for AWS Route53 API credentials
kubectl create namespace externaldns
```

In this example we're using AWS Route 53 for DNS management.  You could use Digital Ocean, Cloudflare, or any other supported DNS provider.

Setup your AWS Credentials.  **Use your own values for the access key and secret access key.**  Create your aws credentials file (~/.aws/credentials):
```bash
[default]
aws_access_key_id = AKIAEXAMPLEKEY123456
aws_secret_access_key = AbCdEfGhIjKlMnOpQrStUvWxYz1234567890+EXAMPLE
```
Create a secret from these credentials:
```bash
kubectl create secret generic external-dns -n externaldns \
  --from-file=dns-creds=~/.aws/credentials
```

Install External DNS with AWS Route53 configuration
```bash
helm install external-dns external-dns/external-dns \
  --namespace externaldns \
  --values ./helm-videocall-deployment/infrastructure/external-dns/route53.yaml
```


#### 2.3.2 Manual DNS Configuration

External DNS can be skipped if you prefer to manage DNS records manually. In this case, you would need to create the following A/CNAME records pointing to your cluster's public IP address:

| DNS Name | Purpose | Service Type |
|---------|---------|-------------|
| webtransport.yourdomain.com | WebTransport server | LoadBalancer |
| app.yourdomain.com | UI application | Ingress |
| api.yourdomain.com | API application | Ingress |
| websocket.yourdomain.com | WebSocket server | Ingress |
| grafana.yourdomain.com | Grafana dashboard | Ingress |

In this simple K3s configuration, use the IP address of the K3s node which can usually be reviewed with the command `hostname -i`.  If you are using a cloud based VM, there may be multiple IPs that point to this host, often times one internal (private) and one external.  If your audience is coming from the internet, ensure you are using the correct IP address here.

For each DNS name in the table above, create an A record in your DNS provider for the appropriate hostname pointing to this IP.

Manual DNS configuration requires updating records whenever your service IPs change (such as after cluster redeployment), whereas External DNS handles this automatically.

## 3. Monitoring Stack (Optional)

### 3.1 Prometheus

Install Prometheus for metrics collection:

```bash
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm dependency build helm-videocall-deployment/infrastructure/prometheus
helm install prometheus helm-videocall-deployment/infrastructure/prometheus --namespace videocall
```

Key configurations to review:
- Retention period
- Storage size and class
- Scrape configurations (especially for service endpoints)

revise `./helm-videocall-deployment/infrastructure/prometheus/values.yaml` as necessary prior to install.


### 3.2 Grafana

Install Grafana for metrics visualization.

Setup the required Helm repository:

```bash
helm repo add grafana  https://grafana.github.io/helm-charts
helm dependency build helm-videocall-deployment/infrastructure/grafana
```
Use a custom value for the Grafana Admin Password:
```bash
export GRAFANA_ADMIN_USER=admin
export GRAFANA_ADMIN_PASSWORD=videocall-monitoring-2024
```
Install:
```bash
helm upgrade --install grafana  helm-videocall-deployment/infrastructure/grafana \
  --namespace videocall \
  --set grafana.adminUser=$GRAFANA_ADMIN_USER  \
  --set grafana.adminPassword=$GRAFANA_ADMIN_PASSWORD  \
  --set grafana.grafana.ini.security.admin_user=$GRAFANA_ADMIN_USER \
  --set grafana.grafana.ini.security.admin_password=$GRAFANA_ADMIN_PASSWORD
```



## 4. Video Call Application

### 4.1 NATS Message Broker

Before installing NATS, add the NATS Helm repository and build chart dependencies:

```bash
helm repo add nats https://nats-io.github.io/k8s/helm/charts/
helm dependency build helm-videocall-deployment/infrastructure/nats
```

Install NATS for application messaging:

```bash
helm install nats helm-videocall-deployment/infrastructure/nats --namespace videocall
```

This uses a very simple, non-persisted NATS configuration. 

### 4.2 Videocall Application (Unified Chart)

Install the complete Videocall application using the unified chart and your custom values file:

```bash
helm install videocall ./helm/videocall \
  --namespace videocall \
  -f my-values.yaml
```

This single command deploys all six application components:
- **UI** - Web application frontend (port 80)
- **WebSocket** - Real-time signaling server (port 8080)
- **WebTransport** - High-performance media transport (port 443 UDP)
- **Website** - Marketing site (port 80, optional)
- **Metrics Client** - Client metrics collection (port 9091)
- **Metrics Server** - Server stats tracking (port 9092)

> **Important**: The WebTransport server does not use the NGINX Ingress Controller. Instead, it's exposed directly using a Kubernetes LoadBalancer Service that handles UDP traffic required for HTTP3/WebTransport protocol. The WebTransport server handles its own TLS termination using mounted certificates, and clients connect directly to this service rather than through NGINX. This is necessary because WebTransport requires direct UDP connectivity for the QUIC protocol.

> **Note**: The WebTransport pod may restart several times before success, typically because it tries to start before cert-manager has obtained the SSL certificate. You can view the pod logs to confirm:
```bash
kubectl logs -l app.kubernetes.io/component=webtransport
```

To verify all components are deployed:
```bash
kubectl get pods -l app.kubernetes.io/instance=videocall
```

You should see pods for each enabled component with unique `app.kubernetes.io/component` labels.


## 5. Post-Installation Verification

### 5.1 Verify Services

Check that all services are running:

```bash
$ kubectl get pods,services,ingress
NAME                                                     READY   STATUS    RESTARTS   AGE
pod/grafana-c44db467f-xkztd                              1/1     Running   0          5h25m
pod/nats-0                                               3/3     Running   0          5d1h
pod/nats-box-69b79775f4-5fxgn                            1/1     Running   0          5d1h
pod/prometheus-kube-state-metrics-686d9fd46c-jx6b9       1/1     Running   0          4h18m
pod/prometheus-prometheus-node-exporter-4zrz4            1/1     Running   0          4h18m
pod/prometheus-prometheus-pushgateway-6bf748ccc9-zlhb6   1/1     Running   0          4h18m
pod/prometheus-server-58cc4bc869-m6pnk                   2/2     Running   0          4h18m
pod/videocall-metrics-client-68889cbdb6-k7sh4            1/1     Running   0          149m
pod/videocall-metrics-server-75447fcc86-nc4pq            1/1     Running   0          4h44m
pod/videocall-ui-cb7f7f5b-j8s5j                          1/1     Running   0          29h
pod/videocall-websocket-7d5685bf44-vs7sf                 1/1     Running   0          29h
pod/videocall-webtransport-6db5b5678f-pthjk              1/1     Running   0          29h

NAME                                          TYPE           CLUSTER-IP      EXTERNAL-IP      PORT(S)                                                 AGE
service/grafana                               ClusterIP      10.43.157.225   <none>           80/TCP                                                  5h25m
service/nats                                  ClusterIP      None            <none>           4222/TCP,6222/TCP,8222/TCP,7777/TCP,7422/TCP,7522/TCP   5d1h
service/prometheus-kube-state-metrics         ClusterIP      10.43.243.0     <none>           8080/TCP                                                5h31m
service/prometheus-prometheus-node-exporter   ClusterIP      10.43.143.191   <none>           9100/TCP                                                5h31m
service/prometheus-prometheus-pushgateway     ClusterIP      10.43.20.225    <none>           9091/TCP                                                5h31m
service/prometheus-server                     ClusterIP      10.43.32.88     <none>           80/TCP                                                  5h31m
service/videocall-metrics-client              ClusterIP      10.43.112.83    <none>           9091/TCP                                                4h45m
service/videocall-metrics-server              ClusterIP      10.43.18.91     <none>           9092/TCP                                                4h45m
service/videocall-ui                          ClusterIP      10.43.115.124   <none>           80/TCP                                                  47h
service/videocall-websocket                   ClusterIP      10.43.136.127   <none>           8080/TCP                                                29h
service/videocall-webtransport-lb             LoadBalancer   10.43.74.88     10.190.252.181   444:32767/TCP,443:32463/UDP                           47h

NAME                                              CLASS    HOSTS                         ADDRESS          PORTS     AGE
ingress.networking.k8s.io/grafana                 <none>   grafana.yourdomain.com        10.190.252.181   80, 443   5h25m
ingress.networking.k8s.io/prometheus-server       <none>   prometheus.yourdomain.com     10.190.252.181   80, 443   4h18m
ingress.networking.k8s.io/videocall-ui            nginx    app.yourdomain.com            10.190.252.181   80, 443   47h
ingress.networking.k8s.io/videocall-websocket     nginx    websocket.yourdomain.com      10.190.252.181   80, 443   29h
                                                           api.yourdomain.com
```

Note the naming: All videocall components now have the `videocall-` prefix and use the `app.kubernetes.io/component` label for differentiation.

### 5.2 Verify TLS Certificates

Ensure certificates are properly issued, they should be ready:

```bash
$ kubectl get certificates -n videocall
NAME                              READY   SECRET                  AGE
grafana-tls                       True    grafana-tls             5h18m
prometheus-tls                    True    prometheus-tls          4h16m
videocall-ui-tls                  True    videocall-ui-tls        47h
videocall-webtransport-cert       True    webtransport-tls        47h
websocket-tls                     True    websocket-tls           29h
api-tls                           True    api-tls                 29h
```


### 5.3 Test Connectivity

Test connectivity to the various services:
- UI: https://app.yourdomain.com   **<-- this is the main url, open it in your browser!!**
- WebSocket: wss://websocket.yourdomain.com
- WebTransport: https://webtransport.yourdomain.com
- Grafana: https://grafana.yourdomain.com

## 6. Configuration Files to Review

The following configuration files should be reviewed and customized for your deployment:

### 6.1 Videocall Application

**Your custom values file** (`my-values.yaml`):
- Domain settings in `global.domain`
- Component enablement flags (`ui.enabled`, `websocket.enabled`, etc.)
- Resource limits for each component
- Ingress hostnames
- Image tags and repositories

**Reference template**: `./helm/videocall/values.yaml` (967 lines, fully documented)
- Contains all available configuration options
- Every parameter is explained with comments
- Use this as a reference when creating your `my-values.yaml`

### 6.2 Infrastructure Components

1. **NATS**: `./helm-videocall-deployment/infrastructure/nats/values.yaml`
   - Configure authentication if needed
   - Adjust resource limits

2. **Prometheus**: `./helm-videocall-deployment/infrastructure/prometheus/values.yaml`
   - Adjust retention and storage

3. **Grafana**: `./helm-videocall-deployment/infrastructure/grafana/values.yaml`
   - Configure admin credentials
   - Set up dashboards and data sources

### Quick Configuration Example

To customize component resources in your `my-values.yaml`:

```yaml
websocket:
  replicaCount: 2
  resources:
    limits:
      cpu: "2000m"
      memory: "4Gi"
    requests:
      cpu: "1000m"
      memory: "2Gi"
```


## 7. Monitoring

Grafana is used for monitoring and visualizing metrics in this deployment.

### Accessing Grafana

1. Log in to your Grafana instance (https://grafana.yourdomain.com). The admin username and password were specified when you installed Grafana (see the `$GRAFANA_ADMIN_USER` and `$GRAFANA_ADMIN_PASSWORD` environment variables).
2. Configure Prometheus as a data source if not already configured
3. Access metrics from the Videocall components:
   - Client metrics: `videocall-metrics-client:9091/metrics`
   - Server stats: `videocall-metrics-server:9092/metrics`

Create dashboards to visualize:
- Active connections per server
- Client engagement metrics
- Media quality statistics
- Server health and resource usage

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
- Check the resource limits in the values.yaml files
- Ensure you've applied the changes with helm upgrade
- Verify the actual resource settings:
  ```bash
  kubectl get deployment -n videocall <deployment-name> -o yaml | grep -A10 resources
  ```
