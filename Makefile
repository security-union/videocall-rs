test:
		docker compose -f docker/docker-compose.prod.yml run yew-ui bash -c "cd /app/yew-ui && cargo test"
		docker compose -f docker/docker-compose.prod.yml run actix-api bash -c "cd /app/actix-api && cargo test"

up:
		docker compose -f docker/docker-compose.prod.yml up
down:
		docker compose -f docker/docker-compose.prod.yml down
dev:
		docker compose -f .devcontainer/docker-compose.dev.yml up
build:
		# FIXME: Using `docker compose` command in `build` have error in relative directory
		# original command:
		# docker compose -f docker/docker-compose.prod.yml build
		docker build -f docker/Dockerfile.yew.prod -t zoom-rs/yew-ui .
		docker build -f docker/Dockerfile.actix.prod -t zoom-rs/actix-api .

connect_to_db:
		docker compose -f docker/docker-compose.prod.yml run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

clippy-fix:
		docker compose -f docker/docker-compose.prod.yml run yew-ui bash -c "cd /app/yew-ui && cargo clippy --fix"
		docker compose -f docker/docker-compose.prod.yml run actix-api bash -c "cd /app/actix-api && cargo clippy --fix"

fmt:
		docker compose -f docker/docker-compose.prod.yml run yew-ui bash -c "cd /app/yew-ui && cargo fmt"
		docker compose -f docker/docker-compose.prod.yml run actix-api bash -c "cd /app/actix-api && cargo fmt"