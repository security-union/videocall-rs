COMPOSE_IT := docker/docker-compose.integration.yaml

.PHONY: tests_up test up down build connect_to_db connect_to_nats clippy-fix fmt check clean

tests_run:
	docker compose -f $(COMPOSE_IT) up -d && docker compose -f $(COMPOSE_IT) run --rm rust-tests bash -c "cargo clippy -- -D warnings && cargo fmt --check && cargo machete && cargo test -- --nocapture --test-threads=1"

tests_build:
	docker compose -f $(COMPOSE_IT) build

tests_down:
	docker compose -f $(COMPOSE_IT) down -v

up:
		docker compose -f docker/docker-compose.yaml up
down:
		docker compose -f docker/docker-compose.yaml down
build:
		docker compose -f docker/docker-compose.yaml build

connect_to_db:
		docker compose -f docker/docker-compose.yaml run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

connect_to_nats:
	docker compose -f docker/docker-compose.yaml exec nats-box sh

clippy-fix:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app && cargo clippy --fix"

fmt:
		docker compose -f docker/docker-compose.yaml run yew-ui bash -c "cd /app && cargo fmt"

check: 
		docker compose -f docker/docker-compose.yaml run websocket-api bash -c "cd /app && cargo clippy --all  -- --deny warnings && cargo fmt --check"

clean:
		docker compose -f docker/docker-compose.yaml down --remove-orphans \
			--volumes --rmi all