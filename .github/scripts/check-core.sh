#!/usr/bin/env bash
# Check that core/ is exactly the paddock release named in CORE, and that
# the plugin has the same version. A local edit of core/ fails here.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
fail() { echo "core check: $*" >&2; exit 1; }

tag=$(tr -d '[:space:]' < CORE)
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "CORE must name a tag such as v0.2.1, not '$tag'"
git fetch -q --no-tags https://github.com/harnessix/paddock "refs/tags/$tag" \
  || fail "harnessix/paddock has no tag $tag"
want=$(git rev-parse 'FETCH_HEAD^{tree}')
have=$(git write-tree --prefix=core/)
[ "$have" = "$want" ] || fail "core/ differs from harnessix/paddock $tag. Run bin/update-core $tag, and change paddock in harnessix/paddock"
version=$(jq -r .version manifest.json)
[ "v$version" = "$tag" ] || fail "manifest.json has $version, CORE has $tag"
echo "core check: ok ($tag)"
