test:
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app/actix-api && cargo test"

up:
		docker compose -f docker/docker-compose.yaml up
down:
		docker compose -f docker/docker-compose.yaml down
build:
		docker compose -f docker/docker-compose.yaml build

connect_to_db:
		docker compose -f docker/docker-compose.yaml run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

clippy-fix:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app && cargo clippy --fix"

fmt:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app && cargo fmt"

check: 
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app && cargo clippy --all  -- --deny warnings && cargo fmt --check"

clean:
		docker compose -f docker/docker-compose.yaml down --remove-orphans \
			--volumes --rmi all