# WebSocket signaling/media server (actix-api `websocket_server`).
# Compose service: websocket-api. Port: $ACTIX_PORT (default 8080).
{ common, packages }:
common.mkServiceImage {
  name = "videocall/websocket-server";
  config = {
    Cmd = [ "${packages.videocall-api-bins}/bin/websocket_server" ];
    ExposedPorts."8080/tcp" = { };
  };
}
