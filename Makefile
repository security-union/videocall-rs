COMPOSE_IT := docker/docker-compose.integration.yaml
COMPOSE_E2E := docker compose -p videocall-e2e -f docker/docker-compose.e2e.yaml
COMPOSE := docker compose --env-file .env -f docker/docker-compose.yaml

# Images the Playwright E2E stack needs (subset of release.nix images.*)
E2E_IMAGES := meeting-api websocket-server dioxus-ui

SQLITE_TEST_DB := /tmp/meeting_api_test.sqlite3

.PHONY: images shell pins-update dev dev-middleware up down \
	dev-websocket dev-webtransport dev-meeting-api dev-metrics dev-server-stats dev-ui dev-website \
	tests_run tests_down tests_sqlite_run connect_to_db connect_to_nats \
	clippy-fix fmt check clean clean-docker \
	e2e e2e-headed e2e-debug e2e-interop e2e-lint e2e-fmt e2e-install e2e-up e2e-down e2e-build e2e-ci

# ---------------------------------------------------------------------------
# Nix-built Docker images (see release.nix / docs/nix-architecture.md).
# Each image attr is a streamLayeredImage script: running it streams the
# tarball into `docker load`. Same flow locally and in CI.
# ---------------------------------------------------------------------------

# Build + load every app image
images:
	@out=$$(nix-build release.nix -A images.all --no-out-link); \
	for img in $$out/*; do \
		echo "==> loading $$(basename $$img)"; \
		"$$img" | docker load; \
	done

# Build + load one image: make image-websocket-server
image-%:
	@script=$$(nix-build release.nix -A images.$* --no-out-link); \
	"$$script" | docker load

# ---------------------------------------------------------------------------
# Dev shells & dependency pins
# ---------------------------------------------------------------------------

# Pinned toolchain shell (optional for dev; rustup works too)
shell:
	nix-shell

# Refresh nixtamal pins (nix/tamal/) — refreshes non-frozen inputs and relocks
pins-update:
	nix-shell -p nixtamal git --run "nixtamal refresh"

# ---------------------------------------------------------------------------
# Stacks
# ---------------------------------------------------------------------------

# Auto-create .env from sample on first run so --env-file never fails
.env:
	@echo "No .env found — creating from docker/.env-sample. Edit it before running make up."
	cp docker/.env-sample .env

# THE dev loop: the whole stack native under process-compose — postgres, nats,
# prometheus, grafana from nixpkgs, every service under cargo-watch, trunk on
# :3001. Zero Docker. Health-gated deps, TUI. State in .data/ (gitignored).
dev:
	nix-shell default.nix -A shells.dev --run dev-stack

# Just the native middleware (hack on a single service with `make dev-<svc>`)
dev-middleware:
	nix-shell default.nix -A shells.dev --run "dev-stack postgres postgres-init nats prometheus grafana"

# Full stack from Nix-built images (Docker)
up: .env images
	$(COMPOSE) up

down: .env
	$(COMPOSE) down

# ---------------------------------------------------------------------------
# Single-service watchers — run one service on the host with hot reload
# against already-running middleware (`make dev-middleware`, or the full
# `make dev` TUI which supervises these itself). Needs cargo + cargo-watch on
# PATH (e.g. via `make shell` / nix-shell default.nix -A shells.backend-dev, or rustup).
# ---------------------------------------------------------------------------

DEV_ENV := NATS_URL=localhost:4222 \
	DATABASE_URL="postgres://postgres:docker@localhost:5432/actix-api-db?sslmode=disable" \
	JWT_SECRET=dev-jwt-secret-change-me \
	RUST_LOG=debug,async_nats=info

dev-websocket:
	$(DEV_ENV) ACTIX_PORT=8080 UI_ENDPOINT=http://localhost:3001 \
	DATABASE_ENABLED=false SERVICE_TYPE=websocket REGION=us-east SERVER_ID=server-1 \
	cargo watch -x 'run --bin websocket_server'

dev-webtransport:
	cd actix-api && $(DEV_ENV) \
	LISTEN_URL=0.0.0.0:4433 HEALTH_LISTEN_URL=0.0.0.0:5321 \
	CERT_PATH=certs/localhost.pem KEY_PATH=certs/localhost.key \
	SERVICE_TYPE=webtransport REGION=us-east SERVER_ID=server-1 RUST_LOG=info \
	cargo watch -x 'run --bin webtransport_server'

dev-meeting-api:
	cd dbmate && DATABASE_URL="postgres://postgres:docker@localhost:5432/actix-api-db?sslmode=disable" \
		dbmate wait && DATABASE_URL="postgres://postgres:docker@localhost:5432/actix-api-db?sslmode=disable" dbmate up
	$(DEV_ENV) LISTEN_ADDR=0.0.0.0:8081 \
	TOKEN_TTL_SECS=60 COOKIE_SECURE=false \
	AFTER_LOGIN_URL=http://localhost:3001 ALLOWED_REDIRECT_URLS=http://localhost:3001 \
	cargo watch -x 'run --bin meeting-api'

dev-metrics:
	$(DEV_ENV) METRICS_PORT=9091 SERVICE_TYPE=client-metrics REGION=us-east SERVER_ID=server-1 \
	cargo watch -x 'run --bin metrics_server'

dev-server-stats:
	$(DEV_ENV) METRICS_PORT=9092 SERVICE_TYPE=server-stats REGION=us-east SERVER_ID=server-1 \
	cargo watch -x 'run --bin metrics_server_snapshot'

# Frontend with hot reload (trunk serve on :3001). Needs trunk + tailwindcss
# (nix-shell default.nix -A shells.frontend provides the pinned versions).
dev-ui:
	./docker/start-dioxus.sh

dev-website:
	cd leptos-website && npm install && LEPTOS_SITE_ADDR=0.0.0.0:4600 cargo leptos watch

# ---------------------------------------------------------------------------
# Integration tests — middleware in compose, tests native in the pinned shell
# ---------------------------------------------------------------------------

tests_run:
	docker compose -f $(COMPOSE_IT) up -d --wait postgres nats
	NATS_URL=localhost:4222 \
	DATABASE_URL="postgres://postgres:docker@localhost:5432/actix-api-db?sslmode=disable" \
	DATABASE_ENABLED=true \
	WEBTRANSPORT_URL=https://127.0.0.1:4433 \
	HEALTH_URL=http://127.0.0.1:5321/healthz \
	INSECURE=true \
	LISTEN_URL=0.0.0.0:4433 \
	HEALTH_LISTEN_URL=0.0.0.0:5321 \
	CERT_PATH=certs/localhost.pem \
	KEY_PATH=certs/localhost.key \
	JWT_SECRET=test-secret-for-integration-tests \
	RUST_LOG=info \
	nix-shell default.nix -A shells.backend-dev --run "\
		set -euo pipefail && \
		(cd dbmate && dbmate wait && dbmate up) && \
		cargo clippy --all -- -D warnings && \
		cargo fmt --all --check && \
		cargo test -p videocall-api -- --nocapture --test-threads=1 && \
		cargo test -p meeting-api -- --nocapture --test-threads=1"

tests_down:
	docker compose -f $(COMPOSE_IT) down -v

# Run the meeting-api integration tests against a local SQLite database.
# Requires the `dbmate` binary on PATH. Serialized (--test-threads=1) because
# tests share the single SQLite file.
tests_sqlite_run:
	rm -f $(SQLITE_TEST_DB) $(SQLITE_TEST_DB)-wal $(SQLITE_TEST_DB)-shm
	cd dbmate/sqlite && DATABASE_URL="sqlite:$(SQLITE_TEST_DB)" dbmate up
	DATABASE_URL="sqlite:$(SQLITE_TEST_DB)" \
		cargo test -p meeting-api --no-default-features --features sqlite -- --test-threads=1

# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------

connect_to_db: .env
	$(COMPOSE) run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

connect_to_nats: .env
	$(COMPOSE) exec nats-box sh

# Lint/format natively in the pinned shell (same toolchain as CI)
clippy-fix:
	nix-shell default.nix -A shells.backend-dev --run "cargo clippy --all --fix --allow-dirty --allow-staged"

fmt:
	nix-shell default.nix -A shells.backend-dev --run "cargo fmt --all"

check:
	nix-shell default.nix -A shells.backend-dev --run "cargo clippy --all -- --deny warnings && cargo fmt --all --check"

clean: .env
	$(COMPOSE) down --remove-orphans --volumes --rmi all

# Clean stale Docker resources (networks, containers)
clean-docker: .env
	$(COMPOSE) down --remove-orphans
	docker network prune -f

# ---------------------------------------------------------------------------
# E2E tests (Playwright)
# ---------------------------------------------------------------------------

# Install e2e dependencies and Playwright browsers
e2e-install:
	cd e2e && npm ci && npx playwright install chromium

# Build + load the Nix images the E2E stack runs (same derivations CI publishes)
e2e-build:
	@for name in $(E2E_IMAGES); do \
		echo "==> building image $$name"; \
		script=$$(nix-build release.nix -A images.$$name --no-out-link) || exit 1; \
		"$$script" | docker load || exit 1; \
	done

# Start the E2E stack (postgres, nats, meeting-api, websocket-api, dioxus-ui)
e2e-up:
	$(COMPOSE_E2E) up -d

# Tear down the E2E stack and remove volumes
e2e-down:
	$(COMPOSE_E2E) down -v

# Run e2e tests headless (assumes stack is already up)
#   make e2e                        — all tests
#   make e2e SPEC=two-users-meeting — single spec (without .spec.ts)
e2e:
	cd e2e && npx playwright test $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Run e2e tests with visible browsers (assumes stack is already up)
#   make e2e-headed                        — all tests
#   make e2e-headed SPEC=two-users-meeting — single spec
e2e-headed:
	cd e2e && npx playwright test --headed $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Run e2e tests in debug mode (step through in Playwright Inspector)
e2e-debug:
	cd e2e && npx playwright test --debug $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Run the WebCodecs VP9 interop gate standalone — needs no docker stack.
# Regenerates the fixture from the current encoder, then decodes it in Chromium.
e2e-interop:
	cargo run -p videocall-codecs --example dump_vp9_ivf --features test-utils -- e2e/fixtures/pure_rust_vp9.ivf
	cd e2e && E2E_SKIP_SERVICE_WAIT=1 npx playwright test tests/webcodecs-vp9-interop.spec.ts

# Full CI pipeline: build images, start stack, run tests, tear down
e2e-ci: e2e-build e2e-install
	$(COMPOSE_E2E) up -d
	cd e2e && npx playwright test; E2E_EXIT=$$?; cd .. && $(COMPOSE_E2E) down -v; exit $$E2E_EXIT

# Lint + format check + typecheck (same as CI)
e2e-lint:
	cd e2e && npm run ci:lint

# Auto-fix lint and formatting issues
e2e-fmt:
	cd e2e && npm run lint:fix && npm run format:fix
