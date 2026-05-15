#!/usr/bin/env bash
# Run cargo-fuzz in cycles until Ctrl-C. Each cycle fuzzes for SECS_PER_CYCLE
# seconds; libFuzzer crashes are archived under `crashes/<timestamp>/` and the
# loop continues. The corpus is cmin'd every CMIN_EVERY cycles to keep memory
# and disk footprint bounded over long runs.
#
# Usage:
#   ./fuzz/fuzz-loop.sh                  # default 600s cycles, cmin every 10
#   ./fuzz/fuzz-loop.sh 1200             # 20-minute cycles
#   ./fuzz/fuzz-loop.sh 600 5            # 10-minute cycles, cmin every 5
#   ./fuzz/fuzz-loop.sh 600 5 12         # … with 12 parallel fork workers
#
# Parallelism: libFuzzer's `-fork=N` runs N worker processes that share the
# corpus; the parent re-spawns any that crash and keeps running for the
# cycle's full `-max_total_time`. Default is `nproc/2` capped at 12. Pass 1
# to disable. Workers each report into the parent's stdout (one log to tail).
#
# Exit codes:
#   0   clean stop on SIGINT/SIGTERM
#   2   build failed on the first cycle (nothing to loop over)

set -uo pipefail

SECS_PER_CYCLE="${1:-600}"
CMIN_EVERY="${2:-10}"
default_fork() {
  local n
  if command -v nproc >/dev/null 2>&1; then
    n=$(nproc)
  elif command -v sysctl >/dev/null 2>&1; then
    n=$(sysctl -n hw.ncpu 2>/dev/null || echo 2)
  else
    n=2
  fi
  local half=$(( n / 2 ))
  (( half < 1 )) && half=1
  (( half > 12 )) && half=12
  echo "$half"
}
FORK="${3:-$(default_fork)}"

cd "$(dirname "$0")"

if [[ ! -d seed_corpus/smelt_loop ]]; then
  echo "fuzz-loop: unpacking seed corpus"
  tar -xzf seed_corpus.tar.gz
fi

mkdir -p crashes artifacts/smelt_loop

START_EPOCH=$(date +%s)
ITERATIONS=0
CRASHES=0
INTERRUPTED=0

on_interrupt() {
  INTERRUPTED=1
}
trap on_interrupt INT TERM

while (( INTERRUPTED == 0 )); do
  ITERATIONS=$((ITERATIONS + 1))
  ELAPSED=$(( $(date +%s) - START_EPOCH ))
  printf '\n=== fuzz-loop cycle %d | elapsed %ds | crashes %d | fork=%d ===\n' \
    "$ITERATIONS" "$ELAPSED" "$CRASHES" "$FORK"

  # Snapshot existing artifacts so we only archive *new* crashes from this cycle.
  PRE_ARTIFACTS=$(ls -1 artifacts/smelt_loop 2>/dev/null | sort)

  set +e
  cargo +nightly fuzz run smelt_loop seed_corpus/smelt_loop -- \
    -max_len=4096 \
    -max_total_time="$SECS_PER_CYCLE" \
    -timeout=10 \
    -fork="$FORK"
  RC=$?
  set -e

  if (( INTERRUPTED )); then
    break
  fi

  if (( RC != 0 )); then
    # Distinguish first-cycle build failure from a crash. cargo-fuzz's "no
    # crashes, exit 0" path means RC==0; non-zero with no new artifact is
    # almost always a build error.
    POST_ARTIFACTS=$(ls -1 artifacts/smelt_loop 2>/dev/null | sort)
    NEW_ARTIFACTS=$(comm -13 <(echo "$PRE_ARTIFACTS") <(echo "$POST_ARTIFACTS"))

    if [[ -z "$NEW_ARTIFACTS" ]]; then
      if (( ITERATIONS == 1 )); then
        echo "fuzz-loop: cargo exited $RC on first cycle with no artifact; assuming build failure"
        exit 2
      fi
      echo "fuzz-loop: cargo exited $RC but produced no new artifact; continuing"
    else
      CRASHES=$((CRASHES + 1))
      TS=$(date +%Y%m%d-%H%M%S)
      SAVE_DIR="crashes/${TS}-cycle${ITERATIONS}"
      mkdir -p "$SAVE_DIR"
      while IFS= read -r f; do
        [[ -z "$f" ]] && continue
        mv "artifacts/smelt_loop/$f" "$SAVE_DIR/"
      done <<< "$NEW_ARTIFACTS"
      printf '*** crash %d archived to %s ***\n' "$CRASHES" "$SAVE_DIR"
    fi
  fi

  if (( ITERATIONS % CMIN_EVERY == 0 )) && (( INTERRUPTED == 0 )); then
    echo "fuzz-loop: cmin (cycle $ITERATIONS)"
    set +e
    cargo +nightly fuzz cmin smelt_loop seed_corpus/smelt_loop
    set -e
  fi
done

ELAPSED=$(( $(date +%s) - START_EPOCH ))
printf '\nfuzz-loop: stopped after %d cycle(s), %ds, %d crash(es)\n' \
  "$ITERATIONS" "$ELAPSED" "$CRASHES"
if (( CRASHES > 0 )); then
  echo "fuzz-loop: archived crashes in fuzz/crashes/"
fi
exit 0
