#!/usr/bin/env bash
# Check changelog fragments between two commits.
# Usage: changelog-fragment.sh BASE HEAD
#
# CHANGELOG_LABELS is a comma-separated list of pull request labels.
# The label no-changelog skips the fragment requirement.
# The label release allows an edit of the Unreleased section.
# CHANGELOG_HEAD_REF is the head branch name. A name that starts with
# release/ allows an edit of the Unreleased section.
# When CHANGELOG_HEAD_REF is empty, the current branch name is used.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "changelog: usage: changelog-fragment.sh BASE HEAD" >&2
  exit 2
fi

base=$1
head=$2

root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "changelog: run this inside the git repository" >&2
  exit 1
}
cd "$root"

git rev-parse --verify --quiet "${base}^{commit}" >/dev/null || {
  echo "changelog: unknown base ${base}" >&2
  exit 1
}
git rev-parse --verify --quiet "${head}^{commit}" >/dev/null || {
  echo "changelog: unknown head ${head}" >&2
  exit 1
}

# Exact label match. A name such as prerelease does not match release.
has_label() {
  local want=$1
  local rest=${CHANGELOG_LABELS-}
  local item
  while [ -n "$rest" ]; do
    item=${rest%%,*}
    case "$rest" in
      *,*) rest=${rest#*,} ;;
      *) rest="" ;;
    esac
    item=${item#"${item%%[![:space:]]*}"}
    item=${item%"${item##*[![:space:]]}"}
    [ "$item" = "$want" ] && return 0
  done
  return 1
}

head_ref=${CHANGELOG_HEAD_REF-}
if [ -z "$head_ref" ]; then
  head_ref=$(git rev-parse --abbrev-ref HEAD)
fi

if has_label no-changelog; then
  echo "changelog: label exemption: yes (no-changelog)"
  skip_fragment=1
else
  echo "changelog: label exemption: no"
  skip_fragment=0
fi

release_reason=""
if has_label release; then
  release_reason="label release"
fi
case "$head_ref" in
  release/*)
    if [ -n "$release_reason" ]; then
      release_reason="head ref ${head_ref} and label release"
    else
      release_reason="head ref ${head_ref}"
    fi
    ;;
esac
if [ -n "$release_reason" ]; then
  echo "changelog: release exemption: yes (${release_reason})"
else
  echo "changelog: release exemption: no"
fi

if ! diffout=$(git diff --name-status --find-renames "${base}...${head}"); then
  echo "changelog: cannot diff ${base}...${head}" >&2
  exit 1
fi

need=0
have=0
frags=()

consider_need() {
  case "$1" in
    src/*|plugin/*|profiles/*|skills/*) need=1 ;;
  esac
}

while IFS=$'\t' read -r state path extra; do
  [ -z "${state:-}" ] && continue
  kind=${state:0:1}
  consider_need "$path"
  if [ -n "${extra:-}" ]; then
    consider_need "$extra"
  fi
  case "$kind" in
    A|M|T|R|C) ;;
    *) continue ;;
  esac
  target=$path
  if [ "$kind" = "R" ] || [ "$kind" = "C" ]; then
    target=$extra
  fi
  case "$target" in
    changes/README.md) ;;
    changes/*)
      have=1
      case "$target" in
        *.md) frags+=("$target") ;;
      esac
      ;;
  esac
done <<< "$diffout"

status=0
if [ "$need" = 1 ] && [ "$have" = 0 ]; then
  if [ "$skip_fragment" = 1 ]; then
    echo "changelog: fragment not required because label no-changelog is set"
  else
    echo "changelog: no fragment for a change under src/, plugin/, profiles/ or skills/"
    status=1
  fi
fi

for p in "${frags[@]+"${frags[@]}"}"; do
  if ! content=$(git show "${head}:${p}" 2>/dev/null); then
    echo "changelog: ${p} is empty"
    status=1
    continue
  fi
  first=$(printf '%s\n' "$content" | awk 'NF { print; exit }')
  if [ -z "$first" ]; then
    echo "changelog: ${p} is empty"
    status=1
  elif [ "${first#- }" = "$first" ]; then
    echo "changelog: ${p} does not start with '- '"
    status=1
  fi
done

if git diff --name-only "${base}...${head}" -- CHANGELOG.md | grep -qx 'CHANGELOG.md'; then
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  extract() {
    local rev=$1 dest=$2
    if git cat-file -e "${rev}:CHANGELOG.md" 2>/dev/null; then
      git show "${rev}:CHANGELOG.md" | awk '
        $0 == "## Unreleased" { on = 1; next }
        on && /^## / { exit }
        on { print }
      ' > "$dest"
    else
      : > "$dest"
    fi
  }
  extract "$base" "$work/old"
  extract "$head" "$work/new"
  if ! cmp -s "$work/old" "$work/new"; then
    if [ -n "$release_reason" ]; then
      echo "changelog: Unreleased edit allowed (${release_reason})"
    else
      echo "changelog: the diff edits ## Unreleased in CHANGELOG.md"
      echo "changelog: a release pull request has head ref release/* or the label release"
      status=1
    fi
  fi
fi

if [ "$status" = 0 ]; then
  echo "changelog: ok"
fi
exit "$status"
