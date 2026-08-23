#!/bin/sh
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
lock="$workspace/testdata/oracles.lock.toml"
destination=${GRAPHY_ORACLES_DIR:-"$workspace/testdata/oracles"}

mkdir -p "$destination"

fetch_oracle() {
    name=$1
    repository=$2
    revision=$3
    shift 3
    target="$destination/$name"

    if [ ! -e "$target/.git" ]; then
        if [ -e "$target" ]; then
            echo "refusing to replace non-git path: $target" >&2
            exit 1
        fi
        git clone --filter=blob:none --no-checkout "$repository" "$target"
    fi

    actual_origin=$(git -C "$target" remote get-url origin)
    if [ "$actual_origin" != "$repository" ]; then
        echo "unexpected origin for $target: $actual_origin" >&2
        exit 1
    fi

    git -C "$target" sparse-checkout init --cone
    git -C "$target" sparse-checkout set "$@"
    git -C "$target" fetch --depth 1 origin "$revision"
    git -C "$target" checkout --detach FETCH_HEAD

    actual_revision=$(git -C "$target" rev-parse HEAD)
    if [ "$actual_revision" != "$revision" ]; then
        echo "revision mismatch for $name: $actual_revision" >&2
        exit 1
    fi
    echo "$name $actual_revision"
}

# Keep the executable behavior explicit while the TOML file is the reviewable
# source of truth. These assertions make duplicated values fail closed.
assert_lock_value() {
    value=$1
    if ! grep -Fq "$value" "$lock"; then
        echo "fetch script and $lock disagree: missing $value" >&2
        exit 1
    fi
}

assert_lock_value 'name = "oxigraph"'
assert_lock_value 'revision = "d2d33273ab13b8d3d11f115d24c296eb873a62ad"'
fetch_oracle oxigraph \
    https://github.com/oxigraph/oxigraph.git \
    d2d33273ab13b8d3d11f115d24c296eb873a62ad \
    LICENSE-APACHE LICENSE-MIT testsuite/oxigraph-tests

assert_lock_value 'name = "rdf4j"'
assert_lock_value 'revision = "270eeb5245ac0cdaf98e93411bc8dc0554737234"'
fetch_oracle rdf4j \
    https://github.com/eclipse-rdf4j/rdf4j.git \
    270eeb5245ac0cdaf98e93411bc8dc0554737234 \
    LICENSE \
    testsuites/sparql/src/main/resources/testcases-sparql-1.1 \
    testsuites/sparql/src/main/resources/testcases-sparql-1.2
