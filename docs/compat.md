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

## `store-schema-v1`

Development builds of smelt 0.8 created `smelt.db` at schema version 1, which kept
the transcript preview, origin and tool state as plain columns. Opening such a
database moves origin and tool state into the compressed `payload` column, drops
the old columns and returns the freed pages to the file system.

Remove `upgrade_v1` and its test in `crates/store/src/db.rs` and the version 1
case in `open` with the 0.8 release.
