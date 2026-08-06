# Meeting REST/auth API. Runs dbmate migrations then the server, mirroring the
# old Dockerfile.meeting-api CMD (`/app/dbmate/startup.sh && meeting-api`).
# Compose service: meeting-api. Port: $LISTEN_ADDR (8081).
{ common, packages, pkgsLinuxStatic }:
common.mkServiceImage {
  name = "videocall/meeting-api";
  contents = [
    packages.meeting-api
    pkgsLinuxStatic.dbmate
    common.dbmateFiles
    # the old image shipped /usr/bin/meeting-api and /usr/bin/dbmate
    (common.usrBinCompat [ packages.meeting-api pkgsLinuxStatic.dbmate ])
  ] ++ common.shellUtils;
  config = {
    Cmd = [
      "/bin/sh"
      "-c"
      "cd /app/dbmate && dbmate wait && dbmate up && exec meeting-api"
    ];
    ExposedPorts."8081/tcp" = { };
  };
}
