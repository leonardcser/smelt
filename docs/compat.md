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

## `state-v0.7`

smelt 0.8 keeps sessions, prompts and usage in one `smelt.db` and renames
credential and cache files. On first start, smelt moves the 0.7 files into
`<state>/v0.7/` and imports them; `smelt session migrate` retries sessions that
failed. The importer reads 0.7 sessions through the `smelt-store` crate pinned
at the `v0.7.0` tag (`smelt-store-v07` in `crates/core/Cargo.toml`).

Remove `crates/core/src/migrate_v07.rs`, its startup hook and `session migrate`
command in `src/main.rs`, the store's `import_prompts` and `import_usage`, and
the `smelt-store-v07` dependency of `smelt-core` in the next minor release after
0.8.

## `history-chain-column`

The `history.chain` column holds a hash chain over the conversation up to each
row, which identifies a history prefix without reading it. It came in a 0.8
patch release without a new schema version, so earlier 0.8 releases still open
the database: they name their columns when they write history and leave the
chain NULL. `db::add_history_chain` adds the column to databases from before
it, and `history::chain_before` folds the bodies of rows without a chain, from
the newest row that has one. An old session pays this once, for about a second
at 10k rows. A migration that computes every chain at once took 12 to 43
seconds on a real 140k-row database.

In 0.9, together with `state-v0.7`: replace `add_history_chain` with a schema 2
migration that adds the column when it is missing. The NULL fallback in
`chain_before` can only go after every row has a chain, which needs that
backfill.

## `claude-code-link-v1`

Earlier 0.8 releases linked a smelt session to its native Claude Code session
in `claude-code.json`, naming the history by a SHA-256 of its whole JSON. Links
are now `claude-code-session.json`, keyed by `HistoryIdentity::key`. When only a
legacy link exists, the native backend asks the host for the history items,
resumes the link once if their digest matches, and removes it. Its checkpoints
are dropped. `migrate_v07` moves 0.7 links to the legacy name, so they take
this path too.

In 0.9, together with `state-v0.7`: remove `LegacyLink`, `legacy_link_path`,
`has_legacy` and `take_legacy` in `claude_code/session.rs`, and the legacy
branch in `Runtime::open`. A native session with only a legacy link then
imports its history once.
