#!/usr/bin/env bash
# refactor/ralph.sh — run the RALPH refactor loop in tmux.
#
# Renames the current tmux window to "ralph" and runs the loop driver
# in-place. Each iteration spawns a fresh tmux window running an
# interactive `claude --permission-mode auto`, pastes refactor/PROMPT.md
# into the input, and submits with Enter. The loop advances when that
# window closes (you /exit claude, or it crashes).
#
# Loop stops when one of:
#   - refactor/check.sh goes red
#   - the iteration produces no new commit (early-stop or block)
#   - Ctrl-C in the ralph window
#
# Usage:
#   refactor/ralph.sh

set -uo pipefail

if [[ -z "${TMUX:-}" ]]; then
  echo "ralph.sh must be run from inside a tmux session" >&2
  exit 1
fi

cd "$(git rev-parse --show-toplevel)"

if [[ ! -f refactor/PROMPT.md ]]; then
  echo "refactor/PROMPT.md not found" >&2
  exit 1
fi

tmux rename-window ralph

iter_win=""
trap '[[ -n "$iter_win" ]] && tmux kill-window -t "$iter_win" 2>/dev/null; exit 130' INT TERM

iter=0
while :; do
  iter=$((iter + 1))
  echo
  echo "=== ralph iteration $iter — $(date '+%Y-%m-%d %H:%M:%S') ==="
  echo

  before=$(git rev-parse HEAD)

  # Fresh tmux window running interactive claude with auto-permission.
  iter_win=$(tmux new-window -n "ralph-$iter" -c "$PWD" -P -F '#{window_id}' \
    "claude --permission-mode auto")

  # Wait for claude's TUI to come up before sending input.
  sleep 4

  # Paste the prompt as a bracketed paste (so embedded newlines don't
  # auto-submit), then send a single Enter to submit.
  tmux load-buffer -b ralph-prompt refactor/PROMPT.md
  tmux paste-buffer -p -b ralph-prompt -t "$iter_win"
  tmux delete-buffer -b ralph-prompt
  sleep 0.5
  tmux send-keys -t "$iter_win" Enter

  # Block until the iteration window closes (claude exited, /exit, or
  # window killed).
  while tmux list-windows -F '#{window_id}' | grep -qx "$iter_win"; do
    sleep 2
  done
  iter_win=""

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
