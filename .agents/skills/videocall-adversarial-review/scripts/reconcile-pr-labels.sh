#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage:
  reconcile-pr-labels.sh host/owner/repo PR start
  reconcile-pr-labels.sh host/owner/repo PR abort
  reconcile-pr-labels.sh host/owner/repo PR finish approve
  reconcile-pr-labels.sh host/owner/repo PR finish changes [--needs-tests] [--conflicts]
  reconcile-pr-labels.sh host/owner/repo PR finish blocked (--needs-tests|--conflicts)...
EOF
  exit 2
}

[[ $# -ge 3 ]] || usage
command -v gh >/dev/null || { echo "gh is required" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

repo_arg=$1
pr=$2
action=$3
shift 3

[[ $repo_arg =~ ^[^/]+/[^/]+/[^/]+$ && $pr =~ ^[0-9]+$ ]] || usage
IFS=/ read -r host owner repo <<< "$repo_arg"
base="repos/$owner/$repo"

encode() { jq -rn --arg value "$1" '$value | @uri'; }

ensure_label() {
  local name=$1 color=$2 description=$3 encoded
  encoded=$(encode "$name")
  if ! gh api --hostname "$host" "$base/labels/$encoded" >/dev/null 2>&1; then
    gh api --hostname "$host" -X POST "$base/labels" \
      -f name="$name" -f color="$color" -f description="$description" >/dev/null
  fi
}

set_workflow_labels() {
  local actual attached expected label
  local -a final=() desired=("$@")
  attached=$(gh api --hostname "$host" "$base/issues/$pr/labels?per_page=100" --paginate \
    --jq '.[].name')

  while IFS= read -r label; do
    [[ -n $label ]] || continue
    case $label in
      "READY FOR REVIEW"|"REVIEW IN PROGRESS..."|"MERGE APPROVED"|"NEEDS CHANGES"|"NEEDS TESTS"|"RESOLVE CONFLICTS") ;;
      *) final+=("$label") ;;
    esac
  done <<< "$attached"
  final+=("${desired[@]}")

  jq -n --args '$ARGS.positional | unique | {labels: .}' -- "${final[@]}" \
    | gh api --hostname "$host" -X PUT "$base/issues/$pr/labels" --input - >/dev/null

  attached=$(gh api --hostname "$host" "$base/issues/$pr/labels?per_page=100" --paginate \
    --jq '.[].name')
  expected=$(printf '%s\n' "${desired[@]}" | sort -u)
  actual=$(grep -Fx \
    -e "READY FOR REVIEW" \
    -e "REVIEW IN PROGRESS..." \
    -e "MERGE APPROVED" \
    -e "NEEDS CHANGES" \
    -e "NEEDS TESTS" \
    -e "RESOLVE CONFLICTS" <<< "$attached" | sort -u || true)
  [[ $actual == "$expected" ]] || {
    echo "workflow label reconciliation did not reach the requested state" >&2
    exit 1
  }
}

case $action in
  start)
    (($# == 0)) || usage
    ensure_label "REVIEW IN PROGRESS..." "FBCA04" "A reviewer is actively reviewing this PR."
    set_workflow_labels "REVIEW IN PROGRESS..."
    ;;
  abort)
    (($# == 0)) || usage
    ensure_label "READY FOR REVIEW" "0E8A16" "Ready for code review."
    set_workflow_labels "READY FOR REVIEW"
    ;;
  finish)
    (($# >= 1)) || usage
    result=$1
    shift
    needs_tests=false
    conflicts=false
    for flag in "$@"; do
      case $flag in
        --needs-tests) needs_tests=true ;;
        --conflicts) conflicts=true ;;
        *) usage ;;
      esac
    done

    case $result in
      approve)
        [[ $needs_tests == false && $conflicts == false ]] || {
          echo "approve is incompatible with test gaps or conflicts" >&2
          exit 2
        }
        ;;
      changes) ;;
      blocked)
        [[ $needs_tests == true || $conflicts == true ]] || usage
        ;;
      *) usage ;;
    esac

    labels=()
    if [[ $result == approve ]]; then
      ensure_label "MERGE APPROVED" "0E8A16" "Review completed and approved for merge."
      labels+=("MERGE APPROVED")
    elif [[ $result == changes ]]; then
      ensure_label "NEEDS CHANGES" "D93F0B" "Blocking review changes are required."
      labels+=("NEEDS CHANGES")
    fi
    if [[ $needs_tests == true ]]; then
      ensure_label "NEEDS TESTS" "B60205" "Required automated test coverage is missing or inadequate."
      labels+=("NEEDS TESTS")
    fi
    if [[ $conflicts == true ]]; then
      ensure_label "RESOLVE CONFLICTS" "FF6B35" "Merge conflicts must be resolved before approval."
      labels+=("RESOLVE CONFLICTS")
    fi
    ((${#labels[@]} > 0)) || { echo "no terminal verdict label selected" >&2; exit 2; }

    set_workflow_labels "${labels[@]}"
    ;;
  *) usage ;;
esac
