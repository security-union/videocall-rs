#!/bin/bash -e
## Only run dbmate is DATABASE_ENABLED is true, else run the websocket server
if [ "$DATABASE_ENABLED" = "true" ]; then
  /usr/src/app/dbmate/startup.sh
fi
websocket_server
