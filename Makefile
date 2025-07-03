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
		docker compose -f docker/docker-compose.yaml down --remove-orphans
		docker compose -f docker/docker-compose.yaml rm -f
		docker rmi docker-website || echo "Failed to remove image: docker-website"
		docker rmi docker-yew-ui || echo "Failed to remove image: docker-yew-ui"
		docker rmi docker-tailwind-yew || echo "Failed to remove image: docker-tailwind-yew"
		docker rmi docker-websocket-api || echo "Failed to remove image: docker-websocket-api"
		docker rmi docker-webtransport-api || echo "Failed to remove image: docker-webtransport-api"
		docker volume rm docker_rustlemania-actix-web-cargo-git-cache
		docker volume rm docker_rustlemania-actix-web-cargo-registry-cache
		docker volume rm docker_rustlemania-actix-web-cargo-target-cache
		docker volume rm docker_rustlemania-actix-web-target-cache
		docker volume rm docker_rustlemania-actix-webtransport-cache
		docker volume rm docker_rustlemania-actix-webtransport-cargo-git-cache
		docker volume rm docker_rustlemania-actix-webtransport-cargo-registry-cache
		docker volume rm docker_rustlemania-actix-webtransport-cargo-target-cache
		docker volume rm docker_rustlemania-leptos-ui-cache
		docker volume rm docker_rustlemania-leptos-ui-cargo-git-cache
		docker volume rm docker_rustlemania-leptos-ui-cargo-registry-cache
		docker volume rm docker_rustlemania-leptos-ui-cargo-target-cache
		docker volume rm docker_rustlemania-yew-ui-cache    
		docker volume rm docker_rustlemania-yew-ui-cargo-git-cache
		docker volume rm docker_rustlemania-yew-ui-cargo-registry-cache
		docker volume rm docker_rustlemania-yew-ui-cargo-target-cache
