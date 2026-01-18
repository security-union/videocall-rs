# macOS Apple Silicon Setup Guide

If you're running on a Mac with Apple Silicon (M1/M2/M3/M4), follow these steps to set up videocall-rs:

## Prerequisites

1. **Install Docker Desktop**
   - Download from https://www.docker.com/products/docker-desktop
   - Choose the Apple Silicon version
   - Install and launch Docker Desktop

## Setup Steps

### 1. Configure Docker Desktop for Apple Silicon

- Open **Docker Desktop**
- Go to **Settings → General**
- Enable **"Use Rosetta for x86/amd64 emulation on Apple Silicon"**
- Enable **"VirtioFS accelerated directory sharing"** (under Virtual Machine Options)
- Click **"Apply & Restart"**

### 2. Clone the Repository

```bash
git clone https://github.com/security-union/videocall-rs.git
cd videocall-rs
```

### 3. Pre-create Docker Volumes

Due to a known Docker Desktop bug on Apple Silicon, manually create volumes before first run:

```bash
docker volume create docker_rustlemania-actix-web-cargo-registry-cache
docker volume create docker_rustlemania-actix-web-cargo-git-cache
docker volume create docker_rustlemania-actix-web-cargo-target-cache
docker volume create docker_rustlemania-actix-web-target-cache
docker volume create docker_rustlemania-yew-ui-cargo-registry-cache
docker volume create docker_rustlemania-yew-ui-cargo-git-cache
docker volume create docker_rustlemania-yew-ui-cargo-target-cache
docker volume create docker_rustlemania-yew-ui-cache
docker volume create docker_rustlemania-actix-webtransport-cargo-registry-cache
docker volume create docker_rustlemania-actix-webtransport-cargo-git-cache
docker volume create docker_rustlemania-actix-webtransport-cargo-target-cache
docker volume create docker_rustlemania-actix-webtransport-cache
docker volume create docker_rustlemania-leptos-ui-cargo-registry-cache
docker volume create docker_rustlemania-leptos-ui-cargo-git-cache
docker volume create docker_rustlemania-leptos-ui-cargo-target-cache
docker volume create docker_rustlemania-leptos-ui-cache
docker volume create docker_rustlemania-bot-cargo-registry-cache
docker volume create docker_rustlemania-bot-cargo-git-cache
docker volume create docker_rustlemania-bot-cargo-target-cache
docker volume create docker_rustlemania-bot-cache
docker volume create docker_prometheus_data
docker volume create docker_grafana_data
```

### 4. Start the Services

```bash
make up
```

**Note:** First build takes 10-20 minutes as Rust compiles all dependencies.

### 5. Monitor Build Progress

In a separate terminal:

```bash
docker compose -f docker/docker-compose.yaml logs -f yew-ui
```

Wait for: `Serving on http://0.0.0.0:80`

### 6. Access the Application (not necessary, only for WebTransport mode)

```bash
# Launch Chrome with WebTransport support
./launch_chrome.sh

# Navigate to:
http://localhost/meeting/<your-name>/<room-name>
```

Example: `http://localhost/meeting/john/test-room`

## Useful Commands

```bash
# Check running containers
docker ps

# View logs for specific service
docker compose -f docker/docker-compose.yaml logs -f websocket-api

# Stop services
make down

# Clean up everything
make clean

# Rebuild from scratch
make rebuild-up
```

## Troubleshooting

**If you get volume creation errors:**
- Make sure you ran all the `docker volume create` commands from Step 3
- Verify Docker Desktop is running with Rosetta enabled
- Try restarting Docker Desktop

**If services won't start:**
```bash
# Check Docker is running
docker ps

# View all logs
docker compose -f docker/docker-compose.yaml logs

# Restart everything
make down
make up
```