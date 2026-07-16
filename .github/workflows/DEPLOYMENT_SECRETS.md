# GitHub Actions Secrets for Deployment Workflows

This document describes the GitHub Actions secrets required for the daily deployment workflows.

## Required Secrets

### `HCL_DAILY_KUBECONFIG`

**Used by:** `daily-deploy-hcl.yaml`  
**Description:** Base64-encoded kubeconfig for the HCL daily k3s cluster at `videocall.fnxlabs.com`

**How to generate:**

```bash
# Extract the kubeconfig from the HCL daily cluster
ssh videocallci.fnxlabs.com "cat /home/jenkins/.kube/k3s-config" | base64 -w0
```

The kubeconfig should point to the cluster API endpoint at `https://10.190.252.83:8443`.

### `LABSWORKSPACE_KUBECONFIG`

**Used by:** `daily-deploy-labsworkspace.yaml`  
**Description:** Base64-encoded kubeconfig for the labsworkspace k3s cluster at `k3s.labsworkspace.fnxlabs.com`

**How to generate:**

```bash
# Extract the kubeconfig from the labsworkspace cluster and base64 encode it
ssh k3s.labsworkspace.fnxlabs.com "cat /etc/rancher/k3s/k3s.yaml" \
  | sed 's|https://127.0.0.1:6443|https://k3s.labsworkspace.fnxlabs.com:6443|' \
  | base64 -w0
```

The kubeconfig should point to the cluster API endpoint at `https://k3s.labsworkspace.fnxlabs.com:6443`.

### `ASCEND_KUBECONFIG`

**Used by:** `daily-deploy-ascend.yaml`  
**Description:** Base64-encoded kubeconfig for the Ascend cluster at `conceptcar7.com`

**How to generate:**

```bash
# From the location where the Ascend kubeconfig is stored
# Replace /path/to/your/ascend-cluster-config with your actual path
cat /path/to/your/ascend-cluster-config | base64 -w0
```

The kubeconfig should point to the cluster API endpoint at `https://10.244.8.238:6443`.

### `HARBOR_USERNAME`

**Used by:** All three deployment workflows  
**Description:** Username for Harbor container registry authentication at `hclcr.io`

### `HARBOR_PASSWORD`

**Used by:** All three deployment workflows  
**Description:** Password for Harbor container registry authentication at `hclcr.io`

### `GCHAT_WEBHOOK_URL`

**Used by:** HCL and labsworkspace workflows (optional)  
**Description:** Google Chat webhook URL for deployment notifications

If not set, deployment notifications will be skipped (non-fatal).

## Setting Secrets

Secrets can be set in the GitHub repository settings:

1. Navigate to: **Settings** → **Secrets and variables** → **Actions**
2. Click **New repository secret**
3. Enter the secret name and value
4. Click **Add secret**

## Security Notes

- All kubeconfigs contain cluster authentication credentials and should be treated as highly sensitive
- Kubeconfigs are decoded at workflow runtime and stored temporarily in `~/.kube/` with `600` permissions
- The kubeconfig files are cleaned up when the workflow job completes via an `if: always()` cleanup step in each workflow
- Harbor credentials are used for both authentication to the API and docker login

## Validation

After setting the secrets, you can validate them by running the deployment workflows manually via `workflow_dispatch`:

- **HCL Daily:** Manually trigger `daily-deploy-hcl.yaml` with a known image tag
- **labsworkspace:** Manually trigger `daily-deploy-labsworkspace.yaml` with a known image tag
- **Ascend:** Manually trigger `daily-deploy-ascend.yaml` with a known image tag

All three workflows will validate kubeconfig access in the "Validate kubectl and kubeconfig access" step before attempting deployment.
