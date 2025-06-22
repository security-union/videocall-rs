#!/bin/bash
# This script finds and terminates any process running on TCP port 80.

PORT=80
PID=$(lsof -t -i:$PORT)

if [ -z "$PID" ]; then
  echo "No process found running on port $PORT."
else
  echo "Process found on port $PORT with PID: $PID. Terminating..."
  kill -9 $PID
  echo "Process terminated."
fi 