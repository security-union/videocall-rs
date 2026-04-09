# VC2 Deployment Runbook

This document describes how to build, push, and deploy updated videocall images to the
`vc2.vc2.fnxlabs.com` Kubernetes cluster.

---

## Prerequisites

### 0. VPN Access

`vc2.vc2.fnxlabs.com` is on the HCL internal network. **CATO VPN must be connected** before
you can reach the machine — whether via SSH or a web browser. Ensure you are on CATO before
attempting any of the steps below.

### 1. SSH Access

Each developer has a personal account on the remote machine and your SSH public key was persisted in your `~/.ssh/authorized_keys` on the server.
SSH keys should be created on Ubuntu.

| Developer | Login |
|-----------|-------|
| Alena | `alena` |
| Anhelina | `anhelina` |
| Ilya | `ilya` |
| Palina | `palina` |

```bash
ssh ilya@vc2.vc2.fnxlabs.com
```

### 2. Docker Hub Account

You need a Docker Hub account to push images.

1. Go to [https://hub.docker.com/signup](https://hub.docker.com/signup) and create a free account. Use your personal email for this and select option for personal usage.
2. Choose a username (e.g. `ilya-hcl`). This becomes your registry prefix.
3. Once registered, log in from your local machine:

```bash
docker login -u <your-dockerhub-username>
```

You will be prompted for your Docker Hub password (or a personal access token if you have 2FA
enabled — see [https://hub.docker.com/settings/security](https://hub.docker.com/settings/security)
to create one).

Verify it works:

```bash
docker info | grep Username
```

---

## Build and Push Images

### Set your registry

Export your Docker Hub username so the build scripts know where to push:

```bash
export REGISTRY=<your-dockerhub-username>
# e.g.
export REGISTRY=ilya-hcl
```

> **Note:** If you are working from the internal HCL GitHub (`github01`) copy of the repo,
> the `cut_build_push_*.sh` scripts already use the `$REGISTRY` variable. If you are building
> from the open-source repo at [github.com/security-union/videocall-rs](https://github.com/security-union/videocall-rs),
> you will need to edit the scripts and replace the hardcoded `securityunion/` prefix with
> `${REGISTRY:-securityunion}/` yourself before running them.

### Run the build scripts

From the root of the repo:

```bash
./cut_build_push_meeting_api.sh
./cut_build_push_ui.sh
./cut_build_push_backend.sh
```

You can pass an optional tag. If you omit it, the current git commit SHA is used:

```bash
# Use a descriptive tag
./cut_build_push_meeting_api.sh fix-cors-0227

# Use the default (git commit SHA)
./cut_build_push_meeting_api.sh
```

Watch the output — each script prints the full image name and tag that was pushed, e.g.:

```
New image uploaded to ilya-hcl/videocall-meeting-api:fix-cors-0227
```

Keep that image name handy; you'll need it in the next step.

The three scripts produce the following images:

| Script | Image name |
|--------|-----------|
| `cut_build_push_meeting_api.sh` | `<REGISTRY>/videocall-meeting-api:<tag>` |
| `cut_build_push_ui.sh` | `<REGISTRY>/videocall-web-ui:<tag>` |
| `cut_build_push_backend.sh` | `<REGISTRY>/videocall-media-server:<tag>` |

---

## Deploy to the Cluster

### 1. SSH to the server

```bash
ssh ilya@vc2.vc2.fnxlabs.com
```

### 2. Navigate to the Helm directory

```bash
cd /home/videocall/videocall/helm
```

### 3. Edit the values file for the component you updated

There is a `vc2-values.yaml` override file for each service:

| Service | Values file |
|---------|-------------|
| Meeting API | `./meeting-api/vc2-values.yaml` |
| Dioxus UI | `./videocall-ui/vc2-values.yaml` |
| WebSocket backend | `./rustlemania-websocket/vc2-values.yaml` |
| WebTransport backend | `./rustlemania-webtransport/vc2-values.yaml` |

Open the relevant file and update the `image` block:

```yaml
image:
  repository: ilya-hcl/videocall-meeting-api
  pullPolicy: Always
  tag: fix-cors-0227
```

Save the file.

### 4. Run `helm upgrade`

```bash
helm upgrade meeting-api meeting-api -f meeting-api/vc2-values.yaml
```

Successful output looks like:

```
Release "meeting-api" has been upgraded. Happy Helming!
NAME: meeting-api
LAST DEPLOYED: Fri Feb 27 21:07:57 2026
NAMESPACE: videocall
STATUS: deployed
REVISION: 20
TEST SUITE: None
```

The four upgrade commands (run whichever services you updated):

```bash
helm upgrade meeting-api    meeting-api              -f meeting-api/vc2-values.yaml
helm upgrade dioxus         videocall-ui             -f videocall-ui/vc2-values.yaml
helm upgrade websocket      rustlemania-websocket    -f rustlemania-websocket/vc2-values.yaml
helm upgrade webtransport   rustlemania-webtransport -f rustlemania-webtransport/vc2-values.yaml
```

### 5. Verify the deployment

Check currently deployed releases:

```bash
helm list
```

Expected output (your revision numbers will differ):

```
NAME            NAMESPACE  REVISION  STATUS    CHART
meeting-api     videocall  20        deployed  meeting-api-0.1.0
ui              videocall  11        deployed  rustlemania-ui-0.1.0
websocket       videocall  8         deployed  rustlemania-websocket-0.1.0
webtransport    videocall  10        deployed  rustlemania-webtransport-0.1.0
```

Check that the new pod is running and has picked up your image:

```bash
kubectl get pods
kubectl describe pod <pod-name> | grep Image
```

if 'kubectl get pods' doesn't workk use commands
```
export KUBECONFIG=~/.kube/config
kubectl config set-context --current --namespace=videocall
```
and after that repeat 'kubectl get pods'.

The app is accessible at https://app.vc2.fnxlabs.com.  Remember you need Cato to access this.

---

## Quick Reference

```
# On your local machine
export REGISTRY=<your-dockerhub-username>
docker login -u $REGISTRY

./cut_build_push_meeting_api.sh <optional-tag>
./cut_build_push_ui.sh          <optional-tag>
./cut_build_push_backend.sh     <optional-tag>

# On vc2.vc2.fnxlabs.com
cd /home/videocall/videocall/helm
# Edit ./meeting-api/vc2-values.yaml   (image.repository and image.tag)
# Edit ./rustlemania-ui/vc2-values.yaml
# Edit ./rustlemania-websocket/vc2-values.yaml
# Edit ./rustlemania-webtransport/vc2-values.yaml

helm upgrade meeting-api     meeting-api             -f meeting-api/vc2-values.yaml
helm upgrade ui              rustlemania-ui          -f rustlemania-ui/vc2-values.yaml
helm upgrade websocket       rustlemania-websocket   -f rustlemania-websocket/vc2-values.yaml
helm upgrade webtransport    rustlemania-webtransport -f rustlemania-webtransport/vc2-values.yaml
```

---

## Learning Resources

If you are new to Helm and Kubernetes, the learning curve is real. Good starting points:

- [Kubernetes basics](https://kubernetes.io/docs/tutorials/kubernetes-basics/)
- [Helm quickstart](https://helm.sh/docs/intro/quickstart/)
- `kubectl cheatsheet`: `kubectl --help` or [https://kubernetes.io/docs/reference/kubectl/cheatsheet/](https://kubernetes.io/docs/reference/kubectl/cheatsheet/)

We can schedule a walkthrough session — reach out to Jay if you'd like to run through this
together the first time.
