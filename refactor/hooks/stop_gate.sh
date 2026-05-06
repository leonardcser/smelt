#!/usr/bin/env bash
# Stop hook: block "done" if refactor/check.sh is red. The agent's
# Stopping rule says check.sh must be green before exit; this just enforces
# it mechanically.
#
# When this stop_gate is part of a RALPH iteration (`RALPH_ITER=1` env from
# refactor/ralph.sh) and the gate passes, also close the iteration's tmux
# window so the loop driver can advance. Backgrounded so the hook returns
# the gate decision before tmux tears down the pane.
#
# Avoids infinite loops by honoring stop_hook_active=true on the input.

set -uo pipefail

input=$(cat)

kill_iter_window() {
  if [[ -n "${RALPH_ITER:-}" && -n "${TMUX_PANE:-}" ]]; then
    ( sleep 0.2 && tmux kill-window -t "$TMUX_PANE" >/dev/null 2>&1 ) &
    disown 2>/dev/null || true
  fi
}

# If a Stop hook is already trying to keep us going, don't recurse — let it
# stop on the next attempt. Ralph's own loop check picks up red state.
if [[ "$input" =~ \"stop_hook_active\"[[:space:]]*:[[:space:]]*true ]]; then
  kill_iter_window
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}" || exit 0

if [[ ! -x refactor/check.sh ]]; then
  kill_iter_window
  exit 0
fi

if ! refactor/check.sh --quiet >/dev/null 2>&1; then
  echo "refactor/check.sh is red — fix the docs before stopping." >&2
  exit 2
fi

kill_iter_window
exit 0
