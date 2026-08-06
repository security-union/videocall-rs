# Server-stats snapshot exporter (actix-api `metrics_server_snapshot`).
# Compose service: server-stats-api. Port: $METRICS_PORT (9092).
{ common, packages }:
common.mkServiceImage {
  name = "videocall/server-stats";
  config = {
    Cmd = [ "${packages.videocall-api-bins}/bin/metrics_server_snapshot" ];
    ExposedPorts."9092/tcp" = { };
  };
}
