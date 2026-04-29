#!/usr/bin/env bash
# Stop hook: block "done" if refactor/check.sh is red. The agent's
# Stopping rule says check.sh must be green before exit; this just enforces
# it mechanically.
#
# Avoids infinite loops by honoring stop_hook_active=true on the input.

set -uo pipefail

input=$(cat)

# If a Stop hook is already trying to keep us going, don't recurse — let it
# stop on the next attempt to avoid getting stuck.
if [[ "$input" =~ \"stop_hook_active\"[[:space:]]*:[[:space:]]*true ]]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}" || exit 0

[[ -x refactor/check.sh ]] || exit 0

if ! refactor/check.sh --quiet >/dev/null 2>&1; then
  echo "refactor/check.sh is red — fix the docs before stopping." >&2
  exit 2
fi

exit 0
