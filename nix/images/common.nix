# Shared helper for app images. streamLayeredImage (not buildLayeredImage) on
# purpose: the derivation output is a *script* that streams the image tarball
# when run — assembly is host-side work at `docker load` time, which keeps
# multi-hundred-MB tarballs out of the store and sidesteps dockerTools-under-
# pkgsCross issues (https://github.com/NixOS/nixpkgs/issues/266840).
#
#   $(nix-build release.nix -A images.<name> --no-out-link) | docker load
{ p }:
let
  inherit (p) pkgs pkgsLinux pkgsLinuxStatic;
in
{
  # /app/dbmate — migrations + schema, used by meeting-api and media-server
  # (mirrors `COPY /app/dbmate /app/dbmate` from the old Dockerfiles).
  dbmateFiles = pkgs.runCommand "dbmate-files" { } ''
    mkdir -p $out/app
    cp -r ${../../dbmate} $out/app/dbmate
  '';

  # /usr/bin compatibility symlinks for a list of packages' bin/ entries.
  # The old Debian-based Dockerfiles shipped binaries in /usr/bin and the helm
  # charts invoke some of them by absolute path (/usr/bin/metrics_server) —
  # keep that contract.
  usrBinCompat = paths: pkgs.runCommand "usr-bin-compat" { } ''
    mkdir -p $out/usr/bin
    for p in ${pkgs.lib.concatMapStringsSep " " (p: "${p}/bin") paths}; do
      for b in "$p"/*; do
        ln -s "$b" "$out/usr/bin/$(basename "$b")"
      done
    done
  '';

  # mkServiceImage: image around one (or more) static musl binaries.
  #   name     — image name (videocall/<…>)
  #   contents — extra store paths beyond certs/tzdata
  #   config   — OCI config (Cmd, Env, ExposedPorts, …). StopSignal defaults
  #              to SIGINT (parity with the old Dockerfiles' STOPSIGNAL).
  mkServiceImage = { name, tag ? "dev", contents ? [ ], config }:
    pkgs.dockerTools.streamLayeredImage {
      inherit name tag;
      contents = [
        pkgsLinux.dockerTools.caCertificates
        pkgsLinux.tzdata
      ] ++ contents;
      config = {
        Env = [
          "PATH=/usr/bin:/bin"
          "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
        ] ++ (config.Env or [ ]);
        StopSignal = config.StopSignal or "SIGINT";
      } // builtins.removeAttrs config [ "Env" "StopSignal" ];
    };

  # Shell + coreutils for images whose Cmd is a shell one-liner (dbmate && …).
  shellUtils = [ pkgsLinuxStatic.busybox ];
}
