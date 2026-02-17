COMPOSE_IT := docker/docker-compose.integration.yaml

.PHONY: tests_up test up down build connect_to_db connect_to_nats clippy-fix fmt check clean clean-docker rebuild rebuild-up dioxus-ui dioxus-ui-only dioxus-tests-docker yew-ui yew-tests yew-tests-docker

tests_run:
	docker compose -f $(COMPOSE_IT) up -d && docker compose -f $(COMPOSE_IT) run --rm rust-tests bash -c "\
		cd /app/dbmate && dbmate wait && dbmate up && \
		cd /app/actix-api && \
		cargo clippy -- -D warnings && \
		cargo fmt --check && \
		cargo machete && \
		cargo test -p videocall-api -- --nocapture --test-threads=1 && \
		cargo test -p meeting-api -- --nocapture --test-threads=1"

tests_build:
	docker compose -f $(COMPOSE_IT) build

tests_down:
	docker compose -f $(COMPOSE_IT) down -v

up:
		docker compose -f docker/docker-compose.yaml up
down:
		docker compose -f docker/docker-compose.yaml down
build:
		docker compose -f docker/docker-compose.yaml build

connect_to_db:
		docker compose -f docker/docker-compose.yaml run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

connect_to_nats:
	docker compose -f docker/docker-compose.yaml exec nats-box sh

clippy-fix:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app && cargo clippy --fix"

fmt:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app && cargo fmt"

check: 
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app && cargo clippy --all  -- --deny warnings && cargo fmt --check"

clean:
		docker compose -f docker/docker-compose.yaml down --remove-orphans \
			--volumes --rmi all

# Clean stale Docker resources (networks, containers)
clean-docker:
		docker compose -f docker/docker-compose.yaml down --remove-orphans
		docker network prune -f

# Rebuild all images from scratch (use after Dockerfile changes or for ARM64 migration)
rebuild:
		docker compose -f docker/docker-compose.yaml build --no-cache

# Rebuild and start (fresh build + run)
rebuild-up:
		docker compose -f docker/docker-compose.yaml build --no-cache
		docker compose -f docker/docker-compose.yaml up

# ---------------------------------------------------------------------------
# Yew UI component tests
# ---------------------------------------------------------------------------

# Native: run tests locally using whatever Chrome/chromedriver is available.
# Works on macOS, Linux, and Windows (with Chrome installed).
# Auto-detects chromedriver from PATH, brew, or common install locations.
#   make yew-tests            — headless (default)
#   make yew-tests HEADED=1   — opens a visible browser so you can watch
yew-tests:
	@DRIVER=""; \
	if command -v chromedriver >/dev/null 2>&1; then \
		DRIVER=$$(command -v chromedriver); \
	elif [ -f /usr/local/bin/chromedriver ]; then \
		DRIVER=/usr/local/bin/chromedriver; \
	elif [ -f /tmp/chromedriver-mac-arm64/chromedriver ]; then \
		DRIVER=/tmp/chromedriver-mac-arm64/chromedriver; \
	elif [ -f /tmp/chromedriver-mac-x64/chromedriver ]; then \
		DRIVER=/tmp/chromedriver-mac-x64/chromedriver; \
	fi; \
	if [ -z "$$DRIVER" ]; then \
		echo "ERROR: chromedriver not found."; \
		echo "  macOS:  brew install chromedriver"; \
		echo "  Linux:  sudo apt-get install chromium-chromedriver"; \
		echo "  Or set: CHROMEDRIVER=/path/to/chromedriver make yew-tests"; \
		exit 1; \
	fi; \
	echo "Using chromedriver at $$DRIVER"; \
	if [ -n "$(HEADED)" ]; then \
		echo "Running in HEADED mode — a browser window will open"; \
		cd yew-ui && NO_HEADLESS=1 CHROMEDRIVER=$$DRIVER cargo test --target wasm32-unknown-unknown; \
	else \
		cd yew-ui && CHROMEDRIVER=$$DRIVER cargo test --target wasm32-unknown-unknown; \
	fi

# Docker: run tests in a container with Chromium pre-installed (no local deps needed).
# Builds the dev image (which includes Chromium) and mounts the project for testing.
yew-tests-docker:
	docker build -f docker/Dockerfile.yew -t yew-dev docker
	docker run --rm -v "$$(pwd):/app" -w /app/yew-ui yew-dev \
		bash -c "CHROMEDRIVER=/usr/bin/chromedriver cargo test --target wasm32-unknown-unknown"

# ---------------------------------------------------------------------------
# Dioxus UI
# ---------------------------------------------------------------------------

# Start dioxus-ui with required backend services
dioxus-ui:
	docker compose -f docker/docker-compose.yaml up dioxus-ui websocket-api meeting-api postgres nats

# Start just dioxus-ui (assumes backend is already running)
dioxus-ui-only:
	docker compose -f docker/docker-compose.yaml up dioxus-ui

# Run dioxus-ui tests in Docker
dioxus-tests-docker:
	docker build -f docker/Dockerfile.yew -t dioxus-dev docker
	docker run --rm -v "$$(pwd):/app" -w /app/dioxus-ui dioxus-dev \
		bash -c "CHROMEDRIVER=/usr/bin/chromedriver cargo test --target wasm32-unknown-unknown"

# ---------------------------------------------------------------------------
# Yew UI
# ---------------------------------------------------------------------------

# Start yew-ui with required backend services
yew-ui:
	docker compose -f docker/docker-compose.yaml up yew-ui websocket-api meeting-api postgres nats