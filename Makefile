test:
		# docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app/yew-ui && cargo test"
		docker compose -f docker/docker-compose.yaml exec -T websocket-api bash -c "cd /app/actix-api && cargo test"

up:
		docker compose -f docker/docker-compose.yaml up -d
down:
		docker compose -f docker/docker-compose.yaml down
build:
		docker compose -f docker/docker-compose.yaml build

# Rebuild all images with the new caching configuration
rebuild:
		docker compose -f docker/docker-compose.yaml down
		docker compose -f docker/docker-compose.yaml build --no-cache
		docker compose -f docker/docker-compose.yaml up -d
		$(MAKE) warm-caches

connect_to_db:
		docker compose -f docker/docker-compose.yaml exec postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

clippy-fix:
		docker compose -f docker/docker-compose.yaml exec -T yew-ui bash -c "cd /app/yew-ui && cargo clippy --fix --allow-dirty --target-dir=/app/yew-ui/target"
		docker compose -f docker/docker-compose.yaml exec -T websocket-api bash -c "cd /app/actix-api && cargo clippy --fix --allow-dirty --target-dir=/app/actix-api/target"

fmt:
		docker compose -f docker/docker-compose.yaml exec -T yew-ui bash -c "cd /app/yew-ui && cargo fmt"
		docker compose -f docker/docker-compose.yaml exec -T websocket-api bash -c "cd /app/actix-api && cargo fmt"

check: 
		docker compose -f docker/docker-compose.yaml exec -T websocket-api bash -c "cd /app/actix-api && cargo clippy --all --target-dir=/app/actix-api/target -- --deny warnings && cargo fmt --check"
		docker compose -f docker/docker-compose.yaml exec -T yew-ui bash -c "cd /app/yew-ui && cargo clippy --all --target-dir=/app/yew-ui/target -- --deny warnings && cargo fmt --check"
		# docker compose -f docker/docker-compose.yaml exec website bash -c "cd /app/leptos-website && cargo clippy --features ssr --all -- --deny warnings"

# Clean cargo caches if needed (use with caution)
clean-caches:
		docker compose -f docker/docker-compose.yaml down -v --remove-orphans
		docker volume prune -f --filter "label=com.docker.compose.project=videocall-rs"

# List all volumes to see cache sizes
list-caches:
		docker volume ls | grep rustlemania

# Warm up caches for faster subsequent runs
warm-caches:
		docker compose -f docker/docker-compose.yaml exec -T yew-ui bash -c "cd /app/yew-ui && cargo fetch"
		docker compose -f docker/docker-compose.yaml exec -T websocket-api bash -c "cd /app/actix-api && cargo fetch"

# Remove orphaned containers
clean-orphans:
		docker compose -f docker/docker-compose.yaml down --remove-orphans