#!/usr/bin/env bash
# Leak guard for the files of this repository outside core/. core/ is a
# copy of harnessix/paddock, which runs its own guard.
# It fails on a private note, a home path other than /home/demo, a
# document that is not public, or a commit email (first argument, a range)
# that is not a noreply address.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
self=.github/scripts/leak-guard.sh
status=0
files=$(git ls-files | grep -v '^core/' || true)

bad=$(grep -E '(^|/)(CLAUDE(\.local)?\.md|AGENTS\.md|\.claude/)' <<<"$files" || true)
[ -z "$bad" ] || { echo "leak guard: private files are tracked:"; echo "$bad"; status=1; }

docs=$(grep -E '\.(md|txt|rst)$' <<<"$files" | grep -vE '^(README\.md|CONTRIBUTING\.md)$' || true)
[ -z "$docs" ] || { echo "leak guard: documents that are not public:"; echo "$docs"; status=1; }

paths=$(grep -v "^$self\$" <<<"$files" | xargs -r grep -nIE '/(home|Users)/[a-z][a-z0-9_-]*/' 2>/dev/null \
  | grep -vE '/(home|Users)/demo/' || true)
[ -z "$paths" ] || { echo "leak guard: home paths:"; echo "$paths"; status=1; }

if [ -n "${1:-}" ]; then
  mails=$(git log --format='%ae%n%ce' "$1" | sort -u | grep -vE '(@users\.noreply\.github\.com|^noreply@github\.com|^noreply@anthropic\.com)$' || true)
  [ -z "$mails" ] || { echo "leak guard: commit emails that are not noreply:"; echo "$mails"; status=1; }
fi

[ "$status" = 0 ] && echo "leak guard: ok"
exit "$status"
