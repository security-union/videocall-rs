# Deploying with helm to kubernetes

1. Create a cluster
1. Label the nodes in the node pool
    ```
    kubectl label nodes <node-1> <node-2> <node-3> node-role=worker
    ```
1. Deploy ingress-nginx
1. Setup DNS records with the ingress-nginx external IP
1. Deploy internal nats and postgres
1. Deploy rustlemania without SSL
1. Deploy cert-manager
1. Create a cert-manager issuer
1. Upgrade rustlemania to include SSL
