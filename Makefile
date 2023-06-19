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
		docker compose -f docker/docker-compose.prod.yml build

connect_to_db:
		docker compose -f docker/docker-compose.prod.yml run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

clippy-fix:
		docker compose -f docker/docker-compose.prod.yml run yew-ui bash -c "cd /app/yew-ui && cargo clippy --fix"
		docker compose -f docker/docker-compose.prod.yml run actix-api bash -c "cd /app/actix-api && cargo clippy --fix"

fmt:
		docker compose -f docker/docker-compose.prod.yml run yew-ui bash -c "cd /app/yew-ui && cargo fmt"
		docker compose -f docker/docker-compose.prod.yml run actix-api bash -c "cd /app/actix-api && cargo fmt"