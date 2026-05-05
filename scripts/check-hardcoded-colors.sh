#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/check-hardcoded-colors.sh [--all|--cached|--unstaged]

Flags newly added color literals (hex/rgb/rgba/hsl/hsla) in dioxus-ui files,
except in allowlisted token definition files.

Options:
  --all       Inspect staged and unstaged changes (default)
  --cached    Inspect staged changes only
  --unstaged  Inspect unstaged changes only
EOF
}

MODE="all"
if [[ $# -gt 1 ]]; then
  usage
  exit 2
fi
if [[ $# -eq 1 ]]; then
  case "$1" in
    --all)
      MODE="all"
      ;;
    --cached)
      MODE="cached"
      ;;
    --unstaged)
      MODE="unstaged"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
fi

ROOT_DIR="$(git rev-parse --show-toplevel)"
cd "$ROOT_DIR"

collect_added_lines() {
  case "$MODE" in
    all)
      {
        git diff --no-color --unified=0 -- dioxus-ui/src dioxus-ui/static
        git diff --no-color --unified=0 --cached -- dioxus-ui/src dioxus-ui/static
      }
      ;;
    cached)
      git diff --no-color --unified=0 --cached -- dioxus-ui/src dioxus-ui/static
      ;;
    unstaged)
      git diff --no-color --unified=0 -- dioxus-ui/src dioxus-ui/static
      ;;
  esac
}

if ! collect_added_lines | awk '
  /^\+\+\+ b\// {
    file = substr($0, 7)
    next
  }
  /^\+/ && $0 !~ /^\+\+\+/ {
    line = substr($0, 2)
    if (file ~ /^(dioxus-ui\/static\/global[.]css|dioxus-ui\/static\/tokens-v0[.]json|dioxus-ui\/src\/theme[.]rs)$/) {
      next
    }
    if (line ~ /@token-exempt/) { next }
    if (line ~ /#[0-9A-Fa-f]{3,8}([[:space:][:punct:]]|$)|rgba?[[:space:]]*\([^)]*\)|hsla?[[:space:]]*\([^)]*\)/) {
      key = file ":" line
      if (!(key in seen)) {
        printf("%s: %s\n", file, line)
        seen[key] = 1
      }
      found = 1
    }
  }
  END {
    exit found ? 1 : 0
  }
'; then
  echo
  echo "Hardcoded color literals found in non-token files."
  echo "Move these into dioxus-ui/static/global.css or dioxus-ui/src/theme.rs."
  exit 1
fi

echo "No new hardcoded color literals found outside allowlisted token files."
