# Prod-parity aggregate: ALL actix-api binaries + dbmate + migrations in one
# image, default command websocket_server — exactly the shape of the old
# Dockerfile.actix that helm's websocket and webtransport charts both run
# (as securityunion/videocall-media-server) with different commands.
{ common, packages, pkgsLinuxStatic }:
common.mkServiceImage {
  name = "videocall/media-server";
  contents = [
    packages.videocall-api-bins
    pkgsLinuxStatic.dbmate
    common.dbmateFiles
    # helm invokes /usr/bin/metrics_server{,_snapshot} by absolute path
    (common.usrBinCompat [ packages.videocall-api-bins pkgsLinuxStatic.dbmate ])
  ] ++ common.shellUtils;
  config = {
    Cmd = [ "websocket_server" ];
  };
}
