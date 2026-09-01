#!/usr/bin/env bash
# Warm the fleet image onto every schedulable worker (#2294). Image and pull
# policy come from statefulset.yaml only, never from an argument.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATEFULSET="$DIR/statefulset.yaml"
TEMPLATE="$DIR/image-prepull-job.yaml.tmpl"
REPO_ROOT="$(cd "$DIR/../../.." && pwd)"
# Excluded: in the image but never executed by a bot — k8s/ also carries the pin
# this reads, and the dashboard's dist/ is dockerignored with no build step here.
DRIFT_PATHS=(
  e2e
  .dockerignore
  ':(exclude)e2e/bots-app/k8s'
  ':(exclude)e2e/bots-app/dashboard'
  ':(exclude)e2e/tests'
  ':(exclude)*.test.ts'
  ':(exclude)*.md'
)
NS="bot-load"
JOB="videocall-bots-image-prepull"
SELECTOR="app.kubernetes.io/name=videocall-bots-image-prepull"
POLL_SECONDS="${POLL_SECONDS:-5}"
DEADLINE_SECONDS="${DEADLINE_SECONDS:-1800}"
BACKOFF_GRACE_POLLS="${BACKOFF_GRACE_POLLS:-12}"

die() {
  echo "prepull: $*" >&2
  exit 1
}

for knob in POLL_SECONDS DEADLINE_SECONDS BACKOFF_GRACE_POLLS; do
  case "${!knob}" in
  '' | *[!0-9]* | 0) die "$knob must be a positive integer, got: ${!knob}" ;;
  esac
done

# Outlast the Job's own deadline, so its Failed condition reports before we do.
WAIT_SECONDS=$((DEADLINE_SECONDS + 60))

# Two containers make "the fleet image" ambiguous, and the value reaches a
# `sed s|..|VALUE|` replacement where GNU sed's `e` flag would execute it.
manifest_value() {
  local key="$1" out
  out="$(grep -E "^[[:space:]]*${key}:[[:space:]]" "$STATEFULSET" | sed -E -e "s/^[[:space:]]*${key}:[[:space:]]*//" -e 's/[[:space:]]*#.*$//' | tr -d '"' | tr -d "'" || true)"
  [ "$(printf '%s\n' "$out" | grep -c . || true)" = "1" ] || die "expected exactly one \`${key}:\` in $STATEFULSET, got: ${out:-<none>}"
  case "$out" in
  *[!A-Za-z0-9._/:@-]*) die "illegal character in ${key}: $out" ;;
  esac
  printf '%s' "$out"
}

# NUL-delimited: a filename containing a space must not word-split out of scan.
manifests() {
  find "$DIR" -maxdepth 1 -type f \( -name '*.yaml' -o -name '*.yml' \) -print0 | sort -z
}

check_agreement() {
  local want policy refs status
  local -a files=()
  want="$(manifest_value image)"
  policy="$(manifest_value imagePullPolicy)"
  [ "$policy" = "IfNotPresent" ] || die "imagePullPolicy is $policy — warming is pointless, every pod re-hits the registry"
  mapfile -d '' -t files < <(manifests)
  [ "${#files[@]}" -gt 0 ] || die "no .yaml/.yml manifests found in $DIR"
  refs="$(grep -hE '^[[:space:]]*-?[[:space:]]*image:[[:space:]]*[^[:space:]]*videocall-bots-app' "${files[@]}")" && status=0 || status=$?
  # grep 1 is no-match (dies below); 2 is unreadable, never agreement.
  [ "$status" -le 1 ] || die "could not read every manifest in $DIR (grep exit $status)"
  refs="$(printf '%s' "$refs" | sed -E 's/^[[:space:]]*-?[[:space:]]*image:[[:space:]]*//' | tr -d '\042\047' | grep . | sort -u || true)"
  [ -n "$refs" ] || die "no bots-app image: line found in $DIR/*.{yaml,yml}"
  [ "$refs" = "$want" ] || die "manifests disagree on the bots-app image: $(printf '%s' "$refs" | tr '\n' ' ')"
}

pinned_commit() {
  local tag sha
  tag="$(manifest_value image)"
  # Digest first: it carries its own `sha256:`, which would win `##*:`.
  tag="${tag%%@*}"
  tag="${tag##*:}"
  sha="${tag##*-}"
  case "$sha" in
  '' | *[!0-9a-f]*) die "cannot read a commit from the pinned tag '$tag' — expected <version>-<date>-<sha>, as build.sh produces" ;;
  esac
  [ "${#sha}" -ge 7 ] || die "the pinned tag's commit '$sha' is shorter than 7 characters — too ambiguous to resolve"
  printf '%s' "$sha"
}

check_source_drift() {
  local mode="${1:-fatal}" sha drifted new report
  if [ "${ALLOW_SOURCE_DRIFT:-0}" = "1" ]; then
    echo "prepull: ALLOW_SOURCE_DRIFT=1 — NOT checking the pinned image against the tree"
    return 0
  fi
  command -v git >/dev/null 2>&1 || die "git is not on PATH, so the pin cannot be checked; set ALLOW_SOURCE_DRIFT=1 to warm anyway"
  git -C "$REPO_ROOT" rev-parse --git-dir >/dev/null 2>&1 || die "$REPO_ROOT is not a git checkout, so the pin cannot be checked; set ALLOW_SOURCE_DRIFT=1 to warm anyway"
  sha="$(pinned_commit)"
  git -C "$REPO_ROOT" rev-parse --verify --quiet "${sha}^{commit}" >/dev/null ||
    die "the pin was built from commit $sha, which this clone does not have — fetch (or unshallow) it, or set ALLOW_SOURCE_DRIFT=1"
  drifted="$(git -C "$REPO_ROOT" diff --name-only "$sha" -- "${DRIFT_PATHS[@]}" || die "git diff against $sha failed")"
  new="$(git -C "$REPO_ROOT" ls-files --others --exclude-standard -- "${DRIFT_PATHS[@]}")"
  drifted="$(printf '%s\n%s' "$drifted" "$new" | grep . | sort -u || true)"
  if [ -n "$drifted" ]; then
    report="the pinned image was built from $sha, but these ship INSIDE it and differ from your tree:
$(printf '%s\n' "$drifted" | sed 's/^/  /')
Pin the ref the newest 'Build Bots Fleet Image (HCL)' run printed (or build.sh by hand) in every manifest, then warm.
ALLOW_SOURCE_DRIFT=1 warms the old image anyway (valid mid-A/B, never for a fresh run)."
    [ "$mode" = "warn" ] || die "$report"
    echo "prepull: WARNING — $report" >&2
    return 0
  fi
  echo "prepull: pinned image matches the tree at $sha"
}

# Effects are compared whole: `PreferNoSchedule` is soft and stays in; a NotReady
# node's `NoExecute` stays out or it inflates completions past the deadline.
target_nodes() {
  kubectl get nodes \
    -o jsonpath='{range .items[*]}{.metadata.name}{"|"}{.spec.unschedulable}{"|"}{range .spec.taints[*]}{.effect}{";"}{end}{"\n"}{end}' |
    awk -F'|' '
      $1 == "" || $2 == "true" { next }
      {
        n = split($3, effects, ";")
        for (i = 1; i <= n; i++) {
          if (effects[i] == "NoSchedule" || effects[i] == "NoExecute") next
        }
        print $1
      }'
}

# The Job only proves N pods ran, not WHICH nodes: a node that joined after the
# scan satisfies anti-affinity without being warm. Only `Succeeded` filled a cache.
verify_coverage() {
  local want got missing
  want="$(target_nodes | sort)"
  [ -n "$want" ] || die "no schedulable, untainted nodes found"
  got="$(kubectl -n "$NS" get pods -l "$SELECTOR" \
    -o jsonpath='{range .items[?(@.status.phase=="Succeeded")]}{.spec.nodeName}{"\n"}{end}' | grep . | sort -u || true)"
  missing="$(comm -23 <(printf '%s\n' "$want") <(printf '%s\n' "$got") | grep . || true)"
  [ -z "$missing" ] || die "schedulable but NOT warm: $(printf '%s' "$missing" | tr '\n' ' ')— re-run before starting a fleet"
  echo "prepull: every schedulable node is warm"
}

render() {
  local completions="$1" image policy out
  case "$completions" in
  '' | *[!0-9]* | 0) die "completions must be a positive integer, got: $completions" ;;
  esac
  image="$(manifest_value image)"
  policy="$(manifest_value imagePullPolicy)"
  out="$(sed -e "s|__BOT_IMAGE__|${image}|g" -e "s|__BOT_IMAGE_PULL_POLICY__|${policy}|g" -e "s|__PREPULL_COMPLETIONS__|${completions}|g" -e "s|__PREPULL_DEADLINE__|${DEADLINE_SECONDS}|g" "$TEMPLATE")"
  printf '%s\n' "$out" | grep -v '^[[:space:]]*#' | grep -q '__' && die "unsubstituted placeholder left in the rendered Job"
  printf '%s\n' "$out"
}

epoch() {
  [ -n "$1" ] || return 0
  date -u -d "$1" +%s 2>/dev/null || true
}

pull_trouble() {
  kubectl -n "$NS" get pods -l "$SELECTOR" \
    -o jsonpath='{range .items[*]}{.spec.nodeName}{" "}{.status.containerStatuses[0].state.waiting.reason}{"\n"}{end}' |
    grep -E ' (Err|Invalid|ImagePullBackOff)' || true
}

report() {
  local rows node phase start ready digest digests="" secs a b pulls clocks
  rows="$(kubectl -n "$NS" get pods -l "$SELECTOR" -o jsonpath='{range .items[*]}{.spec.nodeName}{"|"}{.status.phase}{"|"}{.status.startTime}{"|"}{.status.containerStatuses[0].state.terminated.startedAt}{"|"}{.status.containerStatuses[0].imageID}{"\n"}{end}' || true)"
  while IFS='|' read -r node phase start ready digest; do
    [ -n "$node" ] || continue
    secs="?"
    a="$(epoch "$start")"
    b="$(epoch "$ready")"
    if [ -n "$a" ] && [ -n "$b" ]; then secs="$((b - a))"; fi
    printf 'prepull: %-28s %-10s pull+start %ss  %s\n' "$node" "$phase" "$secs" "$digest"
    if [ -n "$digest" ]; then digests="$digests$digest"$'\n'; fi
  done <<<"$rows"

  if [ "$(printf '%s' "$digests" | grep -c . || true)" -gt 0 ]; then
    if [ "$(printf '%s\n' "$digests" | grep . | sort -u | wc -l)" != "1" ]; then
      die "nodes resolved the tag to DIFFERENT digests — the tag moved mid-warm-up; the fleet would run a mixed image"
    fi
    echo "prepull: all warmed nodes agree on digest $(printf '%s\n' "$digests" | grep . | sort -u)"
  fi

  pulls="$(kubectl -n "$NS" get events --field-selector reason=Pulled \
    -o jsonpath='{range .items[*]}{"  "}{.involvedObject.name}{" "}{.message}{"\n"}{end}' | grep prepull || true)"
  echo "prepull: kubelet pull durations:"
  printf '%s\n' "${pulls:-  (none reported — already cached, or events unavailable)}"

  clocks="$(kubectl -n "$NS" logs -l "$SELECTOR" --tail=-1 --max-log-requests=64 | grep 'clock node=' | sort || true)"
  echo "prepull: node clock samples (UTC epoch, taken at container start):"
  printf '%s\n' "${clocks:-  (none reported — pods already deleted, or logs unavailable)}"
}

case "${1:-run}" in
--print-image)
  manifest_value image
  echo
  ;;
--print-policy)
  manifest_value imagePullPolicy
  echo
  ;;
--print-nodes) target_nodes ;;
--check-agreement)
  check_agreement
  agreed="$(manifest_value image)"
  echo "prepull: all manifests name $agreed"
  ;;
--check-source-drift) check_source_drift ;;
--print-pinned-commit)
  pinned_commit
  echo
  ;;
--render) render "${2:?--render needs a completion count}" ;;
--delete) kubectl -n "$NS" delete job "$JOB" --ignore-not-found --cascade=foreground --wait ;;
run)
  check_agreement
  check_source_drift
  image="$(manifest_value image)"
  policy="$(manifest_value imagePullPolicy)"
  nodes="$(target_nodes)"
  count="$(printf '%s\n' "$nodes" | grep -c . || true)"
  [ "$count" -gt 0 ] || die "no schedulable, untainted nodes found"
  echo "prepull: warming $image (policy $policy) on $count node(s)"
  # Foreground: report() selects pods by name label across generations, so a
  # survivor of the previous run contributes a stale imageID.
  kubectl -n "$NS" delete job "$JOB" --ignore-not-found --cascade=foreground --wait >/dev/null
  render "$count" | kubectl -n "$NS" apply -f -
  waited=0
  stuck=0
  last_trouble=""
  while :; do
    conds="$(kubectl -n "$NS" get job "$JOB" -o jsonpath='{range .status.conditions[*]}{.type}={.status}{"\n"}{end}')" ||
      die "kubectl get job failed — cannot tell whether the warm-up finished"
    case "$conds" in
    *"Complete=True"*)
      echo "prepull: complete"
      break
      ;;
    *"Failed=True"*)
      report
      die "Job failed — some nodes are NOT warm; do not start a run against a partial warm-up"
      ;;
    esac
    trouble="$(pull_trouble)"
    if [ -n "$trouble" ]; then
      [ "$trouble" = "$last_trouble" ] || printf 'prepull: %s\n' "$trouble"
      last_trouble="$trouble"
      case "$trouble" in
      *InvalidImageName* | *ErrImageNeverPull*) die "the image reference is unusable: $trouble" ;;
      esac
      stuck=$((stuck + 1))
      [ "$stuck" -lt "$BACKOFF_GRACE_POLLS" ] ||
        die "still failing to pull after $((stuck * POLL_SECONDS))s: $trouble"
    else
      stuck=0
      last_trouble=""
    fi
    [ "$waited" -lt "$WAIT_SECONDS" ] || {
      report
      die "timed out after ${WAIT_SECONDS}s"
    }
    sleep "$POLL_SECONDS"
    waited=$((waited + POLL_SECONDS))
  done
  report
  verify_coverage
  echo "prepull: delete the Job when done — $0 --delete"
  ;;
--verify-coverage)
  # Warn, never refuse: this reports on the cluster, not the tree.
  check_source_drift warn
  verify_coverage
  ;;
*) die "usage: $0 [run|--print-image|--print-policy|--print-nodes|--print-pinned-commit|--check-agreement|--check-source-drift|--verify-coverage|--render <n>|--delete]
       --verify-coverage reads the Job's pods, so run it BEFORE --delete" ;;
esac
