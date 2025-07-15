#!/bin/bash

# WARNING!! use this script while running without docker.

export TRUNK_SERVE_PORT=8081
export ACTIX_PORT=8081
export LOGIN_URL=${LOGIN_URL:-http://localhost:${TRUNK_SERVE_PORT}/login}
export ACTIX_UI_BACKEND_URL=${ACTIX_UI_BACKEND_URL:-ws://localhost:${ACTIX_PORT}}
export WEBTRANSPORT_HOST=${WEBTRANSPORT_HOST:-https://127.0.0.1:4433}
export ENABLE_OAUTH=${ENABLE_OAUTH:-0}
export WEBTRANSPORT_ENABLED=${WEBTRANSPORT_ENABLED:-0}
export E2EE_ENABLED=${E2EE_ENABLED:-0}
export NATS_URL=${NATS_URL:-0.0.0.0:4222}
export HEALTH_LISTEN_URL=${HEALTH_LISTEN_URL:-0.0.0.0:5321}
export LISTEN_URL=${LISTEN_URL:-0.0.0.0:4433}
export DATABASE_URL=${DATABASE_URL:-postgresql://$USER@localhost/actix-api-db}

server_command="$( ((WEBTRANSPORT_ENABLED)) && echo webtransport_server || echo websocket_server )"

_kill() {
    kill -- -$$
    # the command spawned by cargo watch doesn't get killed with the process group, so kill it explicitly
    pkill -f "$server_command"
}

trap _kill SIGINT SIGTERM SIGQUIT

if ! [ -x "$(command -v trurl)" ] ; then
    echo "please install trurl (see https://github.com/curl/trurl)"
    exit
fi

if ! [ -x "$(command -v nats-server)" ] ; then
    echo "please install nats-server (see https://docs.nats.io/running-a-nats-service/introduction/installation)"
    exit
fi

if ! psql "$DATABASE_URL" -c 'SELECT 1' >/dev/null; then
    echo "please make sure postgresql is running with database defined for: $DATABASE_URL"
    exit
fi

nats-server --addr "$(trurl -g '{host}' "$NATS_URL")" --port "$(trurl -g '{port}' "$NATS_URL")" &

pushd actix-api > /dev/null || exit
cargo watch -x "run --bin $server_command" &
ACTIX_PROC=$!
popd > /dev/null || exit

pushd yew-ui > /dev/null || exit
trunk serve &
popd > /dev/null || exit

wait $ACTIX_PROC
echo "Done running actix and yew, bye"
