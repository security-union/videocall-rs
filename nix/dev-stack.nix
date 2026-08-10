# Native dev stack — the docker-compose-shaped dev loop, without Docker.
#
# `process-compose` (the supervisor the services-flake/devenv ecosystems build
# on) runs EVERYTHING as host processes: postgres + NATS from nixpkgs, the five
# Rust services under cargo-watch, the trunk dev server, and the prometheus +
# grafana observability pair. Dependencies are health-gated exactly like
# docker-compose `depends_on: condition: service_healthy`.
#
#   make dev              -> whole stack, TUI, hot reload everywhere
#   make dev-middleware   -> postgres/nats/prometheus/grafana only
#
# State lives in .data/ (gitignored). Toolchains (cargo, cargo-watch, trunk,
# tailwind, dbmate) come from the surrounding shells.dev nix-shell.
{ p }:
let
  inherit (p) pkgs;
  lib = pkgs.lib;

  yamlFormat = pkgs.formats.yaml { };

  # --- middleware wrappers ---------------------------------------------------

  postgresRun = pkgs.writeShellApplication {
    name = "dev-stack-postgres";
    runtimeInputs = [ pkgs.postgresql_18 ];
    text = ''
      PGDATA="''${DEV_STACK_DATA_DIR:-$PWD/.data}/postgres"
      if [ -s "$PGDATA/PG_VERSION" ] && [ "$(cat "$PGDATA/PG_VERSION")" != "18" ]; then
        echo "error: $PGDATA was initialized by postgres $(cat "$PGDATA/PG_VERSION"), but the dev stack runs 18." >&2
        echo "       Dev data is disposable: rm -rf $PGDATA and rerun." >&2
        exit 1
      fi
      if [ ! -s "$PGDATA/PG_VERSION" ]; then
        initdb -D "$PGDATA" -U postgres --auth=trust --auth-host=trust --encoding=UTF8
      fi
      exec postgres -D "$PGDATA" \
        -c listen_addresses=127.0.0.1 \
        -c port="''${DEV_STACK_PG_PORT:-5432}" \
        -c unix_socket_directories="$PGDATA"
    '';
  };

  # one-shot: create the app database once postgres is healthy (same role the
  # POSTGRES_DB env var played in the postgres container)
  postgresInit = pkgs.writeShellApplication {
    name = "dev-stack-postgres-init";
    runtimeInputs = [ pkgs.postgresql_18 ];
    text = ''
      PGPORT="''${DEV_STACK_PG_PORT:-5432}"
      export PGPORT
      if ! psql -h 127.0.0.1 -U postgres -lqt | cut -d'|' -f1 | grep -qw actix-api-db; then
        createdb -h 127.0.0.1 -U postgres actix-api-db
      fi
    '';
  };

  natsRun = pkgs.writeShellApplication {
    name = "dev-stack-nats";
    runtimeInputs = [ pkgs.nats-server ];
    text = ''
      DATA="''${DEV_STACK_DATA_DIR:-$PWD/.data}/nats"
      mkdir -p "$DATA"
      exec nats-server -js -sd "$DATA" -p "''${DEV_STACK_NATS_PORT:-4222}" -m "''${DEV_STACK_NATS_MON_PORT:-8222}"
    '';
  };

  # prometheus config for host networking: same file as the docker stack, with
  # container DNS rewritten to localhost so the two never drift semantically.
  prometheusConfig = pkgs.runCommand "dev-prometheus-config" { } ''
    mkdir -p $out
    sed -e 's/metrics-api:9091/127.0.0.1:9091/' \
        -e 's/server-stats-api:9092/127.0.0.1:9092/' \
        ${../docker/monitoring/prometheus/prometheus.yml} > $out/prometheus.yml
    cp ${../docker/monitoring/prometheus/alert_rules.yml} $out/alert_rules.yml
  '';

  prometheusRun = pkgs.writeShellApplication {
    name = "dev-stack-prometheus";
    runtimeInputs = [ pkgs.prometheus ];
    text = ''
      mkdir -p "$PWD/.data/prometheus"
      exec prometheus \
        --config.file=${prometheusConfig}/prometheus.yml \
        --storage.tsdb.path="$PWD/.data/prometheus" \
        --storage.tsdb.retention.time=200h \
        --web.listen-address=127.0.0.1:9090 \
        --web.enable-lifecycle
    '';
  };

  # grafana provisioning is written at runtime because the dashboards live in
  # the repo (path only known then); datasource points at native prometheus.
  grafanaRun = pkgs.writeShellApplication {
    name = "dev-stack-grafana";
    runtimeInputs = [ pkgs.grafana ];
    text = ''
      GF="$PWD/.data/grafana"
      mkdir -p "$GF/data" "$GF/plugins" \
        "$GF/provisioning/datasources" "$GF/provisioning/dashboards" \
        "$GF/provisioning/plugins" "$GF/provisioning/alerting"
      cat > "$GF/provisioning/datasources/prometheus.yml" <<EOF
      apiVersion: 1
      datasources:
        - name: Prometheus
          type: prometheus
          access: proxy
          url: http://127.0.0.1:9090
          isDefault: true
          editable: true
      EOF
      cat > "$GF/provisioning/dashboards/videocall.yml" <<EOF
      apiVersion: 1
      providers:
        - name: 'videocall'
          orgId: 1
          folder: 'VideoCall'
          type: file
          disableDeletion: false
          editable: true
          updateIntervalSeconds: 10
          allowUiUpdates: true
          options:
            path: $PWD/docker/monitoring/grafana/dashboards
      EOF
      export GF_PATHS_DATA="$GF/data"
      export GF_PATHS_PLUGINS="$GF/plugins"
      export GF_PATHS_PROVISIONING="$GF/provisioning"
      export GF_SERVER_HTTP_ADDR=127.0.0.1
      export GF_SERVER_HTTP_PORT=3000
      export GF_SECURITY_ADMIN_USER=admin
      export GF_SECURITY_ADMIN_PASSWORD=grafana
      export GF_USERS_ALLOW_SIGN_UP=false
      exec grafana server --homepath ${pkgs.grafana}/share/grafana
    '';
  };

  # --- shared app env (native: middleware on localhost) ----------------------

  # Shared/overridable env (DATABASE_URL, NATS_URL, JWT_SECRET, RUST_LOG,
  # OAUTH_*, UI config …) is NOT listed here — processes inherit it from the
  # dev-stack wrapper, which layers: built-in defaults < .env < .env.local
  # (both untracked). Per-process lists keep only per-service identity.
  commonEnv = [ ];

  middlewareUp = {
    postgres-init.condition = "process_completed_successfully";
    nats.condition = "process_healthy";
  };

  # Stop the cargo-watch trees with SIGINT (^C semantics — reaches the whole
  # process group, which the servers handle gracefully) and give them a few
  # seconds before process-compose escalates to SIGKILL.
  rustShutdown = {
    signal = 2;
    timeout_seconds = 10;
  };

  config = {
    version = "0.5";
    processes = {
      # ---------------- middleware ----------------
      postgres = {
        namespace = "middleware";
        command = lib.getExe postgresRun;
        readiness_probe = {
          exec.command = ''pg_isready -h 127.0.0.1 -p "''${DEV_STACK_PG_PORT:-5432}" -U postgres'';
          initial_delay_seconds = 2;
          period_seconds = 2;
          failure_threshold = 30;
        };
      };
      postgres-init = {
        namespace = "middleware";
        command = lib.getExe postgresInit;
        depends_on.postgres.condition = "process_healthy";
      };
      nats = {
        namespace = "middleware";
        command = lib.getExe natsRun;
        readiness_probe = {
          exec.command = ''curl -sf "http://127.0.0.1:''${DEV_STACK_NATS_MON_PORT:-8222}/healthz"'';
          initial_delay_seconds = 1;
          period_seconds = 2;
          failure_threshold = 30;
        };
      };
      prometheus = {
        namespace = "middleware";
        command = lib.getExe prometheusRun;
      };
      grafana = {
        namespace = "middleware";
        command = lib.getExe grafanaRun;
      };

      # ---------------- app services (cargo watch, hot reload) ----------------
      meeting-api = {
        namespace = "services";
        shutdown = rustShutdown;
        command = "(cd dbmate && dbmate wait && dbmate up) && exec cargo watch -x 'run --bin meeting-api'";
        environment = [ "LISTEN_ADDR=0.0.0.0:8081" ];
        depends_on = middlewareUp;
      };
      websocket = {
        namespace = "services";
        shutdown = rustShutdown;
        command = "exec cargo watch -x 'run --bin websocket_server'";
        environment = [ "ACTIX_PORT=8080" "DATABASE_ENABLED=false" "SERVICE_TYPE=websocket" ];
        depends_on.nats.condition = "process_healthy";
      };
      webtransport = {
        namespace = "services";
        shutdown = rustShutdown;
        working_dir = "actix-api";
        command = "exec cargo watch -x 'run --bin webtransport_server'";
        environment = [
          "LISTEN_URL=0.0.0.0:4433"
          "HEALTH_LISTEN_URL=0.0.0.0:5321"
          "CERT_PATH=certs/localhost.pem"
          "KEY_PATH=certs/localhost.key"
          "SERVICE_TYPE=webtransport"
        ];
        depends_on.nats.condition = "process_healthy";
      };
      metrics = {
        namespace = "services";
        shutdown = rustShutdown;
        command = "exec cargo watch -x 'run --bin metrics_server'";
        environment = [ "METRICS_PORT=9091" "SERVICE_TYPE=client-metrics" ];
        depends_on = middlewareUp;
      };
      server-stats = {
        namespace = "services";
        shutdown = rustShutdown;
        command = "exec cargo watch -x 'run --bin metrics_server_snapshot'";
        environment = [ "METRICS_PORT=9092" "SERVICE_TYPE=server-stats" ];
        depends_on.nats.condition = "process_healthy";
      };
      dioxus-ui = {
        namespace = "services";
        shutdown = rustShutdown;
        command = "exec ./docker/start-dioxus.sh";
        environment = [ "TRUNK_SERVE_PORT=3001" ];
      };
    };
  };

  configFile = yamlFormat.generate "process-compose.yaml" config;
in
pkgs.writeShellApplication {
  name = "dev-stack";
  runtimeInputs = [
    pkgs.process-compose
    pkgs.postgresql_18 # pg_isready for the readiness probe
    pkgs.dbmate
  ];
  text = ''
    # Run from the repo root (state goes to ./.data, dashboards read from repo)
    if [ ! -f default.nix ] || [ ! -d dbmate ]; then
      echo "error: dev-stack must be run from the videocall-rs repo root" >&2
      exit 1
    fi
    mkdir -p "''${DEV_STACK_DATA_DIR:-.data}"
    # Env layering for every process in the stack (children inherit):
    #   built-in defaults  <  .env  <  .env.local   (both untracked)
    # Put secrets/overrides (OAUTH_*, JWT_SECRET, bitrates, …) in .env.local.
    set -a
    # shellcheck disable=SC1091
    [ -f .env ] && . ./.env
    # shellcheck disable=SC1091
    [ -f .env.local ] && . ./.env.local
    set +a
    export NATS_URL="''${NATS_URL:-localhost:4222}"
    export DATABASE_URL="''${DATABASE_URL:-postgres://postgres:docker@localhost:5432/actix-api-db?sslmode=disable}"
    export JWT_SECRET="''${JWT_SECRET:-dev-jwt-secret-change-me}"
    export RUST_LOG="''${RUST_LOG:-debug,async_nats=info}"
    export REGION="''${REGION:-us-east}"
    export SERVER_ID="''${SERVER_ID:-server-1}"
    export UI_ENDPOINT="''${UI_ENDPOINT:-http://localhost:3001}"
    export API_BASE_URL="''${API_BASE_URL:-http://localhost:8081}"
    export ACTIX_UI_BACKEND_URL="''${ACTIX_UI_BACKEND_URL:-ws://localhost:8080}"
    export WEBTRANSPORT_HOST="''${WEBTRANSPORT_HOST:-https://127.0.0.1:4433}"
    export TOKEN_TTL_SECS="''${TOKEN_TTL_SECS:-60}"
    export COOKIE_SECURE="''${COOKIE_SECURE:-false}"
    export AFTER_LOGIN_URL="''${AFTER_LOGIN_URL:-http://localhost:3001}"
    export ALLOWED_REDIRECT_URLS="''${ALLOWED_REDIRECT_URLS:-http://localhost:3001}"
    # process-compose's own REST API defaults to :8080, which shadows the
    # websocket server on 127.0.0.1 — park it well out of the way.
    # PC_PORT_NUM is process-compose's native env knob; overridable so a
    # second instance (e.g. `make tests_run` middleware) can coexist.
    : "''${PC_PORT_NUM:=28080}"
    export PC_PORT_NUM
    exec process-compose up -f ${configFile} "$@"
  '';
}
