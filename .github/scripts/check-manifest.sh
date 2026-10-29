#!/usr/bin/env bash
# Check the Omarchy plugin manifest without the omarchy CLI.
# It checks that manifest.json is valid JSON, has the required fields,
# and that every entry point file exists.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
fail() { echo "manifest check: $*" >&2; exit 1; }

jq empty manifest.json || fail "manifest.json is not valid JSON"
for key in schemaVersion id name version license description kinds entryPoints; do
  jq -e --arg k "$key" 'has($k)' manifest.json >/dev/null || fail "missing field: $key"
done
jq -e '.kinds | type == "array" and length > 0' manifest.json >/dev/null \
  || fail "kinds must be a non-empty array"
for path in $(jq -r '.entryPoints[]' manifest.json); do
  [ -f "$path" ] || fail "entry point not found: $path"
done
echo "manifest check: ok"
