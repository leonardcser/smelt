#!/usr/bin/env bash
# Overnight fuzz session: corpus minimize first (fast targets sequential,
# slow targets parallel), then fuzz from the minimized corpus for the
# remaining time budget. Each phase is logged so the morning-after
# triage is "look at the .log files; were there any crashes?".
#
# Usage:
#   fuzz/scripts/overnight.sh [hours=8]
#
# Layout under /tmp/fuzz-overnight-<stamp>/:
#   cmin-<target>.log     — cargo fuzz cmin output
#   fuzz-<target>.log     — cargo fuzz run output (post-cmin)
#   artifacts/            — any crashes that fell out
#
# Crashes are also copied to fuzz/artifacts/<target>/ where the
# normal triage flow finds them.

set -euo pipefail

HOURS="${1:-8}"
TOTAL_SECONDS=$((HOURS * 3600))
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "${REPO_ROOT}"

STAMP="$(date +%Y%m%d-%H%M%S)"
LOG_DIR="/tmp/fuzz-overnight-${STAMP}"
mkdir -p "${LOG_DIR}/artifacts"
echo ">>> overnight session: ${HOURS}h budget, logs in ${LOG_DIR}"

PHASE_START=$(date +%s)

# ── Phase 1: fast targets (text_ops, attached_ops) — sequential ──────
# Each finishes in ~10 min; no point parallelising and competing with
# the slow ones.
for target in text_ops attached_ops; do
    corpus="fuzz/corpus/${target}"
    if [ ! -d "${corpus}" ] || [ -z "$(ls -A "${corpus}" 2>/dev/null)" ]; then
        echo "${target}: no corpus, skipping cmin"
        continue
    fi
    echo ">>> cmin ${target} ($(find "${corpus}" -type f | wc -l) files)"
    cargo +nightly fuzz cmin --sanitizer=none "${target}" >"${LOG_DIR}/cmin-${target}.log" 2>&1 &
    wait $!
done

# ── Phase 2: slow targets (smelt_loop, lua_loop) — parallel ──────────
PIDS=()
for target in smelt_loop lua_loop; do
    corpus="fuzz/corpus/${target}"
    if [ ! -d "${corpus}" ] || [ -z "$(ls -A "${corpus}" 2>/dev/null)" ]; then
        echo "${target}: no corpus, skipping cmin"
        continue
    fi
    echo ">>> cmin ${target} ($(find "${corpus}" -type f | wc -l) files) — backgrounded"
    cargo +nightly fuzz cmin --sanitizer=none "${target}" >"${LOG_DIR}/cmin-${target}.log" 2>&1 &
    PIDS+=($!)
done
echo ">>> waiting for parallel cmin pids: ${PIDS[*]}"
for pid in "${PIDS[@]}"; do
    wait "${pid}" || echo "cmin pid ${pid} exited non-zero"
done

PHASE1_ELAPSED=$(( $(date +%s) - PHASE_START ))
echo ">>> cmin phase done in ${PHASE1_ELAPSED}s"

# ── Phase 3: fuzz from minimized corpora for remaining time ──────────
REMAINING=$(( TOTAL_SECONDS - PHASE1_ELAPSED ))
if [ "${REMAINING}" -le 60 ]; then
    echo ">>> no time left for post-cmin fuzz (${REMAINING}s)"
    exit 0
fi

# Split remaining time across the 4 targets. Sequential rather than
# parallel — saturating all cores with libFuzzer steals each instance's
# exec/s and hurts more than it helps.
PER_TARGET=$(( REMAINING / 4 ))
echo ">>> fuzzing ${PER_TARGET}s per target"

for target in smelt_loop lua_loop text_ops attached_ops; do
    echo ">>> fuzz ${target} for ${PER_TARGET}s"
    cargo +nightly fuzz run --sanitizer=none "${target}" -- \
        -max_total_time="${PER_TARGET}" \
        -ignore_crashes=1 \
        -print_final_stats=1 \
        >"${LOG_DIR}/fuzz-${target}.log" 2>&1 || echo "  ${target} exited non-zero (crash?)"
done

echo
echo ">>> overnight session done. logs in ${LOG_DIR}"
echo ">>> any new crashes appear under fuzz/artifacts/<target>/."
find fuzz/artifacts -newer "${LOG_DIR}" -type f 2>/dev/null | head -20 || true
