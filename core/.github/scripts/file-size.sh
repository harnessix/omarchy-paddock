#!/usr/bin/env bash
# Check that tracked Rust and QML files stay within their size limits.
#
# A file listed in .github/file-size-baseline (`<lines> <path>`) must
# not grow past that size. A file that is not listed must not be longer
# than 800 lines. When a listed file shrinks, the script prints the lower
# number to put in the baseline, and does not fail.
#
# Usage: file-size.sh
set -euo pipefail

root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "file-size: run this inside the git repository" >&2
  exit 1
}
cd "$root"

default_limit=800
baseline_file=".github/file-size-baseline"
status=0

# Check the baseline lines. bash 3 (macOS) has no associative arrays, so
# limit_for reads the file again for each lookup.
if [ -f "$baseline_file" ]; then
  while read -r lines path; do
    case "${lines:-}" in
      '' | \#*) continue ;;
    esac
    if [ -z "${path:-}" ]; then
      echo "file-size: bad baseline line: ${lines}" >&2
      exit 2
    fi
  done <"$baseline_file"
fi

# The baseline size of a file, or nothing when the file is not listed.
limit_for() {
  [ -f "$baseline_file" ] || return 0
  awk -v p="$1" '$1 !~ /^#/ && $2 == p { print $1; exit }' "$baseline_file"
}

count_lines() {
  # wc -l counts newline characters. Files in this repo end with a newline.
  wc -l <"$1" | tr -d ' '
}

while IFS= read -r -d '' file; do
  size=$(count_lines "$file")
  limit=$(limit_for "$file")
  if [ -n "$limit" ]; then
    if [ "$size" -gt "$limit" ]; then
      echo "file-size: ${file} has ${size} lines; the limit is ${limit}"
      echo "file-size: split the file, or change the baseline in a separate PR with a reason"
      status=1
    elif [ "$size" -lt "$limit" ]; then
      echo "file-size: ${file} shrank to ${size} lines (baseline ${limit}). Put ${size} in ${baseline_file}"
    fi
  else
    if [ "$size" -gt "$default_limit" ]; then
      echo "file-size: ${file} has ${size} lines; the limit is ${default_limit}"
      echo "file-size: split the file, or change the baseline in a separate PR with a reason"
      status=1
    fi
  fi
done < <(git ls-files -z -- '*.rs' '*.qml')

if [ "$status" = 0 ]; then
  echo "file-size: ok"
fi
exit "$status"
