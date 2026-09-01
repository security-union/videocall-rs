#!/usr/bin/env bash
set -euo pipefail

mode="hash"
if [ "${1:-}" = "--check" ]; then
    mode="check"
    shift
fi

dist="${1:-${TRUNK_STAGING_DIR:-dist}}"

# Each name drives an asset triple: <bin>.js, <bin>_bg.wasm, <bin>_loader.js.
workers=(worker_decoder neteq_worker)

hex16='[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]'

loader_tmp=""
index_tmp=""

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

hash_file() {
    sha256sum "$1" | awk '{ print substr($1, 1, 16) }'
}

# nginx serves dist/ as an unprivileged user, so o+r is load-bearing, not cosmetic.
assert_world_readable() {
    local perms
    perms="$(stat -c '%a' "$1" 2>/dev/null || stat -f '%OLp' "$1" 2>/dev/null)" ||
        die "cannot stat $1"
    case "$perms" in
    *[4567]) ;;
    *) die "$1 is mode $perms, not world-readable" ;;
    esac
}

one_match() {
    local pattern="$1"
    local matches=()
    shopt -s nullglob
    # shellcheck disable=SC2206 # $pattern is a glob, quoting it would defeat the match
    matches=("$dist"/$pattern)
    shopt -u nullglob
    [ "${#matches[@]}" -eq 1 ] || die "expected one match for $pattern in $dist, found ${#matches[@]}"
    basename "${matches[0]}"
}

assert_hashed_worker() {
    local bin="$1"

    [ ! -e "$dist/$bin.js" ] || die "$bin.js is unhashed"
    [ ! -e "$dist/${bin}_bg.wasm" ] || die "${bin}_bg.wasm is unhashed"
    [ ! -e "$dist/${bin}_loader.js" ] || die "${bin}_loader.js is unhashed"

    local js_name wasm_name loader_name loader_path
    js_name="$(one_match "$bin-$hex16.js")"
    wasm_name="$(one_match "${bin}_bg-$hex16.wasm")"
    loader_name="$(one_match "${bin}_loader-$hex16.js")"
    loader_path="$dist/$loader_name"

    assert_world_readable "$dist/index.html"
    assert_world_readable "$dist/$js_name"
    assert_world_readable "$dist/$wasm_name"
    assert_world_readable "$loader_path"

    ! grep -Fq "./$bin.js" "$loader_path" || die "$loader_name references unhashed $bin.js"
    ! grep -Fq "./${bin}_bg.wasm" "$loader_path" || die "$loader_name references unhashed ${bin}_bg.wasm"
    grep -Fq "./$js_name" "$loader_path" || die "$loader_name does not reference $js_name"
    grep -Fq "./$wasm_name" "$loader_path" || die "$loader_name does not reference $wasm_name"

    ! grep -Fq "href=\"/${bin}_loader.js\"" "$dist/index.html" || die "index.html references unhashed ${bin}_loader.js"
    grep -Fq "href=\"/$loader_name\"" "$dist/index.html" || die "index.html does not reference $loader_name"
}

hash_worker() {
    local bin="$1"

    [ -f "$dist/$bin.js" ] || die "missing $dist/$bin.js"
    [ -f "$dist/${bin}_bg.wasm" ] || die "missing $dist/${bin}_bg.wasm"
    [ -f "$dist/${bin}_loader.js" ] || die "missing $dist/${bin}_loader.js"

    local js_name wasm_name loader_name
    js_name="$bin-$(hash_file "$dist/$bin.js").js"
    wasm_name="${bin}_bg-$(hash_file "$dist/${bin}_bg.wasm").wasm"
    loader_tmp="$(mktemp "$dist/.${bin}_loader.XXXXXX")"
    index_tmp="$(mktemp "$dist/.index.XXXXXX")"

    cp "$dist/$bin.js" "$dist/$js_name"
    cp "$dist/${bin}_bg.wasm" "$dist/$wasm_name"

    sed \
        -e "s#\\./$bin\\.js#./$js_name#g" \
        -e "s#\\./${bin}_bg\\.wasm#./$wasm_name#g" \
        "$dist/${bin}_loader.js" > "$loader_tmp"

    grep -Fq "./$js_name" "$loader_tmp" || die "loader rewrite missed $js_name"
    grep -Fq "./$wasm_name" "$loader_tmp" || die "loader rewrite missed $wasm_name"

    loader_name="${bin}_loader-$(hash_file "$loader_tmp").js"
    chmod 644 "$loader_tmp"
    mv "$loader_tmp" "$dist/$loader_name"
    loader_tmp=""

    sed \
        -e "s#href=\"/${bin}_loader\\.js\"#href=\"/$loader_name\"#g" \
        "$dist/index.html" > "$index_tmp"

    grep -Fq "href=\"/$loader_name\"" "$index_tmp" || die "index.html rewrite missed $loader_name"
    chmod 644 "$index_tmp"
    mv "$index_tmp" "$dist/index.html"
    index_tmp=""

    rm "$dist/$bin.js" "$dist/${bin}_bg.wasm" "$dist/${bin}_loader.js"
}

cleanup() {
    if [ -n "$loader_tmp" ] && [ -e "$loader_tmp" ]; then
        rm "$loader_tmp"
    fi
    if [ -n "$index_tmp" ] && [ -e "$index_tmp" ]; then
        rm "$index_tmp"
    fi
}
trap cleanup EXIT

[ -d "$dist" ] || die "missing dist directory: $dist"
[ -f "$dist/index.html" ] || die "missing $dist/index.html"

if [ "$mode" = "check" ]; then
    for bin in "${workers[@]}"; do
        assert_hashed_worker "$bin"
    done
    exit 0
fi

for bin in "${workers[@]}"; do
    hash_worker "$bin"
done

for bin in "${workers[@]}"; do
    assert_hashed_worker "$bin"
done
