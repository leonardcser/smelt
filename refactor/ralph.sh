#!/usr/bin/env bash
# refactor/ralph.sh — run the RALPH refactor loop in a new tmux window.
#
# Each iteration is a fresh `claude --permission-mode auto` process driven by
# refactor/PROMPT.md. Loop stops when one of:
#   - claude exits non-zero
#   - refactor/check.sh goes red
#   - the iteration produces no new commit (sign of an early-stop or block)
# Ctrl-C in the tmux window stops the loop.
#
# Usage:
#   refactor/ralph.sh           # spawn a new tmux window running the loop
#   refactor/ralph.sh --here    # run the loop in the current shell (no tmux)

set -uo pipefail

mode="${1:-tmux}"

if [[ "$mode" != "--in-window" && "$mode" != "--here" ]]; then
  if [[ -z "${TMUX:-}" ]]; then
    echo "ralph.sh must be run from inside a tmux session (or use --here)" >&2
    exit 1
  fi
  repo_root=$(git rev-parse --show-toplevel)
  if tmux list-windows -F '#W' | grep -qx ralph; then
    echo "ralph window already exists in this session; switching to it"
    exec tmux select-window -t ralph
  fi
  exec tmux new-window -n ralph -c "$repo_root" "$repo_root/refactor/ralph.sh --in-window"
fi

cd "$(git rev-parse --show-toplevel)"

if [[ ! -f refactor/PROMPT.md ]]; then
  echo "refactor/PROMPT.md not found" >&2
  exit 1
fi

iter=0
while :; do
  iter=$((iter + 1))
  echo
  echo "=== ralph iteration $iter — $(date '+%Y-%m-%d %H:%M:%S') ==="
  echo

  before=$(git rev-parse HEAD)

  # `-p` (print/headless) so claude exits when the agent stops; without it,
  # interactive mode would return to a prompt and the loop would hang.
  # `--verbose` streams tool calls + reasoning into tmux so you can watch
  # the iteration in real time.
  claude --permission-mode auto -p --verbose "$(cat refactor/PROMPT.md)"
  rc=$?

  if [[ $rc -ne 0 ]]; then
    echo
    echo "=== claude exited $rc — loop stopped ==="
    break
  fi

  if ! refactor/check.sh --quiet; then
    echo
    echo "=== refactor/check.sh red — loop stopped ==="
    break
  fi

  after=$(git rev-parse HEAD)
  if [[ "$before" == "$after" ]]; then
    echo
    echo "=== no commit landed this iteration — loop stopped ==="
    break
  fi

  echo
  echo "=== iteration $iter landed (HEAD: $(git log -1 --oneline)) ==="
  sleep 3
done

echo
echo "=== ralph loop ended after $iter iteration(s) ==="
echo "(window stays open; Ctrl-D or 'exit' to close)"
exec bash
