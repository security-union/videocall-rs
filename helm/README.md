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

## Deploying to production

Production is one cluster, `videocall-us-east`. Five charts there are pinned: meeting-api, webtransport, websocket, videocall-ui and videocall-website. Each `helm/global/us-east/<chart>/values.yaml` names its image as `tag: main-<sha8>@sha256:<digest>`, so the pin is the exact image bytes, and `pullPolicy` is `IfNotPresent`. Those five never follow `latest`. Other charts in the cluster, such as metrics-api and engineering-vlog, still run `:latest`.

1. A merge to main builds images tagged `main-<sha8>`. The publish workflows are path-filtered, so not every merge builds every image.
1. When a build finishes, `.github/workflows/pin-images-pr.yaml` runs `helm-image-pin pin` and opens one rolling PR, `deploy: pin images to main-<sha8>`, listing the old and new pins with a compare link.
1. Merging that PR runs `.github/workflows/deploy-production.yaml`. It upgrades meeting-api, webtransport, websocket, videocall-ui and videocall-website in that order. After each rollout it waits until that service reports the pinned sha on `https://api.videocall.rs/api/v1/versions` or `https://app.videocall.rs/version.json` before it starts the next one. The website has no version endpoint, so it only gets the rollout check.

Only a change to one of those five values files starts a deploy. Leave the pin PR alone and nothing ships. A PR that changes a chart (its `Chart.yaml`, templates, or the `helm/<chart>` dependency) deploys nothing when it merges. The chart change goes out with the next pin merge, because `helm upgrade` applies the whole chart, or when someone runs the workflow by hand.

To ship a specific build, change the pin by hand in a PR. `helm-image-pin pin --check` prints the newest builds without writing anything. If a step fails, the workflow stops before the next release and rolls nothing back, so check `helm history <release>` and decide.

### The `helm-image-pin` tool

The pin and preflight checks live in [`helm-image-pin`](../helm-image-pin/README.md), and which chart runs which image, in which order, is written once in [`helm-image-pin.toml`](../helm-image-pin.toml). The tool runs only in jobs without secrets. The deploy job gets a plan file from it, checks that file against `helm-image-pin.toml`, and runs helm, kubectl, curl and jq itself. A PR that touches `helm/` or the config runs the same preflight as a check, through `deploy-preflight.yaml`.
