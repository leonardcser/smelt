# Session storage

Session storage retains lossless current state, every turn boundary and the
recent revisions in full, without copying unchanged archives into every revision.
Older intermediate saves keep only their chain link. New unique content and
revision metadata still consume space; this is not a fixed-size retention policy.

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

- `crates/core/src/session/archives.rs` captures immutable changed archive suffixes
  under the original owner scope and table versions, then acknowledges the exact
  published frame. Captures do not hold whole snapshot vectors, so later edits do
  not copy an entire table merely because a save is pending. Snapshot tables keep
  instance-scoped versions and independent dirty suffixes.
- `crates/tui/src/persist.rs` owns asynchronous persistence, backpressure, exact
  receipt recovery and receipt-gated engine dispatch. Ordinary submission does not
  wait for storage in rendering or key handling. Asynchronous resume polls a
  two-phase writer close; explicit replacement helpers still use a bounded
  synchronous close. A blocked save requires explicit retry.
- `crates/store/src/lineage/` owns transactional publication, immutable archive
  storage, semantic queries, physical sharing and canonical reclamation.

## Execution and persistence lifecycle

Execution state and outstanding storage commands have separate ownership:

- **Queued:** input has not been accepted as a turn.
- **Accepted:** the user message is in the transcript and the working indicator
  is active. Request preparation and canonical submission can still be pending.
- **Running:** the engine has received the accepted request after its canonical
  submission receipt. This receipt still covers a complete prepared batch, not
  an independent small durable intent.
- **Completed, interrupted or failed:** execution has stopped. Terminal storage
  work can remain pending without preventing acceptance of the next turn.

Forcing a queued replacement does not wait for prior terminal storage. Cancelling
a submission already sent to storage keeps only its command identity for receipt
settlement.
A late cancelled receipt schedules its terminal transition; it cannot dispatch
that request or change the working state of a newer accepted turn. Definite
submission failure restores recoverable prompt input without dispatching it.
Matching immutable publication receipts settle their commands even when newer
document changes remain dirty. A newer input-preparation failure does not discard
older cancelled command identities.

Accepted submissions belong to the conversation runtime. Ordinary input and input
with file references share provisional user staging, excluded from history and
persisted transcript records. Ordinary input stages synchronously after turn-start
context hooks; only file expansion uses bounded workers. Workers do not publish
file-cache observations. A matching result promotes the same block identity and
publishes its observations; cancelled or stale work cannot dispatch or alter the
file cache. Request history is captured after context hooks, and rejection removes
the exact staged user block without discarding hook history or later callback
display.
Explicit rewind restores input; forced replacement does not restore cancelled
input into the draft.

Engine model-history preparation uses at most two blocking readers. Store reads
share one snapshot and check cancellation between chunks of at most 256 history
items. The command loop can cancel or supersede preparation, retains ordered
turn commands and continues background requests. Preparation errors carry turn,
session and persistence-epoch identity so stale errors cannot stop a newer turn.
Already-materialized history does not enter the reader pool.

Rewind browsing reads pages of at most 128 user profiles, using incrementally
maintained live navigation and the stored transcript index. It does not scan
assistant history or hydrate transcript bodies. Each page captures its request
once; pages covered by live navigation do not open storage. Stored pages use at
most two physical readers. Lua task cancellation stops waiting requests and is
checked between indexed profile reads in one snapshot. Running readers keep their
capacity until the physical worker exits, and cancelled results are not published.
Independent page requests do not supersede one another.

Selection uses one preparation plan for input, mode and the viewport transcript
boundary. It applies directly only when all dependencies are live. Otherwise a
worker prepares them in one consistent read snapshot, including live targets
that depend on stored mode or transcript data. The UI applies the result only if
its request, session, history epoch, history length, cancellation generation and
draft identity are still current.

Resume and rewind share a bounded operation scheduler, with independent
cancellation lanes. Each lane has two workers and one replaceable waiting slot.
A new request can pass one stalled read. Workers check supersession between
reads and suppress cancelled results; cancellation does not preempt an individual
read. Dropping a scheduler signals shutdown without joining workers on the UI.

Asynchronous resume keeps the source document and writer owned while exact
cancellation commands settle, then starts a pollable close. It sends finalization
permission only after the captured generation is durable, then waits for physical
release without blocking the UI. The loading
indicator animates while typing and debug/performance panels remain available.
Cancellation before permission resumes the actor; cancellation after permission
waits for release and reopens the source. Superseded loads cannot adopt an old
destination. The initial immutable preparation is retained until release. A
bounded worker then checks the destination's authoritative head and current layout
inputs. Matching head, width and target-row count reuse that preparation; any
change triggers a full reload, including same-session loads whose first snapshot
preceded the final save. `session:resume:prepare` measures full preparations, while
`session:resume:reused` records successful reuse. Source mutations after the close
captures its generation cancel adoption instead of discarding new work.

Lua signal callbacks, deferred commands, window invocations, task-resume payload
delivery and task execution share a nested count/time budget. Callback and payload
queues have batch limits, and ready tasks rotate in FIFO order. Task identity,
ready order, timers and external waits have exact indexes; recurring ready, wakeup
and external-resolution lookups do not scan parked tasks. Each work unit promotes
at most one due timer behind already-ready tasks, so timer bursts stay within the
pump budget. Payload delivery alternates between owner-thread registry values and
cross-thread JSON, preserving each inbox's FIFO order. Remaining work wakes the event loop instead of being
discarded. Starting a command steps only that command, not unrelated tasks. A
single callback, payload conversion or coroutine resume is not preempted by this
budget.

All `session.info()` user counts come from maintained navigation metadata, without
opening storage, enumerating turns or constructing provider messages. Stored
sessions retain one prefix count and global ordinals for live users. Append,
truncation and compaction maintain those counts without re-ranking the retained
suffix. Metadata-only saves preserve the prefix count, and older acknowledgements
cannot erase newer users.

Schema v6 and later store cumulative user ordinals in their canonical history
index. Exact prefix counts require a bounded lookup without history-body reads,
including users without transcript records and excluding legacy synthetic User
messages. Changed suffixes inherit the retained prefix ordinal. Resume captures the initial count,
header, archives and transcript tail in one SQLite snapshot. Rewind workers capture
the boundary count in their existing snapshot; application does not read storage.
Submission rollback retains its pre-submission count even after durable compaction.
Read-only v3/v4/v5 fallback scans use chunks of at most 256 and check cancellation
between chunks on the resume/rewind workers. Explicit synchronous history
replacement prepares its count before mutation and fails without appending a suffix
if preparation fails.

These boundaries do not make every session operation nonblocking. Explicit
synchronous replacement helpers retain their bounded close deadline. Full-history
materialization, migration and maintenance remain cold operations.

## Preparation, publication and forks

The UI captures changed history rows, record bounds and archive suffixes. Transcript
captures retain immutable chunk references rather than mutable append-stream handles.
Later appends, truncations and replacements cannot change a captured body. Capture
work follows changed rows and their chunk references, not retained session history.
Shared text bytes are not copied; owned strings and structured values still need
copies. Required transcript hydration can still defer capture.

The existing persistence actor prepares captured rows and streaming hashes before
publication. It retains the exact prepared frame for ambiguous-outcome recovery and
canonical retries. Ordinary saves keep one replaceable desired capture; canonical
command identities and ordering remain separate. No extra save-preparation worker
pool or durability journal is introduced.

Native commands carry changed suffixes and references rather than expanded
archives. Preparation, command fingerprinting, catalog publication and fork
capture therefore do not traverse unchanged archive bodies. Fingerprints include
all command inputs, including the expected head. Model-history capture uses the
session's verified archive-base lineage when available, without waiting for the
derived catalog to locate a lineage it already owns.

Prepared frames bind to a session owner, document epoch, exact generation/head and
archive base. A preceding publication can advance that base only through checked
finalization. Publication and acknowledgement use the finalized frame, not the
original preparation. Replayed receipts must not regress independently
acknowledged snapshot versions or shared-body ownership.

Loaded documents verify their exact heads before adoption and recovery. A durable
close target is not sufficient for replacement: the source document and actor
remain owned until physical writer finalization finishes. Foreign epochs cannot
acknowledge archive mutations. Catalog updates publish lightweight
projections with shared message ownership rather than reloading a full session.

Forks capture an exact source head and share retained archive values. A clean fork
uses acknowledged stored state; preserving unsaved edits additionally requires the
captured acknowledged head to match. Source close/adoption still obeys its existing
interactive deadline. Rewind boundaries, retained forks, receipt results and exact
command recovery remain durable.

## Durable dispatch boundary

Provider dispatch requires the exact canonical changed-batch receipt. That batch
binds the turn ID, expected head, submitted history coordinate and all preceding
changed history to one recoverable publication. A user-only intent would not make
an earlier assistant/tool result durable, and a recovery path could not reconstruct
its exact model context from an intent alone. Publishing separate immutable
context dependencies would still require their writes, references, ordering and
recovery receipts before dispatch. The design therefore retains one canonical
publication rather than adding an intent journal or an independent dependency
lifecycle. Visible acceptance does not wait for this boundary.

Instrumentation separates actor preparation (`persist:canonical:prepare`),
transaction/fingerprint/receipt work (`persist:submit_turn`) and catalog handoff
(`persist:publication:complete`). Preparation includes restoration of frozen content
channels, their indexes and changed-row serialization. These measurements do not
hide preparation inside the transaction timer.

On isolated synthetic fixtures in the shared development environment, ten
submissions at each retained history size produced the following debug-build
samples. Preparation and publication samples also include turn transitions and
ordinary publication work, not only submission.

| Retained rows | Enter range | Preparation p50 / max | Submission p50 / max | Publication p50 / max |
| --- | --- | --- | --- | --- |
| 0 | 3.60-6.48 ms | 0.094 / 0.258 ms | 15.80 / 22.83 ms | 0.581 / 1.440 ms |
| 128 | 25.58-32.66 ms | 0.080 / 0.191 ms | 19.55 / 23.52 ms | 0.538 / 0.713 ms |
| 4,096 | 36.51-47.28 ms | 0.107 / 0.244 ms | 22.94 / 43.91 ms | 0.551 / 1.689 ms |

A separate one-sample changed-response regression used a 1 MiB streamed code block
followed by a new submission while persistence was paused. Preparation reached
60.11 ms and submission took 292.26 ms on the actor. The history append, completion
and Enter each remained below their existing 250 ms UI limits. The fixture verifies
both the preceding assistant body and the new user in canonical history before
accepting successful dispatch. This exposes the proportional cost of new content;
it is not a universal commit-time guarantee. The retained-history results and exact
recovery requirements support the canonical boundary without claiming that large
changed batches are cheap.

## Supported schemas and compatibility

Fresh databases use schema v7. A writable open migrates an exact deployed v3, v4,
v5 or v6 database in one transaction. Read-only access validates the original layout
without writing or migrating it. The `response_diagnostics` request-object role
introduced in v5 remains distinct from full responses under the `response` role,
independent of whether the request body is retained.

Schema validation checks the exact layout, not just its version marker. Unreleased
development layouts are not supported migration sources. Production DDL consists
of `crates/store/src/lineage_schema.sql` and the historical snapshots
`crates/store/src/lineage_v3.sql`, `crates/store/src/lineage_v4.sql`,
`crates/store/src/lineage_v5.sql` and `crates/store/src/lineage_v6.sql`.

Migration preserves original logical bytes, hashes, revision identities and
deployed receipt/journal domains. It backfills completed-node proofs where needed
and builds count-capable history semantic mappings in parent-first order. The v4/v5
derived root mappings are replaced; existing immutable semantic nodes remain exact.
Migration restores installed canonical guards and the caller's foreign-key mode,
and validates the final layout and foreign keys before publishing version markers.
Crashes expose either the original database or the complete v7 database.

Every new revision must reference a completed current-format history semantic
mapping. This SQL guard fences writers opened before migration: an unchanged,
already-indexed root remains safe, but an unindexed changed root cannot publish.

Deployed format-1 revision states remain readable. Verified format-2 projections
allow small envelope reads without rehashing their originals. Full historical
reads remain original-authoritative; cold integrity checks verify projections and
reconstructed bytes. A schema upgrade does not authorize deletion of original
objects or inference of missing legacy receipt outcomes.

See [compatibility debt](compat.md) for independent removal conditions.

## Retention and maintenance

One epoch-fenced, resumable canonical collector releases, marks and sweeps in
bounded transactions. Each pass first releases the content of revisions the
retention policy no longer needs: their history root, transcript root and state
references become NULL together, and their session receipt results are dropped
because they can no longer replay. The revision row stays as a chain link, so
revision identities, parents, extents, commit receipts and turns are unchanged.

Content is kept for:

- branch heads and initial revisions;
- retained revisions;
- turn boundaries: each submitted turn revision and its parent;
- every revision from the 16th newest submitted turn of a branch onwards, which
  covers archive bases and recent command replay.

SQL guards make release the only permitted revision update, refuse it for branch
endpoints, retained revisions and submitted turns, and only allow dropping a
receipt result once its revision is released. Loading a released revision
reports its content as missing; ancestry checks walk chain links only.

Reachability then protects revision chains, the content of unreleased revisions,
fork roots, receipt results, journal recovery and request objects, and the sweep
reclaims sequence trees, states and objects that only released revisions used.
Deleted branches retain their initial revision for creation replay. Idle
persistence uses this collector; it does not perform global physical sharing,
search pruning or vacuuming.

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

## Scaling

Saves, title changes, forks, and Enter preparation do no work proportional to
unchanged archived content: holding live context fixed while growing archives
from 0 to 128 checkpoints leaves allocation and new physical object bytes flat.
New live context and new unique retained content still cost proportional work
and storage.
