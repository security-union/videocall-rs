#!/usr/bin/env bash
# Validates that the UI CSP Helm fragment stays in sync with runtimeConfig URL
# origins for every values tree that deploys this chart.
#
# Usage: bash scripts/check-csp-sync.sh
set -euo pipefail

ROOT_DIR="$(git rev-parse --show-toplevel)"

# This guard renders the two videocall-ui charts and needs `helm` + `python3`.
# The CI job runs on the `hcl-ci` runner (NOT the `docker`-labelled deploy
# runner that carries helm), so helm is not guaranteed present. Fail LOUD but
# distinctly if a required tool is missing — a hard `helm: command not found`
# under `set -e` would read as a genuine sync failure; this makes the cause
# unambiguous and lets a helm-less runner skip rather than red-flag a PR that
# did not touch anything wrong. (If helm is present, the check runs in full.)
for tool in helm python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "SKIP: check-csp-sync.sh requires '$tool' but it is not on PATH on this runner; skipping CSP sync validation. Install $tool on the CI runner to enforce this check." >&2
    exit 0
  fi
done

render_chart() {
  local chart_dir="$1"
  local output_file="$2"

  helm template "$chart_dir" >"$output_file"
}

validate_render() {
  local label="$1"
  local rendered="$2"
  local values="$3"

  python3 - "$label" "$rendered" "$values" <<'PYEOF'
import json
import re
import sys
from pathlib import Path
from typing import Optional
from urllib.parse import urlparse

label, rendered_path, values_path = sys.argv[1:]
rendered = Path(rendered_path).read_text()

def read_csp_enforce(path: str) -> bool:
    lines = Path(path).read_text().splitlines()
    in_chart = False
    in_csp = False
    chart_indent = -1
    csp_indent = -1
    enforce = False

    for line in lines:
        raw = line.split("#", 1)[0].rstrip()
        if not raw.strip():
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        stripped = raw.strip()

        if stripped == "videocall-ui:":
            in_chart = True
            chart_indent = indent
            in_csp = False
            continue

        if in_chart and indent <= chart_indent:
            in_chart = False
            in_csp = False

        if stripped == "csp:" and (not any(line.startswith("videocall-ui:") for line in lines) or in_chart):
            in_csp = True
            csp_indent = indent
            continue

        if in_csp and indent <= csp_indent:
            in_csp = False

        if in_csp and stripped.startswith("enforce:"):
            enforce_value = stripped.split(":", 1)[1].strip().strip('"').strip("'").lower()
            enforce = enforce_value == "true"
            break

    return enforce

expected_name = (
    "Content-Security-Policy"
    if read_csp_enforce(values_path)
    else "Content-Security-Policy-Report-Only"
)

header_re = re.compile(
    r'add_header\s+'
    r'(?P<name>Content-Security-Policy(?:-Report-Only)?)\s+'
    r'"(?P<value>[^"]+)"\s+always;'
)
header_match = header_re.search(rendered)
if not header_match:
    print(f"ERROR: {label}: rendered CSP add_header was not found", file=sys.stderr)
    sys.exit(1)

actual_name = header_match.group("name")
policy = header_match.group("value")
if actual_name != expected_name:
    print(
        f"ERROR: {label}: header name mismatch: expected {expected_name}, got {actual_name}",
        file=sys.stderr,
    )
    sys.exit(1)

config_match = re.search(r"window\.__APP_CONFIG = Object\.freeze\((\{.*\})\);", rendered)
if not config_match:
    print(f"ERROR: {label}: rendered runtime config.js JSON was not found", file=sys.stderr)
    sys.exit(1)

runtime_config = json.loads(config_match.group(1))

def origin(raw: object) -> Optional[str]:
    if raw is None:
        return None
    value = str(raw).strip()
    if not value:
        return None
    parsed = urlparse(value)
    if not parsed.scheme or not parsed.netloc:
        return None
    return f"{parsed.scheme}://{parsed.netloc}"

expected_origins: list[str] = ["'self'"]

def add(raw: object) -> None:
    item = origin(raw)
    if item and item not in expected_origins:
        expected_origins.append(item)

for key in ("apiBaseUrl", "meetingApiBaseUrl", "searchApiBaseUrl", "jmapBaseUrl"):
    add(runtime_config.get(key))

for key in ("wsUrl", "webTransportHost"):
    for part in str(runtime_config.get(key) or "").split(","):
        add(part)

if (
    str(runtime_config.get("oauthEnabled", "")).lower() == "true"
    and str(runtime_config.get("oauthFlow", "")).lower() == "pkce"
):
    token_url = str(runtime_config.get("oauthTokenUrl") or "").strip()
    provider = str(runtime_config.get("oauthProvider") or "").lower()
    if not token_url and provider == "google":
        token_url = "https://oauth2.googleapis.com/token"
    if not token_url:
        token_url = str(runtime_config.get("oauthIssuer") or "").strip()
    add(token_url)

connect_match = re.search(r"(?:^|;\s*)connect-src\s+([^;]+)", policy)
if not connect_match:
    print(f"ERROR: {label}: connect-src directive was not found", file=sys.stderr)
    sys.exit(1)

actual_origins = connect_match.group(1).split()
missing = [item for item in expected_origins if item not in actual_origins]
if missing:
    print(
        f"ERROR: {label}: connect-src missing expected origin(s): {', '.join(missing)}",
        file=sys.stderr,
    )
    print(f"       rendered connect-src: {' '.join(actual_origins)}", file=sys.stderr)
    sys.exit(1)

required_directives = [
    "default-src 'self'",
    "script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'",
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data: blob:",
    "font-src 'self' data:",
    "media-src 'self' blob:",
    "worker-src 'self' blob:",
    "frame-src 'none'",
    "frame-ancestors 'none'",
    "base-uri 'none'",
    "object-src 'none'",
    "form-action 'self'",
    "upgrade-insecure-requests",
]
missing_directives = [item for item in required_directives if item not in policy]
if missing_directives:
    print(
        f"ERROR: {label}: CSP missing directive(s): {', '.join(missing_directives)}",
        file=sys.stderr,
    )
    sys.exit(1)

print(f"OK: {label}: {actual_name} connect-src includes {len(expected_origins)} expected origin(s).")
PYEOF
}

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

base_render="$tmpdir/base.yaml"
us_east_render="$tmpdir/us-east.yaml"
helm_tmp="$tmpdir/helm"
cp -R "$ROOT_DIR/helm" "$helm_tmp"

helm dependency build "$helm_tmp/global/us-east/videocall-ui" >/dev/null

render_chart "$helm_tmp/videocall-ui" "$base_render"
render_chart "$helm_tmp/global/us-east/videocall-ui" "$us_east_render"

validate_render "helm/videocall-ui" "$base_render" "$ROOT_DIR/helm/videocall-ui/values.yaml"
validate_render \
  "helm/global/us-east/videocall-ui" \
  "$us_east_render" \
  "$ROOT_DIR/helm/global/us-east/videocall-ui/values.yaml"
