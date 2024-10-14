#!/bin/bash

# Function to handle Ctrl+C (SIGINT)
cleanup() {
  echo "Caught Ctrl+C, killing all processes..."
  kill $PID1 $PID2
  wait $PID1 $PID2 2>/dev/null
  exit
}

# Trap Ctrl+C signal
trap cleanup SIGINT

# Run the first process in the background and capture its PID
RUST_LOG=info cargo run --release -- --user-id pi-1 --video-device-index 4 --meeting-id test https://transport.rustlemania.com &
PID1=$!

# Run the second process in the background and capture its PID
RUST_LOG=info cargo run --release -- --user-id pi-2 --video-device-index 0 --meeting-id test https://transport.rustlemania.com &
PID2=$!

# Wait for all background processes to finish
wait $PID1 $PID2
