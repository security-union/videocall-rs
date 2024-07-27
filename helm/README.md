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
1. Deploy rustlemania without SSL
1. Deploy cert-manager
1. Create a cert-manager issuer
1. Upgrade rustlemania to include SSL
1. Install kubernetes dashboard through the digital ocean marketplace 
1. Start the kubernetes dashboard locally 
```kubectl -n kubernetes-dashboard port-forward svc/kubernetes-dashboard-kong-proxy 8443:443```
