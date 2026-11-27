#!/usr/bin/env bash
# Pin the production Helm values to the newest image built from main.
#
# For each image repo, walk `git rev-list --first-parent <ref>` from the tip
# and pick the first commit whose `main-<sha8>` tag exists on Docker Hub. Then
# rewrite only the image `tag:` line in the matching values.yaml files, as
# `main-<sha8>@sha256:<digest>` so the pin names exact image bytes.
#
# Order comes from git history, never from Docker Hub push times, so a re-run
# of an old commit's publish job cannot win over a newer commit.
#
# Read-only against Docker Hub and the registry (public, anonymous). No
# cluster access.
#
# Usage:
#   scripts/pin-main-images.sh [--check] [--ref REF] [--max-commits N]
#
#   --check          Dry run. Print the summary, write nothing. Exit 1 if any
#                    pin would change, 0 if all pins are current.
#   --ref REF        Commit to walk from (default: HEAD). In CI this is a
#                    checkout of main with full history.
#   --max-commits N  How many first-parent commits to search for a published
#                    tag (default: 200).
#
# Output: a tab-separated summary on stdout, one row per values file:
#   image  values_file  current_tag  new_tag  status(unchanged|bump)
# Logs go to stderr. Exit 2, with nothing written, on any Docker Hub or
# registry error, a missing tag, an ambiguous values file, a pin that would
# move backwards, or a pinned tag whose digest changed (re-pushed tag).
#
# Requires: bash, git, curl, jq, awk.

set -euo pipefail

CHECK=false
REF=HEAD
MAX_COMMITS=200
HUB_NAMESPACE=securityunion
HUB_API=https://hub.docker.com/v2
REGISTRY=https://registry-1.docker.io
# Docker Hub returns at most 100 tags per page. If main-* tags ever exceed
# this many pages we fail instead of trusting a truncated listing.
MAX_PAGES=30

# image repo (without namespace) -> values files it is pinned in.
# The media server image backs both relay charts.
PINS=(
  "videocall-media-server helm/global/us-east/websocket/values.yaml"
  "videocall-media-server helm/global/us-east/webtransport/values.yaml"
  "videocall-meeting-api helm/global/us-east/meeting-api/values.yaml"
  "videocall-dioxus-ui helm/global/us-east/videocall-ui/values.yaml"
  "video-call-rs-website helm/global/us-east/videocall-website/values.yaml"
)

log() { printf '%s\n' "$*" >&2; }
die() {
  log "error: $*"
  exit 2
}

usage() {
  sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --check | --dry-run) CHECK=true ;;
    --ref)
      [[ $# -ge 2 ]] || usage
      REF="$2"
      shift
      ;;
    --max-commits)
      [[ $# -ge 2 && "$2" =~ ^[0-9]+$ && "$2" -gt 0 ]] || usage
      MAX_COMMITS="$2"
      shift
      ;;
    -h | --help) usage ;;
    *)
      log "unknown argument: $1"
      usage
      ;;
  esac
  shift
done

for tool in git curl jq awk; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
cd "$SCRIPT_DIR/.."
ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

TIP="$(git rev-parse --verify --quiet "${REF}^{commit}")" || die "cannot resolve ref '$REF'"
# Whole first-parent history, newest first. sha8 -> commit lookups go through
# this list (never bare `git rev-parse <sha8>`), so a prefix can only match a
# commit that is actually on main's first-parent line.
git rev-list --first-parent "$TIP" >"$TMP/history"
head -n "$MAX_COMMITS" "$TMP/history" | cut -c1-8 >"$TMP/sha8s"
COMMIT_COUNT="$(wc -l <"$TMP/sha8s" | tr -d ' ')"
log "walking up to $MAX_COMMITS first-parent commits from $REF (${TIP:0:8}), $COMMIT_COUNT available"
if [[ "$(git rev-parse --is-shallow-repository)" == "true" ]]; then
  log "warning: shallow clone; pins older than the fetched history cannot be checked"
fi

# 1-based position of a sha8 in the first-parent history, or empty. Fails
# loudly if the prefix is ambiguous within that history.
history_pos() {
  local matches
  matches="$(grep -n "^$1" "$TMP/history" || true)"
  [[ "$(printf '%s' "$matches" | grep -c . || true)" -le 1 ]] || die "sha8 $1 is ambiguous in first-parent history"
  printf '%s' "${matches%%:*}"
}

# Docker Hub error bodies end up in CI logs: one line, no workflow commands.
sanitize() { head -c 300 | tr '\r\n' '  ' | sed 's/::/: :/g'; }

# GET a Docker Hub API URL into $2. Fails loudly on anything but HTTP 200
# (404 included: callers only use this where the resource must exist).
hub_get() {
  local url="$1" out="$2" code
  code="$(curl -sS --proto '=https' --max-time 30 \
    --retry 4 --retry-delay 5 --retry-connrefused \
    -H 'Accept: application/json' -A 'pin-main-images' \
    -D "$TMP/headers" -o "$out" -w '%{http_code}' "$url")" ||
    die "request to $url failed (network error)"
  if [[ "$code" != "200" ]]; then
    log "Docker Hub answered HTTP $code for $url"
    grep -i '^x-ratelimit' "$TMP/headers" | sanitize >&2 || true
    log "body: $(sanitize <"$out" || true)"
    die "refusing to guess a pin without a clean Docker Hub answer"
  fi
}

# Digest the kubelet resolves for repo:tag, straight from the registry
# (anonymous token, HEAD on the manifest; HEADs do not count as pulls).
registry_digest() {
  local repo="$1" tag="$2" token code digest
  hub_get "https://auth.docker.io/token?service=registry.docker.io&scope=repository:$HUB_NAMESPACE/$repo:pull" "$TMP/token.json"
  token="$(jq -r '.token // empty' "$TMP/token.json")"
  [[ -n "$token" ]] || die "no registry token for $repo"
  code="$(curl -sS --proto '=https' --max-time 30 --retry 4 --retry-delay 5 -I \
    -H "Authorization: Bearer $token" \
    -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json' \
    -D "$TMP/reg-headers" -o /dev/null -w '%{http_code}' \
    "$REGISTRY/v2/$HUB_NAMESPACE/$repo/manifests/$tag")" ||
    die "registry request for $repo:$tag failed (network error)"
  [[ "$code" == "200" ]] || die "registry answered HTTP $code for $repo:$tag"
  digest="$(tr -d '\r' <"$TMP/reg-headers" | awk -F': ' 'tolower($1) == "docker-content-digest" { print $2 }')"
  [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || die "registry gave no usable digest for $repo:$tag"
  printf '%s' "$digest"
}

# Write every main-<sha8> tag of an image repo to $2, one per line.
list_main_tags() {
  local repo="$1" out="$2" page=0 url
  url="$HUB_API/namespaces/$HUB_NAMESPACE/repositories/$repo/tags?page_size=100&name=main-"
  : >"$out"
  while [[ -n "$url" ]]; do
    page=$((page + 1))
    [[ "$page" -le "$MAX_PAGES" ]] || die "$repo has more than $MAX_PAGES pages of main-* tags; raise MAX_PAGES"
    hub_get "$url" "$TMP/page.json"
    jq -e '.results | type == "array"' "$TMP/page.json" >/dev/null ||
      die "unexpected Docker Hub response for $repo"
    # `name=` is a substring filter on Docker Hub, so keep exact matches only.
    jq -r '.results[].name | select(test("^main-[0-9a-f]{8}$"))' "$TMP/page.json" >>"$out"
    url="$(jq -r '.next // empty' "$TMP/page.json")"
    if [[ -n "$url" && "$url" != "$HUB_API/"* ]]; then
      die "unexpected pagination URL from Docker Hub: $url"
    fi
  done
}

# Newest first-parent commit with a published main-<sha8> tag for an image.
# Prints "<tag> <digest>".
resolve_tag() {
  local repo="$1" sha8 hub_digest reg_digest
  list_main_tags "$repo" "$TMP/tags-$repo"
  while read -r sha8; do
    if grep -qx "main-$sha8" "$TMP/tags-$repo"; then
      # Confirm the exact tag, and that Hub and the registry agree on the
      # digest the kubelet will pull.
      hub_get "$HUB_API/namespaces/$HUB_NAMESPACE/repositories/$repo/tags/main-$sha8" "$TMP/tag.json"
      hub_digest="$(jq -r '.digest // empty' "$TMP/tag.json")"
      reg_digest="$(registry_digest "$repo" "main-$sha8")" || exit 2
      [[ "$hub_digest" == "$reg_digest" ]] ||
        die "$repo:main-$sha8 digest differs between Docker Hub ($hub_digest) and the registry ($reg_digest)"
      echo "main-$sha8 $reg_digest"
      return 0
    fi
  done <"$TMP/sha8s"
  die "no main-<sha8> tag for $HUB_NAMESPACE/$repo in the last $COMMIT_COUNT first-parent commits of $REF"
}

# Print the tag of the image block whose repository is $2 in values file $1.
# Fails unless exactly one such block with exactly one tag line exists.
read_tag() {
  local file="$1" image="$2"
  awk -v image="$image" '
    function indent(s) { match(s, /^[ ]*/); return RLENGTH }
    in_block && NF && indent($0) < block_indent { in_block = 0 }
    in_block && $0 ~ /^[ ]*tag:/ && indent($0) == block_indent {
      v = $0; sub(/^[ ]*tag:[ ]*/, "", v); sub(/[ ]+#.*$/, "", v); gsub(/["\047]/, "", v)
      tags++; tag = v
    }
    $0 ~ "^[ ]*repository:[ ]*[\"\047]?" image "[\"\047]?[ ]*$" {
      repos++; in_block = 1; block_indent = indent($0)
    }
    END {
      if (repos != 1 || tags != 1) { exit 1 }
      print tag
    }
  ' "$file"
}

# Rewrite that same tag line to $3, leaving every other byte alone.
write_tag() {
  local file="$1" image="$2" new="$3" final_newline=0
  # Keep a missing trailing newline missing (some values files lack one).
  [[ -n "$(tail -c1 "$file")" ]] || final_newline=1
  awk -v image="$image" -v new="$new" -v final_newline="$final_newline" '
    function indent(s) { match(s, /^[ ]*/); return RLENGTH }
    function emit(s) { printf "%s%s", (NR > 1 ? "\n" : ""), s }
    in_block && NF && indent($0) < block_indent { in_block = 0 }
    in_block && $0 ~ /^[ ]*tag:/ && indent($0) == block_indent {
      match($0, /^[ ]*/); emit(substr($0, 1, RLENGTH) "tag: " new); next
    }
    $0 ~ "^[ ]*repository:[ ]*[\"\047]?" image "[\"\047]?[ ]*$" {
      in_block = 1; block_indent = indent($0)
    }
    { emit($0) }
    END { if (NR > 0 && final_newline) printf "\n" }
  ' "$file" >"$TMP/rewrite"
  cat "$TMP/rewrite" >"$file"
}

# Resolve each distinct image once.
: >"$TMP/resolved"
for entry in "${PINS[@]}"; do
  repo="${entry%% *}"
  if ! grep -q "^$repo " "$TMP/resolved"; then
    out="$(resolve_tag "$repo")" || exit 2
    read -r tag digest <<<"$out"
    log "$HUB_NAMESPACE/$repo: newest main build is $tag@${digest:0:19}"
    echo "$repo $tag $digest" >>"$TMP/resolved"
  fi
done

# Plan every change before writing any, so a failure leaves the tree untouched.
: >"$TMP/plan"
for entry in "${PINS[@]}"; do
  repo="${entry%% *}"
  file="${entry#* }"
  image="$HUB_NAMESPACE/$repo"
  [[ -f "$file" ]] || die "$file not found"
  current="$(read_tag "$file" "$image")" ||
    die "$file: expected exactly one image block for $image with one tag line"
  read -r _ new_tag new_digest < <(grep "^$repo " "$TMP/resolved")
  new="$new_tag@$new_digest"

  if [[ "$current" =~ ^(main-([0-9a-f]{8}))(@(sha256:[0-9a-f]{64}))?$ ]]; then
    cur_tag="${BASH_REMATCH[1]}"
    cur_sha8="${BASH_REMATCH[2]}"
    cur_digest="${BASH_REMATCH[4]}"
    if [[ "$cur_tag" == "$new_tag" ]]; then
      # Same tag. A different digest means someone pushed over it.
      if [[ -n "$cur_digest" && "$cur_digest" != "$new_digest" ]]; then
        die "$file pins $cur_tag@$cur_digest but the registry now serves $new_digest for that tag: the tag was re-pushed. Investigate before pinning."
      fi
    else
      # Refuse to move a pin backwards, or to replace a pin that is not a
      # build of this first-parent history (newer than REF, or off-branch).
      cur_pos="$(history_pos "$cur_sha8")" || exit 2
      new_pos="$(history_pos "${new_tag#main-}")" || exit 2
      [[ -n "$cur_pos" ]] ||
        die "$file pins $current, which is not on the first-parent history of $REF; refusing to replace it automatically"
      if [[ "$cur_pos" -lt "$new_pos" ]]; then
        die "$file pins $current, newer than the newest available $new_tag; refusing to downgrade"
      fi
    fi
  fi

  status=unchanged
  [[ "$current" == "$new" ]] || status=bump
  printf '%s\t%s\t%s\t%s\t%s\n' "$image" "$file" "$current" "$new" "$status" >>"$TMP/plan"
done

printf 'image\tvalues_file\tcurrent_tag\tnew_tag\tstatus\n'
cat "$TMP/plan"

bumps="$(awk -F'\t' '$5 == "bump"' "$TMP/plan" | wc -l | tr -d ' ')"
if [[ "$bumps" -eq 0 ]]; then
  log "all pins current"
  exit 0
fi

if [[ "$CHECK" == "true" ]]; then
  log "$bumps pin(s) out of date (--check: nothing written)"
  exit 1
fi

while IFS=$'\t' read -r image file current new status; do
  [[ "$status" == "bump" ]] || continue
  write_tag "$file" "$image" "$new"
  [[ "$(read_tag "$file" "$image")" == "$new" ]] || die "$file: rewrite did not take"
  log "$file: $current -> $new"
done <"$TMP/plan"
