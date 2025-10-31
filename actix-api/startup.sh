#!/bin/bash -e
## Only run dbmate if DATABASE_ENABLED is true, else run the websocket server
if [ "$DATABASE_ENABLED" = "true" ]; then
  /app/dbmate/startup.sh
fi
websocket_server