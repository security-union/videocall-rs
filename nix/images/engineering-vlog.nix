# Engineering vlog: zola-built static site served by static Caddy.
# Replaces docker/Dockerfile.engineering-vlog (zola:v0.19.1 -> nginx:1.27).
# Docker Hub: securityunion/videocall-engineering-vlog -> helm engineering-vlog chart.
{ common, p }:
let
  inherit (p) pkgs pkgsLinuxStatic;

  site = pkgs.stdenv.mkDerivation {
    pname = "engineering-vlog-site";
    version = "0.1.0";
    src = pkgs.lib.cleanSource ../../engineering-vlog;
    nativeBuildInputs = [ pkgs.zola ];
    buildPhase = ''
      runHook preBuild
      zola build
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      cp -r public $out
      runHook postInstall
    '';
  };

  htmlRoot = pkgs.runCommand "engineering-vlog-html" { } ''
    mkdir -p $out/usr/share/caddy
    cp -r ${site}/. $out/usr/share/caddy/
  '';

  caddyfile = pkgs.writeTextDir "etc/caddy/Caddyfile" ''
    {
      admin off
      auto_https off
      persist_config off
    }

    :80 {
      root * /usr/share/caddy
      file_server
    }
  '';
in
pkgs.dockerTools.streamLayeredImage {
  name = "videocall/engineering-vlog";
  tag = "dev";
  contents = [
    p.caddyStatic
    htmlRoot
    caddyfile
  ];
  extraCommands = ''
    mkdir -p tmp
  '';
  config = {
    Cmd = [ "${p.caddyStatic}/bin/caddy" "run" "--config" "/etc/caddy/Caddyfile" "--adapter" "caddyfile" ];
    ExposedPorts."80/tcp" = { };
    Env = [
      "XDG_CONFIG_HOME=/tmp"
      "XDG_DATA_HOME=/tmp"
      "HOME=/tmp"
    ];
  };
}
