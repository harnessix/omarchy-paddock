#!/usr/bin/env bash
# Move fragments from changes/ into the Unreleased section of CHANGELOG.md.
# Run it from the repository root on a release pull request.
# It does not create a tag or a GitHub release.
set -euo pipefail

root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "changelog: run this inside the git repository" >&2
  exit 1
}
cd "$root"

if [ ! -f CHANGELOG.md ]; then
  echo "changelog: CHANGELOG.md is missing" >&2
  exit 1
fi

shopt -s nullglob
files=()
for f in changes/*.md; do
  [ "$(basename "$f")" = "README.md" ] && continue
  files+=("$f")
done

if [ "${#files[@]}" -eq 0 ]; then
  echo "changelog: no fragments to assemble"
  exit 0
fi

for f in "${files[@]}"; do
  first=$(awk 'NF { print; exit }' "$f")
  if [ -z "$first" ]; then
    echo "changelog: ${f} is empty" >&2
    exit 1
  fi
  case "$first" in
    "- "*) ;;
    *)
      echo "changelog: ${f} does not start with '- '" >&2
      exit 1
      ;;
  esac
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

: > "$work/rows"
for f in "${files[@]}"; do
  n=0
  while IFS= read -r line || [ -n "$line" ]; do
    n=$((n + 1))
    case "$line" in
      "") continue ;;
      *[![:space:]]*) ;;
      *) continue ;;
    esac
    rank=6
    case "$line" in
      "- Security:"*) rank=1 ;;
      "- New:"*) rank=2 ;;
      "- Changed:"*) rank=3 ;;
      "- Fix:"*) rank=4 ;;
      "- Removed:"*) rank=5 ;;
    esac
    printf '%s\t%s\t%06d\t%s\n' "$rank" "$f" "$n" "$line" >> "$work/rows"
  done < "$f"
done

LC_ALL=C sort -t "$(printf '\t')" -k1,1n -k2,2 -k3,3n "$work/rows" > "$work/sorted"
cut -f4- "$work/sorted" > "$work/lines"

if [ ! -s "$work/lines" ]; then
  echo "changelog: fragments have no entries" >&2
  exit 1
fi

set +e
awk '
FNR == NR {
  items[++n] = $0
  next
}
$0 == "## Unreleased" && !done {
  print
  print ""
  for (i = 1; i <= n; i++) print items[i]
  done = 1
  skip = 1
  next
}
skip && $0 ~ /^[[:space:]]*$/ { next }
skip {
  skip = 0
  print ""
  print
  next
}
{ print }
END {
  if (!done) exit 2
}
' "$work/lines" CHANGELOG.md > "$work/out"
rc=$?
set -e
if [ "$rc" -ne 0 ]; then
  echo "changelog: CHANGELOG.md has no ## Unreleased section" >&2
  exit 1
fi

mv "$work/out" CHANGELOG.md

count=$(wc -l < "$work/lines" | tr -d ' ')
echo "changelog: assembled ${count} entries from ${#files[@]} fragments"
while IFS= read -r line; do
  echo "changelog: add ${line}"
done < "$work/lines"

for f in "${files[@]}"; do
  rm -f -- "$f"
  echo "changelog: removed ${f}"
done
