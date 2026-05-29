# BVT Runner Setup (hcl-ci-bvt)

Dedicated EC2 instance for the bvt1 E2E smoke tests, isolated from the
main CI runner to prevent Chrome crashes from resource contention.

## Instance Spec

| Setting | Value |
|---------|-------|
| Instance type | `c7a.xlarge` (4 vCPU, 8 GB RAM) |
| AMI | RHEL 9 (same HCL hardened image as videocallci) |
| Volume | 100 GB gp3 |
| Security group | Same as videocallci (outbound all, inbound SSH from VPN) |
| Hostname | `videocallci-bvt.fnxlabs.com` (or similar) |

## Setup Steps

### 1. Base packages

```bash
# Docker
dnf config-manager --add-repo https://download.docker.com/linux/rhel/docker-ce.repo
dnf install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin
systemctl enable --now docker

# Node.js 22
dnf module install -y nodejs:22
npm install -g npm@latest

# Chromium (for wasm-bindgen-test if needed later)
dnf install -y epel-release
dnf install -y chromium

# Build deps for Rust crates
dnf install -y alsa-lib-devel clang-devel clang-libs openssl-devel
```

### 2. Rust toolchain

```bash
mkdir -p /var/lib/ci/{cargo,rustup}
export CARGO_HOME=/var/lib/ci/cargo
export RUSTUP_HOME=/var/lib/ci/rustup

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
source /var/lib/ci/cargo/env

rustup target add wasm32-unknown-unknown
cargo install wasm-pack
cargo install wasm-bindgen-cli --version 0.2.108
```

### 3. Runner user

```bash
useradd -r -m -d /var/lib/ghrunner ghrunner
usermod -aG docker ghrunner
```

### 4. GitHub Actions runner

```bash
mkdir -p /var/lib/ghrunner/actions-runner
cd /var/lib/ghrunner/actions-runner

# Download latest runner (check github01 for version)
curl -o actions-runner.tar.gz -L https://github01.hclpnp.com/labs-projects/videocall/actions/runners/download/linux-x64
tar xzf actions-runner.tar.gz
rm actions-runner.tar.gz

# Get registration token
TOKEN=$(GH_HOST=github01.hclpnp.com gh api -X POST repos/labs-projects/videocall/actions/runners/registration-token --jq '.token')

# Register with the bvt label
./config.sh --unattended \
  --url https://github01.hclpnp.com/labs-projects/videocall \
  --token "$TOKEN" \
  --name videocallci-bvt \
  --labels self-hosted,linux,x64,hcl-ci-bvt \
  --work /var/lib/ghrunner/_work

# SELinux context
chcon -t bin_t /var/lib/ghrunner/actions-runner/bin/*
chcon -t bin_t /var/lib/ghrunner/actions-runner/*.sh

# Install and start service
RUNNER_ALLOW_RUNASROOT=1 ./svc.sh install ghrunner
systemctl enable --now actions.runner._services.videocallci-bvt.service
```

### 5. PATH setup

Create `/etc/profile.d/ci-cargo.sh`:
```bash
export CARGO_HOME=/var/lib/ci/cargo
export RUSTUP_HOME=/var/lib/ci/rustup
export PATH=/var/lib/ci/cargo/bin:$PATH
```

### 6. Verify

```bash
sudo -u ghrunner /var/lib/ci/cargo/bin/rustc --version
sudo -u ghrunner docker info
sudo -u ghrunner node --version
systemctl status actions.runner._services.videocallci-bvt.service
```

Check the runner appears at:
https://github01.hclpnp.com/labs-projects/videocall/settings/actions/runners

With labels: `self-hosted`, `linux`, `x64`, `hcl-ci-bvt`

## Workflow Change

The bvt1 workflow (`pr-check-e2e-smoke-hcl.yaml`) uses:
```yaml
runs-on: [self-hosted, linux, x64, hcl-ci-bvt]
```

This pins it to this dedicated runner. The full E2E suite remains on
`hcl-ci` (the main 16-CPU runner).

## Concurrency

Two levels of concurrency control:

- **Workflow-level**: `pr-check-e2e-smoke-${{ PR_number }}` with `cancel-in-progress: true`.
  Handles same-PR supersession — a developer pushes a new commit, the old run for
  that PR is cancelled immediately.

- **Job-level**: `hcl-ci-bvt` with `cancel-in-progress: false`.
  Serializes access to the dedicated runner across different PRs. If PR A's bvt1
  is running and PR B's bvt1 starts, B queues until A finishes (~2 min) rather
  than cancelling A's unrelated run.
