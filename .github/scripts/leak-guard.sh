#!/usr/bin/env bash
# Leak guard: stop private files and private data from entering this public repo.
#
# It fails when:
#   1. A tracked file is a private agent or design note
#      (docs/DESIGN.md, CLAUDE.md, CLAUDE.local.md, AGENTS.md, .claude/).
#   2. A tracked file contains a blocked internal name.
#   3. A tracked file contains a home path such as /home/<user>/.
#      Use /home/demo/ in examples.
#   4. A commit in the range (first argument, for example origin/main..HEAD)
#      has an author or committer email that is not a noreply address.
#
# To fix a failure, remove the file or text, or rewrite the commit with a
# GitHub noreply email (Settings > Emails > Keep my email address private).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
self=.github/scripts/leak-guard.sh
status=0

bad_files=$(git ls-files | grep -E '^(docs/DESIGN\.md|(.*/)?CLAUDE(\.local)?\.md|(.*/)?AGENTS\.md|(.*/)?\.claude/)' || true)
if [ -n "$bad_files" ]; then
  echo "leak guard: these private files must not be tracked:"; echo "$bad_files"; status=1
fi

# Internal names, built from parts so this file does not contain them.
blocked="r""e""o""l"
hits=$(git grep -nIi -e "$blocked" -- . ":!$self" || true)
if [ -n "$hits" ]; then
  echo "leak guard: blocked internal name found:"; echo "$hits"; status=1
fi

hits=$(git grep -noIE '/home/[A-Za-z0-9_.-]+' -- . ":!$self" | grep -vE ':/home/demo$' || true)
if [ -n "$hits" ]; then
  echo "leak guard: private home path found (use /home/demo/):"; echo "$hits"; status=1
fi

if [ -n "${1:-}" ]; then
  emails=$(git log --format='%ae%n%ce' "$1" | sort -u)
  for e in $emails; do
    case "$e" in
      *@users.noreply.github.com|noreply@github.com|noreply@anthropic.com) ;;
      # Only the masked address goes to the public log.
      *) echo "leak guard: a commit email is not a noreply address: ${e:0:1}***@${e#*@}"; status=1 ;;
    esac
  done
fi

[ "$status" = 0 ] && echo "leak guard: ok"
exit "$status"
