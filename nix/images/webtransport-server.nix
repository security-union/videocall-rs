# WebTransport/QUIC media server (actix-api `webtransport_server`).
# Compose service: webtransport-api. UDP $LISTEN_URL (4433) + health $HEALTH_LISTEN_URL (5321).
# TLS cert/key are mounted by compose ($CERT_PATH/$KEY_PATH).
{ common, packages }:
common.mkServiceImage {
  name = "videocall/webtransport-server";
  config = {
    Cmd = [ "${packages.videocall-api-bins}/bin/webtransport_server" ];
    ExposedPorts = {
      "4433/udp" = { };
      "5321/tcp" = { };
    };
  };
}
