# videocall-rs — everything is driven from here (see `make help`).
#
# Conventions (GNU-style):
#   make            -> help (nothing builds implicitly)
#   make all        -> build every artifact (nix packages + docker images)
#   make check      -> lint + backend + UI test suites (see check-* for pieces)
#   make clean      -> remove disposable state; distclean -> pristine tree
# Configuration variables below are ?=-assignable: `make check-backend IT_PG_PORT=6543`.

SHELL := bash
.DELETE_ON_ERROR:
.DEFAULT_GOAL := help

# ---------------------------------------------------------------------------
# Configuration (override with make VAR=… or environment)
# ---------------------------------------------------------------------------

COMPOSE ?= docker compose --env-file .env -f docker/docker-compose.yaml

# Each native stack owns a process-compose instance on its own port and a
# disposable data dir, so dev / integration / e2e can all run at once.
DEV_PC_PORT      ?= 28080
IT_PC_PORT       ?= 28081
IT_PG_PORT       ?= 15432
IT_NATS_PORT     ?= 14222
IT_NATS_MON_PORT ?= 18222
IT_DATA          ?= /tmp/videocall-it-data
E2E_PC_PORT      ?= 28082
E2E_DATA         ?= /tmp/videocall-e2e-data
SQLITE_TEST_DB   ?= /tmp/meeting_api_test.sqlite3

# On macOS the nokhwa capture crates shell out to Swift/Xcode, which the pinned
# nix-shell's Apple-SDK env breaks. Exclude them from the workspace clippy
# there (the pre-push hook still lints them with the system toolchain); Linux
# CI keeps full --workspace coverage.
CLIPPY_EXCLUDES := $(shell [ "$$(uname -s)" = "Darwin" ] && echo "--exclude videocall-cli --exclude videocall-nokhwa --exclude videocall-nokhwa-bindings-macos --exclude videocall-nokhwa-core")

.PHONY: help all ensure-nix images shell pins-update \
	dev dev-middleware dev-down status vlog up down \
	dev-websocket dev-webtransport dev-meeting-api dev-metrics dev-server-stats dev-ui dev-website \
	check lint lint-fix fmt \
	check-backend check-backend-down check-backend-sqlite check-ui check-e2e \
	e2e e2e-headed e2e-debug e2e-interop e2e-lint e2e-fmt e2e-install e2e-up e2e-down e2e-build \
	db-shell nats-shell clean clean-docker distclean \
	tests_run tests_down tests_sqlite_run connect_to_db connect_to_nats clippy-fix e2e-ci

##@ General

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"} \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } \
		/^[a-zA-Z0-9_%-]+:.*?##/ { printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@printf "\nDeprecated aliases still work but warn: tests_run, tests_down, tests_sqlite_run,\nconnect_to_db, connect_to_nats, clippy-fix, e2e-ci.\n"

all: images ## Build every artifact (all nix packages + docker images)

# Every nix-backed target depends on this: if Nix is missing, install it with
# the official installer (nixos.org — macOS, Linux, and Windows via WSL2).
# Linux without systemd (e.g. default WSL2) gets the single-user install.
ensure-nix:
	@if ! command -v nix-shell >/dev/null 2>&1; then \
		echo "Nix not found — installing via the official installer (nixos.org)..."; \
		if [ "$$(uname -s)" = "Linux" ] && [ ! -d /run/systemd/system ]; then \
			curl -L https://nixos.org/nix/install | sh -s -- --no-daemon; \
		else \
			curl -L https://nixos.org/nix/install | sh -s -- --daemon; \
		fi; \
		echo ""; \
		echo "Nix installed. Open a NEW terminal (so PATH picks it up) and re-run your make command."; \
		exit 1; \
	fi

##@ Development (native, no Docker)

# THE dev loop: the whole stack native under process-compose — postgres, nats,
# prometheus, grafana from nixpkgs, every service under cargo-watch, trunk on
# :3001. Health-gated deps, TUI. State in .data/ (gitignored).
dev: ensure-nix ## Run the whole dev stack (TUI, hot reload); stop with F10 or dev-down
	nix-shell default.nix -A shells.dev --run dev-stack

dev-middleware: ensure-nix ## Just postgres/nats/prometheus/grafana (pair with dev-<service>)
	nix-shell default.nix -A shells.dev --run "dev-stack postgres postgres-init nats prometheus grafana"

dev-down: ensure-nix ## Stop a running `make dev` stack from another terminal
	nix-shell default.nix -A shells.dev --run "process-compose down -p $${PC_PORT_NUM:-$(DEV_PC_PORT)}"

status: ## Show which native stacks are running (dev / integration / e2e)
	@for inst in "dev:$(DEV_PC_PORT)" "integration:$(IT_PC_PORT)" "e2e:$(E2E_PC_PORT)"; do \
		name=$${inst%%:*}; port=$${inst##*:}; \
		if (exec 3<>/dev/tcp/127.0.0.1/$$port) 2>/dev/null; then \
			echo "$$name stack: RUNNING (process-compose API on :$$port)"; \
		else \
			echo "$$name stack: stopped"; \
		fi; \
	done

vlog: ensure-nix ## Serve the engineering vlog locally with live reload (pinned zola)
	nix-shell default.nix -A shells.vlog --run "cd engineering-vlog && zola serve --interface 127.0.0.1 --port 1111"

##@ Single-service watchers (against `make dev-middleware`)

# Run one service on the host with hot reload. Needs cargo + cargo-watch on
# PATH (e.g. via `make shell` / nix-shell default.nix -A shells.backend-dev, or rustup).

DEV_ENV := NATS_URL=localhost:4222 \
	DATABASE_URL="postgres://postgres:docker@localhost:5432/actix-api-db?sslmode=disable" \
	JWT_SECRET=dev-jwt-secret-change-me \
	RUST_LOG=debug,async_nats=info

dev-websocket: ## Watch the websocket server (:8080)
	$(DEV_ENV) ACTIX_PORT=8080 UI_ENDPOINT=http://localhost:3001 \
	DATABASE_ENABLED=false SERVICE_TYPE=websocket REGION=us-east SERVER_ID=server-1 \
	cargo watch -x 'run --bin websocket_server'

dev-webtransport: ## Watch the webtransport server (udp :4433)
	cd actix-api && $(DEV_ENV) \
	LISTEN_URL=0.0.0.0:4433 HEALTH_LISTEN_URL=0.0.0.0:5321 \
	CERT_PATH=certs/localhost.pem KEY_PATH=certs/localhost.key \
	SERVICE_TYPE=webtransport REGION=us-east SERVER_ID=server-1 RUST_LOG=info \
	cargo watch -x 'run --bin webtransport_server'

dev-meeting-api: ## Watch the meeting REST/auth API (:8081)
	cd dbmate && DATABASE_URL="postgres://postgres:docker@localhost:5432/actix-api-db?sslmode=disable" \
		dbmate wait && DATABASE_URL="postgres://postgres:docker@localhost:5432/actix-api-db?sslmode=disable" dbmate up
	$(DEV_ENV) LISTEN_ADDR=0.0.0.0:8081 \
	TOKEN_TTL_SECS=60 COOKIE_SECURE=false \
	AFTER_LOGIN_URL=http://localhost:3001 ALLOWED_REDIRECT_URLS=http://localhost:3001 \
	cargo watch -x 'run --bin meeting-api'

dev-metrics: ## Watch the client-metrics server (:9091)
	$(DEV_ENV) METRICS_PORT=9091 SERVICE_TYPE=client-metrics REGION=us-east SERVER_ID=server-1 \
	cargo watch -x 'run --bin metrics_server'

dev-server-stats: ## Watch the server-stats server (:9092)
	$(DEV_ENV) METRICS_PORT=9092 SERVICE_TYPE=server-stats REGION=us-east SERVER_ID=server-1 \
	cargo watch -x 'run --bin metrics_server_snapshot'

# Frontend with hot reload (trunk serve on :3001). Needs trunk + tailwindcss
# (nix-shell default.nix -A shells.frontend provides the pinned versions).
dev-ui: ## Watch the Dioxus UI (trunk serve, :3001)
	./docker/start-dioxus.sh

dev-website: ## Watch the leptos marketing site (:4600) — not part of `make dev`
	cd leptos-website && npm install && LEPTOS_SITE_ADDR=0.0.0.0:4600 cargo leptos watch

##@ Containers (Docker — the one remaining use)

# Auto-create .env from sample on first run so --env-file never fails
.env:
	@echo "No .env found — creating from docker/.env-sample. Edit it before running make up."
	cp docker/.env-sample .env

up: .env images ## Prod-parity smoke: run the nix-built images via docker-compose
	$(COMPOSE) up

down: .env ## Stop the container stack (NOT `make dev` — that's dev-down)
	$(COMPOSE) down

##@ Images & shells

# Each image attr is a streamLayeredImage script: running it streams the
# tarball into `docker load`. Same flow locally and in CI.
images: ensure-nix ## Build + load every app image (nix -> docker load)
	@out=$$(nix-build release.nix -A images.all --no-out-link); \
	for img in $$out/*; do \
		echo "==> loading $$(basename $$img)"; \
		"$$img" | docker load; \
	done

image-%: ensure-nix ## Build + load one image, e.g. image-websocket-server
	@script=$$(nix-build release.nix -A images.$* --no-out-link); \
	"$$script" | docker load

shell: ensure-nix ## Enter the pinned toolchain shell (optional; rustup works too)
	nix-shell

pins-update: ensure-nix ## Refresh nixtamal pins (nix/tamal/) and relock
	nix-shell -p nixtamal git --run "nixtamal refresh"

##@ Tests (GNU `check` family)

check: lint check-backend check-ui ## Lint + backend + UI suites (e2e is separate: check-e2e)

check-backend: ensure-nix ## Backend integration tests against throwaway native postgres/NATS
	rm -rf $(IT_DATA) && mkdir -p $(IT_DATA)
	DEV_STACK_DATA_DIR=$(IT_DATA) PC_PORT_NUM=$(IT_PC_PORT) \
	DEV_STACK_PG_PORT=$(IT_PG_PORT) DEV_STACK_NATS_PORT=$(IT_NATS_PORT) DEV_STACK_NATS_MON_PORT=$(IT_NATS_MON_PORT) \
		nix-shell default.nix -A shells.dev --run "dev-stack -D postgres postgres-init nats"
	NATS_URL=localhost:$(IT_NATS_PORT) \
	DATABASE_URL="postgres://postgres:docker@localhost:$(IT_PG_PORT)/actix-api-db?sslmode=disable" \
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
		cargo clippy --workspace $(CLIPPY_EXCLUDES) -- -D warnings && \
		cargo fmt --all --check && \
		cargo test -p videocall-api -- --nocapture --test-threads=1 && \
		cargo test -p meeting-api -- --nocapture --test-threads=1"

check-backend-down: ensure-nix ## Stop the check-backend middleware and delete its state
	-nix-shell default.nix -A shells.dev --run "process-compose down -p $(IT_PC_PORT)"
	rm -rf $(IT_DATA)

# Serialized (--test-threads=1) because tests share the single SQLite file.
check-backend-sqlite: ensure-nix ## meeting-api integration tests on the SQLite backend
	rm -f $(SQLITE_TEST_DB) $(SQLITE_TEST_DB)-wal $(SQLITE_TEST_DB)-shm
	cd dbmate/sqlite && DATABASE_URL="sqlite:$(SQLITE_TEST_DB)" dbmate up
	DATABASE_URL="sqlite:$(SQLITE_TEST_DB)" \
		cargo test -p meeting-api --no-default-features --features sqlite -- --test-threads=1

check-ui: ensure-nix ## Dioxus UI wasm component tests (pinned headless Chrome, parallel)
	nix-shell default.nix -A shells.frontend-tests --run dioxus-ui-component-tests

check-e2e: e2e-build e2e-install ## Full Playwright pipeline: warm builds, native stack, tests, teardown
	$(MAKE) e2e-up
	cd e2e && npx playwright test; E2E_EXIT=$$?; cd .. && $(MAKE) e2e-down; exit $$E2E_EXIT

##@ E2E utilities (Playwright against a running `make e2e-up` stack)

e2e-install: ## Install e2e npm deps and the Playwright Chromium
	cd e2e && npm ci && npx playwright install chromium

# Warm everything the native E2E stack needs: the nix-built UI dist (and, on
# Linux, the server binaries — CI substitutes them from cachix); darwin runs
# the servers from cargo, so prebuild them there.
e2e-build: ensure-nix ## Warm the nix/cargo builds the e2e stack runs
	nix-shell default.nix -A shells.e2e --run true
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		echo "==> prebuilding server binaries (darwin runs them via cargo)"; \
		nix-shell default.nix -A shells.e2e --run "cargo build --bin meeting-api --bin websocket_server"; \
	fi

e2e-up: ensure-nix ## Start the native e2e stack detached (postgres, nats, servers, UI)
	rm -rf $(E2E_DATA)
	nix-shell default.nix -A shells.e2e --run "e2e-stack -D"

e2e-down: ensure-nix ## Stop the e2e stack and delete its state
	-nix-shell default.nix -A shells.e2e --run "process-compose down -p $(E2E_PC_PORT)"
	rm -rf $(E2E_DATA)

e2e: ## Run e2e tests headless; SPEC=<name> for one spec
	cd e2e && npx playwright test $(if $(SPEC),tests/$(SPEC).spec.ts,)

e2e-headed: ## Run e2e tests with visible browsers; SPEC=<name> for one spec
	cd e2e && npx playwright test --headed $(if $(SPEC),tests/$(SPEC).spec.ts,)

e2e-debug: ## Step through e2e tests in the Playwright Inspector
	cd e2e && npx playwright test --debug $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Regenerates the fixture from the current encoder, then decodes it in Chromium.
e2e-interop: ## WebCodecs VP9 interop gate (standalone, no stack needed)
	cargo run -p videocall-codecs --example dump_vp9_ivf --features test-utils -- e2e/fixtures/pure_rust_vp9.ivf
	cd e2e && E2E_SKIP_SERVICE_WAIT=1 npx playwright test tests/webcodecs-vp9-interop.spec.ts

e2e-lint: ## Lint + format check + typecheck e2e/ (same as CI)
	cd e2e && npm run ci:lint

e2e-fmt: ## Auto-fix e2e/ lint and formatting
	cd e2e && npm run lint:fix && npm run format:fix

##@ Code quality

lint: ensure-nix ## cargo clippy -D warnings + cargo fmt --check (pinned toolchain)
	nix-shell default.nix -A shells.backend-dev --run "cargo clippy --all -- --deny warnings && cargo fmt --all --check"

lint-fix: ensure-nix ## Auto-fix clippy findings
	nix-shell default.nix -A shells.backend-dev --run "cargo clippy --all --fix --allow-dirty --allow-staged"

fmt: ensure-nix ## cargo fmt --all
	nix-shell default.nix -A shells.backend-dev --run "cargo fmt --all"

##@ Utilities

db-shell: .env ## psql into the container stack's postgres (needs `make up`)
	$(COMPOSE) run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

nats-shell: .env ## Shell into the container stack's nats-box (needs `make up`)
	$(COMPOSE) exec nats-box sh

##@ Cleaning

clean: ## Remove disposable test/e2e state (safe; keeps dev data and images)
	rm -rf $(IT_DATA) $(E2E_DATA)
	rm -f $(SQLITE_TEST_DB) $(SQLITE_TEST_DB)-wal $(SQLITE_TEST_DB)-shm

clean-docker: .env ## Tear down the container stack, its volumes, images, and stale networks
	$(COMPOSE) down --remove-orphans --volumes --rmi all
	docker network prune -f

distclean: clean ## Pristine: also delete dev-stack data (.data/) — your dev DB!
	rm -rf .data

##@ Deprecated aliases

tests_run: ## (deprecated) use check-backend
	@echo "warning: 'make tests_run' is deprecated; use 'make check-backend'" >&2
	@$(MAKE) check-backend

tests_down: ## (deprecated) use check-backend-down
	@echo "warning: 'make tests_down' is deprecated; use 'make check-backend-down'" >&2
	@$(MAKE) check-backend-down

tests_sqlite_run: ## (deprecated) use check-backend-sqlite
	@echo "warning: 'make tests_sqlite_run' is deprecated; use 'make check-backend-sqlite'" >&2
	@$(MAKE) check-backend-sqlite

connect_to_db: ## (deprecated) use db-shell
	@echo "warning: 'make connect_to_db' is deprecated; use 'make db-shell'" >&2
	@$(MAKE) db-shell

connect_to_nats: ## (deprecated) use nats-shell
	@echo "warning: 'make connect_to_nats' is deprecated; use 'make nats-shell'" >&2
	@$(MAKE) nats-shell

clippy-fix: ## (deprecated) use lint-fix
	@echo "warning: 'make clippy-fix' is deprecated; use 'make lint-fix'" >&2
	@$(MAKE) lint-fix

e2e-ci: ## (deprecated) use check-e2e
	@echo "warning: 'make e2e-ci' is deprecated; use 'make check-e2e'" >&2
	@$(MAKE) check-e2e
