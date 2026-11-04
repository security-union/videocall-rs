COMPOSE_IT := docker/docker-compose.integration.yaml
COMPOSE_E2E := docker compose -p videocall-e2e -f docker/docker-compose.e2e.yaml
COMPOSE_E2E_MONITORING := $(COMPOSE_E2E) -f docker/docker-compose.monitoring.yaml --profile monitoring

.PHONY: tests_up test test-scripts scale-run fidelity-run cq-bot-image changelog changelog-pending build-videocall-postgres test-videocall-postgres test-webtransport-chart test-api-chart up down build connect_to_db connect_to_nats clippy-fix clippy-ci deny fmt check check-style-tokens check-token-drift clean clean-docker rebuild rebuild-up e2e e2e-bvt0 e2e-bvt1 e2e-impair e2e-headed e2e-debug e2e-lint e2e-fmt e2e-install e2e-up e2e-up-impair e2e-up-monitoring e2e-down e2e-build e2e-cert e2e-doctor e2e-ci e2e-bots-clock

tests_run:
	docker compose -f $(COMPOSE_IT) up -d postgres nats && docker compose -f $(COMPOSE_IT) run --rm rust-tests \
		nix develop /app#backend-dev --command bash -c "\
		set -euo pipefail && \
		cd /app/dbmate && DBMATE_WAIT_TIMEOUT=60s dbmate wait && dbmate up && \
		cd /app && \
		cargo clippy --all -- -D warnings && \
		cargo fmt --all --check && \
		cargo test -p videocall-meeting-types --all-features && \
		cargo test -p videocall-api -- --nocapture --test-threads=1 && \
		cargo test -p videocall-api --bin metrics_server -- --ignored --nocapture --test-threads=1 && \
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
# Keep these lines BYTE-IDENTICAL to the workflow steps — if CI changes, change
# here. `scripts/check-clippy-ci-sync.sh` (run by the fmt job in
# pr-check-rust-hcl.yaml) enforces this: editing one list without the other
# turns CI red, so the byte-identical contract is self-enforcing (issue #1500).
# Script test suites (bash/python tooling under scripts/). Not covered by cargo
# test, and nothing else in CI runs them, so a regression here would rot silently.
test-scripts:
		python3 -c 'import sys; sys.version_info >= (3, 9) or sys.exit("scripts require Python >= 3.9, runner has " + sys.version.split()[0])'
		python3 scripts/test_parse_meeting_console_logs_census.py
		@if [ -f scripts/test_claim_sweep.py ]; then \
			python3 scripts/test_claim_sweep.py; \
		else \
			echo "SKIP: scripts/test_claim_sweep.py absent (stripped for public sync)"; \
		fi
		python3 scripts/test_check_clippy_ci_sync.py
		python3 scripts/test_check_protos_regen.py
		python3 scripts/test_check_vendored_pins.py
		python3 scripts/test_e2e_doctor_freshness.py
		python3 scripts/test_e2e_up_stamp_clear.py
		python3 scripts/test_e2e_backend_stop.py
		python3 scripts/test_compose_scale_overlay.py
		python3 scripts/test_trim_cargo_targets.py
		python3 scripts/test_check_native_test_coverage.py
		python3 scripts/test_mutation_check.py
		python3 scripts/test_check_dashboard_metric_refs.py
		python3 scripts/test_check_call_quality_dashboard.py
		python3 scripts/test_call_quality_dashboard_semantics.py
		python3 scripts/test_check_dashboard_configmap_keys.py
		python3 scripts/test_wasm_browser_test.py
		@if [ -f helm/global/hcl/prometheus/values.yaml ] && [ -f helm/global/hcl-daily-deployment/prometheus/values.yaml ]; then \
			python3 scripts/test_check_prometheus_alert_parity.py; \
			PROMTOOL_SKIP_RUNTIME=1 python3 scripts/test_check_prometheus_rules_parse.py; \
		else \
			echo "SKIP: scripts/test_check_prometheus_alert_parity.py (helm/global/hcl* prometheus values stripped for public sync)"; \
			echo "SKIP: scripts/test_check_prometheus_rules_parse.py (same files)"; \
		fi
		@if [ -f scripts/test_verify_audio_single_layer.py ]; then \
			python3 scripts/test_verify_audio_single_layer.py; \
		else \
			echo "SKIP: scripts/test_verify_audio_single_layer.py absent (stripped for public sync)"; \
		fi
		@if [ -f scripts/test_meeting_quality_xref.py ]; then \
			python3 scripts/test_meeting_quality_xref.py; \
		else \
			echo "SKIP: scripts/test_meeting_quality_xref.py absent (stripped for public sync)"; \
		fi
		@if [ -f scripts/test_meeting_quality_xref_load.py ]; then \
			python3 scripts/test_meeting_quality_xref_load.py; \
		else \
			echo "SKIP: scripts/test_meeting_quality_xref_load.py absent (stripped for public sync)"; \
		fi
		python3 scripts/quality/test_call_quality_score.py
		python3 scripts/quality/test_cq_collect.py
		python3 scripts/quality/test_cq_collect_integration.py
		python3 scripts/quality/test_cq_scenario.py
		python3 scripts/quality/test_scenario_run.py
		python3 scripts/quality/test_scenario_run_integration.py
		python3 scripts/quality/test_cq_local.py
		python3 scripts/quality/test_cq_shared_target.py
		python3 scripts/quality/fidelity/test_fidelity.py
		python3 scripts/quality/fidelity/test_fidelity_report.py
		python3 scripts/quality/test_cq_remote.py
		python3 scripts/quality/test_cq_bot_image.py
		python3 scripts/quality/test_cq_probe_cost.py
		@if command -v docker >/dev/null || [ -n "$$CI" ]; then \
			python3 scripts/test_nginx_cache_headers.py; \
		else \
			echo "SKIP: scripts/test_nginx_cache_headers.py (needs docker; never skipped under CI)"; \
		fi
		@if [ -f scripts/test_sync_strip_blocked_paths.py ]; then \
			python3 scripts/test_sync_strip_blocked_paths.py; \
		else \
			echo "SKIP: scripts/test_sync_strip_blocked_paths.py absent (stripped for public sync)"; \
		fi
		@if [ -f scripts/test_update_changelog.py ]; then \
			python3 scripts/test_update_changelog.py; \
		else \
			echo "SKIP: scripts/test_update_changelog.py absent (stripped for public sync)"; \
		fi
		@if [ -f scripts/test_gh_pr_screenshots.py ]; then \
			python3 scripts/test_gh_pr_screenshots.py; \
		else \
			echo "SKIP: scripts/test_gh_pr_screenshots.py absent (stripped for public sync)"; \
		fi

# Stamp built pending sections of dioxus-ui/assets/changelog.json (GH_TOKEN for the Actions API).
changelog:
		@if [ -f scripts/changelog/update-changelog.sh ]; then \
			bash scripts/changelog/update-changelog.sh --stamp; \
		else \
			echo "SKIP: scripts/changelog/update-changelog.sh absent (stripped for public sync)"; \
		fi

# Propose pending sections, one per PR, for PRs no section lists yet (see --help).
# CHANGELOG_REMOTE: the git remote hcl-main and PR-staging are fetched from (pass your HTTPS remote).
CHANGELOG_REMOTE ?= origin
changelog-pending:
		@if [ -f scripts/changelog/update-changelog.sh ]; then \
			git fetch $(CHANGELOG_REMOTE) hcl-main PR-staging && \
			bash scripts/changelog/update-changelog.sh --pending \
				--base $(CHANGELOG_REMOTE)/hcl-main --head $(CHANGELOG_REMOTE)/PR-staging; \
		else \
			echo "SKIP: scripts/changelog/update-changelog.sh absent (stripped for public sync)"; \
		fi

build-videocall-postgres:
		./scripts/build-videocall-postgres-dependencies.sh

test-videocall-postgres:
		./scripts/test-videocall-postgres-chart.sh
		./scripts/test-digitalocean-preview-postgres.sh

test-webtransport-chart:
		python3 ./scripts/check_wt_service_affinity.py
		python3 ./scripts/test_check_wt_service_affinity.py

test-api-chart:
		python3 ./scripts/check_api_readiness_probe.py
		python3 ./scripts/test_check_api_readiness_probe.py
		python3 ./scripts/check_api_migration_hook.py
		python3 ./scripts/test_check_api_migration_hook.py

clippy-ci:
		cargo clippy --all -- -D warnings
		cargo clippy --target wasm32-unknown-unknown -p videocall-client --tests -- -D warnings
		cargo clippy --target wasm32-unknown-unknown -p videocall-client --features netsim --tests -- -D warnings
		cargo clippy -p videocall-aq --tests -- -D warnings
		cargo clippy -p videocall-codecs --features wasm --tests -- -D warnings
		cargo clippy -p videocall-ui --tests -- -D warnings
		cargo clippy -p neteq --no-default-features --features web --tests -- -D warnings
		cargo clippy -p videocall-diagnostics --tests -- -D warnings
		cargo clippy -p bot --tests -- -D warnings
		cargo clippy -p videocall-netsim --tests -- -D warnings
		cargo clippy -p vcprobe --tests -- -D warnings
		cargo clippy -p videocall-cli --tests -- -D warnings
		cargo clippy -p videocall-api --tests -- -D warnings
		cargo clippy -p meeting-api --tests -- -D warnings
		cargo clippy -p videocall-meeting-client --tests -- -D warnings
		cargo clippy -p videocall-meeting-types --tests -- -D warnings
		cargo clippy -p videocall-transport --tests -- -D warnings
		cargo clippy -p videocall-transport --features netsim --tests -- -D warnings
		cargo clippy -p videocall-types --tests -- -D warnings
		cargo clippy --manifest-path vendor/quinn-proto/Cargo.toml --tests -- -D warnings -A clippy::nonminimal_bool

deny:
		bash scripts/check-vendored-pins.sh
		cargo deny --manifest-path vendor/advisories/Cargo.toml --config deny.toml check advisories
		cargo deny check advisories

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

# One scale scenario end to end (scripts/quality/scenario_run.py); the run folder goes to $$CQ_RUNS_DIR or
# ~/videocall-scale-runs. PORTS=auto|default|alt, RELEASE=1 for release builds, ARGS for the runner's own flags.
scale-run:
	@test -n "$(SCENARIO)" || { echo "usage: make scale-run SCENARIO=<name> [PORTS=auto|default|alt] [RELEASE=1] [ARGS=...]"; exit 3; }
	CQ_PORTS=$(or $(PORTS),auto) CQ_RELEASE_BUILD=$(or $(RELEASE),0) python3 scripts/quality/scenario_run.py --scenario scripts/quality/scenarios/$(SCENARIO).yaml $(ARGS)

# Bot-vs-browser fidelity table on a running e2e stack (docs/quality-at-scale/FIDELITY_HARNESS.md).
# SCENARIO names a file in scripts/quality/fidelity/scenarios (default room10); ARGS for the harness's run flags.
fidelity-run:
	python3 scripts/quality/fidelity/fidelity_run.py run --scenario scripts/quality/fidelity/scenarios/$(or $(SCENARIO),room10).yaml $(ARGS)

# The Rust bot image a shared-target scale run checks for: videocall-bot:cq-<sha12> from a clean tree
# (scripts/quality/cq_bot_image.py). ENGINE=docker|podman; SAVE=path.tar.gz also writes the archive for a load host.
cq-bot-image:
	python3 scripts/quality/cq_bot_image.py --engine $(or $(ENGINE),docker) $(if $(SAVE),--save $(SAVE))

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
# The stamps live in the /app bind mount, so `e2e-down -v` leaves them behind
# at their last heartbeat. Clearing here keeps the freshness gate on its "no
# stamp yet" path instead of the previous run's cold one (#2513). The leading
# `-` is required: the container creates the dir as root, so on Linux the clear
# can fail and must not take the stack up with it. See
# scripts/test_e2e_up_stamp_clear.py.
e2e-up: e2e-cert
	-rm -rf e2e/.stack-stamps
	$(COMPOSE_E2E) up -d

# Start the E2E stack WITH the per-client downlink-impairment proxy (issue
# #1080). Adds the `toxiproxy` service (compose profile `impair`) on top of the
# normal stack so the per-receiver-simulcast divergence spec can degrade ONE
# receiver's downlink via toxiproxy's HTTP control API (see
# e2e/helpers/downlink-impair.ts). The proxy is OFF for every other target so
# the standard suite is never slowed.
e2e-up-impair: e2e-cert
	-rm -rf e2e/.stack-stamps
	COMPOSE_PROFILES=impair $(COMPOSE_E2E) up -d

# E2E stack plus docker/docker-compose.monitoring.yaml (#2923); the first run builds two extra backends.
e2e-up-monitoring: e2e-cert
	-rm -rf e2e/.stack-stamps
	$(COMPOSE_E2E_MONITORING) up -d

# Tear down the E2E stack and remove volumes. `--profile impair` and the
# monitoring overlay ensure toxiproxy and the monitoring services are also
# removed (compose only stops profile services if the profile is named on `down`).
e2e-down:
	$(COMPOSE_E2E_MONITORING) --profile impair down -v

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

# Real-Chromium check of the bots-app clock source (no stack needed).
e2e-bots-clock:
	cd e2e && npx playwright test --config bots-app/src/playwright.clock.config.ts

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
	@echo "issue 2809 precondition: sudo sysctl -w net.ipv4.tcp_wmem='4096 16384 131072'"
	cd e2e && npx playwright test --project=impair --workers=1

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
