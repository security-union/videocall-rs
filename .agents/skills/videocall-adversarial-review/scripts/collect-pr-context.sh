#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <pr-url|pr-number> [host/owner/repo]" >&2
  exit 2
}

[[ $# -ge 1 && $# -le 2 ]] || usage
command -v gh >/dev/null || { echo "gh is required" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

target=$1
repo_arg=${2:-}

if [[ $target =~ ^https://([^/]+)/([^/]+)/([^/]+)/pull/([0-9]+)([/?#].*)?$ ]]; then
  host=${BASH_REMATCH[1]}
  owner=${BASH_REMATCH[2]}
  repo=${BASH_REMATCH[3]}
  pr=${BASH_REMATCH[4]}
elif [[ $target =~ ^[0-9]+$ && $repo_arg =~ ^([^/]+)/([^/]+)/([^/]+)$ ]]; then
  host=${BASH_REMATCH[1]}
  owner=${BASH_REMATCH[2]}
  repo=${BASH_REMATCH[3]}
  pr=$target
else
  usage
fi

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
base="repos/$owner/$repo"

fetch_array() {
  local endpoint=$1
  local output=$2
  gh api --hostname "$host" "$endpoint" --paginate --slurp | jq 'add // []' > "$output"
}

gh api --hostname "$host" "$base/pulls/$pr" > "$tmp_dir/pr.json"
fetch_array "$base/pulls/$pr/reviews?per_page=100" "$tmp_dir/reviews.json"
fetch_array "$base/issues/$pr/comments?per_page=100" "$tmp_dir/issue-comments.json"
fetch_array "$base/pulls/$pr/comments?per_page=100" "$tmp_dir/inline-comments.json"
fetch_array "$base/pulls/$pr/commits?per_page=100" "$tmp_dir/commits.json"
fetch_array "$base/pulls/$pr/files?per_page=100" "$tmp_dir/files.json"

head_sha=$(jq -r '.head.sha' "$tmp_dir/pr.json")
base_ref=$(jq -r '.base.ref' "$tmp_dir/pr.json")
encoded_base_ref=$(jq -rn --arg value "$base_ref" '$value | @uri')
gh api --hostname "$host" "$base/commits/$head_sha/check-runs?per_page=100" --paginate --slurp \
  | jq '[.[].check_runs[]]' > "$tmp_dir/checks.json"
gh api --hostname "$host" "$base/commits/$head_sha/status?per_page=100" --paginate --slurp \
  | jq '.[0] + {statuses: [.[].statuses[]]}' > "$tmp_dir/combined-status.json"

if gh api --hostname "$host" "$base/branches/$encoded_base_ref/protection/required_status_checks" \
  > "$tmp_dir/required-status-checks.json" 2> "$tmp_dir/required-status-checks.error"; then
  jq '. + {enabled: true}' "$tmp_dir/required-status-checks.json" \
    > "$tmp_dir/required-status-checks.with-state.json"
  mv "$tmp_dir/required-status-checks.with-state.json" "$tmp_dir/required-status-checks.json"
elif grep -Eq 'Required status checks not enabled|Branch not protected' \
  "$tmp_dir/required-status-checks.error"; then
  printf '%s\n' '{"enabled":false}' > "$tmp_dir/required-status-checks.json"
else
  jq -n --arg error "$(<"$tmp_dir/required-status-checks.error")" \
    '{enabled: null, collection_error: $error}' > "$tmp_dir/required-status-checks.json"
fi

if ! gh api --hostname "$host" "$base/rules/branches/$encoded_base_ref?per_page=100" \
  --paginate --slurp 2> "$tmp_dir/branch-rules.error" \
  | jq 'add // []' > "$tmp_dir/branch-rules.json"; then
  jq -n --arg error "$(<"$tmp_dir/branch-rules.error")" \
    '{collection_error: $error}' > "$tmp_dir/branch-rules.json"
fi

jq -n \
  --arg host "$host" \
  --arg repository "$owner/$repo" \
  --argjson number "$pr" \
  --slurpfile pr "$tmp_dir/pr.json" \
  --slurpfile reviews "$tmp_dir/reviews.json" \
  --slurpfile issue_comments "$tmp_dir/issue-comments.json" \
  --slurpfile inline_comments "$tmp_dir/inline-comments.json" \
  --slurpfile commits "$tmp_dir/commits.json" \
  --slurpfile files "$tmp_dir/files.json" \
  --slurpfile checks "$tmp_dir/checks.json" \
  --slurpfile combined_status "$tmp_dir/combined-status.json" \
  --slurpfile required_status_checks "$tmp_dir/required-status-checks.json" \
  --slurpfile branch_rules "$tmp_dir/branch-rules.json" \
  '{
    host: $host,
    repository: $repository,
    number: $number,
    pull_request: $pr[0],
    formal_reviews: $reviews[0],
    issue_comments: $issue_comments[0],
    inline_comments: $inline_comments[0],
    commits: $commits[0],
    files: $files[0],
    check_runs: $checks[0],
    combined_status: $combined_status[0],
    required_status_checks: $required_status_checks[0],
    branch_rules: $branch_rules[0]
  }'
