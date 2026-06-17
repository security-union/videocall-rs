COMPOSE_IT := docker/docker-compose.integration.yaml
COMPOSE_E2E := docker compose -p videocall-e2e -f docker/docker-compose.e2e.yaml

.PHONY: tests_up test up down build connect_to_db connect_to_nats clippy-fix clippy-ci fmt check check-style-tokens check-token-drift clean clean-docker rebuild rebuild-up e2e e2e-bvt0 e2e-bvt1 e2e-impair e2e-headed e2e-debug e2e-lint e2e-fmt e2e-install e2e-up e2e-up-impair e2e-down e2e-build e2e-cert e2e-doctor e2e-ci

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

# Mirror the EXACT clippy command set run by CI's `cargo clippy` job
# (.github/workflows/pr-check-rust-hcl.yaml). `cargo clippy --all` lints only
# library/binary targets, NOT `#[test]` code, so deny-by-default lints inside
# tests slip through a plain `--all` run and fail in CI instead (e.g. PR #1491
# shipped a `clippy::len_zero` in a neteq `--features web` test). Run this
# locally before every push; /pre-submit runs it and fails the gate on any error.
# Keep these lines BYTE-IDENTICAL to the workflow steps — if CI changes, change here.
clippy-ci:
		cargo clippy --all -- -D warnings
		cargo clippy --target wasm32-unknown-unknown -p videocall-client --tests -- -D warnings
		cargo clippy -p videocall-aq --tests -- -D warnings
		cargo clippy -p videocall-codecs --tests -- -D warnings
		cargo clippy -p videocall-ui --tests -- -D warnings
		cargo clippy -p neteq --no-default-features --features web --tests -- -D warnings

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

# Start the E2E stack WITH the per-client downlink-impairment proxy (issue
# #1080). Adds the `toxiproxy` service (compose profile `impair`) on top of the
# normal stack so the per-receiver-simulcast divergence spec can degrade ONE
# receiver's downlink via toxiproxy's HTTP control API (see
# e2e/helpers/downlink-impair.ts). The proxy is OFF for every other target so
# the standard suite is never slowed.
e2e-up-impair: e2e-cert
	COMPOSE_PROFILES=impair $(COMPOSE_E2E) up -d

# Tear down the E2E stack and remove volumes. `--profile impair` ensures the
# toxiproxy container is also removed when it was started by `e2e-up-impair`
# (compose only stops profile services if the profile is named on `down`).
e2e-down:
	$(COMPOSE_E2E) --profile impair down -v

# Run e2e tests headless. Assumes the stack is already up — bring it up
# with `make e2e-up`, which is also the only target that rotates the cert.
# These run-only targets deliberately do NOT depend on `e2e-cert` because
# regenerating the cert here would leave the running webtransport-api
# container holding the OLD cert (it reads CERT_PATH/KEY_PATH only at
# process startup) while Playwright injects the NEW hash — recreating the
# QUIC handshake mismatch this PR is trying to eliminate. If you need a
# fresh cert, run `make e2e-up` (or `make e2e-cert ARGS=--force` followed
# by `docker restart videocall-e2e-webtransport-api-1`).
#   make e2e                        — all tests (full `dioxus` suite)
#   make e2e SPEC=two-users-meeting — single spec (without .spec.ts)
# Pinned to --project=dioxus so the bvt0/bvt1 projects (which share specs with
# dioxus via tags) don't cause tagged tests to run multiple times.
e2e:
	cd e2e && npx playwright test --project=dioxus $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Run only the bvt0 smoke set (the absolute minimum "is the app alive" check).
# Selects tests tagged `@bvt0`. Intended for the fastest possible per-PR signal.
e2e-bvt0:
	cd e2e && npx playwright test --project=bvt0

# Run the bvt1 smoke superset (includes everything in bvt0 + auth + simple
# meeting). Selects tests tagged `@bvt0` or `@bvt1`. Intended for per-PR CI as
# a faster alternative to the full suite.
e2e-bvt1:
	cd e2e && npx playwright test --project=bvt1

# Run ONLY the toxiproxy-backed half of issue #1080: the `@impair`-tagged
# WebSocket simulcast divergence test. REQUIRES the toxiproxy `impair` profile
# to be up — bring the stack up with `make e2e-up-impair` first. This test is
# grep-inverted out of the default `dioxus`/bvt projects, so it never runs in
# the standard suite; this target is the only one that exercises it.
#
# NOTE: this does NOT cover the whole #1080 suite. #1080's WebTransport half uses
# the CLIENT-SIDE `netsim` hook (no toxiproxy) and therefore runs in the default
# `dioxus` suite via `make e2e` — it is NOT `@impair`-tagged. See the WT
# divergence test in tests/simulcast-per-receiver.spec.ts and
# helpers/downlink-impair.ts.
e2e-impair:
	cd e2e && npx playwright test --project=impair

# Run e2e tests with visible browsers (assumes stack is already up; same
# cert-rotation rule as `make e2e`)
#   make e2e-headed                        — all tests
#   make e2e-headed SPEC=two-users-meeting — single spec
e2e-headed:
	cd e2e && npx playwright test --project=dioxus --headed $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Run e2e tests in debug mode (step through in Playwright Inspector;
# same cert-rotation rule as `make e2e`)
e2e-debug:
	cd e2e && npx playwright test --project=dioxus --debug $(if $(SPEC),tests/$(SPEC).spec.ts,)

# Full CI pipeline: regen cert, build stack, start it, run tests, tear down
e2e-ci: e2e-cert e2e-build e2e-install
	$(COMPOSE_E2E) up -d
	cd e2e && npx playwright test --project=dioxus; E2E_EXIT=$$?; cd .. && $(COMPOSE_E2E) down -v; exit $$E2E_EXIT

# Lint + format check + typecheck (same as CI)
e2e-lint:
	cd e2e && npm run ci:lint

# Auto-fix lint and formatting issues
e2e-fmt:
	cd e2e && npm run lint:fix && npm run format:fix

