# Dioxus UI: Caddy serving the trunk dist, with an entrypoint that regenerates
# /usr/share/nginx/html/config.js from env at container start (runtime config —
# the same contract docker/start-dioxus.sh provides in native dev and the helm
# configmap provides in k8s). Compose services: dioxus-ui (dev + e2e). Port 80.
#
# Caddy (static Go binary) instead of nginx deliberately: cross-nginx drags in
# cross-perl, which does not build at the current nixpkgs pin. The Caddyfile
# replicates nginx.conf: SPA try_files fallback to /index.html and no-cache
# headers on every response.
{ common, packages, p }:
let
  inherit (p) pkgs pkgsLinuxStatic;

  # shared definition (also in the CI warm set) — see nix/pkgs.nix
  caddy = p.caddyStatic;

  htmlRoot = pkgs.runCommand "dioxus-ui-html" { } ''
    mkdir -p $out/usr/share/nginx/html
    cp -r ${packages.dioxus-ui-dist}/. $out/usr/share/nginx/html/
  '';

  caddyfile = pkgs.writeTextDir "etc/caddy/Caddyfile" ''
    {
      admin off
      auto_https off
      persist_config off
    }

    :80 {
      root * /usr/share/nginx/html

      # Disable caching for all responses (mirrors nginx.conf: hashed assets
      # are not available yet)
      header {
        Cache-Control "no-store, no-cache, must-revalidate, max-age=0"
        Pragma "no-cache"
        -ETag
      }

      # SPA fallback (nginx: try_files $uri $uri/ /index.html)
      try_files {path} {path}/ /index.html
      file_server
    }
  '';

  # Runs as PID 1: write runtime config.js from env, then exec caddy. In k8s
  # the helm chart may mount config.js from a configmap (read-only) — skip the
  # env-generated one in that case instead of crashing.
  entrypoint = pkgs.writeScript "dioxus-ui-entrypoint" ''
    #!/bin/sh
    set -eu
    CONFIG=/usr/share/nginx/html/config.js
    # A configmap may be mounted read-only over $CONFIG. Don't trust [ -w ]:
    # the container runs as root, which passes the permission-bit test even on
    # a read-only mount. Probe by attempting the write itself.
    if ! { cat > "$CONFIG"; } 2>/dev/null <<EOF
    window.__APP_CONFIG = Object.freeze({
      apiBaseUrl: "''${API_BASE_URL:-http://localhost:8081}",
      wsUrl: "''${ACTIX_UI_BACKEND_URL:-ws://localhost:8080}",
      webTransportHost: "''${WEBTRANSPORT_HOST:-https://127.0.0.1:4433}",
      oauthEnabled: "''${ENABLE_OAUTH:-false}",
      e2eeEnabled: "''${E2EE_ENABLED:-false}",
      webTransportEnabled: "''${WEBTRANSPORT_ENABLED:-false}",
      firefoxEnabled: "''${FIREFOX_ENABLED:-false}",
      usersAllowedToStream: "''${USERS_ALLOWED_TO_STREAM:-}",
      serverElectionPeriodMs: ''${SERVER_ELECTION_PERIOD_MS:-2000},
      audioBitrateKbps: ''${AUDIO_BITRATE_KBPS:-65},
      videoBitrateKbps: ''${VIDEO_BITRATE_KBPS:-100},
      screenBitrateKbps: ''${SCREEN_BITRATE_KBPS:-100},
      oauthProvider: "''${OAUTH_PROVIDER:-}",
      vadThreshold: ''${VAD_THRESHOLD:-0.02}
    });
    EOF
    then
      echo "config.js is read-only (mounted); keeping the mounted config"
    fi
    exec caddy run --config /etc/caddy/Caddyfile --adapter caddyfile
  '';
in
pkgs.dockerTools.streamLayeredImage {
  name = "videocall/dioxus-ui";
  tag = "dev";
  contents = [
    caddy
    pkgsLinuxStatic.busybox
    htmlRoot
    caddyfile
  ];
  extraCommands = ''
    mkdir -p tmp
  '';
  config = {
    Entrypoint = [ entrypoint ];
    ExposedPorts."80/tcp" = { };
    Env = [
      "PATH=${caddy}/bin:/usr/bin:/bin"
      "XDG_CONFIG_HOME=/tmp"
      "XDG_DATA_HOME=/tmp"
      "HOME=/tmp"
    ];
  };
}
