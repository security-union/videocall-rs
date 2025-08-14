COMPOSE = docker compose -f docker/docker-compose.yaml

.PHONY: test up down build connect_to_db clippy-fix fmt check clean help

test:
	$(COMPOSE) run websocket-api bash -c "cd /app/actix-api && cargo test"

up:
	$(COMPOSE) up

down:
	$(COMPOSE) down

build:
	$(COMPOSE) build

connect_to_db:
	$(COMPOSE) run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

connect_to_nats:
	docker compose -f docker/docker-compose.yaml exec nats-box sh

clippy-fix:
	$(COMPOSE) run yew-ui bash -c "cd /app && cargo clippy --fix"

fmt:
	$(COMPOSE) run yew-ui bash -c "cd /app && cargo fmt"

check:
	$(COMPOSE) run websocket-api bash -c "cd /app && cargo clippy --all -- --deny warnings && cargo fmt --check"

clean:
	$(COMPOSE) down --remove-orphans --volumes --rmi all

help:
	@echo "Available targets:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'


