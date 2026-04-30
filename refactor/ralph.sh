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
#   - refactor/.ralph-done present       — plan complete (exit 0)
#   - refactor/.ralph-needs-input present — every remaining sub-phase
#                                           needs a user decision (exit 0)
#   - the iteration produces no new commit — stuck (exit 1)
#   - Ctrl-C in the ralph window
#
# `check.sh` red is NOT a stop reason — the next fresh iteration will
# see the red docs and try to fix them. If it can't, the no-commit
# branch eventually catches it.
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
  # `RALPH_ITER=1` tells refactor/hooks/stop_gate.sh to close this window
  # when the agent stops cleanly, so the loop advances on its own.
  iter_win=$(tmux new-window -n "ralph-$iter" -c "$PWD" -P -F '#{window_id}' \
    "RALPH_ITER=1 claude --permission-mode auto --effort xhigh")

  # Wait for claude's TUI to come up before sending input. Generous
  # because AGENTS.md auto-loads several large refactor/*.md files;
  # too short and the bracketed paste lands before claude enables
  # bracketed-paste mode and gets eaten.
  sleep 10

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

  if [[ -f refactor/.ralph-done ]]; then
    echo
    echo "=== plan complete: $(cat refactor/.ralph-done) ==="
    echo "=== ralph loop ended cleanly ==="
    exit 0
  fi

  if [[ -f refactor/.ralph-needs-input ]]; then
    echo
    echo "=== your turn: $(cat refactor/.ralph-needs-input) ==="
    echo "=== see open questions in the active refactor/P<n>.md ==="
    exit 0
  fi

  after=$(git rev-parse HEAD)
  if [[ "$before" == "$after" ]]; then
    echo
    echo "=== no commit landed this iteration — loop stopped (stuck) ==="
    break
  fi

  echo
  echo "=== iteration $iter landed (HEAD: $(git log -1 --oneline)) ==="
  sleep 3
done

echo
echo "=== ralph loop ended after $iter iteration(s) ==="
