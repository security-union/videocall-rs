#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

ROOT_DIR="${CHECK_VENDORED_PINS_ROOT:-$(git rev-parse --show-toplevel)}"
PINS="$ROOT_DIR/vendor/advisories/Cargo.toml"
status=0
checked=0

for dir in "$ROOT_DIR"/vendor/*/; do
  name="$(basename "$dir")"
  [[ "$name" == "advisories" ]] && continue
  version="$(awk -F'"' '/^\[/ { p = ($0 == "[package]") } p && /^version = / { print $2; exit }' "$dir/Cargo.toml")"
  pin="$(awk -F'"' -v n="$name" '$0 ~ "^" n " = " { print $2; exit }' "$PINS")"
  if [[ -z "$version" ]]; then
    echo "ERROR: no [package] version found in vendor/$name/Cargo.toml" >&2
    status=1
  elif [[ "$pin" != "=$version" ]]; then
    echo "ERROR: vendor/$name is $version but vendor/advisories/Cargo.toml pins '${pin:-nothing}'" >&2
    status=1
  else
    echo "OK: vendor/$name $version is pinned for the advisory scan"
  fi
  checked=$((checked + 1))
done

if [[ "$checked" -eq 0 ]]; then
  echo "ERROR: no vendored crates found under vendor/; refusing to pass vacuously" >&2
  exit 2
fi
exit "$status"
