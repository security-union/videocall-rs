test:
		# docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app/yew-ui && cargo test"
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo test"

up:
		docker compose -f docker/docker-compose.yaml up
down:
		docker compose -f docker/docker-compose.yaml down

build_api:
		docker buildx build -t rustlemania --load .

build_ui:
		docker buildx build -t rustlemania-ui -f Dockerfile.ui --load .

build:
		docker buildx build -t rustlemania --load .
		docker buildx build -t rustlemania-ui -f Dockerfile.ui --load .

connect_to_db:
		docker compose -f docker/docker-compose.yaml run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

clippy-fix:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app/yew-ui && cargo clippy --fix"
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo clippy --fix"

fmt:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app/yew-ui && cargo fmt"
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo fmt"

check: 
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo clippy --all  -- --deny warnings && cargo fmt --check"
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app/yew-ui && cargo clippy --all  -- --deny warnings && cargo fmt --check"
