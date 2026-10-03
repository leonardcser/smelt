# Compatibility debt

Compatibility code we intend to remove while smelt is alpha is marked with
`COMPAT(<id>)` and documented here.

See [session storage](session-storage.md) for the final schema, ownership and
maintenance design. The entries below define when deployed storage compatibility
can be removed; schema migration alone does not retire historical bytes or receipts.

## `lineage-schema-v3`

Deployed lineage databases use schema v3. Readers validate that original layout
without writes; writers migrate it directly to v4 in one transaction. The migration
preserves original logical rows and object bytes, backfills completed-node proofs
and history semantic indexes, restores installed guards, checks foreign keys, and
publishes both version markers atomically. Fresh databases are created directly in
v4. Only v3 and the exact final v4 layout are supported; unreleased development
layouts are not migration sources, even if their version marker is 4.

Remove the v3 DDL, migration and read-only layout handling only after retained v3
databases have a verified lossless replacement and read-only v3 support is retired.
Format-1 objects and deployed receipt domains have independent removal conditions;
upgrading the schema does not retire their original bytes or hashes.

## `revision-state-v1`

Deployed v3 revision states embed full archive values in format 1. Cold readers
decode them without changing logical bytes, object hashes, revision identities or
receipts. Genuine v3 migration fixtures write that format until their schema
upgrades. Remove the decoder and fixture writer only after retained format-1 data
has a lossless replacement and read-only v3 support is retired.

Schema v4 retains a verified format-2 shared-state projection under an immutable
original-to-derived payload association, without rehashing old states. Ordinary
envelope reads use the projection; full historical reads remain original-authoritative,
and cold doctor/backup audits verify the derivation. New states use this same
format-2 codec. No codec or schema introduced only during development is retained
as compatibility debt.

## `history-semantic-v3`

Read-only v3 databases use bounded-memory history scans for semantic queries.
Writable databases migrate transactionally to the shared, root-specific semantic
index in v4. Remove the scan fallback and v3 fixture-publication guard after
read-only v3 support is retired and migration fixtures can be constructed without
calling the current revision publication seam.

## `lua-session-turn-block-idx`

`smelt.session.turns()` exposes deprecated `block_idx` as an alias of the
canonical `history_idx`. This prevents older rewind dialogs from passing a
missing value to `smelt.session.rewind_to()` and accidentally rewinding to the
start of the session. Remove the alias after third-party dialogs have migrated
to `history_idx` and the old field has passed through a documented deprecation
window.

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
