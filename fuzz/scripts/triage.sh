#!/usr/bin/env bash
# Triage a crash artifact: shrink it, format it, print the result.
# Replaces the manual "cargo fuzz fmt → cargo fuzz tmin → eyeball → maybe
# write it up" dance that nobody actually does end-to-end.
#
# Usage:
#   fuzz/scripts/triage.sh <target> <crash-artifact>
#
# Example:
#   fuzz/scripts/triage.sh lua_loop fuzz/artifacts/lua_loop/crash-1944...
#
# Outputs:
#   - shrunk scenario JSON to stdout
#   - one-liner that turns the shrunk JSON into a regression seed

set -euo pipefail

if [ $# -ne 2 ]; then
    echo "usage: $0 <target> <crash-artifact>" >&2
    exit 2
fi

TARGET="$1"
ARTIFACT="$2"
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "${REPO_ROOT}"

if [ ! -f "${ARTIFACT}" ]; then
    echo "error: artifact not found: ${ARTIFACT}" >&2
    exit 1
fi

case "${TARGET}" in
    smelt_loop|lua_loop) ;;
    *)
        echo "error: triage only handles smelt_loop / lua_loop (others have no scenario form)" >&2
        exit 2
        ;;
esac

TMPDIR_=$(mktemp -d)
trap 'rm -rf "${TMPDIR_}"' EXIT

echo ">>> 1/3 building tools" >&2
cargo build --manifest-path fuzz/Cargo.toml \
    --bin crash_to_scenario --bin shrink_scenario --bin replay_scenario -q

# Bytes -> JSON scenario.
RAW="${TMPDIR_}/raw.json"
./fuzz/target/debug/crash_to_scenario --target "${TARGET}" "${ARTIFACT}" >"${RAW}"

# JSON -> shrunk JSON (delta-debugging).
SHRUNK="${TMPDIR_}/shrunk.json"
echo ">>> 2/3 shrinking (predicate: same panic)" >&2
./fuzz/target/debug/shrink_scenario --target "${TARGET}" "${RAW}" >"${SHRUNK}"

echo ">>> 3/3 shrunk scenario:" >&2
cat "${SHRUNK}"

cat >&2 <<EOF

────────────────────────────────────────────────────────────────────────
to commit as a regression seed:
  cp ${SHRUNK} fuzz/seeds/${TARGET}/regression/<slug>.json
  # edit to add _about and _fix fields before committing
EOF
