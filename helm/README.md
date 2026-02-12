# Deploying with helm to kubernetes

1. Create a cluster
1. Label the nodes in the node pool
    ```
    kubectl label nodes <node-1> <node-2> <node-3> node-role=worker
    ```
1. Deploy ingress-nginx
1. Install external-DNS
    ```
    helm repo add bitnami https://charts.bitnami.com/bitnami
    helm install external-dns bitnami/external-dns \
        --set provider=digitalocean \
        --set digitalocean.apiToken=YOUR_DIGITALOCEAN_API_TOKEN
    ```
1. Deploy internal nats
1. Create an opaque secret named "rusltemania" with the key postgres-password filled in with a random password
1. Create required Kubernetes secrets:
    ```bash
    # JWT secret — shared between meeting-api and media server (websocket/webtransport).
    # Both services must use the same secret so room access tokens can be signed
    # by meeting-api and verified by the media server.
    kubectl create secret generic jwt-secret \
        --from-literal=secret="$(openssl rand -base64 32)"

    # OAuth credentials — used by meeting-api for OIDC login.
    # For confidential clients (e.g. Google), include client-secret.
    # For public clients (e.g. Okta with PKCE), omit client-secret.
    kubectl create secret generic oauth-credentials \
        --from-literal=client-id='YOUR_OAUTH_CLIENT_ID' \
        --from-literal=client-secret='YOUR_OAUTH_CLIENT_SECRET'

    # Postgres credentials (if not already created with the "rusltemania" secret above)
    kubectl create secret generic postgres-credentials \
        --from-literal=password='YOUR_POSTGRES_PASSWORD'
    ```
    See [OAuth Helm Configuration](../docs/OAUTH_HELM_CONFIGURATION.md) for provider-specific setup (Google, Okta, etc.).
1. Deploy rustlemania without SSL
1. Deploy cert-manager
1. Create a cert-manager issuer
1. Upgrade rustlemania to include SSL
1. Install kubernetes dashboard through the digital ocean marketplace 
1. Start the kubernetes dashboard locally 
```kubectl -n kubernetes-dashboard port-forward svc/kubernetes-dashboard-kong-proxy 8443:443```

## Updating the website

1. Update the tag in the videocall-website/values.yaml file
1. Run ```helm dependency update && helm upgrade videocall-website . -f values.yaml```
