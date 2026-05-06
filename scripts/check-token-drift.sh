#!/usr/bin/env bash
# Validates that every v0_core CSS token declared in tokens-v0.json
# exists in global.css with the same value.
#
# Usage: bash scripts/check-token-drift.sh [--json path] [--css path]
set -euo pipefail

ROOT_DIR="$(git rev-parse --show-toplevel)"
JSON_FILE="${1:-$ROOT_DIR/dioxus-ui/static/tokens-v0.json}"
CSS_FILE="${2:-$ROOT_DIR/dioxus-ui/static/global.css}"

if [[ ! -f "$JSON_FILE" ]]; then
  echo "ERROR: Token contract not found: $JSON_FILE"
  exit 2
fi
if [[ ! -f "$CSS_FILE" ]]; then
  echo "ERROR: CSS token source not found: $CSS_FILE"
  exit 2
fi

# Requires: python3 (available in the project's nix dev shell and most CI images)
python3 - "$JSON_FILE" "$CSS_FILE" <<'PYEOF'
import sys, json, re

json_file, css_file = sys.argv[1], sys.argv[2]

with open(json_file) as f:
    contract = json.load(f)

with open(css_file) as f:
    css = f.read()

# Parse :root { ... } block — grab everything between the first :root { and its closing }
root_block = re.search(r':root\s*\{([^}]+(?:\{[^}]*\}[^}]*)*)\}', css, re.DOTALL)
if not root_block:
    print("ERROR: Could not locate :root block in", css_file)
    sys.exit(2)

root_css = root_block.group(1)

# Build a dict of token → raw value from the :root block.
# Handles:
#   --token: value;
#   --token: var(--other);  (alias — resolve one level)
token_re = re.compile(r'(--[\w-]+)\s*:\s*([^;]+);')
raw_tokens: dict[str, str] = {}
for token, value in token_re.findall(root_css):
    raw_tokens[token] = value.strip()

def resolve(token: str, seen: set | None = None) -> str | None:
    """Resolve one level of var(--x) alias."""
    if seen is None:
        seen = set()
    if token in seen:
        return None  # cycle guard
    seen.add(token)
    val = raw_tokens.get(token)
    if val is None:
        return None
    alias = re.fullmatch(r'var\((--[\w-]+)\)', val)
    if alias:
        return resolve(alias.group(1), seen)
    return val

def normalise(v: str) -> str:
    """Lower-case and collapse whitespace for comparison."""
    return re.sub(r'\s+', '', v.lower())

errors: list[str] = []

all_css_tokens: list[dict] = (
    contract.get("v0_core", {}).get("css", [])
    + contract.get("v0_1_component_extension", {}).get("css", [])
)

for entry in all_css_tokens:
    tok = entry["token"]
    expected = entry["value"]
    actual = resolve(tok)
    if actual is None:
        errors.append(f"  MISSING  {tok}  (expected: {expected})")
        continue
    if normalise(actual) != normalise(expected):
        errors.append(f"  DRIFT    {tok}\n"
                      f"           contract : {expected}\n"
                      f"           css file : {actual}")

if errors:
    print(f"\nToken drift detected between {json_file} and {css_file}:\n")
    for e in errors:
        print(e)
    print(f"\n{len(errors)} issue(s) found.")
    print("Update tokens-v0.json or global.css so they match.")
    sys.exit(1)

print(f"OK — all {len(all_css_tokens)} contract tokens match global.css.")
PYEOF
