# Prometheus metrics exporter (actix-api `metrics_server`).
# Compose service: metrics-api. Port: $METRICS_PORT (9091).
{ common, packages }:
common.mkServiceImage {
  name = "videocall/metrics-server";
  config = {
    Cmd = [ "${packages.videocall-api-bins}/bin/metrics_server" ];
    ExposedPorts."9091/tcp" = { };
  };
}
