#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(git rev-parse --show-toplevel)"
SETUP_SCRIPT="$ROOT_DIR/scripts/digitalocean-prod-setup-preview-infra.sh"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

fake_bin="$tmp_dir/bin"
helm_dependency_built="$tmp_dir/helm-dependency-built"
helm_install_args="$tmp_dir/helm-install-args"
mkdir -p "$fake_bin"

cat >"$fake_bin/helm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-} ${2:-}" in
  "dependency list")
    printf '%s\n' \
      'NAME VERSION REPOSITORY STATUS' \
      'postgresql 18.1.3 https://charts.bitnami.com/bitnami ok'
    ;;
  "dependency build")
    touch "$FAKE_HELM_DEPENDENCY_BUILT"
    ;;
  "repo add" | "list -n")
    ;;
  "install postgres")
    if [[ ! -f "$FAKE_HELM_DEPENDENCY_BUILT" ]]; then
      echo "Helm dependency build must run before install." >&2
      exit 1
    fi
    printf '%s\n' "$@" >"$FAKE_HELM_INSTALL_ARGS"
    ;;
  *)
    echo "Unexpected helm command: $*" >&2
    exit 1
    ;;
esac
EOF

cat >"$fake_bin/kubectl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-} ${2:-} ${3:-}" in
  "config current-context ")
    echo test-context
    ;;
  "version --client ")
    ;;
  "get namespace preview-infra")
    ;;
  "get secret postgres-credentials")
    exit 1
    ;;
  "get secret google-oauth-credentials")
    if [[ " $* " == *" -o json "* ]]; then
      printf '%s\n' '{"metadata":{"name":"google-oauth-credentials"},"data":{}}'
    fi
    ;;
  "create secret generic")
    ;;
  "delete secret google-oauth-credentials")
    ;;
  "wait --for=condition=ready pod")
    exit 1
    ;;
  "apply -f -")
    cat >/dev/null
    ;;
  *)
    echo "Unexpected kubectl command: $*" >&2
    exit 1
    ;;
esac
EOF

chmod +x "$fake_bin/helm" "$fake_bin/kubectl"

(
  cd "$ROOT_DIR"
  printf '%s\n' test-password |
    PATH="$fake_bin:$PATH" \
      FAKE_HELM_DEPENDENCY_BUILT="$helm_dependency_built" \
      FAKE_HELM_INSTALL_ARGS="$helm_install_args" \
      bash "$SETUP_SCRIPT" >"$tmp_dir/setup.out"
)

required_args=(
  'helm/videocall-postgres'
  '-f'
  'helm/global/us-east/postgres/values.yaml'
)
for arg in "${required_args[@]}"; do
  if ! grep -Fxq -- "$arg" "$helm_install_args"; then
    echo "ERROR: preview PostgreSQL install omitted required argument: $arg" >&2
    cat "$helm_install_args" >&2
    exit 1
  fi
done

echo "OK: DigitalOcean preview setup installs the shared chart with US East values."
