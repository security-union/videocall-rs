# Define the docker-compose file path for reusability
DOCKER_COMPOSE := 'docker compose -f docker/docker-compose.yaml'

# Test Rule: Run tests in the websocket-api container
test:
	# This runs the tests for the websocket-api service in the docker container
	{{DOCKER_COMPOSE}} run websocket-api bash -c "cd /app/actix-api && cargo test"

# Up Rule: Start up all containers defined in the docker-compose file
up:
	# This brings up the docker containers for all services
	{{DOCKER_COMPOSE}} up

# Down Rule: Shut down all containers and clean up
down:
	# This stops the containers and removes any associated resources
	{{DOCKER_COMPOSE}} down

# Build Rule: Build all services defined in the docker-compose file
build:
	# This builds the docker images and services as defined in the docker-compose.yaml
	{{DOCKER_COMPOSE}} build

# Connect to DB Rule: Connect to the Postgres database container and run psql
connect_to_db:
	# This connects to the postgres container and allows access to the Postgres DB
	{{DOCKER_COMPOSE}} run postgres bash -c "psql -h postgres -d actix-api-db -U postgres"

# Clippy-fix Rule: Run cargo clippy to check and fix code issues
clippy-fix:
	# This runs clippy with the --fix flag for both yew-ui and websocket-api
	{{DOCKER_COMPOSE}} run yew-ui bash -c "cd /app/yew-ui && cargo clippy --fix"
	{{DOCKER_COMPOSE}} run websocket-api bash -c "cd /app/actix-api && cargo clippy --fix"

# Fmt Rule: Format the code for both yew-ui and websocket-api using cargo fmt
fmt:
	# This runs cargo fmt to automatically format the code for both services
	{{DOCKER_COMPOSE}} run yew-ui bash -c "cd /app/yew-ui && cargo fmt"
	{{DOCKER_COMPOSE}} run websocket-api bash -c "cd /app/actix-api && cargo fmt"

# Check Rule: Run cargo clippy and cargo fmt --check to check code for linting issues and formatting issues
check:
	# This checks both the formatting and lints the code for warnings and errors without making changes
	{{DOCKER_COMPOSE}} run websocket-api bash -c "cd /app/actix-api && cargo clippy --all -- --deny warnings && cargo fmt --check"
	{{DOCKER_COMPOSE}} run yew-ui bash -c "cd /app/yew-ui && cargo clippy --all -- --deny warnings && cargo fmt --check"
