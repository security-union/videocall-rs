COMPOSE_IT := docker/docker-compose.integration.yaml
COMPOSE_E2E := docker compose -p videocall-e2e -f docker/docker-compose.e2e.yaml

.PHONY: tests_up test up down build connect_to_db connect_to_nats clippy-fix fmt check check-style-tokens check-token-drift clean clean-docker rebuild rebuild-up e2e e2e-headed e2e-debug e2e-lint e2e-fmt e2e-install e2e-up e2e-down e2e-build e2e-ci

tests_run:
	docker compose -f $(COMPOSE_IT) up -d postgres nats && docker compose -f $(COMPOSE_IT) run --rm rust-tests \
		nix develop /app#backend-dev --command bash -c "\
		set -euo pipefail && \
		cd /app/dbmate && dbmate wait && dbmate up && \
		cd /app && \
		cargo clippy --all -- -D warnings && \
		cargo fmt --all --check && \
		cargo test -p videocall-api -- --nocapture --test-threads=1 && \
		cargo test -p meeting-api -- --nocapture --test-threads=1"

tests_build:
	docker compose -f $(COMPOSE_IT) build

tests_down:
	docker compose -f $(COMPOSE_IT) down -v

COMPOSE := docker compose --env-file .env -f docker/docker-compose.yaml

# Auto-create .env from sample on first run so --env-file never fails
.env:
	@echo "No .env found — creating from docker/.env-sample. Edit it before running make up."
	cp docker/.env-sample .env

up: .env
		$(COMPOSE) up
down:
		$(COMPOSE) down
build:
		$(COMPOSE) build

connect_to_db:
		$(COMPOSE) run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

connect_to_nats:
	$(COMPOSE) exec nats-box sh

clippy-fix:
		$(COMPOSE) run --rm --no-deps -w /app meeting-api nix develop /app#backend-dev --command bash -c "cargo clippy --all --fix --allow-dirty --allow-staged"

fmt:
		$(COMPOSE) run --rm --no-deps -w /app meeting-api nix develop /app#backend-dev --command bash -c "cargo fmt --all"

check:
		$(COMPOSE) run --rm --no-deps -w /app meeting-api nix develop /app#backend-dev --command bash -c "cargo clippy --all -- --deny warnings && cargo fmt --all --check"

check-style-tokens:
		bash scripts/check-hardcoded-colors.sh
		bash scripts/check-token-drift.sh

check-token-drift:
		bash scripts/check-token-drift.sh

clean:
		$(COMPOSE) down --remove-orphans \
			--volumes --rmi all

# Clean stale Docker resources (networks, containers)
clean-docker:
		$(COMPOSE) down --remove-orphans
		docker network prune -f

# Rebuild all images from scratch (use after Dockerfile changes or for ARM64 migration)
rebuild:
		$(COMPOSE) build --no-cache

# Rebuild and start (fresh build + run)
rebuild-up:
		$(COMPOSE) build --no-cache
		$(COMPOSE) up

# ---------------------------------------------------------------------------
# E2E tests (Playwright)
# ---------------------------------------------------------------------------

# Install e2e dependencies and Playwright browsers
e2e-install:
	cd e2e && npm ci && npx playwright install chromium

# Build E2E stack images (same dev Dockerfiles as CI)
e2e-build:
	$(COMPOSE_E2E) build

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

# Full CI pipeline: build stack, start it, run tests, tear down
e2e-ci: e2e-build e2e-install
	$(COMPOSE_E2E) up -d
	cd e2e && npx playwright test; E2E_EXIT=$$?; cd .. && $(COMPOSE_E2E) down -v; exit $$E2E_EXIT

# Lint + format check + typecheck (same as CI)
e2e-lint:
	cd e2e && npm run ci:lint

# Auto-fix lint and formatting issues
e2e-fmt:
	cd e2e && npm run lint:fix && npm run format:fix

