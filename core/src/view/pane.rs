//! The shell script of a view pane: it prints the header, follows the log
//! until the paddock process ends, then prints the end note.

/// GNU tail stops by itself when the process ends (`--pid`).
#[cfg(target_os = "linux")]
pub(super) const SCRIPT: &str = "printf '%s\\n\\n' \"$1\"; tail -n +1 --pid=\"$3\" -f -- \"$2\"; \
     cat -- \"$4\" 2>/dev/null";

/// BSD tail (macOS) has no `--pid`, so a loop stops it, a moment after the
/// end so that the last lines still show.
#[cfg(not(target_os = "linux"))]
pub(super) const SCRIPT: &str = "printf '%s\\n\\n' \"$1\"; tail -n +1 -f -- \"$2\" & t=$!; \
     while kill -0 \"$3\" 2>/dev/null; do sleep 0.2; done; sleep 0.3; \
     kill \"$t\" 2>/dev/null; wait \"$t\" 2>/dev/null; cat -- \"$4\" 2>/dev/null";
