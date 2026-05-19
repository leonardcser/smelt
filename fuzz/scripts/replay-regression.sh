#!/usr/bin/env bash
# Replay every regression seed under fuzz/seeds/<target>/regression/.
# Non-zero exit on the first failure. Used by CI smoke and by devs
# wanting "did I just regress a known bug?" answers locally.
#
# Targets with JSON scenarios (smelt_loop, lua_loop) replay via the
# in-tree replay_scenario binary. Targets that take raw libFuzzer
# bytes (text_ops, attached_ops) replay via `cargo fuzz run --runs=0`
# which executes every file once and exits.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
SEEDS_DIR="${REPO_ROOT}/fuzz/seeds"
cd "${REPO_ROOT}"

JSON_TARGETS=(smelt_loop lua_loop)
BYTE_TARGETS=(text_ops attached_ops)

# Build replay_scenario once up front so each iteration is just a
# subprocess invocation, not a fresh cargo invocation.
echo ">>> building replay_scenario"
cargo build --bin replay_scenario --manifest-path fuzz/Cargo.toml -q

fail=0

for target in "${JSON_TARGETS[@]}"; do
    dir="${SEEDS_DIR}/${target}/regression"
    if [ ! -d "${dir}" ]; then continue; fi
    files=("${dir}"/*.json)
    [ -e "${files[0]}" ] || continue
    echo ">>> ${target}: ${#files[@]} regression seed(s)"
    for seed in "${files[@]}"; do
        name="$(basename "${seed}")"
        if ./fuzz/target/debug/replay_scenario --target "${target}" "${seed}" >/dev/null 2>&1; then
            echo "  ok   ${name}"
        else
            echo "  FAIL ${name}"
            fail=1
        fi
    done
done

for target in "${BYTE_TARGETS[@]}"; do
    dir="${SEEDS_DIR}/${target}/regression"
    if [ ! -d "${dir}" ]; then continue; fi
    files=("${dir}"/*)
    [ -e "${files[0]}" ] || continue
    # `cargo fuzz run --runs=0` executes every file in the corpus dir
    # exactly once and exits. Nightly-only.
    echo ">>> ${target}: ${#files[@]} byte-form seed(s)"
    if cargo +nightly fuzz run --sanitizer=none "${target}" "${dir}" -- -runs=0 >/dev/null 2>&1; then
        echo "  ok"
    else
        echo "  FAIL"
        fail=1
    fi
done

if [ "${fail}" -ne 0 ]; then
    echo
    echo "regression replay FAILED"
    exit 1
fi
echo
echo "all regression seeds passed"
