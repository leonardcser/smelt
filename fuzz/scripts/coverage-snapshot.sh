#!/usr/bin/env bash
# Source-code coverage snapshot for each fuzz target. Runs the target
# against its on-disk corpus, then reports per-target line/function/
# region coverage and timestamps the result under
# fuzz/coverage-history/. Use it to A/B your own changes: snapshot
# before, change, snapshot after, diff the summary files.
#
# Usage:
#   fuzz/scripts/coverage-snapshot.sh [target...]
#
# With no args, snapshots every target that has a corpus. Specify one
# or more target names to limit.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "${REPO_ROOT}"

LLVM_COV="$(find ~/.rustup/toolchains -name llvm-cov -type f 2>/dev/null | head -1)"
if [ -z "${LLVM_COV}" ]; then
    echo "error: llvm-cov not found in ~/.rustup/toolchains" >&2
    echo "       run: rustup component add llvm-tools-preview --toolchain nightly" >&2
    exit 1
fi

ALL_TARGETS=(smelt_loop lua_loop text_ops attached_ops)
if [ $# -eq 0 ]; then
    TARGETS=("${ALL_TARGETS[@]}")
else
    TARGETS=("$@")
fi

HIST_DIR="fuzz/coverage-history"
mkdir -p "${HIST_DIR}"
STAMP="$(date +%Y%m%d-%H%M%S)"
SHA="$(git rev-parse --short HEAD)"
SUMMARY="${HIST_DIR}/${STAMP}-${SHA}.txt"

{
    echo "# fuzz coverage snapshot"
    echo "date: $(date -Iseconds)"
    echo "commit: $(git rev-parse HEAD)"
    echo "branch: $(git rev-parse --abbrev-ref HEAD)"
    echo
} >"${SUMMARY}"

for target in "${TARGETS[@]}"; do
    corpus="fuzz/corpus/${target}"
    if [ ! -d "${corpus}" ] || [ -z "$(ls -A "${corpus}" 2>/dev/null)" ]; then
        echo "${target}: no corpus, skipping" | tee -a "${SUMMARY}"
        continue
    fi
    nfiles=$(find "${corpus}" -maxdepth 1 -type f | wc -l)
    echo ">>> ${target}: ${nfiles} corpus files" >&2
    cargo +nightly fuzz coverage --sanitizer=none "${target}" "${corpus}" >/dev/null 2>&1

    profdata="fuzz/coverage/${target}/coverage.profdata"
    binary="target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/${target}"
    if [ ! -f "${profdata}" ] || [ ! -f "${binary}" ]; then
        echo "${target}: coverage build missing, skipping" | tee -a "${SUMMARY}"
        continue
    fi

    totals=$("${LLVM_COV}" report \
        "${binary}" \
        -instr-profile="${profdata}" \
        -ignore-filename-regex='/.cargo/|/rustc/|/.rustup/|fuzz/' \
        2>/dev/null | tail -1)
    echo "${target} (${nfiles}f): ${totals}" | tee -a "${SUMMARY}"
done

echo >&2
echo "snapshot saved to ${SUMMARY}" >&2
