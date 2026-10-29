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

## `lineage-schema-v7`

`initialize_lineage_schema` upgrades v7 lineage databases to v8 in place. v7
reclaimed by tracing the whole graph after every change and had no reclamation
queue. The migration installs the queue and its triggers, lets history semantic
indexes be deleted once no revision uses them, and queues every row once so
whatever v7 left unreclaimed is still found.

Remove the `7` arm and `migrate_reclamation_queue` once alpha users have opened
their sessions with a v8 build, or when a later schema drops v7 support.

## `linux-gnu-asset`

Linux releases are statically linked musl builds published as
`smelt-<arch>-unknown-linux-musl.tar.gz`. Builds from glibc-linked releases ask
`smelt upgrade` for `smelt-<arch>-unknown-linux-gnu.tar.gz`, so the release
job also publishes each musl archive under that name. After the upgrade,
the installed binary reports the musl target and requests the new name.

Remove the copy loop in the release job once alpha users have upgraded past the
last glibc-linked release.
