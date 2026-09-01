#!/usr/bin/env bash
# Commit the built image's pin on its own branch and open or refresh its PR (#2400).
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$DIR/../../.." && pwd)"
MANIFEST_RE='^e2e/bots-app/k8s/[^/]*\.ya?ml$'
COMMIT_NAME="${COMMIT_NAME:-videocall bots image}"
COMMIT_EMAIL="${COMMIT_EMAIL:-videocall-bots-image@users.noreply.invalid}"

RESP="$(mktemp)"
trap 'rm -f "$RESP"' EXIT

die() {
  echo "::error::repin-pr: $*" >&2
  exit 1
}

git_r() { git -C "$REPO_ROOT" "$@"; }
git_c() { git -C "$REPO_ROOT" -c "user.name=$COMMIT_NAME" -c "user.email=$COMMIT_EMAIL" "$@"; }

summary() {
  [ -n "${GITHUB_STEP_SUMMARY:-}" ] || return 0
  printf '%s\n' "$@" >>"$GITHUB_STEP_SUMMARY"
}

output() {
  [ -n "${GITHUB_OUTPUT:-}" ] || return 0
  printf '%s\n' "$@" >>"$GITHUB_OUTPUT"
}

# Anything the pin branch carries beyond the manifests would be merged by the PR.
stray_paths() {
  local names
  names="$(git_r diff --name-only "$@")"
  printf '%s\n' "$names" | grep -v -E "$MANIFEST_RE" | grep . || true
}

[ -n "${PINNED_REF:-}" ] || die "the build resolved no reference to pin — refusing to open a PR that pins nothing"
for v in PIN_BRANCH BASE_BRANCH API_URL REPO_SLUG SERVER_URL; do
  [ -n "${!v:-}" ] || die "$v is empty, so the job passed no context to pin into"
done
for tool in git curl; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is not on PATH, so the pin cannot be pushed"
done

base_sha="$(git_r rev-parse HEAD)"

# ls-remote --exit-code answers 2 for "no such branch"; anything above is a real
# failure, and treating it as absent would push a first pin over an existing one.
remote_tip=""
ls_status=0
git_r ls-remote --exit-code --heads origin "$PIN_BRANCH" >/dev/null 2>&1 || ls_status=$?
case "$ls_status" in
0)
  git_r fetch --quiet origin "+refs/heads/${PIN_BRANCH}:refs/remotes/origin/${PIN_BRANCH}"
  remote_tip="$(git_r rev-parse "refs/remotes/origin/${PIN_BRANCH}")"
  ;;
2) ;;
*) die "could not ask origin whether $PIN_BRANCH exists (git ls-remote exit $ls_status)" ;;
esac

git_r checkout --quiet -B "$PIN_BRANCH" "$base_sha"
"$DIR/repin.sh" "$PINNED_REF"

pinned="$("$DIR/prepull-image.sh" --print-image)"
[ "$pinned" = "$PINNED_REF" ] || die "after the re-pin the manifests name $pinned, not $PINNED_REF"

tag="${PINNED_REF%@*}"
tag="${tag##*:}"
title="chore(#2293): re-pin the bots fleet image to $tag"
compare="${SERVER_URL}/${REPO_SLUG}/compare/${BASE_BRANCH}...${PIN_BRANCH}?expand=1"

if git_r diff --quiet; then
  echo "repin-pr: $BASE_BRANCH already pins $PINNED_REF — nothing to open"
  summary '## Bots fleet image pin' '' "\`$BASE_BRANCH\` already pins \`$PINNED_REF\`; no PR was needed."
  output "action=already-pinned"
  exit 0
fi

stray="$(stray_paths)"
[ -z "$stray" ] || die "the re-pin touched files outside the manifests: $(printf '%s' "$stray" | tr '\n' ' ')"

# A re-run of the same build must not churn the branch — only the PR body below
# is then re-asserted, which is what a run that pushed but failed to open needs.
if [ -n "$remote_tip" ] && git_r diff --quiet "$remote_tip"; then
  git_r reset --hard --quiet "$remote_tip"
  echo "repin-pr: $PIN_BRANCH already carries $PINNED_REF"
else
  git_c commit --quiet -a -m "$title" -m "$PINNED_REF" -m "Built from ${base_sha} by ${RUN_URL:-a hand build}."

  # Supersede the previous pin instead of rewriting it: our tree wins and its
  # commit becomes a parent, so the push stays a fast-forward. Force is rejected
  # repo-wide by a pre-receive hook, so a non-fast-forward here is unrecoverable.
  if [ -n "$remote_tip" ] && ! git_r merge-base --is-ancestor "$remote_tip" HEAD; then
    git_c merge -s ours --no-edit -m "chore: supersede the previous bots-image pin" "$remote_tip"
    git_r merge-base --is-ancestor "$remote_tip" HEAD ||
      die "$PIN_BRANCH could only be advanced by forcing, which this remote rejects — delete the branch and re-run"
  fi

  stray="$(stray_paths "$base_sha" HEAD)"
  [ -z "$stray" ] || die "$PIN_BRANCH carries changes outside the manifests: $(printf '%s' "$stray" | tr '\n' ' ') — delete the branch and re-run"

  git_r push --quiet origin "HEAD:refs/heads/${PIN_BRANCH}" ||
    die "pushing $PIN_BRANCH was rejected; this never forces, so re-run the job after checking who else advanced it"
fi

body="\`$PINNED_REF\`

Built from ${base_sha}${RUN_URL:+ by ${RUN_URL}}.

The fleet manifests must name the image that ships the tree they are measured
against (#2293), so merge this before the next fleet run and warm with
\`./e2e/bots-app/k8s/prepull-image.sh run\`.

Opened by the \`Build Bots Fleet Image (HCL)\` workflow."

if [ -z "${PR_TOKEN:-}" ]; then
  echo "::warning::repin-pr: pushed $PIN_BRANCH but opened no PR — no BOTS_REPIN_TOKEN is configured. Open it here: $compare"
  summary '## Bots fleet image pin — PR NOT opened' '' \
    "The pin is committed and pushed to \`$PIN_BRANCH\`, but no \`BOTS_REPIN_TOKEN\` secret is configured, so this run could not open the PR." '' \
    "Open it in one click: ${compare}" '' "$body"
  output "action=pushed" "compare_url=$compare"
  exit 0
fi

command -v jq >/dev/null 2>&1 || die "jq is not on PATH, so the API payload cannot be built safely"

api() {
  local method="$1" path="$2" data="${3:-}" code
  if [ -n "$data" ]; then
    code="$(curl -sS -o "$RESP" -w '%{http_code}' -X "$method" \
      -H "Authorization: token ${PR_TOKEN}" -H "Accept: application/vnd.github+json" \
      -d "$data" "${API_URL}${path}")" || die "the $method $path call could not reach $API_URL"
  else
    code="$(curl -sS -o "$RESP" -w '%{http_code}' -X "$method" \
      -H "Authorization: token ${PR_TOKEN}" -H "Accept: application/vnd.github+json" \
      "${API_URL}${path}")" || die "the $method $path call could not reach $API_URL"
  fi
  printf '%s' "$code"
}

# Filtered on head alone: a human retargeting the PR's base hides it from a
# {base,head} search, and `create` then 422s against it forever.
find_open_pr() {
  local code
  code="$(api GET "/repos/${REPO_SLUG}/pulls?state=open&per_page=100&head=${REPO_SLUG%%/*}:${PIN_BRANCH}")"
  [ "$code" = "200" ] || die "listing open PRs for $PIN_BRANCH failed (HTTP $code): $(head -c 400 "$RESP")"
  jq -r '.[0].number // empty' "$RESP"
}

update_pr() {
  local number="$1" code
  code="$(api PATCH "/repos/${REPO_SLUG}/pulls/${number}" "$(jq -nc --arg t "$title" --arg b "$body" '{title:$t,body:$b}')")"
  [ "$code" = "200" ] || die "updating PR #${number} failed (HTTP $code): $(head -c 400 "$RESP")"
}

number="$(find_open_pr)"
if [ -n "$number" ]; then
  update_pr "$number"
  action="updated"
else
  code="$(api POST "/repos/${REPO_SLUG}/pulls" "$(jq -nc --arg t "$title" --arg b "$body" --arg h "$PIN_BRANCH" --arg base "$BASE_BRANCH" '{title:$t,body:$b,head:$h,base:$base}')")"
  if [ "$code" = "201" ]; then
    number="$(jq -r '.number' "$RESP")"
    action="opened"
  else
    refused="$(head -c 400 "$RESP")"
    number="$(find_open_pr)"
    [ -n "$number" ] || die "opening the re-pin PR failed (HTTP $code): $refused"
    update_pr "$number"
    action="updated"
  fi
fi

url="${SERVER_URL}/${REPO_SLUG}/pull/${number}"
echo "repin-pr: $action $url"
summary '## Bots fleet image pin' '' "${action^} ${url} — merge it before the next fleet run." '' "$body"
output "action=$action" "pr_url=$url" "pr_number=$number"
