#!/usr/bin/env bash
# Move every manifest that runs the bots-app image onto one reference (#2345).
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREPULL="$DIR/prepull-image.sh"

die() {
  echo "repin: $*" >&2
  exit 1
}

# A registry port also contains `:`, so a tag is the last field only sans `/`.
repo_of() {
  local r="${1%@*}" t
  t="${r##*:}"
  case "$t" in
  */*) printf '%s' "$r" ;;
  *) printf '%s' "${r%:*}" ;;
  esac
}

ref="${1:-}"
[ -n "$ref" ] || die "usage: $0 <registry/repo:version-date-commit@sha256:digest>"
# The value reaches a `sed s|..|VALUE|` replacement, where `&` would splice.
case "$ref" in
*[!A-Za-z0-9._/:@-]*) die "illegal character in the reference: $ref" ;;
esac

digest="${ref##*@}"
[ "$digest" != "$ref" ] || die "the reference carries no @sha256: digest, so a re-pushed tag could move under the fleet: $ref"
[[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || die "not a well-formed sha256 digest: $digest"

tagged="${ref%@*}"
tag="${tagged##*:}"
case "$tag" in
*/*) die "the reference carries no tag: $ref" ;;
esac
# prepull-image.sh reads the built commit back out of this tag.
commit="${tag##*-}"
[[ "$commit" =~ ^[0-9a-f]{7,}$ ]] || die "cannot read a commit from the tag '$tag' — expected <version>-<date>-<sha>, as build.sh produces"

was="$("$PREPULL" --print-image)"
want_repo="$(repo_of "$ref")"
[ "$(repo_of "$was")" = "$want_repo" ] || die "the manifests pull $(repo_of "$was") but the reference names $want_repo — the fleet would never see it"
app="${want_repo##*/}"

# Sibling of $DIR, so it is on the same filesystem and the moves are renames.
stage="$(mktemp -d "$DIR.repin.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
cp -a "$DIR/." "$stage/"

names=()
while IFS= read -r -d '' f; do
  names+=("${f##*/}")
  sed -E "s|^([[:space:]]*-?[[:space:]]*image:[[:space:]]*)['\"]?[^[:space:]'\"]*${app}[^[:space:]'\"]*['\"]?[[:space:]]*$|\1${ref}|" "$f" >"$stage/${f##*/}"
done < <(find "$DIR" -maxdepth 1 -type f \( -name '*.yaml' -o -name '*.yml' \) -print0 | sort -z)

"$stage/prepull-image.sh" --check-agreement >/dev/null

changed=()
for n in "${names[@]}"; do
  if ! cmp -s "$DIR/$n" "$stage/$n"; then
    mv "$stage/$n" "$DIR/$n"
    changed+=("$n")
  fi
done

if [ "${#changed[@]}" -eq 0 ]; then
  echo "repin: already at $ref"
else
  printf 'repin: %s\n' "${changed[@]}"
  echo "repin: every manifest now names $ref"
fi
