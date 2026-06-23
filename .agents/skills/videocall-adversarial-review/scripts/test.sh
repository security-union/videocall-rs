#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
mkdir -p "$tmp_dir/bin"

cat > "$tmp_dir/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${FAKE_GH_LOG:?}"
args=" $* "

case "$args" in
  *"commits/deadbeef/check-runs"*) printf '%s\n' '[{"check_runs":[]}]' ;;
  *"commits/deadbeef/status"*) printf '%s\n' '[{"state":"success","statuses":[]}]' ;;
  *"branches/PR-staging/protection/required_status_checks"*)
    printf '%s\n' '{"strict":true,"contexts":["required-check"]}'
    ;;
  *"rules/branches/PR-staging"*) printf '%s\n' '[[]]' ;;
  *"pulls/42/reviews"*|*"issues/42/comments"*|*"pulls/42/comments"*|*"pulls/42/commits"*|*"pulls/42/files"*)
    printf '%s\n' '[[]]'
    ;;
  *"pulls/42"*) printf '%s\n' '{"head":{"sha":"deadbeef"},"base":{"ref":"PR-staging"}}' ;;
  *" -X PUT "*"issues/42/labels"*)
    [[ ${FAKE_GH_FAIL_PUT:-false} == false ]] || exit 1
    payload=$(cat)
    jq -r '.labels[]' <<< "$payload" | sort -u > "$FAKE_GH_STATE"
    ;;
  *"issues/42/labels?per_page=100"*" --jq "*) cat "$FAKE_GH_STATE" ;;
  *) cat >/dev/null || true ;;
esac
EOF
chmod +x "$tmp_dir/bin/gh"

export PATH="$tmp_dir/bin:$PATH"
export FAKE_GH_LOG="$tmp_dir/gh.log"
export FAKE_GH_STATE="$tmp_dir/labels"
: > "$FAKE_GH_LOG"
: > "$FAKE_GH_STATE"

set_labels() {
  printf '%s\n' "$@" | sort -u > "$FAKE_GH_STATE"
}

assert_labels() {
  printf '%s\n' "$@" | sort -u > "$tmp_dir/expected-labels"
  sort -u "$FAKE_GH_STATE" > "$tmp_dir/actual-labels"
  diff -u "$tmp_dir/expected-labels" "$tmp_dir/actual-labels"
}

"$script_dir/collect-pr-context.sh" \
  'https://github01.hclpnp.com/labs-projects/videocall/pull/42?diff=split' \
  > "$tmp_dir/context.json"

if ! jq -e '
  .host == "github01.hclpnp.com" and
  .repository == "labs-projects/videocall" and
  .number == 42 and
  .pull_request.head.sha == "deadbeef" and
  (.formal_reviews | length) == 0 and
  (.issue_comments | length) == 0 and
  (.inline_comments | length) == 0 and
  (.commits | length) == 0 and
  (.files | length) == 0 and
  (.check_runs | length) == 0 and
  .combined_status.state == "success" and
  .required_status_checks.enabled == true and
  .required_status_checks.contexts == ["required-check"] and
  .branch_rules == []
' "$tmp_dir/context.json" >/dev/null; then
  cat "$tmp_dir/context.json" >&2
  exit 1
fi

labels="$script_dir/reconcile-pr-labels.sh"
repo=github01.hclpnp.com/labs-projects/videocall

set_labels "READY FOR REVIEW" "unrelated"
"$labels" "$repo" 42 start
assert_labels "REVIEW IN PROGRESS..." "unrelated"

"$labels" "$repo" 42 abort
assert_labels "READY FOR REVIEW" "unrelated"

set_labels "READY FOR REVIEW" "NEEDS CHANGES" "NEEDS TESTS" "RESOLVE CONFLICTS"
"$labels" "$repo" 42 finish approve
assert_labels "MERGE APPROVED"

set_labels "REVIEW IN PROGRESS..." "MERGE APPROVED"
"$labels" "$repo" 42 finish changes --needs-tests --conflicts
assert_labels "NEEDS CHANGES" "NEEDS TESTS" "RESOLVE CONFLICTS"

set_labels "REVIEW IN PROGRESS..." "MERGE APPROVED" "NEEDS CHANGES"
"$labels" "$repo" 42 finish blocked --needs-tests
assert_labels "NEEDS TESTS"

set_labels "REVIEW IN PROGRESS..." "unrelated"
export FAKE_GH_FAIL_PUT=true
if "$labels" "$repo" 42 finish approve >/dev/null 2>&1; then
  echo "simulated atomic replacement failure unexpectedly succeeded" >&2
  exit 1
fi
unset FAKE_GH_FAIL_PUT
assert_labels "REVIEW IN PROGRESS..." "unrelated"
"$labels" "$repo" 42 finish approve
assert_labels "MERGE APPROVED" "unrelated"

if "$labels" "$repo" 42 finish approve --needs-tests >/dev/null 2>&1; then
  echo "approve unexpectedly accepted a test gap" >&2
  exit 1
fi

printf '%s\n' "skill scripts passed"
