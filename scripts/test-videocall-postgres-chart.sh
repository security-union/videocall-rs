#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(git rev-parse --show-toplevel)"
SOURCE_CHART="$ROOT_DIR/helm/videocall-postgres"
DEPENDENCY_BUILDER="$ROOT_DIR/scripts/build-videocall-postgres-dependencies.sh"
HELM_BIN="${HELM_BIN:-helm}"

if ! command -v "$HELM_BIN" >/dev/null 2>&1; then
  echo "ERROR: helm is required." >&2
  exit 2
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

export HELM_CACHE_HOME="$tmp_dir/helm/cache"
export HELM_CONFIG_HOME="$tmp_dir/helm/config"
export HELM_DATA_HOME="$tmp_dir/helm/data"

dependency_repository() {
  "$HELM_BIN" dependency list "$1" |
    awk '$1 == "postgresql" { print $3; found = 1 } END { exit !found }'
}

verify_chart_dependencies() {
  HELM_BIN="$HELM_BIN" "$DEPENDENCY_BUILDER" "$1" >/dev/null
}

chart_dir="$tmp_dir/videocall-postgres"
cp -a "$SOURCE_CHART" "$chart_dir"
rm -rf "$chart_dir/charts"
verify_chart_dependencies "$chart_dir"

stale_chart="$tmp_dir/stale-chart"
cp -a "$chart_dir" "$stale_chart"
repository="$(dependency_repository "$stale_chart")"
stale_repository="${repository%/}/"
awk -v replacement="    repository: $stale_repository" '
  !replaced && /^[[:space:]]+repository:/ {
    print replacement
    replaced = 1
    next
  }
  { print }
  END { if (!replaced) exit 1 }
' "$stale_chart/Chart.yaml" >"$stale_chart/Chart.yaml.next"
mv "$stale_chart/Chart.yaml.next" "$stale_chart/Chart.yaml"

if verify_chart_dependencies "$stale_chart" \
  >"$tmp_dir/stale-lock.out" 2>"$tmp_dir/stale-lock.err"; then
  echo "ERROR: dependency validation accepted a stale Chart.lock." >&2
  exit 1
fi
if ! grep -Eiq 'lock file.*out of sync' "$tmp_dir/stale-lock.err"; then
  echo "ERROR: stale-lock validation failed for an unexpected reason:" >&2
  cat "$tmp_dir/stale-lock.err" >&2
  exit 1
fi

if "$HELM_BIN" template postgres "$chart_dir" \
  >"$tmp_dir/missing.out" 2>"$tmp_dir/missing.err"; then
  echo "ERROR: helm template accepted the chart without cluster values." >&2
  exit 1
fi

normalized_errors="$tmp_dir/missing.normalized"
tr "/'" ".." <"$tmp_dir/missing.err" >"$normalized_errors"
required_paths=(
  'postgresql\.auth\.database'
  'postgresql\.image\.tag'
  'postgresql\.primary\.persistence\.storageClass'
  'postgresql\.primary\.persistence\.size'
)
for path in "${required_paths[@]}"; do
  if ! grep -Eq "$path" "$normalized_errors"; then
    echo "ERROR: missing cluster values did not reject required path $path:" >&2
    cat "$tmp_dir/missing.err" >&2
    exit 1
  fi
done
if ! grep -Eq 'postgresql\.primary\.resources' "$normalized_errors" ||
  ! grep -Eq '\blimits\b' "$normalized_errors" ||
  ! grep -Eq '\brequests\b' "$normalized_errors"; then
  echo "ERROR: missing cluster values did not reject required resource maps:" >&2
  cat "$tmp_dir/missing.err" >&2
  exit 1
fi

shopt -s nullglob
cluster_values=("$ROOT_DIR"/helm/global/*/postgres/values.yaml)
if ((${#cluster_values[@]} == 0)); then
  echo "ERROR: no cluster PostgreSQL values files were found." >&2
  exit 1
fi

probe_chart="$tmp_dir/value-probe"
mkdir -p "$probe_chart/templates"
printf '%s\n' \
  'apiVersion: v2' \
  'name: value-probe' \
  'version: 0.1.0' >"$probe_chart/Chart.yaml"
printf '%s\n' \
  'apiVersion: v1' \
  'kind: ConfigMap' \
  'metadata:' \
  '  name: value-probe' \
  'data:' \
  '  size: {{ required "persistence size is required" .Values.postgresql.primary.persistence.size | quote }}' \
  '  tag: {{ required "image tag is required" .Values.postgresql.image.tag | quote }}' \
  '  digest: {{ .Values.postgresql.image.digest | default "" | quote }}' \
  >"$probe_chart/templates/value.yaml"

probe_value() {
  "$HELM_BIN" template value-probe "$probe_chart" -f "$1" |
    awk -v key="$2:" '$1 == key { gsub(/"/, "", $2); print $2; exit }'
}

for values_file in "${cluster_values[@]}"; do
  values_dir="$(dirname "$values_file")"
  if [[ -e "$values_dir/Chart.yaml" || -e "$values_dir/Chart.lock" ]]; then
    echo "ERROR: $values_dir defines a chart that bypasses $SOURCE_CHART." >&2
    exit 1
  fi

  cluster_name="$(basename "$(dirname "$(dirname "$values_file")")")"
  rendered="$tmp_dir/$cluster_name.out"

  "$HELM_BIN" template postgres "$chart_dir" -f "$values_file" >"$rendered"
  expected_size="$(probe_value "$values_file" size)"
  mapfile -t rendered_sizes < <(
    awk '
      $1 == "storage:" {
        gsub(/"/, "", $2)
        print $2
      }
    ' "$rendered"
  )

  if [[ -z "$expected_size" || ${#rendered_sizes[@]} -ne 1 ||
    "${rendered_sizes[0]}" != "$expected_size" ]]; then
    echo "ERROR: $values_file did not render its configured PVC size." >&2
    exit 1
  fi

  mapfile -t rendered_images < <(
    awk '$1 == "image:" { gsub(/"/, "", $2); print $2 }' "$rendered"
  )
  if ((${#rendered_images[@]} == 0)); then
    echo "ERROR: $values_file rendered no container images." >&2
    exit 1
  fi
  for image in "${rendered_images[@]}"; do
    if [[ "$image" == *:latest ]]; then
      echo "ERROR: $values_file rendered the floating image $image." >&2
      exit 1
    fi
  done

  expected_digest="$(probe_value "$values_file" digest)"
  expected_tag="$(probe_value "$values_file" tag)"
  if [[ -n "$expected_digest" ]]; then
    expected_ref_suffix="@$expected_digest"
  else
    expected_ref_suffix=":$expected_tag"
  fi
  pinned=0
  for image in "${rendered_images[@]}"; do
    if [[ "$image" == *"$expected_ref_suffix" ]]; then
      pinned=1
      break
    fi
  done
  if ((pinned == 0)); then
    echo "ERROR: $values_file did not render its configured image pin" \
      "($expected_ref_suffix); rendered: ${rendered_images[*]}" >&2
    exit 1
  fi
done

complete_values="${cluster_values[0]}"
printf '%s\n' \
  'postgresql:' \
  '  primary:' \
  '    persistence:' \
  '      enabled: false' \
  '      storageClass: ""' \
  '      size: ""' >"$tmp_dir/persistence-disabled.yaml"
"$HELM_BIN" template postgres "$chart_dir" \
  -f "$complete_values" \
  -f "$tmp_dir/persistence-disabled.yaml" \
  >"$tmp_dir/persistence-disabled.out"
if grep -Fq 'volumeClaimTemplates:' "$tmp_dir/persistence-disabled.out"; then
  echo "ERROR: persistence.enabled=false rendered a PVC." >&2
  exit 1
fi

printf '%s\n' \
  'postgresql:' \
  '  primary:' \
  '    persistence:' \
  '      existingClaim: existing-pvc' \
  '      storageClass: ""' \
  '      size: ""' >"$tmp_dir/existing-claim.yaml"
"$HELM_BIN" template postgres "$chart_dir" \
  -f "$complete_values" \
  -f "$tmp_dir/existing-claim.yaml" \
  >"$tmp_dir/existing-claim.out"
if grep -Fq 'volumeClaimTemplates:' "$tmp_dir/existing-claim.out" ||
  ! grep -Fq 'claimName: existing-pvc' "$tmp_dir/existing-claim.out"; then
  echo "ERROR: persistence.existingClaim did not reuse the configured PVC." >&2
  exit 1
fi

package_dir="$tmp_dir/package"
mkdir -p "$package_dir"
"$HELM_BIN" package "$chart_dir" --destination "$package_dir" >/dev/null
package="$(find "$package_dir" -maxdepth 1 -name '*.tgz' -print -quit)"
if [[ -z "$package" ]]; then
  echo "ERROR: helm package did not create an archive." >&2
  exit 1
fi
tar -tzf "$package" >"$tmp_dir/package-contents.txt"
if grep -Eq '^[^/]+/(ci|tests)/' "$tmp_dir/package-contents.txt"; then
  echo "ERROR: the packaged chart contains CI test fixtures." >&2
  exit 1
fi

echo "OK: videocall-postgres dependency, schema, persistence, package, and cluster-value checks passed."
