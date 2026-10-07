# Session storage

Session storage retains lossless current and historical state without copying
unchanged archives into every revision. New unique content and revision metadata
still consume space; this is not a fixed-size retention policy.

## Persistent representation

Each lineage uses SQLite, immutable content-addressed objects and persistent
history/transcript sequences. A small revision envelope references shared archive
roots, the initial message and checkpoint bodies. Changed archive suffixes create
new rows and roots; unchanged values retain their existing references. The shared
revision-state codec is format 2.

History queries use a persistent, root-specific semantic index. Completed sequence
nodes and archive-coordinate proofs support bounded lookups without repeatedly
validating or decoding the entire archive. Full historical reads and integrity
checks remain explicit cold operations.

The implementation has three main ownership boundaries:

- `crates/core/src/session/archives.rs` captures immutable prepared saves and
  acknowledges the exact published frame. Snapshot tables share immutable vectors
  with copy-on-write edits, instance-scoped versions and independent dirty suffixes.
- `crates/tui/src/persist.rs` owns asynchronous persistence, backpressure, exact
  receipt recovery and receipt-gated engine dispatch. It does not add storage waits
  to rendering or key handling. A blocked save requires explicit retry.
- `crates/store/src/lineage/` owns transactional publication, immutable archive
  storage, semantic queries, physical sharing and canonical reclamation.

## Preparation, publication and forks

Native commands carry changed suffixes and references rather than expanded
archives. Preparation, command fingerprinting, catalog publication and fork
capture therefore do not traverse unchanged archive bodies. Fingerprints include
all command inputs, including the expected head.

Prepared frames bind to a session owner, document epoch, exact generation/head and
archive base. A preceding publication can advance that base only through checked
finalization. Publication and acknowledgement use the finalized frame, not the
original preparation. Replayed receipts must not regress independently
acknowledged snapshot versions or shared-body ownership.

Loaded documents verify their exact heads before adoption and recovery. Foreign
epochs cannot acknowledge archive mutations. Catalog updates publish lightweight
projections with shared message ownership rather than reloading a full session.

Forks capture an exact source head and share retained archive values. A clean fork
uses acknowledged stored state; preserving unsaved edits additionally requires the
captured acknowledged head to match. Source close/adoption still obeys its existing
interactive deadline. Rewind boundaries, retained forks, receipt results and exact
command recovery remain durable.

## Supported schemas and compatibility

Fresh databases are created directly with schema v5. A writable open of a genuine
v3 or v4 database migrates it to v5 in one transaction. Read-only v3 and v4 access
validates the original layout without writing or migrating it. Schema v5 adds a
`response_diagnostics` request-object role; full responses retain the `response`
role, independent of whether the request body is retained.

Schema validation checks the exact layout, not just its version marker. Unreleased
development layouts are not supported migration sources. Production DDL consists
of `crates/store/src/lineage_schema.sql`, `crates/store/src/lineage_v3.sql`, and
`crates/store/src/lineage_v4.sql`.

Migration preserves original logical bytes, hashes, revision identities and
deployed receipt/journal domains. It backfills completed-node proofs and semantic
indexes, preserves external guards, restores the caller's foreign-key mode and
validates the final layout and foreign keys before publishing version markers.
Crashes expose either the original v3/v4 database or the complete v5 database.

Deployed format-1 revision states remain readable. Verified format-2 projections
allow small envelope reads without rehashing their originals. Full historical
reads remain original-authoritative; cold integrity checks verify projections and
reconstructed bytes. A schema upgrade does not authorize deletion of original
objects or inference of missing legacy receipt outcomes.

See [compatibility debt](compat.md) for independent removal conditions.

## Retention and maintenance

One epoch-fenced, resumable canonical collector marks and sweeps in bounded
transactions. Reachability protects historical revisions, fork roots, receipt
results, journal recovery and request objects. Deleted branches retain their
initial revision for creation replay. Idle persistence uses this collector; it
does not perform global physical sharing, search pruning or vacuuming.

Explicit cold maintenance runs through the same collector:

```sh
smelt session doctor --json <session>
smelt session gc <session>
smelt session doctor --json <session>
```

`session gc` shares repeated physical object content, prunes cold search data,
reclaims unreachable canonical rows and objects, then vacuums and checkpoints the
database.
Sharing preserves logical object hashes and bytes and commits only cohorts that
save occupied pages. It can reclaim repetition inside still-reachable historical
objects, which ordinary reachability GC alone cannot remove.

The command reports sharing, deletion, page-recovery and WAL-truncation counters.
Object-byte accounting, occupied SQLite pages, free pages and database/WAL file
sizes are different measurements. Free pages alone do not prove file-space recovery.
Large retained histories make this cold operation expensive; it is not an
interactive latency guarantee. An interrupted command may leave committed
maintenance and a nonempty WAL, so retain both the database and its SQLite
companions and use a WAL-aware online backup, not an immutable read of the main
file alone.

Before maintenance on important data, make a consistent SQLite online backup.
Validation that mutates historical state must use additional independent copies,
not the original database or the preserved control. Session bodies, credentials
and arbitrary real-data errors must not appear in evidence logs.

## Measured scaling

The optimized synthetic gates hold live context fixed while varying unchanged
archives. Ten samples per fixture on source
`4a121e10bbde5f451cb3ec9977914a2802592c0417c80720e7e760d2d454f9ac`
produced the following ranges:

| Operation | Retained content | Observed work |
| --- | --- | --- |
| 20 title changes | 0 / 32 / 128 checkpoints | 20,660-26,630 additional physical object bytes |
| 10 title changes | 128-byte / 256-KiB / 1-MiB initial message | 10,070 additional physical object bytes |
| Title-save preparation | 0 / 32 / 128 checkpoints | 43,200-43,826 allocation bytes |
| Actual-command clean fork capture | 0 / 4,096 / 16,384 retained records | 268,495-271,241 UI-thread allocation bytes |
| Enter preparation | 0 / 16 / 64 / 128 checkpoints | About 137,619-138,426 allocation bytes |
| Enter object hydration | Same checkpoint counts | 7,504-8,000 object bytes |

Earlier matched reproductions added 15,867,200 physical object bytes for ten title
changes with a 1-MiB initial message, and 20,982,720 bytes for twenty title changes
with 128 checkpoints. Enter with 128 checkpoints allocated about 174 MB across
the process; the accepted optimized fixture allocated about 2.29-2.34 MB.

These measurements demonstrate removal of the unchanged-archive slope, not a
universal wall-clock speedup. The machine is shared. Actual live context and new
unique retained content still require proportional work and storage.

Final-v4 maintenance on an isolated real-data copy reduced its canonical file
from 11,981,516,800 to 2,591,383,552 bytes, recovering 9,390,133,248 bytes (78.4%).
Physical recovery was measured on source
`9c0a68abf69cc04a6e16878878a5e1ec7b759174d677616ece476a6ac40b325a`.
The store and core archive sources are byte-identical after the landing rebase;
real-data integrity and lifecycle checks also passed on the rebased source.

## Validation

Fresh automated acceptance passed on the measured final implementation source:

- Optimized workspace: 6,250/6,250 tests passed, 22 skipped, including actual CLI
  maintenance, v3 migration/crash controls and applicable UI snapshots.
- Optimized scaling/stress: 3,190/3,190 executions passed. Eight ignored scaling
  gates and the core archive, fork/load/document, persistence actor, history/load
  and writer groups each ran ten times.
- Strict workspace all-target lint, formatting and tracked/untracked whitespace
  checks passed.
- Actual CI: 6,237/6,237 tests passed, 12 skipped; 89.53% line coverage against the
  unchanged 80% floor.
- Independent audits reconciled selected counts, unique passing identities and
  every exact stress iteration. All 33 scaling fixtures and 12 preparation,
  hydration and dispatch groups had ten accepted samples; every measured turn
  dispatched exactly once and all native fingerprint inputs were verified.

Isolated real-data acceptance also passed on the rebased source. Complete current
snapshot, history, transcript and request digests matched the preserved v3 baseline.
All 15,540 original revision graphs, required logical hashes and receipt rows were
retained; doctor reported a healthy v4 database, and repeated maintenance was
idempotent with unchanged file size and a truncated WAL.

Two additional independent v3/v4 copies passed migration and historical lifecycle
checks at revisions 15,000, 7,770, 1,000 and 1: exact source-bound forks, retained
ranges, supplied no-op saves, receipt replay after reopen, rejection boundaries and
integrity checks. Original data and preserved controls were not mutating targets;
interrupted copies and complete validation evidence remain archived locally.

Full acceptance includes optimized workspace tests, eight ignored scaling gates
repeated ten times, core archive/fork/actor/history/writer stress, applicable CLI
and crash/UI controls, strict lint, format/whitespace, independent count and metric
audits, and the actual CI gate:

```sh
CARGO_INCREMENTAL=0 cargo llvm-cov nextest --workspace \
  --features smelt-tui/harness --fail-under-lines 80
```

Large real-data I/O runs separately from timing-sensitive gates. Every acceptance
result binds to a frozen source digest; older results do not prove a newer source.
