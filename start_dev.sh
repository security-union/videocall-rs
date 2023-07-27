#!/bin/bash

# WARNING!! use this script while running without docker.

#- 
# Pull configuration info from /config.toml
#
# Read the values from the config.toml file
source <(grep = config.toml | sed -E 's/ *= */=/g')

# Set the values as environment variables
export $(cut -d= -f1 config.toml)

#-
# Return to original script

children=()

_term() {
    echo "Caught SIGTERM"
    for child in "${children[@]}"; do
        kill -TERM "$child" 2>/dev/null
    done 
}

_int() {
    echo "Caught SIGINT"
    for child in "${children[@]}"; do
        kill -TERM "$child" 2>/dev/null
    done 
}

trap _term SIGTERM
trap _int SIGINT

pushd actix-api;
cargo watch -x "run --bin websocket_server" &
ACTIX_PROC=$!
children+=($ACTIX_PROC)
popd;

pushd yew-ui;
trunk serve &
YEW_PROCESS=$!
children+=($YEW_PROCESS)
popd;

wait $ACTIX_PROC
echo "Done running actix and yew, bye"
