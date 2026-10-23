# Compatibility debt

Compatibility code we intend to remove while smelt is alpha is marked with
`COMPAT(<id>)` and documented here.

See [session storage](session-storage.md) for the final schema, ownership and
maintenance design. The entries below define when deployed storage compatibility
can be removed; schema migration alone does not retire historical bytes or receipts.

## `lineage-schema-v3`

Readers validate the original v3 layout without writes; writers migrate it directly
to v7 in one transaction. The migration preserves original logical rows and object
bytes, backfills completed-node proofs and history semantic indexes, restores
installed guards, checks foreign keys, and publishes both version markers atomically.
Fresh databases use v7. Exact deployed v3, v4, v5 and v6 layouts and the current v7
layout are supported; unreleased development layouts are not migration sources.

Remove the v3 DDL, migration and read-only layout handling only after retained v3
databases have a verified lossless replacement and read-only v3 support is retired.
Format-1 objects and deployed receipt domains have independent removal conditions;
upgrading the schema does not retire their original bytes or hashes.

## `lineage-schema-v4`

The exact v4 schema snapshot remains available for write-free historical reads.
Writers migrate to v7 atomically, adding the `response_diagnostics` request-object
role and rebuilding root mappings with cumulative user ordinals while retaining
canonical object bytes, revisions and receipts. Full responses keep the `response`
role. Legacy summaries, mode changes and process-status messages stored as User
history are not user turns. Counts come from canonical history, including users
without transcript records. Read-only v3/v4/v5 counts use a bounded-memory cold scan;
v4/v5's other semantic queries retain the original index namespace. Runtime count
installation and maintenance keep these scans out of `session.info()` and
interactive rewind application.

Schema v6 requires a completed current-format mapping before revision insertion.
Writers opened before migration cannot bypass count maintenance by publishing an
unindexed changed history root. Remove the v4 schema snapshot, read-only namespace,
count-scan fallback and historical fixture-publication path only after retained v4
stores have verified lossless replacements and read-only v4 support is retired.

## `lineage-schema-v5`

Schema v5 retains shared storage and response diagnostics but has no cumulative
user ordinals. Its exact DDL remains available for write-free validation. Writers
migrate to v7 in one transaction with the same index rebuild and publication fencing
as v4, preserving response roles, immutable objects, revisions and receipts.
Remove the v5 snapshot and count/semantic fallbacks only after retained v5 stores
have verified lossless replacements and read-only v5 support is retired.

## `lineage-schema-v6`

Schema v6 stores revision content references as immutable and non-null. Its exact
DDL remains available for write-free validation. Writers migrate to v7 in one
transaction by rebuilding `lineage_revisions` with releasable content references
and installing the release-only update guard and the receipt-result delete guard;
every row, identity and receipt is preserved. Historical v6 fixtures publish
revisions with the same semantic-index guard as v7. Remove the v6 snapshot and its
migration path only after retained v6 stores have verified lossless replacements
and read-only v6 support is retired.

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
index in v6. Remove this semantic-query scan fallback after read-only v3 support
is retired. Historical count and fixture-publication fallbacks have the independent
removal conditions listed under `lineage-schema-v4`.

## `lua-session-turn-block-idx`

`smelt.session.turns()` exposes deprecated `block_idx` as an alias of the
canonical `history_idx`. This prevents older rewind dialogs from passing a
missing value to `smelt.session.rewind_to()` and accidentally rewinding to the
start of the session. Remove the alias after third-party dialogs have migrated
to `history_idx` and the old field has passed through a documented deprecation
window.

## `unlinked-steering-rewind`

Older steering acknowledgements saved user transcript rows without history origins.
When an exact rewind link is absent, the TUI can recover the selected boundary from
one unlinked row with the same display, command flag, and submission timestamp.
Recovery requires a unique match in both canonical history and the transcript.
It scans at most 1024 history items and 1024 user rows, with 2 MiB content budgets,
and refuses unsaved document changes, incomplete scans, missing timestamps, or
ambiguous matches. The normal rewind commit retains the exact stored prefix; it
does not rebuild old tool output or rewrite retained revisions.

Remove this fallback after affected saved sessions have been retired or repaired
and support for rewinding their unlinked rows is no longer required.

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
