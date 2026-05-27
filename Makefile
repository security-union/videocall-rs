COMPOSE_IT := docker/docker-compose.integration.yaml
COMPOSE_E2E := docker compose -p videocall-e2e -f docker/docker-compose.e2e.yaml

.PHONY: tests_up test up down build connect_to_db connect_to_nats clippy-fix fmt check check-style-tokens check-token-drift clean clean-docker rebuild rebuild-up e2e e2e-headed e2e-debug e2e-lint e2e-fmt e2e-install e2e-up e2e-down e2e-build e2e-cert e2e-doctor e2e-ci

tests_run:
	docker compose -f $(COMPOSE_IT) up -d postgres nats && docker compose -f $(COMPOSE_IT) run --rm rust-tests \
		nix develop /app#backend-dev --command bash -c "\
		set -euo pipefail && \
		cd /app/dbmate && DBMATE_WAIT_TIMEOUT=60s dbmate wait && dbmate up && \
		cd /app && \
		cargo clippy --all -- -D warnings && \
		cargo fmt --all --check && \
		cargo test -p videocall-api -- --nocapture --test-threads=1 && \
		cargo test -p meeting-api -- --nocapture --test-threads=1"

tests_build:
	docker compose -f $(COMPOSE_IT) build

tests_down:
	docker compose -f $(COMPOSE_IT) down -v --remove-orphans --timeout 30

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

# Regenerate the WebTransport dev cert + companion DER-SHA-256 hash file.
# The cert is short-lived (ECDSA P-256, 13 days) so the
# WebTransport `serverCertificateHashes` constructor option will accept it
# (Chromium rejects entries for any cert with > 14 days of validity).
# The script is idempotent: if the existing cert has > 1 day of life left
# AND passes every preflight check (key type, validity, SAN, hash match)
# it does nothing. Pass `ARGS=--force` to force regen, or `ARGS=--verify`
# to run the preflight without writing anything.
e2e-cert:
	bash scripts/regen-dev-cert.sh $(ARGS)

# Diagnostic target: check every prerequisite for `make e2e` and report
# pass/fail. Non-mutating (does NOT regen the cert; use `make e2e-cert
# ARGS=--force` for that). Useful when an E2E run fails confusingly.
e2e-doctor:
	bash scripts/e2e-doctor.sh

# Build E2E stack images (same dev Dockerfiles as CI). Cert must exist
# before the webtransport-api container mounts and reads it at startup.
e2e-build: e2e-cert
	$(COMPOSE_E2E) build

# Start the E2E stack (postgres, nats, meeting-api, websocket-api,
# webtransport-api, dioxus-ui). Re-runs the cert script first so a stale /
# expired cert is regenerated automatically before bringing up the stack.
e2e-up: e2e-cert
	$(COMPOSE_E2E) up -d

# Tear down the E2E stack and remove volumes
e2e-down:
	$(COMPOSE_E2E) down -v

# Run e2e tests headless (assumes stack is already up).
#   make e2e                        — all tests
#   make e2e SPEC=two-users-meeting — single spec (without .spec.ts)
# Re-runs the cert script first: Playwright reads the cert-hash file at
# module load and a stale cert + matching hash still fails Chromium QUIC
# verification with the same error this fix eliminated. Idempotent guard.
e2e: e2e-cert
	cd e2e && npx playwright test $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Run e2e tests with visible browsers (assumes stack is already up)
#   make e2e-headed                        — all tests
#   make e2e-headed SPEC=two-users-meeting — single spec
e2e-headed: e2e-cert
	cd e2e && npx playwright test --headed $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Run e2e tests in debug mode (step through in Playwright Inspector)
e2e-debug: e2e-cert
	cd e2e && npx playwright test --debug $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Full CI pipeline: regen cert, build stack, start it, run tests, tear down
e2e-ci: e2e-cert e2e-build e2e-install
	$(COMPOSE_E2E) up -d
	cd e2e && npx playwright test; E2E_EXIT=$$?; cd .. && $(COMPOSE_E2E) down -v; exit $$E2E_EXIT

# Lint + format check + typecheck (same as CI)
e2e-lint:
	cd e2e && npm run ci:lint

# Auto-fix lint and formatting issues
e2e-fmt:
	cd e2e && npm run lint:fix && npm run format:fix

