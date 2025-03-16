test:
		# docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app/yew-ui && cargo test"
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo test"
		# Test videocall-client package
		cargo test -p videocall-client --lib
		cargo test -p videocall-client --test test_harness

# Optimized parallel testing target for CI
test-parallel:
		# Run API tests with parallelism
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo test -- --test-threads=4"
		# Run videocall-client tests in parallel
		cargo test -p videocall-client --lib -- --test-threads=4
		cargo test -p videocall-client --test test_harness -- --test-threads=4

up:
		docker compose -f docker/docker-compose.yaml up
down:
		docker compose -f docker/docker-compose.yaml down
build:
		docker compose -f docker/docker-compose.yaml build

connect_to_db:
		docker compose -f docker/docker-compose.yaml run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

clippy-fix:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app/yew-ui && cargo clippy --fix"
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo clippy --fix"

fmt:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app/yew-ui && cargo fmt"
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo fmt""

check: 
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo clippy --all  -- --deny warnings && cargo fmt --check"
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app/yew-ui && cargo clippy --all  -- --deny warnings && cargo fmt --check"
		# Run clippy check for videocall-client package
		cargo clippy -p videocall-client --all -- --deny warnings
		# docker compose -f docker/docker-compose.yaml run website bash -c "cd /app/leptos-website && cargo clippy --features ssr --all -- --deny warnings"