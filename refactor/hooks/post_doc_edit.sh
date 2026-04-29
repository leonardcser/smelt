#!/usr/bin/env bash
# After Edit / Write / MultiEdit on a refactor/*.md file, check the line cap
# and yell if we're over. Doc bloat is the failure mode this project guards
# against — detail belongs in git log + commit messages, not in narrative
# duplication inside the markdown.

set -uo pipefail

input=$(cat)

# Pull tool_input.file_path out of the JSON payload without depending on jq.
file=""
if [[ "$input" =~ \"file_path\"[[:space:]]*:[[:space:]]*\"([^\"]+)\" ]]; then
  file="${BASH_REMATCH[1]}"
fi
[[ -z "$file" ]] && exit 0

# Only enforce caps on refactor/*.md.
case "$file" in
  */refactor/*.md|refactor/*.md) ;;
  *) exit 0 ;;
esac

[[ -f "$file" ]] || exit 0

lines=$(wc -l < "$file" | tr -d ' ')

# Per-file caps. The narrative-prone files (P<n>, INVENTORY) get tighter caps;
# spec docs (REFACTOR, ARCHITECTURE) are larger by design.
case "$(basename "$file")" in
  PROMPT.md)                          cap=100 ;;
  README.md)                          cap=250 ;;
  INVENTORY.md)                       cap=400 ;;
  P[0-9].md|P[0-9][0-9].md)           cap=300 ;;
  REFACTOR.md|ARCHITECTURE.md)        cap=1200 ;;
  FEATURES.md)                        cap=500 ;;
  TRACE.md)                           cap=600 ;;
  TESTING.md|DECISIONS.md)            cap=250 ;;
  *)                                  cap=600 ;;
esac

if (( lines > cap )); then
  cat >&2 <<EOF
$file is now $lines lines (cap: $cap).

Doc bloat is the failure mode this refactor guards against. Slim this file
before continuing — detail belongs in git log + commit messages, not in
narrative inside the markdown. Convert prose to one-bullet-with-SHA
entries; move long histories to git.
EOF
  exit 2
fi

exit 0
