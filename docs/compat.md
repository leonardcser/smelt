# Compatibility debt

Compatibility code we intend to remove while smelt is alpha is marked with
`COMPAT(<id>)` and documented here.

## `libfuzzer-shell-argv`

`fuzz/vendor/libfuzzer-sys` contains the published 0.4.13 crate, patched so
`Command::toString()` quotes literal arguments and output-redirection paths for
POSIX shells. Upstream's unquoted launcher breaks fork campaigns and corpus merges
when checkout, build or data paths contain spaces or shell metacharacters. The
build script also tracks headers, and upstream command tests match the quoting.
Windows execution is unchanged; Fuchsia already executes argv directly.

Production and the isolated integration fixture depend on this same local package.
The ordinary fixture parity test checks their canonical source paths and resolved
lockfile versions. The opt-in real integration lane exercises fork, merge, triage,
and shell argument/redirection round trips.

Remove the vendored package and this marker when a published upstream release
preserves literal POSIX argv and output paths and passes that integration lane.
Switch both dependencies together using Cargo, update both lockfiles, and retain
a resolved-runtime parity check. See `fuzz/README.md` for source provenance and
update instructions.

## `history-chain-column`

The `history.chain` column holds a hash chain over the conversation up to each
row, which identifies a history prefix without reading it. It came in a 0.8
patch release, and earlier 0.8 releases wrote history without it, so rows from
0.8 may have a NULL chain. Schema 2 adds the column where it is missing but
leaves existing rows as they are: computing every chain at once took 12 to 43
seconds on a real 140k-row database. `history::chain_before` folds the bodies of
rows without a chain, from the newest row that has one. An old session pays this
once, for about a second at 10k rows.

Remove the NULL fallback in `chain_before` together with a migration that
backfills every chain, once support for 0.8 databases ends.
